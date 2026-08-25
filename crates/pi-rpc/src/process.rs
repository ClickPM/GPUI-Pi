//! pi RPC 子进程监督与线程式客户端。

use std::{
    collections::HashMap,
    ffi::OsString,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::Value;
use thiserror::Error;

use crate::{
    jsonl::{JsonlError, JsonlFramer},
    protocol::{Command, ExtensionUiResponse, RpcEvent, RpcRequest, RpcResponse, RpcSessionState},
};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const REBIND_CALIBRATION_ATTEMPTS: usize = 3;
const REBIND_CALIBRATION_TIMEOUT: Duration = Duration::from_secs(2);
const REBIND_CALIBRATION_DELAY: Duration = Duration::from_millis(20);

/// 单个订阅者允许积压的事件字节上限。
///
/// R22：stdout reader 与 supervisor 永远不因下游满而阻塞，所以订阅积压是这条链路上
/// 唯一可能无界增长的位置。超限时既不静默丢事件（会损坏 assistant 正文），也不阻塞
/// 生产者，而是发终态 `EventBacklogOverflow` 后断开该订阅，由上层按会话失败处理。
///
/// 额度按**单帧上限的倍数**表达，而不是拍一个绝对值：单条帧最大就是
/// [`crate::jsonl::DEFAULT_MAX_FRAME_LEN`]，额度必须显著大于它，否则一两条超大帧
/// （例如一张大图的工具结果）就能在消费者只是短暂卡顿时误触发 fail-stop。
/// 4 倍：既能容下连续几条超大帧，又把 fail-stop 前的内存天花板压在 64MiB。
pub const EVENT_BACKLOG_FRAME_MULTIPLE: usize = 4;
pub const DEFAULT_EVENT_BACKLOG_BYTES: usize =
    crate::jsonl::DEFAULT_MAX_FRAME_LEN * EVENT_BACKLOG_FRAME_MULTIPLE;

/// 非 stdout 来源事件的固定记账开销（字段、枚举与 Box 分配的粗略上界）。
const EVENT_OVERHEAD_BYTES: usize = 128;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub binary: PathBuf,
    pub current_dir: Option<PathBuf>,
    /// 首次启动即恢复此会话；不能先创建空会话再在启动后补设恢复目标。
    pub initial_session: Option<PathBuf>,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub max_restarts: usize,
    pub restart_window: Duration,
    pub restart_delay: Duration,
    pub shutdown_grace_period: Duration,
    pub max_frame_len: usize,
    /// 每个订阅者的事件积压字节上限，见 [`DEFAULT_EVENT_BACKLOG_BYTES`]。
    pub event_backlog_bytes: usize,
}

