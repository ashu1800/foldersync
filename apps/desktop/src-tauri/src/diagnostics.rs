//! 日志与诊断包（提示词 M6）。
//!
//! 日志按天滚动写到 `<data_dir>/logs/foldersync.log`；
//! 诊断包把日志尾巴、设置、索引统计、网络状态打成一个 zip 之外的纯文本包，
//! 用户可直接贴给开发者 —— 不含文件内容，只含元数据。

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// 初始化日志：终端 + 文件（按天滚动）。
///
/// 返回落盘用的 guard，**必须**在进程存活期间持有，drop 会停止写文件。
pub fn init(log_dir: &Path, level: &str) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(log_dir).context("创建日志目录失败")?;

    // 启动时清理 N 天前的旧日志（按天滚动只建不删，忘关 debug 级别会占满盘）
    cleanup_old_logs(log_dir, 14);

    let file_appender = tracing_appender::rolling::daily(log_dir, "foldersync.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true),
        )
        .init();

    install_panic_hook(log_dir);

    Ok(guard)
}

/// 清理超过保留期的旧日志文件（foldersync.log.* / crash-*.log）。
fn cleanup_old_logs(log_dir: &Path, keep_days: u64) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(keep_days * 86_400));
    let Some(cutoff) = cutoff else { return };
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_log = name.starts_with("foldersync.log.") || name.starts_with("crash-");
        if !is_log {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false);
        if expired {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// panic 时把 backtrace 追加写入 `logs/crash-<时间戳>.log`。
/// 之前进程崩溃即死且不留痕迹，用户只能描述"闪退了"。
/// 静默失败：崩溃路径上不能因为写日志失败再 panic。
fn install_panic_hook(log_dir: &Path) {
    let dir = log_dir.to_path_buf();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let file = dir.join(format!("crash-{ts}.log"));
        let msg = format!(
            "time: {}\nthread: {}\nmessage: {info}\nbacktrace:\n{}\n",
            chrono::Local::now().to_rfc3339(),
            std::thread::current().name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::capture(),
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .and_then(|mut f| f.write_all(msg.as_bytes()));
        default_hook(info);
    }));
}

/// 诊断包内容（纯文本，便于贴到 issue）。
pub struct DiagnosticReport {
    pub text: String,
}

impl DiagnosticReport {
    /// 收集诊断信息。
    ///
    /// 刻意**不**包含：文件内容、配对 token、私钥。
    /// 包含：版本、设备 id/指纹、设置、索引统计、活动统计、日志尾巴。
    pub fn collect(
        data_dir: &Path,
        device_name: &str,
        device_id: &str,
        fingerprint: &str,
        settings_json: &str,
        store: &fs_sync_store::Store,
    ) -> Result<Self> {
        let mut out = String::new();

        out.push_str("# FolderSync 诊断报告\n");
        out.push_str(&format!(
            "生成时间: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        ));
        out.push_str(&format!("版本: {}\n", env!("CARGO_PKG_VERSION")));
        out.push_str(&format!(
            "系统: {} {}\n",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        out.push_str(&format!("设备名: {device_name}\n"));
        out.push_str(&format!("设备 ID: {device_id}\n"));
        out.push_str(&format!("指纹: {fingerprint}\n"));
        out.push_str(&format!("数据目录: {}\n\n", data_dir.display()));

        // 网络
        let ts = fs_sync_net::tailscale::detect();
        out.push_str("## 网络\n");
        out.push_str(&format!("Tailscale CLI: {}\n", ts.cli_available));
        out.push_str(&format!(
            "Tailscale 地址: {}\n",
            if ts.ips.is_empty() {
                "(无)".to_string()
            } else {
                ts.ips.join(", ")
            }
        ));
        out.push_str(&format!(
            "MagicDNS: {}\n\n",
            ts.hostname.as_deref().unwrap_or("(无)")
        ));

        // 设置（已脱敏：token/私钥不在 settings 里）
        out.push_str("## 设置\n```json\n");
        out.push_str(settings_json);
        out.push_str("\n```\n\n");

        // 设备与文件夹统计
        out.push_str("## 已配对设备\n");
        match store.list_devices() {
            Ok(devices) if !devices.is_empty() => {
                for d in devices {
                    out.push_str(&format!(
                        "- {} ({})\n",
                        d.name,
                        fs_sync_daemon::fingerprint_str(&d.id)
                    ));
                }
            }
            Ok(_) => out.push_str("(无)\n"),
            Err(e) => out.push_str(&format!("(读取失败: {e})\n")),
        }
        out.push('\n');

        out.push_str("## 文件夹\n");
        match store.list_folders() {
            Ok(folders) if !folders.is_empty() => {
                for f in folders {
                    let entries = store.list_entries(&f.id).unwrap_or_default();
                    let live = entries.iter().filter(|e| !e.deleted).count();
                    let tombstones = entries.iter().filter(|e| e.deleted).count();
                    out.push_str(&format!(
                        "- {} — 路径: {} — 文件 {} / 墓碑 {} / 暂停 {}\n",
                        f.name,
                        f.local_path.display(),
                        live,
                        tombstones,
                        f.paused
                    ));
                }
            }
            Ok(_) => out.push_str("(无)\n"),
            Err(e) => out.push_str(&format!("(读取失败: {e})\n")),
        }
        out.push('\n');

        // 日志尾巴
        out.push_str("## 最近日志\n```\n");
        out.push_str(&tail_logs(data_dir, 300));
        out.push_str("\n```\n");

        Ok(Self { text: out })
    }

    /// 写到一个文本文件，返回路径。
    pub fn write_to(&self, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir).context("创建导出目录失败")?;
        let name = format!(
            "foldersync-diagnostics-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).context("创建诊断文件失败")?;
        f.write_all(self.text.as_bytes())
            .context("写诊断文件失败")?;
        Ok(path)
    }
}

/// 取最近 N 行日志（跨文件，从最新往前）。
fn tail_logs(data_dir: &Path, lines: usize) -> String {
    let dir = data_dir.join("logs");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return "(无日志文件)".to_string();
    };

    // 按修改时间排序，取最新的几个文件
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));

    let mut collected: Vec<String> = Vec::new();
    for (_, path) in files.iter().take(3) {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut file_lines: Vec<&str> = text.lines().collect();
        let need = lines.saturating_sub(collected.len());
        if file_lines.len() > need {
            file_lines = file_lines.split_off(file_lines.len() - need);
        }
        file_lines.reverse();
        collected.extend(file_lines.into_iter().map(|s| s.to_string()));
        if collected.len() >= lines {
            break;
        }
    }

    if collected.is_empty() {
        return "(无日志内容)".to_string();
    }
    collected.reverse();
    collected.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_no_secrets_and_contains_key_sections() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("logs")).unwrap();
        std::fs::write(
            tmp.path().join("logs").join("foldersync.log.2026-10-27"),
            "line one\nline two\nline three\n",
        )
        .unwrap();

