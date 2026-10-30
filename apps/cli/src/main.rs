//! FolderSync CLI：SSH/SFTP + Tailscale / 中继方案。
//!
//! 用法：
//!   foldersync serve [--auto-confirm] [--folder PATH]...   # 启动 SSH 服务端 + 同步
//!   foldersync pair                                         # 生成配对码并等待配对
//!   foldersync join <PAIRING_CODE>                          # 输入配对码完成配对
//!   foldersync status                                       # 身份、Tailscale、中继、信任列表
//!   foldersync sync <LOCAL_PATH> <DEVICE_ID>                 # 单向推送一次（调试）
//!   foldersync relay ...                                     # 见 foldersync-relay

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fs_sync_daemon::{
    FolderRuntime, PairingManager, PeerLoopConfig, SshServerShared, Trust, run_peer_loop,
    run_ssh_server,
};
use fs_sync_net::blobstore::BlobStore;
use fs_sync_net::identity::{
    SshIdentity, fingerprint_text, load_or_create_host_key, public_key_line,
};
use fs_sync_net::message::PairingRequest;
use fs_sync_net::{conn::NetworkMode, pairing as pc, tailscale};
use fs_sync_store::Store;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "foldersync", version, about = "FolderSync SSH/SFTP 同步命令行")]
struct Cli {
    /// 数据目录（默认 ~/.foldersync）
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// 中继服务器地址（tcp://IP:9443），也可用环境变量 FOLDERSYNC_RELAY
    #[arg(long, global = true)]
    relay: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// 启动节点：SSH 服务端 + 配对 + 已信任设备同步
    Serve {
        /// 自动确认配对请求（无 UI 的调试场景）
        #[arg(long)]
        auto_confirm: bool,
        /// 同步文件夹路径（可多次指定）
        #[arg(long = "folder", short = 'f')]
        folders: Vec<String>,
    },
    /// 生成配对码并等待对端配对
    Pair,
    /// 用配对码完成配对
    Join { code: String },
    /// 显示本机身份、Tailscale 状态与信任列表
    Status,
    /// 单向推送一个文件夹到指定设备（调试）
    Sync { path: String, peer: String },
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".foldersync")
}

fn relay_of(cli: &Cli) -> Option<String> {
    cli.relay
        .clone()
        .or_else(|| std::env::var("FOLDERSYNC_RELAY").ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(fs_sync_net::DEFAULT_RELAY_URL.to_string()))
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "foldersync-cli".into())
}

struct Context2 {
    data_dir: PathBuf,
    identity: SshIdentity,
    host_key: russh::keys::PrivateKey,
    store: Arc<Store>,
    trust: Trust,
    pairing: PairingManager,
    blobs: Arc<BlobStore>,
    relay: Option<String>,
}

async fn build_context(cli: &Cli) -> Result<Context2> {
    let data_dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let settings_relay = relay_of(cli);
    let env = fs_sync_net::env::ensure_ssh_env(&data_dir, fs_sync_net::env::default_ssh_port());
    let identity = SshIdentity::load_or_create(&data_dir)?;
    let host_key = load_or_create_host_key(&data_dir)?;
    let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
    let trust = Trust::new(store.clone());
    let pairing = PairingManager::new(
        trust.clone(),
        identity.clone(),
        public_key_line(host_key.public_key())?,
        env.ssh_port,
        hostname(),
        settings_relay.clone(),
        store.clone(),
    );
    let blobs = Arc::new(BlobStore::open(data_dir.join("share"))?);
    Ok(Context2 {
        data_dir,
        identity,
        host_key,
        store,
        trust,
        pairing,
        blobs,
        relay: settings_relay,
    })
}

fn local_addrs() -> Vec<String> {
    tailscale::detect().ips
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd.clone() {
        Cmd::Serve {
            auto_confirm,
            folders,
        } => serve(&cli, auto_confirm, folders).await,
        Cmd::Pair => pair(&cli).await,
        Cmd::Join { code } => join(&cli, code).await,
        Cmd::Status => status(&cli).await,
        Cmd::Sync { path, peer } => sync_once(&cli, path, peer).await,
    }
}

