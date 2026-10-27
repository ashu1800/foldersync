//! 连接管理：拨号、握手、连接类型判定（直连/中继/经 Tailscale）、延迟。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::frame;
use crate::message::{FolderSummary, Message, PROTOCOL_VERSION};
use crate::tailscale::is_tailscale_ip;

/// 连接类型（提示词 4.9 / 4.10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnType {
    /// IP 直连
    Direct,
    /// 中继
    Relay,
    /// 经 Tailscale 网络直连（不断言 Tailscale 内部是否走它自己的中继）
    Tailscale,
}

impl std::fmt::Display for ConnType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnType::Direct => write!(f, "直连"),
            ConnType::Relay => write!(f, "中继"),
            ConnType::Tailscale => write!(f, "经 Tailscale"),
        }
    }
}

/// 对端连接信息（供 UI 展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConnInfo {
    pub endpoint_id: String,
    /// 连接类型；建连瞬间可能尚无路径 → None
    pub conn_type: Option<ConnType>,
    /// 往返延迟（毫秒），未知为 None
    pub rtt_ms: Option<u64>,
}

/// 从 QUIC 传输地址判定连接类型。
fn classify_transport(addr: &TransportAddr) -> ConnType {
    match addr {
        TransportAddr::Ip(sa) => {
            if is_tailscale_ip(sa.ip()) {
                ConnType::Tailscale
            } else {
                ConnType::Direct
            }
        }
        TransportAddr::Relay(_) => ConnType::Relay,
        // 自定义传输（未来可能的扩展）按直连处理
        _ => ConnType::Direct,
    }
}

/// 从已建立连接判定类型：优先取当前选中路径，否则取任一已打开路径。
pub fn classify_conn(conn: &Connection) -> Option<ConnType> {
    let paths = conn.paths();
    let selected = paths
        .iter()
        .find(|p| p.is_selected())
        .or_else(|| paths.iter().next())?;
    Some(classify_transport(selected.remote_addr()))
}

/// 从对端地址列表预判拨号将用的类型（UI 展示用，实际以建连后为准）。
pub fn classify_addr(addr: &EndpointAddr) -> Option<ConnType> {
    // 有非 Tailscale 直连地址优先按直连；否则 Tailscale；否则中继
    let mut has_tailscale = false;
    let mut has_relay = false;
    for t in &addr.addrs {
        match t {
            TransportAddr::Ip(sa) => {
                if is_tailscale_ip(sa.ip()) {
                    has_tailscale = true;
                } else {
                    return Some(ConnType::Direct);
                }
            }
            TransportAddr::Relay(_) => has_relay = true,
            _ => {}
        }
    }
    if has_tailscale {
        Some(ConnType::Tailscale)
    } else if has_relay {
        Some(ConnType::Relay)
    } else {
        None
    }
}

/// Hello 握手结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloInfo {
    pub device_name: String,
    pub folders: Vec<FolderSummary>,
}

/// 双向 Hello 握手：发本端 Hello，收对端 Hello 并校验协议版本。
pub async fn handshake(conn: &Connection, my_name: &str) -> anyhow::Result<HelloInfo> {
    let (mut send, mut recv) = conn.open_bi().await?;
    frame::write_frame(
        &mut send,
        &Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: my_name.to_string(),
            folders: Vec::new(),
        },
    )
    .await?;
    let reply = frame::read_frame(&mut recv).await?;
    match reply {
        Message::Hello {
            protocol_version,
            device_name,
            folders,
        } => {
            anyhow::ensure!(
                protocol_version == PROTOCOL_VERSION,
                "协议版本不匹配: 对端 {protocol_version}, 本端 {PROTOCOL_VERSION}"
            );
            Ok(HelloInfo {
                device_name,
                folders,
            })
        }
        Message::Error { code, message } => {
            anyhow::bail!("对端拒绝 (code={code}): {message}")
        }
        other => anyhow::bail!("期望 Hello，收到 {other:?}"),
    }
}

/// 拨号并完成 Hello 握手。`extra_addrs` 为附加直连地址（如手工 Tailscale IP:端口）。
pub async fn dial_and_hello(
    endpoint: &Endpoint,
    target: EndpointAddr,
    my_name: &str,
    extra_addrs: Vec<SocketAddr>,
) -> anyhow::Result<(Connection, HelloInfo)> {
    let mut addr = target;
    for sa in extra_addrs {
        addr.addrs.insert(TransportAddr::Ip(sa));
    }
    let conn = endpoint.connect(addr, crate::ALPN).await?;
    let hello = handshake(&conn, my_name).await?;
    Ok((conn, hello))
}

