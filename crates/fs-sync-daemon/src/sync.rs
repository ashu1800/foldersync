//! 同步会话：连接建立后的索引交换与文件传输编排（M3 单向，M4 双向扩展）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use fs_sync_core::{FileEntry, SyncAction, diff, scan_folder, validate_path};
use fs_sync_net::blobstore::BlobStore;
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
    let mut scan = scan_folder(root, &matcher)?;
    let existing: HashMap<String, FileEntry> = store
        .list_entries(folder_id)?
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    let mut out = HashMap::new();
    for (path, mut entry) in std::mem::take(&mut scan.entries) {
        // 与数据库已有版本对比：内容未变则沿用旧版本向量，变了则递增
        if let Some(old) = existing.get(&path) {
            // 只看内容：hash 相同而 mtime 不同（拉取落盘、touch）不是新版本，
            // 沿用旧版本向量，只刷新 mtime，避免两端互推
            if old.hash == entry.hash && old.is_dir == entry.is_dir && !old.deleted {
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

    for (path, old) in &existing {
        if out.contains_key(path) {
            continue;
        }
        // 读不了的路径：原样保留旧条目，读失败 ≠ 已删除
        // 已有墓碑：原样保留，否则 replace_entries 会把它抹掉，对端就收不到删除
        if old.deleted || scan.is_unreadable(path) {
            out.insert(path.clone(), old.clone());
            continue;
        }
        // 本地消失的文件 → 墓碑
        {
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
                // 胜者内容落原路径，败者另存。
                // diff 产出的 winner/loser 已经是按 winner_side 挑好的，不能再按 side 对调。
                let (win, lose) = (winner, loser);
                // 本地一侧的内容入 blob 库即可，只有远端一侧需要拉取
                let (win_got, lose_got) = match winner_side {
                    fs_sync_core::Side::Local => (
                        engine.ensure_blob(win).await.map(|_| ()),
                        pull_blob(lose.hash.clone()).await,
                    ),
                    fs_sync_core::Side::Remote => (
                        pull_blob(win.hash.clone()).await,
                        engine.ensure_blob(lose).await.map(|_| ()),
                    ),
                };
                // 确保本地有胜者内容
                if win_got.is_err() {
                    stats.failed += 1;
                    continue;
                }
                // 败者内容也要取到本地，才能作为冲突副本保留（绝不丢数据）
                let loser_bytes = if lose_got.is_ok() {
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

/// 按设备记录构造 SSH 连接目标（依次尝试 Tailscale 地址）。
pub fn ssh_target_from_device(
    device: &fs_sync_store::Device,
) -> Result<fs_sync_net::ssh_client::SshTarget> {
    let host = device
        .addresses
        .first()
        .context("设备没有 Tailscale 地址（请先安装并登录 Tailscale）")?
        .clone();
    Ok(fs_sync_net::ssh_client::SshTarget {
        host,
        port: device.port,
        username: device.username.clone(),
        host_key: device.host_key.clone(),
    })
}

/// 创建一个“按需连接、连接复用”的 SSH blob 拉取器。
///
/// 一次同步批量的多个文件共用同一条 SSH 连接；连接失败会丢弃，下次重连。
pub fn make_ssh_puller(
    device: fs_sync_store::Device,
    identity: fs_sync_net::identity::SshIdentity,
    local_store: Arc<BlobStore>,
) -> impl Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    let client: Arc<tokio::sync::Mutex<Option<fs_sync_net::ssh_client::SshClient>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    move |hash_hex: String| {
        let device = device.clone();
        let identity = identity.clone();
        let store = local_store.clone();
        let client = client.clone();
        Box::pin(async move {
            if store.has(&hash_hex).await {
                return Ok(());
            }
            let mut guard = client.lock().await;
            if guard.is_none() {
                let mut last_err = None;
                for addr in &device.addresses {
                    let target = fs_sync_net::ssh_client::SshTarget {
                        host: addr.clone(),
                        port: device.port,
                        username: device.username.clone(),
                        host_key: device.host_key.clone(),
                    };
                    match fs_sync_net::ssh_client::SshClient::connect(&target, &identity).await {
                        Ok(c) => {
                            *guard = Some(c);
                            break;
                        }
                        Err(e) => last_err = Some(e),
                    }
                }
                if guard.is_none() {
                    return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("没有可用地址")));
                }
            }
            let client = guard.as_ref().unwrap();
            let sftp = client.sftp();
            fs_sync_net::ssh_client::SshClient::download_blob(&sftp, &hash_hex, &store).await
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
pub async fn read_blob_bytes(store: &BlobStore, hash_hex: &str) -> Result<Vec<u8>> {
    store.get_bytes(hash_hex).await.context("读取 blob 失败")
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

    /// 回归：墓碑必须跨扫描保留。曾经第二次扫描时墓碑没进 out，
    /// replace_entries 直接把它抹掉，重启后对端永远收不到这次删除。
    #[test]
    fn tombstone_survives_rescan() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let store = Store::open_in_memory().unwrap();
        scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();
        let second = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        let v = second["a.txt"].version.clone();

        let third = scan_and_index(tmp.path(), "f1", "devA", &store).unwrap();
        assert!(third["a.txt"].deleted, "墓碑在再次扫描后必须还在");
        assert_eq!(third["a.txt"].version, v, "墓碑不应重复递增版本");
        assert!(
            store
                .list_entries("f1")
                .unwrap()
                .iter()
                .any(|e| e.path == "a.txt" && e.deleted)
        );
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
