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
use iroh::{Endpoint, EndpointAddr, EndpointId};
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
