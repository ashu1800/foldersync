//! fs-sync-core：FolderSync 同步引擎的纯逻辑层。
//!
//! 不依赖网络（iroh）与具体存储（SQLite），只做：
//! 文件索引模型、版本向量、差异计算、冲突判定、忽略规则、路径校验、目录扫描。
//! 全部可单测。

mod diff;
mod entry;
mod error;
mod ignore_mod;
mod path;
mod scan;
mod version;

pub use diff::{Side, SyncAction, conflict_winner, diff};
pub use entry::{FileEntry, SyncState};
pub use error::{PathError, Result};
pub use ignore_mod::{
    DEFAULTS_OFF, IgnoreMatcher, matcher_for_folder, read_syncignore, read_syncignore_config,
};
pub use path::{CaseIndex, case_conflict, validate_path};
pub use scan::{hash_file, scan_folder};
pub use version::{Ordering as VvOrdering, VersionVector, compare, merge};

/// 内置默认忽略规则（转出便于上层复用）。
pub fn default_rules() -> Vec<String> {
    ignore_mod::DEFAULT_RULES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// 冲突副本的文件名后缀格式：`原名.sync-conflict-YYYYMMDD-HHMMSS-设备名.扩展名`
pub fn conflict_name(original: &str, device: &str, when: chrono::DateTime<chrono::Utc>) -> String {
    let stamp = when.format("%Y%m%d-%H%M%S");
    match original.rsplit_once('.') {
        // 有扩展名：插在扩展名前
        Some((stem, ext)) if !stem.is_empty() => {
            format!("{stem}.sync-conflict-{stamp}-{device}.{ext}")
        }
        _ => format!("{original}.sync-conflict-{stamp}-{device}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn conflict_name_inserts_before_ext() {
        let t = chrono::Utc
            .with_ymd_and_hms(2026, 10, 27, 10, 30, 5)
            .unwrap();
        assert_eq!(
            conflict_name("report.docx", "home-pc", t),
            "report.sync-conflict-20261027-103005-home-pc.docx"
        );
        // 无扩展名
        assert_eq!(
            conflict_name("Makefile", "home-pc", t),
            "Makefile.sync-conflict-20261027-103005-home-pc"
        );
        // 隐藏文件（点开头）：整体视为无扩展名
        assert_eq!(
            conflict_name(".gitignore", "home-pc", t),
            ".gitignore.sync-conflict-20261027-103005-home-pc"
        );
    }
}
