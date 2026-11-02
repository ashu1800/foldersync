//! 自看门狗：主进程派生一个 `--watchdog <pid>` 子进程监控自己。
//!
//! 为什么不用 Windows 服务：FolderSync 是 GUI 应用且同步目标是**用户目录**，
//! 服务跑在 Session 0 没有用户网络凭据/桌面/网络驱动器访问，SSH 端口与
//! Tailscale 也依赖用户会话。真正的企业级服务化是另一个工程，此处的
//! 看门狗覆盖的是最常见故障——主进程崩溃/被杀后无人重启。
//!
//! 机制：看门狗轮询目标 PID 是否存活；存活则继续等，消失则重新启动
//! 主程序（不带 --watchdog 参数）。用户正常退出时主进程会通知看门狗自杀。
//! 循环重启防护：连续快速崩溃（60 秒内 ≥3 次）时看门狗放弃并退出。

use std::time::{Duration, Instant};

use tracing::{info, warn};

/// 启动参数前缀（主进程识别用）。
pub const WATCHDOG_ARG: &str = "--watchdog";
/// 通知看门狗「正常退出，别重启我」的事件名（Windows named event）。
const EXIT_EVENT: &str = "Local\\FolderSyncWatchdogExit";
/// 快速崩溃阈值：60 秒内重启超过这个次数就放弃。
const CRASH_LIMIT: u32 = 3;

/// 主进程调用：如果自己不是看门狗，派生一个看门狗子进程。
/// 派生失败（权限等）静默——看门狗是尽力而为的兜底。
pub fn spawn() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok(child) = std::process::Command::new(&exe)
        .arg(WATCHDOG_ARG)
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        warn!("看门狗进程启动失败（崩溃自动重启不可用）");
        return;
    };
    info!(pid = child.id(), "看门狗已启动");
}

/// 主进程正常退出时调用：通知看门狗自杀（不重启）。
pub fn release() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};
        let name: Vec<u16> = EXIT_EVENT
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // 看门狗进程已创建同名事件；打开并置位
        let handle = CreateEventW(std::ptr::null(), 0, 0, name.as_ptr());
        if !handle.is_null() {
            SetEvent(handle);
            CloseHandle(handle);
        }
    }
}

/// 看门狗进程入口（`--watchdog <pid>`）。阻塞至主进程退出，然后按需重启。
pub fn run(target_pid: u32) -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        return 1;
    };
    // 正常退出事件：主进程主动置位表示"别重启"
    #[cfg(windows)]
    let exit_event = unsafe {
        use windows_sys::Win32::System::Threading::CreateEventW;
        let name: Vec<u16> = EXIT_EVENT
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // 手动复位事件（bManualReset=1），初始无信号
        CreateEventW(std::ptr::null(), 1, 0, name.as_ptr())
    };

    let mut restarts: Vec<Instant> = Vec::new();
    loop {
        // 等主进程消失（每 2 秒查一次进程存活）
        while process_alive(target_pid) {
            #[cfg(windows)]
            {
                // 有信号 = 主进程正常退出
                if event_signaled(exit_event) {
                    return 0;
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        #[cfg(windows)]
        if event_signaled(exit_event) {
            return 0;
        }
        warn!(pid = target_pid, "主进程已消失，准备重启");

        // 循环崩溃防护：60 秒窗口内 >= CRASH_LIMIT 次就放弃
        let now = Instant::now();
        restarts.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        restarts.push(now);
        if restarts.len() >= CRASH_LIMIT as usize {
            warn!(
                "60 秒内连续崩溃 {} 次，看门狗放弃重启（避免循环崩溃）",
                restarts.len()
            );
            return 1;
        }

        // 重新拉起主程序（无 --watchdog 参数）
        match std::process::Command::new(&exe).spawn() {
            Ok(child) => {
                info!(pid = child.id(), "已重启主进程");
                // 新进程的存活由新看门狗监控（主进程启动时会 spawn 自己的看门狗）
                return 0;
            }
            Err(e) => {
                warn!("重启主进程失败: {e}");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(windows)]
fn event_signaled(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    if handle.is_null() {
        return false;
    }
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    const WAIT_OBJECT_0: u32 = 0;
    const TIMEOUT_MS: u32 = 0;
    unsafe { WaitForSingleObject(handle, TIMEOUT_MS) == WAIT_OBJECT_0 }
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    // /proc/<pid> 存在性检查（Linux/macOS 调试用路径，产品主战场是 Windows）
    std::path::PathBuf::from(format!("/proc/{pid}")).exists()
}
