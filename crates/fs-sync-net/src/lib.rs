//! fs-sync-net：SSH + SFTP + Tailscale / 中继服务器传输层。
//!
//! 模块：
//! - `identity`：SSH 身份密钥与主机密钥
//! - `blobstore`：按 blake3 寻址的本地内容缓存（替代 iroh-blobs）
//! - `frame` / `message`：控制消息帧
//! - `pairing`：配对码（含 SSH 公钥与地址）
//! - `ssh_client`：SSH 客户端（公钥认证 + 控制子系统 + SFTP）
//! - `sftp_fs`：沙箱化 SFTP 服务端文件系统
//! - `relay`：中继服务器 SFTP 客户端
//! - `tailscale` / `env`：环境探测与首次安装初始化

pub mod blobstore;
pub mod conn;
pub mod env;
pub mod frame;
pub mod identity;
pub mod message;
pub mod pairing;
pub mod relay;
pub mod relay_server;
pub mod sftp_fs;
pub mod ssh_client;
pub mod tailscale;

pub use conn::ConnType;

/// 控制消息子系统名。
pub const SSH_SUBSYSTEM: &str = "foldersync";
/// 配对专用子系统名。
pub const SSH_PAIR_SUBSYSTEM: &str = "foldersync-pair";
/// 应用专用 SSH 登录名（不映射系统账号）。
pub const SSH_USER: &str = "foldersync";

/// 官方自建中继（可通过设置页改成自己的服务器）。
pub const DEFAULT_RELAY_URL: &str = "tcp://36.137.140.126:8444";

#[cfg(test)]
mod tests {
    #[test]
    fn subsystem_names_are_stable() {
        assert_eq!(super::SSH_SUBSYSTEM, "foldersync");
        assert_eq!(super::SSH_PAIR_SUBSYSTEM, "foldersync-pair");
    }
}
