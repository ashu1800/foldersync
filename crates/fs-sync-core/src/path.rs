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

#[cfg(test)]
mod tests {
    use super::*;

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
