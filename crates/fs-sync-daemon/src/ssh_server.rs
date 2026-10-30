//! SSH 服务端：对端通过 Tailscale 直连进来时，提供 SFTP 与控制子系统。
//!
//! 认证规则：
//! - 公钥认证：公钥指纹必须已在信任列表里；
//! - 密码认证：只用于配对，密码是配对码里的一次性 token。
//!
//! 子系统：
//! - `sftp`：沙箱到 `<data>/share`，对端按 hash 读取 blob；
//! - `foldersync`：已信任设备的长连接控制消息；
//! - `foldersync-pair`：配对请求/响应。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fs_sync_net::conn::ConnType;
use fs_sync_net::frame;
use fs_sync_net::identity::{SshIdentity, public_key_id};
use fs_sync_net::message::{Message, PairingResponse};
use fs_sync_net::sftp_fs::FsSftpHandler;
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::pairing::PairingManager;
use crate::runtime::{FolderRuntime, PeerLoopConfig, handle_incoming};
use crate::trust::Trust;

/// SSH 服务端共享状态。
pub struct SshServerShared {
    pub identity: SshIdentity,
    pub host_key: russh::keys::PrivateKey,
    pub device_id: String,
    pub device_name: String,
    pub ssh_port: u16,
    pub data_dir: PathBuf,
    pub trust: Trust,
    pub pairing: PairingManager,
    pub runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
    /// 在线设备（服务端视角只可能是 Tailscale 直连）
    pub online: Arc<tokio::sync::Mutex<HashMap<String, ConnType>>>,
}

impl SshServerShared {
    fn peer_cfg(&self) -> PeerLoopConfig {
        PeerLoopConfig {
            identity: self.identity.clone(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
            ssh_port: self.ssh_port,
            mode: fs_sync_net::conn::NetworkMode::Tailscale,
            relay_url: None,
            folder_ids: self
                .runtimes
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.session.folder_id.clone())
                .collect(),
        }
    }
}

/// 运行中的 SSH 服务端；drop 即停止监听。
pub struct SshServerHandle {
    pub port: u16,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for SshServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// 启动 SSH 服务端（后台任务）。
pub async fn run_ssh_server(shared: Arc<SshServerShared>) -> Result<SshServerHandle> {
    let listener = match TcpListener::bind(("0.0.0.0", shared.ssh_port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && shared.ssh_port != 0 => {
            // 端口被占用时退回随机端口（设置页可改端口；不能让应用起不来）
            warn!(port = shared.ssh_port, "SSH 端口被占用，改用随机端口");
            TcpListener::bind(("0.0.0.0", 0)).await?
        }
        Err(e) => {
            return Err(e).with_context(|| format!("SSH 服务端监听 {} 失败", shared.ssh_port));
        }
    };
    let port = listener.local_addr()?.port();
    let config = Arc::new(server::Config {
        keys: vec![shared.host_key.clone()],
        inactivity_timeout: Some(Duration::from_secs(300)),
        auth_rejection_time: Duration::from_millis(300),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        max_auth_attempts: 6,
        nodelay: true,
        ..Default::default()
    });
    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let config = config.clone();
                            let shared = shared.clone();
                            let remote_ip = fs_sync_net::tailscale::is_tailscale_ip(peer.ip())
                                .then(|| peer.ip().to_string());
                            tokio::spawn(async move {
                                let handler = SshSession::new(shared, remote_ip);
                                if let Err(e) = server::run_stream(config, stream, handler).await {
                                    debug!(%peer, "SSH 会话失败: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            warn!("SSH accept 失败: {e}");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
            }
        }
    });
    info!(port, "SSH 服务端已启动");
    Ok(SshServerHandle {
        port,
        stop: Some(stop_tx),
    })
}

enum AuthState {
    None,
    Trusted { peer_id: String },
    Pairing,
}

struct SshSession {
    shared: Arc<SshServerShared>,
    auth: AuthState,
    channels: HashMap<russh::ChannelId, russh::Channel<Msg>>,
    /// 对端 SSH 源地址（仅 Tailscale 网段），用于学习对端地址。
    remote_ip: Option<String>,
}

impl SshSession {
    fn new(shared: Arc<SshServerShared>, remote_ip: Option<String>) -> Self {
        Self {
            shared,
            auth: AuthState::None,
            channels: HashMap::new(),
            remote_ip,
        }
    }
}

impl server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let id = public_key_id(key.public_key);
        if self.shared.trust.is_trusted(&id).unwrap_or(false) {
            info!(device = %id, "SSH 公钥认证通过");
            // 对端能从 Tailscale 连进来，说明它的当前地址就是连接源地址；
            // 反过来记录后，下一轮本机也能主动走 Tailscale，避免一端直连一端中继。
            if let Some(ip) = &self.remote_ip {
                let _ = self.shared.trust.set_addresses(&id, vec![ip.clone()]);
            }
            self.auth = AuthState::Trusted { peer_id: id };
            Ok(Auth::Accept)
        } else {
            warn!(device = %id, "未信任设备的 SSH 公钥，拒绝");
            Ok(Auth::reject())
        }
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        if self.shared.pairing.check_pairing_password(password) {
            info!("配对 token 密码认证通过");
            self.auth = AuthState::Pairing;
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: russh::ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(channel) = self.channels.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        match (name, &self.auth) {
            ("sftp", AuthState::Trusted { .. }) => {
                session.channel_success(channel_id)?;
                let root = self.shared.data_dir.join("share");
                let handler = FsSftpHandler::new(&root).map_err(|e| {
                    russh::Error::IO(std::io::Error::other(format!("SFTP 根目录不可用: {e}")))
                })?;
                tokio::spawn(async move {
                    russh_sftp::server::run(channel.into_stream(), handler).await;
                });
            }
            ("foldersync", AuthState::Trusted { peer_id }) => {
                session.channel_success(channel_id)?;
                let shared = self.shared.clone();
                let peer_id = peer_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = control_loop(shared, peer_id, channel).await {
                        debug!("控制通道结束: {e:#}");
                    }
                });
            }
            ("foldersync-pair", AuthState::Pairing) => {
                session.channel_success(channel_id)?;
                let shared = self.shared.clone();
                tokio::spawn(async move {
                    pairing_loop(shared, channel).await;
                });
            }
            _ => {
                session.channel_failure(channel_id)?;
            }
        }
        Ok(())
    }
}

