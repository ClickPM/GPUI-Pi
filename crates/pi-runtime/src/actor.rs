//! 单 Runtime 的有界命令 Actor。
//!
//! R21 里每一次 `dispatch` / `refresh_metadata` / `request_control` / 校准都新建一个 OS
//! 线程，线程数随请求次数无上限增长。R22 改成**固定 worker 数 + 有界队列**：
//!
//! - **普通通道**（默认容量 32）承载提交、元数据刷新与落盘校准；
//! - **控制通道**独立且更浅，承载 Abort、会话控制与工具预设重启 —— 普通通道被长时间
//!   交互请求占满时，用户仍然能停止或切换会话；
//! - 队列满时投递**立即失败**并由调用方向上报，绝不排队到内存耗尽，也绝不悄悄丢弃。

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};

/// pi-runtime 进程内累计创建过的线程数。
///
/// 只用于验证「连续 follow-up 不新增无界 OS 线程」这一验收项：改造后它在稳态下
/// 必须与请求次数无关。
static SPAWNED_THREADS: AtomicU64 = AtomicU64::new(0);

/// 累计线程创建计数快照。
pub fn spawned_thread_count() -> u64 {
    SPAWNED_THREADS.load(Ordering::Acquire)
}

/// 统一的线程创建入口，保证每一处 spawn 都被计数。
pub(crate) fn spawn_named<F>(name: String, body: F) -> JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    SPAWNED_THREADS.fetch_add(1, Ordering::AcqRel);
    thread::Builder::new()
        .name(name)
        .spawn(body)
        .expect("failed to spawn pi-runtime thread")
}

/// 作业投递失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// 队列已达容量上限；调用方必须把失败暴露给用户，不得静默丢弃。
    Full { channel: Channel, capacity: usize },
    /// Runtime 已停止。
    Closed,
}

impl QueueError {
    pub fn message(self) -> String {
        match self {
            // 两条通道容量不同，文案必须点明是哪一条，否则用户看到「上限 8」会以为
            // 是 32 的命令队列出了问题。
            Self::Full { channel, capacity } => format!(
                "{}已满（上限 {capacity}），请等待当前请求完成后重试",
                channel.display_name()
            ),
            Self::Closed => "runtime 已停止".to_owned(),
        }
    }
}

/// 命令通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// 普通命令：提交、元数据刷新、落盘校准。
    Command,
    /// 控制命令：Abort、会话控制、工具预设重启。
    Control,
}

impl Channel {
    const fn display_name(self) -> &'static str {
        match self {
            Self::Command => "命令队列",
            Self::Control => "控制队列",
        }
    }
}

/// 队列内可合并的作业 key —— 同 key 只保留最新一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKey {
    /// 元数据刷新（commands + controls）是纯粹的「取最新」操作。
    Metadata,
    /// 会话落盘校准同理：排队多份旧校准没有意义。
    Calibration,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Actor 的有界参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorLimits {
    pub command_capacity: usize,
    pub control_capacity: usize,
    pub command_workers: usize,
    pub control_workers: usize,
}

impl Default for ActorLimits {
    fn default() -> Self {
        Self {
            // 立项文档 § 七阶段 E 的初始配置：单 Runtime command queue 32。
            command_capacity: 32,
            control_capacity: 8,
            // 两个普通 worker 让「长时间交互提交」不至于独占整条普通通道。
            // 控制通道也给两个：会话切换 / fork / HTML 导出可能一跑就是几十秒，
            // 单 worker 会让 Abort 排在它们后面 —— 而 Abort 恰恰是最不能等的操作。
            command_workers: 2,
            control_workers: 2,
        }
    }
}

impl ActorLimits {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            command_capacity: self.command_capacity.max(1),
            control_capacity: self.control_capacity.max(1),
            command_workers: self.command_workers.max(1),
            control_workers: self.control_workers.max(1),
        }
    }
}

struct QueueState {
    jobs: VecDeque<(Option<JobKey>, Job)>,
    closed: bool,
}

struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
    channel: Channel,
    capacity: usize,
}

