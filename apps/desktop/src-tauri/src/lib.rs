//! FolderSync 桌面端入口。
//!
//! 组装顺序：加载设置 → 启动 daemon（端点/存储/信任/配对）→ 恢复文件夹监听
//! → 启动对等连接循环 → 建托盘 → 打开主窗口（提示词 5.1 / 5.2）。

pub mod bindings;
pub mod commands;
pub mod diagnostics;
pub mod dto;
pub mod limits;
pub mod protocol;
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
    // 日志同时写终端与文件（M6：日志与诊断包导出）
    let data_dir = default_data_dir();
    let log_guard = diagnostics::init(&data_dir.join("logs"), "info")
        .unwrap_or_else(|e| panic!("日志初始化失败: {e:#}"));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            // 日志 guard 必须活得比进程长，否则文件写入会立刻停
            std::mem::forget(log_guard);
            info!(dir = %data_dir.display(), "启动 FolderSync");

            // daemon 初始化是异步的，塞进 tokio 运行时
            let handle = app.handle().clone();
            let dir_for_init = data_dir.clone();
            tauri::async_runtime::spawn(async move {
                match AppState::init(dir_for_init.clone()).await {
                    Ok(st) => {
                        // 恢复已登记的文件夹监听
                        state::restore_folders(st.clone()).await;

                        // 挂载 ALPN 协议处理器：没有这一步，对端无法连进来
                        let events = make_event_sink(handle.clone());
                        let server = protocol::DesktopServer {
                            state: st.clone(),
                            events: events.clone(),
                        };
                        let _router = iroh::protocol::Router::builder(st.endpoint.clone())
                            .accept(fs_sync_net::ALPN, server)
                            .accept(
                                iroh_blobs::ALPN,
                                iroh_blobs::BlobsProtocol::new(&st.blobs, None),
                            )
                            .spawn();

                        // 对等连接循环：主动连每个已信任设备
                        spawn_peer_loop(st.clone(), events);

                        handle.manage(st);
                        info!("daemon 就绪（协议已挂载）");
                    }
                    Err(e) => {
                        warn!("daemon 初始化失败: {e:#}");
                        // 仍然 manage 一个状态，避免所有命令都返回 "state not managed"
                    }
                }
            });

            // 托盘
            build_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭窗口 → 最小化到托盘（提示词 5.1）
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

/// 构造事件发送器：把后端事件转发到前端。
fn make_event_sink(app: tauri::AppHandle) -> protocol::EventSink {
    Arc::new(move |name: &str, payload: serde_json::Value| {
        let _ = tauri::Emitter::emit(&app, name, payload);
    })
}

