//! 消息定义与编解码：长度前缀帧 + postcard。
//!
//! 帧格式：`u32` 大端长度 + postcard 编码的 `Message`。
//! 所有消息携带 `protocol_version`，不匹配即报错断连。
//!
//! v2 起不再有 iroh：控制消息走 SSH 的 `foldersync` 子系统，
//! 文件内容走 SFTP，按 blake3 hash 取。

use serde::{Deserialize, Serialize};

use fs_sync_core::FileEntry;

/// 当前协议版本。
pub const PROTOCOL_VERSION: u16 = 2;

/// 文件夹摘要（Hello 中交换）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSummary {
    pub folder_id: String,
    pub name: String,
}

/// 一台设备的完整配对信息。
///
/// 配对码只带“发起方”的这一份；配对请求/响应各带一份。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    /// SSH 公钥 SHA256 指纹（设备 ID）
    pub device_id: String,
    pub device_name: String,
    /// `authorized_keys` 一行格式
    pub ssh_public_key: String,
    /// 对端 SSH 服务端主机公钥（首次连接时校验）
    pub host_key: String,
    /// SSH 登录用户名（应用专用账号名，不映射到系统账号）
    pub username: String,
    /// SSH 服务端端口
    pub port: u16,
    /// Tailscale 地址（IP 或 MagicDNS 名），可为空
    #[serde(default)]
    pub addrs: Vec<String>,
    /// 双方约定的中继服务器地址（`tcp://host:port`）
    #[serde(default)]
    pub relay: Option<String>,
    /// 中继房间 ID（配对成功后由发起方生成）
    #[serde(default)]
    pub relay_room: Option<String>,
    /// 中继房间口令
    #[serde(default)]
    pub relay_secret: Option<String>,
}

/// 配对请求（设备 B → A）：token + B 的 SSH 身份与地址。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequest {
    pub token: String,
    pub peer: PeerInfo,
}

/// 配对结果（A → B）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingResponse {
    pub accepted: bool,
    /// 接受时返回 A 的设备信息（含中继房间）；拒绝时为空。
    pub peer: PeerInfo,
}

/// 文件夹共享邀请（提示词 4.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderInvite {
    pub folder_id: String,
    pub folder_name: String,
    /// 邀请方设备名
    pub from_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderInviteResponse {
    pub folder_id: String,
    pub accepted: bool,
}

/// FolderSync 控制消息（SSH 子系统 `foldersync`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        protocol_version: u16,
        device_name: String,
        folders: Vec<FolderSummary>,
    },
    /// 全量索引交换（连接建立后）
    Index {
        folder_id: String,
        entries: Vec<FileEntry>,
    },
    /// 增量更新（实际文件内容走 SFTP，按 hash 拉取）
    IndexUpdate {
        folder_id: String,
        entries: Vec<FileEntry>,
    },
    PairingRequest(PairingRequest),
    PairingResponse(PairingResponse),
    FolderInvite(FolderInvite),
    FolderInviteResponse(FolderInviteResponse),
    Ping,
    Pong,
    Error {
        code: u32,
        message: String,
    },
}

/// 错误码常量（协议层）。
pub mod error_code {
    pub const PROTOCOL_MISMATCH: u32 = 1;
    pub const BAD_TOKEN: u32 = 2;
    pub const NOT_TRUSTED: u32 = 3;
    pub const FOLDER_UNKNOWN: u32 = 4;
    pub const INTERNAL: u32 = 5;
}

/// 编码为帧：4 字节大端长度 + postcard 体。
pub fn encode_frame(msg: &Message) -> anyhow::Result<Vec<u8>> {
    let body = postcard::to_stdvec(msg)?;
    let len = body.len() as u32;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// 从字节流中解码一帧（要求 `buf` 以帧头开始；粘包由调用方用 `read_exact` 规避）。
pub fn decode_frame(buf: &[u8]) -> anyhow::Result<Message> {
    anyhow::ensure!(buf.len() >= 4, "帧过短");
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    anyhow::ensure!(buf.len() >= 4 + len, "帧体不完整");
    let msg: Message = postcard::from_bytes(&buf[4..4 + len])?;
    Ok(msg)
}

/// 帧长度上限（16 MiB）：索引可能大，但要有界防恶意对端。
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use fs_sync_core::VersionVector;

    #[test]
    fn roundtrip_all_variants() {
        let mut vv = VersionVector::new();
        vv.increment("dev-a");
        let entries = vec![FileEntry {
            path: "a.txt".into(),
            size: 5,
            mtime: 100,
            hash: "ab".repeat(32),
            is_dir: false,
            deleted: false,
            version: vv,
            modified_by: "dev-a".into(),
        }];
        let peer = PeerInfo {
            device_id: "SHA256:abc".into(),
            device_name: "home".into(),
            ssh_public_key: "ssh-ed25519 AAAA test".into(),
            host_key: "ssh-ed25519 BBBB".into(),
            username: "foldersync".into(),
            port: 2222,
            addrs: vec!["100.64.0.1".into()],
            relay: Some("tcp://relay:9443".into()),
            relay_room: Some("room".into()),
            relay_secret: Some("secret".into()),
        };
        let msgs = vec![
            Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "home".into(),
                folders: vec![FolderSummary {
                    folder_id: "f1".into(),
                    name: "work".into(),
                }],
            },
            Message::Index {
                folder_id: "f1".into(),
                entries: entries.clone(),
            },
            Message::IndexUpdate {
                folder_id: "f1".into(),
                entries,
            },
            Message::PairingRequest(PairingRequest {
                token: "tok".into(),
                peer: peer.clone(),
            }),
            Message::PairingResponse(PairingResponse {
                accepted: true,
                peer,
            }),
            Message::FolderInvite(FolderInvite {
                folder_id: "f1".into(),
                folder_name: "work".into(),
                from_name: "a".into(),
            }),
            Message::FolderInviteResponse(FolderInviteResponse {
                folder_id: "f1".into(),
                accepted: false,
            }),
            Message::Ping,
            Message::Pong,
            Message::Error {
                code: error_code::BAD_TOKEN,
                message: "bad".into(),
            },
        ];
        for m in msgs {
            let frame = encode_frame(&m).unwrap();
            assert!(frame.len() <= MAX_FRAME);
            assert_eq!(decode_frame(&frame).unwrap(), m);
        }
    }

    #[test]
    fn decode_rejects_short() {
        assert!(decode_frame(&[0, 0]).is_err());
    }
}
