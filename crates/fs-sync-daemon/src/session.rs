//! 实时同步会话（M4）：监听本地变更 + 接收对端增量 + 墓碑 + 周期兜底扫描。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fs_sync_core::{FileEntry, SyncAction, diff, scan_folder, scan_folder_known};
use fs_sync_net::blobstore::BlobStore;
use futures::stream::StreamExt;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::engine::SyncEngine;
use crate::sync::SyncStats;
use crate::watcher::{EventKind, FileEvent};

/// 实时会话配置。
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// 防抖毫秒数（提示词 4.6：300~500ms）
    pub debounce_ms: u64,
    /// 兜底快扫间隔（提示词 4.6：默认 10 分钟）。
    /// 快扫复用未变文件的 hash（size+mtime 一致不读盘），零 I/O 成本。
    pub full_scan_interval: Duration,
    /// 深度校验间隔：全量重哈希，抓"等长同秒写入"这类 mtime 探测不到的变更。
    /// 默认 6 小时——这是等长改写被发现的最坏延迟，对极端场景可接受
    /// （Syncthing 的扫描间隔同样是小时级）。
    pub deep_scan_interval: Duration,
    /// 墓碑保留天数（提示词 4.4）
    pub tombstone_days: i64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 400,
            full_scan_interval: Duration::from_secs(600),
            deep_scan_interval: Duration::from_secs(6 * 3600),
            tombstone_days: 30,
        }
    }
}

/// 实时同步会话：持有文件夹状态与变更队列。
pub struct SyncSession {
    pub engine: SyncEngine,
    pub folder_id: String,
    pub root: PathBuf,
    pub device_id: String,
    /// 人类可读设备名（冲突副本命名用）
    pub device_name: String,
    pub config: SessionConfig,
    /// 当前索引快照（path → entry）
    index: Arc<Mutex<HashMap<String, FileEntry>>>,
    /// 索引版本号：任何索引变更 +1。中继轮询用它判断是否需要重建/上传索引，
    /// 避免 10 万文件的文件夹每秒全量 snapshot + 序列化一次。
    index_version: Arc<std::sync::atomic::AtomicU64>,
    /// 待发送的增量
    pending: Arc<Mutex<Vec<FileEntry>>>,
    /// 缓存的 blob 存储
    pub blobs: Arc<BlobStore>,
    /// 已提示过的大小写冲突路径：避免每秒全量索引时刷爆日志与活动表
    case_clash_notified: Arc<Mutex<HashSet<String>>>,
    /// 已提示过的单文件失败（时间, "path|detail"）：同错误 10 分钟只记一次 activity
    failure_notified: Arc<std::sync::Mutex<Vec<(std::time::Instant, String)>>>,
    /// 传输限速（桌面端注入；CLI/测试为空实现）
    pub throttle: Arc<std::sync::RwLock<crate::runtime::TransferThrottle>>,
}

