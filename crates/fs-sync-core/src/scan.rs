//! 目录扫描：遍历文件夹生成 FileEntry 列表（含 blake3 哈希），应用忽略规则。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use blake3::Hasher;
use ignore::WalkBuilder;

use crate::entry::FileEntry;
use crate::ignore_mod::IgnoreMatcher;
use crate::version::VersionVector;

/// 待并行计算哈希的文件；遍历阶段只收集元数据，避免大文件夹串行读盘。
struct PendingFile {
    rel: String,
    abs: PathBuf,
    size: u64,
    mtime: i64,
    mode: u32,
}

/// 已知索引快照：size+mtime 一致的文件直接复用旧 hash，不重读文件。
///
/// 之前兜底扫描每 10 分钟对所有文件全量重哈希（10 万文件 ≈ 每次读几十 GB），
/// 磁盘持续被打满。绝大多数文件从未变化；同秒等长写入的漏检风险由
/// `needs_fresh_hash`（mtime 变化才跳过）保持——mtime 完全相同才复用，
/// 外部程序改写文件几乎必然更新 mtime。
pub type KnownIndex = HashMap<String, FileEntry>;

/// 扫描结果。
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// path -> entry（path 为相对路径，`/` 分隔）
    pub entries: HashMap<String, FileEntry>,
    /// 被忽略/跳过的路径（符号链接等），供日志展示
    pub skipped: Vec<String>,
    /// 读取失败的路径（权限不足、被占用等）。调用方必须保留这些路径（及其子路径）
    /// 的旧条目，不能当成"已删除"生成墓碑。
    pub unreadable: Vec<String>,
    /// 待哈希文件（rel, size, mtime, abs）。由 scan_folder / scan_folder_known 消费，
    /// 返回给调用方时已清空。
    pending: Vec<(String, u64, i64, PathBuf, u32)>,
}

impl ScanResult {
    /// `path` 本身或其某个祖先目录读取失败。
    pub fn is_unreadable(&self, path: &str) -> bool {
        self.unreadable.iter().any(|u| {
            path == u
                || (path.len() > u.len()
                    && path.starts_with(u.as_str())
                    && path.as_bytes()[u.len()] == b'/')
        })
    }

    /// `path` 本身或其某个祖先被跳过（符号链接等）。跳过 ≠ 删除：
    /// 常规文件被替换成 symlink 后扫描把它归入 skipped，如果当成"已消失"
    /// 产墓碑，对端会把真实文件删掉。调用方必须保留这些路径的旧条目。
    pub fn is_skipped(&self, path: &str) -> bool {
        self.skipped.iter().any(|s| {
            path == s
                || (path.len() > s.len()
                    && path.starts_with(s.as_str())
                    && path.as_bytes()[s.len()] == b'/')
        })
    }
}

/// 把绝对路径转成相对 root 的 `/` 分隔路径。
fn rel_of(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() { None } else { Some(s) }
}

/// 从 walker 错误里挖出出错的路径（错误可能被 WithDepth 等多层包裹）。
fn error_path(err: &ignore::Error) -> Option<&Path> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path.as_path()),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errs) => errs.iter().find_map(error_path),
        _ => None,
    }
}

/// 扫描整个文件夹。跳过符号链接（初版不同步链接，提示词 4.8）。
///
/// `known`：上一次的索引快照。size+mtime 与已知一致（且非墓碑）的文件
/// 直接复用旧 hash，不产生任何文件读取；其余文件照常哈希。
pub fn scan_folder_known(
    root: &Path,
    matcher: &IgnoreMatcher,
    known: &KnownIndex,
) -> std::io::Result<ScanResult> {
    let mut res = scan_metadata(root, matcher)?;
    // 需要真读文件的：known 里没有 / 已删除 / size 或 mtime 变了
    let mut files: Vec<PendingFile> = Vec::new();
    for (rel, size, mtime, abs, mode) in res.pending.drain(..) {
        if let Some(old) = known.get(&rel)
            && !old.deleted
            && !old.is_dir
            && old.size == size
            && old.mtime == mtime
        {
            res.entries.insert(
                rel.clone(),
                FileEntry {
                    path: rel,
                    size,
                    mtime,
                    hash: old.hash.clone(),
                    is_dir: false,
                    deleted: false,
                    version: VersionVector::new(),
                    modified_by: String::new(),
                    mode: old.mode,
                },
            );
            continue;
        }
        files.push(PendingFile {
            rel,
            abs,
            size,
            mtime,
            mode,
        });
    }
    hash_pending(&mut res, files);
    Ok(res)
}

