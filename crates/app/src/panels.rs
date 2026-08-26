use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    Anchor, AnyWindowHandle, App, AppContext as _, Bounds, ClipboardEntry, Context, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, FollowMode, Image, ImageFormat, InteractiveElement as _,
    IntoElement, KeyDownEvent, ListAlignment, ListState, ParentElement as _, PathPromptOptions,
    Pixels, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, Icon, IconName,
    InteractiveElementExt as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _, Toggle, ToggleGroup, ToggleVariants as _},
    dialog::{DialogAction, DialogClose, DialogFooter},
    dock::{Panel, PanelControl, PanelEvent},
    h_flex,
    input::{InputEvent, Paste, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    popover::Popover,
    scroll::ScrollableElement as _,
    status_bar::StatusBar,
    tooltip::Tooltip,
    v_flex,
};
use pi_render::{ConversationDocument, ConversationItem, LivePhase};

use crate::{
    live_session::{
        ComposerMode, ComposerSubmission, ControlOperation, ControlOutcome, ControlRequest,
        ExtensionUiState, RequestFailureKind, RpcIntent, RuntimeEffect, RuntimeEffectKind,
        RuntimeManager, SessionControls, SessionHandle, SessionRuntimeEvent, SessionSnapshot,
        ToolPreset, official_binary,
    },
    session_sidebar::SessionSelected,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionsChanged;

/// 前台会话标签变了。
///
/// 多会话之后标签条成了第二个「切会话」入口，而文件浏览器、工作区根目录这些面板
/// 只认侧栏与新建会话事件。不广播这条，切到另一个项目的标签就会出现
/// 「B 的对话配着 A 的文件树」，工作区操作还打在 A 上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedSessionChanged {
    /// 该标签的工作目录；纯历史预览尚未确定目录时为 `None`。
    pub cwd: Option<PathBuf>,
    /// 该标签的显示标题，供工作区工具栏使用。
    pub title: String,
    /// pi 会话身份，供 tooltip 使用；fresh 会话落盘前没有。
    pub session_key: Option<String>,
}

/// 同时打开的会话标签上限。
///
/// 常驻 pi 进程数由调度器兜底（`total_runtime_slots`），但**标签**另有代价：每个标签
/// 常驻一份 `ConversationDocument`、一个 `ListState` 和一整套会话态，全部与进程无关。
/// 所以标签数必须自己有上限；到顶时明确拒绝，不静默淘汰用户还在用的标签。
const MAX_SESSION_TABS: usize = 8;

pub struct ChatPanel {
    focus_handle: FocusHandle,
    runtime_manager: RuntimeManager,
    /// 会话标签，顺序即标签条顺序。**永远至少有一个** —— 关掉最后一个标签是把它重置
    /// 成空标签，而不是删除，这样 `Deref` 永远有落点。
    sessions: Vec<SessionUiState>,
    /// 用户当前看到的标签下标。渲染只认它。
    focused: usize,
    /// 投影游标：`Deref` / `DerefMut` 的落点。
    ///
    /// 稳态下恒等于 `focused`；只有后台 pump 会用 [`ChatPanel::project`] 把它临时指到
    /// 自己那一槽，好让整套 `self.xxx` 投影逻辑原样服务于后台会话，而不必把
    /// 「写进哪一槽」当参数逐层传下去。
    cursor: usize,
    /// 标签身份的单调发号器；永不复用，避免关掉再开的标签复用同一个 GPUI 元素 id。
    next_tab_id: u64,
    composer: gpui::Entity<TextareaState>,
    drafts: pi_data::DraftStore,
    workspace_bounds: Option<Bounds<Pixels>>,
    message_pane_bounds: Option<Bounds<Pixels>>,
    extension_dialog_body_focus: Option<FocusHandle>,
    extension_dialog_footer_focus: Option<FocusHandle>,
    /// 调度器订阅本体。存在这里就是为了让它随面板一起析构 —— 析构即退订，
    /// 桥接线程随之退出。
    scheduler_subscription: Option<pi_runtime::SchedulerSubscription>,
    _composer_subscription: Subscription,
    probe: Option<LayoutProbe>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetryStatus {
    attempt: u32,
    max_attempts: u32,
    delay_ms: u64,
    error: String,
}

#[derive(Clone, Copy)]
struct ComposerInputShellStyle {
    background: gpui::Hsla,
    border: gpui::Hsla,
    border_layers: u8,
    shadow_layers: u8,
}

fn composer_input_shell_style(focused: bool, cx: &App) -> ComposerInputShellStyle {
    ComposerInputShellStyle {
        background: cx.theme().background,
        border: if focused {
            cx.theme().ring
        } else {
            cx.theme().border
        },
        border_layers: 1,
        shadow_layers: 1,
    }
}

const fn sessions_changed_for_outcome(outcome: &ControlOutcome) -> bool {
    matches!(
        outcome,
        ControlOutcome::Forked { .. }
            | ControlOutcome::Cloned { .. }
            | ControlOutcome::RebindCalibrationFailed {
                operation: ControlOperation::Fork | ControlOperation::Clone,
                ..
            }
    )
}

#[derive(Debug, Clone)]
struct ComposerAttachment {
    draft: pi_data::DraftImage,
    preview: Arc<Image>,
}

#[derive(Debug, Clone)]
enum ComposerPopup {
    Slash(Vec<pi_rpc::RpcSlashCommand>),
    At {
        query: pi_data::AtQuery,
        entries: Vec<pi_data::FileIndexEntry>,
    },
}

#[derive(Debug, Clone)]
pub enum ChatStatus {
    Empty,
    Loading { title: String },
    Ready(Arc<ConversationDocument>),
    Error { title: String, message: String },
}

pub struct SessionUiState {
    /// app 内的标签身份，单调分配、永不复用。
    tab_id: u64,
    /// Manager 分配的会话身份。
    ///
    /// **跨 Park/Resume 恒定**，因此会话态按它隔离而不是按 `RuntimeId`（BACKLOG #18）：
    /// `RuntimeId` 每次 Resume 都会换新，照它隔离等于每次唤醒都丢掉草稿和滚动位置。
    /// 纯历史预览标签还没登记成会话，为 `None`。
    session: Option<pi_runtime::SessionId>,
    /// 标签标题。
    tab_title: String,
    /// 调度器里的状态；由 [`ChatPanel::reconcile_scheduler`] 拉取，app 不自行推断。
    scheduler_state: Option<pi_runtime::SchedulerState>,
    /// 最近一次调度失败的原因（`Failed` 态才有值）。
    scheduler_failure: Option<String>,
    /// 正在后台执行的调度器操作的文案（启动 / 恢复 / 挂起）。
    ///
    /// 这些操作会碰进程，必须放后台；期间标签得有个如实的说明，也得挡住重复点击。
    scheduler_job: Option<&'static str>,
    /// 当前绑定的 Runtime。
    active: Option<SessionHandle>,
    active_generation: u64,
    active_epoch: u64,
    applied_revision: u64,
    effect_cursor: u64,
    /// 上一次已上报的运行时背压计数，用于只在新增时提示。
    backpressure: pi_runtime::BackpressureStats,
    model_names: Arc<std::collections::HashMap<String, String>>,
    popup: Option<ComposerPopup>,
    popup_index: usize,
    file_index: Option<pi_data::FileIndex>,
    extension_widgets_above_scroll: ScrollHandle,
    extension_widgets_below_scroll: ScrollHandle,
    status: ChatStatus,
    load_generation: LoadGeneration,
    composer_mode: ComposerMode,
    draft_key: Option<String>,
    attachments: Vec<ComposerAttachment>,
    slash_commands: Vec<pi_rpc::RpcSlashCommand>,
    controls: Option<SessionControls>,
    tool_preset: ToolPreset,
    control_operation: Option<ControlOperation>,
    branch_tree: Option<pi_data::SessionBranchTree>,
    branch_preview_leaf: Option<String>,
    branch_preview_document: Option<Arc<ConversationDocument>>,
    retry_status: Option<RetryStatus>,
    compacting: bool,
    composer_cwd: Option<PathBuf>,
    pending_draft_restore: bool,
    list_state: ListState,
    list_items: Vec<ListItemSnapshot>,
    tail_attached: bool,
    follow_requested: bool,
    minimap_visible: bool,
    expanded_tools: HashSet<String>,
    expanded_processes: HashSet<String>,
    rpc_success: Option<String>,
    rpc_error: Option<String>,
    host_extension_degradation: Option<String>,
    /// 背压弱提示的最短驻留截止时间。
    ///
    /// 只按「本帧有无新增计数」显隐会让一次性计数只活一帧：在 16–33ms 的合帧节奏下
    /// 既看不清，还会让它下方的 composer 每帧上下跳一行。给一个固定的可读窗口。
    backpressure_note_until: Option<std::time::Instant>,
    /// 背压降级的弱提示。
    ///
    /// 单独一格而不是复用 `host_extension_degradation`：后者承载的是「整个会话持续成立」
    /// 的降级事实（如宿主扩展未加载），而背压计数描述的是**瞬时**事件，两者生命周期
    /// 不同。混在一起会让瞬时提示永久驻留，并把高价值的启动诊断永久顶掉。
    backpressure_note: Option<String>,
    /// 当前 `rpc_error` 是否是「具体、可据以行动的失败原因」。
    ///
    /// 为真时背压提示只能追加、不能替换。它不按帧重置——因为要保护的错误既可能来自
    /// effect（`apply_snapshot` 期间），也可能来自用户操作路径（提交被拒、停止失败），
    /// 后者根本不经过 `apply_snapshot`，按帧重置会让它在下一帧被背压提示整条顶掉。
    rpc_error_protected: bool,
    extension_ui: ExtensionUiState,
    extension_dialog_open: Option<String>,
    extension_dialog_needs_close: bool,
    pending_extension_responses: Vec<pi_rpc::ExtensionUiResponse>,
    extension_response_sender: Option<Arc<dyn crate::live_session::ExtensionResponseSender>>,
    window_title: String,
    next_extension_element_id: u64,
    fresh_session: bool,
}

impl SessionUiState {
    fn new(tab_id: u64, list_state: ListState) -> Self {
        Self {
            tab_id,
            session: None,
            tab_title: "新标签".to_owned(),
            scheduler_state: None,
            scheduler_failure: None,
            scheduler_job: None,
            active: None,
            active_generation: 0,
            active_epoch: 0,
            applied_revision: 0,
            effect_cursor: 0,
            backpressure: pi_runtime::BackpressureStats::default(),
            model_names: Arc::new(std::collections::HashMap::new()),
            popup: None,
            popup_index: 0,
            file_index: None,
            extension_widgets_above_scroll: ScrollHandle::default(),
            extension_widgets_below_scroll: ScrollHandle::default(),
            status: ChatStatus::Empty,
            load_generation: LoadGeneration(0),
            composer_mode: ComposerMode::Steer,
            draft_key: None,
            attachments: Vec::new(),
            slash_commands: Vec::new(),
            controls: None,
            tool_preset: ToolPreset::Inherit,
            control_operation: None,
            branch_tree: None,
            branch_preview_leaf: None,
            branch_preview_document: None,
            retry_status: None,
            compacting: false,
            composer_cwd: None,
            pending_draft_restore: false,
            list_state,
            list_items: Vec::new(),
            tail_attached: true,
            follow_requested: false,
            minimap_visible: true,
            expanded_tools: HashSet::new(),
            expanded_processes: HashSet::new(),
            rpc_success: None,
            rpc_error: None,
            host_extension_degradation: None,
            backpressure_note: None,
            backpressure_note_until: None,
            rpc_error_protected: false,
            extension_ui: ExtensionUiState::default(),
            extension_dialog_open: None,
            extension_dialog_needs_close: false,
            pending_extension_responses: Vec::new(),
            extension_response_sender: None,
            window_title: "GPUI-Pi".to_owned(),
            next_extension_element_id: 0,
            fresh_session: false,
        }
    }
}

impl SessionUiState {
    /// 这个标签的草稿存放键。
    ///
    /// `draft_key` 记的是 **pi 会话身份**（标签去重、`ControlsLoaded` 的键迁移都认它），
    /// 还没登记会话的标签没有它；但草稿必须从标签一诞生就有地方存，否则
    /// 「在空标签上写了字 → 去开别的标签 → 切回来」这条路上字就没了。
    /// 没有会话身份时退回标签自己的身份。
    fn draft_slot_key(&self) -> String {
        self.draft_key
            .clone()
            .unwrap_or_else(|| format!("tab-{}", self.tab_id))
    }
}

impl SessionUiState {
    /// 这个标签此刻能不能再发起一次会话控制操作。
    ///
    /// 两件事都会让「现在这个 Runtime」在下一刻不再成立：`control_operation` 是上一次
    /// 控制请求还没回来，`scheduler_job` 是调度器正在起 / 停这个会话的进程。此刻发出的
    /// 控制请求要么打在一个马上要被摘掉的 Runtime 上，要么与调度器的状态转移撞车——
    /// 改工具预设撞上启动窗口更严重：描述改成了 ReadOnly，进程却已经按旧预设起来了。
    ///
    /// 各个按钮的 `disabled` 与各个 handler 的早退**共用这一个判据**，不再各写一份。
    fn control_busy(&self) -> bool {
        self.control_operation.is_some() || self.scheduler_job.is_some()
    }
}

impl SessionUiState {
    /// 这个标签当前绑定的会话文件。
    ///
    /// 以 `controls.session_file` 为准（内核回报的权威值）；还没拿到 controls 时
    /// 退回历史文档的来源路径。
    fn bound_session_file(&self) -> Option<PathBuf> {
        self.controls
            .as_ref()
            .and_then(|controls| controls.session_file.clone())
            .or_else(|| match &self.status {
                ChatStatus::Ready(document) => Some(document.source_path.clone()),
                _ => None,
            })
            .filter(|path| !path.as_os_str().is_empty())
    }
}

impl ChatPanel {
    /// 投影游标的落点。`min` 只是防御性收敛：下标漂移宁可落到最后一个标签，
    /// 也不要在渲染路径上 panic。
    fn cursor_index(&self) -> usize {
        self.cursor.min(self.sessions.len().saturating_sub(1))
    }
}

impl std::ops::Deref for ChatPanel {
    type Target = SessionUiState;

    fn deref(&self) -> &Self::Target {
        &self.sessions[self.cursor_index()]
    }
}

impl std::ops::DerefMut for ChatPanel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let index = self.cursor_index();
        &mut self.sessions[index]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListItemSnapshot {
    id: String,
    is_process: bool,
    content_identity: usize,
    collapsible: bool,
}

impl ListItemSnapshot {
    fn from_item(item: &ConversationItem) -> Self {
        match item {
            ConversationItem::Message(message) => Self {
                id: message.id.clone(),
                is_process: false,
                content_identity: Arc::as_ptr(message) as usize,
                collapsible: false,
            },
            ConversationItem::Process(group) => Self {
                id: group.id.clone(),
                is_process: true,
                content_identity: group.messages.as_ptr() as usize,
                collapsible: group.collapsible,
            },
        }
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id && self.is_process == other.is_process
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct LayoutProbe {
    pub sidebar: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    pub sidebar_prepaints: std::rc::Rc<std::cell::Cell<usize>>,
    pub workspace: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    pub files: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
}

#[cfg(not(test))]
#[derive(Clone, Copy)]
pub(crate) struct LayoutProbe;

impl LayoutProbe {
    #[cfg(test)]
    pub(crate) fn record_sidebar(&self, bounds: gpui::Bounds<gpui::Pixels>) {
        self.sidebar.set(bounds);
        self.sidebar_prepaints
            .set(self.sidebar_prepaints.get().saturating_add(1));
    }

    #[cfg(not(test))]
    pub(crate) fn record_sidebar(&self, _: gpui::Bounds<gpui::Pixels>) {}

    #[cfg(test)]
    fn record_workspace(&self, bounds: gpui::Bounds<gpui::Pixels>) {
        self.workspace.set(bounds);
    }

    #[cfg(not(test))]
    fn record_workspace(&self, _: gpui::Bounds<gpui::Pixels>) {}

    #[cfg(test)]
    pub(crate) fn record_files(&self, bounds: gpui::Bounds<gpui::Pixels>) {
        self.files.set(bounds);
    }

    #[cfg(not(test))]
    pub(crate) fn record_files(&self, _: gpui::Bounds<gpui::Pixels>) {}
}

const COMPOSER_MAX_ROWS: usize = 8;

/// 标签标题的最大显示长度。标签条要放得下多个会话，标题必须先收敛再进 `Tab`；
/// 完整标题在 tooltip 里（S-8）。
const TAB_LABEL_LIMIT: usize = 18;
/// 失败原因在横幅里的最大显示长度。
///
/// pi 回传的错误长度不受控，横幅又是单行；不先收敛就会把消息区一直往上挤。
const FAILURE_TEXT_LIMIT: usize = 120;

/// 前台标签的调度状态提示。
struct SessionStateNote {
    dot: gpui::Hsla,
    text: String,
}

/// 把调度状态映射成标签条认得的形态。
///
/// 没有登记会话的标签是 `History`，不是 `Parked` —— 前者「还没启动过」，
/// 后者「启动过又让出了进程」，对用户是两件事。
fn tab_state_of(slot: &SessionUiState) -> gpui_pi_ui::SessionTabState {
    match slot.scheduler_state {
        Some(pi_runtime::SchedulerState::Running) => gpui_pi_ui::SessionTabState::Running,
        Some(pi_runtime::SchedulerState::Queued) => gpui_pi_ui::SessionTabState::Queued,
        Some(pi_runtime::SchedulerState::Starting) => gpui_pi_ui::SessionTabState::Starting,
        Some(pi_runtime::SchedulerState::Stopping) => gpui_pi_ui::SessionTabState::Stopping,
        Some(pi_runtime::SchedulerState::Failed) => gpui_pi_ui::SessionTabState::Failed,
        Some(pi_runtime::SchedulerState::Parked) => gpui_pi_ui::SessionTabState::Parked,
        // `IdleWarm` 描述的是池内热进程，不描述 Session；真出现在这里说明状态读错了，
        // 按「没有进程」呈现，不编造一个运行中。
        Some(pi_runtime::SchedulerState::IdleWarm) | None => {
            if slot.session.is_some() {
                gpui_pi_ui::SessionTabState::Parked
            } else {
                gpui_pi_ui::SessionTabState::History
            }
        }
    }
}

/// 「按标签重载」类后台任务的代次：历史渲染与文件索引。
///
/// 单独一个类型，是因为 `SessionUiState` 上还有一个 `active_generation`（Runtime 代次），
/// 两者都是 `u64`，混用编译得过、行为却是回填永远对不上——R24 第二轮独立审查抓到的
/// 正是这个：fresh 会话把 `active_generation` 传给了按 `load_generation` 校验的索引回填，
/// 于是新标签上永远是 1 对 0，`@` 补全在 fresh 会话里从来没工作过。
/// 让它们类型不同，这一类错误就编译不过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoadGeneration(u64);

impl LoadGeneration {
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// 主操作入口的文案。
///
/// 三态各说各的：没登记过是「启动」，登记过没进程是「恢复」，已经在排队则**什么都不用做**
/// ——轮到它会自动启动，给一个可点的「恢复运行」只会让用户以为按钮没生效。
const fn start_action_copy(queued: bool, registered: bool) -> (&'static str, &'static str) {
    match (queued, registered) {
        (true, _) => ("排队中…", "已在公平队列里等运行槽，轮到它会自动启动"),
        (false, true) => ("恢复运行", "重新为该会话申请一个 pi 运行槽"),
        (false, false) => ("启动活会话", "为这份历史启动官方 pi RPC 活会话"),
    }
}

/// 两个路径是不是同一份会话文件。
///
/// Windows 上大小写不敏感，还可能差一个 `\\?\` 前缀，直接比 `PathBuf` 会漏判——
/// 漏判的后果是两个 pi 进程绑上同一份 JSONL。能规范化就以规范化结果为准；
/// 文件已经不在了就退回原样比较。
fn same_session_file(left: &Path, right: &Path) -> bool {
    let normalize =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalize(left) == normalize(right)
}

/// fresh 会话的标签标题。
fn fresh_session_title(cwd: &Path) -> String {
    match cwd.file_name().and_then(std::ffi::OsStr::to_str) {
        Some(name) if !name.is_empty() => format!("新会话 · {name}"),
        _ => "新会话".to_owned(),
    }
}

/// 按**字符**截断并加省略号。
///
/// 不能按字节切：中文标题一刀下去就是非法 UTF-8，直接 panic。
fn truncate_label(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// 造一条新的消息列表状态，并把「是否跟随尾部」的回写钉在**这个标签**上。
///
/// 回写必须按 `tab_id` 找槽，不能走 `Deref`：滚动回调是 `cx.defer` 之后才执行的，
/// 那一刻投影游标可能正指着别的标签，走 `Deref` 会把 A 的滚动状态写进 B。
fn new_list_state(tab_id: u64, panel: gpui::WeakEntity<ChatPanel>) -> ListState {
    // 首次静态会话允许一次全量测量，确保长列表首次出现时 scrollbar 即为精确高度。
    let list_state = ListState::new(0, ListAlignment::Top, px(1200.)).measure_all();
    let scroll_state = list_state.clone();
    list_state.set_scroll_handler(move |event, _, cx| {
        let attached = event.is_following_tail;
        let scroll_state = scroll_state.clone();
        let panel = panel.clone();
        // ListState 在回调时持有可变借用；延后读取/更新，避免 RefCell 重入。
        cx.defer(move |cx| {
            let attached = attached || scroll_state.is_scrolled_to_end().unwrap_or(true);
            let _ = panel.update(cx, |panel, cx| {
                let Some(slot) = panel.sessions.iter_mut().find(|slot| slot.tab_id == tab_id)
                else {
                    return;
                };
                if slot.tail_attached != attached {
                    slot.tail_attached = attached;
                    cx.notify();
                }
            });
        });
    });
    list_state
}

impl ChatPanel {
    pub fn new(
        runtime_manager: RuntimeManager,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, COMPOSER_MAX_ROWS)
                .submit_on_enter(true)
                .placeholder("输入消息；Enter 发送，Shift+Enter 换行")
        });
        let subscription =
            cx.subscribe_in(
                &composer,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        if !this.accept_popup(input, window, cx) {
                            this.submit_composer(input, window, cx);
                        }
                    }
                    InputEvent::Change => this.composer_changed(input, cx),
                    _ => {}
                },
            );
        let first_tab = SessionUiState::new(0, new_list_state(0, cx.weak_entity()));
        Self {
            focus_handle: cx.focus_handle(),
            runtime_manager,
            sessions: vec![first_tab],
            focused: 0,
            cursor: 0,
            next_tab_id: 1,
            composer,
            drafts: pi_data::DraftStore::default(),
            workspace_bounds: None,
            message_pane_bounds: None,
            extension_dialog_body_focus: None,
            extension_dialog_footer_focus: None,
            scheduler_subscription: None,
            _composer_subscription: subscription,
            probe: None,
        }
    }

