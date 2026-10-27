//! 同步会话：连接建立后的索引交换与文件传输编排（M3 单向，M4 双向扩展）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use fs_sync_core::{FileEntry, SyncAction, diff, scan_folder, validate_path};
use fs_sync_store::Store;
use tracing::{info, warn};

use crate::engine::SyncEngine;

/// 一次同步的结果统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// 本地推送到对端的文件数
    pub pushed: u64,
    /// 从对端拉取的文件数
    pub pulled: u64,
    /// 冲突数
    pub conflicts: u64,
    /// 删除数
    pub deleted: u64,
    /// 失败数
    pub failed: u64,
}

impl SyncStats {
    pub fn total(&self) -> u64 {
        self.pushed + self.pulled + self.conflicts + self.deleted + self.failed
    }
}

/// 扫描本地文件夹并同步到 store，返回 path -> entry。
pub fn scan_and_index(
    root: &Path,
    folder_id: &str,
    device_id: &str,
    store: &Store,
) -> Result<HashMap<String, FileEntry>> {
    let matcher = fs_sync_core::matcher_for_folder(root);
    let scan = scan_folder(root, &matcher)?;
    let existing: HashMap<String, FileEntry> = store
        .list_entries(folder_id)?
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    let mut out = HashMap::new();
    for (path, mut entry) in scan.entries {
        // 与数据库已有版本对比：内容未变则沿用旧版本向量，变了则递增
        if let Some(old) = existing.get(&path) {
            if old.hash == entry.hash && old.mtime == entry.mtime && !old.deleted {
                entry.version = old.version.clone();
                entry.modified_by = old.modified_by.clone();
            } else {
                entry.version = old.version.clone();
                entry.version.increment(device_id);
                entry.modified_by = device_id.to_string();
            }
        } else {
            entry.version.increment(device_id);
            entry.modified_by = device_id.to_string();
        }
        out.insert(path, entry);
    }

    // 本地消失的文件 → 墓碑
    for (path, old) in &existing {
        if !old.deleted && !out.contains_key(path) {
            let mut tomb = old.clone();
            tomb.deleted = true;
            tomb.version.increment(device_id);
            tomb.modified_by = device_id.to_string();
            out.insert(path.clone(), tomb);
        }
    }

    // 持久化索引
    let all: Vec<FileEntry> = out.values().cloned().collect();
    store.replace_entries(folder_id, &all)?;
    Ok(out)
}

/// 执行同步：把本地状态与对端状态比对，落盘需要的变更。
///
/// `pull_blob`：由调用方提供"从对端取 blob 到本地存储"的异步函数，
/// 这样本模块不必绑定具体传输实现（便于单测）。
pub async fn run_sync<F, Fut>(
    engine: &SyncEngine,
    local: &HashMap<String, FileEntry>,
    remote: &HashMap<String, FileEntry>,
    device_name: &str,
    mut pull_blob: F,
) -> Result<(SyncStats, Vec<SyncAction>)>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let actions = diff(local, remote);
    let mut stats = SyncStats::default();

    for action in &actions {
        match action {
            SyncAction::Push { .. } => stats.pushed += 1,
            SyncAction::MergeMeta { .. } => {}
            SyncAction::Pull { path } => {
                let Some(entry) = remote.get(path) else {
                    continue;
                };
                if entry.deleted {
                    // 远端墓碑 → 本地删除
                    match engine.apply_delete(path).await {
                        Ok(()) => stats.deleted += 1,
                        Err(e) => {
                            warn!(%path, "删除失败: {e}");
                            stats.failed += 1;
                        }
                    }
                    continue;
                }
                // 目录不需要 blob，直接建目录
                if entry.is_dir {
                    match engine.apply_pulled(entry, "").await {
                        Ok(()) => stats.pulled += 1,
                        Err(e) => {
                            warn!(%path, "建目录失败: {e}");
                            stats.failed += 1;
                        }
                    }
                    continue;
                }
                // 拉取内容（失败按指数退避重试，提示词 4.8）
                match pull_with_retry(&mut pull_blob, &entry.hash, path).await {
                    Ok(()) => match engine.apply_pulled(entry, &entry.hash).await {
                        Ok(()) => stats.pulled += 1,
                        Err(e) => {
                            warn!(%path, "落盘失败: {e}");
                            stats.failed += 1;
                        }
                    },
                    Err(e) => {
                        warn!(%path, "拉取 blob 失败（已重试）: {e}");
                        stats.failed += 1;
                    }
                }
            }
            SyncAction::Conflict {
                path,
                winner_side,
                winner,
                loser,
            } => {
                // 胜者内容落原路径，败者另存
                let (win, lose) = match winner_side {
                    fs_sync_core::Side::Remote => (winner, loser),
                    fs_sync_core::Side::Local => (loser, winner),
                };
                // 确保本地有胜者内容
                if pull_blob(win.hash.clone()).await.is_err() {
                    stats.failed += 1;
                    continue;
                }
                // 败者内容也要取到本地，才能作为冲突副本保留（绝不丢数据）
                let loser_bytes = if pull_blob(lose.hash.clone()).await.is_ok() {
                    read_blob_bytes(&engine.blobs, &lose.hash).await.ok()
                } else {
                    None
                };
                match engine
                    .apply_conflict(win, loser_bytes, &win.hash, device_name)
                    .await
                {
                    Ok(copy) => {
                        info!(%path, copy = %copy, "冲突已处理");
                        stats.conflicts += 1;
                    }
                    Err(e) => {
                        warn!(%path, "冲突处理失败: {e}");
                        stats.failed += 1;
                    }
                }
            }
        }
    }

    Ok((stats, actions))
}

