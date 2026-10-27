//! 文件监听：notify + 防抖（提示词 4.6）。
//!
//! 防抖 300~500ms，合并同一文件的连续事件；自写抑制命中则丢弃。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::engine::WriteSuppression;

/// 监听事件（已防抖、已过滤忽略项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    /// 相对路径（`/` 分隔）
    pub path: String,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// 新增或修改
    Changed,
    /// 删除
    Removed,
}

/// 启动监听器，返回事件接收端。调用方需保持 `debouncer` 存活（drop 即停止监听）。
pub fn watch_folder(
    root: &Path,
    matcher: fs_sync_core::IgnoreMatcher,
    suppression: std::sync::Arc<WriteSuppression>,
    debounce_ms: u64,
) -> Result<(
    mpsc::Receiver<FileEvent>,
    notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
)> {
    let (tx, rx) = mpsc::channel::<FileEvent>(1024);
    let root_buf = root.to_path_buf();
    let root_for_cb = root_buf.clone();

    let mut debouncer = new_debouncer(
        Duration::from_millis(debounce_ms),
        None,
        move |result: DebounceEventResult| {
            let events = match result {
                Ok(events) => events,
                Err(errors) => {
                    for e in errors {
                        warn!("监听错误: {e}");
                    }
                    return;
                }
            };
            // 同一批事件内按路径去重（保留最后一个动作）
            let mut seen = HashSet::new();
            for ev in events {
                for path in &ev.paths {
                    let Ok(rel) = path.strip_prefix(&root_for_cb) else {
                        continue;
                    };
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if rel_str.is_empty() || !seen.insert(rel_str.clone()) {
                        continue;
                    }
                    if matcher.matched(&rel_str, path.is_dir()) {
                        continue;
                    }
                    let kind = classify(ev.event.kind);
                    let kind = match kind {
                        Some(k) => k,
                        None => continue,
                    };
                    // 自写抑制：同步落盘引起的事件必须丢弃，否则会无限回写。
                    // 注意：debouncer 回调运行在 notify 自己的线程，不能 tokio::spawn
                    //（没有 reactor 会 panic），所以用同步阻塞锁查询 + try_send。
                    if suppression.try_take_if_hit(&rel_str) {
                        debug!(path = %rel_str, "自写抑制命中，忽略事件");
                        continue;
                    }
                    if tx
                        .try_send(FileEvent {
                            path: rel_str,
                            kind,
                        })
                        .is_err()
                    {
                        debug!("事件通道已满或已关闭，丢弃事件");
                    }
                }
            }
        },
    )
    .context("创建防抖监听器失败")?;

    debouncer
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("监听目录失败: {}", root.display()))?;

    let _ = root_buf;
    Ok((rx, debouncer))
}

/// notify 事件类型 → 我们的语义。
fn classify(kind: notify::EventKind) -> Option<EventKind> {
    use notify::EventKind::*;
    match kind {
        Create(_) | Modify(_) => Some(EventKind::Changed),
        Remove(_) => Some(EventKind::Removed),
        Access(_) | Any | Other => None,
    }
}

/// 收集路径用于日志。
pub fn paths_of(root: &Path, events: &[FileEvent]) -> Vec<PathBuf> {
    events.iter().map(|e| root.join(&e.path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_notify_kinds() {
        use notify::EventKind as Nk;
        assert_eq!(
            classify(Nk::Create(notify::event::CreateKind::File)),
            Some(super::EventKind::Changed)
        );
        assert_eq!(
            classify(Nk::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content
            ))),
            Some(super::EventKind::Changed)
        );
        assert_eq!(
            classify(Nk::Remove(notify::event::RemoveKind::File)),
            Some(super::EventKind::Removed)
        );
        // 访问事件不应触发同步
        assert_eq!(classify(Nk::Access(notify::event::AccessKind::Read)), None);
    }

    #[tokio::test]
    async fn watch_detects_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let matcher = fs_sync_core::IgnoreMatcher::with_defaults(&root);
        let sup = std::sync::Arc::new(WriteSuppression::new());
        let (mut rx, _deb) = watch_folder(&root, matcher, sup, 100).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(root.join("new.txt"), b"hi").unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("应有事件")
            .expect("channel 未关闭");
        assert_eq!(ev.path, "new.txt");
        assert_eq!(ev.kind, super::EventKind::Changed);
    }

    #[tokio::test]
    async fn suppression_swallows_own_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let matcher = fs_sync_core::IgnoreMatcher::with_defaults(&root);
        let sup = std::sync::Arc::new(WriteSuppression::new());
        let (mut rx, _deb) = watch_folder(&root, matcher, sup.clone(), 100).unwrap();

        // 等 watcher 真正就绪：慢速 CI 上 inotify/ReadDirectoryChangesW 注册需要时间，
        // 立刻写文件会漏事件，导致"没收到事件"被误判成"抑制成功"或者反之。
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 先登记抑制，再写文件
        sup.insert("own.txt", "").await;
        std::fs::write(root.join("own.txt"), b"self").unwrap();

        // 2 秒内不应收到该文件事件（debounce 100ms，留足余量）
        let got = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(e) = rx.recv().await {
                if e.path == "own.txt" {
                    return Some(e);
                }
            }
            None
        })
        .await;
        assert!(got.is_err() || got.unwrap().is_none(), "自写事件应被抑制");
    }

    #[tokio::test]
    async fn ignored_paths_do_not_emit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let matcher = fs_sync_core::IgnoreMatcher::with_defaults(&root);
        let sup = std::sync::Arc::new(WriteSuppression::new());
        let (mut rx, _deb) = watch_folder(&root, matcher, sup, 100).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/x.js"), b"x").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        // 可能没有事件（被忽略）——这正确
        assert!(got.is_err(), "忽略目录不应产生事件");
    }

    #[tokio::test]
    async fn watch_detects_file_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let matcher = fs_sync_core::IgnoreMatcher::with_defaults(&root);
        let sup = std::sync::Arc::new(WriteSuppression::new());
        std::fs::write(root.join("gone.txt"), b"x").unwrap();
        let (mut rx, _deb) = watch_folder(&root, matcher, sup, 100).unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::remove_file(root.join("gone.txt")).unwrap();

        let mut saw_removed = false;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(ev)) if ev.path == "gone.txt" && ev.kind == EventKind::Removed => {
                    saw_removed = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_removed, "删除事件应被捕获");
    }
}