async fn serve(cli: &Cli, auto_confirm: bool, folders: Vec<String>) -> Result<()> {
    let ctx = build_context(cli).await?;
    let runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    for folder in folders {
        let root = PathBuf::from(&folder);
        if !root.is_dir() {
            anyhow::bail!("不是文件夹: {folder}");
        }
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".into());
        let folder_id = format!("folder-{}", &blake3::hash(name.as_bytes()).to_hex()[..16]);
        if !ctx.store.list_folders()?.iter().any(|f| f.id == folder_id) {
            ctx.store.add_folder(&fs_sync_store::Folder {
                id: folder_id.clone(),
                name,
                local_path: root.clone(),
                paused: false,
                tombstone_days: 30,
                created_at: pc::now_secs() as i64,
            })?;
        }
        let rt = FolderRuntime::start(
            folder_id,
            root,
            ctx.identity.id(),
            ctx.store.clone(),
            ctx.blobs.clone(),
            400,
        )
        .await?;
        runtimes.lock().unwrap().push(Arc::new(rt));
    }

    let shared = Arc::new(SshServerShared {
        identity: ctx.identity.clone(),
        host_key: ctx.host_key.clone(),
        device_id: ctx.identity.id(),
        device_name: hostname(),
        ssh_port: fs_sync_net::env::default_ssh_port(),
        data_dir: ctx.data_dir.clone(),
        trust: ctx.trust.clone(),
        pairing: ctx.pairing.clone(),
        runtimes: runtimes.clone(),
        online: Arc::new(tokio::sync::Mutex::new(Default::default())),
    });
    let server = run_ssh_server(shared).await?;
    let actual_port = server.port;
    ctx.pairing.set_ssh_port(actual_port);

    let peer_cfg = Arc::new(RwLock::new(PeerLoopConfig {
        identity: ctx.identity.clone(),
        device_id: ctx.identity.id(),
        device_name: hostname(),
        ssh_port: actual_port,
        mode: NetworkMode::Auto,
        relay_url: ctx.relay.clone(),
        folder_ids: runtimes
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.session.folder_id.clone())
            .collect(),
    }));
    let rt2 = runtimes.clone();
    let trust2 = ctx.trust.clone();
    tokio::spawn(async move {
        run_peer_loop(peer_cfg, rt2, trust2, None).await;
    });

    if auto_confirm {
        let pm = ctx.pairing.clone();
        tokio::spawn(async move {
            loop {
                for p in pm.list_pending().await {
                    pm.confirm(&p.request_id, true).await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }
    println!("FolderSync 已启动（Ctrl+C 停止）");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn pair(cli: &Cli) -> Result<()> {
    let ctx = build_context(cli).await?;
    let code = ctx
        .pairing
        .create_code(local_addrs(), ctx.relay.as_deref())
        .await;
    if let Some(relay) = &ctx.relay {
        ctx.pairing
            .spawn_relay_pairing_watcher(code.clone(), relay.clone());
    }
    println!("{}", pc::encode(&code));
    println!("\n配对码 5 分钟内有效；等待对端输入…");
    let before = ctx.trust.devices()?.len();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while tokio::time::Instant::now() < deadline {
        // CLI 没有 UI：等待 1 秒后自动确认（桌面端才有人工核对步骤）
        for p in ctx.pairing.list_pending().await {
            tokio::time::sleep(Duration::from_secs(1)).await;
            ctx.pairing.confirm(&p.request_id, true).await;
        }
        if ctx.trust.devices()?.len() > before {
            println!("配对成功！");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    anyhow::bail!("等待配对超时")
}

async fn join(cli: &Cli, code_text: String) -> Result<()> {
    let ctx = build_context(cli).await?;
    let code = pc::decode(&code_text, pc::now_secs())?;
    let mut me = ctx.pairing.own_info(local_addrs())?;
    me.relay = ctx.relay.clone().or_else(|| code.peer.relay.clone());
    let request = PairingRequest {
        token: code.token.clone(),
        peer: me,
    };

    let mut response = None;
    let mut errors = Vec::new();
    for host in &code.peer.addrs {
        let target = fs_sync_net::ssh_client::SshTarget {
            host: host.clone(),
            port: code.peer.port,
            username: code.peer.username.clone(),
            host_key: code.peer.host_key.clone(),
        };
        match fs_sync_net::ssh_client::pair_via_ssh(&target, &code.token, request.clone()).await {
            Ok(r) => {
                response = Some(r);
                break;
            }
            Err(e) => errors.push(format!("{host}: {e:#}")),
        }
    }
    if response.is_none()
        && let Some(relay) = code.peer.relay.clone().or(ctx.relay.clone())
    {
        response = Some(fs_sync_net::relay::pair_via_relay(&relay, &code.token, request).await?);
    }
    let response = response.context(format!(
        "没有可用的连接方式（{}）；请安装 Tailscale 或配置中继",
        errors.join("; ")
    ))?;
    if !response.accepted {
        anyhow::bail!("对端拒绝了配对");
    }
    fs_sync_net::identity::verify_peer_info(&response.peer)?;
    ctx.trust.add(&response.peer)?;
    println!(
        "配对成功：{} ({})",
        response.peer.device_name, response.peer.device_id
    );
    Ok(())
}

async fn status(cli: &Cli) -> Result<()> {
    let ctx = build_context(cli).await?;
    let ts = tailscale::detect();
    println!("设备名: {}", hostname());
    println!("设备 ID: {}", ctx.identity.id());
    println!("指纹: {}", fingerprint_text(&ctx.identity.id()));
    println!("Tailscale 已安装: {}", ts.installed);
    println!("Tailscale 已连接: {}", ts.running);
    if !ts.ips.is_empty() {
        println!("Tailscale 地址: {}", ts.ips.join(", "));
    }
    println!("中继: {}", ctx.relay.as_deref().unwrap_or("(未配置)"));
    println!("已配对设备:");
    if ctx.trust.devices()?.is_empty() {
        println!("  (无)");
    }
    for d in ctx.trust.devices()? {
        println!(
            "  - {} [{}] {}:{}",
            d.name,
            d.id,
            d.addresses.join(","),
            d.port
        );
    }
    Ok(())
}

async fn sync_once(cli: &Cli, path: String, peer: String) -> Result<()> {
    let ctx = build_context(cli).await?;
    let device = ctx.trust.get(&peer)?.context("设备未配对（先 join）")?;
    let root = PathBuf::from(&path);
    let folder_id = format!(
        "folder-{}",
        &blake3::hash(root.to_string_lossy().as_bytes()).to_hex()[..16]
    );
    let rt = Arc::new(
        FolderRuntime::start(
            folder_id.clone(),
            root,
            ctx.identity.id(),
            ctx.store.clone(),
            ctx.blobs.clone(),
            100,
        )
        .await?,
    );
    let target = fs_sync_net::ssh_client::SshTarget {
        host: device
            .addresses
            .first()
            .context("设备没有 Tailscale 地址")?
            .clone(),
        port: device.port,
        username: device.username.clone(),
        host_key: device.host_key.clone(),
    };
    let mut client = fs_sync_net::ssh_client::SshClient::connect(&target, &ctx.identity).await?;
    let entries: Vec<_> = rt.session.snapshot().await.into_values().collect();
    client
        .send(&fs_sync_net::message::Message::Index { folder_id, entries })
        .await?;
    println!("已推送索引，等待对端拉取内容 30 秒…");
    tokio::time::sleep(Duration::from_secs(30)).await;
    Ok(())
}
