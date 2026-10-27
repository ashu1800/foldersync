//! 双节点集成测试（提示词第 7 节）。
//!
//! 同一进程内起两个完整节点：各自的临时目录、独立存储、独立 blob 库、
//! 独立 `FolderRuntime`。验证一条完整链路：
//!
//! 本地事件 → 索引 + blob → 对端取索引 → diff → 拉 blob → 落盘 → 墓碑
//!
//! **为什么不经 QUIC**：真实 iroh 传输已在 M3/M4 用手工跨机测试验证
//! （家 ↔ 公司，含直连/中继/Tailscale 三种路径）。把它塞进自动化测试会依赖
//! 公网发现服务与端口可用性，产生随机超时。这里用「两节点直连」把同步语义
//! （版本向量、冲突判定、墓碑收敛）测透，网络层验证留在真机。
//!
//! 覆盖提示词 7 节要求的：新增 / 修改 / 删除 / 双端同时修改 /
//! 一端删除另一端修改 / 离线期间变更 / 收敛性。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use fs_sync_core::FileEntry;
use fs_sync_daemon::FolderRuntime;
use fs_sync_store::Store;
use iroh_blobs::store::fs::FsStore;

struct Node {
    #[allow(dead_code)]
    name: String,
    runtime: Arc<FolderRuntime>,
    blobs: Arc<FsStore>,
    _store: Arc<Store>,
    _tmp: tempfile::TempDir,
}

impl Node {
    fn root(&self) -> std::path::PathBuf {
        self.runtime.root_path()
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.root().join(rel)).ok()
    }

    /// 列出所有文件（相对路径，`/` 分隔，已排序）。
    fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        let base = self.root();
        fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else if let Ok(rel) = p.strip_prefix(base) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        walk(&base, &base, &mut out);
        out.sort();
        out
    }
}

async fn node(name: &str) -> Node {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sync");
    std::fs::create_dir_all(&root).unwrap();

    let store = Arc::new(Store::open(&tmp.path().join("state.sqlite3")).unwrap());
    let blobs = Arc::new(
        fs_sync_net::blobs::open_store(&tmp.path().join("blobs"))
            .await
            .unwrap(),
    );

    let runtime = Arc::new(
        FolderRuntime::start(
            "f1".into(),
            root,
            name.into(),
            store.clone(),
            blobs.clone(),
            50,
        )
        .await
        .unwrap(),
    );

    Node {
        name: name.into(),
        runtime,
        blobs,
        _store: store,
        _tmp: tmp,
    }
}

/// 本地写入：产生条目 + 内容进 blob（模拟 watcher + 事件循环）。
async fn write(node: &Node, rel: &str, content: &[u8]) -> FileEntry {
    let path = node.root().join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    let entry = node
        .runtime
        .session
        .on_local_event(fs_sync_daemon::FileEvent {
            path: rel.into(),
            kind: fs_sync_daemon::EventKind::Changed,
        })
        .await
        .unwrap()
        .expect("写文件应产生条目");
    if !entry.is_dir && !entry.deleted {
        node.runtime
            .session
            .engine
            .ensure_blob(&entry)
            .await
            .unwrap();
    }
    entry
}

/// 本地删除：产生墓碑。
async fn remove(node: &Node, rel: &str) -> FileEntry {
    std::fs::remove_file(node.root().join(rel)).unwrap();
    node.runtime
        .session
        .on_local_event(fs_sync_daemon::FileEvent {
            path: rel.into(),
            kind: fs_sync_daemon::EventKind::Removed,
        })
        .await
        .unwrap()
        .expect("删除应产生墓碑")
}

/// 一次同步：`from` 的全量索引推给 `to`，`to` 依 diff 拉取/删除。
async fn sync(from: &Node, to: &Node) -> Result<()> {
    let entries: HashMap<String, FileEntry> =
        from.runtime.session.snapshot().await.into_iter().collect();

    // 用「读自己 blob 库」代替「从对端下载」：内容已在同一台机器上，
    // 语义等价于下载成功，去掉的只是 QUIC 传输这一跳。
    let from_blobs = from.blobs.clone();
    let to_blobs = to.blobs.clone();
    let puller = move |hash: String| {
        let from_blobs = from_blobs.clone();
        let to_blobs = to_blobs.clone();
        async move {
            let bytes = fs_sync_daemon::read_blob_bytes(&from_blobs, &hash).await?;
            fs_sync_net::blobs::add_bytes(&to_blobs, bytes).await?;
            Ok(())
        }
    };

    to.runtime.session.apply_remote(&entries, puller).await?;
    Ok(())
}

