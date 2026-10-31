//! FolderSync 桌面端入口。
//!
//! 组装顺序：加载设置 → 初始化 SSH 环境/存储/信任/配对 → 恢复文件夹监听
//! → 启动 SSH 服务端与对等连接循环 → 建托盘 → 打开主窗口。

pub mod bindings;
pub mod commands;
pub mod diagnostics;
pub mod dto;
pub mod limits;
pub mod settings;
pub mod state;
pub mod updater;

use std::sync::{Arc, OnceLock};

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};
use tracing::{info, warn};

use state::AppState;

static TRAY_PAUSE: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();
static TRAY_RESUME: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();

/// 默认数据目录：`~/.foldersync`。
fn default_data_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".foldersync")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let data_dir = default_data_dir();
    // 日志级别在初始化日志系统前就要读出来，否则设置页改了也不生效
    let initial_log_level = settings::SettingsStore::load(&data_dir.join("settings.json"))
        .map(|s| s.get().log_level)
        .unwrap_or_else(|_| "info".into());
    let log_guard = diagnostics::init(&data_dir.join("logs"), &initial_log_level)
        .unwrap_or_else(|e| panic!("日志初始化失败: {e:#}"));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            std::mem::forget(log_guard);
            info!(dir = %data_dir.display(), "启动 FolderSync");

            // 必须在窗口加载前完成 manage()：前端挂载后会立刻调用命令，
            // 异步 spawn 初始化会出现短暂的 "state not managed" 报错。
            // 但恢复文件夹（可能要扫描几万文件）放到 manage 之后后台执行，
            // 否则窗口会等到首扫结束才显示。
            let initialized =
                tauri::async_runtime::block_on(async { AppState::init(data_dir.clone()).await });
            match initialized {
                Ok(st) => {
                    let want_auto_start = st.settings_dto().auto_start;
                    let restore = st.clone();
                    app.manage(st);
                    tauri::async_runtime::spawn(async move {
                        state::restore_folders(restore).await;
                    });
                    apply_autostart(app.handle(), want_auto_start);
                    info!("daemon 就绪（SSH 服务端已启动）");
                }
                Err(e) => {
                    warn!("daemon 初始化失败: {e:#}");
                }
            }

            build_tray(app)?;
            spawn_update_checker(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                let close_to_tray = window
                    .app_handle()
                    .try_state::<Arc<AppState>>()
                    .map(|s| s.settings_dto().close_to_tray)
                    .unwrap_or(true);
                if close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_overview,
            commands::get_environment_status,
            commands::repair_ssh_environment,
            commands::open_tailscale_download,
            commands::get_relay_guide,
            commands::list_devices,
            commands::create_pairing_code,
            commands::accept_pairing_code,
            commands::confirm_pairing,
            commands::list_pending_pairings,
            commands::rename_device,
            commands::remove_device,
            commands::list_folders,
            commands::add_folder,
            commands::remove_folder,
            commands::relocate_folder,
            commands::pause_folder,
            commands::resume_folder,
            commands::pause_all,
            commands::resume_all,
            commands::get_folder_tree,
            commands::open_folder,
            commands::open_folder_item,
            commands::open_file,
            commands::set_folder_sharing,
            commands::get_ignore_rules,
            commands::set_ignore_rules,
            commands::list_activity,
            commands::list_conflicts,
            commands::get_conflict_preview,
            commands::resolve_conflict,
            commands::get_settings,
            commands::update_settings,
            commands::open_log_dir,
            commands::export_diagnostics,
            commands::get_log_dir,
            commands::get_local_info,
            commands::check_update,
            commands::install_update,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 启动失败");
}

/// 后台定时检测更新：启动后稍等一会儿查一次，之后每 6 小时一次。
/// 有新版本（或强制更新）时推 `update://available` 给前端弹提示。
/// 检测失败只记日志，绝不影响同步主流程。
fn spawn_update_checker(app: tauri::AppHandle) {
    use tauri::Emitter;
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(updater::FIRST_CHECK_DELAY).await;
        loop {
            match updater::check().await {
                Ok(u) if u.available => {
                    info!(latest = ?u.latest_version, force = u.force, "发现新版本");
                    let _ = app.emit("update://available", u);
                }
                Ok(_) => {}
                Err(e) => warn!("检测更新失败: {e:#}"),
            }
            tokio::time::sleep(updater::CHECK_INTERVAL).await;
        }
    });
}

/// 按设置启用/关闭开机自启（必须在 AppState 就绪后调用）。
pub(crate) fn apply_autostart(app: &tauri::AppHandle, want: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    let currently = mgr.is_enabled().unwrap_or(false);
    if want && !currently {
        match mgr.enable() {
            Ok(()) => info!("已启用开机自启"),
            Err(e) => warn!("启用开机自启失败: {e}"),
        }
    } else if !want && currently {
        match mgr.disable() {
            Ok(()) => info!("已关闭开机自启"),
            Err(e) => warn!("关闭开机自启失败: {e}"),
        }
    }
}

pub(crate) fn update_tray_tooltip(app: &tauri::AppHandle, paused: bool) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        let text = if paused {
            "FolderSync — 已暂停"
        } else {
            "FolderSync — 全部已同步"
        };
        let _ = tray.set_tooltip(Some(text));
    }
    if let Some(item) = TRAY_PAUSE.get() {
        let _ = item.set_checked(paused);
    }
    if let Some(item) = TRAY_RESUME.get() {
        let _ = item.set_checked(!paused);
    }
}

/// 系统托盘：状态图标 + 菜单（打开主窗口 / 暂停全部 / 退出）。
fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
    let pause_all =
        CheckMenuItem::with_id(app, "pause_all", "暂停全部", true, false, None::<&str>)?;
    let resume_all =
        CheckMenuItem::with_id(app, "resume_all", "恢复全部", true, true, None::<&str>)?;
    let _ = TRAY_PAUSE.set(pause_all.clone());
    let _ = TRAY_RESUME.set(resume_all.clone());
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(app, &[&open, &sep, &pause_all, &resume_all, &sep, &quit])?;

    TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().cloned().unwrap())
        .tooltip("FolderSync — 全部已同步")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "pause_all" => {
                if let Some(st) = app.try_state::<Arc<AppState>>() {
                    let st = st.inner().clone();
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        st.pause_all_sync().await;
                        update_tray_tooltip(&app_handle, true);
                        info!("托盘：已暂停全部同步");
                    });
                }
            }
            "resume_all" => {
                if let Some(st) = app.try_state::<Arc<AppState>>() {
                    let st = st.inner().clone();
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        st.resume_all_sync().await;
                        update_tray_tooltip(&app_handle, false);
                        info!("托盘：已恢复全部同步");
                    });
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}
