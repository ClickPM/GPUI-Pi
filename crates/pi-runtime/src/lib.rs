mod actor;
pub mod clock;
mod effects;
pub mod scheduler;

pub use actor::{ActorLimits, live_thread_count, spawned_thread_count};
pub use clock::{Clock, FakeClock, SystemClock};
pub use effects::{BackpressureStats, EffectLimits};
pub use scheduler::{
    IllegalTransition, Priority, QueueFull, SchedulerLimits, SchedulerReport, SchedulerState,
    SessionId, SlotCounts, SlotKind,
};

use actor::{Actor, Channel, JobKey, QueueError};
use effects::EffectBuffer;
use scheduler::{SlotLease, SlotPool, WaitQueue};

use pi_render::{
    ConversationDocument, LiveAssistantUpdate, LiveBlockKind, LiveEvent, LivePhase,
    LiveSessionReducer,
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use pi_rpc::{
    AssistantMessageEvent, AvailableModelsData, Client, ClientConfig, ClientEvent, CloneData,
    Command, CommandsData, CompactionResult, EventDetach, EventStream, ExportPathData,
    ExtensionUiRequest, ExtensionUiResponse, ForkData, ImageContent, ImageKind, Model, NotifyType,
    RpcEvent, RpcSessionState, RpcSlashCommand, StreamingBehavior, ThinkingLevel,
    ThinkingLevelsData, TreeData, WidgetPlacement,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Extension slash command handlers may synchronously wait for several human-operated dialogs
/// before the official RPC emits the prompt response. Keep this bounded, but do not apply the
/// metadata/control timeout to interactive submissions.
const INTERACTIVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// EventReducer 合帧窗口的允许区间（立项文档 § 七阶段 E：16–33ms）。
///
/// 下界保证 120Hz 显示器上不会一帧塞两批；上界保证最慢也有 ~30fps 的可见更新。
pub const EVENT_FRAME_MIN: Duration = Duration::from_millis(16);
pub const EVENT_FRAME_MAX: Duration = Duration::from_millis(33);
/// 默认合帧窗口，约等于 50fps。
pub const DEFAULT_EVENT_FRAME: Duration = Duration::from_millis(20);

/// 把配置值收进允许区间；越界配置被 clamp 而不是被信任。
pub fn clamp_event_frame(frame: Duration) -> Duration {
    frame.clamp(EVENT_FRAME_MIN, EVENT_FRAME_MAX)
}

const MAX_EVENTS_PER_BATCH: usize = 512;
const EXTENSION_TEXT_LIMIT: usize = 4096;
const EXTENSION_EDITABLE_BYTES_LIMIT: usize = 1024 * 1024;
const EXTENSION_SELECT_RAW_BYTES_LIMIT: usize = 1024 * 1024;
const EXTENSION_KEY_LIMIT: usize = 64;
const EXTENSION_STATUS_LIMIT: usize = 256;
const EXTENSION_STATUS_COUNT_LIMIT: usize = 16;
const EXTENSION_WIDGET_COUNT_LIMIT: usize = 8;
const EXTENSION_WIDGET_LINE_LIMIT: usize = 256;
const EXTENSION_WIDGET_LINES_LIMIT: usize = 8;
const EXTENSION_DIALOG_TITLE_LIMIT: usize = 128;
const EXTENSION_DIALOG_MESSAGE_LIMIT: usize = 2048;
const EXTENSION_DIALOG_OPTION_LIMIT: usize = 256;
const EXTENSION_DIALOG_OPTIONS_LIMIT: usize = 50;
const EXTENSION_DIALOG_QUEUE_LIMIT: usize = 32;
const EXTENSION_NOTIFICATION_QUEUE_LIMIT: usize = 64;

pub const UNSUPPORTED_BY_PINNED_RPC: &str = "UNSUPPORTED_BY_PINNED_RPC";

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionDialogRequest {
    pub id: String,
    pub request: ExtensionUiRequest,
    pub select_options: Option<Vec<ExtensionSelectOption>>,
    pub deadline: Option<Instant>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSelectOption {
    pub raw: String,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionNotification {
    pub message: String,
    pub notify_type: NotifyType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionWidget {
    pub raw_key: String,
    pub display_key: String,
    pub lines: Vec<String>,
    pub placement: WidgetPlacement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionStatus {
    pub raw_key: String,
    pub display_key: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtensionUiState {
    dialogs: VecDeque<ExtensionDialogRequest>,
    next_dialog_sequence: u64,
    pending_ids: HashSet<String>,
    statuses: BTreeMap<String, ExtensionStatus>,
    widgets: BTreeMap<String, ExtensionWidget>,
    notifications: VecDeque<ExtensionNotification>,
    diagnostics: VecDeque<String>,
    title: Option<String>,
    editor_text: Option<String>,
    has_seen_extension_ui: bool,
}

impl ExtensionUiState {
    /// 应用请求；无法入队的交互请求必须由调用方立即回传取消，避免扩展永久等待。
    pub fn apply(
        &mut self,
        id: String,
        request: ExtensionUiRequest,
    ) -> Option<ExtensionUiResponse> {
        self.has_seen_extension_ui = true;
        if self.pending_ids.contains(&id) {
            return None;
        }
        let request = sanitize_extension_request(request);
        match request {
            ExtensionUiRequest::Select {
                title,
                options,
                timeout,
            } => {
                if options.is_empty() {
                    return Some(ExtensionUiResponse::cancelled(id));
                }
                if options.len() > EXTENSION_DIALOG_OPTIONS_LIMIT {
                    self.push_diagnostic(format!(
                        "Extension UI Select {id} 有 {} 个选项，超过上限 {EXTENSION_DIALOG_OPTIONS_LIMIT}，已取消",
                        options.len()
                    ));
                    return Some(ExtensionUiResponse::cancelled(id));
                }
                let raw_bytes = options.iter().map(String::len).sum::<usize>();
                if raw_bytes > EXTENSION_SELECT_RAW_BYTES_LIMIT {
                    self.push_diagnostic(format!(
                        "Extension UI Select {id} 原始选项总量超过 1 MiB，已取消"
                    ));
                    return Some(ExtensionUiResponse::cancelled(id));
                }
                if self.dialogs.len() >= EXTENSION_DIALOG_QUEUE_LIMIT {
                    return Some(ExtensionUiResponse::cancelled(id));
                }
                let select_options = options
                    .into_iter()
                    .map(|raw| ExtensionSelectOption {
                        display: sanitize_extension_single_line(
                            &raw,
                            EXTENSION_DIALOG_OPTION_LIMIT,
                        ),
                        raw,
                    })
                    .collect();
                self.pending_ids.insert(id.clone());
                let sequence = self.next_dialog_sequence;
                self.next_dialog_sequence = self.next_dialog_sequence.wrapping_add(1);
                self.dialogs.push_back(ExtensionDialogRequest {
                    id,
                    request: ExtensionUiRequest::Select {
                        title,
                        options: Vec::new(),
                        timeout,
                    },
                    select_options: Some(select_options),
                    deadline: timeout
                        .map(|timeout| Instant::now() + Duration::from_millis(timeout.max(1))),
                    sequence,
                });
            }
            ExtensionUiRequest::Confirm { timeout, .. }
            | ExtensionUiRequest::Input { timeout, .. } => {
                if self.dialogs.len() >= EXTENSION_DIALOG_QUEUE_LIMIT {
                    return Some(ExtensionUiResponse::cancelled(id));
                }
                self.pending_ids.insert(id.clone());
                let sequence = self.next_dialog_sequence;
                self.next_dialog_sequence = self.next_dialog_sequence.wrapping_add(1);
                self.dialogs.push_back(ExtensionDialogRequest {
                    id,
                    request,
                    select_options: None,
                    deadline: timeout
                        .map(|timeout| Instant::now() + Duration::from_millis(timeout.max(1))),
                    sequence,
                });
            }
            ExtensionUiRequest::Editor { title, prefill } => {
                if prefill
                    .as_ref()
                    .is_some_and(|prefill| prefill.len() > EXTENSION_EDITABLE_BYTES_LIMIT)
                {
                    self.push_diagnostic(format!(
                        "Extension UI Editor {id} prefill 超过 1 MiB，已取消"
                    ));
                    return Some(ExtensionUiResponse::cancelled(id));
                }
                let prefill = prefill.map(|prefill| {
                    let sanitized = sanitize_extension_editable(&prefill);
                    if sanitized != prefill {
                        self.push_diagnostic(format!(
                            "Extension UI Editor {id} prefill 含控制/Cf 字符，已过滤但未截断"
                        ));
                    }
                    sanitized
                });
                if self.dialogs.len() >= EXTENSION_DIALOG_QUEUE_LIMIT {
                    return Some(ExtensionUiResponse::cancelled(id));
                }
                self.pending_ids.insert(id.clone());
                let sequence = self.next_dialog_sequence;
                self.next_dialog_sequence = self.next_dialog_sequence.wrapping_add(1);
                self.dialogs.push_back(ExtensionDialogRequest {
                    id,
                    request: ExtensionUiRequest::Editor { title, prefill },
                    select_options: None,
                    deadline: None,
                    sequence,
                });
            }
            ExtensionUiRequest::Notify {
                message,
                notify_type,
            } => {
                if self.notifications.len() >= EXTENSION_NOTIFICATION_QUEUE_LIMIT {
                    self.notifications.pop_front();
                }
                self.notifications.push_back(ExtensionNotification {
                    message: sanitize_extension_text(&message, EXTENSION_TEXT_LIMIT),
                    notify_type: notify_type.unwrap_or(NotifyType::Info),
                });
            }
            ExtensionUiRequest::SetStatus {
                status_key,
                status_text,
            } => {
                if let Some(text) = status_text {
                    if self.statuses.contains_key(&status_key)
                        || self.statuses.len() < EXTENSION_STATUS_COUNT_LIMIT
                    {
                        self.statuses.insert(
                            status_key.clone(),
                            ExtensionStatus {
                                display_key: sanitize_extension_single_line(
                                    &status_key,
                                    EXTENSION_KEY_LIMIT,
                                ),
                                raw_key: status_key,
                                text: sanitize_extension_single_line(&text, EXTENSION_STATUS_LIMIT),
                            },
                        );
                    }
                } else {
                    self.statuses.remove(&status_key);
                }
            }
            ExtensionUiRequest::SetWidget {
                widget_key,
                widget_lines,
                widget_placement,
            } => {
                if let Some(lines) = widget_lines {
                    if self.widgets.contains_key(&widget_key)
                        || self.widgets.len() < EXTENSION_WIDGET_COUNT_LIMIT
                    {
                        let lines = lines
                            .into_iter()
                            .take(EXTENSION_WIDGET_LINES_LIMIT)
                            .map(|line| {
                                sanitize_extension_single_line(&line, EXTENSION_WIDGET_LINE_LIMIT)
                            })
                            .collect();
                        self.widgets.insert(
                            widget_key.clone(),
                            ExtensionWidget {
                                display_key: sanitize_extension_single_line(
                                    &widget_key,
                                    EXTENSION_KEY_LIMIT,
                                ),
                                raw_key: widget_key,
                                lines,
                                placement: widget_placement.unwrap_or(WidgetPlacement::AboveEditor),
                            },
                        );
                    }
                } else {
                    self.widgets.remove(&widget_key);
                }
            }
            ExtensionUiRequest::SetTitle { title } => {
                self.title = Some(sanitize_extension_single_line(&title, EXTENSION_TEXT_LIMIT));
            }
            ExtensionUiRequest::SetEditorText { text } => {
                if text.len() > EXTENSION_EDITABLE_BYTES_LIMIT {
                    self.push_diagnostic(
                        "Extension UI set_editor_text 超过 1 MiB，未替换编辑器内容".to_owned(),
                    );
                } else {
                    let sanitized = sanitize_extension_editable(&text);
                    if sanitized != text {
                        self.push_diagnostic(
                            "Extension UI set_editor_text 含控制/Cf 字符，已过滤但未截断"
                                .to_owned(),
                        );
                    }
                    self.editor_text = Some(sanitized);
                }
            }
        }
        None
    }

    pub fn active_dialog(&self) -> Option<&ExtensionDialogRequest> {
        self.dialogs.front()
    }

    pub fn is_dialog_pending(&self, id: &str) -> bool {
        self.pending_ids.contains(id)
    }

    pub fn finish_dialog(&mut self, id: &str) -> bool {
        if self.dialogs.front().is_none_or(|dialog| dialog.id != id) {
            return false;
        }
        self.dialogs.pop_front();
        self.pending_ids.remove(id)
    }

    pub fn active_dialog_expired(&self, now: Instant) -> bool {
        self.dialogs
            .front()
            .and_then(|dialog| dialog.deadline)
            .is_some_and(|deadline| now >= deadline)
    }

    pub fn statuses(&self) -> impl Iterator<Item = &ExtensionStatus> {
        self.statuses.values()
    }

    pub fn widgets(&self, placement: WidgetPlacement) -> impl Iterator<Item = &ExtensionWidget> {
        self.widgets
            .values()
            .filter(move |widget| widget.placement == placement)
    }

    pub fn take_notification(&mut self) -> Option<ExtensionNotification> {
        self.notifications.pop_front()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn take_editor_text(&mut self) -> Option<String> {
        self.editor_text.take()
    }

    pub fn take_diagnostic(&mut self) -> Option<String> {
        self.diagnostics.pop_front()
    }

    pub const fn has_seen_extension_ui(&self) -> bool {
        self.has_seen_extension_ui
    }

    fn push_diagnostic(&mut self, diagnostic: String) {
        const DIAGNOSTIC_LIMIT: usize = 16;
        if self.diagnostics.len() >= DIAGNOSTIC_LIMIT {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(diagnostic);
    }

    pub fn drain_cancelled_dialogs(&mut self) -> Vec<ExtensionUiResponse> {
        let responses = self
            .dialogs
            .drain(..)
            .map(|dialog| ExtensionUiResponse::cancelled(dialog.id))
            .collect();
        self.pending_ids.clear();
        responses
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub const fn custom_ui_capability(&self) -> &'static str {
        "自定义扩展界面受限"
    }
}

fn sanitize_extension_request(request: ExtensionUiRequest) -> ExtensionUiRequest {
    match request {
        ExtensionUiRequest::Select {
            title,
            options,
            timeout,
        } => ExtensionUiRequest::Select {
            title: sanitize_extension_single_line(&title, EXTENSION_DIALOG_TITLE_LIMIT),
            options,
            timeout,
        },
        ExtensionUiRequest::Confirm {
            title,
            message,
            timeout,
        } => ExtensionUiRequest::Confirm {
            title: sanitize_extension_single_line(&title, EXTENSION_DIALOG_TITLE_LIMIT),
            message: sanitize_extension_text(&message, EXTENSION_DIALOG_MESSAGE_LIMIT),
            timeout,
        },
        ExtensionUiRequest::Input {
            title,
            placeholder,
            timeout,
        } => ExtensionUiRequest::Input {
            title: sanitize_extension_single_line(&title, EXTENSION_DIALOG_TITLE_LIMIT),
            placeholder: placeholder.map(|placeholder| {
                sanitize_extension_single_line(&placeholder, EXTENSION_DIALOG_OPTION_LIMIT)
            }),
            timeout,
        },
        ExtensionUiRequest::Editor { title, prefill } => ExtensionUiRequest::Editor {
            title: sanitize_extension_single_line(&title, EXTENSION_DIALOG_TITLE_LIMIT),
            prefill,
        },
        request => request,
    }
}

fn sanitize_extension_editable(text: &str) -> String {
    text.chars()
        .filter(|character| {
            (*character == '\n' || *character == '\t' || !character.is_control())
                && !is_unicode_format_character(*character)
        })
        .collect()
}

pub fn sanitize_extension_text(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|character| {
            (*character == '\n' || *character == '\t' || !character.is_control())
                && !is_unicode_format_character(*character)
        })
        .take(limit)
        .collect()
}

fn sanitize_extension_single_line(text: &str, limit: usize) -> String {
    let mut result = String::new();
    let mut length = 0;
    let mut pending_space = false;
    for character in text.chars() {
        if character == '\r' || character == '\n' || character == '\t' {
            pending_space = !result.is_empty();
            continue;
        }
        if character.is_control() || is_unicode_format_character(character) {
            continue;
        }
        if pending_space && !character.is_whitespace() && length < limit {
            result.push(' ');
            length += 1;
        }
        pending_space = false;
        if length >= limit {
            break;
        }
        result.push(character);
        length += 1;
    }
    result
}

/// Unicode General_Category=Cf；扩展文本不应能用双向控制或零宽字符伪装 UI。
fn is_unicode_format_character(character: char) -> bool {
    matches!(
        character as u32,
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x13455
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerMode {
    Steer,
    FollowUp,
}

impl ComposerMode {
    pub const fn streaming_behavior(self) -> StreamingBehavior {
        match self {
            Self::Steer => StreamingBehavior::Steer,
            Self::FollowUp => StreamingBehavior::FollowUp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcIntent {
    Prompt,
    Steer,
    FollowUp,
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolPreset {
    #[default]
    Inherit,
    None,
    ReadOnly,
    Default,
    Full,
}

impl ToolPreset {
    pub const ALL: [Self; 5] = [
        Self::Inherit,
        Self::None,
        Self::ReadOnly,
        Self::Default,
        Self::Full,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Inherit => "跟随 pi",
            Self::None => "关闭",
            Self::ReadOnly => "只读",
            Self::Default => "默认",
            Self::Full => "完整",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Inherit => "沿用 settings.json 的 defaultTools 与扩展工具",
            Self::None => "不启用任何工具（扩展工具也不生效）",
            Self::ReadOnly => "内建 read、grep、find、ls（扩展工具不生效）",
            Self::Default => "内建四件套 read、bash、edit、write（扩展工具不生效）",
            Self::Full => "全部 7 个内建工具（扩展工具不生效）",
        }
    }

    pub const fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::Inherit | Self::None => &[],
            Self::ReadOnly => &["read", "grep", "find", "ls"],
            Self::Default => &["read", "bash", "edit", "write"],
            Self::Full => &["bash", "read", "edit", "write", "grep", "find", "ls"],
        }
    }

    pub fn append_args(self, args: &mut Vec<std::ffi::OsString>) {
        if self == Self::Inherit {
            return;
        }
        args.push("--tools".into());
        args.push(self.tool_names().join(",").into());
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionControls {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub models: Vec<Model>,
    pub thinking_levels: Vec<ThinkingLevel>,
    pub session_file: Option<PathBuf>,
    pub session_id: String,
    pub tree: TreeData,
    pub auto_compaction_enabled: bool,
    pub auto_retry_enabled: bool,
    pub is_compacting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOperation {
    Model,
    Thinking,
    Tools,
    Compact,
    AutoCompaction,
    AutoRetry,
    AbortRetry,
    Fork,
    Clone,
    SwitchSession,
    ExportHtml,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlRequest {
    SetModel { provider: String, model_id: String },
    CycleModel,
    SetThinking(ThinkingLevel),
    Compact,
    SetAutoCompaction(bool),
    SetAutoRetry(bool),
    AbortRetry,
    Fork { entry_id: String },
    Clone,
    SwitchSession { path: PathBuf },
    ExportHtml { output_path: PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlOutcome {
    Controls(SessionControls),
    Compacted(CompactionResult),
    Forked {
        data: ForkData,
        controls: SessionControls,
    },
    ForkCancelled(ForkData),
    Cloned {
        data: CloneData,
        controls: SessionControls,
    },
    CloneCancelled,
    Switched(SessionControls),
    SwitchCancelled,
    RebindCalibrationFailed {
        operation: ControlOperation,
        message: String,
        fork_data: Option<ForkData>,
    },
    Exported(ExportPathData),
    RetryAborted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRuntimeEvent {
    CompactionStarted,
    CompactionEnded {
        error: Option<String>,
    },
    RetryStarted {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
    RetryEnded {
        success: bool,
        attempt: u32,
        error: Option<String>,
    },
    AgentEnded {
        will_retry: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposerSubmission {
    pub message: String,
    pub images: Vec<pi_data::DraftImage>,
}

impl ComposerSubmission {
    fn rpc_images(&self) -> Option<Vec<ImageContent>> {
        (!self.images.is_empty()).then(|| {
            self.images
                .iter()
                .map(|image| ImageContent {
                    kind: ImageKind::Image,
                    data: image.data.clone(),
                    mime_type: image.mime_type.clone(),
                })
                .collect()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestFailureKind {
    Rejected,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEffectKind {
    Events {
        follow_tail: bool,
        settled: bool,
        runtime_events: Vec<SessionRuntimeEvent>,
    },
    ExtensionUiBatch {
        requests: Vec<(String, ExtensionUiRequest)>,
    },
    ExtensionUiReset,
    RequestFinished {
        intent: RpcIntent,
        submission: Option<ComposerSubmission>,
        pending_activity_generation: Option<u64>,
        result: Result<(), (RequestFailureKind, String)>,
    },
    CommandsLoaded(Result<Vec<RpcSlashCommand>, String>),
    ControlsLoaded(Result<SessionControls, String>),
    ControlFinished {
        operation: ControlOperation,
        result: Result<ControlOutcome, String>,
    },
    ToolRestartFinished {
        preset: ToolPreset,
        result: Result<(), String>,
    },
    Diagnostic(String),
    Stopped(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeEffect {
    pub sequence: u64,
    pub epoch: u64,
    pub kind: RuntimeEffectKind,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub runtime_id: RuntimeId,
    pub epoch: u64,
    pub revision: u64,
    pub document: Arc<ConversationDocument>,
    pub phase: LivePhase,
    pub steering_queue_len: usize,
    pub follow_up_queue_len: usize,
    pub startup_diagnostic: Option<String>,
    pub effects: Arc<[RuntimeEffect]>,
    /// 权威终态：`None` 表示仍在运行。不随 effect 背压丢失。
    pub terminal: Option<TerminalState>,
    /// 背压统计；任何非零淘汰计数都必须对用户可见。
    pub backpressure: BackpressureStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dirty {
    pub runtime_id: RuntimeId,
    pub epoch: u64,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeId(u64);

impl RuntimeId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    /// Maintenance job 的独立配额，不占用户会话运行槽。
    pub maintenance_slots: usize,
    /// 会话调度器的运行槽、warm pool、队列与 Idle TTL 配置。
    pub scheduler: SchedulerLimits,
    /// 单 Runtime 命令 Actor 的队列容量与 worker 数。
    pub actor: ActorLimits,
    /// 单 Session effect 缓存的固定上限。
    pub effects: EffectLimits,
    /// EventReducer 合帧窗口；越界值按 [`clamp_event_frame`] 收敛。
    pub event_frame: Duration,
    /// 单订阅事件积压字节上限，透传给 `pi-rpc`。
    pub event_backlog_bytes: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            maintenance_slots: 1,
            scheduler: SchedulerLimits::default(),
            actor: ActorLimits::default(),
            effects: EffectLimits::default(),
            event_frame: DEFAULT_EVENT_FRAME,
            event_backlog_bytes: pi_rpc::DEFAULT_EVENT_BACKLOG_BYTES,
        }
    }
}

/// 运行时的有界参数快照，随 Runtime 创建时固化。
#[derive(Debug, Clone, Copy)]
struct RuntimeTuning {
    actor: ActorLimits,
    effects: EffectLimits,
    event_frame: Duration,
    event_backlog_bytes: usize,
}

impl RuntimeTuning {
    fn from_limits(limits: &RuntimeLimits) -> Self {
        // 有界参数在此一次性收敛：下游读到的 tuning 就是生效值，不必各自再 clamp。
        Self {
            actor: limits.actor.sanitized(),
            effects: limits.effects.sanitized(),
            event_frame: clamp_event_frame(limits.event_frame),
            event_backlog_bytes: limits.event_backlog_bytes.max(1),
        }
    }
}

/// Runtime 的权威终态。
///
/// 终态**不依赖 effect 流**：即使 effect 因背压被淘汰，Snapshot 上的终态也必须仍然
/// 正确，UI 才能在任何积压情况下知道会话已经结束。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalState {
    /// 应用主动优雅停止（切换会话、退出）。
    Stopped,
    /// Runtime 失败：崩溃、重启失败、事件积压超限。
    Failed { error: String },
}

#[derive(Clone)]
pub struct RuntimeManager {
    inner: Arc<ManagerInner>,
}

/// 启动一个 Runtime 所需的全部输入。
///
/// Park 之后进程没了，但这份描述留着 —— Resume 时既可能拿它冷启动，也可能拿它
/// 去 warm pool 里匹配一个参数一致的热进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDescriptor {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    /// 会话文件；`None` 表示 fresh 会话（尚未落盘，因此不可 Park、不可 warm 复用）。
    pub session_path: Option<PathBuf>,
    pub tool_preset: ToolPreset,
    pub agent_dir: Option<PathBuf>,
}

/// 热进程的可复用性判据。
///
/// `switch_session` 只能换会话文件，**换不了** cwd、工具集和扩展目录——这些是启动参数。
/// 因此只有这四项完全一致的热进程才能被复用，否则必须冷启动。
#[derive(Debug, Clone, PartialEq, Eq)]
struct WarmKey {
    binary: PathBuf,
    cwd: PathBuf,
    tool_preset: ToolPreset,
    agent_dir: Option<PathBuf>,
}

impl WarmKey {
    fn of(descriptor: &SessionDescriptor) -> Self {
        Self {
            binary: descriptor.binary.clone(),
            cwd: descriptor.cwd.clone(),
            tool_preset: descriptor.tool_preset,
            agent_dir: descriptor.agent_dir.clone(),
        }
    }
}

/// 池内的空闲热进程。
struct WarmRuntime {
    client: Client,
    key: WarmKey,
    idle_since: Duration,
    /// 常驻槽凭证：热进程也占内存，必须一直占着名额直到真正退出。
    lease: SlotLease,
}

/// 调度器状态发生了变化。
///
/// **电平触发，不是边沿触发**：通知本身不带「变成了什么」，收到的一方必须重新查询
/// 自己关心的会话状态。这样被合并掉的帧不会让订阅者停在旧状态上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerChanged;

/// 调度器修订号 + 订阅者名册。
///
/// 每个订阅者只有**一格**待处理通知（`sync_channel(1)` + `try_send` 丢满帧）：
/// 通知是电平触发的，第 N 条与第 N+1 条对订阅者是同一件事——重新查一遍状态。
/// 给 UI 用的通道必须有界，否则界面一卡就长出一条无界队列，正是 R22 要消灭的东西。
///
/// **锁序**：`SchedulerCore` → `SchedulerWatch::watchers`，永不反向。
/// 发布发生在持 core 锁期间，所以这里只允许做不会阻塞的动作。
#[derive(Default)]
struct SchedulerWatch {
    revision: AtomicU64,
    next_watcher_id: AtomicU64,
    watchers: Mutex<Vec<(u64, SyncSender<SchedulerChanged>)>>,
}

impl SchedulerWatch {
    fn subscribe(self: &Arc<Self>) -> SchedulerSubscription {
        let (tx, rx) = mpsc::sync_channel(1);
        let id = self.next_watcher_id.fetch_add(1, Ordering::Relaxed);
        self.watchers.lock().unwrap().push((id, tx));
        SchedulerSubscription {
            id,
            watch: Arc::downgrade(self),
            receiver: Some(rx),
        }
    }

    fn unsubscribe(&self, id: u64) {
        self.watchers
            .lock()
            .unwrap()
            .retain(|(watcher, _)| *watcher != id);
    }

    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    /// 记一次变化并唤醒订阅者。
    fn publish(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
        let mut watchers = self.watchers.lock().unwrap();
        // `Full` 说明上一条还没被取走 —— 电平触发下这条与上一条对订阅者是同一件事，
        // 丢掉即可；只有 `Disconnected` 才把订阅者摘掉。
        watchers.retain(|(_, tx)| {
            !matches!(
                tx.try_send(SchedulerChanged),
                Err(TrySendError::Disconnected(_))
            )
        });
    }
}

/// 一份调度器订阅。
///
/// **丢掉它就是退订**：名册里的发送端随之被摘掉，通道断开，正阻塞在 `recv` 上的
/// 桥接线程立刻返回并退出。UI 侧必须把接收端交给一条阻塞线程，如果只靠
/// 「通道另一端没人了」来收尾，那条线程会一直卡在 `recv` 上直到整个 Manager 析构 ——
/// 关一次窗口漏一条线程。
pub struct SchedulerSubscription {
    id: u64,
    watch: std::sync::Weak<SchedulerWatch>,
    receiver: Option<Receiver<SchedulerChanged>>,
}

impl SchedulerSubscription {
    /// 取走接收端交给桥接线程。只有第一次调用返回 `Some`。
    pub fn take_receiver(&mut self) -> Option<Receiver<SchedulerChanged>> {
        self.receiver.take()
    }
}

impl Drop for SchedulerSubscription {
    fn drop(&mut self) {
        if let Some(watch) = self.watch.upgrade() {
            watch.unsubscribe(self.id);
        }
    }
}

/// 一个已登记的用户会话。
///
/// `Parked` 时 `entry` / `lease` 均为 `None`：零进程零线程，只留描述与轻量摘要。
struct SessionSlot {
    descriptor: SessionDescriptor,
    /// Park 时保留的文档摘要，Resume 时作为 reducer 的起点。
    history: ConversationDocument,
    state: SchedulerState,
    priority: Priority,
    entry: Option<Arc<RuntimeEntry>>,
    lease: Option<SlotLease>,
    failure: Option<String>,
    /// 与 Manager 共享的通知句柄。
    ///
    /// 挂在 Slot 上而不是让各调用点自己发布：状态转移散落在十几处（队列提升、崩溃回收、
    /// Park、Stop、TTL 回收……），漏掉任何一处 UI 就会停在一个再也不会更新的状态上。
    /// 让唯一的转移入口自己负责通知，漏发就成了不可能。
    watch: Arc<SchedulerWatch>,
}

impl SessionSlot {
    /// 受检状态转移。非法转移不改状态，只报错——静默改状态会让槽位回收漏掉一条分支。
    fn transition(&mut self, next: SchedulerState) -> Result<(), IllegalTransition> {
        if self.state == next {
            return Ok(());
        }
        if !self.state.can_transition_to(next) {
            return Err(IllegalTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.watch.publish();
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct SchedulerCounters {
    cold_starts: u64,
    warm_resumes: u64,
    warm_parks: u64,
    idle_reaped: u64,
}

/// 调度器的全部可变状态，统一由一把锁保护。
///
/// **锁序约定**：`core` → `RuntimeEntry::state`，永不反向。任何会阻塞的动作
/// （`Client::spawn`、`switch_session`、`client.shutdown`）都必须在放开 `core` 之后做。
struct SchedulerCore {
    sessions: BTreeMap<SessionId, SessionSlot>,
    queue: WaitQueue,
    warm: VecDeque<WarmRuntime>,
    next_session_id: u64,
    counters: SchedulerCounters,
    /// 已经从会话上摘下、但进程可能还没退干净的运行槽。
    ///
    /// 终态是在 `shutdown` **之前**发布的（R22 刻意如此：崩溃要立刻可见）。一看到终态
    /// 就把运行槽还回去，新会话会在旧进程还活着的时候补位，`total_runtime_slots` 这条
    /// 硬上限就成了空话。这些 lease 先挂在这里，等 `RuntimeEntry::is_released` 为真
    /// 再真正归还 —— 每次 `tick()` 都会清理一遍。
    draining: Vec<(Arc<RuntimeEntry>, SlotLease)>,
}

/// 唤醒 reaper 线程用的停止信号。
struct Reaper {
    stop: Mutex<bool>,
    wake: Condvar,
}

struct ManagerInner {
    next_runtime_id: AtomicU64,
    /// 调度器修订号与订阅者名册。
    watch: Arc<SchedulerWatch>,
    /// R21 兼容通道：`start_fresh` / `start_session` / `stop_user` 维护的唯一活跃会话。
    active_user: Mutex<Option<SessionId>>,
    maintenance: MaintenanceGate,
    tuning: RuntimeTuning,
    limits: SchedulerLimits,
    slots: SlotPool,
    clock: Arc<dyn Clock>,
    core: Mutex<SchedulerCore>,
    /// `None` 表示测试构造：TTL 与队列推进完全由显式 `tick()` 驱动，避免后台线程
    /// 抢在断言之前改状态。
    reaper: Option<Arc<Reaper>>,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        let Some(reaper) = self.reaper.as_ref() else {
            return;
        };
        *reaper.stop.lock().unwrap() = true;
        reaper.wake.notify_all();
    }
}

struct MaintenanceGate {
    limit: usize,
    state: Mutex<usize>,
    ready: Condvar,
}

struct MaintenancePermit<'a> {
    gate: &'a MaintenanceGate,
}

impl MaintenanceGate {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            state: Mutex::new(0),
            ready: Condvar::new(),
        }
    }

    fn acquire(&self) -> MaintenancePermit<'_> {
        let mut active = self.state.lock().unwrap();
        while *active >= self.limit {
            active = self.ready.wait(active).unwrap();
        }
        *active += 1;
        MaintenancePermit { gate: self }
    }
}

impl Drop for MaintenancePermit<'_> {
    fn drop(&mut self) {
        let mut active = self.gate.state.lock().unwrap();
        *active -= 1;
        self.gate.ready.notify_one();
    }
}

struct RuntimeState {
    epoch: u64,
    revision: u64,
    next_effect_sequence: u64,
    effects: EffectBuffer,
    reducer: LiveSessionReducer,
    startup_diagnostic: Option<String>,
    client: Option<Client>,
    calibration_path: Arc<Mutex<Option<PathBuf>>>,
    agent_dir: Option<PathBuf>,
    activity_generation: u64,
    replacing: bool,
    stopped: bool,
    terminal: Option<TerminalState>,
}

struct RuntimeEntry {
    id: RuntimeId,
    /// 拥有本 Runtime 的会话。`RuntimeId` 每次 Resume 都会变，`SessionId` 不会。
    session: SessionId,
    state: Mutex<RuntimeState>,
    subscribers: Mutex<Vec<Sender<Dirty>>>,
    /// 固定线程的命令 Actor；`dispatch` 等只投递作业，绝不新建线程。
    actor: Actor,
    tuning: RuntimeTuning,
    /// 存活的事件 pump 线程数（每个 epoch 至多一个）。
    live_pumps: Arc<AtomicUsize>,
    /// 还有多少条在途作业攥着本 Runtime 的进程句柄。
    ///
    /// `restart_with_tools` 会把 `state.client` 置成 `None`、把旧 client 交给一条控制
    /// 作业。此时 `shutdown_entry` 什么也关不掉，光看它自己无法判断进程是否真的没了。
    /// 计数由**闭包捕获**的守卫维护：作业跑完固然会减，作业还在队列里就被 `close()`
    /// 丢掉时同样会减（闭包被 drop，捕获的守卫跟着 drop）—— 这一点是关键，
    /// 「在闭包体内创建守卫」恰恰盖不住被丢弃那条路。
    client_owners: Arc<AtomicUsize>,
    /// pump 在被 detach 唤醒时，手上那一帧还压着未投递的事件。
    ///
    /// 那一帧已经无处可去（entry 此时已被 Park 置 `stopped`，`publish` 会被 fence 掉）。
    /// 若其中含扩展 UI 请求，pi 里那个扩展可能正等着回应 —— 这样的进程绝不能进 warm pool
    /// 交给下一个会话。正文增量丢一点无妨：pi 自己的会话文件才是权威，Resume 后的
    /// 落盘校准会把它补回来。
    pump_lost_events: Arc<AtomicBool>,
    /// 进程是否已经彻底释放（`shutdown` 已返回）。
    ///
    /// 终态是在 `shutdown` **之前**发布的（R22 刻意如此：崩溃要立刻可见，不能等
    /// grace period）。调度器因此不能一看到终态就归还运行槽 —— 那一刻旧进程可能还活着，
    /// 新会话补位就突破了常驻上限。
    released: Arc<AtomicBool>,
    /// 当前 pump 那条订阅的断开句柄。
    ///
    /// Park 需要在**保留 pi 进程**的前提下结束 pump，而 pump 阻塞在 `recv` 上；
    /// 只有拿到这个句柄才能 `detach()` 把它叫醒。
    events: Mutex<Option<EventDetach>>,
    /// **这个进程实际是用什么参数起来的**。
    ///
    /// 不能拿 `SessionSlot::descriptor` 顶替：`restart_with_tools` 会在调度器背后换掉
    /// 工具预设与二进制，Slot 上那份随即过期。用过期的预设去算 `WarmKey`，一个
    /// 实际带 `--tools full` 的进程会被当成 ReadOnly 放进池子，再被另一个 ReadOnly
    /// 会话接管——那是把写权限跨会话漏出去。
    ///
    /// **锁序：`descriptor` 永远先于 `state`。** 两者会在同一段临界区里被一起持有
    /// （换进程时要原子地更新参数与状态），反序就是死锁。
    descriptor: Mutex<SessionDescriptor>,
}

impl RuntimeEntry {
    fn new_state(
        history: ConversationDocument,
        tuning: RuntimeTuning,
        startup_diagnostic: Option<String>,
        client: Option<Client>,
        calibration_path: Arc<Mutex<Option<PathBuf>>>,
        agent_dir: Option<PathBuf>,
    ) -> RuntimeState {
        RuntimeState {
            epoch: 1,
            revision: 1,
            next_effect_sequence: 0,
            effects: EffectBuffer::new(tuning.effects),
            reducer: LiveSessionReducer::new(history),
            startup_diagnostic,
            client,
            calibration_path,
            agent_dir,
            activity_generation: 0,
            replacing: false,
            stopped: false,
            terminal: None,
        }
    }

    #[cfg(test)]
    fn test_entry(id: RuntimeId, history: ConversationDocument) -> Arc<Self> {
        Self::test_entry_with(
            id,
            history,
            RuntimeTuning::from_limits(&RuntimeLimits::default()),
        )
    }

    #[cfg(test)]
    fn test_entry_with(
        id: RuntimeId,
        history: ConversationDocument,
        tuning: RuntimeTuning,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            session: SessionId(0),
            state: Mutex::new(Self::new_state(
                history,
                tuning,
                None,
                None,
                Arc::new(Mutex::new(None)),
                None,
            )),
            subscribers: Mutex::new(Vec::new()),
            actor: Actor::new(id.get(), tuning.actor),
            tuning,
            live_pumps: Arc::new(AtomicUsize::new(0)),
            client_owners: Arc::new(AtomicUsize::new(0)),
            pump_lost_events: Arc::new(AtomicBool::new(false)),
            released: Arc::new(AtomicBool::new(false)),
            events: Mutex::new(None),
            descriptor: Mutex::new(SessionDescriptor {
                binary: PathBuf::new(),
                cwd: PathBuf::new(),
                session_path: None,
                tool_preset: ToolPreset::Inherit,
                agent_dir: None,
            }),
        })
    }

    fn snapshot(&self) -> SessionSnapshot {
        let mut state = self.state.lock().unwrap();
        SessionSnapshot {
            runtime_id: self.id,
            epoch: state.epoch,
            revision: state.revision,
            document: Arc::new(state.reducer.document()),
            phase: state.reducer.phase(),
            steering_queue_len: state.reducer.steering_queue().len(),
            follow_up_queue_len: state.reducer.follow_up_queue().len(),
            startup_diagnostic: state.startup_diagnostic.clone(),
            effects: state.effects.snapshot().into(),
            terminal: state.terminal.clone(),
            backpressure: BackpressureStats {
                queued_commands: self.actor.queued(Channel::Command),
                queued_controls: self.actor.queued(Channel::Control),
                ..state.effects.stats()
            },
        }
    }

    fn publish(&self, state: &mut RuntimeState, kind: RuntimeEffectKind) {
        state.next_effect_sequence = state.next_effect_sequence.wrapping_add(1);
        let effect = RuntimeEffect {
            sequence: state.next_effect_sequence,
            epoch: state.epoch,
            kind,
        };
        // 有界缓存按语义分级回收：终态另存 `state.terminal`，不依赖 effect 流。
        state.effects.push(effect);
        self.mark_dirty(state);
    }

    /// 把作业投递给 Actor，并在队列拒绝时留下可见计数。
    fn enqueue<F>(
        &self,
        state: &mut RuntimeState,
        channel: Channel,
        key: Option<JobKey>,
        job: F,
    ) -> Result<(), QueueError>
    where
        F: FnOnce() + Send + 'static,
    {
        match self.actor.push(channel, key, job) {
            Ok(()) => Ok(()),
            Err(error) => {
                match key {
                    // 内部维护作业（元数据刷新 / 落盘校准）失败不打扰用户，只记账。
                    Some(_) => state.effects.record_dropped_job(),
                    None => state.effects.record_rejected_command(),
                }
                Err(error)
            }
        }
    }

    /// 记录权威终态。首个终态优先，避免后续噪声覆盖真正的失败原因。
    fn set_terminal(&self, state: &mut RuntimeState, terminal: TerminalState) {
        if state.terminal.is_none() {
            state.terminal = Some(terminal);
        }
    }

    fn mark_dirty(&self, state: &mut RuntimeState) {
        state.revision = state.revision.wrapping_add(1);
        let dirty = Dirty {
            runtime_id: self.id,
            epoch: state.epoch,
            revision: state.revision,
        };
        self.subscribers
            .lock()
            .unwrap()
            .retain(|subscriber| subscriber.send(dirty).is_ok());
    }

    /// 本 Runtime 当前存活的常驻线程数（Actor worker + 事件 pump）。
    ///
    /// 它是固定预算，不随 dispatch / follow-up 次数增长 —— R22 的核心不变量。
    fn live_threads(&self) -> usize {
        self.actor.live_workers() + self.live_pumps.load(Ordering::Acquire)
    }

    /// pump 是否丢弃过一整帧未投递的事件。
    fn pump_lost_events(&self) -> bool {
        self.pump_lost_events.load(Ordering::Acquire)
    }

    /// 标记进程已彻底释放。
    fn mark_released(&self) {
        self.released.store(true, Ordering::Release);
    }

    /// 进程是否已经彻底释放：拆除流程走完了，**且**没有任何在途作业还攥着句柄。
    fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire) && self.client_owners.load(Ordering::Acquire) == 0
    }

    /// 权威终态的廉价读取。
    ///
    /// 调度器在持 `core` 锁时会逐个 Runtime 查终态，不能用 `snapshot()`——那会克隆整份
    /// 文档与 effect 流，把调度锁的持有时间拖到与会话长度成正比。
    fn terminal_state(&self) -> Option<TerminalState> {
        self.state.lock().unwrap().terminal.clone()
    }

    /// 断开当前 pump 的订阅，让它在下一次 `recv` 立刻退出。**不动进程**。
    fn detach_events(&self) {
        if let Some(events) = self.events.lock().unwrap().take() {
            events.detach();
        }
    }

    fn client_for_epoch(&self, epoch: u64) -> Result<Client, String> {
        let state = self.state.lock().unwrap();
        if state.epoch != epoch || state.stopped {
            return Err("runtime epoch 已失效".to_owned());
        }
        state
            .client
            .clone()
            .ok_or_else(|| "runtime 已停止".to_owned())
    }
}

impl Drop for RuntimeEntry {
    fn drop(&mut self) {
        // 兜底关闭 worker。注意这**不是**完整保证：引用链是
        // `RuntimeEntry -> Actor -> Queue -> 排队作业 -> Arc<RuntimeEntry>`，
        // 只要队列里还有作业，强引用就不会归零、本 Drop 也不会执行。真正的保证来自
        // 所有终止路径（`stop_user` / `start_user` 替换 / `fail_runtime` /
        // `publish_tool_restart_failure`）显式调用 `actor.close()`。
        self.actor.close();
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    entry: Arc<RuntimeEntry>,
}

impl SessionHandle {
    pub fn runtime_id(&self) -> RuntimeId {
        self.entry.id
    }

    /// 拥有本 Runtime 的会话身份。
    ///
    /// 与 [`SessionHandle::runtime_id`] 的区别是 R23 的关键：一次 Park/Resume 会换掉
    /// `RuntimeId`（可能换进程、也可能接管另一个热进程），但 `SessionId` 恒定。
    /// 需要跨 Park/Resume 记住的东西（草稿、滚动位置）必须挂在它上面。
    pub fn session_id(&self) -> SessionId {
        self.entry.session
    }

    /// 本 Runtime 当前存活的常驻线程数（Actor worker + 事件 pump）。
    pub fn live_thread_count(&self) -> usize {
        self.entry.live_threads()
    }

    /// Runtime 是否已经静止：队列里没有作业，也没有正在执行的作业。
    ///
    /// 这是 Park 能否**把进程留给 warm pool** 的前提：正在跑的作业手里攥着一个
    /// `Client` 克隆，会和下一个会话的 `switch_session` 撞在同一个内核上。
    /// 启动后的元数据刷新（`get_commands` / `get_state` / models / tree）正是这样一批
    /// 作业，真实 pi 上要跑好几秒 —— 所以「刚起来就 Park」基本只会退化成优雅停机，
    /// 会话仍然正确地进入 `Parked`，只是下次 Resume 得付一次冷启动。
    ///
    /// 想稳定拿到热进程复用，调用方应等本方法为真再 Park。
    pub fn is_quiescent(&self) -> bool {
        self.entry.actor.is_idle()
    }

    /// 当前绑定的 pi 进程 pid。
    ///
    /// 这是「Resume 到底复用了热进程还是冷启动」的客观判据：复用时 pid 与 Park 前相同，
    /// 冷启动时必然不同。
    pub fn process_id(&self) -> Option<u32> {
        self.entry
            .state
            .lock()
            .unwrap()
            .client
            .as_ref()
            .and_then(Client::pid)
    }

    /// 本 Runtime 固化后的命令队列参数（已收敛为生效值）。
    pub fn actor_limits(&self) -> ActorLimits {
        self.entry.actor.limits()
    }

    /// 本 Runtime 固化后的 effect 缓存上限（已收敛为生效值）。
    pub fn effect_limits(&self) -> EffectLimits {
        self.entry.tuning.effects
    }

    /// 本 Runtime 的合帧窗口（已 clamp 进 16–33ms）。
    pub fn event_frame(&self) -> Duration {
        self.entry.tuning.event_frame
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.entry.snapshot()
    }

    pub fn subscribe_dirty(&self) -> Receiver<Dirty> {
        let (tx, rx) = mpsc::channel();
        self.entry.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn refresh_metadata(&self) {
        let entry = self.entry.clone();
        let mut state = entry.state.lock().unwrap();
        if state.stopped {
            return;
        }
        let epoch = state.epoch;
        let Some(client) = state.client.clone() else {
            return;
        };
        let agent_dir = state.agent_dir.clone();
        let job_entry = entry.clone();
        let _ = entry.enqueue(
            &mut state,
            Channel::Command,
            Some(JobKey::Metadata),
            move || {
                let commands = load_commands(&client);
                publish_if_current(
                    &job_entry,
                    epoch,
                    RuntimeEffectKind::CommandsLoaded(commands),
                );
                let controls = load_controls(&client, agent_dir.as_deref());
                let mut state = job_entry.state.lock().unwrap();
                if state.epoch != epoch || state.stopped {
                    return;
                }
                if let Ok(controls) = &controls {
                    apply_controls_identity(&mut state, controls);
                }
                job_entry.publish(&mut state, RuntimeEffectKind::ControlsLoaded(controls));
            },
        );
    }

    /// 回收 UI 已消费的 effect。
    ///
    /// UI 每帧应用完 Snapshot 后必须调用，否则缓存只能靠背压淘汰来保持有界。
    pub fn ack_effects(&self, epoch: u64, sequence: u64) {
        let mut state = self.entry.state.lock().unwrap();
        state.effects.ack(epoch, sequence);
    }

    pub fn dispatch(
        &self,
        intent: RpcIntent,
        submission: Option<ComposerSubmission>,
        mode: ComposerMode,
    ) -> Result<(), String> {
        self.dispatch_with_timeout(intent, submission, mode, request_timeout(intent))
    }

    fn dispatch_with_timeout(
        &self,
        intent: RpcIntent,
        submission: Option<ComposerSubmission>,
        mode: ComposerMode,
        timeout: Duration,
    ) -> Result<(), String> {
        // 提交可能带上百 KB 的 base64 图片，命令构造放在锁外，避免拖住 UI 的 Snapshot 读取。
        let command = dispatch_command(intent, submission.as_ref(), mode);
        let mut state = self.entry.state.lock().unwrap();
        if state.stopped {
            return Err("runtime 已停止".to_owned());
        }
        let epoch = state.epoch;
        let pending_activity_generation = (intent != RpcIntent::Abort
            && state.reducer.phase() != LivePhase::Running)
            .then_some(state.activity_generation);
        let client = state
            .client
            .clone()
            .ok_or_else(|| "runtime 已停止".to_owned())?;
        let entry = self.entry.clone();
        // Abort 走控制通道：普通通道被长时间交互提交占满时，用户仍然必须能停止。
        let channel = if intent == RpcIntent::Abort {
            Channel::Control
        } else {
            Channel::Command
        };
        // 先投递、成功后才改 reducer —— 队列满时不得在 UI 上留下 Running 假象。
        self.entry
            .enqueue(&mut state, channel, None, move || {
                let result = match client.request(command, timeout) {
                    Ok(response) if response.success => Ok(()),
                    Ok(response) => Err((
                        RequestFailureKind::Rejected,
                        response.error.unwrap_or_else(|| "unknown RPC error".into()),
                    )),
                    Err(error) => Err((RequestFailureKind::Ambiguous, error.to_string())),
                };
                let mut state = entry.state.lock().unwrap();
                if state.epoch != epoch || state.stopped {
                    return;
                }
                if result.is_err() {
                    if intent == RpcIntent::Abort {
                        state.reducer.restore_running_if_stopping();
                    } else if pending_activity_generation == Some(state.activity_generation)
                        && state.reducer.phase() == LivePhase::Running
                    {
                        state.reducer.restore_phase(LivePhase::Idle);
                    }
                }
                entry.publish(
                    &mut state,
                    RuntimeEffectKind::RequestFinished {
                        intent,
                        submission,
                        pending_activity_generation,
                        result,
                    },
                );
            })
            .map_err(QueueError::message)?;
        match intent {
            RpcIntent::Abort => state.reducer.set_stopping(),
            _ => state.reducer.set_running(),
        }
        self.entry.mark_dirty(&mut state);
        Ok(())
    }

    pub fn request_control(
        &self,
        operation: ControlOperation,
        request: ControlRequest,
    ) -> Result<(), String> {
        let mut state = self.entry.state.lock().unwrap();
        if state.stopped {
            return Err("runtime 已停止".to_owned());
        }
        let epoch = state.epoch;
        let client = state
            .client
            .clone()
            .ok_or_else(|| "runtime 已停止".to_owned())?;
        let agent_dir = state.agent_dir.clone();
        let entry = self.entry.clone();
        self.entry
            .enqueue(&mut state, Channel::Control, None, move || {
                let result = execute_control(&client, request, agent_dir.as_deref());
                let mut state = entry.state.lock().unwrap();
                if state.epoch != epoch || state.stopped {
                    return;
                }
                if let Ok(outcome) = &result {
                    apply_control_runtime_state(&mut state, outcome);
                }
                entry.publish(
                    &mut state,
                    RuntimeEffectKind::ControlFinished { operation, result },
                );
            })
            .map_err(QueueError::message)?;
        Ok(())
    }

    pub fn respond_extension_ui(
        &self,
        epoch: u64,
        response: ExtensionUiResponse,
    ) -> Result<(), String> {
        self.entry
            .client_for_epoch(epoch)?
            .send_extension_ui_response(&response)
            .map_err(|error| error.to_string())
    }

    pub fn restart_with_tools(
        &self,
        binary: PathBuf,
        session_path: Option<PathBuf>,
        cwd: PathBuf,
        history: ConversationDocument,
        preset: ToolPreset,
    ) -> Result<(), String> {
        // 换进程等于换启动参数：调度器必须拿到新的 binary / cwd / session / 工具预设，
        // 否则 Park 会用过期参数算 `WarmKey`，把一个高权限进程当低权限的放进池子。
        //
        // **锁序：`descriptor` 永远先于 `state`**（见 `RuntimeEntry::descriptor` 注释）。
        // 这一段刻意放在拿 `state` 之前，否则与 `capture_runtime_state` 正好互为反序，
        // 一次「停会话」撞上一次「换工具预设」就会互等到死。
        let restarted_descriptor = SessionDescriptor {
            binary: binary.clone(),
            cwd: cwd.clone(),
            session_path: session_path.clone(),
            tool_preset: preset,
            agent_dir: self.entry.descriptor.lock().unwrap().agent_dir.clone(),
        };
        let mut state = self.entry.state.lock().unwrap();
        if state.stopped {
            return Err("runtime 已停止".to_owned());
        }
        let old_epoch = state.epoch;
        // 先把作业投递出去，成功后才摘掉旧 client —— 队列满绝不能留下没有 client 的僵死 Runtime。
        let old_client = state
            .client
            .clone()
            .ok_or_else(|| "runtime 已停止".to_owned())?;
        let entry = self.entry.clone();
        // 旧 client 即将只由这条作业持有；从现在起计数，直到闭包被执行完或被丢弃。
        let owner = ClientOwnerGuard::acquire(&self.entry);
        let queued = self
            .entry
            .enqueue(&mut state, Channel::Control, None, move || {
                // `owner` 由闭包捕获：无论本作业跑完、提前 return、panic，还是压根没被
                // 执行就被 `close()` 丢掉，计数都会归零。
                let _owner = owner;
                let shutdown = old_client.shutdown().map_err(|error| error.to_string());
                if let Err(error) = shutdown {
                    publish_tool_restart_failure(&entry, old_epoch, preset, error);
                    return;
                }
                let (mut config, diagnostic) =
                    active_session_config(binary, session_path, cwd, preset);
                let agent_dir = entry.state.lock().unwrap().agent_dir.clone();
                if let Some(agent_dir) = agent_dir.as_ref() {
                    config.env.push((
                        pi_data::AGENT_DIR_ENV.into(),
                        agent_dir.as_os_str().to_owned(),
                    ));
                }
                clamp_manager_config(&mut config, entry.tuning);
                let calibration_path = Arc::new(Mutex::new(config.initial_session.clone()));
                let client = match Client::spawn(config) {
                    Ok(client) => client,
                    Err(error) => {
                        publish_tool_restart_failure(&entry, old_epoch, preset, error.to_string());
                        return;
                    }
                };
                let events = client.subscribe();
                let new_epoch;
                {
                    // 同一条锁序：descriptor 先于 state。两把锁一起拿，保证「新进程装上」
                    // 与「权威参数更新」对外是一次原子变更。
                    let mut descriptor = entry.descriptor.lock().unwrap();
                    let mut state = entry.state.lock().unwrap();
                    if state.epoch != old_epoch || state.stopped {
                        drop(state);
                        drop(descriptor);
                        let _ = client.shutdown();
                        // 释放标记由 `ReleaseOnExit` 在 return 时统一置位。
                        return;
                    }
                    state.epoch = state.epoch.wrapping_add(1);
                    new_epoch = state.epoch;
                    state.reducer = LiveSessionReducer::new(history);
                    state.activity_generation = 0;
                    state.replacing = false;
                    state.startup_diagnostic = diagnostic;
                    state.calibration_path = calibration_path.clone();
                    state.client = Some(client);
                    // 只有真正换成功了才改权威 descriptor：失败路径上旧进程仍在，
                    // 旧参数依然是事实。
                    *descriptor = restarted_descriptor;
                    entry.publish(&mut state, RuntimeEffectKind::ExtensionUiReset);
                    entry.publish(
                        &mut state,
                        RuntimeEffectKind::ToolRestartFinished {
                            preset,
                            result: Ok(()),
                        },
                    );
                }
                spawn_event_pump(entry.clone(), new_epoch, calibration_path, events);
                SessionHandle { entry }.refresh_metadata();
            });
        queued.map_err(QueueError::message)?;
        // 入队成功后才进入替换态；此前的所有 early return 都保持调用前状态不变。
        state.replacing = true;
        state.client = None;
        Ok(())
    }
}

/// 调度器为一次启动准备好的全部输入。
struct Admission {
    descriptor: SessionDescriptor,
    history: ConversationDocument,
    /// 运行槽凭证：从这一刻起槽位就被占住，失败路径靠 Drop 归还。
    lease: SlotLease,
    /// `Some` 表示复用池内热进程（`switch_session` 首选路径），`None` 表示冷启动。
    warm: Option<Client>,
}

/// 一次准入尝试的结论。
enum Admitted {
    /// 会话已经在跑，直接把现有 Handle 还回去。
    AlreadyRunning(SessionHandle),
    /// 抢到运行槽，可以开始启动。
    Start(Admission),
    /// 没抢到运行槽，已进入公平队列。
    Queued,
    /// 常驻槽被不可复用的热进程占满：先在锁外把它关掉，再重试准入。
    Evict(WarmRuntime),
}

/// reaper 的轮询间隔。
///
/// 取 TTL 的四分之一，保证过期热进程最多多活 25% 的 TTL；再夹进 [200ms, 5s]，
/// 让极短 TTL 不至于把 CPU 烧在轮询上、极长 TTL 也仍能及时发现崩溃的 Runtime。
fn reaper_interval(idle_ttl: Duration) -> Duration {
    (idle_ttl / 4).clamp(Duration::from_millis(200), Duration::from_secs(5))
}

impl RuntimeManager {
    pub fn new(limits: RuntimeLimits) -> Self {
        Self::build(limits, Arc::new(SystemClock::new()), true)
    }

    /// 注入时钟的测试构造。
    ///
    /// **不启动 reaper 线程**：Idle TTL、aging 与队列提升全部由显式 [`RuntimeManager::tick`]
    /// 驱动。否则后台线程会抢在断言之前改状态，「推进到 TTL 前一格仍在池中」这类
    /// 确定性验收就无从写起。
    pub fn with_test_clock(limits: RuntimeLimits, clock: Arc<dyn Clock>) -> Self {
        Self::build(limits, clock, false)
    }

    fn build(limits: RuntimeLimits, clock: Arc<dyn Clock>, reap: bool) -> Self {
        let scheduler = limits.scheduler.sanitized();
        let manager = Self {
            inner: Arc::new(ManagerInner {
                next_runtime_id: AtomicU64::new(0),
                watch: Arc::new(SchedulerWatch::default()),
                active_user: Mutex::new(None),
                maintenance: MaintenanceGate::new(limits.maintenance_slots),
                tuning: RuntimeTuning::from_limits(&limits),
                limits: scheduler,
                slots: SlotPool::new(scheduler),
                clock,
                core: Mutex::new(SchedulerCore {
                    sessions: BTreeMap::new(),
                    queue: WaitQueue::new(scheduler),
                    warm: VecDeque::new(),
                    next_session_id: 0,
                    counters: SchedulerCounters::default(),
                    draining: Vec::new(),
                }),
                reaper: reap.then(|| {
                    Arc::new(Reaper {
                        stop: Mutex::new(false),
                        wake: Condvar::new(),
                    })
                }),
            }),
        };
        manager.spawn_reaper();
        manager
    }

    /// 整个 Manager 只有这一条后台线程，与 Runtime 数量无关。
    fn spawn_reaper(&self) {
        let Some(reaper) = self.inner.reaper.clone() else {
            return;
        };
        let interval = reaper_interval(self.inner.limits.idle_ttl);
        // 弱引用：reaper 绝不能让 Manager（以及它持有的全部 pi 进程）续命。
        let weak = Arc::downgrade(&self.inner);
        actor::spawn_named("pi-runtime-scheduler-reaper".to_owned(), move || {
            loop {
                {
                    let stop = reaper.stop.lock().unwrap();
                    if *stop {
                        break;
                    }
                    let (stop, _) = reaper.wake.wait_timeout(stop, interval).unwrap();
                    if *stop {
                        break;
                    }
                }
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                RuntimeManager { inner }.tick();
            }
        });
    }

    fn next_runtime_id(&self) -> RuntimeId {
        RuntimeId(self.inner.next_runtime_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// 当前调度器有界参数（已收敛为生效值）。
    pub fn scheduler_limits(&self) -> SchedulerLimits {
        self.inner.limits
    }

    /// 登记一个会话，**不启动任何进程**：新会话直接是 `Parked`。
    ///
    /// 「创建 20 个 Session，常驻 pi 不超上限」正是靠这一点成立：登记是纯内存操作。
    pub fn create_session(
        &self,
        descriptor: SessionDescriptor,
        history: ConversationDocument,
    ) -> SessionId {
        let mut core = self.inner.core.lock().unwrap();
        core.next_session_id = core.next_session_id.wrapping_add(1);
        let id = SessionId(core.next_session_id);
        core.sessions.insert(
            id,
            SessionSlot {
                descriptor,
                history,
                state: SchedulerState::Parked,
                priority: Priority::default(),
                entry: None,
                lease: None,
                failure: None,
                watch: Arc::clone(&self.inner.watch),
            },
        );
        // 登记本身就是一次可见变化：UI 的标签在这一刻起就该显示 `Parked`。
        self.inner.watch.publish();
        id
    }

    /// 订阅调度器状态变化。
    ///
    /// 通道容量 1 且满帧即丢：通知是**电平触发**的，收到后请重新查询
    /// [`RuntimeManager::session_state`] 等，不要把通知当成事件流累积。
    pub fn subscribe_scheduler(&self) -> SchedulerSubscription {
        self.inner.watch.subscribe()
    }

    /// 调度器修订号；每次会话状态变化 +1。测试用它判断「确实发生过变化」。
    pub fn scheduler_revision(&self) -> u64 {
        self.inner.watch.revision()
    }

    pub fn session_state(&self, session: SessionId) -> Option<SchedulerState> {
        let core = self.inner.core.lock().unwrap();
        core.sessions.get(&session).map(|slot| slot.state)
    }

    pub fn session_handle(&self, session: SessionId) -> Option<SessionHandle> {
        let core = self.inner.core.lock().unwrap();
        core.sessions
            .get(&session)?
            .entry
            .clone()
            .map(|entry| SessionHandle { entry })
    }

    /// 会话最近一次失败的原因（`Failed` 态才有值）。
    pub fn session_failure(&self, session: SessionId) -> Option<String> {
        let core = self.inner.core.lock().unwrap();
        core.sessions.get(&session)?.failure.clone()
    }

    /// 会话当前的描述。
    ///
    /// Runtime 在跑时以**它自己那份**为准：`restart_with_tools` 会在调度器背后换掉工具
    /// 预设与二进制，Slot 上那份要等到 Park / stop 才同步。返回过期的预设会让调用方以为
    /// 一个高权限进程还是只读的。
    pub fn session_descriptor(&self, session: SessionId) -> Option<SessionDescriptor> {
        let core = self.inner.core.lock().unwrap();
        let slot = core.sessions.get(&session)?;
        Some(match slot.entry.as_ref() {
            Some(entry) => entry.descriptor.lock().unwrap().clone(),
            None => slot.descriptor.clone(),
        })
    }

    /// 改一个**当前不占着 pi 进程**的会话的工具预设。
    ///
    /// 占着进程时一律拒绝：进程已经按旧参数起来了，换预设必须重启进程
    /// （[`SessionHandle::restart_with_tools`]）。只改描述会让「描述」与「进程实际
    /// 拥有的权限」分家——R23 审查 P1-1 修的就是这条，这里不能再开一个后门。
    ///
    /// 判据是**状态**而不是 `entry`：[`RuntimeManager::admit`] 进入 `Starting` 时就把
    /// 描述克隆给了正在启动的那个进程，而 `entry` 要到 `finish_start` 才装上。只看
    /// `entry` 会让整个启动窗口都能改预设，结果进程按旧预设起来、UI 显示的却是新预设。
    /// [`SchedulerState::holds_process`] 正是「这个状态占着（或正在占）一个进程」，
    /// 与运行槽会计同源，将来加状态也不必回来补一遍判据。
    pub fn set_session_tool_preset(
        &self,
        session: SessionId,
        tool_preset: ToolPreset,
    ) -> Result<(), String> {
        let mut core = self.inner.core.lock().unwrap();
        let slot = core
            .sessions
            .get_mut(&session)
            .ok_or_else(|| "会话不存在".to_owned())?;
        if slot.state.holds_process() {
            return Err(if slot.state == SchedulerState::Running {
                "会话正在运行，改工具预设需要重启进程".to_owned()
            } else {
                format!("会话正处于 {} 状态，请稍候重试", slot.state.label())
            });
        }
        slot.descriptor.tool_preset = tool_preset;
        Ok(())
    }

    pub fn scheduler_report(&self) -> SchedulerReport {
        let core = self.inner.core.lock().unwrap();
        // 槽位计数必须在调度锁内取：先读槽位再拿锁会拼出一份「状态与槽位互相矛盾」的
        // 报表（例如 running 已经归零而 user 槽还显示占用）。
        let mut report = SchedulerReport {
            slots: self.inner.slots.counts(),
            warm: core.warm.len(),
            cold_starts: core.counters.cold_starts,
            warm_resumes: core.counters.warm_resumes,
            warm_parks: core.counters.warm_parks,
            idle_reaped: core.counters.idle_reaped,
            ..SchedulerReport::default()
        };
        for slot in core.sessions.values() {
            match slot.state {
                SchedulerState::Parked => report.parked += 1,
                SchedulerState::Queued => report.queued += 1,
                SchedulerState::Starting => report.starting += 1,
                SchedulerState::Running => report.running += 1,
                SchedulerState::Stopping => report.stopping += 1,
                SchedulerState::Failed => report.failed += 1,
                // Session 永远不会是池内热进程；热进程单独统计在 `warm` 里。
                SchedulerState::IdleWarm => {}
            }
        }
        report.draining = core.draining.len();
        // **常驻数直接取运行槽计数**，不去累加各个状态桶。
        //
        // 状态桶会在拆除窗口里短暂地谁都不认领这个进程：Park 兜底停机、warm 淘汰、
        // TTL 回收都是「先从集合里摘走、再到锁外 shutdown」，那一瞬间按桶求和会报 0，
        // 而进程明明还在退出。lease 才是从 spawn 前占用、到 shutdown 后归还的权威计数。
        report.resident_pi = report.slots.resident;
        report
    }

    /// 请求运行一个会话。
    ///
    /// 抢到运行槽返回 `Ok(Some(handle))`；抢不到则进入公平队列并返回 `Ok(None)`，
    /// 后续由 [`RuntimeManager::tick`] 提升。队列已满时返回 `Err` 且**不改动**会话状态。
    pub fn request_run(
        &self,
        session: SessionId,
        priority: Priority,
    ) -> Result<Option<SessionHandle>, String> {
        self.tick();
        self.admit_and_start(session, priority)
    }

    /// 只更新一个会话的调度优先级，**不做任何状态转移、也不起进程**。
    ///
    /// 为什么单独有这一步：[`RuntimeManager::request_run`] 的第一件事是 `tick()`，
    /// 而 `tick()` 里的 `promote_queued` 是按**队列里现有的**优先级挑人的。
    /// 「切到一个 Queued 标签」如果直接调 `request_run`，这次 tick 会先用旧优先级
    /// 把刚空出来的槽让给别的后台会话，自己才在随后的 `admit` 里被抬成 FOREGROUND ——
    /// 抬了个寂寞。调用方要先落优先级、再申请。
    ///
    /// 队列侧沿用 [`WaitQueue::push`] 的既有规则：对已排队条目取 `min`（只升不降），
    /// 且不刷新 `enqueued_at`，因此反复调用不会把 aging 玩坏。
    pub fn reprioritize(&self, session: SessionId, priority: Priority) {
        let now = self.inner.clock.now();
        let mut core = self.inner.core.lock().unwrap();
        let Some(slot) = core.sessions.get_mut(&session) else {
            return;
        };
        slot.priority = priority;
        if core.queue.contains(session) {
            let _ = core.queue.push(session, priority, now);
        }
    }

    fn admit_and_start(
        &self,
        session: SessionId,
        priority: Priority,
    ) -> Result<Option<SessionHandle>, String> {
        // 每一轮要么给出结论，要么淘汰掉一个热进程；因此最多「池容量 + 1」轮。
        for _ in 0..=self.inner.limits.warm_idle.saturating_add(1) {
            let admitted = {
                let mut core = self.inner.core.lock().unwrap();
                self.admit(&mut core, session, priority)?
            };
            match admitted {
                Admitted::AlreadyRunning(handle) => return Ok(Some(handle)),
                Admitted::Queued => return Ok(None),
                Admitted::Start(admission) => {
                    return self.finish_start(session, admission).map(Some);
                }
                // 已经出了锁，关进程不会卡住调度器；`WarmRuntime` 落地时归还常驻槽。
                Admitted::Evict(warm) => {
                    let _ = warm.client.shutdown();
                }
            }
        }
        Err("调度器无法为该会话腾出运行槽，请稍后重试".to_owned())
    }

    fn admit(
        &self,
        core: &mut SchedulerCore,
        session: SessionId,
        priority: Priority,
    ) -> Result<Admitted, String> {
        let slot = core
            .sessions
            .get_mut(&session)
            .ok_or_else(|| "会话不存在".to_owned())?;
        // 会话侧记录的是「调用方最近一次的意图」；队列侧另有一条不降级规则
        // （[`WaitQueue::push`] 对已排队条目取 `min`），避免等待中的条目被反复降级。
        slot.priority = priority;
        match slot.state {
            SchedulerState::Running => {
                return match slot.entry.clone() {
                    Some(entry) => Ok(Admitted::AlreadyRunning(SessionHandle { entry })),
                    // Running 却没有 entry 属于内部不变量被破坏，宁可报错也不静默继续。
                    None => Err("会话状态不一致：Running 但没有运行时".to_owned()),
                };
            }
            SchedulerState::Starting | SchedulerState::Stopping => {
                return Err(format!(
                    "会话正处于 {} 状态，请稍候重试",
                    slot.state.label()
                ));
            }
            SchedulerState::IdleWarm => {
                return Err("会话状态不一致：Session 不会处于 IdleWarm".to_owned());
            }
            SchedulerState::Parked | SchedulerState::Queued | SchedulerState::Failed => {}
        }
        let descriptor = slot.descriptor.clone();
        let history = slot.history.clone();
        let key = WarmKey::of(&descriptor);
        // 会话文件必须真实存在：`switch_session` 切不到一个还没落盘的 fresh 会话。
        let switchable = descriptor
            .session_path
            .as_ref()
            .is_some_and(|path| path.is_file());

        // ① 首选：复用参数一致的热进程。
        if switchable && let Some(index) = core.warm.iter().position(|warm| warm.key == key) {
            let warm = core.warm.remove(index).expect("index from position");
            match warm.lease.upgrade_to_user() {
                Ok(lease) => {
                    let client = warm.client;
                    Self::mark_starting(core, session)?;
                    return Ok(Admitted::Start(Admission {
                        descriptor,
                        history,
                        lease,
                        warm: Some(client),
                    }));
                }
                Err(lease) => {
                    // 用户并发已满：热进程原样放回池里，本会话转入排队。
                    core.warm.push_back(WarmRuntime {
                        client: warm.client,
                        key: warm.key,
                        idle_since: warm.idle_since,
                        lease,
                    });
                }
            }
        }

        // ② 回退：冷启动。
        if let Some(lease) = self.inner.slots.try_acquire_user() {
            Self::mark_starting(core, session)?;
            return Ok(Admitted::Start(Admission {
                descriptor,
                history,
                lease,
                warm: None,
            }));
        }

        // ③ 常驻槽被「用不上的」热进程占满，而用户并发还有余量：热进程必须给真实需求让路。
        //    淘汰**最久没被用过**的那一个，而不是队首 —— 复用失败时热进程会被原样放回
        //    队尾，池内顺序因此不能当成空闲时长顺序。
        if self.inner.slots.counts().user < self.inner.limits.user_session_slots {
            let oldest = core
                .warm
                .iter()
                .enumerate()
                .min_by_key(|(_, warm)| warm.idle_since)
                .map(|(index, _)| index);
            if let Some(index) = oldest
                && let Some(evicted) = core.warm.remove(index)
            {
                return Ok(Admitted::Evict(evicted));
            }
        }

        // ④ 确实没有槽：入队等待。
        let now = self.inner.clock.now();
        core.queue
            .push(session, priority, now)
            .map_err(|full| full.to_string())?;
        let slot = core
            .sessions
            .get_mut(&session)
            .ok_or_else(|| "会话不存在".to_owned())?;
        if let Err(error) = slot.transition(SchedulerState::Queued) {
            core.queue.remove(session);
            return Err(error.to_string());
        }
        Ok(Admitted::Queued)
    }

    fn mark_starting(core: &mut SchedulerCore, session: SessionId) -> Result<(), String> {
        core.queue.remove(session);
        let slot = core
            .sessions
            .get_mut(&session)
            .ok_or_else(|| "会话不存在".to_owned())?;
        slot.transition(SchedulerState::Starting)
            .map_err(|error| error.to_string())
    }

    fn finish_start(
        &self,
        session: SessionId,
        admission: Admission,
    ) -> Result<SessionHandle, String> {
        let Admission {
            descriptor,
            history,
            lease,
            warm,
        } = admission;
        let mut reused_warm = false;
        let mut started = None;
        if let Some(client) = warm {
            match self.adopt_warm(session, client, &descriptor, history.clone()) {
                Ok(entry) => {
                    reused_warm = true;
                    started = Some(entry);
                }
                // 热进程坏了不该让整次 Resume 失败：`adopt_warm` 已清理掉它，这里退回冷启动。
                Err(_) => started = None,
            }
        }
        let entry = match started {
            Some(entry) => entry,
            None => match self.cold_start(session, &descriptor, history) {
                Ok(entry) => entry,
                Err(error) => {
                    self.fail_session(session, error.clone());
                    return Err(error);
                }
            },
        };

        let mut core = self.inner.core.lock().unwrap();
        let Some(slot) = core.sessions.get_mut(&session) else {
            drop(core);
            // 启动期间会话被移除：新建的 Runtime 不能留成孤儿进程。
            shutdown_entry(&entry);
            return Err("会话已被移除".to_owned());
        };
        if let Err(error) = slot.transition(SchedulerState::Running) {
            drop(core);
            shutdown_entry(&entry);
            return Err(error.to_string());
        }
        slot.entry = Some(Arc::clone(&entry));
        slot.lease = Some(lease);
        slot.failure = None;
        if reused_warm {
            core.counters.warm_resumes = core.counters.warm_resumes.wrapping_add(1);
        } else {
            core.counters.cold_starts = core.counters.cold_starts.wrapping_add(1);
        }
        Ok(SessionHandle { entry })
    }

    /// 复用池内热进程：新建一个轻量 Runtime 容器，再用 `switch_session` 把会话切过去。
    ///
    /// 立项文档 § 三要求 Resume 复用而不是重写这条链路 —— 成本从「重起一个约 203MB 的
    /// 进程」降到一次 RPC 往返。
    fn adopt_warm(
        &self,
        session: SessionId,
        client: Client,
        descriptor: &SessionDescriptor,
        history: ConversationDocument,
    ) -> Result<Arc<RuntimeEntry>, String> {
        let session_path = descriptor
            .session_path
            .clone()
            .ok_or_else(|| "fresh 会话没有会话文件，无法复用热进程".to_owned())?;
        let calibration_path = Arc::new(Mutex::new(Some(session_path.clone())));
        let entry = self.new_entry(
            session,
            descriptor.clone(),
            history,
            None,
            Some(client.clone()),
            Arc::clone(&calibration_path),
        );
        // 先订阅再切会话：切换过程中的事件不能漏给 reducer。
        let events = client.subscribe();
        spawn_event_pump(Arc::clone(&entry), 1, calibration_path, events);
        let outcome = execute_control(
            &client,
            ControlRequest::SwitchSession { path: session_path },
            descriptor.agent_dir.as_deref(),
        );
        match outcome {
            Ok(ControlOutcome::Switched(controls)) => {
                {
                    let mut state = entry.state.lock().unwrap();
                    apply_controls_identity(&mut state, &controls);
                    entry.publish(&mut state, RuntimeEffectKind::ControlsLoaded(Ok(controls)));
                }
                SessionHandle {
                    entry: Arc::clone(&entry),
                }
                .refresh_metadata();
                Ok(entry)
            }
            other => {
                // 热进程没能接管：连同它一起关掉，调用方回退冷启动。
                shutdown_entry(&entry);
                Err(match other {
                    Ok(outcome) => format!("热进程未能接管会话：{outcome:?}"),
                    Err(error) => format!("热进程 switch_session 失败：{error}"),
                })
            }
        }
    }

    /// 冷启动：无可复用热进程时的回退路径。
    fn cold_start(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
        history: ConversationDocument,
    ) -> Result<Arc<RuntimeEntry>, String> {
        let (mut config, diagnostic) = active_session_config(
            descriptor.binary.clone(),
            descriptor.session_path.clone(),
            descriptor.cwd.clone(),
            descriptor.tool_preset,
        );
        if let Some(agent_dir) = descriptor.agent_dir.as_ref() {
            config.env.push((
                pi_data::AGENT_DIR_ENV.into(),
                agent_dir.as_os_str().to_owned(),
            ));
        }
        clamp_manager_config(&mut config, self.inner.tuning);
        let calibration_path = Arc::new(Mutex::new(config.initial_session.clone()));
        let client = Client::spawn(config).map_err(|error| error.to_string())?;
        let events = client.subscribe();
        let entry = self.new_entry(
            session,
            descriptor.clone(),
            history,
            diagnostic,
            Some(client),
            Arc::clone(&calibration_path),
        );
        spawn_event_pump(Arc::clone(&entry), 1, calibration_path, events);
        SessionHandle {
            entry: Arc::clone(&entry),
        }
        .refresh_metadata();
        Ok(entry)
    }

    fn new_entry(
        &self,
        session: SessionId,
        descriptor: SessionDescriptor,
        history: ConversationDocument,
        diagnostic: Option<String>,
        client: Option<Client>,
        calibration_path: Arc<Mutex<Option<PathBuf>>>,
    ) -> Arc<RuntimeEntry> {
        let id = self.next_runtime_id();
        let tuning = self.inner.tuning;
        let agent_dir = descriptor.agent_dir.clone();
        Arc::new(RuntimeEntry {
            id,
            session,
            state: Mutex::new(RuntimeEntry::new_state(
                history,
                tuning,
                diagnostic,
                client,
                calibration_path,
                agent_dir,
            )),
            subscribers: Mutex::new(Vec::new()),
            actor: Actor::new(id.get(), tuning.actor),
            tuning,
            live_pumps: Arc::new(AtomicUsize::new(0)),
            client_owners: Arc::new(AtomicUsize::new(0)),
            pump_lost_events: Arc::new(AtomicBool::new(false)),
            released: Arc::new(AtomicBool::new(false)),
            events: Mutex::new(None),
            descriptor: Mutex::new(descriptor),
        })
    }

    fn fail_session(&self, session: SessionId, error: String) {
        let mut core = self.inner.core.lock().unwrap();
        if let Some(slot) = core.sessions.get_mut(&session) {
            let _ = slot.transition(SchedulerState::Failed);
            slot.entry = None;
            // lease 在这里 Drop，运行槽随之归还。
            slot.lease = None;
            slot.failure = Some(error);
        }
    }

    /// Park：让出进程，会话转 `Parked`。
    ///
    /// 优先把进程交给 warm pool（下一次 Resume 就能 `switch_session` 秒接管）；
    /// 池满、会话尚未落盘或作业没能及时收干净时退化为优雅停机。两种落点都是 `Parked`。
    ///
    /// 会话正在执行请求时返回 `Err` 且**不改动任何状态** —— Park 是「让出进程」，
    /// 不是「打断请求」；想强行结束请用 [`RuntimeManager::stop_session`]。
    pub fn park(&self, session: SessionId) -> Result<(), String> {
        // ① 只读地看一眼 Runtime。这一步刻意不改任何状态：下面可能因为会话正忙而拒绝，
        //    那时必须原样返回，不能留下一个已经被改成 Stopping 的半截会话。
        let entry = {
            let mut core = self.inner.core.lock().unwrap();
            let slot = core
                .sessions
                .get_mut(&session)
                .ok_or_else(|| "会话不存在".to_owned())?;
            match slot.state {
                SchedulerState::Parked => return Ok(()),
                SchedulerState::Queued => {
                    slot.transition(SchedulerState::Parked)
                        .map_err(|error| error.to_string())?;
                    core.queue.remove(session);
                    return Ok(());
                }
                SchedulerState::Failed => {
                    slot.transition(SchedulerState::Parked)
                        .map_err(|error| error.to_string())?;
                    slot.failure = None;
                    return Ok(());
                }
                SchedulerState::Running => {}
                other => return Err(format!("会话处于 {} 状态，无法 Park", other.label())),
            }
            slot.entry
                .clone()
                .ok_or_else(|| "会话状态不一致：Running 但没有运行时".to_owned())?
        };

        // ② 在**任何锁之外**给后台作业一点收尾时间。启动/接管后的元数据刷新是一批
        //    在跑的作业，等一下就没了；这一步不改任何状态，等不到也只是转入下面的拒绝。
        wait_for_actor_idle(&entry, PARK_SETTLE_BUDGET);

        // ③ 预留：**在同一把调度锁里**确认会话仍归这个 Runtime、完成 Runtime 侧的
        //    原子封锁、并把运行槽与 Runtime 一起摘下来。
        //
        //    必须是同一把锁：`begin_park` 会先发布 `Stopped` 终态，而槽位此时还是
        //    `Running`；若中途放开调度锁，并发的 `request_run` 会先 `tick()` 把这个
        //    entry 当成"外部停掉的"回收掉、再装上一个新的 Runtime，随后我们回来无条件
        //    改状态，就把那个新 Runtime 连同它的进程一起挤掉了。
        let (entry, lease, parked) = {
            let mut core = self.inner.core.lock().unwrap();
            let slot = core
                .sessions
                .get_mut(&session)
                .ok_or_else(|| "会话不存在".to_owned())?;
            if slot.state != SchedulerState::Running
                || !slot
                    .entry
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                return Err("会话已经被其他操作接管，请重试".to_owned());
            }
            let parked = reserve_park(&entry).map_err(ParkRefusal::message)?;
            slot.transition(SchedulerState::Stopping)
                .map_err(|error| error.to_string())?;
            let entry = slot
                .entry
                .take()
                .ok_or_else(|| "会话状态不一致：Running 但没有运行时".to_owned())?;
            let lease = slot
                .lease
                .take()
                .ok_or_else(|| "会话状态不一致：Running 但没有运行槽".to_owned())?;
            (entry, lease, parked)
        };

        // ④ 锁外拆 Runtime：关 Actor、叫醒 pump。进程本身还活着，去留由下一步决定。
        let parked = finish_park(&entry, parked);

        // ⑤ 落调度器状态。warm 容量在**同一把锁内**复检：并发 Park 各自在锁外判断
        //    「池里还有位置」，会让 warm_idle=1 的池装进两个进程、多占几百 MB。
        let descriptor = entry.descriptor.lock().unwrap().clone();
        let key = WarmKey::of(&descriptor);
        // 未落盘的会话不进池：它的进程手里还攥着一个从没写进磁盘的会话，把这样的进程
        // 交给别人 `switch_session` 是我们没有对真实 pi 验证过的状态。文件探测放在锁外。
        let persisted = parked
            .session_file
            .as_ref()
            .is_some_and(|path| path.is_file());
        let now = self.inner.clock.now();
        let surplus = {
            let mut core = self.inner.core.lock().unwrap();
            if let Some(slot) = core.sessions.get_mut(&session) {
                // 这里不能用 `?`：Runtime 与运行槽已经在我们手上，提前返回会让 lease
                // 先于进程被归还（正是 R23 审查 P2-5 那条）。`Stopping -> Parked`
                // 由上一步的转移保证合法，真出现异常也只能带着状态往下走完回收。
                let _ = slot.transition(SchedulerState::Parked);
                apply_captured_state(
                    slot,
                    &descriptor,
                    parked.history,
                    parked.session_file.clone(),
                );
            }
            let has_room = core.warm.len() < self.inner.limits.warm_idle;
            match parked.client {
                Some(client) if parked.reusable && persisted && has_room => {
                    core.warm.push_back(WarmRuntime {
                        client,
                        key,
                        idle_since: now,
                        // 交还用户会话槽、保留常驻槽：热进程仍然占着内存。
                        lease: lease.downgrade_to_warm(),
                    });
                    core.counters.warm_parks = core.counters.warm_parks.wrapping_add(1);
                    None
                }
                // 进不了池就必须关掉。**lease 跟着进程走**：先归还槽位再关进程的话，
                // 并发的 `request_run` 会在旧进程还没退出时补位，突破常驻上限。
                client => Some((client, lease)),
            }
        };
        if let Some((client, lease)) = surplus {
            if let Some(client) = client {
                let _owner = ClientOwnerGuard::acquire(&entry);
                let _ = client.shutdown();
            }
            entry.mark_released();
            // 运行槽最后归还：`_owner` 已随上面的作用域结束而落地，此刻进程确实没了。
            drop(lease);
        }
        self.tick();
        Ok(())
    }

    /// 停止会话：关掉进程，会话保留在注册表里（转 `Parked`，可再次 `request_run`）。
    pub fn stop_session(&self, session: SessionId) {
        let (owns_teardown, entry) = {
            let mut core = self.inner.core.lock().unwrap();
            core.queue.remove(session);
            match core.sessions.get_mut(&session) {
                // 已经有另一次 stop / park 把 Runtime 摘走并正在关它。这里**什么都不能动**：
                // 再走一遍收尾会把那次调用的 lease 提前归还，甚至在它之后覆盖新 Runtime
                // 的状态。让那次调用自己收口。
                Some(slot) if slot.state == SchedulerState::Stopping && slot.entry.is_none() => {
                    (false, None)
                }
                Some(slot) => {
                    if slot.state != SchedulerState::Parked {
                        let _ = slot.transition(SchedulerState::Stopping);
                    }
                    (true, slot.entry.take())
                }
                None => (false, None),
            }
        };
        if !owns_teardown {
            self.tick();
            return;
        }
        // 先取快照再停机：pi 可能已经把会话落了盘，不收进 Slot 的话，下一次
        // `request_run` 会拿着 `session_path=None` 和最初的历史重开一个新会话 ——
        // 用户的对话就这么凭空消失了。
        let captured = entry.as_ref().map(|entry| capture_runtime_state(entry));
        if let Some(entry) = entry.as_ref() {
            shutdown_entry(entry);
        }
        {
            let mut core = self.inner.core.lock().unwrap();
            let released = core.sessions.get_mut(&session).and_then(|slot| {
                let _ = slot.transition(SchedulerState::Parked);
                slot.failure = None;
                if let Some((descriptor, history, session_file)) = captured {
                    apply_captured_state(slot, &descriptor, history, session_file);
                }
                slot.lease.take()
            });
            // 运行槽跟着进程走。`shutdown_entry` 在有替换作业在跑时不会宣布已释放 ——
            // 那时旧进程还攥在那条作业手里，提前还槽就会被别的会话补位。
            if let Some(lease) = released {
                match entry.as_ref() {
                    Some(entry) if !entry.is_released() => {
                        core.draining.push((Arc::clone(entry), lease));
                    }
                    _ => drop(lease),
                }
            }
        }
        self.tick();
    }

    /// 停止并注销会话。
    pub fn remove_session(&self, session: SessionId) {
        self.stop_session(session);
        // 注销的如果正好是兼容通道的活跃会话，指针必须一起清掉：留着它，
        // `active_user_session()` 会返回一个已经不存在的 id，而 `stop_user` 再也匹配不上。
        {
            let mut active = self.inner.active_user.lock().unwrap();
            if *active == Some(session) {
                *active = None;
            }
        }
        let mut core = self.inner.core.lock().unwrap();
        core.queue.remove(session);
        // 注销不经过 `transition`，得自己发通知，否则 UI 永远看不到「这个会话没了」。
        if core.sessions.remove(&session).is_some() {
            self.inner.watch.publish();
        }
    }

    /// 推进调度器：回收崩溃的 Runtime、按 Idle TTL 回收热进程、提升排队会话。
    ///
    /// 所有耗时动作（关进程、启动进程）都在放开调度锁之后进行。
    pub fn tick(&self) {
        let now = self.inner.clock.now();
        let expired = {
            let mut core = self.inner.core.lock().unwrap();
            // 先收已经退干净的进程的运行槽，再判断谁能开跑。
            core.draining.retain(|(entry, _)| !entry.is_released());
            Self::reap_terminated(&mut core);
            Self::reap_idle_warm(&mut core, now, self.inner.limits.idle_ttl)
        };
        for warm in expired {
            let _ = warm.client.shutdown();
        }
        self.promote_queued(now);
    }

    /// 把已经进入终态的 Runtime 从运行槽上摘下来。
    ///
    /// 崩溃由 pump 线程写进 `SessionSnapshot.terminal`，调度器自己不订阅事件；
    /// 不做这一步，一个崩掉的会话会永久占着运行槽。
    fn reap_terminated(core: &mut SchedulerCore) {
        let mut releases: Vec<(Arc<RuntimeEntry>, SlotLease)> = Vec::new();
        for slot in core.sessions.values_mut() {
            if !matches!(
                slot.state,
                SchedulerState::Running | SchedulerState::Starting
            ) {
                continue;
            }
            let Some(entry) = slot.entry.clone() else {
                continue;
            };
            let Some(terminal) = entry.terminal_state() else {
                continue;
            };
            // 崩溃之前 pi 往往已经把会话落了盘。不在这里把身份与历史收进 Slot，
            // `Failed` 之后的重试就会当成新会话重开，用户丢掉整段对话。
            let (descriptor, history, session_file) = capture_runtime_state(&entry);
            match terminal {
                TerminalState::Failed { error } => {
                    let _ = slot.transition(SchedulerState::Failed);
                    slot.failure = Some(error);
                }
                TerminalState::Stopped => {
                    // 走到这里说明进程在调度器之外被停掉了；补齐状态机与槽位回收。
                    let _ = slot.transition(SchedulerState::Stopping);
                    let _ = slot.transition(SchedulerState::Parked);
                }
            }
            apply_captured_state(slot, &descriptor, history, session_file);
            slot.entry = None;
            // 运行槽跟着进程走：进程还没退干净就先挂进 `draining`，不能直接还回去。
            if let Some(lease) = slot.lease.take() {
                releases.push((entry, lease));
            }
        }
        for (entry, lease) in releases {
            if !entry.is_released() {
                core.draining.push((entry, lease));
            }
        }
    }

    /// 回收空闲超过 TTL 的热进程。
    ///
    /// 逐个判定而不是「从队首取到第一个未过期的为止」：复用失败时热进程会被原样放回
    /// 队尾，池内顺序因此并不等于空闲时长顺序，按顺序短路会漏掉排在后面的过期进程。
    /// 池容量本来就很小（初值 1），全扫的代价可以忽略。
    fn reap_idle_warm(
        core: &mut SchedulerCore,
        now: Duration,
        idle_ttl: Duration,
    ) -> Vec<WarmRuntime> {
        let mut expired = Vec::new();
        let mut retained = VecDeque::with_capacity(core.warm.len());
        while let Some(warm) = core.warm.pop_front() {
            if now.saturating_sub(warm.idle_since) >= idle_ttl {
                expired.push(warm);
            } else {
                retained.push_back(warm);
            }
        }
        core.warm = retained;
        core.counters.idle_reaped = core.counters.idle_reaped.wrapping_add(expired.len() as u64);
        expired
    }

    fn promote_queued(&self, now: Duration) {
        let budget = self.inner.core.lock().unwrap().queue.len();
        for _ in 0..budget {
            // 只 peek 不 pop：真正的出队由 `admit` 在抢到运行槽之后做。先 pop 再放回
            // 会重置 `enqueued_at`，把这条条目辛苦攒下的 aging 一次清零 —— 恰好惩罚
            // 等得最久的那个，公平队列就白做了。
            let candidate = {
                let core = self.inner.core.lock().unwrap();
                if self.inner.slots.counts().user >= self.inner.limits.user_session_slots {
                    return;
                }
                core.queue.peek_next(now)
            };
            let Some(session) = candidate else {
                return;
            };
            let priority = self
                .inner
                .core
                .lock()
                .unwrap()
                .sessions
                .get(&session)
                .map(|slot| slot.priority)
                .unwrap_or_default();
            let _ = self.admit_and_start(session, priority);
            // 没能推进（仍在队列里）就停手，否则同一条目会被反复重试到 budget 用尽。
            if self.session_state(session) == Some(SchedulerState::Queued) {
                return;
            }
        }
    }

    pub fn start_fresh(
        &self,
        binary: PathBuf,
        cwd: PathBuf,
        history: ConversationDocument,
        tool_preset: ToolPreset,
        agent_dir: Option<PathBuf>,
    ) -> Result<SessionHandle, String> {
        self.start_user(binary, None, cwd, history, tool_preset, agent_dir)
    }

    pub fn start_session(
        &self,
        binary: PathBuf,
        session_path: PathBuf,
        cwd: PathBuf,
        history: ConversationDocument,
        tool_preset: ToolPreset,
        agent_dir: Option<PathBuf>,
    ) -> Result<SessionHandle, String> {
        self.start_user(
            binary,
            Some(session_path),
            cwd,
            history,
            tool_preset,
            agent_dir,
        )
    }

    /// R21 起的单会话兼容通道。
    ///
    /// 外部语义与改造前完全一致：**先把新会话拉起来，成功之后才停掉旧的** ——
    /// spawn 失败时用户手里的旧会话必须原样还在。
    fn start_user(
        &self,
        binary: PathBuf,
        session_path: Option<PathBuf>,
        cwd: PathBuf,
        history: ConversationDocument,
        tool_preset: ToolPreset,
        agent_dir: Option<PathBuf>,
    ) -> Result<SessionHandle, String> {
        // 「读 previous → 起新的 → 改 active → 收旧的」整段必须串行。两个窗口并发调用时
        // 各自读到同一个 previous，最后一个覆盖 `active_user`，先起来的那个 Runtime
        // 就变成没人认领、`stop_user` 也停不掉的常驻进程。
        let mut active = self.inner.active_user.lock().unwrap();
        let previous = *active;
        let session = self.create_session(
            SessionDescriptor {
                binary,
                cwd,
                session_path,
                tool_preset,
                agent_dir,
            },
            history,
        );
        match self.start_compat(session, previous) {
            Ok(handle) => {
                *active = Some(session);
                // 关旧进程可能要等几秒，别攥着 active 锁做。
                drop(active);
                if let Some(previous) = previous {
                    self.remove_session(previous);
                }
                Ok(handle)
            }
            Err(error) => {
                // `remove_session` 自己要拿 `active_user` 锁（注销活跃会话时得把指针清掉），
                // 而 std 的 Mutex 不可重入 —— 这里不先放锁就是自锁。
                drop(active);
                self.remove_session(session);
                // `start_compat` 保证旧会话要么没被动过、要么已经被拉回来；只有回滚
                // 也失败时它才会真的消失，那时 active 不能再指向一个不存在的 id。
                if previous.is_some_and(|previous| self.session_state(previous).is_none()) {
                    let mut active = self.inner.active_user.lock().unwrap();
                    // 重新取锁期间可能已经有别的调用装上了新会话，只清我们认得的那个。
                    if *active == previous {
                        *active = None;
                    }
                }
                Err(error)
            }
        }
    }

    fn start_compat(
        &self,
        session: SessionId,
        previous: Option<SessionId>,
    ) -> Result<SessionHandle, String> {
        if let Some(handle) = self.admit_and_start(session, Priority::FOREGROUND)? {
            return Ok(handle);
        }
        // 走到这里说明用户并发被配成 1，新旧会话在抢同一个槽。
        let Some(previous) = previous else {
            return Err("没有可用的会话运行槽".to_owned());
        };
        self.dequeue_session(session);
        // 让位用 Park 而不是 remove：Park 会拒绝正在执行请求的旧会话（那时旧会话
        // **原样保住**），成功时旧会话也仍然登记在册 —— 新会话起不来还能原地拉回来。
        self.park(previous)
            .map_err(|error| format!("没有可用的会话运行槽，且旧会话无法让位：{error}"))?;
        match self.admit_and_start(session, Priority::FOREGROUND) {
            Ok(Some(handle)) => Ok(handle),
            outcome => {
                // 回滚：把旧会话拉回来。它会拿到新的 `RuntimeId`，调用方必须用
                // `session_handle(active_user_session())` 重新取句柄。
                let restored = self.request_run(previous, Priority::FOREGROUND);
                let reason = match outcome {
                    Err(error) => error,
                    _ => "没有可用的会话运行槽".to_owned(),
                };
                Err(match restored {
                    Ok(Some(_)) => format!("{reason}；旧会话已恢复，请重新获取会话句柄"),
                    _ => format!("{reason}；旧会话恢复失败"),
                })
            }
        }
    }

    /// 把一个会话从等待队列里摘掉并退回 `Parked`。
    fn dequeue_session(&self, session: SessionId) {
        let mut core = self.inner.core.lock().unwrap();
        core.queue.remove(session);
        if let Some(slot) = core.sessions.get_mut(&session) {
            let _ = slot.transition(SchedulerState::Parked);
        }
    }

    /// 兼容通道当前的活跃会话。
    ///
    /// `start_*` 失败并触发回滚后，旧会话会换一个 `RuntimeId`，调用方需要靠它
    /// 重新取句柄。
    pub fn active_user_session(&self) -> Option<SessionId> {
        *self.inner.active_user.lock().unwrap()
    }

    pub fn stop_user(&self, runtime_id: RuntimeId) {
        // 匹配与清空必须在**同一次持锁**里完成：中间放开的话，并发的 `start_fresh`
        // 会把新会话装进 `active_user`，随后这里一句无条件置 `None` 就把新会话抹掉，
        // 它的 Runtime 从此再也停不掉。
        let session = {
            let mut active = self.inner.active_user.lock().unwrap();
            let matched = {
                let core = self.inner.core.lock().unwrap();
                active.filter(|session| {
                    core.sessions
                        .get(session)
                        .and_then(|slot| slot.entry.as_ref())
                        .is_some_and(|entry| entry.id == runtime_id)
                })
            };
            if matched.is_some() {
                *active = None;
            }
            matched
        };
        let Some(session) = session else {
            return;
        };
        self.remove_session(session);
    }

    pub fn export_historical_html(
        &self,
        request: HistoricalHtmlExportRequest,
    ) -> Result<HistoricalHtmlExport, String> {
        let _permit = self.inner.maintenance.acquire();
        export_historical_html_impl(request, self.inner.tuning)
    }
}

/// Park 之前给后台作业的收尾时间。
///
/// 这段等待发生在**所有锁之外、且不改任何状态**：等到了就正常 Park，等不到就明确
/// 拒绝（`ParkRefusal::PendingJobs`），绝不会把排队作业丢掉。给一个短上限是为了不让
/// 一次偶发的慢 RPC 把 Park 变成秒级卡顿。
const PARK_SETTLE_BUDGET: Duration = Duration::from_millis(500);
const PARK_SETTLE_POLL: Duration = Duration::from_millis(5);

/// 一次 Park 从 Runtime 手里拿到的全部东西。
struct ParkedRuntime {
    /// 摘下来的进程；崩溃会话没有它。
    client: Option<Client>,
    history: ConversationDocument,
    session_file: Option<PathBuf>,
    /// 这个进程能不能进 warm pool 被别的会话接管。
    reusable: bool,
}

/// Park 被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParkRefusal {
    /// 正在执行一次提交：Park 掉它等于无声地扔掉 in-flight 的 assistant 输出。
    Busy,
    /// Actor 里还有排队或在跑的作业。控制类作业（Compact / Fork / SwitchSession）
    /// **不会**改 reducer 的 phase，光看 phase 会以为空闲；而 `actor.close()` 会把
    /// 排队作业直接丢掉，正在跑的那条又会被抽掉 client —— 一次用户点过的 Compact
    /// 或者一次不可重放的 Fork 就这么没了。
    PendingJobs,
    /// 正在替换进程（`restart_with_tools`）：既没有稳定的 client，也没有稳定的参数。
    Replacing,
}

impl ParkRefusal {
    fn message(self) -> String {
        match self {
            Self::Busy => "会话仍在执行请求，请先停止当前请求再 Park".to_owned(),
            Self::PendingJobs => "会话仍有未完成的后台作业，请稍后重试".to_owned(),
            Self::Replacing => "会话正在切换工具预设，请稍后重试".to_owned(),
        }
    }
}

/// 等 Actor 把手头的作业跑完。**不改任何状态**，等不到就由调用方去拒绝。
fn wait_for_actor_idle(entry: &Arc<RuntimeEntry>, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !entry.actor.is_idle() {
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(PARK_SETTLE_POLL);
    }
}

/// 预留一次 Park：在 Runtime 自己那把锁里一次完成「确认可停 + 封住新命令 + 摘走进程」。
///
/// 三件事必须原子。`dispatch` / `request_control` 全程持同一把 `state` 锁，因此它们
/// 要么排在我们前面（我们看到 `Running` 或未清空的队列，拒绝 Park 且**不动任何状态**），
/// 要么排在后面（看到 `stopped`，自己被拒）—— 中间不存在「检查已过、封锁未生效」的缝隙。
///
/// 返回成功即意味着：此后不会再有新作业入队，队列是空的，也没有作业在跑。
fn reserve_park(entry: &Arc<RuntimeEntry>) -> Result<ParkedRuntime, ParkRefusal> {
    let mut state = entry.state.lock().unwrap();
    if state.replacing {
        return Err(ParkRefusal::Replacing);
    }
    let crashed = state.terminal.is_some();
    if !crashed {
        if state.reducer.phase() != LivePhase::Idle {
            return Err(ParkRefusal::Busy);
        }
        if !entry.actor.is_idle() {
            return Err(ParkRefusal::PendingJobs);
        }
        state.stopped = true;
        entry.set_terminal(&mut state, TerminalState::Stopped);
        entry.publish(&mut state, RuntimeEffectKind::Stopped(None));
    }
    let client = state.client.take();
    // 崩溃残骸只是回收，不能拿去给别的会话复用。
    let reusable = client.is_some() && !crashed;
    let history = state.reducer.document();
    let session_file = state.calibration_path.lock().unwrap().clone();
    Ok(ParkedRuntime {
        client,
        history,
        session_file,
        reusable,
    })
}

/// 拆掉 Runtime 的线程。进程本身还活着，去留由调用方决定。
fn finish_park(entry: &Arc<RuntimeEntry>, mut parked: ParkedRuntime) -> ParkedRuntime {
    // 队列在 `reserve_park` 里已经确认为空且无人在跑，这里的 close 只是让 worker 退出。
    entry.actor.close();
    // 叫醒阻塞在 recv 上的 pump：进程继续活着，只有本 Runtime 停止消费事件。
    entry.detach_events();
    // 等 pump 真的退出再决定进程去留。哨兵已经在它的队列里，这一步通常是亚毫秒级；
    // 等它不只是为了读 `pump_lost_events`，也保证进程易主时旧 pump 已经彻底离场。
    let deadline = Instant::now() + PARK_SETTLE_BUDGET;
    while entry.live_pumps.load(Ordering::Acquire) > 0 {
        if Instant::now() >= deadline {
            // 没能确认 pump 收尾：宁可多付一次冷启动，也不把状态不明的进程交给别人。
            parked.reusable = false;
            break;
        }
        std::thread::sleep(PARK_SETTLE_POLL);
    }
    if entry.pump_lost_events() {
        parked.reusable = false;
    }
    // 进程已经从这个 Runtime 手里转走（进池或即将被关掉）：本 entry 不再持有任何进程。
    entry.mark_released();
    parked
}

/// 取一个 Runtime 的当前身份与历史，供 Slot 在丢弃它之前留档。
fn capture_runtime_state(
    entry: &RuntimeEntry,
) -> (SessionDescriptor, ConversationDocument, Option<PathBuf>) {
    let descriptor = entry.descriptor.lock().unwrap().clone();
    let mut state = entry.state.lock().unwrap();
    let history = state.reducer.document();
    let session_file = state.calibration_path.lock().unwrap().clone();
    (descriptor, history, session_file)
}

/// 把 Runtime 的实际身份与历史写回 Slot，供下一次 Resume 使用。
fn apply_captured_state(
    slot: &mut SessionSlot,
    descriptor: &SessionDescriptor,
    history: ConversationDocument,
    session_file: Option<PathBuf>,
) {
    // 以 Runtime **实际**跑的参数为准：`restart_with_tools` 可能已经换过工具预设。
    slot.descriptor = descriptor.clone();
    // 只在真的落盘之后才记会话文件：指向一个还不存在的路径会让下一次冷启动失败。
    if session_file.as_ref().is_some_and(|path| path.is_file()) {
        slot.descriptor.session_path = session_file;
    }
    slot.history = history;
}

/// 「这条作业攥着 Runtime 的进程句柄」的 RAII 记账。
///
/// 必须由作业闭包**捕获**（而不是在闭包体内创建）：作业还在队列里就被 `close()` 丢掉时，
/// 闭包连同守卫一起 drop，计数照样归零。R23 第三轮审查整改时先写成「闭包体内创建」，
/// 结果恰恰漏掉这条路，运行槽被永久扣住。
struct ClientOwnerGuard {
    owners: Arc<AtomicUsize>,
}

impl ClientOwnerGuard {
    fn acquire(entry: &RuntimeEntry) -> Self {
        entry.client_owners.fetch_add(1, Ordering::AcqRel);
        Self {
            owners: Arc::clone(&entry.client_owners),
        }
    }
}

impl Drop for ClientOwnerGuard {
    fn drop(&mut self) {
        self.owners.fetch_sub(1, Ordering::AcqRel);
    }
}

fn shutdown_entry(entry: &RuntimeEntry) {
    let (client, _owner) = {
        let mut state = entry.state.lock().unwrap();
        state.stopped = true;
        // BACKLOG #12（指派给 R22）：优雅停止此前不产生任何可观察终态，观察者只能等超时。
        entry.set_terminal(&mut state, TerminalState::Stopped);
        entry.publish(&mut state, RuntimeEffectKind::Stopped(None));
        let client = state.client.take();
        // 关进程要花到 grace period，这段时间里进程还活着。不记账的话，并发的 `park`
        // 会看到「有终态 + 没有 client」就宣布释放，运行槽在旧进程退出前就被让出去。
        let owner = client.as_ref().map(|_| ClientOwnerGuard::acquire(entry));
        (client, owner)
    };
    // 先关队列再关进程：worker 可能正阻塞在一次长请求上，关队列让它跑完当前作业后退出。
    // 关队列会连同排队作业一起 drop，替换作业手里的旧 client 因此也在这一步被关掉。
    entry.actor.close();
    if let Some(client) = client {
        let _ = client.shutdown();
    }
    // 拆除流程到此走完。换进程作业可能还攥着旧 client —— 那部分由
    // `client_owners` 记账，`is_released()` 要两者都满足才为真。
    entry.mark_released();
}

fn clamp_manager_config(config: &mut ClientConfig, tuning: RuntimeTuning) {
    config.max_restarts = 0;
    config.event_backlog_bytes = tuning.event_backlog_bytes;
}

fn publish_if_current(entry: &RuntimeEntry, epoch: u64, kind: RuntimeEffectKind) {
    let mut state = entry.state.lock().unwrap();
    if state.epoch != epoch || state.stopped {
        return;
    }
    entry.publish(&mut state, kind);
}

fn publish_tool_restart_failure(
    entry: &RuntimeEntry,
    epoch: u64,
    preset: ToolPreset,
    error: String,
) {
    let mut state = entry.state.lock().unwrap();
    if state.epoch != epoch || state.stopped {
        // 被 fence 掉也要置位：会话已经被停掉时，这条路径**曾经**是唯一漏掉释放标记的
        // 出口，`reap_terminated` 于是永远跳过它，运行槽被永久扣住。
        drop(state);
        entry.mark_released();
        return;
    }
    // old Client 已退出；先 fence 旧 pump，再保留更准确的重启失败终态。
    state.replacing = false;
    state.stopped = true;
    state
        .reducer
        .set_error(format!("工具预设重启失败：{error}"));
    entry.set_terminal(
        &mut state,
        TerminalState::Failed {
            error: error.clone(),
        },
    );
    entry.publish(
        &mut state,
        RuntimeEffectKind::ToolRestartFinished {
            preset,
            result: Err(error),
        },
    );
    drop(state);
    entry.actor.close();
    // 旧进程在进入本函数之前就已经 shutdown、新进程压根没起来 —— 没有活着的进程了。
    // 不置这个标记，`reap_terminated` 会永远跳过这个会话，运行槽被永久扣住。
    entry.mark_released();
}

fn fail_runtime(entry: &RuntimeEntry, epoch: u64, error: String) {
    let (client, _owner) = {
        let mut state = entry.state.lock().unwrap();
        if state.epoch != epoch || state.stopped || state.replacing {
            return;
        }
        state.stopped = true;
        let client = state.client.take();
        // 与 `shutdown_entry` 同理：终态先于 shutdown 发布，这段窗口里进程还在退出，
        // 必须记账，否则并发的 `park` / `stop_session` 会提前归还运行槽。
        let owner = client.as_ref().map(|_| ClientOwnerGuard::acquire(entry));
        state.reducer.set_error(error.clone());
        entry.set_terminal(
            &mut state,
            TerminalState::Failed {
                error: error.clone(),
            },
        );
        entry.publish(&mut state, RuntimeEffectKind::Stopped(Some(error)));
        (client, owner)
    };
    entry.actor.close();
    // Client::shutdown 可能等待 supervisor/stdout 线程退出；必须在 RuntimeState 锁外执行，
    // 否则 UI 拉取 Snapshot 会被进程清理时延连带阻塞。
    if let Some(client) = client {
        let _ = client.shutdown();
    }
    // 进程真的没了之后才允许调度器归还运行槽（见 `reap_terminated`）。
    entry.mark_released();
}

fn dispatch_command(
    intent: RpcIntent,
    submission: Option<&ComposerSubmission>,
    mode: ComposerMode,
) -> Command {
    match intent {
        RpcIntent::Prompt => Command::Prompt {
            message: submission
                .map(|submission| submission.message.clone())
                .unwrap_or_default(),
            images: submission.and_then(ComposerSubmission::rpc_images),
            streaming_behavior: None,
        },
        RpcIntent::Steer | RpcIntent::FollowUp => Command::Prompt {
            message: submission
                .map(|submission| submission.message.clone())
                .unwrap_or_default(),
            images: submission.and_then(ComposerSubmission::rpc_images),
            streaming_behavior: Some(mode.streaming_behavior()),
        },
        RpcIntent::Abort => Command::Abort,
    }
}

fn load_commands(client: &Client) -> Result<Vec<RpcSlashCommand>, String> {
    client
        .request_data::<CommandsData>(Command::GetCommands, REQUEST_TIMEOUT)
        .map(|mut data| {
            data.commands.sort_by(|left, right| {
                slash_source_order(left.source)
                    .cmp(&slash_source_order(right.source))
                    .then_with(|| left.name.cmp(&right.name))
            });
            data.commands
        })
        .map_err(|error| error.to_string())
}

const fn request_timeout(intent: RpcIntent) -> Duration {
    match intent {
        RpcIntent::Prompt | RpcIntent::Steer | RpcIntent::FollowUp => INTERACTIVE_REQUEST_TIMEOUT,
        RpcIntent::Abort => REQUEST_TIMEOUT,
    }
}

pub(crate) fn load_controls(
    client: &Client,
    agent_dir: Option<&Path>,
) -> Result<SessionControls, String> {
    let state = client
        .request_data::<RpcSessionState>(Command::GetState, REQUEST_TIMEOUT)
        .map_err(|error| error.to_string())?;
    load_controls_from_state(client, state, agent_dir)
}

fn load_controls_from_state(
    client: &Client,
    state: RpcSessionState,
    agent_dir: Option<&Path>,
) -> Result<SessionControls, String> {
    let mut models = client
        .request_data::<AvailableModelsData>(Command::GetAvailableModels, REQUEST_TIMEOUT)
        .map_err(|error| error.to_string())?
        .models;
    models.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.id.cmp(&right.id))
    });
    let thinking_levels = client
        .request_data::<ThinkingLevelsData>(Command::GetAvailableThinkingLevels, REQUEST_TIMEOUT)
        .map_err(|error| error.to_string())?
        .levels;
    let tree = client
        .request_data::<TreeData>(Command::GetTree, REQUEST_TIMEOUT)
        .map_err(|error| error.to_string())?;
    let auto_retry_enabled = agent_dir
        .map(pi_data::read_auto_retry_enabled)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or(true);
    Ok(SessionControls {
        model: state.model,
        thinking_level: state.thinking_level,
        models,
        thinking_levels,
        session_file: state.session_file.map(PathBuf::from),
        session_id: state.session_id,
        tree,
        auto_compaction_enabled: state.auto_compaction_enabled,
        auto_retry_enabled,
        is_compacting: state.is_compacting,
    })
}

pub(crate) fn execute_control(
    client: &Client,
    request: ControlRequest,
    agent_dir: Option<&Path>,
) -> Result<ControlOutcome, String> {
    match request {
        ControlRequest::SetModel { provider, model_id } => {
            client
                .request_data::<Model>(Command::SetModel { provider, model_id }, REQUEST_TIMEOUT)
                .map_err(|error| error.to_string())?;
        }
        ControlRequest::CycleModel => {
            let response = client
                .request(Command::CycleModel, REQUEST_TIMEOUT)
                .map_err(|error| error.to_string())?;
            if !response.success {
                return Err(response.error.unwrap_or_else(|| "unknown RPC error".into()));
            }
        }
        ControlRequest::SetThinking(level) => {
            let response = client
                .request(Command::SetThinkingLevel { level }, REQUEST_TIMEOUT)
                .map_err(|error| error.to_string())?;
            if !response.success {
                return Err(response.error.unwrap_or_else(|| "unknown RPC error".into()));
            }
        }
        ControlRequest::Compact => {
            let result = client
                .request_data::<CompactionResult>(
                    Command::Compact {
                        custom_instructions: None,
                    },
                    Duration::from_secs(300),
                )
                .map_err(|error| error.to_string())?;
            return Ok(ControlOutcome::Compacted(result));
        }
        ControlRequest::SetAutoCompaction(enabled) => {
            ensure_success(client, Command::SetAutoCompaction { enabled })?;
        }
        ControlRequest::SetAutoRetry(enabled) => {
            ensure_success(client, Command::SetAutoRetry { enabled })?;
        }
        ControlRequest::AbortRetry => {
            ensure_success(client, Command::AbortRetry)?;
            return Ok(ControlOutcome::RetryAborted);
        }
        ControlRequest::Fork { entry_id } => {
            let outcome = client
                .request_session_rebind_data::<ForkData>(
                    Command::Fork { entry_id },
                    REQUEST_TIMEOUT,
                )
                .map_err(|error| error.to_string())?;
            if outcome.data.cancelled {
                return Ok(ControlOutcome::ForkCancelled(outcome.data));
            }
            let data = outcome.data;
            let state = match calibrated_state("fork", outcome.calibration) {
                Ok(state) => state,
                Err(message) => {
                    return Ok(ControlOutcome::RebindCalibrationFailed {
                        operation: ControlOperation::Fork,
                        message,
                        fork_data: Some(data),
                    });
                }
            };
            let controls = match load_controls_from_state(client, state, agent_dir) {
                Ok(controls) => controls,
                Err(error) => {
                    return Ok(ControlOutcome::RebindCalibrationFailed {
                        operation: ControlOperation::Fork,
                        message: format!(
                            "fork 已成功，但会话控制元数据刷新失败；请勿重复操作：{error}"
                        ),
                        fork_data: Some(data),
                    });
                }
            };
            return Ok(ControlOutcome::Forked { data, controls });
        }
        ControlRequest::Clone => {
            let outcome = client
                .request_session_rebind_data::<CloneData>(Command::Clone, REQUEST_TIMEOUT)
                .map_err(|error| error.to_string())?;
            if outcome.data.cancelled {
                return Ok(ControlOutcome::CloneCancelled);
            }
            let state = match calibrated_state("clone", outcome.calibration) {
                Ok(state) => state,
                Err(message) => {
                    return Ok(ControlOutcome::RebindCalibrationFailed {
                        operation: ControlOperation::Clone,
                        message,
                        fork_data: None,
                    });
                }
            };
            let controls = match load_controls_from_state(client, state, agent_dir) {
                Ok(controls) => controls,
                Err(error) => {
                    return Ok(ControlOutcome::RebindCalibrationFailed {
                        operation: ControlOperation::Clone,
                        message: format!(
                            "clone 已成功，但会话控制元数据刷新失败；请勿重复操作：{error}"
                        ),
                        fork_data: None,
                    });
                }
            };
            return Ok(ControlOutcome::Cloned {
                data: outcome.data,
                controls,
            });
        }
        ControlRequest::SwitchSession { path } => {
            let outcome = client
                .request_session_rebind_data::<pi_rpc::SwitchSessionData>(
                    Command::SwitchSession {
                        session_path: path.to_string_lossy().into_owned(),
                    },
                    REQUEST_TIMEOUT,
                )
                .map_err(|error| error.to_string())?;
            if outcome.data.cancelled {
                return Ok(ControlOutcome::SwitchCancelled);
            }
            let state = match calibrated_state("switch_session", outcome.calibration) {
                Ok(state) => state,
                Err(message) => {
                    return Ok(ControlOutcome::RebindCalibrationFailed {
                        operation: ControlOperation::SwitchSession,
                        message,
                        fork_data: None,
                    });
                }
            };
            return Ok(match load_controls_from_state(client, state, agent_dir) {
                Ok(controls) => ControlOutcome::Switched(controls),
                Err(error) => ControlOutcome::RebindCalibrationFailed {
                    operation: ControlOperation::SwitchSession,
                    message: format!(
                        "switch_session 已成功，但会话控制元数据刷新失败；请勿重复操作：{error}"
                    ),
                    fork_data: None,
                },
            });
        }
        ControlRequest::ExportHtml { output_path } => {
            let data = client
                .request_data::<ExportPathData>(
                    Command::ExportHtml {
                        output_path: Some(output_path.to_string_lossy().into_owned()),
                    },
                    Duration::from_secs(60),
                )
                .map_err(|error| error.to_string())?;
            return Ok(ControlOutcome::Exported(data));
        }
    }
    load_controls(client, agent_dir).map(ControlOutcome::Controls)
}

fn apply_controls_identity(state: &mut RuntimeState, controls: &SessionControls) {
    let Some(path) = controls.session_file.as_ref() else {
        return;
    };
    *state.calibration_path.lock().unwrap() = Some(path.clone());
    state
        .reducer
        .set_session_identity(controls.session_id.clone(), path.clone());
}

fn apply_control_runtime_state(state: &mut RuntimeState, outcome: &ControlOutcome) {
    let controls = match outcome {
        ControlOutcome::Controls(controls)
        | ControlOutcome::Switched(controls)
        | ControlOutcome::Forked { controls, .. }
        | ControlOutcome::Cloned { controls, .. } => Some(controls),
        _ => None,
    };
    let Some(controls) = controls else {
        return;
    };
    apply_controls_identity(state, controls);
    if let Some(path) = controls.session_file.as_ref()
        && let Ok(document) = pi_render::render_path(path)
    {
        state.reducer.calibrate(document);
        apply_controls_identity(state, controls);
    }
}

fn calibrated_state(
    operation: &str,
    calibration: Option<Result<RpcSessionState, pi_rpc::ClientError>>,
) -> Result<RpcSessionState, String> {
    calibration
        .ok_or_else(|| format!("{operation} 已取消"))?
        .map_err(|error| format!("{operation} 已成功，但会话元数据校准失败；请勿重复操作：{error}"))
}

fn ensure_success(client: &Client, command: Command) -> Result<(), String> {
    let response = client
        .request(command, REQUEST_TIMEOUT)
        .map_err(|error| error.to_string())?;
    if response.success {
        Ok(())
    } else {
        Err(response.error.unwrap_or_else(|| "unknown RPC error".into()))
    }
}

const fn slash_source_order(source: pi_rpc::SlashCommandSource) -> u8 {
    match source {
        pi_rpc::SlashCommandSource::Extension => 0,
        pi_rpc::SlashCommandSource::Prompt => 1,
        pi_rpc::SlashCommandSource::Skill => 2,
    }
}

fn active_session_config(
    binary: PathBuf,
    session_path: Option<PathBuf>,
    cwd: PathBuf,
    tool_preset: ToolPreset,
) -> (ClientConfig, Option<String>) {
    active_session_config_with_materializer(binary, session_path, cwd, tool_preset, || {
        pi_rpc::materialize_host_extension()
    })
}

fn active_session_config_with_materializer(
    binary: PathBuf,
    session_path: Option<PathBuf>,
    cwd: PathBuf,
    tool_preset: ToolPreset,
    materialize: impl FnOnce() -> std::io::Result<PathBuf>,
) -> (ClientConfig, Option<String>) {
    let mut config = ClientConfig::new(binary);
    config.current_dir = Some(cwd);
    config.initial_session = session_path;
    config.args = vec!["--no-context-files".into()];
    let diagnostic = match materialize() {
        Ok(host_extension) => {
            config
                .args
                .extend(["-e".into(), host_extension.into_os_string()]);
            None
        }
        Err(error) => Some(format!("项目命令环境扩展未加载：{error}")),
    };
    tool_preset.append_args(&mut config.args);
    (config, diagnostic)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalHtmlExport {
    pub path: PathBuf,
    pub cleanup_warning: Option<String>,
}

fn finish_historical_export(
    export: Result<PathBuf, String>,
    shutdown: Result<(), String>,
) -> Result<HistoricalHtmlExport, String> {
    match export {
        Ok(path) => Ok(HistoricalHtmlExport {
            path,
            cleanup_warning: shutdown.err(),
        }),
        Err(error) => {
            // shutdown 仍已尝试；主导出失败优先展示，清理失败附带保留可观测性。
            Err(match shutdown {
                Ok(()) => error,
                Err(shutdown_error) => format!("{error}；进程清理也失败：{shutdown_error}"),
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoricalHtmlExportRequest {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub session_path: PathBuf,
    pub output_path: PathBuf,
}

fn export_historical_html_impl(
    request: HistoricalHtmlExportRequest,
    tuning: RuntimeTuning,
) -> Result<HistoricalHtmlExport, String> {
    let HistoricalHtmlExportRequest {
        binary,
        cwd,
        session_path,
        output_path,
    } = request;
    let mut config = ClientConfig::new(binary);
    config.current_dir = Some(cwd);
    config.initial_session = Some(session_path);
    config.args = vec![
        "--no-extensions".into(),
        "--no-skills".into(),
        "--no-prompt-templates".into(),
        "--no-context-files".into(),
        "--offline".into(),
    ];
    clamp_manager_config(&mut config, tuning);
    let client = Client::spawn(config).map_err(|error| error.to_string())?;
    let result = client
        .request_data::<ExportPathData>(
            Command::ExportHtml {
                output_path: Some(output_path.to_string_lossy().into_owned()),
            },
            Duration::from_secs(60),
        )
        .map(|data| PathBuf::from(data.path))
        .map_err(|error| error.to_string());
    let shutdown = client.shutdown().map_err(|error| error.to_string());
    finish_historical_export(result.map(|_| output_path), shutdown)
}

pub fn official_binary() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("vendor")
        .join("pi")
        .join(pi_rpc::pi_binary_name())
}

fn spawn_event_pump(
    entry: Arc<RuntimeEntry>,
    epoch: u64,
    calibration_path: Arc<Mutex<Option<PathBuf>>>,
    events: EventStream,
) {
    let frame = entry.tuning.event_frame;
    let live_pumps = Arc::clone(&entry.live_pumps);
    let lost_events = Arc::clone(&entry.pump_lost_events);
    live_pumps.fetch_add(1, Ordering::AcqRel);
    // 把断开句柄留一份在 entry 上，Park 才有办法从外部叫醒阻塞中的 pump。
    *entry.events.lock().unwrap() = Some(events.detach_handle());
    actor::spawn_named(
        format!("pi-runtime-event-pump-{}-{epoch}", entry.id.get()),
        move || {
            let mut activity_generation = 0_u64;
            loop {
                let first = match events.recv() {
                    Ok(event) => event,
                    Err(_) => {
                        fail_runtime(&entry, epoch, "会话已崩溃，请重新启动".to_owned());
                        break;
                    }
                };
                let mut projected = ProjectedPumpFrame::default();
                project_pump_event(first, &mut projected, &mut activity_generation);
                let deadline = Instant::now() + frame;
                let mut disconnected = false;
                while projected.batch.len() < MAX_EVENTS_PER_BATCH {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    match events.recv_timeout(deadline.saturating_duration_since(now)) {
                        Ok(event) => {
                            project_pump_event(event, &mut projected, &mut activity_generation)
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if let Some(error) = projected.terminal_failure {
                    fail_runtime(&entry, epoch, error);
                    break;
                }
                // 这一帧收进来的事件，只要 entry 已经被 fence 掉就无处可去。含扩展 UI
                // 请求时，pi 里那个扩展可能正等着回应 —— 这样的进程不能进 warm pool。
                let frame_has_payload = !projected.batch.is_empty()
                    || !projected.extension_requests.is_empty()
                    || !projected.runtime_events.is_empty()
                    || projected.extension_reset;
                if projected.detached {
                    // Park 让位：进程还活着，只是不再由本 Runtime 消费事件。
                    if frame_has_payload {
                        lost_events.store(true, Ordering::Release);
                    }
                    break;
                }
                let mut state = entry.state.lock().unwrap();
                if state.epoch != epoch || state.stopped {
                    // 帧是被 deadline 或 512 条上限截断的，哨兵还排在后面 —— 但 Park
                    // 已经置了 `stopped`，这一帧同样丢定了，标记不能只挂在哨兵那条路上。
                    if frame_has_payload {
                        lost_events.store(true, Ordering::Release);
                    }
                    break;
                }
                if projected.extension_reset {
                    entry.publish(&mut state, RuntimeEffectKind::ExtensionUiReset);
                }
                if !projected.extension_requests.is_empty() {
                    entry.publish(
                        &mut state,
                        RuntimeEffectKind::ExtensionUiBatch {
                            requests: coalesce_extension_ui_requests(projected.extension_requests),
                        },
                    );
                }
                if projected
                    .batch
                    .iter()
                    .any(|event| matches!(event, LiveEvent::AgentStart))
                {
                    state.activity_generation = state.activity_generation.wrapping_add(1);
                }
                if !projected.batch.is_empty() || !projected.runtime_events.is_empty() {
                    let outcome = state.reducer.apply_batch(projected.batch);
                    entry.publish(
                        &mut state,
                        RuntimeEffectKind::Events {
                            follow_tail: outcome.follow_tail,
                            settled: outcome.settled,
                            runtime_events: projected.runtime_events,
                        },
                    );
                }
                drop(state);
                if projected.settled
                    && let Some(session_path) = calibration_path.lock().unwrap().clone()
                {
                    enqueue_calibration(&entry, epoch, activity_generation, session_path);
                }
                if disconnected {
                    fail_runtime(&entry, epoch, "会话已崩溃，请重新启动".to_owned());
                    break;
                }
            }
            live_pumps.fetch_sub(1, Ordering::AcqRel);
        },
    );
}

/// 把落盘校准投递给 Actor。
///
/// 校准是纯粹的「取最新」刷新：排队多份旧校准没有意义，因此用 [`JobKey::Calibration`]
/// 在队列内合并；队列满时跳过并计数，下一次 settled 会重新触发。
fn enqueue_calibration(
    entry: &Arc<RuntimeEntry>,
    epoch: u64,
    calibration: u64,
    session_path: PathBuf,
) {
    let mut state = entry.state.lock().unwrap();
    if state.epoch != epoch || state.stopped {
        return;
    }
    let job_entry = entry.clone();
    let _ = entry.enqueue(
        &mut state,
        Channel::Command,
        Some(JobKey::Calibration),
        move || {
            let result = pi_render::render_path(session_path).map_err(|error| error.to_string());
            let mut state = job_entry.state.lock().unwrap();
            if state.epoch != epoch || state.stopped {
                return;
            }
            if state.reducer.phase() != LivePhase::Idle || state.activity_generation != calibration
            {
                return;
            }
            match result {
                Ok(document) => {
                    state.reducer.calibrate(document);
                    job_entry.mark_dirty(&mut state);
                }
                Err(error) => job_entry.publish(
                    &mut state,
                    RuntimeEffectKind::Diagnostic(format!(
                        "会话落盘校准失败（activity {calibration}）：{error}"
                    )),
                ),
            }
        },
    );
}

#[derive(Default)]
struct ProjectedPumpFrame {
    batch: Vec<LiveEvent>,
    runtime_events: Vec<SessionRuntimeEvent>,
    extension_requests: Vec<(String, ExtensionUiRequest)>,
    extension_reset: bool,
    settled: bool,
    terminal_failure: Option<String>,
    /// 订阅被主动断开（Park）：pump 正常退出，**不是**会话失败。
    detached: bool,
}

fn project_pump_event(
    event: ClientEvent,
    projected: &mut ProjectedPumpFrame,
    activity_generation: &mut u64,
) {
    if let ClientEvent::Rpc(event) = &event {
        match event.as_ref() {
            RpcEvent::ExtensionUiRequest { .. } => {}
            RpcEvent::AgentEnd { will_retry, .. } => {
                projected
                    .runtime_events
                    .push(SessionRuntimeEvent::AgentEnded {
                        will_retry: *will_retry,
                    })
            }
            RpcEvent::CompactionStart { .. } => projected
                .runtime_events
                .push(SessionRuntimeEvent::CompactionStarted),
            RpcEvent::CompactionEnd { error_message, .. } => {
                projected
                    .runtime_events
                    .push(SessionRuntimeEvent::CompactionEnded {
                        error: error_message.clone(),
                    });
                projected.settled = true;
            }
            RpcEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                projected
                    .runtime_events
                    .push(SessionRuntimeEvent::RetryStarted {
                        attempt: *attempt,
                        max_attempts: *max_attempts,
                        delay_ms: *delay_ms,
                        error: error_message.clone(),
                    });
            }
            RpcEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => {
                projected
                    .runtime_events
                    .push(SessionRuntimeEvent::RetryEnded {
                        success: *success,
                        attempt: *attempt,
                        error: final_error.clone(),
                    });
            }
            _ => {}
        }
    }
    match event {
        ClientEvent::Rpc(event) => match *event {
            RpcEvent::ExtensionUiRequest { id, request } => {
                projected.extension_requests.push((id, request))
            }
            event => {
                if let Some(event) = project_rpc_event(event) {
                    if matches!(event, LiveEvent::AgentStart) {
                        *activity_generation = activity_generation.wrapping_add(1);
                    }
                    projected.settled |= matches!(event, LiveEvent::AgentSettled);
                    projected.batch.push(event);
                }
            }
        },
        ClientEvent::Lifecycle(pi_rpc::LifecycleEvent::Restarted { .. }) => {
            projected.extension_reset = true;
            projected.extension_requests.clear();
        }
        ClientEvent::Lifecycle(pi_rpc::LifecycleEvent::Exited { code, .. }) => {
            projected.terminal_failure = Some(match code {
                Some(code) => format!("会话已崩溃，请重新启动（退出码 {code}）"),
                None => "会话已崩溃，请重新启动（进程异常退出）".to_owned(),
            });
        }
        ClientEvent::Lifecycle(pi_rpc::LifecycleEvent::RestartFailed { error }) => {
            projected.terminal_failure = Some(format!("会话已崩溃，请重新启动：{error}"));
        }
        ClientEvent::Lifecycle(pi_rpc::LifecycleEvent::EventBacklogOverflow {
            queued_bytes,
            limit,
        }) => {
            // 事件积压超限意味着后续事件流已经不完整；与其带着窟窿继续渲染，
            // 不如按会话失败明确终止，让用户重启会话。
            projected.terminal_failure = Some(format!(
                "事件积压超过上限（{queued_bytes} / {limit} 字节），会话已停止，请重新启动"
            ));
        }
        ClientEvent::Lifecycle(pi_rpc::LifecycleEvent::Detached) => {
            // Park：进程被交给 warm pool 继续活着，只有本 pump 退出。
            // 绝不能走 terminal_failure —— 那会把一次正常的让位报成会话崩溃。
            projected.detached = true;
        }
        event => {
            if let Some(event) = project_event(event) {
                projected.batch.push(event);
            }
        }
    }
}

fn coalesce_extension_ui_requests(
    requests: Vec<(String, ExtensionUiRequest)>,
) -> Vec<(String, ExtensionUiRequest)> {
    let mut result = Vec::<(String, ExtensionUiRequest)>::new();
    let mut coalesced = std::collections::HashMap::<(u8, String), usize>::new();
    for (id, request) in requests {
        let key = match &request {
            ExtensionUiRequest::SetStatus { status_key, .. } => Some((0, status_key.clone())),
            ExtensionUiRequest::SetWidget { widget_key, .. } => Some((1, widget_key.clone())),
            _ => None,
        };
        if let Some(key) = key {
            if let Some(index) = coalesced.get(&key).copied() {
                result[index] = (id, request);
            } else {
                coalesced.insert(key, result.len());
                result.push((id, request));
            }
        } else {
            result.push((id, request));
        }
    }
    result
}

fn project_event(event: ClientEvent) -> Option<LiveEvent> {
    match event {
        ClientEvent::Rpc(event) => project_rpc_event(*event),
        ClientEvent::Unknown(value) => Some(LiveEvent::Diagnostic(format!(
            "未识别的 pi RPC 事件：{value}"
        ))),
        ClientEvent::Lifecycle(_) => None,
    }
}

fn project_rpc_event(event: RpcEvent) -> Option<LiveEvent> {
    match event {
        RpcEvent::AgentStart => Some(LiveEvent::AgentStart),
        RpcEvent::AgentEnd { .. } => Some(LiveEvent::AgentEnd),
        RpcEvent::AgentSettled => Some(LiveEvent::AgentSettled),
        RpcEvent::MessageStart { message } => Some(LiveEvent::MessageStart { message: message.0 }),
        RpcEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => Some(LiveEvent::MessageUpdate(project_update(
            assistant_message_event,
        ))),
        RpcEvent::MessageEnd { message } => Some(LiveEvent::MessageEnd { message: message.0 }),
        RpcEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(LiveEvent::ToolExecutionStart {
            id: tool_call_id,
            name: tool_name,
            arguments: args,
        }),
        RpcEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => Some(LiveEvent::ToolExecutionUpdate {
            id: tool_call_id,
            name: tool_name,
            arguments: args,
            partial_result,
        }),
        RpcEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => Some(LiveEvent::ToolExecutionEnd {
            id: tool_call_id,
            name: tool_name,
            result,
            is_error,
        }),
        RpcEvent::QueueUpdate {
            steering,
            follow_up,
        } => Some(LiveEvent::QueueUpdate {
            steering,
            follow_up,
        }),
        _ => None,
    }
}

fn project_update(event: AssistantMessageEvent) -> LiveAssistantUpdate {
    match event {
        AssistantMessageEvent::Start => LiveAssistantUpdate::Start,
        AssistantMessageEvent::TextStart { content_index } => LiveAssistantUpdate::BlockStart {
            index: content_index,
            kind: LiveBlockKind::Text,
        },
        AssistantMessageEvent::TextDelta {
            content_index,
            delta,
        } => LiveAssistantUpdate::BlockDelta {
            index: content_index,
            kind: LiveBlockKind::Text,
            delta,
        },
        AssistantMessageEvent::TextEnd {
            content_index,
            content,
        } => LiveAssistantUpdate::BlockEnd {
            index: content_index,
            kind: LiveBlockKind::Text,
            content: content.into(),
        },
        AssistantMessageEvent::ThinkingStart { content_index } => LiveAssistantUpdate::BlockStart {
            index: content_index,
            kind: LiveBlockKind::Thinking,
        },
        AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
        } => LiveAssistantUpdate::BlockDelta {
            index: content_index,
            kind: LiveBlockKind::Thinking,
            delta,
        },
        AssistantMessageEvent::ThinkingEnd {
            content_index,
            content,
        } => LiveAssistantUpdate::BlockEnd {
            index: content_index,
            kind: LiveBlockKind::Thinking,
            content: content.into(),
        },
        AssistantMessageEvent::ToolcallStart { content_index } => LiveAssistantUpdate::BlockStart {
            index: content_index,
            kind: LiveBlockKind::ToolCall,
        },
        AssistantMessageEvent::ToolcallDelta {
            content_index,
            delta,
        } => LiveAssistantUpdate::BlockDelta {
            index: content_index,
            kind: LiveBlockKind::ToolCall,
            delta,
        },
        AssistantMessageEvent::ToolcallEnd {
            content_index,
            tool_call,
        } => LiveAssistantUpdate::BlockEnd {
            index: content_index,
            kind: LiveBlockKind::ToolCall,
            content: tool_call,
        },
        AssistantMessageEvent::Done { .. } => LiveAssistantUpdate::Done,
        AssistantMessageEvent::Error { reason, error } => LiveAssistantUpdate::Error {
            message: format!("assistant stream {reason:?}: {}", error.0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn default_tuning() -> RuntimeTuning {
        RuntimeTuning::from_limits(&RuntimeLimits::default())
    }

    fn fake_binary() -> PathBuf {
        std::env::current_exe()
            .unwrap()
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join(format!(
                "runtime_fake_child{}",
                std::env::consts::EXE_SUFFIX
            ))
    }

    fn wait_for_snapshot(
        handle: &SessionHandle,
        timeout: Duration,
        predicate: impl Fn(&SessionSnapshot) -> bool,
    ) -> SessionSnapshot {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = handle.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for runtime snapshot"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn runtime_handle() -> (RuntimeManager, SessionHandle) {
        let manager = RuntimeManager::new(RuntimeLimits::default());
        let handle = manager
            .start_fresh(
                fake_binary(),
                std::env::temp_dir(),
                test_document("runtime-test"),
                ToolPreset::Inherit,
                None,
            )
            .unwrap();
        (manager, handle)
    }

    #[test]
    #[ignore = "requires GPUI_PI_TEST_FAKE_CHILD=target/debug/fake_child.exe"]
    fn session_controls_and_switches_use_typed_rpc_state() {
        let binary = std::env::var_os("GPUI_PI_TEST_FAKE_CHILD")
            .map(PathBuf::from)
            .expect("GPUI_PI_TEST_FAKE_CHILD must point to pi-rpc fake_child");
        let agent_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            agent_dir.path().join("settings.json"),
            r#"{"retry":{"enabled":false}}"#,
        )
        .unwrap();
        let mut config = ClientConfig::new(binary);
        config.env.push((
            pi_data::AGENT_DIR_ENV.into(),
            agent_dir.path().as_os_str().to_owned(),
        ));
        let client = Client::spawn(config).unwrap();
        let controls = load_controls(&client, Some(agent_dir.path())).unwrap();
        assert_eq!(controls.models.len(), 2);
        assert!(!controls.auto_retry_enabled);
        assert_eq!(controls.model.as_ref().unwrap().id, "model-one");
        assert_eq!(
            controls.thinking_levels,
            [ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High,]
        );

        let controls =
            execute_control(&client, ControlRequest::CycleModel, Some(agent_dir.path())).unwrap();
        let ControlOutcome::Controls(controls) = controls else {
            panic!("expected controls")
        };
        assert_eq!(controls.model.as_ref().unwrap().id, "model-two");
        let controls = execute_control(
            &client,
            ControlRequest::SetThinking(ThinkingLevel::High),
            Some(agent_dir.path()),
        )
        .unwrap();
        let ControlOutcome::Controls(controls) = controls else {
            panic!("expected controls")
        };
        assert_eq!(controls.thinking_level, ThinkingLevel::High);
        let controls = execute_control(
            &client,
            ControlRequest::SetModel {
                provider: "provider-one".to_owned(),
                model_id: "model-one".to_owned(),
            },
            Some(agent_dir.path()),
        )
        .unwrap();
        let ControlOutcome::Controls(controls) = controls else {
            panic!("expected controls")
        };
        assert_eq!(controls.model.as_ref().unwrap().id, "model-one");
        client.shutdown().unwrap();
    }

    #[test]
    fn historical_export_success_survives_shutdown_failure_with_warning() {
        let output = PathBuf::from("exported.html");
        let result =
            finish_historical_export(Ok(output.clone()), Err("shutdown timed out".to_owned()))
                .unwrap();
        assert_eq!(result.path, output);
        assert_eq!(
            result.cleanup_warning.as_deref(),
            Some("shutdown timed out")
        );

        let error = finish_historical_export(
            Err("export failed".to_owned()),
            Err("shutdown failed".to_owned()),
        )
        .unwrap_err();
        assert!(error.contains("export failed"));
        assert!(error.contains("shutdown failed"));
    }

    #[test]
    fn active_session_config_always_loads_host_extension_without_changing_tool_presets() {
        let expected = [
            (ToolPreset::Inherit, None),
            (ToolPreset::None, Some("")),
            (ToolPreset::ReadOnly, Some("read,grep,find,ls")),
            (ToolPreset::Default, Some("read,bash,edit,write")),
            (ToolPreset::Full, Some("bash,read,edit,write,grep,find,ls")),
        ];
        for (preset, allowlist) in expected {
            let (config, diagnostic) = active_session_config(
                PathBuf::from("pi.exe"),
                Some(PathBuf::from("session.jsonl")),
                PathBuf::from("project"),
                preset,
            );
            assert!(diagnostic.is_none());
            assert_eq!(config.args[0], "--no-context-files");
            assert_eq!(config.args[1], "-e");
            let extension_path = Path::new(&config.args[2]);
            assert!(extension_path.is_file());
            assert_eq!(
                extension_path.file_name().and_then(|name| name.to_str()),
                Some("project-command-environment.ts")
            );
            match allowlist {
                Some(allowlist) => assert_eq!(
                    &config.args[3..],
                    &["--tools", allowlist].map(std::ffi::OsString::from)
                ),
                None => assert_eq!(config.args.len(), 3),
            }
        }
    }

    #[test]
    fn fresh_active_session_config_has_no_initial_session() {
        let (config, diagnostic) = active_session_config_with_materializer(
            PathBuf::from("pi.exe"),
            None,
            PathBuf::from("project"),
            ToolPreset::Inherit,
            || Ok(PathBuf::from("host.ts")),
        );
        assert!(diagnostic.is_none());
        assert_eq!(config.current_dir.as_deref(), Some(Path::new("project")));
        assert_eq!(config.initial_session, None);
    }

    #[test]
    fn active_session_config_degrades_without_extension_and_reports_diagnostic() {
        let (config, diagnostic) = active_session_config_with_materializer(
            PathBuf::from("pi.exe"),
            Some(PathBuf::from("session.jsonl")),
            PathBuf::from("project"),
            ToolPreset::ReadOnly,
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
            },
        );
        assert_eq!(
            config.args,
            ["--no-context-files", "--tools", "read,grep,find,ls"].map(std::ffi::OsString::from)
        );
        assert_eq!(
            diagnostic.as_deref(),
            Some("项目命令环境扩展未加载：denied")
        );
    }

    #[test]
    fn streaming_intents_use_atomic_prompt_behavior() {
        assert_eq!(
            ComposerMode::Steer.streaming_behavior(),
            StreamingBehavior::Steer
        );
        assert_eq!(
            ComposerMode::FollowUp.streaming_behavior(),
            StreamingBehavior::FollowUp
        );
    }

    #[test]
    fn human_operated_submissions_outlive_the_short_rpc_timeout() {
        for intent in [RpcIntent::Prompt, RpcIntent::Steer, RpcIntent::FollowUp] {
            assert!(request_timeout(intent) > Duration::from_secs(30));
            assert_eq!(request_timeout(intent), INTERACTIVE_REQUEST_TIMEOUT);
        }
        assert_eq!(request_timeout(RpcIntent::Abort), REQUEST_TIMEOUT);
    }

    #[test]
    fn restarted_discards_only_pre_restart_extension_requests_in_same_frame() {
        let mut projected = ProjectedPumpFrame::default();
        let mut activity_generation = 0;
        for event in [
            ClientEvent::Rpc(Box::new(RpcEvent::ExtensionUiRequest {
                id: "before".into(),
                request: ExtensionUiRequest::Notify {
                    message: "before".into(),
                    notify_type: None,
                },
            })),
            ClientEvent::Lifecycle(pi_rpc::LifecycleEvent::Restarted {
                pid: 42,
                session_file: None,
            }),
            ClientEvent::Rpc(Box::new(RpcEvent::ExtensionUiRequest {
                id: "after-1".into(),
                request: ExtensionUiRequest::SetStatus {
                    status_key: "key".into(),
                    status_text: Some("one".into()),
                },
            })),
            ClientEvent::Rpc(Box::new(RpcEvent::ExtensionUiRequest {
                id: "after-2".into(),
                request: ExtensionUiRequest::SetStatus {
                    status_key: "key".into(),
                    status_text: Some("two".into()),
                },
            })),
        ] {
            project_pump_event(event, &mut projected, &mut activity_generation);
        }
        assert!(projected.extension_reset);
        let coalesced = coalesce_extension_ui_requests(projected.extension_requests);
        assert_eq!(coalesced.len(), 1);
        assert_eq!(coalesced[0].0, "after-2");
    }

    #[test]
    fn extension_ui_request_sanitization_limits_dialogs_statuses_and_widgets() {
        let mut state = ExtensionUiState::default();
        let raw_option = format!("family 👨‍👩‍👧‍👦\tvalue\u{202e}{}", "x".repeat(300));
        state.apply(
            "select".into(),
            ExtensionUiRequest::Select {
                title: format!("title\nspoof\u{202e}\u{200b}\u{0}{}", "x".repeat(200)),
                options: vec![raw_option.clone()],
                timeout: Some(10),
            },
        );
        let dialog = state.active_dialog().unwrap();
        let ExtensionUiRequest::Select { title, options, .. } = &dialog.request else {
            panic!("select request expected");
        };
        assert!(!title.chars().any(char::is_control));
        assert!(!title.contains('\u{202e}'));
        assert!(!title.contains('\u{200b}'));
        assert!(title.starts_with("title spoof"));
        assert_eq!(title.chars().count(), EXTENSION_DIALOG_TITLE_LIMIT);
        assert!(options.is_empty(), "raw option 不得进入 render request");
        let option = &dialog.select_options.as_ref().unwrap()[0];
        assert_eq!(option.raw, raw_option);
        assert!(!option.display.contains('\u{202e}'));
        assert!(!option.display.contains('\t'));
        assert!(option.display.chars().count() <= EXTENSION_DIALOG_OPTION_LIMIT);
        for index in 0..(EXTENSION_STATUS_COUNT_LIMIT + 4) {
            state.apply(
                format!("status-{index}"),
                ExtensionUiRequest::SetStatus {
                    status_key: format!("key-{index}"),
                    status_text: Some("x".repeat(EXTENSION_STATUS_LIMIT + 20)),
                },
            );
        }
        assert_eq!(state.statuses().count(), EXTENSION_STATUS_COUNT_LIMIT);
        for index in 0..(EXTENSION_WIDGET_COUNT_LIMIT + 4) {
            state.apply(
                format!("widget-{index}"),
                ExtensionUiRequest::SetWidget {
                    widget_key: format!("key-{index}"),
                    widget_lines: Some(vec!["line".into(); EXTENSION_WIDGET_LINES_LIMIT + 5]),
                    widget_placement: None,
                },
            );
        }
        assert_eq!(state.widgets.len(), EXTENSION_WIDGET_COUNT_LIMIT);
        assert!(
            state
                .widgets
                .values()
                .all(|widget| widget.lines.len() == EXTENSION_WIDGET_LINES_LIMIT)
        );
    }

    #[test]
    fn extension_ui_bounds_dialogs_and_notifications_without_hanging() {
        let mut state = ExtensionUiState::default();
        for index in 0..EXTENSION_DIALOG_QUEUE_LIMIT {
            assert!(
                state
                    .apply(
                        format!("dialog-{index}"),
                        ExtensionUiRequest::Confirm {
                            title: "Confirm".into(),
                            message: "Continue?".into(),
                            timeout: None,
                        },
                    )
                    .is_none()
            );
        }
        assert_eq!(state.dialogs.len(), EXTENSION_DIALOG_QUEUE_LIMIT);
        assert_eq!(
            state.apply(
                "overflow".into(),
                ExtensionUiRequest::Editor {
                    title: "Editor".into(),
                    prefill: None,
                },
            ),
            Some(ExtensionUiResponse::cancelled("overflow"))
        );
        assert_eq!(
            state.apply(
                "empty".into(),
                ExtensionUiRequest::Select {
                    title: "Empty".into(),
                    options: Vec::new(),
                    timeout: None,
                },
            ),
            Some(ExtensionUiResponse::cancelled("empty"))
        );
        assert_eq!(
            state.apply(
                "too-many-options".into(),
                ExtensionUiRequest::Select {
                    title: "Too many".into(),
                    options: vec!["value".into(); EXTENSION_DIALOG_OPTIONS_LIMIT + 1],
                    timeout: None,
                },
            ),
            Some(ExtensionUiResponse::cancelled("too-many-options"))
        );
        assert!(
            state
                .take_diagnostic()
                .is_some_and(|diagnostic| diagnostic.contains("超过上限"))
        );
        assert_eq!(
            state.apply(
                "dialog-0".into(),
                ExtensionUiRequest::Select {
                    title: "Duplicate".into(),
                    options: Vec::new(),
                    timeout: None,
                },
            ),
            None,
            "重复 id 的空 Select 不得发送第二个 cancelled response"
        );
        for index in 0..(EXTENSION_NOTIFICATION_QUEUE_LIMIT + 3) {
            state.apply(
                format!("notify-{index}"),
                ExtensionUiRequest::Notify {
                    message: index.to_string(),
                    notify_type: None,
                },
            );
        }
        assert_eq!(
            state.notifications.len(),
            EXTENSION_NOTIFICATION_QUEUE_LIMIT
        );
        assert_eq!(state.notifications.front().unwrap().message, "3");
        assert_eq!(
            state.drain_cancelled_dialogs().len(),
            EXTENSION_DIALOG_QUEUE_LIMIT
        );
        assert!(state.active_dialog().is_none());
    }

    #[test]
    fn extension_editable_payloads_are_not_silently_truncated() {
        let mut state = ExtensionUiState::default();
        let raw = format!("line\nwith\ttab\u{0}\u{202e}{}", "x".repeat(5000));
        let expected = sanitize_extension_editable(&raw);
        assert!(expected.len() > EXTENSION_TEXT_LIMIT);
        assert_eq!(
            state.apply(
                "editor".into(),
                ExtensionUiRequest::Editor {
                    title: "Editor".into(),
                    prefill: Some(raw.clone()),
                },
            ),
            None
        );
        let ExtensionUiRequest::Editor {
            prefill: Some(prefill),
            ..
        } = &state.active_dialog().unwrap().request
        else {
            panic!("editor expected");
        };
        assert_eq!(prefill, &expected);
        state.finish_dialog("editor");
        state.apply(
            "set-editor".into(),
            ExtensionUiRequest::SetEditorText { text: raw },
        );
        assert_eq!(state.take_editor_text().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn status_and_widget_raw_keys_do_not_collide_after_display_sanitization() {
        let mut state = ExtensionUiState::default();
        for key in ["same", "sa\u{200b}me"] {
            state.apply(
                format!("status-{key}"),
                ExtensionUiRequest::SetStatus {
                    status_key: key.into(),
                    status_text: Some(key.into()),
                },
            );
            state.apply(
                format!("widget-{key}"),
                ExtensionUiRequest::SetWidget {
                    widget_key: key.into(),
                    widget_lines: Some(vec!["line".into()]),
                    widget_placement: None,
                },
            );
        }
        assert_eq!(state.statuses().count(), 2);
        assert_eq!(state.widgets.len(), 2);
        state.apply(
            "remove-status".into(),
            ExtensionUiRequest::SetStatus {
                status_key: "sa\u{200b}me".into(),
                status_text: None,
            },
        );
        assert_eq!(state.statuses().count(), 1);
        assert_eq!(state.statuses().next().unwrap().raw_key, "same");
    }

    #[test]
    fn extension_ui_coalescing_keeps_first_key_position_and_latest_value() {
        let requests = vec![
            (
                "status-1".into(),
                ExtensionUiRequest::SetStatus {
                    status_key: "status".into(),
                    status_text: Some("one".into()),
                },
            ),
            (
                "notify".into(),
                ExtensionUiRequest::Notify {
                    message: "middle".into(),
                    notify_type: None,
                },
            ),
            (
                "status-2".into(),
                ExtensionUiRequest::SetStatus {
                    status_key: "status".into(),
                    status_text: Some("two".into()),
                },
            ),
        ];
        let coalesced = coalesce_extension_ui_requests(requests);
        assert_eq!(coalesced.len(), 2);
        assert_eq!(coalesced[0].0, "status-2");
        assert!(matches!(
            &coalesced[0].1,
            ExtensionUiRequest::SetStatus {
                status_text: Some(text),
                ..
            } if text == "two"
        ));
        assert!(matches!(coalesced[1].1, ExtensionUiRequest::Notify { .. }));
    }

    #[test]
    fn extension_ui_state_upserts_sanitizes_and_resets() {
        let mut state = ExtensionUiState::default();
        state.apply(
            "status-2".into(),
            ExtensionUiRequest::SetStatus {
                status_key: "z".into(),
                status_text: Some("run\u{1b}[31m".into()),
            },
        );
        state.apply(
            "status-1".into(),
            ExtensionUiRequest::SetStatus {
                status_key: "a".into(),
                status_text: Some("ready".into()),
            },
        );
        assert_eq!(
            state
                .statuses()
                .map(|status| (status.raw_key.as_str(), status.text.as_str()))
                .collect::<Vec<_>>(),
            [("a", "ready"), ("z", "run[31m")]
        );
        state.apply(
            "widget".into(),
            ExtensionUiRequest::SetWidget {
                widget_key: "fixture".into(),
                widget_lines: Some(vec!["line\u{0}".into()]),
                widget_placement: None,
            },
        );
        assert_eq!(
            state
                .widgets(WidgetPlacement::AboveEditor)
                .next()
                .unwrap()
                .lines,
            ["line"]
        );
        state.apply(
            "dialog".into(),
            ExtensionUiRequest::Confirm {
                title: "Confirm".into(),
                message: "Continue?".into(),
                timeout: None,
            },
        );
        assert_eq!(state.active_dialog().unwrap().id, "dialog");
        assert!(!state.finish_dialog("stale"));
        assert!(state.finish_dialog("dialog"));
        state.reset();
        assert!(state.statuses().next().is_none());
        assert!(state.active_dialog().is_none());
    }

    fn test_document(id: &str) -> ConversationDocument {
        ConversationDocument {
            session_id: id.to_owned(),
            source_path: PathBuf::from(format!("{id}.jsonl")),
            cwd: PathBuf::from("project"),
            messages: Arc::from([]),
            items: Arc::from([]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        }
    }

    #[test]
    fn dispatch_rejected_after_prior_activity_restores_idle_from_runtime_generation() {
        let (manager, handle) = runtime_handle();
        handle
            .dispatch(
                RpcIntent::Prompt,
                Some(ComposerSubmission {
                    message: "complete".into(),
                    images: Vec::new(),
                }),
                ComposerMode::Steer,
            )
            .unwrap();
        wait_for_snapshot(&handle, Duration::from_secs(3), |snapshot| {
            snapshot.phase == LivePhase::Idle
                && handle.entry.state.lock().unwrap().activity_generation == 1
        });
        handle
            .dispatch(
                RpcIntent::Prompt,
                Some(ComposerSubmission {
                    message: "reject".into(),
                    images: Vec::new(),
                }),
                ComposerMode::Steer,
            )
            .unwrap();
        let snapshot = wait_for_snapshot(&handle, Duration::from_secs(3), |snapshot| {
            snapshot.effects.iter().any(|effect| {
                matches!(
                    &effect.kind,
                    RuntimeEffectKind::RequestFinished {
                        pending_activity_generation: Some(1),
                        result: Err((RequestFailureKind::Rejected, _)),
                        ..
                    }
                )
            })
        });
        assert_eq!(snapshot.phase, LivePhase::Idle);
        manager.stop_user(handle.runtime_id());
    }

    #[test]
    fn dispatch_ambiguous_timeout_restores_idle_from_runtime_generation() {
        let (manager, handle) = runtime_handle();
        handle
            .dispatch(
                RpcIntent::Prompt,
                Some(ComposerSubmission {
                    message: "complete".into(),
                    images: Vec::new(),
                }),
                ComposerMode::Steer,
            )
            .unwrap();
        wait_for_snapshot(&handle, Duration::from_secs(3), |snapshot| {
            snapshot.phase == LivePhase::Idle
                && handle.entry.state.lock().unwrap().activity_generation == 1
        });
        handle
            .dispatch_with_timeout(
                RpcIntent::Prompt,
                Some(ComposerSubmission {
                    message: "ignored".into(),
                    images: Vec::new(),
                }),
                ComposerMode::Steer,
                Duration::from_millis(40),
            )
            .unwrap();
        let snapshot = wait_for_snapshot(&handle, Duration::from_secs(3), |snapshot| {
            snapshot.effects.iter().any(|effect| {
                matches!(
                    &effect.kind,
                    RuntimeEffectKind::RequestFinished {
                        pending_activity_generation: Some(1),
                        result: Err((RequestFailureKind::Ambiguous, _)),
                        ..
                    }
                )
            })
        });
        assert_eq!(snapshot.phase, LivePhase::Idle);
        manager.stop_user(handle.runtime_id());
    }

    #[test]
    fn one_shot_effects_remain_reliable_beyond_256_publications() {
        let entry = RuntimeEntry::test_entry(RuntimeId(6), test_document("effects"));
        for index in 0..300 {
            let mut state = entry.state.lock().unwrap();
            entry.publish(
                &mut state,
                RuntimeEffectKind::ControlFinished {
                    operation: ControlOperation::ExportHtml,
                    result: Err(format!("result-{index}")),
                },
            );
        }
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.effects.len(), 300);
        assert_eq!(snapshot.effects.first().unwrap().sequence, 1);
        assert_eq!(snapshot.effects.last().unwrap().sequence, 300);
    }

    #[test]
    fn crash_with_restart_disabled_publishes_one_failed_terminal_state() {
        let (manager, handle) = runtime_handle();
        handle
            .dispatch(
                RpcIntent::Prompt,
                Some(ComposerSubmission {
                    message: "crash".into(),
                    images: Vec::new(),
                }),
                ComposerMode::Steer,
            )
            .unwrap();
        let snapshot = wait_for_snapshot(&handle, Duration::from_secs(5), |snapshot| {
            snapshot.effects.iter().any(|effect| {
                matches!(
                    &effect.kind,
                    RuntimeEffectKind::Stopped(Some(error))
                        if error.contains("会话已崩溃") && error.contains("请重新启动")
                )
            })
        });
        assert_eq!(snapshot.phase, LivePhase::Error);
        assert_eq!(
            snapshot
                .effects
                .iter()
                .filter(|effect| matches!(effect.kind, RuntimeEffectKind::Stopped(_)))
                .count(),
            1
        );
        assert!(!snapshot.effects.iter().any(|effect| {
            matches!(
                &effect.kind,
                RuntimeEffectKind::Diagnostic(message)
                    if message.contains("Restarting") || message.contains("Restarted")
            )
        }));
        assert!(
            handle
                .dispatch(RpcIntent::Abort, None, ComposerMode::Steer)
                .is_err()
        );
        manager.stop_user(handle.runtime_id());
    }

    #[test]
    fn tool_restart_failure_fences_old_pump_stopped_effect() {
        let (manager, handle) = runtime_handle();
        let epoch = handle.snapshot().epoch;
        handle
            .restart_with_tools(
                PathBuf::from("definitely-missing-pi-runtime-binary.exe"),
                None,
                std::env::temp_dir(),
                test_document("restart"),
                ToolPreset::ReadOnly,
            )
            .unwrap();
        let snapshot = wait_for_snapshot(&handle, Duration::from_secs(5), |snapshot| {
            snapshot.effects.iter().any(|effect| {
                matches!(
                    &effect.kind,
                    RuntimeEffectKind::ToolRestartFinished { result: Err(_), .. }
                )
            })
        });
        assert_eq!(snapshot.epoch, epoch);
        assert_eq!(snapshot.phase, LivePhase::Error);
        assert!(
            !snapshot
                .effects
                .iter()
                .any(|effect| matches!(effect.kind, RuntimeEffectKind::Stopped(_)))
        );
        manager.stop_user(handle.runtime_id());
    }

    #[test]
    fn start_user_spawn_failure_preserves_existing_active_handle() {
        let (manager, handle) = runtime_handle();
        let runtime_id = handle.runtime_id();
        let error = match manager.start_fresh(
            PathBuf::from("definitely-missing-pi-runtime-binary.exe"),
            std::env::temp_dir(),
            test_document("failed-new"),
            ToolPreset::Inherit,
            None,
        ) {
            Ok(_) => panic!("missing binary unexpectedly spawned"),
            Err(error) => error,
        };
        assert!(!error.is_empty());
        let active = (*manager.inner.active_user.lock().unwrap()).expect("旧会话必须原样还在");
        assert_eq!(manager.session_state(active), Some(SchedulerState::Running));
        let entry = manager
            .session_handle(active)
            .expect("活跃会话仍持有 Runtime");
        assert_eq!(entry.runtime_id(), runtime_id);
        assert!(entry.snapshot().terminal.is_none());
        // 失败的那次尝试不得留下任何残迹：会话注册表与运行槽都必须回到只剩旧会话。
        let report = manager.scheduler_report();
        assert_eq!(report.running, 1);
        assert_eq!(report.resident_pi, 1);
        assert_eq!(report.failed, 0);
        manager.stop_user(runtime_id);
    }

    #[test]
    fn runtime_id_revision_and_handle_are_stable() {
        let entry = RuntimeEntry::test_entry(RuntimeId(7), test_document("fixture"));
        let handle = SessionHandle {
            entry: entry.clone(),
        };
        let clone = handle.clone();
        assert_eq!(handle.runtime_id(), RuntimeId(7));
        assert_eq!(clone.runtime_id(), RuntimeId(7));
        let initial = handle.snapshot();
        {
            let mut state = entry.state.lock().unwrap();
            entry.publish(&mut state, RuntimeEffectKind::Diagnostic("changed".into()));
        }
        let changed = clone.snapshot();
        assert_eq!(changed.epoch, initial.epoch);
        assert!(changed.revision > initial.revision);
        assert_eq!(changed.effects.last().unwrap().sequence, 1);
    }

    #[test]
    fn old_epoch_effects_are_fenced() {
        let entry = RuntimeEntry::test_entry(RuntimeId(8), test_document("fixture"));
        {
            let mut state = entry.state.lock().unwrap();
            state.epoch = 2;
        }
        publish_if_current(&entry, 1, RuntimeEffectKind::Diagnostic("stale".into()));
        assert!(entry.snapshot().effects.is_empty());
        publish_if_current(&entry, 2, RuntimeEffectKind::Diagnostic("current".into()));
        assert_eq!(entry.snapshot().effects.len(), 1);
    }

    #[test]
    fn manager_config_always_disables_rpc_auto_restart() {
        let mut config = ClientConfig::new("pi.exe");
        config.max_restarts = 99;
        clamp_manager_config(&mut config, default_tuning());
        assert_eq!(config.max_restarts, 0);
    }

    #[test]
    fn maintenance_gate_serializes_shared_callers() {
        let gate = Arc::new(MaintenanceGate::new(1));
        let first = gate.acquire();
        let acquired = Arc::new(AtomicU64::new(0));
        let worker_gate = gate.clone();
        let worker_acquired = acquired.clone();
        let worker = thread::spawn(move || {
            let _permit = worker_gate.acquire();
            worker_acquired.store(1, Ordering::Release);
        });
        thread::sleep(Duration::from_millis(30));
        assert_eq!(acquired.load(Ordering::Acquire), 0);
        drop(first);
        worker.join().unwrap();
        assert_eq!(acquired.load(Ordering::Acquire), 1);
    }

    #[test]
    fn projects_agent_end_and_settled_separately() {
        assert_eq!(
            project_event(ClientEvent::Rpc(Box::new(RpcEvent::AgentEnd {
                messages: Vec::new(),
                will_retry: false,
            }))),
            Some(LiveEvent::AgentEnd)
        );
        assert_eq!(
            project_event(ClientEvent::Rpc(Box::new(RpcEvent::AgentSettled))),
            Some(LiveEvent::AgentSettled)
        );
    }

    // ---------- R22：有界 Actor 与事件背压 ----------

    fn tuning_with(effects: EffectLimits) -> RuntimeTuning {
        RuntimeTuning::from_limits(&RuntimeLimits {
            effects,
            ..RuntimeLimits::default()
        })
    }

    fn bounded_entry(id: u64, effects: EffectLimits) -> Arc<RuntimeEntry> {
        RuntimeEntry::test_entry_with(
            RuntimeId(id),
            test_document("bounded"),
            tuning_with(effects),
        )
    }

    fn publish_now(entry: &Arc<RuntimeEntry>, kind: RuntimeEffectKind) {
        let mut state = entry.state.lock().unwrap();
        entry.publish(&mut state, kind);
    }

    fn events_effect(
        follow_tail: bool,
        settled: bool,
        runtime_events: Vec<SessionRuntimeEvent>,
    ) -> RuntimeEffectKind {
        RuntimeEffectKind::Events {
            follow_tail,
            settled,
            runtime_events,
        }
    }

    fn fat_submission(bytes: usize) -> ComposerSubmission {
        ComposerSubmission {
            message: "x".repeat(bytes),
            images: Vec::new(),
        }
    }

    #[test]
    fn event_frame_is_clamped_into_the_documented_window() {
        assert!(EVENT_FRAME_MIN <= DEFAULT_EVENT_FRAME && DEFAULT_EVENT_FRAME <= EVENT_FRAME_MAX);
        assert_eq!(clamp_event_frame(Duration::from_millis(1)), EVENT_FRAME_MIN);
        assert_eq!(clamp_event_frame(Duration::from_secs(1)), EVENT_FRAME_MAX);
        assert_eq!(clamp_event_frame(DEFAULT_EVENT_FRAME), DEFAULT_EVENT_FRAME);
        // 越界配置必须在 tuning 固化时就被收敛，而不是留给各个消费点自己 clamp。
        let tuning = RuntimeTuning::from_limits(&RuntimeLimits {
            event_frame: Duration::from_secs(5),
            ..RuntimeLimits::default()
        });
        assert_eq!(tuning.event_frame, EVENT_FRAME_MAX);
    }

    #[test]
    fn default_command_queue_capacity_matches_the_design_document() {
        assert_eq!(ActorLimits::default().command_capacity, 32);
        assert!(ActorLimits::default().control_capacity >= 1);
    }

    #[test]
    fn latest_only_keys_supersede_older_values_without_touching_reliable_results() {
        let entry = bounded_entry(50, EffectLimits::default());
        publish_now(
            &entry,
            RuntimeEffectKind::CommandsLoaded(Err("first".into())),
        );
        publish_now(
            &entry,
            RuntimeEffectKind::ControlFinished {
                operation: ControlOperation::Compact,
                result: Err("one-shot".into()),
            },
        );
        publish_now(
            &entry,
            RuntimeEffectKind::CommandsLoaded(Err("second".into())),
        );
        publish_now(
            &entry,
            RuntimeEffectKind::CommandsLoaded(Err("third".into())),
        );

        let snapshot = entry.snapshot();
        let commands = snapshot
            .effects
            .iter()
            .filter(|effect| matches!(effect.kind, RuntimeEffectKind::CommandsLoaded(_)))
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), 1, "同 key 只保留最新一条");
        assert!(matches!(
            &commands[0].kind,
            RuntimeEffectKind::CommandsLoaded(Err(message)) if message == "third"
        ));
        assert!(
            snapshot
                .effects
                .iter()
                .any(|effect| matches!(&effect.kind, RuntimeEffectKind::ControlFinished { .. })),
            "一次性结果不得被 latest-only 合并顺带丢掉"
        );
        assert!(snapshot.backpressure.coalesced >= 2);
    }

    #[test]
    fn adjacent_event_frames_merge_and_keep_latest_runtime_event_per_family() {
        let entry = bounded_entry(51, EffectLimits::default());
        publish_now(
            &entry,
            events_effect(
                false,
                false,
                vec![
                    SessionRuntimeEvent::CompactionStarted,
                    SessionRuntimeEvent::RetryStarted {
                        attempt: 1,
                        max_attempts: 3,
                        delay_ms: 10,
                        error: "first".into(),
                    },
                ],
            ),
        );
        publish_now(
            &entry,
            events_effect(
                true,
                false,
                vec![SessionRuntimeEvent::RetryStarted {
                    attempt: 2,
                    max_attempts: 3,
                    delay_ms: 10,
                    error: "second".into(),
                }],
            ),
        );
        publish_now(
            &entry,
            events_effect(
                false,
                true,
                vec![SessionRuntimeEvent::RetryEnded {
                    success: true,
                    attempt: 2,
                    error: None,
                }],
            ),
        );

        let snapshot = entry.snapshot();
        assert_eq!(snapshot.effects.len(), 1, "相邻帧必须合并成一条");
        let RuntimeEffectKind::Events {
            follow_tail,
            settled,
            runtime_events,
        } = &snapshot.effects[0].kind
        else {
            panic!("expected merged Events effect");
        };
        assert!(*follow_tail, "follow_tail 取并集");
        assert!(*settled, "settled 取并集");
        assert_eq!(
            runtime_events,
            &vec![
                SessionRuntimeEvent::CompactionStarted,
                SessionRuntimeEvent::RetryEnded {
                    success: true,
                    attempt: 2,
                    error: None,
                },
            ],
            "每个事件族只保留最新一条，族之间顺序不变"
        );
        // 合并后序号跟随最新一次发布，UI 的 cursor 才能继续单调推进。
        assert_eq!(snapshot.effects[0].sequence, 3);
    }

    #[test]
    fn extension_ui_batches_merge_by_id_but_never_reorder_across_a_reset() {
        let entry = bounded_entry(52, EffectLimits::default());
        let request = |title: &str| ExtensionUiRequest::SetTitle {
            title: title.to_owned(),
        };
        publish_now(
            &entry,
            RuntimeEffectKind::ExtensionUiBatch {
                requests: vec![("a".into(), request("first"))],
            },
        );
        publish_now(
            &entry,
            RuntimeEffectKind::ExtensionUiBatch {
                requests: vec![("a".into(), request("second"))],
            },
        );
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.effects.len(), 1);
        let RuntimeEffectKind::ExtensionUiBatch { requests } = &snapshot.effects[0].kind else {
            panic!("expected batch");
        };
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].1, request("second"), "同 id 取最新");

        // reset 是顺序敏感的屏障：它之前的请求不能被之后的批次合并回来。
        publish_now(&entry, RuntimeEffectKind::ExtensionUiReset);
        publish_now(
            &entry,
            RuntimeEffectKind::ExtensionUiBatch {
                requests: vec![("b".into(), request("after-reset"))],
            },
        );
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.effects.len(), 3);
        assert!(matches!(
            snapshot.effects[1].kind,
            RuntimeEffectKind::ExtensionUiReset
        ));
        let RuntimeEffectKind::ExtensionUiBatch { requests } = &snapshot.effects[2].kind else {
            panic!("expected batch after reset");
        };
        assert_eq!(
            requests.len(),
            1,
            "reset 之后的批次不得吸收 reset 之前的请求"
        );
        assert_eq!(requests[0].0, "b");
    }

    #[test]
    fn byte_ceiling_strips_reconstructable_payloads_before_dropping_results() {
        let limits = EffectLimits {
            max_bytes: 64 * 1024,
            max_effects: 512,
            max_diagnostics: 4,
        };
        let entry = bounded_entry(53, limits);
        for index in 0..8 {
            publish_now(
                &entry,
                RuntimeEffectKind::RequestFinished {
                    intent: RpcIntent::Prompt,
                    submission: Some(fat_submission(32 * 1024)),
                    pending_activity_generation: None,
                    result: Err((RequestFailureKind::Rejected, format!("rejected-{index}"))),
                },
            );
        }
        let snapshot = entry.snapshot();
        assert!(
            snapshot.backpressure.buffered_bytes <= limits.max_bytes,
            "字节上限是硬上限：{} > {}",
            snapshot.backpressure.buffered_bytes,
            limits.max_bytes
        );
        assert!(snapshot.backpressure.stripped_submissions > 0);
        assert_eq!(
            snapshot.backpressure.dropped_results, 0,
            "剥离负载已经够用时不得淘汰一次性结果"
        );
        assert_eq!(snapshot.effects.len(), 8, "8 条结果全部保留");
        for (index, effect) in snapshot.effects.iter().enumerate() {
            let RuntimeEffectKind::RequestFinished { result, .. } = &effect.kind else {
                panic!("expected request result");
            };
            assert_eq!(
                result.as_ref().unwrap_err().1,
                format!("rejected-{index}"),
                "错误结果本身必须完整保留"
            );
        }
    }

    #[test]
    fn diagnostics_are_bounded_and_counted() {
        let limits = EffectLimits {
            max_bytes: 4 * 1024 * 1024,
            max_effects: 512,
            max_diagnostics: 4,
        };
        let entry = bounded_entry(54, limits);
        for index in 0..20 {
            publish_now(
                &entry,
                RuntimeEffectKind::Diagnostic(format!("diagnostic-{index}")),
            );
        }
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.effects.len(), 4, "诊断保留条数固定");
        assert_eq!(snapshot.backpressure.dropped_diagnostics, 16);
        assert!(matches!(
            &snapshot.effects[3].kind,
            RuntimeEffectKind::Diagnostic(message) if message == "diagnostic-19"
        ));
    }

    #[test]
    fn authoritative_terminal_state_survives_effect_eviction() {
        let limits = EffectLimits {
            max_bytes: 64 * 1024,
            max_effects: 8,
            max_diagnostics: 2,
        };
        let entry = bounded_entry(55, limits);
        fail_runtime(&entry, 1, "会话已崩溃".to_owned());
        assert_eq!(
            entry.snapshot().terminal,
            Some(TerminalState::Failed {
                error: "会话已崩溃".to_owned()
            })
        );

        // 把缓存灌到条数上限之外，逼出对可靠条目的淘汰。
        for index in 0..64 {
            publish_now(
                &entry,
                RuntimeEffectKind::ControlFinished {
                    operation: ControlOperation::Compact,
                    result: Err(format!("later-{index}")),
                },
            );
        }
        let snapshot = entry.snapshot();
        assert!(snapshot.effects.len() <= limits.max_effects);
        assert!(
            snapshot.backpressure.dropped_results > 0,
            "触发淘汰时必须留下可见计数"
        );
        assert_eq!(
            snapshot.terminal,
            Some(TerminalState::Failed {
                error: "会话已崩溃".to_owned()
            }),
            "权威终态不依赖 effect 流，任何背压下都必须仍然可读"
        );
    }

    #[test]
    fn ack_reclaims_consumed_effects() {
        let entry = bounded_entry(56, EffectLimits::default());
        for index in 0..10 {
            publish_now(&entry, RuntimeEffectKind::Diagnostic(format!("d-{index}")));
        }
        let snapshot = entry.snapshot();
        assert!(snapshot.backpressure.buffered_effects > 0);
        let last = snapshot.effects.last().unwrap().sequence;

        let handle = SessionHandle {
            entry: entry.clone(),
        };
        handle.ack_effects(snapshot.epoch, last);
        let after = entry.snapshot();
        assert_eq!(after.backpressure.buffered_effects, 0);
        assert_eq!(after.backpressure.buffered_bytes, 0);
        assert!(after.effects.is_empty());
    }

    /// 只看 reducer 相位，不读 Snapshot、不 ack —— 用来模拟「UI 完全停止消费」。
    fn wait_for_idle_without_consuming(handle: &SessionHandle, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let phase = handle.entry.state.lock().unwrap().reducer.phase();
            if phase == LivePhase::Idle {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for idle runtime"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn document_contains_markdown(document: &ConversationDocument, needle: &str) -> bool {
        document.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, pi_render::Block::Markdown(markdown) if markdown.source.contains(needle))
            })
        })
    }

    fn submission(message: &str) -> Option<ComposerSubmission> {
        Some(ComposerSubmission {
            message: message.to_owned(),
            images: Vec::new(),
        })
    }

    #[test]
    fn command_queue_is_bounded_while_the_control_channel_stays_available() {
        let actor = ActorLimits {
            command_capacity: 4,
            control_capacity: 4,
            command_workers: 1,
            control_workers: 1,
        };
        let manager = RuntimeManager::new(RuntimeLimits {
            actor,
            ..RuntimeLimits::default()
        });
        let handle = manager
            .start_fresh(
                fake_binary(),
                std::env::temp_dir(),
                test_document("bounded-queue"),
                ToolPreset::Inherit,
                None,
            )
            .unwrap();

        // fake child 对未知 prompt 会长时间沉睡，worker 与队列都被真实占住。
        // 启动时的元数据作业也在同一条通道上，所以「第几次被拒」不是确定值：
        // 一直投递到出现拒绝为止，并记录**被拒那一次**之前的快照。
        let mut rejected = None;
        for index in 0..256 {
            let before = handle.snapshot();
            let dispatched = handle.dispatch(
                RpcIntent::Prompt,
                submission(&format!("hang-{index}")),
                ComposerMode::Steer,
            );
            if let Err(error) = dispatched {
                rejected = Some((before, error));
                break;
            }
        }
        let (before, rejection) = rejected.expect("有界队列必须在有限次投递后拒绝");
        assert!(
            rejection.contains("命令队列已满"),
            "拒绝原因必须对用户可读：{rejection}"
        );

        // 被拒绝的投递不得改动 reducer —— 否则 UI 会停在假的 Running 上。
        let after = handle.snapshot();
        assert_eq!(before.phase, after.phase, "队列满不得改变会话相位");
        assert_eq!(
            before.backpressure.rejected_commands + 1,
            after.backpressure.rejected_commands,
            "每次拒绝都必须留下可见计数"
        );
        assert!(after.backpressure.queued_commands <= actor.command_capacity);

        // 控制通道与普通通道分离：普通队列饱和时停止请求仍然被受理。
        handle
            .dispatch(RpcIntent::Abort, None, ComposerMode::Steer)
            .expect("控制通道必须在普通通道饱和时仍可投递");

        // 线程预算固定：worker + 事件 pump，与投递次数无关。
        assert_eq!(
            handle.live_thread_count(),
            actor.command_workers + actor.control_workers + 1
        );
        manager.stop_user(handle.runtime_id());
    }

    #[test]
    fn paused_ui_still_resumes_with_a_complete_final_snapshot() {
        let (manager, handle) = runtime_handle();
        // 等启动元数据就绪，但**不 ack**：验收要求「UI 暂停消费后恢复仍得到完整最终
        // Snapshot」，其中就包括此前没来得及消费的最新 controls。
        wait_for_snapshot(&handle, Duration::from_secs(5), |snapshot| {
            snapshot
                .effects
                .iter()
                .any(|effect| matches!(effect.kind, RuntimeEffectKind::ControlsLoaded(Ok(_))))
        });

        handle
            .dispatch(RpcIntent::Prompt, submission("stream"), ComposerMode::Steer)
            .unwrap();
        // 整个流式过程中完全不读 Snapshot、不 ack，模拟 UI 卡死。
        wait_for_idle_without_consuming(&handle, Duration::from_secs(30));

        let snapshot = handle.snapshot();
        assert_eq!(snapshot.phase, LivePhase::Idle);
        assert!(
            document_contains_markdown(&snapshot.document, "authoritative"),
            "恢复消费后必须拿到权威终版正文，而不是被背压截断的中间态"
        );
        assert!(
            snapshot.backpressure.buffered_bytes <= handle.effect_limits().max_bytes,
            "effect 缓存必须始终在字节上限内：{} > {}",
            snapshot.backpressure.buffered_bytes,
            handle.effect_limits().max_bytes
        );
        assert!(
            snapshot.backpressure.coalesced > 0,
            "1500 条流式更新必须被合帧，而不是逐条堆进 effect 流"
        );
        assert_eq!(
            snapshot.backpressure.dropped_results, 0,
            "一次性结果不得因为 UI 停摆而被丢弃"
        );
        assert!(
            snapshot.effects.iter().any(|effect| matches!(
                &effect.kind,
                RuntimeEffectKind::RequestFinished { result: Ok(()), .. }
            )),
            "提交结果必须仍然可读"
        );
        assert!(
            snapshot
                .effects
                .iter()
                .any(|effect| matches!(effect.kind, RuntimeEffectKind::ControlsLoaded(Ok(_)))),
            "暂停期间未消费的最新 controls 必须仍在最终 Snapshot 中"
        );
        assert!(
            snapshot
                .effects
                .iter()
                .any(|effect| matches!(effect.kind, RuntimeEffectKind::CommandsLoaded(Ok(_)))),
            "暂停期间未消费的 slash 命令列表同理"
        );
        assert_eq!(snapshot.terminal, None, "正常结束的会话没有终态");
        manager.stop_user(handle.runtime_id());
    }

    #[test]
    fn graceful_stop_is_observable_as_a_terminal_state() {
        let (manager, handle) = runtime_handle();
        assert_eq!(handle.snapshot().terminal, None);

        manager.stop_user(handle.runtime_id());

        let snapshot = handle.snapshot();
        assert_eq!(
            snapshot.terminal,
            Some(TerminalState::Stopped),
            "优雅停止必须留下可观察终态，观察者不该只能靠超时判断"
        );
        assert!(
            snapshot
                .effects
                .iter()
                .any(|effect| matches!(effect.kind, RuntimeEffectKind::Stopped(None))),
            "优雅停止同时发布无错误的终态 effect"
        );
        // 队列关闭后不再接受任何投递。
        assert!(
            handle
                .dispatch(
                    RpcIntent::Prompt,
                    submission("after-stop"),
                    ComposerMode::Steer
                )
                .is_err()
        );
    }

    // ---------- R22 代码审查整改的回归测试 ----------

    /// H1 回归：合并路径此前直接 `return`，跳过 `enforce_limits()`，
    /// 字节/条数硬上限在「连续只含扩展请求的帧」这条路上完全不生效。
    #[test]
    fn merged_entries_are_still_subject_to_the_byte_ceiling() {
        let limits = EffectLimits {
            max_bytes: 64 * 1024,
            max_effects: 512,
            max_diagnostics: 4,
        };
        let entry = bounded_entry(60, limits);
        // 每帧一个**新 id** 的对话请求：既不能按语义 key 折叠，也不能按 id 折叠，
        // 只能真实堆进同一条 ExtensionUiBatch —— 正是当年绕过上限的那条路径。
        for index in 0..64 {
            publish_now(
                &entry,
                RuntimeEffectKind::ExtensionUiBatch {
                    requests: vec![(
                        format!("dialog-{index}"),
                        ExtensionUiRequest::Confirm {
                            title: "T".repeat(2048),
                            message: "M".repeat(2048),
                            timeout: None,
                        },
                    )],
                },
            );
        }
        let snapshot = entry.snapshot();
        assert!(
            snapshot.backpressure.buffered_bytes <= limits.max_bytes,
            "合并后的条目必须同样受字节上限约束：{} > {}",
            snapshot.backpressure.buffered_bytes,
            limits.max_bytes
        );
    }

    /// H1 次要点回归：pi 每次调用都生成新 id，只按 id 去重会让同一个 statusKey
    /// 无限堆积。跨帧合并必须和 pump 内单帧合并用同一套语义 key。
    #[test]
    fn merged_extension_batches_fold_status_and_widget_keys_not_just_ids() {
        let entry = bounded_entry(61, EffectLimits::default());
        for index in 0..50 {
            publish_now(
                &entry,
                RuntimeEffectKind::ExtensionUiBatch {
                    requests: vec![(
                        format!("req-{index}"),
                        ExtensionUiRequest::SetStatus {
                            status_key: "build".into(),
                            status_text: Some(format!("step {index}")),
                        },
                    )],
                },
            );
        }
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.effects.len(), 1);
        let RuntimeEffectKind::ExtensionUiBatch { requests } = &snapshot.effects[0].kind else {
            panic!("expected batch");
        };
        assert_eq!(requests.len(), 1, "同一个 statusKey 必须折叠成一条");
        assert_eq!(
            requests[0].1,
            ExtensionUiRequest::SetStatus {
                status_key: "build".into(),
                status_text: Some("step 49".into()),
            },
            "折叠后保留最新的值"
        );
    }

    /// M2 回归：已经交付给 UI 的队尾条目不得再被合并。
    ///
    /// 否则抬序号会让 UI 把旧内容重复应用一遍（错误横幅永远退不掉），
    /// 不抬序号则新内容永远越不过 cursor。
    #[test]
    fn delivered_entries_are_never_merged_into() {
        let entry = bounded_entry(62, EffectLimits::default());
        publish_now(
            &entry,
            events_effect(
                false,
                false,
                vec![SessionRuntimeEvent::CompactionEnded {
                    error: Some("boom".into()),
                }],
            ),
        );
        // UI 拉取快照 = 这条已经交付。
        let delivered = entry.snapshot();
        assert_eq!(delivered.effects.len(), 1);
        let first_sequence = delivered.effects[0].sequence;

        // 交付之后到来的新帧必须另起一条，而不是并进已交付的那条。
        publish_now(&entry, events_effect(true, true, Vec::new()));
        let after = entry.snapshot();
        assert_eq!(after.effects.len(), 2, "已交付条目不得被合并");
        assert_eq!(
            after.effects[0].sequence, first_sequence,
            "已交付条目序号不得被改写"
        );
        assert!(after.effects[1].sequence > first_sequence);
        let RuntimeEffectKind::Events { runtime_events, .. } = &after.effects[1].kind else {
            panic!("expected events");
        };
        assert!(
            runtime_events.is_empty(),
            "新条目不得携带 UI 已经应用过的运行时事件"
        );

        // 未交付条目之间仍然合并（UI 停摆时的正常路径）：连续 publish 而中间不取快照，
        // 因为取快照本身就会把交付水位推到队尾。
        let before_stall = after.effects.len();
        for _ in 0..10 {
            publish_now(&entry, events_effect(false, false, Vec::new()));
        }
        assert_eq!(
            entry.snapshot().effects.len(),
            before_stall + 1,
            "UI 停摆期间的连续帧必须合并成一条"
        );
    }

    /// M2 回归：ack 必须能真正回收已消费条目，稳态下缓存回到空。
    #[test]
    fn ack_reclaims_the_tail_even_while_new_frames_keep_arriving() {
        let entry = bounded_entry(63, EffectLimits::default());
        let handle = SessionHandle {
            entry: entry.clone(),
        };
        for _ in 0..20 {
            publish_now(&entry, events_effect(false, false, Vec::new()));
            let snapshot = entry.snapshot();
            let cursor = snapshot.effects.last().unwrap().sequence;
            handle.ack_effects(snapshot.epoch, cursor);
            assert_eq!(
                entry.snapshot().backpressure.buffered_effects,
                0,
                "每帧 ack 之后缓存必须回到空"
            );
        }
    }

    /// N13 补测：`ControlsLoaded` 与 `CommandsLoaded` 共用跨条目淘汰分支，
    /// 但此前只有 `CommandsLoaded` 有直接断言。
    #[test]
    fn controls_loaded_keeps_only_the_latest_value() {
        let entry = bounded_entry(64, EffectLimits::default());
        for index in 0..5 {
            publish_now(
                &entry,
                RuntimeEffectKind::ControlsLoaded(Err(format!("controls-{index}"))),
            );
        }
        let snapshot = entry.snapshot();
        let controls = snapshot
            .effects
            .iter()
            .filter(|effect| matches!(effect.kind, RuntimeEffectKind::ControlsLoaded(_)))
            .collect::<Vec<_>>();
        assert_eq!(controls.len(), 1);
        assert!(matches!(
            &controls[0].kind,
            RuntimeEffectKind::ControlsLoaded(Err(message)) if message == "controls-4"
        ));
        assert_eq!(snapshot.backpressure.coalesced, 4, "每淘汰一条旧值计一次");
    }

    /// L5 回归：合帧丢弃的运行时事件必须计数，不能无声消失。
    #[test]
    fn dropped_runtime_events_are_counted() {
        let entry = bounded_entry(65, EffectLimits::default());
        publish_now(
            &entry,
            events_effect(
                false,
                false,
                vec![SessionRuntimeEvent::RetryEnded {
                    success: false,
                    attempt: 1,
                    error: Some("first".into()),
                }],
            ),
        );
        publish_now(
            &entry,
            events_effect(
                false,
                false,
                vec![SessionRuntimeEvent::RetryStarted {
                    attempt: 2,
                    max_attempts: 3,
                    delay_ms: 10,
                    error: "second".into(),
                }],
            ),
        );
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.effects.len(), 1);
        assert_eq!(
            snapshot.backpressure.dropped_runtime_events, 1,
            "同族取最新丢掉的那条必须计数"
        );
    }

    /// L8 回归：控制通道被拒的文案必须点明是控制队列，别让用户以为是 32 的命令队列。
    #[test]
    fn queue_full_message_names_the_channel() {
        let command = actor::QueueError::Full {
            channel: actor::Channel::Command,
            capacity: 32,
        }
        .message();
        assert!(
            command.contains("命令队列") && command.contains("32"),
            "{command}"
        );
        let control = actor::QueueError::Full {
            channel: actor::Channel::Control,
            capacity: 8,
        }
        .message();
        assert!(
            control.contains("控制队列") && control.contains("8"),
            "{control}"
        );
    }
    fn slot_fixture(state: SchedulerState) -> SessionSlot {
        SessionSlot {
            descriptor: SessionDescriptor {
                binary: PathBuf::from("pi"),
                cwd: PathBuf::from("."),
                session_path: None,
                tool_preset: ToolPreset::Inherit,
                agent_dir: None,
            },
            history: test_document("slot"),
            state,
            priority: Priority::default(),
            entry: None,
            lease: None,
            failure: None,
            watch: Arc::new(SchedulerWatch::default()),
        }
    }

    /// 复审 P1-4：控制类作业（Compact / Fork / SwitchSession）**不改 reducer 的 phase**。
    ///
    /// 只看 phase 会把「队列里还压着一次 Fork」当成空闲，而 `actor.close()` 会把排队作业
    /// 直接丢掉、正在跑的那条又会被抽掉 client —— 一次用户点过的 Compact 或一次不可重放的
    /// Fork 就这么无声消失。因此 Park 的准入必须把 Actor 的排队与在执行一起算进去。
    #[test]
    fn reserve_park_refuses_while_actor_jobs_are_outstanding() {
        let entry = RuntimeEntry::test_entry(RuntimeId(11), test_document("pending"));
        assert_eq!(entry.state.lock().unwrap().reducer.phase(), LivePhase::Idle);

        let (release, blocked) = std::sync::mpsc::channel::<()>();
        entry
            .actor
            .push(actor::Channel::Control, None, move || {
                let _ = blocked.recv();
            })
            .expect("control queue has room");
        // 等它真正被 worker 取走，构造出「phase 空闲但作业在跑」这个状态。
        let deadline = Instant::now() + Duration::from_secs(30);
        while entry.actor.in_flight() == 0 {
            assert!(Instant::now() < deadline, "控制作业迟迟没有开始执行");
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(
            reserve_park(&entry).err(),
            Some(ParkRefusal::PendingJobs),
            "phase 空闲但还有作业在跑时必须拒绝 Park"
        );
        {
            let state = entry.state.lock().unwrap();
            assert!(!state.stopped, "被拒绝的 Park 不得置停止位");
            assert!(state.terminal.is_none(), "被拒绝的 Park 不得写终态");
        }

        release.send(()).expect("unblock the control job");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !entry.actor.is_idle() {
            assert!(Instant::now() < deadline, "控制作业没有收尾");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(reserve_park(&entry).is_ok(), "作业收干净之后必须可以 Park");
    }

    /// 复审（四轮）P1：「已释放」必须同时满足两件事 —— 拆除流程走完，**且**没有在途作业
    /// 还攥着进程句柄。
    ///
    /// 终态是在 `shutdown` 之前发布的，`fail_runtime` / `shutdown_entry` 关进程要花到
    /// grace period。只看标记位的话，并发的 `park` 会在旧进程还在退出时就把运行槽让出去。
    #[test]
    fn a_runtime_is_not_released_while_a_job_still_owns_its_process() {
        let entry = RuntimeEntry::test_entry(RuntimeId(41), test_document("owned"));
        assert!(!entry.is_released());

        let owner = ClientOwnerGuard::acquire(&entry);
        shutdown_entry(&entry);
        assert!(
            entry.released.load(Ordering::Acquire),
            "拆除流程本身确实走完了"
        );
        assert!(
            !entry.is_released(),
            "但还有在途作业攥着进程句柄，此刻不得归还运行槽"
        );

        drop(owner);
        assert!(entry.is_released(), "持有者归零之后才算真的释放");
    }

    /// 复审（三轮）P2：pump 被 detach 唤醒时手上那一帧如果还压着事件，这些事件已经
    /// 无处可去（entry 已被置 `stopped`，publish 会被 fence 掉）。其中若含扩展 UI 请求，
    /// pi 里那个扩展可能正等着回应 —— 这样的进程绝不能进 warm pool 交给下一个会话。
    #[test]
    fn a_runtime_whose_pump_dropped_a_frame_is_never_pooled() {
        let entry = RuntimeEntry::test_entry(RuntimeId(31), test_document("lossy"));
        entry.pump_lost_events.store(true, Ordering::Release);
        let parked = finish_park(
            &entry,
            ParkedRuntime {
                client: None,
                history: test_document("lossy"),
                session_file: None,
                reusable: true,
            },
        );
        assert!(
            !parked.reusable,
            "丢过一帧事件的进程状态不明，只能关掉、不能复用"
        );

        // 对照：没丢过事件的照常可复用。
        let clean = RuntimeEntry::test_entry(RuntimeId(32), test_document("clean"));
        let parked = finish_park(
            &clean,
            ParkedRuntime {
                client: None,
                history: test_document("clean"),
                session_file: None,
                reusable: true,
            },
        );
        assert!(parked.reusable);
    }

    /// 复审 P1-6：终态是在 `shutdown` **之前**发布的，因此「有终态」不等于「进程没了」。
    ///
    /// 调度器靠 `is_released()` 决定何时归还运行槽；任何一条拆除路径漏掉这个标记，
    /// 要么让运行槽被永久扣住（漏置），要么让新会话在旧进程还活着时补位（早置）。
    /// 这条用例逐路径钉死。
    #[test]
    fn every_teardown_path_marks_the_runtime_as_released() {
        let fresh = RuntimeEntry::test_entry(RuntimeId(21), test_document("fresh"));
        assert!(!fresh.is_released(), "刚建好的 Runtime 还持有进程");

        let stopped = RuntimeEntry::test_entry(RuntimeId(22), test_document("stopped"));
        shutdown_entry(&stopped);
        assert!(stopped.is_released(), "优雅停止之后必须标记已释放");

        let failed = RuntimeEntry::test_entry(RuntimeId(23), test_document("failed"));
        fail_runtime(&failed, 1, "boom".to_owned());
        assert!(failed.is_released(), "崩溃收尾之后必须标记已释放");

        // 工具预设重启失败：旧进程已在调用前 shutdown、新进程没起来，同样没有活进程了。
        // 漏掉这一处会让 `reap_terminated` 永远跳过该会话，运行槽被永久扣住。
        let restart_failed =
            RuntimeEntry::test_entry(RuntimeId(24), test_document("restart-failed"));
        publish_tool_restart_failure(
            &restart_failed,
            1,
            ToolPreset::Full,
            "spawn 失败".to_owned(),
        );
        assert!(
            restart_failed.is_released(),
            "工具预设重启失败之后必须标记已释放，否则运行槽再也回不来"
        );

        let parked = RuntimeEntry::test_entry(RuntimeId(25), test_document("parked"));
        let reservation = reserve_park(&parked).expect("空闲 Runtime 可以 Park");
        finish_park(&parked, reservation);
        assert!(
            parked.is_released(),
            "Park 之后进程已经转走，本 entry 不再持有任何进程"
        );
    }

    /// Park 的准入判据：忙的拒绝、空闲的放行、崩溃残骸也放行。
    ///
    /// 这三条与「拒绝时不得改动任何状态」一起，是 `park` 不会无声掐掉 in-flight 请求的
    /// 全部依据；端到端那条在 `tests/scheduler_fake_child.rs`。
    #[test]
    fn begin_park_refuses_a_busy_runtime_and_leaves_it_untouched() {
        let busy = RuntimeEntry::test_entry(RuntimeId(1), test_document("busy"));
        busy.state.lock().unwrap().reducer.set_running();
        assert_eq!(
            reserve_park(&busy).err(),
            Some(ParkRefusal::Busy),
            "正在执行请求的 Runtime 不得被 Park"
        );
        {
            let state = busy.state.lock().unwrap();
            assert!(!state.stopped, "被拒绝的 Park 不得置停止位");
            assert!(state.terminal.is_none(), "被拒绝的 Park 不得写终态");
            assert_eq!(state.reducer.phase(), LivePhase::Running);
        }

        let idle = RuntimeEntry::test_entry(RuntimeId(2), test_document("idle"));
        let parked = reserve_park(&idle).expect("空闲 Runtime 可以让出进程");
        // fixture 没有真实进程，因此拿不到 client、也就不可复用；但状态必须已封住。
        assert!(parked.client.is_none());
        assert!(!parked.reusable);
        assert_eq!(
            idle.state.lock().unwrap().terminal,
            Some(TerminalState::Stopped)
        );

        let crashed = RuntimeEntry::test_entry(RuntimeId(3), test_document("crashed"));
        {
            let mut state = crashed.state.lock().unwrap();
            state.reducer.set_running();
            state.terminal = Some(TerminalState::Failed {
                error: "boom".to_owned(),
            });
        }
        let salvage = reserve_park(&crashed).expect("崩溃残骸可以回收");
        assert!(!salvage.reusable);
        assert_eq!(
            crashed.state.lock().unwrap().terminal,
            Some(TerminalState::Failed {
                error: "boom".to_owned()
            }),
            "回收残骸不得覆盖原本的失败原因"
        );
    }

    #[test]
    fn illegal_slot_transitions_leave_the_state_untouched() {
        let mut slot = slot_fixture(SchedulerState::Running);
        // 跳过 Stopping 直接回 Parked 会漏掉进程回收与运行槽归还。
        let error = slot
            .transition(SchedulerState::Parked)
            .expect_err("Running -> Parked 必须被拒绝");
        assert_eq!(error.to_string(), "非法状态转移：Running -> Parked");
        assert_eq!(
            slot.state,
            SchedulerState::Running,
            "被拒绝的转移不得改变状态"
        );
        // 同状态自转移是幂等的 no-op，不算非法。
        slot.transition(SchedulerState::Running).expect("自转移");
    }

    /// 七态各自可达的**状态机层**证据。
    ///
    /// `Parked` / `Queued` / `Running` / `Failed` / `IdleWarm`（`SchedulerReport::warm`）
    /// 另有 `tests/scheduler_fake_child.rs` 的端到端断言；`Starting` 与 `Stopping` 按设计
    /// 是持锁窗口之外的短暂中间态，端到端观察必然是竞态的，因此在这一层逐条钉死。
    #[test]
    fn every_scheduler_state_is_reachable_through_the_transition_table() {
        let mut slot = slot_fixture(SchedulerState::Parked);
        for next in [
            SchedulerState::Queued,
            SchedulerState::Starting,
            SchedulerState::Running,
            SchedulerState::Stopping,
            SchedulerState::Parked,
        ] {
            slot.transition(next)
                .unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(slot.state, next);
        }
        // 启动失败与运行中崩溃都落到 Failed，且 Failed 可重试。
        slot.transition(SchedulerState::Starting).expect("重试启动");
        slot.transition(SchedulerState::Failed).expect("启动失败");
        slot.transition(SchedulerState::Queued)
            .expect("失败后重排队");

        // IdleWarm 是池内热进程的状态：出池即被接管（Starting）或被回收（Stopping）。
        let mut warm = slot_fixture(SchedulerState::IdleWarm);
        warm.transition(SchedulerState::Starting).expect("被接管");
        let mut reaped = slot_fixture(SchedulerState::IdleWarm);
        reaped.transition(SchedulerState::Stopping).expect("被回收");
    }

    #[test]
    fn default_scheduler_limits_match_the_project_baseline() {
        // 立项文档 § 七阶段 E 的初始配置；改这些值等于改产品行为，必须先改文档。
        let limits = RuntimeLimits::default().scheduler;
        assert_eq!(limits.user_session_slots, 2);
        assert_eq!(limits.total_runtime_slots, 3);
        assert_eq!(limits.warm_idle, 1);
        assert_eq!(limits.idle_ttl, Duration::from_secs(180));
    }

    #[test]
    fn reaper_interval_stays_inside_a_sane_band() {
        assert_eq!(
            reaper_interval(Duration::from_secs(180)),
            Duration::from_secs(5),
            "长 TTL 也要保持秒级巡检，才能及时发现崩溃的 Runtime"
        );
        assert_eq!(
            reaper_interval(Duration::from_millis(100)),
            Duration::from_millis(200),
            "极短 TTL 不得把 CPU 烧在轮询上"
        );
        assert_eq!(
            reaper_interval(Duration::from_secs(8)),
            Duration::from_secs(2)
        );
    }

    /// 「`with_test_clock` 不起 reaper 线程」这条断言必须在独立进程里做
    /// （见 `tests/reaper_thread_budget.rs`）—— 线程计数器是进程级的，lib 测试并行跑时
    /// 任何一条别的用例都会把它顶掉。这里只覆盖时钟本身完全由测试推进。
    #[test]
    fn a_test_clock_only_advances_when_the_test_says_so() {
        let clock = Arc::new(FakeClock::new());
        let injected: Arc<dyn Clock> = clock.clone();
        let manager = RuntimeManager::with_test_clock(RuntimeLimits::default(), injected);
        assert_eq!(manager.inner.clock.now(), Duration::ZERO);
        manager.tick();
        assert_eq!(manager.inner.clock.now(), Duration::ZERO, "tick 不推进时钟");
        clock.advance(Duration::from_secs(1));
        assert_eq!(manager.inner.clock.now(), Duration::from_secs(1));
        assert_eq!(manager.scheduler_report().resident_pi, 0);
    }

    #[test]
    fn a_parked_session_holds_no_process_and_no_runtime() {
        let clock = Arc::new(FakeClock::new());
        let injected: Arc<dyn Clock> = clock.clone();
        let manager = RuntimeManager::with_test_clock(RuntimeLimits::default(), injected);
        let session = manager.create_session(
            SessionDescriptor {
                binary: fake_binary(),
                cwd: std::env::temp_dir(),
                session_path: None,
                tool_preset: ToolPreset::Inherit,
                agent_dir: None,
            },
            test_document("parked"),
        );
        assert_eq!(manager.session_state(session), Some(SchedulerState::Parked));
        assert!(
            manager.session_handle(session).is_none(),
            "Parked 会话不得持有 Runtime"
        );
        assert_eq!(manager.scheduler_report().resident_pi, 0);
        // 「登记不创建线程」由 `tests/reaper_thread_budget.rs` 在独立进程里断言。
    }

    fn parked_session(manager: &RuntimeManager, label: &str) -> SessionId {
        manager.create_session(
            SessionDescriptor {
                binary: fake_binary(),
                cwd: std::env::temp_dir(),
                session_path: None,
                tool_preset: ToolPreset::Inherit,
                agent_dir: None,
            },
            test_document(label),
        )
    }

    /// 第十一轮审查 P1：占着进程的四态一律不许从描述这条路改工具预设。
    ///
    /// `Starting` / `Stopping` 里 `slot.entry` 还是空的，而 `admit` 早在进入 `Starting`
    /// 时就把描述克隆给了正在启动的那个进程。只看 `entry` 会让整个启动窗口都能改预设，
    /// 结果进程按旧预设起来、UI 显示的却是新预设——正是 R23 审查 P1-1 禁止的那种分家。
    #[test]
    fn tool_preset_edits_are_refused_in_every_state_that_holds_a_process() {
        let manager = RuntimeManager::with_test_clock(
            RuntimeLimits::default(),
            Arc::new(FakeClock::new()) as Arc<dyn Clock>,
        );
        let session = parked_session(&manager, "preset-race");
        // 直接写状态而不走 `transition`：要钉的是「七态各自允不允许改预设」，
        // 不是转移表恰好能走通哪几条路径。
        let force_state = |state: SchedulerState| {
            manager
                .inner
                .core
                .lock()
                .unwrap()
                .sessions
                .get_mut(&session)
                .expect("会话仍在")
                .state = state;
        };
        let preset_of = || {
            manager
                .session_descriptor(session)
                .map(|descriptor| descriptor.tool_preset)
        };

        for (state, preset) in [
            (SchedulerState::Parked, ToolPreset::ReadOnly),
            (SchedulerState::Queued, ToolPreset::None),
            (SchedulerState::Failed, ToolPreset::Default),
        ] {
            force_state(state);
            manager
                .set_session_tool_preset(session, preset)
                .unwrap_or_else(|error| {
                    panic!("{} 还没有进程，应当允许改预设：{error}", state.label())
                });
            assert_eq!(
                preset_of(),
                Some(preset),
                "{} 的改动必须落到描述上，下次启动才会按新预设起进程",
                state.label()
            );
        }

        let settled = preset_of();
        for state in [
            SchedulerState::Starting,
            SchedulerState::Running,
            SchedulerState::Stopping,
            SchedulerState::IdleWarm,
        ] {
            force_state(state);
            let refused = manager
                .set_session_tool_preset(session, ToolPreset::Full)
                .expect_err(state.label());
            if state == SchedulerState::Running {
                assert!(refused.contains("重启"), "{refused}");
            } else {
                assert!(refused.contains(state.label()), "{refused}");
            }
            assert_eq!(
                preset_of(),
                settled,
                "{} 被拒绝的调用不得改动描述",
                state.label()
            );
        }
    }

    /// 第十二轮审查 P2：优先级必须在**提升 tick 之前**落到队列上。
    ///
    /// `request_run` 的第一件事是 `tick()`，而 `promote_queued` 按队列里现有的优先级挑人。
    /// 「切到一个排队标签」如果只靠 `admit` 里那句 `slot.priority = priority`，
    /// 这次调用会先用旧优先级把刚空出来的槽让给别的后台会话，自己才被抬成前台。
    #[test]
    fn reprioritize_raises_a_queued_entry_before_the_next_promotion_tick() {
        let manager = RuntimeManager::with_test_clock(
            RuntimeLimits::default(),
            Arc::new(FakeClock::new()) as Arc<dyn Clock>,
        );
        let early = parked_session(&manager, "early");
        let late = parked_session(&manager, "late");
        let now = manager.inner.clock.now();
        {
            let mut core = manager.inner.core.lock().unwrap();
            for session in [early, late] {
                core.queue
                    .push(session, Priority::BACKGROUND, now)
                    .expect("入队");
                core.sessions.get_mut(&session).expect("会话仍在").state = SchedulerState::Queued;
            }
        }
        let peek = || manager.inner.core.lock().unwrap().queue.peek_next(now);
        assert_eq!(peek(), Some(early), "同优先级下先入队的先被挑中");

        manager.reprioritize(late, Priority::FOREGROUND);
        assert_eq!(peek(), Some(late), "抬到前台后，下一次提升就该轮到它");
        assert_eq!(
            manager
                .inner
                .core
                .lock()
                .unwrap()
                .sessions
                .get(&late)
                .map(|slot| slot.priority),
            Some(Priority::FOREGROUND),
            "会话侧也要记下调用方的意图"
        );

        // 沿用 `WaitQueue::push` 的「只升不降」：反复调用不得把它降回去。
        manager.reprioritize(late, Priority::BACKGROUND);
        assert_eq!(peek(), Some(late));

        // 不在队列里的会话只更新 slot.priority，不得被顺手塞进队列。
        let idle = parked_session(&manager, "idle");
        manager.reprioritize(idle, Priority::FOREGROUND);
        assert!(
            !manager.inner.core.lock().unwrap().queue.contains(idle),
            "reprioritize 不是入队入口"
        );
    }

    #[test]
    fn scheduler_notices_are_level_triggered_and_coalesce_to_one_pending_slot() {
        let manager = RuntimeManager::with_test_clock(
            RuntimeLimits::default(),
            Arc::new(FakeClock::new()) as Arc<dyn Clock>,
        );
        let mut subscription = manager.subscribe_scheduler();
        let notices = subscription.take_receiver().expect("first take");
        assert!(
            subscription.take_receiver().is_none(),
            "接收端只能被取走一次"
        );
        let before = manager.scheduler_revision();

        let first = parked_session(&manager, "watch-a");
        let second = parked_session(&manager, "watch-b");
        manager.remove_session(first);

        // 三次变化，修订号一定涨了三次；但订阅者手里最多只有一格待处理通知。
        assert_eq!(manager.scheduler_revision(), before + 3);
        assert_eq!(notices.try_recv(), Ok(SchedulerChanged));
        assert_eq!(
            notices.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "电平触发：合并后只留一格，不得堆成事件流"
        );
        // 通知只说「变了」，状态得自己查——这正是电平触发要求调用方做的事。
        assert_eq!(manager.session_state(first), None);
        assert_eq!(manager.session_state(second), Some(SchedulerState::Parked));

        // 取走之后的下一次变化必须再次唤醒，不能因为丢过帧就永远静默。
        manager.remove_session(second);
        assert_eq!(notices.try_recv(), Ok(SchedulerChanged));
    }

    #[test]
    fn dropping_a_subscription_unregisters_it_and_disconnects_the_bridge() {
        let manager = RuntimeManager::with_test_clock(
            RuntimeLimits::default(),
            Arc::new(FakeClock::new()) as Arc<dyn Clock>,
        );
        let mut live = manager.subscribe_scheduler();
        let live_rx = live.take_receiver().expect("receiver");
        let mut short_lived = manager.subscribe_scheduler();
        // 模拟 UI 的用法：接收端交给一条阻塞线程，订阅本体留在面板上。
        let short_rx = short_lived.take_receiver().expect("receiver");
        assert_eq!(manager.inner.watch.watchers.lock().unwrap().len(), 2);

        // 退订必须**当场**摘掉名册项并断开通道，不能等到下一次发布 ——
        // 桥接线程正阻塞在 `recv` 上，等下一次发布可能永远等不到。
        drop(short_lived);
        assert_eq!(manager.inner.watch.watchers.lock().unwrap().len(), 1);
        assert_eq!(
            short_rx.recv(),
            Err(std::sync::mpsc::RecvError),
            "退订后阻塞中的接收端必须立刻返回"
        );

        let session = parked_session(&manager, "watch-drop");
        assert_eq!(live_rx.try_recv(), Ok(SchedulerChanged));
        drop(live);
        assert!(
            manager.inner.watch.watchers.lock().unwrap().is_empty(),
            "最后一个订阅退订后名册应为空"
        );
        manager.remove_session(session);
    }

    #[test]
    fn a_stalled_subscriber_never_blocks_the_publisher() {
        let manager = RuntimeManager::with_test_clock(
            RuntimeLimits::default(),
            Arc::new(FakeClock::new()) as Arc<dyn Clock>,
        );
        // 订阅了却一次也不取：发布方持着调度锁，任何阻塞都会把整个调度器卡死。
        let mut stalled = manager.subscribe_scheduler();
        let _stalled_rx = stalled.take_receiver().expect("receiver");
        let sessions = (0..64)
            .map(|index| parked_session(&manager, &format!("stalled-{index}")))
            .collect::<Vec<_>>();
        for session in sessions {
            manager.remove_session(session);
        }
        assert_eq!(manager.scheduler_report().parked, 0);
    }
}
