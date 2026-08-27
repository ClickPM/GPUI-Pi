//! 资源采样抽象与内存水位治理。
//!
//! # 为什么要有一层抽象
//!
//! 水位策略如果直接读真机内存，测试就只能"跑起来看看"——机器一忙结论就翻，撞上
//! CLAUDE.md 红线 4（同一验收项反复不过就得停下呼人）。因此采样口是可注入的
//! [`ResourceProbe`]，策略层用 [`FakeResourceProbe`] 做确定性验收，与 R23 的 fake clock
//! 同构：真机数值只用来**标定阈值**，不用来判定策略对不对。

use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use pi_rpc::{JobStats, SystemMemory};

/// 可注入的系统资源采样口。
pub trait ResourceProbe: Send + Sync {
    /// 系统内存采样。
    ///
    /// 返回 `None` 表示这次拿不到数据（平台不支持、查询失败）。**策略必须把"未知"
    /// 当成"没有压力"放行**：把采样失败当高压，会让一台查不到内存的机器上所有后台
    /// 会话永久排队，而且没有任何可见原因。
    fn system_memory(&self) -> Option<SystemMemory>;

    /// 一棵 Runtime 进程树的采样（活跃进程数 + 整树私有内存）。
    ///
    /// `measure` 是这棵树的真实取数口，由调用方（`SessionHandle`）提供。默认实现直接
    /// 用它；测试替身可以忽略它、返回预置值。
    ///
    /// 立项文档 § 七 R25 要求的是「**内存与进程数**采样经可注入抽象」，只把内存那一半
    /// 做成可注入是不够的 —— R26 起会有依赖进程数的策略，届时必须能脱离真机验收。
    fn process_tree(&self, measure: &dyn Fn() -> Option<JobStats>) -> Option<JobStats> {
        measure()
    }
}

/// 生产实现：走 `pi-rpc` 的平台 FFI。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemResourceProbe;

impl ResourceProbe for SystemResourceProbe {
    fn system_memory(&self) -> Option<SystemMemory> {
        pi_rpc::platform::system_memory().ok()
    }
}

/// 测试替身：采样值完全由测试写入，与真机内存曲线无关。
#[derive(Debug, Default)]
pub struct FakeResourceProbe {
    sample: Mutex<Option<SystemMemory>>,
    /// `None` 表示"没有预置值，按真实取数口走"；`Some(None)` 表示"模拟采样失败"。
    tree: Mutex<Option<Option<JobStats>>>,
}

impl FakeResourceProbe {
    pub fn new(total_bytes: u64, available_bytes: u64) -> Self {
        let probe = Self::default();
        probe.set_available(total_bytes, available_bytes);
        probe
    }

    /// 模拟"采样拿不到数据"。
    pub fn unavailable() -> Self {
        Self::default()
    }

    pub fn set_available(&self, total_bytes: u64, available_bytes: u64) {
        *self.sample.lock().unwrap() = Some(SystemMemory {
            total_bytes,
            available_bytes,
            load_percent: percent_used(total_bytes, available_bytes),
        });
    }

    pub fn set_unavailable(&self) {
        *self.sample.lock().unwrap() = None;
    }

    /// 预置进程树采样值，覆盖真实取数口。
    pub fn set_process_tree(&self, stats: JobStats) {
        *self.tree.lock().unwrap() = Some(Some(stats));
    }

    /// 模拟"进程树采样拿不到数据"（例如非 Windows 平台）。
    pub fn set_process_tree_unavailable(&self) {
        *self.tree.lock().unwrap() = Some(None);
    }
}

impl ResourceProbe for FakeResourceProbe {
    fn system_memory(&self) -> Option<SystemMemory> {
        *self.sample.lock().unwrap()
    }

    fn process_tree(&self, measure: &dyn Fn() -> Option<JobStats>) -> Option<JobStats> {
        // 没预置就老实走真实取数口：让"用 fake clock 但不关心进程树"的用例保持原行为。
        self.tree
            .lock()
            .unwrap()
            .map_or_else(measure, |preset| preset)
    }
}

fn percent_used(total_bytes: u64, available_bytes: u64) -> u32 {
    if total_bytes == 0 {
        return 0;
    }
    let used = total_bytes.saturating_sub(available_bytes);
    u32::try_from(used.saturating_mul(100) / total_bytes).unwrap_or(100)
}

