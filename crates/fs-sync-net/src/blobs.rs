//! iroh-blobs 集成：内容寻址传输。
//!
//! 服务端：`BlobsProtocol` 挂到 `iroh_blobs::ALPN`，对端可按 hash 拉取。
//! 客户端：`Remote`（rpc over QUIC）向对端请求 blob，落本地 FsStore。
//!
//! 因为 `GetStreamPair for Connection` 存在，可在**同一条 iroh 连接**上直接
//! 开流拉取，无需再建连接——但 iroh-blobs 的 `Remote` 需要一个 rpc 客户端，
//! 所以这里用 `Store::connect`（独立 rpc 端口）保持与上游 API 一致。

use anyhow::{Context, Result};
use iroh_blobs::BlobsProtocol;
use iroh_blobs::store::fs::FsStore;
use std::path::Path;

/// 打开（或创建）本地 blob 存储。
pub async fn open_store(root: &Path) -> Result<FsStore> {
    std::fs::create_dir_all(root).context("创建 blob 存储目录失败")?;
    FsStore::load(root).await.context("打开 FsStore 失败")
}

/// 构建服务端协议处理器（供 Router::accept 挂载）。
pub fn protocol(store: &FsStore) -> BlobsProtocol {
    BlobsProtocol::new(store, None)
}

/// 把文件内容加入 blob 存储，返回 blake3 hash（hex）。
///
/// 注意：iroh-blobs 的 `Hash` 与 fs-sync-core 用的 blake3 是同一算法，
/// 因此索引里的 hash 可直接用作 blob 地址。
pub async fn add_file(store: &FsStore, path: &Path) -> Result<String> {
    let tag = store.add_path(path).await.context("加入 blob 存储失败")?;
    Ok(tag.hash.to_string())
}

/// 把字节加入 blob 存储，返回 hash（hex）。
pub async fn add_bytes(store: &FsStore, data: Vec<u8>) -> Result<String> {
    let tag = store.add_bytes(data).await.context("加入 blob 存储失败")?;
    Ok(tag.hash.to_string())
}

/// 从本地 blob 存储导出到目标路径（原子写：先写临时文件再 rename）。
pub async fn export_to(store: &FsStore, hash_hex: &str, dest: &Path) -> Result<()> {
    let hash = parse_hash(hash_hex).context("hash 格式无效（需 64 位 hex）")?;
    let data = store
        .blobs()
        .get_bytes(hash)
        .await
        .context("读取 blob 失败")?;

    let tmp = dest.with_extension(format!(".foldersync-tmp-{}", std::process::id()));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("创建目标目录失败")?;
    }
    tokio::fs::write(&tmp, &data)
        .await
        .context("写临时文件失败")?;
    tokio::fs::rename(&tmp, dest)
        .await
        .context("原子替换目标文件失败")?;
    Ok(())
}

/// 校验本地 blob 是否已存在（避免重复传输）。
pub async fn has_blob(store: &FsStore, hash_hex: &str) -> bool {
    let Some(h) = parse_hash(hash_hex) else {
        return false;
    };
    store.blobs().has(h).await.unwrap_or(false)
}

/// 安全解析 hash：iroh-blobs 的 `Hash::from_str` 对短串会 panic（data-encoding 断言），
/// 因此这里先校验长度与字符合法性。
pub fn parse_hash(hash_hex: &str) -> Option<iroh_blobs::Hash> {
    // blake3 hex 固定 64 字符
    let s = hash_hex.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_and_export_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("blobs");
        let store = open_store(&store_dir).await.unwrap();

        let data = b"hello foldersync".to_vec();
        let hash = add_bytes(&store, data.clone()).await.unwrap();
        assert!(has_blob(&store, &hash).await);

        // hash 与 blake3 一致
        assert_eq!(hash, blake3::hash(&data).to_hex().to_string());

        let dest = tmp.path().join("out.txt");
        export_to(&store, &hash, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        // 临时文件不残留
        assert!(
            !dest
                .with_extension(format!(".foldersync-tmp-{}", std::process::id()))
                .exists()
        );
    }

    #[tokio::test]
    async fn missing_blob_reports_false() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(&tmp.path().join("b")).await.unwrap();
        assert!(!has_blob(&store, &"00".repeat(32)).await);
        assert!(!has_blob(&store, "not-a-hash").await);
    }

    /// 大文件往返（提示词 7 节：≥1GB）。
    ///
    /// 标 `#[ignore]`：要写 1GB 临时文件，日常测试不该跑。
    /// 跑法：`cargo test -p fs-sync-net large_file -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn large_file_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(&tmp.path().join("blobs")).await.unwrap();

        // 1 GiB + 一点，跨过整数边界
        const SIZE: usize = 1024 * 1024 * 1024 + 4096;
        let src = tmp.path().join("big.bin");
        {
            use std::io::Write;
            let mut f = std::io::BufWriter::new(std::fs::File::create(&src).unwrap());
            // 1 MiB 一块地写，内容用 xorshift 生成（可复现、非全零）
            let mut chunk = vec![0u8; 1024 * 1024];
            let mut x: u64 = 0x1234_5678_9ABC_DEF0;
            let mut written = 0usize;
            while written < SIZE {
                for b in chunk.iter_mut() {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    *b = x as u8;
                }
                let take = (SIZE - written).min(chunk.len());
                f.write_all(&chunk[..take]).unwrap();
                written += take;
            }
        }

        let t0 = std::time::Instant::now();
        let hash = add_file(&store, &src).await.unwrap();
        let add_elapsed = t0.elapsed();
        println!("1GiB 入库耗时: {add_elapsed:?}");

        // 导回去比对
        let dest = tmp.path().join("out.bin");
        let t1 = std::time::Instant::now();
        export_to(&store, &hash, &dest).await.unwrap();
        let export_elapsed = t1.elapsed();
        println!("1GiB 导出耗时: {export_elapsed:?}");

        let a = std::fs::metadata(&src).unwrap().len();
        let b = std::fs::metadata(&dest).unwrap().len();
        assert_eq!(a, b, "大小必须一致");
        assert_eq!(a as usize, SIZE);

        // 逐字节比对（大文件最容易在这里出错）
        let src_bytes = std::fs::read(&src).unwrap();
        let dest_bytes = std::fs::read(&dest).unwrap();
        assert_eq!(src_bytes.len(), dest_bytes.len());
        assert!(src_bytes == dest_bytes, "1GiB 内容必须完全一致");

        // 内容寻址：同内容再入库应得到同一个 hash
        let hash2 = add_file(&store, &dest).await.unwrap();
        assert_eq!(hash, hash2, "内容寻址要求同内容同 hash");
    }
}
