//! 消息定义与编解码：长度前缀帧 + postcard。
//!
//! 帧格式：`u32` 大端长度 + postcard 编码的 `Message`。
//! 所有消息携带 `protocol_version`，不匹配即报错断连。

use serde::{Deserialize, Serialize};

use fs_sync_core::FileEntry;

/// 当前协议版本。
pub const PROTOCOL_VERSION: u16 = 1;

/// 文件夹摘要（Hello 中交换）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSummary {
    pub folder_id: String,
    pub name: String,
}

/// 配对请求（设备 B → A）：token + 自己的名字。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequest {
    pub token: String,
    pub device_name: String,
}

/// 配对结果（A → B）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingResponse {
    pub accepted: bool,
    /// 接受时返回 A 的设备名，供 B 显示
    pub device_name: String,
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

/// FolderSync 控制消息（ALPN `/foldersync/1`）。
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
    /// 增量更新
    IndexUpdate {
        folder_id: String,
        entries: Vec<FileEntry>,
    },
    /// 请求文件内容（实际数据走 iroh-blobs，按 hash）
    Request {
        folder_id: String,
        path: String,
        hash: String,
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
            Message::Request {
                folder_id: "f1".into(),
                path: "a.txt".into(),
                hash: "ab".repeat(32),
            },
            Message::PairingRequest(PairingRequest {
                token: "tok".into(),
                device_name: "b".into(),
            }),
            Message::PairingResponse(PairingResponse {
                accepted: true,
                device_name: "a".into(),
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