impl SyncSession {
    pub fn new(
        engine: SyncEngine,
        folder_id: String,
        root: PathBuf,
        device_id: String,
        blobs: Arc<BlobStore>,
    ) -> Self {
        Self {
            engine,
            folder_id,
            root,
            device_id: device_id.clone(),
            device_name: std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| device_id[..device_id.len().min(8)].to_string()),
            config: SessionConfig::default(),
            index: Arc::new(Mutex::new(HashMap::new())),
            index_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending: Arc::new(Mutex::new(Vec::new())),
            blobs,
            case_clash_notified: Arc::new(Mutex::new(HashSet::new())),
            failure_notified: Arc::new(std::sync::Mutex::new(Vec::new())),
            throttle: Arc::new(std::sync::RwLock::new(
                crate::runtime::TransferThrottle::default(),
            )),
        }
    }

    /// 首次扫描并建立索引（启动时调用）。
    pub async fn initial_scan(&self) -> Result<usize> {
        let root = self.root.clone();
        let folder_id = self.folder_id.clone();
        let device_id = self.device_id.clone();
        let store = self.engine.store.clone();

        let map = tokio::task::spawn_blocking(move || {
            crate::sync::scan_and_index(&root, &folder_id, &device_id, &store)
        })
        .await
        .context("扫描任务失败")??;

        let n = map.len();
        *self.index.lock().await = map;
        info!(entries = n, "初始扫描完成");
        Ok(n)
    }

    /// 处理一条本地文件事件：重新扫描该文件 → 更新索引 → 入待发队列。
    pub async fn on_local_event(&self, ev: FileEvent) -> Result<Option<FileEntry>> {
        let abs = self.root.join(&ev.path);
        match ev.kind {
            EventKind::Removed => self.tombstone_local(&ev.path).await,
            EventKind::Changed => {
                let meta = match std::fs::metadata(&abs) {
                    Ok(m) => m,
                    // 改名的源路径：notify 报的是 Modify(Name)，不会再来 Removed 事件。
                    // 以前这里直接忽略，旧名要等 10 分钟兜底扫描才变墓碑，对端两份都在。
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return self.tombstone_local(&ev.path).await;
                    }
                    // 读不了（权限、占用）≠ 已删除
                    Err(_) => return Ok(None),
                };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                let mut idx = self.index.lock().await;
                let old = idx.get(&ev.path).cloned();

                // 目录：只记录，不需要 hash
                if meta.is_dir() {
                    // 目录里每增删一个文件都会给目录本身报一次 Modify。目录没有内容，
                    // 已经是活目录就不算新版本，否则每次改动都多推一条、版本号一路涨
                    if old.as_ref().is_some_and(|o| o.is_dir && !o.deleted) {
                        return Ok(None);
                    }
                    let mut e = old.unwrap_or_else(|| {
                        FileEntry::new(&ev.path, 0, 0, "", true, &self.device_id)
                    });
                    e.is_dir = true;
                    e.deleted = false;
                    e.version.increment(&self.device_id);
                    e.modified_by = self.device_id.clone();
                    self.engine.store.upsert_entry(&self.folder_id, &e)?;
                    idx.insert(ev.path.clone(), e.clone());
                    self.bump_index_version();
                    self.pending.lock().await.push(e.clone());
                    return Ok(Some(e));
                }

                // 快速路径：size + mtime 未变则跳过哈希
                if let Some(o) = &old
                    && !o.deleted
                    && o.size == meta.len()
                    && o.mtime == mtime
                {
                    return Ok(None);
                }

                // 写入静默检查（quiescence）：文件刚被写过（mtime 距今 < 2 秒）时
                // 内容可能还在写入中（录制、Office 保存、下载），立即哈希会把半成品
                // 当成完整版本推给对端。等待到 mtime 稳定（≥2 秒不动）再哈希。
                // 等待期间 mtime 还在前移 = 真的还在写（OBS 录制、数据库）：
                // 放弃本次事件——写完时 notify 会再来一个事件，自然重新处理。
                // 上限 5 秒兜底，防极端场景无限等待。
                let mut last_seen_mtime = mtime;
                for _ in 0..10 {
                    let age = std::fs::metadata(&abs).ok().and_then(|m| {
                        m.modified().ok().map(|t| {
                            (
                                t.duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(0),
                                t.elapsed().ok(),
                            )
                        })
                    });
                    match age {
                        Some((_, Some(d))) if d >= std::time::Duration::from_secs(2) => break,
                        Some((secs, _)) => {
                            if secs != last_seen_mtime {
                                // mtime 还在前进 = 持续写入，不哈希半成品
                                return Ok(None);
                            }
                            last_seen_mtime = secs;
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        None => break, // 读不了元数据：交给后面的正常错误处理
                    }
                }
                // 等待期间可能被删除/改名
                let meta = match std::fs::metadata(&abs) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return self.tombstone_local(&ev.path).await;
                    }
                    Err(_) => return Ok(None),
                };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                let hash = fs_sync_core::hash_file(&abs)?;
                // 内容没变（拉取落盘后抑制窗口已过、touch 等）：只刷新元数据，不算新版本
                if let Some(o) = &old
                    && !o.deleted
                    && !o.is_dir
                    && o.hash == hash
                {
                    let mut e = o.clone();
                    e.size = meta.len();
                    e.mtime = mtime;
                    self.engine.store.upsert_entry(&self.folder_id, &e)?;
                    idx.insert(ev.path.clone(), e);
                    self.bump_index_version();
                    return Ok(None);
                }
                let mut entry = match old {
                    // 已有条目：递增版本
                    Some(mut o) => {
                        o.version.increment(&self.device_id);
                        o
                    }
                    // 新文件：FileEntry::new 已把版本置为 {device: 1}
                    None => FileEntry::new(
                        &ev.path,
                        meta.len(),
                        mtime,
                        hash.clone(),
                        false,
                        &self.device_id,
                    ),
                };
                entry.size = meta.len();
                entry.mtime = mtime;
                entry.hash = hash;
                entry.is_dir = false;
                entry.deleted = false;
                entry.modified_by = self.device_id.clone();

                self.engine.store.upsert_entry(&self.folder_id, &entry)?;
                idx.insert(ev.path.clone(), entry.clone());
                self.bump_index_version();
                self.pending.lock().await.push(entry.clone());
                debug!(path = %ev.path, "入队变更");
                Ok(Some(entry))
            }
        }
    }

    /// 本地路径已消失：把活条目变成墓碑并入队。已经是墓碑或从未记录过的路径不动。
    async fn tombstone_local(&self, path: &str) -> Result<Option<FileEntry>> {
        let mut idx = self.index.lock().await;
        let Some(old) = idx.get(path).filter(|o| !o.deleted) else {
            return Ok(None);
        };
        let mut tomb = old.clone();
        tomb.deleted = true;
        tomb.version.increment(&self.device_id);
        tomb.modified_by = self.device_id.clone();
        self.engine.store.upsert_entry(&self.folder_id, &tomb)?;
        idx.insert(path.to_string(), tomb.clone());
        self.bump_index_version();
        self.pending.lock().await.push(tomb.clone());
        debug!(path = %path, "入队墓碑");
        Ok(Some(tomb))
    }

    /// 取出并清空待发增量。
    pub async fn take_pending(&self) -> Vec<FileEntry> {
        std::mem::take(&mut *self.pending.lock().await)
    }

    /// 当前待发送的增量条数（概览页「待同步」用）。
    pub async fn pending_len(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// 当前索引快照。
    pub async fn snapshot(&self) -> HashMap<String, FileEntry> {
        self.index.lock().await.clone()
    }

    /// 索引版本号：只在索引内容变化时递增。中继轮询据此跳过无谓的
    /// 全量 snapshot + 序列化（10 万文件每秒一次是不可接受的）。
    pub fn index_version(&self) -> u64 {
        self.index_version
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn bump_index_version(&self) {
        self.index_version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 应用对端的**全量**索引：算出需要拉取的动作并执行。
    /// 本地有、对端索引里没有的条目会排进 pending 推给对端。
    pub async fn apply_remote<F, Fut>(
        &self,
        remote: &HashMap<String, FileEntry>,
        pull_blob: F,
    ) -> Result<SyncStats>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        self.apply_inner(remote, pull_blob, false).await
    }

    /// 应用对端的**增量**（IndexUpdate）：只比对消息里带的路径。
    ///
    /// 增量只含几条变更，"本地有、消息里没有"不代表对端缺。以前增量也走全量比对，
    /// 每收到一条增量就把本地独有的条目全推回去，对端收到后又推回来，两端无限互推。
    pub async fn apply_remote_update<F, Fut>(
        &self,
        remote: &HashMap<String, FileEntry>,
        pull_blob: F,
    ) -> Result<SyncStats>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        self.apply_inner(remote, pull_blob, true).await
    }

    async fn pull_with_retry<F, Fut>(&self, pull_blob: &mut F, hash: &str) -> Result<()>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            match pull_blob(hash.to_string()).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    if attempt < 2 {
                        tokio::time::sleep(std::time::Duration::from_millis(200 * (1 << attempt)))
                            .await;
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("拉取失败")))
    }

    async fn apply_inner<F, Fut>(
        &self,
        remote: &HashMap<String, FileEntry>,
        mut pull_blob: F,
        partial: bool,
    ) -> Result<SyncStats>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let local = self.snapshot().await;
        let actions = if partial {
            let scoped: HashMap<String, FileEntry> = local
                .iter()
                .filter(|(p, _)| remote.contains_key(*p))
                .map(|(p, e)| (p.clone(), e.clone()))
                .collect();
            diff(&scoped, remote)
        } else {
            diff(&local, remote)
        };
        let mut stats = SyncStats::default();
        // Windows / macOS 默认文件系统大小写不敏感：对端的 `Readme.md` 落盘会直接
        // 覆盖本地的 `README.md`。只在这两个平台检查，一轮只建一次索引。
        let mut case_idx =
            cfg!(any(windows, target_os = "macos")).then(|| fs_sync_core::CaseIndex::new(&local));

        // 文件内容拉取是有界并发的（高延迟链路下串行逐个 await 吞吐极差）；
        // 其余动作（删除/建目录/冲突/元数据）操作共享状态，保持串行。
        // 先收集，最后一轮并发执行。
        let mut pull_jobs: Vec<(String, FileEntry)> = Vec::new();
        for action in &actions {
            let path = match action {
                SyncAction::Push { path }
                | SyncAction::MergeMeta { path, .. }
                | SyncAction::Pull { path }
                | SyncAction::Conflict { path, .. } => path,
            };
            if let SyncAction::Pull { .. } = action
                && let Some(entry) = remote.get(path)
                && !entry.deleted
                && !entry.is_dir
            {
                // 命中就跳过，等用户手动改名（目录不检查：落进同一个目录不丢数据）
                if let Some(existing) = case_idx.as_ref().and_then(|ci| ci.clash(path)) {
                    // 中继/全量索引每轮都会重新看到同一条，只提示一次
                    let first = self.case_clash_notified.lock().await.insert(path.clone());
                    if first {
                        warn!(%path, %existing, "对端文件与本地文件仅大小写不同，跳过以免覆盖，请手动改名");
                        let _ = self.engine.store.log_activity(
                            "conflict",
                            &self.folder_id,
                            path,
                            "对端文件与本机文件仅大小写不同，已跳过；请手动改名后重新同步",
                        );
                    } else {
                        debug!(%path, %existing, "大小写冲突仍在，已跳过");
                    }
                    stats.failed += 1;
                    continue;
                }
                // 收集时立即登记：同批 job 之间的 `a.txt` / `A.txt` 也要互相
                // 拦住，否则并发落盘时后者覆盖前者（大小写不敏感文件系统）。
                // 落盘成功后无需重复 insert（results 循环里的 insert 幂等）。
                if let Some(ci) = &mut case_idx {
                    ci.insert(path);
                }
                pull_jobs.push((path.clone(), entry.clone()));
                continue;
            }
            match action {
                SyncAction::Push { path } => {
                    stats.pushed += 1;
                    // 本地是胜者：排入待发队列，由连接循环推送给对端
                    if let Some(entry) = local.get(path) {
                        self.pending.lock().await.push(entry.clone());
                    }
                }
                SyncAction::MergeMeta { path, merged } => {
                    let mut idx = self.index.lock().await;
                    if let Some(e) = idx.get_mut(path) {
                        e.version = merged.clone();
                        self.engine.store.upsert_entry(&self.folder_id, e)?;
                    }
                }
                SyncAction::Pull { path } => {
                    let Some(entry) = remote.get(path) else {
                        continue;
                    };
                    if entry.deleted {
                        match self.engine.apply_delete(path).await {
                            Ok(()) => {
                                if let Some(ci) = &mut case_idx {
                                    ci.remove(path);
                                }
                                self.mark_deleted(path).await;
                                stats.deleted += 1;
                            }
                            Err(e) => {
                                warn!(%path, "删除失败: {e}");
                                self.log_failure(path, &format!("删除失败: {e}"));
                                stats.failed += 1;
                            }
                        }
                        continue;
                    }
                    if entry.is_dir {
                        if self.engine.apply_pulled(entry, "").await.is_ok() {
                            self.set_index_entry(entry.clone()).await;
                            stats.pulled += 1;
                        } else {
                            self.log_failure(path, "建目录失败");
                            stats.failed += 1;
                        }
                        continue;
                    }
                    unreachable!("文件拉取应已进并发队列");
                }
                SyncAction::Conflict {
                    path,
                    winner_side,
                    winner,
                    loser,
                } => {
                    self.apply_conflict_action(
                        path,
                        *winner_side,
                        winner,
                        loser,
                        &mut pull_blob,
                        &mut stats,
                    )
                    .await;
                }
            }
        }

        // 有界并发拉取文件内容（4 路）：传输是网络 I/O 密集，互不依赖；
        // 落盘在 apply_pulled 里是原子写，并发安全。
        // pull_blob 是 FnMut（内含连接缓存），用 Arc<Mutex> 共享：锁只包住
        // "调用闭包拿 future"的瞬间，网络传输本身在锁外并发进行。
        const PULL_CONCURRENCY: usize = 4;
        let puller = Arc::new(tokio::sync::Mutex::new(pull_blob));
        let results = futures::stream::iter(pull_jobs)
            .map(|(_path, entry)| {
                let this = &self;
                let puller = puller.clone();
                let throttle = self.throttle.read().map(|t| t.clone()).unwrap_or_default();
                async move {
                    // 带退避重试（1 → 2 → 3 次），每次尝试在锁内生成 future
                    let mut last_err: Option<anyhow::Error> = None;
                    let mut pulled = Err(anyhow::anyhow!("拉取未开始"));
                    for attempt in 0..3 {
                        let fut = {
                            let mut guard = puller.lock().await;
                            guard(entry.hash.clone())
                        };
                        match fut.await {
                            Ok(()) => {
                                pulled = Ok(());
                                break;
                            }
                            Err(e) => {
                                last_err = Some(e);
                                if attempt < 2 {
                                    tokio::time::sleep(std::time::Duration::from_millis(
                                        200 * (1 << attempt),
                                    ))
                                    .await;
                                }
                            }
                        }
                    }
                    match pulled {
                        Ok(()) => {
                            throttle.consume_download(entry.size).await;
                            match this.engine.apply_pulled(&entry, &entry.hash).await {
                                Ok(()) => Ok(entry),
                                Err(e) => {
                                    warn!(path = %entry.path, "落盘失败: {e}");
                                    this.log_failure(&entry.path, &format!("落盘失败: {e}"));
                                    Err(())
                                }
                            }
                        }
                        Err(_) => {
                            let detail = format!(
                                "拉取失败: {:#}",
                                last_err.unwrap_or_else(|| anyhow::anyhow!("未知错误"))
                            );
                            warn!(path = %entry.path, "{detail}");
                            this.log_failure(&entry.path, &detail);
                            Err(())
                        }
                    }
                }
            })
            .buffer_unordered(PULL_CONCURRENCY)
            .collect::<Vec<std::result::Result<FileEntry, ()>>>()
            .await;
        for result in results {
            match result {
                Ok(entry) => {
                    if let Some(ci) = &mut case_idx {
                        ci.insert(&entry.path);
                    }
                    self.set_index_entry(entry).await;
                    stats.pulled += 1;
                }
                Err(()) => stats.failed += 1,
            }
        }
        Ok(stats)
    }

    /// 处理一个冲突动作：删除 vs 修改按"修改优先于删除"保留文件；
    /// 双改按胜者落原路径、败者存副本，并合并版本向量让双端收敛。
    async fn apply_conflict_action<F, Fut>(
        &self,
        path: &str,
        winner_side: fs_sync_core::Side,
        winner: &FileEntry,
        loser: &FileEntry,
        pull_blob: &mut F,
        stats: &mut SyncStats,
    ) where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        // diff 产出的 winner/loser 已经是按 winner_side 挑好的，不能再按 side 对调
        let (win, lose) = (winner, loser);
        let win_local = matches!(winner_side, fs_sync_core::Side::Local);

        // 删除 vs 修改 的冲突（提示词 4.4：修改优先于删除）：
        // 删除侧没有内容可拉，直接保留文件侧，不产生副本、不丢数据。
        if win.deleted || lose.deleted {
            let keep = if win.deleted { lose } else { win };
            if keep.deleted {
                // 两侧都是删除 → 一致，无需动作
                stats.deleted += 1;
            } else {
                // 保留文件侧：合并版本向量，让双端收敛到"文件存在"
                let mut merged = keep.clone();
                merged.version = fs_sync_core::merge(&win.version, &lose.version);
                merged.version.increment(&self.device_id);
                merged.deleted = false;
                merged.modified_by = self.device_id.clone();
                self.set_index_entry(merged.clone()).await;
                let _ = self.engine.store.upsert_entry(&self.folder_id, &merged);
                self.pending.lock().await.push(merged);
                info!(%path, "删除/修改冲突：保留文件（修改优先于删除）");
                stats.conflicts += 1;
            }
            return;
        }

        // 本地一侧的内容在本地文件里，入 blob 库即可；只有远端一侧才需要拉取
        let (win_got, lose_got) = if win_local {
            (
                self.engine.ensure_blob(win).await.map(|_| ()),
                self.pull_with_retry(pull_blob, &lose.hash).await,
            )
        } else {
            (
                self.pull_with_retry(pull_blob, &win.hash).await,
                self.engine.ensure_blob(lose).await.map(|_| ()),
            )
        };
        if let Err(e) = win_got {
            warn!(%path, "冲突胜者内容不可得: {e}");
            stats.failed += 1;
            return;
        }
        let loser_bytes = match lose_got {
            Ok(()) => crate::sync::read_blob_bytes(&self.blobs, &lose.hash)
                .await
                .ok(),
            Err(e) => {
                // 拿不到落败版本内容不是致命错误——apply_conflict 会把
                // 本地现有文件改名保住（见 engine.rs）。这里记一条日志，
                // 不要因为拉取失败就跳过整个冲突处理。
                warn!(%path, "落败版本内容不可读，将保留本地文件为副本: {e}");
                None
            }
        };
        match self
            .engine
            .apply_conflict(win, loser_bytes, &win.hash, &self.device_name)
            .await
        {
            Ok(copy) => {
                info!(%path, copy = %copy, "冲突已处理，副本另存");
                // 关键：冲突后必须把「双方版本合并」写进本地索引并递增，
                // 否则下一轮仍被判为并发冲突，会不断产生新副本。
                let mut merged = win.clone();
                merged.version = fs_sync_core::merge(&win.version, &lose.version);
                merged.version.increment(&self.device_id);
                merged.modified_by = self.device_id.clone();
                merged.deleted = false;
                self.set_index_entry(merged.clone()).await;
                let _ = self.engine.store.upsert_entry(&self.folder_id, &merged);
                // 副本也进内存索引：SQLite 里有了（engine 落的），但
                // build_index/全量索引走 snapshot()（内存），不进的话
                // 对端永远看不到这份副本。
                if let Some(copy_entry) = self
                    .engine
                    .store
                    .list_entries(&self.folder_id)
                    .ok()
                    .and_then(|es| es.into_iter().find(|e| e.path == copy))
                {
                    self.set_index_entry(copy_entry).await;
                }
                self.pending.lock().await.push(merged);
                stats.conflicts += 1;
            }
            Err(e) => {
                warn!(%path, "冲突处理失败: {e}");
                stats.failed += 1;
            }
        }
    }

    /// 测试辅助：直接替换内存索引。
    #[cfg(test)]
    pub async fn force_index(&self, map: HashMap<String, FileEntry>) {
        *self.index.lock().await = map;
    }

    /// 单文件同步失败落 activity 表（kind=error），让用户在活动页看到
    /// "哪个文件、为什么失败"，而不是只有一行"N 个文件失败"。
    /// 同一文件同一错误 10 分钟内只记一次——中继/全量索引每轮都会重试，
    /// 不去重会刷爆活动表。
    fn log_failure(&self, path: &str, detail: &str) {
        let mut seen = self
            .failure_notified
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        seen.retain(|(t, _)| now.duration_since(*t) < std::time::Duration::from_secs(600));
        let key = format!("{path}|{detail}");
        if seen.iter().any(|(_, k)| *k == key) {
            return;
        }
        seen.push((now, key));
        let _ = self
            .engine
            .store
            .log_activity("error", &self.folder_id, path, detail);
    }

    async fn set_index_entry(&self, entry: FileEntry) {
        self.index.lock().await.insert(entry.path.clone(), entry);
        self.bump_index_version();
    }

    /// 记录远端墓碑：本地索引与数据库都标记为已删除。
    async fn mark_deleted(&self, path: &str) {
        let mut idx = self.index.lock().await;
        let mut entry = idx.get(path).cloned().unwrap_or_else(|| FileEntry {
            path: path.to_string(),
            size: 0,
            mtime: 0,
            hash: String::new(),
            is_dir: false,
            deleted: true,
            version: fs_sync_core::VersionVector::new(),
            modified_by: self.device_id.clone(),
            mode: 0,
        });
        entry.deleted = true;
        entry.version.increment(&self.device_id);
        entry.modified_by = self.device_id.clone();
        idx.insert(path.to_string(), entry.clone());
        let _ = self.engine.store.upsert_entry(&self.folder_id, &entry);
        self.bump_index_version();
    }

    /// 兜底扫描。
    ///
    /// `deep = true`：全量重哈希（慢，抓等长同秒写入）；`deep = false`：
    /// size+mtime 一致的文件复用索引 hash，不读盘（快，每 10 分钟跑）。
    /// 之前每次兜底扫描对所有文件重读重哈希，10 万文件 = 每次读几十 GB，
    /// 磁盘持续被打满。等长同秒写入由 6 小时一次的深扫兜底。
    pub async fn full_scan(&self) -> Result<Vec<FileEntry>> {
        self.full_scan_inner(false).await
    }

    /// 深度校验扫描：无视已知索引，所有文件重哈希。
    pub async fn deep_scan(&self) -> Result<Vec<FileEntry>> {
        self.full_scan_inner(true).await
    }

    async fn full_scan_inner(&self, deep: bool) -> Result<Vec<FileEntry>> {
        let root = self.root.clone();
        let matcher = fs_sync_core::matcher_for_folder(&root);
        let matcher_for_gone = matcher.clone();
        let known: fs_sync_core::KnownIndex = self.index.lock().await.clone();
        let scan_res = if deep {
            tokio::task::spawn_blocking(move || scan_folder(&root, &matcher)).await
        } else {
            tokio::task::spawn_blocking(move || scan_folder_known(&root, &matcher, &known)).await
        };
        let mut scan = scan_res.context("兜底扫描任务失败")??;
        if !scan.unreadable.is_empty() {
            warn!(count = scan.unreadable.len(), first = %scan.unreadable[0], "兜底扫描有路径读取失败，保留旧条目");
        }

        let mut idx = self.index.lock().await;
        let mut changed = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for (path, mut fresh) in std::mem::take(&mut scan.entries) {
            seen.insert(path.clone());
            // 判等不能只看 size + mtime：外部程序在同一秒内写入等长内容时三者全同，
            // 会漏掉真实变更，导致「索引说已同步、磁盘还是旧内容」而且永不修正。
            // 大小和时间戳都相同才去比对 hash（扫一遍已经算过 hash，直接可用）。
            let unchanged = match idx.get(&path) {
                Some(old) => {
                    !old.deleted
                        && old.size == fresh.size
                        && old.mtime == fresh.mtime
                        && old.is_dir == fresh.is_dir
                        && old.hash == fresh.hash
                }
                None => false,
            };
            // 内容相同、仅 mtime/size 元数据不同（拉取落盘、touch）：不是新版本，
            // 只刷新本地记录，不递增版本、不广播，否则两端会来回互推
            let meta_only = !unchanged
                && matches!(idx.get(&path), Some(old)
                    if !old.deleted && old.is_dir == fresh.is_dir && old.hash == fresh.hash);
            match idx.get(&path) {
                _ if unchanged => {
                    // 未变化
                }
                Some(old) if meta_only => {
                    fresh.version = old.version.clone();
                    fresh.modified_by = old.modified_by.clone();
                    self.engine.store.upsert_entry(&self.folder_id, &fresh)?;
                    idx.insert(path.clone(), fresh);
                }
                old => {
                    fresh.version = old.map(|o| o.version.clone()).unwrap_or_default();
                    fresh.version.increment(&self.device_id);
                    fresh.modified_by = self.device_id.clone();
                    self.engine.store.upsert_entry(&self.folder_id, &fresh)?;
                    idx.insert(path.clone(), fresh.clone());
                    changed.push(fresh);
                }
            }
        }

        // 关键：扫描发现的新内容必须进 blob 存储，否则索引里有 hash、
        // 对端却拉不到内容（报 "encode error: 读取 blob 失败"）。
        // watcher 路径靠 ensure_blob 做这件事，兜底扫描漏了就会让对端永久卡住。
        let to_blob: Vec<FileEntry> = changed
            .iter()
            .filter(|e| !e.is_dir && !e.deleted)
            .cloned()
            .collect();
        drop(idx);
        for e in to_blob {
            if let Err(err) = self.engine.ensure_blob(&e).await {
                warn!(path = %e.path, "兜底扫描加入 blob 失败: {err}");
            }
        }
        let mut idx = self.index.lock().await;

        // 扫描中消失的 → 墓碑；但被忽略规则排除的不算删除，直接移除索引
        let ignored_gone: Vec<String> = idx
            .iter()
            .filter(|(p, e)| {
                !e.deleted
                    && !seen.contains(*p)
                    && !scan.is_unreadable(p)
                    && matcher_for_gone.matched(p, e.is_dir)
            })
            .map(|(p, _)| p.clone())
            .collect();
        for path in ignored_gone {
            idx.remove(&path);
            let _ = self.engine.store.delete_entry(&self.folder_id, &path);
        }
        let gone: Vec<String> = idx
            .iter()
            // 读不了的路径不算消失：读失败 ≠ 已删除；
            // 被跳过的路径（符号链接）也不算消失：链接 ≠ 已删除，
            // 否则常规文件被替换成 symlink 后，对端会把真实文件删掉。
            .filter(|(p, e)| {
                !e.deleted && !seen.contains(*p) && !scan.is_unreadable(p) && !scan.is_skipped(p)
            })
            .map(|(p, _)| p.clone())
            .collect();
        for path in gone {
            if let Some(old) = idx.get(&path) {
                let mut tomb = old.clone();
                tomb.deleted = true;
                tomb.version.increment(&self.device_id);
                tomb.modified_by = self.device_id.clone();
                self.engine.store.upsert_entry(&self.folder_id, &tomb)?;
                idx.insert(path, tomb.clone());
                changed.push(tomb);
            }
        }

        if !changed.is_empty() {
            info!(count = changed.len(), "兜底扫描发现变更");
            self.bump_index_version();
        }
        Ok(changed)
    }

    /// 清理过期墓碑（提示词 4.4：默认保留 30 天）。
    ///
    /// 判据是**删除时间**（entries.deleted_at，由 store 层在置墓碑时自动记录），
    /// 不是文件 mtime。以前用 mtime：删除一个 31 天没动过的文件，墓碑 10 分钟内
    /// 就被清掉，对端离线回来后按"本地有、远端无"把文件复活，删除永久丢失。
    pub fn purge_expired_tombstones(&self) -> Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff = now - self.config.tombstone_days * 86400;
        let expired = self
            .engine
            .store
            .list_expired_tombstones(&self.folder_id, cutoff)?;
        for path in &expired {
            self.engine.store.delete_entry(&self.folder_id, path)?;
        }
        if !expired.is_empty() {
            info!(purged = expired.len(), "清理过期墓碑");
        }
        Ok(expired.len())
    }
}

