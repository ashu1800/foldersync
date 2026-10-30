//! SSH 客户端：公钥认证 + `foldersync` 控制子系统 + SFTP 文件传输。
//!
//! 一台设备既是客户端（主动连对端）也是服务端（接受对端连入），
//! 所以这里只实现客户端一半；服务端在 `fs-sync-daemon::ssh_server`。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::blobstore::BlobStore;
use crate::frame;
use crate::identity::SshIdentity;
use crate::message::{Message, PairingRequest, PairingResponse};

/// 连接目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// 对端 SSH 主机公钥（`authorized_keys` 一行格式）
    pub host_key: String,
}

impl SshTarget {
    pub fn display(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

struct ClientHandler {
    expected_host_key: String,
    actual_host_key: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let openssh = key.public_key().to_openssh().unwrap_or_default();
        *self.actual_host_key.lock().unwrap() = Some(openssh.clone());
        // 主机密钥固定校验：配对码里已经带了，不接受的直接断开。
        Ok(openssh == self.expected_host_key)
    }
}

/// 一条已建立的 SSH 同步连接。
pub struct SshClient {
    _session: russh::client::Handle<ClientHandler>,
    reader: Box<dyn AsyncRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    sftp: Arc<tokio::sync::Mutex<SftpSession>>,
    rtt_ms: u64,
}

impl SshClient {
    /// 连接对端并完成公钥认证。
    pub async fn connect(target: &SshTarget, identity: &SshIdentity) -> Result<Self> {
        anyhow::ensure!(
            !target.host_key.trim().is_empty(),
            "缺少对端 SSH 主机公钥，请重新配对"
        );
        let actual = Arc::new(Mutex::new(None));
        let handler = ClientHandler {
            expected_host_key: target.host_key.clone(),
            actual_host_key: actual.clone(),
        };
        let config = Arc::new(russh::client::Config {
            inactivity_timeout: Some(Duration::from_secs(120)),
            ..Default::default()
        });
        let started = Instant::now();
        let mut session =
            russh::client::connect(config, (target.host.as_str(), target.port), handler)
                .await
                .with_context(|| format!("连接对端 {} 失败", target.display()))?;

        let key = PrivateKeyWithHashAlg::new(Arc::new(identity.private_key.clone()), None);
        let auth = session
            .authenticate_publickey(target.username.clone(), key)
            .await
            .context("SSH 公钥认证失败")?;
        if !auth.success() {
            bail!("SSH 公钥认证被拒绝（对端未信任本机？请重新配对）");
        }
        let rtt_ms = started.elapsed().as_millis() as u64;

        // 控制子系统
        let control = session
            .channel_open_session()
            .await
            .context("打开控制通道失败")?;
        control
            .request_subsystem(true, "foldersync")
            .await
            .context("对端不支持 foldersync 子系统")?;
        let (reader, writer) = tokio::io::split(control.into_stream());

        // SFTP 子系统
        let sftp_channel = session
            .channel_open_session()
            .await
            .context("打开 SFTP 通道失败")?;
        sftp_channel
            .request_subsystem(true, "sftp")
            .await
            .context("对端不支持 SFTP")?;
        let sftp = SftpSession::new(sftp_channel.into_stream())
            .await
            .context("初始化 SFTP 会话失败")?;

        Ok(Self {
            _session: session,
            reader: Box::new(reader),
            writer: Box::new(writer),
            sftp: Arc::new(tokio::sync::Mutex::new(sftp)),
            rtt_ms,
        })
    }

    /// 建连耗时（毫秒），作为 RTT 的近似。
    pub fn rtt_ms(&self) -> u64 {
        self.rtt_ms
    }

    /// 发一条控制消息。
    pub async fn send(&mut self, msg: &Message) -> Result<()> {
        frame::write_frame(&mut self.writer, msg).await
    }

