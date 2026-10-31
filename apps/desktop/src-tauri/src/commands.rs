//! Tauri commands：前端唯一入口。
//!
//! 所有命令返回 `Result<T, ErrorDto>`，错误码稳定可本地化。

use std::sync::Arc;

use fs_sync_daemon::{EventKind, FileEvent};
use fs_sync_net::message::{PairingRequest, PeerInfo};
use fs_sync_store::Folder;
use tauri::State;
use tracing::{info, warn};

use crate::dto::*;
use crate::state::{AppState, fingerprint_of, hostname};

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 原子替换目标文件：Windows 用 MoveFileEx(REPLACE_EXISTING)，
/// 其他平台 rename 本身就是原子替换。调用方先把内容完整写到同目录临时文件。
#[cfg(windows)]
fn atomic_replace_file(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let from: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace_file(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(src, dst)
}

/// 概览：顶部总状态 + 设备/文件夹聚合。
#[tauri::command]
pub async fn get_overview(state: State<'_, Arc<AppState>>) -> CmdResult<OverviewDto> {
    let folders = state.folders().await;
    let devices = state.devices().await;

    let online = devices.iter().filter(|d| d.online).count() as u32;
    let paused_all = state.paused_all.load(std::sync::atomic::Ordering::Relaxed);
    let any_paused = paused_all || folders.iter().any(|f| f.paused);
    let error_count = folders
        .iter()
        .filter(|f| f.status == FolderStatus::Error)
        .count() as u32;
    let remaining: u32 = folders.iter().map(|f| f.pending).sum();
    let syncing: u32 = folders
        .iter()
        .filter(|f| f.status == FolderStatus::Syncing)
        .map(|f| f.pending)
        .sum();
    let busy = folders
        .iter()
        .any(|f| matches!(f.status, FolderStatus::Scanning | FolderStatus::Syncing));

    let status = if folders.is_empty() || devices.is_empty() {
        OverallStatus::NotConfigured
    } else if error_count > 0 {
        OverallStatus::Error
    } else if any_paused {
        OverallStatus::Paused
    } else if remaining > 0 || busy {
        OverallStatus::Syncing
    } else {
        OverallStatus::Synced
    };

    let ts = fs_sync_net::tailscale::detect();
    let ts_desc = ts.ips.first().map(|ip| {
        let name = ts.hostname.clone().unwrap_or_else(|| "Tailscale".into());
        format!("{name} ({ip})")
    });

    Ok(OverviewDto {
        status,
        device_name: state.device_name.clone(),
        device_id: state.device_id.clone(),
        fingerprint: fingerprint_of(&state.device_id),
        tailscale: ts_desc,
        online_devices: online,
        syncing_files: syncing,
        remaining_files: remaining,
        error_count,
    })
}

// ---------- 环境引导 ----------

/// 本机 SSH / Tailscale / 中继环境状态。
#[tauri::command]
pub async fn get_environment_status(state: State<'_, Arc<AppState>>) -> CmdResult<EnvStatusDto> {
    Ok(state.env_status())
}

/// 重新初始化 SSH 环境（生成密钥、目录、尝试修复防火墙）。
#[tauri::command]
pub async fn repair_ssh_environment(state: State<'_, Arc<AppState>>) -> CmdResult<EnvStatusDto> {
    let settings = state.settings_dto();
    let env = fs_sync_net::env::ensure_ssh_env(&state.data_dir, settings.ssh_port);
    if !env.firewall_ok {
        // 普通权限加规则会失败，这里主动弹 UAC（失败也不阻塞，UI 会提示重试）
        let _ = fs_sync_net::env::repair_firewall_elevated(settings.ssh_port);
    }
    Ok(state.env_status())
}

/// 打开 Tailscale 下载页（引导安装）。
#[tauri::command]
pub async fn open_tailscale_download() -> CmdResult<()> {
    open_url("https://tailscale.com/download")
        .map_err(|e| ErrorDto::new("open_failed", format!("打开下载页失败: {e}")))
}

/// 中继服务器搭建步骤（Markdown 文本，前端弹窗展示）。
#[tauri::command]
pub async fn get_relay_guide() -> CmdResult<String> {
    Ok(concat!(
        "## 搭建 FolderSync 中继服务器\n\n",
        "1. 找一台有公网 IP 的服务器（1 核 1G 即可）。\n",
        "2. 下载 `foldersync-relay` 可执行文件，运行：\n\n",
        "```bash\nfoldersync-relay --port 9443 --data-dir ./foldersync-relay-data\n```\n\n",
        "3. 在服务器防火墙放行 TCP 9443（云厂商安全组也要放行）。\n",
        "4. 回到 FolderSync 设置页，把中继地址填成 `tcp://服务器IP:9443`。\n",
        "5. 两台电脑填同一个地址，然后在设备页重新配对/同步。\n\n",
        "安全提示：中继是自建可信服务器。SSH/SFTP 只保护传输链路，文件会以普通文件落在中继服务器磁盘上，",
        "请用磁盘加密、文件权限和防火墙保护服务器；不要使用不受信任的公共中继。"
    )
    .to_string())
}

fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

// ---------- 设备 ----------

/// 已配对设备列表。
#[tauri::command]
pub async fn list_devices(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<DeviceDto>> {
    Ok(state.devices().await)
}

/// 推送单个文件夹的最新状态。
async fn emit_folder_status(app: &tauri::AppHandle, state: &Arc<AppState>, folder_id: &str) {
    use tauri::Emitter;
    if let Some(f) = state
        .folders()
        .await
        .into_iter()
        .find(|f| f.id == folder_id)
    {
        let _ = app.emit("folder://status", f);
    }
}

/// 本机用于配对码的 Tailscale 地址。
fn local_tailscale_addrs() -> Vec<String> {
    fs_sync_net::tailscale::detect().ips
}

/// 生成配对码（5 分钟有效，一次性）。
#[tauri::command]
pub async fn create_pairing_code(state: State<'_, Arc<AppState>>) -> CmdResult<PairingCodeDto> {
    let relay = state.settings.get().relay_url;
    let code = state
        .pairing
        .create_code(local_tailscale_addrs(), relay.as_deref())
        .await;
    // 中继方案：生成后在后台监听配对房间，等对端提交 SSH 公钥
    if let Some(url) = relay.clone() {
        state.pairing.spawn_relay_pairing_watcher(code.clone(), url);
    }
    let encoded = fs_sync_net::pairing::encode(&code);
    let expires_at = code.expires_at as i64;
    Ok(PairingCodeDto {
        code: encoded.clone(),
        expires_at,
        ttl_seconds: (expires_at - now_secs()).max(0),
        qr_content: encoded,
    })
}

/// 用配对码完成配对。
#[tauri::command]
pub async fn accept_pairing_code(
    state: State<'_, Arc<AppState>>,
    code: String,
) -> CmdResult<DeviceDto> {
    let parsed = fs_sync_net::pairing::decode(&code, now_secs() as u64)
        .map_err(|e| ErrorDto::new("bad_code", format!("配对码无效: {e}")))?;

    let mut me = state
        .pairing
        .own_info(local_tailscale_addrs())
        .map_err(|e| ErrorDto::new("identity", format!("读取本机身份失败: {e:#}")))?;
    // 对端可能已经配了中继但本机还没配：沿用配对码里的中继地址，保证响应能带回
    me.relay = state
        .settings
        .get()
        .relay_url
        .clone()
        .or_else(|| parsed.peer.relay.clone());
    let request = PairingRequest {
        token: parsed.token.clone(),
        peer: me,
    };

    // 优先 Tailscale 直连；失败再回退中继。
    let mut response = None;
    let mut errors = Vec::new();
    for host in &parsed.peer.addrs {
        let target = fs_sync_net::ssh_client::SshTarget {
            host: host.clone(),
            port: parsed.peer.port,
            username: parsed.peer.username.clone(),
            host_key: parsed.peer.host_key.clone(),
        };
        match fs_sync_net::ssh_client::pair_via_ssh(&target, &parsed.token, request.clone()).await {
            Ok(r) => {
                response = Some(r);
                break;
            }
            Err(e) => errors.push(format!("{host}: {e:#}")),
        }
    }
    if response.is_none()
        && let Some(relay) = parsed
            .peer
            .relay
            .clone()
            .or_else(|| state.settings.get().relay_url.clone())
    {
        match fs_sync_net::relay::pair_via_relay(&relay, &parsed.token, request).await {
            Ok(r) => response = Some(r),
            Err(e) => errors.push(format!("中继 {relay}: {e:#}")),
        }
    }
    let response = response.ok_or_else(|| {
        if parsed.peer.addrs.is_empty() && parsed.peer.relay.is_none() {
            ErrorDto::new(
                "no_network",
                "双方都没有可用的网络：请先安装并登录 Tailscale，或在服务器上搭建 FolderSync 中继",
            )
        } else {
            ErrorDto::new(
                "connect_failed",
                format!("连接对端失败（已尝试所有地址）: {}", errors.join("; ")),
            )
        }
    })?;

    if !response.accepted {
        return Err(ErrorDto::new("rejected", "对端拒绝了配对请求".to_string()));
    }
    fs_sync_net::identity::verify_peer_info(&response.peer)
        .map_err(|e| ErrorDto::new("bad_peer", format!("对端身份校验失败: {e:#}")))?;
    state
        .trust
        .add(&response.peer)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    info!(device = %response.peer.device_name, "配对成功");
    Ok(device_dto(response.peer))
}

fn device_dto(peer: PeerInfo) -> DeviceDto {
    DeviceDto {
        id: peer.device_id.clone(),
        name: peer.device_name,
        fingerprint: fingerprint_of(&peer.device_id),
        online: false,
        conn: ConnKind::Unknown,
        rtt_ms: None,
        address: peer.addrs.first().cloned(),
        port: peer.port,
        relay_ready: peer.relay_room.is_some(),
    }
}

/// 确认/拒绝一个待处理的配对请求。
#[tauri::command]
pub async fn confirm_pairing(
    state: State<'_, Arc<AppState>>,
    request_id: String,
    accept: bool,
) -> CmdResult<()> {
    if !state.pairing.confirm(&request_id, accept).await {
        return Err(ErrorDto::new("not_found", "配对请求不存在或已过期"));
    }
    Ok(())
}

/// 待确认的配对请求列表。
#[tauri::command]
pub async fn list_pending_pairings(
    state: State<'_, Arc<AppState>>,
) -> CmdResult<Vec<PendingPairingDto>> {
    let now = now_secs();
    Ok(state
        .pairing
        .list_pending()
        .await
        .into_iter()
        .map(|p| PendingPairingDto {
            request_id: p.request_id,
            device_name: p.peer_name,
            fingerprint: p.fingerprint,
            expires_at: now + 120,
        })
        .collect())
}

/// 重命名已配对设备。
#[tauri::command]
pub async fn rename_device(
    state: State<'_, Arc<AppState>>,
    id: String,
    name: String,
) -> CmdResult<()> {
    let mut device = state
        .trust
        .get(&id)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?
        .ok_or_else(|| ErrorDto::new("not_found", "设备不存在"))?;
    device.name = name;
    state
        .trust
        .upsert(&device)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    Ok(())
}

/// 取消配对。
#[tauri::command]
pub async fn remove_device(state: State<'_, Arc<AppState>>, id: String) -> CmdResult<()> {
    state
        .trust
        .remove(&id)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    Ok(())
}

// ---------- 文件夹 ----------

/// 文件夹列表。
#[tauri::command]
pub async fn list_folders(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<FolderDto>> {
    Ok(state.folders().await)
}

/// 添加并开始同步一个文件夹。
#[tauri::command]
pub async fn add_folder(
    state: State<'_, Arc<AppState>>,
    req: AddFolderRequest,
) -> CmdResult<FolderDto> {
    let path = std::path::PathBuf::from(&req.path);
    if !path.is_dir() {
        return Err(ErrorDto::new(
            "not_a_dir",
            format!("不是文件夹: {}", req.path),
        ));
    }
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "根目录".into());

    let folder_id = format!("folder-{}", &blake3::hash(name.as_bytes()).to_hex()[..16]);
    if state
        .store
        .list_folders()
        .unwrap_or_default()
        .iter()
        .any(|f| f.id == folder_id)
    {
        return Err(ErrorDto::new("duplicate", "该文件夹已在同步列表中"));
    }

    let mut ignore_rules = state.settings_dto().default_ignore;
    ignore_rules.extend(req.ignore.custom.clone());
    if !ignore_rules.is_empty() || !req.ignore.use_defaults {
        let ignore_path = path.join(".syncignore");
        let mut text = String::new();
        if !req.ignore.use_defaults {
            text.push_str(fs_sync_core::DEFAULTS_OFF);
            text.push('\n');
        }
        text.push_str(&ignore_rules.join("\n"));
        let _ = std::fs::write(&ignore_path, text);
    }

    state
        .store
        .add_folder(&Folder {
            id: folder_id.clone(),
            name: name.clone(),
            local_path: path.clone(),
            paused: false,
            tombstone_days: 30,
            created_at: now_secs(),
            shared_with: req.device_ids.clone(),
        })
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;

    // 首扫大文件夹可能要几十秒：先返回，后台启动，界面显示「扫描中」
    state
        .starting_folders
        .lock()
        .await
        .insert(folder_id.clone());
    let st = state.inner().clone();
    let start_id = folder_id.clone();
    let start_path = path.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = st.start_folder(start_id, start_path).await {
            warn!("启动同步失败: {e:#}");
        }
    });

    state
        .log_activity("info", &folder_id, "", "文件夹已加入同步")
        .ok();

    Ok(FolderDto {
        id: folder_id,
        name,
        path: req.path,
        status: FolderStatus::Scanning,
        paused: false,
        last_sync_at: None,
        pending: 0,
        file_count: 0,
        shared_with: req.device_ids,
        last_error: None,
    })
}

