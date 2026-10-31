//! 端到端：两台设备只通过中继服务器（无 Tailscale、无入站端口）同步文件。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use fs_sync_daemon::{FolderRuntime, PeerLoopConfig, Trust, run_peer_loop};
use fs_sync_net::blobstore::BlobStore;
use fs_sync_net::conn::NetworkMode;
use fs_sync_net::identity::{SshIdentity, load_or_create_host_key};
use fs_sync_net::message::PeerInfo;
use fs_sync_net::pairing::new_relay_room;
use fs_sync_net::relay_server::{RelayServerConfig, run_relay_server};
use fs_sync_store::Store;

struct Node {
    _dir: tempfile::TempDir,
    identity: SshIdentity,
    trust: Trust,
    runtime: Arc<FolderRuntime>,
}

async fn node() -> Node {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("share/blobs")).unwrap();
    let folder = data.join("docs");
    std::fs::create_dir_all(&folder).unwrap();

    let identity = SshIdentity::load_or_create(&data).unwrap();
    let _host_key = load_or_create_host_key(&data).unwrap();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let trust = Trust::new(store.clone());
    let runtime = Arc::new(
        FolderRuntime::start(
            "f1".into(),
            folder,
            identity.id(),
            store.clone(),
            Arc::new(BlobStore::open(data.join("share")).unwrap()),
            100,
        )
        .await
        .unwrap(),
    );
    Node {
        _dir: dir,
        identity,
        trust,
        runtime,
    }
}

fn peer(node: &Node, relay: &str, room: &str, secret: &str) -> PeerInfo {
    PeerInfo {
        device_id: node.identity.id(),
        device_name: "node".into(),
        ssh_public_key: node.identity.public_key_line().unwrap(),
        host_key: "ssh-ed25519 PLACEHOLDER".into(), // 中继模式不会用 SSH 主机密钥
        username: fs_sync_net::SSH_USER.into(),
        port: 2222,
        addrs: Vec::new(),
        relay: Some(relay.to_string()),
        relay_room: Some(room.to_string()),
        relay_secret: Some(secret.to_string()),
    }
}

#[tokio::test]
async fn two_nodes_sync_through_relay() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("fs_sync_daemon=debug,fs_sync_net=debug")
        .try_init();
    let relay_dir = tempfile::tempdir().unwrap();
    let relay = run_relay_server(RelayServerConfig {
        data_dir: relay_dir.path().join("relay"),
        port: 0,
        room_retention_days: 30,
    })
    .await
    .unwrap();
    let url = format!("tcp://127.0.0.1:{}", relay.port);
    let (room, secret) = new_relay_room();

    let a = node().await;
    let b = node().await;
    std::fs::write(a.runtime.root_path().join("from-a.txt"), b"alpha").unwrap();
    std::fs::write(b.runtime.root_path().join("from-b.txt"), b"beta").unwrap();
    for node in [&a, &b] {
        node.runtime.session.full_scan().await.unwrap();
        for e in node.runtime.session.snapshot().await.values() {
            if !e.is_dir {
                node.runtime.session.engine.ensure_blob(e).await.unwrap();
            }
        }
    }

    a.trust.add(&peer(&b, &url, &room, &secret)).unwrap();
    b.trust.add(&peer(&a, &url, &room, &secret)).unwrap();

    let cfg = |node: &Node| {
        Arc::new(RwLock::new(PeerLoopConfig {
            identity: node.identity.clone(),
            device_id: node.identity.id(),
            device_name: "node".into(),
            ssh_port: 2222,
            mode: NetworkMode::Relay,
            relay_url: Some(url.clone()),
            store: node.runtime.session.engine.store.clone(),
            blobs: node.runtime.session.blobs.clone(),
            sync_root: node._dir.path().join("FolderSync"),
            paused_all: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }))
    };
    let cfg_a = cfg(&a);
    let cfg_b = cfg(&b);
    let trust_a = a.trust.clone();
    let trust_b = b.trust.clone();
    let rt_a = Arc::new(std::sync::Mutex::new(vec![a.runtime.clone()]));
    let rt_b = Arc::new(std::sync::Mutex::new(vec![b.runtime.clone()]));
    tokio::spawn(async move { run_peer_loop(cfg_a, rt_a, trust_a, None).await });
    tokio::spawn(async move { run_peer_loop(cfg_b, rt_b, trust_b, None).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let a_has_b = a.runtime.root_path().join("from-b.txt").exists();
        let b_has_a = b.runtime.root_path().join("from-a.txt").exists();
        if a_has_b && b_has_a {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "30 秒内未完成中继双向同步: a_has_b={a_has_b}, b_has_a={b_has_a}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(
        std::fs::read(a.runtime.root_path().join("from-b.txt")).unwrap(),
        b"beta"
    );
}
