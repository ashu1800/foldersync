//! 常驻同步守护：双向实时同步主循环（SSH/SFTP 与中继两种方案）。
//!
//! - Tailscale 方案：主动连对端 SSH 服务端，控制消息走 `foldersync` 子系统，
//!   文件内容走同一条 SSH 连接上的 SFTP。
//! - 中继方案：双方各自连中继房间，上传自己的索引与 blob、轮询对端索引。
//! - 断线重连（指数退避），无可用方案时在 UI 引导用户安装 Tailscale 或搭建中继。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

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

/// 待推送队列：先进先出，同一路径只保留最新条目。
///
/// 去重让队列大小以文件数为上限：没有连接在消费时（中继模式、对端离线）不会无限增长，
/// 同一文件连改多次也只推最后一版。
#[derive(Debug, Default)]
pub struct Outbox {
    order: std::collections::VecDeque<String>,
    entries: HashMap<String, FileEntry>,
}

impl Outbox {
    pub fn push(&mut self, e: FileEntry) {
        if self.entries.insert(e.path.clone(), e.clone()).is_none() {
            self.order.push_back(e.path);
        }
    }

    pub fn extend(&mut self, it: impl IntoIterator<Item = FileEntry>) {
        for e in it {
            self.push(e);
        }
    }

    pub fn pop(&mut self) -> Option<FileEntry> {
        let path = self.order.pop_front()?;
        self.entries.remove(&path)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn take_all(&mut self) -> Vec<FileEntry> {
        let mut out = Vec::with_capacity(self.order.len());
        while let Some(e) = self.pop() {
            out.push(e);
        }
        out
    }
}

/// 一个被同步文件夹的运行时状态。
pub struct FolderRuntime {
    pub session: Arc<SyncSession>,
    /// 已连接设备的推送通道：device id → 发送端
    pub peers: Arc<Mutex<HashMap<String, mpsc::Sender<Message>>>>,
    /// 待推送给对端的本地变更（由事件循环写入，连接循环消费）
    pub outbox: Arc<tokio::sync::Mutex<Outbox>>,
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
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(sess2.config.full_scan_interval);
            interval.tick().await;
            loop {
                interval.tick().await;
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
            _debouncer: debouncer,
        })
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
        let stats = self.session.apply_remote_update(&remote, puller).await?;
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
        let result = if use_relay {
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
            sync_round_ssh(&cfg, &peer, &runtimes).await
        };
        if let Some(p) = &online {
            p.lock().await.leave(&peer.id, conn_ty);
        }
        match result {
            Ok(()) => {
                backoff = std::time::Duration::from_secs(2);
                auto_fallback_relay = false;
            }
            Err(e) => {
                info!(peer = %peer.name, "同步会话结束: {e:#}");
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

    client
        .send(&Message::Hello {
            protocol_version: fs_sync_net::message::PROTOCOL_VERSION,
            device_name: cfg.device_name.clone(),
            folders: Vec::new(),
        })
        .await?;

    // 交换全量索引（每个文件夹一条）。文件夹列表直接读 runtimes，不用配置里的快照：
    // 应用启动时对等循环比"恢复文件夹"先跑，快照是空的，整个会话就一条都不推。
    let mut announced: HashSet<String> = HashSet::new();
    announce_new_folders(&mut client, runtimes, &mut announced).await?;

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
            (folder_id, entry) = recv_local_change(runtimes, &announced) => {
                let msg = Message::IndexUpdate {
                    folder_id,
                    entries: vec![entry],
                };
                client.send(&msg).await.context("推送本地变更失败")?;
                debug!(path = %msg_path(&msg), "已推送本地变更");
            }
            // 会话期间新加入的文件夹：补发全量索引
            _ = rescan.tick() => {
                announce_new_folders(&mut client, runtimes, &mut announced).await?;
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
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    announced: &mut HashSet<String>,
) -> Result<()> {
    let rts = all_runtimes(runtimes);
    announced.retain(|id| rts.iter().any(|r| &r.session.folder_id == id));
    for rt in rts {
        let fid = rt.session.folder_id.clone();
        if announced.contains(&fid) {
            continue;
        }
        // 全量索引已经包含全部条目，对端据此 diff 拉取即可。
        // 以前还把每个条目再排进 outbox 逐条推一遍：10 万文件就是 10 万条重复消息。
        let entries: Vec<FileEntry> = rt.session.snapshot().await.into_values().collect();
        info!(folder = %fid, count = entries.len(), "发送全量索引");
        client
            .send(&Message::Index {
                folder_id: fid.clone(),
                entries,
            })
            .await?;
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
            let Some(rt) = find_runtime(runtimes, folder_id) else {
                return Ok(());
            };
            // 增量只比对消息里的路径，否则本地独有的条目每次都被推回去，两端互推
            let partial = matches!(msg, Message::IndexUpdate { .. });
            let remote: HashMap<String, FileEntry> = entries
                .iter()
                .cloned()
                .map(|e| (e.path.clone(), e))
                .collect();
            let stats = match sftp {
                Some(sftp) => {
                    let sftp = sftp.clone();
                    let blobs = rt.session.blobs.clone();
                    let puller = move |hash: String| {
                        let sftp = sftp.clone();
                        let blobs = blobs.clone();
                        async move {
                            fs_sync_net::ssh_client::SshClient::download_blob(&sftp, &hash, &blobs)
                                .await
                        }
                    };
                    if partial {
                        rt.session.apply_remote_update(&remote, puller).await?
                    } else {
                        rt.session.apply_remote(&remote, puller).await?
                    }
                }
                None => {
                    let puller = crate::sync::make_ssh_puller(
                        peer.clone(),
                        cfg.identity.clone(),
                        rt.session.blobs.clone(),
                    );
                    if partial {
                        rt.session.apply_remote_update(&remote, puller).await?
                    } else {
                        rt.session.apply_remote(&remote, puller).await?
                    }
                }
            };
            rt.flush_pending().await;
            if stats.pulled + stats.deleted + stats.conflicts > 0 {
                info!(
                    peer = %peer.name,
                    pulled = stats.pulled,
                    deleted = stats.deleted,
                    conflicts = stats.conflicts,
                    "已应用对端增量"
                );
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

/// 等待任一运行时的本地变更。只取已发过全量索引的文件夹：
/// 新文件夹的变更等全量索引发出后再推，顺序不会乱。
async fn recv_local_change(
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    announced: &HashSet<String>,
) -> (String, FileEntry) {
    loop {
        let rts: Vec<Arc<FolderRuntime>> = all_runtimes(runtimes)
            .into_iter()
            .filter(|r| announced.contains(&r.session.folder_id))
            .collect();
        for rt in rts {
            let entry = rt.outbox.lock().await.pop();
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
    fn outbox_is_fifo_and_dedupes_by_path() {
        let mut o = Outbox::default();
        o.push(fe("a", "1"));
        o.push(fe("b", "1"));
        o.push(fe("a", "2")); // 同路径：覆盖内容，不改变排队位置
        assert_eq!(o.len(), 2);
        let first = o.pop().unwrap();
        assert_eq!((first.path.as_str(), first.hash.as_str()), ("a", "2"));
        assert_eq!(o.pop().unwrap().path, "b");
        assert!(o.pop().is_none());
        assert!(o.is_empty());
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
}
