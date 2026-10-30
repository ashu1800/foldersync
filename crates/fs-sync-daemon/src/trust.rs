//! 信任列表：只允许已配对设备的 SSH 公钥连接。

use std::sync::Arc;

use fs_sync_net::message::PeerInfo;
use fs_sync_store::{Device, Store};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("设备未授权: {0}")]
    NotTrusted(String),
    #[error("存储错误: {0}")]
    Store(#[from] fs_sync_store::StoreError),
}

/// 信任列表封装（设备列表 + 配对 token 暂存）。
#[derive(Clone)]
pub struct Trust {
    store: Arc<Store>,
}

impl Trust {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// 是否已信任该设备。
    pub fn is_trusted(&self, id: &str) -> Result<bool, TrustError> {
        Ok(self.store.list_devices()?.iter().any(|d| d.id == id))
    }

    /// 写入信任（幂等）。`PeerInfo` 是配对时交换回来的完整 SSH 信息。
    pub fn add(&self, peer: &PeerInfo) -> Result<(), TrustError> {
        self.store.upsert_device(&Device {
            id: peer.device_id.clone(),
            name: peer.device_name.clone(),
            paired_at: now_secs(),
            ssh_public_key: peer.ssh_public_key.clone(),
            host_key: peer.host_key.clone(),
            username: peer.username.clone(),
            addresses: peer.addrs.clone(),
            port: peer.port,
            relay_url: peer.relay.clone(),
            relay_room: peer.relay_room.clone(),
            relay_secret: peer.relay_secret.clone(),
        })?;
        Ok(())
    }

    /// 直接写入完整设备记录（重命名/更新地址时用）。
    pub fn upsert(&self, device: &Device) -> Result<(), TrustError> {
        self.store.upsert_device(device)?;
        Ok(())
    }

    /// 更新设备的 Tailscale 地址（对端 SSH 连入时从连接源地址学习）。
    pub fn set_addresses(&self, id: &str, addresses: Vec<String>) -> Result<(), TrustError> {
        if let Some(mut device) = self.get(id)?
            && device.addresses != addresses
        {
            device.addresses = addresses;
            self.upsert(&device)?;
        }
        Ok(())
    }

    /// 移除信任。
    pub fn remove(&self, id: &str) -> Result<(), TrustError> {
        self.store.remove_device(id)?;
        Ok(())
    }

    /// 设备名（未信任返回 None）。
    pub fn device_name(&self, id: &str) -> Result<Option<String>, TrustError> {
        Ok(self
            .store
            .list_devices()?
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| d.name))
    }

    /// 完整设备记录。
    pub fn get(&self, id: &str) -> Result<Option<Device>, TrustError> {
        Ok(self.store.list_devices()?.into_iter().find(|d| d.id == id))
    }

    /// 全部已信任设备。
    pub fn devices(&self) -> Result<Vec<Device>, TrustError> {
        Ok(self.store.list_devices()?)
    }

    /// 校验通过则返回设备名，否则 NotTrusted。
    pub fn require(&self, id: &str) -> Result<String, TrustError> {
        self.device_name(id)?
            .ok_or_else(|| TrustError::NotTrusted(id.to_string()))
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trust() -> Trust {
        Trust::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    fn peer(id: &str, name: &str) -> PeerInfo {
        PeerInfo {
            device_id: id.into(),
            device_name: name.into(),
            ssh_public_key: "ssh-ed25519 AAAA".into(),
            host_key: "ssh-ed25519 BBBB".into(),
            username: "foldersync".into(),
            port: 2222,
            ..Default::default()
        }
    }

    #[test]
    fn unknown_device_rejected() {
        let t = trust();
        assert!(!t.is_trusted("SHA256:x").unwrap());
        assert!(matches!(
            t.require("SHA256:x"),
            Err(TrustError::NotTrusted(_))
        ));
    }

    #[test]
    fn add_then_trusted() {
        let t = trust();
        t.add(&peer("SHA256:a", "home-pc")).unwrap();
        assert!(t.is_trusted("SHA256:a").unwrap());
        assert_eq!(t.require("SHA256:a").unwrap(), "home-pc");
        assert_eq!(t.devices().unwrap().len(), 1);
    }

    #[test]
    fn learns_peer_address() {
        let t = trust();
        t.add(&peer("SHA256:a", "home-pc")).unwrap();
        t.set_addresses("SHA256:a", vec!["100.64.0.2".into()])
            .unwrap();
        assert_eq!(
            t.get("SHA256:a").unwrap().unwrap().addresses,
            vec!["100.64.0.2"]
        );
    }

    #[test]
    fn remove_revokes() {
        let t = trust();
        t.add(&peer("SHA256:a", "x")).unwrap();
        t.remove("SHA256:a").unwrap();
        assert!(!t.is_trusted("SHA256:a").unwrap());
    }

    #[test]
    fn add_is_idempotent_and_updates_name() {
        let t = trust();
        t.add(&peer("SHA256:a", "old")).unwrap();
        t.add(&peer("SHA256:a", "new")).unwrap();
        assert_eq!(t.require("SHA256:a").unwrap(), "new");
        assert_eq!(t.devices().unwrap().len(), 1);
    }
}
