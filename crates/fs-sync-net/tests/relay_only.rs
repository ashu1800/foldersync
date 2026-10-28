//! 同一中继下的纯中继连通验证（不带直连地址，强制走中继）。
//!
//! 手动跑：
//! ```text
//! cargo test -p fs-sync-net --test relay_only -- --ignored --nocapture
//! ```

use std::time::Duration;

use fs_sync_net::{ALPN, Echo, NetConfig, bind_endpoint_with, mount};
use iroh::{Endpoint, SecretKey};

const CUSTOM: &str = "https://sync.ashu180.top:8444";

async fn bind(relay: Option<&str>) -> Endpoint {
    let cfg = NetConfig {
        relay_url: relay.map(|s| s.to_string()),
        listen_port: None,
    };
    bind_endpoint_with(&cfg, SecretKey::generate())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn same_relay_only_path_works() {
    let a = bind(Some(CUSTOM)).await;
    let _ra = mount(a.clone(), Echo);
    let b = bind(Some(CUSTOM)).await;
    a.online().await;
    b.online().await;

    // 只给中继，不给任何 IP 地址
    let relay: iroh::RelayUrl = CUSTOM.parse().unwrap();
    let addr = iroh::EndpointAddr::from(a.id()).with_relay_url(relay);

    let conn = tokio::time::timeout(Duration::from_secs(20), b.connect(addr, ALPN))
        .await
        .expect("20 秒超时：同一中继也连不上")
        .expect("连接失败");

    // 地址里只有中继、没有任何 IP，能连上就说明中继确实在转发
    println!("连接建立，远端 id = {}", conn.remote_id());

    b.close().await;
    a.close().await;
}
