//! 常驻同步守护：M4 的双向实时同步主循环。
//!
//! 组成（提示词 4.6 / 4.9 / 4.10）：
//! - 启动时扫描并建立索引
//! - notify 监听本地变更 → 增量推送给已连接设备
//! - 收到对端 IndexUpdate → 拉取内容落盘
//! - 周期兜底全量扫描
//! - 断线重连（指数退避）、连接失败回退中继（iroh 内建）

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use fs_sync_core::FileEntry;
use fs_sync_net::frame;
use fs_sync_net::message::Message;
use fs_sync_store::Store;
use iroh::{Endpoint, EndpointId};
use iroh_blobs::store::fs::FsStore;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::engine::SyncEngine;
use crate::session::SyncSession;
use crate::watcher::{FileEvent, watch_folder};

/// 一个被同步文件夹的运行时状态。
pub struct FolderRuntime {
    pub session: Arc<SyncSession>,
    /// 已连接设备的推送通道：endpoint_id hex → 发送端
    pub peers: Arc<Mutex<HashMap<String, mpsc::Sender<Message>>>>,
    /// 待推送给对端的本地变更（由事件循环写入，连接循环消费）
    pub outbox: Arc<tokio::sync::Mutex<Vec<FileEntry>>>,
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
        blobs: Arc<FsStore>,
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
        // （M3 实测教训：只建索引不存内容 → 对端 "Unable to download"）
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
        let outbox = Arc::new(tokio::sync::Mutex::new(Vec::<FileEntry>::new()));
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
                        outbox_for_events.lock().await.push(entry);
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
                    Err(e) => warn!("兜底扫描失败: {e}"),
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

