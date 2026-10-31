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
    /// 共享文件夹名；旧版本索引没有该字段，按空处理（不自动建目录）
    #[serde(default)]
    folder_name: String,
    entries: Vec<FileEntry>,
}

/// 中继同步主循环。断线立即返回，由上层监督任务退避重连。
pub async fn relay_sync_round(
    cfg: &PeerLoopConfig,
    peer: &Device,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> Result<()> {
    let relay_url = cfg
        .relay_url
        .clone()
        .or_else(|| peer.relay_url.clone())
        .context("没有配置中继服务器地址")?;
    let room = peer.relay_room.clone().context("没有中继房间信息")?;
    let secret = peer.relay_secret.clone().context("没有中继房间口令")?;

    let relay = Arc::new(Mutex::new(
        RelaySftp::connect(&relay_url, &room, &secret, None)
            .await
            .context("连接中继失败")?,
    ));
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
    // 已确认存在于中继的 blob：避免每轮对每个文件做一次 SFTP stat
    let mut uploaded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_peer_stamp: Option<(u64, std::time::SystemTime)> = None;

    loop {
        // 1. 生成本机索引并上传缺失的 blob（先内容后索引，避免对端读到索引却拉不到内容）
        let my_index = build_index(cfg, runtimes, &peer.id).await;
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
                            }
                            last_applied = text;
                        }
                    }
                    Err(e) => warn!(peer = %peer.name, "解析对端索引失败: {e}"),
                }
            }
        }

        debug!(peer = %peer.name, "中继同步轮询");
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
        let entries: Vec<FileEntry> = rt.session.snapshot().await.into_values().collect();
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

/// 中继索引里出现本机没有的文件夹时，按邀请自动创建（仅新建空目录，绝不覆盖已有内容）。
async fn ensure_peer_folder(
    cfg: &PeerLoopConfig,
    peer: &Device,
    folder: &RelayFolder,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> bool {
    if find_runtime(runtimes, &folder.folder_id).is_some() {
        return true;
    }
    let name = folder.folder_name.trim();
    if !crate::ssh_server::is_safe_folder_name(name) {
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
    let target = cfg.sync_root.join(name);
    if existing.iter().any(|f| f.local_path == target) {
        warn!(path = %target.display(), "中继邀请路径已被其他文件夹占用");
        return false;
    }
    if std::fs::create_dir_all(&cfg.sync_root).is_err() {
        return false;
    }
    let Ok(root_canon) = std::fs::canonicalize(&cfg.sync_root) else {
        return false;
    };
    if target.exists() {
        let empty = std::fs::read_dir(&target)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false);
        if !empty {
            warn!(peer = %peer.name, path = %target.display(), "拒绝中继邀请复用非空目录");
            return false;
        }
    }
    if std::fs::create_dir_all(&target).is_err() {
        return false;
    }
    let Ok(canon_target) = std::fs::canonicalize(&target) else {
        return false;
    };
    if !canon_target.starts_with(&root_canon) || canon_target == root_canon {
        return false;
    }
    if cfg
        .store
        .add_folder_from_invite(&fs_sync_store::Folder {
            id: folder.folder_id.clone(),
            name: name.to_string(),
            local_path: target.clone(),
            paused: false,
            tombstone_days: 30,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            shared_with: vec![peer.id.clone()],
        })
        .is_err()
    {
        return false;
    }
    let created_path = target.clone();
    let rt = match FolderRuntime::start(
        folder.folder_id.clone(),
        target,
        cfg.device_id.clone(),
        cfg.store.clone(),
        cfg.blobs.clone(),
        400,
    )
    .await
    {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            let _ = cfg.store.discard_folder_record(&folder.folder_id);
            warn!(peer = %peer.name, "中继邀请启动同步失败: {e}");
            return false;
        }
    };
    rt.set_shared_with(vec![peer.id.clone()]);
    let pushed = {
        let mut guard = runtimes.lock().unwrap();
        let valid = !cfg.paused_all.load(std::sync::atomic::Ordering::Relaxed)
            && cfg
                .store
                .list_folders()
                .map(|v| {
                    v.iter().any(|f| {
                        f.id == folder.folder_id && !f.paused && f.local_path == created_path
                    })
                })
                .unwrap_or(false);
        if valid {
            guard.push(rt.clone());
        }
        valid
    };
    if !pushed {
        rt.cancel();
        // 若用户只是暂停/重定位，保留文件夹记录；只有记录确实不存在才回滚。
        let still_exists = cfg
            .store
            .list_folders()
            .map(|v| v.iter().any(|f| f.id == folder.folder_id))
            .unwrap_or(true);
        if !still_exists {
            let _ = cfg.store.discard_folder_record(&folder.folder_id);
        }
        return false;
    }
    let _ = cfg.store.log_activity(
        "info",
        &folder.folder_id,
        "",
        &format!("已接受来自「{}」的共享文件夹（中继）", peer.name),
    );
    info!(peer = %peer.name, folder = %folder.folder_name, "已接受中继共享文件夹");
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
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_invite_creates_folder_once() {
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
            store: store.clone(),
            blobs,
            sync_root: data.join("FolderSync"),
            paused_all: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        assert!(ensure_peer_folder(&cfg, &peer, &folder, &runtimes).await);
        assert_eq!(store.list_folders().unwrap().len(), 1);
        assert_eq!(
            store.list_folders().unwrap()[0].local_path,
            data.join("FolderSync/Shared")
        );
        // 第二轮不应重复创建
        assert!(ensure_peer_folder(&cfg, &peer, &folder, &runtimes).await);
        assert_eq!(store.list_folders().unwrap().len(), 1);
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
