//! 会话调度器的纯逻辑内核。
//!
//! 本模块只处理**状态、槽位与顺序**，不碰进程、不碰 RPC、不依赖 GPUI，因此七态状态机、
//! RAII 运行槽、aging 公平队列与 Idle TTL 都能被确定性单测覆盖。真正的 spawn / `switch_session`
//! 由 [`crate::RuntimeManager`] 在本模块之上编排。
//!
//! 三个必须分清的计数（立项文档 § 三「会话与进程模型」）：
//!
//! - **Session**：可以有很多个，`Parked` 的 Session 零进程零线程，只留会话文件与轻量摘要；
//! - **用户运行槽**（`user_session_slots`）：同时处于 `Running` 的用户会话数上限；
//! - **常驻 Runtime 槽**（`total_runtime_slots`）：常驻 pi 进程数上限，**含 warm pool**——
//!   一个 `IdleWarm` 热进程仍然占着约 203MB，不把它算进同一份预算，「Resident Pi 不超上限」
//!   这条验收就是假的。

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

/// 稳定的会话身份。
///
/// 与 [`crate::RuntimeId`] 的区别是本轮的关键设计：`RuntimeId` 标识**运行时容器（进程宿主）**，
/// 一次 Resume 会换一个；`SessionId` 标识**用户会话本身**，跨 Park/Resume 恒定，
/// 因此才是 UI 侧会话态（草稿、附件、滚动位置）该挂靠的主键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub(crate) u64);

impl SessionId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// 调度实体状态（立项文档 § 七 R23 钉死的七态）。
///
/// 前六态描述 **Session**；[`SchedulerState::IdleWarm`] 描述 **池内热进程**——
/// 它不属于任何 Session（"可被任意 Session 复用的热进程，不是某个 Session 的预热副本"），
/// 但与 Session 共享同一份常驻 Runtime 预算，所以必须在同一个枚举里被调度器看见。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SchedulerState {
    /// 已登记但没有进程：只保留会话文件与轻量摘要。
    Parked,
    /// 想运行但没抢到运行槽，正在公平队列里等待。
    Queued,
    /// 已占到运行槽，正在冷启动或从 warm pool 接管。
    Starting,
    /// 池内空闲热进程，等待被任意 Session 复用；受 Idle TTL 回收。
    IdleWarm,
    /// 正常运行中。
    Running,
    /// 正在让出进程（Park 或 Stop 的中间态）。
    Stopping,
    /// 启动失败或运行中崩溃；再次 `request_run` 可重试。
    Failed,
}

impl SchedulerState {
    /// 该状态是否占着一个常驻 pi 进程。
    pub const fn holds_process(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::IdleWarm | Self::Running | Self::Stopping
        )
    }

    /// 合法状态转移表。
    ///
    /// 显式列出而不是「随便赋值」，是因为槽位归还挂在状态转移上：一次静默的
    /// `Running -> Parked` 跳过 `Stopping`，就会漏掉进程回收，运行槽从此泄漏。
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Parked, Self::Queued)
                | (Self::Parked, Self::Starting)
                | (Self::Queued, Self::Starting)
                | (Self::Queued, Self::Parked)
                | (Self::Starting, Self::Running)
                | (Self::Starting, Self::Failed)
                | (Self::Starting, Self::Stopping)
                | (Self::Running, Self::Stopping)
                | (Self::Running, Self::Failed)
                | (Self::Stopping, Self::Parked)
                | (Self::Stopping, Self::Failed)
                | (Self::Failed, Self::Queued)
                | (Self::Failed, Self::Starting)
                | (Self::Failed, Self::Parked)
                // 热进程只在池内出现：进池由 Runtime 侧构造，出池即被接管或回收。
                | (Self::IdleWarm, Self::Starting)
                | (Self::IdleWarm, Self::Stopping)
        )
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Parked => "Parked",
            Self::Queued => "Queued",
            Self::Starting => "Starting",
            Self::IdleWarm => "IdleWarm",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Failed => "Failed",
        }
    }
}