    /// 分配一个新标签的身份。
    fn allocate_tab_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id = self.next_tab_id.wrapping_add(1);
        id
    }

    /// 在指定标签的上下文中执行一段投影逻辑。
    ///
    /// 整套会话态投影（`apply_snapshot` / `apply_runtime_effect` / `sync_list_document`……）
    /// 都写成 `self.xxx`，靠 `Deref` 落到「当前标签」。后台会话要复用同一套逻辑，就得
    /// 让「当前标签」临时变成它自己那一槽 —— 否则只能把目标下标当参数逐层传下去，
    /// 上千行投影代码要改一遍，还留下「某个分支忘了带下标」的长期隐患。
    fn project<R>(&mut self, index: usize, body: impl FnOnce(&mut Self) -> R) -> R {
        let previous = std::mem::replace(&mut self.cursor, index);
        let result = body(self);
        self.cursor = previous;
        result
    }

    /// 当前投影的是不是用户正看着的那个标签。
    ///
    /// 后台事件泵会把游标临时指到自己那一槽，只有二者相同时才轮得到动全局资源
    /// （窗口标题、工作区面板）。
    fn projecting_focused_tab(&self) -> bool {
        self.cursor == self.focused
    }

    fn slot_index_for_tab(&self, tab_id: u64) -> Option<usize> {
        self.sessions.iter().position(|slot| slot.tab_id == tab_id)
    }

    /// 把一段**延后执行**的投影逻辑钉回发起它的那个标签。
    ///
    /// 异步回调（文件读取、原生选择器、Extension UI 超时、响应写回）回来时，前台可能
    /// 已经换了标签；走 `Deref` 就会把 A 的结果写进 B。所有跨 await 点的续体都必须在
    /// 发起时捕获 `tab_id`，回来再用它定位。标签已经关掉就整段跳过——它的会话态
    /// 已经不存在，硬找一个替身写进去只会造出更难查的串台。
    fn project_tab<R>(&mut self, tab_id: u64, body: impl FnOnce(&mut Self) -> R) -> Option<R> {
        let index = self.slot_index_for_tab(tab_id)?;
        Some(self.project(index, body))
    }

    /// 按 pi 会话身份找标签。
    ///
    /// `draft_key` 就是这份身份：历史选择时是侧栏给的会话 id，fresh 会话落盘后由
    /// `ControlsLoaded` 迁移成真实 session id。
    fn slot_index_for_key(&self, key: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|slot| slot.draft_key.as_deref() == Some(key))
    }

    /// 这个标签还是干净的吗？干净的标签可以直接复用，不必再开一个。
    ///
    /// 「干净」的判据是**用户往里放过东西没有**：会话、历史、附件、草稿文字，任何一样
    /// 都算用过。放过东西却被当成空白复用，就是把用户的输入静默丢掉或串进新会话。
    ///
    /// 草稿走 `draft_slot_key`，因此调用前必须先 `save_current_draft` ——
    /// composer 是全窗口一个，内容只有存下来之后才落在这个标签自己身上。
    fn is_focused_tab_pristine(&self) -> bool {
        let slot = &self.sessions[self.focused];
        slot.session.is_none()
            && slot.draft_key.is_none()
            && slot.attachments.is_empty()
            && matches!(slot.status, ChatStatus::Empty)
            && self.drafts.get(&slot.draft_slot_key()).text.is_empty()
    }

    /// 开一个新标签，返回它的下标。
    ///
    /// 当前标签还没被用过时原地复用它 —— 第一次点开会话不该先长出一条只有一个标签的
    /// 标签条。到达 [`MAX_SESSION_TABS`] 时明确失败，不淘汰用户还在用的标签。
    fn open_tab(
        &mut self,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        // 先存草稿再判断：判据要看「用户往这个标签里放过东西没有」，而 composer 的
        // 内容只有存下来之后才在标签自己身上。
        self.save_current_draft(cx);
        if self.is_focused_tab_pristine() {
            let index = self.focused;
            let tab_id = self.sessions[index].tab_id;
            // **原地重建**，而不是只改个标题。
            //
            // 「pristine」只保证用户没往这个标签里放过东西，不保证槽里没有残留的
            // 瞬时状态——比如往空标签里拖了一张非法图片：附件没加上，`rpc_error`
            // 却留下了。只改标题就会把那条不相干的红色横幅带进新开的会话。
            // 逐个字段去清就是在追着枚举，以后新增一个瞬时字段就漏一个；
            // 直接换一份全新的槽，新增字段自动被覆盖。
            let mut slot = SessionUiState::new(tab_id, new_list_state(tab_id, cx.weak_entity()));
            slot.tab_title = title;
            // 这三项是**用户偏好**不是瞬时状态：在空标签上先挑好工具预设 / 收起
            // minimap 再开会话是正常用法，重建不该把它们抹掉。
            slot.minimap_visible = self.sessions[index].minimap_visible;
            slot.composer_mode = self.sessions[index].composer_mode;
            slot.tool_preset = self.sessions[index].tool_preset;
            self.sessions[index] = slot;
            return Ok(index);
        }
        if self.sessions.len() >= MAX_SESSION_TABS {
            return Err(format!(
                "最多同时打开 {MAX_SESSION_TABS} 个会话标签；请先关闭一个再试"
            ));
        }
        self.suspend_foreground_dialog(window, cx);
        let tab_id = self.allocate_tab_id();
        let mut slot = SessionUiState::new(tab_id, new_list_state(tab_id, cx.weak_entity()));
        slot.tab_title = title;
        self.sessions.push(slot);
        let index = self.sessions.len() - 1;
        self.focused = index;
        self.cursor = index;
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        // 新标签的 `window_title` 是初值 "GPUI-Pi"，而窗口上挂的可能是上一个会话
        // 由 Extension UI 设的标题。`process_extension_ui` 只在「与本标签记录值不同」
        // 时才写窗口，两者恰好相等就永远不会纠正——必须在这里无条件写一次。
        self.apply_window_title(window);
        Ok(index)
    }

    /// 切到另一个标签。
    ///
    /// 只动 UI 绑定：**不启动也不停止任何进程**。后台会话继续跑，这正是 R24 的核心承诺。
    pub(crate) fn focus_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.sessions.len() || index == self.focused {
            return;
        }
        self.save_current_draft(cx);
        self.suspend_foreground_dialog(window, cx);
        self.focused = index;
        self.cursor = index;
        self.prepare_draft_restore();
        self.workspace_bounds = None;
        self.message_pane_bounds = None;
        // 排队中的标签被切到前台就该排在前面；`WaitQueue` 只升不降，不会误伤别人。
        // 走后台：`request_run` 抢到槽就会就地冷启动一个进程。
        if self.scheduler_state == Some(pi_runtime::SchedulerState::Queued) {
            self.request_run_in_background(index, "启动中…", window, cx);
        }
        self.sync_scheduler_states();
        self.apply_window_title(window);
        self.process_extension_ui(window, cx);
        self.emit_focused_session(cx);
        cx.notify();
    }

    /// 广播当前前台标签的身份，让工作区级面板跟着切过去。
    fn emit_focused_session(&mut self, cx: &mut Context<Self>) {
        cx.emit(FocusedSessionChanged {
            cwd: self.composer_cwd.clone(),
            title: self.tab_title.clone(),
            session_key: self.draft_key.clone(),
        });
    }

    /// 把窗口标题切到前台标签自己那一份。
    ///
    /// `process_extension_ui` 只在「标题与本标签记录的值不同」时才写窗口；换标签时这两个
    /// 值可能恰好相等，而窗口上挂着的还是上一个标签的标题。换标签必须无条件写一次。
    fn apply_window_title(&mut self, window: &mut Window) {
        let title = self.extension_ui.title().unwrap_or("GPUI-Pi").to_owned();
        self.window_title = title.clone();
        window.set_window_title(&title);
    }

    /// 关闭一个标签：注销会话（进程随之回收），标签从条上摘掉。
    pub(crate) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.sessions.len() {
            return;
        }
        if index == self.focused {
            self.save_current_draft(cx);
            self.suspend_foreground_dialog(window, cx);
        }
        // 关标签前先把这个标签欠 pi 的 Extension UI 响应结清；进程一旦回收就再也送不出去。
        // 只有关的是前台标签时才动窗口级状态——关后台标签不该影响前台的对话框。
        if index == self.focused {
            self.project(index, |panel| {
                panel.reset_foreground_extension_ui(window, cx)
            });
        } else {
            self.project(index, |panel| panel.reset_extension_ui_slot(cx));
        }
        // 匿名标签的草稿存在 `tab-{id}` 下，标签一走这个键就再也取不到了。
        // 不清掉就是一条随开关次数增长的泄漏——带图片的草稿可能有几十 MB。
        // 有 `draft_key` 的标签不动：它的草稿按 pi 会话身份存，重新打开还要用。
        if self.sessions[index].draft_key.is_none() {
            let orphan = self.sessions[index].draft_slot_key();
            self.drafts.clear(&orphan);
        }
        if let Some(session) = self.sessions[index].session.take() {
            // 标签立刻从条上摘掉，进程回收放后台：`remove_session` 要等优雅停机，
            // 随后的 `tick()` 还可能就地拉起一个排队会话，两者都不能占着 UI 线程。
            // `tick()` 是必须的——不推这一下，排队会话要等 reaper 轮询
            //（默认 TTL 下最长 45s）才补位。
            self.spawn_scheduler_job(
                window,
                cx,
                move |manager| {
                    manager.remove_session(session);
                    manager.tick();
                },
                |_, (), _, _| {},
            );
        }
        if self.sessions.len() == 1 {
            // 最后一个标签不删除而是重置：`Deref` 必须永远有落点。
            let tab_id = self.allocate_tab_id();
            self.sessions[0] =
                SessionUiState::new(tab_id, new_list_state(tab_id, cx.weak_entity()));
            self.focused = 0;
        } else {
            self.sessions.remove(index);
            // 关掉当前标签或它左边的标签都会让下标左移一位。
            if self.focused > index || self.focused == self.sessions.len() {
                self.focused = self.focused.saturating_sub(1);
            }
        }
        self.cursor = self.focused;
        self.prepare_draft_restore();
        self.apply_window_title(window);
        self.emit_focused_session(cx);
        self.reconcile_scheduler(window, cx);
    }

    /// 挂起当前标签的会话：让出 pi 进程，会话保留。
    ///
    /// 整个 `park` 都在后台线程做。它会等作业排空、拆 Actor、可能关进程，收尾时还会
    /// `tick()` 一次把排队会话就地提升上来——在 GPUI 主线程上做这些就是整窗口卡死。
    fn park_active_session(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.park_focused_session(window, cx);
    }

    fn park_focused_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.focused;
        let Some(session) = self.sessions[index].session else {
            return;
        };
        if self.sessions[index].control_busy() {
            return;
        }
        let tab_id = self.sessions[index].tab_id;
        self.sessions[index].scheduler_job = Some("挂起中…");
        cx.notify();
        self.spawn_scheduler_job(
            window,
            cx,
            move |manager| manager.park(session),
            move |panel, result, _, _| {
                panel.project_tab(tab_id, |panel| {
                    panel.scheduler_job = None;
                    match result {
                        // 成功不再另出一条绿条：常驻的状态说明已经写着「会话已挂起」，
                        // 两条并排说同一件事只是把消息区又挤掉一行。
                        Ok(()) => {
                            panel.rpc_success = None;
                            panel.clear_rpc_error();
                        }
                        Err(error) => {
                            panel.rpc_success = None;
                            panel.rpc_error_protected = true;
                            panel.rpc_error = Some(format!("挂起失败：{error}"));
                        }
                    }
                });
            },
        );
    }

    /// 一个标签在标签条上的形态。
    fn tab_items(&self) -> Vec<gpui_pi_ui::SessionTabItem> {
        self.sessions
            .iter()
            .map(|slot| {
                let state = tab_state_of(slot);
                gpui_pi_ui::SessionTabItem::new(
                    format!("tab-{}", slot.tab_id),
                    truncate_label(&slot.tab_title, TAB_LABEL_LIMIT),
                    format!("{} · {}", slot.tab_title, state.label()),
                    state,
                )
            })
            .collect()
    }

    /// 前台标签需要额外解释的调度状态。
    ///
    /// `Running` 不出横幅：正常运行是默认预期，为它常驻一行提示只会挤压消息区。
    /// `Starting` / `Stopping` 也不出——它们是放开调度锁前的短暂中间态，闪一下反而是噪声。
    fn session_state_note(&self, cx: &App) -> Option<SessionStateNote> {
        let slot = &self.sessions[self.focused];
        // 后台正在办的事优先说：这期间调度状态还停在旧值（比如刚点了「恢复运行」，
        // 会话仍是 `Parked`），照旧值出提示只会让用户以为按钮没生效。
        if let Some(job) = slot.scheduler_job {
            return Some(SessionStateNote {
                dot: cx.theme().warning,
                text: job.to_owned(),
            });
        }
        let state = slot.scheduler_state?;
        let text = match state {
            pi_runtime::SchedulerState::Queued => format!(
                "排队中：同时运行的会话已达上限 {}，轮到它就会自动启动",
                self.runtime_manager.scheduler_limits().user_session_slots
            ),
            pi_runtime::SchedulerState::Parked => {
                "会话已挂起：pi 进程已让出，点「恢复运行」继续".to_owned()
            }
            pi_runtime::SchedulerState::Failed => format!(
                "会话已失败：{}",
                truncate_label(
                    slot.scheduler_failure.as_deref().unwrap_or("原因未知"),
                    FAILURE_TEXT_LIMIT,
                )
            ),
            _ => return None,
        };
        Some(SessionStateNote {
            dot: tab_state_of(slot).dot(cx),
            text,
        })
    }

    /// 切走前台标签时把它的 Extension UI 对话框收起来。
    ///
    /// 收起不等于取消：请求仍留在该标签自己的队列里，切回来时
    /// `maybe_open_extension_dialog` 会重新打开它。窗口级对话框是全局资源，
    /// 不收起来它就会浮在另一个会话的界面上。
    fn suspend_foreground_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.extension_dialog_open.take().is_some() {
            self.request_extension_dialog_close(window, cx);
        } else {
            self.clear_extension_dialog_focus();
        }
    }

    /// 把调度器里的状态同步到各标签。状态由调度器给出，app 不自行推断。
    fn sync_scheduler_states(&mut self) {
        for slot in &mut self.sessions {
            let Some(session) = slot.session else {
                slot.scheduler_state = None;
                slot.scheduler_failure = None;
                continue;
            };
            slot.scheduler_state = self.runtime_manager.session_state(session);
            slot.scheduler_failure = self.runtime_manager.session_failure(session);
        }
    }

    /// 按调度器的当前状态重新对齐所有标签的 Runtime 绑定。
    ///
    /// 队列提升、崩溃回收、Park 兜底都发生在调度器内部，且提升后拿到的是一个
    /// **新的 `RuntimeId`**；不在这里重新取句柄，标签会永远停在 `Queued`。
    fn reconcile_scheduler(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_scheduler_states();
        for index in 0..self.sessions.len() {
            let Some(session) = self.sessions[index].session else {
                continue;
            };
            match self.sessions[index].scheduler_state {
                Some(pi_runtime::SchedulerState::Running) => {
                    let Some(handle) = self.runtime_manager.session_handle(session) else {
                        continue;
                    };
                    let bound = self.sessions[index]
                        .active
                        .as_ref()
                        .map(SessionHandle::runtime_id);
                    if bound != Some(handle.runtime_id()) {
                        self.attach_runtime(index, handle, window, cx);
                    }
                }
                // 会话在别处被注销了（例如运行时兜底清理）：标签退回「只有历史」的形态。
                None => self.project(index, |panel| {
                    panel.session = None;
                    panel.active = None;
                    panel.extension_response_sender = None;
                }),
                // 其余状态都没有进程可绑：举着一个死句柄只会让 UI 以为还能提交。
                _ => self.project(index, |panel| {
                    panel.active = None;
                    panel.extension_response_sender = None;
                }),
            }
        }
        cx.notify();
    }

    /// 起一条调度器通知桥：把 `pi-runtime` 的阻塞通道接到 GPUI 的异步上下文。
    ///
    /// 全面板只有这一条，与会话数无关，且**只在真的登记了会话之后才起**——
    /// 从没启动过活会话的面板不需要监听调度器，也就不该为此常驻一条线程。
    /// 订阅本体存在面板上，面板析构即退订，桥接线程随即从 `recv` 上返回退出，
    /// 关一次窗口不留线程。
    fn ensure_scheduler_bridge(&mut self, window_handle: AnyWindowHandle, cx: &mut Context<Self>) {
        if self.scheduler_subscription.is_some() {
            return;
        }
        let mut subscription = self.runtime_manager.subscribe_scheduler();
        let Some(receiver) = subscription.take_receiver() else {
            return;
        };
        self.scheduler_subscription = Some(subscription);
        // 桥接侧也必须有界且合并，否则 `pi-runtime` 那头「容量 1、满帧即丢」的保证
        // 到这里就作废了：GPUI 执行器一卡住，这条线程会把上游一条条取空、原样堆进
        // 一条无界队列。合并规则与上游一致——通知是电平触发的，堆多少条都是同一件事。
        let (mut tx, mut rx) = mpsc::channel(0);
        std::thread::Builder::new()
            .name("pi-runtime-scheduler-bridge".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    if let Err(error) = tx.try_send(())
                        && error.is_disconnected()
                    {
                        break;
                    }
                }
            })
            .expect("failed to spawn scheduler bridge");
        cx.spawn(async move |panel, cx| {
            while rx.next().await.is_some() {
                let alive = window_handle
                    .update(cx, |_, window, cx| {
                        panel
                            .update(cx, |panel, cx| panel.reconcile_scheduler(window, cx))
                            .is_ok()
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
            }
        })
        .detach();
    }

    #[cfg(test)]
    pub(crate) fn with_probe(mut self, probe: LayoutProbe) -> Self {
        self.probe = Some(probe);
        self
    }

    #[cfg(test)]
    pub(crate) fn set_extension_response_sender_for_test(
        &mut self,
        sender: std::sync::Arc<dyn crate::live_session::ExtensionResponseSender>,
    ) {
        self.extension_response_sender = Some(sender);
    }

    /// 造两个各自绑定不同工作目录的标签，供工作区级同步的测试使用。
    #[cfg(test)]
    pub(crate) fn open_two_tabs_for_test(
        &mut self,
        first: PathBuf,
        second: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.draft_key = Some("tab-one".to_owned());
        self.composer_cwd = Some(first);
        self.open_tab("二号".to_owned(), window, cx)
            .expect("第二个标签");
        self.draft_key = Some("tab-two".to_owned());
        self.composer_cwd = Some(second);
    }

    #[cfg(test)]
    pub(crate) fn set_tab_titles_for_test(&mut self, first: &str, second: &str) {
        self.sessions[0].tab_title = first.to_owned();
        self.sessions[1].tab_title = second.to_owned();
    }

    #[cfg(test)]
    pub(crate) fn composer_value_for_test(&self, cx: &App) -> String {
        self.composer.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn apply_extension_request_for_test(
        &mut self,
        id: impl Into<String>,
        request: pi_rpc::ExtensionUiRequest,
    ) {
        let id = id.into();
        let duplicate = self.extension_ui.is_dialog_pending(&id);
        if let Some(response) = self.extension_ui.apply(id.clone(), request) {
            self.pending_extension_responses.push(response);
        } else if duplicate {
            self.rpc_error = Some(format!("重复 Extension UI 请求 {id} 已忽略"));
        }
    }

    fn current_draft(&self, cx: &App) -> pi_data::ComposerDraft {
        pi_data::ComposerDraft {
            text: self.composer.read(cx).value().to_string(),
            images: self
                .attachments
                .iter()
                .map(|attachment| attachment.draft.clone())
                .collect(),
        }
    }

    fn save_current_draft(&mut self, cx: &App) {
        let key = self.draft_slot_key();
        self.drafts.set(key, self.current_draft(cx));
    }

    fn prepare_draft_restore(&mut self) {
        let draft = self.drafts.get(&self.draft_slot_key());
        self.pending_draft_restore = true;
        self.attachments = draft
            .images
            .into_iter()
            .filter_map(attachment_from_draft)
            .collect();
    }

    /// 为一个标签建立 `@` 补全用的文件索引。
    ///
    /// 结果按 `tab_id` 回填：索引是后台任务，回来时用户可能已经切走，
    /// 走 `Deref` 会把 A 的项目索引装进 B 的补全面板。
    fn start_file_index(
        &self,
        tab_id: u64,
        generation: LoadGeneration,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |panel, cx| {
            let result = executor
                .spawn(async move { pi_data::build_file_index(&cwd) })
                .await;
            let _ = panel.update(cx, |panel, cx| {
                let Some(index) = panel.slot_index_for_tab(tab_id) else {
                    return;
                };
                let focused = index == panel.focused;
                panel.project(index, |panel| {
                    if generation != panel.load_generation {
                        return;
                    }
                    panel.file_index = Some(result);
                    // 只有前台标签的补全面板由输入框驱动：composer 全窗口只有一个，
                    // 拿它的内容去重算后台标签的补全等于用别人的输入。
                    if focused {
                        panel.refresh_popup(cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn begin_active_generation(&mut self) -> u64 {
        self.active_generation = self.active_generation.wrapping_add(1);
        self.host_extension_degradation = None;
        self.backpressure_note = None;
        self.backpressure_note_until = None;
        self.active_generation
    }

    fn clear_rpc_error(&mut self) {
        self.rpc_error = None;
        self.rpc_error_protected = false;
    }

    /// 这个标签是不是正在交出进程？
    ///
    /// Park / Stop 会把正在执行的元数据请求连同 client 一起抽走，那几条 `get_state` /
    /// `get_commands` 必然超时（实测「加载会话控制失败：request req_5 timed out」）。
    /// 用户刚点了「挂起」，进程没了正是他要的结果，不是需要他处理的故障——
    /// 报出来只会让一次正常操作看起来失败了，而且横幅一进一出还会顶得 composer 跳行。
    fn is_tearing_down(&self) -> bool {
        self.scheduler_job.is_some() || self.active.is_none()
    }

    fn set_host_extension_degradation(&mut self, diagnostic: Option<&str>) {
        self.host_extension_degradation = diagnostic.map(str::to_owned);
    }

    /// 侧栏选中一个历史会话。
    ///
    /// 已经开着的同一个会话直接切过去 —— 它可能正跑着，重新载入一次历史等于把用户的
    /// 活会话换成一份只读快照。
    pub fn load_selection(
        &mut self,
        selection: SessionSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if let Some(index) = self.slot_index_for_key(&selection.id) {
            // 侧栏那边可能刚把这个会话改过名，标题以本次选择为准。
            self.sessions[index].tab_title = selection.title.clone();
            if index == self.focused {
                // `focus_tab` 对「已经是前台」会直接返回，改名后的身份就发不出去了。
                self.emit_focused_session(cx);
                cx.notify();
            } else {
                self.focus_tab(index, window, cx);
            }
            return Ok(());
        }
        // 失败必须交回调用方：工作区会跟着这次选择去搬文件树和工具栏标题，
        // 这里只在自己身上留一条错误的话，界面就会「聊天还在旧会话、工作区已经
        // 指向被拒绝的那个」。
        let index = self.open_tab(selection.title.clone(), window, cx)?;
        self.load_history_into(index, selection, cx);
        self.emit_focused_session(cx);
        cx.notify();
        Ok(())
    }

    /// 把一份历史会话装进指定标签。调用方保证该标签已经是干净的。
    fn load_history_into(
        &mut self,
        index: usize,
        selection: SessionSelected,
        cx: &mut Context<Self>,
    ) {
        let (tab_id, generation) = self.project(index, |panel| {
            panel.load_generation = panel.load_generation.next();
            panel.begin_active_generation();
            panel.tab_title = selection.title.clone();
            panel.status = ChatStatus::Loading {
                title: selection.title.clone(),
            };
            panel.draft_key = Some(selection.id.clone());
            panel.composer_cwd = Some(selection.cwd.clone());
            panel.prepare_draft_restore();
            (panel.tab_id, panel.load_generation)
        });
        self.start_file_index(tab_id, generation, selection.cwd.clone(), cx);
        let executor = cx.background_executor().clone();
        cx.spawn(async move |panel, cx| {
            let path = selection.path;
            let title = selection.title;
            let result = executor
                .spawn(async move {
                    pi_render::render_path(&path)
                        .map(Arc::new)
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = panel.update(cx, |panel, cx| {
                if panel.finish_load(tab_id, generation, title, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 侧栏「新建会话」：开一个新标签并登记一个 fresh 会话。
    ///
    /// 与 R23 之前最大的差别：**不再停掉已有的活会话**。抢不到运行槽时新会话进入
    /// `Queued`，标签照常建立，等调度器补位。
    pub fn start_new_session(
        &mut self,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        // 标题带上项目目录：同一个窗口里可能同时开着好几个 fresh 会话，
        // 全叫「新会话」的标签条等于没有标签。
        let index = self.open_tab(fresh_session_title(&cwd), window, cx)?;
        let tab_id = self.sessions[index].tab_id;
        let load_generation = self.project(index, |panel| {
            let generation = panel.begin_active_generation();
            // 文件索引按 `load_generation` 校验回填，这里必须一起推进。
            panel.load_generation = panel.load_generation.next();
            let document = ConversationDocument {
                session_id: format!("fresh-{tab_id}-{generation}"),
                source_path: PathBuf::new(),
                cwd: cwd.clone(),
                messages: Arc::from([]),
                items: Arc::from([]),
                minimap: Arc::from([]),
                diagnostics: Arc::from([]),
            };
            panel.status = ChatStatus::Ready(Arc::new(document.clone()));
            panel.sync_list_document(&document, true);
            panel.list_state.reset(0);
            panel.list_items.clear();
            // **不给 fresh 会话编一个 `draft_key`**：那个字段是对外的 pi 会话身份
            // （去重认它、工作区 tooltip 也显示它），编出来的值会以「真实身份」的
            // 名义漏到界面上。草稿自有 `draft_slot_key()` 兜底，等 `ControlsLoaded`
            // 拿到真的 session id 再迁移过去。
            panel.draft_key = None;
            let draft_slot = panel.draft_slot_key();
            panel.drafts.clear(&draft_slot);
            panel.composer_cwd = Some(cwd.clone());
            panel.fresh_session = true;
            panel.load_generation
        });
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        let document = match &self.sessions[index].status {
            ChatStatus::Ready(document) => (**document).clone(),
            // 上面刚写进去的就是 Ready；走到这里说明代码被改坏了，不要静默继续。
            _ => return Err("新会话初始文档缺失".to_owned()),
        };
        let descriptor = pi_runtime::SessionDescriptor {
            binary: official_binary(),
            cwd: cwd.clone(),
            session_path: None,
            tool_preset: ToolPreset::Inherit,
            agent_dir: pi_data::agent_dir(),
        };
        self.start_file_index(tab_id, load_generation, cwd, cx);
        self.bind_session(index, descriptor, document, window, cx);
        self.emit_focused_session(cx);
        cx.notify();
        Ok(())
    }

    /// 给一个标签登记会话并请求运行。
    ///
    /// 抢不到运行槽不是错误：会话进入 `Queued`，标签保持可见，等调度器通知再接线。
    /// 只有登记本身失败（例如队列已满）才回滚注销，绝不留下一个没人认领的会话。
    fn bind_session(
        &mut self,
        index: usize,
        descriptor: pi_runtime::SessionDescriptor,
        document: ConversationDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_scheduler_bridge(window.window_handle(), cx);
        // 登记是纯内存操作（R23 验收：登记 20 个会话零进程），同步做没问题；
        // 真正会拉起进程的 `request_run` 必须进后台。
        let session = self.runtime_manager.create_session(descriptor, document);
        self.sessions[index].session = Some(session);
        self.sessions[index].scheduler_failure = None;
        self.sync_scheduler_states();
        self.request_run_in_background(index, "启动中…", window, cx);
    }

    /// 标签的调度优先级：只有用户正在看的那个才是前台。
    fn priority_for(&self, index: usize) -> pi_runtime::Priority {
        if index == self.focused {
            pi_runtime::Priority::FOREGROUND
        } else {
            pi_runtime::Priority::BACKGROUND
        }
    }

    /// 在后台线程执行一段**会碰进程**的调度器操作，完成后回到 UI 线程收尾并 reconcile。
    ///
    /// `RuntimeManager` 的 `park` / `request_run` / `remove_session` / `tick` 都可能同步地
    /// 关掉一个 pi 进程、做一次 `switch_session` 往返，甚至冷启一个新进程 ——
    /// `park` 收尾时的那次 `tick()` 就会把排队会话**就地**提升上来
    /// （`pi-runtime` 的 `park_finishes_a_queued_handoff_on_the_calling_thread` 钉死了这条）。
    ///
    /// 这些工作一旦落在 GPUI 主线程上，就是整窗口卡死一次进程交接的时间；多会话之后更糟，
    /// 因为它同时冻住了**其他会话**的流式渲染。R24 视觉验收里「2 活跃 + 1 排队时点挂起
    /// 程序崩溃退出、单活跃时正常」正是这条：单会话时 `tick()` 无事可做，所以看不出来。
    fn spawn_scheduler_job<R: Send + 'static>(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce(RuntimeManager) -> R + Send + 'static,
        settle: impl FnOnce(&mut Self, R, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let manager = self.runtime_manager.clone();
        cx.spawn_in(window, async move |panel, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { job(manager) })
                .await;
            let _ = cx.update(|window, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    settle(panel, outcome, window, cx);
                    // 绑定统一交给 reconcile：它按调度器的**当前**状态取句柄，
                    // 不依赖这次调用恰好返回了什么。
                    panel.reconcile_scheduler(window, cx);
                });
            });
        })
        .detach();
    }

    /// 后台申请运行槽。
    ///
    /// 拿到句柄这件事交给 `reconcile_scheduler`，这里只负责把 busy 标记立起来再放下 ——
    /// 排队会话后来被调度器提升时走的也是同一条 reconcile 路径，两边保持一致。
    fn request_run_in_background(
        &mut self,
        index: usize,
        label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions[index].session else {
            return;
        };
        if self.sessions[index].scheduler_job.is_some() {
            return;
        }
        let tab_id = self.sessions[index].tab_id;
        let priority = self.priority_for(index);
        self.sessions[index].scheduler_job = Some(label);
        cx.notify();
        self.spawn_scheduler_job(
            window,
            cx,
            move |manager| manager.request_run(session, priority).map(|_| ()),
            move |panel, result, _, _| {
                panel.project_tab(tab_id, |panel| {
                    panel.scheduler_job = None;
                    match result {
                        // 抢到槽和进了队列都不是错误：状态由 reconcile 如实反映。
                        Ok(()) => {
                            panel.rpc_success = None;
                            panel.clear_rpc_error();
                            panel.scheduler_failure = None;
                        }
                        Err(error) => {
                            panel.rpc_success = None;
                            panel.rpc_error_protected = true;
                            panel.rpc_error = Some(error);
                        }
                    }
                });
            },
        );
    }

    /// 历史渲染完成后回填到**发起它的那个标签**。
    ///
    /// 必须按 `tab_id` 找槽：渲染是后台任务，回来时用户可能已经切到别的标签，
    /// 走 `Deref` 会把 A 的历史写进 B。
    fn finish_load(
        &mut self,
        tab_id: u64,
        generation: LoadGeneration,
        title: String,
        result: Result<Arc<ConversationDocument>, String>,
    ) -> bool {
        let Some(index) = self.slot_index_for_tab(tab_id) else {
            return false;
        };
        self.project(index, |panel| {
            if generation != panel.load_generation {
                return false;
            }
            panel.status = match result {
                Ok(document) => {
                    panel.sync_list_document(&document, true);
                    panel.list_state.scroll_to_end();
                    ChatStatus::Ready(document)
                }
                Err(message) => ChatStatus::Error { title, message },
            };
            true
        })
    }

    /// 在当前标签上把一份历史变成活会话。
    fn start_live(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.is_some() || self.control_busy() {
            return;
        }
        let index = self.focused;
        // 已经登记过（Parked / Failed / Queued）的会话走重跑，不再登记第二遍 ——
        // 再登记一次会把同一段对话变成两个互不相识的会话。
        if self.sessions[index].session.is_some() {
            self.resume_session(index, window, cx);
            return;
        }
        let ChatStatus::Ready(history) = &self.status else {
            return;
        };
        let history = history.clone();
        let tool_preset = self.tool_preset;
        self.begin_active_generation();
        let session_path = history.source_path.clone();
        let cwd = session_cwd(&session_path).unwrap_or_else(|| PathBuf::from("."));
        let descriptor = pi_runtime::SessionDescriptor {
            binary: official_binary(),
            cwd,
            session_path: Some(session_path),
            tool_preset,
            agent_dir: pi_data::agent_dir(),
        };
        self.rpc_success = None;
        self.rpc_error = None;
        self.bind_session(index, descriptor, (*history).clone(), window, cx);
        cx.notify();
    }

    /// 重新请求运行一个已登记的会话（Parked / Failed / Queued 都走这里）。
    fn resume_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.request_run_in_background(index, "恢复中…", window, cx);
    }

    /// 把一个刚拿到的 Runtime 装到标签上，并为它起一条事件泵。
    fn attach_runtime(
        &mut self,
        index: usize,
        handle: SessionHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let receiver = handle.subscribe_dirty();
        let tab_id = self.sessions[index].tab_id;
        let terminal = self.project(index, |panel| {
            panel.install_active(handle);
            // 立刻拉一次：`subscribe_dirty` 只登记发送端，不补发当前修订号。
            // 启动/热接管的元数据可能在我们订阅**之前**就跑完了，那几条 Dirty
            // 发给了零个订阅者；不在这里补一次，这个标签就会一直没有模型、
            // 没有 slash 命令、没有启动诊断，直到下一次运行时事件——空闲会话
            // 可能永远等不到。
            panel.pull_runtime_snapshot(cx)
        });
        self.spawn_dirty_pump(tab_id, receiver, window.window_handle(), cx);
        if terminal {
            // 订阅不补发之前的 Dirty：进程若在「Manager 发布 Running」与「订阅装好」
            // 之间就退出了，上面这次补拉就是**唯一**一次能看到终态的机会。
            // 不在这里收口，运行槽会一直挂在 Running 上，排队会话要等 reaper 轮询
            //（默认 TTL 下最长 45s）才补位。走后台，`tick()` 可能就地拉起新会话。
            self.spawn_scheduler_job(window, cx, |manager| manager.tick(), |_, (), _, _| {});
        }
        cx.notify();
    }

    fn install_active(&mut self, active: SessionHandle) {
        let snapshot = active.snapshot();
        self.active_epoch = snapshot.epoch;
        self.applied_revision = 0;
        self.effect_cursor = 0;
        // 背压计数是每 Runtime 累加的，换 Runtime 必须一起归零，否则旧计数会压住新提示。
        // 提示本身也在这里一并清掉：让「归零基线」和「清空提示」待在同一个函数里，
        // 避免以后新增调用点时漏清而重现提示驻留。
        self.backpressure = pi_runtime::BackpressureStats::default();
        self.backpressure_note = None;
        self.backpressure_note_until = None;
        self.set_host_extension_degradation(snapshot.startup_diagnostic.as_deref());
        self.extension_response_sender = Some(Arc::new(
            crate::live_session::HandleExtensionResponseSender::new(active.clone(), snapshot.epoch),
        ));
        self.active = Some(active);
    }

    /// 为一个标签的 Runtime 起事件泵。
    ///
    /// 每条泵只服务**它自己那个标签**：投影游标由 `tab_id` 定位，因此后台会话照常
    /// 消费 effect（不消费的话 R22 的有界 effect 缓存会把它们淘汰掉，用户切回来就
    /// 少了一段），但只有前台标签才允许驱动 window —— 通知、窗口标题和 Extension UI
    /// 对话框是全局资源，后台会话去动它们等于替用户抢屏幕。
    fn spawn_dirty_pump(
        &self,
        tab_id: u64,
        receiver: std::sync::mpsc::Receiver<pi_runtime::Dirty>,
        window_handle: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        // 有界且合并，与调度器桥同一条规则。`Dirty` 只是「有变化了」的信号——
        // `pull_runtime_snapshot` 回来会重新读一次完整 Snapshot，中间那些丢掉无损。
        // 这里必须有界：多会话之后每个 Runtime 各有一条桥，一条无界队列会被会话数
        // 乘一遍，正是 R22 要消灭的东西。
        let (mut tx, mut rx) = mpsc::channel(0);
        std::thread::Builder::new()
            .name("pi-runtime-dirty-bridge".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    if let Err(error) = tx.try_send(())
                        && error.is_disconnected()
                    {
                        break;
                    }
                }
            })
            .expect("failed to spawn runtime dirty bridge");
        cx.spawn(async move |panel, cx| {
            while rx.next().await.is_some() {
                let should_stop = window_handle
                    .update(cx, |_, window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                let Some(index) = panel.slot_index_for_tab(tab_id) else {
                                    // 标签已经关掉：这条泵没有归属了，退出。
                                    return true;
                                };
                                let should_stop =
                                    panel.project(index, |panel| panel.pull_runtime_snapshot(cx));
                                if should_stop {
                                    // 这个 Runtime 进了终态：推一次调度让 Manager 收回运行槽，
                                    // 排队中的会话才能马上补位，而不是等 reaper 轮询。
                                    // 放后台——这里正跑在 GPUI 主线程上，而 `tick()` 可能
                                    // 就地把排队会话拉起来。
                                    panel.spawn_scheduler_job(
                                        window,
                                        cx,
                                        |manager| manager.tick(),
                                        |_, (), _, _| {},
                                    );
                                    panel.reconcile_scheduler(window, cx);
                                }
                                if index == panel.focused {
                                    panel.process_extension_ui(window, cx);
                                }
                                cx.notify();
                                should_stop
                            })
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);
                if should_stop {
                    return;
                }
            }
        })
        .detach();
    }

    fn pull_runtime_snapshot(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(active) = self.active.clone() else {
            return true;
        };
        let snapshot = active.snapshot();
        if snapshot.runtime_id != active.runtime_id() {
            return false;
        }
        // 终态取自 Snapshot 的权威字段，而**不是**「`Stopped` effect 有没有被投影出来」：
        // effect 会被背压淘汰，终态不会。收不到终态这条泵就会一直阻塞在下一条 Dirty 上，
        // 而崩溃的 Runtime 再也不会产生 Dirty —— 运行槽要拖到 reaper 轮询才回收，
        // 排队中的会话跟着一起等（默认 TTL 下最长 45s）。
        let terminal = snapshot.terminal.is_some();
        if snapshot.revision <= self.applied_revision {
            return terminal;
        }
        let epoch_changed = snapshot.epoch != self.active_epoch;
        if epoch_changed {
            self.active_epoch = snapshot.epoch;
            self.effect_cursor = 0;
            self.extension_response_sender = Some(Arc::new(
                crate::live_session::HandleExtensionResponseSender::new(active, snapshot.epoch),
            ));
        }
        self.applied_revision = snapshot.revision;
        self.apply_snapshot(snapshot, cx) || terminal
    }

    fn apply_snapshot(&mut self, snapshot: SessionSnapshot, cx: &mut Context<Self>) -> bool {
        let effects = snapshot
            .effects
            .iter()
            .filter(|effect| effect.epoch == snapshot.epoch && effect.sequence > self.effect_cursor)
            .cloned()
            .collect::<Vec<_>>();
        let mut settled = false;
        for effect in effects {
            self.effect_cursor = self.effect_cursor.max(effect.sequence);
            settled |= self.apply_runtime_effect(effect, cx);
        }
        // R22：回收已消费 effect，让运行时缓存在稳态下保持接近空，
        // 而不是靠背压淘汰来维持有界。
        if let Some(active) = self.active.as_ref() {
            active.ack_effects(snapshot.epoch, self.effect_cursor);
        }
        self.report_backpressure(snapshot.backpressure);
        let document = snapshot.document;
        self.sync_list_document(&document, settled);
        self.status = ChatStatus::Ready(document);
        if self.active_epoch != snapshot.epoch {
            return false;
        }
        false
    }

    /// 把运行时的背压淘汰暴露给用户。
    ///
    /// 只在计数**新增**时提示一次：这些计数是单调累加的，每帧重复报会刷屏。
    /// 把运行时的背压淘汰暴露给用户。
    ///
    /// 只在计数**新增**时提示一次：这些计数是单调累加的，每帧重复报会刷屏。
    fn report_backpressure(&mut self, stats: pi_runtime::BackpressureStats) {
        /// 弱提示的最短可读驻留时间。
        const NOTE_MIN_DISPLAY: std::time::Duration = std::time::Duration::from_secs(5);
        /// 追加背压说明时用来判重的关键词，避免同一条错误被反复追加而无限变长。
        const ALERT_MARK: &str = "事件积压过多";

        let previous = self.backpressure;
        self.backpressure = stats;
        let grew = |current: u64, before: u64| current.saturating_sub(before);
        let dropped_results = grew(stats.dropped_results, previous.dropped_results);
        let dropped_coalescable = grew(stats.dropped_coalescable, previous.dropped_coalescable);
        let dropped_runtime_events = grew(
            stats.dropped_runtime_events,
            previous.dropped_runtime_events,
        );
        let dropped_diagnostics = grew(stats.dropped_diagnostics, previous.dropped_diagnostics);
        let dropped_jobs = grew(stats.dropped_jobs, previous.dropped_jobs);

        // 真正丢了用户结果才占错误位。
        let alert = if dropped_results > 0 {
            Some(format!(
                "{ALERT_MARK}，{dropped_results} 条操作结果未能送达界面；界面状态可能不完整，建议重新载入会话"
            ))
        } else if dropped_coalescable > 0 {
            Some(format!(
                "{ALERT_MARK}，{dropped_coalescable} 条界面更新已被合并丢弃"
            ))
        } else {
            None
        };
        if let Some(alert) = alert {
            self.rpc_success = None;
            self.rpc_error = match self.rpc_error.take() {
                // 已有具体失败原因时只追加，不替换；已经追加过就不再重复，防止无限变长。
                Some(existing) if self.rpc_error_protected => {
                    if existing.contains(ALERT_MARK) {
                        Some(existing)
                    } else {
                        Some(format!("{existing}；{alert}"))
                    }
                }
                _ => Some(alert),
            };
        }

        // 以下都是「自愈型」降级：下一次 settle 或控制操作会重新拉取，只进弱提示位。
        // 它描述的是瞬时事件，必须自行退场；但退场要给一个可读窗口，否则在 16–33ms 的
        // 合帧节奏下只活一帧，既看不清又让下方的 composer 逐帧跳动。
        let mut notes = Vec::new();
        if dropped_runtime_events > 0 {
            notes.push(format!("{dropped_runtime_events} 条过程状态被合帧取最新"));
        }
        if dropped_diagnostics > 0 {
            notes.push(format!("{dropped_diagnostics} 条积压诊断被丢弃"));
        }
        if dropped_jobs > 0 {
            notes.push(format!("{dropped_jobs} 次元数据刷新因队列繁忙被跳过"));
        }
        let now = std::time::Instant::now();
        if notes.is_empty() {
            if self
                .backpressure_note_until
                .is_some_and(|until| now >= until)
            {
                self.backpressure_note = None;
                self.backpressure_note_until = None;
            }
        } else {
            self.backpressure_note = Some(notes.join("；"));
            self.backpressure_note_until = Some(now + NOTE_MIN_DISPLAY);
        }
    }

    fn apply_runtime_effect(&mut self, effect: RuntimeEffect, cx: &mut Context<Self>) -> bool {
        match effect.kind {
            RuntimeEffectKind::Events {
                follow_tail,
                settled,
                runtime_events,
            } => {
                if follow_tail && self.tail_attached {
                    self.follow_requested = true;
                }
                self.apply_runtime_events(runtime_events);
                settled
            }
            RuntimeEffectKind::ExtensionUiBatch { requests } => {
                for (id, request) in requests {
                    let duplicate = self.extension_ui.is_dialog_pending(&id);
                    if let Some(response) = self.extension_ui.apply(id.clone(), request) {
                        self.pending_extension_responses.push(response);
                    } else if duplicate {
                        self.rpc_error_protected = true;
                        self.rpc_error = Some(format!("重复 Extension UI 请求 {id} 已忽略"));
                    }
                }
                false
            }
            RuntimeEffectKind::ExtensionUiReset => {
                self.clear_extension_ui_for_lifecycle();
                false
            }
            RuntimeEffectKind::RequestFinished {
                intent,
                submission,
                result,
                ..
            } => {
                match result {
                    Ok(()) => {
                        self.rpc_success = None;
                        self.clear_rpc_error();
                    }
                    Err((kind, error)) => {
                        self.rpc_success = None;
                        let mut restored_draft = false;
                        if should_restore_submission(kind)
                            && let Some(submission) = submission
                        {
                            let restored = self.drafts.restore_submission(
                                &self.draft_slot_key(),
                                pi_data::ComposerDraft {
                                    text: submission.message,
                                    images: submission.images,
                                },
                            );
                            self.pending_draft_restore = true;
                            self.attachments = restored
                                .images
                                .into_iter()
                                .filter_map(attachment_from_draft)
                                .collect();
                            restored_draft = true;
                        }
                        self.rpc_error_protected = true;
                        self.rpc_error = Some(match kind {
                            // R22 起 `submission` 可能因 effect 缓存的字节上限被剥离，
                            // 此时草稿并没有真的恢复；文案必须跟着实际可见状态走，
                            // 否则横幅说「已恢复」而输入框和附件条都是空的。
                            RequestFailureKind::Rejected if restored_draft => {
                                format!("pi 明确拒绝提交，已恢复草稿：{error}")
                            }
                            RequestFailureKind::Rejected => {
                                format!("pi 明确拒绝提交（草稿因积压未能保留）：{error}")
                            }
                            RequestFailureKind::Ambiguous => {
                                format!("提交结果不明确，为避免重复 turn 未自动恢复：{error}")
                            }
                        });
                        if intent == RpcIntent::Abort {
                            self.rpc_error_protected = true;
                            self.rpc_error = Some(format!("停止失败：{error}"));
                        }
                    }
                }
                false
            }
            RuntimeEffectKind::CommandsLoaded(result) => {
                match result {
                    Ok(commands) => {
                        self.slash_commands = commands;
                        self.refresh_popup_without_input();
                    }
                    // 拆除期间的元数据失败是交出进程的必然产物，不是故障。
                    Err(_) if self.is_tearing_down() => {}
                    Err(error) => {
                        self.rpc_error_protected = true;
                        self.rpc_error = Some(format!("加载 slash 命令失败：{error}"));
                    }
                }
                false
            }
            RuntimeEffectKind::ControlsLoaded(result) => {
                match result {
                    Ok(controls) => {
                        if self.fresh_session {
                            let old_key = self.draft_slot_key();
                            migrate_draft_key(&mut self.drafts, &old_key, &controls.session_id);
                            self.draft_key = Some(controls.session_id.clone());
                            // 身份刚从「无」变成真实 session id：前台标签得把它广播出去，
                            // 否则工作区 tooltip 会一直停在校准之前的状态。
                            if self.projecting_focused_tab() {
                                self.emit_focused_session(cx);
                            }
                            if controls.session_file.is_some() {
                                self.fresh_session = false;
                                cx.emit(SessionsChanged);
                            }
                        }
                        self.apply_controls(controls);
                    }
                    Err(_) if self.is_tearing_down() => {}
                    Err(error) => {
                        self.rpc_error_protected = true;
                        self.rpc_error = Some(format!("加载会话控制失败：{error}"));
                    }
                }
                false
            }
            RuntimeEffectKind::ControlFinished { operation, result } => {
                self.control_operation = None;
                match result {
                    Ok(outcome) => self.apply_control_outcome(operation, outcome, cx),
                    Err(error) => {
                        self.compacting = false;
                        self.rpc_error_protected = true;
                        self.rpc_error = Some(format!("会话操作失败：{error}"));
                        if let Some(active) = self.active.as_ref() {
                            active.refresh_metadata();
                        }
                    }
                }
                false
            }
            RuntimeEffectKind::ToolRestartFinished { preset, result } => {
                self.pending_extension_responses.clear();
                self.extension_ui.reset();
                self.extension_dialog_needs_close |= self.extension_dialog_open.take().is_some();
                self.control_operation = None;
                match result {
                    Ok(()) => {
                        self.tool_preset = preset;
                        self.rpc_success = None;
                        self.rpc_error = None;
                    }
                    Err(error) => {
                        self.rpc_success = None;
                        self.rpc_error =
                            Some(format!("工具预设重启失败；请重新启动活会话：{error}"));
                        self.active = None;
                    }
                }
                false
            }
            RuntimeEffectKind::Diagnostic(message) => {
                self.host_extension_degradation = Some(message);
                false
            }
            RuntimeEffectKind::Stopped(error) => {
                self.extension_ui.reset();
                self.extension_response_sender = None;
                self.pending_extension_responses.clear();
                self.extension_dialog_needs_close |= self.extension_dialog_open.take().is_some();
                if let Some(error) = error {
                    self.rpc_error_protected = true;
                    self.rpc_error = Some(error);
                }
                self.control_operation = None;
                self.active = None;
                true
            }
        }
    }

    pub(crate) fn process_extension_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // MainPanel render 与 pump 都可能在同一帧调用。所有投影操作必须幂等：queue 项只
        // take 一次、title/editor 仅在变化时写入、dialog 由 id/pending-close 串行化。
        if self.extension_dialog_needs_close && self.extension_dialog_is_topmost(window, cx) {
            self.extension_dialog_needs_close = false;
            window.close_dialog(cx);
            self.clear_extension_dialog_focus();
        }
        if self.extension_dialog_needs_close && !window.has_active_dialog(cx) {
            self.extension_dialog_needs_close = false;
            self.clear_extension_dialog_focus();
        }
        while let Some(diagnostic) = self.extension_ui.take_diagnostic() {
            self.rpc_error = Some(diagnostic);
        }
        for response in self
            .pending_extension_responses
            .drain(..)
            .collect::<Vec<_>>()
        {
            self.send_extension_response(response, cx);
        }
        while self
            .extension_ui
            .active_dialog_expired(std::time::Instant::now())
        {
            let Some(id) = self
                .extension_ui
                .active_dialog()
                .map(|dialog| dialog.id.clone())
            else {
                break;
            };
            if self.extension_dialog_open.as_deref() == Some(&id) {
                self.discard_extension_dialog(&id, "Extension UI 请求已超时", window, cx);
                break;
            }
            // 排队期间已经超时的请求直接取消，不打开一帧再关闭。
            self.send_extension_response(pi_rpc::ExtensionUiResponse::cancelled(&id), cx);
            self.extension_ui.finish_dialog(&id);
            self.rpc_error = Some("Extension UI 请求在队列中已超时".to_owned());
        }
        while let Some(notification) = self.extension_ui.take_notification() {
            let notification = match notification.notify_type {
                pi_rpc::NotifyType::Info => Notification::info(notification.message),
                pi_rpc::NotifyType::Warning => Notification::warning(notification.message),
                pi_rpc::NotifyType::Error => Notification::error(notification.message),
            };
            window.push_notification(notification, cx);
        }
        let title = self.extension_ui.title().unwrap_or("GPUI-Pi").to_owned();
        if title != self.window_title {
            self.window_title = title.clone();
            window.set_window_title(&title);
        }
        if let Some(text) = self.extension_ui.take_editor_text() {
            self.composer
                .update(cx, |input, cx| input.set_value(text, window, cx));
            self.save_current_draft(cx);
        }
        if self.extension_dialog_needs_close {
            return;
        }
        self.maybe_open_extension_dialog(window, cx);
    }

    fn send_extension_response(
        &mut self,
        response: pi_rpc::ExtensionUiResponse,
        cx: &mut Context<Self>,
    ) {
        let Some(sender) = self.extension_response_sender.clone() else {
            self.rpc_error = Some("Extension UI 响应未写回：活会话已结束".to_owned());
            return;
        };
        let id = response.id().to_owned();
        // 写回失败的诊断属于**这个会话**，不属于用户此刻正看着的那个标签。
        let tab_id = self.tab_id;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |panel, cx| {
            let result = executor.spawn(async move { sender.send(response) }).await;
            let _ = panel.update(cx, |panel, cx| {
                panel.project_tab(tab_id, |panel| {
                    if let Err(error) = result {
                        panel.rpc_error = Some(format!(
                            "Extension UI 响应 {id} 未写回，已丢弃并继续队列：{error}"
                        ));
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn extension_dialog_is_topmost(&self, window: &Window, cx: &mut App) -> bool {
        [
            self.extension_dialog_body_focus.as_ref(),
            self.extension_dialog_footer_focus.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|focus| focus.contains_focused(window, cx) || focus.within_focused(window, cx))
    }

    fn clear_extension_dialog_focus(&mut self) {
        self.extension_dialog_body_focus = None;
        self.extension_dialog_footer_focus = None;
    }

    fn request_extension_dialog_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.extension_dialog_is_topmost(window, cx) {
            window.close_dialog(cx);
            self.clear_extension_dialog_focus();
            self.extension_dialog_needs_close = false;
        } else {
            self.extension_dialog_needs_close = true;
        }
    }

    fn discard_extension_dialog(
        &mut self,
        id: &str,
        reason: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.extension_dialog_open.as_deref() == Some(id) {
            self.finish_extension_dialog(id, pi_rpc::ExtensionUiResponse::cancelled(id), cx);
            self.request_extension_dialog_close(window, cx);
            self.rpc_error = Some(reason.to_owned());
            cx.notify();
        }
    }

    fn maybe_open_extension_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog_request) = self.extension_ui.active_dialog().cloned() else {
            self.extension_dialog_open = None;
            if !self.extension_dialog_needs_close {
                self.clear_extension_dialog_focus();
            }
            return;
        };
        if self.extension_dialog_open.as_deref() == Some(&dialog_request.id) {
            return;
        }
        if window.has_active_dialog(cx) {
            return;
        }
        self.extension_dialog_open = Some(dialog_request.id.clone());
        let dialog_body_focus = cx.focus_handle();
        let dialog_footer_focus = cx.focus_handle();
        self.extension_dialog_body_focus = Some(dialog_body_focus.clone());
        self.extension_dialog_footer_focus = Some(dialog_footer_focus.clone());
        if let Some(deadline) = dialog_request.deadline {
            let panel = cx.weak_entity();
            let timeout_id = dialog_request.id.clone();
            let timeout_sequence = dialog_request.sequence;
            // 超时属于开出这个对话框的标签。不钉住的话，定时器会去检查前台标签的
            // 队列——id 撞上就取消了**别人**的请求，撞不上则这条超时被静默吞掉。
            let tab_id = self.tab_id;
            let timer = cx.background_executor().clone();
            cx.spawn(async move |_, cx| {
                timer
                    .timer(deadline.saturating_duration_since(std::time::Instant::now()))
                    .await;
                let _ = panel.update(cx, |panel, cx| {
                    panel.project_tab(tab_id, |panel| {
                        if panel.extension_ui.active_dialog().is_some_and(|dialog| {
                            dialog.id == timeout_id && dialog.sequence == timeout_sequence
                        }) {
                            panel.expire_extension_dialog(&timeout_id, cx);
                            panel.rpc_error = Some("Extension UI 请求已超时".to_owned());
                        }
                    });
                });
            })
            .detach();
        }
        let panel = cx.entity();
        let id = dialog_request.id.clone();
        let request = dialog_request.request.clone();
        let select_options = dialog_request.select_options.clone();
        let element_id = self.next_extension_element_id;
        self.next_extension_element_id = self.next_extension_element_id.wrapping_add(1);
        let open_value_dialog = |title: String,
                                 initial_value: String,
                                 placeholder: Option<String>,
                                 multiline: bool,
                                 window: &mut Window,
                                 cx: &mut Context<Self>| {
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, if multiline { 8 } else { 3 })
                    .submit_on_enter(!multiline)
                    .placeholder(placeholder.unwrap_or_default())
                    .default_value(initial_value)
            });
            let ok_input = input.clone();
            let ok_panel = panel.clone();
            let cancel_panel = panel.clone();
            let cancel_id = id.clone();
            let ok_id = id.clone();
            let builder_body_focus = dialog_body_focus.clone();
            let builder_footer_focus = dialog_footer_focus.clone();
            window.open_dialog(cx, move |dialog, _, _| {
                dialog
                    .title(title.clone())
                    .child(
                        div()
                            .debug_selector(|| "extension-dialog-textarea".into())
                            .track_focus(&builder_body_focus)
                            .child(Textarea::new(&input)),
                    )
                    .close_button(false)
                    .keyboard(false)
                    .overlay_closable(false)
                    .on_ok({
                        let ok_input = ok_input.clone();
                        let ok_panel = ok_panel.clone();
                        let ok_id = ok_id.clone();
                        move |_, _, cx| {
                            let value = ok_input.read(cx).value().to_string();
                            ok_panel.update(cx, |panel, cx| {
                                panel.finish_extension_dialog(
                                    &ok_id,
                                    pi_rpc::ExtensionUiResponse::value(&ok_id, value),
                                    cx,
                                );
                            });
                            true
                        }
                    })
                    .on_cancel({
                        let cancel_panel = cancel_panel.clone();
                        let cancel_id = cancel_id.clone();
                        move |_, _, cx| {
                            cancel_panel.update(cx, |panel, cx| {
                                panel.finish_extension_dialog(
                                    &cancel_id,
                                    pi_rpc::ExtensionUiResponse::cancelled(&cancel_id),
                                    cx,
                                );
                            });
                            true
                        }
                    })
                    .footer(
                        div().track_focus(&builder_footer_focus).child(
                            DialogFooter::new()
                                .child(
                                    div()
                                        .debug_selector(|| "extension-dialog-cancel".into())
                                        .child(
                                            DialogClose::new().child(
                                                Button::new("extension-dialog-cancel")
                                                    .track_focus(&builder_footer_focus)
                                                    .secondary()
                                                    .label("取消"),
                                            ),
                                        ),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "extension-dialog-submit".into())
                                        .child(
                                            DialogAction::new().child(
                                                Button::new("extension-dialog-submit")
                                                    .track_focus(&builder_footer_focus)
                                                    .primary()
                                                    .label("提交"),
                                            ),
                                        ),
                                ),
                        ),
                    )
            });
            dialog_body_focus.focus(window, cx);
        };
        match request {
            pi_rpc::ExtensionUiRequest::Select { title, .. } => {
                let options = Arc::new(select_options.unwrap_or_default());
                let builder_body_focus = dialog_body_focus.clone();
                let builder_footer_focus = dialog_footer_focus.clone();
                window.open_dialog(cx, move |dialog, _, _| {
                    let cancel_panel = panel.clone();
                    let cancel_id = id.clone();
                    dialog
                        .title(title.clone())
                        .close_button(false)
                        .keyboard(false)
                        .overlay_closable(false)
                        .child(v_flex().track_focus(&builder_body_focus).gap_1().children(
                            options.iter().enumerate().map(|(index, option)| {
                                let option_panel = panel.clone();
                                let option_id = id.clone();
                                let value = option.raw.clone();
                                Button::new(format!("extension-select-option-{element_id}-{index}"))
                                    .track_focus(&builder_body_focus)
                                    .debug_selector(move || {
                                        format!("extension-select-option-{index}")
                                    })
                                    .secondary()
                                    .label(option.display.clone())
                                    .on_click(move |_, window, cx| {
                                        option_panel.update(cx, |panel, cx| {
                                            panel.finish_extension_dialog(
                                                &option_id,
                                                pi_rpc::ExtensionUiResponse::value(
                                                    &option_id,
                                                    value.clone(),
                                                ),
                                                cx,
                                            );
                                        });
                                        window.close_dialog(cx);
                                    })
                            }),
                        ))
                        .on_ok(|_, _, _| false)
                        .on_cancel({
                            let cancel_panel = cancel_panel.clone();
                            let cancel_id = cancel_id.clone();
                            move |_, _, cx| {
                                cancel_panel.update(cx, |panel, cx| {
                                    panel.finish_extension_dialog(
                                        &cancel_id,
                                        pi_rpc::ExtensionUiResponse::cancelled(&cancel_id),
                                        cx,
                                    );
                                });
                                true
                            }
                        })
                        .footer(
                            div().track_focus(&builder_footer_focus).child(
                                DialogFooter::new().child(
                                    DialogClose::new().child(
                                        Button::new("extension-select-cancel")
                                            .track_focus(&builder_footer_focus)
                                            .secondary()
                                            .label("取消"),
                                    ),
                                ),
                            ),
                        )
                });
                dialog_body_focus.focus(window, cx);
            }
            pi_rpc::ExtensionUiRequest::Confirm { title, message, .. } => {
                let ok_panel = panel.clone();
                let cancel_panel = panel.clone();
                let ok_id = id.clone();
                let cancel_id = id.clone();
                let builder_body_focus = dialog_body_focus.clone();
                let builder_footer_focus = dialog_footer_focus.clone();
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog
                        .title(title.clone())
                        .close_button(false)
                        .keyboard(false)
                        .overlay_closable(false)
                        .child(
                            div()
                                .track_focus(&builder_body_focus)
                                .child(message.clone()),
                        )
                        .on_ok({
                            let ok_panel = ok_panel.clone();
                            let ok_id = ok_id.clone();
                            move |_, _, cx| {
                                ok_panel.update(cx, |panel, cx| {
                                    panel.finish_extension_dialog(
                                        &ok_id,
                                        pi_rpc::ExtensionUiResponse::confirmed(&ok_id, true),
                                        cx,
                                    );
                                });
                                true
                            }
                        })
                        .on_cancel({
                            let cancel_panel = cancel_panel.clone();
                            let cancel_id = cancel_id.clone();
                            move |_, _, cx| {
                                cancel_panel.update(cx, |panel, cx| {
                                    panel.finish_extension_dialog(
                                        &cancel_id,
                                        pi_rpc::ExtensionUiResponse::cancelled(&cancel_id),
                                        cx,
                                    );
                                });
                                true
                            }
                        })
                        .footer(
                            div().track_focus(&builder_footer_focus).child(
                                DialogFooter::new()
                                    .child(
                                        div()
                                            .debug_selector(|| "extension-confirm-cancel".into())
                                            .child(
                                                DialogClose::new().child(
                                                    Button::new("extension-confirm-cancel")
                                                        .track_focus(&builder_footer_focus)
                                                        .secondary()
                                                        .label("取消"),
                                                ),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .debug_selector(|| "extension-confirm-submit".into())
                                            .child(
                                                DialogAction::new().child(
                                                    Button::new("extension-confirm-submit")
                                                        .track_focus(&builder_footer_focus)
                                                        .primary()
                                                        .label("确认"),
                                                ),
                                            ),
                                    ),
                            ),
                        )
                });
                dialog_body_focus.focus(window, cx);
            }
            pi_rpc::ExtensionUiRequest::Input {
                title, placeholder, ..
            } => open_value_dialog(title, String::new(), placeholder, false, window, cx),
            pi_rpc::ExtensionUiRequest::Editor { title, prefill } => {
                open_value_dialog(title, prefill.unwrap_or_default(), None, true, window, cx)
            }
            _ => {
                self.pending_extension_responses
                    .push(pi_rpc::ExtensionUiResponse::cancelled(&id));
                self.extension_ui.finish_dialog(&id);
                self.extension_dialog_open = None;
                self.clear_extension_dialog_focus();
                self.rpc_error = Some("未知 Extension UI dialog 请求已取消".to_owned());
            }
        }
    }

    /// 让一个**已到期**的请求收口，无论它此刻是否正显示在窗口上。
    ///
    /// 截止时间属于**请求**，不属于窗口。标签被切走时对话框会被收起
    /// （`extension_dialog_open` 清空），但 pi 那头仍在等一个响应；只按
    /// 「窗口上正开着」判断，后台标签的超时请求就会永远悬着。
    fn expire_extension_dialog(&mut self, id: &str, cx: &mut Context<Self>) {
        if self
            .extension_ui
            .active_dialog()
            .map(|dialog| dialog.id.as_str())
            != Some(id)
        {
            return;
        }
        // 只有确实是自己开着的那一个才请求关窗；别人的对话框不归这里管。
        if self.extension_dialog_open.as_deref() == Some(id) {
            self.extension_dialog_needs_close = true;
            self.extension_dialog_open = None;
        }
        self.send_extension_response(pi_rpc::ExtensionUiResponse::cancelled(id), cx);
        self.extension_ui.finish_dialog(id);
        cx.notify();
    }

    fn finish_extension_dialog(
        &mut self,
        id: &str,
        response: pi_rpc::ExtensionUiResponse,
        cx: &mut Context<Self>,
    ) {
        if self.extension_dialog_open.as_deref() != Some(id)
            || self
                .extension_ui
                .active_dialog()
                .map(|dialog| dialog.id.as_str())
                != Some(id)
        {
            return;
        }
        self.send_extension_response(response, cx);
        self.extension_ui.finish_dialog(id);
        self.extension_dialog_open = None;
        cx.notify();
    }

    fn clear_extension_ui_for_lifecycle(&mut self) {
        // lifecycle reset 可能紧邻进程替换；旧 dialog 的 cancelled 不能排队写给新 client。
        self.pending_extension_responses.clear();
        self.extension_ui.reset();
        self.extension_dialog_needs_close |= self.extension_dialog_open.take().is_some();
    }

    /// 结清**这个标签**欠 pi 的 Extension UI 债务。
    ///
    /// 只碰标签自己的状态。窗口级的那一半（对话框、焦点句柄、窗口标题）在
    /// [`ChatPanel::reset_foreground_extension_ui`] 里，两者必须分开：关一个**后台**
    /// 标签时若把窗口级状态一起清掉，前台正开着的对话框就会失去焦点句柄，
    /// 从此既认不出它是最上层、也关不掉它——一个关不掉的模态浮在别的会话上，
    /// 它那条请求也永远回不去。
    fn reset_extension_ui_slot(&mut self, cx: &mut Context<Self>) {
        let cancelled = self.extension_ui.drain_cancelled_dialogs();
        for response in cancelled {
            self.send_extension_response(response, cx);
        }
        self.extension_ui.reset();
        self.extension_response_sender = None;
    }

    /// 前台标签的 Extension UI 收尾：标签自己的那份，加上窗口级的那份。
    fn reset_foreground_extension_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_dialog = self.extension_dialog_open.take().is_some();
        self.reset_extension_ui_slot(cx);
        if had_dialog {
            self.request_extension_dialog_close(window, cx);
        } else {
            self.clear_extension_dialog_focus();
        }
        // `extension_ui` 刚被清空，标题自然回落到 "GPUI-Pi"；走同一个入口，
        // 免得这里和别处对「窗口标题该是什么」各写一份。
        self.apply_window_title(window);
    }

    fn apply_runtime_events(&mut self, events: Vec<SessionRuntimeEvent>) {
        for event in events {
            match event {
                SessionRuntimeEvent::CompactionStarted => self.compacting = true,
                SessionRuntimeEvent::CompactionEnded { error } => {
                    self.compacting = false;
                    if let Some(error) = error {
                        self.rpc_success = None;
                        self.rpc_error_protected = true;
                        self.rpc_error = Some(format!("Compaction 失败：{error}"));
                    }
                }
                SessionRuntimeEvent::RetryStarted {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error,
                } => {
                    self.retry_status = Some(RetryStatus {
                        attempt,
                        max_attempts,
                        delay_ms,
                        error,
                    });
                }
                SessionRuntimeEvent::RetryEnded { error, .. } => {
                    self.retry_status = None;
                    if let Some(error) = error {
                        self.rpc_success = None;
                        self.rpc_error_protected = true;
                        self.rpc_error = Some(format!("Auto-retry 结束：{error}"));
                    }
                }
                SessionRuntimeEvent::AgentEnded { will_retry } => {
                    if !will_retry && self.retry_status.is_some() {
                        self.retry_status = None;
                    }
                }
            }
        }
    }

    fn apply_control_outcome(
        &mut self,
        operation: ControlOperation,
        outcome: ControlOutcome,
        cx: &mut Context<Self>,
    ) {
        let sessions_changed = sessions_changed_for_outcome(&outcome);
        self.rpc_success = None;
        self.rpc_error = None;
        match outcome {
            ControlOutcome::Controls(controls) | ControlOutcome::Switched(controls) => {
                self.apply_session_rebind(controls);
            }
            ControlOutcome::Compacted(_) => {
                self.compacting = false;
                if let Some(active) = self.active.as_ref() {
                    active.refresh_metadata();
                }
            }
            ControlOutcome::Forked { data, controls } => {
                self.apply_session_rebind(controls);
                self.drafts.set(
                    self.draft_slot_key(),
                    pi_data::ComposerDraft {
                        text: data.text,
                        images: Vec::new(),
                    },
                );
                self.pending_draft_restore = true;
                self.attachments.clear();
            }
            ControlOutcome::ForkCancelled(_) | ControlOutcome::CloneCancelled => {}
            ControlOutcome::RebindCalibrationFailed {
                operation: _,
                message,
                fork_data,
            } => {
                if let Some(data) = fork_data {
                    self.drafts.set(
                        self.draft_slot_key(),
                        pi_data::ComposerDraft {
                            text: data.text,
                            images: Vec::new(),
                        },
                    );
                    self.pending_draft_restore = true;
                    self.attachments.clear();
                }
                self.rpc_error = Some(message);
                if let Some(active) = self.active.as_ref() {
                    active.refresh_metadata();
                }
            }
            ControlOutcome::Cloned { controls, .. } => {
                self.apply_session_rebind(controls);
            }
            ControlOutcome::SwitchCancelled => {}
            ControlOutcome::Exported(data) => {
                self.rpc_success = Some(format!("HTML 已导出：{}", data.path));
            }
            ControlOutcome::RetryAborted => {
                if operation == ControlOperation::AbortRetry {
                    self.retry_status = None;
                }
            }
        }
        if sessions_changed {
            cx.emit(SessionsChanged);
        }
    }

    fn apply_session_rebind(&mut self, controls: SessionControls) {
        if let Some(path) = controls.session_file.clone() {
            self.draft_key = Some(controls.session_id.clone());
            if let Ok(document) = pi_render::render_path(&path) {
                let document = Arc::new(document);
                self.sync_list_document(&document, true);
                self.status = ChatStatus::Ready(document);
            }
        }
        self.branch_preview_leaf = None;
        self.branch_preview_document = None;
        self.apply_controls(controls);
    }

    fn apply_controls(&mut self, controls: SessionControls) {
        self.compacting = controls.is_compacting;
        self.branch_tree = self.current_session_branch_tree(
            controls.session_file.as_deref(),
            controls.tree.leaf_id.as_deref(),
        );
        self.model_names = Arc::new(
            controls
                .models
                .iter()
                .map(|model| {
                    (
                        format!("{}\0{}", model.provider, model.id),
                        model.name.clone(),
                    )
                })
                .collect(),
        );
        self.controls = Some(controls);
    }

    fn current_session_branch_tree(
        &self,
        path: Option<&std::path::Path>,
        authoritative_leaf_id: Option<&str>,
    ) -> Option<pi_data::SessionBranchTree> {
        path.and_then(|path| pi_data::load_session(path).ok())
            .map(|session| session.branch_tree_at_leaf(authoritative_leaf_id))
    }

    fn set_model(&mut self, provider: String, model_id: String, cx: &mut Context<Self>) {
        if self.control_operation.is_some() {
            return;
        }
        let Some(active) = self.active.clone() else {
            return;
        };
        if active.snapshot().phase != LivePhase::Idle {
            return;
        }
        self.control_operation = Some(ControlOperation::Model);
        self.rpc_success = None;
        self.rpc_error = None;
        let _ = active.request_control(
            ControlOperation::Model,
            ControlRequest::SetModel { provider, model_id },
        );
        cx.notify();
    }

    fn can_cycle_model(&self) -> bool {
        self.control_operation.is_none()
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.snapshot().phase == LivePhase::Idle)
    }

    fn cycle_model(&mut self, cx: &mut Context<Self>) {
        if !self.can_cycle_model() {
            return;
        }
        let active = self
            .active
            .clone()
            .expect("can_cycle_model requires an active session");
        self.control_operation = Some(ControlOperation::Model);
        self.rpc_success = None;
        self.rpc_error = None;
        let _ = active.request_control(ControlOperation::Model, ControlRequest::CycleModel);
        cx.notify();
    }

    fn set_thinking(&mut self, level: pi_rpc::ThinkingLevel, cx: &mut Context<Self>) {
        if self.control_operation.is_some() {
            return;
        }
        let Some(active) = self.active.clone() else {
            return;
        };
        if active.snapshot().phase != LivePhase::Idle {
            return;
        }
        self.control_operation = Some(ControlOperation::Thinking);
        self.rpc_success = None;
        self.rpc_error = None;
        let _ = active.request_control(
            ControlOperation::Thinking,
            ControlRequest::SetThinking(level),
        );
        cx.notify();
    }

    fn set_tool_preset(&mut self, preset: ToolPreset, cx: &mut Context<Self>) {
        if self.control_busy() || preset == self.tool_preset {
            return;
        }
        let Some(active) = self.active.take() else {
            // 没有 Runtime 时改预设必须同步到 Manager 的会话描述上：
            // 恢复运行是拿**那份描述**去起进程的，只改 UI 就会出现
            // 「界面写着 ReadOnly、进程却按挂起前那套更宽的工具起来」。
            if let Some(session) = self.session
                && let Err(error) = self
                    .runtime_manager
                    .set_session_tool_preset(session, preset)
            {
                self.rpc_success = None;
                self.rpc_error_protected = true;
                self.rpc_error = Some(format!("切换工具预设失败：{error}"));
                cx.notify();
                return;
            }
            self.tool_preset = preset;
            self.rpc_success = None;
            self.rpc_error = None;
            cx.notify();
            return;
        };
        if active.snapshot().phase != LivePhase::Idle {
            self.active = Some(active);
            return;
        }
        let ChatStatus::Ready(history) = &self.status else {
            self.active = Some(active);
            return;
        };
        let history = history.clone();
        let session_path = restart_session_path(self.controls.as_ref(), &history);
        let cwd = session_path
            .as_deref()
            .and_then(session_cwd)
            .or_else(|| self.composer_cwd.clone())
            .unwrap_or_else(|| history.cwd.clone());
        self.control_operation = Some(ControlOperation::Tools);
        self.rpc_success = None;
        self.rpc_error = None;
        self.begin_active_generation();
        self.active = Some(active.clone());
        let _ = active.restart_with_tools(
            official_binary(),
            session_path,
            cwd,
            (*history).clone(),
            preset,
        );
        cx.notify();
    }

    fn begin_control(
        &mut self,
        operation: ControlOperation,
        request: ControlRequest,
        cx: &mut Context<Self>,
    ) {
        if self.control_busy() {
            return;
        }
        let Some(active) = self.active.clone() else {
            self.rpc_success = None;
            self.rpc_error = Some("请先启动活会话".to_owned());
            cx.notify();
            return;
        };
        if active.snapshot().phase != LivePhase::Idle && operation != ControlOperation::AbortRetry {
            return;
        }
        self.control_operation = Some(operation);
        self.rpc_success = None;
        self.rpc_error = None;
        let _ = active.request_control(operation, request);
        cx.notify();
    }

    fn set_auto_compaction(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.begin_control(
            ControlOperation::AutoCompaction,
            ControlRequest::SetAutoCompaction(enabled),
            cx,
        );
    }

    fn set_auto_retry(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.begin_control(
            ControlOperation::AutoRetry,
            ControlRequest::SetAutoRetry(enabled),
            cx,
        );
    }

    fn abort_retry(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.begin_control(ControlOperation::AbortRetry, ControlRequest::AbortRetry, cx);
    }

    fn fork_message(&mut self, entry_id: String, cx: &mut Context<Self>) {
        // 只允许权威当前分支中的 user entry；预览其它 leaf 时必须先 clone/fork，不能写错 branch。
        let forkable = self.branch_tree.as_ref().is_some_and(|tree| {
            tree.active_path.contains(&entry_id)
                && tree
                    .nodes
                    .iter()
                    .any(|node| node.id == entry_id && node.forkable_user_message.is_some())
        });
        if !forkable {
            self.rpc_success = None;
            self.rpc_error = Some("只能从当前分支的用户消息创建 fork".to_owned());
            cx.notify();
            return;
        }
        self.begin_control(
            ControlOperation::Fork,
            ControlRequest::Fork { entry_id },
            cx,
        );
    }

    fn preview_branch(&mut self, leaf_id: String, cx: &mut Context<Self>) {
        let Some(path) = self
            .controls
            .as_ref()
            .and_then(|controls| controls.session_file.clone())
        else {
            return;
        };
        match pi_render::render_path_at_leaf(&path, &leaf_id) {
            Ok(Some(document)) => {
                self.branch_preview_leaf = Some(leaf_id);
                self.branch_preview_document = Some(Arc::new(document));
                self.rpc_success = None;
                self.rpc_error = None;
            }
            Ok(None) => {
                self.rpc_success = None;
                self.rpc_error = Some("该分支不可安全投影".to_owned());
            }
            Err(error) => {
                self.rpc_success = None;
                self.rpc_error = Some(format!("加载分支预览失败：{error}"));
            }
        }
        cx.notify();
    }

    fn clear_branch_preview(&mut self, cx: &mut Context<Self>) {
        self.branch_preview_leaf = None;
        self.branch_preview_document = None;
        cx.notify();
    }

    fn choose_session_switch(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.control_operation.is_some() || self.active.is_none() {
            return;
        }
        // 原生选择器可能开着好几秒。不钉住标签，用户切一下界面就会把**别人的**
        // 活会话切到这里挑的文件上。
        let tab_id = self.tab_id;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择要切换到的 pi 会话 JSONL".into()),
        });
        cx.spawn_in(window, async move |panel, cx| {
            let path = receiver
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            let Some(path) = path else { return };
            let _ = panel.update(cx, |panel, cx| {
                panel.apply_session_switch_choice(tab_id, path, cx);
            });
        })
        .detach();
    }

    /// 把用户在原生选择器里挑中的会话文件应用到**发起这次切换的那个标签**。
    ///
    /// 单独成一个方法而不是写在续体里：这是选择器回来之后唯一会改状态的地方，
    /// 拎出来才能在没有真实 `SessionHandle` 的测试里直接钉住「落在哪个标签」。
    fn apply_session_switch_choice(&mut self, tab_id: u64, path: PathBuf, cx: &mut Context<Self>) {
        let Some(index) = self.slot_index_for_tab(tab_id) else {
            return;
        };
        // 不能切进另一个标签已经登记的会话文件：那会让**两个** pi 进程绑同一份
        // JSONL，各自往里追加，落盘历史交错甚至写坏。这条路径绕开了侧栏选择时的
        // `slot_index_for_key` 去重，必须自己查一次。
        if let Some(owner) = self
            .sessions
            .iter()
            .enumerate()
            .find(|(other, slot)| {
                *other != index
                    && slot.session.is_some()
                    && slot
                        .bound_session_file()
                        .is_some_and(|owned| same_session_file(&owned, &path))
            })
            .map(|(_, slot)| slot.tab_title.clone())
        {
            self.project(index, |panel| {
                panel.rpc_success = None;
                panel.rpc_error_protected = true;
                panel.rpc_error = Some(format!("该会话已在标签「{owner}」中打开，请直接切过去"));
                cx.notify();
            });
            return;
        }
        self.project_tab(tab_id, |panel| {
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                panel.begin_control(
                    ControlOperation::SwitchSession,
                    ControlRequest::SwitchSession { path },
                    cx,
                );
            } else {
                panel.rpc_success = None;
                panel.rpc_error = Some("只能切换到 .jsonl 会话文件".to_owned());
                cx.notify();
            }
        });
    }

    fn export_html(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.control_operation.is_some() || self.active.is_none() {
            return;
        }
        let start = self
            .composer_cwd
            .clone()
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| PathBuf::from("."));
        let session_id = self
            .controls
            .as_ref()
            .map(|controls| controls.session_id.clone())
            .unwrap_or_else(|| "session".to_owned());
        // 同上：导出的必须是**发起导出的那个会话**，不是选完路径时正看着的那个。
        let tab_id = self.tab_id;
        let receiver =
            cx.prompt_for_new_path(&start, Some(&format!("pi-session-{session_id}.html")));
        cx.spawn_in(window, async move |panel, cx| {
            let destination = receiver.await.ok().into_iter().flatten().flatten().next();
            let Some(destination) = destination else {
                return;
            };
            let _ = panel.update(cx, |panel, cx| {
                panel.project_tab(tab_id, |panel| {
                    panel.begin_control(
                        ControlOperation::ExportHtml,
                        ControlRequest::ExportHtml {
                            output_path: destination,
                        },
                        cx,
                    );
                });
            });
        })
        .detach();
    }

    fn composer_changed(&mut self, input: &gpui::Entity<TextareaState>, cx: &mut Context<Self>) {
        if self.pending_draft_restore {
            self.pending_draft_restore = false;
        }
        self.save_current_draft(cx);
        let input = input.read(cx);
        self.refresh_popup_for_value(input.value().as_ref(), input.cursor());
        cx.notify();
    }

    fn refresh_popup(&mut self, cx: &App) {
        let input = self.composer.read(cx);
        self.refresh_popup_for_value(input.value().as_ref(), input.cursor());
    }

    fn refresh_popup_without_input(&mut self) {
        if let Some(ComposerPopup::Slash(_)) = self.popup {
            self.popup = Some(ComposerPopup::Slash(self.filtered_slash_commands("")));
        }
    }

    fn refresh_popup_for_value(&mut self, value: &str, cursor: usize) {
        let cursor = cursor.min(value.len());
        if cursor == value.len()
            && let Some(query) = slash_query(value)
        {
            self.popup = Some(ComposerPopup::Slash(self.filtered_slash_commands(query)));
            self.popup_index = 0;
            return;
        }
        if self.composer_cwd.is_some()
            && let Some(query) = pi_data::extract_at_query(&value[..cursor])
        {
            let entries = self.file_index.as_ref().map_or_else(Vec::new, |index| {
                pi_data::filter_file_entries(&index.entries, &query.query, pi_data::AT_RESULT_LIMIT)
            });
            self.popup = Some(ComposerPopup::At { query, entries });
            self.popup_index = 0;
            return;
        }
        self.popup = None;
        self.popup_index = 0;
    }

    fn filtered_slash_commands(&self, query: &str) -> Vec<pi_rpc::RpcSlashCommand> {
        let query = query.to_lowercase();
        self.slash_commands
            .iter()
            .filter(|command| {
                command.name.to_lowercase().contains(&query)
                    || command
                        .description
                        .as_deref()
                        .is_some_and(|description| description.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    }

    fn accept_popup(
        &mut self,
        input: &gpui::Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(popup) = self.popup.clone() else {
            return false;
        };
        let (value, cursor) = {
            let state = input.read(cx);
            (state.value().to_string(), state.cursor())
        };
        let (next, next_cursor) = match popup {
            ComposerPopup::Slash(commands) => commands.get(self.popup_index).map(|command| {
                let next = format!("/{} ", command.name);
                let cursor = next.len();
                (next, cursor)
            }),
            ComposerPopup::At { query, entries } => entries
                .get(self.popup_index)
                .map(|entry| pi_data::apply_at_insertion(&value, cursor, &query, entry)),
        }
        .unwrap_or_else(|| (String::new(), 0));
        if next.is_empty() {
            return false;
        }
        input.update(cx, |input, cx| {
            input.set_value(next, window, cx);
            input.set_selected_range(next_cursor..next_cursor, cx);
        });
        let state = input.read(cx);
        self.refresh_popup_for_value(state.value().as_ref(), state.cursor());
        true
    }

    fn composer_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_cycle_model_keystroke(event) && self.can_cycle_model() {
            self.cycle_model(cx);
            cx.stop_propagation();
            return;
        }
        let Some(popup) = &self.popup else {
            return;
        };
        let len = match popup {
            ComposerPopup::Slash(commands) => commands.len(),
            ComposerPopup::At { entries, .. } => entries.len(),
        };
        match event.keystroke.key.as_str() {
            "up" => {
                self.popup_index = self.popup_index.saturating_sub(1);
                cx.stop_propagation();
                cx.notify();
            }
            "down" => {
                if len > 0 {
                    self.popup_index = (self.popup_index + 1).min(len - 1);
                }
                cx.stop_propagation();
                cx.notify();
            }
            "tab" => {
                let input = self.composer.clone();
                self.accept_popup(&input, window, cx);
                cx.stop_propagation();
                cx.notify();
            }
            "escape" => {
                self.popup = None;
                self.popup_index = 0;
                cx.stop_propagation();
                cx.notify();
            }
            _ => {}
        }
    }

    fn submit_composer(
        &mut self,
        input: &gpui::Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rpc_success = None;
        if self.branch_preview_leaf.is_some() {
            self.rpc_error = Some("当前是只读分支预览；返回当前分支后才能发送".to_owned());
            cx.notify();
            return;
        }
        if self.control_operation.is_some() || self.compacting {
            self.rpc_error = Some("会话操作进行中，暂不能发送消息".to_owned());
            cx.notify();
            return;
        }
        let message = input.read(cx).value().trim().to_owned();
        if message.is_empty() && self.attachments.is_empty() {
            return;
        }
        let Some(active) = self.active.clone() else {
            self.rpc_error = Some("请先启动活会话".to_owned());
            cx.notify();
            return;
        };
        let intent = match active.snapshot().phase {
            LivePhase::Stopping => {
                self.rpc_error = Some("正在停止，暂不能发送消息".to_owned());
                cx.notify();
                return;
            }
            LivePhase::Running => match self.composer_mode {
                ComposerMode::Steer => RpcIntent::Steer,
                ComposerMode::FollowUp => RpcIntent::FollowUp,
            },
            LivePhase::Idle | LivePhase::Error => RpcIntent::Prompt,
        };
        let submission = build_submission(message, &self.attachments);
        // R22：命令队列有界，投递可能被拒。此时必须保留草稿与附件供用户重试，
        // 不能像成功路径那样清空输入框。
        if let Err(error) = active.dispatch(intent, Some(submission), self.composer_mode) {
            self.rpc_success = None;
            self.rpc_error = Some(error);
            self.rpc_error_protected = true;
            // 浮层必须一并收起：否则它会继续遮挡 composer，和刚弹出的错误横幅抢注意力。
            self.popup = None;
            cx.notify();
            return;
        }
        input.update(cx, |input, cx| input.set_value("", window, cx));
        self.attachments.clear();
        self.popup = None;
        self.drafts.clear(&self.draft_slot_key());
        cx.notify();
    }

    fn choose_images(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 原生选择器可能开着好几秒；附件该落在**点按钮时**那个标签上。
        let tab_id = self.tab_id;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择图片附件".into()),
        });
        cx.spawn_in(window, async move |panel, cx| {
            let Some(paths) = receiver.await.ok().and_then(Result::ok).flatten() else {
                return;
            };
            let _ = cx.update(|_, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    panel.start_attach_paths(tab_id, paths, cx);
                });
            });
        })
        .detach();
    }

    /// 读盘并把图片挂到**发起这次附件操作的那个标签**上。
    ///
    /// `tab_id` 由调用方在用户动作发生的那一刻捕获：拖拽是当时的前台标签，
    /// 原生选择器则是点「添加图片」时的那个——选择器可能开着好几秒，
    /// 期间用户完全可能切走。
    fn start_attach_paths(&self, tab_id: u64, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(index) = self.slot_index_for_tab(tab_id) else {
            return;
        };
        let generation = self.sessions[index].load_generation;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |panel, cx| {
            let result = executor
                .spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| {
                            std::fs::read(&path)
                                .map_err(|error| format!("{}：{error}", path.display()))
                                .and_then(|bytes| {
                                    pi_data::image_from_bytes(bytes)
                                        .map_err(|error| format!("{}：{error}", path.display()))
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .await;
            let _ = panel.update(cx, |panel, cx| {
                panel.project_tab(tab_id, |panel| {
                    if generation != panel.load_generation {
                        return;
                    }
                    match result {
                        Ok(images) => {
                            if let Err(error) = panel.add_draft_images(images, cx) {
                                panel.rpc_error = Some(error.to_string());
                            }
                        }
                        Err(error) => {
                            panel.rpc_success = None;
                            panel.rpc_error = Some(error);
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn add_draft_images(
        &mut self,
        images: Vec<pi_data::DraftImage>,
        cx: &mut Context<Self>,
    ) -> Result<(), pi_data::ImageValidationError> {
        pi_data::validate_image_batch(self.attachments.len(), &images)?;
        self.attachments
            .extend(images.into_iter().filter_map(attachment_from_draft));
        self.save_current_draft(cx);
        self.rpc_success = None;
        self.rpc_error = None;
        Ok(())
    }

    fn attach_clipboard_images(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let decision = classify_clipboard_paste(item.entries);
        match decision {
            ClipboardPasteDecision::TextOnly => false,
            ClipboardPasteDecision::Images { images, warning } => {
                self.rpc_success = None;
                let result = self.add_draft_images(images, cx);
                self.rpc_error = clipboard_image_add_feedback(result, warning);
                true
            }
            ClipboardPasteDecision::ImageError { message, has_text } => {
                self.rpc_success = None;
                self.rpc_error = Some(message);
                !has_text
            }
        }
    }

    fn capture_composer_paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if self.attach_clipboard_images(cx) {
            cx.stop_propagation();
            cx.notify();
        } else {
            // 纯文本或“图片全失败但含文本”必须继续交给 Textarea 的原生 Paste。
            cx.propagate();
        }
    }

    fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
            self.save_current_draft(cx);
            cx.notify();
        }
    }

    fn abort(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = self.active.clone()
            && active.snapshot().phase == LivePhase::Running
        {
            if let Err(error) = active.dispatch(RpcIntent::Abort, None, self.composer_mode) {
                self.rpc_error = Some(format!("停止失败：{error}"));
                self.rpc_error_protected = true;
            }
            cx.notify();
        }
    }

    /// ToggleGroup 回调：`checks` 是点击后每个 toggle 的新勾选状态。
    ///
    /// 语义仍是单选——点已选中的那个会把它翻成 false，此时保持当前模式不变，
    /// 免得出现「两个模式都没选中」的空档。
    fn select_mode(&mut self, checks: &[bool], cx: &mut Context<Self>) {
        let Some(next) = next_composer_mode(checks, self.composer_mode) else {
            return;
        };
        if self.composer_mode != next {
            self.composer_mode = next;
            cx.notify();
        }
    }

    fn resume_follow(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.tail_attached = true;
        self.follow_requested = false;
        self.list_state.scroll_to_end();
        self.list_state.set_follow_mode(FollowMode::Tail);
        window.refresh();
        cx.notify();
    }

    fn sync_list_document(&mut self, document: &ConversationDocument, settled: bool) {
        let next_items = document
            .items
            .iter()
            .map(ListItemSnapshot::from_item)
            .collect::<Vec<_>>();
        let old_len = self.list_items.len();
        let shared_prefix = self
            .list_items
            .iter()
            .zip(&next_items)
            .take_while(|(old, new)| old.same_identity(new))
            .count();
        let structure_changed =
            old_len != next_items.len() || shared_prefix < old_len.min(next_items.len());

        if old_len == 0 {
            self.list_state.reset(next_items.len());
        } else if structure_changed {
            self.list_state.splice(
                shared_prefix..old_len,
                next_items.len().saturating_sub(shared_prefix),
            );
        }
        for (index, (old, new)) in self.list_items.iter().zip(&next_items).enumerate() {
            if old.same_identity(new)
                && (old.content_identity != new.content_identity
                    || old.collapsible != new.collapsible)
            {
                self.list_state.splice(index..index + 1, 1);
            }
        }
        self.list_items = next_items;

        if (settled || structure_changed) && self.list_state.is_scrolled_to_end().is_none() {
            // settled/结构变化后只补齐一次离屏 unknown，流式同项更新不做周期全表测量。
            let _ = self.list_state.clone().measure_all();
        }
    }

    fn toggle_minimap(&mut self, cx: &mut Context<Self>) {
        self.minimap_visible = !self.minimap_visible;
        cx.notify();
    }

    fn update_workspace_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.workspace_bounds != Some(bounds) {
            self.workspace_bounds = Some(bounds);
            cx.notify();
        }
    }

    fn update_message_pane_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.message_pane_bounds != Some(bounds) {
            self.message_pane_bounds = Some(bounds);
            cx.notify();
        }
    }

    fn toggle_tool(&mut self, key: String, item_id: String, cx: &mut Context<Self>) {
        let anchor = if let ChatStatus::Ready(document) = &self.status {
            document.items.iter().position(|item| match item {
                ConversationItem::Message(message) => message.id == item_id,
                ConversationItem::Process(group) => {
                    group.messages.iter().any(|message| message.id == item_id)
                }
            })
        } else {
            None
        };
        let logical_anchor = self.list_state.logical_scroll_top();
        let was_tailing = self.tail_attached
            && (self.list_state.is_following_tail()
                || self.list_state.is_scrolled_to_end().unwrap_or(true));
        if !was_tailing {
            self.list_state.pause_following_tail();
            self.tail_attached = false;
        }
        if !self.expanded_tools.insert(key.clone()) {
            self.expanded_tools.remove(&key);
        }
        if let Some(index) = anchor {
            self.list_state.splice(index..index + 1, 1);
            // splice 命中逻辑顶项时会把 offset 重置为 0；立刻恢复原 ListOffset，
            // 不依赖下一帧测量即可保住用户正在阅读的视口锚点。
            if !was_tailing {
                self.list_state.scroll_to(logical_anchor);
            }
        }
        cx.notify();
    }

    fn toggle_process(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.expanded_processes.insert(key.clone()) {
            self.expanded_processes.remove(&key);
        }
        if let Some(index) = self.list_items.iter().position(|item| item.id == key) {
            self.list_state.splice(index..index + 1, 1);
        }
        cx.notify();
    }

    fn render_composer_popup(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let popup = self.popup.as_ref()?;
        let rows = match popup {
            ComposerPopup::Slash(commands) => commands
                .iter()
                .enumerate()
                .map(|(index, command)| {
                    let description = command.description.clone().unwrap_or_default();
                    h_flex()
                        .debug_selector(|| "composer-popup-item".into())
                        .gap_2()
                        .px_2()
                        .py_1()
                        .when(index == self.popup_index, |row| {
                            row.bg(cx.theme().secondary_active)
                        })
                        .child(div().font_semibold().child(format!("/{}", command.name)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(cx.theme().muted_foreground)
                                .child(description),
                        )
                        .into_any_element()
                })
                .collect::<Vec<_>>(),
            ComposerPopup::At { entries, .. } => entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    h_flex()
                        .debug_selector(|| "composer-popup-item".into())
                        .gap_2()
                        .px_2()
                        .py_1()
                        .when(index == self.popup_index, |row| {
                            row.bg(cx.theme().secondary_active)
                        })
                        .child(Icon::new(if entry.is_dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        }))
                        .child(entry.path.clone())
                        .into_any_element()
                })
                .collect::<Vec<_>>(),
        };
        Some(
            v_flex()
                .debug_selector(|| "composer-popup".into())
                .max_h(px(180.))
                .overflow_y_scrollbar()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_attachments(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.attachments.is_empty() {
            return None;
        }
        let panel = cx.entity();
        Some(
            h_flex()
                .debug_selector(|| "composer-attachments".into())
                .gap_2()
                .overflow_x_scrollbar()
                .children(
                    self.attachments
                        .iter()
                        .enumerate()
                        .map(|(index, attachment)| {
                            h_flex()
                                .id(("attachment", index))
                                .gap_1()
                                .p_1()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().border)
                                .child(img(attachment.preview.clone()).size(px(56.)))
                                .child(
                                    Button::new(("remove-attachment", index))
                                        .xsmall()
                                        .label("删除")
                                        .on_click({
                                            let panel = panel.clone();
                                            move |_, _, cx| {
                                                panel.update(cx, |panel, cx| {
                                                    panel.remove_attachment(index, cx);
                                                });
                                            }
                                        }),
                                )
                        }),
                )
                .into_any_element(),
        )
    }
}

impl EventEmitter<PanelEvent> for ChatPanel {}
impl EventEmitter<crate::main_panel::OpenFileRequest> for ChatPanel {}
impl EventEmitter<SessionsChanged> for ChatPanel {}
impl EventEmitter<FocusedSessionChanged> for ChatPanel {}

impl Focusable for ChatPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for ChatPanel {
    fn panel_name(&self) -> &'static str {
        "gpui-pi-chat"
    }

    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some("对话".into())
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "对话"
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> Option<PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for ChatPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 防御性重置：投影游标只该在 `project` 内部临时偏离，渲染永远看前台标签。
        // 万一有哪条路径漏了还原，这一行让它在下一帧自愈，而不是把 A 的会话画成 B。
        self.cursor = self.focused;
        #[cfg(test)]
        let probe = self.probe.clone();
        #[cfg(not(test))]
        let probe = self.probe;
        if self.tail_attached && matches!(self.list_state.is_scrolled_to_end(), Some(false)) {
            // 覆盖滚动条拖拽、PageUp/Home 等路径。
            self.tail_attached = false;
        }
        if self.follow_requested && self.tail_attached {
            self.list_state.scroll_to_end();
        }
        self.follow_requested = false;
        let panel = cx.entity();
        let model_panel = panel.clone();
        let thinking_panel = panel.clone();
        let tools_panel = panel.clone();
        let visible_status = self
            .branch_preview_document
            .as_ref()
            .map(|document| ChatStatus::Ready(document.clone()))
            .unwrap_or_else(|| self.status.clone());
        let content = match &visible_status {
            ChatStatus::Empty => centered_state(
                IconName::Bot,
                "选择一个历史会话",
                "加载历史后可启动对应的官方 pi RPC 活会话",
                cx,
            ),
            ChatStatus::Loading { title } => {
                centered_state(IconName::LoaderCircle, "正在后台加载历史会话…", title, cx)
            }
            ChatStatus::Error { title, message } => v_flex()
                .debug_selector(|| "chat-error".into())
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .text_color(cx.theme().danger)
                .child(div().font_semibold().child(format!("{title} 加载失败")))
                .child(div().text_sm().child(message.clone()))
                .into_any_element(),
            ChatStatus::Ready(document) => {
                let tail_panel = cx.entity();
                let written_source_root = document.cwd.clone();
                gpui_pi_ui::ChatWindow::new(document.clone(), self.list_state.clone())
                    .on_open_written_file({
                        let panel = cx.entity();
                        move |relative_path, cx| {
                            let source_root = written_source_root.clone();
                            panel.update(cx, |_, cx| {
                                cx.emit(crate::main_panel::OpenFileRequest {
                                    source_root,
                                    relative_path,
                                });
                            });
                        }
                    })
                    .model_names(self.model_names.clone())
                    .show_minimap(self.minimap_visible)
                    .expanded_tools(Arc::new(self.expanded_tools.clone()))
                    .expanded_processes(Arc::new(self.expanded_processes.clone()))
                    .on_toggle_tool({
                        let panel = cx.entity();
                        move |key, item_id, cx| {
                            panel.update(cx, |panel, cx| {
                                panel.toggle_tool(key, item_id, cx);
                            });
                        }
                    })
                    .on_toggle_process({
                        let panel = cx.entity();
                        move |key, cx| {
                            panel.update(cx, |panel, cx| panel.toggle_process(key, cx));
                        }
                    })
                    .on_fork_message({
                        let panel = cx.entity();
                        move |entry_id, cx| {
                            panel.update(cx, |panel, cx| panel.fork_message(entry_id, cx));
                        }
                    })
                    .on_toggle_minimap({
                        let panel = cx.entity();
                        move |cx| {
                            panel.update(cx, |panel, cx| panel.toggle_minimap(cx));
                        }
                    })
                    .on_message_pane_bounds({
                        let panel = cx.entity();
                        move |bounds, cx| {
                            panel.update(cx, |panel, cx| {
                                panel.update_message_pane_bounds(bounds, cx)
                            });
                        }
                    })
                    .on_tail_attachment_change(move |attached, _, cx| {
                        tail_panel.update(cx, |panel, cx| {
                            panel.tail_attached = attached;
                            if !attached {
                                panel.list_state.pause_following_tail();
                            }
                            cx.notify();
                        });
                    })
                    .on_tail_detach({
                        let panel = cx.entity();
                        move |_, cx| {
                            panel.update(cx, |panel, cx| {
                                panel.tail_attached = false;
                                panel.list_state.pause_following_tail();
                                cx.notify();
                            });
                        }
                    })
                    .into_any_element()
            }
        };
        let composer_pane_layout = self
            .workspace_bounds
            .zip(self.message_pane_bounds)
            .map(|(workspace, pane)| (pane.origin.x - workspace.origin.x, pane.size.width));
        let phase = self.active.as_ref().map(|active| active.snapshot().phase);
        let running = matches!(phase, Some(LivePhase::Running));
        let stopping = matches!(phase, Some(LivePhase::Stopping));
        let live_started = self.active.is_some();
        let select_tab_panel = cx.entity();
        let close_tab_panel = cx.entity();
        let tab_items = self.tab_items();
        let show_session_tabs =
            self.sessions.len() > 1 || self.sessions.iter().any(|slot| slot.session.is_some());
        let session_state_note = self.session_state_note(cx);
        // 会话已登记但没有进程时，主操作是「恢复运行」而不是「启动活会话」。
        // 排队中的会话轮到就会**自动**启动：给它一个可点的「恢复运行」是在暗示
        // 用户必须做点什么，而切到这个标签本身已经把它的优先级抬到前台了。
        let session_registered = self.session.is_some();
        let session_queued = self.scheduler_state == Some(pi_runtime::SchedulerState::Queued);
        let (start_label, start_tooltip) = start_action_copy(session_queued, session_registered);
        // 后台还有调度作业没落地、或上一次控制请求还没回来时，所有会碰进程的入口
        // 一律不接受点击：重复点击会叠出第二次进程操作，而第一次的结果还没回来。
        // 判据只有 [`SessionUiState::control_busy`] 这一个，按钮与 handler 共用。
        let control_busy = self.control_busy();
        let can_park = live_started
            && !control_busy
            && self
                .active
                .as_ref()
                .is_some_and(SessionHandle::is_quiescent);
        if self.pending_draft_restore {
            let input = self.composer.clone();
            let text = self.drafts.get(&self.draft_slot_key()).text;
            input.update(cx, |input, cx| input.set_value(text, window, cx));
            self.pending_draft_restore = false;
        }
        let popup = self.render_composer_popup(cx);
        let attachments = self.render_attachments(cx);
        let queue_summary = self.active.as_ref().and_then(|active| {
            let snapshot = active.snapshot();
            let steering = snapshot.steering_queue_len;
            let follow_up = snapshot.follow_up_queue_len;
            (steering + follow_up > 0)
                .then(|| format!("队列：steer {steering} · follow-up {follow_up}"))
        });
        if self.rpc_error.is_some() {
            // 错误反馈优先于较早的成功反馈，禁止绿红两条同时出现。
            self.rpc_success = None;
        }
        let controls_enabled = session_controls_enabled(phase, control_busy);
        let tools_enabled =
            !running && !stopping && !control_busy && matches!(self.status, ChatStatus::Ready(_));
        let current_model = self
            .controls
            .as_ref()
            .and_then(|controls| controls.model.as_ref())
            .map_or_else(|| "模型".to_owned(), |model| model.name.clone());
        let current_thinking = self.controls.as_ref().map_or_else(
            || "Thinking".to_owned(),
            |controls| thinking_label(controls.thinking_level).to_owned(),
        );
        let models = self
            .controls
            .as_ref()
            .map(|controls| controls.models.clone())
            .unwrap_or_default();
        let thinking_levels = self
            .controls
            .as_ref()
            .map(|controls| controls.thinking_levels.clone())
            .unwrap_or_default();
        let current_model_ref = self
            .controls
            .as_ref()
            .and_then(|controls| controls.model.as_ref())
            .map(|model| (model.provider.clone(), model.id.clone()));
        let current_thinking_level = self
            .controls
            .as_ref()
            .map(|controls| controls.thinking_level);
        let selected_tool_preset = self.tool_preset;

        let above_widgets = self
            .extension_ui
            .widgets(pi_rpc::WidgetPlacement::AboveEditor)
            .cloned()
            .collect::<Vec<_>>();
        let below_widgets = self
            .extension_ui
            .widgets(pi_rpc::WidgetPlacement::BelowEditor)
            .cloned()
            .collect::<Vec<_>>();
        let custom_ui_capability = self.extension_ui.custom_ui_capability();
        let show_custom_ui_capability = self.extension_ui.has_seen_extension_ui();
        let statuses = self.extension_ui.statuses().cloned().collect::<Vec<_>>();
        // S-8：StatusBar 一行最多 3 个独立文本节点。能力与 overflow 都先占额度，
        // 剩余额度才分配给状态，避免“状态上限 + overflow”实际渲染出第 4 段。
        let status_node_budget = 3usize - usize::from(show_custom_ui_capability);
        let status_visible_limit = if statuses.len() > status_node_budget {
            status_node_budget.saturating_sub(1)
        } else {
            statuses.len()
        };
        let hidden_status_count = statuses.len().saturating_sub(status_visible_limit);
        let hidden_status_tooltip = statuses
            .iter()
            .skip(status_visible_limit)
            .map(|status| format!("{}: {}", status.display_key, status.text))
            .collect::<Vec<_>>()
            .join("\n");
        let visible_statuses = statuses
            .iter()
            .take(status_visible_limit)
            .cloned()
            .collect::<Vec<_>>();
        let composer_focused = self.composer.focus_handle(cx).is_focused(window);
        let composer_shell_style = composer_input_shell_style(composer_focused, cx);
        let auto_compaction = self
            .controls
            .as_ref()
            .is_some_and(|controls| controls.auto_compaction_enabled);
        let auto_retry = self
            .controls
            .as_ref()
            .is_some_and(|controls| controls.auto_retry_enabled);
        let branch_nodes = self
            .branch_tree
            .as_ref()
            .map(|tree| tree.nodes.clone())
            .unwrap_or_default();
        let active_path = self
            .branch_tree
            .as_ref()
            .map(|tree| tree.active_path.clone())
            .unwrap_or_default();
        let selected_preview_leaf = self.branch_preview_leaf.clone();
        let branch_panel = panel.clone();
        let branch_trigger = Button::new("branch-navigator-trigger")
            .debug_selector(|| "branch-navigator-trigger".into())
            .ghost()
            .small()
            .icon(IconName::Copy)
            .label("分支")
            .tooltip("查看会话分支树")
            .disabled(!live_started || branch_nodes.is_empty());
        let branch_popover = Popover::new("branch-navigator")
            .anchor(Anchor::TopLeft)
            .trigger(branch_trigger)
            .content(move |_, _, cx| {
                let rows = if branch_nodes.is_empty() {
                    vec![
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("当前会话没有可显示的分支")
                            .into_any_element(),
                    ]
                } else {
                    branch_nodes
                        .iter()
                        .map(|node| {
                            let id = node.id.clone();
                            let panel = branch_panel.clone();
                            let active = active_path.contains(&node.id);
                            let selected = selected_preview_leaf.as_deref() == Some(&node.id);
                            let label = node.label.clone().unwrap_or_else(|| node.preview.clone());
                            h_flex()
                                .id(SharedString::from(format!("branch-node-{}", node.id)))
                                .debug_selector(|| "branch-node".into())
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .pl(px(node.depth.saturating_mul(12) as f32))
                                .pr_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .when(selected, |row| row.bg(cx.theme().accent.opacity(0.16)))
                                .hover(|row| row.bg(cx.theme().muted))
                                .child(div().size_2().flex_none().rounded_full().bg(if active {
                                    cx.theme().accent
                                } else {
                                    cx.theme().border
                                }))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(if active {
                                            cx.theme().foreground
                                        } else {
                                            cx.theme().muted_foreground
                                        })
                                        .child(label),
                                )
                                .on_click(move |_, _, cx| {
                                    panel.update(cx, |panel, cx| {
                                        panel.preview_branch(id.clone(), cx)
                                    });
                                })
                                .into_any_element()
                        })
                        .collect()
                };
                v_flex()
                    .debug_selector(|| "branch-navigator-content".into())
                    // 使用 GPUI 既有尺寸刻度，不自造 popover 像素尺寸（规范红线 4）。
                    .w_80()
                    .max_h_64()
                    .overflow_y_scrollbar()
                    .gap_1()
                    .children(rows)
            });

        let workspace_panel = cx.entity();
        div()
            .id("chat-workspace")
            .debug_selector(|| "chat-workspace".into())
            .on_prepaint(move |bounds, _, cx| {
                workspace_panel.update(cx, |panel, cx| {
                    panel.update_workspace_bounds(bounds, cx)
                });
            })
            .when_some(probe, |this, probe| {
                this.on_prepaint(move |bounds, _, _| probe.record_workspace(bounds))
            })
            .track_focus(&self.focus_handle)
            // Paste 是 Textarea 自己消费的 action；必须在祖先 capture 阶段先分流图片。
            .capture_action(cx.listener(Self::capture_composer_paste))
            .on_key_down(cx.listener(Self::composer_key_down))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let tab_id = this.tab_id;
                this.start_attach_paths(tab_id, paths.paths().to_vec(), cx);
            }))
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .size_full()
                    .min_h_0()
                    // 纯历史预览（一个标签、还没登记会话）不画标签条：那条横条不承载
                    // 任何信息，却要占掉一行消息区。一旦有了活会话就必须画 —— 状态点是
                    // 用户唯一能看到会话在不在跑的地方，关闭入口也只在标签上。
                    .when(show_session_tabs, |view| {
                        view.child(
                            gpui_pi_ui::SessionTabs::new(tab_items, self.focused)
                                .on_select(move |index, window, cx| {
                                    select_tab_panel.update(cx, |panel, cx| {
                                        panel.focus_tab(index, window, cx)
                                    });
                                })
                                .on_close(move |index, window, cx| {
                                    close_tab_panel.update(cx, |panel, cx| {
                                        panel.close_tab(index, window, cx)
                                    });
                                }),
                        )
                    })
                    .child(div().flex_1().min_h_0().child(content))
                    .when_some(session_state_note, |view, note| {
                        view.child(
                            h_flex()
                                .debug_selector(|| "session-state-note".into())
                                .gap_2()
                                .px_3()
                                .py_1()
                                .child(div().size_2().flex_none().rounded_full().bg(note.dot))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(note.text),
                                ),
                        )
                    })
                    .when(!self.tail_attached, |view| {
                        view.child(
                            h_flex().justify_center().child(
                                Button::new("follow-latest")
                                    .debug_selector(|| "follow-latest".into())
                                    .small()
                                    .label("跟随最新")
                                    .on_click(cx.listener(Self::resume_follow)),
                            ),
                        )
                    })
                    .when_some(queue_summary, |view, summary| {
                        view.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(summary),
                        )
                    })
                    .when_some(self.branch_preview_leaf.clone(), |view, leaf| {
                        view.child(
                            h_flex()
                                .debug_selector(|| "branch-preview-banner".into())
                                .gap_2()
                                .px_3()
                                .py_1()
                                .bg(cx.theme().accent.opacity(0.16))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .child(format!("只读分支预览：{leaf}；发送已禁用")),
                                )
                                .child(
                                    Button::new("close-branch-preview")
                                        .ghost()
                                        .small()
                                        .label("返回当前分支")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.clear_branch_preview(cx)
                                        })),
                                ),
                        )
                    })
                    .when_some(self.retry_status.clone(), |view, retry| {
                        view.child(
                            h_flex()
                                .debug_selector(|| "auto-retry-status".into())
                                .gap_2()
                                .px_3()
                                .py_1()
                                .child(div().size_2().rounded_full().bg(cx.theme().warning))
                                .child(
                                    div().flex_1().text_xs().child(format!(
                                        "Auto-retry {}/{} · {}ms · {}",
                                        retry.attempt, retry.max_attempts, retry.delay_ms, retry.error
                                    )),
                                )
                                .child(
                                    Button::new("abort-retry")
                                        .ghost()
                                        .small()
                                        .label("取消重试")
                                        .disabled(abort_retry_disabled(control_busy))
                                        .on_click(cx.listener(Self::abort_retry)),
                                ),
                        )
                    })
                    .when(self.compacting, |view| {
                        view.child(
                            h_flex()
                                .debug_selector(|| "compaction-status".into())
                                .gap_2()
                                .px_3()
                                .py_1()
                                .child(div().size_2().rounded_full().bg(cx.theme().warning))
                                .child(
                                    div()
                                        .text_xs()
                                        .child("正在压缩上下文；官方 RPC 无取消命令"),
                                ),
                        )
                    })
                    .when_some(self.rpc_success.clone(), |view, success| {
                        view.child(
                            div()
                                .debug_selector(|| "live-success".into())
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(cx.theme().success)
                                .child(success),
                        )
                    })
                    .when_some(
                        self.host_extension_degradation.clone(),
                        |view, diagnostic| {
                            view.child(
                                div()
                                    .debug_selector(|| "host-extension-degradation".into())
                                    .px_3()
                                    .py_1()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child(diagnostic),
                            )
                        },
                    )
                    .when_some(self.backpressure_note.clone(), |view, note| {
                        view.child(
                            div()
                                .debug_selector(|| "backpressure-note".into())
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child(note),
                        )
                    })
                    .when_some(self.rpc_error.clone(), |view, error| {
                        view.child(
                            div()
                                .debug_selector(|| "live-error".into())
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(error),
                        )
                    })
                    .when(!above_widgets.is_empty(), |view| {
                        view.child(render_extension_widgets(
                            "extension-widgets-above",
                            &above_widgets,
                            &self.extension_widgets_above_scroll,
                            cx,
                        ))
                    })
                    .child(
                        v_flex()
                            .debug_selector(|| "live-composer".into())
                            .flex_none()
                            .w_full()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            // 通栏只承担顶边框与背景，内容列与消息列同宽同轴。
                            .bg(cx.theme().background)
                            .child(
                                h_flex()
                                    .debug_selector(|| "composer-column-outer".into())
                                    .w_full()
                                    .when_some(composer_pane_layout, |column, (inset, width)| {
                                        column.ml(inset).w(width)
                                    })
                                    .px_4()
                                    .justify_center()
                                    .child(
                                        v_flex()
                                            .debug_selector(|| "composer-content-column".into())
                                            .flex_none()
                                            .w_full()
                                            .min_w_0()
                                            // 与 chat.rs message-column 共享 S-13 的 820px 上限。
                                            .max_w(px(820.))
                                            .gap_2()
                                            .py_2()
                                    .when_some(popup, |composer, popup| composer.child(popup))
                            .when_some(attachments, |composer, attachments| {
                                composer.child(attachments)
                            })
                            .child(

                                h_flex()
                                    .debug_selector(|| "r13-session-controls".into())
                                    .gap_2()
                                    .child(branch_popover)
                                    .child(
                                        Button::new("session-actions")
                                            .debug_selector(|| "session-actions".into())
                                            .ghost()
                                            .small()
                                            .label("会话")
                                            .tooltip("会话分支、压缩与切换")
                                            .disabled(!live_started)
                                            .dropdown_menu({
                                                let panel = panel.clone();
                                                move |menu, _, _| {
                                                    let clone_panel = panel.clone();
                                                    let compact_panel = panel.clone();
                                                    let auto_compaction_panel = panel.clone();
                                                    let auto_retry_panel = panel.clone();
                                                    let switch_panel = panel.clone();
                                                    menu.item(
                                                        PopupMenuItem::new("Clone 当前分支")
                                                            .on_click(move |_, _, cx| {
                                                                clone_panel.update(cx, |panel, cx| {
                                                                    panel.begin_control(
                                                                        ControlOperation::Clone,
                                                                        ControlRequest::Clone,
                                                                        cx,
                                                                    );
                                                                });
                                                            }),
                                                    )
                                                    .item(
                                                        PopupMenuItem::new("手动 Compact")
                                                            .on_click(move |_, _, cx| {
                                                                compact_panel.update(cx, |panel, cx| {
                                                                    panel.begin_control(
                                                                        ControlOperation::Compact,
                                                                        ControlRequest::Compact,
                                                                        cx,
                                                                    );
                                                                });
                                                            }),
                                                    )
                                                    .item(
                                                        PopupMenuItem::new("Auto-compaction")
                                                            .checked(auto_compaction)
                                                            .on_click(move |_, _, cx| {
                                                                auto_compaction_panel.update(cx, |panel, cx| {
                                                                    panel.set_auto_compaction(!auto_compaction, cx);
                                                                });
                                                            }),
                                                    )
                                                    .item(
                                                        PopupMenuItem::new("Auto-retry")
                                                            .checked(auto_retry)
                                                            .on_click(move |_, _, cx| {
                                                                auto_retry_panel.update(cx, |panel, cx| {
                                                                    panel.set_auto_retry(!auto_retry, cx);
                                                                });
                                                            }),
                                                    )
                                                    .item(
                                                        PopupMenuItem::new("切换会话文件")
                                                            .on_click(move |_, window, cx| {
                                                                switch_panel.update(cx, |panel, cx| {
                                                                    panel.choose_session_switch(
                                                                        &gpui::ClickEvent::default(),
                                                                        window,
                                                                        cx,
                                                                    );
                                                                });
                                                            }),
                                                    )
                                                }
                                            }),
                                    )
                                    .child(
                                        Button::new("export-html")
                                            .debug_selector(|| "export-html".into())
                                            .ghost()
                                            .small()
                                            .icon(IconName::Copy)
                                            .label("导出")
                                            .tooltip("导出当前会话 HTML")
                                            .disabled(!controls_enabled)
                                            .on_click(cx.listener(Self::export_html)),
                                    ),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "composer-textarea-viewport".into())
                                    // 输入壳是独立纵向行；附件增加高度时应由消息画布让位，
                                    // 不能让此 viewport 收缩后由 flex_none 子壳溢出覆盖操作行。
                                    .flex_none()
                                    .child(
                                        div()
                                            .debug_selector(|| "composer-textarea-control".into())
                                            .flex_none()
                                            .min_h_0()
                                            .rounded_xl()
                                            // Textarea 关闭 appearance 后不再自带表面；壳体显式使用
                                            // editor token，避免透明层与 Windows 阴影混成灰色糊块。
                                            .bg(composer_shell_style.background)
                                            .when(composer_shell_style.border_layers == 1, |shell| {
                                                shell.border_1()
                                            })
                                            .border_color(composer_shell_style.border)
                                            .when(composer_shell_style.shadow_layers == 1, |shell| {
                                                shell.shadow_sm()
                                            })
                                            .overflow_hidden()
                                            .child(
                                                Textarea::new(&self.composer)
                                                    .appearance(false)
                                                    .bordered(false)
                                                    .w_full(),
                                            ),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .debug_selector(|| "composer-actions".into())
                                    .flex_none()
                                    .gap_2()
                                    // 左：附件与上下文控件，一律 ghost + small（规范 5.6）。
                                    .child(
                                        Button::new("attach-images")
                                            .debug_selector(|| "attach-images".into())
                                            .ghost()
                                            .small()
                                            .label("添加图片")
                                            .tooltip("添加图片附件")
                                            .disabled(
                                                self.attachments.len()
                                                    >= pi_data::MAX_ATTACHED_IMAGES,
                                            )
                                            .on_click(cx.listener(Self::choose_images)),
                                    )
                                    // 一个按钮承担「起 / 停」两个方向：会话没有进程时它是
                                    // 启动入口，有进程时它是挂起入口。两个常驻按钮会让
                                    // composer 操作行长期多一格，而其中一格永远是灰的。
                                    .when(!live_started, |actions| {
                                        actions.child(
                                            Button::new("start-live-session")
                                                .debug_selector(|| "start-live-session".into())
                                                .ghost()
                                                .small()
                                                .label(start_label)
                                                .tooltip(start_tooltip)
                                                .disabled(
                                                    session_queued
                                                        || control_busy
                                                        || !matches!(
                                                            self.status,
                                                            ChatStatus::Ready(_)
                                                        ),
                                                )
                                                .on_click(cx.listener(Self::start_live)),
                                        )
                                    })
                                    .when(live_started, |actions| {
                                        actions.child(
                                            Button::new("park-live-session")
                                                .debug_selector(|| "park-live-session".into())
                                                .ghost()
                                                .small()
                                                .label("挂起会话")
                                                .tooltip(if can_park {
                                                    "让出 pi 进程；会话与对话都保留，可随时恢复"
                                                } else {
                                                    // BACKLOG #22：不静止时挂起会退化成优雅停机，
                                                    // 下次恢复要多付一次冷启动。这里如实说明。
                                                    "会话正忙；等它空下来再挂起可省一次冷启动"
                                                })
                                                .disabled(!can_park)
                                                .on_click(cx.listener(Self::park_active_session)),
                                        )
                                    })
                                    .child(
                                        Button::new("model-selector")
                                            .debug_selector(|| "model-selector".into())
                                            .ghost()
                                            .small()
                                            .label(
                                                if self.control_operation
                                                    == Some(ControlOperation::Model)
                                                {
                                                    "切换中…".to_owned()
                                                } else {
                                                    current_model
                                                },
                                            )
                                            .tooltip("切换模型；Ctrl+P 循环")
                                            .disabled(!controls_enabled || models.is_empty())
                                            .dropdown_menu(move |mut menu, _, _| {
                                                for model in &models {
                                                    let provider = model.provider.clone();
                                                    let model_id = model.id.clone();
                                                    let panel = model_panel.clone();
                                                    let selected = current_model_ref
                                                        .as_ref()
                                                        .is_some_and(|current| {
                                                            current.0 == provider
                                                                && current.1 == model_id
                                                        });
                                                    menu = menu.item(
                                                        PopupMenuItem::new(model.name.clone())
                                                            .checked(selected)
                                                            .on_click(move |_, _, cx| {
                                                                panel.update(cx, |panel, cx| {
                                                                    panel.set_model(
                                                                        provider.clone(),
                                                                        model_id.clone(),
                                                                        cx,
                                                                    );
                                                                });
                                                            }),
                                                    );
                                                }
                                                menu.scrollable(true)
                                            }),
                                    )
                                    .child(
                                        Button::new("thinking-selector")
                                            .debug_selector(|| "thinking-selector".into())
                                            .ghost()
                                            .small()
                                            .label(
                                                if self.control_operation
                                                    == Some(ControlOperation::Thinking)
                                                {
                                                    "切换中…".to_owned()
                                                } else {
                                                    current_thinking
                                                },
                                            )
                                            .tooltip("切换思考级别")
                                            .disabled(
                                                !controls_enabled || thinking_levels.is_empty(),
                                            )
                                            .dropdown_menu(move |mut menu, _, _| {
                                                for level in &thinking_levels {
                                                    let level = *level;
                                                    let panel = thinking_panel.clone();
                                                    menu = menu.item(
                                                        PopupMenuItem::new(thinking_label(level))
                                                            .checked(
                                                                current_thinking_level
                                                                    == Some(level),
                                                            )
                                                            .on_click(move |_, _, cx| {
                                                                panel.update(cx, |panel, cx| {
                                                                    panel.set_thinking(level, cx);
                                                                });
                                                            }),
                                                    );
                                                }
                                                menu
                                            }),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Button::new("tools-selector")
                                            .debug_selector(|| "tools-selector".into())
                                            .ghost()
                                            .small()
                                            .label(
                                                if self.control_operation
                                                    == Some(ControlOperation::Tools)
                                                {
                                                    "工具重启中…".to_owned()
                                                } else {
                                                    format!("工具：{}", self.tool_preset.label())
                                                },
                                            )
                                            .tooltip("切换工具预设；会重启活会话")
                                            .disabled(!tools_enabled)
                                            .dropdown_menu(move |mut menu, _, _| {
                                                for preset in ToolPreset::ALL {
                                                    let panel = tools_panel.clone();
                                                    menu = menu.item(
                                                        PopupMenuItem::new(format!(
                                                            "{} · {}",
                                                            preset.label(),
                                                            preset.description()
                                                        ))
                                                        .checked(selected_tool_preset == preset)
                                                        .on_click(move |_, _, cx| {
                                                            panel.update(cx, |panel, cx| {
                                                                panel.set_tool_preset(preset, cx);
                                                            });
                                                        }),
                                                    );
                                                }
                                                menu
                                            }),
                                    )
                                    // 右：模式切换 → 停止（仅运行态）→ 发送（唯一常驻主操作）。
                                    .child(
                                        div()
                                            .debug_selector(|| "composer-mode-toggle".into())
                                            .child(
                                                ToggleGroup::new("composer-mode")
                                                    .small()
                                                    .outline()
                                                    .segmented()
                                                    .child(
                                                        Toggle::new("composer-steer")
                                                            .checked(
                                                                self.composer_mode
                                                                    == ComposerMode::Steer,
                                                            )
                                                            // Toggle 本身不是 InteractiveElement，
                                                            // 标签包一层 div 才能给测试留下可定位的选择器。
                                                            .child(
                                                                div()
                                                                    .debug_selector(|| {
                                                                        "composer-mode-steer".into()
                                                                    })
                                                                    .child("Steer"),
                                                            ),
                                                    )
                                                    .child(
                                                        Toggle::new("composer-follow-up")
                                                            .checked(
                                                                self.composer_mode
                                                                    == ComposerMode::FollowUp,
                                                            )
                                                            .child(
                                                                div()
                                                                    .debug_selector(|| {
                                                                        "composer-mode-follow-up"
                                                                            .into()
                                                                    })
                                                                    .child("Follow-up"),
                                                            ),
                                                    )
                                                    .on_click(cx.listener(
                                                        |this, checks: &Vec<bool>, _, cx| {
                                                            this.select_mode(checks, cx);
                                                        },
                                                    )),
                                            ),
                                    )
                                    // 停止是运行态才有意义的破坏性操作，空闲时不占位（规范 1.4）。
                                    .when(running || stopping, |row| {
                                        row.child(
                                            Button::new("abort-live")
                                                .debug_selector(|| "abort-live".into())
                                                .ghost()
                                                .small()
                                                .danger()
                                                .label(if stopping {
                                                    "正在停止…"
                                                } else {
                                                    "停止"
                                                })
                                                .disabled(!running)
                                                .on_click(cx.listener(Self::abort)),
                                        )
                                    })
                                    .child(
                                        Button::new("send-live")
                                            .debug_selector(|| "send-live".into())
                                            .small()
                                            .primary()
                                            .label(if running { "加入队列" } else { "发送" })
                                            .disabled(
                                                !live_started
                                                    || stopping
                                                    || self.control_operation.is_some()
                                                    || self.branch_preview_leaf.is_some()
                                                    || self.compacting,
                                            )
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                let input = this.composer.clone();
                                                this.submit_composer(&input, window, cx);
                                            })),
                                    ),
                            ),
                            ),
                            ),
                    )
                    .when(!below_widgets.is_empty(), |view| {
                        view.child(render_extension_widgets(
                            "extension-widgets-below",
                            &below_widgets,
                            &self.extension_widgets_below_scroll,
                            cx,
                        ))
                    })
                    .when(!statuses.is_empty() || show_custom_ui_capability, |view| {
                        view.child(
                            div()
                                .debug_selector(|| "extension-status-bar".into())
                                .child(
                                    StatusBar::new()
                                        .when(!visible_statuses.is_empty(), |bar| {
                                            bar.left(
                                                h_flex()
                                                    .min_w_0()
                                                    .gap_2()
                                                    .children(
                                                        visible_statuses
                                                            .into_iter()
                                                            .enumerate()
                                                            .map(|(index, status)| {
                                                                let full = format!(
                                                                    "{}: {}",
                                                                    status.display_key, status.text
                                                                );
                                                                div()
                                                                    .id(format!(
                                                                        "extension-status-item-{index}"
                                                                    ))
                                                                    .debug_selector(move || {
                                                                        format!(
                                                                            "extension-status-item-{index}"
                                                                        )
                                                                    })
                                                                    .text_xs()
                                                                    .text_color(
                                                                        cx.theme().muted_foreground,
                                                                    )
                                                                    .child(elide_extension_text(
                                                                        &full, 40,
                                                                    ))
                                                                    .tooltip(move |window, cx| {
                                                                        Tooltip::new(full.clone())
                                                                            .build(window, cx)
                                                                    })
                                                            }),
                                                    )
                                                    .when(hidden_status_count > 0, |row| {
                                                        row.child(
                                                            div()
                                                                .id("extension-status-overflow")
                                                                .debug_selector(|| {
                                                                    "extension-status-overflow"
                                                                        .into()
                                                                })
                                                                .text_xs()
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                )
                                                                .child(format!(
                                                                    "还有 {hidden_status_count} 项"
                                                                ))
                                                                .tooltip(move |window, cx| {
                                                                    Tooltip::new(
                                                                        hidden_status_tooltip
                                                                            .clone(),
                                                                    )
                                                                    .build(window, cx)
                                                                }),
                                                        )
                                                    }),
                                            )
                                        })
                                        .when(show_custom_ui_capability, |bar| {
                                            bar.right(
                                                div()
                                                    .id("extension-custom-ui-capability")
                                                    .debug_selector(|| {
                                                        "extension-custom-ui-capability".into()
                                                    })
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(custom_ui_capability)
                                                    .tooltip(move |window, cx| {
                                                        Tooltip::new(format!(
                                                            "钉死 RPC 不支持 extension custom UI（{}）",
                                                            crate::live_session::UNSUPPORTED_BY_PINNED_RPC
                                                        ))
                                                        .build(window, cx)
                                                    }),
                                            )
                                        }),
                                ),
                        )
                    }),
            )
    }
}

