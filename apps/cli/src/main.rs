//! FolderSync CLI：M0 回显验证 + M2 配对与连接。
//!
//! 用法：
//!   foldersync serve [--data-dir DIR]                # 启动节点（配对服务）
//!   foldersync ping <ENDPOINT_ID> <MSG>              # M0 回显验证
//!   foldersync pair [--data-dir DIR]                 # 生成配对码
//!   foldersync join <PAIRING_CODE> [--data-dir DIR]  # 输入配对码完成配对
//!   foldersync status [--data-dir DIR]               # 显示身份与信任列表

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fs_sync_daemon::{
    PairingManager, SyncEngine, Trust, pairing::detect_tailscale, run_sync, sync::make_puller,
};
use fs_sync_net::conn::{classify_conn, measure_rtt};
use fs_sync_net::message::{
    Message, PROTOCOL_VERSION, PairingRequest, PairingResponse, error_code,
};
use fs_sync_net::{frame, pairing as pc};
use fs_sync_store::Store;
use iroh::EndpointId;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "foldersync", version, about = "FolderSync P2P 同步命令行")]
struct Cli {
    /// 数据目录（默认 ~/.foldersync）
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动节点：配对服务 + 已信任设备接入（前台运行）
    Serve {
        /// 自动确认配对请求（无 UI 的调试场景；正式桌面端应由用户点确认）
        #[arg(long)]
        auto_confirm: bool,
        /// 同步文件夹路径（可多次指定；M4 双向实时同步）
        #[arg(long = "folder", short = 'f')]
        folders: Vec<String>,
    },
    /// M0 回显验证：连接远端并回显
    Ping { target: String, message: String },
    /// 生成配对码（含本机 Tailscale 地址提示）
    Pair,
    /// 用配对码完成配对
    Join { code: String },
    /// 显示本机身份、Tailscale 状态与信任列表
    Status,
    /// 添加并同步一个文件夹（M3 单向验证：本机 → 对端）
    Sync {
        /// 本地文件夹路径
        path: String,
        /// 对端 EndpointId（已配对设备）
        peer: String,
    },
}

/// 全局同步运行时注册表（serve 进程内共享）。
type RuntimeRegistry = Arc<std::sync::Mutex<Vec<Arc<fs_sync_daemon::FolderRuntime>>>>;

/// 存放当前会话的运行时（SyncServer 通过它分发对端增量）。
static RUNTIMES: std::sync::OnceLock<RuntimeRegistry> = std::sync::OnceLock::new();

fn config_runtimes(r: RuntimeRegistry) {
    let _ = RUNTIMES.set(r);
}

fn get_runtimes() -> RuntimeRegistry {
    RUNTIMES.get().cloned().unwrap_or_default()
}

/// 协议服务端：处理已信任设备接入、配对请求与索引交换。
#[derive(Clone)]
struct SyncServer {
    pairing: PairingManager,
    trust: Trust,
    store: Arc<Store>,
    blobs: Arc<iroh_blobs::store::fs::FsStore>,
    endpoint: iroh::Endpoint,
    data_dir: std::path::PathBuf,
}

impl std::fmt::Debug for SyncServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncServer").finish_non_exhaustive()
    }
}

impl ProtocolHandler for SyncServer {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        let trusted = self.trust.is_trusted(&peer).unwrap_or(false);
        let conn_type = classify_conn(&conn)
            .map(|c| c.to_string())
            .unwrap_or_else(|| "未知".into());

        let mut bi = conn.accept_bi().await?;
        let msg = frame::read_frame(&mut bi.1)
            .await
            .map_err(|e| AcceptError::from_boxed(Box::new(std::io::Error::other(e.to_string()))))?;

