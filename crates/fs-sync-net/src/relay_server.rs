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
    /// 单房间磁盘上限（字节）。客户端上传超限时 SFTP 写入报错。
    /// 0 = 不限额。公网部署建议设置，防匿名磁盘填充 DoS。
    pub room_quota_bytes: u64,
    /// healthz/指标 HTTP 监听地址；None = 不开启。
    /// 例 `127.0.0.1:9444`。返回 JSON：status / rooms / disk_bytes。
    pub health_addr: Option<std::net::SocketAddr>,
}

/// 运行中的中继服务端；drop 即停止监听与后台任务。
pub struct RelayServerHandle {
    pub port: u16,
    stop: Arc<tokio::sync::Notify>,
}

impl Drop for RelayServerHandle {
    fn drop(&mut self) {
        self.stop.notify_waiters();
    }
}

/// 启动中继服务端。
pub async fn run_relay_server(config: RelayServerConfig) -> Result<RelayServerHandle> {
    std::fs::create_dir_all(config.data_dir.join("rooms")).context("创建中继数据目录失败")?;
    let host_key = load_or_create_host_key(&config.data_dir)?;
    // 双栈监听：[::] 和 0.0.0.0 各绑一个。不能假设 [::] 一定收 v4-mapped
    //（实测有 Windows 环境绑 v6 成功但 127.0.0.1 被拒）。真双栈时第二个
    // bind 报 AddrInUse = 已覆盖，忽略。v6/v4 都绑不上才报错。
    let v6 = TcpListener::bind(("::", config.port)).await.ok();
    let v6_port = match &v6 {
        Some(l) => l.local_addr()?.port(),
        None => config.port,
    };
    let v4 = match TcpListener::bind(("0.0.0.0", v6_port)).await {
        Ok(l) => Some(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
        Err(e) if v6.is_some() => {
            warn!("IPv4 监听失败（已有 IPv6）: {e}");
            None
        }
        Err(e) => return Err(e).with_context(|| format!("中继监听 {} 失败", config.port)),
    };
    let listeners: Vec<TcpListener> = [v6, v4].into_iter().flatten().collect();
    anyhow::ensure!(!listeners.is_empty(), "中继无法绑定任何监听地址");
    let port = listeners[0].local_addr()?.port();
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
    let quota_bytes = config.room_quota_bytes;
    // 停止信号：多个后台任务共享，oneshot 只有一份所以用 Notify。
    let stop = Arc::new(tokio::sync::Notify::new());
    // cleanup 独立任务：以前跟 accept 挤在同一个 select! 里，clean 房间
    // （遍历整棵房间树）期间新连接全部排队。现在各跑各的。
    {
        let rooms_dir = rooms_dir.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut cleanup = tokio::time::interval(Duration::from_secs(6 * 3600));
            cleanup.tick().await;
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    _ = cleanup.tick() => cleanup_rooms(&rooms_dir, retention_days),
                }
            }
        });
    }
    // healthz/指标端点：部署方探测存活与磁盘占用用。
    if let Some(health_addr) = config.health_addr {
        let rooms_dir = rooms_dir.clone();
        let listener = TcpListener::bind(health_addr)
            .await
            .with_context(|| format!("healthz 监听 {health_addr} 失败"))?;
        info!(addr = %health_addr, "healthz 端点已开启");
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let rooms_dir = rooms_dir.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        // 丢弃请求头（不需要解析），直接回 JSON
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf).await;
                        let (rooms, disk) = rooms_stats(&rooms_dir);
                        let body = format!(
                            "{{\"status\":\"ok\",\"rooms\":{rooms},\"disk_bytes\":{disk}}}"
                        );
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    });
                }
            }
        });
    }
    // 每个 listener 一个 accept 任务（v4/v6 各一），统一用 stop Notify。
    for listener in listeners {
        let ssh_config = ssh_config.clone();
        let data_dir = data_dir.clone();
        let accept_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_stop.notified() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => {
                                let config = ssh_config.clone();
                                let data_dir = data_dir.clone();
                                tokio::spawn(async move {
                                    let handler = RelaySession::new(data_dir, quota_bytes);
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
    }
    info!(port, dir = %config.data_dir.display(), "中继服务器已启动");
    Ok(RelayServerHandle { port, stop })
}

/// 房间数量与磁盘占用（healthz 用）。
fn rooms_stats(rooms_dir: &std::path::Path) -> (usize, u64) {
    let mut count = 0;
    let mut total = 0u64;
    fn dir_size(p: &std::path::Path, total: &mut u64) {
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                dir_size(&path, total);
            } else if let Ok(m) = e.metadata() {
                *total += m.len();
            }
        }
    }
    let Ok(rd) = std::fs::read_dir(rooms_dir) else {
        return (0, 0);
    };
    for entry in rd.flatten() {
        if entry.path().is_dir() {
            count += 1;
            dir_size(&entry.path(), &mut total);
        }
    }
    (count, total)
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
    /// 单房间磁盘上限（字节），0 = 不限
    quota_bytes: u64,
}

impl RelaySession {
    fn new(data_dir: PathBuf, quota_bytes: u64) -> Self {
        Self {
            data_dir,
            room: None,
            channels: HashMap::new(),
            quota_bytes,
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
        let mut handler = FsSftpHandler::new(&root)
            .map_err(|e| russh::Error::IO(std::io::Error::other(e.to_string())))?;
        if self.quota_bytes > 0 {
            let quota_root = root.clone();
            let quota = self.quota_bytes;
            handler = handler.with_quota_check(Box::new(move || {
                let (mut total, _) = (0u64, ());
                fn dir_size(p: &std::path::Path, total: &mut u64) {
                    let Ok(rd) = std::fs::read_dir(p) else {
                        return;
                    };
                    for e in rd.flatten() {
                        let path = e.path();
                        if path.is_dir() {
                            dir_size(&path, total);
                        } else if let Ok(m) = e.metadata() {
                            *total += m.len();
                        }
                    }
                }
                dir_size(&quota_root, &mut total);
                total < quota
            }));
        }
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
            room_quota_bytes: 0,
            health_addr: None,
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
            room_quota_bytes: 0,
            health_addr: None,
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