/// ToggleGroup 的新勾选状态 → composer 模式。
///
/// ToggleGroup 是多选语义：它把被点的那一段取反后，把**整个**勾选向量回传。
/// 所以 `[true, true]` 不代表「两个都选中」，而是「在 Steer 已选中时点了 Follow-up」。
/// 这里靠与当前模式对应的向量比对，找出真正被点的那一段，从而还原成单选。
/// 点已选中的那一段会得到 `[false, false]`，同样能定位到它本身，模式保持不变。
const fn next_composer_mode(checks: &[bool], current: ComposerMode) -> Option<ComposerMode> {
    let [steer, follow_up] = match *checks {
        [steer, follow_up] => [steer, follow_up],
        _ => return None,
    };
    let (steer_now, follow_up_now) = match current {
        ComposerMode::Steer => (true, false),
        ComposerMode::FollowUp => (false, true),
    };
    if steer != steer_now {
        Some(ComposerMode::Steer)
    } else if follow_up != follow_up_now {
        Some(ComposerMode::FollowUp)
    } else {
        None
    }
}

const fn session_controls_enabled(phase: Option<LivePhase>, busy: bool) -> bool {
    matches!(phase, Some(LivePhase::Idle)) && !busy
}

const fn abort_retry_disabled(busy: bool) -> bool {
    // begin_control 对所有控制操作共用同一个 busy 门禁，按钮状态必须与之完全一致。
    busy
}

