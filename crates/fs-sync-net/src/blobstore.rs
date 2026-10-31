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

    /// 导出到目标路径（原子写：临时文件 + rename）。
    pub async fn export_to(&self, hash: &str, dest: &Path) -> Result<()> {
        anyhow::ensure!(is_valid_hash(hash), "hash 格式无效（需 64 位 hex）");
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("创建目标目录失败")?;
        }
        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "blob".into());
        let tmp = dest.with_file_name(format!(
            ".foldersync-tmp-{name}-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut src = tokio::fs::File::open(self.blob_path(hash))
            .await
            .context("读取 blob 失败")?;
        let mut out = tokio::fs::File::create(&tmp)
            .await
            .context("创建临时目标文件失败")?;
        tokio::io::copy(&mut src, &mut out)
            .await
            .context("写临时文件失败")?;
        out.flush().await.ok();
        drop(out);
        tokio::fs::rename(&tmp, dest)
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
}