/// 对等连接循环：主动连每个已信任设备，交换索引并持续收发增量。
///
/// 断线后指数退避重连（提示词 4.8 / 7：断网再联网自动恢复）。
fn spawn_peer_loop(state: Arc<AppState>, events: protocol::EventSink) {
    tauri::async_runtime::spawn(async move {
        let mut backoff = std::time::Duration::from_secs(2);
        loop {
            let peers = state.trust.devices().unwrap_or_default();
            for peer in peers {
                let Ok(peer_id) = peer.id.parse::<iroh::EndpointId>() else {
                    continue;
                };
                if peer_id == state.endpoint_id {
                    continue;
                }
                let folder_ids: Vec<String> = {
                    let guard = state.runtimes.lock().await;
                    guard.keys().cloned().collect()
                };
                if folder_ids.is_empty() {
                    // 没有文件夹就没必要连接，省电省流量
                    continue;
                }

                match sync_round(&state, peer_id, &folder_ids, &events).await {
                    Ok(()) => backoff = std::time::Duration::from_secs(2),
                    Err(e) => tracing::debug!(peer = %peer.name, "同步会话结束: {e:#}"),
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
        }
    });
}

/// 一轮同步会话：连上 → 发全量索引 → 持续收发，直到断线。
async fn sync_round(
    state: &Arc<AppState>,
    peer_id: iroh::EndpointId,
    folder_ids: &[String],
    events: &protocol::EventSink,
) -> anyhow::Result<()> {
    use fs_sync_net::frame;
    use fs_sync_net::message::Message;

    let conn = state
        .endpoint
        .connect(peer_id, fs_sync_net::ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("连接对端失败: {e}"))?;

    // 报告连接方式（前端"直连/中继/经 Tailscale"）
    let kind = match fs_sync_net::conn::classify_conn(&conn) {
        Some(fs_sync_net::conn::ConnType::Direct) => dto::ConnKind::Direct,
        Some(fs_sync_net::conn::ConnType::Relay) => dto::ConnKind::Relay,
        Some(fs_sync_net::conn::ConnType::Tailscale) => dto::ConnKind::Tailscale,
        None => dto::ConnKind::Unknown,
    };
    let rtt = fs_sync_net::conn::measure_rtt(&conn).map(|d| d.as_millis() as u64);
    state
        .conns
        .lock()
        .await
        .insert(peer_id.to_string(), (kind, rtt));
    info!(peer = %peer_id.fmt_short(), conn = ?kind, "同步会话已建立");

    let (mut send, mut recv) = conn.open_bi().await?;

    // Hello 握手：让对端确认我们已信任
    frame::write_frame(
        &mut send,
        &Message::Hello {
            protocol_version: fs_sync_net::message::PROTOCOL_VERSION,
            device_name: state.device_name.clone(),
            folders: folder_ids
                .iter()
                .map(|id| fs_sync_net::message::FolderSummary {
                    folder_id: id.clone(),
                    name: id.clone(),
                })
                .collect(),
        },
    )
    .await?;

    // 全量索引
    for fid in folder_ids {
        let snap = {
            let guard = state.runtimes.lock().await;
            guard.get(fid).cloned()
        };
        if let Some(rt) = snap {
            let entries: Vec<_> = rt.session.snapshot().await.into_values().collect();
            frame::write_frame(
                &mut send,
                &Message::Index {
                    folder_id: fid.clone(),
                    entries,
                },
            )
            .await?;
        }
    }

    // 持续收发
    loop {
        tokio::select! {
            msg = frame::read_frame(&mut recv) => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        state.conns.lock().await.remove(&peer_id.to_string());
                        return Err(anyhow::anyhow!("读对端消息失败: {e}"));
                    }
                };
                match msg {
                    Message::Index { folder_id, entries }
                    | Message::IndexUpdate { folder_id, entries } => {
                        let rt = {
                            let guard = state.runtimes.lock().await;
                            guard.get(&folder_id).cloned()
                        };
                        if let Some(rt) = rt {
                            let count = entries.len();
                            let hint = entries.first().map(|e| e.path.clone()).unwrap_or_default();
                            let remote: std::collections::HashMap<_, _> =
                                entries.into_iter().map(|e| (e.path.clone(), e)).collect();
                            let puller = fs_sync_daemon::make_puller(
                                state.endpoint.clone(),
                                peer_id,
                                rt.session.blobs.clone(),
                            );
                            match rt.session.apply_remote(&remote, puller).await {
                                Ok(stats) if stats.pulled + stats.deleted + stats.conflicts > 0 => {
                                    info!(pulled = stats.pulled, deleted = stats.deleted, "已应用对端变更");
                                    // 下载限速：按拉取到的条目大小消耗配额（M6）
                                    let bytes: u64 = remote
                                        .values()
                                        .filter(|e| !e.is_dir && !e.deleted)
                                        .map(|e| e.size)
                                        .sum();
                                    state.limits_snapshot().download.consume(bytes).await;
                                    protocol::emit_activity(
                                        events,
                                        dto::ActivityKind::Download,
                                        &folder_id,
                                        &hint,
                                        &format!("同步 {count} 项"),
                                    );
                                }
                                Ok(_) => {}
                                Err(e) => warn!("应用对端变更失败: {e:#}"),
                            }
                        }
                    }
                    Message::Ping => {
                        frame::write_frame(&mut send, &Message::Pong).await.ok();
                    }
                    Message::Pong => {}
                    other => tracing::debug!("忽略消息: {other:?}"),
                }
            }
            entry = recv_local_change(state, folder_ids) => {
                let (fid, ent) = entry;
                // 上传限速：按条目大小消耗配额（M6）
                if !ent.is_dir && !ent.deleted {
                    state.limits_snapshot().upload.consume(ent.size).await;
                }
                let msg = Message::IndexUpdate {
                    folder_id: fid,
                    entries: vec![ent],
                };
                if let Err(e) = frame::write_frame(&mut send, &msg).await {
                    return Err(anyhow::anyhow!("推送本地变更失败: {e}"));
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {
                frame::write_frame(&mut send, &Message::Ping).await.ok();
            }
        }
    }
}

/// 等待任一文件夹的本地变更。
///
/// 永不返回 None：tokio::select! 的分支一旦模式不匹配就被永久禁用
/// （见 docs/decisions.md 里 M4 的教训）。
async fn recv_local_change(
    state: &Arc<AppState>,
    folder_ids: &[String],
) -> (String, fs_sync_core::FileEntry) {
    loop {
        let rts: Vec<(String, Arc<fs_sync_daemon::FolderRuntime>)> = {
            let guard = state.runtimes.lock().await;
            folder_ids
                .iter()
                .filter_map(|id| guard.get(id).map(|rt| (id.clone(), rt.clone())))
                .collect()
        };
        for (id, rt) in &rts {
            let mut ob = rt.outbox.lock().await;
            if let Some(entry) = ob.pop() {
                return (id.clone(), entry);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
            // 左键点击托盘图标 → 显示主窗口
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