/// 移除文件夹同步（不删本地文件）。
#[tauri::command]
pub async fn remove_folder(state: State<'_, Arc<AppState>>, id: String) -> CmdResult<()> {
    // 先标记删除，再停 runtime、最后删库：避免首扫恰好完成时把 runtime 挂回来
    state.removing_folders.lock().unwrap().insert(id.clone());
    state.stop_folder(&id).await;
    let result = state
        .store
        .remove_folder(&id)
        .map_err(|e| ErrorDto::new("store", e.to_string()));
    state.removing_folders.lock().unwrap().remove(&id);
    result
}

/// 重新定位文件夹（外置盘/网络盘路径变化后）。
#[tauri::command]
pub async fn relocate_folder(
    state: State<'_, Arc<AppState>>,
    id: String,
    path: String,
) -> CmdResult<()> {
    let new_path = std::path::PathBuf::from(&path);
    if !new_path.is_dir() {
        return Err(ErrorDto::new("bad_path", "目标目录不存在或不是文件夹"));
    }
    if state
        .store
        .list_folders()
        .unwrap_or_default()
        .iter()
        .all(|f| f.id != id)
    {
        return Err(ErrorDto::new("not_found", "文件夹不存在"));
    }
    state.stop_folder(&id).await;
    state
        .store
        .set_folder_path(&id, &new_path)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    let st = state.inner().clone();
    let folder_id = id.clone();
    tauri::async_runtime::spawn(async move {
        let _ = st.start_folder(folder_id, new_path).await;
    });
    Ok(())
}

