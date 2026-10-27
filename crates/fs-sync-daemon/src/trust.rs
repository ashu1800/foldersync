//! 信任列表：只允许已配对设备连接。

use std::sync::Arc;

use fs_sync_store::{Device, Store};
use iroh::EndpointId;
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
    pub fn is_trusted(&self, id: &EndpointId) -> Result<bool, TrustError> {
        let target = id.to_string();
        Ok(self.store.list_devices()?.iter().any(|d| d.id == target))
    }

    /// 写入信任（幂等）。
    pub fn add(&self, id: &EndpointId, name: &str) -> Result<(), TrustError> {
        self.store.upsert_device(&Device {
            id: id.to_string(),
            name: name.to_string(),
            paired_at: now_secs(),
        })?;
        Ok(())
    }

    /// 移除信任。
    pub fn remove(&self, id: &EndpointId) -> Result<(), TrustError> {
        self.store.remove_device(&id.to_string())?;
        Ok(())
    }

    /// 设备名（未信任返回 None）。
    pub fn device_name(&self, id: &EndpointId) -> Result<Option<String>, TrustError> {
        let target = id.to_string();
        Ok(self
            .store
            .list_devices()?
            .into_iter()
            .find(|d| d.id == target)
            .map(|d| d.name))
    }

    /// 全部已信任设备。
    pub fn devices(&self) -> Result<Vec<Device>, TrustError> {
        Ok(self.store.list_devices()?)
    }

    /// 校验通过则返回设备名，否则 NotTrusted。
    pub fn require(&self, id: &EndpointId) -> Result<String, TrustError> {
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
    use iroh::SecretKey;

    fn trust() -> Trust {
        Trust::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn unknown_device_rejected() {
        let t = trust();
        let id = SecretKey::generate().public();
        assert!(!t.is_trusted(&id).unwrap());
        assert!(matches!(t.require(&id), Err(TrustError::NotTrusted(_))));
    }

    #[test]
    fn add_then_trusted() {
        let t = trust();
        let id = SecretKey::generate().public();
        t.add(&id, "home-pc").unwrap();
        assert!(t.is_trusted(&id).unwrap());
        assert_eq!(t.require(&id).unwrap(), "home-pc");
        assert_eq!(t.devices().unwrap().len(), 1);
    }

    #[test]
    fn remove_revokes() {
        let t = trust();
        let id = SecretKey::generate().public();
        t.add(&id, "x").unwrap();
        t.remove(&id).unwrap();
        assert!(!t.is_trusted(&id).unwrap());
    }

    #[test]
    fn add_is_idempotent_and_updates_name() {
        let t = trust();
        let id = SecretKey::generate().public();
        t.add(&id, "old").unwrap();
        t.add(&id, "new").unwrap();
        assert_eq!(t.require(&id).unwrap(), "new");
        assert_eq!(t.devices().unwrap().len(), 1);
    }
}