/// 内存与进程数的有界参数。
///
/// 这些是**可配置初值**，不是产品契约：立项文档 § 七只规定"内存水位阻止后台启动并
/// 优先回收 IdleWarm"，具体数值须按 Windows 实测调整并记录依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimits {
    /// 系统可用内存低于该值即进入高水位。`0` 表示关闭水位策略。
    pub low_available_bytes: u64,
    /// 高水位解除阈值；必须不低于 `low_available_bytes`。
    ///
    /// 两个阈值分开是为了**迟滞**：单阈值会让系统在临界点上反复进出高压，
    /// 后台会话被起了又停、停了又起。
    pub resume_available_bytes: u64,
    /// 每个 Runtime 进程树的活跃进程数硬上限（Job Object）。
    ///
    /// 它覆盖 extension 自行 spawn 的子代理 —— Manager 的并发配额管不到那些进程，
    /// 但它们在 Runtime 的 job 内，这条硬上限对它们有效。
    pub max_processes_per_runtime: Option<u32>,
    /// 每个 Runtime 进程树的提交内存硬上限（Job Object）。
    pub runtime_memory_bytes: Option<u64>,
    /// 两次真实采样之间的最小间隔；`ZERO` 表示每次都重新采样（测试用）。
    pub sample_interval: Duration,
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            // 阈值按 R25 实测标定，不沿用立项文档那句"约 203MB"：
            // 本机（pi 0.84.2 / Windows 11）一个刚起好、静止的 Runtime 整树实测
            // **1 个进程、私有提交约 377MiB**（立项文档那个数字量纲不同，多半是工作集）。
            //
            // 低水位取 ≈ 4 倍单 Runtime 占用：低于它再起一个后台会话，剩余内存就掉到
            // 一个 Runtime 的量级以下了，那时候才动作已经晚了。
            low_available_bytes: 1536 * 1024 * 1024,
            // 解除阈值再高约 1GiB（≈ 又一个 Runtime 的余量），给迟滞留出足够宽的带，
            // 避免一次冷启动的抖动就把状态来回翻。
            resume_available_bytes: 2560 * 1024 * 1024,
            // pi + 若干工具 + extension 自行 spawn 的子代理；实测静止时只有 1 个进程，
            // 64 是"跑飞了"的护栏，不是日常配额。
            max_processes_per_runtime: Some(64),
            // 单棵树 4GiB ≈ 实测占用的 10 倍，只拦真正的失控，不误伤正常会话。
            runtime_memory_bytes: Some(4 * 1024 * 1024 * 1024),
            sample_interval: Duration::from_millis(500),
        }
    }
}

impl MemoryLimits {
    /// 把配置收进自洽区间；越界配置被收敛而不是被信任。
    pub(crate) fn sanitized(self) -> Self {
        Self {
            // 解除阈值不得低于进入阈值，否则迟滞变成"进得去出不来"。
            resume_available_bytes: self.resume_available_bytes.max(self.low_available_bytes),
            max_processes_per_runtime: self.max_processes_per_runtime.filter(|value| *value > 0),
            runtime_memory_bytes: self.runtime_memory_bytes.filter(|value| *value > 0),
            ..self
        }
    }

    /// 关闭水位策略（保留 Job Object 硬上限）。
    pub fn without_watermark(self) -> Self {
        Self {
            low_available_bytes: 0,
            resume_available_bytes: 0,
            ..self
        }
    }

    pub(crate) fn job_limits(self) -> pi_rpc::JobLimits {
        pi_rpc::JobLimits {
            max_active_processes: self.max_processes_per_runtime,
            job_memory_bytes: self.runtime_memory_bytes,
        }
    }

    fn watermark_enabled(self) -> bool {
        self.low_available_bytes > 0
    }
}

/// 一次可观测的水位状态，供 UI 与验收读取。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryPressureReport {
    /// 当前是否处于高水位。
    pub under_pressure: bool,
    /// 水位策略是否启用（`low_available_bytes == 0` 时关闭）。
    pub enabled: bool,
    /// 最近一次成功采样；`None` 表示还没采到过或平台不支持。
    pub last_sample: Option<SystemMemory>,
}

#[derive(Default)]
struct GovernorState {
    under_pressure: bool,
    sampled_at: Option<Duration>,
    last_sample: Option<SystemMemory>,
}

/// 带迟滞与采样节流的内存水位治理器。
pub(crate) struct MemoryGovernor {
    probe: Arc<dyn ResourceProbe>,
    limits: MemoryLimits,
    state: Mutex<GovernorState>,
}

impl MemoryGovernor {
    pub(crate) fn new(probe: Arc<dyn ResourceProbe>, limits: MemoryLimits) -> Self {
        Self {
            probe,
            limits,
            state: Mutex::new(GovernorState::default()),
        }
    }

    pub(crate) fn limits(&self) -> MemoryLimits {
        self.limits
    }