const fn should_restore_submission(kind: RequestFailureKind) -> bool {
    matches!(kind, RequestFailureKind::Rejected)
}

fn build_submission(message: String, attachments: &[ComposerAttachment]) -> ComposerSubmission {
    ComposerSubmission {
        message,
        images: attachments
            .iter()
            .map(|attachment| attachment.draft.clone())
            .collect(),
    }
}

fn is_cycle_model_keystroke(event: &KeyDownEvent) -> bool {
    event.keystroke.modifiers.control
        && !event.keystroke.modifiers.alt
        && !event.keystroke.modifiers.shift
        && !event.keystroke.modifiers.platform
        && event.keystroke.key.eq_ignore_ascii_case("p")
}

fn slash_query(value: &str) -> Option<&str> {
    let query = value.strip_prefix('/')?;
    (!query.chars().any(char::is_whitespace)).then_some(query)
}

fn attachment_from_draft(draft: pi_data::DraftImage) -> Option<ComposerAttachment> {
    let bytes = STANDARD.decode(&draft.data).ok()?;
    let format = match draft.mime_type.as_str() {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        _ => return None,
    };
    Some(ComposerAttachment {
        preview: Arc::new(Image::from_bytes(format, bytes)),
        draft,
    })
}

fn gpui_image_to_draft(image: Image) -> Result<pi_data::DraftImage, pi_data::ImageValidationError> {
    pi_data::image_from_clipboard_bytes(image.format.mime_type(), image.bytes)
}