    /// 处理对端推来的增量。
    pub async fn handle_remote_update(
        &self,
        endpoint: Endpoint,
        peer: EndpointId,
        entries: Vec<FileEntry>,
    ) -> Result<()> {
        let blobs = self.session.blobs.clone();
        let puller = crate::sync::make_puller(endpoint, peer, blobs);
        let remote: HashMap<String, FileEntry> =
            entries.into_iter().map(|e| (e.path.clone(), e)).collect();
        let stats = self.session.apply_remote(&remote, puller).await?;
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

    /// 对端上线：推送完整索引（对端据此补齐）。
    pub async fn on_peer_connect(&self, peer: EndpointId, tx: mpsc::Sender<Message>) {
        self.peers.lock().await.insert(peer.to_string(), tx);
        info!(peer = %peer.fmt_short(), "对端已接入同步会话");
    }

    pub async fn on_peer_disconnect(&self, peer: &EndpointId) {
        self.peers.lock().await.remove(&peer.to_string());
        info!(peer = %peer.fmt_short(), "对端已断开");
    }
}

/// 接收端写循环：把推送队列的消息写到连接流。
pub async fn run_peer_writer(
    mut tx_rx: mpsc::Receiver<Message>,
    mut send: iroh::endpoint::SendStream,
) -> Result<()> {
    while let Some(msg) = tx_rx.recv().await {
        if let Err(e) = frame::write_frame(&mut send, &msg).await {
            warn!("写增量失败: {e}");
            return Err(e);
        }
    }
    Ok(())
}

/// 对等连接循环（M4 核心）：对每个已信任设备周期性建立连接、交换索引、
/// 接收增量并应用到本地。断线后指数退避重连（提示词 4.8）。
pub async fn run_peer_loop(
    endpoint: Endpoint,
    device_id: String,
    folder_ids: Vec<String>,
    runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    trust: crate::trust::Trust,
    device_name: String,
) {
    let mut backoff = std::time::Duration::from_secs(2);
    loop {
        let peers = trust.devices().unwrap_or_default();
        for peer in peers {
            let Ok(peer_id) = peer.id.parse::<EndpointId>() else {
                continue;
            };
            if peer_id.to_string() == device_id {
                continue;
            }
            match sync_round(&endpoint, peer_id, &device_name, &folder_ids, &runtimes).await {
                Ok(()) => {
                    backoff = std::time::Duration::from_secs(2);
                }
                Err(e) => {
                    info!(peer = %peer.name, "同步会话结束: {e:#}");
                }
            }
        }
        tokio::time::sleep(backoff).await;
        // 指数退避上限 60 秒
        backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
    }
}

/// 一轮同步：连上对端 → 交换索引 → 持续收发增量，直到连接断开。
async fn sync_round(
    endpoint: &Endpoint,
    peer_id: EndpointId,
    _device_name: &str,
    folder_ids: &[String],
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> Result<()> {
    let conn = endpoint
        .connect(peer_id, fs_sync_net::ALPN)
        .await
        .context("连接对端失败")?;
    info!(
        peer = %peer_id.fmt_short(),
        conn_type = %fs_sync_net::conn::classify_conn(&conn).map(|c| c.to_string()).unwrap_or_default(),
        "同步会话已建立"
    );

    let (mut send, mut recv) = conn.open_bi().await?;

    // 交换全量索引（每个文件夹一条）
    for fid in folder_ids {
        let rt_opt = {
            let guard = runtimes.lock().unwrap();
            guard.iter().find(|r| &r.session.folder_id == fid).cloned()
        };
        if let Some(rt) = rt_opt {
            let entries: Vec<FileEntry> = rt.session.snapshot().await.into_values().collect();
            frame::write_frame(
                &mut send,
                &Message::Index {
                    folder_id: fid.clone(),
                    entries: entries.clone(),
                },
            )
            .await?;
            // 首轮全量索引里对端没有的条目，直接排入 outbox 待推送
            let pushable: Vec<FileEntry> = entries
                .into_iter()
                .filter(|e| !e.is_dir && !e.deleted)
                .collect();
            if !pushable.is_empty() {
                rt.outbox.lock().await.extend(pushable);
            }
        }
    }

    // 收对端索引并应用；同时把本地变更推出去
    loop {
        tokio::select! {
            msg = frame::read_frame(&mut recv) => {
                match msg {
                    Ok(msg) => handle_incoming(msg, &mut send, endpoint, peer_id, runtimes).await?,
                    Err(e) => {
                        warn!(peer = %peer_id.fmt_short(), "读对端消息失败: {e:#}");
                        return Err(e.context("读对端消息失败"));
                    }
                }
            }
            entry = recv_local_change(runtimes, folder_ids) => {
                let msg = Message::IndexUpdate {
                    folder_id: entry.0,
                    entries: vec![entry.1],
                };
                if let Err(e) = frame::write_frame(&mut send, &msg).await {
                    return Err(e.context("推送本地变更失败"));
                }
                info!(path = %msg_path(&msg), "已推送本地变更");
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {
                // 保活
                frame::write_frame(&mut send, &Message::Ping).await?;
            }
        }
    }
}

fn msg_path(msg: &Message) -> &str {
    match msg {
        Message::IndexUpdate { entries, .. } => {
            entries.first().map(|e| e.path.as_str()).unwrap_or("")
        }
        _ => "",
    }
}

/// 等待任一运行时的本地变更。
///
/// **必须永不返回 None**：tokio::select! 的分支一旦模式不匹配就被永久禁用，
/// 若这里返回 None，后续本地变更要等 read_frame 或保活超时才可能推送（实测延迟 20s）。
async fn recv_local_change(
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    folder_ids: &[String],
) -> (String, FileEntry) {
    loop {
        let rts: Vec<Arc<FolderRuntime>> = {
            let guard = runtimes.lock().unwrap();
            guard
                .iter()
                .filter(|r| folder_ids.contains(&r.session.folder_id))
                .cloned()
                .collect()
        };
        for rt in &rts {
            let mut ob = rt.outbox.lock().await;
            if let Some(entry) = ob.pop() {
                return (rt.session.folder_id.clone(), entry);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
/// 处理对端来信（Index / IndexUpdate / Ping / Error）。
async fn handle_incoming(
    msg: Message,
    send: &mut iroh::endpoint::SendStream,
    endpoint: &Endpoint,
    peer_id: EndpointId,
    runtimes: &Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> Result<()> {
    match msg {
        Message::Index { folder_id, entries } | Message::IndexUpdate { folder_id, entries } => {
            let rt = {
                let guard = runtimes.lock().unwrap();
                guard
                    .iter()
                    .find(|r| r.session.folder_id == folder_id)
                    .cloned()
            };
            let Some(rt) = rt else {
                tracing::warn!(folder = %folder_id, "对端推送未知文件夹，忽略");
                return Ok(());
            };
            let remote: HashMap<String, FileEntry> =
                entries.into_iter().map(|e| (e.path.clone(), e)).collect();
            let puller =
                crate::sync::make_puller(endpoint.clone(), peer_id, rt.session.blobs.clone());
            match rt.session.apply_remote(&remote, puller).await {
                Ok(stats) if stats.pulled + stats.deleted + stats.conflicts > 0 => {
                    info!(
                        pulled = stats.pulled,
                        deleted = stats.deleted,
                        conflicts = stats.conflicts,
                        "已应用对端变更"
                    );
                }
                Ok(_) => {}
                Err(e) => warn!("应用对端变更失败: {e}"),
            }
            // apply_remote 会把本地胜出条目排入 session.pending，转进 outbox 待推送
            let pending = rt.session.take_pending().await;
            if !pending.is_empty() {
                rt.outbox.lock().await.extend(pending);
            }
        }
        Message::Ping => {
            frame::write_frame(send, &Message::Pong).await?;
        }
        Message::Pong => {}
        Message::Error { code, message } => {
            warn!(code, %message, "对端返回错误");
        }
        other => debug!("忽略消息: {other:?}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn folder_runtime_scans_and_watches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sync");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();

        let blobs = Arc::new(
            fs_sync_net::blobs::open_store(&tmp.path().join("blobs"))
                .await
                .unwrap(),
        );
        let store = Arc::new(Store::open_in_memory().unwrap());

        let rt = FolderRuntime::start("f1".into(), root.clone(), "devA".into(), store, blobs, 100)
            .await
            .unwrap();

        // 初始扫描已索引
        let snap = rt.session.snapshot().await;
        assert!(snap.contains_key("a.txt"));

        // 写新文件 → 事件循环应更新索引
        std::fs::write(root.join("b.txt"), b"new").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let snap = rt.session.snapshot().await;
        assert!(
            snap.contains_key("b.txt"),
            "监听应捕获新文件，当前: {snap:?}"
        );
    }

    #[tokio::test]
    async fn peer_registry_tracks_connections() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("s");
        std::fs::create_dir_all(&root).unwrap();
        let blobs = Arc::new(
            fs_sync_net::blobs::open_store(&tmp.path().join("b"))
                .await
                .unwrap(),
        );
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rt = FolderRuntime::start("f1".into(), root, "devA".into(), store, blobs, 100)
            .await
            .unwrap();

        let peer = iroh::SecretKey::generate().public();
        let (tx, _rx) = mpsc::channel(8);
        rt.on_peer_connect(peer, tx).await;
        assert_eq!(rt.peers.lock().await.len(), 1);

        rt.on_peer_disconnect(&peer).await;
        assert!(rt.peers.lock().await.is_empty());
    }
}
