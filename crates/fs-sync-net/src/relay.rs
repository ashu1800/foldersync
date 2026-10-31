//! 中继服务器客户端：用 SSH + SFTP 连接中继的“房间”。
//!
//! 中继服务端只提供两件事：
//! 1. 按房间隔离的 SFTP 文件空间（房间 ID 就是身份，口令二次校验由服务端实现）；
//! 2. 无入站端口要求 —— 客户端只主动向外连，NAT 后也能用。
//!
//! 文件传输全部走 SFTP；控制信息以 JSON 文件形式放在房间里，双方轮询。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use russh::keys::PublicKeyOrCertificate;

use crate::message::{PairingRequest, PairingResponse};
use russh_sftp::client::SftpSession;
use tokio::io::AsyncWriteExt;

/// 中继地址（`tcp://host:port` 或 `host:port`）。
pub fn parse_relay_url(url: &str) -> Result<(String, u16)> {
    let raw = url
        .trim()
        .strip_prefix("tcp://")
        .or_else(|| url.trim().strip_prefix("ssh://"))
        // 兼容 0.1.x 的 iroh 中继地址（http://IP:8444）
        .or_else(|| url.trim().strip_prefix("http://"))
        .or_else(|| url.trim().strip_prefix("https://"))
        .unwrap_or(url.trim());
    let (host, port) = raw
        .rsplit_once(':')
        .with_context(|| format!("中继地址缺少端口: {url}（格式 tcp://IP:9443）"))?;
    let port: u16 = port.parse().context("中继端口无效")?;
    anyhow::ensure!(!host.is_empty(), "中继地址缺少主机");
    Ok((host.to_string(), port))
}

struct RelayHandler {
    expected_host_key: Option<String>,
    actual_host_key: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for RelayHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let openssh = key.public_key().to_openssh().unwrap_or_default();
        *self.actual_host_key.lock().unwrap() = Some(openssh.clone());
        match &self.expected_host_key {
            // 首次连接：TOFU，由调用方保存 actual_host_key
            None => Ok(true),
            Some(expected) => Ok(expected == &openssh),
        }
    }
}

/// 一条已认证的中继 SFTP 连接。
pub struct RelaySftp {
    /// 保持会话存活；SFTP 通道依赖它。
    _session: russh::client::Handle<RelayHandler>,
    sftp: SftpSession,
    host_key: String,
}

impl RelaySftp {
    /// 连接中继房间。`expected_host_key` 为空表示首次连接（TOFU）。
    pub async fn connect(
        relay_url: &str,
        room: &str,
        secret: &str,
        expected_host_key: Option<&str>,
    ) -> Result<Self> {
        let (host, port) = parse_relay_url(relay_url)?;
        let actual = Arc::new(Mutex::new(None));
        let handler = RelayHandler {
            expected_host_key: expected_host_key.map(str::to_string),
            actual_host_key: actual.clone(),
        };
        let config = Arc::new(russh::client::Config {
            inactivity_timeout: Some(Duration::from_secs(120)),
            ..Default::default()
        });
        let mut session = russh::client::connect(config, (host.as_str(), port), handler)
            .await
            .context("连接中继服务器失败")?;
        let auth = session
            .authenticate_password(room, secret)
            .await
            .context("中继认证失败")?;
        if !auth.success() {
            bail!("中继房间口令错误或房间不存在");
        }
        let host_key = actual.lock().unwrap().clone().unwrap_or_default();

        let channel = session
            .channel_open_session()
            .await
            .context("打开中继会话失败")?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .context("中继不支持 SFTP")?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .context("初始化 SFTP 失败")?;
        Ok(Self {
            _session: session,
            sftp,
            host_key,
        })
    }

    /// 中继服务端主机公钥（首次连接后应保存，后续固定校验）。
    pub fn host_key(&self) -> &str {
        &self.host_key
    }

    /// 读一个小文件；不存在返回 None，其他读取错误照实返回。
    pub async fn read_text(&self, path: &str) -> Result<Option<String>> {
        match self.sftp.read(path).await {
            Ok(data) => Ok(Some(String::from_utf8(data).context("中继文件不是 UTF-8")?)),
            Err(russh_sftp::client::error::Error::Status(s))
                if s.status_code == russh_sftp::protocol::StatusCode::NoSuchFile =>
            {
                Ok(None)
            }
            Err(e) => Err(anyhow::anyhow!("读取中继文件失败: {path}: {e}")),
        }
    }

    /// 远端文件的 (大小, 修改时间)，用于“变了才读”。
    pub async fn stamp(&self, path: &str) -> Option<(u64, std::time::SystemTime)> {
        let meta = self.sftp.metadata(path).await.ok()?;
        meta.modified().ok().map(|m| (meta.len(), m))
    }

