//! 配对会话管理：生成配对码、等待对端请求、用户确认、写入信任列表。
//!
//! 流程（提示词 4.2）：
//! 1. A 生成配对码（token + 过期时间）
//! 2. B 连接 A，发送 token 与设备名
//! 3. A 校验 token（一次性、未过期），转交 UI 确认
//! 4. 双方写信任列表，token 作废

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fs_sync_net::pairing::{PairingCode, now_secs};
use fs_sync_net::tailscale;
use iroh::EndpointId;
use tokio::sync::{Mutex, oneshot};
use tracing::{info, warn};

use crate::trust::Trust;
use fs_sync_store::Store;

/// 待确认的配对请求。
#[derive(Debug, Clone)]
pub struct PendingPairing {
    pub request_id: String,
    /// 发起方 EndpointId
    pub peer_id: String,
    pub peer_name: String,
    /// 对端公钥指纹（前 16 位 hex，供用户核对）
    pub fingerprint: String,
}

/// 待确认请求表：request_id -> (请求详情, 用户决定的回复通道)
type PendingMap = HashMap<String, (PendingPairing, oneshot::Sender<bool>)>;

/// 配对会话管理器。
#[derive(Clone)]
pub struct PairingManager {
    trust: Trust,
    /// 本端 EndpointId
    my_id: EndpointId,
    /// 待用户确认的配对请求
    pending: Arc<Mutex<PendingMap>>,
    /// 持久化层：token 跨进程共享（pair 与 serve 是不同进程）
    store: Arc<Store>,
}

impl PairingManager {
    pub fn new(trust: Trust, my_id: EndpointId, store: Arc<Store>) -> Self {
        Self {
            trust,
            my_id,
            pending: Arc::new(Mutex::new(HashMap::new())),
            store,
        }
    }

    /// 生成配对码。
    ///
    /// `relay` 是本机正在用的中继 URL，会写进配对码 —— 拨号方据此直连中继，
    /// 不依赖 DNS 地址发现。之前只写 EndpointId，DNS 一旦被墙/被代理干扰就
    /// 必然超时（实测跨省两台机器都连不上）。
    pub async fn create_code(&self, addrs: Vec<String>, relay: Option<&str>) -> PairingCode {
        let mut code = fs_sync_net::pairing::generate(&self.my_id.to_string(), now_secs(), relay);
        code.addrs = addrs;
        // 持久化 token，供另一个进程（serve）校验
        if let Err(e) = self
            .store
            .add_pairing_token(&code.token, code.expires_at as i64)
        {
            warn!("配对 token 持久化失败: {e}");
        }
        info!(endpoint = %self.my_id.fmt_short(), "生成配对码");
        code
    }