/// 暂停文件夹同步。
#[tauri::command]
pub async fn pause_folder(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
) -> CmdResult<()> {
    state
        .store
        .set_folder_paused(&id, true)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    state.stop_folder(&id).await;
    emit_folder_status(&app, &state, &id).await;
    Ok(())
}

/// 恢复文件夹同步。
#[tauri::command]
pub async fn resume_folder(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
) -> CmdResult<()> {
    state
        .store
        .set_folder_paused(&id, false)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    let folder = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.id == id)
        .ok_or_else(|| ErrorDto::new("not_found", "文件夹不存在"))?;
    state
        .start_folder(folder.id, folder.local_path)
        .await
        .map_err(|e| ErrorDto::new("runtime", format!("{e:#}")))?;
    emit_folder_status(&app, &state, &id).await;
    Ok(())
}

/// 暂停全部。
#[tauri::command]
pub async fn pause_all(app: tauri::AppHandle, state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    state.pause_all_sync().await;
    crate::update_tray_tooltip(&app, true);
    Ok(())
}

/// 恢复全部。
#[tauri::command]
pub async fn resume_all(app: tauri::AppHandle, state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    state.resume_all_sync().await;
    crate::update_tray_tooltip(&app, false);
    Ok(())
}

/// 文件夹文件树（只读浏览）。
#[tauri::command]
pub async fn get_folder_tree(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
    subpath: Option<String>,
) -> CmdResult<Vec<TreeNodeDto>> {
    if !state
        .store
        .list_folders()
        .unwrap_or_default()
        .iter()
        .any(|f| f.id == folder_id)
    {
        return Err(ErrorDto::new("not_found", "文件夹不存在"));
    }

    let prefix = subpath.unwrap_or_default();
    let entries = state
        .store
        .list_entries(&folder_id)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;

    Ok(entries
        .into_iter()
        .filter(|e| !e.deleted)
        .filter(|e| {
            if prefix.is_empty() {
                !e.path.contains('/')
            } else {
                e.path.starts_with(&format!("{prefix}/"))
                    && e.path[prefix.len() + 1..].split('/').count() == 1
            }
        })
        .map(|e| TreeNodeDto {
            name: e.path.rsplit('/').next().unwrap_or(&e.path).to_string(),
            is_conflict: e.path.contains(".sync-conflict-"),
            state: if e.deleted {
                FileState::Deleted
            } else {
                FileState::Synced
            },
            path: e.path,
            is_dir: e.is_dir,
            size: e.size as i64,
            mtime: e.mtime,
        })
        .collect())
}

