//! mDNS/DNS-SD 本地发现：同一 LAN 内公布本机 SSH 服务，发现已配对对端的地址。
//!
//! 只补地址、不自动配对：只有指纹已在信任列表里的设备才会被补进连接目标，
//! 陌生人广播的服务一律忽略。无 Tailscale、中继不可达时，这是第三条路
//! （Syncthing 的 local discovery 同理）。
//!
//! 服务类型：`_foldersync._tcp.local.`；TXT：`id=<设备指纹 hex 前 32>`。

use std::collections::HashMap;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info, warn};

/// mDNS 服务类型常量。
pub const SERVICE_TYPE: &str = "_foldersync._tcp.local.";

/// 设备指纹 → mDNS 实例名需要稳定且合法（字母数字与 '-'）。
fn instance_name(device_id: &str) -> String {
    let safe: String = device_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .take(48)
        .collect();
    format!("foldersync-{safe}")
}

/// 从 TXT 记录取设备指纹。
fn txt_device_id(properties: &mdns_sd::TxtProperties) -> Option<String> {
    properties.get_property_val_str("id").map(str::to_string)
}

/// 一条发现的局域网服务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanPeer {
    pub device_id: String,
    pub ip: String,
    pub port: u16,
}

/// 本机 mDNS 发布句柄；drop 即停止广播。
pub struct MdnsPublisher {
    daemon: ServiceDaemon,
    service_fqdn: String,
}

impl MdnsPublisher {
    /// 发布本机 SSH 服务。失败只记日志返回 None（mDNS 不可用不影响同步）。
    pub fn publish(device_id: &str, port: u16) -> Option<Self> {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                warn!("mDNS 服务启动失败（本地发现不可用）: {e}");
                return None;
            }
        };
        let fqdn = format!("{}.{}", instance_name(device_id), SERVICE_TYPE);
        let host = format!("foldersync-{}.local.", instance_name(device_id));
        let props = [("id", device_id)];
        let info = match ServiceInfo::new(
            SERVICE_TYPE,
            &fqdn,
            &host,
            "", // 让 mdns-sd 自动绑定本机地址
            port,
            &props[..],
        ) {
            Ok(i) => i.enable_addr_auto(),
            Err(e) => {
                warn!("mDNS 服务信息构造失败: {e}");
                return None;
            }
        };
        match daemon.register(info) {
            Ok(_) => {
                info!(port, "mDNS 服务已公布（LAN 本地发现开启）");
                Some(Self {
                    daemon,
                    service_fqdn: fqdn,
                })
            }
            Err(e) => {
                warn!("mDNS 注册失败: {e}");
                None
            }
        }
    }

    /// 停止广播（正常关闭时调用；drop 也会兜底 unregister）。
    pub fn stop(&self) {
        let _ = self.daemon.unregister(&self.service_fqdn);
    }
}

/// 浏览 LAN 内的 FolderSync 服务，收集 `timeout` 内发现的对端。
///
/// 返回按 device_id 去重的列表。每次调用新建一个浏览器、到时即关——
/// 调用方以分钟级频率轮询即可，不做常驻监听（省一条后台线程和状态机）。
pub fn discover(timeout: Duration) -> Vec<LanPeer> {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            debug!("mDNS 浏览器启动失败: {e}");
            return Vec::new();
        }
    };
    let receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(r) => r,
        Err(e) => {
            debug!("mDNS browse 失败: {e}");
            return Vec::new();
        }
    };
    let mut found: HashMap<String, LanPeer> = HashMap::new();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        match receiver.recv_timeout(deadline - now) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let props = info.get_properties();
                let Some(device_id) = txt_device_id(props) else {
                    continue;
                };
                let port = info.get_port();
                for ip in info.get_addresses() {
                    // 只收 IPv4：IPv6 直连受双栈监听限制（见 P3-4）
                    if ip.is_ipv4() {
                        found.insert(
                            device_id.clone(),
                            LanPeer {
                                device_id: device_id.clone(),
                                ip: ip.to_string(),
                                port,
                            },
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    found.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_name_is_sanitized() {
        assert_eq!(
            instance_name("SHA256:ab12:cd34/ef56"),
            "foldersync-SHA256ab12cd34ef56"
        );
        let long = "x".repeat(200);
        assert!(instance_name(&long).len() <= 60);
    }

    #[test]
    fn discover_empty_is_ok() {
        // 没有服务可发现时必须安静返回空，不能报错挂起
        let found = discover(Duration::from_millis(200));
        assert!(found.is_empty() || found.iter().all(|p| p.port > 0));
    }
}