/// 非法状态转移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalTransition {
    pub from: SchedulerState,
    pub to: SchedulerState,
}

impl std::fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "非法状态转移：{} -> {}",
            self.from.label(),
            self.to.label()
        )
    }
}

/// 调度优先级；数值越小越优先。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(pub u8);

impl Priority {
    /// 用户当前正在看的会话。
    pub const FOREGROUND: Self = Self(0);
    /// 后台会话（R24 起真正用得上）。
    pub const BACKGROUND: Self = Self(1);
}

impl Default for Priority {
    fn default() -> Self {
        Self::FOREGROUND
    }
}

/// 调度器的有界参数。
///
/// 初值取自立项文档 § 七阶段 E「用户会话并发 2、总 Runtime 3、Warm Idle 1、Idle TTL 3 分钟」；
/// 队列容量与 aging 步长立项文档未规定，由本轮选定并记录在任务卡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerLimits {
    /// 同时 `Running` 的用户会话数上限。
    pub user_session_slots: usize,
    /// 常驻 pi 进程数上限（用户会话 + warm pool）。
    pub total_runtime_slots: usize,
    /// warm pool 容量。
    pub warm_idle: usize,
    /// 热进程空闲多久后被回收。
    pub idle_ttl: Duration,
    /// 等待队列容量；满了直接拒绝，不排队到内存耗尽。
    pub queue_capacity: usize,
    /// 每等待一个步长，有效优先级提升一级；[`Duration::ZERO`] 表示关闭 aging。
    pub aging_step: Duration,
}

impl Default for SchedulerLimits {
    fn default() -> Self {
        Self {
            user_session_slots: 2,
            total_runtime_slots: 3,
            warm_idle: 1,
            idle_ttl: Duration::from_secs(180),
            // 队列只存轻量条目（id + 优先级 + 时刻），64 足够覆盖「一次性打开一整个项目的会话列表」，
            // 又不至于让用户排到看不见尽头。
            queue_capacity: 64,
            // 5s：比一次冷启动（实测秒级）长一档，避免刚入队就被 aging 抬到抢占前台。
            aging_step: Duration::from_secs(5),
        }
    }
}

impl SchedulerLimits {
    /// 把配置收进自洽区间；越界配置被收敛而不是被信任。
    pub(crate) fn sanitized(self) -> Self {
        let user_session_slots = self.user_session_slots.max(1);
        // 常驻上限不得小于用户并发，否则用户槽永远抢不到进程，调度器直接死锁。
        let total_runtime_slots = self.total_runtime_slots.max(user_session_slots);
        Self {
            user_session_slots,
            total_runtime_slots,
            warm_idle: self.warm_idle.min(total_runtime_slots),
            idle_ttl: self.idle_ttl,
            queue_capacity: self.queue_capacity.max(1),
            aging_step: self.aging_step,
        }
    }
}

/// 槽位种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// 用户会话运行槽：同时占用 1 个用户槽和 1 个常驻槽。
    UserSession,
    /// 热进程槽：只占 1 个常驻槽。
    Warm,
}

/// 槽位占用快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotCounts {
    pub user: usize,
    pub resident: usize,
}

#[derive(Debug, Default)]
struct SlotState {
    counts: SlotCounts,
}

#[derive(Debug)]
pub(crate) struct SlotPoolInner {
    limits: SchedulerLimits,
    state: Mutex<SlotState>,
}

impl SlotPoolInner {
    fn release(&self, kind: SlotKind) {
        let mut state = self.state.lock().unwrap();
        state.counts.resident = state.counts.resident.saturating_sub(1);
        if kind == SlotKind::UserSession {
            state.counts.user = state.counts.user.saturating_sub(1);
        }
    }

    fn convert_user_to_warm(&self) {
        let mut state = self.state.lock().unwrap();
        state.counts.user = state.counts.user.saturating_sub(1);
    }