/// 获取文件夹的忽略规则。
#[tauri::command]
pub async fn get_ignore_rules(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
) -> CmdResult<IgnoreRulesDto> {
    let folder = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| ErrorDto::new("not_found", "文件夹不存在"))?;

    let (use_defaults, custom) = fs_sync_core::read_syncignore_config(&folder.local_path);

    Ok(IgnoreRulesDto {
        use_defaults,
        custom,
        defaults: fs_sync_core::default_rules(),
    })
}

/// 保存文件夹的忽略规则。
#[tauri::command]
pub async fn set_ignore_rules(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
    rules: IgnoreRulesDto,
) -> CmdResult<()> {
    let folder = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| ErrorDto::new("not_found", "文件夹不存在"))?;
    let custom: Vec<String> = rules
        .custom
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    std::fs::write(folder.local_path.join(".syncignore"), {
        let mut text = String::new();
        if !rules.use_defaults {
            text.push_str(fs_sync_core::DEFAULTS_OFF);
            text.push('\n');
        }
        text.push_str(&custom.join("\n"));
        text
    })
    .map_err(|e| ErrorDto::new("io", format!("写入忽略规则失败: {e}")))?;

    // 规则变化后重启该文件夹运行时：新 matcher 立即生效，
    // 且重扫会把刚被忽略的路径从索引中移除（不生成墓碑、不删对端文件）。
    let running = state
        .runtimes
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.session.folder_id == folder_id);
    if running {
        state.stop_folder(&folder_id).await;
        let _ = state
            .start_folder(folder_id.clone(), folder.local_path.clone())
            .await;
    }
    Ok(())
}

