//! 配对码纯中继连通验证（配对码里带中继，不依赖 DNS 发现）。
//!
//! 手动跑：
//! ```text
//! cargo test -p fs-sync-net --test relay_only -- --ignored --nocapture
//! ```

use std::time::Duration;

use fs_sync_net::pairing;
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

/// 真实配对链路：A 生成带中继的配对码 → B 解码 → B 用码里的地址连 A。
/// 这条链路不查 DNS，只靠配对码里的中继，正是两台机器跨省配对用的路径。
#[tokio::test]
#[ignore]
async fn pairing_code_alone_is_enough_to_connect() {
    let a = bind(Some(CUSTOM)).await;
    let _ra = mount(a.clone(), Echo);
    let b = bind(Some(CUSTOM)).await;
    a.online().await;
    b.online().await;

    let code = pairing::generate(&a.id().to_string(), pairing::now_secs(), Some(CUSTOM));
    let text = pairing::encode(&code);

    let decoded = pairing::decode(&text, pairing::now_secs()).expect("配对码解析失败");
    let addr = pairing::to_endpoint_addr(&decoded).expect("生成拨号地址失败");

    let conn = tokio::time::timeout(Duration::from_secs(20), b.connect(addr, ALPN))
        .await
        .expect("20 秒超时：配对码里的中继没能建立连接")
        .expect("连接失败");
    println!("配对码直连成功，远端 id = {}", conn.remote_id());

    b.close().await;
    a.close().await;
}
