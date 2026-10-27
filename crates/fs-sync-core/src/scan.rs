//! 目录扫描：遍历文件夹生成 FileEntry 列表（含 blake3 哈希），应用忽略规则。

use std::collections::HashMap;
use std::path::Path;

use blake3::Hasher;
use ignore::WalkBuilder;

use crate::entry::FileEntry;
use crate::ignore_mod::IgnoreMatcher;
use crate::version::VersionVector;

/// 扫描结果。
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// path -> entry（path 为相对路径，`/` 分隔）
    pub entries: HashMap<String, FileEntry>,
    /// 被忽略/跳过的路径（符号链接等），供日志展示
    pub skipped: Vec<String>,
}

/// 扫描整个文件夹。跳过符号链接（初版不同步链接，提示词 4.8）。
pub fn scan_folder(root: &Path, matcher: &IgnoreMatcher) -> std::io::Result<ScanResult> {
    let mut res = ScanResult::default();

    let walker = WalkBuilder::new(root)
        .follow_links(false)
        .filter_entry(|_| true) // 不用 walker 内置过滤，统一走我们的 matcher
        .build();

    for item in walker {
        let e = match item {
            Ok(e) => e,
            Err(_) => continue, // 读不了的条目跳过
        };
        let abs = e.path();
        if abs == root {
            continue;
        }
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        let meta = match e.file_type() {
            Some(ft) if ft.is_symlink() => {
                res.skipped.push(rel_str);
                continue;
            }
            Some(ft) => ft,
            None => continue,
        };

        let is_dir = meta.is_dir();
        if matcher.matched(&rel_str, is_dir) {
            continue;
        }

        let (size, mtime, hash) = if is_dir {
            (0u64, 0i64, String::new())
        } else {
            let md = match std::fs::metadata(abs) {
                Ok(m) => m,
                Err(_) => {
                    res.skipped.push(rel_str);
                    continue;
                }
            };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let hash = hash_file(abs).unwrap_or_default();
            (md.len(), mtime, hash)
        };

        let entry = FileEntry {
            path: rel_str.clone(),
            size,
            mtime,
            hash,
            is_dir,
            deleted: false,
            version: VersionVector::new(),
            modified_by: String::new(), // 扫描不产生版本，由调用方按文件夹逻辑填充
        };
        res.entries.insert(rel_str, entry);
    }

    Ok(res)
}

/// 分块计算文件 blake3 哈希，返回 hex。
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Hasher::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore_mod::IgnoreMatcher;

    fn write(p: &Path, content: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn scan_lists_files_and_dirs_with_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("a.txt"), "hello");
        write(&root.join("sub/b.txt"), "world");
        std::fs::create_dir(root.join("empty_dir")).unwrap();

        let m = IgnoreMatcher::with_defaults(root);
        let r = scan_folder(root, &m).unwrap();

        assert!(r.entries.contains_key("a.txt"));
        assert!(r.entries.contains_key("sub"));
        assert!(r.entries.contains_key("sub/b.txt"));
        assert!(r.entries.contains_key("empty_dir"));

        let a = &r.entries["a.txt"];
        assert_eq!(a.size, 5);
        assert_eq!(a.hash, blake3::hash(b"hello").to_hex().to_string());
        assert!(!a.is_dir);

        assert!(r.entries["sub"].is_dir);
        assert_eq!(r.entries["sub"].hash, "");
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn scan_respects_ignore_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("keep.rs"), "fn main(){}");
        write(&root.join("node_modules/lib.js"), "x");
        write(&root.join("debug.log"), "x");

        let m = IgnoreMatcher::new(root, &["*.log".to_string()]);
        let r = scan_folder(root, &m).unwrap();
        assert!(r.entries.contains_key("keep.rs"));
        assert!(!r.entries.contains_key("debug.log"));
    }

    #[test]
    fn scan_skips_symlinks() {
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            write(&root.join("real.txt"), "data");
            std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
            let m = IgnoreMatcher::with_defaults(root);
            let r = scan_folder(root, &m).unwrap();
            assert!(r.entries.contains_key("real.txt"));
            assert!(r.skipped.iter().any(|s| s == "link.txt"));
        }
    }

    /// 规模测试（提示词 7 节）：10 万个小文件的扫描要能跑完且不炸内存。
    ///
    /// 标 `#[ignore]`：单跑约几十秒，不该拖慢日常 `cargo test`。
    /// 用 `cargo test -p fs-sync-core -- --ignored` 显式运行。
    #[test]
    #[ignore]
    fn scan_100k_files() {
        let tmp = tempfile::tempdir().unwrap();
        // 100 个目录 × 1000 个文件
        for d in 0..100 {
            let dir = tmp.path().join(format!("d{d:03}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..1000 {
                std::fs::write(dir.join(format!("f{f:04}.txt")), format!("{d}-{f}")).unwrap();
            }
        }

        let matcher = IgnoreMatcher::with_defaults(tmp.path());
        let start = std::time::Instant::now();
        let result = scan_folder(tmp.path(), &matcher).unwrap();
        let elapsed = start.elapsed();

        // 目录本身也计入（同步目录结构需要），所以是 100000 文件 + 100 目录
        assert_eq!(result.entries.len(), 100_100, "应扫到 10 万文件 + 100 目录");
        let files = result.entries.values().filter(|e| !e.is_dir).count();
        assert_eq!(files, 100_000, "文件数应为 10 万");
        println!("10 万文件扫描耗时: {elapsed:?}");

        // 兜底断言：不能慢到不可用（机械硬盘上会慢些，给足余量）
        assert!(
            elapsed < std::time::Duration::from_secs(180),
            "10 万文件扫描超过 3 分钟: {elapsed:?}"
        );
    }
}