// ---------- 活动 ----------

/// 活动列表（游标分页）。
#[tauri::command]
pub async fn list_activity(
    state: State<'_, Arc<AppState>>,
    filter: ActivityFilter,
) -> CmdResult<Vec<ActivityDto>> {
    let rows = state
        .store
        .list_activity_filtered(
            filter.folder_id.as_deref(),
            filter.kind.map(activity_kind_str),
            filter.limit.unwrap_or(200),
            filter.before_id,
        )
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|a| ActivityDto {
            id: a.id,
            // store 里是毫秒，DTO 约定 Unix 秒（前端 formatTime 会再 ×1000）
            at: a.at / 1000,
            kind: match a.kind.as_str() {
                "upload" => ActivityKind::Upload,
                "download" => ActivityKind::Download,
                "delete" => ActivityKind::Delete,
                "conflict" => ActivityKind::Conflict,
                "error" => ActivityKind::Error,
                _ => ActivityKind::Info,
            },
            folder_id: a.folder_id,
            path: a.path,
            size: a.size,
            message: a.detail,
        })
        .collect())
}

fn activity_kind_str(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Upload => "upload",
        ActivityKind::Download => "download",
        ActivityKind::Delete => "delete",
        ActivityKind::Conflict => "conflict",
        ActivityKind::Error => "error",
        ActivityKind::Info => "info",
    }
}

// ---------- 冲突 ----------

pub fn conflict_dtos_for(state: &AppState, folder_id: Option<String>) -> Vec<ConflictDto> {
    let mut out = Vec::new();
    for f in state.store.list_folders().unwrap_or_default() {
        if let Some(ref want) = folder_id
            && &f.id != want
        {
            continue;
        }
        let entries = state.store.list_entries(&f.id).unwrap_or_default();
        // 已处理（删掉/改名）的副本在索引里是墓碑，以前没过滤，界面上永远消不掉；
        // 再看一眼磁盘，防止索引还没跟上
        for e in entries.iter().filter(|e| {
            !e.deleted && e.path.contains(".sync-conflict-") && f.local_path.join(&e.path).is_file()
        }) {
            let name = e.path.rsplit('/').next().unwrap_or(&e.path);
            let (original_name, device) = parse_conflict_name(name);
            let original_path = match e.path.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{original_name}"),
                None => original_name.clone(),
            };
            let abs_orig = f.local_path.join(&original_path);
            let meta_orig = std::fs::metadata(&abs_orig).ok();
            out.push(ConflictDto {
                id: e.path.clone(),
                folder_id: f.id.clone(),
                original_path,
                conflict_path: e.path.clone(),
                conflict_size: e.size as i64,
                original_size: meta_orig.as_ref().map(|m| m.len() as i64),
                conflict_mtime: e.mtime,
                original_mtime: meta_orig
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
                source_device: device,
            });
        }
    }
    out
}

const PREVIEW_LIMIT: u64 = 64 * 1024;

fn read_text_preview(path: &std::path::Path) -> std::io::Result<(String, bool, bool)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let mut buf = vec![0u8; PREVIEW_LIMIT as usize];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    let binary = buf.contains(&0);
    Ok((
        String::from_utf8_lossy(&buf).to_string(),
        metadata.len() > n as u64,
        binary,
    ))
}

/// 读取冲突两侧的文本预览，供用户判断保留哪一版。
#[tauri::command]
pub async fn get_conflict_preview(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
    original_path: String,
    conflict_path: String,
) -> CmdResult<ConflictPreviewDto> {
    let folder = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| ErrorDto::new("not_found", "文件夹不存在"))?;
    let conflict = resolve_inside_folder(&folder, &conflict_path)?;
    let (conflict_text, conflict_truncated, conflict_binary) = read_text_preview(&conflict)
        .map_err(|e| ErrorDto::new("io", format!("读取冲突副本失败: {e}")))?;
    let (original, original_truncated, original_binary) =
        match resolve_inside_folder(&folder, &original_path)
            .ok()
            .filter(|p| p.is_file())
        {
            Some(path) => match read_text_preview(&path) {
                Ok((text, truncated, binary)) => (Some(text), truncated, binary),
                Err(_) => (None, false, false),
            },
            None => (None, false, false),
        };
    Ok(ConflictPreviewDto {
        original,
        conflict: conflict_text,
        truncated: conflict_truncated || original_truncated,
        binary: conflict_binary || original_binary,
    })
}

