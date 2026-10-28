//! 跨机中继探针：验证两台机器能否真的通过自建中继互通。
//!
//! ```text
//! relay_probe listen              # 打印 ID，等 3 分钟
//! relay_probe connect <ENDPOINT_ID>
//! ```

use std::time::Duration;

use fs_sync_net::{ALPN, Echo, NetConfig, bind_endpoint_with, mount};
use iroh::{EndpointAddr, EndpointId, SecretKey};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let ep = bind_endpoint_with(&NetConfig::default(), SecretKey::generate()).await?;

    match args.get(1).map(String::as_str) {
        Some("listen") => {
            let _r = mount(ep.clone(), Echo);
            println!("ID {}", ep.id());
            for s in ep.bound_sockets() {
                println!("SOCK {}", s);
            }
            ep.online().await;
            println!("ONLINE");
            tokio::time::sleep(Duration::from_secs(180)).await;
        }
        Some("connect") => {
            let id: EndpointId = args[2].parse()?;
            ep.online().await;
            let mut addr = EndpointAddr::from(id);
            if let Some(direct) = args.get(3) {
                let sa: std::net::SocketAddr = direct.parse()?;
                addr = addr.with_ip_addr(sa);
                println!("只用直连地址 {sa}");
            } else {
                let relay = fs_sync_net::DEFAULT_RELAY_URL.parse()?;
                addr = addr.with_relay_url(relay);
                println!("只用中继");
            }
            let conn = tokio::time::timeout(Duration::from_secs(25), ep.connect(addr, ALPN))
                .await
                .map_err(|_| anyhow::anyhow!("连接超时（25s）"))??;
            let (mut s, mut r) = conn.open_bi().await?;
            s.write_all(b"ping").await?;
            s.finish()?;
            let out = r.read_to_end(1024).await?;
            println!("REPLY {}", String::from_utf8_lossy(&out));
        }
        _ => println!("usage: relay_probe listen | connect <ID>"),
    }
    Ok(())
}
