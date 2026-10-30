//! 设备身份：SSH 密钥对与 SSH 主机密钥（0600 权限）。
//!
//! - `ssh_identity`：客户端私钥，配对时把公钥交给对端，对端把它写进信任列表。
//! - `ssh_host_ed25519`：本机 SSH 服务端主机密钥，配对码里带公钥供对端校验。
//!
//! 两者都在首次启动时生成，后续只读取。旧版本的 `secret.key`（iroh）不再使用。

use anyhow::{Context, Result};
use russh::keys::{Algorithm, PrivateKey, PublicKey, ssh_key};
use std::path::{Path, PathBuf};

/// 一台设备的 SSH 身份。
#[derive(Clone)]
pub struct SshIdentity {
    pub private_key: PrivateKey,
    pub public_key: PublicKey,
}

impl std::fmt::Debug for SshIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshIdentity")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

impl SshIdentity {
    /// 读取或生成客户端身份密钥。
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir).context("创建数据目录失败")?;
        let file = identity_path(data_dir);
        let private_key = if file.exists() {
            russh::keys::load_secret_key(&file, None).context("读取 SSH 身份密钥失败")?
        } else {
            let key = generate_key()?;
            write_private_key(&file, &key)?;
            key
        };
        let public_key = private_key.public_key().clone();
        Ok(Self {
            private_key,
            public_key,
        })
    }

    /// 设备 ID：SSH 公钥的 SHA256 指纹（形如 `SHA256:xxxx`）。
    pub fn id(&self) -> String {
        public_key_id(&self.public_key)
    }

    /// `authorized_keys` 一行格式。
    pub fn public_key_line(&self) -> Result<String> {
        public_key_line(&self.public_key)
    }
}

/// 读取或生成 SSH 服务端主机密钥。
pub fn load_or_create_host_key(data_dir: &Path) -> Result<PrivateKey> {
    std::fs::create_dir_all(data_dir).context("创建数据目录失败")?;
    let file = host_key_path(data_dir);
    if file.exists() {
        russh::keys::load_secret_key(&file, None).context("读取 SSH 主机密钥失败")
    } else {
        let key = generate_key()?;
        write_private_key(&file, &key)?;
        Ok(key)
    }
}

/// 生成 Ed25519 私钥。
pub fn generate_key() -> Result<PrivateKey> {
    Ok(PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519)?)
}

/// 保存私钥为 OpenSSH 文本格式，并收紧权限。
pub fn write_private_key(path: &Path, key: &PrivateKey) -> Result<()> {
    let text = key
        .to_openssh(ssh_key::LineEnding::LF)
        .context("编码 SSH 私钥失败")?;
    std::fs::write(path, text.as_bytes()).context("写入 SSH 私钥失败")?;
    restrict_permissions(path);
    Ok(())
}

/// 公钥的 `authorized_keys` 一行文本。
pub fn public_key_line(key: &PublicKey) -> Result<String> {
    key.to_openssh().context("编码 SSH 公钥失败")
}

/// 校验对端公布的 `PeerInfo` 自洽：设备 ID 必须等于 SSH 公钥指纹，且主机公钥非空。
pub fn verify_peer_info(peer: &crate::message::PeerInfo) -> anyhow::Result<()> {
    anyhow::ensure!(
        !peer.host_key.trim().is_empty(),
        "对端没有提供 SSH 主机公钥"
    );
    let parsed = parse_public_key(&peer.ssh_public_key)?;
    anyhow::ensure!(
        public_key_id(&parsed) == peer.device_id,
        "对端设备 ID 与 SSH 公钥不一致"
    );
    Ok(())
}

/// 公钥设备 ID（SHA256 指纹）。
pub fn public_key_id(key: &PublicKey) -> String {
    format!("{}", key.fingerprint(ssh_key::HashAlg::Sha256))
}

/// 解析 `authorized_keys` 一行（或裸 base64）为公钥。
pub fn parse_public_key(text: &str) -> Result<PublicKey> {
    let text = text.trim();
    // ssh_key 的 from_openssh 同时接受 "ssh-ed25519 AAAA..." 与裸 base64
    PublicKey::from_openssh(text).context("解析 SSH 公钥失败")
}

/// 公钥指纹（分组 hex，便于 UI 人工核对）。
pub fn fingerprint_text(id: &str) -> String {
    let body = id.strip_prefix("SHA256:").unwrap_or(id);
    let body = body.trim_end_matches('=');
    body.chars()
        .collect::<Vec<_>>()
        .chunks(4)
        .take(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn identity_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh_identity")
}

pub fn host_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh_host_ed25519")
}

/// 文件权限收紧：Unix 0600。Windows 上用户 profile 目录的默认 ACL 已限定为本人。
fn restrict_permissions(file: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let a = SshIdentity::load_or_create(tmp.path()).unwrap();
        let b = SshIdentity::load_or_create(tmp.path()).unwrap();
        assert_eq!(a.id(), b.id());
        assert!(a.id().starts_with("SHA256:"));
        // 公钥可以重新解析，且 id 一致
        let parsed = parse_public_key(&a.public_key_line().unwrap()).unwrap();
        assert_eq!(public_key_id(&parsed), a.id());
    }

    #[test]
    fn host_key_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let a = load_or_create_host_key(tmp.path()).unwrap();
        let b = load_or_create_host_key(tmp.path()).unwrap();
        assert_eq!(
            a.public_key().to_openssh().unwrap(),
            b.public_key().to_openssh().unwrap()
        );
    }

    #[test]
    fn fingerprint_is_grouped() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SshIdentity::load_or_create(tmp.path()).unwrap().id();
        let fp = fingerprint_text(&id);
        assert_eq!(fp.split('-').count(), 4, "指纹应为 4 组: {fp}");
    }

    #[test]
    fn malformed_public_key_rejected() {
        assert!(parse_public_key("not a key").is_err());
    }
}
