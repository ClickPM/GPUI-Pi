//! 可注入时钟。
//!
//! R23 的 Idle TTL 与队列 aging 都是时间函数。如果直接读 [`std::time::Instant`]，
//! 验收就只能靠 `sleep` 去逼近，既慢又必然 flake（红线 4 明确禁止靠放宽标准通过）。
//! 因此调度器只通过本 trait 取时间：生产用 [`SystemClock`]，测试用 [`FakeClock`]
//! 精确推进到 TTL 前后各一格，断言完全确定。
//!
//! 时间用「自时钟原点起的 [`Duration`]」表示而不是 `Instant`：`Instant` 无法构造
//! 任意时刻，fake 实现根本没法写。

use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// 单调时钟。
///
/// 实现必须保证 [`Clock::now`] 单调不减 —— 调度器用差值判断等待时长与空闲时长，
/// 时间回退会让 aging 与 TTL 同时失效。
pub trait Clock: Send + Sync + 'static {
    /// 自时钟原点起经过的时间。
    fn now(&self) -> Duration;
}

/// 生产时钟：以创建时刻为原点包装 [`Instant`]。
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// 测试时钟：只有显式 [`FakeClock::advance`] 才会前进。
///
/// 用纳秒计数而不是 `Mutex<Duration>`，是为了让 `now()` 在被 reaper 之类的并发读者
/// 频繁调用时不需要抢锁。
#[derive(Debug, Default)]
pub struct FakeClock {
    nanos: AtomicU64,
    /// 仅用于 `advance` 的互斥，保证「读-加-写」不丢更新。
    advance_lock: Mutex<()>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// 推进时钟。返回推进后的时刻。
    pub fn advance(&self, delta: Duration) -> Duration {
        let _guard = self.advance_lock.lock().unwrap();
        let next = self
            .nanos
            .load(Ordering::Acquire)
            .saturating_add(u64::try_from(delta.as_nanos()).unwrap_or(u64::MAX));
        self.nanos.store(next, Ordering::Release);
        Duration::from_nanos(next)
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn fake_clock_only_moves_when_advanced() {
        let clock = FakeClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        assert_eq!(clock.now(), Duration::ZERO, "读时间本身不得推进时钟");
        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.now(), Duration::from_secs(5));
        clock.advance(Duration::from_millis(500));
        assert_eq!(clock.now(), Duration::from_millis(5500));
    }

    #[test]
    fn fake_clock_advances_are_not_lost_under_concurrency() {
        let clock = Arc::new(FakeClock::new());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let clock = Arc::clone(&clock);
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        clock.advance(Duration::from_millis(1));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(clock.now(), Duration::from_millis(800));
    }

    #[test]
    fn system_clock_is_monotonic() {
        let clock = SystemClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }
}