async fn sync_both(a: &Node, b: &Node) -> Result<()> {
    sync(a, b).await?;
    sync(b, a).await?;
    Ok(())
}

async fn sync_rounds(a: &Node, b: &Node, rounds: usize) {
    for _ in 0..rounds {
        sync_both(a, b).await.unwrap();
    }
}

fn conflicts(node: &Node) -> Vec<String> {
    node.files()
        .into_iter()
        .filter(|p| p.contains(".sync-conflict-"))
        .collect()
}

// ---------- 测试 ----------

#[tokio::test]
async fn new_file_propagates() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "hello.txt", b"from A").await;
    sync(&a, &b).await.unwrap();

    assert_eq!(b.read("hello.txt").as_deref(), Some(b"from A".as_slice()));
}

#[tokio::test]
async fn modified_file_propagates() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "doc.txt", b"v1").await;
    sync(&a, &b).await.unwrap();
    assert_eq!(b.read("doc.txt").as_deref(), Some(b"v1".as_slice()));

    write(&a, "doc.txt", b"v2 with more content").await;
    sync(&a, &b).await.unwrap();
    assert_eq!(
        b.read("doc.txt").as_deref(),
        Some(b"v2 with more content".as_slice()),
        "修改应传播"
    );
}

#[tokio::test]
async fn deleted_file_propagates_as_tombstone() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "gone.txt", b"bye").await;
    sync(&a, &b).await.unwrap();
    assert!(b.read("gone.txt").is_some(), "先确认 B 收到了");

    remove(&a, "gone.txt").await;
    sync(&a, &b).await.unwrap();

    assert!(b.read("gone.txt").is_none(), "删除应传播（墓碑）");
}

#[tokio::test]
async fn bidirectional_sync_converges() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "only-a.txt", b"AAA").await;
    write(&b, "only-b.txt", b"BBB").await;

    sync_rounds(&a, &b, 2).await;

    assert_eq!(a.read("only-b.txt").as_deref(), Some(b"BBB".as_slice()));
    assert_eq!(b.read("only-a.txt").as_deref(), Some(b"AAA".as_slice()));
}

#[tokio::test]
async fn concurrent_edit_keeps_both_versions() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "shared.txt", b"base").await;
    sync(&a, &b).await.unwrap();

    // 构造真并发：两端都基于同一祖先各自修改，且**改完才同步**。
    // 这里要注意 B 必须先有本地条目（上一步 sync 已建立 {A:1}），
    // 再自增成 {A:1,B:1}；A 自增成 {A:2} —— 互不支配 → 冲突。
    write(&a, "shared.txt", b"edited by A").await;
    write(&b, "shared.txt", b"edited by B").await;
    let entries: HashMap<String, FileEntry> =
        a.runtime.session.snapshot().await.into_iter().collect();
    let fb = a.blobs.clone();
    let tb = b.blobs.clone();
    let puller = move |hash: String| {
        let fb = fb.clone();
        let tb = tb.clone();
        async move {
            let bytes = fs_sync_daemon::read_blob_bytes(&fb, &hash).await?;
            fs_sync_net::blobs::add_bytes(&tb, bytes).await?;
            Ok(())
        }
    };
    let stats = b
        .runtime
        .session
        .apply_remote(&entries, puller)
        .await
        .unwrap();
    eprintln!("A→B apply_remote 统计: {stats:?}");
    eprintln!("一次 A→B 后 B 文件: {:#?}", b.files());

    assert!(
        !conflicts(&a).is_empty() || !conflicts(&b).is_empty(),
        "并发修改应产生冲突副本（A={:?} B={:?}）",
        conflicts(&a),
        conflicts(&b)
    );

    let mut contents: Vec<String> = Vec::new();
    for n in [&a, &b] {
        for f in n.files().iter().filter(|p| p.contains("shared")) {
            if let Some(bytes) = n.read(f) {
                contents.push(String::from_utf8_lossy(&bytes).to_string());
            }
        }
    }
    assert!(
        contents.iter().any(|c| c == "edited by A") && contents.iter().any(|c| c == "edited by B"),
        "两个版本都不能丢，实际: {contents:?}"
    );
}

