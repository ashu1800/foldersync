//! FolderSync 中继服务器。
//!
//! 部署在一台有公网 IP 的服务器上（Windows / Linux 均可）：
//! ```text
//! foldersync-relay --port 9443 --data-dir /var/lib/foldersync-relay
//! ```
//! 客户端只需要能出站访问这个端口，不需要任何入站端口。

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "foldersync-relay",
    version,
    about = "FolderSync 中继服务器（按房间隔离的 SFTP）"
)]
struct Cli {
    /// 监听端口
    #[arg(long, default_value_t = 9443)]
    port: u16,
    /// 数据目录（房间文件与主机密钥）
    #[arg(long, default_value = "./foldersync-relay-data")]
    data_dir: PathBuf,
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
    let handle =
        fs_sync_net::relay_server::run_relay_server(fs_sync_net::relay_server::RelayServerConfig {
            data_dir: cli.data_dir,
            port: cli.port,
        })
        .await?;
    println!(
        "FolderSync 中继服务器已启动：0.0.0.0:{}（Ctrl+C 停止）",
        handle.port
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}