/// 事件处理循环：从 watcher 接收事件并落到会话。
pub async fn run_event_loop(
    session: Arc<SyncSession>,
    mut rx: mpsc::Receiver<FileEvent>,
    on_change: impl Fn(Vec<FileEntry>) + Send + Sync + 'static,
) {
    let on_change = Arc::new(on_change);
    while let Some(ev) = rx.recv().await {
        match session.on_local_event(ev).await {
            Ok(Some(entry)) => {
                // 立即把内容放进 blob 存储，供对端拉取
                if !entry.is_dir
                    && !entry.deleted
                    && let Err(e) = session.engine.ensure_blob(&entry).await
                {
                    warn!(path = %entry.path, "加入 blob 存储失败: {e}");
                }
                let pending = session.take_pending().await;
                if !pending.is_empty() {
                    on_change(pending);
                }
            }
            Ok(None) => {}
            Err(e) => warn!("处理本地事件失败: {e}"),
        }
    }
}

/// 周期任务：兜底快扫（复用 hash，零读盘）+ 低频深扫（全量重哈希）+ 墓碑清理。
pub async fn run_periodic(
    session: Arc<SyncSession>,
    on_change: impl Fn(Vec<FileEntry>) + Send + Sync + 'static,
) {
    let on_change = Arc::new(on_change);
    let mut interval = tokio::time::interval(session.config.full_scan_interval);
    interval.tick().await; // 跳过立即触发
    let mut deep = tokio::time::interval(session.config.deep_scan_interval);
    deep.tick().await; // 跳过立即触发
    let mut deep_due = false;
    loop {
        tokio::select! {
            _ = deep.tick() => deep_due = true,
            _ = interval.tick() => {}
        }
        // 深扫 tick 到了：这一轮改跑深扫（与快扫重叠时顺带覆盖快扫职责）
        let do_deep = std::mem::take(&mut deep_due);
        let scan_result = if do_deep {
            session.deep_scan().await
        } else {
            session.full_scan().await
        };
        match scan_result {
            Ok(changed) if !changed.is_empty() => {
                for e in &changed {
                    if !e.is_dir && !e.deleted {
                        let _ = session.engine.ensure_blob(e).await;
                    }
                }
                on_change(changed);
            }
            Ok(_) if do_deep => debug!("深扫完成，无变化"),
            Ok(_) => debug!("兜底扫描无变化"),
            Err(e) => warn!("兜底扫描失败: {e}"),
        }
        if let Err(e) = session.purge_expired_tombstones() {
            warn!("墓碑清理失败: {e}");
        }
    }
}