impl Queue {
    fn new(channel: Channel, capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                jobs: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            channel,
            capacity,
        }
    }

    fn push(&self, key: Option<JobKey>, job: Job) -> Result<(), QueueError> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(QueueError::Closed);
        }
        if let Some(key) = key
            && let Some(slot) = state
                .jobs
                .iter_mut()
                .find(|(queued, _)| *queued == Some(key))
        {
            // 同 key latest-only：覆盖排队中的旧作业，队列深度不增长。
            slot.1 = job;
            return Ok(());
        }
        if state.jobs.len() >= self.capacity {
            return Err(QueueError::Full {
                channel: self.channel,
                capacity: self.capacity,
            });
        }
        state.jobs.push_back((key, job));
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<Job> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some((_, job)) = state.jobs.pop_front() {
                return Some(job);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    fn close(&self) {
        // 丢弃未执行的作业，断开「作业闭包持有 Arc<RuntimeEntry> / Client」这条引用链。
        let abandoned = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            std::mem::take(&mut state.jobs)
        };
        self.ready.notify_all();
        // 必须在锁外 drop：排队作业可能持有最后一个 `Client` 句柄，它的 Drop 会同步等待
        // pi 退出（grace period + taskkill，最坏数秒）。在锁内 drop 会连带卡住
        // `RuntimeEntry::snapshot()`（先拿 state 锁再读队列深度），把 UI 一起冻住。
        drop(abandoned);
    }

    fn len(&self) -> usize {
        self.state.lock().unwrap().jobs.len()
    }
}

/// 单 Runtime 的固定线程 Actor。
pub(crate) struct Actor {
    command: Arc<Queue>,
    control: Arc<Queue>,
    limits: ActorLimits,
    live_workers: Arc<AtomicUsize>,
}

impl Actor {
    pub(crate) fn new(runtime_id: u64, limits: ActorLimits) -> Self {
        let limits = limits.sanitized();
        let command = Arc::new(Queue::new(Channel::Command, limits.command_capacity));
        let control = Arc::new(Queue::new(Channel::Control, limits.control_capacity));
        let live_workers = Arc::new(AtomicUsize::new(0));
        for index in 0..limits.command_workers {
            spawn_worker(
                format!("pi-runtime-cmd-{runtime_id}-{index}"),
                Arc::clone(&command),
                Arc::clone(&live_workers),
            );
        }
        for index in 0..limits.control_workers {
            spawn_worker(
                format!("pi-runtime-ctl-{runtime_id}-{index}"),
                Arc::clone(&control),
                Arc::clone(&live_workers),
            );
        }
        Self {
            command,
            control,
            limits,
            live_workers,
        }
    }

    pub(crate) fn push<F>(
        &self,
        channel: Channel,
        key: Option<JobKey>,
        job: F,
    ) -> Result<(), QueueError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.queue(channel).push(key, Box::new(job))
    }

    pub(crate) fn close(&self) {
        self.command.close();
        self.control.close();
    }

    pub(crate) fn limits(&self) -> ActorLimits {
        self.limits
    }

    pub(crate) fn queued(&self, channel: Channel) -> usize {
        self.queue(channel).len()
    }

    /// 当前存活的 worker 线程数；固定不随请求次数增长。
    pub(crate) fn live_workers(&self) -> usize {
        self.live_workers.load(Ordering::Acquire)
    }

    fn queue(&self, channel: Channel) -> &Arc<Queue> {
        match channel {
            Channel::Command => &self.command,
            Channel::Control => &self.control,
        }
    }
}