        let store = fs_sync_store::Store::open_in_memory().unwrap();
        let report = DiagnosticReport::collect(
            tmp.path(),
            "测试机",
            "abc123",
            "abcd-ef01",
            "{\"log_level\":\"info\"}",
            &store,
        )
        .unwrap();

        // 关键信息在
        assert!(report.text.contains("FolderSync 诊断报告"));
        assert!(report.text.contains("测试机"));
        assert!(report.text.contains("abcd-ef01"));
        assert!(report.text.contains("line three"), "应包含日志尾巴");
        // 敏感信息不在
        assert!(!report.text.contains("secret.key"));
        assert!(!report.text.to_lowercase().contains("private"));
    }

    #[test]
    fn tail_returns_most_recent_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let content: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        std::fs::write(logs.join("foldersync.log.a"), content).unwrap();

        let tail = tail_logs(tmp.path(), 10);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines.last(), Some(&"line100"), "最后一行应是最新的");
    }

    #[test]
    fn tail_handles_missing_log_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(tail_logs(tmp.path(), 10), "(无日志文件)");
    }

    #[test]
    fn report_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let report = DiagnosticReport {
            text: "hello".into(),
        };
        let path = report.write_to(tmp.path()).unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello");
    }

    /// 启动时清理超期日志：新的保留、旧的删除、非日志文件不动。
    #[test]
    fn cleanup_old_logs_removes_only_expired_log_files() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();

        let old = logs.join("foldersync.log.2026-01-01");
        let new = logs.join("foldersync.log.2026-10-31");
        let crash = logs.join("crash-20260101-000000.log");
        let other = logs.join("settings.json");
        for p in [&old, &new, &crash, &other] {
            std::fs::write(p, "x").unwrap();
        }
        // 把 old / crash 的修改时间拨到 30 天前
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
        for p in [&old, &crash] {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(stale).unwrap();
        }

        cleanup_old_logs(&logs, 14);

        assert!(!old.exists(), "超期日志应被删除");
        assert!(!crash.exists(), "超期崩溃日志应被删除");
        assert!(new.exists(), "新日志必须保留");
        assert!(other.exists(), "非日志文件不能动");
    }

    /// panic hook 会把崩溃信息写入 crash-*.log。
    #[test]
    fn panic_hook_writes_crash_log() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();

        // 安装 hook 后主动 panic 一次（catch_unwind 接住），验证文件落地。
        // panic_hook 是进程级全局的，这里装的是测试进程的 hook；生产代码路径相同。
        install_panic_hook(&logs);
        let result = std::panic::catch_unwind(|| {
            panic!("hook 验证用 panic");
        });
        assert!(result.is_err(), "panic 应被 catch_unwind 接住");

        let crash_files: Vec<_> = std::fs::read_dir(&logs)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("crash-"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(!crash_files.is_empty(), "崩溃日志应落地");
        let text = std::fs::read_to_string(&crash_files[0]).unwrap();
        assert!(text.contains("hook 验证用 panic"), "崩溃信息应写入");
        assert!(text.contains("backtrace:"), "应包含 backtrace 段");
    }
}
