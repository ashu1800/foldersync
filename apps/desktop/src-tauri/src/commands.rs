//! Tauri commands：前端唯一入口（提示词 5.3）。
//!
//! 所有命令返回 `Result<T, ErrorDto>`，错误码稳定可本地化。

use std::sync::Arc;

use fs_sync_daemon::{EventKind, FileEvent};
use fs_sync_store::Folder;
use tauri::State;
use tracing::info;

use crate::dto::*;
use crate::state::{AppState, fingerprint_of, hostname};

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 概览：顶部总状态 + 设备/文件夹聚合。
#[tauri::command]
pub async fn get_overview(state: State<'_, Arc<AppState>>) -> CmdResult<OverviewDto> {
    let folders = state.folders().await;
    let devices = state.devices().await;

    let online = devices.iter().filter(|d| d.online).count() as u32;
    let paused_all = state.paused_all.load(std::sync::atomic::Ordering::Relaxed);
    let any_paused = paused_all || folders.iter().any(|f| f.paused);
    let any_error = folders.iter().any(|f| f.status == FolderStatus::Error);
    let remaining: u32 = folders.iter().map(|f| f.pending).sum();

    let status = if any_error {
        OverallStatus::Error
    } else if any_paused {
        OverallStatus::Paused
    } else if remaining > 0 {
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
        endpoint_id: state.endpoint_id.to_string(),
        fingerprint: fingerprint_of(&state.endpoint_id),
        tailscale: ts_desc,
        online_devices: online,
        syncing_files: 0,
        remaining_files: remaining,
        error_count: if any_error { 1 } else { 0 },
    })
}

// ---------- 设备 ----------

