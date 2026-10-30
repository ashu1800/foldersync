//! 路径安全校验（提示词 4.8：对端路径不可信）。
//!
//! 规则：拒绝绝对路径、`..`、空组件、Windows 保留名与非法字符；
//! 调用方负责最终落盘时确认规范化路径仍位于文件夹根内。

use crate::error::{PathError, Result};

/// Windows 保留设备名（不含扩展名也算）。
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Windows 文件名非法字符（不含路径分隔符本身）。
const ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// 校验对端发来的相对路径，通过则原样返回（统一 `/` 分隔）。
pub fn validate_path(path: &str) -> Result<String> {
    // 不 trim：尾随空格本身就是要拦截的非法形态
    let p = path;

    if p.is_empty() || p.trim().is_empty() {
        return Err(PathError::Empty);
    }
    if p.len() > 1024 {
        return Err(PathError::TooLong(p.to_string()));
    }
    // 反斜杠一律视为分隔符（对端可能 Windows 风格），先归一
    let normalized = p.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains("//") {
        return Err(PathError::Absolute(p.to_string()));
    }
    // 盘符（如 C:/xxx）
    if normalized.len() >= 2 {
        let b = normalized.as_bytes();
        if b[1] == b':' && b[0].to_ascii_uppercase().is_ascii_alphabetic() {
            return Err(PathError::Absolute(p.to_string()));
        }
    }
    // Windows 的 \\?\ 与 \\.\ 前缀在归一后已被 // 拦截

    for comp in normalized.split('/') {
        if comp.is_empty() {
            return Err(PathError::BadComponent(p.to_string()));
        }
        if comp == "." || comp == ".." {
            return Err(PathError::BadComponent(p.to_string()));
        }
        // 保留名：CON、CON.txt 都拒绝
        let stem = comp.split('.').next().unwrap_or(comp);
        if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
            return Err(PathError::ReservedOrIllegal(p.to_string()));
        }
        if comp
            .chars()
            .any(|c| ILLEGAL_CHARS.contains(&c) || c.is_control())
        {
            return Err(PathError::ReservedOrIllegal(p.to_string()));
        }
        // 尾随空格/点在 Windows 上有歧义，直接拒绝
        if comp.ends_with(' ') || comp.ends_with('.') {
            return Err(PathError::ReservedOrIllegal(p.to_string()));
        }
    }

    Ok(normalized)
}

/// 检测两个路径是否仅大小写不同（Windows/macOS 上视为同一路径，按冲突处理）。
pub fn case_conflict(a: &str, b: &str) -> bool {
    a != b && a.eq_ignore_ascii_case(b)
}

/// 大小写不敏感文件系统上的落盘前检查：本地是否已有一个"仅大小写不同"的活路径。
/// 命中时调用方应跳过这次拉取，否则 `Readme.md` 会直接覆盖本地的 `README.md`。
///
/// 按小写路径建索引，一轮 apply 只建一次：逐个线性扫描的话，
/// 首次同步 10 万个新文件就是 10^10 次比较。
/// 折叠规则与 [`case_conflict`] 一致，只折叠 ASCII。
#[derive(Debug, Default)]
pub struct CaseIndex {
    by_lower: std::collections::HashMap<String, String>,
}

impl CaseIndex {
    /// 只收录活条目：墓碑对应的文件已不在磁盘上，落盘不会覆盖任何东西。
    pub fn new(local: &std::collections::HashMap<String, crate::FileEntry>) -> Self {
        let mut idx = Self::default();
        for (p, e) in local {
            if !e.deleted {
                idx.insert(p);
            }
        }
        idx
    }

    /// 返回与 `incoming` 仅大小写不同的本地路径。比较整条路径，
    /// 所以 `a/B.txt` 和 `A/b.txt` 也算冲突。完全同名不算，交给正常的版本比较。
    pub fn clash(&self, incoming: &str) -> Option<&str> {
        self.by_lower
            .get(&incoming.to_ascii_lowercase())
            .map(String::as_str)
            .filter(|p| case_conflict(p, incoming))
    }

