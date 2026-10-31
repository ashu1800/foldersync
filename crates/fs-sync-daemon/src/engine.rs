//! 同步引擎：把 core 的 diff 结果落实为文件操作（M3 单向、M4 双向共用）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use fs_sync_core::{FileEntry, SyncAction, conflict_name, validate_path};
use fs_sync_net::blobstore::BlobStore;
use fs_sync_store::Store;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// 自写抑制表：同步落盘后 notify 会再触发一次事件，需忽略。
/// key = 相对路径，value = (预期 hash, 插入时刻)
#[derive(Debug, Default)]
pub struct WriteSuppression {
    inner: Mutex<HashMap<String, (String, std::time::Instant)>>,
}

impl WriteSuppression {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记"这次写入是我们自己干的"。
    pub async fn insert(&self, rel_path: &str, hash: &str) {
        self.inner.lock().await.insert(
            rel_path.to_string(),
            (hash.to_string(), std::time::Instant::now()),
        );
    }

    /// 命中则消费并返回 true（一次性）。
    pub async fn take_if_hit(&self, rel_path: &str, hash: Option<&str>) -> bool {
        let mut map = self.inner.lock().await;
        // 过期清理（TTL 5s）
        map.retain(|_, (_, at)| at.elapsed() < std::time::Duration::from_secs(5));
        match map.get(rel_path) {
            Some((expected, _)) => {
                let matched = hash.is_none_or(|h| h == expected);
                if matched {
                    map.remove(rel_path);
                }
                matched
            }
            None => false,
        }
    }

    /// 清空（暂停同步时调用）。
    pub async fn clear(&self) {
        self.inner.lock().await.clear();
    }

    /// 同步版本的一次性查询（供 notify 回调等非 async 上下文使用）。
    ///
    /// 用 `try_lock` 避免在回调线程阻塞：竞争时保守返回"未命中"，
    /// 最坏情况只是多产生一次事件（后续 hash 比对会过滤掉），不会丢数据。
    /// 同步版本（供 notify 回调线程使用，那里不能 await）。
    ///
    /// 拿不到锁时**短暂自旋**而不是直接放弃：`insert` 与事件回调几乎同时发生是
    /// 常态（我们刚写完文件，watcher 立刻报事件），此时放弃就等于把自己的写入
    /// 当成外部变更，触发一次无谓的回写。实测在 CI 的慢机器上会稳定复现。
    ///
    /// 自旋上限 50ms：真的锁不上就说明有别的长事务在跑，此时选择"不抑制"
    /// 更安全（宁可多同步一次，不能漏掉外部变更）。
    pub fn try_take_if_hit(&self, rel_path: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        loop {
            match self.inner.try_lock() {
                Ok(mut map) => {
                    map.retain(|_, (_, at)| at.elapsed() < std::time::Duration::from_secs(5));
                    return map.remove(rel_path).is_some();
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(_) => return false,
            }
        }
    }
}

/// 同步引擎：持有文件夹根目录、索引存储与 blob 存储。
#[derive(Clone)]
pub struct SyncEngine {
    pub folder_id: String,
    pub root: PathBuf,
    pub store: Arc<Store>,
    /// blob 存储实例（按 blake3 寻址，进程内共享同一目录）
    pub blobs: Arc<BlobStore>,
    pub suppression: Arc<WriteSuppression>,
}

impl SyncEngine {
    pub fn new(folder_id: String, root: PathBuf, store: Arc<Store>, blobs: Arc<BlobStore>) -> Self {
        Self {
            folder_id,
            root,
            store,
            blobs,
            suppression: Arc::new(WriteSuppression::new()),
        }
    }