    /// 是否处于高水位。`now` 由 Manager 的注入时钟给出，采样节流因此也是确定性的。
    pub(crate) fn under_pressure(&self, now: Duration) -> bool {
        if !self.limits.watermark_enabled() {
            return false;
        }
        let mut state = self.state.lock().unwrap();
        let due = state.sampled_at.is_none_or(|last| {
            now.checked_sub(last)
                .is_none_or(|elapsed| elapsed >= self.limits.sample_interval)
        });
        if due {
            state.last_sample = self.probe.system_memory();
            state.sampled_at = Some(now);
        }
        let Some(memory) = state.last_sample else {
            // 采不到就放行。见 `ResourceProbe::system_memory` 的说明。
            state.under_pressure = false;
            return false;
        };
        state.under_pressure = if state.under_pressure {
            memory.available_bytes < self.limits.resume_available_bytes
        } else {
            memory.available_bytes < self.limits.low_available_bytes
        };
        state.under_pressure
    }

    pub(crate) fn report(&self) -> MemoryPressureReport {
        let state = self.state.lock().unwrap();
        MemoryPressureReport {
            under_pressure: state.under_pressure,
            enabled: self.limits.watermark_enabled(),
            last_sample: state.last_sample,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn limits() -> MemoryLimits {
        MemoryLimits {
            low_available_bytes: GIB,
            resume_available_bytes: 2 * GIB,
            sample_interval: Duration::ZERO,
            ..MemoryLimits::default()
        }
        .sanitized()
    }

    #[test]
    fn pressure_engages_below_low_and_only_releases_above_resume() {
        let probe = Arc::new(FakeResourceProbe::new(16 * GIB, 8 * GIB));
        let governor = MemoryGovernor::new(probe.clone(), limits());
        assert!(!governor.under_pressure(Duration::ZERO), "充裕时不应有压力");

        probe.set_available(16 * GIB, GIB / 2);
        assert!(
            governor.under_pressure(Duration::from_secs(1)),
            "低于低水位应进入高压"
        );

        // 迟滞：回到低水位之上、但仍低于解除阈值时，压力必须保持。
        probe.set_available(16 * GIB, GIB + GIB / 2);
        assert!(
            governor.under_pressure(Duration::from_secs(2)),
            "介于两个阈值之间时应保持高压，否则会在临界点上反复抖动"
        );

        probe.set_available(16 * GIB, 3 * GIB);
        assert!(
            !governor.under_pressure(Duration::from_secs(3)),
            "越过解除阈值应退出高压"
        );
    }

    #[test]
    fn unavailable_samples_never_create_pressure() {
        let probe = Arc::new(FakeResourceProbe::unavailable());
        let governor = MemoryGovernor::new(probe.clone(), limits());
        assert!(!governor.under_pressure(Duration::ZERO));

        // 先进高压，再让采样失效：不能卡在高压出不来。
        probe.set_available(16 * GIB, GIB / 2);
        assert!(governor.under_pressure(Duration::from_secs(1)));
        probe.set_unavailable();
        assert!(
            !governor.under_pressure(Duration::from_secs(2)),
            "采样失效时必须放行，而不是把上一轮的高压永久留住"
        );
    }

    #[test]
    fn sampling_is_throttled_by_the_injected_clock() {
        let probe = Arc::new(FakeResourceProbe::new(16 * GIB, 8 * GIB));
        let governor = MemoryGovernor::new(
            probe.clone(),
            MemoryLimits {
                sample_interval: Duration::from_millis(500),
                ..limits()
            },
        );
        assert!(!governor.under_pressure(Duration::ZERO));

        probe.set_available(16 * GIB, GIB / 2);
        assert!(
            !governor.under_pressure(Duration::from_millis(499)),
            "节流窗口内不应重新采样"
        );
        assert!(
            governor.under_pressure(Duration::from_millis(500)),
            "窗口到点后应看到新样本"
        );
    }

    #[test]
    fn watermark_can_be_disabled_without_losing_job_limits() {
        let limits = MemoryLimits::default().without_watermark().sanitized();
        let governor = MemoryGovernor::new(Arc::new(FakeResourceProbe::new(16 * GIB, 0)), limits);
        assert!(!governor.under_pressure(Duration::ZERO));
        assert!(!governor.report().enabled);
        assert_eq!(
            limits.job_limits().max_active_processes,
            MemoryLimits::default().max_processes_per_runtime,
            "关掉水位不应连 Job Object 硬上限一起关掉"
        );
    }

    #[test]
    fn resume_threshold_is_clamped_above_the_low_one() {
        let limits = MemoryLimits {
            low_available_bytes: 2 * GIB,
            resume_available_bytes: GIB,
            ..MemoryLimits::default()
        }
        .sanitized();
        assert_eq!(limits.resume_available_bytes, 2 * GIB);
    }

    #[test]
    fn zero_job_limits_are_treated_as_absent() {
        let limits = MemoryLimits {
            max_processes_per_runtime: Some(0),
            runtime_memory_bytes: Some(0),
            ..MemoryLimits::default()
        }
        .sanitized();
        assert_eq!(limits.job_limits().max_active_processes, None);
        assert_eq!(limits.job_limits().job_memory_bytes, None);
    }
}
