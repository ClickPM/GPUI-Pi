use std::{collections::HashSet, path::PathBuf, sync::Arc};

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

pub struct ChatPanel {
    focus_handle: FocusHandle,
    runtime_manager: RuntimeManager,
    active_generation: u64,
    active: Option<SessionHandle>,
    active_epoch: u64,
    applied_revision: u64,
    effect_cursor: u64,
    /// 上一次已上报的运行时背压计数，用于只在新增时提示。
    backpressure: pi_runtime::BackpressureStats,
    session: SessionUiState,
    composer: gpui::Entity<TextareaState>,
    drafts: pi_data::DraftStore,
    model_names: Arc<std::collections::HashMap<String, String>>,
    popup: Option<ComposerPopup>,
    popup_index: usize,
    file_index: Option<pi_data::FileIndex>,
    workspace_bounds: Option<Bounds<Pixels>>,
    message_pane_bounds: Option<Bounds<Pixels>>,
    extension_widgets_above_scroll: ScrollHandle,
    extension_widgets_below_scroll: ScrollHandle,
    extension_dialog_body_focus: Option<FocusHandle>,
    extension_dialog_footer_focus: Option<FocusHandle>,
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
    status: ChatStatus,
    load_generation: u64,
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
    fn new(list_state: ListState) -> Self {
        Self {
            status: ChatStatus::Empty,
            load_generation: 0,
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

impl std::ops::Deref for ChatPanel {
    type Target = SessionUiState;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl std::ops::DerefMut for ChatPanel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
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
        // 首次静态会话允许一次全量测量，确保长列表首次出现时 scrollbar 即为精确高度。
        let list_state = ListState::new(0, ListAlignment::Top, px(1200.)).measure_all();
        let scroll_state = list_state.clone();
        let panel = cx.weak_entity();
        list_state.set_scroll_handler(move |event, _, cx| {
            let attached = event.is_following_tail;
            let scroll_state = scroll_state.clone();
            let panel = panel.clone();
            // ListState 在回调时持有可变借用；延后读取/更新，避免 RefCell 重入。
            cx.defer(move |cx| {
                let attached = attached || scroll_state.is_scrolled_to_end().unwrap_or(true);
                let _ = panel.update(cx, |panel, cx| {
                    if panel.tail_attached != attached {
                        panel.tail_attached = attached;
                        cx.notify();
                    }
                });
            });
        });
        Self {
            focus_handle: cx.focus_handle(),
            runtime_manager,
            active_generation: 0,
            active: None,
            active_epoch: 0,
            applied_revision: 0,
            effect_cursor: 0,
            backpressure: pi_runtime::BackpressureStats::default(),
            session: SessionUiState::new(list_state),
            composer,
            drafts: pi_data::DraftStore::default(),
            model_names: Arc::new(std::collections::HashMap::new()),
            popup: None,
            popup_index: 0,
            file_index: None,
            workspace_bounds: None,
            message_pane_bounds: None,
            extension_widgets_above_scroll: ScrollHandle::default(),
            extension_widgets_below_scroll: ScrollHandle::default(),
            extension_dialog_body_focus: None,
            extension_dialog_footer_focus: None,
            _composer_subscription: subscription,
            probe: None,
        }
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
        if let Some(key) = self.draft_key.clone() {
            self.drafts.set(key, self.current_draft(cx));
        }
    }

    fn prepare_draft_restore(&mut self) {
        let draft = self
            .draft_key
            .as_deref()
            .map(|key| self.drafts.get(key))
            .unwrap_or_default();
        self.pending_draft_restore = true;
        self.attachments = draft
            .images
            .into_iter()
            .filter_map(attachment_from_draft)
            .collect();
    }

    fn start_file_index(&self, generation: u64, cwd: PathBuf, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |panel, cx| {
            let result = executor
                .spawn(async move { pi_data::build_file_index(&cwd) })
                .await;
            let _ = panel.update(cx, |panel, cx| {
                if generation == panel.load_generation {
                    panel.file_index = Some(result);
                    panel.refresh_popup(cx);
                    cx.notify();
                }
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

    fn set_host_extension_degradation(&mut self, diagnostic: Option<&str>) {
        self.host_extension_degradation = diagnostic.map(str::to_owned);
    }

    pub fn load_selection(
        &mut self,
        selection: SessionSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_current_draft(cx);
        self.reset_extension_ui(window, cx);
        self.load_generation = self.load_generation.wrapping_add(1);
        self.begin_active_generation();
        let generation = self.load_generation;
        if let Some(active) = self.active.take() {
            self.runtime_manager.stop_user(active.runtime_id());
        }
        self.status = ChatStatus::Loading {
            title: selection.title.clone(),
        };
        self.rpc_success = None;
        self.rpc_error = None;
        self.host_extension_degradation = None;
        self.backpressure_note = None;
        self.backpressure_note_until = None;
        self.fresh_session = false;
        self.draft_key = Some(selection.id.clone());
        self.composer_cwd = Some(selection.cwd.clone());
        self.file_index = None;
        self.slash_commands.clear();
        self.controls = None;
        self.model_names = Arc::new(std::collections::HashMap::new());
        self.tool_preset = ToolPreset::Inherit;
        self.control_operation = None;
        self.branch_tree = None;
        self.branch_preview_leaf = None;
        self.branch_preview_document = None;
        self.retry_status = None;
        self.compacting = false;
        self.popup = None;
        self.popup_index = 0;
        self.prepare_draft_restore();
        self.start_file_index(generation, selection.cwd.clone(), cx);
        self.tail_attached = true;
        self.follow_requested = false;
        self.minimap_visible = true;
        self.workspace_bounds = None;
        self.message_pane_bounds = None;
        self.expanded_tools.clear();
        self.expanded_processes.clear();
        // 新会话使用独立 ListState；首次加载仍只执行一次静态全量测量。
        self.list_state = ListState::new(0, ListAlignment::Top, px(1200.)).measure_all();
        let scroll_state = self.list_state.clone();
        let panel = cx.weak_entity();
        self.list_state.set_scroll_handler(move |event, _, cx| {
            let attached = event.is_following_tail;
            let scroll_state = scroll_state.clone();
            let panel = panel.clone();
            cx.defer(move |cx| {
                let attached = attached || scroll_state.is_scrolled_to_end().unwrap_or(true);
                let _ = panel.update(cx, |panel, cx| {
                    if panel.tail_attached != attached {
                        panel.tail_attached = attached;
                        cx.notify();
                    }
                });
            });
        });
        self.list_items.clear();
        cx.notify();
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
                if panel.finish_load(generation, title, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn start_new_session(
        &mut self,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let generation = self.active_generation.wrapping_add(1);
        let document = ConversationDocument {
            session_id: format!("fresh-{generation}"),
            source_path: PathBuf::new(),
            cwd: cwd.clone(),
            messages: Arc::from([]),
            items: Arc::from([]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        };
        let active = self.runtime_manager.start_fresh(
            official_binary(),
            cwd.clone(),
            document.clone(),
            ToolPreset::Inherit,
            pi_data::agent_dir(),
        )?;
        let receiver = active.subscribe_dirty();

        self.save_current_draft(cx);
        self.reset_extension_ui(window, cx);
        if let Some(old) = self.active.take() {
            self.runtime_manager.stop_user(old.runtime_id());
        }
        let reset = reset_session_scoped_state(
            self.load_generation,
            &mut self.file_index,
            &mut self.popup,
            &mut self.popup_index,
        );
        self.load_generation = reset.load_generation;
        let load_generation = self.load_generation;
        self.active_generation = generation;
        self.status = ChatStatus::Ready(Arc::new(document.clone()));
        self.sync_list_document(&document, true);
        self.list_state.reset(0);
        self.list_items.clear();
        self.install_active(active);
        self.spawn_dirty_pump(receiver, window.window_handle(), cx);
        self.draft_key = Some(format!("fresh-{generation}"));
        let draft_key = self.draft_key.clone().unwrap_or_default();
        self.drafts.clear(&draft_key);
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.composer_cwd = Some(cwd.clone());
        self.start_file_index(load_generation, cwd, cx);
        self.attachments.clear();
        self.slash_commands.clear();
        self.controls = None;
        self.model_names = Arc::new(std::collections::HashMap::new());
        self.tool_preset = ToolPreset::Inherit;
        self.control_operation = None;
        self.branch_tree = None;
        self.branch_preview_leaf = None;
        self.branch_preview_document = None;
        self.retry_status = None;
        self.compacting = false;
        self.rpc_success = None;
        self.rpc_error = None;
        self.host_extension_degradation = None;
        self.backpressure_note = None;
        self.backpressure_note_until = None;
        self.fresh_session = true;
        self.tail_attached = true;
        self.follow_requested = false;
        self.expanded_tools.clear();
        self.expanded_processes.clear();
        cx.notify();
        Ok(())
    }

    fn finish_load(
        &mut self,
        generation: u64,
        title: String,
        result: Result<Arc<ConversationDocument>, String>,
    ) -> bool {
        if generation != self.load_generation {
            return false;
        }
        self.status = match result {
            Ok(document) => {
                self.sync_list_document(&document, true);
                self.list_state.scroll_to_end();
                ChatStatus::Ready(document)
            }
            Err(message) => ChatStatus::Error { title, message },
        };
        true
    }

    fn start_live(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.is_some() || self.control_operation.is_some() {
            return;
        }
        let ChatStatus::Ready(history) = &self.status else {
            return;
        };
        let history = history.clone();
        self.begin_active_generation();
        let session_path = history.source_path.clone();
        let cwd = session_cwd(&session_path).unwrap_or_else(|| PathBuf::from("."));
        match self.runtime_manager.start_session(
            official_binary(),
            session_path,
            cwd,
            (*history).clone(),
            self.tool_preset,
            pi_data::agent_dir(),
        ) {
            Ok(active) => {
                let receiver = active.subscribe_dirty();
                self.install_active(active);
                self.rpc_success = None;
                self.rpc_error = None;
                self.spawn_dirty_pump(receiver, window.window_handle(), cx);
            }
            Err(error) => {
                self.rpc_success = None;
                self.rpc_error = Some(error);
            }
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

    fn spawn_dirty_pump(
        &self,
        receiver: std::sync::mpsc::Receiver<pi_runtime::Dirty>,
        window_handle: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let (tx, mut rx) = mpsc::unbounded();
        std::thread::Builder::new()
            .name("pi-runtime-dirty-bridge".into())
            .spawn(move || {
                while let Ok(dirty) = receiver.recv() {
                    if tx.unbounded_send(dirty).is_err() {
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
                                let should_stop = panel.pull_runtime_snapshot(cx);
                                panel.process_extension_ui(window, cx);
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
        if snapshot.runtime_id != active.runtime_id() || snapshot.revision <= self.applied_revision
        {
            return false;
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
        self.apply_snapshot(snapshot, cx)
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
                            && let (Some(key), Some(submission)) =
                                (self.draft_key.clone(), submission)
                        {
                            let restored = self.drafts.restore_submission(
                                &key,
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
                            if let Some(old_key) = self.draft_key.clone() {
                                migrate_draft_key(&mut self.drafts, &old_key, &controls.session_id);
                            }
                            self.draft_key = Some(controls.session_id.clone());
                            if controls.session_file.is_some() {
                                self.fresh_session = false;
                                cx.emit(SessionsChanged);
                            }
                        }
                        self.apply_controls(controls);
                    }
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
            if let Some(key) = self.draft_key.clone() {
                self.drafts.set(key, self.current_draft(cx));
            }
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
        let executor = cx.background_executor().clone();
        cx.spawn(async move |panel, cx| {
            let result = executor.spawn(async move { sender.send(response) }).await;
            let _ = panel.update(cx, |panel, cx| {
                if let Err(error) = result {
                    panel.rpc_error = Some(format!(
                        "Extension UI 响应 {id} 未写回，已丢弃并继续队列：{error}"
                    ));
                }
                cx.notify();
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
            let timer = cx.background_executor().clone();
            cx.spawn(async move |_, cx| {
                timer
                    .timer(deadline.saturating_duration_since(std::time::Instant::now()))
                    .await;
                let _ = panel.update(cx, |panel, cx| {
                    if panel.extension_ui.active_dialog().is_some_and(|dialog| {
                        dialog.id == timeout_id && dialog.sequence == timeout_sequence
                    }) {
                        panel.extension_dialog_needs_close = true;
                        panel.finish_extension_dialog(
                            &timeout_id,
                            pi_rpc::ExtensionUiResponse::cancelled(&timeout_id),
                            cx,
                        );
                        panel.rpc_error = Some("Extension UI 请求已超时".to_owned());
                        cx.notify();
                    }
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

    fn reset_extension_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancelled = self.extension_ui.drain_cancelled_dialogs();
        for response in cancelled {
            self.send_extension_response(response, cx);
        }
        if self.extension_dialog_open.take().is_some() {
            self.request_extension_dialog_close(window, cx);
        } else {
            self.clear_extension_dialog_focus();
        }
        self.extension_ui.reset();
        self.extension_response_sender = None;
        self.window_title = "GPUI-Pi".to_owned();
        window.set_window_title("GPUI-Pi");
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
                    self.draft_key.clone().unwrap_or_default(),
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
                        self.draft_key.clone().unwrap_or_default(),
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
        if self.control_operation.is_some() || preset == self.tool_preset {
            return;
        }
        let Some(active) = self.active.take() else {
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
        if self.control_operation.is_some() {
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
        })
        .detach();
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
        let receiver =
            cx.prompt_for_new_path(&start, Some(&format!("pi-session-{session_id}.html")));
        cx.spawn_in(window, async move |panel, cx| {
            let destination = receiver.await.ok().into_iter().flatten().flatten().next();
            let Some(destination) = destination else {
                return;
            };
            let _ = panel.update(cx, |panel, cx| {
                panel.begin_control(
                    ControlOperation::ExportHtml,
                    ControlRequest::ExportHtml {
                        output_path: destination,
                    },
                    cx,
                );
            });
        })
        .detach();
    }

    fn composer_changed(&mut self, input: &gpui::Entity<TextareaState>, cx: &mut Context<Self>) {
        if self.pending_draft_restore {
            self.pending_draft_restore = false;
        }
        if let Some(key) = self.draft_key.clone() {
            self.drafts.set(key, self.current_draft(cx));
        }
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
        if let Some(key) = self.draft_key.clone() {
            self.drafts.clear(&key);
        }
        cx.notify();
    }

    fn choose_images(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
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
                    panel.start_attach_paths(paths, cx);
                });
            });
        })
        .detach();
    }

    fn start_attach_paths(&self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let generation = self.load_generation;
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
        if let Some(key) = self.draft_key.clone() {
            self.drafts.set(key, self.current_draft(cx));
        }
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
            if let Some(key) = self.draft_key.clone() {
                self.drafts.set(key, self.current_draft(cx));
            }
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
        if self.pending_draft_restore {
            let input = self.composer.clone();
            let text = self
                .draft_key
                .as_deref()
                .map(|key| self.drafts.get(key).text)
                .unwrap_or_default();
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
        let controls_enabled = session_controls_enabled(phase, self.control_operation.is_some());
        let tools_enabled = !running
            && !stopping
            && self.control_operation.is_none()
            && matches!(self.status, ChatStatus::Ready(_));
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
                this.start_attach_paths(paths.paths().to_vec(), cx);
            }))
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .size_full()
                    .min_h_0()
                    .child(div().flex_1().min_h_0().child(content))
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
                                        .disabled(abort_retry_disabled(
                                            self.control_operation.is_some(),
                                        ))
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
                                    .child(
                                        Button::new("start-live-session")
                                            .ghost()
                                            .small()
                                            .label(if live_started {
                                                "活会话已启动"
                                            } else {
                                                "启动活会话"
                                            })
                                            .disabled(
                                                live_started
                                                    || self.control_operation.is_some()
                                                    || !matches!(self.status, ChatStatus::Ready(_)),
                                            )
                                            .on_click(cx.listener(Self::start_live)),
                                    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionScopedReset {
    load_generation: u64,
}

fn reset_session_scoped_state(
    load_generation: u64,
    file_index: &mut Option<pi_data::FileIndex>,
    popup: &mut Option<ComposerPopup>,
    popup_index: &mut usize,
) -> SessionScopedReset {
    *file_index = None;
    *popup = None;
    *popup_index = 0;
    SessionScopedReset {
        load_generation: load_generation.wrapping_add(1),
    }
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
            panel.update(cx, |panel, cx| panel.reset_extension_ui(window, cx));
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
                panel.update(cx, |panel, cx| panel.reset_extension_ui(window, cx));
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
    fn fresh_session_reset_clears_project_scoped_index_and_popup_state() {
        let mut file_index = Some(pi_data::FileIndex {
            entries: vec![pi_data::FileIndexEntry {
                path: "old-project.rs".into(),
                is_dir: false,
            }],
            truncated: false,
        });
        let mut popup = Some(ComposerPopup::Slash(Vec::new()));
        let mut popup_index = 7;
        let reset = reset_session_scoped_state(10, &mut file_index, &mut popup, &mut popup_index);
        assert!(file_index.is_none());
        assert!(popup.is_none());
        assert_eq!(popup_index, 0);
        assert_eq!(reset.load_generation, 11);
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
            panel.load_generation = 2;
            panel.active_generation = 2;
            assert!(!panel.finish_load(1, "old".to_owned(), Ok(document("old"))));
            assert!(matches!(panel.status, ChatStatus::Empty));
            assert!(panel.finish_load(2, "new".to_owned(), Ok(document("new"))));
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
}