/// 列出所有冲突副本文件。
#[tauri::command]
pub async fn list_conflicts(
    state: State<'_, Arc<AppState>>,
    folder_id: Option<String>,
) -> CmdResult<Vec<ConflictDto>> {
    Ok(conflict_dtos_for(&state, folder_id))
}

/// 从 `名字.sync-conflict-20260101-120000-设备名.ext` 解析原名与设备。
fn parse_conflict_name(name: &str) -> (String, String) {
    let Some(pos) = name.find(".sync-conflict-") else {
        return (name.to_string(), String::new());
    };
    let stem = &name[..pos];
    let rest = &name[pos + ".sync-conflict-".len()..];
    let (ext, body) = match rest.rsplit_once('.') {
        Some((body, ext)) => (format!(".{ext}"), body),
        None => (String::new(), rest),
    };
    let device = body.splitn(3, '-').nth(2).unwrap_or("").to_string();
    (format!("{stem}{ext}"), device)
}

/// 解决冲突（保留其一或两者）。
#[tauri::command]
pub async fn resolve_conflict(
    state: State<'_, Arc<AppState>>,
    conflict_id: String,
    resolution: ConflictResolution,
    folder_id: Option<String>,
) -> CmdResult<()> {
    if !conflict_id.contains(".sync-conflict-") {
        return Err(ErrorDto::new("invalid", "不是冲突副本"));
    }
    let mut found = None;
    for f in state.store.list_folders().unwrap_or_default() {
        if let Some(ref want) = folder_id
            && &f.id != want
        {
            continue;
        }
        let entries = state.store.list_entries(&f.id).unwrap_or_default();
        if entries.iter().any(|e| e.path == conflict_id) {
            found = Some(f);
            break;
        }
    }
    let folder = found.ok_or_else(|| ErrorDto::new("not_found", "冲突文件不存在"))?;

    let (dir, name) = match conflict_id.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, conflict_id.as_str()),
    };
    let join = |n: &str| dir.map_or_else(|| n.to_string(), |d| format!("{d}/{n}"));
    let (original_name, _) = parse_conflict_name(name);
    let original_rel = join(&original_name);
    let conflict_abs = folder.local_path.join(&conflict_id);
    let original_abs = folder.local_path.join(&original_rel);
    // 这次操作后新出现/变化的路径，需要通知同步层
    let mut touched = vec![conflict_id.clone()];

    if conflict_abs.is_file() {
        match resolution {
            ConflictResolution::KeepConflict => {
                // 先把副本完整写到同目录临时文件并 fsync，再原子替换原文件；
                // 任何一步失败原文件都保持原样，不会出现“写了一半”的损坏。
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let tmp = folder.local_path.join(format!(
                    ".foldersync-tmp-replace-{}-{nanos}.tmp",
                    std::process::id()
                ));
                std::fs::copy(&conflict_abs, &tmp)
                    .map_err(|e| ErrorDto::new("io", format!("准备替换失败: {e}")))?;
                let sync_ok = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&tmp)
                    .and_then(|f| f.sync_all());
                if let Err(e) = sync_ok {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(ErrorDto::new("io", format!("刷盘失败: {e}")));
                }
                if let Err(e) = atomic_replace_file(&tmp, &original_abs) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(ErrorDto::new("io", format!("替换失败: {e}")));
                }
                std::fs::remove_file(&conflict_abs)
                    .map_err(|e| ErrorDto::new("io", format!("删除副本失败: {e}")))?;
                touched.push(original_rel);
            }
            ConflictResolution::KeepOriginal => {
                if !original_abs.is_file() {
                    return Err(ErrorDto::new(
                        "original_missing",
                        "原文件已不存在，不能只保留原文件；请改选「保留副本」或「两者都保留」",
                    ));
                }
                std::fs::remove_file(&conflict_abs)
                    .map_err(|e| ErrorDto::new("io", format!("删除副本失败: {e}")))?;
            }
            ConflictResolution::KeepBoth => {
                // 改成不带冲突标记的名字，否则它仍会被当成冲突列出来
                let kept_rel = join(&kept_copy_name(name));
                let kept_abs = folder.local_path.join(&kept_rel);
                if kept_abs.exists() {
                    return Err(ErrorDto::new("exists", format!("{kept_rel} 已存在")));
                }
                std::fs::rename(&conflict_abs, &kept_abs)
                    .map_err(|e| ErrorDto::new("io", format!("改名失败: {e}")))?;
                touched.push(kept_rel);
            }
        }
    }
    // 副本已经不在磁盘上（之前手动删了，或另一台设备已处理）：
    // 视为已处理，只把索引里的记录变成墓碑，让列表里这一项消失

    let rt = {
        let guard = state.runtimes.lock().unwrap();
        guard
            .iter()
            .find(|r| r.session.folder_id == folder.id)
            .cloned()
    };
    if let Some(rt) = rt {
        // 不等监听器：立即更新索引（副本路径不存在 → 墓碑），并推给对端
        for path in touched {
            let _ = rt
                .session
                .on_local_event(FileEvent {
                    path,
                    kind: EventKind::Changed,
                })
                .await;
        }
        rt.flush_pending().await;
    }
    Ok(())
}