    /// 把远端条目写入本地（内容已从 blob 存储取到）。
    ///
    /// 流程（提示词 4.5）：校验路径 → 原子落盘 → 更新索引 → 登记自写抑制。
    pub async fn apply_pulled(&self, entry: &FileEntry, blob_hash: &str) -> Result<()> {
        // 安全：路径必过校验（对端不可信）
        let rel =
            validate_path(&entry.path).with_context(|| format!("拒绝非法路径: {}", entry.path))?;
        let dest = self.root.join(&rel);

        // 防御：规范化后必须仍在文件夹内
        ensure_within(&self.root, &dest)?;

        if entry.is_dir {
            tokio::fs::create_dir_all(&dest)
                .await
                .with_context(|| format!("创建目录失败: {}", dest.display()))?;
            self.mark_applied(entry).await?;
            return Ok(());
        }

        // 从共享 blob 存储导出（原子写）
        self.blobs.export_to(blob_hash, &dest).await?;

        // 落盘文件的 mtime 对齐对端条目。否则本地 mtime 是"现在"，兜底扫描看到
        // mtime 变化就会递增版本推回去，两端来回互推。失败不致命（扫描侧还有 hash 判等兜底）。
        if entry.mtime > 0 {
            let p = dest.clone();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.mtime as u64);
            let r = tokio::task::spawn_blocking(move || {
                std::fs::File::options()
                    .write(true)
                    .open(&p)?
                    .set_modified(t)
            })
            .await;
            if let Ok(Err(e)) = r {
                warn!(path = %rel, "设置 mtime 失败: {e}");
            }
        }

        // 自写抑制：避免监听循环
        self.suppression.insert(&rel, blob_hash).await;
        self.mark_applied(entry).await?;

        // 活动日志：只记文件，不记目录；下载完成即记录，大小用于界面展示
        if !entry.is_dir {
            let _ = self.store.log_activity_sized(
                "download",
                &self.folder_id,
                &entry.path,
                entry.size as i64,
                "已从对端下载",
            );
        }

        info!(path = %rel, size = entry.size, "文件已落盘");
        Ok(())
    }

    /// 删除本地文件（墓碑生效）。
    pub async fn apply_delete(&self, rel_path: &str) -> Result<()> {
        let rel = validate_path(rel_path).with_context(|| format!("拒绝非法路径: {rel_path}"))?;
        let dest = self.root.join(&rel);
        ensure_within(&self.root, &dest)?;

        if dest.is_dir() {
            // 只删空目录，避免误删用户内容
            if let Err(e) = tokio::fs::remove_dir(&dest).await {
                warn!(path = %rel, "目录非空或无法删除，跳过: {e}");
            }
        } else if dest.exists() {
            tokio::fs::remove_file(&dest)
                .await
                .with_context(|| format!("删除文件失败: {}", dest.display()))?;
            self.suppression.insert(&rel, "").await;
            let _ = self
                .store
                .log_activity("delete", &self.folder_id, rel_path, "已同步对端删除");
            info!(path = %rel, "文件已删除");
        }
        Ok(())
    }

    /// 冲突处理：胜者内容落原路径，败者另存 `原名.sync-conflict-...`。
    pub async fn apply_conflict(
        &self,
        winner: &FileEntry,
        loser_bytes: Option<Vec<u8>>,
        winner_hash: &str,
        device_name: &str,
    ) -> Result<String> {
        let rel = validate_path(&winner.path)?;

        // 落败版本必须先落地成副本，否则下面的覆盖就是静默数据丢失。
        // loser_bytes 拿不到的场景（对端内容不可读、blob 缺失）不能"算了"，
        // 而是要保住本地现有文件——把它改名成副本，再把胜者写到原路径。
        let stamp = chrono::Utc::now();
        let conflict_rel = conflict_name(&rel, device_name, stamp);
        let conflict_dest = self.root.join(&conflict_rel);
        ensure_within(&self.root, &conflict_dest)?;

        match loser_bytes {
            Some(bytes) => {
                if let Some(parent) = conflict_dest.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&conflict_dest, &bytes)
                    .await
                    .context("写冲突副本失败")?;
            }
            None => {
                // 没有内容可写：本地已有文件就是唯一的落败版本，直接改名保住它。
                // 本地不存在时（理论上是第一次同步）才真的无内容可保。
                let local_abs = self.root.join(&rel);
                if local_abs.exists() {
                    if let Some(parent) = conflict_dest.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::rename(&local_abs, &conflict_dest)
                        .await
                        .context("保留本地版本失败")?;
                    warn!(
                        path = %rel,
                        copy = %conflict_rel,
                        "冲突：对端内容不可读，已把本地版本另存为副本"
                    );
                }
            }
        }
        self.suppression.insert(&conflict_rel, "").await;

        // 胜者落到原路径
        self.apply_pulled(winner, winner_hash).await?;

        self.store.log_activity(
            "conflict",
            &self.folder_id,
            &rel,
            &format!("保留原版本，副本: {conflict_rel}"),
        )?;

