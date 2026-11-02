//! 中继方案同步：双方各自连中继房间，用 SFTP 交换索引与 blob。
//!
//! 房间目录布局：
//! ```text
//! devices/<device-id>/index.json
//! devices/<device-id>/blobs/<blake3>
//! ```
//! 每个设备只写自己的目录、读对端目录。索引是原子写的，blob 按 hash 寻址。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use fs_sync_core::FileEntry;
use fs_sync_net::blobstore::BlobStore;
use fs_sync_net::message::PROTOCOL_VERSION;
use fs_sync_net::relay::RelaySftp;
use fs_sync_store::Device;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::runtime::{FolderRuntime, PeerLoopConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayIndex {
    protocol_version: u16,
    device_id: String,
    device_name: String,
    folders: Vec<RelayFolder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayFolder {
    folder_id: String,
    /// 共享文件夹名；旧版本索引没有该字段时回退为按 folder_id 生成的安全名称
    #[serde(default)]
    folder_name: String,
    entries: Vec<FileEntry>,
}

/// 解析中继地址列表：支持逗号分隔多个（`tcp://a:9443,tcp://b:9443`），
/// 按序尝试做故障转移。设置里只填一个时行为不变。
fn relay_url_candidates(cfg: &PeerLoopConfig, peer: &Device) -> Vec<String> {
    let primary = cfg.relay_url.clone().or_else(|| peer.relay_url.clone());
    match primary {
        Some(url) => url
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    }
}

/// 中继同步主循环。断线立即返回，由上层监督任务退避重连。
pub async fn relay_sync_round(
    cfg: &PeerLoopConfig,
    peer: &Device,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> Result<()> {
    let room = peer.relay_room.clone().context("没有中继房间信息")?;
    let secret = peer.relay_secret.clone().context("没有中继房间口令")?;
    let candidates = relay_url_candidates(cfg, peer);
    anyhow::ensure!(!candidates.is_empty(), "没有配置中继服务器地址");

    // 按序尝试所有中继地址：第一个成功者优先，坏了自动切下一个（故障转移）。
    // host key 固定校验按地址分别记录（每个中继一份 TOFU）。
    let mut last_err = None;
    let mut connected: Option<(String, Arc<Mutex<RelaySftp>>)> = None;
    for relay_url in &candidates {
        let expected = cfg.relay_host_keys.get(relay_url);
        match RelaySftp::connect(relay_url, &room, &secret, expected.as_deref()).await {
            Ok(relay) => {
                let actual = relay.host_key().to_string();
                if expected.as_deref() != Some(actual.as_str()) {
                    cfg.relay_host_keys.set(relay_url, &actual);
                    if expected.is_some() {
                        warn!(relay = %relay_url, "中继 host key 已变化，已更新固定记录");
                    } else {
                        info!(relay = %relay_url, "已固定中继 host key（TOFU 首连）");
                    }
                }
                connected = Some((relay_url.clone(), Arc::new(Mutex::new(relay))));
                break;
            }
            Err(e) => {
                warn!(relay = %relay_url, "连接中继失败，尝试下一个: {e:#}");
                last_err = Some(e);
            }
        }
    }
    let (relay_url, relay) = connected.with_context(|| {
        last_err
            .map(|e| e.context("所有中继地址均连接失败"))
            .unwrap()
    })?;
    info!(peer = %peer.name, relay = %relay_url, "中继同步会话已建立");
    for rt in crate::runtime::all_runtimes(runtimes) {
        if rt.shared_with_peer(&peer.id)
            && rt
                .last_error()
                .map(|e| !e.contains("文件夹邀请被拒绝"))
                .unwrap_or(false)
        {
            rt.set_last_error(None);
        }
    }

    let my_index_path = format!("devices/{}/index.json", device_dir(&cfg.device_id));
    let peer_index_path = format!("devices/{}/index.json", device_dir(&peer.id));
    let mut last_uploaded = String::new();
    let mut last_applied = String::new();
    // 已确认存在于中继的 blob：跨重连保留（进程级共享，按中继地址分键），
    // 重连后不再对每个文件做 SFTP exists()。本地会话集合作"本轮有疑问的"暂存。
    let mut uploaded: std::collections::HashSet<String> = cfg
        .relay_uploaded_blobs
        .as_ref()
        .and_then(|m| m.lock().ok()?.get(&relay_url).cloned())
        .unwrap_or_default();
    let mut last_peer_stamp: Option<(u64, std::time::SystemTime)> = None;
    // 各文件夹索引版本号快照：版本没变就跳过本轮的 snapshot + 序列化 +
    // 全量上传。10 万文件的文件夹每秒全量重建是不可接受的。
    let mut last_index_versions: HashMap<String, u64> = HashMap::new();

    loop {
        // 1. 生成本机索引并上传缺失的 blob（先内容后索引，避免对端读到索引却拉不到内容）
        // 快速路径：所有共享文件夹的索引版本都没变 → 跳过重建与上传，
        // 直接进入对端索引检查。新落盘的 blob 由对端按 hash 自行从
        // 「拉取失败重试」兜住（我们的索引里 hash 已存在）。
        let index_unchanged = {
            let rts = crate::runtime::all_runtimes(runtimes);
            !last_uploaded.is_empty()
                && rts
                    .iter()
                    .filter(|rt| rt.shared_with_peer(&peer.id))
                    .all(|rt| {
                        last_index_versions.get(&rt.session.folder_id)
                            == Some(&rt.session.index_version())
                    })
        };
        if index_unchanged {
            // 仍要看对端索引有没有变（对端可能推送了变更）
            let peer_stamp = relay.lock().await.stamp(&peer_index_path).await;
            if peer_stamp == last_peer_stamp {
                let entries = last_uploaded.matches("\"path\"").count();
                let interval = if entries > 5000 { 5 } else { 1 };
                debug!(peer = %peer.name, "中继轮询（索引无变化，跳过重建）");
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                continue;
            }
            // 对端变了：走完整轮（不重建我们的索引也无妨，但逻辑简单起见重建）
            last_uploaded.clear();
        }
        let my_index = {
            let idx = build_index(cfg, runtimes, &peer.id).await;
            let rts = crate::runtime::all_runtimes(runtimes);
            for rt in rts {
                if rt.shared_with_peer(&peer.id) {
                    last_index_versions
                        .insert(rt.session.folder_id.clone(), rt.session.index_version());
                }
            }
            idx
        };
        let my_json = serde_json::to_string(&my_index)?;
        for folder in &my_index.folders {
            for entry in folder.entries.iter().filter(|e| !e.is_dir && !e.deleted) {
                if uploaded.contains(&entry.hash) {
                    continue;
                }
                ensure_blob(runtimes, &folder.folder_id, entry).await;
                let blob_path = format!(
                    "devices/{}/blobs/{}",
                    device_dir(&cfg.device_id),
                    entry.hash
                );
                let handle = relay.lock().await;
                let mut ready = handle.exists(&blob_path).await;
                if !ready {
                    let store = find_runtime(runtimes, &folder.folder_id)
                        .map(|rt| rt.session.blobs.clone());
                    if let Some(store) = store {
                        let local = store.blob_path(&entry.hash);
                        if local.exists() {
                            handle
                                .upload(&blob_path, &local)
                                .await
                                .with_context(|| format!("上传 blob 失败: {}", entry.hash))?;
                            ready = true;
                            if let Some(rt) = find_runtime(runtimes, &folder.folder_id) {
                                let throttle = rt
                                    .session
                                    .throttle
                                    .read()
                                    .map(|t| t.clone())
                                    .unwrap_or_default();
                                throttle.consume_upload(entry.size).await;
                            }
                        }
                    }
                }
                if !ready {
                    // 本地内容还没生成好或 runtime 暂缺，下轮重试，别标记为已上传
                    continue;
                }
                uploaded.insert(entry.hash.clone());
            }
        }
        if my_json != last_uploaded {
            if !last_uploaded.is_empty() {
                log_index_diff(cfg, runtimes, &last_uploaded, &my_index).await;
            }
            relay
                .lock()
                .await
                .write_text(&my_index_path, &my_json)
                .await?;
            last_uploaded = my_json;
            // 中继每轮上传全量索引，逐条 outbox 已无意义；只跳过当前中继 peer 的游标，
            // 不能 clear_all 把其他 SSH 对端的待发送事件一起清掉。
            for rt in crate::runtime::all_runtimes(runtimes) {
                rt.outbox.lock().await.clear_for_peer(&peer.id);
            }
        }

        // 2. 读对端索引；变了就应用（先看 (size, mtime)，避免每秒下载大 JSON）
        let peer_stamp = relay.lock().await.stamp(&peer_index_path).await;
        if peer_stamp != last_peer_stamp {
            last_peer_stamp = peer_stamp;
            let peer_json = relay.lock().await.read_text(&peer_index_path).await?;
            if let Some(text) = peer_json
                && text != last_applied
            {
                match serde_json::from_str::<RelayIndex>(&text) {
                    Ok(index) => {
                        if index.protocol_version != PROTOCOL_VERSION {
                            warn!(peer = %peer.name, "中继协议版本不一致");
                        } else {
                            for folder in index.folders {
                                if find_runtime(runtimes, &folder.folder_id).is_none()
                                    && !ensure_peer_folder(cfg, peer, &folder, runtimes).await
                                {
                                    continue;
                                }
                                let Some(rt) = find_runtime(runtimes, &folder.folder_id) else {
                                    continue;
                                };
                                // 未分享给对端的文件夹不参与同步
                                if !rt.shared_with_peer(&peer.id) {
                                    continue;
                                }
                                let remote: HashMap<String, FileEntry> = folder
                                    .entries
                                    .into_iter()
                                    .map(|e| (e.path.clone(), e))
                                    .collect();
                                let relay = relay.clone();
                                let blobs = rt.session.blobs.clone();
                                let peer_dir = device_dir(&peer.id);
                                let puller = move |hash: String| {
                                    let relay = relay.clone();
                                    let blobs = blobs.clone();
                                    let peer_dir = peer_dir.clone();
                                    async move { pull_blob(&relay, &peer_dir, &hash, &blobs).await }
                                };
                                let stats = {
                                    rt.begin_transfer();
                                    let result = rt.session.apply_remote(&remote, puller).await;
                                    rt.end_transfer();
                                    result
                                }?;
                                // 中继每轮上传全量索引，合并结果已包含在内；这里只是别让 pending 堆积
                                rt.flush_pending().await;
                                if stats.failed > 0 {
                                    let msg = format!(
                                        "中继同步有 {} 个文件失败，将自动重试",
                                        stats.failed
                                    );
                                    if rt.last_error().as_deref() != Some(msg.as_str()) {
                                        rt.set_last_error(Some(msg.clone()));
                                        let _ = rt.session.engine.store.log_activity(
                                            "error",
                                            &folder.folder_id,
                                            "",
                                            &msg,
                                        );
                                    }
                                } else if rt
                                    .last_error()
                                    .map(|e| e.contains("中继同步有"))
                                    .unwrap_or(false)
                                {
                                    rt.set_last_error(None);
                                }
                                if stats.pulled + stats.deleted + stats.conflicts > 0 {
                                    info!(
                                        folder = %folder.folder_id,
                                        pulled = stats.pulled,
                                        deleted = stats.deleted,
                                        conflicts = stats.conflicts,
                                        "已应用中继增量"
                                    );
                                }
                                // 有失败时**不**记录 last_applied：下轮 stamp 比较即使
                                // 相同，text != last_applied 仍成立，会重新 diff——
                                // 已成功的文件 diff 判无变化直接跳过，只重拉失败的。
                                // 之前无条件记录，对端索引不变时失败文件永远不重试，
                                // 与活动日志里"将自动重试"的承诺自相矛盾。
                                if stats.failed == 0 {
                                    last_applied = text.clone();
                                }
                            }
                        }
                    }
                    Err(e) => warn!(peer = %peer.name, "解析对端索引失败: {e}"),
                }
            }
        }

        debug!(peer = %peer.name, "中继同步轮询");
        // 写回进程级已上传缓存：下轮重连不用重新 stat
        if let Some(shared) = &cfg.relay_uploaded_blobs
            && let Ok(mut map) = shared.lock()
        {
            map.insert(relay_url.clone(), uploaded.clone());
        }
        // 大文件夹降低轮询频率：每轮仍要序列化全量索引（分页协议前的上限）
        let entries: usize = my_index.folders.iter().map(|f| f.entries.len()).sum();
        let interval = if entries > 5000 { 5 } else { 1 };
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

async fn build_index(
    cfg: &PeerLoopConfig,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    peer_id: &str,
) -> RelayIndex {
    // 实时读取：会话期间新增的文件夹下一轮就会出现在索引里
    let rts = crate::runtime::all_runtimes(runtimes);
    let mut folders = Vec::new();
    for rt in rts {
        if !rt.shared_with_peer(peer_id) {
            continue;
        }
        // **必须排序**：HashMap 遍历顺序不稳定（RandomState），不排序的话
        // 同一份索引每秒序列化出的 JSON 字节序都不同——"索引变了"的字符串
        // 比较恒真，两端都会每轮重传/重算全量 diff。排序后字节级确定。
        let mut entries: Vec<FileEntry> = rt.session.snapshot().await.into_values().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        folders.push(RelayFolder {
            folder_id: rt.session.folder_id.clone(),
            folder_name: rt
                .session
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&rt.session.folder_id)
                .to_string(),
            entries,
        });
    }
    RelayIndex {
        protocol_version: PROTOCOL_VERSION,
        device_id: cfg.device_id.clone(),
        device_name: cfg.device_name.clone(),
        folders,
    }
}

/// 旧版中继索引没有 `folder_name`，用 folder_id 尾部生成稳定、安全的显示名。
fn fallback_folder_name(folder_id: &str) -> String {
    let suffix: String = folder_id
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if suffix.is_empty() {
        "共享文件夹".to_string()
    } else {
        format!("共享文件夹-{suffix}")
    }
}

/// 中继索引里出现本机没有的文件夹时，挂起为待确认邀请（由用户在界面接受/拒绝）。
/// 已存在同 id 或已拒绝过的保持原语义：不自动恢复、不重复提示。
async fn ensure_peer_folder(
    cfg: &PeerLoopConfig,
    peer: &Device,
    folder: &RelayFolder,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> bool {
    if find_runtime(runtimes, &folder.folder_id).is_some() {
        return true;
    }
    let name = if folder.folder_name.trim().is_empty() {
        fallback_folder_name(&folder.folder_id)
    } else {
        folder.folder_name.trim().to_string()
    };
    if !crate::ssh_server::is_safe_folder_name(&name) {
        warn!(peer = %peer.name, folder = %folder.folder_id, "拒绝非法中继共享文件夹名");
        return false;
    }
    if cfg
        .store
        .is_folder_invite_declined(&folder.folder_id)
        .unwrap_or(true)
    {
        return false;
    }
    if cfg.paused_all.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let existing = cfg.store.list_folders().unwrap_or_default();
    if existing.iter().any(|f| f.id == folder.folder_id) {
        // 已存在但 runtime 没起来，通常是用户暂停了：不自动恢复。
        return false;
    }
    // 新文件夹：挂起待确认，不再自动在同步根目录建目录。
    // 恒回 true：索引里这条让后续流程自然跳过（找不到 runtime 就 continue），
    // 回 false 与 true 在调用点效果相同，但 true 语义是"已处理"。
    let received_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let inv = fs_sync_store::PendingFolderInvite {
        folder_id: folder.folder_id.clone(),
        folder_name: name.clone(),
        from_device: peer.name.clone(),
        received_at,
    };
    if let Err(e) = cfg.store.upsert_pending_folder_invite(&inv) {
        warn!(peer = %peer.name, folder = %folder.folder_id, "记录待确认中继邀请失败: {e}");
        return false;
    }
    if let Some(cb) = cfg.on_invite.as_ref() {
        cb(inv.clone());
    }
    info!(
        peer = %peer.name,
        folder = %name,
        "中继共享邀请已挂起，等待用户确认"
    );
    true
}

async fn ensure_blob(
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    folder_id: &str,
    entry: &FileEntry,
) {
    if let Some(rt) = find_runtime(runtimes, folder_id)
        && let Err(e) = rt.session.engine.ensure_blob(entry).await
    {
        warn!(path = %entry.path, "中继同步加入 blob 失败: {e}");
    }
}

/// 对比上一轮索引，把本轮新增/修改/删除的文件记入活动日志。
/// 只在索引真正变化时调用，避免每秒轮询刷日志。
async fn log_index_diff(
    _cfg: &PeerLoopConfig,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    previous_json: &str,
    current: &RelayIndex,
) {
    let Ok(previous) = serde_json::from_str::<RelayIndex>(previous_json) else {
        return;
    };
    let previous: HashMap<(&str, &str), &FileEntry> = previous
        .folders
        .iter()
        .flat_map(|f| {
            f.entries
                .iter()
                .map(move |e| ((f.folder_id.as_str(), e.path.as_str()), e))
        })
        .collect();

    for folder in &current.folders {
        let Some(rt) = find_runtime(runtimes, &folder.folder_id) else {
            continue;
        };
        for entry in &folder.entries {
            if entry.is_dir {
                continue;
            }
            let old = previous.get(&(folder.folder_id.as_str(), entry.path.as_str()));
            let changed = match old {
                None => !entry.deleted,
                Some(old) => old.hash != entry.hash || old.deleted != entry.deleted,
            };
            if !changed {
                continue;
            }
            let (kind, detail) = if entry.deleted {
                ("delete", "已同步删除到对端")
            } else {
                ("upload", "已推送到对端")
            };
            let _ = rt.session.engine.store.log_activity_sized(
                kind,
                &folder.folder_id,
                &entry.path,
                entry.size as i64,
                detail,
            );
        }
    }
}

fn find_runtime(
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    folder_id: &str,
) -> Option<Arc<FolderRuntime>> {
    runtimes
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.session.folder_id == folder_id)
        .cloned()
}

/// 设备 ID（`SHA256:.../...`）不能直接做目录名：取 blake3 前 16 字节 hex。
fn device_dir(id: &str) -> String {
    blake3::hash(id.as_bytes()).to_hex()[..32].to_string()
}

/// 从对端设备目录下载 blob 到本地库。
async fn pull_blob(
    relay: &Arc<Mutex<RelaySftp>>,
    peer_dir: &str,
    hash: &str,
    store: &BlobStore,
) -> Result<()> {
    if store.has(hash).await {
        return Ok(());
    }
    anyhow::ensure!(
        fs_sync_net::blobstore::is_valid_hash(hash),
        "hash 格式无效（需 64 位 hex）"
    );
    let tmp = store
        .root()
        .join(format!(".relay-download-{}-{hash}", std::process::id()));
    {
        let relay = relay.lock().await;
        relay
            .download_to(&format!("devices/{peer_dir}/blobs/{hash}"), &tmp)
            .await
            .with_context(|| format!("从中继拉取 blob 失败: {hash}"))?;
    }
    store.adopt_downloaded(&tmp, hash).await.inspect_err(|_| {
        // 校验失败说明续传基础已坏：删掉临时文件，下次从 0 开始。
        // 传输中途失败时 tmp 保留，下一轮 pull_blob 从断点续传。
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_invite_pends_for_user_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_path_buf();
        let identity = fs_sync_net::identity::SshIdentity::load_or_create(&data).unwrap();
        let store = Arc::new(fs_sync_store::Store::open_in_memory().unwrap());
        let blobs = Arc::new(BlobStore::open(data.join("blobs")).unwrap());
        let cfg = PeerLoopConfig {
            identity: identity.clone(),
            device_id: identity.id(),
            device_name: "node-b".into(),
            ssh_port: 2222,
            mode: fs_sync_net::conn::NetworkMode::Relay,
            relay_url: None,
            relay_host_keys: crate::runtime::test_relay_host_keys(),
            relay_uploaded_blobs: None,
            store: store.clone(),
            blobs,
            sync_root: data.join("FolderSync"),
            paused_all: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            on_invite: None,
        };
        let peer = Device {
            id: "peer-a".into(),
            name: "node-a".into(),
            paired_at: 0,
            ssh_public_key: String::new(),
            host_key: String::new(),
            username: "foldersync".into(),
            addresses: Vec::new(),
            port: 2222,
            relay_url: None,
            relay_room: None,
            relay_secret: None,
        };
        let runtimes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let folder = RelayFolder {
            folder_id: "folder-shared".into(),
            folder_name: "Shared".into(),
            entries: Vec::new(),
        };
        // 新邀请：挂起为待确认，不建目录、不建文件夹记录
        assert!(ensure_peer_folder(&cfg, &peer, &folder, &runtimes).await);
        assert!(store.list_folders().unwrap().is_empty());
        assert!(!data.join("FolderSync/Shared").exists());
        let pending = store.list_pending_folder_invites().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].folder_id, "folder-shared");
        assert_eq!(pending[0].folder_name, "Shared");
        assert_eq!(pending[0].from_device, "node-a");

        // 第二轮：同一邀请只更新时间戳，不产生重复行
        assert!(ensure_peer_folder(&cfg, &peer, &folder, &runtimes).await);
        assert_eq!(store.list_pending_folder_invites().unwrap().len(), 1);

        // 用户拒绝后：不再挂起
        store
            .upsert_declined_folder_invite(&folder.folder_id)
            .unwrap();
        assert!(!ensure_peer_folder(&cfg, &peer, &folder, &runtimes).await);
        assert_eq!(store.list_pending_folder_invites().unwrap().len(), 1);

        // 旧版索引没有 folder_name：按 folder_id 生成安全回退名
        let legacy = RelayFolder {
            folder_id: "folder-legacy".into(),
            folder_name: String::new(),
            entries: Vec::new(),
        };
        assert!(ensure_peer_folder(&cfg, &peer, &legacy, &runtimes).await);
        let pending = store.list_pending_folder_invites().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(
            pending
                .iter()
                .any(|p| p.folder_name == fallback_folder_name("folder-legacy"))
        );
    }

    #[test]
    fn old_index_without_folder_name_gets_safe_fallback() {
        let name = fallback_folder_name("folder-abcd/ef12");
        assert_eq!(name, "共享文件夹-bcdef12");
        assert!(crate::ssh_server::is_safe_folder_name(&name));
    }

    #[test]
    fn relay_index_roundtrip() {
        let index = RelayIndex {
            protocol_version: PROTOCOL_VERSION,
            device_id: "a".into(),
            device_name: "home".into(),
            folders: vec![RelayFolder {
                folder_id: "f1".into(),
                folder_name: "docs".into(),
                entries: Vec::new(),
            }],
        };
        let text = serde_json::to_string(&index).unwrap();
        let back: RelayIndex = serde_json::from_str(&text).unwrap();
        assert_eq!(back.device_id, "a");
        assert_eq!(back.folders.len(), 1);
    }
}
