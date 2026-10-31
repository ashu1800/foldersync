//! 应用状态：把 daemon 各组件组装成 Tauri 可用的一份共享状态。
//!
//! 一个进程内只有一份（`tauri::State<AppState>`），所有 command 通过它访问
//! SSH 服务端、存储、信任列表、运行时与设置。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use fs_sync_daemon::{
    FolderRuntime, PairingManager, PeerLoopConfig, SshServerHandle, SshServerShared, Trust,
    run_peer_loop, run_ssh_server,
};
use fs_sync_net::blobstore::BlobStore;
use fs_sync_net::identity::{SshIdentity, public_key_line};
use fs_sync_store::Store;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::dto::{ConnKind, DeviceDto, EnvStatusDto, FolderStatus, SettingsDto};
use crate::settings::SettingsStore;

/// 连接状态表：peer id → (连接方式, rtt 毫秒)。
pub type ConnMap = Arc<Mutex<HashMap<String, (ConnKind, Option<u64>)>>>;

/// 待确认配对请求：request_id → (设备名, 指纹, 过期时间)。
pub type PendingPairings = Arc<Mutex<HashMap<String, (String, String, i64)>>>;

/// 全局应用状态。
pub struct AppState {
    /// 数据目录
    pub data_dir: PathBuf,
    /// 本机 SSH 身份
    pub identity: SshIdentity,
    /// 本机 SSH 主机公钥
    pub host_public_key: String,
    /// 本机设备 ID（SSH 公钥指纹）
    pub device_id: String,
    /// 本机 SSH 服务端口
    pub ssh_port: u16,
    /// 索引与信任存储
    pub store: Arc<Store>,
    /// blob 存储（`<data>/share/blobs`）
    pub blobs: Arc<BlobStore>,
    /// 信任列表
    pub trust: Trust,
    /// 配对管理
    pub pairing: PairingManager,
    /// 本机设备名
    pub device_name: String,
    /// 已启动的同步文件夹运行时
    pub runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    /// 正在做首次扫描的文件夹（扫描完成后进入 runtimes）
    pub starting_folders: Arc<Mutex<HashSet<String>>>,
    /// 正在删除的文件夹：阻止首扫完成后把 runtime 又挂回来
    pub removing_folders: Arc<std::sync::Mutex<HashSet<String>>>,
    /// 连接状态：设备 id → (连接方式, rtt)
    pub conns: ConnMap,
    /// SSH 服务端观察到的在线设备
    pub online: Arc<Mutex<fs_sync_net::conn::Presence>>,
    /// 待确认的配对请求：request_id → (设备名, 指纹, 过期时间)
    pub pending_pairings: PendingPairings,
    /// 设置
    pub settings: Arc<SettingsStore>,
    /// 暂停全部同步
    pub paused_all: Arc<std::sync::atomic::AtomicBool>,
    /// 带宽限速
    pub limits: Arc<std::sync::RwLock<crate::limits::Limits>>,
    /// 对等连接配置（设置变更时可热更新）
    pub peer_cfg: Arc<std::sync::RwLock<PeerLoopConfig>>,
    /// SSH 服务端句柄（drop 即停）
    pub ssh_server: Mutex<Option<SshServerHandle>>,
}

impl AppState {
    /// 启动所有 daemon 组件。
    pub async fn init(data_dir: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&data_dir).context("创建数据目录失败")?;
        let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
        // 活动表只保留最近 2 万条，避免长期使用后数据库无限膨胀
        let _ = store.prune_activity(20_000);

        let settings = Arc::new(SettingsStore::load(&data_dir.join("settings.json"))?);
        let snapshot = settings.get();

        // 首次启动初始化 SSH 身份、主机密钥、SFTP 沙箱目录与防火墙
        let env = fs_sync_net::env::ensure_ssh_env(&data_dir, snapshot.ssh_port);
        let identity = SshIdentity::load_or_create(&data_dir).context("加载 SSH 身份失败")?;
        let host_key = fs_sync_net::identity::load_or_create_host_key(&data_dir)
            .context("加载 SSH 主机密钥失败")?;
        let host_public_key = public_key_line(host_key.public_key())?;
        let device_id = identity.id();

