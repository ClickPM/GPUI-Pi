use std::{path::PathBuf, sync::Arc};

use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, Image, ImageFormat,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    dock::{Panel, PanelControl, PanelEvent},
    h_flex,
    input::{Editor, EditorState},
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_pi_ui::{WorkspaceContentTab, WorkspaceContentTabs};

use crate::panels::ChatPanel;

const CHAT_TAB_ID: &str = "chat";
const MAX_FILE_TABS: usize = 16;

#[derive(Debug, Clone)]
pub struct OpenFileRequest {
    pub source_root: PathBuf,
    pub relative_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct OpenDiffRequest {
    pub source_root: PathBuf,
    pub relative_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileTabKind {
    Source,
    Diff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileTab {
    id: String,
    source_root: PathBuf,
    relative_path: PathBuf,
    label: String,
    kind: FileTabKind,
    generation: u64,
    status: FileTabStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileTabStatus {
    Loading,
    Text(Arc<pi_data::TextFileContent>),
    Diff {
        kind: pi_data::GitFileStatusKind,
        diff: Arc<pi_render::DiffBlock>,
    },
    Image {
        kind: pi_data::ImageKind,
        size: u64,
        image: Arc<Image>,
    },
    Unsupported {
        size: Option<u64>,
        reason: String,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveContent {
    Chat,
    File(String),
}

pub struct MainPanel {
    focus_handle: FocusHandle,
    chat: gpui::Entity<ChatPanel>,
    root: Option<PathBuf>,
    root_generation: u64,
    next_tab_generation: u64,
    tabs: Vec<FileTab>,
    active: ActiveContent,
    editor: gpui::Entity<EditorState>,
    image: Option<Arc<Image>>,
    _open_subscription: Subscription,
    _chat_open_subscription: Subscription,
    _diff_subscription: Subscription,
}

impl MainPanel {
    pub fn new(
        chat: gpui::Entity<ChatPanel>,
        explorer: &gpui::Entity<crate::file_explorer::FileExplorerPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("text")
                .folding(true)
                .default_value("")
        });
        let open_subscription = cx.subscribe_in(
            explorer,
            window,
            |panel, _, event: &OpenFileRequest, window, cx| {
                panel.open_file(
                    event.source_root.clone(),
                    event.relative_path.clone(),
                    window,
                    cx,
                )
            },
        );
        let chat_open_subscription = cx.subscribe_in(
            &chat,
            window,
            |panel, _, event: &OpenFileRequest, window, cx| {
                panel.open_file(
                    event.source_root.clone(),
                    event.relative_path.clone(),
                    window,
                    cx,
                )
            },
        );
        let diff_subscription = cx.subscribe_in(
            explorer,
            window,
            |panel, _, event: &OpenDiffRequest, window, cx| {
                panel.open_diff(
                    event.source_root.clone(),
                    event.relative_path.clone(),
                    window,
                    cx,
                )
            },
        );
        Self {
            focus_handle: cx.focus_handle(),
            chat,
            root: None,
            root_generation: 0,
            next_tab_generation: 0,
            tabs: Vec::new(),
            active: ActiveContent::Chat,
            editor,
            image: None,
            _open_subscription: open_subscription,
            _chat_open_subscription: chat_open_subscription,
            _diff_subscription: diff_subscription,
        }
    }

    #[cfg(test)]
    pub(crate) fn chat_for_test(&self) -> &gpui::Entity<ChatPanel> {
        &self.chat
    }

    #[cfg(test)]
    pub(crate) fn root_for_test(&self) -> Option<&std::path::Path> {
        self.root.as_deref()
    }

    pub fn set_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        if same_project_root(self.root.as_deref(), root.as_deref()) {
            return;
        }
        self.root_generation = self.root_generation.wrapping_add(1);
        self.root = root;
        for tab in self.tabs.drain(..) {
            if let FileTabStatus::Image { image, .. } = tab.status {
                image.remove_asset(cx);
            }
        }
        self.active = ActiveContent::Chat;
        self.image = None;
        cx.notify();
    }

    fn open_file(
        &mut self,
        source_root: PathBuf,
        relative_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = source_tab_id(&source_root, &relative_path);
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.active = ActiveContent::File(id);
            self.refresh_active_editor(window, cx);
            cx.notify();
            return;
        }
        if self.tabs.len() >= MAX_FILE_TABS {
            let removed = self.tabs.remove(0);
            if let FileTabStatus::Image { image, .. } = removed.status {
                image.remove_asset(cx);
            }
        }
        self.next_tab_generation = self.next_tab_generation.wrapping_add(1);
        let generation = self.next_tab_generation;
        self.tabs.push(FileTab {
            id: id.clone(),
            label: file_label(&relative_path),
            source_root: source_root.clone(),
            relative_path: relative_path.clone(),
            kind: FileTabKind::Source,
            generation,
            status: FileTabStatus::Loading,
        });
        self.active = ActiveContent::File(id.clone());
        self.image = None;
        cx.notify();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |panel, cx| {
            let result = executor
                .spawn(async move {
                    let files = pi_data::ProjectFiles::open(&source_root)?;
                    files.read(&relative_path)
                })
                .await;
            let _ = cx.update(|window, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    if panel.finish_file_load(&id, generation, result) {
                        if panel.active == ActiveContent::File(id.clone()) {
                            panel.refresh_active_editor(window, cx);
                        }
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    fn open_diff(
        &mut self,
        source_root: PathBuf,
        relative_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = diff_tab_id(&source_root, &relative_path);
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.active = ActiveContent::File(id);
            self.refresh_active_editor(window, cx);
            cx.notify();
            return;
        }
        if self.tabs.len() >= MAX_FILE_TABS {
            let removed = self.tabs.remove(0);
            if let FileTabStatus::Image { image, .. } = removed.status {
                image.remove_asset(cx);
            }
        }
        self.next_tab_generation = self.next_tab_generation.wrapping_add(1);
        let generation = self.next_tab_generation;
        self.tabs.push(FileTab {
            id: id.clone(),
            label: format!("{} · Diff", file_label(&relative_path)),
            source_root: source_root.clone(),
            relative_path: relative_path.clone(),
            kind: FileTabKind::Diff,
            generation,
            status: FileTabStatus::Loading,
        });
        self.active = ActiveContent::File(id.clone());
        self.image = None;
        cx.notify();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |panel, cx| {
            let result = executor
                .spawn(async move { pi_data::git_file_diff(&source_root, &relative_path) })
                .await;
            let _ = cx.update(|_, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    if panel.finish_diff_load(&id, generation, result) {
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    fn finish_diff_load(
        &mut self,
        id: &str,
        generation: u64,
        result: Result<pi_data::GitFileDiff, pi_data::GitError>,
    ) -> bool {
        let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id && tab.generation == generation)
        else {
            return false;
        };
        tab.status = match result {
            Ok(pi_data::GitFileDiff::Supported { kind, patch }) => FileTabStatus::Diff {
                kind,
                diff: Arc::new(pi_render::parse_unified_diff(&patch)),
            },
            Ok(pi_data::GitFileDiff::Unsupported(reason)) => FileTabStatus::Unsupported {
                size: None,
                reason: format!("此改动不支持原生 diff：{}", diff_unsupported_label(&reason)),
            },
            Err(error) => FileTabStatus::Error(error.to_string()),
        };
        true
    }

    fn finish_file_load(
        &mut self,
        id: &str,
        generation: u64,
        result: Result<pi_data::FileContent, pi_data::FileAccessError>,
    ) -> bool {
        let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id && tab.generation == generation)
        else {
            return false;
        };
        tab.status = match result {
            Ok(pi_data::FileContent::Text(content)) => FileTabStatus::Text(Arc::new(content)),
            Ok(pi_data::FileContent::Image(content)) => {
                let kind = content.kind;
                let size = content.size;
                let image = image_format(kind)
                    .map(|format| Arc::new(Image::from_bytes(format, content.bytes)));
                match image {
                    Some(image) => FileTabStatus::Image { kind, size, image },
                    None => FileTabStatus::Error("不支持的图片格式".to_owned()),
                }
            }
            Ok(pi_data::FileContent::Unsupported { size, reason }) => FileTabStatus::Unsupported {
                size: Some(size),
                reason,
            },
            Err(error) => FileTabStatus::Error(error.to_string()),
        };
        true
    }

    fn refresh_active_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_active_editor_without_window();
        if let Some((text, language)) = self.active_text() {
            let editor = self.editor.clone();
            let text = text.to_owned();
            editor.update(cx, |state, cx| {
                state.set_highlighter(language, cx);
                state.set_value(text, window, cx);
            });
        }
    }

    fn refresh_active_editor_without_window(&mut self) {
        self.image = match &self.active_tab().map(|tab| &tab.status) {
            Some(FileTabStatus::Image { image, .. }) => Some(image.clone()),
            _ => None,
        };
    }

    fn active_tab(&self) -> Option<&FileTab> {
        let ActiveContent::File(id) = &self.active else {
            return None;
        };
        self.tabs.iter().find(|tab| &tab.id == id)
    }

    fn active_text(&self) -> Option<(&str, &'static str)> {
        match &self.active_tab()?.status {
            FileTabStatus::Text(content) => Some((&content.text, content.language)),
            _ => None,
        }
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == 0 {
            self.active = ActiveContent::Chat;
            self.image = None;
        } else if let Some(tab) = self.tabs.get(index - 1) {
            self.active = ActiveContent::File(tab.id.clone());
            self.refresh_active_editor(window, cx);
        }
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file_index) = index.checked_sub(1) else {
            return;
        };
        if file_index >= self.tabs.len() {
            return;
        }
        let was_active = self
            .tabs
            .get(file_index)
            .is_some_and(|tab| self.active == ActiveContent::File(tab.id.clone()));
        let removed = self.tabs.remove(file_index);
        if let FileTabStatus::Image { image, .. } = removed.status {
            image.remove_asset(cx);
        }
        if was_active {
            if self.tabs.is_empty() {
                self.active = ActiveContent::Chat;
                self.image = None;
            } else {
                let next_index = file_index.min(self.tabs.len() - 1);
                self.active = ActiveContent::File(self.tabs[next_index].id.clone());
                self.refresh_active_editor(window, cx);
            }
        }
        cx.notify();
    }

    fn retry_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else {
            return;
        };
        self.next_tab_generation = self.next_tab_generation.wrapping_add(1);
        let generation = self.next_tab_generation;
        if let Some(candidate) = self
            .tabs
            .iter_mut()
            .find(|candidate| candidate.id == tab.id)
        {
            candidate.generation = generation;
            candidate.status = FileTabStatus::Loading;
        }
        self.image = None;
        cx.notify();
        let executor = cx.background_executor().clone();
        match tab.kind {
            FileTabKind::Source => {
                let id = tab.id;
                let source_root = tab.source_root;
                let relative_path = tab.relative_path;
                cx.spawn_in(window, async move |panel, cx| {
                    let result = executor
                        .spawn(async move {
                            let files = pi_data::ProjectFiles::open(&source_root)?;
                            files.read(&relative_path)
                        })
                        .await;
                    let _ = cx.update(|window, cx| {
                        let _ = panel.update(cx, |panel, cx| {
                            if panel.finish_file_load(&id, generation, result) {
                                if panel.active == ActiveContent::File(id.clone()) {
                                    panel.refresh_active_editor(window, cx);
                                }
                                cx.notify();
                            }
                        });
                    });
                })
                .detach();
            }
            FileTabKind::Diff => {
                let id = tab.id;
                let source_root = tab.source_root;
                let relative_path = tab.relative_path;
                cx.spawn_in(window, async move |panel, cx| {
                    let result = executor
                        .spawn(async move { pi_data::git_file_diff(source_root, relative_path) })
                        .await;
                    let _ = panel.update(cx, |panel, cx| {
                        if panel.finish_diff_load(&id, generation, result) {
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
    }

    fn tab_models(&self) -> Vec<WorkspaceContentTab> {
        let mut tabs = vec![WorkspaceContentTab::new(CHAT_TAB_ID, "对话", "对话").fixed()];
        tabs.extend(self.tabs.iter().map(|tab| {
            WorkspaceContentTab::new(
                tab.id.clone(),
                tab.label.clone(),
                tab.relative_path.to_string_lossy().into_owned(),
            )
        }));
        tabs
    }

    fn selected_index(&self) -> usize {
        match &self.active {
            ActiveContent::Chat => 0,
            ActiveContent::File(id) => self
                .tabs
                .iter()
                .position(|tab| &tab.id == id)
                .map_or(0, |index| index + 1),
        }
    }

    fn render_file(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(tab) = self.active_tab() else {
            return centered_state(IconName::File, "没有打开的文件", "从右侧文件树选择文件", cx);
        };
        let relative = tab.relative_path.to_string_lossy().into_owned();
        let is_error = matches!(tab.status, FileTabStatus::Error(_));
        let toolbar = h_flex()
            .flex_none()
            .h_8()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(relative),
            )
            .when(is_error, |row| {
                row.child(
                    Button::new("retry-file")
                        .small()
                        .ghost()
                        .label("重试")
                        .on_click(
                            cx.listener(|panel, _, window, cx| panel.retry_active(window, cx)),
                        ),
                )
            });
        let body = match &tab.status {
            FileTabStatus::Loading => centered_state(
                IconName::LoaderCircle,
                "正在后台读取文件…",
                "仅访问当前项目目录",
                cx,
            ),
            FileTabStatus::Diff { kind, diff } => v_flex()
                .debug_selector(|| "git-diff-viewer".into())
                .size_full()
                .child(
                    h_flex()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("Git {}", git_status_label(*kind))),
                )
                .child(
                    // standalone 文件 tab 允许单一容器双轴滚动；聊天里的 diff renderer
                    // 仍由消息流统一滚动，不引入嵌套滚动。
                    div()
                        .debug_selector(|| "standalone-diff-scroll".into())
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_scrollbar()
                        .p_3()
                        .child(gpui_pi_ui::DiffView::new(diff.clone())),
                )
                .into_any_element(),
            FileTabStatus::Text(content) => v_flex()
                .debug_selector(|| "file-text-viewer".into())
                .size_full()
                .child(
                    h_flex()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .gap_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(content.language)
                        .child(format!("{} 行", content.lines))
                        .child(format_size(content.size)),
                )
                .child(
                    div().flex_1().min_h_0().child(
                        div()
                            .debug_selector(|| "readonly-file-editor".into())
                            .size_full()
                            .child(
                                Editor::new(&self.editor)
                                    .appearance(false)
                                    .bordered(false)
                                    .readonly(true)
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_xs()
                                    .size_full(),
                            ),
                    ),
                )
                .into_any_element(),
            FileTabStatus::Image { kind, size, .. } => {
                let image = self.image.clone();
                v_flex()
                    .debug_selector(|| "file-image-viewer".into())
                    .size_full()
                    .child(
                        h_flex()
                            .flex_none()
                            .px_3()
                            .py_1()
                            .gap_3()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(kind.mime_type())
                            .child(format_size(*size)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .p_4()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(cx.theme().muted.opacity(0.42))
                            .children(image.map(|image| {
                                img(image)
                                    .debug_selector(|| "file-preview-image".into())
                                    .max_w_full()
                                    .max_h_full()
                            })),
                    )
                    .into_any_element()
            }
            FileTabStatus::Unsupported { size, reason } => centered_state(
                IconName::File,
                "此文件不在原生安全预览范围内",
                size.map_or_else(
                    || reason.clone(),
                    |size| format!("{reason} · {}", format_size(size)),
                ),
                cx,
            ),
            FileTabStatus::Error(error) => {
                centered_state(IconName::TriangleAlert, "文件读取失败", error, cx)
            }
        };
        v_flex()
            .size_full()
            .min_h_0()
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

impl EventEmitter<PanelEvent> for MainPanel {}

impl Focusable for MainPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for MainPanel {
    fn panel_name(&self) -> &'static str {
        "gpui-pi-main"
    }

    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some("工作区".into())
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "工作区"
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

impl Render for MainPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.chat
            .update(cx, |chat, cx| chat.process_extension_ui(window, cx));
        let panel = cx.entity();
        let select_panel = panel.clone();
        let close_panel = panel.clone();
        let body = match self.active {
            ActiveContent::Chat => self.chat.clone().into_any_element(),
            ActiveContent::File(_) => self.render_file(cx),
        };
        v_flex()
            .id("main-workspace")
            .debug_selector(|| "main-workspace".into())
            .track_focus(&self.focus_handle)
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(
                WorkspaceContentTabs::new(self.tab_models(), self.selected_index())
                    .on_select(move |index, window, cx| {
                        select_panel.update(cx, |panel, cx| panel.select_tab(index, window, cx));
                    })
                    .on_close(move |index, window, cx| {
                        close_panel.update(cx, |panel, cx| panel.close_tab(index, window, cx));
                    }),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

fn same_project_root(left: Option<&std::path::Path>, right: Option<&std::path::Path>) -> bool {
    left.map(pi_data::project_identity_key) == right.map(pi_data::project_identity_key)
}

fn source_tab_id(root: &std::path::Path, path: &std::path::Path) -> String {
    format!(
        "file:{}:{}",
        pi_data::project_identity_key(root),
        path.to_string_lossy().replace('\\', "/")
    )
}

fn diff_tab_id(root: &std::path::Path, path: &std::path::Path) -> String {
    format!(
        "diff:{}:{}",
        pi_data::project_identity_key(root),
        path.to_string_lossy().replace('\\', "/")
    )
}

fn file_label(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn image_format(kind: pi_data::ImageKind) -> Option<ImageFormat> {
    match kind {
        pi_data::ImageKind::Png => Some(ImageFormat::Png),
        pi_data::ImageKind::Jpeg => Some(ImageFormat::Jpeg),
        pi_data::ImageKind::Gif => Some(ImageFormat::Gif),
        pi_data::ImageKind::Webp => Some(ImageFormat::Webp),
        pi_data::ImageKind::Bmp => Some(ImageFormat::Bmp),
        pi_data::ImageKind::Ico => Some(ImageFormat::Ico),
    }
}

fn git_status_label(kind: pi_data::GitFileStatusKind) -> &'static str {
    match kind {
        pi_data::GitFileStatusKind::Modified => "已修改",
        pi_data::GitFileStatusKind::Added => "已新增",
        pi_data::GitFileStatusKind::Deleted => "已删除",
        pi_data::GitFileStatusKind::Renamed => "已重命名",
        pi_data::GitFileStatusKind::Untracked => "未跟踪",
        pi_data::GitFileStatusKind::Conflict => "有冲突",
    }
}

fn diff_unsupported_label(reason: &pi_data::GitDiffUnsupported) -> &'static str {
    match reason {
        pi_data::GitDiffUnsupported::NoChanges => "没有改动",
        pi_data::GitDiffUnsupported::Binary => "二进制文件",
        pi_data::GitDiffUnsupported::TooLarge => "文件过大",
        pi_data::GitDiffUnsupported::NotAFile => "不是普通文件",
        pi_data::GitDiffUnsupported::MissingHunk => "没有可显示的 hunk",
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn centered_state(
    icon: IconName,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    cx: &App,
) -> gpui::AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .p_6()
        .child(gpui_component::Icon::new(icon).size_6())
        .child(div().font_semibold().child(title.into()))
        .child(
            div()
                .max_w(px(520.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(detail.into()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MainPanelDialogHarness {
        panel: gpui::Entity<MainPanel>,
    }

    impl Render for MainPanelDialogHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.panel.clone())
                .children(gpui_component::Root::render_dialog_layer(window, cx))
                .children(gpui_component::Root::render_notification_layer(window, cx))
        }
    }

    #[derive(Default)]
    struct RecordingResponseSender {
        responses: Mutex<Vec<pi_rpc::ExtensionUiResponse>>,
    }

    impl crate::live_session::ExtensionResponseSender for RecordingResponseSender {
        fn send(&self, response: pi_rpc::ExtensionUiResponse) -> Result<(), String> {
            self.responses.lock().unwrap().push(response);
            Ok(())
        }
    }

    #[gpui::test]
    fn extension_ui_is_driven_while_file_tab_is_active(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let sender = Arc::new(RecordingResponseSender::default());
        let test_sender = sender.clone();
        let handle = cx.open_window(gpui::size(px(800.), px(560.)), move |window, cx| {
            let explorer = cx.new(|cx| crate::file_explorer::FileExplorerPanel::new(window, cx));
            let chat = cx.new(|cx| {
                ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                )
            });
            chat.update(cx, |chat, _| {
                chat.set_extension_response_sender_for_test(test_sender.clone());
            });
            let panel = cx.new(|cx| MainPanel::new(chat.clone(), &explorer, window, cx));
            panel.update(cx, |panel, cx| {
                panel.tabs.push(FileTab {
                    id: "file:fixture".to_owned(),
                    source_root: PathBuf::from("C:/fixture"),
                    relative_path: PathBuf::from("fixture.txt"),
                    label: "fixture.txt".to_owned(),
                    kind: FileTabKind::Source,
                    generation: 1,
                    status: FileTabStatus::Text(Arc::new(pi_data::TextFileContent {
                        text: "fixture".to_owned(),
                        language: "text",
                        lines: 1,
                        size: 7,
                    })),
                });
                panel.active = ActiveContent::File("file:fixture".to_owned());
                cx.notify();
            });
            *output.borrow_mut() = Some((panel.clone(), chat));
            let harness = cx.new(|_| MainPanelDialogHarness { panel });
            gpui_component::Root::new(harness, window, cx)
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        let (panel, chat) = captured.borrow().clone().unwrap();
        chat.update(cx, |chat, cx| {
            for (id, request) in [
                (
                    "confirm",
                    pi_rpc::ExtensionUiRequest::Confirm {
                        title: "Confirm".into(),
                        message: "Continue?".into(),
                        timeout: None,
                    },
                ),
                (
                    "select",
                    pi_rpc::ExtensionUiRequest::Select {
                        title: "Select".into(),
                        options: vec!["Alpha".into()],
                        timeout: None,
                    },
                ),
                (
                    "input",
                    pi_rpc::ExtensionUiRequest::Input {
                        title: "Input".into(),
                        placeholder: Some("value".into()),
                        timeout: None,
                    },
                ),
                (
                    "editor-dialog",
                    pi_rpc::ExtensionUiRequest::Editor {
                        title: "Editor".into(),
                        prefill: Some("prefill".into()),
                    },
                ),
                (
                    "notify",
                    pi_rpc::ExtensionUiRequest::Notify {
                        message: "file tab notification".into(),
                        notify_type: None,
                    },
                ),
                (
                    "title",
                    pi_rpc::ExtensionUiRequest::SetTitle {
                        title: "File Tab Extension".into(),
                    },
                ),
                (
                    "editor",
                    pi_rpc::ExtensionUiRequest::SetEditorText {
                        text: "hidden chat editor".into(),
                    },
                ),
                (
                    "status",
                    pi_rpc::ExtensionUiRequest::SetStatus {
                        status_key: "fixture".into(),
                        status_text: Some("ready".into()),
                    },
                ),
                (
                    "widget",
                    pi_rpc::ExtensionUiRequest::SetWidget {
                        widget_key: "fixture".into(),
                        widget_lines: Some(vec!["one\ntwo".into()]),
                        widget_placement: None,
                    },
                ),
            ] {
                chat.apply_extension_request_for_test(id, request);
            }
            cx.notify();
        });
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert!(visual.debug_bounds("extension-confirm-submit").is_some());
        assert_eq!(visual.window_title().as_deref(), Some("File Tab Extension"));
        visual.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        let option = visual
            .debug_bounds("extension-select-option-0")
            .expect("select dialog did not advance while file tab active");
        visual.simulate_click(option.center(), Default::default());
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert!(visual.debug_bounds("extension-dialog-submit").is_some());
        visual.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert!(visual.debug_bounds("extension-dialog-submit").is_some());
        visual.dispatch_action(gpui_component::dialog::Cancel);
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert_eq!(sender.responses.lock().unwrap().len(), 4);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.select_tab(0, window, cx));
        });
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert!(visual.debug_bounds("extension-widgets-above").is_some());
        assert!(visual.debug_bounds("extension-status-item-0").is_some());
        assert_eq!(
            chat.read_with(cx, |chat, cx| chat.composer_value_for_test(cx)),
            "hidden chat editor"
        );
    }

    #[gpui::test]
    fn standalone_diff_tab_renders_scrollbar_overlay_for_both_axis_overflow(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let handle = cx.open_window(gpui::size(px(800.), px(560.)), move |window, cx| {
            let explorer = cx.new(|cx| crate::file_explorer::FileExplorerPanel::new(window, cx));
            let chat = cx.new(|cx| {
                ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                )
            });
            let panel = cx.new(|cx| MainPanel::new(chat, &explorer, window, cx));
            panel.update(cx, |panel, cx| {
                let patch = (0..80)
                    .map(|line| format!("fixture {line:03} {}", "x".repeat(320)))
                    .collect::<Vec<_>>()
                    .join("\n");
                panel.tabs.push(FileTab {
                    id: "diff:fixture".to_owned(),
                    source_root: PathBuf::from("C:/fixture"),
                    relative_path: PathBuf::from("src/main.rs"),
                    label: "main.rs".to_owned(),
                    kind: FileTabKind::Diff,
                    generation: 1,
                    status: FileTabStatus::Diff {
                        kind: pi_data::GitFileStatusKind::Modified,
                        diff: Arc::new(pi_render::parse_unified_diff(&patch)),
                    },
                });
                panel.active = ActiveContent::File("diff:fixture".to_owned());
                cx.notify();
            });
            *output.borrow_mut() = Some(panel.clone());
            gpui_component::Root::new(panel, window, cx)
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        let overlay = visual
            .debug_bounds("scrollbar-overlay")
            .expect("gpui-component Scrollable scrollbar overlay missing");
        assert!(overlay.size.width > px(0.) && overlay.size.height > px(0.));
    }

    #[test]
    fn tab_identity_normalizes_windows_separators() {
        assert_eq!(
            source_tab_id(
                std::path::Path::new("C:/repo"),
                std::path::Path::new(r"src\main.rs")
            ),
            format!(
                "file:{}:src/main.rs",
                pi_data::project_identity_key(std::path::Path::new("C:/repo"))
            )
        );
    }

    #[gpui::test]
    fn stale_file_load_is_rejected(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        cx.open_window(gpui::size(px(800.), px(560.)), move |window, cx| {
            let explorer = cx.new(|cx| crate::file_explorer::FileExplorerPanel::new(window, cx));
            let chat = cx.new(|cx| {
                ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                )
            });
            let panel = cx.new(|cx| MainPanel::new(chat, &explorer, window, cx));
            *output.borrow_mut() = Some(panel.clone());
            gpui_component::Root::new(panel, window, cx)
        });
        let panel = captured.borrow().clone().unwrap();
        panel.update(cx, |panel, _| {
            panel.tabs.push(FileTab {
                id: "file:a".to_owned(),
                source_root: PathBuf::from("C:/root"),
                relative_path: PathBuf::from("a"),
                label: "a".to_owned(),
                kind: FileTabKind::Source,
                generation: 3,
                status: FileTabStatus::Loading,
            });
            let result = Ok(pi_data::FileContent::Text(pi_data::TextFileContent {
                text: "old".to_owned(),
                language: "text",
                size: 3,
                lines: 1,
            }));
            assert!(panel.finish_file_load("file:a", 3, result));
            assert!(matches!(panel.tabs[0].status, FileTabStatus::Text(_)));
            let result = Ok(pi_data::FileContent::Text(pi_data::TextFileContent {
                text: "old".to_owned(),
                language: "text",
                size: 3,
                lines: 1,
            }));
            assert!(!panel.finish_file_load("file:a", 2, result));
            assert!(matches!(panel.tabs[0].status, FileTabStatus::Text(_)));
        });
    }

    #[gpui::test]
    fn tab_selection_and_close_keep_chat_offset_correct(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let window = cx.open_window(gpui::size(px(800.), px(560.)), move |window, cx| {
            let explorer = cx.new(|cx| crate::file_explorer::FileExplorerPanel::new(window, cx));
            let chat = cx.new(|cx| {
                ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                )
            });
            let panel = cx.new(|cx| MainPanel::new(chat, &explorer, window, cx));
            *output.borrow_mut() = Some(panel.clone());
            gpui_component::Root::new(panel, window, cx)
        });
        let panel = captured.borrow().clone().unwrap();
        let _ = window.update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.tabs = vec![
                    FileTab {
                        id: "file:a".to_owned(),
                        source_root: PathBuf::from("C:/a"),
                        relative_path: PathBuf::from("a"),
                        label: "a".to_owned(),
                        kind: FileTabKind::Source,
                        generation: 1,
                        status: FileTabStatus::Loading,
                    },
                    FileTab {
                        id: "file:b".to_owned(),
                        source_root: PathBuf::from("C:/b"),
                        relative_path: PathBuf::from("b"),
                        label: "b".to_owned(),
                        kind: FileTabKind::Source,
                        generation: 2,
                        status: FileTabStatus::Loading,
                    },
                ];
                panel.select_tab(2, window, cx);
                assert_eq!(panel.active, ActiveContent::File("file:b".to_owned()));
                panel.close_tab(0, window, cx);
                assert_eq!(panel.tabs.len(), 2);
                panel.close_tab(2, window, cx);
                assert_eq!(panel.active, ActiveContent::File("file:a".to_owned()));
                panel.close_tab(1, window, cx);
                assert_eq!(panel.active, ActiveContent::Chat);
            });
        });
    }

    #[test]
    fn diff_tab_identity_separates_worktrees_with_same_relative_path() {
        let path = std::path::Path::new("src/main.rs");
        assert_ne!(
            diff_tab_id(std::path::Path::new("C:/repo-a"), path),
            diff_tab_id(std::path::Path::new("C:/repo-b"), path)
        );
    }

    #[gpui::test]
    fn stale_diff_load_is_rejected(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        cx.open_window(gpui::size(px(800.), px(560.)), move |window, cx| {
            let explorer = cx.new(|cx| crate::file_explorer::FileExplorerPanel::new(window, cx));
            let chat = cx.new(|cx| {
                ChatPanel::new(
                    pi_runtime::RuntimeManager::new(Default::default()),
                    window,
                    cx,
                )
            });
            let panel = cx.new(|cx| MainPanel::new(chat, &explorer, window, cx));
            *output.borrow_mut() = Some(panel.clone());
            gpui_component::Root::new(panel, window, cx)
        });
        let panel = captured.borrow().clone().unwrap();
        panel.update(cx, |panel, _| {
            panel.tabs.push(FileTab {
                id: "diff:a".to_owned(),
                source_root: PathBuf::from("C:/root"),
                relative_path: PathBuf::from("a"),
                label: "a".to_owned(),
                kind: FileTabKind::Diff,
                generation: 8,
                status: FileTabStatus::Loading,
            });
            let result = Ok(pi_data::GitFileDiff::Supported {
                kind: pi_data::GitFileStatusKind::Modified,
                patch: "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new".to_owned(),
            });
            assert!(!panel.finish_diff_load("diff:a", 7, result));
            assert!(matches!(panel.tabs[0].status, FileTabStatus::Loading));
        });
    }

    #[test]
    fn format_sizes_are_stable() {
        assert_eq!(format_size(10), "10 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0 MiB");
    }
}