/// 已信任设备的控制消息循环。
async fn control_loop(
    shared: Arc<SshServerShared>,
    peer_id: String,
    channel: russh::Channel<Msg>,
) -> Result<()> {
    let peer = match shared.trust.get(&peer_id)? {
        Some(p) => p,
        None => anyhow::bail!("设备已不在信任列表"),
    };
    shared
        .online
        .lock()
        .await
        .insert(peer_id.clone(), ConnType::Tailscale);
    info!(peer = %peer.name, "对端 SSH 控制通道已建立");

    let (mut reader, mut writer) = tokio::io::split(channel.into_stream());
    let result = async {
        loop {
            let msg = frame::read_frame(&mut reader).await?;
            match &msg {
                Message::Hello { .. } => {
                    frame::write_frame(
                        &mut writer,
                        &Message::Hello {
                            protocol_version: fs_sync_net::message::PROTOCOL_VERSION,
                            device_name: shared.device_name.clone(),
                            folders: Vec::new(),
                        },
                    )
                    .await?;
                }
                Message::Index { .. } | Message::IndexUpdate { .. } => {
                    let cfg = shared.peer_cfg();
                    handle_incoming(&msg, &peer, &cfg, &shared.runtimes, None).await?;
                }
                Message::Ping => {
                    frame::write_frame(&mut writer, &Message::Pong).await?;
                }
                _ => {}
            }
        }
    }
    .await;
    shared.online.lock().await.remove(&peer_id);
    info!(peer = %peer.name, "对端 SSH 控制通道已断开");
    result
}

/// 配对专用循环：只处理一次 PairingRequest。
async fn pairing_loop(shared: Arc<SshServerShared>, channel: russh::Channel<Msg>) {
    let (mut reader, mut writer) = tokio::io::split(channel.into_stream());
    let request = match frame::read_frame(&mut reader).await {
        Ok(Message::PairingRequest(req)) => req,
        _ => return,
    };
    let result = shared
        .pairing
        .on_pairing_request(request.peer, &request.token, Duration::from_secs(120))
        .await;
    let response = match result {
        Ok(Some(peer)) => PairingResponse {
            accepted: true,
            peer,
        },
        Ok(None) => PairingResponse {
            accepted: false,
            peer: Default::default(),
        },
        Err(e) => {
            warn!("配对请求处理失败: {e:#}");
            PairingResponse {
                accepted: false,
                peer: Default::default(),
            }
        }
    };
    let _ = frame::write_frame(&mut writer, &Message::PairingResponse(response)).await;
}

