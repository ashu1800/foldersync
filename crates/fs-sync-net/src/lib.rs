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

/// 默认中继：自建（36.137.140.126，国内服务器，纯 IP 直连）。
///
/// 纯 HTTP：没有域名就没有正规证书，iroh 载荷本身端到端加密，
/// 中继只转发密文，链路层明文不降低安全性。iroh 客户端对 http:// 的
/// RelayUrl 自动走 ws://。2026-10-30 从 sync.ashu180.top:8444（新加坡，
/// 跨境链路被干扰导致配对超时）迁来。
pub const DEFAULT_RELAY_URL: &str = "http://36.137.140.126:8444";

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

/// 中继表：**只放自建中继**。
///
/// 为什么不把官方中继一起放进来做兜底：iroh 会按延迟在多个 relay 之间自动切换
/// home relay。混着放之后，两端会各自在不同时刻跳到不同的中继上，而中继只在
/// 「连在同一个中继上」的节点之间转发 —— 于是配对时好时坏，诊断日志里能看到
/// 每秒都在 sync.ashu180.top / usw1 / aps1 / euc1 之间来回跳。
///
/// 只放一个中继，选路就没有了歧义，两端必然在同一个中继上。
///
/// 代价：自建中继挂了就没有中继可用（直连/Tailscale 仍然能用）。
/// 要换回官方中继，把 `relay_url` 设成 `None` 即可（那条路径走 presets::N0）。
fn relay_map_with(custom: iroh::RelayUrl) -> iroh::RelayMap {
    iroh::RelayMode::custom([custom]).relay_map()
}

/// 自建中继模式下用 `presets::Minimal`，**不装 N0 的 DNS/pkarr 地址发现**。
///
/// 为什么：`presets::N0` 会挂上 `PkarrPublisher` / `PkarrResolver` /
/// `DnsAddressLookup`，这些都要去问境外的 `dns.iroh.link`。国内网络下这些查询
/// 要么被墙要么极慢，而 `endpoint.connect(addr)` 会带着它们一起跑，
/// 结果就是明明配了直连地址，却卡到 30 秒超时（实测两台机器都这样）。
///
/// 我们并不需要地址发现：配对码里已经带上了中继 URL 和本机 Tailscale 直连地址，
/// 拨号方拿到的就是完整地址。去掉它反而更快、更可控。
fn builder_with_relay(secret: SecretKey, relay: iroh::RelayUrl) -> iroh::endpoint::Builder {
    tracing::info!(url = %relay, "使用自建中继（关闭 DNS 发现，只连这一个中继）");
    Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .relay_mode(iroh::RelayMode::Custom(relay_map_with(relay)))
}

/// 按配置绑定 Endpoint。
///
/// 与 [`bind_endpoint`] 的区别是：可以指定自建中继、固定端口。
/// 配置项非法（比如中继 URL 写错）时**回退到默认配置**而不是启动失败——
/// 用户填错一个 URL 不应该让整个应用起不来。
pub async fn bind_endpoint_with(config: &NetConfig, secret: SecretKey) -> Result<Endpoint> {
    // 先解析中继：解析成功走 Minimal（无 DNS 发现），失败/没配才回退 N0。
    let custom_relay =
        config
            .relay_url
            .as_deref()
            .and_then(|url| match url.parse::<iroh::RelayUrl>() {
                Ok(relay) => Some(relay),
                Err(e) => {
                    tracing::warn!(url, "自建中继 URL 无效，回退到官方中继: {e}");
                    None
                }
            });

    let mut builder = match custom_relay.clone() {
        Some(relay) => builder_with_relay(secret.clone(), relay),
        None => Endpoint::builder(presets::N0).secret_key(secret.clone()),
    };

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
                        let fresh = match custom_relay {
                            Some(relay) => builder_with_relay(secret.clone(), relay),
                            None => Endpoint::builder(presets::N0).secret_key(secret.clone()),
                        };
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

    /// 只放自建中继。混入官方中继会让 iroh 在多个 relay 间跳来跳去，
    /// 两端落到不同中继上就配不上对（诊断日志里抓到的抖动）。
    #[test]
    fn relay_map_has_exactly_the_custom_relay() {
        let custom: iroh::RelayUrl = crate::DEFAULT_RELAY_URL.parse().unwrap();
        let map = relay_map_with(custom.clone());
        assert!(map.contains(&custom), "自建中继必须在 map 里");
        assert_eq!(map.len(), 1, "relay map 只能有自建中继，否则会来回跳");
    }
}
