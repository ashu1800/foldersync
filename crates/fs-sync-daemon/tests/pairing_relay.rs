//! 端到端：通过中继房间完成 SSH 公钥交换配对（无 Tailscale）。

use std::sync::Arc;
use std::time::Duration;

use fs_sync_daemon::{PairingManager, Trust};
use fs_sync_net::identity::{SshIdentity, load_or_create_host_key, public_key_line};
use fs_sync_net::relay_server::{RelayServerConfig, run_relay_server};
use fs_sync_store::Store;

fn manager(dir: &std::path::Path, relay: &str) -> PairingManager {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let trust = Trust::new(store.clone());
    let identity = SshIdentity::load_or_create(dir).unwrap();
    let host = load_or_create_host_key(dir).unwrap();
    PairingManager::new(
        trust,
        identity,
        public_key_line(host.public_key()).unwrap(),
        2222,
        "node".into(),
        Some(relay.to_string()),
        store,
    )
}

#[tokio::test]
async fn relay_pairing_exchanges_ssh_keys() {
    let relay_dir = tempfile::tempdir().unwrap();
    let relay = run_relay_server(RelayServerConfig {
        data_dir: relay_dir.path().join("relay"),
        port: 0,
        room_retention_days: 30,
    })
    .await
    .unwrap();
    let url = format!("tcp://127.0.0.1:{}", relay.port);

    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let a = manager(a_dir.path(), &url);
    let b = manager(b_dir.path(), &url);

    let code = a.create_code(Vec::new(), Some(&url)).await;
    a.spawn_relay_pairing_watcher(code.clone(), url.clone());

    // A 侧模拟用户点“接受”
    let pm = a.clone();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if let Some(p) = pm.list_pending().await.into_iter().next() {
                pm.confirm(&p.request_id, true).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let request = fs_sync_net::message::PairingRequest {
        token: code.token.clone(),
        peer: b.own_info(Vec::new()).unwrap(),
    };
    let response = fs_sync_net::relay::pair_via_relay(&url, &code.token, request)
        .await
        .unwrap();
    assert!(response.accepted, "A 应接受配对");
    assert!(
        response.peer.relay_room.is_some(),
        "响应应带中继房间，供之后的同步使用"
    );
    b.trust().add(&response.peer).unwrap();

    let devices_a = a.trust().devices().unwrap();
    let devices_b = b.trust().devices().unwrap();
    assert_eq!(devices_a.len(), 1);
    assert_eq!(devices_b.len(), 1);
    assert!(devices_a[0].relay_room.is_some());
    assert!(devices_b[0].relay_room.is_some());
}