        // 双向实时同步需要长连接：首条处理完后继续读后续消息，
        // 直到对端断开（客户端 sync_round 维持连接，服务端不能主动 close）。
        let mut first = Some(msg);
        loop {
            let msg = match first.take() {
                Some(m) => m,
                None => match frame::read_frame(&mut bi.1).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!("对端连接结束: {e}");
                        return Ok(());
                    }
                },
            };
            match msg {
                Message::IndexUpdate { folder_id, entries } => {
                    // 对端增量：交给对应的运行时拉取落盘（M4 双向实时同步核心）
                    if !trusted {
                        conn.close(1u32.into(), b"not trusted");
                        return Ok(());
                    }
                    let Some(rt) = get_runtimes()
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|r| r.session.folder_id == folder_id)
                        .cloned()
                    else {
                        frame::write_frame(
                            &mut bi.0,
                            &Message::Error {
                                code: error_code::FOLDER_UNKNOWN,
                                message: format!("未知文件夹: {folder_id}"),
                            },
                        )
                        .await
                        .ok();
                        return Ok(());
                    };
                    let count = entries.len();
                    if let Err(e) = rt
                        .handle_remote_update(self.endpoint.clone(), peer, entries)
                        .await
                    {
                        tracing::warn!("应用对端增量失败: {e}");
                    } else {
                        tracing::info!(count, folder = %folder_id, "已应用对端增量");
                    }
                    continue;
                }
                Message::Index {
                    folder_id,
                    entries: remote_entries,
                } => {
                    // 对端推送索引：回复本地索引，并把对端新增/更新的文件拉下来落盘
                    if !trusted {
                        conn.close(1u32.into(), b"not trusted");
                        return Ok(());
                    }
                    tracing::info!(folder = %folder_id, count = remote_entries.len(), "收到对端索引");

                    let local_entries = self.store.list_entries(&folder_id).unwrap_or_default();
                    frame::write_frame(
                        &mut bi.0,
                        &Message::Index {
                            folder_id: folder_id.clone(),
                            entries: local_entries.clone(),
                        },
                    )
                    .await
                    .map_err(|e| {
                        AcceptError::from_boxed(Box::new(std::io::Error::other(e.to_string())))
                    })?;

                    // 本地接收根：<data_dir>/inbox/<folder_id>
                    let inbox = self
                        .store
                        .list_folders()
                        .ok()
                        .and_then(|fs| fs.into_iter().find(|f| f.id == folder_id))
                        .map(|f| f.local_path)
                        .unwrap_or_else(|| self.data_dir.join("inbox").join(&folder_id));
                    std::fs::create_dir_all(&inbox).ok();

                    let engine = SyncEngine::new(
                        folder_id.clone(),
                        inbox.clone(),
                        self.store.clone(),
                        self.blobs.clone(),
                    );
                    let local_map: std::collections::HashMap<_, _> = local_entries
                        .into_iter()
                        .map(|e| (e.path.clone(), e))
                        .collect();
                    let remote_map: std::collections::HashMap<_, _> = remote_entries
                        .iter()
                        .cloned()
                        .map(|e| (e.path.clone(), e))
                        .collect();

                    // 从对端拉取内容（同一连接使用 blobs ALPN 由 downloader 处理）
                    let puller = make_puller(self.endpoint.clone(), peer, self.blobs.clone());
                    match run_sync(&engine, &local_map, &remote_map, &hostname(), puller).await {
                        Ok((stats, _)) => {
                            tracing::info!(
                                pulled = stats.pulled,
                                deleted = stats.deleted,
                                failed = stats.failed,
                                "服务端同步完成"
                            );
                            println!(
                                "[同步] 从 {} 拉取 {} 个文件，失败 {}",
                                peer.fmt_short(),
                                stats.pulled,
                                stats.failed
                            );
                        }
                        Err(e) => tracing::warn!("服务端同步失败: {e}"),
                    }
                    continue;
                }
                Message::Hello { device_name, .. } => {
                    if !trusted {
                        tracing::warn!(%device_name, "未信任设备尝试接入，已拒绝");
                        conn.close(1u32.into(), b"not trusted");
                        return Ok(());
                    }
                    tracing::info!(device = %device_name, conn_type = %conn_type, "已信任设备接入");
                    frame::write_frame(
                        &mut bi.0,
                        &Message::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            device_name: hostname(),
                            folders: Vec::new(),
                        },
                    )
                    .await
                    .map_err(|e| {
                        AcceptError::from_boxed(Box::new(std::io::Error::other(e.to_string())))
                    })?;
                    // 索引交换在 Index 分支处理，这里只完成握手
                    continue;
                }
                Message::PairingRequest(req) => {
                    match self
                        .pairing
                        .on_pairing_request(
                            peer,
                            &req.token,
                            &req.device_name,
                            Duration::from_secs(120),
                        )
                        .await
                    {
                        Ok(accepted) => {
                            frame::write_frame(
                                &mut bi.0,
                                &Message::PairingResponse(PairingResponse {
                                    accepted,
                                    device_name: hostname(),
                                }),
                            )
                            .await
                            .map_err(|e| {
                                AcceptError::from_boxed(Box::new(std::io::Error::other(
                                    e.to_string(),
                                )))
                            })?;
                        }
                        Err(e) => {
                            let _ = frame::write_frame(
                                &mut bi.0,
                                &Message::Error {
                                    code: error_code::BAD_TOKEN,
                                    message: e.to_string(),
                                },
                            )
                            .await;
                        }
                    }
                    continue;
                }
                Message::Pong | Message::Ping => {
                    continue;
                }
                other => {
                    tracing::warn!("收到意外消息: {other:?}");
                    continue;
                }
            }
        }
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".foldersync")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir).context("创建数据目录失败")?;

    match cli.cmd {
        Cmd::Serve {
            auto_confirm,
            folders,
        } => serve(&data_dir, auto_confirm, &folders).await,
        Cmd::Ping { target, message } => ping(&target, &message).await,
        Cmd::Pair => pair(&data_dir).await,
        Cmd::Join { code } => join(&data_dir, &code).await,
        Cmd::Status => status(&data_dir).await,
        Cmd::Sync { path, peer } => sync_folder(&data_dir, &path, &peer).await,
    }
}

