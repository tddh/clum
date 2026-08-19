//! Token-bucket rate limiter for bandwidth throttling.
//!
//! Zero external dependencies — built on `AtomicU64` + `tokio::time::sleep`.
//! Tokens are measured in **bytes**. A limiter with `rate = 0` is a no-op
//! (unlimited).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Bandwidth configuration for file transfers.
#[derive(Debug, Clone, Default)]
pub struct BandwidthConfig {
    /// Per-stream bandwidth limit in **bytes per second**. 0 = unlimited.
    pub per_stream: u64,
    /// Global (aggregate) bandwidth limit in **bytes per second**. 0 = unlimited.
    pub global: u64,
}

impl BandwidthConfig {
    pub fn from_mbps(upload_mbps: u64, download_mbps: u64) -> (Self, Self) {
        (
            Self {
                per_stream: upload_mbps * 125_000, // Mbps → bytes/s
                global: 0,
            },
            Self {
                per_stream: download_mbps * 125_000,
                global: 0,
            },
        )
    }
}

/// A token-bucket rate limiter.
///
/// Internally uses `AtomicU64` for the token counter so that multiple
/// concurrent streams can share a single global limiter without lock
/// contention on the fast path.
pub struct RateLimiter {
    /// Bytes-per-second refill rate. 0 = unlimited (no-op).
    rate: u64,
    /// Current available tokens (bytes). Saturates at `max_tokens`.
    tokens: AtomicU64,
    /// Upper bound for the token bucket (2 × rate, minimum 1 MB).
    max_tokens: u64,
    /// Last refill timestamp.
    last_refill: Mutex<Instant>,
}

impl RateLimiter {
    /// Create a new limiter. `bytes_per_sec = 0` means unlimited.
    pub fn new(bytes_per_sec: u64) -> Self {
        let max_tokens = if bytes_per_sec == 0 {
            0
        } else {
            (bytes_per_sec * 2).max(1024 * 1024) // at least 1 MB burst
        };
        Self {
            rate: bytes_per_sec,
            tokens: AtomicU64::new(max_tokens), // start full
            max_tokens,
            last_refill: Mutex::new(Instant::now()),
        }
    }

    /// Number of tokens to refill since the last call.
    async fn refill(&self, now: Instant) -> u64 {
        let mut last = self.last_refill.lock().await;
        let elapsed = now.duration_since(*last);
        *last = now;

        if elapsed.is_zero() {
            return 0;
        }

        // tokens = rate × elapsed_seconds
        let nanos = elapsed.as_nanos() as u128;
        let new_tokens = (self.rate as u128 * nanos / 1_000_000_000) as u64;
        if new_tokens == 0 {
            return 0;
        }

        let current = self.tokens.load(Ordering::Relaxed);
        let target = (current + new_tokens).min(self.max_tokens);
        // Only update if different — reduces contention
        if target != current {
            self.tokens.store(target, Ordering::Relaxed);
            new_tokens
        } else {
            0
        }
    }

    /// Consume `n` tokens. Returns the amount actually consumed.
    fn consume(&self, n: u64) -> u64 {
        let mut current = self.tokens.load(Ordering::Relaxed);
        loop {
            let taken = current.min(n);
            if taken == 0 {
                return 0;
            }
            match self.tokens.compare_exchange_weak(
                current,
                current - taken,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return taken,
                Err(actual) => current = actual,
            }
        }
    }

    /// Wait until at least `n` bytes of capacity are available, then
    /// consume them.  For an unlimited limiter this returns immediately.
    pub async fn acquire(&self, n: u64) {
        if self.rate == 0 || n == 0 {
            return;
        }

        let mut remaining = n;
        loop {
            let now = Instant::now();
            self.refill(now).await;

            let taken = self.consume(remaining);
            remaining -= taken;
            if remaining == 0 {
                return;
            }

            // Not enough tokens — sleep for the deficit
            let deficit = remaining;
            let sleep_ns = (deficit as u128 * 1_000_000_000 / self.rate as u128) as u64;
            if sleep_ns > 0 {
                tokio::time::sleep(Duration::from_nanos(sleep_ns.min(1_000_000_000))).await;
            }
        }
    }

    /// Check if this limiter is active (rate > 0).
    pub fn is_active(&self) -> bool {
        self.rate > 0
    }
}

/// A pair of limiters for a file transfer operation.
///
/// Both `per_stream` and `global` are consulted on every `acquire` so
/// that a single global bucket can throttle many concurrent streams.
#[derive(Clone)]
pub struct BandwidthLimiter {
    per_stream: Arc<RateLimiter>,
    global: Option<Arc<RateLimiter>>,
}

impl BandwidthLimiter {
    /// Create a limiter from a config. Returns `None` when neither limit is active.
    pub fn new(config: &BandwidthConfig, global: Option<Arc<RateLimiter>>) -> Option<Self> {
        let per = RateLimiter::new(config.per_stream);
        if !per.is_active() && global.as_ref().is_none_or(|g| !g.is_active()) {
            return None;
        }
        Some(Self {
            per_stream: Arc::new(per),
            global,
        })
    }