/// 便捷：为一组待发条目生成 IndexUpdate 消息。
pub fn to_index_update(folder_id: &str, entries: Vec<FileEntry>) -> fs_sync_net::message::Message {
    fs_sync_net::message::Message::IndexUpdate {
        folder_id: folder_id.to_string(),
        entries,
    }
}

/// 供上层构造"推送器"：把本地条目推给对端指定设备。
pub type PushFn = Arc<dyn Fn(String, Vec<FileEntry>) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use fs_sync_core::VersionVector;
    use fs_sync_store::Store;

    async fn session() -> (Arc<SyncSession>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sync");
        std::fs::create_dir_all(&root).unwrap();
        let blobs = Arc::new(BlobStore::open(tmp.path().join("share")).unwrap());
        let store = Arc::new(Store::open_in_memory().unwrap());
        let engine = SyncEngine::new("f1".into(), root.clone(), store, blobs.clone());
        let s = SyncSession::new(engine, "f1".into(), root, "devA".into(), blobs);
        (Arc::new(s), tmp)
    }

    fn entry(path: &str, hash: &str, dev: &str, counter: u64) -> FileEntry {
        let mut vv = VersionVector::new();
        for _ in 0..counter {
            vv.increment(dev);
        }
        FileEntry {
            path: path.into(),
            size: 3,
            mtime: 1,
            hash: hash.into(),
            is_dir: false,
            deleted: false,
            version: vv,
            modified_by: dev.into(),
            mode: 0,
        }
    }

    #[tokio::test]
    async fn local_event_bumps_version_and_queues() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("a.txt"), b"hello").unwrap();

        let entry = s
            .on_local_event(FileEvent {
                path: "a.txt".into(),
                kind: EventKind::Changed,
            })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(entry.path, "a.txt");
        assert_eq!(entry.version.get("devA"), 1);
        assert!(!entry.deleted);
        assert_eq!(s.take_pending().await.len(), 1);
        // 已落库
        assert_eq!(s.engine.store.list_entries("f1").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unchanged_file_event_is_noop() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("a.txt"), b"hello").unwrap();
        s.on_local_event(FileEvent {
            path: "a.txt".into(),
            kind: EventKind::Changed,
        })
        .await
        .unwrap();
        s.take_pending().await;

        // 同 size+mtime → 跳过（快速路径）
        let again = s
            .on_local_event(FileEvent {
                path: "a.txt".into(),
                kind: EventKind::Changed,
            })
            .await
            .unwrap();
        assert!(again.is_none(), "未变化不应产生新版本");
    }

    #[tokio::test]
    async fn removal_creates_tombstone_with_bumped_version() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("a.txt"), b"hello").unwrap();
        let first = s
            .on_local_event(FileEvent {
                path: "a.txt".into(),
                kind: EventKind::Changed,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.version.get("devA"), 1);

        std::fs::remove_file(s.root.join("a.txt")).unwrap();
        let tomb = s
            .on_local_event(FileEvent {
                path: "a.txt".into(),
                kind: EventKind::Removed,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(tomb.deleted);
        assert_eq!(tomb.version.get("devA"), 2, "墓碑版本必须递增");
    }

    /// 回归：墓碑过期按删除时间（deleted_at）判定，不再按文件 mtime。
    /// 删一个 mtime 很老（31 天未动）的文件，墓碑必须立刻可保留满 30 天，
    /// 不能在下一轮周期任务里就被清掉（否则对端离线回来文件复活）。
    #[tokio::test]
    async fn old_file_tombstone_survives_by_delete_time_not_mtime() {
        let (s, _t) = session().await;
        // 写入后把磁盘 mtime 拨回 40 天前，模拟"很久没动过的文件"
        let p = s.root.join("stale.txt");
        std::fs::write(&p, b"stale").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86400);
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(old).unwrap();
        drop(f);

        s.on_local_event(FileEvent {
            path: "stale.txt".into(),
            kind: EventKind::Changed,
        })
        .await
        .unwrap();

        std::fs::remove_file(&p).unwrap();
        let tomb = s
            .on_local_event(FileEvent {
                path: "stale.txt".into(),
                kind: EventKind::Removed,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(tomb.deleted);
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            tomb.mtime < now_secs - 30 * 86400,
            "前置条件：文件 mtime 在 30 天前（mtime={}</{})",
            tomb.mtime,
            now_secs - 30 * 86400
        );

        // 现在（tombstone_days=30）清理：文件 mtime 已超期，但删除时间是现在，
        // 墓碑必须存活。旧实现按 mtime 判定会在这里把它清掉。
        let purged = s.purge_expired_tombstones().unwrap();
        assert_eq!(purged, 0, "按删除时间判定，刚产生的墓碑不应过期");
        assert!(
            s.engine
                .store
                .list_entries("f1")
                .unwrap()
                .iter()
                .any(|e| e.path == "stale.txt" && e.deleted),
            "墓碑必须还在，否则对端离线回来会复活文件"
        );
    }

    #[tokio::test]
    async fn full_scan_detects_offline_changes() {
        let (s, _t) = session().await;
        s.initial_scan().await.unwrap();
        // 模拟监听漏掉的事件：直接写文件
        std::fs::write(s.root.join("missed.txt"), b"offline").unwrap();

        let changed = s.full_scan().await.unwrap();
        assert!(
            changed.iter().any(|e| e.path == "missed.txt"),
            "兜底扫描应发现漏掉的文件"
        );
    }

    /// 回归：拉取落盘后 mtime 要对齐对端，且之后的兜底扫描 / 监听事件
    /// 不能把"同内容只改了 mtime"当成新版本推回去（两端互推）。
    #[tokio::test]
    async fn pulled_file_does_not_bounce_back() {
        let (s, _t) = session().await;
        s.initial_scan().await.unwrap();
        let hash = s.blobs.add_bytes(b"abc".to_vec()).await.unwrap();
        let mut remote = entry("p.txt", &hash, "devB", 1);
        remote.mtime = 1_600_000_000;
        let stats = s
            .apply_remote(
                &HashMap::from([("p.txt".to_string(), remote.clone())]),
                |_h| async { Ok(()) },
            )
            .await
            .unwrap();
        assert_eq!(stats.pulled, 1);
        s.take_pending().await;

        let disk_mtime = std::fs::metadata(s.root.join("p.txt"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(disk_mtime, remote.mtime, "落盘文件 mtime 应对齐对端");

        assert!(
            s.full_scan().await.unwrap().is_empty(),
            "拉取后兜底扫描不应产生变更"
        );

        // 即便 mtime 被改（touch），内容不变也不算新版本
        let f = std::fs::File::options()
            .write(true)
            .open(s.root.join("p.txt"))
            .unwrap();
        f.set_modified(std::time::SystemTime::now()).unwrap();
        drop(f);
        assert!(
            s.full_scan().await.unwrap().is_empty(),
            "仅 mtime 变化不应递增版本"
        );
        let ev = s
            .on_local_event(FileEvent {
                path: "p.txt".into(),
                kind: EventKind::Changed,
            })
            .await
            .unwrap();
        assert!(ev.is_none());
        let snap: HashMap<String, FileEntry> = s.snapshot().await.into_iter().collect();
        assert_eq!(snap["p.txt"].version, remote.version, "版本不应被本端递增");
        assert!(s.take_pending().await.is_empty());
    }

    /// 回归：增量只比对消息里的路径。曾经增量也走全量比对，本地独有的条目
    /// 每收到一条增量就被推回去一次，两端无限互推（真机上每秒约 10 条）。
    #[tokio::test]
    async fn partial_update_does_not_push_unrelated_local_entries() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("mine.txt"), b"local only").unwrap();
        s.initial_scan().await.unwrap();
        s.take_pending().await;

        let hash = s.blobs.add_bytes(b"theirs".to_vec()).await.unwrap();
        let update = HashMap::from([("t.txt".to_string(), entry("t.txt", &hash, "devB", 1))]);
        let stats = s
            .apply_remote_update(&update, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(stats.pulled, 1);
        assert_eq!(stats.pushed, 0, "增量不应把无关的本地条目推回去");
        assert!(s.take_pending().await.is_empty());

        // 全量索引里没有 mine.txt：这时才该推
        let full = s
            .apply_remote(&update, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(full.pushed, 1);
    }

    /// 回归：改名时 notify 对源路径报 Modify(Name) 而不是 Remove，
    /// 源路径必须当场变墓碑，否则对端要等兜底扫描才删旧名。
    #[tokio::test]
    async fn rename_source_becomes_tombstone() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("old.txt"), b"x").unwrap();
        s.initial_scan().await.unwrap();
        std::fs::rename(s.root.join("old.txt"), s.root.join("new.txt")).unwrap();

        let ev = s
            .on_local_event(FileEvent {
                path: "old.txt".into(),
                kind: EventKind::Changed,
            })
            .await
            .unwrap();
        assert!(ev.is_some_and(|e| e.deleted), "源路径应变墓碑");
        // 重复事件不应再递增
        let again = s
            .on_local_event(FileEvent {
                path: "old.txt".into(),
                kind: EventKind::Removed,
            })
            .await
            .unwrap();
        assert!(again.is_none(), "已是墓碑不应重复入队");
    }

    /// 回归：目录里增删文件会给目录报 Modify，已存在的活目录不应每次都递增版本。
    #[tokio::test]
    async fn existing_dir_change_does_not_bump_version() {
        let (s, _t) = session().await;
        std::fs::create_dir(s.root.join("d")).unwrap();
        s.initial_scan().await.unwrap();
        let before = s.snapshot().await["d"].version.clone();
        let ev = s
            .on_local_event(FileEvent {
                path: "d".into(),
                kind: EventKind::Changed,
            })
            .await
            .unwrap();
        assert!(ev.is_none());
        assert_eq!(s.snapshot().await["d"].version, before);
    }

    /// 大小写不敏感平台：对端的 `Readme.md` 不能覆盖本地的 `README.md`。
    #[cfg(any(windows, target_os = "macos"))]
    #[tokio::test]
    async fn case_clash_does_not_overwrite_local() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("README.md"), b"mine").unwrap();
        s.initial_scan().await.unwrap();
        let hash = s.blobs.add_bytes(b"theirs".to_vec()).await.unwrap();
        let remote = HashMap::from([(
            "Readme.md".to_string(),
            entry("Readme.md", &hash, "devB", 1),
        )]);

        let stats = s
            .apply_remote(&remote, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(stats.pulled, 0, "大小写冲突的文件不应落盘");
        assert_eq!(std::fs::read(s.root.join("README.md")).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn full_scan_creates_tombstones_for_deleted() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("gone.txt"), b"x").unwrap();
        s.initial_scan().await.unwrap();
        std::fs::remove_file(s.root.join("gone.txt")).unwrap();

        let changed = s.full_scan().await.unwrap();
        let tomb = changed.iter().find(|e| e.path == "gone.txt").unwrap();
        assert!(tomb.deleted);
    }

    #[tokio::test]
    async fn full_scan_drops_newly_ignored_without_tombstone() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("secret.log"), b"x").unwrap();
        s.initial_scan().await.unwrap();
        assert!(s.snapshot().await.contains_key("secret.log"));

        std::fs::write(s.root.join(".syncignore"), "*.log\n").unwrap();
        let changed = s.full_scan().await.unwrap();
        assert!(
            changed.iter().all(|e| e.path != "secret.log"),
            "被忽略的文件不能进墓碑队列: {changed:?}"
        );
        assert!(!s.snapshot().await.contains_key("secret.log"));
        assert!(
            s.engine
                .store
                .list_entries(&s.folder_id)
                .unwrap()
                .iter()
                .all(|e| e.path != "secret.log")
        );
    }

    #[tokio::test]
    async fn pull_failure_retries_before_giving_up() {
        let (s, _t) = session().await;
        let hash = s.blobs.add_bytes(b"retry-me".to_vec()).await.unwrap();
        let remote = HashMap::from([("r.txt".to_string(), entry("r.txt", &hash, "devB", 1))]);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        let stats = s
            .apply_remote(&remote, move |_h| {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                async move {
                    if n < 3 {
                        Err(anyhow::anyhow!("模拟网络抖动"))
                    } else {
                        Ok(())
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(stats.pulled, 1);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(std::fs::read(s.root.join("r.txt")).unwrap(), b"retry-me");
    }

    #[tokio::test]
    async fn apply_remote_pulls_missing_file() {
        let (s, _t) = session().await;
        // 准备远端内容
        let hash = s.blobs.add_bytes(b"remote".to_vec()).await.unwrap();
        let remote = HashMap::from([("r.txt".to_string(), entry("r.txt", &hash, "devB", 1))]);

        let stats = s
            .apply_remote(&remote, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(stats.pulled, 1);
        assert_eq!(std::fs::read(s.root.join("r.txt")).unwrap(), b"remote");
    }

    #[tokio::test]
    async fn apply_remote_handles_tombstone_delete() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("del.txt"), b"x").unwrap();
        s.initial_scan().await.unwrap();

        // 远端墓碑必须「支配」本地版本才会删除（并发时按冲突处理，修改优先于删除）
        let local = s.snapshot().await;
        let mut tomb = local["del.txt"].clone();
        tomb.deleted = true;
        tomb.version.increment(&self_dev());
        let remote = HashMap::from([("del.txt".to_string(), tomb)]);

        let stats = s
            .apply_remote(&remote, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(stats.deleted, 1);
        assert!(!s.root.join("del.txt").exists());
    }

    fn self_dev() -> String {
        "devA".to_string()
    }

    #[tokio::test]
    async fn concurrent_tombstone_vs_modify_keeps_file() {
        let (s, _t) = session().await;
        std::fs::write(s.root.join("del.txt"), b"x").unwrap();
        s.initial_scan().await.unwrap();

        // 远端并发删除（不同设备、互不支配）→ 按冲突处理，修改优先于删除，文件保留
        let mut tomb = entry("del.txt", "", "devB", 1);
        tomb.deleted = true;
        let remote = HashMap::from([("del.txt".to_string(), tomb)]);

        let stats = s
            .apply_remote(&remote, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(
            stats.conflicts, 1,
            "并发删除应走冲突而非直接删除；pulled={} deleted={} pushed={} failed={}",
            stats.pulled, stats.deleted, stats.pushed, stats.failed
        );
        assert!(s.root.join("del.txt").exists(), "修改优先于删除");
    }

    /// 回归：冲突处理完必须收敛，同一冲突不应反复产生新副本。
    ///
    /// 曾经的问题：apply_conflict 后只把远端原版本写进索引，没合并双方版本向量，
    /// 下一轮 diff 仍判 Concurrent → 每秒生成一个新副本，目录里堆满 sync-conflict 文件。
    #[tokio::test]
    async fn conflict_converges_and_does_not_repeat() {
        let (s, _t) = session().await;
        s.initial_scan().await.unwrap();

        std::fs::write(s.root.join("c.txt"), b"local").unwrap();
        s.on_local_event(FileEvent {
            path: "c.txt".into(),
            kind: EventKind::Changed,
        })
        .await
        .unwrap();
        s.take_pending().await;

        // 双方内容都已在本地 blob 存储（模拟已拉取完成）
        let local_hash = s.blobs.add_bytes(b"local".to_vec()).await.unwrap();
        let remote_hash = s.blobs.add_bytes(b"remote".to_vec()).await.unwrap();
        // 让本地条目与本轮内容一致
        let local_map =
            HashMap::from([("c.txt".to_string(), entry("c.txt", &local_hash, "devA", 1))]);
        s.force_index(local_map).await;
        let remote =
            HashMap::from([("c.txt".to_string(), entry("c.txt", &remote_hash, "devB", 1))]);

        let first = s
            .apply_remote(&remote, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(first.conflicts, 1, "首轮应判定为冲突");

        let copies = |root: &std::path::Path| {
            std::fs::read_dir(root)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains("sync-conflict"))
                .count()
        };
        let after_first = copies(&s.root);
        assert_eq!(after_first, 1, "应恰好产生一份冲突副本");

        // 同一远端版本再来一次：本地版本已合并支配，不应再冲突
        let second = s
            .apply_remote(&remote, |_h| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(second.conflicts, 0, "重复应用同一远端版本不应再次冲突");
        assert_eq!(copies(&s.root), after_first, "冲突副本不应持续增加");
    }

    /// 回归：本地胜出时，原路径必须是本地内容，远端内容进冲突副本。
    /// 曾经按 winner_side 把 winner/loser 又对调了一次，本地胜出时反而用败者覆盖了原文件。
    #[tokio::test]
    async fn conflict_local_winner_keeps_local_content() {
        for (local_mtime, remote_mtime, expect_main, expect_copy) in [
            (100, 1, "local", "remote"), // 本地 mtime 新 → 本地胜
            (1, 100, "remote", "local"), // 远端 mtime 新 → 远端胜
        ] {
            let (s, _t) = session().await;
            std::fs::write(s.root.join("c.txt"), b"local").unwrap();
            let local_hash = s.blobs.add_bytes(b"local".to_vec()).await.unwrap();
            let remote_hash = s.blobs.add_bytes(b"remote".to_vec()).await.unwrap();
            let mut l = entry("c.txt", &local_hash, "devA", 1);
            l.mtime = local_mtime;
            let mut r = entry("c.txt", &remote_hash, "devB", 1);
            r.mtime = remote_mtime;
            s.force_index(HashMap::from([("c.txt".to_string(), l)]))
                .await;

            let stats = s
                .apply_remote(&HashMap::from([("c.txt".to_string(), r)]), |_h| async {
                    Ok(())
                })
                .await
                .unwrap();
            assert_eq!(stats.conflicts, 1);

            let main = std::fs::read_to_string(s.root.join("c.txt")).unwrap();
            assert_eq!(main, expect_main, "原路径应是胜者内容");
            let copy = std::fs::read_dir(&s.root)
                .unwrap()
                .filter_map(|e| e.ok())
                .find(|e| e.file_name().to_string_lossy().contains("sync-conflict"))
                .expect("应产生冲突副本");
            assert_eq!(std::fs::read_to_string(copy.path()).unwrap(), expect_copy);
        }
    }
}
