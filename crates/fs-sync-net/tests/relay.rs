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
const DEFAULT_RELAY: &str = "https://sync.ashu180.top:8444";

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

    let mut watcher = endpoint.home_relay_status();
    let connected = watcher
        .get()
        .into_iter()
        .filter(|s| s.is_connected())
        .count();
    assert!(connected > 0, "中继状态里没有已连接项");
    println!("已连上 {connected} 个中继：{relay}");

    endpoint.close().await;
}