/// 当前连接的 RTT（取选中路径）。
pub fn measure_rtt(conn: &Connection) -> Option<Duration> {
    let paths = conn.paths();
    paths
        .iter()
        .find(|p| p.is_selected())
        .or_else(|| paths.iter().next())
        .map(|p| p.rtt())
}

/// 同一设备的连接注册表：保证一对设备只维持一个同步连接（提示词 4.10.6）。
#[derive(Default)]
pub struct ConnRegistry {
    /// endpoint_id hex → 当前活跃连接
    conns: Mutex<HashMap<String, Arc<Connection>>>,
}

impl ConnRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记新连接；该设备已有活跃连接时返回 `Err(已有连接)`，由调用方择一保留（通常丢弃新连接）。
    pub async fn register(
        &self,
        id: &EndpointId,
        conn: Connection,
    ) -> Result<Arc<Connection>, Arc<Connection>> {
        let key = id.to_string();
        let mut map = self.conns.lock().await;
        if let Some(existing) = map.get(&key)
            && existing.close_reason().is_none()
        {
            return Err(existing.clone());
        }
        let arc = Arc::new(conn);
        map.insert(key, arc.clone());
        Ok(arc)
    }

    /// 注销连接（确认关闭后调用，避免误删新连接）。
    pub async fn unregister(&self, id: &EndpointId, conn: &Connection) {
        let key = id.to_string();
        let mut map = self.conns.lock().await;
        if let Some(cur) = map.get(&key)
            && cur.stable_id() == conn.stable_id()
        {
            map.remove(&key);
        }
    }

    /// 查询某设备当前活跃连接。
    pub async fn get(&self, id: &EndpointId) -> Option<Arc<Connection>> {
        let map = self.conns.lock().await;
        map.get(&id.to_string())
            .filter(|c| c.close_reason().is_none())
            .cloned()
    }

    /// 列出全部活跃连接（供状态上报）。
    pub async fn list(&self) -> Vec<PeerConnInfo> {
        let map = self.conns.lock().await;
        map.iter()
            .filter(|(_, c)| c.close_reason().is_none())
            .map(|(id, c)| PeerConnInfo {
                endpoint_id: id.clone(),
                conn_type: classify_conn(c),
                rtt_ms: measure_rtt(c).map(|d| d.as_millis() as u64),
            })
            .collect()
    }

    /// 清空（断开全部）。
    pub async fn clear(&self) {
        let mut map = self.conns.lock().await;
        for c in map.values() {
            c.close(0u32.into(), b"shutdown");
        }
        map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn classify_transport_variants() {
        let direct = TransportAddr::Ip("192.168.1.5:1234".parse().unwrap());
        assert_eq!(classify_transport(&direct), ConnType::Direct);

        let ts = TransportAddr::Ip("100.102.104.88:40000".parse().unwrap());
        assert_eq!(classify_transport(&ts), ConnType::Tailscale);

        let relay = TransportAddr::Relay("https://relay.example./".parse().unwrap());
        assert_eq!(classify_transport(&relay), ConnType::Relay);

        let _ = Ipv4Addr::new(0, 0, 0, 0);
    }

    #[test]
    fn classify_addr_prefers_direct_then_tailscale_then_relay() {
        let id: EndpointId = iroh::SecretKey::generate().public();

        // 仅有中继
        let mut a = EndpointAddr::from(id);
        a.addrs
            .insert(TransportAddr::Relay("https://r.example./".parse().unwrap()));
        assert_eq!(classify_addr(&a), Some(ConnType::Relay));

        // 加 Tailscale 地址后优先 Tailscale
        a.addrs
            .insert(TransportAddr::Ip("100.80.1.2:9000".parse().unwrap()));
        assert_eq!(classify_addr(&a), Some(ConnType::Tailscale));

        // 再加普通直连地址 → Direct 优先
        a.addrs
            .insert(TransportAddr::Ip("1.2.3.4:9000".parse().unwrap()));
        assert_eq!(classify_addr(&a), Some(ConnType::Direct));

        // 空地址
        let empty = EndpointAddr::from(id);
        assert_eq!(classify_addr(&empty), None);
    }

    #[test]
    fn conn_type_display_chinese() {
        assert_eq!(ConnType::Direct.to_string(), "直连");
        assert_eq!(ConnType::Relay.to_string(), "中继");
        assert_eq!(ConnType::Tailscale.to_string(), "经 Tailscale");
    }
}
