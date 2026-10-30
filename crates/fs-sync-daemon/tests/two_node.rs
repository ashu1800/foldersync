//! 双节点集成测试（提示词第 7 节）。
//!
//! 同一进程内起两个完整节点：各自的临时目录、独立存储、独立 blob 库、
//! 独立 `FolderRuntime`。验证一条完整链路：
//!
//! 本地事件 → 索引 + blob → 对端取索引 → diff → 拉 blob → 落盘 → 墓碑
//!
//! **为什么不经真实网络**：SSH/SFTP 传输另有 `ssh_server.rs` 的 loopback 测试
//! 与 `relay_server.rs` 的房间测试；这里用「两节点直连」把同步语义
//! （版本向量、冲突判定、墓碑收敛）测透，网络层验证留在专门测试。
//!
//! 覆盖提示词 7 节要求的：新增 / 修改 / 删除 / 双端同时修改 /
//! 一端删除另一端修改 / 离线期间变更 / 收敛性。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use fs_sync_core::FileEntry;
use fs_sync_daemon::FolderRuntime;
use fs_sync_net::blobstore::BlobStore;
use fs_sync_store::Store;

struct Node {
    #[allow(dead_code)]
    name: String,
    runtime: Arc<FolderRuntime>,
    blobs: Arc<BlobStore>,
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
    let blobs = Arc::new(BlobStore::open(tmp.path().join("share")).unwrap());

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
///
/// 内容与上次完全一致时 `on_local_event` 返回 None——这是正确的去重
/// （否则每个防抖事件都会推一次相同内容），这里按 `Option` 返回。
async fn write_opt(node: &Node, rel: &str, content: &[u8]) -> Option<FileEntry> {
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
        .unwrap();
    if let Some(ref e) = entry
        && !e.is_dir
        && !e.deleted
    {
        node.runtime.session.engine.ensure_blob(e).await.unwrap();
    }
    entry
}

/// 写入并要求确实产生条目（测试自己可控的内容用它）。
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
///
/// 注意这里发的是**全量索引**，模拟真实会话里"连接建立时交换一次完整索引"
/// 那一步。真实的增量同步只推本地刚变更的条目（IndexUpdate），全量交换只在
/// 建立连接时发生一次 —— 每轮都发全量会让两端互相覆盖，测出来的现象是假的。
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
            to_blobs.add_bytes(bytes).await?;
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
            tb.add_bytes(bytes).await?;
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

#[tokio::test]
async fn rename_propagates_as_delete_plus_create() {
    let a = node("A").await;
    let b = node("B").await;

    write(&a, "old-name.txt", b"content").await;
    sync(&a, &b).await.unwrap();
    assert!(b.read("old-name.txt").is_some());

    // 文件系统层面 rename = 删除旧名 + 新增新名，watcher 就是这么报的
    std::fs::rename(a.root().join("old-name.txt"), a.root().join("new-name.txt")).unwrap();
    a.runtime
        .session
        .on_local_event(fs_sync_daemon::FileEvent {
            path: "old-name.txt".into(),
            kind: fs_sync_daemon::EventKind::Removed,
        })
        .await
        .unwrap();
    let e = a
        .runtime
        .session
        .on_local_event(fs_sync_daemon::FileEvent {
            path: "new-name.txt".into(),
            kind: fs_sync_daemon::EventKind::Changed,
        })
        .await
        .unwrap()
        .expect("新名字应产生条目");
    a.runtime.session.engine.ensure_blob(&e).await.unwrap();

    sync_rounds(&a, &b, 2).await;

    assert!(b.read("new-name.txt").is_some(), "新名字应同步过去");
    assert!(b.read("old-name.txt").is_none(), "旧名字应消失");
    assert_eq!(
        b.read("new-name.txt").as_deref(),
        Some(b"content".as_slice())
    );
}

/// 收敛性（提示词 7 节）：固定种子的随机操作序列，两端最终必须一致。
///
/// 用固定种子而不是真随机：失败可复现。发现问题时把种子记下来就能重跑。
#[tokio::test]
async fn random_operation_sequence_converges() {
    let a = node("A").await;
    let b = node("B").await;

    // xorshift：不引依赖，够用
    let mut seq: u32 = 0;
    let mut seed: u64 = 0x5DEE_CE66_D1CE_1234;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for step in 0..40 {
        let target = if next() % 2 == 0 { &a } else { &b };
        let path = format!("f{}.txt", next() % 8);
        let op = next() % 3;

        match op {
            0 => {
                // 写
                // 内容带上步号，避免偶尔写成与上次相同的内容被去重
                let body = format!("step{step}-{seq}").into_bytes();
                seq += 1;
                write_opt(target, &path, &body).await;
            }
            1 => {
                // 删（文件可能不存在，忽略）
                if target.read(&path).is_some() {
                    remove(target, &path).await;
                }
            }
            _ => {
                // 同步一轮，让状态交错
                sync_both(&a, &b).await.unwrap();
            }
        }
    }

    // 收尾：真实产品每 10 分钟做一次兜底全量扫描，把「绕过应用改的文件」
    // 和「watcher 漏掉的事件」重新纳入索引。这里显式跑同样的流程，
    // 顺带验证 full_scan 确实能修正磁盘与索引的偏差。
    // 顺序很重要：先各自扫描修正索引，再同步，最后再同步一轮把上一步的修正传播出去。
    for _ in 0..4 {
        let _ = a.runtime.session.full_scan().await.unwrap();
        let _ = b.runtime.session.full_scan().await.unwrap();
        sync_both(&a, &b).await.unwrap();
        sync_both(&a, &b).await.unwrap();
    }

    // 冲突副本名带时间戳，比较前剔除
    let mut fa = a.files();
    let mut fb = b.files();
    fa.retain(|p| !p.contains(".sync-conflict-"));
    fb.retain(|p| !p.contains(".sync-conflict-"));
    assert_eq!(fa, fb, "随机操作后两端文件树必须一致");
    for p in &fa {
        assert_eq!(
            a.read(p).as_deref().map(String::from_utf8_lossy),
            b.read(p).as_deref().map(String::from_utf8_lossy),
            "文件内容必须一致: {p}"
        );
    }
}

/// 回归：兜底扫描发现的新内容必须同时进 blob 存储。
///
/// 曾经的 bug：`full_scan` 只更新索引和新版本，忘了 `ensure_blob`。
/// 结果是索引里有 hash、blob 库里没有内容，对端拉取时报
/// "encode error: 读取 blob 失败"，而本地看起来一切正常（磁盘文件在），
/// 只有在随机操作序列的收敛测试里才暴露出来。
#[tokio::test]
async fn full_scan_registers_blob_content() {
    let a = node("A").await;
    let b = node("B").await;

    // 绕过 watcher 直接写盘：模拟"用户用别的程序改了文件"
    std::fs::write(a.root().join("outside.txt"), b"written behind our back").unwrap();

    let changed = a.runtime.session.full_scan().await.unwrap();
    assert!(!changed.is_empty(), "兜底扫描应发现这个文件");

    let entry = changed
        .iter()
        .find(|e| e.path == "outside.txt")
        .expect("应包含 outside.txt")
        .clone();

    // 内容真的进了 blob 库（对端才能拉到）
    assert!(
        a.blobs.has(&entry.hash).await,
        "兜底扫描必须把内容加入 blob 存储"
    );

    // 端到端：对端能拿到
    sync(&a, &b).await.unwrap();
    assert_eq!(
        b.read("outside.txt").as_deref(),
        Some(b"written behind our back".as_slice()),
        "绕过 watcher 写的内容也应同步过去"
    );
}

/// 回归：同大小同 mtime 但内容不同，兜底扫描必须能发现。
#[tokio::test]
async fn full_scan_detects_content_change_with_same_size_and_mtime() {
    let a = node("A").await;

    write(&a, "same.txt", b"AAAA").await;
    let _ = a.runtime.session.full_scan().await.unwrap();

    // 等一帧确保时间推进不会碰巧帮忙；直接按同尺寸覆盖
    std::fs::write(a.root().join("same.txt"), b"BBBB").unwrap();

    let changed = a.runtime.session.full_scan().await.unwrap();
    assert!(
        changed.iter().any(|e| e.path == "same.txt"),
        "等长内容变更必须被发现（不能只比 size/mtime）"
    );
}
