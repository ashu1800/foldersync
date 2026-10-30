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

    let my_index_path = format!("devices/{}/index.json", device_dir(&cfg.device_id));
    let peer_index_path = format!("devices/{}/index.json", device_dir(&peer.id));
    let mut last_uploaded = String::new();
    let mut last_applied = String::new();

    loop {
        // 1. 生成本机索引并上传缺失的 blob（先内容后索引，避免对端读到索引却拉不到内容）
        let my_index = build_index(cfg, runtimes).await;
        let my_json = serde_json::to_string(&my_index)?;
        for folder in &my_index.folders {
            for entry in folder.entries.iter().filter(|e| !e.is_dir && !e.deleted) {
                ensure_blob(runtimes, &folder.folder_id, entry).await;
                let blob_path = format!(
                    "devices/{}/blobs/{}",
                    device_dir(&cfg.device_id),
                    entry.hash
                );
                let handle = relay.lock().await;
                if !handle.exists(&blob_path).await {
                    let store = find_runtime(runtimes, &folder.folder_id)
                        .map(|rt| rt.session.blobs.clone());
                    if let Some(store) = store {
                        let local = store.blob_path(&entry.hash);
                        if local.exists() {
                            handle
                                .upload(&blob_path, &local)
                                .await
                                .with_context(|| format!("上传 blob 失败: {}", entry.hash))?;
                        }
                    }
                }
            }
        }
        if my_json != last_uploaded {
            relay
                .lock()
                .await
                .write_text(&my_index_path, &my_json)
                .await?;
            last_uploaded = my_json;
        }

        // 2. 读对端索引；变了就应用
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
                            let Some(rt) = find_runtime(runtimes, &folder.folder_id) else {
                                continue;
                            };
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
                            let stats = rt.session.apply_remote(&remote, puller).await?;
                            // 中继每轮上传全量索引，合并结果已包含在内；这里只是别让 pending 堆积
                            rt.flush_pending().await;
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

        debug!(peer = %peer.name, "中继同步轮询");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn build_index(
    cfg: &PeerLoopConfig,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> RelayIndex {
    let rts: Vec<Arc<FolderRuntime>> = {
        let guard = runtimes.lock().unwrap();
        guard
            .iter()
            .filter(|r| cfg.folder_ids.contains(&r.session.folder_id))
            .cloned()
            .collect()
    };
    let mut folders = Vec::new();
    for rt in rts {
        let entries: Vec<FileEntry> = rt.session.snapshot().await.into_values().collect();
        folders.push(RelayFolder {
            folder_id: rt.session.folder_id.clone(),
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

    #[test]
    fn relay_index_roundtrip() {
        let index = RelayIndex {
            protocol_version: PROTOCOL_VERSION,
            device_id: "a".into(),
            device_name: "home".into(),
            folders: vec![RelayFolder {
                folder_id: "f1".into(),
                entries: Vec::new(),
            }],
        };
        let text = serde_json::to_string(&index).unwrap();
        let back: RelayIndex = serde_json::from_str(&text).unwrap();
        assert_eq!(back.device_id, "a");
        assert_eq!(back.folders.len(), 1);
    }
}
