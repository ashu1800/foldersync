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

/// 设备在线状态：按连接计数。
///
/// 同一对端可能同时有好几条连接（本机连出的同步会话、对端连入的控制通道、
/// 拉文件时临时开的连接）。以前用 `HashMap<id, ConnType>`，任何一条断开都会
/// `remove`，短连接一关就把仍在线的设备显示成"未连接"。
#[derive(Debug, Default)]
pub struct Presence {
    conns: std::collections::HashMap<String, Vec<ConnType>>,
    /// 最近一次成功连接测得的往返延迟（毫秒）
    rtt_ms: std::collections::HashMap<String, u64>,
}

impl Presence {
    /// 登记一条连接。
    pub fn enter(&mut self, id: &str, ty: ConnType) {
        self.conns.entry(id.to_string()).or_default().push(ty);
    }

    /// 注销一条同类型的连接；最后一条注销后设备离线。
    pub fn leave(&mut self, id: &str, ty: ConnType) {
        if let Some(v) = self.conns.get_mut(id) {
            if let Some(i) = v.iter().position(|t| *t == ty) {
                v.swap_remove(i);
            }
            if v.is_empty() {
                self.conns.remove(id);
            }
        }
    }

    /// 当前连接方式；同时有直连和中继时报直连。
    pub fn get(&self, id: &str) -> Option<ConnType> {
        let v = self.conns.get(id)?;
        v.iter()
            .copied()
            .find(|t| *t == ConnType::Tailscale)
            .or_else(|| v.first().copied())
    }

    /// 记录最近一次连接的 RTT。
    pub fn set_rtt(&mut self, id: &str, rtt_ms: u64) {
        self.rtt_ms.insert(id.to_string(), rtt_ms);
    }

    /// 读取最近一次连接的 RTT（离线后保留，便于界面显示历史延迟）。
    pub fn rtt(&self, id: &str) -> Option<u64> {
        self.rtt_ms.get(id).copied()
    }
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
    fn presence_counts_connections() {
        let mut p = Presence::default();
        p.enter("a", ConnType::Tailscale); // 长连接
        p.enter("a", ConnType::Tailscale); // 拉文件的短连接
        p.leave("a", ConnType::Tailscale); // 短连接关闭
        assert_eq!(
            p.get("a"),
            Some(ConnType::Tailscale),
            "长连接还在，仍应在线"
        );
        p.enter("a", ConnType::Relay);
        assert_eq!(p.get("a"), Some(ConnType::Tailscale), "直连优先");
        p.leave("a", ConnType::Tailscale);
        assert_eq!(p.get("a"), Some(ConnType::Relay));
        p.leave("a", ConnType::Relay);
        assert_eq!(p.get("a"), None);
        p.leave("a", ConnType::Relay); // 多余的 leave 不出错
        assert_eq!(p.get("b"), None);
        p.set_rtt("a", 23);
        assert_eq!(p.rtt("a"), Some(23));
        assert_eq!(p.rtt("b"), None);
    }

    #[test]
    fn display_chinese() {
        assert_eq!(ConnType::Tailscale.to_string(), "经 Tailscale");
        assert_eq!(ConnType::Relay.to_string(), "中继");
    }
}
