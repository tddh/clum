//! Full-jitter exponential backoff — 全项目统一退避原语。
//!
//! 公式：`sleep = random_between(0, min(cap, base * 2^attempt))`
//!
//! 相比纯指数退避（所有客户端以完全相同的时序重试 → 惊群效应），
//! full jitter 将每次等待时间在 `[0, 指数上限]` 区间内均匀随机化，
//! 多实例同时断连重连时天然分散，避免对服务器造成同步连接风暴。

use std::time::Duration;

/// Full-jitter 指数退避器。
///
/// 用法：
/// ```
/// use std::time::Duration;
/// use clum_core::backoff::FullJitterBackoff;
///
/// let mut b = FullJitterBackoff::new(Duration::from_millis(500), Duration::from_secs(30));
/// let delay = b.next_delay(); // 失败时等待 delay 后重试
/// b.reset();                  // 成功时重置退避
/// ```
pub struct FullJitterBackoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
    rng: fastrand::Rng,
}

impl FullJitterBackoff {
    /// 以系统熵源随机种子创建。
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
            rng: fastrand::Rng::new(),
        }
    }

    /// 以固定种子创建（测试用，结果可复现）。
    pub fn with_seed(base: Duration, cap: Duration, seed: u64) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
            rng: fastrand::Rng::with_seed(seed),
        }
    }

    /// 成功/连接正常结束后调用：attempt 归零，退避回到初始区间。
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// 返回本次应 sleep 的时长，内部 attempt += 1。
    ///
    /// 第 n 次调用（n 从 1 起）的上限为 `min(cap, base * 2^(n-1))`，
    /// 实际等待为 `[0, 上限]` 内的均匀随机值。
    pub fn next_delay(&mut self) -> Duration {
        // 指数上限 = base * 2^attempt，checked_shl + saturating 防溢出
        let base_ms = self.base.as_millis();
        let mul = 1u128.checked_shl(self.attempt).unwrap_or(u128::MAX);
        let exp_ms = base_ms.saturating_mul(mul);
        let max_ms = exp_ms.min(self.cap.as_millis());
        self.attempt = self.attempt.saturating_add(1);
        if max_ms == 0 {
            return Duration::ZERO;
        }
        let max_ms = max_ms.min(u128::from(u64::MAX)) as u64;
        Duration::from_millis(self.rng.u64(0..=max_ms))
    }

    /// 当前尝试次数（自上次 reset 起）。
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(500);
    const CAP: Duration = Duration::from_secs(30);

    /// 独立计算第 n 次调用的理论上限（不依赖实现内部逻辑）。
    fn expected_upper(attempt: u32) -> Duration {
        let mul = 1u128.checked_shl(attempt).unwrap_or(u128::MAX);
        let exp = BASE.as_millis().saturating_mul(mul);
        Duration::from_millis(exp.min(CAP.as_millis()).min(u128::from(u64::MAX)) as u64)
    }

    #[test]
    fn delay_within_upper_bound() {
        let mut b = FullJitterBackoff::with_seed(BASE, CAP, 42);
        for attempt in 0..10u32 {
            let upper = expected_upper(attempt);
            let d = b.next_delay();
            assert!(
                d <= upper,
                "attempt {attempt}: delay {d:?} exceeds upper bound {upper:?}"
            );
        }
    }

    #[test]
    fn delay_capped_at_cap() {
        let mut b = FullJitterBackoff::with_seed(BASE, CAP, 7);
        for _ in 0..20 {
            let d = b.next_delay();
            assert!(d <= CAP, "delay {d:?} exceeds cap {CAP:?}");
        }
    }

    #[test]
    fn no_overflow_at_huge_attempt() {
        let mut b = FullJitterBackoff::with_seed(BASE, CAP, 99);
        for _ in 0..5 {
            b.next_delay();
        }
        // 手动推进到接近 u32 上限，验证不 panic 且被 cap 约束
        b.attempt = u32::MAX - 1;
        let d = b.next_delay();
        assert!(d <= CAP, "delay {d:?} exceeds cap at huge attempt");
    }

    #[test]
    fn attempt_increments() {
        let mut b = FullJitterBackoff::with_seed(BASE, CAP, 1);
        assert_eq!(b.attempt(), 0);
        b.next_delay();
        assert_eq!(b.attempt(), 1);
        b.next_delay();
        b.next_delay();
        assert_eq!(b.attempt(), 3);
    }

    #[test]
    fn reset_returns_to_initial_range() {
        let mut b = FullJitterBackoff::with_seed(BASE, CAP, 13);
        for _ in 0..8 {
            b.next_delay(); // 推到 cap 区间
        }
        b.reset();
        assert_eq!(b.attempt(), 0);
        let d = b.next_delay();
        assert!(
            d <= BASE,
            "after reset delay {d:?} should be within base {BASE:?}"
        );
    }

    #[test]
    fn jitter_distribution_covers_range() {
        // 固定种子 + 大量采样：full jitter 应铺满 [0, cap] 区间。
        // 若实现返回固定值/无随机，此测试必然失败。
        let mut b = FullJitterBackoff::with_seed(BASE, CAP, 20240813);
        let mut max_seen = 0u64;
        let mut min_seen = u64::MAX;
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..5000 {
            let d = b.next_delay();
            let ms = d.as_millis() as u64;
            max_seen = max_seen.max(ms);
            min_seen = min_seen.min(ms);
            distinct.insert(ms);
        }
        // attempt 推进到 cap 区间后，采样应覆盖到接近 30s 上限
        assert!(
            max_seen >= 25_000,
            "max seen {max_seen}ms, expected >= 25000ms"
        );
        assert!(
            max_seen - min_seen >= 20_000,
            "spread {max_seen}-{min_seen} too narrow"
        );
        assert!(
            distinct.len() > 1000,
            "only {} distinct values",
            distinct.len()
        );
    }

    #[test]
    fn zero_delay_possible_at_low_attempt() {
        // full jitter 允许 0 延迟（服务器刚恢复时可立即重连）。
        // 固定种子下每次调用区间为 [0, 500ms]，5000 次采样碰不到 0 的概率
        // 约 (500/501)^5000 ≈ 0.005%，不会偶发失败。
        let mut b = FullJitterBackoff::with_seed(BASE, CAP, 555);
        let mut saw_zero = false;
        for _ in 0..5000 {
            b.reset();
            if b.next_delay().is_zero() {
                saw_zero = true;
                break;
            }
        }
        assert!(saw_zero, "expected at least one zero delay in 5000 samples");
    }
}
