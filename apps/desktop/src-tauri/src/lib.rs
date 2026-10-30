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

use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};
use tracing::{info, warn};

use state::AppState;

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
    let log_guard = diagnostics::init(&data_dir.join("logs"), "info")
        .unwrap_or_else(|e| panic!("日志初始化失败: {e:#}"));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            std::mem::forget(log_guard);
            info!(dir = %data_dir.display(), "启动 FolderSync");

            let handle = app.handle().clone();
            let dir_for_init = data_dir.clone();
            tauri::async_runtime::spawn(async move {
                match AppState::init(dir_for_init).await {
                    Ok(st) => {
                        let want_auto_start = st.settings_dto().auto_start;
                        state::restore_folders(st.clone()).await;
                        handle.manage(st);
                        apply_autostart(&handle, want_auto_start);
                        info!("daemon 就绪（SSH 服务端已启动）");
                    }
                    Err(e) => {
                        warn!("daemon 初始化失败: {e:#}");
                    }
                }
            });

            build_tray(app)?;
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
            commands::pause_folder,
            commands::resume_folder,
            commands::pause_all,
            commands::resume_all,
            commands::get_folder_tree,
            commands::get_ignore_rules,
            commands::set_ignore_rules,
            commands::list_activity,
            commands::list_conflicts,
            commands::resolve_conflict,
            commands::get_settings,
            commands::update_settings,
            commands::open_log_dir,
            commands::export_diagnostics,
            commands::get_log_dir,
            commands::get_local_info,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 启动失败");
}

/// 按设置启用/关闭开机自启（必须在 AppState 就绪后调用）。
fn apply_autostart(app: &tauri::AppHandle, want: bool) {
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

/// 系统托盘：状态图标 + 菜单（打开主窗口 / 暂停全部 / 退出）。
fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
    let pause_all = MenuItem::with_id(app, "pause_all", "暂停全部", true, None::<&str>)?;
    let resume_all = MenuItem::with_id(app, "resume_all", "恢复全部", true, None::<&str>)?;
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
                    st.paused_all
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    info!("托盘：已暂停全部同步");
                }
            }
            "resume_all" => {
                if let Some(st) = app.try_state::<Arc<AppState>>() {
                    st.paused_all
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    info!("托盘：已恢复全部同步");
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