    fn try_convert_warm_to_user(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.counts.user >= self.limits.user_session_slots {
            return false;
        }
        state.counts.user += 1;
        true
    }
}

/// 运行槽池。
#[derive(Debug, Clone)]
pub struct SlotPool {
    inner: Arc<SlotPoolInner>,
}

impl SlotPool {
    pub fn new(limits: SchedulerLimits) -> Self {
        Self {
            inner: Arc::new(SlotPoolInner {
                limits: limits.sanitized(),
                state: Mutex::new(SlotState::default()),
            }),
        }
    }

    pub fn limits(&self) -> SchedulerLimits {
        self.inner.limits
    }

    pub fn counts(&self) -> SlotCounts {
        self.inner.state.lock().unwrap().counts
    }

    /// 申请一个用户会话运行槽。抢不到返回 `None`，调用方应转入等待队列。
    pub fn try_acquire_user(&self) -> Option<SlotLease> {
        let mut state = self.inner.state.lock().unwrap();
        if state.counts.user >= self.inner.limits.user_session_slots
            || state.counts.resident >= self.inner.limits.total_runtime_slots
        {
            return None;
        }
        state.counts.user += 1;
        state.counts.resident += 1;
        drop(state);
        Some(SlotLease::new(
            Arc::clone(&self.inner),
            SlotKind::UserSession,
        ))
    }

    /// 申请一个热进程槽（预热路径用；Park 降级走 [`SlotLease::downgrade_to_warm`]）。
    pub fn try_acquire_warm(&self) -> Option<SlotLease> {
        let mut state = self.inner.state.lock().unwrap();
        if state.counts.resident >= self.inner.limits.total_runtime_slots {
            return None;
        }
        state.counts.resident += 1;
        drop(state);
        Some(SlotLease::new(Arc::clone(&self.inner), SlotKind::Warm))
    }
}

#[derive(Debug)]
struct LeaseInner {
    pool: Arc<SlotPoolInner>,
    kind: SlotKind,
}

/// 运行槽的 RAII 凭证。
///
/// 归还只发生在 `Drop`，因此**任何**退出路径都会归还：正常收尾、`?` 提前返回、
/// 甚至 worker panic 展开。R21/R22 的教训是「显式归还」总会漏掉一条分支，
/// 而漏掉一次就意味着一个运行槽永久消失。
#[derive(Debug)]
pub struct SlotLease {
    inner: Option<LeaseInner>,
}

impl SlotLease {
    fn new(pool: Arc<SlotPoolInner>, kind: SlotKind) -> Self {
        Self {
            inner: Some(LeaseInner { pool, kind }),
        }
    }

    pub fn kind(&self) -> SlotKind {
        self.inner
            .as_ref()
            .map(|inner| inner.kind)
            .unwrap_or(SlotKind::Warm)
    }

    /// Park 到 warm pool：交还用户会话槽，保留常驻槽。
    ///
    /// 必须是一次原子降级而不是「先 drop 再 acquire」——后者中间有个空窗，
    /// 别的 Session 会抢走这个常驻槽，热进程随即无处安放。
    pub fn downgrade_to_warm(mut self) -> Self {
        let Some(inner) = self.inner.take() else {
            return Self { inner: None };
        };
        if inner.kind == SlotKind::Warm {
            return Self { inner: Some(inner) };
        }
        inner.pool.convert_user_to_warm();
        Self {
            inner: Some(LeaseInner {
                pool: inner.pool,
                kind: SlotKind::Warm,
            }),
        }
    }