async fn serve(data_dir: &std::path::Path, auto_confirm: bool, folders: &[String]) -> Result<()> {
    let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
    let secret = fs_sync_net::identity::load_or_create_secret(data_dir)?;
    // 用和桌面端相同的中继，否则 CLI 与 GUI 会各在各的中继上找不到对方
    let endpoint = fs_sync_net::bind_endpoint_with(&fs_sync_net::NetConfig::default(), secret)
        .await
        .context("iroh Endpoint 绑定失败")?;

    let my_id = endpoint.id();
    let trust = Trust::new(store.clone());
    let pairing = PairingManager::new(trust.clone(), my_id, store.clone());
    // blob 存储 + 服务端：同时挂载同步协议与 iroh-blobs 内容协议
    let blobs = Arc::new(
        fs_sync_net::blobs::open_store(&data_dir.join("blobs"))
            .await
            .context("打开 blob 存储失败")?,
    );

    // M4：为每个 --folder 启动实时双向同步运行时
    let runtimes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut folder_ids = Vec::new();
    for f in folders {
        let path = std::path::PathBuf::from(f);
        anyhow::ensure!(path.is_dir(), "不是文件夹: {f}");
        // ponytail: CLI 阶段用「文件夹名」派生 folder_id，两端同名即匹配。
        // 正式模型（提示词 4.3）应为创建方生成 UUID 并经 FolderInvite 下发，M5 接入。
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".into());
        let folder_id = format!("folder-{}", &blake3::hash(name.as_bytes()).to_hex()[..16]);
        folder_ids.push(folder_id.clone());
        // 登记到 store（接收端据此定位本地路径）
        let _ = store.add_folder(&fs_sync_store::Folder {
            id: folder_id.clone(),
            name: name.clone(),
            local_path: path.clone(),
            paused: false,
            tombstone_days: 30,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        });
        let rt = fs_sync_daemon::FolderRuntime::start(
            folder_id.clone(),
            path.clone(),
            my_id.to_string(),
            store.clone(),
            blobs.clone(),
            400,
        )
        .await
        .with_context(|| format!("启动同步文件夹失败: {f}"))?;
        println!(
            "同步文件夹已挂载: {} → {}",
            path.display(),
            &folder_id[..20]
        );
        runtimes.lock().unwrap().push(Arc::new(rt));
    }
    config_runtimes(runtimes.clone());

    // 对等连接循环：主动连每个已信任设备，交换索引并持续收发增量（M4 核心）
    {
        let ep = endpoint.clone();
        let runtimes = runtimes.clone();
        let trust2 = trust.clone();
        let ids = folder_ids.clone();
        let me = my_id.to_string();
        let name = hostname();
        tokio::spawn(async move {
            fs_sync_daemon::run_peer_loop(ep, me, ids, runtimes, trust2, name).await
        });
    }

    let server = SyncServer {
        pairing: pairing.clone(),
        trust: trust.clone(),
        store: store.clone(),
        blobs: blobs.clone(),
        endpoint: endpoint.clone(),
        data_dir: data_dir.to_path_buf(),
    };
    let router = Router::builder(endpoint.clone())
        .accept(fs_sync_net::ALPN, server)
        .accept(
            iroh_blobs::ALPN,
            iroh_blobs::BlobsProtocol::new(&blobs, None),
        )
        .spawn();

    // 无 UI 时自动确认配对（CLI 调试用）；桌面端由界面确认
    if auto_confirm {
        let pm = pairing.clone();
        tokio::spawn(async move {
            loop {
                for p in pm.list_pending().await {
                    tracing::info!(
                        peer = %p.peer_name,
                        fingerprint = %p.fingerprint,
                        "自动确认配对请求"
                    );
                    pm.confirm(&p.request_id, true).await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }

    let ts = detect_tailscale();
    println!("FolderSync 节点已启动");
    println!("设备 ID: {my_id}");
    println!("数据目录: {}", data_dir.display());
    match (&ts.hostname, ts.ips.is_empty()) {
        (Some(h), _) => println!("Tailscale: {h} ({})", ts.ips.join(", ")),
        (None, false) => println!("Tailscale 地址: {}", ts.ips.join(", ")),
        (None, true) => println!("Tailscale: 未检测到"),
    }
    let devs = trust.devices().unwrap_or_default();
    println!("已配对设备: {} 台", devs.len());
    for d in &devs {
        println!("  - {} ({})", d.name, &d.id[..12]);
    }
    println!("\n可用命令：foldersync pair | join <code> | status");

    tokio::signal::ctrl_c().await.context("等待 Ctrl+C 失败")?;
    let _ = router.shutdown().await;
    endpoint.close().await;
    Ok(())
}

async fn ping(target: &str, message: &str) -> Result<()> {
    let id: EndpointId = target
        .parse()
        .with_context(|| format!("解析 EndpointId 失败: {target}"))?;
    let echoed = fs_sync_net::echo_roundtrip(id.into(), message.as_bytes()).await?;
    if echoed == message.as_bytes() {
        println!("回显校验通过（{} 字节）", echoed.len());
        Ok(())
    } else {
        bail!("回显内容不一致！")
    }
}

async fn pair(data_dir: &std::path::Path) -> Result<()> {
    let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
    let secret = fs_sync_net::identity::load_or_create_secret(data_dir)?;
    let my_id = secret.public();
    let trust = Trust::new(store.clone());
    let pairing = PairingManager::new(trust, my_id, store);

    let ts = detect_tailscale();
    let code = pairing.create_code(&ts.ips).await;
    println!("配对码（5 分钟内有效，一次性）：\n");
    println!("{}", pc::encode(&code));
    println!("\n对端执行：foldersync join <上面的配对码>");
    if let Some(h) = &ts.hostname {
        println!("本机 Tailscale: {h}");
    }
    Ok(())
}

async fn join(data_dir: &std::path::Path, text: &str) -> Result<()> {
    let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
    let secret = fs_sync_net::identity::load_or_create_secret(data_dir)?;
    let endpoint = fs_sync_net::bind_endpoint_with(&fs_sync_net::NetConfig::default(), secret)
        .await
        .context("iroh Endpoint 绑定失败")?;

    let code = pc::decode(text, pc::now_secs()).context("配对码无效")?;
    let addr = pc::to_endpoint_addr(&code)?;
    let peer_id = addr.id;

    println!("连接对端 {} ...", &peer_id.to_string()[..12]);
    let conn = endpoint
        .connect(addr, fs_sync_net::ALPN)
        .await
        .context("连接对端失败")?;
    println!(
        "已连接（{}）",
        classify_conn(&conn)
            .map(|c| c.to_string())
            .unwrap_or_else(|| "未知".into())
    );

    let (mut send, mut recv) = conn.open_bi().await?;
    frame::write_frame(
        &mut send,
        &Message::PairingRequest(PairingRequest {
            token: code.token.clone(),
            device_name: hostname(),
        }),
    )
    .await?;

    match frame::read_frame(&mut recv).await? {
        Message::PairingResponse(resp) if resp.accepted => {
            Trust::new(store).add(&peer_id, &resp.device_name)?;
            println!("配对成功！对端设备：{}", resp.device_name);
            if let Some(rtt) = measure_rtt(&conn) {
                println!("延迟: {} ms", rtt.as_millis());
            }
        }
        Message::PairingResponse(_) => bail!("对端拒绝配对"),
        Message::Error { code, message } => bail!("对端错误 (code={code}): {message}"),
        other => bail!("意外回复: {other:?}"),
    }
    endpoint.close().await;
    Ok(())
}

async fn status(data_dir: &std::path::Path) -> Result<()> {
    let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
    let secret = fs_sync_net::identity::load_or_create_secret(data_dir)?;
    let ts = detect_tailscale();

    println!("设备 ID: {}", secret.public());
    println!("指纹: {}", fs_sync_daemon::fingerprint(&secret.public()));
    println!("数据目录: {}", data_dir.display());
    println!(
        "Tailscale CLI: {}",
        if ts.cli_available {
            "可用"
        } else {
            "不可用"
        }
    );
    if let Some(h) = &ts.hostname {
        println!("Tailscale 主机名: {h}");
    }
    println!(
        "Tailscale 地址: {}",
        if ts.ips.is_empty() {
            "无".to_string()
        } else {
            ts.ips.join(", ")
        }
    );
    println!("\n已配对设备:");
    let devs = store.list_devices()?;
    if devs.is_empty() {
        println!("  （无）");
    }
    for d in devs {
        println!("  - {}  {}...", d.name, &d.id[..12]);
    }
    Ok(())
}

/// M3 单向同步：扫描本地文件夹 → 建立连接 → 交换索引 → 推送文件。
async fn sync_folder(data_dir: &std::path::Path, local_path: &str, peer: &str) -> Result<()> {
    use fs_sync_daemon::sync::{make_puller, scan_and_index};
    use fs_sync_daemon::{SyncEngine, run_sync};

    let root = std::path::PathBuf::from(local_path);
    anyhow::ensure!(root.is_dir(), "本地路径不是文件夹: {local_path}");

    let store = Arc::new(Store::open(&data_dir.join("state.sqlite3"))?);
    let secret = fs_sync_net::identity::load_or_create_secret(data_dir)?;
    let my_id = secret.public();
    let endpoint = fs_sync_net::bind_endpoint_with(&fs_sync_net::NetConfig::default(), secret)
        .await
        .context("iroh Endpoint 绑定失败")?;

    let peer_id: EndpointId = peer.parse().context("对端 EndpointId 无效")?;

    // 文件夹身份：用路径派生稳定 folder_id
    let folder_id = format!(
        "folder-{}",
        &blake3::hash(root.to_string_lossy().as_bytes()).to_hex()[..16]
    );

    // blob 存储（全进程共享一个实例）
    let blobs = Arc::new(
        fs_sync_net::blobs::open_store(&data_dir.join("blobs-client"))
            .await
            .context("打开 blob 存储失败")?,
    );

    println!("扫描本地文件夹: {}", root.display());
    let local = scan_and_index(&root, &folder_id, &my_id.to_string(), &store)?;
    let files = local.values().filter(|e| !e.is_dir && !e.deleted).count();
    println!("本地条目: {} 个文件", files);

    let engine = SyncEngine::new(
        folder_id.clone(),
        root.clone(),
        store.clone(),
        blobs.clone(),
    );

    // 把所有本地文件放进 blob 存储（供对端拉取）
    let mut added = 0;
    for e in local.values().filter(|e| !e.is_dir && !e.deleted) {
        if engine.ensure_blob(e).await.is_ok() {
            added += 1;
        }
    }
    println!("已加入 blob 存储: {added} 个文件");

    // sync 端也必须提供 blobs 服务：对端要回来拉取内容。
    // 不挂这个协议，对端会报 "Unable to download"。
    let _router = Router::builder(endpoint.clone())
        .accept(
            iroh_blobs::ALPN,
            iroh_blobs::BlobsProtocol::new(&blobs, None),
        )
        .spawn();

    // 连接对端并交换索引
    println!("连接对端 {} ...", &peer.to_string()[..12]);
    let conn = endpoint
        .connect(peer_id, fs_sync_net::ALPN)
        .await
        .context("连接对端失败")?;
    println!(
        "已连接（{}）",
        classify_conn(&conn)
            .map(|c| c.to_string())
            .unwrap_or_else(|| "未知".into())
    );

    // 协商文件夹：发 Index，收对端 Index
    let (mut send, mut recv) = conn.open_bi().await?;
    let entries: Vec<_> = local.values().cloned().collect();
    frame::write_frame(
        &mut send,
        &Message::Index {
            folder_id: folder_id.clone(),
            entries: entries.clone(),
        },
    )
    .await?;
    println!("已发送索引（{} 条）", entries.len());

    let remote = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        frame::read_frame(&mut recv),
    )
    .await
    {
        Ok(Ok(Message::Index { entries, .. })) => {
            println!("收到对端索引（{} 条）", entries.len());
            entries.into_iter().map(|e| (e.path.clone(), e)).collect()
        }
        Ok(Ok(other)) => bail!("对端返回意外消息: {other:?}"),
        Ok(Err(e)) => bail!("读取对端索引失败: {e}"),
        Err(_) => bail!("等待对端索引超时"),
    };

    // 本端视角的 diff：Push=对端需要我们的文件，Pull=我们需要对端的文件。
    // 提示词 M3 是「A→B 单向」：A 推 B 拉。因此这里统计将要推给对端的条目，
    // 并保持在线让对端把内容拉走；同时把对端独有的文件拉到本地（真正的双向基础）。
    let puller = make_puller(endpoint.clone(), peer_id, blobs.clone());
    let (stats, _actions) = run_sync(&engine, &local, &remote, "local", puller).await?;

    println!("\n本地视角完成：");
    println!("  需推送给对端: {} 个", stats.pushed);
    println!("  已从对端拉取: {} 个", stats.pulled);
    println!("  冲突:   {}", stats.conflicts);
    println!("  删除:   {}", stats.deleted);
    println!("  失败:   {}", stats.failed);

    // 保持在线，等对端把索引里标记为 Push 的内容拉完（否则对端下载失败）
    if stats.pushed > 0 {
        println!("\n等待对端拉取内容（最多 60 秒）...");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        // 对端拉完会关闭连接；这里等连接关闭或超时
        let _ = tokio::time::timeout_at(deadline, conn.closed()).await;
        println!("对端已完成拉取");
    }

    endpoint.close().await;
    Ok(())
}