        let blobs =
            Arc::new(BlobStore::open(data_dir.join("share")).context("打开 blob 存储失败")?);

        let device_name = if snapshot.device_name.is_empty() {
            hostname()
        } else {
            snapshot.device_name.clone()
        };

        let sync_root = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| data_dir.clone())
            .join("FolderSync");

        let trust = Trust::new(store.clone());
        let pairing = PairingManager::new(
            trust.clone(),
            identity.clone(),
            host_public_key.clone(),
            env.ssh_port,
            device_name.clone(),
            snapshot.relay_url.clone(),
            store.clone(),
        );
        let runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let removing_folders: Arc<std::sync::Mutex<HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(HashSet::new()));
        let paused_all = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let online: Arc<Mutex<fs_sync_net::conn::Presence>> =
            Arc::new(Mutex::new(Default::default()));
        let shared = Arc::new(SshServerShared {
            identity: identity.clone(),
            host_key,
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            ssh_port: env.ssh_port,
            data_dir: data_dir.clone(),
            sync_root: sync_root.clone(),
            trust: trust.clone(),
            pairing: pairing.clone(),
            store: store.clone(),
            blobs: blobs.clone(),
            invite_lock: tokio::sync::Mutex::new(()),
            removing_folders: removing_folders.clone(),
            paused_all: paused_all.clone(),
            runtimes: runtimes.clone(),
            online: online.clone(),
        });
        let server = run_ssh_server(shared)
            .await
            .context("启动 SSH 服务端失败")?;
        // 端口被占用时服务端会回退随机端口，配对码要公布实际端口
        let actual_port = server.port;
        pairing.set_ssh_port(actual_port);

        let peer_cfg = Arc::new(std::sync::RwLock::new(PeerLoopConfig {
            identity: identity.clone(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            ssh_port: actual_port,
            mode: snapshot.network_mode.into(),
            relay_url: snapshot.relay_url.clone(),
            store: store.clone(),
            blobs: blobs.clone(),
            sync_root: sync_root.clone(),
            paused_all: paused_all.clone(),
        }));

        let state = Arc::new(Self {
            data_dir,
            identity,
            host_public_key,
            device_id,
            ssh_port: actual_port,
            store,
            blobs,
            trust,
            pairing,
            device_name,
            runtimes,
            starting_folders: Arc::new(Mutex::new(HashSet::new())),
            removing_folders,
            conns: Arc::new(Mutex::new(HashMap::new())),
            online,
            pending_pairings: Arc::new(Mutex::new(HashMap::new())),
            settings,
            paused_all,
            limits: Arc::new(std::sync::RwLock::new(crate::limits::Limits::new(
                snapshot.upload_limit_kbps,
                snapshot.download_limit_kbps,
            ))),
            peer_cfg,
            ssh_server: Mutex::new(Some(server)),
        });

        // 启动对等连接循环（信任列表为空时只是空转）
        let cfg = state.peer_cfg.clone();
        let runtimes = state.runtimes.clone();
        let trust = state.trust.clone();
        let online = state.online.clone();
        tokio::spawn(async move {
            run_peer_loop(cfg, runtimes, trust, Some(online)).await;
        });

        Ok(state)
    }

    /// 为某个文件夹启动实时同步运行时。
    pub async fn start_folder(
        &self,
        folder_id: String,
        path: PathBuf,
    ) -> Result<Arc<FolderRuntime>> {
        // 首次扫描可能几十秒（大目录）：先登记「扫描中」，避免界面误报“错误”
        self.starting_folders.lock().await.insert(folder_id.clone());
        let started = FolderRuntime::start(
            folder_id.clone(),
            path.clone(),
            self.device_id.clone(),
            self.store.clone(),
            self.blobs.clone(),
            400,
        )
        .await;
        self.starting_folders.lock().await.remove(&folder_id);
        let rt = started?;
        // 首扫期间用户可能已暂停/删除文件夹：不要把手动停止后的 runtime 又挂回去
        let current = self
            .store
            .list_folders()
            .unwrap_or_default()
            .into_iter()
            .find(|f| f.id == folder_id);
        let paused_all = self.paused_all.load(std::sync::atomic::Ordering::Relaxed);
        match current {
            Some(f) if !f.paused && !paused_all => {
                rt.set_shared_with(f.shared_with);
            }
            _ => {
                rt.cancel();
                return Err(anyhow::anyhow!("文件夹已暂停或删除，放弃启动"));
            }
        }
        // 注入桌面端限速器：每次传输后按当前设置结算，改设置无需重启同步
        let upload_limits = self.limits.clone();
        let download_limits = self.limits.clone();
        let upload: fs_sync_daemon::runtime::ThrottleFn = Arc::new(move |bytes| {
            let limiter = upload_limits
                .read()
                .map(|l| l.upload.clone())
                .unwrap_or_else(|e| e.into_inner().upload.clone());
            Box::pin(async move { limiter.consume(bytes).await })
        });
        let download: fs_sync_daemon::runtime::ThrottleFn = Arc::new(move |bytes| {
            let limiter = download_limits
                .read()
                .map(|l| l.download.clone())
                .unwrap_or_else(|e| e.into_inner().download.clone());
            Box::pin(async move { limiter.consume(bytes).await })
        });
        if let Ok(mut guard) = rt.session.throttle.write() {
            *guard = fs_sync_daemon::runtime::TransferThrottle {
                upload: Some(upload),
                download: Some(download),
            };
        }
        let rt = Arc::new(rt);
        let old = {
            // 锁顺序与 remove_folder 一致：removing -> runtimes
            let removing = self.removing_folders.lock().unwrap();
            if removing.contains(&folder_id) {
                drop(removing);
                rt.cancel();
                return Err(anyhow::anyhow!("文件夹正在删除，放弃启动"));
            }
            let mut guard = self.runtimes.lock().unwrap();
            let current = self
                .store
                .list_folders()
                .unwrap_or_default()
                .into_iter()
                .find(|f| f.id == folder_id);
            let still_valid = current
                .map(|f| !f.paused && f.local_path == path)
                .unwrap_or(false)
                && !self.paused_all.load(std::sync::atomic::Ordering::Relaxed);
            if !still_valid {
                drop(guard);
                drop(removing);
                rt.cancel();
                return Err(anyhow::anyhow!("文件夹已暂停、删除或路径已变化，放弃启动"));
            }
            let old = guard
                .iter()
                .find(|r| r.session.folder_id == folder_id)
                .cloned();
            guard.retain(|r| r.session.folder_id != folder_id);
            guard.push(rt.clone());
            old
        };
        if let Some(old) = old {
            old.cancel();
        }
        Ok(rt)
    }

    /// 停止并移除某个文件夹的运行时。
    pub async fn stop_folder(&self, folder_id: &str) {
        let removed = {
            let mut guard = self.runtimes.lock().unwrap();
            let found = guard
                .iter()
                .find(|r| r.session.folder_id == folder_id)
                .cloned();
            guard.retain(|r| r.session.folder_id != folder_id);
            found
        };
        if let Some(rt) = removed {
            rt.cancel();
        }
    }

    /// 已配对设备列表（含在线状态）。
    pub async fn devices(&self) -> Vec<DeviceDto> {
        let conns = self.conns.lock().await;
        let online = self.online.lock().await;
        self.trust
            .devices()
            .unwrap_or_default()
            .into_iter()
            .map(|d| {
                let (conn, rtt) =
                    conns
                        .get(&d.id)
                        .copied()
                        .unwrap_or_else(|| match online.get(&d.id) {
                            Some(fs_sync_net::ConnType::Tailscale) => {
                                (ConnKind::Tailscale, online.rtt(&d.id))
                            }
                            Some(fs_sync_net::ConnType::Relay) => {
                                (ConnKind::Relay, online.rtt(&d.id))
                            }
                            None => (ConnKind::Unknown, online.rtt(&d.id)),
                        });
                DeviceDto {
                    fingerprint: fs_sync_net::identity::fingerprint_text(&d.id),
                    online: conn != ConnKind::Unknown,
                    conn,
                    rtt_ms: rtt,
                    id: d.id,
                    name: d.name,
                    address: d.addresses.first().cloned(),
                    port: d.port,
                    relay_ready: d.relay_room.is_some(),
                }
            })
            .collect()
    }

    /// 各文件夹状态。
    pub async fn folders(&self) -> Vec<crate::dto::FolderDto> {
        let paused_all = self.paused_all.load(std::sync::atomic::Ordering::Relaxed);
        let runtimes: Vec<Arc<FolderRuntime>> = self.runtimes.lock().unwrap().clone();
        let starting = self.starting_folders.lock().await.clone();
        let mut out = Vec::new();
        for f in self.store.list_folders().unwrap_or_default() {
            let runtime = runtimes
                .iter()
                .find(|r| r.session.folder_id == f.id)
                .cloned();
            let pending = match &runtime {
                Some(rt) => rt.pending_count().await,
                None => 0,
            };
            let paused = f.paused || paused_all;
            let missing_path = if f.local_path.is_dir() {
                None
            } else {
                Some("本地文件夹不存在，请恢复路径或移除该同步项".to_string())
            };
            let last_error = runtime
                .as_ref()
                .and_then(|rt| rt.last_error())
                .or(missing_path);
            let status = if paused {
                FolderStatus::Paused
            } else if starting.contains(&f.id) {
                FolderStatus::Scanning
            } else if last_error.is_some() {
                FolderStatus::Error
            } else if let Some(_rt) = runtime {
                if pending > 0 {
                    FolderStatus::Syncing
                } else {
                    FolderStatus::Idle
                }
            } else {
                FolderStatus::Error
            };
            let live = self.store.count_live_entries(&f.id).unwrap_or(0);
            out.push(crate::dto::FolderDto {
                id: f.id.clone(),
                name: f.name,
                path: f.local_path.to_string_lossy().to_string(),
                status,
                paused,
                last_sync_at: self.last_sync_at(&f.id),
                pending,
                file_count: live,
                shared_with: f.shared_with.clone(),
                last_error,
            });
        }
        out
    }

    /// 暂停全部文件夹（托盘与命令共用）。
    pub async fn pause_all_sync(&self) {
        self.paused_all
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let ids: Vec<String> = self
            .store
            .list_folders()
            .unwrap_or_default()
            .into_iter()
            .map(|f| f.id)
            .collect();
        for id in ids {
            self.stop_folder(&id).await;
        }
    }

    /// 恢复全部文件夹（托盘与命令共用）。
    pub async fn resume_all_sync(&self) {
        self.paused_all
            .store(false, std::sync::atomic::Ordering::Relaxed);
        for f in self.store.list_folders().unwrap_or_default() {
            if !f.paused {
                let _ = self.start_folder(f.id, f.local_path).await;
            }
        }
    }

    fn last_sync_at(&self, folder_id: &str) -> Option<i64> {
        self.store
            .list_activity(Some(folder_id), 1)
            .ok()
            // store 里是毫秒，DTO 约定 Unix 秒；以前直接透传，界面显示成 58798 年
            .and_then(|v| v.first().map(|a| a.at / 1000))
    }

    /// 记录一条活动。
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

    /// 更新设置并持久化；SSH 端口/中继/模式改动会同步到运行时。
    pub fn update_settings(&self, patch: crate::dto::SettingsPatch) -> Result<SettingsDto> {
        let next = self.settings.update(patch)?;
        if let Ok(mut cfg) = self.peer_cfg.write() {
            cfg.mode = next.network_mode.into();
            cfg.relay_url = next.relay_url.clone();
            cfg.ssh_port = next.ssh_port;
        }
        self.replace_limits(next.upload_limit_kbps, next.download_limit_kbps);
        Ok(next)
    }

    /// 当前设置。
    pub fn settings_dto(&self) -> SettingsDto {
        self.settings.get()
    }

    /// 本机环境状态。
    pub fn env_status(&self) -> EnvStatusDto {
        let settings = self.settings.get();
        let mut env: EnvStatusDto = fs_sync_net::env::detect_env(
            &self.data_dir,
            self.ssh_port,
            settings.relay_url.as_deref(),
        )
        .into();
        // 就绪与否必须结合当前网络模式判断：只配了中继但模式=tailscale 时不能算可用
        env.ready = match settings.network_mode {
            crate::dto::NetworkMode::Tailscale => {
                env.ssh_ready && env.tailscale_installed && env.tailscale_running
            }
            crate::dto::NetworkMode::Relay => {
                env.ssh_ready && env.relay_configured && env.relay_reachable
            }
            crate::dto::NetworkMode::Auto => {
                env.ssh_ready
                    && ((env.tailscale_installed && env.tailscale_running)
                        || (env.relay_configured && env.relay_reachable))
            }
        };
        env
    }

    /// 替换限速器。
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