    /// Wait for `n` bytes of capacity across both the per-stream and
    /// global buckets.  Both must have enough tokens.
    pub async fn acquire(&self, n: u64) {
        self.per_stream.acquire(n).await;
        if let Some(ref g) = self.global {
            g.acquire(n).await;
        }
    }

    /// Create a new per-stream limiter sharing the same global bucket.
    pub fn clone_stream(&self) -> Self {
        Self {
            per_stream: Arc::new(RateLimiter::new(self.per_stream.rate)),
            global: self.global.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MB: u64 = 1024 * 1024;
    /// 1 MB/s 限速器——初始桶为 max(2MB, 1MB) = 2MB
    const RATE_1MB: u64 = 1024 * 1024;

    #[tokio::test]
    async fn rate_zero_is_unlimited() {
        let limiter = RateLimiter::new(0);
        let start = tokio::time::Instant::now();
        limiter.acquire(100 * MB).await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "rate=0 应无限速，实际耗时 {:?}",
            start.elapsed()
        );
        assert!(!limiter.is_active());
    }

    #[tokio::test]
    async fn acquire_zero_returns_immediately() {
        let limiter = RateLimiter::new(RATE_1MB);
        let start = tokio::time::Instant::now();
        limiter.acquire(0).await;
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn initial_bucket_is_full() {
        let limiter = RateLimiter::new(1_000);
        let start = tokio::time::Instant::now();
        limiter.acquire(MB).await;
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "初始满桶应瞬时通过 1MB（max_tokens=1MB 下限），实际耗时 {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn acquire_beyond_capacity_wait_for_refill() {
        let limiter = RateLimiter::new(RATE_1MB);
        limiter.acquire(2 * MB).await;
        let start = tokio::time::Instant::now();
        // 再要 1MB —— 桶为空，需等待 refill 积累 1MB ≈ 1s
        limiter.acquire(MB).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "1MB/s 下 1MB 缺口应等待 ~1s，实际 {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "等待时间异常偏长 {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn refill_accumulates_partial_tokens() {
        let limiter = RateLimiter::new(RATE_1MB);
        limiter.acquire(2 * MB).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let start = tokio::time::Instant::now();
        // 250ms 已积累 ~250KB；请求 240KB（>从空桶 refill 所需 240ms）应瞬时通过
        limiter.acquire(240 * 1024).await;
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "refill 后应已积累足够 token（空桶需 ~240ms），实际耗时 {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn concurrent_acquire_completes_without_deadlock() {
        let limiter = Arc::new(RateLimiter::new(100 * RATE_1MB));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let l = Arc::clone(&limiter);
            handles.push(tokio::spawn(async move {
                l.acquire(MB).await;
            }));
        }
        let start = tokio::time::Instant::now();
        for h in handles {
            h.await.expect("task panicked");
        }
        // 8×1MB 远小于 200MB 桶——全部瞬时通过。验证并发 CAS 竞争不死锁/活锁，
        // 不验证 token 守恒（那需要挤干桶后测总吞吐）
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "并发 acquire 不应被拖慢，实际 {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn from_mbps_converts_to_bytes_per_sec() {
        let (upload, download) = BandwidthConfig::from_mbps(10, 5);
        assert_eq!(upload.per_stream, 10 * 125_000);
        assert_eq!(download.per_stream, 5 * 125_000);
        assert_eq!(upload.global, 0);
        assert_eq!(download.global, 0);
    }

    #[test]
    fn bandwidth_limiter_none_when_both_inactive() {
        let config = BandwidthConfig {
            per_stream: 0,
            global: 0,
        };
        assert!(BandwidthLimiter::new(&config, None).is_none());
    }

    #[test]
    fn bandwidth_limiter_some_when_per_stream_active() {
        let config = BandwidthConfig {
            per_stream: RATE_1MB,
            global: 0,
        };
        let limiter = BandwidthLimiter::new(&config, None);
        assert!(limiter.is_some());
    }

    #[tokio::test]
    async fn bandwidth_limiter_global_bucket_is_shared_by_clone_stream() {
        let global = Arc::new(RateLimiter::new(RATE_1MB));
        let config = BandwidthConfig {
            per_stream: 100 * RATE_1MB,
            global: 0,
        };
        let stream_a = BandwidthLimiter::new(&config, Some(Arc::clone(&global))).unwrap();
        let stream_b = stream_a.clone_stream();
        stream_a.acquire(2 * MB).await;
        let start = tokio::time::Instant::now();
        stream_b.acquire(MB).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "全局桶共享后 stream_b 应等待 ~1s，实际 {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn bandwidth_limiter_per_stream_independent_after_clone() {
        let config = BandwidthConfig {
            per_stream: RATE_1MB,
            global: 0,
        };
        let stream_a = BandwidthLimiter::new(&config, None).unwrap();
        let stream_b = stream_a.clone_stream();
        stream_a.acquire(2 * MB).await;
        let start = tokio::time::Instant::now();
        stream_b.acquire(MB).await;
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "clone_stream 后 per_stream 应独立，实际耗时 {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn is_active_reflects_rate() {
        assert!(RateLimiter::new(1).is_active());
        assert!(!RateLimiter::new(0).is_active());
    }
}