/// 扫描整个文件夹（无已知索引，全部文件重哈希）。等价 `scan_folder_known(root, m, &{})`。
pub fn scan_folder(root: &Path, matcher: &IgnoreMatcher) -> std::io::Result<ScanResult> {
    let mut res = scan_metadata(root, matcher)?;
    let files = std::mem::take(&mut res.pending)
        .into_iter()
        .map(|(rel, size, mtime, abs, mode)| PendingFile {
            rel,
            abs,
            size,
            mtime,
            mode,
        })
        .collect();
    hash_pending(&mut res, files);
    Ok(res)
}

/// 遍历目录收集元数据；文件部分暂存在 `ScanResult::pending`。
fn scan_metadata(root: &Path, matcher: &IgnoreMatcher) -> std::io::Result<ScanResult> {
    let mut res = ScanResult::default();

    // 关掉 walker 的所有内置过滤（隐藏文件、.gitignore、.ignore 等），
    // 否则 `.env`、被 gitignore 的文件会被当成"已删除"传给对端。
    // 忽略规则统一走我们的 matcher，并在 filter_entry 里剪枝，忽略目录不再往下走。
    let filter_root = root.to_path_buf();
    let filter_matcher = matcher.clone();
    let walker = WalkBuilder::new(root)
        .follow_links(false)
        .hidden(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        .parents(false)
        .filter_entry(move |e| match rel_of(&filter_root, e.path()) {
            Some(rel) => {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                !filter_matcher.matched(&rel, is_dir)
            }
            None => true,
        })
        .build();

    for item in walker {
        let e = match item {
            Ok(e) => e,
            Err(err) => {
                // 读不了的目录/条目：记下来，调用方保留旧条目
                match error_path(&err) {
                    // 根目录本身读不了：整体失败，绝不能返回空结果让调用方全删
                    Some(p) if p == root => {
                        return Err(std::io::Error::other(err.to_string()));
                    }
                    Some(p) => {
                        if let Some(rel) = rel_of(root, p) {
                            res.unreadable.push(rel);
                        }
                    }
                    None => return Err(std::io::Error::other(err.to_string())),
                }
                continue;
            }
        };
        let abs = e.path();
        let rel_str = match rel_of(root, abs) {
            Some(r) => r,
            None => continue, // 根目录本身
        };

        let meta = match e.file_type() {
            Some(ft) if ft.is_symlink() => {
                res.skipped.push(rel_str);
                continue;
            }
            Some(ft) => ft,
            None => continue,
        };

        // 忽略规则已在 filter_entry 里处理（含目录剪枝）
        if meta.is_dir() {
            res.entries.insert(
                rel_str.clone(),
                FileEntry {
                    path: rel_str,
                    size: 0,
                    mtime: 0,
                    hash: String::new(),
                    is_dir: true,
                    deleted: false,
                    version: VersionVector::new(),
                    modified_by: String::new(),
                    mode: 0,
                },
            );
            continue;
        }

        // walker 已带 metadata（follow_links(false) 时是链接目标/本体的 stat）：
        // 不再逐文件二次 std::fs::metadata，百万文件省百万次系统调用。
        // metadata 拿不到（竞态删除）→ 与旧逻辑一致进 unreadable。
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => {
                res.unreadable.push(rel_str);
                continue;
            }
        };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Unix 权限位随条目同步；Windows 上没有等价语义，恒 0
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            md.permissions().mode()
        };
        #[cfg(not(unix))]
        let mode = 0u32;
        res.pending
            .push((rel_str, md.len(), mtime, abs.to_path_buf(), mode));
    }

    Ok(res)
}

