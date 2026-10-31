//! 常驻同步守护：双向实时同步主循环（SSH/SFTP 与中继两种方案）。
//!
//! - Tailscale 方案：主动连对端 SSH 服务端，控制消息走 `foldersync` 子系统，
//!   文件内容走同一条 SSH 连接上的 SFTP。
//! - 中继方案：双方各自连中继房间，上传自己的索引与 blob、轮询对端索引。
//! - 断线重连（指数退避），无可用方案时在 UI 引导用户安装 Tailscale 或搭建中继。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result};
use fs_sync_core::FileEntry;
use fs_sync_net::blobstore::BlobStore;
use fs_sync_net::conn::NetworkMode;
use fs_sync_net::identity::SshIdentity;
use fs_sync_net::message::Message;
use fs_sync_store::{Device, Store};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::engine::SyncEngine;
use crate::session::SyncSession;
use crate::watcher::{FileEvent, watch_folder};

/// 前端事件回调（daemon 不依赖 Tauri）。
pub type EventSink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// 传输限速回调：参数为字节数，返回需要等待的 future。
pub type ThrottleFn = Arc<dyn Fn(u64) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// 上传/下载限速器（由桌面端注入，daemon 只负责在传输后调用）。
#[derive(Clone, Default)]
pub struct TransferThrottle {
    pub upload: Option<ThrottleFn>,
    pub download: Option<ThrottleFn>,
}

impl TransferThrottle {
    pub async fn consume_upload(&self, bytes: u64) {
        if let Some(f) = &self.upload {
            f(bytes).await;
        }
    }

    pub async fn consume_download(&self, bytes: u64) {
        if let Some(f) = &self.download {
            f(bytes).await;
        }
    }
}

/// 待推送队列：先进先出，同一路径只保留最新条目。
///
/// 每个设备维护独立游标：同一文件夹分享给多台设备时，一条本地变更
/// 必须能被每台设备各自看到，不能像单队列 `pop` 那样被先到的会话抢走。
#[derive(Debug, Default)]
pub struct Outbox {
    /// (seq, path)：更新同一路径不移动旧事件位置，但旧事件的 seq 会失配而被跳过
    events: std::collections::VecDeque<(u64, String)>,
    entries: HashMap<String, (u64, FileEntry)>,
    next_seq: u64,
    /// 每台设备已消费到的 seq
    cursors: HashMap<String, u64>,
}

