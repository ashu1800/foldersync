//! 内容寻址的 blob 存储（替代 iroh-blobs）。
//!
//! 目录结构：`<root>/blobs/<blake3 hex>`。文件内容按 hash 命名，天然去重。
//! 上传/下载都走“临时文件 + rename”，保证读者永远看不到半截文件。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 本地 blob 库。Clone 只是复制路径，底层文件系统天然共享。
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// 打开（或创建）blob 库。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("blobs")).context("创建 blob 存储目录失败")?;
        Ok(Self { root })
    }

    /// blob 根目录（SSH/SFTP 对外暴露的目录）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 单个 blob 的本地路径。
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(hash)
    }

    /// 是否已有该内容。
    pub async fn has(&self, hash: &str) -> bool {
        if !is_valid_hash(hash) {
            return false;
        }
        tokio::fs::metadata(self.blob_path(hash))
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    /// 把本地文件加入 blob 库，返回 blake3 hex。
    pub async fn add_file(&self, src: &Path) -> Result<String> {
        let mut reader = tokio::fs::File::open(src)
            .await
            .with_context(|| format!("打开文件失败: {}", src.display()))?;
        let tmp = self.temp_path();
        let mut writer = tokio::fs::File::create(&tmp)
            .await
            .context("创建 blob 临时文件失败")?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = reader.read(&mut buf).await.context("读取源文件失败")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n]).await.context("写 blob 失败")?;
        }
        writer.flush().await.ok();
        drop(writer);
        let hash = hasher.finalize().to_hex().to_string();
        self.finish(tmp, &hash).await?;
        Ok(hash)
    }

    /// 把字节加入 blob 库，返回 blake3 hex。
    pub async fn add_bytes(&self, data: Vec<u8>) -> Result<String> {
        let hash = blake3::hash(&data).to_hex().to_string();
        if self.has(&hash).await {
            return Ok(hash);
        }
        let tmp = self.temp_path();
        tokio::fs::write(&tmp, &data)
            .await
            .context("写 blob 临时文件失败")?;
        self.finish(tmp, &hash).await?;
        Ok(hash)
    }

    /// 读取整个 blob。
    pub async fn get_bytes(&self, hash: &str) -> Result<Vec<u8>> {
        anyhow::ensure!(is_valid_hash(hash), "hash 格式无效（需 64 位 hex）");
        tokio::fs::read(self.blob_path(hash))
            .await
            .context("读取 blob 失败")
    }

    /// 计算文件 blake3（与核心扫描使用同一算法）。
    pub async fn hash_file(&self, path: &Path) -> Result<String> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// 删除不在 `keep` 集合里的 blob，返回删除数量。
    ///
    /// 之前 blob 只增不减：每次修改文件都新增一个按 hash 寻址的 blob，
    /// 旧版本永久保留，常用文档库用一年能膨胀几十 GB。调用方（兜底扫描
    /// 周期任务）传入当前所有活条目的 hash 集合；墓碑对应的内容会随之
    /// 清掉——这正是预期行为（文件删除后其历史内容没有保留价值）。
    /// 根目录下的遗留临时文件（.tmp-* / .download-* / .relay-download-*，
    /// 超过 1 小时）顺带清理。
    pub async fn gc(&self, keep: &std::collections::HashSet<String>) -> Result<usize> {
        let mut removed = 0;
        let blobs_dir = self.root.join("blobs");
        let mut rd = tokio::fs::read_dir(&blobs_dir).await?;
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            if !is_valid_hash(&name) {
                continue;
            }
            if !keep.contains(&name) && tokio::fs::remove_file(e.path()).await.is_ok() {
                removed += 1;
            }
        }
        // 清理 root 下的遗留临时文件（崩溃/断电残留）
        let mut rd = tokio::fs::read_dir(&self.root).await?;
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            let is_tmp = name.starts_with(".tmp-")
                || name.starts_with(".download-")
                || name.starts_with(".relay-download-");
            if is_tmp {
                let stale = match tokio::fs::metadata(e.path()).await {
                    Ok(m) => m
                        .modified()
                        .map(|t| {
                            t.elapsed()
                                .map(|d| d > std::time::Duration::from_secs(3600))
                                .unwrap_or(false)
                        })
                        .unwrap_or(false),
                    Err(_) => false,
                };
                if stale && tokio::fs::remove_file(e.path()).await.is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// 导出到目标路径（原子写：临时文件 + rename）。
    pub async fn export_to(&self, hash: &str, dest: &Path) -> Result<()> {
        anyhow::ensure!(is_valid_hash(hash), "hash 格式无效（需 64 位 hex）");
        // 调用方可能已加 \\?\ 长路径前缀；临时文件名必须从**原始形态**派生
        //（with_file_name 作用在前缀形态上会生成 `\...` 开头的坏路径），
        // 真正打开时再统一走 long_path。
        let raw = fs_sync_core::strip_long_path(dest);
        if let Some(parent) = raw.parent() {
            tokio::fs::create_dir_all(fs_sync_core::long_path(parent))
                .await
                .context("创建目标目录失败")?;
        }
        let name = raw
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "blob".into());
        let tmp = raw.with_file_name(format!(
            ".foldersync-tmp-{name}-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut src = tokio::fs::File::open(self.blob_path(hash))
            .await
            .context("读取 blob 失败")?;
        let mut out = tokio::fs::File::create(fs_sync_core::long_path(&tmp))
            .await
            .context("创建临时目标文件失败")?;
        tokio::io::copy(&mut src, &mut out)
            .await
            .context("写临时文件失败")?;
        out.flush().await.ok();
        drop(out);
        tokio::fs::rename(fs_sync_core::long_path(&tmp), fs_sync_core::long_path(&raw))
            .await
            .context("原子替换目标文件失败")?;
        Ok(())
    }

    /// 把已下载的临时文件校验后移入 blob 库。
    pub async fn adopt_downloaded(&self, tmp: &Path, expected_hash: &str) -> Result<()> {
        let actual = hash_file(tmp).await?;
        anyhow::ensure!(
            actual == expected_hash,
            "下载内容校验失败: 期望 {expected_hash}, 实际 {actual}"
        );
        self.finish(tmp.to_path_buf(), expected_hash).await
    }

    fn temp_path(&self) -> PathBuf {
        use rand::Rng;
        let suffix: u64 = rand::rng().random();
        self.root
            .join(format!(".tmp-{}-{suffix:x}", std::process::id()))
    }

    async fn finish(&self, tmp: PathBuf, hash: &str) -> Result<()> {
        let dest = self.blob_path(hash);
        if dest.exists() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Ok(());
        }
        tokio::fs::rename(&tmp, &dest)
            .await
            .context("落盘 blob 失败")?;
        Ok(())
    }
}