#[derive(Debug, PartialEq, Eq)]
enum ClipboardPasteDecision {
    TextOnly,
    Images {
        images: Vec<pi_data::DraftImage>,
        warning: Option<String>,
    },
    ImageError {
        message: String,
        has_text: bool,
    },
}

fn classify_clipboard_paste(entries: Vec<ClipboardEntry>) -> ClipboardPasteDecision {
    classify_clipboard_paste_with(entries, gpui_image_to_draft)
}

fn classify_clipboard_paste_with(
    entries: Vec<ClipboardEntry>,
    mut convert: impl FnMut(Image) -> Result<pi_data::DraftImage, pi_data::ImageValidationError>,
) -> ClipboardPasteDecision {
    let has_text = entries
        .iter()
        .any(|entry| matches!(entry, ClipboardEntry::String(_)));
    let mut images = Vec::new();
    let mut errors = Vec::new();
    let mut saw_image = false;
    for entry in entries {
        if let ClipboardEntry::Image(image) = entry {
            saw_image = true;
            match convert(image) {
                Ok(image) => images.push(image),
                Err(error) => errors.push(error.to_string()),
            }
        }
    }
    if !images.is_empty() {
        let warning = (!errors.is_empty()).then(|| {
            format!(
                "已添加 {} 张剪贴板图片；另有 {} 张未添加：{}",
                images.len(),
                errors.len(),
                errors.join("；")
            )
        });
        ClipboardPasteDecision::Images { images, warning }
    } else if saw_image {
        ClipboardPasteDecision::ImageError {
            message: format!("剪贴板图片读取失败：{}", errors.join("；")),
            has_text,
        }
    } else {
        ClipboardPasteDecision::TextOnly
    }
}

fn clipboard_image_add_feedback(
    result: Result<(), pi_data::ImageValidationError>,
    warning: Option<String>,
) -> Option<String> {
    match result {
        Ok(()) => warning,
        Err(error) => Some(error.to_string()),
    }
}

fn migrate_draft_key(drafts: &mut pi_data::DraftStore, from: &str, to: &str) {
    if from == to {
        return;
    }
    let draft = drafts.get(from);
    drafts.set(to.to_owned(), draft);
    drafts.clear(from);
}

fn restart_session_path(
    controls: Option<&SessionControls>,
    history: &ConversationDocument,
) -> Option<PathBuf> {
    controls
        .and_then(|controls| controls.session_file.clone())
        .filter(|path| !path.as_os_str().is_empty() && path.is_file())
        .or_else(|| {
            (!history.source_path.as_os_str().is_empty() && history.source_path.is_file())
                .then(|| history.source_path.clone())
        })
}

const fn thinking_label(level: pi_rpc::ThinkingLevel) -> &'static str {
    match level {
        pi_rpc::ThinkingLevel::Off => "Off",
        pi_rpc::ThinkingLevel::Minimal => "Minimal",
        pi_rpc::ThinkingLevel::Low => "Low",
        pi_rpc::ThinkingLevel::Medium => "Medium",
        pi_rpc::ThinkingLevel::High => "High",
        pi_rpc::ThinkingLevel::Xhigh => "XHigh",
        pi_rpc::ThinkingLevel::Max => "Max",
    }
}

fn elide_extension_text(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let prefix = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn render_extension_widgets(
    selector: &'static str,
    widgets: &[crate::live_session::ExtensionWidget],
    scroll_handle: &ScrollHandle,
    cx: &App,
) -> gpui::AnyElement {
    let placement = selector;
    let viewport_selector = format!("{selector}-viewport");
    let content = v_flex()
        .w_full()
        .min_h_full()
        .flex_none()
        .gap_2()
        .px_2()
        .children(widgets.iter().enumerate().map(|(widget_index, widget)| {
            v_flex()
                .id(format!("extension-widget-{placement}-{widget_index}"))
                .debug_selector(move || format!("{placement}-widget-{widget_index}"))
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border.opacity(0.8))
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(cx.theme().muted_foreground)
                        .child(widget.display_key.clone()),
                )
                .children(
                    widget
                        .lines
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(index, line)| {
                            div()
                                .id(format!(
                                    "extension-widget-line-{placement}-{widget_index}-{index}"
                                ))
                                .debug_selector(move || {
                                    format!("{placement}-widget-{widget_index}-line-{index}")
                                })
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(elide_extension_text(&line, 80))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(line.clone()).build(window, cx)
                                })
                        }),
                )
        }));
    let scroll_area = v_flex()
        .id(format!("{selector}-area"))
        .debug_selector(move || viewport_selector.clone())
        .size_full()
        .max_h(px(144.))
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .lock_scroll_axis()
        .child(content);

    div()
        .id(selector)
        .debug_selector(move || selector.into())
        .relative()
        .w_full()
        .max_h(px(144.))
        .child(scroll_area)
        // scrollbar 必须与滚动 area 同级，否则 gpui 会把内容 offset 也施加到 overlay。
        .vertical_scrollbar(scroll_handle)
        .into_any_element()
}

fn centered_state(
    icon: IconName,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    cx: &App,
) -> gpui::AnyElement {
    v_flex()
        .debug_selector(|| "chat-empty-or-loading".into())
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .p_6()
        .child(Icon::new(icon).size(gpui::px(32.)))
        .child(div().font_semibold().child(title.into()))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(detail.into()),
        )
        .into_any_element()
}

