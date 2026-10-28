//! fs-sync-net：基于 iroh 的传输层。
//!
//! 模块：identity（设备密钥）、message（协议消息）、frame（帧编解码）、
//! pairing（配对码）、conn（连接管理与类型判定）、tailscale（探测）、echo（M0 验证）。

pub mod blobs;
pub mod conn;
pub mod frame;
pub mod identity;
pub mod message;
pub mod pairing;
pub mod tailscale;

use anyhow::{Context, Result};
use iroh::endpoint::{Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use std::sync::Arc;

/// FolderSync 自定义协议的 ALPN 标识（协议版本随消息携带）。
pub const ALPN: &[u8] = b"/foldersync/1";

/// M0 阶段的回显协议处理器：收到什么就回什么，用于验证 iroh 连通性。
#[derive(Debug, Clone)]
pub struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let n = tokio::io::copy(&mut recv, &mut send)
            .await
            .map_err(AcceptError::from_err)?;
        tracing::info!("回显完成，共 {n} 字节");
        send.finish()?;
        // 等待对端确认接收（finish 后 recv 端读到 EOF）再关闭，避免过早释放资源。
        let _ = connection.closed().await;
        Ok(())
    }
}

/// 绑定 iroh Endpoint（N0 preset：官方 DNS 寻址 + 官方中继）。
pub async fn bind_endpoint() -> Result<Endpoint> {
    Endpoint::bind(presets::N0)
        .await
        .context("iroh Endpoint 绑定失败")
}

/// 默认中继：自建（66.42.99.110 / sync.ashu180.top）。
///
/// 443 和 80 被同机的 nginx 占用，所以中继监听 8444，**RelayUrl 必须带端口**。
/// 想改回 iroh 官方中继就把 `NetConfig::relay_url` 设为 `None`。
pub const DEFAULT_RELAY_URL: &str = "https://sync.ashu180.top:8444";

/// 网络配置（提示词 4.x / 设置页）。
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// 自建中继 URL；None 用 iroh 官方中继。
    pub relay_url: Option<String>,
    /// 固定监听端口；None 随机端口。
    pub listen_port: Option<u16>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            relay_url: Some(DEFAULT_RELAY_URL.to_string()),
            listen_port: None,
        }
    }
}

/// 自建中继 + 官方中继的组合 RelayMap。
///
/// 自建排前面（首选），但**必须**把官方中继也留着。
///
/// 踩过的坑：早期只放自建的，结果和还在用官方中继的对端完全连不上。中继只转发到
/// "连在同一个中继上"的节点，两端各在一个中继上时没有任何共享通路，打洞再失败就
/// 直接超时（实测配对 10 秒 timeout，界面只显示"连接对码失败"）。
fn relay_map_with(custom: iroh::RelayUrl) -> iroh::RelayMap {
    // RelayMap 内部是 Arc，extend 接收 &self，不需要 mut
    let map = iroh::RelayMode::custom([custom]).relay_map();
    map.extend(&iroh::RelayMode::Default.relay_map());
    map
}

/// 按配置绑定 Endpoint。
///
/// 与 [`bind_endpoint`] 的区别是：可以指定自建中继、固定端口。
/// 配置项非法（比如中继 URL 写错）时**回退到默认配置**而不是启动失败——
/// 用户填错一个 URL 不应该让整个应用起不来。
pub async fn bind_endpoint_with(config: &NetConfig, secret: SecretKey) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::N0).secret_key(secret);

    if let Some(ref url) = config.relay_url {
        match url.parse::<iroh::RelayUrl>() {
            Ok(relay) => {
                builder = builder.relay_mode(iroh::RelayMode::Custom(relay_map_with(relay)));
                tracing::info!(%url, "使用自建中继（官方中继保留为兜底）");
            }
            Err(e) => {
                tracing::warn!(%url, "自建中继 URL 无效，回退到官方中继: {e}");
            }
        }
    }

    if let Some(port) = config.listen_port {
        let addr: std::net::SocketAddr = match format!("0.0.0.0:{port}").parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(port, "端口解析失败，使用随机端口: {e}");
                std::net::SocketAddr::from(([0, 0, 0, 0], 0))
            }
        };
        // bind_addr 消耗 builder 且失败时无法回退，所以先探测端口可用性：
        // 能绑上才指定，绑不上就沿用默认（随机端口）。
        match std::net::TcpListener::bind(addr) {
            Ok(probe) => {
                drop(probe); // 立刻释放，避免和 iroh 抢端口
                let bound = builder.bind_addr(addr);
                match bound {
                    Ok(b) => builder = b,
                    Err(e) => {
                        // 这里 builder 已被消耗，无法继续用；
                        // 直接换一个全新 builder 走默认配置。
                        tracing::warn!(port, "监听地址无效，改用默认配置: {e}");
                        let mut fresh = Endpoint::builder(presets::N0);
                        if let Some(ref url) = config.relay_url
                            && let Ok(relay) = url.parse::<iroh::RelayUrl>()
                        {
                            fresh =
                                fresh.relay_mode(iroh::RelayMode::Custom(relay_map_with(relay)));
                        }
                        return fresh.bind().await.context("iroh Endpoint 绑定失败");
                    }
                }
            }
            Err(e) => tracing::warn!(port, "端口不可用，使用随机端口: {e}"),
        }
    }

    builder.bind().await.context("iroh Endpoint 绑定失败")
}

/// 绑定 Endpoint 并挂载任意 ProtocolHandler。
pub fn mount<P: ProtocolHandler>(endpoint: Endpoint, handler: P) -> Router {
    Router::builder(endpoint).accept(ALPN, handler).spawn()
}

/// 绑定 Endpoint + 回显协议（M0 验证入口）。
pub async fn bind_echo_node() -> Result<(Endpoint, Router)> {
    let endpoint = bind_endpoint().await?;
    let router = mount(endpoint.clone(), Echo);
    Ok((endpoint, router))
}

/// 连接到远端节点并发送一条消息，返回回显内容（M0 验证用）。
pub async fn echo_roundtrip(addr: EndpointAddr, message: &[u8]) -> Result<Vec<u8>> {
    let endpoint = bind_endpoint().await?;
    let conn = endpoint.connect(addr, ALPN).await.context("连接远端失败")?;
    let (mut send, mut recv) = conn.open_bi().await.context("打开双向流失败")?;
    send.write_all(message).await.context("发送失败")?;
    send.finish()?;
    let echoed = recv.read_to_end(64 * 1024).await.context("读取回显失败")?;
    endpoint.close().await;
    Ok(echoed)
}

/// 便捷封装：从 EndpointId 构造 EndpointAddr（仅公钥、无直连地址，走发现/中继）。
pub fn addr_from_id(id: EndpointId) -> EndpointAddr {
    EndpointAddr::from(id)
}

/// 供后续里程碑使用的共享状态容器。
pub type Shared<T> = Arc<tokio::sync::RwLock<T>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有自建中继会和对端失去共享通路，必须保留官方中继兜底。
    #[test]
    fn relay_map_keeps_official_fallbacks() {
        let custom: iroh::RelayUrl = "https://sync.ashu180.top:8444".parse().unwrap();
        let map = relay_map_with(custom.clone());
        assert!(map.contains(&custom), "自建中继必须在 map 里");
        assert!(
            map.len() >= 2,
            "官方中继必须保留为兜底，实际只有 {} 个中继",
            map.len()
        );
    }
}