/// 供测试/诊断：本机 SSH 主机公钥文本。
pub fn host_public_key_text(key: &russh::keys::PrivateKey) -> String {
    key.public_key().to_openssh().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_sync_net::blobstore::BlobStore;
    use fs_sync_store::Store;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        shared: Arc<SshServerShared>,
        server: SshServerHandle,
        client_id: SshIdentity,
        _store: Arc<Store>,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let identity = SshIdentity::load_or_create(tmp.path()).unwrap();
        let host = fs_sync_net::identity::load_or_create_host_key(tmp.path()).unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let trust = Trust::new(store.clone());
        let client_id = SshIdentity::load_or_create(&tmp.path().join("client")).unwrap();
        // 把客户端公钥写进信任列表
        trust
            .add(&fs_sync_net::message::PeerInfo {
                device_id: client_id.id(),
                device_name: "client".into(),
                ssh_public_key: client_id.public_key_line().unwrap(),
                host_key: "ssh-ed25519 HOST".into(),
                username: fs_sync_net::SSH_USER.into(),
                port: 2222,
                ..Default::default()
            })
            .unwrap();
        let pairing = PairingManager::new(
            trust.clone(),
            identity.clone(),
            host_public_key_text(&host),
            0,
            "server".into(),
            None,
            store.clone(),
        );
        let shared = Arc::new(SshServerShared {
            identity,
            host_key: host,
            device_id: "server".into(),
            device_name: "server".into(),
            ssh_port: 0,
            data_dir: tmp.path().to_path_buf(),
            trust,
            pairing,
            runtimes: Arc::new(std::sync::Mutex::new(Vec::new())),
            online: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        });
        let server = run_ssh_server(shared.clone()).await.unwrap();
        Fixture {
            _tmp: tmp,
            shared,
            server,
            client_id,
            _store: store,
        }
    }

    #[tokio::test]
    async fn sftp_roundtrip_over_real_ssh() {
        let f = fixture().await;
        // 服务端 share/blobs 里放一个文件
        let blobs = BlobStore::open(f.shared.data_dir.join("share")).unwrap();
        let hash = blobs.add_bytes(b"hello over sftp".to_vec()).await.unwrap();

        let target = fs_sync_net::ssh_client::SshTarget {
            host: "127.0.0.1".into(),
            port: f.server.port,
            username: fs_sync_net::SSH_USER.into(),
            host_key: host_public_key_text(&f.shared.host_key),
        };
        let client = fs_sync_net::ssh_client::SshClient::connect(&target, &f.client_id)
            .await
            .unwrap();
        let local = BlobStore::open(f._tmp.path().join("client-share")).unwrap();
        fs_sync_net::ssh_client::SshClient::download_blob(&client.sftp(), &hash, &local)
            .await
            .unwrap();
        assert_eq!(local.get_bytes(&hash).await.unwrap(), b"hello over sftp");
    }

    #[tokio::test]
    async fn pairing_over_ssh_exchanges_keys() {
        let f = fixture().await;
        let code = f.shared.pairing.create_code(Vec::new(), None).await;
        let pm = f.shared.pairing.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if let Some(p) = pm.list_pending().await.into_iter().next() {
                    pm.confirm(&p.request_id, true).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let target = fs_sync_net::ssh_client::SshTarget {
            host: "127.0.0.1".into(),
            port: f.server.port,
            username: fs_sync_net::SSH_USER.into(),
            host_key: host_public_key_text(&f.shared.host_key),
        };
        let pair_client = SshIdentity::load_or_create(&f._tmp.path().join("pair-client")).unwrap();
        let request = fs_sync_net::message::PairingRequest {
            token: code.token.clone(),
            peer: fs_sync_net::message::PeerInfo {
                device_id: pair_client.id(),
                device_name: "pair-client".into(),
                ssh_public_key: pair_client.public_key_line().unwrap(),
                host_key: "ssh-ed25519 CLIENTHOST".into(),
                username: fs_sync_net::SSH_USER.into(),
                port: 2222,
                ..Default::default()
            },
        };
        let response = fs_sync_net::ssh_client::pair_via_ssh(&target, &code.token, request)
            .await
            .unwrap();
        assert!(response.accepted);
        assert!(f.shared.trust.is_trusted(&pair_client.id()).unwrap());
        assert!(response.peer.relay_room.is_some(), "应生成中继房间备用");
    }

    #[tokio::test]
    async fn untrusted_key_rejected() {
        let f = fixture().await;
        let stranger = SshIdentity::load_or_create(&f._tmp.path().join("stranger")).unwrap();
        let target = fs_sync_net::ssh_client::SshTarget {
            host: "127.0.0.1".into(),
            port: f.server.port,
            username: fs_sync_net::SSH_USER.into(),
            host_key: host_public_key_text(&f.shared.host_key),
        };
        let r = fs_sync_net::ssh_client::SshClient::connect(&target, &stranger).await;
        assert!(r.is_err(), "未信任公钥必须被拒绝");
    }
}