/// "两者都保留"时副本的新名字：去掉冲突标记，保留时间与设备信息。
/// `a.sync-conflict-20260101-120000-PC.txt` → `a.kept-20260101-120000-PC.txt`
fn kept_copy_name(conflict_name: &str) -> String {
    conflict_name.replacen(".sync-conflict-", ".kept-", 1)
}

// ---------- 设置 ----------

/// 读取设置。
#[tauri::command]
pub async fn get_settings(state: State<'_, Arc<AppState>>) -> CmdResult<SettingsDto> {
    Ok(state.settings_dto())
}

/// 更新设置（只改提供的字段）。
#[tauri::command]
pub async fn update_settings(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    patch: SettingsPatch,
) -> CmdResult<SettingsDto> {
    let auto_start = patch.auto_start;
    let next = state
        .update_settings(patch)
        .map_err(|e| ErrorDto::new("settings", format!("保存设置失败: {e}")))?;
    state.replace_limits(next.upload_limit_kbps, next.download_limit_kbps);
    if let Some(enabled) = auto_start {
        crate::apply_autostart(&app, enabled);
    }
    Ok(next)
}

/// 更新文件夹共享给哪些设备（空列表 = 所有已配对设备）。
#[tauri::command]
pub async fn set_folder_sharing(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
    device_ids: Vec<String>,
) -> CmdResult<()> {
    if state
        .store
        .list_folders()
        .unwrap_or_default()
        .iter()
        .all(|f| f.id != folder_id)
    {
        return Err(ErrorDto::new("not_found", "文件夹不存在"));
    }
    state
        .store
        .set_folder_shared_with(&folder_id, &device_ids)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    if let Some(rt) = state
        .runtimes
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.session.folder_id == folder_id)
    {
        rt.set_shared_with(device_ids);
    }
    Ok(())
}

/// 打开文件夹内某个文件所在目录（冲突定位用）。
#[tauri::command]
pub async fn open_folder_item(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
    path: String,
) -> CmdResult<()> {
    let folder = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| ErrorDto::new("not_found", "文件夹不存在"))?;
    let abs = resolve_inside_folder(&folder, &path)?;
    if !abs.exists() {
        return Err(ErrorDto::new("not_found", "文件不存在"));
    }
    let dir = if abs.is_dir() {
        abs
    } else {
        abs.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| folder.local_path.clone())
    };
    tauri_plugin_opener::open_path(&dir, None::<&str>)
        .map_err(|e| ErrorDto::new("open_failed", format!("打开目录失败: {e}")))
}

fn resolve_inside_folder(
    folder: &fs_sync_store::Folder,
    path: &str,
) -> Result<std::path::PathBuf, ErrorDto> {
    let rel = fs_sync_core::validate_path(path)
        .map_err(|e| ErrorDto::new("bad_path", format!("路径无效: {e}")))?;
    let base = folder
        .local_path
        .canonicalize()
        .map_err(|e| ErrorDto::new("bad_path", format!("文件夹不存在: {e}")))?;
    let abs = folder.local_path.join(rel);
    let real = abs
        .canonicalize()
        .map_err(|e| ErrorDto::new("not_found", format!("文件不存在: {e}")))?;
    if !real.starts_with(&base) {
        return Err(ErrorDto::new("bad_path", "路径指向同步目录之外"));
    }
    Ok(real)
}

/// 用系统默认程序打开文件夹中的单个文件（冲突对比用）。
#[tauri::command]
pub async fn open_file(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
    path: String,
) -> CmdResult<()> {
    let folder = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| ErrorDto::new("not_found", "文件夹不存在"))?;
    let abs = resolve_inside_folder(&folder, &path)?;
    if !abs.is_file() {
        return Err(ErrorDto::new("not_found", "文件不存在"));
    }
    tauri_plugin_opener::open_path(&abs, None::<&str>)
        .map_err(|e| ErrorDto::new("open_failed", format!("打开文件失败: {e}")))
}

/// 用系统文件管理器打开文件夹。
#[tauri::command]
pub async fn open_folder(state: State<'_, Arc<AppState>>, folder_id: String) -> CmdResult<()> {
    let folder = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| ErrorDto::new("not_found", "文件夹不存在"))?;
    tauri_plugin_opener::open_path(&folder.local_path, None::<&str>)
        .map_err(|e| ErrorDto::new("open_failed", format!("打开文件夹失败: {e}")))
}