/// 已配对设备列表。
#[tauri::command]
pub async fn list_devices(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<DeviceDto>> {
    Ok(state.devices().await)
}

/// 推送单个文件夹的最新状态（提示词 5.3 的 `folder://status`）。
///
/// 命令本身已经返回了新状态，但其它窗口/托盘菜单也可能触发变化，
/// 所以统一用事件广播一次，前端不必重拉整个列表。
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

/// 生成配对码（5 分钟有效，一次性）。
#[tauri::command]
pub async fn create_pairing_code(state: State<'_, Arc<AppState>>) -> CmdResult<PairingCodeDto> {
    // 把当前中继写进配对码，对端不依赖 DNS 发现也能连上
    let relay = state.settings.get().relay_url;
    let code = state.pairing.create_code(relay.as_deref()).await;
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
    let parsed = fs_sync_net::pairing::decode(
        &code,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
    .map_err(|e| ErrorDto::new("bad_code", format!("配对码无效: {e}")))?;
    let peer = parsed
        .endpoint_id
        .parse::<iroh::EndpointId>()
        .map_err(|e| ErrorDto::new("bad_code", format!("配对码中的设备 ID 无效: {e}")))?;

    // 用配对码里的完整地址（含中继）拨号，而不是只给 EndpointId 等 DNS 解析
    let addr = fs_sync_net::pairing::to_endpoint_addr(&parsed)
        .map_err(|e| ErrorDto::new("bad_code", format!("配对码地址无效: {e}")))?;
    let conn = state
        .endpoint
        .connect(addr, fs_sync_net::ALPN)
        .await
        .map_err(|e| ErrorDto::new("connect_failed", format!("连接对端失败: {e}")))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| ErrorDto::new("connect_failed", e.to_string()))?;

    fs_sync_net::frame::write_frame(
        &mut send,
        &fs_sync_net::message::Message::PairingRequest(fs_sync_net::message::PairingRequest {
            token: parsed.token.clone(),
            device_name: state.device_name.clone(),
        }),
    )
    .await
    .map_err(|e| ErrorDto::new("connect_failed", e.to_string()))?;

    let resp = fs_sync_net::frame::read_frame(&mut recv)
        .await
        .map_err(|e| ErrorDto::new("connect_failed", e.to_string()))?;

    match resp {
        fs_sync_net::message::Message::PairingResponse(r) if r.accepted => {
            state
                .trust
                .add(&peer, &r.device_name)
                .map_err(|e| ErrorDto::new("store", e.to_string()))?;
            info!(%peer, name = %r.device_name, "配对成功");
            Ok(DeviceDto {
                id: peer.to_string(),
                name: r.device_name,
                fingerprint: fingerprint_of(&peer),
                online: true,
                conn: ConnKind::Relay,
                rtt_ms: None,
            })
        }
        fs_sync_net::message::Message::PairingResponse(_) => {
            Err(ErrorDto::new("rejected", "对端拒绝了配对请求".to_string()))
        }
        fs_sync_net::message::Message::Error { message, .. } => {
            Err(ErrorDto::new("pairing_failed", message))
        }
        _ => Err(ErrorDto::new("pairing_failed", "对端返回了意外响应")),
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
    if !accept {
        state.pending_pairings.lock().await.remove(&request_id);
    }
    Ok(())
}

/// 待确认的配对请求列表。
#[tauri::command]
pub async fn list_pending_pairings(
    state: State<'_, Arc<AppState>>,
) -> CmdResult<Vec<PendingPairingDto>> {
    let map = state.pending_pairings.lock().await;
    let now = now_secs();
    Ok(map
        .iter()
        .filter(|(_, (_, _, exp))| *exp > now)
        .map(|(id, (name, fp, exp))| PendingPairingDto {
            request_id: id.clone(),
            device_name: name.clone(),
            fingerprint: fp.clone(),
            expires_at: *exp,
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
    let peer = id
        .parse::<iroh::EndpointId>()
        .map_err(|e| ErrorDto::new("bad_id", e.to_string()))?;
    state
        .trust
        .add(&peer, &name)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
    Ok(())
}

/// 取消配对。
#[tauri::command]
pub async fn remove_device(state: State<'_, Arc<AppState>>, id: String) -> CmdResult<()> {
    let peer = id
        .parse::<iroh::EndpointId>()
        .map_err(|e| ErrorDto::new("bad_id", e.to_string()))?;
    state
        .trust
        .remove(&peer)
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

    // ponytail: CLI/桌面初版用文件夹名派生 id（见 docs/decisions.md），
    // 接入邀请流程后改为创建方生成 UUID。
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

    // 写入忽略规则
    if !req.ignore.custom.is_empty() {
        let ignore_path = path.join(".syncignore");
        let _ = std::fs::write(&ignore_path, req.ignore.custom.join("\n"));
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
        })
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;

    state
        .start_folder(folder_id.clone(), path.clone())
        .await
        .map_err(|e| ErrorDto::new("runtime", format!("启动同步失败: {e:#}")))?;

    state
        .log_activity("info", &folder_id, "", "文件夹已加入同步")
        .ok();

    Ok(FolderDto {
        id: folder_id,
        name,
        path: req.path,
        status: FolderStatus::Idle,
        paused: false,
        last_sync_at: None,
        pending: 0,
        file_count: 0,
        shared_with: req.device_ids,
    })
}

/// 移除文件夹同步（不删本地文件）。
#[tauri::command]
pub async fn remove_folder(state: State<'_, Arc<AppState>>, id: String) -> CmdResult<()> {
    state.stop_folder(&id).await;
    state
        .store
        .remove_folder(&id)
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;
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
pub async fn pause_all(state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    state
        .paused_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let ids: Vec<String> = state
        .store
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.id)
        .collect();
    for id in ids {
        state.stop_folder(&id).await;
    }
    Ok(())
}

/// 恢复全部。
#[tauri::command]
pub async fn resume_all(state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    state
        .paused_all
        .store(false, std::sync::atomic::Ordering::Relaxed);
    for f in state.store.list_folders().unwrap_or_default() {
        if !f.paused {
            let _ = state.start_folder(f.id, f.local_path).await;
        }
    }
    Ok(())
}

/// 文件夹文件树（只读浏览）。
#[tauri::command]
pub async fn get_folder_tree(
    state: State<'_, Arc<AppState>>,
    folder_id: String,
    subpath: Option<String>,
) -> CmdResult<Vec<TreeNodeDto>> {
    // 校验文件夹确实存在（防止前端传错 id 时返回空列表误导用户）
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

    let custom = std::fs::read_to_string(folder.local_path.join(".syncignore"))
        .map(|t| t.lines().map(|s| s.to_string()).collect())
        .unwrap_or_default();

    Ok(IgnoreRulesDto {
        use_defaults: true,
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
    std::fs::write(
        folder.local_path.join(".syncignore"),
        rules.custom.join("\n"),
    )
    .map_err(|e| ErrorDto::new("io", format!("写入忽略规则失败: {e}")))?;
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
        .list_activity(filter.folder_id.as_deref(), filter.limit.unwrap_or(200))
        .map_err(|e| ErrorDto::new("store", e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|a| ActivityDto {
            id: a.id,
            at: a.at,
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
            size: 0,
            message: a.detail,
        })
        .collect())
}

// ---------- 冲突 ----------

/// 收集冲突条目（命令与事件共用）。
///
/// 独立出来是因为 `conflict://new` 事件也要用同一套解析逻辑，
/// 不能在事件路径里复制一份。
pub fn conflict_dtos_for(state: &AppState, folder_id: Option<String>) -> Vec<ConflictDto> {
    let mut out = Vec::new();
    for f in state.store.list_folders().unwrap_or_default() {
        if let Some(ref want) = folder_id
            && &f.id != want
        {
            continue;
        }
        let entries = state.store.list_entries(&f.id).unwrap_or_default();
        for e in entries
            .iter()
            .filter(|e| e.path.contains(".sync-conflict-"))
        {
            // 从文件名解析原文件与来源设备
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

/// 列出所有冲突副本文件（提示词 5.2）。
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
    // rest = 20260101-120000-设备[.ext]
    let (ext, body) = match rest.rsplit_once('.') {
        Some((body, ext)) => (format!(".{ext}"), body),
        None => (String::new(), rest),
    };
    // body = 20260101-120000-设备名
    let device = body.splitn(3, '-').nth(2).unwrap_or("").to_string();
    (format!("{stem}{ext}"), device)
}

/// 解决冲突（保留其一或两者）。
#[tauri::command]
pub async fn resolve_conflict(
    state: State<'_, Arc<AppState>>,
    conflict_id: String,
    resolution: ConflictResolution,
) -> CmdResult<()> {
    let mut found = None;
    for f in state.store.list_folders().unwrap_or_default() {
        let entries = state.store.list_entries(&f.id).unwrap_or_default();
        if entries.iter().any(|e| e.path == conflict_id) {
            found = Some(f);
            break;
        }
    }
    let folder = found.ok_or_else(|| ErrorDto::new("not_found", "冲突文件不存在"))?;

    let name = conflict_id.rsplit('/').next().unwrap_or(&conflict_id);
    let (original_name, _) = parse_conflict_name(name);
    let original_rel = match conflict_id.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{original_name}"),
        None => original_name,
    };
    let conflict_abs = folder.local_path.join(&conflict_id);
    let original_abs = folder.local_path.join(&original_rel);

    match resolution {
        ConflictResolution::KeepConflict => {
            std::fs::rename(&conflict_abs, &original_abs)
                .map_err(|e| ErrorDto::new("io", format!("替换失败: {e}")))?;
        }
        ConflictResolution::KeepOriginal => {
            std::fs::remove_file(&conflict_abs)
                .map_err(|e| ErrorDto::new("io", format!("删除副本失败: {e}")))?;
        }
        ConflictResolution::KeepBoth => {
            // 副本换个后缀留档，原文件不动
            let archived = conflict_abs.with_extension("resolved.bak");
            std::fs::rename(&conflict_abs, &archived)
                .map_err(|e| ErrorDto::new("io", format!("存档失败: {e}")))?;
        }
    }

    // 触发重新扫描，让索引跟上磁盘现实
    if let Some(rt) = state.runtimes.lock().await.get(&folder.id).cloned() {
        let _ = rt
            .session
            .on_local_event(FileEvent {
                path: original_rel,
                kind: EventKind::Changed,
            })
            .await;
    }
    Ok(())
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
    state: State<'_, Arc<AppState>>,
    patch: SettingsPatch,
) -> CmdResult<SettingsDto> {
    let next = state
        .update_settings(patch)
        .map_err(|e| ErrorDto::new("settings", format!("保存设置失败: {e}")))?;
    // 限速改动立即生效（重建限速器，无需重启）
    state.replace_limits(next.upload_limit_kbps, next.download_limit_kbps);
    Ok(next)
}

/// 打开日志目录（调用系统文件管理器）。
#[tauri::command]
pub async fn open_log_dir(state: State<'_, Arc<AppState>>) -> CmdResult<String> {
    Ok(state.data_dir.join("logs").to_string_lossy().to_string())
}

/// 本机信息（调试/诊断用）。
#[tauri::command]
pub async fn get_local_info(state: State<'_, Arc<AppState>>) -> CmdResult<serde_json::Value> {
    Ok(serde_json::json!({
        "endpoint_id": state.endpoint_id.to_string(),
        "fingerprint": fingerprint_of(&state.endpoint_id),
        "device_name": state.device_name,
        "hostname": hostname(),
        "data_dir": state.data_dir.to_string_lossy(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
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
    fn non_conflict_name_passes_through() {
        let (orig, dev) = parse_conflict_name("normal.txt");
        assert_eq!(orig, "normal.txt");
        assert!(dev.is_empty());
    }
}

/// 生成并保存诊断包（M6），返回文件路径。
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
        &state.endpoint_id.to_string(),
        &fingerprint_of(&state.endpoint_id),
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

/// 日志目录路径（前端用来打开或提示用户）。
#[tauri::command]
pub async fn get_log_dir(state: State<'_, Arc<AppState>>) -> CmdResult<String> {
    Ok(state.data_dir.join("logs").to_string_lossy().to_string())
}