/// 计算文件 blake3（与核心扫描使用同一算法）。
pub async fn hash_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// 校验 64 位 hex。
pub fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_and_export_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path().join("share")).unwrap();
        let data = b"hello foldersync".to_vec();
        let hash = store.add_bytes(data.clone()).await.unwrap();
        assert_eq!(hash, blake3::hash(&data).to_hex().to_string());
        assert!(store.has(&hash).await);

        let dest = tmp.path().join("out.txt");
        store.export_to(&hash, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        // 临时文件不残留
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path().join("share"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件");
    }

    #[tokio::test]
    async fn add_file_matches_blake3_and_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path().join("share")).unwrap();
        let src = tmp.path().join("a.bin");
        std::fs::write(&src, b"content").unwrap();
        let h1 = store.add_file(&src).await.unwrap();
        let h2 = store.add_file(&src).await.unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1, blake3::hash(b"content").to_hex().to_string());
        let n = std::fs::read_dir(tmp.path().join("share/blobs"))
            .unwrap()
            .count();
        assert_eq!(n, 1, "同内容只应存一份");
    }

    #[tokio::test]
    async fn invalid_hash_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path()).unwrap();
        assert!(!store.has("not-a-hash").await);
        assert!(store.get_bytes("short").await.is_err());
    }

    #[tokio::test]
    async fn adopt_downloaded_verifies_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path().join("share")).unwrap();
        let downloaded = tmp.path().join("download.tmp");
        std::fs::write(&downloaded, b"abc").unwrap();
        let good = blake3::hash(b"abc").to_hex().to_string();
        store.adopt_downloaded(&downloaded, &good).await.unwrap();
        assert!(store.has(&good).await);

        let bad = tmp.path().join("bad.tmp");
        std::fs::write(&bad, b"xyz").unwrap();
        assert!(store.adopt_downloaded(&bad, &good).await.is_err());
    }

    /// GC：keep 之外的内容被删、keep 之内的保留、临时文件按年龄清理。
    #[tokio::test]
    async fn gc_removes_unreferenced_blobs_and_stale_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path().join("share")).unwrap();
        let h_keep = store.add_bytes(b"keep".to_vec()).await.unwrap();
        let h_drop = store.add_bytes(b"drop".to_vec()).await.unwrap();

        // 造一个 2 小时前的遗留临时文件
        let stale_tmp = tmp.path().join("share/.tmp-999-deadbeef");
        std::fs::write(&stale_tmp, b"leftover").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        let f = std::fs::File::options()
            .write(true)
            .open(&stale_tmp)
            .unwrap();
        f.set_modified(old).unwrap();
        drop(f);

        let removed = store
            .gc(&std::collections::HashSet::from([h_keep.clone()]))
            .await
            .unwrap();
        assert_eq!(removed, 2, "应删 1 个孤儿 blob + 1 个过期临时文件");
        assert!(store.has(&h_keep).await, "keep 集合内的必须保留");
        assert!(!store.has(&h_drop).await, "keep 之外的应被删");
        assert!(!stale_tmp.exists());
    }
}