#[tokio::test]
async fn delete_vs_modify_keeps_file() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "keep.txt", b"original").await;
    sync(&a, &b).await.unwrap();

    remove(&a, "keep.txt").await;
    write(&b, "keep.txt", b"modified by B").await;

    sync_rounds(&a, &b, 2).await;

    let exists = a.read("keep.txt").is_some() || b.read("keep.txt").is_some();
    assert!(exists, "修改应优先于删除，文件不能丢");
}

#[tokio::test]
async fn offline_changes_merge_on_reconnect() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "base.txt", b"base").await;
    sync(&a, &b).await.unwrap();

    write(&a, "offline-a.txt", b"A offline").await;
    write(&b, "offline-b.txt", b"B offline").await;

    sync_rounds(&a, &b, 2).await;

    assert_eq!(
        b.read("offline-a.txt").as_deref(),
        Some(b"A offline".as_slice()),
        "A 的离线变更应合并到 B"
    );
    assert_eq!(
        a.read("offline-b.txt").as_deref(),
        Some(b"B offline".as_slice()),
        "B 的离线变更应合并到 A"
    );
}

#[tokio::test]
async fn nested_directories_are_preserved() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "deep/nested/file.txt", b"nested").await;
    write(&a, "deep/other.txt", b"other").await;
    sync(&a, &b).await.unwrap();

    assert_eq!(
        b.read("deep/nested/file.txt").as_deref(),
        Some(b"nested".as_slice())
    );
    assert_eq!(
        b.read("deep/other.txt").as_deref(),
        Some(b"other".as_slice())
    );
}

#[tokio::test]
async fn repeated_sync_does_not_bump_versions() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "stable.txt", b"content").await;
    sync(&a, &b).await.unwrap();

    let first = b
        .runtime
        .session
        .snapshot()
        .await
        .get("stable.txt")
        .map(|e| e.version.clone());

    sync(&a, &b).await.unwrap();
    sync(&a, &b).await.unwrap();

    let after = b
        .runtime
        .session
        .snapshot()
        .await
        .get("stable.txt")
        .map(|e| e.version.clone());

    assert_eq!(first, after, "无变更的重复同步不应递增版本（否则无限回写）");
    assert_eq!(b.read("stable.txt").as_deref(), Some(b"content".as_slice()));
}

#[tokio::test]
async fn empty_file_syncs() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "empty.txt", b"").await;
    sync(&a, &b).await.unwrap();

    assert_eq!(b.read("empty.txt").as_deref(), Some(b"".as_slice()));
}

#[tokio::test]
async fn final_file_trees_are_identical() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "a1.txt", b"one").await;
    write(&a, "dir/a2.txt", b"two").await;
    write(&b, "b1.txt", b"three").await;
    write(&b, "dir/b2.txt", b"four").await;

    sync_rounds(&a, &b, 3).await;

    let mut fa = a.files();
    let mut fb = b.files();
    fa.retain(|p| !p.contains(".sync-conflict-"));
    fb.retain(|p| !p.contains(".sync-conflict-"));
    assert_eq!(fa, fb, "同步后两端文件树应一致");

    for path in &fa {
        assert_eq!(a.read(path), b.read(path), "文件内容应一致: {path}");
    }
}

#[tokio::test]
async fn conflict_copy_uses_device_name() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "x.txt", b"1").await;
    sync(&a, &b).await.unwrap();
    write(&a, "x.txt", b"from-a").await;
    write(&b, "x.txt", b"from-b").await;
    sync_rounds(&a, &b, 2).await;

    let all = [conflicts(&a), conflicts(&b)].concat();
    assert!(!all.is_empty(), "应有冲突副本");
    // 副本名形如 x.sync-conflict-20261027-071810-<设备名>.txt
    // 设备名来自 SyncSession::device_name（本机是 COMPUTERNAME），不是 device_id；
    // 后者是 64 位 hex，放进文件名会长到无法阅读。
    for c in all {
        assert!(c.contains(".sync-conflict-"), "应含冲突标记: {c}");
        let stem = c.rsplit('.').nth(1).unwrap_or(&c);
        let device = stem.splitn(3, '-').nth(2).unwrap_or("");
        assert!(!device.is_empty(), "冲突副本应带设备名，实际: {c}");
        assert!(
            device.len() < 40,
            "设备名不该是 64 位 hex（说明误传了 device_id）: {device}"
        );
    }
}