    /// 热进程被某个 Session 接管：占用一个用户会话槽，常驻槽原样保留。
    ///
    /// 同样必须是原子升级——先 drop 再 acquire 会在中间空窗里把常驻槽让给别人，
    /// 而这个热进程明明已经在跑着。用户并发已满时返回 `Err`，**并把 lease 原样交还**，
    /// 调用方可以安全地把热进程放回池里。
    pub fn upgrade_to_user(mut self) -> Result<Self, Self> {
        let Some(inner) = self.inner.take() else {
            return Err(Self { inner: None });
        };
        if inner.kind == SlotKind::UserSession {
            return Ok(Self { inner: Some(inner) });
        }
        if inner.pool.try_convert_warm_to_user() {
            Ok(Self {
                inner: Some(LeaseInner {
                    pool: inner.pool,
                    kind: SlotKind::UserSession,
                }),
            })
        } else {
            Err(Self { inner: Some(inner) })
        }
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.pool.release(inner.kind);
        }
    }
}

/// 队列已满。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFull {
    pub capacity: usize,
}

impl std::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "等待队列已满（上限 {}），请先关闭或停止一些会话",
            self.capacity
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueEntry {
    session: SessionId,
    priority: Priority,
    enqueued_at: Duration,
    /// 入队序号，用于同等有效优先级下的 FIFO 破平。
    sequence: u64,
}

/// aging 收益的上限（步数）。
///
/// 没有上限时，一条等了很久的条目其有效优先级会一路跑到 `i64` 深处；封顶既避免溢出，
/// 也让「排在最前」这件事一旦达成就不再继续膨胀。
const MAX_AGING_STEPS: i64 = 1024;

/// 带 aging 的有界公平队列。
///
/// 纯优先级队列会饿死低优先级条目：只要前台会话不断新建，后台会话永远排在后面。
/// aging 让等待时间按 `aging_step` 折算成优先级提升，等得足够久的低优先级条目
/// **一定**会越过后来的高优先级条目。
#[derive(Debug)]
pub struct WaitQueue {
    capacity: usize,
    aging_step: Duration,
    entries: VecDeque<QueueEntry>,
    next_sequence: u64,
}

impl WaitQueue {
    pub fn new(limits: SchedulerLimits) -> Self {
        let limits = limits.sanitized();
        Self {
            capacity: limits.queue_capacity,
            aging_step: limits.aging_step,
            entries: VecDeque::new(),
            next_sequence: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn contains(&self, session: SessionId) -> bool {
        self.entries.iter().any(|entry| entry.session == session)
    }

    /// 入队。已在队列中的 Session 只更新优先级，不重复排队、也不刷新等待起点
    /// （否则反复点击就能无限刷新 aging，把公平性玩坏）。
    pub fn push(
        &mut self,
        session: SessionId,
        priority: Priority,
        now: Duration,
    ) -> Result<(), QueueFull> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.session == session)
        {
            entry.priority = entry.priority.min(priority);
            return Ok(());
        }
        if self.entries.len() >= self.capacity {
            return Err(QueueFull {
                capacity: self.capacity,
            });
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.entries.push_back(QueueEntry {
            session,
            priority,
            enqueued_at: now,
            sequence,
        });
        Ok(())
    }

    pub fn remove(&mut self, session: SessionId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.session != session);
        self.entries.len() != before
    }

    /// 有效优先级：数值越小越先被调度。
    fn effective_priority(&self, entry: &QueueEntry, now: Duration) -> i64 {
        let waited = now.saturating_sub(entry.enqueued_at);
        let steps = if self.aging_step.is_zero() {
            0
        } else {
            i64::try_from(waited.as_nanos() / self.aging_step.as_nanos()).unwrap_or(MAX_AGING_STEPS)
        };
        i64::from(entry.priority.0) - steps.min(MAX_AGING_STEPS)
    }

    /// 取出当前最该被调度的条目。
    pub fn pop_next(&mut self, now: Duration) -> Option<SessionId> {
        let best = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| (self.effective_priority(entry, now), entry.sequence))
            .map(|(index, _)| index)?;
        self.entries.remove(best).map(|entry| entry.session)
    }

    /// 只看不取，供报表与测试使用。
    pub fn peek_next(&self, now: Duration) -> Option<SessionId> {
        self.entries
            .iter()
            .min_by_key(|entry| (self.effective_priority(entry, now), entry.sequence))
            .map(|entry| entry.session)
    }
}