/// 从设备 ID 生成可读指纹。
pub fn fingerprint_of(id: &str) -> String {
    fs_sync_net::identity::fingerprint_text(id)
}

/// 从字符串生成指纹（兼容旧数据）。
pub fn fingerprint_str(id: &str) -> String {
    fs_sync_net::identity::fingerprint_text(id)
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

/// 重连退避策略。
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    next: std::time::Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    const INITIAL: std::time::Duration = std::time::Duration::from_secs(2);
    const MAX: std::time::Duration = std::time::Duration::from_secs(60);

    pub fn new() -> Self {
        Self {
            next: Self::INITIAL,
        }
    }

    pub fn current(&self) -> std::time::Duration {
        self.next
    }

    pub fn reset(&mut self) {
        self.next = Self::INITIAL;
    }

    pub fn back_off(&mut self) {
        self.next = (self.next * 2).min(Self::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_grouped_and_stable() {
        let fp = fingerprint_str("SHA256:abcdefghijklmnopqrstuvwxyz");
        assert!(fp.len() > 8);
        assert_eq!(fp, fingerprint_str("SHA256:abcdefghijklmnopqrstuvwxyz"));
        assert_ne!(fp, fingerprint_str("SHA256:zyxwvutsrqponmlkjihgfedcba"));
    }

    #[test]
    fn hostname_is_non_empty() {
        assert!(!hostname().is_empty());
    }

    #[test]
    fn backoff_starts_at_two_seconds() {
        assert_eq!(Backoff::new().current(), std::time::Duration::from_secs(2));
    }

    #[test]
    fn backoff_doubles_and_caps_at_sixty() {
        let mut b = Backoff::new();
        let mut seen = Vec::new();
        for _ in 0..10 {
            seen.push(b.current().as_secs());
            b.back_off();
        }
        assert_eq!(seen[0], 2);
        assert_eq!(seen[1], 4);
        assert_eq!(seen[2], 8);
        assert_eq!(b.current(), std::time::Duration::from_secs(60));
        assert!(seen.iter().all(|&s| s <= 60));
    }

    #[test]
    fn backoff_resets_after_success() {
        let mut b = Backoff::new();
        b.back_off();
        b.back_off();
        assert_eq!(b.current(), std::time::Duration::from_secs(8));
        b.reset();
        assert_eq!(b.current(), std::time::Duration::from_secs(2));
    }
}