fn spawn_worker(name: String, queue: Arc<Queue>, live: Arc<AtomicUsize>) {
    live.fetch_add(1, Ordering::AcqRel);
    spawn_named(name, move || {
        while let Some(job) = queue.pop() {
            job();
        }
        live.fetch_sub(1, Ordering::AcqRel);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    #[test]
    fn command_queue_is_bounded_and_reports_capacity() {
        // 单 worker + 容量 2：占住 worker 后队列只收 2 条，第 3 条必须失败而不是排队。
        let actor = Actor::new(
            1,
            ActorLimits {
                command_capacity: 2,
                control_capacity: 1,
                command_workers: 1,
                control_workers: 1,
            },
        );
        let (release_tx, release_rx) = mpsc::channel::<()>();
        actor
            .push(Channel::Command, None, move || {
                let _ = release_rx.recv();
            })
            .expect("occupying job accepted");
        assert!(wait_until(|| actor.queued(Channel::Command) == 0));

        for _ in 0..2 {
            actor
                .push(Channel::Command, None, || {})
                .expect("queued within capacity");
        }
        assert_eq!(
            actor.push(Channel::Command, None, || {}),
            Err(QueueError::Full {
                channel: Channel::Command,
                capacity: 2
            })
        );

        let _ = release_tx.send(());
        assert!(wait_until(|| actor.queued(Channel::Command) == 0));
        actor
            .push(Channel::Command, None, || {})
            .expect("queue drains and accepts again");
        actor.close();
    }

    #[test]
    fn control_channel_is_not_blocked_by_a_saturated_command_channel() {
        let actor = Actor::new(2, ActorLimits::default());
        // 阻塞作业统一等在同一个 receiver 上；drop sender 即可一次性释放全部作业。
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        // 一直投递到普通通道明确拒绝为止（worker 抢占进度会影响可接受条数）。
        let mut accepted = 0_usize;
        loop {
            let release_rx = Arc::clone(&release_rx);
            let pushed = actor.push(Channel::Command, None, move || {
                let _ = release_rx.lock().unwrap().recv();
            });
            match pushed {
                Ok(()) => accepted += 1,
                Err(QueueError::Full { channel, capacity }) => {
                    assert_eq!(channel, Channel::Command);
                    assert_eq!(capacity, actor.limits().command_capacity);
                    break;
                }
                Err(other) => panic!("unexpected queue error: {other:?}"),
            }
            assert!(accepted < 1_000, "有界队列必须在有限次投递后拒绝");
        }
        assert!(accepted >= actor.limits().command_capacity);

        let (done_tx, done_rx) = mpsc::channel();
        actor
            .push(Channel::Control, None, move || {
                let _ = done_tx.send(());
            })
            .expect("control channel stays available");
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "控制通道必须在普通通道饱和时仍然可用"
        );

        drop(release_tx);
        actor.close();
    }

    #[test]
    fn keyed_jobs_keep_only_the_latest_and_do_not_grow_the_queue() {
        let actor = Actor::new(
            3,
            ActorLimits {
                command_capacity: 4,
                control_capacity: 1,
                command_workers: 1,
                control_workers: 1,
            },
        );
        let (release_tx, release_rx) = mpsc::channel::<()>();
        actor
            .push(Channel::Command, None, move || {
                let _ = release_rx.recv();
            })
            .expect("occupying job accepted");
        assert!(wait_until(|| actor.queued(Channel::Command) == 0));

        let (tx, rx) = mpsc::channel();
        for value in 0..50 {
            let tx = tx.clone();
            actor
                .push(Channel::Command, Some(JobKey::Calibration), move || {
                    let _ = tx.send(value);
                })
                .expect("可合并作业不应把队列撑满");
        }
        drop(tx);
        assert_eq!(actor.queued(Channel::Command), 1, "同 key 只保留一条");

        let _ = release_tx.send(());
        let observed = rx.iter().collect::<Vec<_>>();
        assert_eq!(observed, vec![49], "只有最新一条 keyed 作业被执行");
        actor.close();
    }

    #[test]
    fn worker_threads_are_fixed_and_exit_on_close() {
        let actor = Actor::new(4, ActorLimits::default());
        let limits = actor.limits();
        let expected = limits.command_workers + limits.control_workers;
        assert!(wait_until(|| actor.live_workers() == expected));

        let (tx, rx) = mpsc::channel();
        let mut submitted = 0_usize;
        for _ in 0..500 {
            let tx = tx.clone();
            // 队列容量有限，投递失败是允许的；本测试只关心线程数不随请求次数增长。
            if actor
                .push(Channel::Command, None, move || {
                    let _ = tx.send(());
                })
                .is_ok()
            {
                submitted += 1;
            }
        }
        drop(tx);
        for _ in 0..submitted {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("已接受的作业必须被执行");
        }
        assert_eq!(
            actor.live_workers(),
            expected,
            "执行作业不得新建 worker 线程"
        );

        actor.close();
        assert!(
            wait_until(|| actor.live_workers() == 0),
            "close 后 worker 必须退出"
        );
    }

    #[test]
    fn closed_queue_rejects_further_jobs() {
        let actor = Actor::new(5, ActorLimits::default());
        actor.close();
        assert_eq!(
            actor.push(Channel::Command, None, || {}),
            Err(QueueError::Closed)
        );
        assert_eq!(
            actor.push(Channel::Control, None, || {}),
            Err(QueueError::Closed)
        );
    }
}