    /// 收一条控制消息。
    ///
    /// 注意：不是取消安全的（内部两次 read_exact），不要直接放进 `select!`，
    /// 否则别的分支先就绪时会丢掉半帧，流从此错位。需要 select 时用 [`Self::spawn_reader`]。
    pub async fn recv(&mut self) -> Result<Message> {
        frame::read_frame(&mut self.reader).await
    }

    /// 把读半边移到独立任务里逐帧读取，经 channel 交出。channel 的 `recv` 是取消安全的。
    /// 读出错（含对端关闭）时发出最后一个 `Err` 后结束。之后不要再调用 [`Self::recv`]。
    pub fn spawn_reader(&mut self) -> tokio::sync::mpsc::Receiver<Result<Message>> {
        let mut reader = std::mem::replace(&mut self.reader, Box::new(tokio::io::empty()));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            loop {
                let r = frame::read_frame(&mut reader).await;
                let stop = r.is_err();
                if tx.send(r).await.is_err() || stop {
                    break;
                }
            }
        });
        rx
    }

    /// SFTP 会话句柄（多个拉取任务共享）。
    pub fn sftp(&self) -> Arc<tokio::sync::Mutex<SftpSession>> {
        self.sftp.clone()
    }

    /// 从对端 SFTP 下载 blob 到本地 blob 库。
    pub async fn download_blob(
        sftp: &Arc<tokio::sync::Mutex<SftpSession>>,
        hash: &str,
        store: &BlobStore,
    ) -> Result<()> {
        anyhow::ensure!(
            crate::blobstore::is_valid_hash(hash),
            "hash 格式无效（需 64 位 hex）"
        );
        if store.has(hash).await {
            return Ok(());
        }
        let tmp = store
            .root()
            .join(format!(".download-{}-{hash}", std::process::id()));
        {
            let sftp = sftp.lock().await;
            let mut remote = sftp
                .open(format!("blobs/{hash}"))
                .await
                .with_context(|| format!("对端缺少文件内容: {hash}"))?;
            let mut local = tokio::fs::File::create(&tmp)
                .await
                .context("创建下载临时文件失败")?;
            tokio::io::copy(&mut remote, &mut local)
                .await
                .context("从对端下载文件失败")?;
            tokio::io::AsyncWriteExt::flush(&mut local).await.ok();
        }
        store.adopt_downloaded(&tmp, hash).await.inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }
}

/// 用配对码里的 token 做 SSH 密码认证，走 `foldersync-pair` 子系统完成配对。
pub async fn pair_via_ssh(
    target: &SshTarget,
    token: &str,
    request: PairingRequest,
) -> Result<PairingResponse> {
    let actual = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        expected_host_key: target.host_key.clone(),
        actual_host_key: actual,
    };
    let config = Arc::new(russh::client::Config {
        inactivity_timeout: Some(Duration::from_secs(150)),
        ..Default::default()
    });
    let mut session = russh::client::connect(config, (target.host.as_str(), target.port), handler)
        .await
        .with_context(|| format!("连接对端 {} 失败", target.display()))?;
    let auth = session
        .authenticate_password(target.username.clone(), token)
        .await
        .context("配对密码认证失败")?;
    if !auth.success() {
        bail!("配对码无效或已过期");
    }
    let channel = session
        .channel_open_session()
        .await
        .context("打开配对通道失败")?;
    channel
        .request_subsystem(true, crate::SSH_PAIR_SUBSYSTEM)
        .await
        .context("对端不支持配对子系统")?;
    let (mut reader, mut writer) = tokio::io::split(channel.into_stream());
    frame::write_frame(&mut writer, &Message::PairingRequest(request)).await?;
    match frame::read_frame(&mut reader).await? {
        Message::PairingResponse(resp) => Ok(resp),
        Message::Error { message, .. } => bail!("对端返回错误: {message}"),
        _ => bail!("对端返回了意外的配对响应"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_display() {
        let t = SshTarget {
            host: "100.64.0.2".into(),
            port: 2222,
            username: "foldersync".into(),
            host_key: "ssh-ed25519 AAAA".into(),
        };
        assert_eq!(t.display(), "foldersync@100.64.0.2:2222");
    }
}