/// 在已建立的连接上，通过 iroh-blobs 从对端拉取 blob。
///
/// 采用 iroh-blobs 官方高层 API：`Store::downloader(&endpoint)` +
/// `Downloader::download(hash, providers)`，其中 providers 传对端 EndpointId。
/// 这样无需手工管理 rpc 连接，iroh-blobs 会在自己的 ALPN 上直接与对端通信。
pub fn make_puller(
    endpoint: iroh::Endpoint,
    peer_id: iroh::EndpointId,
    local_store: Arc<iroh_blobs::store::fs::FsStore>,
) -> impl Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    let downloader = local_store.downloader(&endpoint);
    move |hash_hex: String| {
        let downloader = downloader.clone();
        let store = local_store.clone();
        Box::pin(async move {
            let hash = fs_sync_net::blobs::parse_hash(&hash_hex).context("hash 无效")?;
            if store.blobs().has(hash).await.unwrap_or(false) {
                return Ok(());
            }
            // DownloadProgress 实现了 IntoFuture<Output = Result<()>>，直接 await 到底
            downloader.download(hash, vec![peer_id]).await?;
            Ok(())
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    }
}

/// 带指数退避重试的 blob 拉取（1s → 2s → 4s，最多 3 次）。
///
/// 提示词 4.8：文件被占用/只读等瞬时失败不阻塞其他文件，重试后放弃。
async fn pull_with_retry<F, Fut>(pull: &mut F, hash: &str, path: &str) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut delay = std::time::Duration::from_secs(1);
    let mut last_err = None;
    for attempt in 1..=3 {
        match pull(hash.to_string()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(%path, attempt, "拉取失败，{delay:?} 后重试: {e}");
                last_err = Some(e);
                if attempt < 3 {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("未知错误")))
}

/// 从 blob 存储读出内容（冲突副本落盘用）。
pub async fn read_blob_bytes(
    store: &iroh_blobs::store::fs::FsStore,
    hash_hex: &str,
) -> Result<Vec<u8>> {
    let hash = fs_sync_net::blobs::parse_hash(hash_hex).context("hash 无效")?;
    let bytes = store
        .blobs()
        .get_bytes(hash)
        .await
        .context("读取 blob 失败")?;
    Ok(bytes.to_vec())
}

/// 从文件夹根解析绝对路径并确保不逃逸。
pub fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel = validate_path(rel)?;
    Ok(root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_index_assigns_version_for_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let store = Store::open_in_memory().unwrap();

        let map = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        let e = &map["a.txt"];
        assert_eq!(e.version.get("devA"), 1);
        assert_eq!(e.modified_by, "devA");
        assert!(!e.deleted);
        // 已落库
        assert_eq!(store.list_entries("f1").unwrap().len(), 1);
    }

    #[test]
    fn unchanged_file_keeps_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let store = Store::open_in_memory().unwrap();

        let first = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        let v1 = first["a.txt"].version.get("devA");
        // 再扫一次（内容未变）
        let second = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        assert_eq!(
            second["a.txt"].version.get("devA"),
            v1,
            "内容未变不应递增版本"
        );
    }

    #[test]
    fn modified_file_bumps_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let store = Store::open_in_memory().unwrap();
        let first = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        let v1 = first["a.txt"].version.get("devA");

        std::fs::write(tmp.path().join("a.txt"), b"changed").unwrap();
        let second = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        assert!(
            second["a.txt"].version.get("devA") > v1,
            "内容变化必须递增版本"
        );
    }

    #[test]
    fn deleted_file_becomes_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let store = Store::open_in_memory().unwrap();
        scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();

        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();
        let second = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        assert!(second["a.txt"].deleted, "消失的文件应变成墓碑");
    }

    #[test]
    fn resolve_in_root_rejects_escape() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_in_root(tmp.path(), "ok/a.txt").is_ok());
        assert!(resolve_in_root(tmp.path(), "../evil").is_err());
        assert!(resolve_in_root(tmp.path(), "/abs").is_err());
    }

    #[test]
    fn stats_total() {
        let s = SyncStats {
            pushed: 2,
            pulled: 3,
            conflicts: 1,
            deleted: 0,
            failed: 1,
        };
        assert_eq!(s.total(), 7);
    }
}
