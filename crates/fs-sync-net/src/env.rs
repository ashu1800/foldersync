//! 本机 SSH / Tailscale / 中继环境检测与初始化。
//!
//! 客户端安装后第一次启动会调用 [`ensure_ssh_env`]：
//! 生成身份密钥与主机密钥、创建 SFTP 沙箱目录，并在 Windows 上尝试放行防火墙。
//! 检测永远不返回错误，失败信息放进 [`EnvStatus::notes`] 供 UI 引导。

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::identity::{SshIdentity, load_or_create_host_key};
use crate::tailscale::{self, TailscaleInfo};

/// 默认 SSH 服务端口（避开系统 sshd 的 22）。
pub fn default_ssh_port() -> u16 {
    2222
}

/// 防火墙规则名。
pub const FIREWALL_RULE: &str = "FolderSync SSH";

/// 当前环境状态（前端引导页用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvStatus {
    /// 本机 SSH 身份与主机密钥是否就绪
    pub ssh_ready: bool,
    /// 实际监听端口
    pub ssh_port: u16,
    /// Windows 防火墙是否已放行
    pub firewall_ok: bool,
    pub tailscale: TailscaleInfo,
    /// 是否配置了中继服务器
    pub relay_configured: bool,
    /// 中继服务器当前是否可连
    pub relay_reachable: bool,
    /// 是否至少有一种可用传输方式
    pub ready: bool,
    /// 建议的动作：`install_tailscale` / `setup_relay` / `repair_firewall` / 空
    pub suggestions: Vec<String>,
    /// 人类可读说明（诊断用）
    pub notes: Vec<String>,
}

/// 首次启动时初始化 SSH 环境。幂等，可重复调用。
pub fn ensure_ssh_env(data_dir: &Path, port: u16) -> EnvStatus {
    let mut notes = Vec::new();
    let mut ssh_ready = false;

    let share = data_dir.join("share");
    let _ = std::fs::create_dir_all(share.join("blobs"));
    match SshIdentity::load_or_create(data_dir) {
        Ok(id) => {
            notes.push(format!("SSH 设备指纹: {}", id.id()));
        }
        Err(e) => notes.push(format!("SSH 身份密钥初始化失败: {e:#}")),
    }
    match load_or_create_host_key(data_dir) {
        Ok(_) => ssh_ready = true,
        Err(e) => notes.push(format!("SSH 主机密钥初始化失败: {e:#}")),
    }

    let firewall_ok = ensure_firewall_rule(port);
    if !firewall_ok {
        notes.push("Windows 防火墙未放行，入站连接可能失败；请在引导页点“修复防火墙”。".into());
    }

    let tailscale = tailscale::detect();
    if !tailscale.installed {
        notes.push("未检测到 Tailscale：可安装 Tailscale 走 SFTP 直连方案。".into());
    } else if !tailscale.running {
        notes.push("Tailscale 已安装但未连接：请登录并启用。".into());
    }

    EnvStatus {
        ssh_ready,
        ssh_port: port,
        firewall_ok,
        tailscale,
        relay_configured: false,
        relay_reachable: false,
        ready: ssh_ready,
        suggestions: Vec::new(),
        notes,
    }
}

