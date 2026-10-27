//! 配对码：`foldersync:<base64url(JSON)>` 文本，含端点公钥 + 一次性 token + 过期时间。

use anyhow::{Context, Result, bail};
use base64::Engine;
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// 配对码默认有效期（秒）。
pub const DEFAULT_TTL_SECS: u64 = 300;

/// 配对码负载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    pub endpoint_id: String,
    /// 一次性 token（16 字节 hex）
    pub token: String,
    /// 过期时间（Unix 秒）
    pub expires_at: u64,
    /// 附加直连地址（如本机 Tailscale IP:端口），可为空
    #[serde(default)]
    pub addrs: Vec<String>,
}

/// 生成新配对码。
pub fn generate(my_id: &str, now: u64) -> PairingCode {
    let token: [u8; 16] = rand::rng().random();
    PairingCode {
        endpoint_id: my_id.to_string(),
        token: hex::encode(token),
        expires_at: now + DEFAULT_TTL_SECS,
        addrs: Vec::new(),
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

/// 配对码 → 拨号地址（含附加直连地址）。
pub fn to_endpoint_addr(code: &PairingCode) -> Result<EndpointAddr> {
    let id: EndpointId = code
        .endpoint_id
        .parse()
        .context("配对码中的 EndpointId 无效")?;
    let mut addr = EndpointAddr::from(id);
    for s in &code.addrs {
        if let Ok(sa) = s.parse::<std::net::SocketAddr>() {
            addr.addrs.insert(TransportAddr::Ip(sa));
        }
    }
    Ok(addr)
}

/// 当前 Unix 秒。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_expiry() {
        let id = iroh::SecretKey::generate().public().to_string();
        let code = generate(&id, 1_000_000);
        let text = encode(&code);
        assert!(text.starts_with("foldersync:"));
        assert_eq!(decode(&text, 1_000_000).unwrap(), code);
        // 恰好过期时刻仍有效
        assert!(decode(&text, code.expires_at).is_ok());
        // 过期一秒后失效
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
        let code = generate(&iroh::SecretKey::generate().public().to_string(), 0);
        assert_eq!(code.token.len(), 32);
        assert_eq!(hex::decode(&code.token).unwrap().len(), 16);
    }

    #[test]
    fn tokens_differ_between_codes() {
        let a = generate(&iroh::SecretKey::generate().public().to_string(), 0);
        let b = generate(&iroh::SecretKey::generate().public().to_string(), 0);
        assert_ne!(a.token, b.token);
    }

    #[test]
    fn extra_addrs_survive_roundtrip() {
        let mut code = generate(&iroh::SecretKey::generate().public().to_string(), 0);
        code.addrs = vec!["100.102.104.88:40000".into(), "bad-addr".into()];
        let text = encode(&code);
        let got = decode(&text, 0).unwrap();
        assert_eq!(got.addrs.len(), 2);
        let addr = to_endpoint_addr(&got).unwrap();
        // 只有合法地址被注入
        assert_eq!(addr.addrs.len(), 1);
    }
}