fn session_cwd(path: &std::path::Path) -> Option<PathBuf> {
    pi_data::load_session(path)
        .ok()
        .map(|session| PathBuf::from(session.header.cwd))
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, path::PathBuf};

    use gpui::{
        AppContext as _, ClipboardItem, Modifiers, MouseButton, Pixels, Point, TestAppContext,
        VisualTestContext, point, size,
    };
    use gpui_component::Root;
    use pi_render::MessageRole;

    use super::*;

    fn document(message: &str) -> Arc<ConversationDocument> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"id\":\"s\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"C:/fixture\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"{message}\"}}}}\n"
            ),
        )
        .unwrap();
        Arc::new(pi_render::render_path(path).unwrap())
    }

    fn rich_document() -> Arc<ConversationDocument> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rich.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"id\":\"rich\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"C:/fixture\"}\n",
                "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"---\\ntitle: Fixture\\ntags: [ui]\\n---\\nhello\"},{\"type\":\"image\",\"data\":\"<redacted>\",\"mimeType\":\"image/png\"}]}}\n",
                "{\"type\":\"message\",\"id\":\"trace\",\"parentId\":\"u\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"inspect the fixture\"},{\"type\":\"toolCall\",\"id\":\"tool\",\"name\":\"bash\",\"arguments\":{\"command\":\"cargo test\"}}]}}\n",
                "{\"type\":\"message\",\"id\":\"r\",\"parentId\":\"trace\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"tool\",\"toolName\":\"bash\",\"content\":[{\"type\":\"text\",\"text\":\"\\u001b[31mfailed\\u001b[0m\"}],\"details\":{\"patch\":\"--- a/a.rs\\n+++ b/a.rs\\n@@ -1 +1 @@\\n-old\\n+new\"},\"isError\":true}}\n",
                "{\"type\":\"message\",\"id\":\"answer\",\"parentId\":\"r\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"# Answer\\n```rust\\nfn main() {}\\n```\\n```mermaid\\ngraph TD; A-->B\\n```\"}]}}\n"
            ),
        )
        .unwrap();
        Arc::new(pi_render::render_path(path).unwrap())
    }

    fn long_scroll_document() -> Arc<ConversationDocument> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long-scroll.jsonl");
        let mut fixture = std::fs::File::create(&path).unwrap();
        writeln!(
            fixture,
            r#"{{"type":"session","id":"long-scroll","timestamp":"2026-01-01T00:00:00Z","cwd":"C:/fixture"}}"#
        )
        .unwrap();

        let mut parent_id: Option<String> = None;
        for turn in 0..32 {
            let user_id = format!("scroll-user-{turn}");
            let parent = parent_id
                .as_deref()
                .map_or_else(|| "null".to_owned(), |id| format!(r#""{id}""#));
            writeln!(
                fixture,
                r#"{{"type":"message","id":"{user_id}","parentId":{parent},"message":{{"role":"user","content":"User turn {turn}"}}}}"#
            )
            .unwrap();

            let assistant_id = format!("scroll-assistant-{turn}");
            let line_count = if turn == 0 { 120 } else { 8 };
            let content = (0..line_count)
                .map(|line| format!("Assistant turn {turn}, line {line}"))
                .collect::<Vec<_>>()
                .join(r"\n\n");
            writeln!(
                fixture,
                r#"{{"type":"message","id":"{assistant_id}","parentId":"{user_id}","message":{{"role":"assistant","content":"{content}"}}}}"#
            )
            .unwrap();
            parent_id = Some(assistant_id);
        }
        drop(fixture);

        Arc::new(pi_render::render_path(path).unwrap())
    }

    fn markdown_message(
        id: &str,
        role: MessageRole,
        prefix: &str,
        line_count: usize,
    ) -> Arc<pi_render::Message> {
        Arc::new(pi_render::Message {
            id: id.to_owned(),
            role,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks: vec![pi_render::Block::Markdown(pi_render::MarkdownBlock {
                source: (0..line_count)
                    .map(|line| format!("{prefix} line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            })],
        })
    }

    fn replace_message_item(
        document: &ConversationDocument,
        index: usize,
        id: &str,
        line_count: usize,
    ) -> Arc<ConversationDocument> {
        let message = markdown_message(id, MessageRole::Assistant, "Streamed", line_count);
        let mut messages = document.messages.to_vec();
        if let ConversationItem::Message(previous) = &document.items[index]
            && let Some(message_index) = messages
                .iter()
                .position(|candidate| candidate.id == previous.id)
        {
            messages[message_index] = message.clone();
        }
        let mut items = document.items.to_vec();
        items[index] = ConversationItem::Message(message);
        Arc::new(ConversationDocument {
            session_id: document.session_id.clone(),
            source_path: document.source_path.clone(),
            cwd: PathBuf::new(),
            messages: messages.into(),
            items: items.into(),
            minimap: document.minimap.clone(),
            diagnostics: document.diagnostics.clone(),
        })
    }

    fn active_tail_document() -> Arc<ConversationDocument> {
        let user = markdown_message("tail-user", MessageRole::User, "Question", 1);
        let process_message = Arc::new(pi_render::Message {
            id: "tail-trace".to_owned(),
            role: MessageRole::Assistant,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks: vec![pi_render::Block::Thinking(
                (0..160)
                    .map(|line| format!("Expanded process detail {line}"))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )],
        });
        let process = pi_render::ProcessGroup {
            id: "process-tail-user".to_owned(),
            messages: Arc::from([process_message.clone()]),
            message_count: 1,
            tool_call_count: 0,
            collapsible: false,
        };
        Arc::new(ConversationDocument {
            session_id: "active-tail".to_owned(),
            source_path: PathBuf::from("active-tail.jsonl"),
            cwd: PathBuf::new(),
            messages: Arc::from([user.clone(), process_message]),
            items: Arc::from([
                ConversationItem::Message(user),
                ConversationItem::Process(process),
            ]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        })
    }

    fn settled_tail_document() -> Arc<ConversationDocument> {
        let active = active_tail_document();
        let user = match &active.items[0] {
            ConversationItem::Message(message) => message.clone(),
            ConversationItem::Process(_) => unreachable!(),
        };
        let process = match &active.items[1] {
            ConversationItem::Process(group) => pi_render::ProcessGroup {
                collapsible: true,
                ..group.clone()
            },
            ConversationItem::Message(_) => unreachable!(),
        };
        let answer = markdown_message("tail-answer", MessageRole::Assistant, "Final answer", 40);
        let mut messages = active.messages.to_vec();
        messages.push(answer.clone());
        Arc::new(ConversationDocument {
            session_id: active.session_id.clone(),
            source_path: active.source_path.clone(),
            cwd: PathBuf::new(),
            messages: messages.into(),
            items: Arc::from([
                ConversationItem::Message(user),
                ConversationItem::Process(process),
                ConversationItem::Message(answer),
            ]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        })
    }

    fn draw_frames(visual: &mut VisualTestContext, count: usize) {
        for _ in 0..count {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
    }

    fn start_scrollbar_thumb_drag(
        visual: &mut VisualTestContext,
        panel: &gpui::Entity<ChatPanel>,
        cx: &mut TestAppContext,
    ) -> Point<Pixels> {
        let scroll_bounds = visual.debug_bounds("chat-message-scroll").unwrap();
        let (scroll_max, offset_before_down) = panel.read_with(cx, |panel, _| {
            (
                panel.list_state.max_offset_for_scrollbar().y,
                panel.list_state.scroll_px_offset_for_scrollbar().y,
            )
        });
        assert!(scroll_max > px(0.), "fixture must be scrollable");
        let viewport_height = scroll_bounds.size.height;
        let content_height = viewport_height + scroll_max;
        let thumb_height = (viewport_height / content_height * viewport_height).max(px(48.));
        let thumb_travel = viewport_height - thumb_height;
        let scroll_percentage = (-offset_before_down / scroll_max).clamp(0., 1.);
        let thumb = point(
            scroll_bounds.right() - px(8.),
            scroll_bounds.top() + thumb_travel * scroll_percentage + thumb_height / 2.,
        );

        visual.simulate_mouse_move(thumb, None, Modifiers::default());
        visual.simulate_mouse_down(thumb, MouseButton::Left, Modifiers::default());
        let offset_after_down = panel.read_with(cx, |panel, _| {
            panel.list_state.scroll_px_offset_for_scrollbar().y
        });
        assert_eq!(
            offset_after_down, offset_before_down,
            "pressing the thumb must not trigger a scrollbar track jump"
        );

        point(
            scroll_bounds.right() - px(8.),
            scroll_bounds.bottom() - px(4.),
        )
    }

    fn finish_scrollbar_drag_to_bottom(
        visual: &mut VisualTestContext,
        panel: &gpui::Entity<ChatPanel>,
        cx: &mut TestAppContext,
        near_track_bottom: Point<Pixels>,
    ) {
        draw_frames(visual, 1);
        let offset_before_move = panel.read_with(cx, |panel, _| {
            panel.list_state.scroll_px_offset_for_scrollbar().y
        });
        visual.simulate_mouse_move(
            near_track_bottom,
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        let offset_after_first_move = panel.read_with(cx, |panel, _| {
            panel.list_state.scroll_px_offset_for_scrollbar().y
        });
        // 首次 move 可能被 Scrollbar 帧率限制挡住；补发一次不同位置的 pressed move，
        // 但不在按住期间额外 draw，避免拖拽几何被中途重建。
        let second_drag_position =
            point(near_track_bottom.x - px(1.), near_track_bottom.y + px(16.));
        visual.simulate_mouse_move(
            second_drag_position,
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        visual.run_until_parked();
        let offset_after_move = panel.read_with(cx, |panel, _| {
            panel.list_state.scroll_px_offset_for_scrollbar().y
        });
        assert!(
            offset_after_first_move != offset_before_move
                || offset_after_move != offset_before_move,
            "pressed mouse move must drag the thumb rather than act as a track click"
        );

        visual.simulate_mouse_up(near_track_bottom, MouseButton::Left, Modifiers::default());
        panel.update(cx, |panel, _| {
            assert!(!panel.list_state.is_scrollbar_dragging());
        });
        draw_frames(visual, 4);
    }

    fn drag_scrollbar_to_bottom(
        visual: &mut VisualTestContext,
        panel: &gpui::Entity<ChatPanel>,
        cx: &mut TestAppContext,
    ) {
        let near_track_bottom = start_scrollbar_thumb_drag(visual, panel, cx);
        finish_scrollbar_drag_to_bottom(visual, panel, cx, near_track_bottom);
        if panel.read_with(cx, |panel, _| panel.list_state.is_scrolled_to_end()) != Some(true) {
            // unknown 高度在首个拖拽跨帧补测后可能增长；用新几何再拖一次即可到真实底部。
            let near_track_bottom = start_scrollbar_thumb_drag(visual, panel, cx);
            finish_scrollbar_drag_to_bottom(visual, panel, cx, near_track_bottom);
        }
    }

    fn assert_drag_reached_last_item(
        panel: &gpui::Entity<ChatPanel>,
        last_item_index: usize,
        cx: &mut TestAppContext,
    ) {
        panel.update(cx, |panel, _| {
            assert_eq!(panel.list_state.is_scrolled_to_end(), Some(true));
            let last_bounds = panel
                .list_state
                .bounds_for_item(last_item_index)
                .expect("last item must be laid out after dragging to the bottom");
            let viewport = panel.list_state.viewport_bounds();
            assert!(last_bounds.top() < viewport.bottom());
            assert!(last_bounds.bottom() > viewport.top());
        });
    }

    struct ChatDialogHarness {
        panel: gpui::Entity<ChatPanel>,
    }

    struct FocusDialogFixture {
        focus: FocusHandle,
    }

    impl Render for FocusDialogFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus).child("other dialog")
        }
    }

    impl Render for ChatDialogHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.panel
                .update(cx, |panel, cx| panel.process_extension_ui(window, cx));
            div()
                .size_full()
                .child(self.panel.clone())
                .children(Root::render_dialog_layer(window, cx))
        }
    }

    fn render_status_with_panel_sized(
        cx: &mut TestAppContext,
        status: ChatStatus,
        window_size: gpui::Size<gpui::Pixels>,
    ) -> (VisualTestContext, gpui::Entity<ChatPanel>) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let result = captured.clone();
        let handle = cx.open_window(window_size, move |window, cx| {
            let panel = cx.new(|cx| {
                let mut panel = ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                );
                if let ChatStatus::Ready(document) = &status {
                    panel.sync_list_document(document, true);
                }
                panel.status = status;
                panel
            });
            *result.borrow_mut() = Some(panel.clone());
            let harness = cx.new(|_| ChatDialogHarness { panel });
            Root::new(harness, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..8 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        let panel = captured.borrow().clone().unwrap();
        (visual, panel)
    }

    fn render_status_with_panel(
        cx: &mut TestAppContext,
        status: ChatStatus,
    ) -> (VisualTestContext, gpui::Entity<ChatPanel>) {
        render_status_with_panel_sized(cx, status, size(gpui::px(520.), gpui::px(480.)))
    }

    fn render_status(cx: &mut TestAppContext, status: ChatStatus) -> VisualTestContext {
        render_status_with_panel(cx, status).0
    }

    fn fixture_controls(session_id: &str) -> SessionControls {
        fixture_controls_with_file(session_id, None)
    }

    fn fixture_controls_with_file(
        session_id: &str,
        session_file: Option<PathBuf>,
    ) -> SessionControls {
        SessionControls {
            model: None,
            thinking_level: pi_rpc::ThinkingLevel::Off,
            models: Vec::new(),
            thinking_levels: Vec::new(),
            session_file,
            session_id: session_id.to_owned(),
            tree: pi_rpc::TreeData {
                tree: Vec::new(),
                leaf_id: None,
            },
            auto_compaction_enabled: false,
            auto_retry_enabled: false,
            is_compacting: false,
        }
    }

    fn fixture_model(id: &str, name: &str, provider: &str) -> pi_rpc::Model {
        pi_rpc::Model {
            id: id.to_owned(),
            name: name.to_owned(),
            api: "fixture".to_owned(),
            provider: provider.to_owned(),
            base_url: "https://fixture.invalid".to_owned(),
            reasoning: true,
            input: vec![pi_rpc::ModelInput::Text],
            cost: pi_rpc::ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: None,
            },
            context_window: 100_000,
            max_tokens: 4_096,
            extra: Default::default(),
        }
    }

    #[gpui::test]
    fn extension_status_bar_is_absent_until_status_or_capability_is_seen(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        assert!(visual.debug_bounds("extension-status-bar").is_none());
        assert!(visual.debug_bounds("extension-status-item-0").is_none());
        assert!(
            visual
                .debug_bounds("extension-custom-ui-capability")
                .is_none()
        );

        panel.update(cx, |panel, cx| {
            panel.extension_ui.apply(
                "status".into(),
                pi_rpc::ExtensionUiRequest::SetStatus {
                    status_key: "fixture".into(),
                    status_text: Some("ready".into()),
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        assert!(visual.debug_bounds("extension-status-bar").is_some());
        assert!(visual.debug_bounds("extension-status-item-0").is_some());

        panel.update(cx, |panel, cx| {
            panel.extension_ui.reset();
            panel.extension_ui.apply(
                "capability".into(),
                pi_rpc::ExtensionUiRequest::SetTitle {
                    title: "Seen extension".into(),
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        assert!(visual.debug_bounds("extension-status-bar").is_some());
        assert!(visual.debug_bounds("extension-status-item-0").is_none());
        assert!(
            visual
                .debug_bounds("extension-custom-ui-capability")
                .is_some()
        );
    }

    #[gpui::test]
    fn extension_status_bar_limits_visible_text_fragments_and_summarizes_overflow(
        cx: &mut TestAppContext,
    ) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            for index in 0..5 {
                panel.extension_ui.apply(
                    format!("status-{index}"),
                    pi_rpc::ExtensionUiRequest::SetStatus {
                        status_key: format!("key-{index}"),
                        status_text: Some(format!("value-{index}")),
                    },
                );
            }
            // 任意 extension UI 请求都会启用能力片段，因此左侧最多只能再显示 2 段。
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        assert!(visual.debug_bounds("extension-status-item-0").is_some());
        assert!(visual.debug_bounds("extension-status-item-1").is_none());
        assert!(visual.debug_bounds("extension-status-item-2").is_none());
        assert!(visual.debug_bounds("extension-status-overflow").is_some());
        assert!(
            visual
                .debug_bounds("extension-custom-ui-capability")
                .is_some()
        );
        let visible_text_nodes = [
            "extension-status-item-0",
            "extension-status-item-1",
            "extension-status-item-2",
            "extension-status-overflow",
            "extension-custom-ui-capability",
        ]
        .into_iter()
        .filter(|selector| visual.debug_bounds(selector).is_some())
        .count();
        assert_eq!(visible_text_nodes, 3, "S-8 要求独立文本节点总数不超过 3");
    }

    #[gpui::test]
    fn extension_widgets_status_and_editor_text_render_in_native_layers(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        assert!(
            visual
                .debug_bounds("extension-custom-ui-capability")
                .is_none()
        );
        let long_editor = format!("line\nwith\ttab\u{0}\u{202e}{}", "x".repeat(5000));
        let expected_editor =
            crate::live_session::sanitize_extension_text(&long_editor, usize::MAX);
        panel.update(cx, |panel, cx| {
            panel.extension_ui.apply(
                "above".into(),
                pi_rpc::ExtensionUiRequest::SetWidget {
                    widget_key: "above".into(),
                    widget_lines: Some(vec!["above line".into()]),
                    widget_placement: None,
                },
            );
            panel.extension_ui.apply(
                "below".into(),
                pi_rpc::ExtensionUiRequest::SetWidget {
                    widget_key: "below".into(),
                    widget_lines: Some(vec!["below line".into()]),
                    widget_placement: Some(pi_rpc::WidgetPlacement::BelowEditor),
                },
            );
            panel.extension_ui.apply(
                "status".into(),
                pi_rpc::ExtensionUiRequest::SetStatus {
                    status_key: "fixture".into(),
                    status_text: Some("ready".into()),
                },
            );
            panel.extension_ui.apply(
                "text".into(),
                pi_rpc::ExtensionUiRequest::SetEditorText {
                    text: long_editor.clone(),
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 4);
        assert!(visual.debug_bounds("extension-widgets-above").is_some());
        assert!(visual.debug_bounds("extension-widgets-below").is_some());
        assert!(visual.debug_bounds("extension-status-item-0").is_some());
        assert!(
            visual
                .debug_bounds("extension-custom-ui-capability")
                .is_some()
        );
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.composer.read(cx).value().to_string()),
            expected_editor
        );
    }

    #[gpui::test]
    fn extension_widget_scroll_handles_are_independent(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            let lines = (0..8)
                .map(|index| format!("widget line {index}"))
                .collect::<Vec<_>>();
            for widget_index in 0..3 {
                for (placement, wire_placement) in [
                    ("above", None),
                    ("below", Some(pi_rpc::WidgetPlacement::BelowEditor)),
                ] {
                    let id = format!("{placement}-{widget_index}");
                    panel.extension_ui.apply(
                        id.clone(),
                        pi_rpc::ExtensionUiRequest::SetWidget {
                            widget_key: id,
                            widget_lines: Some(lines.clone()),
                            widget_placement: wire_placement,
                        },
                    );
                }
            }
            cx.notify();
        });
        draw_frames(&mut visual, 4);
        let above_viewport_before = visual
            .debug_bounds("extension-widgets-above-viewport")
            .expect("above widget viewport missing");
        let below_viewport_before = visual
            .debug_bounds("extension-widgets-below-viewport")
            .expect("below widget viewport missing");
        let above_first_before = visual
            .debug_bounds("extension-widgets-above-widget-0-line-0")
            .expect("above first line missing")
            .origin
            .y;
        let below_first_before = visual
            .debug_bounds("extension-widgets-below-widget-0-line-0")
            .expect("below first line missing")
            .origin
            .y;
        assert_eq!(above_viewport_before.size.height, px(144.));
        assert_eq!(below_viewport_before.size.height, px(144.));
        panel.update(cx, |panel, _| {
            assert!(panel.extension_widgets_above_scroll.max_offset().y > px(0.));
            assert!(panel.extension_widgets_below_scroll.max_offset().y > px(0.));
            panel
                .extension_widgets_above_scroll
                .set_offset(point(px(0.), px(-80.)));
        });
        draw_frames(&mut visual, 3);
        let above_viewport_after = visual
            .debug_bounds("extension-widgets-above-viewport")
            .expect("above widget viewport disappeared");
        let below_viewport_after = visual
            .debug_bounds("extension-widgets-below-viewport")
            .expect("below widget viewport disappeared");
        let above_first_after = visual
            .debug_bounds("extension-widgets-above-widget-0-line-0")
            .expect("above first line disappeared")
            .origin
            .y;
        let below_first_after = visual
            .debug_bounds("extension-widgets-below-widget-0-line-0")
            .expect("below first line disappeared")
            .origin
            .y;
        assert_eq!(above_viewport_after, above_viewport_before);
        assert_eq!(below_viewport_after, below_viewport_before);
        assert!(above_first_after < above_first_before);
        assert_eq!(below_first_after, below_first_before);
        panel.update(cx, |panel, _| {
            assert!(panel.extension_widgets_above_scroll.offset().y < px(0.));
            assert_eq!(panel.extension_widgets_below_scroll.offset().y, px(0.));
        });
    }

    #[derive(Default)]
    struct RecordingResponseSender {
        responses: std::sync::Mutex<Vec<pi_rpc::ExtensionUiResponse>>,
        error: Option<String>,
    }

    impl crate::live_session::ExtensionResponseSender for RecordingResponseSender {
        fn send(&self, response: pi_rpc::ExtensionUiResponse) -> Result<(), String> {
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            self.responses.lock().unwrap().push(response);
            Ok(())
        }
    }

    #[gpui::test]
    fn extension_dialog_queue_uses_real_callbacks_and_response_sink(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        let sender = std::sync::Arc::new(RecordingResponseSender::default());
        let raw_select = format!("family 👨‍👩‍👧‍👦\tvalue\u{202e}{}", "x".repeat(300));
        panel.update(cx, |panel, cx| {
            panel.extension_response_sender = Some(sender.clone());
            for (id, request) in [
                (
                    "select",
                    pi_rpc::ExtensionUiRequest::Select {
                        title: "Select".into(),
                        options: vec![raw_select.clone(), "Beta".into()],
                        timeout: None,
                    },
                ),
                (
                    "confirm",
                    pi_rpc::ExtensionUiRequest::Confirm {
                        title: "Confirm".into(),
                        message: "Continue?".into(),
                        timeout: None,
                    },
                ),
                (
                    "input",
                    pi_rpc::ExtensionUiRequest::Input {
                        title: "Input".into(),
                        placeholder: Some("placeholder-only".into()),
                        timeout: None,
                    },
                ),
                (
                    "editor",
                    pi_rpc::ExtensionUiRequest::Editor {
                        title: "Editor".into(),
                        prefill: Some("prefill".into()),
                    },
                ),
            ] {
                panel.extension_ui.apply(id.into(), request);
            }
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        let option = visual
            .debug_bounds("extension-select-option-0")
            .expect("select option footer missing");
        visual.simulate_click(option.center(), Default::default());
        draw_frames(&mut visual, 3);
        visual.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
        draw_frames(&mut visual, 3);
        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel
                    .extension_ui
                    .active_dialog()
                    .map(|dialog| dialog.id.as_str()),
                Some("input")
            );
            cx.notify();
        });
        visual.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
        draw_frames(&mut visual, 3);
        visual.dispatch_action(gpui_component::dialog::Cancel);
        draw_frames(&mut visual, 3);
        visual.run_until_parked();
        let responses = sender.responses.lock().unwrap();
        assert_eq!(responses.len(), 4);
        assert_eq!(
            responses[0],
            pi_rpc::ExtensionUiResponse::value("select", raw_select)
        );
        assert_eq!(
            responses[1],
            pi_rpc::ExtensionUiResponse::confirmed("confirm", true)
        );
        assert_eq!(
            responses[2],
            pi_rpc::ExtensionUiResponse::value("input", "")
        );
        assert_eq!(
            responses[3],
            pi_rpc::ExtensionUiResponse::cancelled("editor")
        );
        panel.update(cx, |panel, _| {
            assert!(panel.extension_ui.active_dialog().is_none());
        });
    }

    #[gpui::test]
    fn extension_dialog_send_failure_discards_and_advances(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        let sender = std::sync::Arc::new(RecordingResponseSender {
            responses: std::sync::Mutex::new(Vec::new()),
            error: Some("sink failed".to_owned()),
        });
        panel.update(cx, |panel, cx| {
            panel.extension_response_sender = Some(sender);
            for id in ["first", "second"] {
                panel.extension_ui.apply(
                    id.into(),
                    pi_rpc::ExtensionUiRequest::Confirm {
                        title: id.into(),
                        message: "Continue?".into(),
                        timeout: None,
                    },
                );
            }
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.finish_extension_dialog(
                    "first",
                    pi_rpc::ExtensionUiResponse::confirmed("first", true),
                    cx,
                );
            });
            window.close_dialog(cx);
        });
        draw_frames(&mut visual, 3);
        visual.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(panel.extension_dialog_open.as_deref(), Some("second"));
            assert!(
                panel
                    .rpc_error
                    .as_deref()
                    .is_some_and(|error| error.contains("sink failed"))
            );
        });
    }

    #[gpui::test]
    fn extension_dialog_pending_close_never_pops_other_dialog_and_converges_once(
        cx: &mut TestAppContext,
    ) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        let sender = std::sync::Arc::new(RecordingResponseSender::default());
        panel.update(cx, |panel, cx| {
            panel.extension_response_sender = Some(sender.clone());
            panel.extension_ui.apply(
                "extension".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "Extension".into(),
                    message: "Continue?".into(),
                    timeout: None,
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        let other_focus = visual.update(|window, cx| {
            let focus = cx.focus_handle();
            let fixture = cx.new(|_| FocusDialogFixture {
                focus: focus.clone(),
            });
            window.open_dialog(cx, move |dialog, _, _| dialog.child(fixture.clone()));
            focus.focus(window, cx);
            focus
        });
        draw_frames(&mut visual, 3);
        assert!(visual.update(|window, cx| other_focus.contains_focused(window, cx)));
        panel.update(cx, |panel, cx| {
            panel.finish_extension_dialog(
                "extension",
                pi_rpc::ExtensionUiResponse::cancelled("extension"),
                cx,
            );
            panel.extension_dialog_needs_close = true;
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        assert!(visual.update(|window, cx| window.has_active_dialog(cx)));
        assert!(visual.update(|window, cx| other_focus.contains_focused(window, cx)));
        visual.update(|window, _| window.blur());
        panel.update(cx, |panel, cx| {
            panel.extension_dialog_needs_close = true;
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(visual.update(|window, cx| window.has_active_dialog(cx)));
        visual.update(|window, cx| other_focus.focus(window, cx));
        panel.update(cx, |panel, cx| {
            panel.extension_dialog_needs_close = true;
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(visual.update(|window, cx| other_focus.contains_focused(window, cx)));
        visual.update(|window, cx| window.close_dialog(cx));
        draw_frames(&mut visual, 3);
        draw_frames(&mut visual, 3);
        assert!(panel.read_with(cx, |panel, _| !panel.extension_dialog_needs_close));
        assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));
        visual.run_until_parked();
        assert_eq!(sender.responses.lock().unwrap().len(), 1);
    }

    #[gpui::test]
    fn extension_dialog_body_and_footer_focus_timeout_reset_and_advance(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));

        panel.update(cx, |panel, cx| {
            panel.extension_ui.apply(
                "input-timeout".into(),
                pi_rpc::ExtensionUiRequest::Input {
                    title: "Input".into(),
                    placeholder: None,
                    timeout: Some(200),
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        let body_focus = panel.read_with(cx, |panel, _| {
            panel.extension_dialog_body_focus.clone().unwrap()
        });
        visual.update(|window, cx| body_focus.focus(window, cx));
        visual
            .executor()
            .advance_clock(std::time::Duration::from_millis(220));
        visual.run_until_parked();
        draw_frames(&mut visual, 4);
        assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));

        panel.update(cx, |panel, cx| {
            panel.extension_ui.apply(
                "editor-reset".into(),
                pi_rpc::ExtensionUiRequest::Editor {
                    title: "Editor".into(),
                    prefill: Some("fixture".into()),
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        let editor_bounds = visual
            .debug_bounds("extension-dialog-textarea")
            .expect("editor Textarea wrapper missing");
        visual.simulate_click(editor_bounds.center(), Default::default());
        let body_focus = panel.read_with(cx, |panel, _| {
            panel.extension_dialog_body_focus.clone().unwrap()
        });
        assert!(visual.update(|window, cx| {
            body_focus.contains_focused(window, cx) || body_focus.within_focused(window, cx)
        }));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.reset_foreground_extension_ui(window, cx)
            });
        });
        draw_frames(&mut visual, 3);
        assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));

        for request in [
            pi_rpc::ExtensionUiRequest::Confirm {
                title: "Confirm footer".into(),
                message: "Continue?".into(),
                timeout: None,
            },
            pi_rpc::ExtensionUiRequest::Select {
                title: "Select footer".into(),
                options: vec!["Alpha".into()],
                timeout: None,
            },
        ] {
            panel.update(cx, |panel, cx| {
                panel.extension_ui.apply("footer-reset".into(), request);
                cx.notify();
            });
            draw_frames(&mut visual, 3);
            let footer_focus = panel.read_with(cx, |panel, _| {
                panel.extension_dialog_footer_focus.clone().unwrap()
            });
            visual.update(|window, cx| footer_focus.focus(window, cx));
            assert!(visual.update(|window, cx| {
                footer_focus.contains_focused(window, cx) || footer_focus.within_focused(window, cx)
            }));
            visual.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.reset_foreground_extension_ui(window, cx)
                });
            });
            draw_frames(&mut visual, 3);
            assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));
        }

        panel.update(cx, |panel, cx| {
            panel.extension_ui.apply(
                "confirm-advance".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "Confirm".into(),
                    message: "Continue?".into(),
                    timeout: None,
                },
            );
            panel.extension_ui.apply(
                "select-next".into(),
                pi_rpc::ExtensionUiRequest::Select {
                    title: "Next".into(),
                    options: vec!["Beta".into()],
                    timeout: None,
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        let submit = visual
            .debug_bounds("extension-confirm-submit")
            .expect("confirm footer submit missing");
        visual.simulate_click(submit.center(), Default::default());
        draw_frames(&mut visual, 3);
        assert!(visual.debug_bounds("extension-select-option-0").is_some());
    }

    #[gpui::test]
    fn queued_expired_extension_dialog_is_cancelled_without_opening(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        let sender = std::sync::Arc::new(RecordingResponseSender::default());
        panel.update(cx, |panel, cx| {
            panel.extension_response_sender = Some(sender.clone());
            panel.extension_ui.apply(
                "expired".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "Expired".into(),
                    message: "Must not flash".into(),
                    timeout: Some(50),
                },
            );
            panel.extension_ui.apply(
                "next".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "Next".into(),
                    message: "Visible".into(),
                    timeout: None,
                },
            );
            cx.notify();
        });
        visual
            .executor()
            .advance_clock(std::time::Duration::from_millis(60));
        visual.run_until_parked();
        draw_frames(&mut visual, 3);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.extension_dialog_open.clone()),
            Some("next".to_owned())
        );
        visual.run_until_parked();
        assert_eq!(
            sender.responses.lock().unwrap().as_slice(),
            &[pi_rpc::ExtensionUiResponse::cancelled("expired")]
        );
    }

    #[gpui::test]
    fn extension_dialog_timeout_closes_and_advances_fifo(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            panel.extension_ui.apply(
                "timed".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "Timed".into(),
                    message: "Continue?".into(),
                    timeout: Some(1),
                },
            );
            panel.extension_ui.apply(
                "next".into(),
                pi_rpc::ExtensionUiRequest::Input {
                    title: "Next".into(),
                    placeholder: None,
                    timeout: None,
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 6);
        panel.update(cx, |panel, _| {
            assert_eq!(panel.extension_dialog_open.as_deref(), Some("next"));
            assert_eq!(
                panel
                    .extension_ui
                    .active_dialog()
                    .map(|dialog| dialog.id.as_str()),
                Some("next")
            );
            assert!(
                panel
                    .rpc_error
                    .as_deref()
                    .is_some_and(|error| error.contains("超时"))
            );
        });
    }

    #[gpui::test]
    fn empty_chat_renders_state_selector(cx: &mut TestAppContext) {
        let mut empty = render_status(cx, ChatStatus::Empty);
        assert!(empty.debug_bounds("chat-empty-or-loading").is_some());
        assert!(empty.debug_bounds("live-composer").is_some());
    }

    #[gpui::test]
    fn scrollbar_thumb_drag_reaches_end_of_long_variable_height_chat(cx: &mut TestAppContext) {
        let document = long_scroll_document();
        let last_item_index = document.items.len() - 1;
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document));
        panel.update(cx, |panel, _| {
            // 未修复时这里允许 None，确保测试仍执行真实 drag，并在最终到不了底时失败。
            assert_ne!(panel.list_state.is_scrolled_to_end(), Some(true));
            assert_ne!(
                panel.list_state.item_is_below_viewport(last_item_index),
                Some(false),
                "last message must begin outside the viewport or remain unknown"
            );
        });

        let near_track_bottom = start_scrollbar_thumb_drag(&mut visual, &panel, cx);
        panel.update(cx, |panel, _| {
            assert!(panel.list_state.is_scrollbar_dragging());
        });
        finish_scrollbar_drag_to_bottom(&mut visual, &panel, cx, near_track_bottom);
        assert_drag_reached_last_item(&panel, last_item_index, cx);
    }

    #[gpui::test]
    fn offscreen_same_id_growth_is_measured_when_scrollbar_drag_starts(cx: &mut TestAppContext) {
        let document = long_scroll_document();
        let last_item_index = document.items.len() - 1;
        let last_item_id = document.items[last_item_index].id().to_owned();
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document.clone()));
        let grown = replace_message_item(&document, last_item_index, &last_item_id, 160);
        panel.update(cx, |panel, cx| {
            panel.sync_list_document(&grown, false);
            panel.status = ChatStatus::Ready(grown.clone());
            assert_eq!(panel.list_state.is_scrolled_to_end(), None);
            cx.notify();
        });
        draw_frames(&mut visual, 1);

        drag_scrollbar_to_bottom(&mut visual, &panel, cx);
        assert_drag_reached_last_item(&panel, last_item_index, cx);
    }

    #[gpui::test]
    fn settled_active_tail_invalidates_retained_process_and_measures_new_answer(
        cx: &mut TestAppContext,
    ) {
        let history = long_scroll_document();
        let active_tail = active_tail_document();
        let settled_tail = settled_tail_document();
        let mut active_items = history.items.to_vec();
        active_items.extend(active_tail.items.iter().cloned());
        let active = Arc::new(ConversationDocument {
            session_id: history.session_id.clone(),
            source_path: history.source_path.clone(),
            cwd: PathBuf::new(),
            messages: history.messages.clone(),
            items: active_items.into(),
            minimap: history.minimap.clone(),
            diagnostics: history.diagnostics.clone(),
        });
        let mut settled_items = history.items.to_vec();
        // settled 投影保留同一个 User Arc；Process 身份保留但折叠，并新增 Answer。
        settled_items.push(active_tail.items[0].clone());
        settled_items.extend(settled_tail.items[1..].iter().cloned());
        let settled = Arc::new(ConversationDocument {
            session_id: history.session_id.clone(),
            source_path: history.source_path.clone(),
            cwd: PathBuf::new(),
            messages: history.messages.clone(),
            items: settled_items.into(),
            minimap: history.minimap.clone(),
            diagnostics: history.diagnostics.clone(),
        });
        let process_index = history.items.len() + 1;
        let last_item_index = settled.items.len() - 1;
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(active));
        panel.update(cx, |panel, cx| {
            panel.sync_list_document(&settled, true);
            panel.status = ChatStatus::Ready(settled.clone());
            assert!(panel.list_items[process_index].collapsible);
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        panel.update(cx, |panel, _| {
            assert_eq!(
                panel.list_state.item_is_below_viewport(last_item_index),
                Some(true)
            );
        });

        drag_scrollbar_to_bottom(&mut visual, &panel, cx);
        assert_drag_reached_last_item(&panel, last_item_index, cx);
    }

    #[gpui::test]
    fn real_process_and_detail_toggles_render_details(cx: &mut TestAppContext) {
        let document = rich_document();
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document));

        let process_toggle = visual.debug_bounds("process-group-toggle").unwrap();
        visual.simulate_click(process_toggle.center(), Modifiers::default());
        draw_frames(&mut visual, 2);
        assert!(visual.debug_bounds("process-group-details").is_some());
        assert!(visual.debug_bounds("thinking-card").is_some());
        assert!(visual.debug_bounds("tool-card").is_some());

        panel.update(cx, |panel, cx| {
            panel.toggle_tool("trace:thinking:0".to_owned(), "trace".to_owned(), cx);
        });
        draw_frames(&mut visual, 2);
        assert!(visual.debug_bounds("thinking-card-details").is_some());

        panel.update(cx, |panel, cx| {
            panel.toggle_tool("trace:tool:tool".to_owned(), "trace".to_owned(), cx);
        });
        draw_frames(&mut visual, 2);
        assert!(visual.debug_bounds("tool-card-details").is_some());
    }

    #[test]
    fn splice_restore_preserves_logical_list_offset() {
        let state = ListState::new(3, ListAlignment::Top, px(1200.));
        let expected = gpui::ListOffset {
            item_ix: 1,
            offset_in_item: px(17.),
        };
        state.scroll_to(expected);
        state.splice(1..2, 1);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(0.));
        state.scroll_to(expected);
        let restored = state.logical_scroll_top();
        assert_eq!(restored.item_ix, expected.item_ix);
        assert_eq!(restored.offset_in_item, expected.offset_in_item);
    }

    #[gpui::test]
    fn tool_detail_toggle_keeps_top_level_item_anchor(cx: &mut TestAppContext) {
        let document = rich_document();
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document));
        let process_toggle = visual.debug_bounds("process-group-toggle").unwrap();
        visual.simulate_click(process_toggle.center(), Modifiers::default());
        draw_frames(&mut visual, 3);
        let before = panel.read_with(cx, |panel, _| {
            let index = panel
                .list_items
                .iter()
                .position(|item| item.id == "process-u")
                .unwrap();
            let bounds = panel.list_state.bounds_for_item(index).unwrap();
            (
                index,
                bounds.top() - panel.list_state.viewport_bounds().top(),
            )
        });
        panel.update(cx, |panel, cx| {
            panel.toggle_tool("trace:tool:tool".to_owned(), "trace".to_owned(), cx);
        });
        draw_frames(&mut visual, 4);
        let after = panel.read_with(cx, |panel, _| {
            let bounds = panel.list_state.bounds_for_item(before.0).unwrap();
            bounds.top() - panel.list_state.viewport_bounds().top()
        });
        assert!((after - before.1).abs() <= px(1.));
    }

    #[gpui::test]
    fn tool_detail_toggle_keeps_default_normal_list_attached_at_tail(cx: &mut TestAppContext) {
        let document = rich_document();
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document));
        panel.update(cx, |panel, _| {
            panel.tail_attached = true;
            panel.list_state.scroll_to_end();
            assert!(!panel.list_state.is_following_tail());
        });
        draw_frames(&mut visual, 2);
        panel.update(cx, |panel, cx| {
            assert_ne!(panel.list_state.is_scrolled_to_end(), Some(false));
            panel.toggle_tool("trace:tool:tool".to_owned(), "trace".to_owned(), cx);
            assert!(panel.tail_attached);
            assert!(!panel.list_state.is_following_tail());
        });
    }

    #[gpui::test]
    fn ready_chat_folds_completed_process_trace(cx: &mut TestAppContext) {
        let (mut ready, panel) = render_status_with_panel(cx, ChatStatus::Ready(rich_document()));
        let scroll = ready.debug_bounds("chat-message-scroll").unwrap();
        assert!(scroll.size.height > px(0.), "message viewport collapsed");
        for selector in [
            "chat-window",
            "chat-message",
            "chat-minimap",
            "process-group",
            "live-composer",
        ] {
            assert!(ready.debug_bounds(selector).is_some(), "missing {selector}");
        }
        for hidden in [
            "process-group-details",
            "thinking-card",
            "thinking-card-details",
            "tool-card",
            "tool-card-details",
        ] {
            assert!(
                ready.debug_bounds(hidden).is_none(),
                "{hidden} must stay lazy while process is collapsed"
            );
        }
        let bounds = ready.debug_bounds("process-group-toggle").unwrap();
        ready.simulate_click(bounds.center(), gpui::Modifiers::default());
        for _ in 0..3 {
            ready.update(|window, cx| window.draw(cx).clear(cx));
            ready.run_until_parked();
        }
        for selector in ["process-group-details", "thinking-card", "tool-card"] {
            assert!(ready.debug_bounds(selector).is_some(), "missing {selector}");
        }
        assert!(ready.debug_bounds("thinking-card-details").is_none());
        assert!(ready.debug_bounds("tool-card-details").is_none());

        panel.update(cx, |panel, cx| {
            panel.toggle_tool("trace:thinking:0".to_owned(), "trace".to_owned(), cx);
        });
        for _ in 0..2 {
            ready.update(|window, cx| window.draw(cx).clear(cx));
            ready.run_until_parked();
        }
        assert!(ready.debug_bounds("thinking-card-details").is_some());
    }

    #[gpui::test]
    fn minimap_navigation_detaches_tail_follow(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let result = captured.clone();
        let handle = cx.open_window(size(gpui::px(520.), gpui::px(480.)), move |window, cx| {
            let panel = cx.new(|cx| {
                let mut panel = ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                );
                let document = rich_document();
                panel.sync_list_document(&document, true);
                panel.status = ChatStatus::Ready(document);
                *result.borrow_mut() = Some(cx.entity());
                panel
            });
            Root::new(panel, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        let bounds = visual.debug_bounds("chat-minimap-node").unwrap();
        visual.simulate_click(bounds.center(), gpui::Modifiers::default());
        let panel = captured.borrow().clone().unwrap();
        panel.update(cx, |panel, _| assert!(!panel.tail_attached));
    }

    fn assert_attachment_composer_rows_do_not_overlap(
        visual: &mut VisualTestContext,
        expected_attachment: bool,
    ) {
        let viewport = visual
            .debug_bounds("composer-textarea-viewport")
            .expect("composer textarea viewport missing");
        let control = visual
            .debug_bounds("composer-textarea-control")
            .expect("composer textarea control missing");
        let actions = visual
            .debug_bounds("composer-actions")
            .expect("composer actions missing");
        assert!(viewport.bottom() <= actions.top());
        assert!(control.bottom() <= actions.top());
        assert!(
            actions.top() - control.bottom() >= px(8.),
            "必须保留规范 gap_2"
        );
        if expected_attachment {
            let attachments = visual
                .debug_bounds("composer-attachments")
                .expect("production attachment strip missing");
            assert!(attachments.size.height > px(0.));
            assert!(attachments.bottom() <= viewport.top());
            assert!(viewport.top() - attachments.bottom() >= px(8.));
        } else {
            assert!(visual.debug_bounds("composer-attachments").is_none());
        }
    }

    #[gpui::test]
    fn composer_attachment_keeps_input_and_actions_separate_in_empty_and_ready_states(
        cx: &mut TestAppContext,
    ) {
        for status in [ChatStatus::Empty, ChatStatus::Ready(document("hello"))] {
            let (mut visual, panel) =
                render_status_with_panel_sized(cx, status, size(px(1000.), px(1000.)));
            assert_attachment_composer_rows_do_not_overlap(&mut visual, false);

            panel.update(cx, |panel, cx| {
                let draft =
                    pi_data::image_from_bytes(b"\x89PNG\r\n\x1a\nlayout-fixture".to_vec()).unwrap();
                panel.attachments = vec![attachment_from_draft(draft).unwrap()];
                cx.notify();
            });
            draw_frames(&mut visual, 3);
            assert_attachment_composer_rows_do_not_overlap(&mut visual, true);

            panel.update(cx, |panel, cx| {
                panel.attachments.clear();
                cx.notify();
            });
            draw_frames(&mut visual, 3);
            assert_attachment_composer_rows_do_not_overlap(&mut visual, false);
        }
    }

    #[gpui::test]
    fn composer_popup_attachment_and_file_prompt_render(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            panel.slash_commands = vec![pi_rpc::RpcSlashCommand {
                name: "fixture".into(),
                description: Some("Fixture command".into()),
                source: pi_rpc::SlashCommandSource::Prompt,
                source_info: pi_rpc::SourceInfo {
                    path: "C:/fixture.md".into(),
                    source: "fixture".into(),
                    scope: pi_rpc::SourceScope::Project,
                    origin: pi_rpc::SourceOrigin::TopLevel,
                    base_dir: None,
                },
            }];
            let draft = pi_data::image_from_bytes(b"\x89PNG\r\n\x1a\nfixture".to_vec()).unwrap();
            panel.attachments = vec![attachment_from_draft(draft).unwrap()];
            panel.popup = Some(ComposerPopup::Slash(panel.slash_commands.clone()));
            cx.notify();
        });
        for _ in 0..2 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert!(visual.debug_bounds("composer-popup").is_some());
        assert!(visual.debug_bounds("composer-popup-item").is_some());
        assert!(visual.debug_bounds("composer-attachments").is_some());

        let button = visual.debug_bounds("attach-images").unwrap();
        visual.simulate_click(button.center(), gpui::Modifiers::default());
        assert!(visual.did_prompt_for_paths());
        visual.simulate_path_prompt_response(|options| {
            assert!(options.files && !options.directories && options.multiple);
            None
        });
        visual.run_until_parked();
    }

    #[gpui::test]
    fn at_popup_uses_utf8_cursor_and_directory_accept_drills_down(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.composer_cwd = Some(PathBuf::from("C:/fixture"));
                panel.file_index = Some(pi_data::FileIndex {
                    entries: vec![
                        pi_data::FileIndexEntry {
                            path: "src".into(),
                            is_dir: true,
                        },
                        pi_data::FileIndexEntry {
                            path: "src/main.rs".into(),
                            is_dir: false,
                        },
                    ],
                    truncated: false,
                });
                let cursor = "前缀 @sr".len();
                panel.composer.update(cx, |input, cx| {
                    input.set_value("前缀 @sr 后续", window, cx);
                    input.set_selected_range(cursor..cursor, cx);
                });
                panel.refresh_popup(cx);
                assert!(panel.accept_popup(&panel.composer.clone(), window, cx));
                let input = panel.composer.read(cx);
                assert_eq!(input.value().as_ref(), "前缀 @src/ 后续");
                assert_eq!(input.cursor(), "前缀 @src/".len());
                let ComposerPopup::At { query, entries } = panel.popup.as_ref().unwrap() else {
                    panic!("directory acceptance must immediately reopen @ popup");
                };
                assert_eq!(query.query, "src/");
                assert!(entries.iter().any(|entry| entry.path == "src/main.rs"));
            });
        });
        visual.run_until_parked();
    }

    #[gpui::test]
    fn switching_sessions_saves_and_restores_isolated_drafts(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("one".into());
                panel.composer.update(cx, |input, cx| {
                    input.set_value("draft one", window, cx);
                });
                panel.save_current_draft(cx);
                panel.draft_key = Some("two".into());
                panel.prepare_draft_restore();
                assert_eq!(panel.drafts.get("one").text, "draft one");
                assert_eq!(panel.drafts.get("two"), pi_data::ComposerDraft::default());
                panel.drafts.set(
                    "two",
                    pi_data::ComposerDraft {
                        text: "draft two".into(),
                        images: Vec::new(),
                    },
                );
                panel.prepare_draft_restore();
                let restored = panel.drafts.get("two");
                panel.composer.update(cx, |input, cx| {
                    input.set_value(restored.text, window, cx);
                });
                assert_eq!(panel.composer.read(cx).value().as_ref(), "draft two");
                assert_eq!(panel.drafts.get("one").text, "draft one");
            });
        });
    }

    fn fixture_selection(id: &str, title: &str, cwd: &std::path::Path) -> SessionSelected {
        SessionSelected {
            id: id.to_owned(),
            path: cwd.join(format!("{id}.jsonl")),
            cwd: cwd.to_path_buf(),
            title: title.to_owned(),
        }
    }

    /// 给一个标签登记一个**只登记不启动**的会话。
    ///
    /// `create_session` 是纯内存操作（R23 验收：登记 20 个会话零进程），因此这里可以在
    /// 没有 pi 二进制的测试环境里得到真实的调度器状态，而不是编一个假状态糊弄断言。
    fn register_parked_session(panel: &mut ChatPanel, index: usize, cwd: &std::path::Path) {
        let session = panel.runtime_manager.create_session(
            pi_runtime::SessionDescriptor {
                binary: PathBuf::from("pi-not-launched"),
                cwd: cwd.to_path_buf(),
                session_path: Some(cwd.join("registered.jsonl")),
                tool_preset: ToolPreset::Inherit,
                agent_dir: None,
            },
            (*document("registered")).clone(),
        );
        panel.sessions[index].session = Some(session);
        panel.sync_scheduler_states();
    }

    /// T2：`SessionUiState` 按标签多实例隔离 —— 草稿、展开集合、滚动跟随、横幅与
    /// 补全面板各归各的，切走再切回原样恢复。
    #[gpui::test]
    fn session_tabs_isolate_draft_expansion_and_scroll_state(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("session-one".into());
                panel.tab_title = "一号".into();
                panel.expanded_tools.insert("tool-a".into());
                panel.tail_attached = false;
                panel.rpc_error = Some("一号的错误".into());
                panel.composer.update(cx, |input, cx| {
                    input.set_value("一号草稿", window, cx);
                });

                let second = panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
                assert_eq!(second, 1);
                assert_eq!(panel.focused, 1);
                // 新标签是干净的：不继承上一个标签的任何会话态。
                assert!(panel.expanded_tools.is_empty());
                assert!(panel.rpc_error.is_none());
                assert!(panel.tail_attached);
                assert!(panel.file_index.is_none());
                assert!(panel.popup.is_none());
                assert_eq!(panel.composer.read(cx).value().as_ref(), "");

                panel.draft_key = Some("session-two".into());
                panel.expanded_tools.insert("tool-b".into());
                panel.composer.update(cx, |input, cx| {
                    input.set_value("二号草稿", window, cx);
                });

                // 切回一号：它自己的展开集合、横幅、跟随状态都还在。
                panel.focus_tab(0, window, cx);
                assert_eq!(panel.focused, 0);
                assert!(panel.expanded_tools.contains("tool-a"));
                assert!(!panel.expanded_tools.contains("tool-b"));
                assert_eq!(panel.rpc_error.as_deref(), Some("一号的错误"));
                assert!(!panel.tail_attached);
                assert_eq!(panel.drafts.get("session-one").text, "一号草稿");
                assert_eq!(panel.drafts.get("session-two").text, "二号草稿");
            });
        });
        // 草稿回填发生在渲染时（`pending_draft_restore`），画一帧再断言输入框。
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.run_until_parked();
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.composer.read(cx).value().as_ref(), "一号草稿");
        });
    }

    /// T2：切换标签只动 UI 绑定 —— 不注销、不停止任何会话。
    #[gpui::test]
    fn focusing_another_tab_never_unregisters_a_background_session(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("背景")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("background".into());
                register_parked_session(panel, 0, workspace.path());
                let background = panel.sessions[0].session.expect("已登记");

                panel
                    .open_tab("前台".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.focus_tab(0, window, cx);
                panel.focus_tab(1, window, cx);

                assert_eq!(
                    panel.runtime_manager.session_state(background),
                    Some(pi_runtime::SchedulerState::Parked),
                    "切换标签不得注销后台会话"
                );
                assert_eq!(panel.sessions[0].session, Some(background));
                // 登记不占进程，这条顺带守住「切换本身不会拉起 pi」。
                assert_eq!(panel.runtime_manager.scheduler_report().resident_pi, 0);
            });
        });
    }

    /// T2：关标签才注销会话；关掉最后一个标签是重置而不是删除。
    ///
    /// 同时钉住 R24 崩溃整改后的契约：**标签立刻从条上消失，进程回收在后台**。
    /// `remove_session` 要等优雅停机、随后的 `tick()` 还可能就地拉起一个排队会话，
    /// 这两件事都不能占着 GPUI 主线程。
    #[gpui::test]
    fn closing_a_tab_unregisters_its_session_and_never_empties_the_strip(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        let second = visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("one".into());
                panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.draft_key = Some("two".into());
                register_parked_session(panel, 1, workspace.path());
                let second = panel.sessions[1].session.expect("已登记");

                panel.close_tab(1, window, cx);
                // UI 是即时的：标签当场没了，不等后台把进程收干净。
                assert_eq!(panel.sessions.len(), 1);
                assert_eq!(panel.focused, 0);
                second
            })
        });
        visual.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(
                panel.runtime_manager.session_state(second),
                None,
                "后台回收落地后，会话必须已从调度器注销"
            );
        });

        let only_tab = visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                let only_tab = panel.tab_id;
                panel.close_tab(0, window, cx);
                assert_eq!(panel.sessions.len(), 1, "标签条永远不为空");
                assert_ne!(panel.tab_id, only_tab, "最后一个标签是被重置，不是被复用");
                assert!(matches!(panel.status, ChatStatus::Empty));
                assert!(panel.draft_key.is_none());
                only_tab
            })
        });
        assert_ne!(only_tab, u64::MAX);
    }

    /// R24 视觉验收暴露的崩溃的回归用例：挂起**绝不能**在 GPUI 主线程上做。
    ///
    /// `RuntimeManager::park` 会等作业排空、拆 Actor、可能关进程，收尾还会 `tick()`
    /// 一次把排队会话就地提升上来（见 `pi-runtime` 的
    /// `park_finishes_a_queued_handoff_on_the_calling_thread`）。这一整套一旦占住主线程，
    /// 就是整窗口卡死一次进程交接——用户实测「2 活跃 + 1 排队时点挂起程序崩溃退出」。
    /// 这里钉的是结构：点击**同步返回**，只立起 busy 标记，真正的操作落在后台。
    #[gpui::test]
    fn parking_hands_the_process_work_to_a_background_task(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("one".into());
                register_parked_session(panel, 0, workspace.path());

                panel.park_focused_session(window, cx);
                assert_eq!(
                    panel.scheduler_job,
                    Some("挂起中…"),
                    "点击必须立刻返回并立起 busy 标记"
                );
                // 重复点击被挡住：第一次的结果还没回来，再叠一次进程操作只会更糟。
                panel.park_focused_session(window, cx);
                assert_eq!(panel.scheduler_job, Some("挂起中…"));
            });
        });
        // busy 期间界面照常有话说，且说的是「正在办」而不是旧状态。
        let note = panel.update(cx, |panel, cx| {
            panel
                .session_state_note(cx)
                .map(|note| note.text)
                .expect("busy 期间必须有说明")
        });
        assert_eq!(note, "挂起中…");

        visual.run_until_parked();
        panel.update(cx, |panel, _| {
            assert!(
                panel.scheduler_job.is_none(),
                "后台落地后必须清掉 busy 标记"
            );
        });
    }

    /// T2：同一个历史会话不会被开成两个标签，重复选中直接切过去。
    #[gpui::test]
    fn selecting_an_open_session_focuses_its_tab_instead_of_reloading(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Empty);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                let first = fixture_selection("alpha", "Alpha", workspace.path());
                panel
                    .load_selection(first.clone(), window, cx)
                    .expect("首个标签");
                assert_eq!(panel.sessions.len(), 1, "干净标签被原地复用");
                assert_eq!(panel.draft_key.as_deref(), Some("alpha"));

                panel
                    .load_selection(
                        fixture_selection("beta", "Beta", workspace.path()),
                        window,
                        cx,
                    )
                    .expect("第二个标签");
                assert_eq!(panel.sessions.len(), 2);
                assert_eq!(panel.focused, 1);

                panel.load_selection(first, window, cx).expect("切回第一个");
                assert_eq!(panel.sessions.len(), 2, "重复选中不得再开一个标签");
                assert_eq!(panel.focused, 0);
            });
        });
    }

    /// T2：标签数有界，到顶时明确失败且不动已有标签。
    #[gpui::test]
    fn the_tab_strip_is_bounded_and_refuses_instead_of_evicting(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("one".into());
                for index in 1..MAX_SESSION_TABS {
                    let opened = panel
                        .open_tab(format!("标签 {index}"), window, cx)
                        .expect("上限之内");
                    assert_eq!(opened, index);
                    panel.draft_key = Some(format!("key-{index}"));
                }
                assert_eq!(panel.sessions.len(), MAX_SESSION_TABS);
                let refused = panel
                    .open_tab("溢出".to_owned(), window, cx)
                    .expect_err("超出上限必须失败");
                assert!(refused.contains(&MAX_SESSION_TABS.to_string()), "{refused}");
                assert_eq!(panel.sessions.len(), MAX_SESSION_TABS, "失败不得动已有标签");
                assert_eq!(panel.focused, MAX_SESSION_TABS - 1);

                // 第四轮独立审查 P2：失败必须交回调用方。工作区会跟着这次选择去搬
                // 文件树和工具栏标题，只在自己身上留一条错误的话，界面就会
                // 「聊天还在旧会话、工作区已经指向被拒绝的那个」。
                let overflow = panel.load_selection(
                    fixture_selection("overflow", "溢出会话", std::path::Path::new("C:/fixture")),
                    window,
                    cx,
                );
                assert!(overflow.is_err(), "标签已满时选择必须返回 Err");
                assert_eq!(panel.sessions.len(), MAX_SESSION_TABS);
            });
        });
    }

    /// T2：调度状态由调度器给出，四态各有自己的标签形态与说明。
    #[gpui::test]
    fn scheduler_states_drive_the_tab_dot_and_the_state_note(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("状态")));
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                // 还没登记会话：是「历史」，不是「已挂起」。
                assert_eq!(
                    tab_state_of(&panel.sessions[0]),
                    gpui_pi_ui::SessionTabState::History
                );
                assert!(panel.session_state_note(cx).is_none());

                register_parked_session(panel, 0, workspace.path());
                assert_eq!(
                    tab_state_of(&panel.sessions[0]),
                    gpui_pi_ui::SessionTabState::Parked
                );
                let parked = panel.session_state_note(cx).expect("挂起要有说明");
                assert!(parked.text.contains("已挂起"), "{}", parked.text);

                for (state, expected) in [
                    (
                        pi_runtime::SchedulerState::Running,
                        gpui_pi_ui::SessionTabState::Running,
                    ),
                    (
                        pi_runtime::SchedulerState::Queued,
                        gpui_pi_ui::SessionTabState::Queued,
                    ),
                    (
                        pi_runtime::SchedulerState::Starting,
                        gpui_pi_ui::SessionTabState::Starting,
                    ),
                    (
                        pi_runtime::SchedulerState::Stopping,
                        gpui_pi_ui::SessionTabState::Stopping,
                    ),
                    (
                        pi_runtime::SchedulerState::Failed,
                        gpui_pi_ui::SessionTabState::Failed,
                    ),
                ] {
                    panel.sessions[0].scheduler_state = Some(state);
                    assert_eq!(tab_state_of(&panel.sessions[0]), expected);
                }

                // 运行中与两个短暂中间态不出横幅；排队与失败必须出，且失败要带原因。
                for quiet in [
                    pi_runtime::SchedulerState::Running,
                    pi_runtime::SchedulerState::Starting,
                    pi_runtime::SchedulerState::Stopping,
                ] {
                    panel.sessions[0].scheduler_state = Some(quiet);
                    assert!(panel.session_state_note(cx).is_none(), "{quiet:?}");
                }
                panel.sessions[0].scheduler_state = Some(pi_runtime::SchedulerState::Queued);
                assert!(
                    panel
                        .session_state_note(cx)
                        .expect("排队要有说明")
                        .text
                        .contains("排队中")
                );
                panel.sessions[0].scheduler_state = Some(pi_runtime::SchedulerState::Failed);
                panel.sessions[0].scheduler_failure = Some("pi 启动失败".into());
                assert!(
                    panel
                        .session_state_note(cx)
                        .expect("失败要有说明")
                        .text
                        .contains("pi 启动失败")
                );
            });
        });
    }

    /// T3：纯历史预览不画标签条；一旦有第二个标签（或有活会话）就出现，并带状态点。
    #[gpui::test]
    fn the_tab_strip_appears_only_once_a_second_session_is_open(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        assert!(
            visual.debug_bounds("session-tabs").is_none(),
            "单会话不该长出一条只有一个标签的横条"
        );
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("one".into());
                panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
            });
        });
        draw_frames(&mut visual, 4);
        let strip = visual.debug_bounds("session-tabs").expect("标签条应出现");
        assert!(strip.size.width > gpui::px(0.));
        assert!(
            visual.debug_bounds("session-tab-state-dot").is_some(),
            "每个标签都要有状态点"
        );
        assert!(visual.debug_bounds("close-session-tab").is_some());
    }

    /// 单个标签一旦登记了会话也要画标签条：状态点是用户唯一能看到会话在不在跑的地方，
    /// 关闭入口也只在标签上——不画就等于把一个活着的 pi 进程藏起来。
    #[gpui::test]
    fn a_single_live_session_still_gets_a_tab_so_it_can_be_seen_and_closed(
        cx: &mut TestAppContext,
    ) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("独苗")));
        assert!(visual.debug_bounds("session-tabs").is_none());
        panel.update(cx, |panel, _| {
            register_parked_session(panel, 0, workspace.path());
        });
        draw_frames(&mut visual, 4);
        assert!(visual.debug_bounds("session-tabs").is_some());
        assert!(visual.debug_bounds("close-session-tab").is_some());
    }

    /// 后台事件泵靠投影游标写进**自己那一槽**；写完必须还原，前台标签不受污染。
    #[gpui::test]
    fn projecting_into_a_background_tab_never_touches_the_foreground_one(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("前台")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("front".into());
                panel
                    .open_tab("后台".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.draft_key = Some("back".into());
                panel.focus_tab(0, window, cx);

                panel.project(1, |panel| {
                    panel.rpc_error = Some("后台会话的错误".into());
                    panel.compacting = true;
                });
                assert_eq!(panel.cursor, panel.focused, "投影结束必须还原游标");
                assert!(panel.rpc_error.is_none(), "后台错误不得渗到前台标签");
                assert!(!panel.compacting);
                assert_eq!(
                    panel.sessions[1].rpc_error.as_deref(),
                    Some("后台会话的错误")
                );
                assert!(panel.sessions[1].compacting);
            });
        });
    }

    /// 独立代码审查 P1：跨 await 点的续体必须钉回发起它的标签。
    ///
    /// 附件读盘期间用户切走标签是完全正常的操作；不钉标签的话，图片要么挂到别人身上，
    /// 要么因为对错了 `load_generation` 被静默丢弃。
    #[gpui::test]
    fn an_attachment_started_on_one_tab_never_lands_on_another(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let png = workspace.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nattach-fixture").expect("write png");

        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("one".into());
                let origin = panel.tab_id;
                panel.start_attach_paths(origin, vec![png.clone()], cx);
                // 读盘还没回来就切走——这正是不钉标签会出错的那一刻。
                panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.draft_key = Some("two".into());
            });
        });
        visual.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(panel.focused, 1);
            assert_eq!(
                panel.sessions[0].attachments.len(),
                1,
                "附件必须落回发起它的标签"
            );
            assert!(
                panel.sessions[1].attachments.is_empty(),
                "切过去的标签不得凭空多出一张图"
            );
            assert!(panel.sessions[1].rpc_error.is_none());
        });
    }

    /// 独立代码审查 P1：Extension UI 超时定时器也必须钉回开出它的标签。
    ///
    /// 不钉住的话，定时器会去检查前台标签的队列——id 撞上就取消了别人的请求，
    /// 撞不上则这条超时被静默吞掉，pi 那头永远等不到响应。
    #[gpui::test]
    fn an_extension_dialog_timeout_only_cancels_its_own_tabs_request(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        // 超时给得足够长，好让「打开对话框 → 切走标签 → 定时器到点」这三步的先后
        // 由测试说了算，而不是靠抢时间。
        panel.update(cx, |panel, cx| {
            panel.draft_key = Some("one".into());
            panel.extension_ui.apply(
                "shared-id".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "一号的确认".into(),
                    message: "Continue?".into(),
                    timeout: Some(5_000),
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        panel.update(cx, |panel, _| {
            assert_eq!(
                panel.extension_dialog_open.as_deref(),
                Some("shared-id"),
                "对话框要先真的打开，定时器才会被装上"
            );
        });

        // 切到第二个标签，并让它挂上一条**同名**的待处理请求。
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.draft_key = Some("two".into());
                panel.extension_ui.apply(
                    "shared-id".into(),
                    pi_rpc::ExtensionUiRequest::Confirm {
                        title: "二号的确认".into(),
                        message: "Continue?".into(),
                        timeout: None,
                    },
                );
                cx.notify();
            });
        });
        draw_frames(&mut visual, 3);

        visual
            .executor()
            .advance_clock(std::time::Duration::from_millis(6_000));
        visual.run_until_parked();
        draw_frames(&mut visual, 3);

        panel.update(cx, |panel, _| {
            assert_eq!(panel.focused, 1);
            // 一号的请求超时后被取消，诊断也留在一号。
            assert!(
                panel.sessions[0].extension_ui.active_dialog().is_none(),
                "发起标签的请求应已超时取消"
            );
            assert!(
                panel.sessions[0]
                    .rpc_error
                    .as_deref()
                    .is_some_and(|error| error.contains("超时")),
                "超时诊断必须留在发起标签：{:?}",
                panel.sessions[0].rpc_error
            );
            // 二号的同名请求毫发无伤。
            let survivor = panel.sessions[1]
                .extension_ui
                .active_dialog()
                .expect("另一个标签的同名请求不得被别人的超时取消");
            assert_eq!(survivor.id, "shared-id");
            assert!(
                matches!(
                    &survivor.request,
                    pi_rpc::ExtensionUiRequest::Confirm { title, .. } if title == "二号的确认"
                ),
                "留下的必须是二号自己那条请求"
            );
            assert!(panel.sessions[1].rpc_error.is_none());
        });
    }

    /// 独立代码审查 P1 的同类路径：原生选择器回来后，结果必须落在**发起它的标签**。
    ///
    /// 选择器可能开着好几秒。不钉住的话，用户切一下界面就会把别人的活会话切到这里
    /// 挑的文件上——比附件挂错标签严重得多。
    #[gpui::test]
    fn a_session_switch_choice_lands_on_the_tab_that_opened_the_picker(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.draft_key = Some("one".into());
                let origin = panel.tab_id;
                panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.draft_key = Some("two".into());
                assert_eq!(panel.focused, 1);

                // 选择器是在一号上打开的，尽管此刻前台是二号。
                panel.apply_session_switch_choice(origin, PathBuf::from("note.txt"), cx);
                assert_eq!(
                    panel.sessions[0].rpc_error.as_deref(),
                    Some("只能切换到 .jsonl 会话文件"),
                    "结果必须回到发起选择器的标签"
                );
                assert!(
                    panel.sessions[1].rpc_error.is_none(),
                    "前台标签不得替别人背这条错误"
                );

                // 标签已经关掉时整段跳过，不找替身写进去。
                panel.close_tab(0, window, cx);
                panel.apply_session_switch_choice(origin, PathBuf::from("note.txt"), cx);
                assert!(panel.sessions.iter().all(|slot| slot.rpc_error.is_none()));
            });
        });
    }

    /// 视觉审查 V-3：排队中的会话不该给一个「恢复运行」按钮。
    #[test]
    fn a_queued_session_shows_that_it_will_start_by_itself() {
        assert_eq!(start_action_copy(false, false).0, "启动活会话");
        assert_eq!(start_action_copy(false, true).0, "恢复运行");
        // 排队优先于「已登记」：这一格按钮此刻没有任何可执行的动作。
        assert_eq!(start_action_copy(true, true).0, "排队中…");
        assert!(start_action_copy(true, true).1.contains("自动启动"));
    }

    /// 视觉审查 V-6：挂起过程中不该闪一条红色「加载会话控制失败」。
    ///
    /// Park 会把在执行的元数据请求连同 client 一起抽走，那几条必然超时；用户刚点了
    /// 「挂起」，这正是他要的结果。实测在 2 活跃 + 1 排队场景下能看到
    /// 「加载会话控制失败：request req_5 timed out」一闪而过。
    #[gpui::test]
    fn teardown_metadata_failures_are_not_reported_as_errors(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        let controls_error = |sequence: u64| pi_runtime::RuntimeEffect {
            sequence,
            epoch: 0,
            kind: pi_runtime::RuntimeEffectKind::ControlsLoaded(Err(
                "request req_5 timed out".to_owned()
            )),
        };
        let commands_error = |sequence: u64| pi_runtime::RuntimeEffect {
            sequence,
            epoch: 0,
            kind: pi_runtime::RuntimeEffectKind::CommandsLoaded(Err(
                "request req_2 timed out".to_owned()
            )),
        };

        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                // 正在挂起：两条元数据失败都不该出横幅。
                panel.scheduler_job = Some("挂起中…");
                panel.apply_runtime_effect(controls_error(1), cx);
                panel.apply_runtime_effect(commands_error(2), cx);
                assert!(panel.rpc_error.is_none(), "{:?}", panel.rpc_error);

                // 挂起落地、已经没有 Runtime 了：迟到的失败同样不该出横幅，
                // 它已经无法据以行动。
                panel.scheduler_job = None;
                assert!(panel.active.is_none());
                panel.apply_runtime_effect(controls_error(3), cx);
                assert!(panel.rpc_error.is_none(), "{:?}", panel.rpc_error);
            });
        });
    }

    /// 第十轮独立审查 P1：不许把一个标签切进另一个标签已经占着的会话文件。
    ///
    /// 那会让两个 pi 进程绑同一份 JSONL，各自往里追加，落盘历史交错甚至写坏。
    /// 「切换会话」走的是原生选择器，绕开了侧栏选择时的去重，必须自己查一次。
    #[gpui::test]
    fn switching_into_a_session_another_tab_owns_is_refused(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let owned = workspace.path().join("owned.jsonl");
        std::fs::write(&owned, "").expect("write session file");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                // 一号已经登记了会话，并且绑在 owned.jsonl 上。
                panel.draft_key = Some("one".into());
                panel.tab_title = "一号".to_owned();
                register_parked_session(panel, 0, workspace.path());
                panel.apply_controls(fixture_controls_with_file("one", Some(owned.clone())));

                let second = panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.draft_key = Some("two".into());
                let second_tab = panel.sessions[second].tab_id;

                // 二号试图切到一号占着的那份文件：必须被拒，且不得开始任何控制操作。
                panel.apply_session_switch_choice(second_tab, owned.clone(), cx);
                assert!(
                    panel
                        .rpc_error
                        .as_deref()
                        .is_some_and(|error| error.contains("一号")),
                    "拒绝理由要指明是谁占着：{:?}",
                    panel.rpc_error
                );
                assert!(panel.control_operation.is_none(), "被拒时不得发起控制操作");
                assert!(
                    panel.sessions[0].rpc_error.is_none(),
                    "错误只属于发起切换的标签"
                );
            });
        });
    }

    /// 第九轮独立审查 P2：关一个后台标签不得动前台标签的对话框。
    ///
    /// 焦点句柄是**窗口级**的（只有前台标签会开对话框）。关后台标签时如果把它们
    /// 一起清掉，前台那个对话框就再也认不出自己是最上层，于是既关不掉、
    /// 它那条请求也回不去——一个关不掉的模态浮在别的会话上。
    #[gpui::test]
    fn closing_a_background_tab_leaves_the_foreground_dialog_alone(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("一号")));
        // 前台标签开一个 Extension UI 对话框。
        panel.update(cx, |panel, cx| {
            panel.draft_key = Some("one".into());
            panel.extension_ui.apply(
                "dlg".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "确认".into(),
                    message: "Continue?".into(),
                    timeout: None,
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        panel.update(cx, |panel, _| {
            assert_eq!(panel.extension_dialog_open.as_deref(), Some("dlg"));
            assert!(panel.extension_dialog_body_focus.is_some());
        });

        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                // 开第二个标签再切回前台，然后关掉那个后台标签。
                panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("第二个标签");
                panel.draft_key = Some("two".into());
                panel.focus_tab(0, window, cx);
                assert_eq!(panel.focused, 0);

                panel.close_tab(1, window, cx);
                assert_eq!(panel.sessions.len(), 1);
                assert!(
                    panel.extension_dialog_body_focus.is_some(),
                    "关后台标签不得清掉前台对话框的焦点句柄"
                );
                assert_eq!(
                    panel.extension_dialog_open.as_deref(),
                    Some("dlg"),
                    "前台对话框应原样开着"
                );
                assert_eq!(
                    panel
                        .extension_ui
                        .active_dialog()
                        .map(|dialog| dialog.id.as_str()),
                    Some("dlg"),
                    "前台标签的请求不该被别人的关闭流程清掉"
                );
            });
        });
    }

    /// 第八轮独立审查 P3：fresh 会话在校准之前不得对外暴露编造的会话身份。
    ///
    /// `draft_key` 是对外的 pi 会话身份（标签去重认它、工作区 tooltip 显示它）。
    /// 从前 fresh 会话会先塞一个 `fresh-{tab}-{generation}`，那个编出来的值会以
    /// 「真实身份」的名义漏到界面上，直到 `ControlsLoaded` 才被换掉。
    #[gpui::test]
    fn a_fresh_session_publishes_no_identity_until_it_is_calibrated(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Empty);
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                panel.fresh_session = true;
                panel.draft_key = None;
                // 校准之前：没有身份可广播，草稿仍然有地方存。
                assert!(panel.draft_key.is_none());
                let slot = panel.draft_slot_key();
                assert!(slot.starts_with("tab-"), "{slot}");
                panel.drafts.set(
                    slot.clone(),
                    pi_data::ComposerDraft {
                        text: "开会话前先写了半句".into(),
                        images: Vec::new(),
                    },
                );

                // 校准：拿到真实 session id，草稿一并迁过去。
                panel.apply_runtime_effect(
                    pi_runtime::RuntimeEffect {
                        sequence: 1,
                        epoch: 0,
                        kind: pi_runtime::RuntimeEffectKind::ControlsLoaded(Ok(fixture_controls(
                            "real-session-id",
                        ))),
                    },
                    cx,
                );
                assert_eq!(panel.draft_key.as_deref(), Some("real-session-id"));
                assert_eq!(
                    panel.drafts.get("real-session-id").text,
                    "开会话前先写了半句",
                    "校准时草稿要跟着迁到真实身份下"
                );
                assert!(
                    panel.drafts.get(&slot).text.is_empty(),
                    "临时键迁移后应清空"
                );
            });
        });
    }

    /// 第七轮独立审查 P2：复用空白标签时，槽里的瞬时状态必须一并清掉。
    ///
    /// 往空标签里拖一张非法图片：附件没加上，`rpc_error` 却留下了，而这仍然满足
    /// 「用户没往里放过东西」。只改标题就复用，那条不相干的红色横幅会跟进新会话。
    #[gpui::test]
    fn reusing_the_pristine_tab_drops_its_leftover_error(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Empty);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                // 用户偏好先设好，重建不该把它抹掉。
                panel.set_tool_preset(ToolPreset::ReadOnly, cx);
                panel.minimap_visible = false;

                // 一次失败的拖入：只留下错误，没有附件。
                panel.rpc_error = Some("无法识别的图片格式".to_owned());
                panel.rpc_error_protected = true;
                assert!(
                    panel.is_focused_tab_pristine(),
                    "只留了个错误横幅，仍然算没往里放过东西"
                );

                let index = panel
                    .open_tab("新会话".to_owned(), window, cx)
                    .expect("复用空白标签");
                assert_eq!(index, 0, "干净标签仍然原地复用");
                assert!(
                    panel.rpc_error.is_none(),
                    "上一段的错误不得跟进新会话：{:?}",
                    panel.rpc_error
                );
                assert!(!panel.rpc_error_protected);
                assert_eq!(panel.tab_title, "新会话");
                // 偏好留下。
                assert_eq!(panel.tool_preset, ToolPreset::ReadOnly);
                assert!(!panel.minimap_visible);
            });
        });
    }

    /// 第六轮独立审查 P2：重开一个已打开的会话要带上最新标题；关掉匿名标签要清草稿。
    #[gpui::test]
    fn reopening_a_renamed_session_updates_its_tab_and_closing_frees_its_draft(
        cx: &mut TestAppContext,
    ) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Empty);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel
                    .load_selection(
                        fixture_selection("alpha", "旧名字", workspace.path()),
                        window,
                        cx,
                    )
                    .expect("首次打开");
                assert_eq!(panel.tab_title, "旧名字");

                // 侧栏改名之后再点同一行：标签标题必须跟着改，而不是停在旧名字。
                panel
                    .load_selection(
                        fixture_selection("alpha", "新名字", workspace.path()),
                        window,
                        cx,
                    )
                    .expect("重开同一个会话");
                assert_eq!(panel.sessions.len(), 1, "同一个会话不该开成两个标签");
                assert_eq!(panel.tab_title, "新名字");
            });
        });

        // 匿名标签（没有会话身份）关掉之后，它的草稿键再也取不到，必须清掉。
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                let opened = panel
                    .open_tab("草稿标签".to_owned(), window, cx)
                    .expect("再开一个匿名标签");
                let orphan = panel.sessions[opened].draft_slot_key();
                assert!(panel.sessions[opened].draft_key.is_none());
                panel.composer.update(cx, |input, cx| {
                    input.set_value("会被丢掉的草稿", window, cx);
                });
                panel.save_current_draft(cx);
                assert!(!panel.drafts.get(&orphan).text.is_empty());

                panel.close_tab(opened, window, cx);
                assert!(
                    panel.drafts.get(&orphan).text.is_empty(),
                    "匿名标签关掉后，它那份取不回来的草稿必须一起清掉"
                );
            });
        });
    }

    /// 第五轮独立审查 P2：留在空标签上的草稿文字不能丢。
    ///
    /// 空标签没有 `draft_key`（那是 pi 会话身份），从前草稿就无处可存：
    /// 写了字 → 去开别的标签 → 切回来，字没了。现在每个标签从诞生起就有草稿落点。
    #[gpui::test]
    fn text_typed_on_an_empty_tab_survives_a_detour_to_another_tab(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Empty);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                assert!(panel.draft_key.is_none(), "空标签没有会话身份");
                panel.composer.update(cx, |input, cx| {
                    input.set_value("还没想好发给谁的一段话", window, cx);
                });
                // 写过字就不算没用过：这个标签不该被当成空白复用。
                panel.save_current_draft(cx);
                assert!(!panel.is_focused_tab_pristine());

                panel
                    .open_tab("二号".to_owned(), window, cx)
                    .expect("另开一个标签");
                assert_eq!(panel.sessions.len(), 2);
                assert_eq!(panel.composer.read(cx).value().as_ref(), "");

                panel.focus_tab(0, window, cx);
            });
        });
        // 草稿回填发生在渲染时（`pending_draft_restore`）。
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.run_until_parked();
        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.composer.read(cx).value().as_ref(),
                "还没想好发给谁的一段话",
                "切走再切回来，空标签上的字必须还在"
            );
        });
    }

    /// 第二轮独立审查 P2：空标签上挂着的附件不能被静默带进新会话。
    ///
    /// 这类附件没有 `draft_key` 可存，复用这个标签就等于把一张无关的图带进新会话；
    /// 反过来，直接清掉又是在丢用户已经做过的操作。判成「不干净」另开一个标签，两头都不亏。
    #[gpui::test]
    fn an_attachment_on_the_empty_tab_keeps_it_from_being_reused(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Empty);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                // 干净的空标签会被原地复用，不长出第二个标签。
                assert!(panel.is_focused_tab_pristine());

                let draft = pi_data::image_from_bytes(b"\x89PNG\r\n\x1a\ncarry".to_vec())
                    .expect("fixture png");
                panel
                    .add_draft_images(vec![draft], cx)
                    .expect("挂一张图到空标签上");
                assert_eq!(panel.attachments.len(), 1);
                assert!(!panel.is_focused_tab_pristine(), "挂了附件就不算没用过");

                let opened = panel
                    .open_tab("新会话".to_owned(), window, cx)
                    .expect("另开一个标签");
                assert_eq!(opened, 1, "带着附件的标签不该被复用");
                assert!(panel.attachments.is_empty(), "新标签不得继承别人的附件");
                assert_eq!(
                    panel.sessions[0].attachments.len(),
                    1,
                    "原标签的附件也不该被悄悄丢掉"
                );
            });
        });
    }

    #[test]
    fn tab_labels_truncate_by_character_so_multibyte_titles_stay_valid() {
        assert_eq!(truncate_label("短标题", 18), "短标题");
        let long = "一二三四五六七八九十一二三四五六七八九十";
        let truncated = truncate_label(long, 18);
        assert_eq!(truncated.chars().count(), 19, "18 个字符 + 省略号");
        assert!(truncated.ends_with('…'));
        // 按字节切会在这里 panic；按字符切必须原样保留每个汉字。
        assert!(long.starts_with(truncated.trim_end_matches('…')));
    }

    /// T2 ④：Steer / Follow-up 合并成 ToggleGroup 后仍是单选，点击可来回切换。
    #[gpui::test]
    fn composer_mode_toggle_group_switches_selection(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, _| {
            assert_eq!(panel.composer_mode, ComposerMode::Steer);
        });

        let group = visual
            .debug_bounds("composer-mode-toggle")
            .expect("composer 模式切换组必须渲染");
        assert!(group.size.width > px(0.));
        // 新增左侧会话控件后布局更密，先重新绘制，避免测试持有布局前的命中坐标。
        draw_frames(&mut visual, 1);
        let follow_up = visual
            .debug_bounds("composer-mode-follow-up")
            .expect("Follow-up 段必须保持可点击")
            .center();
        visual.simulate_click(follow_up, Modifiers::default());
        visual.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(panel.composer_mode, ComposerMode::FollowUp);
        });

        let steer = visual
            .debug_bounds("composer-mode-steer")
            .expect("Steer 段必须保持可点击")
            .center();
        visual.simulate_click(steer, Modifiers::default());
        visual.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(panel.composer_mode, ComposerMode::Steer);
        });
    }

    /// ToggleGroup 回传的是整个勾选向量，必须还原成单选，且点已选中的那一段不清空模式。
    #[test]
    fn toggle_group_checks_are_reduced_to_a_single_mode() {
        use ComposerMode::{FollowUp, Steer};

        // Steer 已选中时点 Follow-up → [true, true]。
        assert_eq!(next_composer_mode(&[true, true], Steer), Some(FollowUp));
        // Follow-up 已选中时点 Steer → 同样是 [true, true]，但被点的是另一段。
        assert_eq!(next_composer_mode(&[true, true], FollowUp), Some(Steer));
        // 点已选中的那一段 → [false, false]，模式保持不变。
        assert_eq!(next_composer_mode(&[false, false], Steer), Some(Steer));
        assert_eq!(
            next_composer_mode(&[false, false], FollowUp),
            Some(FollowUp)
        );
        // 与当前状态一致说明没有任何一段被点，什么都不做。
        assert_eq!(next_composer_mode(&[true, false], Steer), None);
        assert_eq!(next_composer_mode(&[false, true], FollowUp), None);
        // 段数对不上时保持沉默，不猜。
        assert_eq!(next_composer_mode(&[true], Steer), None);
    }

    /// 模型、thinking、工具三个选择器在会话控制数据到达后都进入渲染树。
    #[gpui::test]
    fn session_control_selectors_render_current_state(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            let model = fixture_model("model-one", "Model One", "provider-one");
            panel.apply_controls(SessionControls {
                model: Some(model.clone()),
                thinking_level: pi_rpc::ThinkingLevel::High,
                models: vec![model],
                thinking_levels: vec![pi_rpc::ThinkingLevel::Off, pi_rpc::ThinkingLevel::High],
                session_file: None,
                session_id: "fixture".to_owned(),
                tree: pi_rpc::TreeData {
                    tree: Vec::new(),
                    leaf_id: None,
                },
                auto_compaction_enabled: true,
                auto_retry_enabled: true,
                is_compacting: false,
            });
            panel.tool_preset = ToolPreset::ReadOnly;
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        for selector in [
            "model-selector",
            "thinking-selector",
            "tools-selector",
            "r13-session-controls",
            "branch-navigator-trigger",
            "session-actions",
            "export-html",
        ] {
            assert!(
                visual.debug_bounds(selector).is_some(),
                "missing {selector}"
            );
        }
    }

    #[test]
    fn branch_navigator_uses_component_size_scales() {
        let source = include_str!("panels.rs");
        let production = source.split("mod tests {").next().unwrap();
        let old_width = [".w(px(", "360.", "))"].concat();
        let old_height = [".max_h(px(", "260.", "))"].concat();
        assert!(production.contains(".w_80()"));
        assert!(production.contains(".max_h_64()"));
        assert!(!production.contains(&old_width));
        assert!(!production.contains(&old_height));
    }

    #[gpui::test]
    fn host_extension_degradation_is_generation_scoped_and_survives_successes(
        cx: &mut TestAppContext,
    ) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            panel.active_generation = 4;
            panel.set_host_extension_degradation(Some("项目命令环境扩展未加载：denied"));
            panel.rpc_error = Some("temporary".to_owned());
            panel.clear_rpc_error();
            assert!(panel.rpc_error.is_none());
            assert_eq!(
                panel.host_extension_degradation.as_deref(),
                Some("项目命令环境扩展未加载：denied")
            );
            assert_eq!(panel.begin_active_generation(), 5);
            assert!(panel.host_extension_degradation.is_none());
            panel.set_host_extension_degradation(Some("still degraded"));
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(visual.debug_bounds("host-extension-degradation").is_some());
        panel.update(cx, |panel, cx| {
            panel.set_host_extension_degradation(None);
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(visual.debug_bounds("host-extension-degradation").is_none());
    }

    #[gpui::test]
    fn tool_restart_failure_clears_busy_state_without_dropping_handle(cx: &mut TestAppContext) {
        let (_visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            panel.control_operation = Some(ControlOperation::Tools);
            panel.rpc_error = None;
            assert!(!panel.apply_runtime_effect(
                RuntimeEffect {
                    sequence: 1,
                    epoch: 2,
                    kind: RuntimeEffectKind::ToolRestartFinished {
                        preset: ToolPreset::ReadOnly,
                        result: Err("spawn failed".to_owned()),
                    },
                },
                cx,
            ));
            assert!(panel.active.is_none());
            assert!(panel.control_operation.is_none());
            assert!(panel.rpc_error.as_deref().is_some_and(|error| {
                error.contains("重新启动活会话") && error.contains("spawn failed")
            }));
        });
    }

    #[gpui::test]
    fn lifecycle_extension_reset_discards_pending_cancelled_responses(cx: &mut TestAppContext) {
        let (_visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, _| {
            panel.extension_ui.apply(
                "old-dialog".into(),
                pi_rpc::ExtensionUiRequest::Confirm {
                    title: "Old".into(),
                    message: "Old process".into(),
                    timeout: None,
                },
            );
            panel
                .pending_extension_responses
                .push(pi_rpc::ExtensionUiResponse::cancelled("already-pending"));
            panel.extension_dialog_open = Some("old-dialog".into());
            panel.clear_extension_ui_for_lifecycle();
            assert!(panel.pending_extension_responses.is_empty());
            assert!(panel.extension_ui.active_dialog().is_none());
            assert!(panel.extension_dialog_open.is_none());
            assert!(panel.extension_dialog_needs_close);
        });
    }

    #[gpui::test]
    fn tool_preset_can_be_selected_before_starting_live_session(cx: &mut TestAppContext) {
        let (_visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            assert!(panel.active.is_none());
            panel.set_tool_preset(ToolPreset::ReadOnly, cx);
            assert_eq!(panel.tool_preset, ToolPreset::ReadOnly);
            assert!(panel.active.is_none());
        });
    }

    /// 停止按钮只在运行态出现，空闲时不占位（规范 1.4 三级操作可见性）。
    #[gpui::test]
    fn abort_button_is_absent_until_running(cx: &mut TestAppContext) {
        let mut visual = render_status(cx, ChatStatus::Ready(document("hello")));
        assert!(visual.debug_bounds("send-live").is_some());
        assert!(visual.debug_bounds("abort-live").is_none());
    }

    #[gpui::test]
    fn composer_input_shell_uses_editor_surface_single_border_and_shadow(cx: &mut TestAppContext) {
        let mut visual = render_status(cx, ChatStatus::Ready(document("hello")));
        let shell = visual
            .debug_bounds("composer-textarea-control")
            .expect("composer input shell missing");
        let viewport = visual
            .debug_bounds("composer-textarea-viewport")
            .expect("composer textarea viewport missing");
        assert!(shell.size.width > px(0.) && shell.size.height > px(0.));
        assert!(shell.top() >= viewport.top() && shell.bottom() <= viewport.bottom());
        visual.update(|_, cx| {
            let style = composer_input_shell_style(false, cx);
            assert_eq!(style.background, cx.theme().background);
            assert_eq!(style.border, cx.theme().border);
            assert_eq!(style.border_layers, 1);
            assert_eq!(style.shadow_layers, 1);
            let focused = composer_input_shell_style(true, cx);
            assert_eq!(focused.background, style.background);
            assert_eq!(focused.border, cx.theme().ring);
            assert_eq!(focused.border_layers, 1);
            assert_eq!(focused.shadow_layers, 1);
        });
    }

    #[gpui::test]
    fn minimum_chat_keeps_composer_visible(cx: &mut TestAppContext) {
        let mut ready = render_status(cx, ChatStatus::Ready(document("hello")));
        let chat = ready.debug_bounds("chat-workspace").unwrap();
        let composer = ready.debug_bounds("live-composer").unwrap();
        assert!(chat.size.width > px(0.) && composer.size.height > px(0.));
        assert!(composer.bottom() <= chat.bottom());
    }

    #[gpui::test]
    fn composer_input_tracks_real_message_column_with_minimap_expanded_and_collapsed(
        cx: &mut TestAppContext,
    ) {
        for width in [640., 1000.] {
            let document = rich_document();
            assert!(!document.minimap.is_empty());
            let (mut visual, panel) = render_status_with_panel_sized(
                cx,
                ChatStatus::Ready(document),
                size(px(width), px(480.)),
            );
            // 首帧由 ChatWindow prepaint 回传消息 pane，后续帧让 ChatPanel 消费 bounds。
            draw_frames(&mut visual, 2);
            panel.update(cx, |panel, _| {
                assert!(panel.message_pane_bounds.is_some());
                assert!(panel.workspace_bounds.is_some());
            });
            let message_column = visual
                .debug_bounds("message-column")
                .expect("message column missing with expanded minimap");
            let input_shell = visual
                .debug_bounds("composer-textarea-control")
                .expect("composer input shell missing");
            assert_eq!(input_shell.origin.x, message_column.origin.x);
            assert_eq!(input_shell.right(), message_column.right());

            panel.update(cx, |panel, cx| panel.toggle_minimap(cx));
            draw_frames(&mut visual, 3);
            assert!(visual.debug_bounds("chat-minimap-collapsed").is_some());
            let collapsed_column = visual
                .debug_bounds("message-column")
                .expect("message column missing with collapsed minimap");
            let collapsed_shell = visual
                .debug_bounds("composer-textarea-control")
                .expect("composer input shell missing after minimap collapse");
            assert_eq!(collapsed_shell.origin.x, collapsed_column.origin.x);
            assert_eq!(collapsed_shell.right(), collapsed_column.right());
        }
    }

    #[gpui::test]
    fn multiline_extension_text_keeps_full_state_and_bounded_composer_region(
        cx: &mut TestAppContext,
    ) {
        assert_eq!(COMPOSER_MAX_ROWS, 8);
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        panel.update(cx, |panel, cx| {
            panel.extension_ui.apply(
                "below".into(),
                pi_rpc::ExtensionUiRequest::SetWidget {
                    widget_key: "below".into(),
                    widget_lines: Some(vec!["below widget".into()]),
                    widget_placement: Some(pi_rpc::WidgetPlacement::BelowEditor),
                },
            );
            cx.notify();
        });
        draw_frames(&mut visual, 3);
        let composer_before = visual
            .debug_bounds("live-composer")
            .expect("composer missing before long text");
        let actions_before = visual
            .debug_bounds("composer-actions")
            .expect("composer actions missing before long text");
        let below_before = visual
            .debug_bounds("extension-widgets-below")
            .expect("below widget missing before long text");
        let expected = (0..40)
            .map(|index| format!("extension line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.composer.update(cx, |input, cx| {
                    input.set_value(expected.clone(), window, cx);
                });
                cx.notify();
            });
        });
        draw_frames(&mut visual, 4);
        let viewport = visual
            .debug_bounds("composer-textarea-viewport")
            .expect("composer textarea viewport missing");
        let control = visual
            .debug_bounds("composer-textarea-control")
            .expect("composer textarea control wrapper missing");
        let composer_after = visual
            .debug_bounds("live-composer")
            .expect("composer missing after long text");
        let actions_after = visual
            .debug_bounds("composer-actions")
            .expect("composer actions missing after long text");
        let below_after = visual
            .debug_bounds("extension-widgets-below")
            .expect("below widget missing after long text");
        let chat = visual
            .debug_bounds("chat-workspace")
            .expect("chat workspace missing");
        let content_column = visual
            .debug_bounds("composer-content-column")
            .expect("composer content column missing");
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.composer.read(cx).value().to_string()),
            expected
        );
        assert!(viewport.size.height > px(0.));
        assert!(control.top() >= viewport.top() && control.bottom() <= viewport.bottom());
        assert!(composer_after.size.height >= composer_before.size.height);
        assert!(composer_after.size.height < px(320.));
        assert!(actions_after.origin.y >= actions_before.origin.y);
        assert!(below_after.origin.y >= below_before.origin.y);
        assert!(viewport.bottom() <= actions_after.top());
        assert!(actions_after.bottom() <= below_after.top());
        assert!(below_after.bottom() <= chat.bottom());
        let message_column = visual
            .debug_bounds("message-column")
            .expect("message column missing");
        assert_eq!(content_column.size.width, message_column.size.width);
        assert_eq!(content_column.origin.x, message_column.origin.x);
    }

    #[test]
    fn clipboard_partition_keeps_successes_and_only_falls_back_to_text_without_images() {
        let text = ClipboardEntry::String(gpui::ClipboardString::new("cells".into()));
        let png = ClipboardEntry::Image(Image::from_bytes(ImageFormat::Png, vec![1]));
        let bmp = ClipboardEntry::Image(Image::from_bytes(ImageFormat::Bmp, vec![2]));
        let accepted = pi_data::DraftImage {
            data: "image".into(),
            mime_type: "image/png".into(),
        };

        let mixed = classify_clipboard_paste_with(vec![text.clone(), png, bmp.clone()], |image| {
            if image.format == ImageFormat::Png {
                Ok(accepted.clone())
            } else {
                Err(pi_data::ImageValidationError::Unsupported)
            }
        });
        assert!(matches!(
            mixed,
            ClipboardPasteDecision::Images { images, warning: Some(_) }
                if images == vec![accepted]
        ));

        let failed_with_text = classify_clipboard_paste_with(vec![text, bmp.clone()], |_| {
            Err(pi_data::ImageValidationError::Unsupported)
        });
        assert!(matches!(
            failed_with_text,
            ClipboardPasteDecision::ImageError { has_text: true, .. }
        ));
        let failed_only = classify_clipboard_paste_with(vec![bmp], |_| {
            Err(pi_data::ImageValidationError::Unsupported)
        });
        assert!(matches!(
            failed_only,
            ClipboardPasteDecision::ImageError {
                has_text: false,
                ..
            }
        ));
    }

    #[gpui::test]
    fn composer_paste_action_captures_png_as_attachment_without_changing_text(
        cx: &mut TestAppContext,
    ) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.composer.update(cx, |input, cx| {
                    input.set_value("keep text", window, cx);
                    input.focus(window, cx);
                });
            });
        });
        draw_frames(&mut visual, 2);

        let png = Image::from_bytes(
            ImageFormat::Png,
            b"\x89PNG\r\n\x1a\nclipboard-fixture".to_vec(),
        );
        visual.write_to_clipboard(ClipboardItem::new_image(&png));
        visual.dispatch_action(Paste);
        draw_frames(&mut visual, 2);

        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.attachments.len(), 1);
            assert_eq!(panel.composer.read(cx).value().as_ref(), "keep text");
        });
    }

    #[gpui::test]
    fn composer_paste_action_propagates_plain_text_to_textarea(cx: &mut TestAppContext) {
        let (mut visual, panel) =
            render_status_with_panel(cx, ChatStatus::Ready(document("hello")));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.composer.update(cx, |input, cx| {
                    input.set_value("before ", window, cx);
                    let cursor = input.value().len();
                    input.set_selected_range(cursor..cursor, cx);
                    input.focus(window, cx);
                });
            });
        });
        draw_frames(&mut visual, 2);

        visual.write_to_clipboard(ClipboardItem::new_string("plain text".to_owned()));
        visual.dispatch_action(Paste);
        draw_frames(&mut visual, 2);

        panel.read_with(cx, |panel, cx| {
            assert!(panel.attachments.is_empty());
            assert_eq!(
                panel.composer.read(cx).value().as_ref(),
                "before plain text"
            );
        });
    }

    #[test]
    fn clipboard_batch_rejection_preserves_limit_error_over_partial_warning() {
        let png = ClipboardEntry::Image(Image::from_bytes(ImageFormat::Png, vec![1]));
        let bmp = ClipboardEntry::Image(Image::from_bytes(ImageFormat::Bmp, vec![2]));
        let accepted = pi_data::DraftImage {
            data: "image".into(),
            mime_type: "image/png".into(),
        };
        let decision = classify_clipboard_paste_with(vec![png, bmp], |image| {
            if image.format == ImageFormat::Png {
                Ok(accepted.clone())
            } else {
                Err(pi_data::ImageValidationError::Unsupported)
            }
        });
        let ClipboardPasteDecision::Images { images, warning } = decision else {
            panic!("PNG + BMP 应分类为成功图片附带 partial warning");
        };
        let batch_result = pi_data::validate_image_batch(pi_data::MAX_ATTACHED_IMAGES, &images);
        assert_eq!(
            clipboard_image_add_feedback(batch_result, warning),
            Some(pi_data::ImageValidationError::TooMany.to_string())
        );
        assert_eq!(
            clipboard_image_add_feedback(Ok(()), Some("部分失败".to_owned())),
            Some("部分失败".to_owned())
        );
    }

    #[test]
    fn fresh_draft_key_migration_moves_content_and_clears_temporary_key() {
        let mut drafts = pi_data::DraftStore::default();
        let draft = pi_data::ComposerDraft {
            text: "unsent".into(),
            images: Vec::new(),
        };
        drafts.set("fresh-1", draft.clone());
        migrate_draft_key(&mut drafts, "fresh-1", "real-session");
        assert_eq!(drafts.get("real-session"), draft);
        assert_eq!(drafts.get("fresh-1"), pi_data::ComposerDraft::default());
    }

    #[gpui::test]
    fn stale_generation_does_not_replace_newer_chat(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let result = captured.clone();
        cx.open_window(size(gpui::px(520.), gpui::px(480.)), move |window, cx| {
            let panel = cx.new(|cx| {
                ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                )
            });
            *result.borrow_mut() = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = captured.borrow().clone().unwrap();
        panel.update(cx, |panel, _| {
            let tab_id = panel.tab_id;
            panel.load_generation = LoadGeneration(2);
            panel.active_generation = 2;
            assert!(!panel.finish_load(
                tab_id,
                LoadGeneration(1),
                "old".to_owned(),
                Ok(document("old"))
            ));
            assert!(matches!(panel.status, ChatStatus::Empty));
            // 已经关掉的标签不该被任何迟到的渲染结果复活。
            assert!(!panel.finish_load(
                tab_id + 999,
                LoadGeneration(2),
                "gone".to_owned(),
                Ok(document("gone"))
            ));
            assert!(matches!(panel.status, ChatStatus::Empty));
            assert!(panel.finish_load(
                tab_id,
                LoadGeneration(2),
                "new".to_owned(),
                Ok(document("new"))
            ));
            assert!(matches!(panel.status, ChatStatus::Ready(_)));
        });
    }

    #[test]
    fn restart_path_prefers_existing_control_file_and_rejects_unpersisted_paths() {
        let directory = tempfile::tempdir().unwrap();
        let control_path = directory.path().join("control.jsonl");
        let history_path = directory.path().join("history.jsonl");
        std::fs::write(&control_path, "").unwrap();
        std::fs::write(&history_path, "").unwrap();
        let mut history = (*document("hello")).clone();
        history.source_path = history_path.clone();
        let controls = SessionControls {
            model: None,
            thinking_level: pi_rpc::ThinkingLevel::Off,
            models: Vec::new(),
            thinking_levels: Vec::new(),
            session_file: Some(control_path.clone()),
            session_id: "session".into(),
            tree: pi_rpc::TreeData {
                tree: Vec::new(),
                leaf_id: None,
            },
            auto_compaction_enabled: true,
            auto_retry_enabled: true,
            is_compacting: false,
        };
        assert_eq!(
            restart_session_path(Some(&controls), &history),
            Some(control_path)
        );

        let missing = directory.path().join("missing.jsonl");
        let mut unpersisted = controls;
        unpersisted.session_file = Some(missing);
        history.source_path = PathBuf::new();
        assert_eq!(restart_session_path(Some(&unpersisted), &history), None);
    }

    #[test]
    fn ctrl_p_is_the_model_cycle_shortcut() {
        let ctrl_p = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-p").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        let ctrl_shift_p = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-shift-p").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        assert!(is_cycle_model_keystroke(&ctrl_p));
        assert!(!is_cycle_model_keystroke(&ctrl_shift_p));
    }

    #[test]
    fn composer_popup_queries_and_image_only_submission_are_pure() {
        assert_eq!(slash_query("/fix"), Some("fix"));
        assert_eq!(slash_query("/fix now"), None);
        let draft = pi_data::image_from_bytes(b"\x89PNG\r\n\x1a\nfixture".to_vec()).unwrap();
        let attachment = attachment_from_draft(draft.clone()).unwrap();
        let submission = build_submission(String::new(), &[attachment]);
        assert!(submission.message.is_empty());
        assert_eq!(submission.images, vec![draft]);
    }

    #[test]
    fn explicit_rejection_restores_but_ambiguous_failure_does_not() {
        let mut drafts = pi_data::DraftStore::default();
        drafts.set(
            "session",
            pi_data::ComposerDraft {
                text: "new typing".into(),
                images: Vec::new(),
            },
        );
        let rejected = pi_data::ComposerDraft {
            text: "rejected".into(),
            images: Vec::new(),
        };
        if should_restore_submission(RequestFailureKind::Rejected) {
            drafts.restore_submission("session", rejected);
        }
        assert_eq!(drafts.get("session").text, "rejected\n\nnew typing");
        let before = drafts.get("session");
        if should_restore_submission(RequestFailureKind::Ambiguous) {
            drafts.restore_submission("session", pi_data::ComposerDraft::default());
        }
        assert_eq!(drafts.get("session"), before);
    }

    #[test]
    fn session_list_refreshes_only_for_successful_fork_or_clone() {
        let controls = SessionControls {
            model: None,
            thinking_level: pi_rpc::ThinkingLevel::Off,
            models: Vec::new(),
            thinking_levels: Vec::new(),
            session_file: None,
            session_id: "fixture".to_owned(),
            tree: pi_rpc::TreeData {
                tree: Vec::new(),
                leaf_id: None,
            },
            auto_compaction_enabled: true,
            auto_retry_enabled: true,
            is_compacting: false,
        };
        assert!(sessions_changed_for_outcome(&ControlOutcome::Forked {
            data: pi_rpc::ForkData {
                text: "fork".to_owned(),
                cancelled: false,
            },
            controls: controls.clone(),
        }));
        assert!(sessions_changed_for_outcome(&ControlOutcome::Cloned {
            data: pi_rpc::CloneData { cancelled: false },
            controls,
        }));
        assert!(sessions_changed_for_outcome(
            &ControlOutcome::RebindCalibrationFailed {
                operation: ControlOperation::Fork,
                message: "success with warning".to_owned(),
                fork_data: None,
            }
        ));
        assert!(!sessions_changed_for_outcome(
            &ControlOutcome::ForkCancelled(pi_rpc::ForkData {
                text: String::new(),
                cancelled: true,
            })
        ));
        assert!(!sessions_changed_for_outcome(
            &ControlOutcome::CloneCancelled
        ));
        assert!(!sessions_changed_for_outcome(&ControlOutcome::RetryAborted));
    }

    #[test]
    fn session_controls_only_enable_for_idle_non_busy_sessions() {
        assert!(session_controls_enabled(Some(LivePhase::Idle), false));
        for phase in [
            None,
            Some(LivePhase::Running),
            Some(LivePhase::Stopping),
            Some(LivePhase::Error),
        ] {
            assert!(!session_controls_enabled(phase, false));
        }
        assert!(!session_controls_enabled(Some(LivePhase::Idle), true));
        assert!(!abort_retry_disabled(false));
        assert!(abort_retry_disabled(true));
    }

    #[test]
    fn selection_carries_real_session_path() {
        let event = SessionSelected {
            id: "id".to_owned(),
            path: PathBuf::from("C:/sessions/id.jsonl"),
            cwd: PathBuf::from("C:/project"),
            title: "title".to_owned(),
        };
        assert_eq!(event.path.file_name().unwrap(), "id.jsonl");
    }

    // ---------- R22 视觉审查整改：可见状态的机械验证 ----------
    //
    // 本轮走 CODE_ONLY 兜底、没有截图，这几条是新增可见状态的唯一机械证据。
    // 覆盖边界：都直接调用 `report_backpressure`，因此**未覆盖** `rpc_error_protected`
    // 在 9 处 effect 写入点上的赋值是否齐全，也未覆盖用户操作路径的端到端行为
    // （`submit` / `abort` 需要一个会拒绝 dispatch 的 `SessionHandle`，而 app 测试
    // 二进制里没有可用的 pi 子进程 fixture）。

    fn backpressure_stats(
        dropped_results: u64,
        dropped_jobs: u64,
    ) -> pi_runtime::BackpressureStats {
        pi_runtime::BackpressureStats {
            dropped_results,
            dropped_jobs,
            ..pi_runtime::BackpressureStats::default()
        }
    }

    #[gpui::test]
    fn backpressure_note_has_its_own_slot_and_expires_on_its_own(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("hi")));
        panel.update(cx, |panel, cx| {
            // 持续成立的启动降级先占住它自己的槽位。
            panel.set_host_extension_degradation(Some("项目命令环境扩展未加载：denied"));
            panel.report_backpressure(backpressure_stats(0, 2));
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(
            visual.debug_bounds("backpressure-note").is_some(),
            "背压弱提示必须有自己的一行"
        );
        assert!(
            visual.debug_bounds("host-extension-degradation").is_some(),
            "瞬时背压提示不得顶掉整会话成立的启动降级诊断"
        );

        panel.update(cx, |panel, cx| {
            // 可读窗口内即使没有新增计数也不能立刻消失，否则只活一帧、还会让下方抖动。
            panel.report_backpressure(backpressure_stats(0, 2));
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(
            visual.debug_bounds("backpressure-note").is_some(),
            "最短驻留窗口内必须保持可见"
        );

        panel.update(cx, |panel, cx| {
            // 把驻留窗口拨到已过期，再报一帧无新增计数：此时必须自行退场。
            panel.backpressure_note_until = Some(std::time::Instant::now());
            panel.report_backpressure(backpressure_stats(0, 2));
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(
            visual.debug_bounds("backpressure-note").is_none(),
            "过了可读窗口就必须退场，不能驻留到会话切换"
        );
        assert!(
            visual.debug_bounds("host-extension-degradation").is_some(),
            "启动降级诊断不受背压提示退场影响"
        );
    }

    #[gpui::test]
    fn backpressure_alert_appends_to_a_protected_error_exactly_once(cx: &mut TestAppContext) {
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("hi")));
        panel.update(cx, |panel, cx| {
            // 模拟用户操作路径写入的具体失败原因（提交被拒 / 停止失败走的就是这条路），
            // 它不经过 apply_snapshot，正是上一轮审查指出的「保护范围之外」的场景。
            panel.rpc_error = Some("命令队列已满（上限 32），请等待当前请求完成后重试".to_owned());
            panel.rpc_error_protected = true;
            panel.report_backpressure(backpressure_stats(3, 0));
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        let error = panel.read_with(cx, |panel, _| panel.rpc_error.clone().unwrap());
        assert!(
            error.contains("命令队列已满"),
            "具体失败原因不得被顶掉：{error}"
        );
        assert!(error.contains("3 条操作结果未能送达界面"), "{error}");
        assert!(visual.debug_bounds("live-error").is_some());

        let appended_once = error.clone();
        panel.update(cx, |panel, cx| {
            // 再来一批淘汰：已经追加过就不再重复追加，否则横幅会越长越离谱。
            panel.report_backpressure(backpressure_stats(9, 0));
            cx.notify();
        });
        let error = panel.read_with(cx, |panel, _| panel.rpc_error.clone().unwrap());
        assert_eq!(error, appended_once, "同一条错误只追加一次背压说明");
    }

    #[gpui::test]
    fn error_banner_and_attachment_strip_coexist_without_overlap_at_1280x820(
        cx: &mut TestAppContext,
    ) {
        // 覆盖边界（如实标注）：本用例**不驱动真实提交路径**，只锁「错误横幅在场时
        // 附件条与 composer 各行不重叠」这一布局性质——既有的附件布局用例跑在
        // 1000×1000 且没有横幅在场。被拒后「保留草稿」「收起浮层」的行为本身
        // 需要一个会拒绝 dispatch 的 SessionHandle，app 测试二进制内无此 fixture，
        // 因此未被机械覆盖。
        let (mut visual, panel) = render_status_with_panel_sized(
            cx,
            ChatStatus::Ready(document("hi")),
            size(px(1280.), px(820.)),
        );
        panel.update(cx, |panel, cx| {
            let png = vec![0x89_u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x66];
            let draft = pi_data::image_from_bytes(png).unwrap();
            panel.attachments = vec![attachment_from_draft(draft).unwrap()];
            panel.rpc_success = None;
            panel.rpc_error = Some("命令队列已满（上限 32），请等待当前请求完成后重试".to_owned());
            panel.rpc_error_protected = true;
            cx.notify();
        });
        draw_frames(&mut visual, 2);
        assert!(
            visual.debug_bounds("live-error").is_some(),
            "错误横幅必须出现"
        );
        assert!(
            visual.debug_bounds("composer-attachments").is_some(),
            "附件条必须与错误横幅共存"
        );
        assert_attachment_composer_rows_do_not_overlap(&mut visual, true);
    }

    /// 第十一轮独立审查 P1/P2：调度器正在起 / 停这个会话时，会碰进程的入口一律锁死。
    ///
    /// 启动窗口里标签上的 `active` 还是空的，Manager 侧 `Starting` 也还没装上 `entry`，
    /// 于是「改工具预设」会一路走到只改会话描述那条分支——界面写成 ReadOnly，
    /// 而正在起的那个进程早就拿着旧预设出发了。按钮的 `disabled` 与 handler 的早退
    /// 共用 `control_busy` 这一个判据，两边不会再各走各的。
    #[gpui::test]
    fn runtime_controls_are_locked_while_a_scheduler_job_owns_the_session(cx: &mut TestAppContext) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (mut visual, panel) = render_status_with_panel(cx, ChatStatus::Ready(document("hi")));
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                register_parked_session(panel, 0, workspace.path());
                let session = panel.session.expect("已登记会话");
                let preset_in_manager = |panel: &ChatPanel| {
                    panel
                        .runtime_manager
                        .session_descriptor(session)
                        .map(|descriptor| descriptor.tool_preset)
                };
                assert!(panel.active.is_none());
                assert_eq!(panel.tool_preset, ToolPreset::Inherit);

                panel.scheduler_job = Some("启动中…");
                panel.set_tool_preset(ToolPreset::ReadOnly, cx);
                assert_eq!(
                    panel.tool_preset,
                    ToolPreset::Inherit,
                    "调度作业在飞时不得改 UI 上的预设"
                );
                assert_eq!(
                    preset_in_manager(panel),
                    Some(ToolPreset::Inherit),
                    "更不得改会话描述：正在起的进程用的就是那一份"
                );

                // 同一个判据也挡住其它控制类操作，且不留下误导性的错误横幅。
                panel.set_auto_compaction(true, cx);
                assert!(panel.control_operation.is_none());
                assert!(panel.rpc_error.is_none(), "{:?}", panel.rpc_error);

                // 作业落地后照常可改，并且必须同步到 Manager 的描述上。
                panel.scheduler_job = None;
                panel.set_tool_preset(ToolPreset::ReadOnly, cx);
                assert_eq!(panel.tool_preset, ToolPreset::ReadOnly);
                assert_eq!(preset_in_manager(panel), Some(ToolPreset::ReadOnly));
            });
        });
    }
}
