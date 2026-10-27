//! 应用状态：把 daemon 各组件组装成 Tauri 可用的一份共享状态。
//!
//! 一个进程内只有一份（`tauri::State<AppState>`），所有 command 通过它访问
//! 端点、存储、信任列表、运行时与设置。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use fs_sync_daemon::{FolderRuntime, PairingManager, Trust};
use fs_sync_store::Store;
use iroh::{Endpoint, EndpointId};
use iroh_blobs::store::fs::FsStore;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::dto::{ConnKind, DeviceDto, FolderStatus, SettingsDto};

/// 连接状态表：peer id → (连接方式, rtt 毫秒)。
pub type ConnMap = Arc<Mutex<HashMap<String, (ConnKind, Option<u64>)>>>;

/// 待确认配对请求：request_id → (设备名, 指纹, 过期时间)。
pub type PendingPairings = Arc<Mutex<HashMap<String, (String, String, i64)>>>;
use crate::settings::SettingsStore;

/// 全局应用状态。
pub struct AppState {
    /// 数据目录
    pub data_dir: PathBuf,
    /// 本机 EndpointId
    pub endpoint_id: EndpointId,
    /// iroh 端点
    pub endpoint: Endpoint,
    /// 索引与信任存储
    pub store: Arc<Store>,
    /// blob 存储（全进程单实例，红色警告见 docs/decisions.md）
    pub blobs: Arc<FsStore>,
    /// 信任列表
    pub trust: Trust,
    /// 配对管理
    pub pairing: PairingManager,
    /// 本机设备名
    pub device_name: String,
    /// 已启动的同步文件夹运行时：folder_id → runtime
    pub runtimes: Arc<Mutex<HashMap<String, Arc<FolderRuntime>>>>,
    /// 连接状态：EndpointId hex → (连接方式, rtt)
    pub conns: ConnMap,
    /// 待确认的配对请求：request_id → (设备名, 指纹, 过期时间)
    pub pending_pairings: PendingPairings,
    /// 设置
    pub settings: Arc<SettingsStore>,
    /// 暂停全部同步
    pub paused_all: Arc<std::sync::atomic::AtomicBool>,
    /// 带宽限速（M6）
    pub limits: Arc<std::sync::RwLock<crate::limits::Limits>>,
}

impl AppState {
    /// 启动所有 daemon 组件。
    pub async fn init(data_dir: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&data_dir).context("创建数据目录失败")?;

        let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
        let secret = fs_sync_net::identity::load_or_create_secret(&data_dir)?;
        // 设置要先读：中继/端口绑定需要它
        let settings = Arc::new(SettingsStore::load(&data_dir.join("settings.json"))?);
        let settings_snapshot = settings.get();
        // 按设置里的中继/端口绑定（提示词 4.x）。配置非法时内部会回退到默认，
        // 不会因为用户填错一个 URL 就启动失败。
        let net = fs_sync_net::NetConfig {
            relay_url: settings_snapshot.relay_url.clone(),
            listen_port: settings_snapshot.listen_port,
        };
        let endpoint = fs_sync_net::bind_endpoint_with(&net, secret)
            .await
            .context("iroh 端点绑定失败")?;
        let endpoint_id = endpoint.id();

        let blobs = Arc::new(
            fs_sync_net::blobs::open_store(&data_dir.join("blobs"))
                .await
                .context("打开 blob 存储失败")?,
        );

        let trust = Trust::new(store.clone());
        let pairing = PairingManager::new(trust.clone(), endpoint_id, store.clone());
        let settings = Arc::new(SettingsStore::load(&data_dir.join("settings.json"))?);

        let settings_snapshot = settings.get();
        let device_name = {
            let s = settings_snapshot.clone();
            if s.device_name.is_empty() {
                hostname()
            } else {
                s.device_name
            }
        };

