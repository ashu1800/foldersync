//! 忽略规则：基于 `ignore` crate 的 gitignore 语法。
//!
//! 每个文件夹支持根目录 `.syncignore` + 内置默认规则（可开关由上层拼装）。

use ignore::gitignore::GitignoreBuilder;
use std::path::{Path, PathBuf};

/// 默认内置忽略规则（提示词 4.7）。
pub const DEFAULT_RULES: &[&str] = &[
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    "__pycache__/",
    ".venv/",
    ".idea/",
    "*.swp",
    ".DS_Store",
    "Thumbs.db",
    ".foldersync-tmp-*",
    // 忽略规则本身不参与同步：否则 .syncignore 会被当作普通文件传到对端
    ".syncignore",
    // .git 默认忽略：两端同时写 .git 易损坏仓库，由 git 自己管理
    ".git/",
];

/// 编译好的匹配器。不可变、可克隆、Send+Sync。
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    inner: std::sync::Arc<ignore::gitignore::Gitignore>,
}

impl IgnoreMatcher {
    /// 从规则列表构建。`root` 是文件夹根（gitignore 的相对路径基准）。
    pub fn new(root: &Path, rules: &[String]) -> Self {
        let mut b = GitignoreBuilder::new(root);
        for r in rules {
            // 无效规则忽略并继续（用户手写的规则可能个别有误）
            let _ = b.add_line(None, r);
        }
        let inner = b
            .build()
            .expect("gitignore build 不应失败：add_line 已忽略坏行");
        Self {
            inner: std::sync::Arc::new(inner),
        }
    }

    /// 构建默认规则匹配器。
    pub fn with_defaults(root: &Path) -> Self {
        Self::new(
            root,
            &DEFAULT_RULES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
    }

    /// 判断相对路径（`/` 分隔）是否被忽略。`is_dir` 影响目录后缀规则。
    ///
    /// gitignore 的目录规则（`dir/`）会忽略该目录下所有内容，
    /// 因此对文件的检查要连同其每一级父目录一起判断。
    pub fn matched(&self, rel_path: &str, is_dir: bool) -> bool {
        let full: PathBuf = self.inner.path().join(rel_path);
        if matches!(self.inner.matched(&full, is_dir), ignore::Match::Ignore(_)) {
            return true;
        }
        // 目录规则命中父目录：文件继承忽略状态
        if !is_dir {
            let mut cur = match full.parent() {
                Some(p) => p.to_path_buf(),
                None => return false,
            };
            let root = self.inner.path();
            while cur.starts_with(root) && cur != root {
                if matches!(self.inner.matched(&cur, true), ignore::Match::Ignore(_)) {
                    return true;
                }
                match cur.parent() {
                    Some(p) => cur = p.to_path_buf(),
                    None => break,
                }
            }
        }
        false
    }
}

/// `.syncignore` 里关闭内置默认规则的指令行。
pub const DEFAULTS_OFF: &str = "# foldersync: defaults=off";

/// 读取文件夹的忽略配置：是否启用内置默认规则 + 自定义规则。
pub fn read_syncignore_config(root: &Path) -> (bool, Vec<String>) {
    let text = std::fs::read_to_string(root.join(".syncignore")).unwrap_or_default();
    let use_defaults = !text.lines().any(|l| l.trim() == DEFAULTS_OFF);
    let rules = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    (use_defaults, rules)
}

/// 读取文件夹根目录的 `.syncignore`（不存在则空）。
pub fn read_syncignore(root: &Path) -> Vec<String> {
    read_syncignore_config(root).1
}

/// 无论是否启用默认规则都必须忽略的基础设施文件：
/// `.syncignore` 自己一旦被同步，对端会读到 `defaults=off`，默认保护全部失效；
/// `.git/` 双端同时写会损坏仓库；临时文件不应跨端传播。
pub const MANDATORY_RULES: &[&str] = &[".syncignore", ".foldersync-tmp-*", ".git/"];

/// 默认规则 + .syncignore 合并后的完整匹配器（daemon 扫描入口）。
pub fn matcher_for_folder(root: &Path) -> IgnoreMatcher {
    let (use_defaults, custom) = read_syncignore_config(root);
    let mut rules: Vec<String> = MANDATORY_RULES.iter().map(|s| s.to_string()).collect();
    if use_defaults {
        rules.extend(DEFAULT_RULES.iter().map(|s| s.to_string()));
    }
    rules.extend(custom);
    IgnoreMatcher::new(root, &rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rules_ignore_common_junk() {
        let m = IgnoreMatcher::with_defaults(Path::new("/proj"));
        assert!(m.matched("node_modules/express/index.js", false));
        assert!(m.matched("node_modules", true));
        assert!(m.matched("target/debug/foo.exe", false));
        assert!(m.matched(".DS_Store", false));
        assert!(m.matched("sub/.DS_Store", false));
        assert!(m.matched(".foldersync-tmp-1234", false));
        assert!(m.matched(".git/HEAD", false));
        assert!(!m.matched("src/main.rs", false));
        assert!(!m.matched("node_modules_backup/x", false)); // 前缀相似但不同名
    }

    #[test]
    fn custom_rules_applied() {
        let m = IgnoreMatcher::new(
            Path::new("/p"),
            &["*.log".to_string(), "secret/".to_string()],
        );
        assert!(m.matched("logs/a.log", false));
        assert!(m.matched("secret/key.pem", false));
        assert!(!m.matched("src/a.rs", false));
    }

    #[test]
    fn syncignore_file_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".syncignore"),
            "# 注释\n*.tmp\nbuild_out/\n",
        )
        .unwrap();
        let rules = read_syncignore(tmp.path());
        assert_eq!(rules, vec!["*.tmp".to_string(), "build_out/".to_string()]);
        let m = IgnoreMatcher::new(tmp.path(), &rules);
        assert!(m.matched("a.tmp", false));
    }

    #[test]
    fn defaults_can_be_disabled_via_syncignore_directive() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".syncignore"),
            format!("{DEFAULTS_OFF}\n*.log\n"),
        )
        .unwrap();
        let (use_defaults, rules) = read_syncignore_config(tmp.path());
        assert!(!use_defaults);
        assert_eq!(rules, vec!["*.log".to_string()]);
        let m = matcher_for_folder(tmp.path());
        assert!(m.matched("a.log", false), "自定义规则仍生效");
        assert!(
            !m.matched("node_modules/x.js", false),
            "关闭默认规则后 node_modules 不应被忽略"
        );
        assert!(m.matched(".syncignore", false), "配置文件本身必须始终忽略");
        assert!(m.matched(".git/HEAD", false), ".git 必须始终忽略");
        assert!(
            m.matched(".foldersync-tmp-a", false),
            "临时文件必须始终忽略"
        );
    }
}