impl ClientConfig {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            current_dir: None,
            initial_session: None,
            args: Vec::new(),
            env: Vec::new(),
            max_restarts: 3,
            restart_window: Duration::from_secs(30),
            restart_delay: Duration::from_millis(100),
            shutdown_grace_period: Duration::from_secs(2),
            max_frame_len: crate::jsonl::DEFAULT_MAX_FRAME_LEN,
            event_backlog_bytes: DEFAULT_EVENT_BACKLOG_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    Started {
        pid: u32,
        resumed_session: Option<PathBuf>,
    },
    Exited {
        pid: u32,
        code: Option<i32>,
        success: bool,
    },
    Restarting {
        attempt: usize,
        session_file: Option<PathBuf>,
    },
    Restarted {
        pid: u32,
        session_file: Option<PathBuf>,
    },
    RestartFailed {
        error: String,
    },
    Stderr {
        line: String,
    },
    /// 订阅者积压超过 [`ClientConfig::event_backlog_bytes`]：该订阅已被断开。
    ///
    /// 这是本订阅流的最后一个事件。发出它而不是丢弃中间事件，是因为丢事件会静默
    /// 破坏 assistant 正文与工具结果的完整性；上层必须按会话失败处理。
    EventBacklogOverflow {
        queued_bytes: usize,
        limit: usize,
    },
    /// 订阅方主动断开（[`EventStream::detach`]）：这是本订阅流的最后一个事件。
    ///
    /// 与 [`LifecycleEvent::Exited`] 的区别是**进程还活着**。R23 的 Park 需要在保留
    /// pi 进程的前提下确定性地结束 reducer pump 线程，而 pump 阻塞在 `recv` 上，
    /// 只有一个哨兵事件能把它叫醒；靠丢 `Client` 唤醒会连进程一起杀掉，靠等下一条
    /// 业务事件唤醒则在空闲会话上永远等不到。
    Detached,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Rpc(Box<RpcEvent>),
    Unknown(Value),
    Lifecycle(LifecycleEvent),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ClientError {
    #[error("failed to spawn pi: {0}")]
    Spawn(String),
    #[error("pi RPC process is not running")]
    NotRunning,
    #[error("failed to write pi stdin: {0}")]
    Write(String),
    #[error("request {id} timed out")]
    Timeout { id: String },
    #[error("request {id} failed because pi exited")]
    ProcessExited { id: String },
    #[error("RPC command {command} failed: {message}")]
    Rpc { command: String, message: String },
    #[error("invalid RPC response data: {0}")]
    Decode(String),
    #[error("pi RPC supervisor stopped: {0}")]
    Supervisor(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRebindOutcome<T> {
    /// 非幂等主命令已经成功返回的数据；校准失败不得遮蔽它。
    pub data: T,
    /// `None` 表示命令被用户取消且没有切换会话；`Err` 表示主命令成功但元数据校准失败。
    pub calibration: Option<Result<RpcSessionState, ClientError>>,
}

struct PendingRequest {
    tx: Sender<Result<RpcResponse, ClientError>>,
}

/// 队列内的事件连同它的记账字节数一起传递，接收端才能在 recv 时准确归还额度。
struct QueuedEvent {
    bytes: usize,
    event: ClientEvent,
}

struct Subscriber {
    tx: Sender<QueuedEvent>,
    queued_bytes: Arc<AtomicUsize>,
    limit: usize,
}

/// 有界事件订阅流。
///
/// 队列本身仍是非阻塞的 mpsc —— 生产者永远不会因为消费者慢而阻塞，stdout reader 与
/// supervisor 因此在任何背压路径下都能继续 drain 子进程；有界性由字节额度保证。
pub struct EventStream {
    rx: Receiver<QueuedEvent>,
    queued_bytes: Arc<AtomicUsize>,
    /// 只持弱引用：订阅流绝不能反过来让 pi 进程续命。
    shared: Weak<Shared>,
}

impl EventStream {
    fn settle(&self, queued: QueuedEvent) -> ClientEvent {
        self.queued_bytes.fetch_sub(queued.bytes, Ordering::AcqRel);
        queued.event
    }

    /// 主动断开本订阅，**不影响进程与其他订阅者**。见 [`EventDetach`]。
    pub fn detach(&self) {
        self.detach_handle().detach();
    }

    /// 取一个可跨线程持有的断开句柄。
    ///
    /// [`EventStream`] 自身因为内含 [`Receiver`] 而不是 `Sync`，没法被消费线程之外的
    /// 结构共享；但「叫醒并断开这条订阅」这件事必须由外部发起，所以单独拆出这个
    /// `Send + Sync` 的小句柄。
    pub fn detach_handle(&self) -> EventDetach {
        EventDetach {
            shared: self.shared.clone(),
            queued_bytes: Arc::clone(&self.queued_bytes),
        }
    }

    pub fn recv(&self) -> Result<ClientEvent, mpsc::RecvError> {
        self.rx.recv().map(|queued| self.settle(queued))
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<ClientEvent, RecvTimeoutError> {
        self.rx
            .recv_timeout(timeout)
            .map(|queued| self.settle(queued))
    }

    pub fn try_recv(&self) -> Result<ClientEvent, mpsc::TryRecvError> {
        self.rx.try_recv().map(|queued| self.settle(queued))
    }

    /// 当前尚未被消费的事件字节数，用于观测背压水位。
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }
}

/// 订阅流的断开句柄：`Send + Sync`，可以由消费线程之外的持有者调用。
#[derive(Clone)]
pub struct EventDetach {
    /// 只持弱引用：断开句柄绝不能反过来让 pi 进程续命。
    shared: Weak<Shared>,
    /// 用它做订阅身份标识 —— 每条订阅的字节计数器都是独立的 `Arc`。
    queued_bytes: Arc<AtomicUsize>,
}

impl EventDetach {
    /// 断开对应订阅，**不影响进程与其他订阅者**。
    ///
    /// 把该订阅从订阅表摘除后投一条 [`LifecycleEvent::Detached`] 哨兵：阻塞中的 `recv`
    /// 因此立刻返回该事件，之后（发送端已随摘除而 drop）返回 `Err`。生产侧的
    /// stdout reader 与 supervisor 全程不受影响，drain 不中断。
    ///
    /// 幂等：重复调用只是找不到自己，直接返回。
    pub fn detach(&self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut subscribers = shared.subscribers.lock().unwrap();
        let Some(index) = subscribers
            .iter()
            .position(|subscriber| Arc::ptr_eq(&subscriber.queued_bytes, &self.queued_bytes))
        else {
            return;
        };
        let subscriber = subscribers.remove(index);
        // 先放锁再发哨兵：send 只是入队，但没必要在订阅表锁内做。
        drop(subscribers);
        let _ = subscriber.tx.send(QueuedEvent {
            bytes: 0,
            event: ClientEvent::Lifecycle(LifecycleEvent::Detached),
        });
    }
}

struct Shared {
    writer: Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<String, PendingRequest>>,
    subscribers: Mutex<Vec<Subscriber>>,
    shutdown: AtomicBool,
    next_id: AtomicU64,
    pid: AtomicU64,
    resume_session: Mutex<Option<PathBuf>>,
    event_backlog_bytes: usize,
}

/// 可 clone 的同步客户端。每个 blocking request 只阻塞调用线程；stdout/stderr/监督各自独立线程。
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
    supervisor: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Client {
    pub fn spawn(config: ClientConfig) -> Result<Self, ClientError> {
        let initial_session = config.initial_session.clone();
        let shared = Arc::new(Shared {
            writer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            pid: AtomicU64::new(0),
            resume_session: Mutex::new(initial_session),
            event_backlog_bytes: config.event_backlog_bytes.max(1),
        });
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let supervisor = thread::Builder::new()
            .name("pi-rpc-supervisor".into())
            .spawn(move || supervise(config, thread_shared, start_tx))
            .map_err(|error| ClientError::Spawn(error.to_string()))?;

        match start_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                supervisor: Arc::new(Mutex::new(Some(supervisor))),
            }),
            Ok(Err(error)) => {
                let _ = supervisor.join();
                Err(error)
            }
            Err(error) => {
                let _ = supervisor.join();
                Err(ClientError::Supervisor(error.to_string()))
            }
        }
    }

