//! 中继服务器：一个按房间隔离的 SFTP 服务端。
//!
//! 房间协议：
//! - SSH 用户名 = 房间 ID（64 位 hex）
//! - SSH 密码 = 房间口令（64 位 hex）
//! - 服务端校验 `blake3(口令) == 房间 ID`，无需预注册房间
//!
//! 房间目录：`<data_dir>/rooms/<room-id>/`，客户端只看到自己房间的 `/`。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::identity::load_or_create_host_key;
use crate::sftp_fs::FsSftpHandler;

/// 中继服务端配置。
#[derive(Debug, Clone)]
pub struct RelayServerConfig {
    pub data_dir: PathBuf,
    pub port: u16,
    /// 空闲房间保留天数，0 表示不清理
    pub room_retention_days: u64,
}

/// 运行中的中继服务端；drop 即停止监听。
pub struct RelayServerHandle {
    pub port: u16,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for RelayServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// 启动中继服务端。
pub async fn run_relay_server(config: RelayServerConfig) -> Result<RelayServerHandle> {
    std::fs::create_dir_all(config.data_dir.join("rooms")).context("创建中继数据目录失败")?;
    let host_key = load_or_create_host_key(&config.data_dir)?;
    let listener = TcpListener::bind(("0.0.0.0", config.port))
        .await
        .with_context(|| format!("中继监听 {} 失败", config.port))?;
    let port = listener.local_addr()?.port();
    let ssh_config = Arc::new(server::Config {
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(300)),
        auth_rejection_time: Duration::from_millis(200),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        max_auth_attempts: 4,
        nodelay: true,
        ..Default::default()
    });
    let data_dir = config.data_dir.clone();
    let rooms_dir = data_dir.join("rooms");
    let retention_days = config.room_retention_days;
    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut cleanup = tokio::time::interval(Duration::from_secs(6 * 3600));
        cleanup.tick().await;
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = cleanup.tick() => cleanup_rooms(&rooms_dir, retention_days),
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let config = ssh_config.clone();
                            let data_dir = data_dir.clone();
                            tokio::spawn(async move {
                                let handler = RelaySession::new(data_dir);
                                if let Err(e) = server::run_stream(config, stream, handler).await {
                                    debug!(%peer, "中继 SSH 会话失败: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            warn!("中继 accept 失败: {e}");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
            }
        }
    });
    info!(port, dir = %config.data_dir.display(), "中继服务器已启动");
    Ok(RelayServerHandle {
        port,
        stop: Some(stop_tx),
    })
}

/// 删除超过保留期的空闲房间；房间内最新文件时间作为活跃时间。
fn cleanup_rooms(rooms_dir: &std::path::Path, retention_days: u64) {
    if retention_days == 0 {
        return;
    }
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(retention_days.saturating_mul(86_400)))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(rooms_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(newest) = newest_mtime(&path) else {
            continue;
        };
        if newest < cutoff && std::fs::remove_dir_all(&path).is_ok() {
            info!(room = %path.display(), "已清理过期中继房间");
        }
    }
}

fn newest_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    // 不跟随符号链接，避免房间里的链接把清理逻辑带到数据目录之外。
    if meta.file_type().is_symlink() {
        return None;
    }
    if !meta.is_dir() {
        return meta.modified().ok();
    }
    let mut newest = None;
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        if let Some(t) = newest_mtime(&entry.path())
            && newest.is_none_or(|current| t > current)
        {
            newest = Some(t);
        }
    }
    // 空房间用目录自身时间，避免永远不清理。
    newest.or_else(|| meta.modified().ok())
}

struct RelaySession {
    data_dir: PathBuf,
    room: Option<String>,
    channels: HashMap<russh::ChannelId, russh::Channel<Msg>>,
}

impl RelaySession {
    fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            room: None,
            channels: HashMap::new(),
        }
    }
}

/// 房间 ID 必须是 64 位 hex，且等于 blake3(口令)。
pub fn room_matches_password(room: &str, password: &str) -> bool {
    room.len() == 64
        && room.bytes().all(|b| b.is_ascii_hexdigit())
        && blake3::hash(password.as_bytes()).to_hex().as_str() == room
}

impl server::Handler for RelaySession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if room_matches_password(user, password) {
            self.room = Some(user.to_string());
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
        if name != "sftp" || self.room.is_none() {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let room = self.room.clone().unwrap();
        let root = self.data_dir.join("rooms").join(&room);
        let handler = FsSftpHandler::new(&root)
            .map_err(|e| russh::Error::IO(std::io::Error::other(e.to_string())))?;
        session.channel_success(channel_id)?;
        tokio::spawn(async move {
            russh_sftp::server::run(channel.into_stream(), handler).await;
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::new_relay_room;
    use crate::relay::RelaySftp;

    #[test]
    fn password_must_match_room_hash() {
        let (room, secret) = new_relay_room();
        assert!(room_matches_password(&room, &secret));
        assert!(!room_matches_password(&room, "wrong"));
        assert!(!room_matches_password("short", &secret));
    }

    #[test]
    fn cleanup_removes_only_expired_rooms() {
        let tmp = tempfile::tempdir().unwrap();
        let rooms = tmp.path().join("rooms");
        let old = rooms.join("old");
        let new = rooms.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let old_file = old.join("index.json");
        let file = std::fs::File::create(&old_file).unwrap();
        let old_time = std::time::SystemTime::now() - Duration::from_secs(40 * 86_400);
        file.set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        cleanup_rooms(&rooms, 30);
        assert!(!old.exists());
        assert!(new.exists());
    }

    #[tokio::test]
    async fn two_clients_share_one_room_over_sftp() {
        let tmp = tempfile::tempdir().unwrap();
        let server = run_relay_server(RelayServerConfig {
            data_dir: tmp.path().join("relay"),
            port: 0,
            room_retention_days: 30,
        })
        .await
        .unwrap();
        let (room, secret) = new_relay_room();

        let a = RelaySftp::connect(
            &format!("tcp://127.0.0.1:{}", server.port),
            &room,
            &secret,
            None,
        )
        .await
        .unwrap();
        a.write_text("devices/a/index.json", r#"{"hello":"world"}"#)
            .await
            .unwrap();

        let b = RelaySftp::connect(
            &format!("tcp://127.0.0.1:{}", server.port),
            &room,
            &secret,
            Some(a.host_key()),
        )
        .await
        .unwrap();
        let text = b.read_text("devices/a/index.json").await.unwrap();
        assert_eq!(text.as_deref(), Some(r#"{"hello":"world"}"#));

        // 覆盖写：中继同步会反复更新索引，必须支持替换已有文件
        a.write_text("devices/a/index.json", r#"{"hello":"again"}"#)
            .await
            .unwrap();
        let text = b.read_text("devices/a/index.json").await.unwrap();
        assert_eq!(text.as_deref(), Some(r#"{"hello":"again"}"#));
    }

    #[tokio::test]
    async fn wrong_password_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let server = run_relay_server(RelayServerConfig {
            data_dir: tmp.path().join("relay"),
            port: 0,
            room_retention_days: 30,
        })
        .await
        .unwrap();
        let (room, _secret) = new_relay_room();
        let r = RelaySftp::connect(
            &format!("tcp://127.0.0.1:{}", server.port),
            &room,
            "deadbeef",
            None,
        )
        .await;
        assert!(r.is_err());
    }
}