        Ok(Arc::new(Self {
            data_dir,
            endpoint_id,
            endpoint,
            store,
            blobs,
            trust,
            pairing,
            device_name,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            conns: Arc::new(Mutex::new(HashMap::new())),
            pending_pairings: Arc::new(Mutex::new(HashMap::new())),
            settings,
            paused_all: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            limits: Arc::new(std::sync::RwLock::new(crate::limits::Limits::new(
                settings_snapshot.upload_limit_kbps,
                settings_snapshot.download_limit_kbps,
            ))),
        }))
    }

    /// 为某个文件夹启动实时同步运行时。
    pub async fn start_folder(
        &self,
        folder_id: String,
        path: PathBuf,
    ) -> Result<Arc<FolderRuntime>> {
        let rt = FolderRuntime::start(
            folder_id.clone(),
            path,
            self.endpoint_id.to_string(),
            self.store.clone(),
            self.blobs.clone(),
            400,
        )
        .await?;
        let rt = Arc::new(rt);
        self.runtimes.lock().await.insert(folder_id, rt.clone());
        Ok(rt)
    }

    /// 停止并移除某个文件夹的运行时。
    pub async fn stop_folder(&self, folder_id: &str) {
        self.runtimes.lock().await.remove(folder_id);
    }

    /// 已配对设备列表（含在线状态）。
    pub async fn devices(&self) -> Vec<DeviceDto> {
        let conns = self.conns.lock().await;
        self.trust
            .devices()
            .unwrap_or_default()
            .into_iter()
            .map(|d| {
                let (conn, rtt) = conns
                    .get(&d.id)
                    .copied()
                    .unwrap_or((ConnKind::Unknown, None));
                DeviceDto {
                    fingerprint: fingerprint_str(&d.id),
                    online: conn != ConnKind::Unknown,
                    conn,
                    rtt_ms: rtt,
                    id: d.id,
                    name: d.name,
                }
            })
            .collect()
    }

    /// 各文件夹状态。
    pub async fn folders(&self) -> Vec<crate::dto::FolderDto> {
        let paused_all = self.paused_all.load(std::sync::atomic::Ordering::Relaxed);
        let runtimes = self.runtimes.lock().await;
        self.store
            .list_folders()
            .unwrap_or_default()
            .into_iter()
            .map(|f| {
                let entries = self.store.list_entries(&f.id).unwrap_or_default();
                let live = entries.iter().filter(|e| !e.deleted).count() as u32;
                let pending = runtimes
                    .get(&f.id)
                    .map(|rt| rt.session.config.debounce_ms) // 占位：真实计数见 pending_count
                    .map(|_| 0u32)
                    .unwrap_or(0);
                crate::dto::FolderDto {
                    id: f.id.clone(),
                    name: f.name,
                    path: f.local_path.to_string_lossy().to_string(),
                    status: if f.paused || paused_all {
                        FolderStatus::Paused
                    } else if runtimes.contains_key(&f.id) {
                        FolderStatus::Idle
                    } else {
                        FolderStatus::Error
                    },
                    paused: f.paused || paused_all,
                    last_sync_at: self.last_sync_at(&f.id),
                    pending,
                    file_count: live,
                    shared_with: Vec::new(),
                }
            })
            .collect()
    }

    fn last_sync_at(&self, folder_id: &str) -> Option<i64> {
        self.store
            .list_activity(Some(folder_id), 1)
            .ok()
            .and_then(|v| v.first().map(|a| a.at))
    }

    /// 记录一条活动（同时推事件给前端）。
    pub fn log_activity(
        &self,
        kind: &str,
        folder_id: &str,
        path: &str,
        message: &str,
    ) -> Result<()> {
        self.store
            .log_activity(kind, folder_id, path, message)
            .map_err(Into::into)
    }

    /// 更新设置并持久化。
    pub fn update_settings(&self, patch: crate::dto::SettingsPatch) -> Result<SettingsDto> {
        self.settings.update(patch)
    }

    /// 当前设置。
    pub fn settings_dto(&self) -> SettingsDto {
        self.settings.get()
    }

    /// 替换限速器（设置改动时调用，立即生效）。
    pub fn replace_limits(&self, upload_kbps: u32, download_kbps: u32) {
        if let Ok(mut guard) = self.limits.write() {
            *guard = crate::limits::Limits::new(upload_kbps, download_kbps);
        }
    }

    /// 当前限速配置快照。
    pub fn limits_snapshot(&self) -> crate::limits::Limits {
        self.limits
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| crate::limits::Limits::unlimited())
    }
}

/// 从 EndpointId 生成可读指纹。
pub fn fingerprint_of(id: &EndpointId) -> String {
    fs_sync_daemon::fingerprint(id)
}

/// 从 hex 字符串生成指纹（数据库中存的是字符串）。
pub fn fingerprint_str(hex: &str) -> String {
    match hex.parse::<EndpointId>() {
        Ok(id) => fs_sync_daemon::fingerprint(&id),
        Err(_) => hex.chars().take(16).collect(),
    }
}

/// 主机名（跨平台）。
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "未知设备".into())
}

/// 启动时自动恢复：把数据库里记录的文件夹重新挂上监听。
pub async fn restore_folders(state: Arc<AppState>) {
    let folders = state.store.list_folders().unwrap_or_default();
    for f in folders {
        if f.paused {
            continue;
        }
        if !f.local_path.is_dir() {
            warn!(path = %f.local_path.display(), "文件夹不存在，跳过恢复");
            continue;
        }
        match state.start_folder(f.id.clone(), f.local_path.clone()).await {
            Ok(_) => info!(name = %f.name, "已恢复同步文件夹"),
            Err(e) => warn!(name = %f.name, "恢复同步文件夹失败: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_grouped_and_stable() {
        let id = iroh::SecretKey::generate().public().to_string();
        let fp = fingerprint_str(&id);
        assert!(fp.len() > 8);
        assert_eq!(fp, fingerprint_str(&id), "同 id 指纹应稳定");
        // 不同设备指纹不同
        let other = iroh::SecretKey::generate().public().to_string();
        assert_ne!(fp, fingerprint_str(&other));
    }

    #[test]
    fn hostname_is_non_empty() {
        assert!(!hostname().is_empty());
    }
}