/// 调度器对外可观测的计数。
///
/// `warm_resumes` / `cold_starts` 是「Resume 优先复用 `switch_session`」这条验收的
/// 唯一客观判据：两条路径各走一次，计数必须各自 +1 且互不串台。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchedulerReport {
    /// 当前常驻 pi 进程数（Starting / Running / Stopping 的用户会话 + IdleWarm 热进程）。
    pub resident_pi: usize,
    pub parked: usize,
    pub queued: usize,
    pub starting: usize,
    pub running: usize,
    pub stopping: usize,
    pub failed: usize,
    pub warm: usize,
    /// 已从会话上摘下、但进程可能还没退干净的运行槽数。
    ///
    /// 它们照样占着常驻名额 —— 这段拆除窗口里谎报「还有余量」就会让新会话在旧进程
    /// 还活着时补位。
    pub draining: usize,
    /// 累计冷启动次数（无可复用热进程时的回退路径）。
    pub cold_starts: u64,
    /// 累计热进程复用次数（`switch_session` 首选路径）。
    pub warm_resumes: u64,
    /// 累计 Park 到 warm pool 的次数。
    pub warm_parks: u64,
    /// 累计被 Idle TTL 回收的热进程数。
    pub idle_reaped: u64,
    /// 当前槽位占用。
    pub slots: SlotCounts,
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: SessionId = SessionId(1);
    const S2: SessionId = SessionId(2);
    const S3: SessionId = SessionId(3);

    #[test]
    fn legal_and_illegal_transitions_are_explicit() {
        assert!(SchedulerState::Parked.can_transition_to(SchedulerState::Queued));
        assert!(SchedulerState::Queued.can_transition_to(SchedulerState::Starting));
        assert!(SchedulerState::Starting.can_transition_to(SchedulerState::Running));
        assert!(SchedulerState::Running.can_transition_to(SchedulerState::Stopping));
        assert!(SchedulerState::Stopping.can_transition_to(SchedulerState::Parked));
        assert!(SchedulerState::Running.can_transition_to(SchedulerState::Failed));
        assert!(SchedulerState::Failed.can_transition_to(SchedulerState::Starting));
        assert!(SchedulerState::IdleWarm.can_transition_to(SchedulerState::Starting));

        // 跳过 Stopping 直接回 Parked 会漏掉进程回收 —— 必须被拒绝。
        assert!(!SchedulerState::Running.can_transition_to(SchedulerState::Parked));
        // Session 永远不会变成池内热进程。
        assert!(!SchedulerState::Running.can_transition_to(SchedulerState::IdleWarm));
        assert!(!SchedulerState::Parked.can_transition_to(SchedulerState::Running));
        assert!(!SchedulerState::Queued.can_transition_to(SchedulerState::Running));
    }

    #[test]
    fn only_process_holding_states_count_as_resident() {
        assert!(SchedulerState::Starting.holds_process());
        assert!(SchedulerState::Running.holds_process());
        assert!(SchedulerState::Stopping.holds_process());
        assert!(SchedulerState::IdleWarm.holds_process());
        assert!(!SchedulerState::Parked.holds_process());
        assert!(!SchedulerState::Queued.holds_process());
        assert!(!SchedulerState::Failed.holds_process());
    }

    fn limits() -> SchedulerLimits {
        SchedulerLimits {
            user_session_slots: 2,
            total_runtime_slots: 3,
            warm_idle: 1,
            idle_ttl: Duration::from_secs(180),
            queue_capacity: 4,
            aging_step: Duration::from_secs(5),
        }
    }

    #[test]
    fn user_slots_and_resident_slots_are_enforced_independently() {
        let pool = SlotPool::new(limits());
        let a = pool.try_acquire_user().expect("first user slot");
        let b = pool.try_acquire_user().expect("second user slot");
        assert_eq!(
            pool.counts(),
            SlotCounts {
                user: 2,
                resident: 2
            }
        );
        // 用户并发已满：第三个用户会话必须排队，即使常驻槽还剩一个。
        assert!(pool.try_acquire_user().is_none());
        // 剩下的那个常驻槽仍可用于预热。
        let warm = pool.try_acquire_warm().expect("warm slot uses the spare");
        assert!(pool.try_acquire_warm().is_none(), "常驻上限必须硬成立");
        drop((a, b, warm));
        assert_eq!(pool.counts(), SlotCounts::default());
    }

    #[test]
    fn lease_returns_the_slot_on_every_exit_path_including_panic() {
        let pool = SlotPool::new(limits());
        {
            let _lease = pool.try_acquire_user().expect("slot");
            assert_eq!(pool.counts().user, 1);
        }
        assert_eq!(pool.counts(), SlotCounts::default(), "作用域结束即归还");

        let panicking = std::panic::catch_unwind({
            let pool = pool.clone();
            move || {
                let _lease = pool.try_acquire_user().expect("slot");
                panic!("worker 崩了");
            }
        });
        assert!(panicking.is_err());
        assert_eq!(
            pool.counts(),
            SlotCounts::default(),
            "panic 展开也必须归还运行槽"
        );
    }

    #[test]
    fn downgrading_a_lease_frees_the_user_slot_but_keeps_the_process_slot() {
        let pool = SlotPool::new(limits());
        let lease = pool.try_acquire_user().expect("slot");
        let warm = lease.downgrade_to_warm();
        assert_eq!(warm.kind(), SlotKind::Warm);
        assert_eq!(
            pool.counts(),
            SlotCounts {
                user: 0,
                resident: 1
            },
            "热进程仍然占着常驻名额"
        );
        drop(warm);
        assert_eq!(pool.counts(), SlotCounts::default());
    }

    #[test]
    fn upgrading_a_warm_lease_takes_a_user_slot_without_touching_the_process_slot() {
        let pool = SlotPool::new(limits());
        let warm = pool.try_acquire_user().expect("slot").downgrade_to_warm();
        let user = warm.upgrade_to_user().expect("user slot available");
        assert_eq!(user.kind(), SlotKind::UserSession);
        assert_eq!(
            pool.counts(),
            SlotCounts {
                user: 1,
                resident: 1
            },
            "接管热进程不得额外占一个常驻名额"
        );
        drop(user);
        assert_eq!(pool.counts(), SlotCounts::default());
    }

    #[test]
    fn a_failed_upgrade_hands_the_warm_lease_back_intact() {
        let pool = SlotPool::new(SchedulerLimits {
            user_session_slots: 1,
            ..limits()
        });
        let warm = pool
            .try_acquire_warm()
            .expect("warm slot")
            .downgrade_to_warm();
        let _busy = pool.try_acquire_user().expect("the only user slot");
        let returned = warm.upgrade_to_user().expect_err("user slots exhausted");
        assert_eq!(returned.kind(), SlotKind::Warm);
        assert_eq!(
            pool.counts(),
            SlotCounts {
                user: 1,
                resident: 2
            },
            "升级失败不得吞掉常驻槽"
        );
        drop(returned);
        assert_eq!(
            pool.counts(),
            SlotCounts {
                user: 1,
                resident: 1
            }
        );
    }

    #[test]
    fn queue_is_bounded_and_rejects_instead_of_growing() {
        let mut queue = WaitQueue::new(limits());
        for id in 1..=4 {
            queue
                .push(SessionId(id), Priority::FOREGROUND, Duration::ZERO)
                .expect("within capacity");
        }
        let full = queue
            .push(SessionId(5), Priority::FOREGROUND, Duration::ZERO)
            .expect_err("capacity reached");
        assert_eq!(full, QueueFull { capacity: 4 });
        assert_eq!(queue.len(), 4, "拒绝之后队列长度不得变化");
    }

    #[test]
    fn re_pushing_a_queued_session_does_not_refresh_its_aging() {
        let mut queue = WaitQueue::new(limits());
        queue
            .push(S1, Priority::BACKGROUND, Duration::ZERO)
            .expect("queued");
        queue
            .push(S2, Priority::FOREGROUND, Duration::from_secs(9))
            .expect("queued");
        // 反复请求同一个会话：优先级可以升，但等待起点不变。
        queue
            .push(S1, Priority::BACKGROUND, Duration::from_secs(9))
            .expect("already queued");
        assert_eq!(queue.len(), 2);
        // t=10s：S1 等了 10s（2 个 aging 步）有效优先级 1-2 = -1；S2 等了 1s，有效优先级 0。
        assert_eq!(queue.peek_next(Duration::from_secs(10)), Some(S1));
    }

    #[test]
    fn aging_lets_a_long_waiting_background_session_overtake_a_fresh_foreground_one() {
        let mut queue = WaitQueue::new(limits());
        queue
            .push(S1, Priority::BACKGROUND, Duration::ZERO)
            .expect("queued");
        queue
            .push(S2, Priority::FOREGROUND, Duration::from_secs(10))
            .expect("queued");

        // 刚入队时前台优先：S1 有效优先级 1，S2 有效优先级 0。
        assert_eq!(queue.peek_next(Duration::from_secs(10)), Some(S1));
        // 上面这一步 S1 已经等了 10s（2 步 aging），有效优先级 -1，已经反超。
        // 把 aging 关掉重跑同一场景，验证「反超确实是 aging 造成的」。
        let mut no_aging = WaitQueue::new(SchedulerLimits {
            aging_step: Duration::ZERO,
            ..limits()
        });
        no_aging
            .push(S1, Priority::BACKGROUND, Duration::ZERO)
            .expect("queued");
        no_aging
            .push(S2, Priority::FOREGROUND, Duration::from_secs(10))
            .expect("queued");
        assert_eq!(
            no_aging.peek_next(Duration::from_secs(10)),
            Some(S2),
            "关闭 aging 后必须退化为纯优先级队列"
        );
        assert_eq!(
            no_aging.peek_next(Duration::from_secs(100_000)),
            Some(S2),
            "关闭 aging 后等多久都不会反超"
        );
    }

    #[test]
    fn equal_effective_priority_falls_back_to_fifo() {
        let mut queue = WaitQueue::new(limits());
        queue
            .push(S1, Priority::FOREGROUND, Duration::ZERO)
            .expect("queued");
        queue
            .push(S2, Priority::FOREGROUND, Duration::ZERO)
            .expect("queued");
        queue
            .push(S3, Priority::FOREGROUND, Duration::ZERO)
            .expect("queued");
        assert_eq!(queue.pop_next(Duration::from_secs(1)), Some(S1));
        assert_eq!(queue.pop_next(Duration::from_secs(1)), Some(S2));
        assert_eq!(queue.pop_next(Duration::from_secs(1)), Some(S3));
        assert_eq!(queue.pop_next(Duration::from_secs(1)), None);
    }

    #[test]
    fn removing_a_queued_session_is_idempotent() {
        let mut queue = WaitQueue::new(limits());
        queue
            .push(S1, Priority::FOREGROUND, Duration::ZERO)
            .expect("queued");
        assert!(queue.contains(S1));
        assert!(queue.remove(S1));
        assert!(!queue.remove(S1));
        assert!(queue.is_empty());
    }

    #[test]
    fn limits_are_sanitized_into_a_self_consistent_range() {
        let sane = SchedulerLimits {
            user_session_slots: 0,
            total_runtime_slots: 0,
            warm_idle: 99,
            idle_ttl: Duration::from_secs(1),
            queue_capacity: 0,
            aging_step: Duration::ZERO,
        }
        .sanitized();
        assert_eq!(sane.user_session_slots, 1);
        assert_eq!(
            sane.total_runtime_slots, 1,
            "常驻上限不得小于用户并发，否则永远抢不到进程"
        );
        assert_eq!(sane.warm_idle, 1);
        assert_eq!(sane.queue_capacity, 1);
    }
}
