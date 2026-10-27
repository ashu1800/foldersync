//! 带宽限速（提示词 M6 / 4.x：可选上传/下载上限）。
//!
//! 实现方式：令牌桶 + 传输后按需 sleep。
//! 不是对 iroh 内部的精细限流（那需要改 iroh 实现），而是**每传输完一批数据后
//! 补足配额**——对用户感知足够，实现只有几十行。
//!
//! ponytail: 这是"够用"的限速，不是精确整形。真要精确控制需要包一层
//! 带限速的 AsyncRead/AsyncWrite，等有真实需求再说。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// 一个方向的令牌桶。
#[derive(Debug)]
pub struct RateLimiter {
    /// 每秒允许的字节数（0 = 不限速）
    bytes_per_sec: u64,
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// 当前可用令牌
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// 创建限速器。`kbps == 0` 表示不限速。
    pub fn new(kbps: u32) -> Arc<Self> {
        Arc::new(Self {
            bytes_per_sec: kbps as u64 * 1024,
            state: Mutex::new(Bucket {
                tokens: kbps as f64 * 1024.0,
                last_refill: Instant::now(),
            }),
        })
    }

    /// 不限速的空实现。
    pub fn unlimited() -> Arc<Self> {
        Self::new(0)
    }

    pub fn is_limited(&self) -> bool {
        self.bytes_per_sec > 0
    }

    /// 消耗 `bytes` 字节的配额，不够就等到够。
    ///
    /// 调用放在传输之后：先干活后算账，避免限速拖慢小文件。
    pub async fn consume(&self, bytes: u64) {
        if self.bytes_per_sec == 0 || bytes == 0 {
            return;
        }

        let wait = {
            let mut b = self.state.lock().await;
            // 按流逝时间补令牌
            let elapsed = b.last_refill.elapsed().as_secs_f64();
            b.last_refill = Instant::now();
            b.tokens =
                (b.tokens + elapsed * self.bytes_per_sec as f64).min(self.bytes_per_sec as f64);

            if b.tokens >= bytes as f64 {
                b.tokens -= bytes as f64;
                None
            } else {
                let deficit = bytes as f64 - b.tokens;
                b.tokens = 0.0;
                let secs = deficit / self.bytes_per_sec as f64;
                Some(Duration::from_secs_f64(secs.min(5.0)))
            }
        };

        if let Some(d) = wait {
            tokio::time::sleep(d).await;
        }
    }
}

/// 上传/下载两个方向的限速器。
#[derive(Debug, Clone)]
pub struct Limits {
    pub upload: Arc<RateLimiter>,
    pub download: Arc<RateLimiter>,
}

impl Limits {
    pub fn new(upload_kbps: u32, download_kbps: u32) -> Self {
        Self {
            upload: RateLimiter::new(upload_kbps),
            download: RateLimiter::new(download_kbps),
        }
    }

    pub fn unlimited() -> Self {
        Self {
            upload: RateLimiter::unlimited(),
            download: RateLimiter::unlimited(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let limiter = RateLimiter::unlimited();
        assert!(!limiter.is_limited());
        let start = Instant::now();
        limiter.consume(10 * 1024 * 1024).await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "不限速不应等待"
        );
    }

    #[tokio::test]
    async fn small_transfer_within_bucket_is_instant() {
        // 100 KB/s，桶初始 100 KB
        let limiter = RateLimiter::new(100);
        assert!(limiter.is_limited());
        let start = Instant::now();
        limiter.consume(10 * 1024).await; // 10 KB，桶里够
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn over_budget_transfer_waits() {
        let limiter = RateLimiter::new(10); // 10 KB/s
        // 先耗尽桶
        limiter.consume(10 * 1024).await;
        // 再要 5 KB：理论需 0.5 秒
        let start = Instant::now();
        limiter.consume(5 * 1024).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(300),
            "应等待约 0.5 秒，实际 {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn zero_bytes_is_noop() {
        let limiter = RateLimiter::new(1);
        let start = Instant::now();
        limiter.consume(0).await;
        assert!(start.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn limits_new_builds_both_directions() {
        let l = Limits::new(100, 200);
        assert_eq!(l.upload.bytes_per_sec, 100 * 1024);
        assert_eq!(l.download.bytes_per_sec, 200 * 1024);
    }
}