impl Outbox {
    pub fn push(&mut self, e: FileEntry) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if let Some((old_seq, old_entry)) = self.entries.remove(&e.path) {
            // 同一路径更新：移除旧事件，把最新版本排到队尾，保证每台设备都能看到
            self.events
                .retain(|(s, p)| !(*s == old_seq && *p == old_entry.path));
        }
        self.entries.insert(e.path.clone(), (seq, e.clone()));
        self.events.push_back((seq, e.path));
    }

    pub fn extend(&mut self, it: impl IntoIterator<Item = FileEntry>) {
        for e in it {
            self.push(e);
        }
    }

    /// 新设备接入时登记游标：之前的历史由全量索引负责，不重放旧事件。
    pub fn register_peer(&mut self, peer: &str) {
        self.cursors
            .entry(peer.to_string())
            .or_insert(self.next_seq);
    }

    /// 取该设备尚未消费的下一条变更。每台设备独立推进自己的游标。
    pub fn take_for_peer(&mut self, peer: &str) -> Option<FileEntry> {
        self.register_peer(peer);
        let cursor = *self.cursors.get(peer).unwrap_or(&0);
        let mut picked: Option<(usize, u64, FileEntry)> = None;
        for (i, (seq, path)) in self.events.iter().enumerate() {
            if *seq < cursor {
                continue;
            }
            let Some((cur_seq, entry)) = self.entries.get(path) else {
                continue;
            };
            if *cur_seq != *seq {
                continue; // 已被更新版本取代，跳过旧事件
            }
            picked = Some((i, *seq, entry.clone()));
            break;
        }
        if let Some((_idx, seq, entry)) = picked {
            self.cursors.insert(peer.to_string(), seq + 1);
            // 不按单个 peer 删除事件：可能还有其他设备没消费；由 prune 按最小游标清理
            self.prune();
            Some(entry)
        } else {
            self.cursors.insert(peer.to_string(), self.next_seq);
            self.prune();
            None
        }
    }

    /// 清空队列（中继模式用全量索引，不需要逐条推送）。
    pub fn clear_all(&mut self) {
        self.events.clear();
        self.entries.clear();
        self.cursors.clear();
        self.next_seq = 0;
    }

    /// 只跳过某个 peer 的待推送事件，不影响其他 peer 的游标。
    pub fn clear_for_peer(&mut self, peer: &str) {
        self.cursors.insert(peer.to_string(), self.next_seq);
        self.prune();
    }

    fn prune(&mut self) {
        if let Some(min) = self.cursors.values().min().copied() {
            self.events.retain(|(seq, _)| *seq >= min);
            self.entries.retain(|_, (seq, _)| *seq >= min);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn take_all(&mut self) -> Vec<FileEntry> {
        let mut out: Vec<FileEntry> = self.entries.values().map(|(_, e)| e.clone()).collect();
        self.clear_all();
        out.sort_by_key(|e| e.path.clone());
        out
    }
}

/// 每 peer/folder 的分页索引缓冲：(下一块序号, 已收到的条目)
type IndexChunkBuffer = HashMap<String, (u32, Vec<FileEntry>)>;

/// 一个被同步文件夹的运行时状态。
pub struct FolderRuntime {
    pub session: Arc<SyncSession>,
    /// 已连接设备的推送通道：device id → 发送端
    pub peers: Arc<Mutex<HashMap<String, mpsc::Sender<Message>>>>,
    /// 待推送给对端的本地变更（由事件循环写入，连接循环消费）
    pub outbox: Arc<tokio::sync::Mutex<Outbox>>,
    /// 分享给哪些设备（空 = 所有已配对设备）
    pub shared_with: Arc<std::sync::RwLock<Vec<String>>>,
    /// 正在进行中的远端应用/下载批次数（状态显示用）
    active_transfers: Arc<AtomicU32>,
    /// 最近一次同步失败原因（成功后清空，用于 UI 显示 Error 而不是假 Idle）
    last_error: Arc<std::sync::RwLock<Option<String>>>,
    /// 分页索引接收缓冲：peer + folder → (下一块序号, 已收到的条目)
    incoming_index_chunks: Arc<Mutex<IndexChunkBuffer>>,
    /// 停止运行时唤醒后台任务（drop 不会自动取消 spawn 的任务）
    cancel_notify: Arc<tokio::sync::Notify>,
    /// 持有 debouncer 防 drop
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
}

impl FolderRuntime {
    /// 该运行时监听的本地根目录。
    pub fn root_path(&self) -> std::path::PathBuf {
        self.session.root.clone()
    }

    /// 启动一个文件夹的实时同步。
    pub async fn start(
        folder_id: String,
        root: PathBuf,
        device_id: String,
        store: Arc<Store>,
        blobs: Arc<BlobStore>,
        debounce_ms: u64,
    ) -> Result<Self> {
        let engine = SyncEngine::new(folder_id.clone(), root.clone(), store, blobs.clone());
        let session = Arc::new(SyncSession::new(
            engine,
            folder_id.clone(),
            root.clone(),
            device_id,
            blobs,
        ));
        session.initial_scan().await?;

        // 初始扫描后把既有文件加入 blob 存储，否则对端拉不到内容
        let initial = session.snapshot().await;
        let mut added = 0;
        for e in initial.values().filter(|e| !e.is_dir && !e.deleted) {
            match session.engine.ensure_blob(e).await {
                Ok(_) => added += 1,
                Err(err) => warn!(path = %e.path, "初始加入 blob 失败: {err}"),
            }
        }
        if added > 0 {
            info!(added, "初始文件已加入 blob 存储");
        }

        let peers: Arc<Mutex<HashMap<String, mpsc::Sender<Message>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // 监听
        let matcher = fs_sync_core::matcher_for_folder(&root);
        let (rx, debouncer) = watch_folder(
            &root,
            matcher,
            session.engine.suppression.clone(),
            debounce_ms,
        )?;

        // 事件循环：本地变更 → 入 blob → 写入 outbox（由连接循环推送）
        let sess = session.clone();
        let outbox = Arc::new(tokio::sync::Mutex::new(Outbox::default()));
        let outbox_for_events = outbox.clone();
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        tokio::spawn(async move {
            let mut rx: mpsc::Receiver<FileEvent> = rx;
            while let Some(ev) = rx.recv().await {
                match sess.on_local_event(ev).await {
                    Ok(Some(entry)) => {
                        if !entry.is_dir
                            && !entry.deleted
                            && let Err(e) = sess.engine.ensure_blob(&entry).await
                        {
                            warn!(path = %entry.path, "加入 blob 失败: {e}");
                        }
                        // on_local_event 已把条目放进 session.pending；从 pending 取，
                        // 否则 pending 只进不出，永远堆积
                        let pending = sess.take_pending().await;
                        outbox_for_events.lock().await.extend(pending);
                    }
                    Ok(None) => {}
                    Err(e) => warn!("本地事件处理失败: {e}"),
                }
            }
        });

        // 兜底扫描
        let sess2 = session.clone();
        let outbox2 = outbox.clone();
        let cancel2 = cancel_notify.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(sess2.config.full_scan_interval);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = cancel2.notified() => break,
                    _ = interval.tick() => {}
                }
                match sess2.full_scan().await {
                    Ok(changed) if !changed.is_empty() => {
                        info!(count = changed.len(), "兜底扫描发现变更");
                        for e in changed {
                            if !e.is_dir && !e.deleted {
                                let _ = sess2.engine.ensure_blob(&e).await;
                            }
                            outbox2.lock().await.push(e);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => warn!(兜底扫描失败 = %e),
                }
                if let Err(e) = sess2.purge_expired_tombstones() {
                    warn!("墓碑清理失败: {e}");
                }
            }
        });

        Ok(Self {
            session,
            peers,
            outbox,
            shared_with: Arc::new(std::sync::RwLock::new(Vec::new())),
            active_transfers: Arc::new(AtomicU32::new(0)),
            last_error: Arc::new(std::sync::RwLock::new(None)),
            incoming_index_chunks: Arc::new(Mutex::new(HashMap::new())),
            cancel_notify,
            _debouncer: debouncer,
        })
    }

    /// 该文件夹是否分享给某台设备（空列表 = 所有设备）。
    pub fn shared_with_peer(&self, peer_id: &str) -> bool {
        let shared = self.shared_with.read().unwrap_or_else(|e| e.into_inner());
        shared.is_empty() || shared.iter().any(|id| id == peer_id)
    }

    /// 替换共享设备列表（空 = 所有设备）。
    pub fn set_shared_with(&self, device_ids: Vec<String>) {
        *self.shared_with.write().unwrap_or_else(|e| e.into_inner()) = device_ids;
    }

    /// 标记一段同步工作开始/结束（用于「同步中」状态）。
    pub fn begin_transfer(&self) {
        self.active_transfers.fetch_add(1, Ordering::Relaxed);
    }

    pub fn end_transfer(&self) {
        self.active_transfers.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn active_transfers(&self) -> u32 {
        self.active_transfers.load(Ordering::Relaxed)
    }

    /// 记录失败原因（None = 清除）。
    pub fn set_last_error(&self, error: Option<String>) {
        *self.last_error.write().unwrap_or_else(|e| e.into_inner()) = error;
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 通知后台任务退出（停止/删除文件夹时调用）。
    pub fn cancel(&self) {
        self.cancel_notify.notify_one();
    }

    /// 处理对端推来的增量（Tailscale 方案：通过 SSH 再拉内容）。
    pub async fn handle_remote_update_ssh(
        &self,
        peer: &Device,
        identity: &SshIdentity,
        entries: Vec<FileEntry>,
    ) -> Result<()> {
        let puller = crate::sync::make_ssh_puller(
            peer.clone(),
            identity.clone(),
            self.session.blobs.clone(),
        );
        let remote: HashMap<String, FileEntry> =
            entries.into_iter().map(|e| (e.path.clone(), e)).collect();
        self.begin_transfer();
        let stats = self.session.apply_remote_update(&remote, puller).await;
        self.end_transfer();
        let stats = stats?;
        self.flush_pending().await;
        if stats.pulled + stats.deleted + stats.conflicts > 0 {
            info!(
                pulled = stats.pulled,
                deleted = stats.deleted,
                conflicts = stats.conflicts,
                "已应用对端增量"
            );
        }
        Ok(())
    }

    /// 对端上线：登记推送通道。
    pub async fn on_peer_connect(&self, peer: String, tx: mpsc::Sender<Message>) {
        self.peers.lock().await.insert(peer.clone(), tx);
        info!(peer = %peer, "对端已接入同步会话");
    }

    pub async fn on_peer_disconnect(&self, peer: &str) {
        self.peers.lock().await.remove(peer);
        info!(peer = %peer, "对端已断开");
    }

    /// 临时取出待推送的本地变更。
    pub async fn take_outbox(&self) -> Vec<FileEntry> {
        self.outbox.lock().await.take_all()
    }

    /// 该文件夹当前待同步的文件数（outbox + session 待发送队列 + 进行中批次）。
    pub async fn pending_count(&self) -> u32 {
        let outbox = self.outbox.lock().await.len();
        let pending = self.session.pending_len().await;
        (outbox + pending) as u32 + self.active_transfers()
    }

    /// 把 session.pending（apply_remote 产生的冲突合并 / 删改合并结果）转进 outbox，
    /// 让对端尽快收到收敛后的版本；同时避免 pending 只进不出。
    pub async fn flush_pending(&self) {
        let pending = self.session.take_pending().await;
        if !pending.is_empty() {
            self.outbox.lock().await.extend(pending);
        }
    }
}

/// 对等连接循环配置。
#[derive(Clone)]
pub struct PeerLoopConfig {
    pub identity: SshIdentity,
    pub device_id: String,
    pub device_name: String,
    pub ssh_port: u16,
    pub mode: NetworkMode,
    pub relay_url: Option<String>,
    /// 中继模式自动接受共享邀请时需要创建本地文件夹
    pub store: Arc<Store>,
    pub blobs: Arc<BlobStore>,
    pub sync_root: PathBuf,
    pub paused_all: Arc<AtomicBool>,
}

/// 对等连接循环：周期检查信任列表，为每个设备起一个独立监督任务。
///
/// `cfg` 用 `RwLock` 共享：设置页改网络模式 / 中继地址后立即生效，不必重启应用。
pub async fn run_peer_loop(
    cfg: Arc<std::sync::RwLock<PeerLoopConfig>>,
    runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    trust: crate::trust::Trust,
    online: Option<Arc<Mutex<fs_sync_net::conn::Presence>>>,
) {
    let mut spawned: HashSet<String> = HashSet::new();
    loop {
        let own_id = cfg.read().unwrap().device_id.clone();
        let peers = trust.devices().unwrap_or_default();
        // 已移除信任的设备允许以后重新配对时重新拉起监督任务
        spawned.retain(|id| peers.iter().any(|p| &p.id == id));
        for peer in peers {
            if peer.id == own_id || spawned.contains(&peer.id) {
                continue;
            }
            spawned.insert(peer.id.clone());
            let cfg = cfg.clone();
            let runtimes = runtimes.clone();
            let online = online.clone();
            let trust = trust.clone();
            tokio::spawn(async move {
                peer_supervisor(cfg, peer, runtimes, online, trust).await;
            });
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// 单个设备的断线重连监督。
async fn peer_supervisor(
    cfg: Arc<std::sync::RwLock<PeerLoopConfig>>,
    peer: Device,
    runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    online: Option<Arc<Mutex<fs_sync_net::conn::Presence>>>,
    trust: crate::trust::Trust,
) {
    let mut backoff = std::time::Duration::from_secs(2);
    let mut auto_fallback_relay = false;
    loop {
        // 设备可能已被移除，或对端地址已更新：每轮都读最新记录
        let peer = match trust.get(&peer.id) {
            Ok(Some(p)) => p,
            Ok(None) => {
                info!(peer = %peer.name, "设备已移除信任，停止同步任务");
                return;
            }
            Err(e) => {
                warn!(peer = %peer.name, "读取信任列表失败: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };
        let cfg = cfg.read().unwrap().clone();
        let use_relay = match cfg.mode {
            NetworkMode::Relay => true,
            NetworkMode::Tailscale => false,
            NetworkMode::Auto => {
                let ts = fs_sync_net::tailscale::detect();
                auto_fallback_relay || peer.addresses.is_empty() || !ts.running
            }
        };
        let conn_ty = if use_relay {
            fs_sync_net::ConnType::Relay
        } else {
            fs_sync_net::ConnType::Tailscale
        };
        if use_relay && (peer.relay_room.is_none() || peer.relay_secret.is_none()) {
            warn!(peer = %peer.name, "没有中继房间信息，请重新配对或改用 Tailscale");
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            continue;
        }
        // 在线计数：本轮会话期间算一条连接，结束时只注销这一条
        if let Some(p) = &online {
            p.lock().await.enter(&peer.id, conn_ty);
        }
        let session = async {
            if use_relay {
                if cfg.mode == NetworkMode::Auto && !auto_fallback_relay {
                    // 对端刚获得 Tailscale 地址时，主动结束中继会话并改走直连，
                    // 避免出现「一端 Tailscale、一端中继」的分裂状态。
                    tokio::select! {
                        result = crate::relay_sync::relay_sync_round(&cfg, &peer, &runtimes) => result,
                        _ = wait_for_peer_tailscale(&trust, &peer.id) => {
                            info!(peer = %peer.name, "检测到对端 Tailscale 地址，切换为直连");
                            Err(anyhow::anyhow!("切换到 Tailscale 直连"))
                        }
                    }
                } else if cfg.mode == NetworkMode::Auto {
                    // 直连失败后回退到了中继。中继轮次正常情况下不会返回，
                    // 以前 auto_fallback_relay 就永远不会被清掉，直连恢复了也一直走中继。
                    // 冷却一段时间后探测对端 SSH 端口，通了就结束中继、回到直连。
                    // 冷却期保证即使直连"时通时不通"，最多每 RELAY_RETRY_DIRECT_AFTER 跳一次。
                    tokio::select! {
                        result = crate::relay_sync::relay_sync_round(&cfg, &peer, &runtimes) => result,
                        _ = wait_for_direct_reachable(&trust, &peer.id) => {
                            info!(peer = %peer.name, "对端直连已恢复，从中继切回 Tailscale");
                            // 走 Ok 分支：重置退避并清掉 auto_fallback_relay
                            Ok(())
                        }
                    }
                } else {
                    crate::relay_sync::relay_sync_round(&cfg, &peer, &runtimes).await
                }
            } else {
                sync_round_ssh(&cfg, &peer, &runtimes, online.as_ref()).await
            }
        };
        let result = tokio::select! {
            result = session => result,
            _ = wait_until_untrusted(&trust, &peer.id) => {
                info!(peer = %peer.name, "设备已取消配对，主动结束同步会话");
                Err(anyhow::anyhow!("设备已取消配对"))
            }
        };
        if let Some(p) = &online {
            p.lock().await.leave(&peer.id, conn_ty);
        }
        match result {
            Ok(()) => {
                backoff = std::time::Duration::from_secs(2);
                auto_fallback_relay = false;
                for rt in all_runtimes(&runtimes) {
                    if rt.shared_with_peer(&peer.id)
                        && rt
                            .last_error()
                            .map(|e| !e.contains("文件夹邀请被拒绝"))
                            .unwrap_or(false)
                    {
                        rt.set_last_error(None);
                    }
                }
            }
            Err(e) => {
                info!(peer = %peer.name, "同步会话结束: {e:#}");
                let message = format!("与设备「{}」同步失败：{e}", peer.name);
                for rt in all_runtimes(&runtimes) {
                    if !rt.shared_with_peer(&peer.id) {
                        continue;
                    }
                    if rt.last_error().as_deref() != Some(message.as_str()) {
                        rt.set_last_error(Some(message.clone()));
                        let _ = rt.session.engine.store.log_activity(
                            "error",
                            &rt.session.folder_id,
                            "",
                            &message,
                        );
                    }
                }
                // auto 模式：Tailscale 路径失败后自动改走中继（若已配对房间）
                if cfg.mode == NetworkMode::Auto
                    && !use_relay
                    && peer.relay_room.is_some()
                    && peer.relay_secret.is_some()
                {
                    auto_fallback_relay = true;
                    info!(peer = %peer.name, "Tailscale 连接失败，自动切换到中继");
                }
                backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// 等待设备从信任列表移除；用于立即结束正在进行的同步会话。
async fn wait_until_untrusted(trust: &crate::trust::Trust, peer_id: &str) {
    loop {
        if !trust.is_trusted(peer_id).unwrap_or(false) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

/// 等待对端出现可用的 Tailscale 地址（用于从中继切回直连）。
async fn wait_for_peer_tailscale(trust: &crate::trust::Trust, peer_id: &str) {
    loop {
        let has_addr = trust
            .get(peer_id)
            .ok()
            .flatten()
            .map(|p| !p.addresses.is_empty())
            .unwrap_or(false);
        if has_addr && fs_sync_net::tailscale::detect().running {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

/// 回退到中继后，至少等这么久才尝试切回直连（防止来回跳）。
const RELAY_RETRY_DIRECT_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// 冷却期过后，周期探测对端 SSH 端口，任一地址 TCP 可连即返回。
async fn wait_for_direct_reachable(trust: &crate::trust::Trust, peer_id: &str) {
    tokio::time::sleep(RELAY_RETRY_DIRECT_AFTER).await;
    loop {
        if fs_sync_net::tailscale::detect().running
            && let Some(p) = trust.get(peer_id).ok().flatten()
        {
            for addr in &p.addresses {
                let probe = tokio::net::TcpStream::connect((addr.as_str(), p.port));
                if let Ok(Ok(_)) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), probe).await
                {
                    return;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// 一轮 Tailscale SSH 同步：连上对端 → 交换索引 → 持续收发增量，直到连接断开。
async fn sync_round_ssh(
    cfg: &PeerLoopConfig,
    peer: &Device,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    online: Option<&Arc<Mutex<fs_sync_net::conn::Presence>>>,
) -> Result<()> {
    let mut last_err = None;
    let mut client = None;
    for addr in &peer.addresses {
        let target = fs_sync_net::ssh_client::SshTarget {
            host: addr.clone(),
            port: peer.port,
            username: peer.username.clone(),
            host_key: peer.host_key.clone(),
        };
        match fs_sync_net::ssh_client::SshClient::connect(&target, &cfg.identity).await {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let mut client = client
        .ok_or_else(|| last_err.unwrap_or_else(|| anyhow::anyhow!("设备没有 Tailscale 地址")))?;
    info!(peer = %peer.name, rtt_ms = client.rtt_ms(), "SSH 同步会话已建立");
    for rt in all_runtimes(runtimes) {
        if rt.shared_with_peer(&peer.id)
            && rt
                .last_error()
                .map(|e| !e.contains("文件夹邀请被拒绝"))
                .unwrap_or(false)
        {
            rt.set_last_error(None);
        }
    }
    if let Some(p) = online {
        p.lock().await.set_rtt(&peer.id, client.rtt_ms());
    }

    client
        .send(&Message::Hello {
            protocol_version: fs_sync_net::message::PROTOCOL_VERSION,
            device_name: cfg.device_name.clone(),
            folders: Vec::new(),
        })
        .await?;
    match client.recv().await? {
        Message::Hello {
            protocol_version, ..
        } if protocol_version == fs_sync_net::message::PROTOCOL_VERSION => {}
        Message::Hello {
            protocol_version, ..
        } => anyhow::bail!(
            "对端协议版本 {protocol_version} 不兼容（本机 {}）",
            fs_sync_net::message::PROTOCOL_VERSION
        ),
        Message::Error { message, .. } => anyhow::bail!("对端拒绝连接: {message}"),
        _ => anyhow::bail!("对端未完成协议握手"),
    }

    // 交换全量索引（每个文件夹一条）。文件夹列表直接读 runtimes，不用配置里的快照：
    // 应用启动时对等循环比"恢复文件夹"先跑，快照是空的，整个会话就一条都不推。
    let mut announced: HashSet<String> = HashSet::new();
    announce_new_folders(
        &mut client,
        &peer.id,
        &cfg.device_name,
        runtimes,
        &mut announced,
    )
    .await?;

    let sftp = client.sftp();
    // 读帧放到独立任务：read_frame 不是取消安全的，直接放进 select! 时
    // 只要别的分支（本地变更、心跳）先就绪，读了一半的帧就丢了，之后整条流错位。
    let mut incoming = client.spawn_reader();
    // 用 interval 而不是每轮新建 sleep：后者在消息频繁时会被不断重置，永远不触发
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
    ping.tick().await;
    let mut rescan = tokio::time::interval(std::time::Duration::from_secs(3));
    rescan.tick().await;
    loop {
        tokio::select! {
            msg = incoming.recv() => {
                match msg {
                    Some(Ok(msg)) => handle_incoming(&msg, peer, cfg, runtimes, Some(&sftp)).await?,
                    Some(Err(e)) => {
                        warn!(peer = %peer.name, "读对端消息失败: {e:#}");
                        return Err(e.context("读对端消息失败"));
                    }
                    None => anyhow::bail!("对端控制通道已关闭"),
                }
            }
            (folder_id, entry) = recv_local_change(runtimes, &announced, &peer.id) => {
                let log_path = entry.path.clone();
                let log_size = entry.size as i64;
                let log_kind = if entry.deleted { "delete" } else { "upload" };
                let msg = Message::IndexUpdate {
                    folder_id: folder_id.clone(),
                    entries: vec![entry],
                };
                client.send(&msg).await.context("推送本地变更失败")?;
                if let Some(rt) = find_runtime(runtimes, &folder_id) {
                    let _ = rt.session.engine.store.log_activity_sized(
                        log_kind,
                        &folder_id,
                        &log_path,
                        log_size,
                        if log_kind == "delete" {
                            "已同步删除到对端"
                        } else {
                            "已推送到对端"
                        },
                    );
                    let throttle = rt
                        .session
                        .throttle
                        .read()
                        .map(|t| t.clone())
                        .unwrap_or_default();
                    throttle.consume_upload(log_size.max(0) as u64).await;
                }
                debug!(path = %msg_path(&msg), "已推送本地变更");
            }
            // 会话期间新加入的文件夹：补发全量索引
            _ = rescan.tick() => {
                announce_new_folders(&mut client, &peer.id, &cfg.device_name, runtimes, &mut announced).await?;
            }
            _ = ping.tick() => {
                client.send(&Message::Ping).await?;
            }
        }
    }
}

/// 给还没发过全量索引的文件夹发一次 Index，记入 `announced`；已移除的文件夹从中删掉。
async fn announce_new_folders(
    client: &mut fs_sync_net::ssh_client::SshClient,
    peer_id: &str,
    device_name: &str,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    announced: &mut HashSet<String>,
) -> Result<()> {
    let rts = all_runtimes(runtimes);
    announced.retain(|id| rts.iter().any(|r| &r.session.folder_id == id));
    for rt in rts {
        let fid = rt.session.folder_id.clone();
        if announced.contains(&fid) || !rt.shared_with_peer(peer_id) {
            continue;
        }
        // 先登记游标再发全量索引：索引期间产生的新变更不会漏推给该设备
        rt.outbox.lock().await.register_peer(peer_id);
        // 邀请：对端没有同名文件夹时自动创建，避免“两边手动同名添加”
        let folder_name = rt
            .root_path()
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| fid.clone());
        if let Err(e) = client
            .send(&Message::FolderInvite(fs_sync_net::message::FolderInvite {
                folder_id: fid.clone(),
                folder_name,
                from_name: device_name.to_string(),
            }))
            .await
        {
            debug!(folder = %fid, "发送文件夹邀请失败（对端可能不支持）: {e}");
        }
        // 全量索引已经包含全部条目，对端据此 diff 拉取即可。
        // 以前还把每个条目再排进 outbox 逐条推一遍：10 万文件就是 10 万条重复消息。
        let entries: Vec<FileEntry> = rt.session.snapshot().await.into_values().collect();
        info!(folder = %fid, count = entries.len(), "发送全量索引");
        if entries.len() > 4000 {
            let total = entries.len();
            for (i, chunk) in entries.chunks(4000).enumerate() {
                let done = (i + 1) * 4000 >= total;
                client
                    .send(&Message::IndexChunk {
                        folder_id: fid.clone(),
                        entries: chunk.to_vec(),
                        chunk_index: i as u32,
                        done,
                    })
                    .await?;
            }
        } else {
            client
                .send(&Message::Index {
                    folder_id: fid.clone(),
                    entries,
                })
                .await?;
        }
        announced.insert(fid);
    }
    Ok(())
}

/// 当前所有文件夹运行时（实时读取，不缓存）。
pub(crate) fn all_runtimes(
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> Vec<Arc<FolderRuntime>> {
    runtimes.lock().unwrap().iter().cloned().collect()
}

/// 应用一条完整索引或增量索引（分页索引收齐后也走这里）。
async fn apply_index_entries(
    folder_id: &str,
    entries: Vec<FileEntry>,
    partial: bool,
    peer: &Device,
    cfg: &PeerLoopConfig,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    sftp: Option<&Arc<tokio::sync::Mutex<russh_sftp::client::SftpSession>>>,
) -> Result<()> {
    let Some(rt) = find_runtime(runtimes, folder_id) else {
        return Ok(());
    };
    // 未分享给该设备的文件夹：即使对端推索引也忽略，避免单向泄露
    if !rt.shared_with_peer(&peer.id) {
        return Ok(());
    }
    // 完整索引/增量会打断同 peer 未完成的分页缓冲，避免旧块拼进新索引。
    rt.incoming_index_chunks
        .lock()
        .await
        .remove(&format!("{}:{}", peer.id, folder_id));
    let remote: HashMap<String, FileEntry> =
        entries.into_iter().map(|e| (e.path.clone(), e)).collect();
    rt.begin_transfer();
    let stats = match sftp {
        Some(sftp) => {
            let sftp = sftp.clone();
            let blobs = rt.session.blobs.clone();
            let puller = move |hash: String| {
                let sftp = sftp.clone();
                let blobs = blobs.clone();
                async move {
                    fs_sync_net::ssh_client::SshClient::download_blob(&sftp, &hash, &blobs).await
                }
            };
            if partial {
                rt.session.apply_remote_update(&remote, puller).await
            } else {
                rt.session.apply_remote(&remote, puller).await
            }
        }
        None => {
            let puller = crate::sync::make_ssh_puller(
                peer.clone(),
                cfg.identity.clone(),
                rt.session.blobs.clone(),
            );
            if partial {
                rt.session.apply_remote_update(&remote, puller).await
            } else {
                rt.session.apply_remote(&remote, puller).await
            }
        }
    };
    rt.end_transfer();
    let stats = stats?;
    rt.flush_pending().await;
    if stats.failed > 0 {
        let msg = format!("{} 个文件同步失败，将自动重试", stats.failed);
        if rt.last_error().as_deref() != Some(msg.as_str()) {
            let _ = rt
                .session
                .engine
                .store
                .log_activity("error", folder_id, "", &msg);
            rt.set_last_error(Some(msg.clone()));
            warn!(peer = %peer.name, failed = stats.failed, "{msg}");
        }
    } else if rt
        .last_error()
        .map(|e| !e.contains("文件夹邀请被拒绝"))
        .unwrap_or(false)
    {
        rt.set_last_error(None);
    }
    if stats.pulled + stats.deleted + stats.conflicts > 0 {
        info!(
            peer = %peer.name,
            pulled = stats.pulled,
            deleted = stats.deleted,
            conflicts = stats.conflicts,
            "已应用对端增量"
        );
    }
    Ok(())
}

/// 处理收到的控制消息。
pub(crate) async fn handle_incoming(
    msg: &Message,
    peer: &Device,
    cfg: &PeerLoopConfig,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    sftp: Option<&Arc<tokio::sync::Mutex<russh_sftp::client::SftpSession>>>,
) -> Result<()> {
    match msg {
        Message::Index { folder_id, entries } | Message::IndexUpdate { folder_id, entries } => {
            let partial = matches!(msg, Message::IndexUpdate { .. });
            apply_index_entries(
                folder_id,
                entries.clone(),
                partial,
                peer,
                cfg,
                runtimes,
                sftp,
            )
            .await?;
        }
        Message::IndexChunk {
            folder_id,
            entries,
            chunk_index,
            done,
        } => {
            let Some(rt) = find_runtime(runtimes, folder_id) else {
                return Ok(());
            };
            if !rt.shared_with_peer(&peer.id) {
                return Ok(());
            }
            let key = format!("{}:{}", peer.id, folder_id);
            let complete = {
                let mut chunks = rt.incoming_index_chunks.lock().await;
                if *chunk_index == 0 {
                    chunks.remove(&key);
                }
                let entry = chunks.entry(key.clone()).or_insert((0, Vec::new()));
                if *chunk_index != entry.0 {
                    warn!(
                        peer = %peer.name,
                        folder = %folder_id,
                        expected = entry.0,
                        got = *chunk_index,
                        "分页索引块序号不连续，丢弃本次索引"
                    );
                    chunks.remove(&key);
                    return Ok(());
                }
                entry.1.extend(entries.iter().cloned());
                entry.0 += 1;
                if *done {
                    chunks.remove(&key).map(|(_, entries)| entries)
                } else {
                    None
                }
            };
            if let Some(collected) = complete {
                apply_index_entries(folder_id, collected, false, peer, cfg, runtimes, sftp).await?;
            }
        }
        Message::FolderInviteResponse(resp) => {
            if let Some(rt) = find_runtime(runtimes, &resp.folder_id) {
                if resp.accepted {
                    if rt
                        .last_error()
                        .map(|e| e.contains("文件夹邀请被拒绝"))
                        .unwrap_or(false)
                    {
                        rt.set_last_error(None);
                    }
                } else {
                    let msg = format!(
                        "文件夹邀请被拒绝：{}",
                        resp.reason.as_deref().unwrap_or("对端未说明原因")
                    );
                    if rt.last_error().as_deref() != Some(msg.as_str()) {
                        rt.set_last_error(Some(msg.clone()));
                        let _ = rt.session.engine.store.log_activity(
                            "error",
                            &resp.folder_id,
                            "",
                            &msg,
                        );
                    }
                }
            }
        }
        Message::Ping => {}
        Message::Pong => {}
        Message::Error { code, message } => {
            warn!(code, message, "对端返回错误");
        }
        _ => {}
    }
    Ok(())
}

fn find_runtime(
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    folder_id: &str,
) -> Option<Arc<FolderRuntime>> {
    runtimes
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.session.folder_id == folder_id)
        .cloned()
}

fn msg_path(msg: &Message) -> &str {
    match msg {
        Message::IndexUpdate { entries, .. } => {
            entries.first().map(|e| e.path.as_str()).unwrap_or("")
        }
        _ => "",
    }
}

/// 等待任一运行时的本地变更。只取已发过全量索引、且分享给该设备的文件夹：
/// 新文件夹的变更等全量索引发出后再推，顺序不会乱。每台设备独立游标。
async fn recv_local_change(
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    announced: &HashSet<String>,
    peer_id: &str,
) -> (String, FileEntry) {
    loop {
        let rts: Vec<Arc<FolderRuntime>> = all_runtimes(runtimes)
            .into_iter()
            .filter(|r| announced.contains(&r.session.folder_id) && r.shared_with_peer(peer_id))
            .collect();
        for rt in rts {
            let entry = rt.outbox.lock().await.take_for_peer(peer_id);
            if let Some(e) = entry {
                return (rt.session.folder_id.clone(), e);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fe(path: &str, hash: &str) -> FileEntry {
        FileEntry::new(path, 1, 1, hash, false, "devA")
    }

    #[test]
    fn outbox_delivers_each_change_to_every_peer() {
        let mut o = Outbox::default();
        o.register_peer("p1");
        o.register_peer("p2");
        o.push(fe("a", "1"));
        o.push(fe("b", "1"));
        o.push(fe("a", "2")); // 同路径：内容更新，旧事件被替换
        assert_eq!(o.len(), 2);

        // 同一 queue 两台设备各自都能拿到全部变更，先到的不会抢走
        let mut p1: Vec<_> = std::iter::from_fn(|| o.take_for_peer("p1"))
            .map(|e| (e.path, e.hash))
            .collect();
        p1.sort();
        assert_eq!(p1, vec![("a".into(), "2".into()), ("b".into(), "1".into())]);
        let mut p2: Vec<_> = std::iter::from_fn(|| o.take_for_peer("p2"))
            .map(|e| (e.path, e.hash))
            .collect();
        p2.sort();
        assert_eq!(p2, vec![("a".into(), "2".into()), ("b".into(), "1".into())]);
        assert!(o.take_for_peer("p1").is_none());
        assert!(o.is_empty());
    }

    #[test]
    fn outbox_register_peer_starts_at_current_tail() {
        let mut o = Outbox::default();
        o.push(fe("old", "1"));
        o.register_peer("late");
        assert!(o.take_for_peer("late").is_none(), "注册前历史交给全量索引");
        o.push(fe("new", "1"));
        assert_eq!(o.take_for_peer("late").unwrap().path, "new");
    }

    #[test]
    fn outbox_clear_for_peer_keeps_other_peers() {
        let mut o = Outbox::default();
        o.register_peer("ssh");
        o.register_peer("relay");
        o.push(fe("a", "1"));
        o.clear_for_peer("relay");
        assert!(o.take_for_peer("relay").is_none());
        assert_eq!(o.take_for_peer("ssh").unwrap().path, "a");
    }

    #[tokio::test]
    async fn peer_registry_tracks_connections() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("s");
        std::fs::create_dir_all(&root).unwrap();
        let blobs = Arc::new(BlobStore::open(tmp.path().join("share")).unwrap());
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rt = FolderRuntime::start("f1".into(), root, "devA".into(), store, blobs, 100)
            .await
            .unwrap();

        let peer = "SHA256:abc".to_string();
        let (tx, _rx) = mpsc::channel(8);
        rt.on_peer_connect(peer.clone(), tx).await;
        assert_eq!(rt.peers.lock().await.len(), 1);

        rt.on_peer_disconnect(&peer).await;
        assert!(rt.peers.lock().await.is_empty());
    }

    #[tokio::test]
    async fn index_chunks_are_assembled_before_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().to_path_buf();
        let root = data.join("docs");
        std::fs::create_dir_all(&root).unwrap();
        let identity = fs_sync_net::identity::SshIdentity::load_or_create(&data).unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let blobs = Arc::new(BlobStore::open(data.join("share")).unwrap());
        let rt = Arc::new(
            FolderRuntime::start(
                "f1".into(),
                root,
                identity.id(),
                store.clone(),
                blobs.clone(),
                50,
            )
            .await
            .unwrap(),
        );
        let runtimes = Arc::new(std::sync::Mutex::new(vec![rt.clone()]));
        let cfg = PeerLoopConfig {
            identity: identity.clone(),
            device_id: identity.id(),
            device_name: "local".into(),
            ssh_port: 2222,
            mode: NetworkMode::Tailscale,
            relay_url: None,
            store,
            blobs,
            sync_root: data.join("FolderSync"),
            paused_all: Arc::new(AtomicBool::new(false)),
        };
        let peer = Device {
            id: "peer".into(),
            name: "remote".into(),
            paired_at: 0,
            ssh_public_key: String::new(),
            host_key: String::new(),
            username: "foldersync".into(),
            addresses: Vec::new(),
            port: 2222,
            relay_url: None,
            relay_room: None,
            relay_secret: None,
        };
        let dir1 = FileEntry::new("one", 0, 1, "", true, "remote");
        let dir2 = FileEntry::new("two", 0, 1, "", true, "remote");
        handle_incoming(
            &Message::IndexChunk {
                folder_id: "f1".into(),
                entries: vec![dir1],
                chunk_index: 0,
                done: false,
            },
            &peer,
            &cfg,
            &runtimes,
            None,
        )
        .await
        .unwrap();
        assert!(
            rt.session.snapshot().await.is_empty(),
            "未收完的分页不能提前应用"
        );
        handle_incoming(
            &Message::IndexChunk {
                folder_id: "f1".into(),
                entries: vec![dir2],
                chunk_index: 1,
                done: true,
            },
            &peer,
            &cfg,
            &runtimes,
            None,
        )
        .await
        .unwrap();
        let snap = rt.session.snapshot().await;
        assert!(snap.contains_key("one"));
        assert!(snap.contains_key("two"));
    }
}
