//! 连接类型：Tailscale 直连（SFTP）或中继服务器。

use serde::{Deserialize, Serialize};

/// 网络方案选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// 有 Tailscale 就走 SFTP 直连，否则走中继
    #[default]
    Auto,
    /// 仅 Tailscale（SSH/SFTP 直连）
    Tailscale,
    /// 仅中继服务器
    Relay,
}

/// 传输方案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnType {
    /// 经 Tailscale 的 SSH/SFTP 直连
    Tailscale,
    /// 经中继服务器（SSH/SFTP）
    Relay,
}

impl std::fmt::Display for ConnType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnType::Tailscale => write!(f, "经 Tailscale"),
            ConnType::Relay => write!(f, "中继"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_chinese() {
        assert_eq!(ConnType::Tailscale.to_string(), "经 Tailscale");
        assert_eq!(ConnType::Relay.to_string(), "中继");
    }
}