/// 打开日志目录。
#[tauri::command]
pub async fn open_log_dir(state: State<'_, Arc<AppState>>) -> CmdResult<String> {
    let dir = state.data_dir.join("logs");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.to_string_lossy().to_string();
    tauri_plugin_opener::open_path(&path, None::<&str>)
        .map_err(|e| ErrorDto::new("open_failed", format!("打开日志目录失败: {e}")))?;
    Ok(path)
}

/// 检测更新（Basic 版本更新服务）。失败时 message 是服务端原文。
#[tauri::command]
pub async fn check_update() -> CmdResult<UpdateDto> {
    crate::updater::check()
        .await
        .map_err(|e| ErrorDto::new("update_check_failed", format!("{e:#}")))
}

/// 下载、校验并安装最新版本。成功时安装器接管，本进程退出，不会返回。
/// 下载进度通过 `update://progress` 事件推送。
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> CmdResult<()> {
    use tauri::Emitter;
    let fail = |e: anyhow::Error| ErrorDto::new("update_install_failed", format!("{e:#}"));
    // 每次都重新取：download_url 只有 1 小时有效
    let remote = crate::updater::fetch_info().await.map_err(fail)?;
    let dto = crate::updater::evaluate(&remote, crate::updater::current_version());
    if !dto.available {
        return Err(ErrorDto::new("update_not_available", "没有可安装的新版本"));
    }
    let progress_app = app.clone();
    let path = crate::updater::download_verified(&remote, &move |downloaded, total| {
        let _ = progress_app.emit("update://progress", UpdateProgressDto { downloaded, total });
    })
    .await
    .map_err(fail)?;
    crate::updater::launch_installer(&path).map_err(fail)?;
    info!("安装器已启动，退出旧版本");
    app.exit(0);
    Ok(())
}

/// 本机信息（调试/诊断用）。
#[tauri::command]
pub async fn get_local_info(state: State<'_, Arc<AppState>>) -> CmdResult<serde_json::Value> {
    Ok(serde_json::json!({
        "device_id": state.device_id,
        "fingerprint": fingerprint_of(&state.device_id),
        "device_name": state.device_name,
        "hostname": hostname(),
        "data_dir": state.data_dir.to_string_lossy(),
        "ssh_port": state.ssh_port,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// 生成并保存诊断包，返回文件路径。
#[tauri::command]
pub async fn export_diagnostics(
    state: State<'_, Arc<AppState>>,
    out_dir: Option<String>,
) -> CmdResult<String> {
    let settings_json =
        serde_json::to_string_pretty(&state.settings_dto()).unwrap_or_else(|_| "{}".into());
    let report = crate::diagnostics::DiagnosticReport::collect(
        &state.data_dir,
        &state.device_name,
        &state.device_id,
        &fingerprint_of(&state.device_id),
        &settings_json,
        &state.store,
    )
    .map_err(|e| ErrorDto::new("diagnostics", format!("收集诊断信息失败: {e:#}")))?;

    let dir = out_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.data_dir.join("diagnostics"));
    let path = report
        .write_to(&dir)
        .map_err(|e| ErrorDto::new("diagnostics", format!("写诊断文件失败: {e:#}")))?;
    info!(path = %path.display(), "诊断包已导出");
    Ok(path.to_string_lossy().to_string())
}

/// 日志目录路径。
#[tauri::command]
pub async fn get_log_dir(state: State<'_, Arc<AppState>>) -> CmdResult<String> {
    Ok(state.data_dir.join("logs").to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conflict_name_with_extension() {
        let (orig, dev) =
            parse_conflict_name("report.sync-conflict-20261027-103005-DESKTOP-T7CO944.docx");
        assert_eq!(orig, "report.docx");
        assert_eq!(dev, "DESKTOP-T7CO944");
    }

    #[test]
    fn parses_conflict_name_without_extension() {
        let (orig, dev) = parse_conflict_name("Makefile.sync-conflict-20261027-103005-ASHU");
        assert_eq!(orig, "Makefile");
        assert_eq!(dev, "ASHU");
    }

    #[test]
    fn kept_copy_name_drops_conflict_marker() {
        let kept = kept_copy_name("k1.sync-conflict-20261030-083719-DESKTOP-T7CO944.txt");
        assert_eq!(kept, "k1.kept-20261030-083719-DESKTOP-T7CO944.txt");
        assert!(!kept.contains(".sync-conflict-"), "否则仍会被列为冲突");
    }

    #[test]
    fn non_conflict_name_passes_through() {
        let (orig, dev) = parse_conflict_name("normal.txt");
        assert_eq!(orig, "normal.txt");
        assert!(dev.is_empty());
    }
}