        Ok(conflict_rel)
    }

    /// 更新本地索引（写入 store）。
    async fn mark_applied(&self, entry: &FileEntry) -> Result<()> {
        self.store.upsert_entry(&self.folder_id, entry)?;
        Ok(())
    }

    /// 把本地条目推给对端前，确保内容已进 blob 存储，返回 hash。
    pub async fn ensure_blob(&self, entry: &FileEntry) -> Result<String> {
        let rel = validate_path(&entry.path)?;
        let abs = self.root.join(&rel);
        if self.blobs.has(&entry.hash).await {
            return Ok(entry.hash.clone());
        }
        self.blobs.add_file(&abs).await
    }

    /// 执行一批 diff 动作中「需要本地落盘」的部分，返回需推送的路径列表。
    pub async fn apply_actions(&self, actions: &[SyncAction]) -> Result<Vec<String>> {
        let mut to_push = Vec::new();
        for action in actions {
            match action {
                SyncAction::Push { path } => to_push.push(path.clone()),
                SyncAction::MergeMeta { path, merged } => {
                    // 内容一致，只更新版本向量
                    if let Some(mut e) = self
                        .store
                        .list_entries(&self.folder_id)?
                        .into_iter()
                        .find(|x| &x.path == path)
                    {
                        e.version = merged.clone();
                        self.store.upsert_entry(&self.folder_id, &e)?;
                    }
                }
                SyncAction::Pull { .. } => { /* 由调用方拉取 blob 后调用 apply_pulled */ }
                SyncAction::Conflict { path, .. } => {
                    warn!(path = %path, "冲突动作需在完整同步流程中处理");
                }
            }
        }
        Ok(to_push)
    }
}

