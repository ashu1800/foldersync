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
    Status {
        /// 以 JSON 输出（脚本/监控用）
        #[arg(long)]
        json: bool,
    },
    /// 单向推送一个文件夹到指定设备（调试）
    Sync { path: String, peer: String },
    /// 列出冲突副本（默认全部文件夹）
    Conflicts {
        /// 只看某个文件夹名
        #[arg(long = "folder", short = 'f')]
        folder: Option<String>,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
        /// 保留某个副本：`keep=conflict|original`（配合 --path）
        #[arg(long)]
        keep: Option<String>,
        /// 要解决的冲突副本路径（配合 --keep）
        #[arg(long)]
        path: Option<String>,
    },
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".foldersync")
}

fn relay_of(cli: &Cli, settings: &serde_json::Value) -> Option<String> {
    cli.relay
        .clone()
        .or_else(|| std::env::var("FOLDERSYNC_RELAY").ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            settings["relay_url"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| Some(fs_sync_net::DEFAULT_RELAY_URL.to_string()))
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "foldersync-cli".into())
}

/// CLI 的中继 host key 固定表：存 `<data_dir>/relay_host_keys.json`（url → 公钥行）。
/// 与桌面端 settings.json 各自独立，互不影响。
fn cli_relay_host_keys(
    data_dir: &std::path::Path,
) -> std::sync::Arc<fs_sync_daemon::runtime::RelayHostKeys> {
    use std::sync::Arc;
    let path = Arc::new(data_dir.join("relay_host_keys.json"));
    let path_for_get = path.clone();
    let get = Arc::new(move |url: &str| {
        let text = std::fs::read_to_string(&*path_for_get).ok()?;
        let map: std::collections::HashMap<String, String> = serde_json::from_str(&text).ok()?;
        map.get(url).cloned()
    });
    let set = Arc::new(move |url: &str, key: &str| {
        let current: std::collections::HashMap<String, String> = std::fs::read_to_string(&*path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut map = current;
        map.insert(url.to_string(), key.to_string());
        if let Ok(json) = serde_json::to_string_pretty(&map) {
            let _ = std::fs::write(&*path, json);
        }
    });
    Arc::new(fs_sync_daemon::runtime::RelayHostKeys::new(get, set))
}

/// 把设置里的全局默认忽略规则追加到文件夹 `.syncignore`（不覆盖已有内容）。
fn apply_default_ignore(data_dir: &std::path::Path, root: &std::path::Path) -> Result<()> {
    let Ok(text) = std::fs::read_to_string(data_dir.join("settings.json")) else {
        return Ok(());
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(());
    };
    let rules: Vec<String> = settings["default_ignore"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .map(str::to_string)
        .collect();
    if rules.is_empty() {
        return Ok(());
    }
    let ignore_path = root.join(".syncignore");
    let mut existing = std::fs::read_to_string(&ignore_path).unwrap_or_default();
    for rule in rules {
        if !existing.lines().any(|line| line.trim() == rule) {
            if !existing.is_empty() && !existing.ends_with('\n') {
                existing.push('\n');
            }
            existing.push_str(&rule);
            existing.push('\n');
        }
    }
    std::fs::write(ignore_path, existing)?;
    Ok(())
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
    device_name: String,
    network_mode: NetworkMode,
    ssh_port: u16,
}

async fn build_context(cli: &Cli) -> Result<Context2> {
    let data_dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let file_settings: serde_json::Value = std::fs::read_to_string(data_dir.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let settings_relay = relay_of(cli, &file_settings);
    let device_name = file_settings["device_name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(hostname);
    let network_mode = match file_settings["network_mode"].as_str() {
        Some("tailscale") => NetworkMode::Tailscale,
        Some("relay") => NetworkMode::Relay,
        _ => NetworkMode::Auto,
    };
    let ssh_port = file_settings["ssh_port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or_else(fs_sync_net::env::default_ssh_port);
    let env = fs_sync_net::env::ensure_ssh_env(&data_dir, ssh_port);
    let identity = SshIdentity::load_or_create(&data_dir)?;
    let host_key = load_or_create_host_key(&data_dir)?;
    let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
    let trust = Trust::new(store.clone());
    let pairing = PairingManager::new(
        trust.clone(),
        identity.clone(),
        public_key_line(host_key.public_key())?,
        env.ssh_port,
        device_name.clone(),
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
        device_name,
        network_mode,
        ssh_port: env.ssh_port,
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
        Cmd::Status { json } => status(&cli, json).await,
        Cmd::Sync { path, peer } => sync_once(&cli, path, peer).await,
        Cmd::Conflicts {
            folder,
            json,
            keep,
            path,
        } => conflicts(&cli, folder, json, keep, path).await,
    }
}

async fn start_sync_server(
    ctx: &Context2,
    runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>>,
) -> Result<fs_sync_daemon::SshServerHandle> {
    let shared = Arc::new(SshServerShared {
        identity: ctx.identity.clone(),
        host_key: ctx.host_key.clone(),
        device_id: ctx.identity.id(),
        device_name: ctx.device_name.clone(),
        ssh_port: ctx.ssh_port,
        data_dir: ctx.data_dir.clone(),
        sync_root: std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.data_dir.clone())
            .join("FolderSync"),
        trust: ctx.trust.clone(),
        pairing: ctx.pairing.clone(),
        store: ctx.store.clone(),
        blobs: ctx.blobs.clone(),
        invite_lock: tokio::sync::Mutex::new(()),
        removing_folders: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        paused_all: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        runtimes,
        online: Arc::new(tokio::sync::Mutex::new(Default::default())),
        relay_host_keys: cli_relay_host_keys(&ctx.data_dir),
        sftp_bandwidth: None,
    });
    let server = run_ssh_server(shared).await?;
    ctx.pairing.set_ssh_port(server.port);
    Ok(server)
}

async fn serve(cli: &Cli, auto_confirm: bool, folders: Vec<String>) -> Result<()> {
    let ctx = build_context(cli).await?;
    // 活动表只保留最近 2 万条（与桌面端一致），长期 serve 不至于无限膨胀
    let _ = ctx.store.prune_activity(20_000);
    let runtimes: Arc<std::sync::Mutex<Vec<Arc<FolderRuntime>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    for folder in folders {
        let root = PathBuf::from(&folder);
        if !root.is_dir() {
            anyhow::bail!("不是文件夹: {folder}");
        }
        apply_default_ignore(&ctx.data_dir, &root)?;
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
                shared_with: Vec::new(),
            })?;
        }
        let stored = ctx
            .store
            .list_folders()?
            .into_iter()
            .find(|f| f.id == folder_id);
        let shared_with = stored
            .as_ref()
            .map(|f| f.shared_with.clone())
            .unwrap_or_default();
        let rt = FolderRuntime::start(
            folder_id,
            root,
            ctx.identity.id(),
            ctx.store.clone(),
            ctx.blobs.clone(),
            400,
        )
        .await?;
        rt.set_shared_with(shared_with);
        runtimes.lock().unwrap().push(Arc::new(rt));
    }

    let server = start_sync_server(&ctx, runtimes.clone()).await?;
    let actual_port = server.port;

    let peer_cfg = Arc::new(RwLock::new(PeerLoopConfig {
        identity: ctx.identity.clone(),
        device_id: ctx.identity.id(),
        device_name: ctx.device_name.clone(),
        ssh_port: actual_port,
        mode: ctx.network_mode,
        relay_url: ctx.relay.clone(),
        relay_host_keys: cli_relay_host_keys(&ctx.data_dir),
        relay_uploaded_blobs: Some(Arc::new(std::sync::Mutex::new(Default::default()))),
        store: ctx.store.clone(),
        blobs: ctx.blobs.clone(),
        sync_root: std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.data_dir.clone())
            .join("FolderSync"),
        paused_all: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

async fn status(cli: &Cli, json: bool) -> Result<()> {
    let ctx = build_context(cli).await?;
    let ts = tailscale::detect();
    if json {
        let devices: Vec<_> = ctx
            .trust
            .devices()?
            .into_iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "name": d.name,
                    "addresses": d.addresses,
                    "port": d.port,
                })
            })
            .collect();
        let out = serde_json::json!({
            "device_name": hostname(),
            "device_id": ctx.identity.id(),
            "fingerprint": fingerprint_text(&ctx.identity.id()),
            "tailscale": {
                "installed": ts.installed,
                "running": ts.running,
                "ips": ts.ips,
            },
            "relay": ctx.relay,
            "devices": devices,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
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

/// 一条冲突副本信息（CLI 展示用）。
struct ConflictRow {
    folder: String,
    folder_path: String,
    path: String,
    original_path: String,
    size: i64,
    mtime: i64,
    device: String,
}

/// 扫描全部（或指定）文件夹的冲突副本。与桌面端同规则：
/// 索引里有 `.sync-conflict-`、非墓碑、磁盘上仍存在。
fn scan_conflicts(ctx: &Context2, want: Option<&str>) -> Result<Vec<ConflictRow>> {
    let mut rows = Vec::new();
    for f in ctx.store.list_folders()? {
        if let Some(w) = want
            && f.id != w
            && f.name != w
        {
            continue;
        }
        for e in ctx
            .store
            .list_entries(&f.id)?
            .into_iter()
            .filter(|e| !e.deleted && e.path.contains(".sync-conflict-"))
            .filter(|e| f.local_path.join(&e.path).is_file())
        {
            let name = e.path.rsplit('/').next().unwrap_or(&e.path);
            let (original_name, device) = fs_sync_core::parse_conflict_name(name);
            let original_path = match e.path.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{original_name}"),
                None => original_name.clone(),
            };
            rows.push(ConflictRow {
                folder: f.name.clone(),
                folder_path: f.local_path.display().to_string(),
                path: e.path.clone(),
                original_path,
                size: e.size as i64,
                mtime: e.mtime,
                device,
            });
        }
    }
    Ok(rows)
}

async fn conflicts(
    cli: &Cli,
    folder: Option<String>,
    json: bool,
    keep: Option<String>,
    path: Option<String>,
) -> Result<()> {
    let ctx = build_context(cli).await?;
    // 解决模式：--keep conflict|original --path <冲突副本路径>
    if let (Some(keep), Some(conflict_path)) = (keep, path) {
        let mode = keep.as_str();
        anyhow::ensure!(
            mode == "conflict" || mode == "original",
            "--keep 只接受 conflict 或 original"
        );
        // 找到副本所在的文件夹
        let mut target = None;
        for f in ctx.store.list_folders()? {
            if ctx
                .store
                .list_entries(&f.id)?
                .iter()
                .any(|e| e.path == conflict_path)
            {
                target = Some(f);
                break;
            }
        }
        let f = target.context("找不到该冲突副本（检查路径与文件夹）")?;
        let conflict_abs = f.local_path.join(&conflict_path);
        let name = conflict_path.rsplit('/').next().unwrap_or(&conflict_path);
        let (original_name, _) = fs_sync_core::parse_conflict_name(name);
        let original_path = match conflict_path.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{original_name}"),
            None => original_name.clone(),
        };
        let original_abs = f.local_path.join(&original_path);
        match mode {
            "conflict" => {
                // 副本顶掉原文件：副本改名回原名（原文件先删）
                if original_abs.exists() {
                    std::fs::remove_file(&original_abs)
                        .with_context(|| format!("删除原文件失败: {}", original_abs.display()))?;
                }
                std::fs::rename(&conflict_abs, &original_abs)
                    .with_context(|| format!("改名失败: {}", conflict_abs.display()))?;
                // 索引：原路径复活、副本变墓碑（引擎下次全量交换时同步到对端）
                let mut e = ctx
                    .store
                    .list_entries(&f.id)?
                    .into_iter()
                    .find(|e| e.path == conflict_path)
                    .context("索引里没有该副本")?;
                let new_hash = fs_sync_core::hash_file(&original_abs)?;
                let meta = std::fs::metadata(&original_abs)?;
                e.path = original_path.clone();
                e.hash = new_hash;
                e.size = meta.len() as u64;
                e.mtime = meta
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() as i64;
                e.deleted = false;
                ctx.store.upsert_entry(&f.id, &e)?;
                let mut tomb = e;
                tomb.path = conflict_path.clone();
                tomb.deleted = true;
                ctx.store.upsert_entry(&f.id, &tomb)?;
                println!("已保留副本，原路径更新为副本内容");
            }
            "original" => {
                std::fs::remove_file(&conflict_abs)
                    .with_context(|| format!("删除副本失败: {}", conflict_abs.display()))?;
                let mut e = ctx
                    .store
                    .list_entries(&f.id)?
                    .into_iter()
                    .find(|e| e.path == conflict_path)
                    .context("索引里没有该副本")?;
                e.deleted = true;
                ctx.store.upsert_entry(&f.id, &e)?;
                println!("已删除副本，保留原文件");
            }
            _ => unreachable!(),
        }
        println!(
            "注意：若 `serve` 正在运行，其内存索引与本命令的改动会在下一次全量交换时对齐；建议处理完冲突后重启 serve。"
        );
        return Ok(());
    }

    let rows = scan_conflicts(&ctx, folder.as_deref())?;
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "folder": r.folder,
                    "folder_path": r.folder_path,
                    "conflict_path": r.path,
                    "original_path": r.original_path,
                    "size": r.size,
                    "mtime": r.mtime,
                    "device": r.device,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("没有冲突副本");
        return Ok(());
    }
    for r in rows {
        println!(
            "[{}] {} ({} 字节, 设备: {})；原名: {}",
            r.folder, r.path, r.size, r.device, r.original_path
        );
    }
    println!("\n解决: foldersync conflicts --path <冲突路径> --keep conflict|original");
    Ok(())
}

async fn sync_once(cli: &Cli, path: String, peer: String) -> Result<()> {
    let ctx = build_context(cli).await?;
    let device = ctx.trust.get(&peer)?.context("设备未配对（先 join）")?;
    let root = PathBuf::from(&path);
    apply_default_ignore(&ctx.data_dir, &root)?;
    let folder_name = root
        .file_name()
        .context("同步路径没有文件夹名")?
        .to_string_lossy();
    // 与桌面端一致：folder_id 由文件夹名生成（两端必须同名才能匹配）
    let folder_id = format!(
        "folder-{}",
        &blake3::hash(folder_name.as_bytes()).to_hex()[..16]
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
    // 一次性同步只针对指定设备，避免把文件夹暴露给其他已配对设备
    rt.set_shared_with(vec![device.id.clone()]);
    // 必须同时起 SSH/SFTP 服务端：对端收到索引后会回连拉 blob
    let runtimes = Arc::new(std::sync::Mutex::new(vec![rt.clone()]));
    let _server = start_sync_server(&ctx, runtimes).await?;
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
