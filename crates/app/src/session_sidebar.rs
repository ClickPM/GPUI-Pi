use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, Icon, IconName,
    InteractiveElementExt as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::{DialogAction, DialogClose, DialogFooter},
    dock::{Panel, PanelControl, PanelEvent},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    scroll::ScrollableElement as _,
    tooltip::Tooltip,
    v_flex,
};
use pi_data::{
    ProjectSessionView, RunningSessionOverlay, SessionSummary, SessionView, delete_leaf_session,
    export_session_jsonl, list_sessions, rename_session,
};

use crate::live_session::{HistoricalHtmlExportRequest, RuntimeManager, official_binary};
use crate::panels::LayoutProbe;
use crate::trust_prompt::prompt_project_trust;

#[derive(Debug, Clone, PartialEq)]
pub enum SidebarStatus {
    Loading,
    Ready(Vec<ProjectSessionView>),
    Error(String),
}

#[derive(Clone)]
pub struct SessionSelected {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeSelected {
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct NewSessionRequested {
    pub cwd: PathBuf,
}

const PROJECT_SESSION_VISIBLE_ROWS: usize = 8;

pub struct SessionSidebar {
    focus_handle: FocusHandle,
    status: SidebarStatus,
    selected_id: Option<String>,
    sessions_root: Option<PathBuf>,
    agent_dir: Option<PathBuf>,
    running: RunningSessionOverlay,
    summaries: Vec<SessionSummary>,
    diagnostics_count: usize,
    busy_actions: HashSet<String>,
    rename_input: gpui::Entity<InputState>,
    rename_target: Option<String>,
    load_generation: u64,
    /// 当前鼠标悬停的会话行。行内低频操作（重命名/导出/删除）只在悬停行渲染，
    /// 规范 S-9：一行默认不铺开三个图标按钮。
    hovered_id: Option<String>,
    browsing_cwd: Option<PathBuf>,
    worktrees: Option<pi_data::WorktreeSnapshot>,
    worktree_error: Option<String>,
    worktree_generation: u64,
    worktree_busy: bool,
    worktree_expanded: bool,
    /// linked worktree 的移除操作只在对应行 hover 时出现（规范 S-9）。
    hovered_worktree_index: Option<usize>,
    worktree_input: gpui::Entity<InputState>,
    collapsed_projects: HashSet<String>,
    project_scrolls: HashMap<String, ScrollHandle>,
    pending_reveal_project: Option<String>,
    probe: Option<LayoutProbe>,
    runtime_manager: RuntimeManager,
}

impl SessionSidebar {
    #[cfg(not(test))]
    pub fn new(
        runtime_manager: RuntimeManager,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("会话名称"));
        let agent_dir = pi_data::agent_dir();
        let sessions_root = agent_dir.as_ref().map(|dir| dir.join("sessions"));
        let worktree_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("新建 worktree 分支"));
        let mut sidebar = Self {
            focus_handle: cx.focus_handle(),
            status: SidebarStatus::Loading,
            selected_id: None,
            sessions_root,
            agent_dir,
            running: RunningSessionOverlay::default(),
            summaries: Vec::new(),
            diagnostics_count: 0,
            busy_actions: HashSet::new(),
            rename_input,
            rename_target: None,
            load_generation: 0,
            hovered_id: None,
            browsing_cwd: None,
            worktrees: None,
            worktree_error: None,
            worktree_generation: 0,
            worktree_busy: false,
            worktree_expanded: false,
            hovered_worktree_index: None,
            worktree_input,
            collapsed_projects: HashSet::new(),
            project_scrolls: HashMap::new(),
            pending_reveal_project: None,
            probe: None,
            runtime_manager,
        };
        sidebar.refresh(window, cx);
        sidebar
    }

