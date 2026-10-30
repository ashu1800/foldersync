//! 自建中继连通性检查。
//!
//! 需要外网、且中继已部署，所以默认 `#[ignore]`，不会拖慢正常测试。
//!
//! 跑法：
//! ```text
//! cargo test -p fs-sync-net --test relay -- --ignored --nocapture
//! ```
//!
//! 换中继时改 `RELAY` 或设环境变量 `FOLDERSYNC_RELAY_URL`。

use std::time::Duration;

use fs_sync_net::{NetConfig, bind_endpoint_with};
use iroh::{SecretKey, Watcher};

/// 默认中继地址。端口必须写：443/80 被 nginx 占用，中继在 8444。
const DEFAULT_RELAY: &str = "http://36.137.140.126:8444";

#[tokio::test]
#[ignore = "需要外网与已部署的中继，手动跑：cargo test -p fs-sync-net --test relay -- --ignored"]
async fn relay_accepts_connection() {
    let relay = std::env::var("FOLDERSYNC_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY.to_string());

    let cfg = NetConfig {
        relay_url: Some(relay.clone()),
        listen_port: None,
    };
    let endpoint = bind_endpoint_with(&cfg, SecretKey::generate())
        .await
        .expect("绑定 Endpoint 失败");

    // online() 会一直等到至少连上一个中继；超时说明中继不可用或端口被墙
    let ok = tokio::time::timeout(Duration::from_secs(20), endpoint.online())
        .await
        .is_ok();
    assert!(ok, "20 秒内没能连上中继 {relay}");

    // 自建中继必须真的连上（否则等于没用上自己的服务器）。
    // 多个中继是并发连接的，online() 只保证"至少一个"通了，所以这里再等一会儿。
    let custom: iroh::RelayUrl = relay.parse().expect("中继 URL 非法");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let statuses = loop {
        let mut watcher = endpoint.home_relay_status();
        let statuses = watcher.get();
        if statuses
            .iter()
            .any(|s| s.is_connected() && *s.url() == custom)
        {
            break statuses;
        }
        if std::time::Instant::now() > deadline {
            panic!("自建中继 {relay} 15 秒内没连上，实际状态: {statuses:?}");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };

    let connected = statuses.iter().filter(|s| s.is_connected()).count();
    println!(
        "中继 map 共 {} 个，已连上 {connected} 个，其中自建 {relay} 已连上",
        statuses.len()
    );

    endpoint.close().await;
}