/// 完整环境检测（含中继）。
pub fn detect_env(data_dir: &Path, port: u16, relay_url: Option<&str>) -> EnvStatus {
    let ssh_ready = data_dir.join("ssh_host_ed25519").exists()
        && data_dir.join("ssh_identity").exists()
        && data_dir.join("share/blobs").is_dir();
    let firewall_ok = check_firewall_rule();
    let tailscale = tailscale::detect();
    let relay_configured = relay_url.map(|u| !u.trim().is_empty()).unwrap_or(false);
    let relay_reachable = relay_url.map(probe_relay).unwrap_or(false);

    let mut suggestions = Vec::new();
    let mut notes = Vec::new();
    if !tailscale.installed {
        suggestions.push("install_tailscale".into());
        notes.push(
            "未安装 Tailscale。安装并登录后，两台电脑可在同一虚拟局域网内通过 SFTP 直连。".into(),
        );
    } else if !tailscale.running {
        suggestions.push("tailscale_login".into());
        notes.push("Tailscale 已安装但没有连上，请打开 Tailscale 登录。".into());
    }
    if !relay_configured {
        suggestions.push("setup_relay".into());
        notes.push(
            "未配置中继服务器。可在自己的服务器上运行 foldersync-relay，然后在设置里填入地址。"
                .into(),
        );
    } else if !relay_reachable {
        suggestions.push("check_relay".into());
        notes.push("中继服务器当前连不上，请检查地址、端口和防火墙。".into());
    }
    if !ssh_ready {
        suggestions.push("repair_ssh".into());
        notes.push("SSH 服务端环境未初始化，请点“初始化 SSH 环境”。".into());
    }
    if !firewall_ok {
        suggestions.push("repair_firewall".into());
        notes.push("Windows 防火墙未放行 FolderSync SSH 端口。".into());
    }

    let has_path = tailscale.running || relay_reachable;
    EnvStatus {
        ssh_ready,
        ssh_port: port,
        firewall_ok,
        tailscale,
        relay_configured,
        relay_reachable,
        ready: ssh_ready && has_path,
        suggestions,
        notes,
    }
}

/// 探测中继 TCP 端口是否可连（2 秒超时）。
pub fn probe_relay(url: &str) -> bool {
    let Ok((host, port)) = crate::relay::parse_relay_url(url) else {
        return false;
    };
    let addr = match (host.as_str(), port).to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return false,
        },
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
}

/// 检查 Windows 防火墙规则是否存在（其它平台一律视为通过）。
pub fn check_firewall_rule() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "show",
                "rule",
                &format!("name={FIREWALL_RULE}"),
            ])
            .creation_flags(0x0800_0000)
            .output();
        matches!(out, Ok(o) if o.status.success())
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// 尝试添加防火墙规则；没有管理员权限时返回 false（由 UI 引导提权修复）。
pub fn ensure_firewall_rule(port: u16) -> bool {
    #[cfg(windows)]
    {
        if check_firewall_rule() {
            return true;
        }
        use std::os::windows::process::CommandExt;
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let out = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={FIREWALL_RULE}"),
                "dir=in",
                "action=allow",
                "protocol=TCP",
                &format!("localport={port}"),
                &format!("program={exe}"),
                "profile=any",
            ])
            .creation_flags(0x0800_0000)
            .output();
        matches!(out, Ok(o) if o.status.success()) && check_firewall_rule()
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        true
    }
}

/// 以管理员权限重新执行防火墙配置（会弹 UAC）。
#[cfg(windows)]
pub fn repair_firewall_elevated(port: u16) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let script = format!(
        "Start-Process -FilePath 'netsh' -Verb RunAs -ArgumentList 'advfirewall firewall add rule name=\"{FIREWALL_RULE}\" dir=in action=allow protocol=TCP localport={port} program=\"{}\" profile=any'",
        exe.display()
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn repair_firewall_elevated(_port: u16) -> anyhow::Result<()> {
    Ok(())
}

/// SFTP 沙箱根目录（对端可读写的唯一区域）。
pub fn share_root(data_dir: &Path) -> PathBuf {
    data_dir.join("share")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_keys_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let status = ensure_ssh_env(tmp.path(), default_ssh_port());
        assert!(status.ssh_ready);
        assert!(tmp.path().join("ssh_identity").exists());
        assert!(tmp.path().join("ssh_host_ed25519").exists());
        assert!(tmp.path().join("share/blobs").is_dir());
    }

    #[test]
    fn detect_reports_not_ready_without_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let status = detect_env(tmp.path(), default_ssh_port(), None);
        assert!(!status.ssh_ready);
        assert!(!status.ready);
        assert!(status.suggestions.contains(&"repair_ssh".to_string()));
    }

    #[test]
    fn probe_rejects_bad_url() {
        assert!(!probe_relay("not a url"));
    }
}