    #[cfg(test)]
    pub(crate) fn new_empty(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            status: SidebarStatus::Ready(Vec::new()),
            selected_id: None,
            sessions_root: None,
            agent_dir: None,
            running: RunningSessionOverlay::default(),
            summaries: Vec::new(),
            diagnostics_count: 0,
            busy_actions: HashSet::new(),
            rename_input: cx.new(|cx| InputState::new(window, cx).placeholder("会话名称")),
            rename_target: None,
            load_generation: 0,
            hovered_id: None,
            browsing_cwd: None,
            worktrees: None,
            worktree_error: None,
            worktree_generation: 0,
            worktree_busy: false,
            worktree_expanded: false,
            hovered_worktree_index: None,
            worktree_input: cx
                .new(|cx| InputState::new(window, cx).placeholder("新建 worktree 分支")),
            collapsed_projects: HashSet::new(),
            project_scrolls: HashMap::new(),
            pending_reveal_project: None,
            probe: None,
            runtime_manager: RuntimeManager::new(Default::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_probe(mut self, probe: LayoutProbe) -> Self {
        self.probe = Some(probe);
        self
    }

    pub(crate) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        self.status = SidebarStatus::Loading;
        cx.notify();
        let Some(root) = self.sessions_root.clone() else {
            self.status = SidebarStatus::Error("无法定位 pi agent 数据目录".to_owned());
            return;
        };
        let executor = cx.background_executor().clone();
        let running = self.running.clone();
        cx.spawn_in(window, async move |sidebar, cx| {
            let (summaries, diagnostics_count, view) = executor
                .spawn(async move {
                    let listed = list_sessions(root);
                    let diagnostics_count = listed.diagnostics.len();
                    let view = pi_data::build_session_view(listed.sessions.clone(), &running);
                    (listed.sessions, diagnostics_count, view)
                })
                .await;
            let applied = sidebar
                .update(cx, |sidebar, cx| {
                    let applied =
                        sidebar.finish_refresh(generation, summaries, diagnostics_count, view);
                    if applied {
                        cx.notify();
                    }
                    applied
                })
                .unwrap_or(false);
            if applied && diagnostics_count > 0 {
                let _ = cx.update(|window, cx| {
                    window.push_notification(
                        Notification::warning(format!(
                            "会话扫描跳过了 {diagnostics_count} 个异常文件或目录"
                        )),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    fn finish_refresh(
        &mut self,
        generation: u64,
        summaries: Vec<SessionSummary>,
        diagnostics_count: usize,
        view: Vec<ProjectSessionView>,
    ) -> bool {
        if self.load_generation != generation {
            return false;
        }
        let available = view
            .iter()
            .map(|project| project.key.clone())
            .collect::<HashSet<_>>();
        self.collapsed_projects
            .retain(|key| available.contains(key));
        self.project_scrolls
            .retain(|key, _| available.contains(key));
        for key in &available {
            self.project_scrolls.entry(key.clone()).or_default();
        }
        self.summaries = summaries;
        self.diagnostics_count = diagnostics_count;
        self.status = SidebarStatus::Ready(view);
        true
    }

    fn handle_refresh(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh(window, cx);
    }

    fn select_session(
        &mut self,
        session: &SessionView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_id = Some(session.id.clone());
        self.pending_reveal_project = project_key_for_session(&self.status, &session.id);
        self.set_browsing_cwd(Some(session.cwd.clone()), cx);
        cx.emit(SessionSelected {
            id: session.id.clone(),
            path: session.path.clone(),
            cwd: session.cwd.clone(),
            title: session.title.clone(),
        });
        prompt_project_trust(self.agent_dir.clone(), &session.cwd, window, cx);
        cx.notify();
    }

    pub fn set_browsing_cwd(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) {
        if self
            .browsing_cwd
            .as_ref()
            .map(pi_data::project_identity_key)
            == cwd.as_ref().map(pi_data::project_identity_key)
        {
            return;
        }
        self.browsing_cwd = cwd;
        self.reload_worktrees(cx);
    }

    fn reload_worktrees(&mut self, cx: &mut Context<Self>) {
        self.worktree_generation = self.worktree_generation.wrapping_add(1);
        let generation = self.worktree_generation;
        self.worktrees = None;
        self.worktree_error = None;
        // 刷新后的索引属于新快照，旧 hover 不得误点亮另一行。
        self.hovered_worktree_index = None;
        let Some(cwd) = self.browsing_cwd.clone() else {
            cx.notify();
            return;
        };
        let cwd_key = pi_data::project_identity_key(&cwd);
        let executor = cx.background_executor().clone();
        cx.spawn(async move |sidebar, cx| {
            let result = executor
                .spawn(async move { pi_data::list_worktrees(cwd) })
                .await;
            let _ = sidebar.update(cx, |sidebar, cx| {
                if sidebar.finish_worktree_refresh(generation, &cwd_key, result) {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_worktree_refresh(
        &mut self,
        generation: u64,
        cwd_key: &str,
        result: Result<pi_data::WorktreeSnapshot, pi_data::GitError>,
    ) -> bool {
        if generation != self.worktree_generation
            || self
                .browsing_cwd
                .as_ref()
                .map(pi_data::project_identity_key)
                .as_deref()
                != Some(cwd_key)
        {
            return false;
        }
        match result {
            Ok(snapshot) => {
                self.worktrees = Some(snapshot);
                self.worktree_error = None;
            }
            Err(error) => {
                self.worktrees = None;
                self.worktree_error = Some(error.to_string());
            }
        }
        true
    }

    fn switch_worktree(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        self.set_browsing_cwd(Some(cwd.clone()), cx);
        cx.emit(WorktreeSelected { cwd });
    }

    fn create_worktree(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree_busy {
            return;
        }
        let Some(cwd) = self.browsing_cwd.clone() else {
            return;
        };
        let branch = self.worktree_input.read(cx).value().trim().to_owned();
        if branch.is_empty() {
            return;
        }
        self.worktree_busy = true;
        cx.notify();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |sidebar, cx| {
            let result = executor
                .spawn(async move { pi_data::add_worktree(cwd, &branch) })
                .await;
            let _ = cx.update(|window, cx| {
                let _ = sidebar.update(cx, |sidebar, cx| {
                    sidebar.worktree_busy = false;
                    match result {
                        Ok(worktree) => {
                            sidebar
                                .worktree_input
                                .update(cx, |input, cx| input.set_value("", window, cx));
                            sidebar.switch_worktree(worktree.path, cx);
                            window.push_notification(Notification::success("worktree 已创建"), cx);
                        }
                        Err(error) => {
                            window.push_notification(Notification::error(error.to_string()), cx)
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn request_remove_worktree(
        &self,
        path: PathBuf,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cwd) = self.browsing_cwd.clone() else {
            return;
        };
        let sidebar = cx.entity();
        let (title, description) = remove_worktree_dialog_copy(force);
        window.open_dialog(cx, move |dialog, _, _| {
            let sidebar = sidebar.clone();
            let cwd = cwd.clone();
            let path = path.clone();
            dialog
                .title(title)
                .close_button(false)
                .overlay_closable(false)
                .child(description)
                // 普通 Dialog 不会根据 button_props 自动生成 footer；显式使用组件库
                // DialogClose/DialogAction，确保短文案与 dirty 长文案下操作区都参与测量。
                .on_ok(move |_, window, cx| {
                    let started = sidebar.update(cx, |sidebar, cx| {
                        if sidebar.worktree_busy {
                            return false;
                        }
                        sidebar.worktree_busy = true;
                        cx.notify();
                        true
                    });
                    if !started {
                        return false;
                    }
                    let executor = cx.background_executor().clone();
                    let sidebar = sidebar.clone();
                    let cwd = cwd.clone();
                    let path = path.clone();
                    let remove_path = path.clone();
                    let handle = window.window_handle();
                    cx.spawn(async move |cx| {
                        let result = executor
                            .spawn(
                                async move { pi_data::remove_worktree(cwd, &remove_path, force) },
                            )
                            .await;
                        let _ = handle.update(cx, |_, window, cx| {
                            sidebar.update(cx, |sidebar, cx| {
                                sidebar.worktree_busy = false;
                                match result {
                                    Ok(()) => {
                                        let fallback = sidebar
                                            .worktrees
                                            .as_ref()
                                            .map(|snapshot| snapshot.project_root.clone());
                                        if sidebar.browsing_cwd.as_ref().is_some_and(|current| {
                                            pi_data::project_identity_key(current)
                                                == pi_data::project_identity_key(&path)
                                        }) {
                                            if let Some(fallback) = fallback {
                                                sidebar.switch_worktree(fallback, cx);
                                            }
                                        } else {
                                            sidebar.reload_worktrees(cx);
                                        }
                                        window.push_notification(
                                            Notification::success("worktree 已移除"),
                                            cx,
                                        );
                                    }
                                    Err(pi_data::GitError::DirtyWorktree) if !force => {
                                        sidebar.request_remove_worktree(
                                            path.clone(),
                                            true,
                                            window,
                                            cx,
                                        );
                                    }
                                    Err(error) => window.push_notification(
                                        Notification::error(error.to_string()),
                                        cx,
                                    ),
                                }
                                cx.notify();
                            });
                        });
                    })
                    .detach();
                    true
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            div()
                                .debug_selector(|| "cancel-remove-worktree".into())
                                .child(
                                    DialogClose::new().child(
                                        Button::new("cancel-remove-worktree")
                                            .secondary()
                                            .label("取消"),
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "confirm-remove-worktree".into())
                                .child(
                                    DialogAction::new().child(
                                        Button::new("confirm-remove-worktree")
                                            .danger()
                                            .label("移除"),
                                    ),
                                ),
                        ),
                )
        });
    }

    fn start_rename(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(summary) = self.summaries.iter().find(|item| item.id == id) else {
            return;
        };
        if self.running.contains(&summary.id) {
            window.push_notification(Notification::warning("运行中的会话不能重命名"), cx);
            return;
        }
        let value = summary.name.clone().unwrap_or_default();
        self.rename_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        self.rename_target = Some(id);
        cx.notify();
    }

    fn commit_rename(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.rename_target.clone() else {
            return;
        };
        if !self.busy_actions.insert(id.clone()) {
            return;
        }
        let Some(summary) = self.summaries.iter().find(|item| item.id == id).cloned() else {
            self.busy_actions.remove(&id);
            return;
        };
        let value = self.rename_input.read(cx).value().to_string();
        let running = self.running.contains(&id);
        let executor = cx.background_executor().clone();
        cx.notify();
        cx.spawn_in(window, async move |sidebar, cx| {
            let result = executor
                .spawn(async move { rename_session(&summary, &value, running) })
                .await;
            let _ = cx.update(|window, cx| {
                let _ = sidebar.update(cx, |sidebar, cx| {
                    sidebar.busy_actions.remove(&id);
                    match result {
                        Ok(()) => {
                            sidebar.rename_target = None;
                            window.push_notification(Notification::success("会话名称已更新"), cx);
                            sidebar.refresh(window, cx);
                        }
                        Err(error) => window.push_notification(
                            Notification::error(format!("{error}；请刷新会话列表后重试")),
                            cx,
                        ),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn confirm_delete(&self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(summary) = self.summaries.iter().find(|item| item.id == id).cloned() else {
            return;
        };
        let summaries = self.summaries.clone();
        let running = self.running.contains(&id);
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let summary = summary.clone();
            let summaries = summaries.clone();
            let entity = entity.clone();
            dialog
                .title("删除会话？")
                .overlay_closable(false)
                .child("只允许删除没有子会话的历史会话。此操作不可撤销。")
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text("删除")
                        .cancel_text("取消")
                        .show_cancel(true)
                        .ok_variant(ButtonVariant::Danger)
                        .on_ok(move |_, window, cx| {
                            let started = entity.update(cx, |sidebar, cx| {
                                let started = sidebar.busy_actions.insert(summary.id.clone());
                                if started {
                                    cx.notify();
                                }
                                started
                            });
                            if !started {
                                return true;
                            }
                            let summary = summary.clone();
                            let summaries = summaries.clone();
                            let id = summary.id.clone();
                            let entity = entity.clone();
                            let executor = cx.background_executor().clone();
                            let window_handle = window.window_handle();
                            cx.spawn(async move |cx| {
                                let result = executor
                                    .spawn(async move {
                                        delete_leaf_session(&summary, &summaries, running)
                                    })
                                    .await;
                                let _ = window_handle.update(cx, |_, window, cx| {
                                    entity.update(cx, |sidebar, cx| {
                                        sidebar.busy_actions.remove(&id);
                                        match result {
                                            Ok(()) => {
                                                window.push_notification(
                                                    Notification::success("会话已删除"),
                                                    cx,
                                                );
                                                sidebar.refresh(window, cx);
                                            }
                                            Err(error) => window.push_notification(
                                                Notification::error(format!(
                                                    "{error}；请刷新会话列表后重试"
                                                )),
                                                cx,
                                            ),
                                        }
                                        cx.notify();
                                    });
                                });
                            })
                            .detach();
                            true
                        }),
                )
        });
    }

    fn export_session_html(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.busy_actions.insert(id.clone()) {
            return;
        }
        let Some(summary) = self.summaries.iter().find(|item| item.id == id).cloned() else {
            self.busy_actions.remove(&id);
            return;
        };
        let start = if summary.cwd.is_dir() {
            summary.cwd.clone()
        } else {
            PathBuf::from(".")
        };
        let file_name = format!("pi-session-{}.html", summary.id);
        let receiver = cx.prompt_for_new_path(&start, Some(&file_name));
        let executor = cx.background_executor().clone();
        let runtime_manager = self.runtime_manager.clone();
        cx.notify();
        cx.spawn_in(window, async move |sidebar, cx| {
            let destination = receiver.await.ok().into_iter().flatten().flatten().next();
            let result = if let Some(destination) = destination {
                let request = HistoricalHtmlExportRequest {
                    binary: official_binary(),
                    cwd: summary.cwd.clone(),
                    session_path: summary.path.clone(),
                    output_path: destination,
                };
                Some(
                    executor
                        .spawn(async move { runtime_manager.export_historical_html(request) })
                        .await,
                )
            } else {
                None
            };
            let _ = cx.update(|window, cx| {
                let _ = sidebar.update(cx, |sidebar, cx| {
                    sidebar.busy_actions.remove(&id);
                    match result {
                        Some(Ok(export)) => {
                            if let Some(warning) = export.cleanup_warning {
                                window.push_notification(
                                    Notification::warning(format!(
                                        "会话 HTML 已导出到 {}，但导出进程清理失败：{warning}",
                                        export.path.display()
                                    )),
                                    cx,
                                );
                            } else {
                                window.push_notification(
                                    Notification::success("会话 HTML 已导出"),
                                    cx,
                                );
                            }
                        }
                        Some(Err(error)) => window.push_notification(
                            Notification::error(format!("HTML 导出失败：{error}")),
                            cx,
                        ),
                        None => {}
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn export_session(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.busy_actions.insert(id.clone()) {
            return;
        }
        let Some(summary) = self.summaries.iter().find(|item| item.id == id).cloned() else {
            self.busy_actions.remove(&id);
            return;
        };
        let start = if summary.cwd.is_dir() {
            summary.cwd.clone()
        } else {
            PathBuf::from(".")
        };
        let file_name = format!("pi-session-{}.jsonl", summary.id);
        let receiver = cx.prompt_for_new_path(&start, Some(&file_name));
        let executor = cx.background_executor().clone();
        cx.notify();
        cx.spawn_in(window, async move |sidebar, cx| {
            let destination = receiver.await.ok().into_iter().flatten().flatten().next();
            let result = if let Some(destination) = destination {
                Some(
                    executor
                        .spawn(async move { export_session_jsonl(&summary, destination) })
                        .await,
                )
            } else {
                None
            };
            let _ = cx.update(|window, cx| {
                let _ = sidebar.update(cx, |sidebar, cx| {
                    sidebar.busy_actions.remove(&id);
                    match result {
                        Some(Ok(())) => {
                            window.push_notification(Notification::success("原始 JSONL 已导出"), cx)
                        }
                        Some(Err(error)) => window.push_notification(
                            Notification::error(format!("{error}；请刷新会话列表后重试")),
                            cx,
                        ),
                        None => {}
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn render_session_node(
        &self,
        node: &SessionView,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = node.id.clone();
        let hover_id = node.id.clone();
        let rename_id = id.clone();
        let delete_id = id.clone();
        let export_id = id.clone();
        let export_html_id = id.clone();
        let selected = self.selected_id.as_deref() == Some(&node.id);
        let busy = self.busy_actions.contains(&node.id);
        // 规范 S-8：可见元信息保持单行三片段以内，用量、花费与分支收进 tooltip。
        let metric = format_metrics(node);
        let metric_tooltip = session_metric_tooltip(node);
        // 低频操作在悬停行才出现；busy 时保持可见，否则禁用态一闪就消失，用户看不到反馈。
        let actions_visible = self.hovered_id.as_deref() == Some(&node.id) || busy;
        v_flex()
            .w_full()
            .child(
                div()
                    .id(SharedString::from(format!("session-row-{}", node.id)))
                    .debug_selector({
                        let id = node.id.clone();
                        move || format!("session-row-{id}")
                    })
                    .ml(px(depth as f32 * 12.))
                    .p_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(cx.theme().accent.opacity(0.16)))
                    .hover(|row| row.bg(cx.theme().muted))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        let next = hovered.then(|| hover_id.clone());
                        if this.hovered_id != next {
                            this.hovered_id = next;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let SidebarStatus::Ready(projects) = &this.status
                            && let Some(session) = find_session(projects, &id)
                        {
                            let session = session.clone();
                            this.select_session(&session, window, cx);
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(
                                Icon::new(if node.running {
                                    IconName::LoaderCircle
                                } else {
                                    IconName::File
                                })
                                .size_4()
                                .text_color(if node.running {
                                    cx.theme().success
                                } else {
                                    cx.theme().muted_foreground
                                }),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(div().text_sm().truncate().child(node.title.clone()))
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "session-metric-{}",
                                                node.id
                                            )))
                                            .text_xs()
                                            .truncate()
                                            .text_color(cx.theme().muted_foreground)
                                            .tooltip(move |window, cx| {
                                                Tooltip::new(metric_tooltip.clone())
                                                    .build(window, cx)
                                            })
                                            .child(metric),
                                    ),
                            )
                            .when(actions_visible, |row| {
                                row.child(
                                    div()
                                        .flex()
                                        .flex_none()
                                        .gap_1()
                                        .child(
                                            Button::new(format!("rename-{}", node.id))
                                                .debug_selector({
                                                    let id = node.id.clone();
                                                    move || format!("rename-{id}")
                                                })
                                                .ghost()
                                                .xsmall()
                                                .icon(IconName::Settings)
                                                .tooltip("重命名")
                                                .disabled(busy)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.start_rename(
                                                            rename_id.clone(),
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                )),
                                        )
                                        .child(
                                            Button::new(format!("export-html-{}", node.id))
                                                .ghost()
                                                .xsmall()
                                                .icon(IconName::File)
                                                .tooltip("导出 HTML")
                                                .disabled(busy)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.export_session_html(
                                                            export_html_id.clone(),
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                )),
                                        )
                                        .child(
                                            Button::new(format!("export-{}", node.id))
                                                .debug_selector({
                                                    let id = node.id.clone();
                                                    move || format!("export-{id}")
                                                })
                                                .ghost()
                                                .xsmall()
                                                .icon(IconName::Copy)
                                                .tooltip("导出原始 JSONL")
                                                .disabled(busy)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.export_session(
                                                            export_id.clone(),
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                )),
                                        )
                                        .child(
                                            Button::new(format!("delete-{}", node.id))
                                                .debug_selector({
                                                    let id = node.id.clone();
                                                    move || format!("delete-{id}")
                                                })
                                                .ghost()
                                                .xsmall()
                                                .icon(IconName::Delete)
                                                .tooltip("删除")
                                                .disabled(busy)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.confirm_delete(
                                                            delete_id.clone(),
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                )),
                                        ),
                                )
                            }),
                    ),
            )
            .children(
                node.children
                    .iter()
                    .map(|child| self.render_session_node(child, depth + 1, cx)),
            )
            .into_any_element()
    }
}

impl EventEmitter<PanelEvent> for SessionSidebar {}
impl EventEmitter<SessionSelected> for SessionSidebar {}
impl EventEmitter<WorktreeSelected> for SessionSidebar {}
impl EventEmitter<NewSessionRequested> for SessionSidebar {}

impl Focusable for SessionSidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SessionSidebar {
    fn panel_name(&self) -> &'static str {
        "gpui-pi-sidebar"
    }
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some("会话".into())
    }
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "会话"
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

impl Render for SessionSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        let probe = self.probe.clone();
        #[cfg(not(test))]
        let probe = self.probe;
        let worktree_controls = self
            .worktrees
            .as_ref()
            .filter(|snapshot| snapshot.is_top_level)
            .map(|snapshot| {
                let current = self.browsing_cwd.clone();
                let rows = snapshot
                    .worktrees
                    .iter()
                    .enumerate()
                    .map(|(index, worktree)| {
                        let path = worktree.path.clone();
                        let remove_path = path.clone();
                        let branch = worktree
                            .branch
                            .clone()
                            .unwrap_or_else(|| "detached HEAD".to_owned());
                        let selected = current.as_ref().is_some_and(|cwd| {
                            pi_data::project_identity_key(cwd)
                                == pi_data::project_identity_key(&path)
                        });
                        let can_remove = !worktree.is_main;
                        let remove_visible = self.hovered_worktree_index == Some(index);
                        h_flex()
                            .id(("worktree-row", index))
                            .debug_selector(move || format!("worktree-row-{index}"))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(selected, |row| row.bg(cx.theme().accent.opacity(0.16)))
                            .hover(|row| row.bg(cx.theme().muted))
                            .on_hover(cx.listener(move |sidebar, hovered: &bool, _, cx| {
                                if *hovered {
                                    if sidebar.hovered_worktree_index != Some(index) {
                                        sidebar.hovered_worktree_index = Some(index);
                                        cx.notify();
                                    }
                                } else if sidebar.hovered_worktree_index == Some(index) {
                                    sidebar.hovered_worktree_index = None;
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |sidebar, _, _, cx| {
                                sidebar.switch_worktree(path.clone(), cx)
                            }))
                            .child(
                                Icon::new(if selected {
                                    IconName::Check
                                } else {
                                    IconName::Folder
                                })
                                .size_4(),
                            )
                            .child(div().flex_1().min_w_0().truncate().text_xs().child(branch))
                            .when(can_remove && remove_visible, |row| {
                                row.child(
                                    Button::new(("remove-worktree", index))
                                        .debug_selector(move || format!("remove-worktree-{index}"))
                                        .ghost()
                                        .small()
                                        .icon(IconName::Delete)
                                        .tooltip("移除 worktree checkout")
                                        .disabled(self.worktree_busy)
                                        .on_click(cx.listener(move |sidebar, _, window, cx| {
                                            cx.stop_propagation();
                                            sidebar.request_remove_worktree(
                                                remove_path.clone(),
                                                false,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                            })
                    });
                v_flex()
                    .debug_selector(|| "worktree-switcher".into())
                    .gap_1()
                    .child(
                        Button::new("toggle-worktree-manager")
                            .small()
                            .ghost()
                            .icon(worktree_disclosure_icon(self.worktree_expanded))
                            .label("Worktree")
                            .tooltip("展开或收起 worktree 管理")
                            .on_click(cx.listener(|sidebar, _, _, cx| {
                                sidebar.worktree_expanded = !sidebar.worktree_expanded;
                                cx.notify();
                            })),
                    )
                    .when(self.worktree_expanded, |manager| {
                        manager
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(Input::new(&self.worktree_input)),
                                    )
                                    .child(
                                        Button::new("create-worktree")
                                            .small()
                                            .primary()
                                            .label(if self.worktree_busy {
                                                "处理中…"
                                            } else {
                                                "新建"
                                            })
                                            .tooltip("按分支创建 worktree")
                                            .disabled(self.worktree_busy)
                                            .on_click(cx.listener(Self::create_worktree)),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .debug_selector(|| "worktree-rows-scroll".into())
                                    .max_h(px(220.))
                                    .overflow_y_scrollbar()
                                    .children(rows),
                            )
                    })
            });
        let pending_reveal_project = self.pending_reveal_project.take();
        let content = match &self.status {
            SidebarStatus::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child("正在后台加载会话…")
                .into_any_element(),
            SidebarStatus::Error(error) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(cx.theme().danger)
                .child("会话加载失败")
                .child(error.clone())
                .into_any_element(),
            SidebarStatus::Ready(projects) if projects.is_empty() => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child("没有历史会话")
                .into_any_element(),
            SidebarStatus::Ready(projects) => {
                let groups = projects
                    .iter()
                    .map(|project| {
                        let key = project.key.clone();
                        let toggle_key = key.clone();
                        let create_cwd = project.root.clone();
                        let collapsed = self.collapsed_projects.contains(&key);
                        let scroll = self.project_scrolls.get(&key).cloned().unwrap_or_default();
                        let session_count = session_count(&project.sessions);
                        if pending_reveal_project.as_deref() == Some(key.as_str())
                            && let Some(selected) = self.selected_id.as_deref()
                            && let Some(index) =
                                selected_top_level_index(&project.sessions, selected)
                        {
                            scroll.scroll_to_item(index);
                        }
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .id(SharedString::from(format!("project-header-{key}")))
                                    .debug_selector(|| "project-session-header".into())
                                    .gap_1p5()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|row| row.bg(cx.theme().muted))
                                    .on_click(cx.listener(move |sidebar, _, _, cx| {
                                        if !sidebar.collapsed_projects.insert(toggle_key.clone()) {
                                            sidebar.collapsed_projects.remove(&toggle_key);
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        Icon::new(if collapsed {
                                            IconName::ChevronRight
                                        } else {
                                            IconName::ChevronDown
                                        })
                                        .size_4(),
                                    )
                                    .child(
                                        div()
                                            .debug_selector(|| "project-session-title".into())
                                            .flex_1()
                                            .min_w_0()
                                            .text_sm()
                                            .font_semibold()
                                            .text_color(cx.theme().foreground)
                                            .truncate()
                                            .child(project_label(&project.root)),
                                    )
                                    .child(
                                        Button::new(format!("new-session-{key}"))
                                            .debug_selector(|| "new-project-session".into())
                                            .ghost()
                                            .small()
                                            .icon(IconName::Plus)
                                            .label("新建会话")
                                            .tooltip("在此项目启动全新会话")
                                            .on_click(cx.listener(
                                                move |sidebar, _, window, cx| {
                                                    cx.stop_propagation();
                                                    sidebar.selected_id = None;
                                                    sidebar.set_browsing_cwd(
                                                        Some(create_cwd.clone()),
                                                        cx,
                                                    );
                                                    prompt_project_trust(
                                                        sidebar.agent_dir.clone(),
                                                        &create_cwd,
                                                        window,
                                                        cx,
                                                    );
                                                    cx.emit(NewSessionRequested {
                                                        cwd: create_cwd.clone(),
                                                    });
                                                    cx.notify();
                                                },
                                            )),
                                    ),
                            )
                            .when(!collapsed, |group| {
                                let session_rows = project
                                    .sessions
                                    .iter()
                                    .map(|session| self.render_session_node(session, 0, cx))
                                    .collect::<Vec<_>>();
                                group.child(
                                    // 两层分责：外层只做严格 paint/hitbox clip，内层只做滚动。
                                    // pb_1 使用既有 spacing token，把 512px 外框的有效绘制区收至
                                    // 8×60 + 7×4 = 508px，完整保留第 8 行并截断第 9 行。
                                    div()
                                        .debug_selector(|| "project-session-viewport".into())
                                        .when(
                                            session_count > PROJECT_SESSION_VISIBLE_ROWS,
                                            |viewport| viewport.max_h_128(),
                                        )
                                        .pb_1()
                                        .overflow_hidden()
                                        .child(
                                            v_flex()
                                                .id(SharedString::from(format!(
                                                    "project-scroll-{key}"
                                                )))
                                                .debug_selector(|| "project-session-scroll".into())
                                                .size_full()
                                                .min_h_0()
                                                .gap_1()
                                                .track_scroll(&scroll)
                                                .overflow_y_scroll()
                                                .lock_scroll_axis()
                                                .vertical_scrollbar(&scroll)
                                                .children(session_rows),
                                        ),
                                )
                            })
                    })
                    .collect::<Vec<_>>();
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .gap_3()
                    .children(groups)
                    .into_any_element()
            }
        };
        div()
            .id("session-sidebar")
            .debug_selector(|| "session-sidebar".into())
            .when_some(probe, |this, probe| {
                this.on_prepaint(move |bounds, _, _| probe.record_sidebar(bounds))
            })
            .track_focus(&self.focus_handle)
            .size_full()
            .min_w_0()
            .p_2()
            .bg(cx.theme().sidebar)
            .child(
                v_flex()
                    .size_full()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().font_semibold().child("项目与会话"))
                            .child(
                                Button::new("refresh-sessions")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Redo)
                                    .tooltip("刷新")
                                    .on_click(cx.listener(Self::handle_refresh)),
                            ),
                    )
                    .when_some(worktree_controls, |view, controls| view.child(controls))
                    .when_some(
                        self.worktree_error
                            .as_deref()
                            .map(worktree_error_presentation),
                        |view, error| {
                            view.child(
                                div()
                                    .text_xs()
                                    .text_color(if error.warning {
                                        cx.theme().warning
                                    } else {
                                        cx.theme().muted_foreground
                                    })
                                    .child(error.text),
                            )
                        },
                    )
                    .when(self.rename_target.is_some(), |view| {
                        view.child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(Input::new(&self.rename_input).w_full()),
                                )
                                .child(
                                    Button::new("commit-rename")
                                        .small()
                                        .primary()
                                        .label("保存")
                                        .disabled(
                                            self.rename_target
                                                .as_ref()
                                                .is_some_and(|id| self.busy_actions.contains(id)),
                                        )
                                        .on_click(cx.listener(Self::commit_rename)),
                                ),
                        )
                    })
                    .when(self.diagnostics_count > 0, |view| {
                        view.child(
                            div()
                                .id("session-diagnostics")
                                .debug_selector(|| "session-diagnostics".into())
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child(format!(
                                    "扫描跳过 {} 个异常文件或目录",
                                    self.diagnostics_count
                                )),
                        )
                    })
                    .child(content),
            )
    }
}

/// 会话行只保留运行/时间/消息数；用量、花费与分支由同一行的 tooltip 承载。
fn format_metrics(session: &SessionView) -> String {
    let running = if session.running { "运行中 · " } else { "" };
    format!(
        "{running}{} · {} 条消息",
        format_relative_time(session.modified),
        session.message_count,
    )
}

fn session_metric_tooltip(session: &SessionView) -> String {
    let context = session.metrics.recent_context_tokens.map_or_else(
        || "最近上下文：未知".to_owned(),
        |tokens| format!("最近上下文：{tokens} tokens"),
    );
    let branch = session.branch.as_deref().map_or_else(
        || "分支：未记录".to_owned(),
        |branch| format!("分支：{branch}"),
    );
    format!(
        "{branch}\n累计用量：{} tokens\n累计花费：${:.4}\n{context}",
        session.metrics.cumulative_tokens, session.metrics.cumulative_cost,
    )
}

fn format_relative_time(modified: std::time::SystemTime) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(modified)
        .map_or(0, |duration| duration.as_secs());
    match seconds {
        0..=59 => "刚刚".to_owned(),
        60..=3_599 => format!("{} 分钟前", seconds / 60),
        3_600..=86_399 => format!("{} 小时前", seconds / 3_600),
        86_400..=2_591_999 => format!("{} 天前", seconds / 86_400),
        _ => format!("{} 个月前", seconds / 2_592_000),
    }
}

const fn remove_worktree_dialog_copy(force: bool) -> (&'static str, &'static str) {
    if force {
        (
            "强制移除 dirty worktree？",
            "将丢弃此 checkout 的未提交改动；目录链接仍会被安全门禁拒绝。",
        )
    } else {
        ("移除 worktree？", "只移除 checkout 目录，保留分支。")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeErrorPresentation {
    text: String,
    warning: bool,
}

/// “非 Git 仓库”是当前目录能力提示，不应以告警状态呈现；其余执行失败保留诊断原文。
fn worktree_error_presentation(error: &str) -> WorktreeErrorPresentation {
    let normalized = error.to_ascii_lowercase();
    let is_non_repository = normalized.contains("not a git repository")
        || error.contains("不是 Git 仓库")
        || error.contains("不是 git 仓库");
    if is_non_repository {
        WorktreeErrorPresentation {
            text: "当前目录不是 Git 仓库".to_owned(),
            warning: false,
        }
    } else {
        WorktreeErrorPresentation {
            text: format!("Worktree：{error}"),
            warning: true,
        }
    }
}

fn worktree_disclosure_icon(expanded: bool) -> IconName {
    if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}

fn project_label(root: &Path) -> String {
    root.file_name()
        .filter(|name| !name.is_empty())
        .map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
}

fn session_count(sessions: &[SessionView]) -> usize {
    sessions
        .iter()
        .map(|session| 1 + session_count(&session.children))
        .sum()
}

fn selected_top_level_index(sessions: &[SessionView], id: &str) -> Option<usize> {
    sessions
        .iter()
        .position(|session| session_contains(session, id))
}

fn session_contains(session: &SessionView, id: &str) -> bool {
    session.id == id
        || session
            .children
            .iter()
            .any(|child| session_contains(child, id))
}

fn project_key_for_session(status: &SidebarStatus, id: &str) -> Option<String> {
    let SidebarStatus::Ready(projects) = status else {
        return None;
    };
    projects
        .iter()
        .find(|project| {
            project
                .sessions
                .iter()
                .any(|session| session_contains(session, id))
        })
        .map(|project| project.key.clone())
}

fn find_session<'a>(projects: &'a [ProjectSessionView], id: &str) -> Option<&'a SessionView> {
    fn find<'a>(sessions: &'a [SessionView], id: &str) -> Option<&'a SessionView> {
        sessions.iter().find_map(|session| {
            if session.id == id {
                Some(session)
            } else {
                find(&session.children, id)
            }
        })
    }
    projects
        .iter()
        .find_map(|project| find(&project.sessions, id))
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use gpui::{Modifiers, TestAppContext, VisualTestContext, point, size};
    use gpui_component::Root;
    use pi_data::SessionMetrics;

    use super::*;

    fn fixture_session(id: impl Into<String>) -> SessionView {
        let id = id.into();
        SessionView {
            id: id.clone(),
            path: PathBuf::from(format!("{id}.jsonl")),
            cwd: PathBuf::from("C:/fixture/project"),
            title: format!("Fixture session {id}"),
            modified: SystemTime::UNIX_EPOCH,
            message_count: 3,
            metrics: SessionMetrics {
                cumulative_tokens: 100,
                cumulative_cost: 0.125,
                recent_context_tokens: Some(42_000),
            },
            branch: Some("feature/fixture".to_owned()),
            running: id == "fixture-session",
            children: Vec::new(),
        }
    }

    fn ready_sidebar(window: &mut Window, cx: &mut Context<SessionSidebar>) -> SessionSidebar {
        let session = SessionView {
            title: "Fixture session".to_owned(),
            ..fixture_session("fixture-session")
        };
        SessionSidebar {
            focus_handle: cx.focus_handle(),
            status: SidebarStatus::Ready(vec![ProjectSessionView {
                key: "fixture".to_owned(),
                root: PathBuf::from("C:/fixture/project"),
                modified: SystemTime::UNIX_EPOCH,
                sessions: vec![session],
            }]),
            selected_id: None,
            sessions_root: None,
            agent_dir: None,
            running: RunningSessionOverlay::new(["fixture-session".to_owned()]),
            summaries: Vec::new(),
            diagnostics_count: 2,
            busy_actions: HashSet::new(),
            rename_input: cx.new(|cx| InputState::new(window, cx)),
            rename_target: None,
            load_generation: 0,
            hovered_id: None,
            browsing_cwd: Some(PathBuf::from("C:/fixture/project")),
            worktrees: None,
            worktree_error: None,
            worktree_generation: 0,
            worktree_busy: false,
            worktree_expanded: false,
            hovered_worktree_index: None,
            worktree_input: cx.new(|cx| InputState::new(window, cx)),
            collapsed_projects: HashSet::new(),
            project_scrolls: HashMap::new(),
            pending_reveal_project: None,
            probe: None,
            runtime_manager: RuntimeManager::new(Default::default()),
        }
    }

    #[test]
    fn worktree_error_presentation_distinguishes_non_repository_from_failures() {
        let non_repository = worktree_error_presentation(
            "fatal: not a git repository (or any of the parent directories): .git",
        );
        assert_eq!(non_repository.text, "当前目录不是 Git 仓库");
        assert!(!non_repository.warning);

        for error in ["git command timed out", "failed to spawn git.exe"] {
            let failure = worktree_error_presentation(error);
            assert!(failure.warning);
            assert!(failure.text.contains(error));
        }
    }

    #[gpui::test]
    fn stale_worktree_generation_and_cwd_do_not_replace_newer_state(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        cx.open_window(size(px(320.), px(560.)), move |window, cx| {
            let entity = cx.new(|cx| ready_sidebar(window, cx));
            *output.borrow_mut() = Some(entity.clone());
            Root::new(entity, window, cx)
        });
        let sidebar = captured.borrow().clone().unwrap();
        sidebar.update(cx, |sidebar, _| {
            sidebar.worktree_generation = 2;
            sidebar.browsing_cwd = Some(PathBuf::from("C:/fixture/b"));
            let stale = pi_data::WorktreeSnapshot {
                project_root: PathBuf::from("C:/fixture/a"),
                is_top_level: true,
                worktrees: Vec::new(),
            };
            assert!(!sidebar.finish_worktree_refresh(
                1,
                &pi_data::project_identity_key(Path::new("C:/fixture/a")),
                Ok(stale.clone()),
            ));
            assert!(!sidebar.finish_worktree_refresh(
                2,
                &pi_data::project_identity_key(Path::new("C:/fixture/a")),
                Ok(stale),
            ));
            assert!(sidebar.worktrees.is_none());
        });
    }

    fn worktree_sidebar(window: &mut Window, cx: &mut Context<SessionSidebar>) -> SessionSidebar {
        let mut sidebar = ready_sidebar(window, cx);
        sidebar.worktree_expanded = true;
        sidebar.worktrees = Some(pi_data::WorktreeSnapshot {
            project_root: PathBuf::from("C:/fixture/project"),
            is_top_level: true,
            worktrees: vec![
                pi_data::WorktreeInfo {
                    path: PathBuf::from("C:/fixture/project"),
                    branch: Some("main".to_owned()),
                    is_main: true,
                    is_current: false,
                },
                pi_data::WorktreeInfo {
                    path: PathBuf::from("C:/fixture/project-worktrees/feature"),
                    branch: Some("feature/visual".to_owned()),
                    is_main: false,
                    is_current: true,
                },
            ],
        });
        sidebar
    }

    struct WorktreeDialogHarness {
        sidebar: gpui::Entity<SessionSidebar>,
    }

    impl Render for WorktreeDialogHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.sidebar.clone())
                .children(Root::render_dialog_layer(window, cx))
        }
    }

    #[gpui::test]
    fn worktree_remove_action_is_hover_only(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let handle = cx.open_window(size(px(320.), px(560.)), |window, cx| {
            let sidebar = cx.new(|cx| worktree_sidebar(window, cx));
            Root::new(sidebar, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        draw(&mut visual, 3);
        assert!(
            visual.debug_bounds("remove-worktree-0").is_none(),
            "main worktree 永远不能渲染删除按钮"
        );
        assert!(
            visual.debug_bounds("remove-worktree-1").is_none(),
            "linked worktree 删除按钮默认必须隐藏"
        );
        let main = visual
            .debug_bounds("worktree-row-0")
            .expect("main worktree row missing");
        let linked = visual
            .debug_bounds("worktree-row-1")
            .expect("linked worktree row missing");
        visual.simulate_mouse_move(main.center(), None, Modifiers::default());
        draw(&mut visual, 2);
        assert!(
            visual.debug_bounds("remove-worktree-0").is_none(),
            "main worktree hover 时仍不能渲染删除按钮"
        );

        // 先进入 row 0 再向下移动到 row 1，覆盖 row 1 enter 先于 row 0 leave 的事件顺序。
        visual.simulate_mouse_move(linked.center(), None, Modifiers::default());
        draw(&mut visual, 2);
        let remove = visual
            .debug_bounds("remove-worktree-1")
            .expect("linked worktree 删除按钮必须在跨行 hover 后出现");
        assert!(remove.size.width > px(0.) && remove.size.height > px(0.));
        assert!(visual.debug_bounds("remove-worktree-0").is_none());

        visual.simulate_mouse_move(point(px(1.), px(1.)), None, Modifiers::default());
        draw(&mut visual, 2);
        assert!(
            visual.debug_bounds("remove-worktree-1").is_none(),
            "指针移出 linked 行后删除按钮必须重新隐藏"
        );
    }

    #[gpui::test]
    fn worktree_remove_dialog_renders_footer_for_normal_and_force_copy(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let handle = cx.open_window(size(px(800.), px(560.)), move |window, cx| {
            let sidebar = cx.new(|cx| worktree_sidebar(window, cx));
            *output.borrow_mut() = Some(sidebar.clone());
            let harness = cx.new(|_| WorktreeDialogHarness { sidebar });
            Root::new(harness, window, cx)
        });
        let sidebar = captured.borrow().clone().unwrap();
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        visual.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.request_remove_worktree(
                    PathBuf::from("C:/fixture/project-worktrees/feature"),
                    false,
                    window,
                    cx,
                );
            });
        });
        visual.update(|window, cx| assert!(window.has_active_dialog(cx)));
        draw(&mut visual, 3);
        assert_non_zero_debug_bounds(&mut visual, "cancel-remove-worktree");
        assert_non_zero_debug_bounds(&mut visual, "confirm-remove-worktree");

        visual.update(|window, cx| window.close_dialog(cx));
        visual.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.request_remove_worktree(
                    PathBuf::from("C:/fixture/project-worktrees/feature"),
                    true,
                    window,
                    cx,
                );
            });
        });
        visual.update(|window, cx| assert!(window.has_active_dialog(cx)));
        draw(&mut visual, 3);
        assert_non_zero_debug_bounds(&mut visual, "cancel-remove-worktree");
        assert_non_zero_debug_bounds(&mut visual, "confirm-remove-worktree");
        assert_eq!(
            remove_worktree_dialog_copy(false),
            ("移除 worktree？", "只移除 checkout 目录，保留分支。")
        );
        assert_eq!(
            remove_worktree_dialog_copy(true),
            (
                "强制移除 dirty worktree？",
                "将丢弃此 checkout 的未提交改动；目录链接仍会被安全门禁拒绝。",
            )
        );
    }

    #[gpui::test]
    fn stale_refresh_generation_does_not_replace_newer_state(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let sidebar = std::rc::Rc::new(std::cell::RefCell::new(None));
        let captured = sidebar.clone();
        cx.open_window(size(px(320.), px(560.)), move |window, cx| {
            let entity = cx.new(|cx| ready_sidebar(window, cx));
            *captured.borrow_mut() = Some(entity.clone());
            Root::new(entity, window, cx)
        });
        let sidebar = sidebar.borrow().clone().unwrap();
        sidebar.update(cx, |sidebar, _| {
            sidebar.load_generation = 2;
            assert!(!sidebar.finish_refresh(1, Vec::new(), 0, Vec::new()));
            assert!(matches!(
                sidebar.status,
                SidebarStatus::Ready(ref projects) if !projects.is_empty()
            ));
        });
    }

    const ROW_ACTIONS: [&str; 3] = [
        "rename-fixture-session",
        "export-fixture-session",
        "delete-fixture-session",
    ];

    fn draw(visual: &mut VisualTestContext, frames: usize) {
        for _ in 0..frames {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
    }

    fn assert_non_zero_debug_bounds(visual: &mut VisualTestContext, selector: &'static str) {
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing debug selector {selector}"));
        assert!(
            bounds.size.width > px(0.) && bounds.size.height > px(0.),
            "{selector} 必须实际参与布局并具有非零尺寸"
        );
    }

    fn ready_sidebar_window(cx: &mut TestAppContext) -> VisualTestContext {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let handle = cx.open_window(size(px(320.), px(560.)), |window, cx| {
            let sidebar = cx.new(|cx| ready_sidebar(window, cx));
            Root::new(sidebar, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        draw(&mut visual, 3);
        visual
    }

    fn many_sessions_sidebar(
        window: &mut Window,
        cx: &mut Context<SessionSidebar>,
    ) -> SessionSidebar {
        let mut sidebar = ready_sidebar(window, cx);
        let SidebarStatus::Ready(projects) = &mut sidebar.status else {
            unreachable!()
        };
        projects[0].sessions = (0..10)
            .map(|index| SessionView {
                title: format!("提交清单并修复首批问题 · 子代理会话 {index} 的长标题"),
                branch: Some(format!("feature/视觉复验-{index}")),
                ..fixture_session(format!("session-{index}"))
            })
            .collect();
        sidebar
            .project_scrolls
            .insert(projects[0].key.clone(), ScrollHandle::default());
        sidebar
    }

    #[gpui::test]
    fn project_session_scroll_shows_eight_rows_and_does_not_snap_back(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let handle = cx.open_window(size(px(320.), px(700.)), move |window, cx| {
            let sidebar = cx.new(|cx| many_sessions_sidebar(window, cx));
            *output.borrow_mut() = Some(sidebar.clone());
            Root::new(sidebar, window, cx)
        });
        let sidebar = captured.borrow().clone().unwrap();
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        draw(&mut visual, 4);
        let viewport = visual
            .debug_bounds("project-session-viewport")
            .expect("项目会话严格裁切 viewport 必须存在");
        let scroll_bounds = visual
            .debug_bounds("project-session-scroll")
            .expect("项目会话内层滚动区必须存在");
        let first = visual
            .debug_bounds("session-row-session-0")
            .expect("第一行必须存在");
        let eighth = visual
            .debug_bounds("session-row-session-7")
            .expect("第八行必须存在");
        let ninth = visual
            .debug_bounds("session-row-session-8")
            .expect("第九行必须存在");
        assert_eq!(viewport.size.height, px(512.), "外层必须保持 max_h_128");
        assert_eq!(
            scroll_bounds.size.height,
            px(508.),
            "pb_1 后有效 clip 高度必须为 508px"
        );
        assert_eq!(scroll_bounds.origin.x, viewport.origin.x);
        assert_eq!(scroll_bounds.origin.y, viewport.origin.y);
        assert_eq!(first.size.height, px(60.), "规范 p_2 的双行会话行高度");
        assert!(
            first.top() >= viewport.top() && eighth.bottom() <= viewport.bottom(),
            "默认 viewport 必须完整显示前 8 条会话"
        );
        let effective_clip_bottom = scroll_bounds.bottom();
        assert_eq!(
            eighth.bottom(),
            effective_clip_bottom,
            "有效 clip 必须紧接第 8 行"
        );
        assert!(
            ninth.top() >= effective_clip_bottom,
            "第九行默认必须严格位于有效 clip 外"
        );
        sidebar.update(cx, |sidebar, _| assert!(sidebar.selected_id.is_none()));
        // 点击截图中最可能露出的第 9 行标题上沿候选点，而非只点 center；
        // 正确 clip 后该 hitbox 不得穿透外层 viewport。
        let clipped_title_candidate = point(ninth.left() + px(32.), ninth.top() + px(8.));
        visual.simulate_click(clipped_title_candidate, Modifiers::default());
        sidebar.update(cx, |sidebar, _| {
            assert!(sidebar.selected_id.is_none(), "被裁切的第 9 行不得命中")
        });

        let scroll = sidebar.read_with(cx, |sidebar, _| {
            sidebar.project_scrolls.get("fixture").unwrap().clone()
        });
        scroll.set_offset(gpui::point(px(0.), -first.size.height * 2.));
        sidebar.update(cx, |_, cx| cx.notify());
        draw(&mut visual, 3);
        assert!(scroll.offset().y < px(0.), "普通重绘不得把用户滚动弹回顶部");
        let scrolled_ninth = visual
            .debug_bounds("session-row-session-8")
            .expect("滚动后第九行必须仍存在");
        assert!(
            scrolled_ninth.top() >= scroll_bounds.top()
                && scrolled_ninth.bottom() <= scroll_bounds.bottom(),
            "滚动后第九行必须完整进入 viewport"
        );
        visual.simulate_click(scrolled_ninth.center(), Modifiers::default());
        sidebar.update(cx, |sidebar, _| {
            assert_eq!(sidebar.selected_id.as_deref(), Some("session-8"));
        });
    }

    #[test]
    fn selected_child_reveals_its_top_level_session() {
        let child = fixture_session("child");
        let mut parent = fixture_session("parent");
        parent.children.push(child);
        let sibling = fixture_session("sibling");
        assert_eq!(
            selected_top_level_index(&[parent, sibling], "child"),
            Some(0)
        );
    }

    #[gpui::test]
    fn ready_sidebar_renders_project_row_and_diagnostics(cx: &mut TestAppContext) {
        let mut visual = ready_sidebar_window(cx);
        assert!(visual.debug_bounds("session-sidebar").is_some());
        assert!(visual.debug_bounds("session-diagnostics").is_some());
        let row = visual
            .debug_bounds("session-row-fixture-session")
            .expect("session row missing");
        assert!(row.size.width > px(0.) && row.size.height > px(0.));
    }

    #[gpui::test]
    fn project_header_collapses_sessions_and_keeps_new_session_entry_visible(
        cx: &mut TestAppContext,
    ) {
        let mut visual = ready_sidebar_window(cx);
        assert_non_zero_debug_bounds(&mut visual, "project-session-header");
        let title = visual
            .debug_bounds("project-session-title")
            .expect("项目标题必须存在");
        let action = visual
            .debug_bounds("new-project-session")
            .expect("新建会话入口必须存在");
        assert!(title.size.height >= action.size.height * 0.5);
        assert_non_zero_debug_bounds(&mut visual, "new-project-session");
        let header = visual.debug_bounds("project-session-header").unwrap();
        visual.simulate_click(header.center(), Modifiers::default());
        draw(&mut visual, 2);
        assert!(visual.debug_bounds("session-row-fixture-session").is_none());
        assert_non_zero_debug_bounds(&mut visual, "new-project-session");
    }

    /// T2 ③：低频行操作按规范 S-9 收进 hover 态，hover 前不占位、hover 后可点。
    #[gpui::test]
    fn session_row_actions_are_hover_only(cx: &mut TestAppContext) {
        let mut visual = ready_sidebar_window(cx);
        for action in ROW_ACTIONS {
            assert!(
                visual.debug_bounds(action).is_none(),
                "hover 前不应渲染 {action}"
            );
        }

        let row = visual
            .debug_bounds("session-row-fixture-session")
            .expect("session row missing");
        visual.simulate_mouse_move(row.center(), None, Modifiers::default());
        draw(&mut visual, 2);
        for action in ROW_ACTIONS {
            assert!(
                visual.debug_bounds(action).is_some(),
                "hover 后必须渲染 {action}"
            );
        }

        // 指针移出该行后操作重新收起，避免一屏里每行都挂三个图标。
        visual.simulate_mouse_move(
            point(row.center().x, row.origin.y - px(4.)),
            None,
            Modifiers::default(),
        );
        draw(&mut visual, 2);
        for action in ROW_ACTIONS {
            assert!(
                visual.debug_bounds(action).is_none(),
                "移出行后不应继续渲染 {action}"
            );
        }
    }

    #[test]
    fn worktree_disclosure_icon_tracks_expansion() {
        assert!(matches!(
            worktree_disclosure_icon(false),
            IconName::ChevronRight
        ));
        assert!(matches!(
            worktree_disclosure_icon(true),
            IconName::ChevronDown
        ));
    }

    #[test]
    fn metrics_split_into_two_short_lines() {
        let session = SessionView {
            id: "s".to_owned(),
            path: PathBuf::from("s.jsonl"),
            cwd: PathBuf::from("C:/fixture"),
            title: "t".to_owned(),
            modified: SystemTime::now(),
            message_count: 3,
            metrics: SessionMetrics {
                cumulative_tokens: 100,
                cumulative_cost: 0.125,
                recent_context_tokens: Some(42_000),
            },
            branch: Some("feature/fixture".to_owned()),
            running: true,
            children: Vec::new(),
        };
        let metric = format_metrics(&session);
        // 规范 S-8：紧凑指标行最多 3 个片段，分支与 token/cost 收进 tooltip/详情。
        assert_eq!(metric, "运行中 · 刚刚 · 3 条消息");
        assert_eq!(metric.matches(" · ").count(), 2);

        let plain = SessionView {
            branch: None,
            running: false,
            ..session
        };
        assert_eq!(format_metrics(&plain), "刚刚 · 3 条消息");
    }
}