    /// 确保远程文件的父目录存在（SFTP 没有 mkdir -p，逐级创建）。
    async fn ensure_parent(&self, path: &str) -> Result<()> {
        let Some((parent, _)) = path.rsplit_once('/') else {
            return Ok(());
        };
        let mut current = String::new();
        for part in parent.split('/') {
            if part.is_empty() {
                continue;
            }
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            let _ = self.sftp.create_dir(current.clone()).await;
        }
        Ok(())
    }

    /// 原子写一个小文件：先写 `.tmp` 再 rename。
    pub async fn write_text(&self, path: &str, text: &str) -> Result<()> {
        self.ensure_parent(path).await?;
        let tmp = format!(
            "{path}.tmp-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        );
        let mut file = self
            .sftp
            .create(tmp.clone())
            .await
            .with_context(|| format!("写中继文件失败: {path}"))?;
        file.write_all(text.as_bytes())
            .await
            .with_context(|| format!("写中继文件失败: {path}"))?;
        file.flush().await.ok();
        file.close().await.ok();
        // SFTP 的 rename 在 Windows 上不覆盖已存在文件：先删旧文件再提交。
        let _ = self.sftp.remove_file(path).await;
        self.sftp
            .rename(tmp.clone(), path)
            .await
            .with_context(|| format!("提交中继文件失败: {path}"))?;
        let _ = tmp;
        Ok(())
    }

    /// 下载文件到本地（流式，不占内存）。
    pub async fn download_to(&self, remote: &str, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let tmp = dest.with_extension(format!(".relay-tmp-{}", std::process::id()));
        let mut src = self
            .sftp
            .open(remote)
            .await
            .with_context(|| format!("中继文件不存在: {remote}"))?;
        let mut out = tokio::fs::File::create(&tmp)
            .await
            .context("创建下载临时文件失败")?;
        tokio::io::copy(&mut src, &mut out)
            .await
            .context("下载中继文件失败")?;
        out.flush().await.ok();
        drop(out);
        tokio::fs::rename(&tmp, dest)
            .await
            .context("落盘下载文件失败")?;
        Ok(())
    }

    /// 上传本地文件（流式）。
    pub async fn upload(&self, remote: &str, src: &Path) -> Result<()> {
        self.ensure_parent(remote).await?;
        let tmp = format!(
            "{remote}.tmp-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        );
        let mut input = tokio::fs::File::open(src)
            .await
            .with_context(|| format!("打开本地文件失败: {}", src.display()))?;
        let mut dst = self
            .sftp
            .create(tmp.clone())
            .await
            .with_context(|| format!("创建中继文件失败: {remote}"))?;
        tokio::io::copy(&mut input, &mut dst)
            .await
            .context("上传中继文件失败")?;
        dst.flush().await.ok();
        dst.shutdown().await.ok();
        drop(dst);
        self.sftp
            .rename(tmp.clone(), remote)
            .await
            .with_context(|| format!("提交中继文件失败: {remote}"))?;
        let _ = tmp;
        Ok(())
    }

    /// 文件是否存在（避免重复上传）。
    pub async fn exists(&self, remote: &str) -> bool {
        self.sftp.metadata(remote).await.is_ok()
    }

    /// 删除文件（忽略不存在）。
    pub async fn remove(&self, remote: &str) {
        let _ = self.sftp.remove_file(remote).await;
    }
}

/// 通过中继完成配对（设备 B 侧）：写请求文件 → 轮询响应文件。
pub async fn pair_via_relay(
    relay_url: &str,
    token: &str,
    request: PairingRequest,
) -> Result<PairingResponse> {
    let room = crate::pairing::pair_room(token);
    let relay = RelaySftp::connect(relay_url, &room, token, None)
        .await
        .context("连接配对中继失败")?;
    let request_json = serde_json::to_string(&request)?;
    relay
        .write_text("pairing/request.json", &request_json)
        .await?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(130);
    loop {
        if tokio::time::Instant::now() > deadline {
            bail!("等待对端确认配对超时（130 秒）");
        }
        if let Some(text) = relay.read_text("pairing/response.json").await? {
            let response: PairingResponse =
                serde_json::from_str(&text).context("配对响应格式错误")?;
            return Ok(response);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relay_urls() {
        assert_eq!(
            parse_relay_url("tcp://1.2.3.4:9443").unwrap(),
            ("1.2.3.4".into(), 9443)
        );
        assert_eq!(
            parse_relay_url("relay.example.com:22").unwrap(),
            ("relay.example.com".into(), 22)
        );
        // 兼容 0.1.x 保存的 iroh http 地址
        assert_eq!(
            parse_relay_url("http://36.137.140.126:8444").unwrap(),
            ("36.137.140.126".into(), 8444)
        );
        assert!(parse_relay_url("no-port").is_err());
        assert!(parse_relay_url("tcp://:9443").is_err());
    }
}
