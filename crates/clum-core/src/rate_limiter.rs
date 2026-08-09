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