/// 确认 `path` 规范化后仍在 `root` 内（防路径穿越）。
pub fn ensure_within(root: &Path, path: &Path) -> Result<()> {
    let root_c = root
        .canonicalize()
        .with_context(|| format!("文件夹根不存在: {}", root.display()))?;
    // 目标可能还不存在：规范化其最近的已存在祖先
    let mut parent = path;
    while !parent.exists() {
        parent = match parent.parent() {
            Some(p) => p,
            None => break,
        };
    }
    let parent_c = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    if !parent_c.starts_with(&root_c) {
        anyhow::bail!(
            "路径逃逸文件夹根: {} 不在 {} 内",
            path.display(),
            root.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_sync_core::VersionVector;

    fn entry(path: &str, hash: &str) -> FileEntry {
        FileEntry {
            path: path.into(),
            size: 3,
            mtime: 1,
            hash: hash.into(),
            is_dir: false,
            deleted: false,
            version: VersionVector::new(),
            modified_by: "peer".into(),
        }
    }

    async fn engine() -> (SyncEngine, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sync");
        std::fs::create_dir_all(&root).unwrap();
        let blobs = Arc::new(BlobStore::open(tmp.path().join("share")).unwrap());
        let store = Arc::new(Store::open_in_memory().unwrap());
        (SyncEngine::new("f1".into(), root, store, blobs), tmp)
    }

    #[tokio::test]
    async fn pulled_file_lands_and_is_suppressed() {
        let (eng, _tmp) = engine().await;
        let hash = eng.blobs.add_bytes(b"abc".to_vec()).await.unwrap();

        let e = entry("dir/a.txt", &hash);
        eng.apply_pulled(&e, &hash).await.unwrap();

        assert_eq!(std::fs::read(eng.root.join("dir/a.txt")).unwrap(), b"abc");
        // 自写抑制已登记，且一次性
        assert!(eng.suppression.take_if_hit("dir/a.txt", Some(&hash)).await);
        assert!(!eng.suppression.take_if_hit("dir/a.txt", Some(&hash)).await);
    }

    #[tokio::test]
    async fn pulled_file_logs_download_activity_with_size() {
        let (eng, _tmp) = engine().await;
        let hash = eng.blobs.add_bytes(b"hello".to_vec()).await.unwrap();
        let mut e = entry("a.txt", &hash);
        e.size = 5;
        eng.apply_pulled(&e, &hash).await.unwrap();

        let rows = eng.store.list_activity(Some("f1"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "download");
        assert_eq!(rows[0].path, "a.txt");
        assert_eq!(rows[0].size, 5);
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let (eng, _tmp) = engine().await;
        let e = entry("../escape.txt", "");
        assert!(eng.apply_pulled(&e, "").await.is_err(), "必须拒绝路径穿越");
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let (eng, _tmp) = engine().await;
        assert!(
            eng.apply_pulled(&entry("C:/Windows/evil.dll", ""), "")
                .await
                .is_err()
        );
        assert!(
            eng.apply_pulled(&entry("/etc/passwd", ""), "")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn delete_removes_file() {
        let (eng, _tmp) = engine().await;
        std::fs::write(eng.root.join("gone.txt"), b"x").unwrap();
        eng.apply_delete("gone.txt").await.unwrap();
        assert!(!eng.root.join("gone.txt").exists());
    }

    #[tokio::test]
    async fn delete_keeps_nonempty_dir() {
        let (eng, _tmp) = engine().await;
        std::fs::create_dir_all(eng.root.join("d")).unwrap();
        std::fs::write(eng.root.join("d/keep.txt"), b"x").unwrap();
        eng.apply_delete("d").await.unwrap();
        assert!(eng.root.join("d").exists(), "非空目录不应被删除");
    }

    #[tokio::test]
    async fn ensure_blob_adds_file() {
        let (eng, _tmp) = engine().await;
        std::fs::write(eng.root.join("a.txt"), b"hello").unwrap();
        let mut e = entry("a.txt", "");
        e.hash = blake3::hash(b"hello").to_hex().to_string();
        let hash = eng.ensure_blob(&e).await.unwrap();
        assert_eq!(hash, e.hash);
        assert!(eng.blobs.has(&hash).await);
    }

    #[tokio::test]
    async fn conflict_keeps_both_versions() {
        let (eng, _tmp) = engine().await;
        let hash = eng.blobs.add_bytes(b"winner".to_vec()).await.unwrap();
        let winner = entry("doc.txt", &hash);
        let copy = eng
            .apply_conflict(&winner, Some(b"loser".to_vec()), &hash, "home-pc")
            .await
            .unwrap();

        // 胜者占原路径
        assert_eq!(std::fs::read(eng.root.join("doc.txt")).unwrap(), b"winner");
        // 败者另存，含 .sync-conflict 标记
        assert!(copy.contains(".sync-conflict-"), "冲突副本命名: {copy}");
        assert_eq!(std::fs::read(eng.root.join(&copy)).unwrap(), b"loser");
    }

    /// 回归：落败版本内容拿不到时，绝不能静默覆盖本地文件。
    ///
    /// 曾经的 bug：`if let Some(bytes) = loser_bytes` 在 None 时直接跳过写副本，
    /// 接着用胜者覆盖原路径 —— 本地那份修改被无声删除。
    /// 双节点集成测试（tests/two_node.rs::concurrent_edit_keeps_both_versions）
    /// 第一次跑出来就是这个现象。
    #[tokio::test]
    async fn conflict_without_loser_bytes_preserves_local_file() {
        let (eng, _tmp) = engine().await;

        // 本地已有被修改过的文件（这就是落败版本，唯一的副本在磁盘上）
        std::fs::write(eng.root.join("doc.txt"), b"local edit").unwrap();

        let hash = eng
            .blobs
            .add_bytes(b"remote winner".to_vec())
            .await
            .unwrap();
        let winner = entry("doc.txt", &hash);

        // loser_bytes = None：模拟对端内容不可读
        let copy = eng
            .apply_conflict(&winner, None, &hash, "home-pc")
            .await
            .unwrap();

        // 胜者占原路径
        assert_eq!(
            std::fs::read(eng.root.join("doc.txt")).unwrap(),
            b"remote winner"
        );
        // 本地版本没丢：被改名成冲突副本
        assert!(
            eng.root.join(&copy).exists(),
            "本地版本应被保留为副本: {copy}"
        );
        assert_eq!(
            std::fs::read(eng.root.join(&copy)).unwrap(),
            b"local edit",
            "副本内容应是本地原内容，不能丢"
        );
    }

    #[test]
    fn ensure_within_blocks_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        assert!(ensure_within(&root, &root.join("ok.txt")).is_ok());
        assert!(ensure_within(&root, &tmp.path().join("outside.txt")).is_err());
    }
}
