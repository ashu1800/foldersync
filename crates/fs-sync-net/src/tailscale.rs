//! Tailscale 探测与环境判定。
//!
//! - 识别本机 Tailscale 地址（IPv4 100.64.0.0/10，IPv6 fd7a:115c:a1e0::/48）
//! - 区分「没装」「装了没登录/没运行」「已连接」
//! - 探测失败一律静默回退，绝不阻塞主流程

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Tailscale IPv4 网段：100.64.0.0/10
const TS_V4_NET: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 0);
const TS_V4_PREFIX: u8 = 10;
/// Tailscale IPv6 ULA 前缀：fd7a:115c:a1e0::/48
const TS_V6_PREFIX: [u8; 6] = [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0];

/// 判断 IPv4 是否属于 Tailscale 网段（100.64.0.0/10）。
pub fn is_tailscale_v4(ip: Ipv4Addr) -> bool {
    let mask = u32::MAX << (32 - TS_V4_PREFIX as u32);
    u32::from(ip) & mask == u32::from(TS_V4_NET) & mask
}

/// 判断 IPv6 是否属于 Tailscale ULA 前缀（fd7a:115c:a1e0::/48）。
pub fn is_tailscale_v6(ip: Ipv6Addr) -> bool {
    ip.octets()[..6] == TS_V6_PREFIX
}

/// 判断任意 IP 是否 Tailscale 地址。
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_tailscale_v4(v4),
        IpAddr::V6(v6) => is_tailscale_v6(v6),
    }
}

/// 本机 Tailscale 检测结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleInfo {
    /// 是否检测到 Tailscale 安装（CLI 或网段地址）
    pub installed: bool,
    /// 是否已连接（有地址且后台状态为 Running）
    pub running: bool,
    /// 检测到的本机 Tailscale IP
    pub ips: Vec<String>,
    /// `tailscale status --json` 得到的 MagicDNS 名（可能为空）
    pub hostname: Option<String>,
    /// 是否检测到可用的 tailscale CLI
    pub cli_available: bool,
}

/// 探测本机 Tailscale：优先 CLI，失败则枚举本机地址筛网段。全程不报错。
pub fn detect() -> TailscaleInfo {
    let mut info = TailscaleInfo::default();

    match run_tailscale_status() {
        Ok(status) => {
            info.installed = true;
            info.cli_available = true;
            info.running = status.backend_state.as_deref() == Some("Running")
                || !status.self_.tailscale_ips.is_empty();
            info.hostname = Some(status.self_.dns_name.trim_end_matches('.').to_string())
                .filter(|h| !h.is_empty());
            info.ips = status
                .self_
                .tailscale_ips
                .iter()
                .flatten()
                .cloned()
                .collect();
        }
        Err(StatusError::NotInstalled) => {}
        Err(StatusError::NotRunning) => {
            info.installed = true;
            info.cli_available = true;
        }
        Err(StatusError::Unknown) => {}
    }

    if info.ips.is_empty() {
        info.ips = local_tailscale_ips();
        if !info.ips.is_empty() {
            info.installed = true;
            info.running = true;
        }
    }
    info
}

#[derive(Debug)]
enum StatusError {
    NotInstalled,
    NotRunning,
    Unknown,
}

/// 执行 `tailscale status --json` 并解析。
fn run_tailscale_status() -> Result<TailscaleStatus, StatusError> {
    let mut cmd = std::process::Command::new("tailscale");
    cmd.arg("status").arg("--json");
    // Windows 下不弹控制台窗口
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StatusError::NotInstalled
        } else {
            StatusError::Unknown
        }
    })?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if text.contains("not running") || text.contains("stopped") {
            return Err(StatusError::NotRunning);
        }
        return Err(StatusError::Unknown);
    }
    serde_json::from_slice(&out.stdout).map_err(|_| StatusError::Unknown)
}

/// 从本机地址列表筛出 Tailscale 网段 IP（不依赖 CLI）。
fn local_tailscale_ips() -> Vec<String> {
    // ponytail: 用系统命令枚举地址，跨平台库（if-addrs）等真正需要时再引入
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("ipconfig");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000);
        }
        c
    } else {
        let mut c = std::process::Command::new("ip");
        c.arg("-o").arg("addr");
        c
    };
    let Ok(out) = cmd.output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ips = Vec::new();
    for tok in text.split_whitespace() {
        // 去掉 "addr:100.64.0.1" 前缀与 "/10" 后缀
        let tok = tok.rsplit(':').next().unwrap_or(tok);
        let tok = tok.split('/').next().unwrap_or(tok);
        if let Ok(ip) = tok.parse::<IpAddr>()
            && is_tailscale_ip(ip)
        {
            let s = ip.to_string();
            if !ips.contains(&s) {
                ips.push(s);
            }
        }
    }
    ips
}

/// `tailscale status --json` 的最小解析结构。
#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: Option<String>,
    #[serde(rename = "Self")]
    self_: TailscaleSelf,
}

#[derive(Debug, Deserialize)]
struct TailscaleSelf {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<Option<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_range_classification() {
        assert!(is_tailscale_v4(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailscale_v4(Ipv4Addr::new(100, 81, 188, 46)));
        assert!(is_tailscale_v4(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_tailscale_v4(Ipv4Addr::new(100, 128, 0, 0)));
        assert!(!is_tailscale_v4(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_tailscale_v4(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_tailscale_v4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn v6_prefix_classification() {
        let mut ok = [0u8; 16];
        ok[..6].copy_from_slice(&TS_V6_PREFIX);
        assert!(is_tailscale_v6(Ipv6Addr::from(ok)));

        let mut bad = [0u8; 16];
        bad[..6].copy_from_slice(&[0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe1]);
        assert!(!is_tailscale_v6(Ipv6Addr::from(bad)));
    }

    #[test]
    fn any_ip_dispatch() {
        assert!(is_tailscale_ip("100.100.1.1".parse().unwrap()));
        assert!(is_tailscale_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailscale_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_tailscale_ip("::1".parse().unwrap()));
    }

    #[test]
    fn detect_never_panics() {
        let info = detect();
        for ip in &info.ips {
            let parsed: IpAddr = ip.parse().expect("detect 返回的 IP 必须可解析");
            assert!(is_tailscale_ip(parsed));
        }
    }
}