    /// 清理过期 token。
    pub async fn purge_expired(&self) {
        let _ = self
            .store
            .consume_pairing_token("__nonexistent__", now_secs() as i64);
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
    /// 返回 `Ok(accepted)`；token 无效直接返回 Err。
    pub async fn on_pairing_request(
        &self,
        peer_id: EndpointId,
        token: &str,
        peer_name: &str,
        confirm_timeout: Duration,
    ) -> Result<bool> {
        if !self.consume_token(token).await {
            anyhow::bail!("配对 token 无效");
        }
        let request_id = {
            let mut b = [0u8; 8];
            use rand::RngCore;
            rand::rng().fill_bytes(&mut b);
            hex::encode(b)
        };
        let pending = PendingPairing {
            request_id: request_id.clone(),
            peer_id: peer_id.to_string(),
            peer_name: peer_name.to_string(),
            fingerprint: fingerprint(&peer_id),
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(request_id.clone(), (pending, tx));

        info!(request = %request_id, peer = %peer_name, "配对请求待确认");

        // 等待 UI 确认；超时视为拒绝并清理
        let accepted = match tokio::time::timeout(confirm_timeout, rx).await {
            Ok(Ok(decision)) => decision,
            _ => {
                self.pending.lock().await.remove(&request_id);
                warn!(request = %request_id, "配对确认超时");
                false
            }
        };

        if accepted {
            self.trust.add(&peer_id, peer_name)?;
            info!(peer = %peer_name, "配对完成");
        } else {
            info!(request = %request_id, "配对被拒绝");
        }
        Ok(accepted)
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
}

/// 公钥指纹：分组 hex，便于肉眼核对。
/// 从 hex 字符串生成指纹（数据库中存的是字符串）。
pub fn fingerprint_str(hex: &str) -> String {
    match hex.parse::<EndpointId>() {
        Ok(id) => fingerprint(&id),
        Err(_) => hex.chars().take(16).collect(),
    }
}

pub fn fingerprint(id: &EndpointId) -> String {
    let hex = id.to_string();
    hex.chars()
        .collect::<Vec<_>>()
        .chunks(4)
        .take(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// 本机 Tailscale 检测（转发，便于 CLI 使用）。
pub fn detect_tailscale() -> tailscale::TailscaleInfo {
    tailscale::detect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (PairingManager, Arc<Store>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let trust = Trust::new(store.clone());
        let id = iroh::SecretKey::generate().public();
        (PairingManager::new(trust, id, store.clone()), store)
    }

    /// 回归：pair 与 serve 是两个独立进程，token 必须经 SQLite 跨进程共享。
    #[tokio::test]
    async fn token_shared_across_pairing_manager_instances() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        // 模拟 `foldersync pair` 进程
        let pairer = PairingManager::new(
            Trust::new(store.clone()),
            iroh::SecretKey::generate().public(),
            store.clone(),
        );
        let code = pairer.create_code(Vec::new(), None).await;

        // 模拟 `foldersync serve` 进程（不同的 PairingManager 实例，同一 Store）
        let server = PairingManager::new(
            Trust::new(store.clone()),
            iroh::SecretKey::generate().public(),
            store.clone(),
        );
        let peer = iroh::SecretKey::generate().public();
        let r = server
            .on_pairing_request(peer, &code.token, "company", Duration::from_millis(30))
            .await;
        // token 被认可（进入等待确认，超时返回 false 而非报错）
        assert!(r.is_ok(), "跨实例 token 应被接受: {r:?}");
    }

    #[tokio::test]
    async fn rejects_unknown_token() {
        let (pm, _) = manager();
        let peer = iroh::SecretKey::generate().public();
        let r = pm
            .on_pairing_request(peer, "deadbeef", "b", Duration::from_millis(50))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn token_is_single_use() {
        let (pm, _) = manager();
        let peer = iroh::SecretKey::generate().public();
        let code = pm.create_code(Vec::new(), None).await;

        // 第一次使用：等待确认超时 → false
        let r1 = pm
            .on_pairing_request(peer, &code.token, "b", Duration::from_millis(30))
            .await;
        assert!(!r1.unwrap());

        // token 已消费，第二次必须失败
        let r2 = pm
            .on_pairing_request(peer, &code.token, "b", Duration::from_millis(30))
            .await;
        assert!(r2.is_err());
    }

    #[tokio::test]
    async fn accept_writes_trust() {
        let (pm, store) = manager();
        let peer = iroh::SecretKey::generate().public();
        let code = pm.create_code(Vec::new(), None).await;

        let pm2 = pm.clone();
        let peer_name = "company-pc";
        let handle = tokio::spawn(async move {
            pm2.on_pairing_request(peer, &code2_token(code), peer_name, Duration::from_secs(5))
                .await
        });

        // 等请求进入 pending，然后确认
        for _ in 0..50 {
            let pend = pm.list_pending().await;
            if let Some(p) = pend.first() {
                assert_eq!(p.peer_name, "company-pc");
                assert!(!p.fingerprint.is_empty());
                assert!(pm.confirm(&p.request_id.clone(), true).await);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let accepted = handle.await.unwrap().unwrap();
        assert!(accepted);
        // 已写入信任列表
        assert!(
            store
                .list_devices()
                .unwrap()
                .iter()
                .any(|d| d.name == "company-pc")
        );
    }

    fn code2_token(c: PairingCode) -> String {
        c.token
    }

    #[tokio::test]
    async fn reject_does_not_write_trust() {
        let (pm, store) = manager();
        let peer = iroh::SecretKey::generate().public();
        let code = pm.create_code(Vec::new(), None).await;
        let token = code.token.clone();

        let pm2 = pm.clone();
        let handle = tokio::spawn(async move {
            pm2.on_pairing_request(peer, &token, "x", Duration::from_secs(5))
                .await
        });

        for _ in 0..50 {
            if let Some(p) = pm.list_pending().await.first() {
                assert!(pm.confirm(&p.request_id.clone(), false).await);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!handle.await.unwrap().unwrap());
        assert!(store.list_devices().unwrap().is_empty());
    }

    #[test]
    fn fingerprint_is_grouped() {
        let id = iroh::SecretKey::generate().public();
        let f = fingerprint(&id);
        assert_eq!(f.split('-').count(), 4, "指纹应为 4 组: {f}");
    }
}
