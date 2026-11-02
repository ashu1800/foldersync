//! 文件条目模型。

use serde::{Deserialize, Serialize};

use crate::version::VersionVector;

/// 单个文件/目录的同步条目。
///
/// `path` 约定：相对文件夹根、`/` 分隔、Unicode NFC 规范化（扫描与接收时各自保证）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对路径，`/` 分隔
    pub path: String,
    /// 内容字节数（目录为 0）
    pub size: u64,
    /// 修改时间（Unix 秒；目录为 0）
    pub mtime: i64,
    /// blake3 内容哈希（32 字节，hex 存储；目录为空串）
    pub hash: String,
    pub is_dir: bool,
    /// 墓碑标记：true 表示该路径已被删除
    pub deleted: bool,
    /// 版本向量：device_id -> counter
    pub version: VersionVector,
    /// 最后修改该文件的设备
    pub modified_by: String,
    /// Unix 权限位（如 0o755）。Windows 上恒 0（Windows 没有等价语义，
    /// 落盘时跳过 chmod）。
    ///
    /// **协议兼容性**：必须 `skip`。postcard 是非自描述格式，`default` 不起
    /// 作用——不带 skip 的话旧版本对端发来的条目会因缺字段解码失败，
    /// 我们发出去的会让旧对端错位。mode 目前只在本地索引/存储里流转，
    /// 跨设备同步留给将来的协议版本。
    #[serde(skip)]
    pub mode: u32,
}

impl FileEntry {
    /// 构造一个新条目（deleted=false）。
    pub fn new(
        path: impl Into<String>,
        size: u64,
        mtime: i64,
        hash: impl Into<String>,
        is_dir: bool,
        device: &str,
    ) -> Self {
        let mut version = VersionVector::new();
        version.increment(device);
        Self {
            path: path.into(),
            size,
            mtime,
            hash: hash.into(),
            is_dir,
            deleted: false,
            version,
            modified_by: device.to_string(),
            mode: 0,
        }
    }

    /// 内容相同（哈希一致）时只合并版本向量，不需要传数据。
    pub fn same_content(&self, other: &FileEntry) -> bool {
        !self.is_dir && !other.is_dir && self.hash == other.hash
    }
}

/// 文件夹内单个文件的同步状态（供 UI 展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// 与远端一致
    InSync,
    /// 本地有变更待推送
    LocalPending,
    /// 远端有变更待拉取
    RemotePending,
    /// 冲突（产生副本）
    Conflict,
    /// 错误（如被占用）
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_entry_starts_own_version() {
        let e = FileEntry::new("a.txt", 3, 100, "deadbeef", false, "dev-a");
        assert_eq!(e.version.get("dev-a"), 1);
        assert_eq!(e.modified_by, "dev-a");
        assert!(!e.deleted);
    }

    #[test]
    fn same_content_requires_hash_match() {
        let a = FileEntry::new("a.txt", 3, 100, "h1", false, "dev-a");
        let mut b = a.clone();
        b.version.increment("dev-b");
        assert!(a.same_content(&b));
        let c = FileEntry::new("a.txt", 3, 100, "h2", false, "dev-a");
        assert!(!a.same_content(&c));
    }
}