    /// 本轮新落盘的路径也要登记，同一批里的 `a.txt` / `A.txt` 才能被拦住。
    pub fn insert(&mut self, path: &str) {
        self.by_lower
            .insert(path.to_ascii_lowercase(), path.to_string());
    }

    /// 删除落盘后注销，仅大小写改名（先删旧名）才能在同一轮里完成。
    pub fn remove(&mut self, path: &str) {
        let key = path.to_ascii_lowercase();
        if self.by_lower.get(&key).is_some_and(|p| p == path) {
            self.by_lower.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_index_policy() {
        use crate::FileEntry;
        let mut local = std::collections::HashMap::new();
        local.insert(
            "README.md".to_string(),
            FileEntry::new("README.md", 1, 1, "h", false, "A"),
        );
        local.insert(
            "a/B.txt".to_string(),
            FileEntry::new("a/B.txt", 1, 1, "h", false, "A"),
        );
        let mut tomb = FileEntry::new("old.txt", 1, 1, "h", false, "A");
        tomb.deleted = true;
        local.insert("old.txt".to_string(), tomb);
        let mut idx = CaseIndex::new(&local);

        // 活文件仅大小写不同 → 冲突，返回本地那条路径
        assert_eq!(idx.clash("Readme.md"), Some("README.md"));
        // 父目录大小写不同也算
        assert_eq!(idx.clash("A/b.txt"), Some("a/B.txt"));
        // 完全同名不是大小写冲突（正常的版本比较流程处理）
        assert_eq!(idx.clash("README.md"), None);
        // 本地是墓碑：磁盘上已没有文件
        assert_eq!(idx.clash("OLD.txt"), None);
        assert_eq!(idx.clash("other.md"), None);

        // 同一批里先落盘的路径也要拦住后来的
        idx.insert("new.txt");
        assert_eq!(idx.clash("NEW.txt"), Some("new.txt"));
        // 删掉旧名后，仅大小写改名可以落盘
        idx.remove("README.md");
        assert_eq!(idx.clash("Readme.md"), None);
        // remove 只注销完全同名的那条
        idx.remove("NEW.TXT");
        assert_eq!(idx.clash("New.txt"), Some("new.txt"));
    }

    #[test]
    fn accepts_normal_relative_paths() {
        assert_eq!(validate_path("a/b/c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(validate_path("single").unwrap(), "single");
        assert_eq!(validate_path("a\\b.txt").unwrap(), "a/b.txt"); // 归一反斜杠
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate_path(""), Err(PathError::Empty));
        assert_eq!(validate_path("   "), Err(PathError::Empty));
    }

    #[test]
    fn rejects_absolute() {
        assert!(validate_path("/etc/passwd").is_err());
        assert!(validate_path("C:/Windows/system32").is_err());
        assert!(validate_path("c:\\x").is_err());
    }

    #[test]
    fn rejects_dot_components() {
        assert!(validate_path("a/../b").is_err());
        assert!(validate_path("..").is_err());
        assert!(validate_path("./a").is_err());
        assert!(validate_path("a//b").is_err());
    }

    #[test]
    fn rejects_windows_reserved_names() {
        assert!(validate_path("CON").is_err());
        assert!(validate_path("dir/con.txt").is_err());
        assert!(validate_path("com1").is_err());
        // 大小写不敏感
        assert!(validate_path("Aux").is_err());
    }

    #[test]
    fn rejects_illegal_chars() {
        assert!(validate_path("a<b").is_err());
        assert!(validate_path("a:b").is_err());
        assert!(validate_path("a?b").is_err());
    }

    #[test]
    fn rejects_trailing_dot_or_space() {
        assert!(validate_path("a.").is_err());
        assert!(validate_path("a ").is_err());
        // 但正常含点的文件名可以
        assert!(validate_path("a.txt").is_ok());
    }

    #[test]
    fn case_conflict_detection() {
        assert!(case_conflict("Readme.md", "readme.md"));
        assert!(!case_conflict("a.txt", "a.txt"));
        assert!(!case_conflict("a.txt", "b.txt"));
    }
}
