//! 配对会话管理：生成配对码、等待对端请求、用户确认、写入信任列表。
//!
//! 配对时双方交换的是 SSH 公钥与服务端主机公钥：
//! 1. A 生成配对码（含 A 的公钥、主机公钥、Tailscale 地址/中继地址、一次性 token）
//! 2. B 通过 Tailscale SSH（token 作密码）或中继房间把 B 的公钥交给 A
//! 3. A 用户确认后，双方写信任列表；A 生成中继房间号/口令随响应返回
//! 4. 之后重连只靠 SSH 公钥认证

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fs_sync_net::identity::SshIdentity;
use fs_sync_net::message::PeerInfo;
use fs_sync_net::pairing::{PairingCode, new_relay_room, now_secs};
use fs_sync_net::tailscale;
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, info, warn};

use crate::trust::Trust;
use fs_sync_store::Store;

/// 待确认的配对请求。
#[derive(Debug, Clone)]
pub struct PendingPairing {
    pub request_id: String,
    /// 发起方设备 ID
    pub peer_id: String,
    pub peer_name: String,
    /// 对端 SSH 公钥指纹（前 16 位，供用户核对）
    pub fingerprint: String,
}

/// 待确认请求表：request_id -> (请求详情, 用户决定的回复通道)
type PendingMap = HashMap<String, (PendingPairing, oneshot::Sender<bool>)>;

/// 配对会话管理器。
#[derive(Clone)]
pub struct PairingManager {
    trust: Trust,
    identity: SshIdentity,
    host_public_key: String,
    ssh_port: Arc<std::sync::RwLock<u16>>,
    device_name: String,
    relay_url: Option<String>,
    /// 待用户确认的配对请求
    pending: Arc<Mutex<PendingMap>>,
    /// 持久化层：token 跨进程共享（pair 与 serve 是不同进程）
    store: Arc<Store>,
}

impl PairingManager {
    pub fn new(
        trust: Trust,
        identity: SshIdentity,
        host_public_key: String,
        ssh_port: u16,
        device_name: String,
        relay_url: Option<String>,
        store: Arc<Store>,
    ) -> Self {
        Self {
            trust,
            identity,
            host_public_key,
            ssh_port: Arc::new(std::sync::RwLock::new(ssh_port)),
            device_name,
            relay_url,
            pending: Arc::new(Mutex::new(HashMap::new())),
            store,
        }
    }

    /// 本机对外公布的信息（配对码 / 配对响应）。
    pub fn own_info(&self, addrs: Vec<String>) -> Result<PeerInfo> {
        Ok(PeerInfo {
            device_id: self.identity.id(),
            device_name: self.device_name.clone(),
            ssh_public_key: self.identity.public_key_line()?,
            host_key: self.host_public_key.clone(),
            username: fs_sync_net::SSH_USER.to_string(),
            port: *self.ssh_port.read().unwrap(),
            addrs,
            relay: self.relay_url.clone(),
            relay_room: None,
            relay_secret: None,
        })
    }

    /// 生成配对码。`addrs` 是本机 Tailscale 地址。
    pub async fn create_code(&self, addrs: Vec<String>, relay: Option<&str>) -> PairingCode {
        let mut peer = self.own_info(addrs).unwrap_or_default();
        if let Some(relay) = relay {
            peer.relay = Some(relay.to_string());
        }
        let code = fs_sync_net::pairing::generate(peer, now_secs());
        if let Err(e) = self
            .store
            .add_pairing_token(&code.token, code.expires_at as i64)
        {
            warn!("配对 token 持久化失败: {e}");
        }
        info!(device = %self.device_name, "生成配对码");
        code
    }

    /// 清理过期 token。
    pub async fn purge_expired(&self) {
        let _ = self
            .store
            .consume_pairing_token("__nonexistent__", now_secs() as i64);
    }

    /// 配对密码认证：只校验 token 有效，不消费（消费发生在配对请求处理时）。
    pub fn check_pairing_password(&self, token: &str) -> bool {
        self.store
            .validate_pairing_token(token, now_secs() as i64)
            .unwrap_or(false)
    }

    /// 校验收到的 token（一次性，跨进程持久化）。
    async fn consume_token(&self, token: &str) -> bool {
        match self.store.consume_pairing_token(token, now_secs() as i64) {
            Ok(true) => true,
            Ok(false) => {
                warn!("配对 token 无效、已使用或已过期");
                false
            }
            Err(e) => {
                warn!("配对 token 校验失败: {e}");
                false
            }
        }
    }

