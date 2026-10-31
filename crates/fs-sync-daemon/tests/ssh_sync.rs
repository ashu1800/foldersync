//! 端到端：两台设备通过真实 SSH/SFTP 互相连接并同步文件。
//!
//! 覆盖：SSH 公钥认证、控制子系统、SFTP blob 拉取、双向索引交换。
//! 不依赖 Tailscale 网络本身（用 127.0.0.1 模拟直连地址）。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use fs_sync_daemon::{
    FolderRuntime, PairingManager, PeerLoopConfig, SshServerShared, Trust, run_peer_loop,
    run_ssh_server,
};
use fs_sync_net::blobstore::BlobStore;
use fs_sync_net::conn::NetworkMode;
use fs_sync_net::identity::{SshIdentity, load_or_create_host_key, public_key_line};
use fs_sync_net::message::PeerInfo;
use fs_sync_store::Store;

struct Node {
    _dir: tempfile::TempDir,
    identity: SshIdentity,
    host_key: russh::keys::PrivateKey,
    trust: Trust,
    runtime: Arc<FolderRuntime>,
    shared: Arc<SshServerShared>,
}

async fn node() -> Node {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("share/blobs")).unwrap();
    let folder = data.join("docs");
    std::fs::create_dir_all(&folder).unwrap();

    let identity = SshIdentity::load_or_create(&data).unwrap();
    let host_key = load_or_create_host_key(&data).unwrap();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let trust = Trust::new(store.clone());
    let blobs = Arc::new(BlobStore::open(data.join("share")).unwrap());
    let runtime = Arc::new(
        FolderRuntime::start(
            "f1".into(),
            folder,
            identity.id(),
            store.clone(),
            blobs.clone(),
            100,
        )
        .await
        .unwrap(),
    );
    let pairing = PairingManager::new(
        trust.clone(),
        identity.clone(),
        public_key_line(host_key.public_key()).unwrap(),
        2222,
        "node".into(),
        None,
        store.clone(),
    );
    let sync_root = data.join("FolderSync");
    let shared = Arc::new(SshServerShared {
        identity: identity.clone(),
        host_key: host_key.clone(),
        device_id: identity.id(),
        device_name: "node".into(),
        ssh_port: 0,
        data_dir: data,
        sync_root,
        trust: trust.clone(),
        pairing,
        store: store.clone(),
        blobs,
        invite_lock: tokio::sync::Mutex::new(()),
        removing_folders: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        paused_all: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        runtimes: Arc::new(std::sync::Mutex::new(vec![runtime.clone()])),
        online: Arc::new(tokio::sync::Mutex::new(Default::default())),
    });
    Node {
        _dir: dir,
        identity,
        host_key,
        trust,
        runtime,
        shared,
    }
}

fn peer_info(node: &Node, port: u16) -> PeerInfo {
    PeerInfo {
        device_id: node.identity.id(),
        device_name: "node".into(),
        ssh_public_key: node.identity.public_key_line().unwrap(),
        host_key: public_key_line(node.host_key.public_key()).unwrap(),
        username: fs_sync_net::SSH_USER.into(),
        port,
        addrs: vec!["127.0.0.1".into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn two_nodes_sync_files_over_real_ssh() {
    let a = node().await;
    let b = node().await;

    std::fs::write(a.runtime.root_path().join("from-a.txt"), b"alpha").unwrap();
    std::fs::write(b.runtime.root_path().join("from-b.txt"), b"beta").unwrap();
    // 重新扫描，把测试文件纳入索引与 blob
    let _ = a.runtime.session.full_scan().await.unwrap();
    let _ = b.runtime.session.full_scan().await.unwrap();
    for e in a.runtime.session.snapshot().await.values() {
        if !e.is_dir {
            a.runtime.session.engine.ensure_blob(e).await.unwrap();
        }
    }
    for e in b.runtime.session.snapshot().await.values() {
        if !e.is_dir {
            b.runtime.session.engine.ensure_blob(e).await.unwrap();
        }
    }

    let server_a = run_ssh_server(a.shared.clone()).await.unwrap();
    let server_b = run_ssh_server(b.shared.clone()).await.unwrap();

    let pa = peer_info(&a, server_a.port);
    let pb = peer_info(&b, server_b.port);
    a.trust.add(&pb).unwrap();
    b.trust.add(&pa).unwrap();

    let cfg_a = Arc::new(RwLock::new(PeerLoopConfig {
        identity: a.identity.clone(),
        device_id: a.identity.id(),
        device_name: "node-a".into(),
        ssh_port: server_a.port,
        mode: NetworkMode::Tailscale,
        relay_url: None,
        store: a.shared.store.clone(),
        blobs: a.shared.blobs.clone(),
        sync_root: a.shared.sync_root.clone(),
        paused_all: a.shared.paused_all.clone(),
    }));
    let cfg_b = Arc::new(RwLock::new(PeerLoopConfig {
        identity: b.identity.clone(),
        device_id: b.identity.id(),
        device_name: "node-b".into(),
        ssh_port: server_b.port,
        mode: NetworkMode::Tailscale,
        relay_url: None,
        store: b.shared.store.clone(),
        blobs: b.shared.blobs.clone(),
        sync_root: b.shared.sync_root.clone(),
        paused_all: b.shared.paused_all.clone(),
    }));

    let rt_a = a.shared.runtimes.clone();
    let rt_b = b.shared.runtimes.clone();
    let trust_a = a.trust.clone();
    let trust_b = b.trust.clone();
    tokio::spawn(async move { run_peer_loop(cfg_a, rt_a, trust_a, None).await });
    tokio::spawn(async move { run_peer_loop(cfg_b, rt_b, trust_b, None).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let a_has_b = a.runtime.root_path().join("from-b.txt").exists();
        let b_has_a = b.runtime.root_path().join("from-a.txt").exists();
        if a_has_b && b_has_a {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "20 秒内未完成双向同步: a_has_b={a_has_b}, b_has_a={b_has_a}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert_eq!(
        std::fs::read(a.runtime.root_path().join("from-b.txt")).unwrap(),
        b"beta"
    );
    assert_eq!(
        std::fs::read(b.runtime.root_path().join("from-a.txt")).unwrap(),
        b"alpha"
    );
    drop(a);
    drop(b);
}
