//! 配对码：`foldersync:<base64url(JSON)>` 文本，含对端 SSH 身份、地址、一次性 token。

use anyhow::{Context, Result, bail};
use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::message::PeerInfo;

/// 配对码默认有效期（秒）。
pub const DEFAULT_TTL_SECS: u64 = 300;

/// 配对码负载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    /// 发起方（显示配对码的一方）的 SSH 身份与网络地址。
    pub peer: PeerInfo,
    /// 一次性 token（16 字节 hex）
    pub token: String,
    /// 过期时间（Unix 秒）
    pub expires_at: u64,
}

/// 生成新配对码。
pub fn generate(peer: PeerInfo, now: u64) -> PairingCode {
    let token: [u8; 16] = rand::rng().random();
    PairingCode {
        peer,
        token: hex::encode(token),
        expires_at: now + DEFAULT_TTL_SECS,
    }
}

/// 序列化为可展示/可扫码文本。
pub fn encode(code: &PairingCode) -> String {
    let json = serde_json::to_vec(code).expect("PairingCode 序列化不应失败");
    format!(
        "foldersync:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    )
}

/// 解析并校验过期时间。
pub fn decode(text: &str, now: u64) -> Result<PairingCode> {
    let b64 = text
        .trim()
        .strip_prefix("foldersync:")
        .context("不是 FolderSync 配对码")?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(b64)
        .context("配对码 base64 解码失败")?;
    let code: PairingCode = serde_json::from_slice(&json).context("配对码 JSON 解析失败")?;
    if code.expires_at < now {
        bail!("配对码已过期");
    }
    Ok(code)
}

/// 当前 Unix 秒。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 配对阶段的中继房间：房间 ID = blake3(token)，口令 = token。
///
/// token 本身是 128 位随机数，知道它才能进房间；配对完成后改用正式的
/// 32 字节房间口令（见 [`new_relay_room`]）。
pub fn pair_room(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// 生成中继房间：随机 32 字节口令，房间 ID = blake3(口令)。
///
/// 这样中继服务端不需要预注册房间：只要校验 `blake3(口令) == 房间 ID`
/// 就能确认对方知道房间秘密，同时目录名仍是定长 hex。
pub fn new_relay_room() -> (String, String) {
    let mut secret = [0u8; 32];
    rand::rng().fill(&mut secret);
    let secret = hex::encode(secret);
    let room = blake3::hash(secret.as_bytes()).to_hex().to_string();
    (room, secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PeerInfo {
        PeerInfo {
            device_id: "SHA256:abc".into(),
            device_name: "home".into(),
            ssh_public_key: "ssh-ed25519 AAAA".into(),
            host_key: "ssh-ed25519 BBBB".into(),
            username: "foldersync".into(),
            port: 2222,
            addrs: vec!["100.64.0.1".into()],
            relay: Some("tcp://relay:9443".into()),
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_and_expiry() {
        let code = generate(peer(), 1_000_000);
        let text = encode(&code);
        assert!(text.starts_with("foldersync:"));
        assert_eq!(decode(&text, 1_000_000).unwrap(), code);
        assert!(decode(&text, code.expires_at).is_ok());
        assert!(decode(&text, code.expires_at + 1).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("not-a-code", 0).is_err());
        assert!(decode("foldersync:!!!", 0).is_err());
        assert!(decode("foldersync:e30", 0).is_err()); // "{}" 缺字段
    }

    #[test]
    fn token_is_hex16_bytes() {
        let code = generate(peer(), 0);
        assert_eq!(code.token.len(), 32);
        assert_eq!(hex::decode(&code.token).unwrap().len(), 16);
    }

    #[test]
    fn tokens_differ_between_codes() {
        let a = generate(peer(), 0);
        let b = generate(peer(), 0);
        assert_ne!(a.token, b.token);
    }

    #[test]
    fn relay_room_is_random_hex32() {
        let (room, secret) = new_relay_room();
        assert_eq!(room.len(), 64);
        assert_eq!(secret.len(), 64);
        assert_ne!(room, secret);
        assert_ne!(new_relay_room().0, room);
    }
}