    /// 处理对端配对请求：校验 token，挂起等待用户确认，确认后写信任列表。
    ///
    /// 返回 `Ok(Some(自己的 PeerInfo))` 表示接受；`Ok(None)` 表示用户拒绝。
    pub async fn on_pairing_request(
        &self,
        peer: PeerInfo,
        token: &str,
        confirm_timeout: Duration,
    ) -> Result<Option<PeerInfo>> {
        if !self.consume_token(token).await {
            anyhow::bail!("配对 token 无效");
        }
        // 防止对端伪造 device_id：必须与它交出的 SSH 公钥一致
        fs_sync_net::identity::verify_peer_info(&peer)?;

        let request_id = {
            let mut b = [0u8; 8];
            use rand::RngCore;
            rand::rng().fill_bytes(&mut b);
            hex::encode(b)
        };
        let pending = PendingPairing {
            request_id: request_id.clone(),
            peer_id: peer.device_id.clone(),
            peer_name: peer.device_name.clone(),
            fingerprint: fs_sync_net::identity::fingerprint_text(&peer.device_id),
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(request_id.clone(), (pending, tx));

        info!(request = %request_id, peer = %peer.device_name, "配对请求待确认");

        // 等待 UI 确认；超时视为拒绝并清理
        let accepted = match tokio::time::timeout(confirm_timeout, rx).await {
            Ok(Ok(decision)) => decision,
            _ => {
                self.pending.lock().await.remove(&request_id);
                warn!(request = %request_id, "配对确认超时");
                false
            }
        };

        if !accepted {
            info!(request = %request_id, "配对被拒绝");
            return Ok(None);
        }

        // 生成双方共用的中继房间（即使当前走 Tailscale，也便于以后切换）
        let (room, secret) = match (&peer.relay_room, &peer.relay_secret) {
            (Some(r), Some(s)) => (r.clone(), s.clone()),
            _ => new_relay_room(),
        };
        let relay_url = self.relay_url.clone().or(peer.relay.clone());
        let mut peer = peer;
        peer.relay = relay_url.clone();
        peer.relay_room = Some(room.clone());
        peer.relay_secret = Some(secret.clone());
        self.trust.add(&peer)?;

        let mut me = self.own_info(Vec::new())?;
        me.relay = relay_url;
        me.relay_room = Some(room);
        me.relay_secret = Some(secret);
        info!(peer = %peer.device_name, "配对完成");
        Ok(Some(me))
    }

    /// 列出待确认请求（供 UI 展示 / CLI 轮询）。
    pub async fn list_pending(&self) -> Vec<PendingPairing> {
        self.pending
            .lock()
            .await
            .values()
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// 用户确认或拒绝。
    pub async fn confirm(&self, request_id: &str, accept: bool) -> bool {
        if let Some((_, tx)) = self.pending.lock().await.remove(request_id) {
            let _ = tx.send(accept);
            true
        } else {
            false
        }
    }

    /// 信任列表访问（服务端认证用）。
    pub fn trust(&self) -> &Trust {
        &self.trust
    }

    /// 更新本机实际监听端口（端口被占用回退随机端口时用）。
    pub fn set_ssh_port(&self, port: u16) {
        if let Ok(mut guard) = self.ssh_port.write() {
            *guard = port;
        }
    }

    /// 启动中继配对监听：轮询配对房间的请求文件，用户确认后写响应。
    ///
    /// 只在配置了中继服务器时调用；Tailscale 直连配对走 SSH 服务端，不需要它。
    pub fn spawn_relay_pairing_watcher(&self, code: PairingCode, relay_url: String) {
        let pm = self.clone();
        tokio::spawn(async move {
            let room = fs_sync_net::pairing::pair_room(&code.token);
            let relay =
                match fs_sync_net::relay::RelaySftp::connect(&relay_url, &room, &code.token, None)
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("中继配对监听连接失败: {e:#}");
                        return;
                    }
                };
            // 配对码有效期 + 确认窗口
            let deadline = tokio::time::Instant::now()
                + Duration::from_secs(code.expires_at.saturating_sub(now_secs()) + 130);
            loop {
                if tokio::time::Instant::now() > deadline {
                    return;
                }
                match relay.read_text("pairing/request.json").await {
                    Ok(Some(text)) => {
                        let Ok(request) =
                            serde_json::from_str::<fs_sync_net::message::PairingRequest>(&text)
                        else {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        };
                        let response = match pm
                            .on_pairing_request(
                                request.peer,
                                &request.token,
                                Duration::from_secs(120),
                            )
                            .await
                        {
                            Ok(Some(peer)) => fs_sync_net::message::PairingResponse {
                                accepted: true,
                                peer,
                            },
                            _ => fs_sync_net::message::PairingResponse {
                                accepted: false,
                                peer: Default::default(),
                            },
                        };
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = relay.write_text("pairing/response.json", &json).await;
                        }
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => debug!("中继配对轮询失败: {e:#}"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
}

/// 公钥指纹：分组 hex，便于肉眼核对。
pub fn fingerprint_str(id: &str) -> String {
    fs_sync_net::identity::fingerprint_text(id)
}

/// 本机 Tailscale 检测（转发，便于 CLI 使用）。
pub fn detect_tailscale() -> tailscale::TailscaleInfo {
    tailscale::detect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn manager(dir: &Path) -> (PairingManager, Arc<Store>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let trust = Trust::new(store.clone());
        let identity = SshIdentity::load_or_create(dir).unwrap();
        let host = fs_sync_net::identity::load_or_create_host_key(dir).unwrap();
        let pm = PairingManager::new(
            trust,
            identity,
            host.public_key().to_openssh().unwrap(),
            2222,
            "me".into(),
            Some("tcp://127.0.0.1:9443".into()),
            store.clone(),
        );
        (pm, store)
    }

    fn peer() -> PeerInfo {
        let tmp = tempfile::tempdir().unwrap();
        let id = SshIdentity::load_or_create(tmp.path()).unwrap();
        PeerInfo {
            device_id: id.id(),
            device_name: "company".into(),
            ssh_public_key: id.public_key_line().unwrap(),
            host_key: "ssh-ed25519 BBBB".into(),
            username: "foldersync".into(),
            port: 2222,
            addrs: vec!["100.64.0.3".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn token_shared_across_pairing_manager_instances() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pairer = {
            let trust = Trust::new(store.clone());
            let id = SshIdentity::load_or_create(tmp.path()).unwrap();
            PairingManager::new(
                trust,
                id,
                "ssh-ed25519 HOST".into(),
                2222,
                "a".into(),
                None,
                store.clone(),
            )
        };
        let code = pairer.create_code(Vec::new(), None).await;

        let server = {
            let trust = Trust::new(store.clone());
            let id = SshIdentity::load_or_create(tmp.path()).unwrap();
            PairingManager::new(
                trust,
                id,
                "ssh-ed25519 HOST".into(),
                2222,
                "b".into(),
                None,
                store.clone(),
            )
        };
        let r = server
            .on_pairing_request(peer(), &code.token, Duration::from_millis(30))
            .await;
        assert!(r.is_ok(), "跨实例 token 应被接受: {r:?}");
    }

    #[tokio::test]
    async fn rejects_unknown_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (pm, _) = manager(tmp.path());
        let r = pm
            .on_pairing_request(peer(), "deadbeef", Duration::from_millis(50))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn token_is_single_use() {
        let tmp = tempfile::tempdir().unwrap();
        let (pm, _) = manager(tmp.path());
        let code = pm.create_code(Vec::new(), None).await;
        let r1 = pm
            .on_pairing_request(peer(), &code.token, Duration::from_millis(30))
            .await;
        assert_eq!(r1.unwrap(), None, "无人确认应视为拒绝");
        let r2 = pm
            .on_pairing_request(peer(), &code.token, Duration::from_millis(30))
            .await;
        assert!(r2.is_err());
    }

    #[tokio::test]
    async fn accept_writes_trust_and_returns_relay_room() {
        let tmp = tempfile::tempdir().unwrap();
        let (pm, store) = manager(tmp.path());
        let code = pm.create_code(Vec::new(), None).await;
        let peer = peer();
        let pm2 = pm.clone();
        let token = code.token.clone();
        let handle = tokio::spawn(async move {
            pm2.on_pairing_request(peer, &token, Duration::from_secs(5))
                .await
        });

        for _ in 0..50 {
            if let Some(p) = pm.list_pending().await.first() {
                assert!(pm.confirm(&p.request_id.clone(), true).await);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let me = handle.await.unwrap().unwrap().expect("应接受");
        assert!(me.relay_room.is_some());
        assert!(me.relay_secret.is_some());
        assert_eq!(store.list_devices().unwrap().len(), 1);
        assert_eq!(store.list_devices().unwrap()[0].relay_room, me.relay_room);
    }

    #[tokio::test]
    async fn reject_does_not_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let (pm, store) = manager(tmp.path());
        let code = pm.create_code(Vec::new(), None).await;
        let pm2 = pm.clone();
        let token = code.token.clone();
        let handle = tokio::spawn(async move {
            pm2.on_pairing_request(peer(), &token, Duration::from_secs(5))
                .await
        });
        for _ in 0..50 {
            if let Some(p) = pm.list_pending().await.first() {
                assert!(pm.confirm(&p.request_id.clone(), false).await);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(handle.await.unwrap().unwrap(), None);
        assert!(store.list_devices().unwrap().is_empty());
    }

    #[test]
    fn fingerprint_is_grouped() {
        let f = fingerprint_str("SHA256:abcdefghijklmnopqrstuvwxyz");
        assert_eq!(f.split('-').count(), 4, "指纹应为 4 组: {f}");
    }
}