/// 对收集到的文件并行哈希并写入 res.entries；失败的进 unreadable。
fn hash_pending(res: &mut ScanResult, files: Vec<PendingFile>) {
    let hashes = hash_pending_files(&files);
    for (file, hash) in files.into_iter().zip(hashes) {
        // 哈希失败（文件被独占锁定等）不能写空 hash，否则会被当成内容变化广播出去
        let Some(hash) = hash else {
            res.unreadable.push(file.rel);
            continue;
        };
        res.entries.insert(
            file.rel.clone(),
            FileEntry {
                path: file.rel,
                size: file.size,
                mtime: file.mtime,
                hash,
                is_dir: false,
                deleted: false,
                version: VersionVector::new(),
                modified_by: String::new(), // 扫描不产生版本，由调用方按文件夹逻辑填充
                mode: file.mode,
            },
        );
    }
}

/// 并行哈希；返回顺序与 `files` 一致，读取失败为 None。
fn hash_pending_files(files: &[PendingFile]) -> Vec<Option<String>> {
    let mut hashes = vec![None; files.len()];
    if files.is_empty() {
        return hashes;
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16)
        .min(files.len());
    if workers <= 1 {
        for (i, file) in files.iter().enumerate() {
            hashes[i] = hash_file(&file.abs).ok();
        }
        return hashes;
    }

    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= files.len() {
                        break;
                    }
                    let _ = tx.send((i, hash_file(&files[i].abs).ok()));
                }
            });
        }
        drop(tx);
        for (i, hash) in rx {
            hashes[i] = hash;
        }
    });
    hashes
}

/// 分块计算文件 blake3 哈希，返回 hex。
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Hasher::new();
    // 8KB（io::copy 默认）会让 blake3 喂不满：百万小文件场景每次 read 都有
    // 系统调用开销。256KB 与 blob 侧一致，机械盘顺序读也更友好。
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
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
    fn parallel_hash_returns_results_in_input_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for i in 0..16 {
            let path = tmp.path().join(format!("{i}.txt"));
            let content = format!("value-{i}");
            std::fs::write(&path, &content).unwrap();
            files.push(PendingFile {
                rel: format!("{i}.txt"),
                abs: path,
                size: content.len() as u64,
                mtime: 0,
                mode: 0,
            });
        }
        let hashes = hash_pending_files(&files);
        assert_eq!(hashes.len(), files.len());
        for (i, hash) in hashes.into_iter().enumerate() {
            assert_eq!(
                hash.unwrap(),
                blake3::hash(format!("value-{i}").as_bytes())
                    .to_hex()
                    .to_string()
            );
        }
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
    fn scan_includes_hidden_and_gitignored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join(".env"), "SECRET=1");
        write(&root.join(".hidden/x.txt"), "x");
        write(&root.join(".gitignore"), "secret.txt\n");
        write(&root.join("secret.txt"), "s");
        write(&root.join("node_modules/lib.js"), "x");

        let m = IgnoreMatcher::with_defaults(root);
        let r = scan_folder(root, &m).unwrap();
        assert!(r.entries.contains_key(".env"), "隐藏文件不能被跳过");
        assert!(r.entries.contains_key(".hidden/x.txt"));
        assert!(
            r.entries.contains_key("secret.txt"),
            ".gitignore 不应影响同步"
        );
        // 我们自己的忽略规则仍然生效，且整棵目录被剪掉
        assert!(!r.entries.contains_key("node_modules"));
        assert!(!r.entries.contains_key("node_modules/lib.js"));
        assert!(r.unreadable.is_empty());
    }

    #[test]
    fn is_unreadable_matches_self_and_children_only() {
        let r = ScanResult {
            unreadable: vec!["locked".into()],
            ..Default::default()
        };
        assert!(r.is_unreadable("locked"));
        assert!(r.is_unreadable("locked/a.txt"));
        assert!(!r.is_unreadable("locked2"));
        assert!(!r.is_unreadable("other"));
    }

    #[test]
    fn is_skipped_matches_self_and_children_only() {
        let r = ScanResult {
            skipped: vec!["link.txt".into()],
            ..Default::default()
        };
        assert!(r.is_skipped("link.txt"));
        assert!(
            r.is_skipped("link.txt/child.txt"),
            "链接目录下的子路径也要保留"
        );
        assert!(!r.is_skipped("link.txt2"));
        assert!(!r.is_skipped("other.txt"));
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