    /// 订阅事件流。stdout reader 与 reducer pump 分线程消费，慢 UI 不会让订阅在 burst
    /// 中被静默永久断开；上层必须持续 drain 并按帧合并事件。
    ///
    /// R22：订阅有字节额度（[`ClientConfig::event_backlog_bytes`]）。超限只会终止本订阅，
    /// 不会阻塞广播线程，因此 pi 的 stdout 始终能继续被 drain。
    pub fn subscribe(&self) -> EventStream {
        let (tx, rx) = mpsc::channel();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        self.shared.subscribers.lock().unwrap().push(Subscriber {
            tx,
            queued_bytes: Arc::clone(&queued_bytes),
            limit: self.shared.event_backlog_bytes,
        });
        EventStream {
            rx,
            queued_bytes,
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// 当前活跃进程退出后，监督器用于自动恢复的会话文件。
    ///
    /// `new_session` / `switch_session` 成功后，上层应立即用新会话路径调用本方法；
    /// 成功的 `get_state` 响应也会自动更新该值。
    pub fn set_resume_session(&self, session_file: Option<PathBuf>) {
        *self.shared.resume_session.lock().unwrap() = session_file;
    }

    pub fn resume_session(&self) -> Option<PathBuf> {
        self.shared.resume_session.lock().unwrap().clone()
    }

    pub fn pid(&self) -> Option<u32> {
        u32::try_from(self.shared.pid.load(Ordering::Acquire))
            .ok()
            .filter(|pid| *pid != 0)
    }

    pub fn request(&self, command: Command, timeout: Duration) -> Result<RpcResponse, ClientError> {
        let resume_hint = rebind_resume_hint(&command);
        let rebinds_session = is_session_rebind(&command);
        let response = self.request_once(command, timeout)?;
        if response.success && rebinds_session && !response_cancelled(&response) {
            self.prepare_resume_target(resume_hint);
            if let Err(error) = self.calibrate_resume_session() {
                broadcast(
                    &self.shared,
                    ClientEvent::Lifecycle(LifecycleEvent::Stderr {
                        line: format!(
                            "session command succeeded but resume metadata calibration failed: {error}"
                        ),
                    }),
                );
            }
        }
        Ok(response)
    }

    /// 发送一次非幂等会话切换命令，并把后续校准结果与主结果分开返回。
    ///
    /// 调用方看到 `Ok` 即可确认主命令只发送了一次且已经成功；`calibration=Err`
    /// 只能作为“成功但元数据未知”处理，禁止把主命令自动重试。
    pub fn request_session_rebind_data<T: for<'de> serde::Deserialize<'de>>(
        &self,
        command: Command,
        timeout: Duration,
    ) -> Result<SessionRebindOutcome<T>, ClientError> {
        debug_assert!(is_session_rebind(&command));
        let resume_hint = rebind_resume_hint(&command);
        let response = self.request_once(command, timeout)?;
        if !response.success {
            return Err(ClientError::Rpc {
                command: response.command,
                message: response.error.unwrap_or_else(|| "unknown RPC error".into()),
            });
        }
        let cancelled = response_cancelled(&response);
        if !cancelled {
            // 即使主结果 data 意外无法解码，也先撤销旧恢复目标，避免成功切换后崩溃回旧会话。
            self.prepare_resume_target(resume_hint);
        }
        let data = response
            .decode_data()
            .map_err(|error| ClientError::Decode(error.to_string()))?;
        let calibration = if cancelled {
            None
        } else {
            Some(self.calibrate_resume_session())
        };
        Ok(SessionRebindOutcome { data, calibration })
    }

    fn request_once(
        &self,
        command: Command,
        timeout: Duration,
    ) -> Result<RpcResponse, ClientError> {
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err(ClientError::NotRunning);
        }
        let id = format!(
            "req_{}",
            self.shared.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let request = RpcRequest {
            id: Some(id.clone()),
            command,
        };
        let (tx, rx) = mpsc::channel();
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id.clone(), PendingRequest { tx });
        if let Err(error) = write_json(&self.shared, &request) {
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => {
                self.shared.pending.lock().unwrap().remove(&id);
                Err(ClientError::Timeout { id })
            }
            Err(RecvTimeoutError::Disconnected) => Err(ClientError::ProcessExited { id }),
        }
    }

    fn prepare_resume_target(&self, resume_hint: Option<PathBuf>) {
        // switch 可立即使用调用方给出的路径；fork/clone/new_session 的目标未知时先清空，
        // 宁可恢复成新会话，也不能在校准窗口内崩溃后静默回到旧会话。
        self.set_resume_session(resume_hint);
    }

    fn calibrate_resume_session(&self) -> Result<RpcSessionState, ClientError> {
        let mut last_error = None;
        for attempt in 0..REBIND_CALIBRATION_ATTEMPTS {
            match self.request_once(Command::GetState, REBIND_CALIBRATION_TIMEOUT) {
                Ok(response) if response.success => {
                    let state = response
                        .decode_data::<RpcSessionState>()
                        .map_err(|error| ClientError::Decode(error.to_string()))?;
                    self.set_resume_session(state.session_file.clone().map(PathBuf::from));
                    return Ok(state);
                }
                Ok(response) => {
                    last_error = Some(ClientError::Rpc {
                        command: response.command,
                        message: response.error.unwrap_or_else(|| "unknown RPC error".into()),
                    });
                }
                Err(error) => last_error = Some(error),
            }
            if attempt + 1 < REBIND_CALIBRATION_ATTEMPTS {
                thread::sleep(REBIND_CALIBRATION_DELAY);
            }
        }
        Err(last_error.unwrap_or_else(|| {
            ClientError::Supervisor("session metadata calibration exhausted".into())
        }))
    }

    pub fn request_data<T: for<'de> serde::Deserialize<'de>>(
        &self,
        command: Command,
        timeout: Duration,
    ) -> Result<T, ClientError> {
        let response = self.request(command, timeout)?;
        if !response.success {
            return Err(ClientError::Rpc {
                command: response.command,
                message: response.error.unwrap_or_else(|| "unknown RPC error".into()),
            });
        }
        response
            .decode_data()
            .map_err(|error| ClientError::Decode(error.to_string()))
    }

    pub fn send_extension_ui_response(
        &self,
        response: &ExtensionUiResponse,
    ) -> Result<(), ClientError> {
        write_json(&self.shared, response)
    }

    /// 主动 shutdown：先关闭 stdin 允许 pi 正常 dispose，监督线程不会自动重启。
    pub fn shutdown(&self) -> Result<(), ClientError> {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.writer.lock().unwrap().take();
        if let Some(handle) = self.supervisor.lock().unwrap().take() {
            handle
                .join()
                .map_err(|_| ClientError::Supervisor("supervisor thread panicked".into()))?;
        }
        Ok(())
    }

    /// 进程树强杀，供用户显式取消或测试外部故障。未调用 shutdown 时会触发自动重启。
    pub fn kill_process_tree(&self) -> Result<(), ClientError> {
        let pid = self.pid().ok_or(ClientError::NotRunning)?;
        kill_process_tree(pid).map_err(|error| ClientError::Supervisor(error.to_string()))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if Arc::strong_count(&self.supervisor) == 1 {
            let _ = self.shutdown();
        }
    }
}

fn is_session_rebind(command: &Command) -> bool {
    matches!(
        command,
        Command::NewSession { .. }
            | Command::SwitchSession { .. }
            | Command::Fork { .. }
            | Command::Clone
    )
}

fn rebind_resume_hint(command: &Command) -> Option<PathBuf> {
    match command {
        Command::SwitchSession { session_path } => Some(PathBuf::from(session_path)),
        _ => None,
    }
}

fn response_cancelled(response: &RpcResponse) -> bool {
    response
        .data
        .as_ref()
        .and_then(|data| data.get("cancelled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn write_json<T: serde::Serialize>(shared: &Shared, value: &T) -> Result<(), ClientError> {
    let mut line =
        serde_json::to_vec(value).map_err(|error| ClientError::Write(error.to_string()))?;
    line.push(b'\n');
    let mut writer = shared.writer.lock().unwrap();
    let stdin = writer.as_mut().ok_or(ClientError::NotRunning)?;
    stdin
        .write_all(&line)
        .and_then(|()| stdin.flush())
        .map_err(|error| ClientError::Write(error.to_string()))
}

fn supervise(
    config: ClientConfig,
    shared: Arc<Shared>,
    start_tx: mpsc::SyncSender<Result<(), ClientError>>,
) {
    let mut first_start = Some(start_tx);
    let mut restart_times = Vec::new();
    let mut restarting = false;

    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        let resume_session = shared.resume_session.lock().unwrap().clone();
        let spawned = spawn_child(&config, resume_session.as_deref());
        let (mut child, stdout, stderr, stdin) = match spawned {
            Ok(parts) => parts,
            Err(error) => {
                if let Some(tx) = first_start.take() {
                    let _ = tx.send(Err(error));
                } else {
                    broadcast(
                        &shared,
                        ClientEvent::Lifecycle(LifecycleEvent::RestartFailed {
                            error: error.to_string(),
                        }),
                    );
                }
                fail_all_pending(&shared);
                return;
            }
        };
        let pid = child.id();
        shared.pid.store(u64::from(pid), Ordering::Release);
        *shared.writer.lock().unwrap() = Some(stdin);
        let (io_tx, io_rx) = mpsc::channel();
        let stdout_handle = spawn_stdout_reader(stdout, config.max_frame_len, io_tx.clone());
        let stderr_handle = spawn_stderr_reader(stderr, io_tx.clone());

        if let Some(tx) = first_start.take() {
            broadcast(
                &shared,
                ClientEvent::Lifecycle(LifecycleEvent::Started {
                    pid,
                    resumed_session: resume_session.clone(),
                }),
            );
            let _ = tx.send(Ok(()));
        } else if restarting {
            broadcast(
                &shared,
                ClientEvent::Lifecycle(LifecycleEvent::Restarted {
                    pid,
                    session_file: resume_session.clone(),
                }),
            );
        }

        let mut shutdown_started = None;
        let mut next_shutdown_kill = None;
        let status = loop {
            while let Ok(message) = io_rx.try_recv() {
                handle_io_message(&shared, message);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if shared.shutdown.load(Ordering::Acquire) {
                        shared.writer.lock().unwrap().take();
                        let started = shutdown_started.get_or_insert_with(Instant::now);
                        let now = Instant::now();
                        if started.elapsed() >= config.shutdown_grace_period
                            && next_shutdown_kill.is_none_or(|next| now >= next)
                            && let Err(error) = kill_process_tree(pid)
                        {
                            broadcast(
                                &shared,
                                ClientEvent::Lifecycle(LifecycleEvent::RestartFailed {
                                    error: format!("shutdown process-tree kill failed: {error}"),
                                }),
                            );
                            next_shutdown_kill = Some(now + Duration::from_millis(100));
                        }
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(error) => {
                    broadcast(
                        &shared,
                        ClientEvent::Lifecycle(LifecycleEvent::RestartFailed {
                            error: error.to_string(),
                        }),
                    );
                    let _ = kill_process_tree(pid);
                    match child.wait() {
                        Ok(status) => break status,
                        Err(_) => return,
                    }
                }
            }
        };

        *shared.writer.lock().unwrap() = None;
        while let Ok(message) = io_rx.try_recv() {
            handle_io_message(&shared, message);
        }
        let _ = stdout_handle.join();
        let _ = stderr_handle.join();
        while let Ok(message) = io_rx.try_recv() {
            handle_io_message(&shared, message);
        }
        shared.pid.store(0, Ordering::Release);
        broadcast(
            &shared,
            ClientEvent::Lifecycle(LifecycleEvent::Exited {
                pid,
                code: status.code(),
                success: status.success(),
            }),
        );
        fail_all_pending(&shared);

        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }

        let now = Instant::now();
        restart_times.retain(|time| now.duration_since(*time) <= config.restart_window);
        if restart_times.len() >= config.max_restarts {
            broadcast(
                &shared,
                ClientEvent::Lifecycle(LifecycleEvent::RestartFailed {
                    error: format!(
                        "restart limit {} reached within {:?}",
                        config.max_restarts, config.restart_window
                    ),
                }),
            );
            return;
        }
        restart_times.push(now);
        restarting = true;
        let restart_session = shared.resume_session.lock().unwrap().clone();
        broadcast(
            &shared,
            ClientEvent::Lifecycle(LifecycleEvent::Restarting {
                attempt: restart_times.len(),
                session_file: restart_session,
            }),
        );
        thread::sleep(config.restart_delay);
    }
}

type SpawnedChild = (Child, ChildStdout, ChildStderr, ChildStdin);

fn spawn_child(
    config: &ClientConfig,
    session_file: Option<&Path>,
) -> Result<SpawnedChild, ClientError> {
    let mut command = ProcessCommand::new(&config.binary);
    command.args(["--mode", "rpc"]);
    command.args(&config.args);
    command.envs(config.env.iter().cloned());
    if let Some(session_file) = session_file {
        command.arg("--session").arg(session_file);
    }
    if let Some(current_dir) = &config.current_dir {
        command.current_dir(current_dir);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| ClientError::Spawn(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ClientError::Spawn("missing stdout pipe".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ClientError::Spawn("missing stderr pipe".into()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ClientError::Spawn("missing stdin pipe".into()))?;
    Ok((child, stdout, stderr, stdin))
}

#[derive(Debug)]
enum IoMessage {
    /// `bytes` 是解析前的原始帧长度，供订阅背压精确记账。
    Stdout {
        bytes: usize,
        parsed: Result<Value, String>,
    },
    Stderr(String),
}

fn spawn_stdout_reader(
    mut stdout: ChildStdout,
    max_frame_len: usize,
    tx: Sender<IoMessage>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("pi-rpc-stdout".into())
        .spawn(move || {
            let mut framer = JsonlFramer::new(max_frame_len);
            let mut chunk = [0_u8; 8192];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => {
                        if let Ok(Some(frame)) = framer.finish() {
                            send_stdout_frame(&tx, frame);
                        }
                        return;
                    }
                    Ok(read) => match framer.push(&chunk[..read]) {
                        Ok(frames) => {
                            for frame in frames {
                                send_stdout_frame(&tx, frame);
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(IoMessage::Stdout {
                                bytes: 0,
                                parsed: Err(error.to_string()),
                            });
                            return;
                        }
                    },
                    Err(error) => {
                        let _ = tx.send(IoMessage::Stdout {
                            bytes: 0,
                            parsed: Err(error.to_string()),
                        });
                        return;
                    }
                }
            }
        })
        .expect("failed to spawn stdout reader")
}

fn send_stdout_frame(tx: &Sender<IoMessage>, frame: Vec<u8>) {
    if frame.is_empty() {
        return;
    }
    let bytes = frame.len();
    let parsed = serde_json::from_slice(&frame).map_err(|error| error.to_string());
    let _ = tx.send(IoMessage::Stdout { bytes, parsed });
}

fn spawn_stderr_reader(mut stderr: ChildStderr, tx: Sender<IoMessage>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("pi-rpc-stderr".into())
        .spawn(move || {
            let mut framer = JsonlFramer::new(crate::jsonl::DEFAULT_MAX_FRAME_LEN);
            let mut chunk = [0_u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => {
                        if let Ok(Some(frame)) = framer.finish() {
                            let _ = tx.send(IoMessage::Stderr(
                                String::from_utf8_lossy(&frame).into_owned(),
                            ));
                        }
                        return;
                    }
                    Ok(read) => match framer.push(&chunk[..read]) {
                        Ok(frames) => {
                            for frame in frames {
                                let _ = tx.send(IoMessage::Stderr(
                                    String::from_utf8_lossy(&frame).into_owned(),
                                ));
                            }
                        }
                        Err(JsonlError::FrameTooLarge { .. }) => return,
                    },
                    Err(_) => return,
                }
            }
        })
        .expect("failed to spawn stderr reader")
}

fn handle_io_message(shared: &Shared, message: IoMessage) {
    match message {
        IoMessage::Stderr(line) => broadcast(
            shared,
            ClientEvent::Lifecycle(LifecycleEvent::Stderr { line }),
        ),
        IoMessage::Stdout {
            parsed: Err(error), ..
        } => broadcast(
            shared,
            ClientEvent::Unknown(Value::String(format!("invalid stdout JSON: {error}"))),
        ),
        IoMessage::Stdout {
            bytes,
            parsed: Ok(value),
        } => {
            if value.get("type").and_then(Value::as_str) == Some("response") {
                match serde_json::from_value::<RpcResponse>(value.clone()) {
                    Ok(response) => {
                        if response.success
                            && response.command == "get_state"
                            && let Ok(state) = response.decode_data::<RpcSessionState>()
                        {
                            *shared.resume_session.lock().unwrap() =
                                state.session_file.map(PathBuf::from);
                        }
                        if let Some(id) = response.id.clone()
                            && let Some(pending) = shared.pending.lock().unwrap().remove(&id)
                        {
                            let _ = pending.tx.send(Ok(response));
                            return;
                        }
                        broadcast_sized(shared, ClientEvent::Unknown(value), bytes);
                    }
                    Err(_) => broadcast_sized(shared, ClientEvent::Unknown(value), bytes),
                }
            } else {
                match serde_json::from_value::<RpcEvent>(value.clone()) {
                    Ok(event) => broadcast_sized(shared, ClientEvent::Rpc(Box::new(event)), bytes),
                    Err(_) => broadcast_sized(shared, ClientEvent::Unknown(value), bytes),
                }
            }
        }
    }
}

fn fail_all_pending(shared: &Shared) {
    let pending = std::mem::take(&mut *shared.pending.lock().unwrap());
    for (id, request) in pending {
        let _ = request.tx.send(Err(ClientError::ProcessExited { id }));
    }
}

fn broadcast(shared: &Shared, event: ClientEvent) {
    let bytes = estimate_event_bytes(&event);
    broadcast_sized(shared, event, bytes);
}

/// 广播一条已知字节数的事件。
///
/// 对 stdout 来源的事件，`bytes` 是解析前的原始 JSONL 帧长度 —— 比结构化估算更准，
/// 且不需要为了记账再序列化一次。
fn broadcast_sized(shared: &Shared, event: ClientEvent, bytes: usize) {
    let bytes = bytes.saturating_add(EVENT_OVERHEAD_BYTES);
    shared.subscribers.lock().unwrap().retain(|subscriber| {
        let queued = subscriber.queued_bytes.load(Ordering::Acquire);
        if queued.saturating_add(bytes) > subscriber.limit {
            // 不丢中间事件、也不阻塞生产者：发终态后断开，让上层按会话失败处理。
            let _ = subscriber.tx.send(QueuedEvent {
                bytes: 0,
                event: ClientEvent::Lifecycle(LifecycleEvent::EventBacklogOverflow {
                    queued_bytes: queued,
                    limit: subscriber.limit,
                }),
            });
            return false;
        }
        subscriber.queued_bytes.fetch_add(bytes, Ordering::AcqRel);
        if subscriber
            .tx
            .send(QueuedEvent {
                bytes,
                event: event.clone(),
            })
            .is_err()
        {
            subscriber.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
            return false;
        }
        true
    });
}

/// 非 stdout 来源事件的字节估算：只算真正持有的堆字符串，其余按固定开销记账。
fn estimate_event_bytes(event: &ClientEvent) -> usize {
    match event {
        ClientEvent::Lifecycle(LifecycleEvent::Stderr { line }) => line.len(),
        ClientEvent::Lifecycle(LifecycleEvent::RestartFailed { error }) => error.len(),
        ClientEvent::Lifecycle(_) => 0,
        ClientEvent::Unknown(Value::String(text)) => text.len(),
        // 结构化 Unknown / Rpc 只在 stdout 路径产生，那里走 broadcast_sized 传真实帧长。
        ClientEvent::Unknown(_) | ClientEvent::Rpc(_) => 0,
    }
}

#[cfg(windows)]
pub fn kill_process_tree(pid: u32) -> std::io::Result<()> {
    let status = ProcessCommand::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "taskkill exited with {status}"
        )))
    }
}

#[cfg(unix)]
pub fn kill_process_tree(pid: u32) -> std::io::Result<()> {
    let status = ProcessCommand::new("kill")
        // `--` 避免 procps-ng kill 把负 PGID 误解为旧式 signal 参数。
        .args(["-TERM", "--", &format!("-{pid}")])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("kill exited with {status}")))
    }
}
