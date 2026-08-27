use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use gpui::{
    App, Bounds, Div, FontStyle, FontWeight, HighlightStyle, Hsla, Image, ImageFormat,
    InteractiveElement as _, IntoElement, ListOffset, ListState, ParentElement as _, Pixels, Point,
    SharedString, Size, StatefulInteractiveElement as _, Styled as _, StyledText, Window, div, img,
    list, prelude::FluentBuilder as _, px, relative,
};
use gpui_base::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex, h_resizable, resizable_panel,
    scroll::ScrollableElement as _,
    text::TextView,
    v_flex,
};

type TailAttachmentHandler = Arc<dyn Fn(bool, &mut Window, &mut App)>;
type TailDetachHandler = Arc<dyn Fn(&mut Window, &mut App)>;
type DetailToggleHandler = Arc<dyn Fn(String, String, &mut App)>;
type ProcessToggleHandler = Arc<dyn Fn(String, &mut App)>;
type MinimapToggleHandler = Arc<dyn Fn(&mut App)>;
type MessagePaneBoundsHandler = Arc<dyn Fn(Bounds<Pixels>, &mut App)>;
type OpenWrittenFileHandler = Arc<dyn Fn(PathBuf, &mut App)>;
type ForkMessageHandler = Arc<dyn Fn(String, &mut App)>;

use pi_render::{
    AnsiColor, AnsiStyle, AnsiText, Block, CodeBlock, ConversationDocument, ConversationItem,
    FrontmatterCard, ImageBlock, ImageState, Message, MessageRole, ProcessGroup, SubagentCard,
    SubagentStats, SubagentStatus, ToolCard, ToolOutput, ToolStatus,
};

fn detail_key(message_id: &str, block_index: usize, kind: &str) -> String {
    format!("{message_id}:{kind}:{block_index}")
}

/// 卡片统一弱边框。
///
/// 规范 S-4 / 红线 3：状态只能以点、图标、文字、竖条呈现，描整卡的边框必须与状态无关，
/// 否则一屏里几张不同状态的卡片会互相抢视觉重心。
fn card_border(cx: &App) -> Hsla {
    cx.theme().border.opacity(0.8)
}

/// 从属内容的「左竖线 + 缩进」容器（规范 5.3）。
///
/// 工具输出、思考正文、diff 都用它表达隶属于上一层，而不是再套一张描边卡片（红线 2）。
/// 行距由调用方按内容密度自己给：diff 要逐行紧贴，工具输出要 `gap_2`。
fn subordinate_column(cx: &App) -> Div {
    v_flex()
        .min_w_0()
        .ml_1p5()
        .pl_3p5()
        .border_l_1()
        .border_color(card_border(cx))
}

/// 状态点（规范 4.5）：8px 圆点，是卡片里唯一允许上状态色的地方。
fn status_dot(color: Hsla) -> Div {
    div().size_2().flex_none().rounded_full().bg(color)
}

/// 折叠指示箭头。放在 header 最右侧，与状态文字分列两端。
const fn disclosure_icon(expanded: bool) -> IconName {
    if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}

/// 消息列最大行宽（规范 S-13；`820` 在红线 4 的 `px(n)` 白名单内）。
///
/// 通栏长行是「读起来累」的首因：超宽窗口只允许留白变大，不允许行长变长。
pub(crate) const MESSAGE_COLUMN_MAX_WIDTH: f32 = 820.;

/// 消息正文与用户气泡共用的行高倍数（规范 S-18 / § 5.8）。
///
/// 组件库 typography token 的默认行高约 1.43，比阅读基线（pi-web 1.7）明显偏挤；
/// 两处必须引用同一常量，禁止各写各的字面量（守卫见测试）。
pub(crate) const BODY_LINE_HEIGHT: f32 = 1.7;

/// 用户气泡的可断言样式（规范 S-14 / § 5.8）。
///
/// `debug_bounds` 只能断位置与尺寸，颜色/圆角/宽度比例断不了，
/// 所以把这几个值收进纯函数，渲染与测试消费同一来源。
pub(crate) struct UserBubbleStyle {
    pub bg: Hsla,
    pub border: Hsla,
    pub selected_border: Hsla,
    pub radius: Pixels,
    pub max_w_ratio: f32,
}

pub(crate) fn user_bubble_style(cx: &App) -> UserBubbleStyle {
    // 规范 v2.2 勘误：`accent` 在 gpui-component 里是中性 hover 色，10% 透明度铺不出
    // 可见的身份色。气泡走 base.blue（浅 blue-600 / 深 blue-400），即 pi-web 的用户蓝。
    let identity = cx.theme().blue;
    UserBubbleStyle {
        bg: identity.opacity(0.10),
        border: identity.opacity(0.2),
        selected_border: identity,
        radius: px(12.),
        max_w_ratio: 0.85,
    }
}

/// 消息列容器（规范 S-13）：820px 居中列 + 列外 16px 留白。
///
/// 消息流是虚拟化 `list`，没有统一的内容父节点，所以这层必须套在每一个表项外层
/// （消息与处理详情组都算）。
fn message_column(content: gpui::AnyElement) -> gpui::AnyElement {
    div()
        .w_full()
        .px_4()
        .flex()
        .justify_center()
        .child(
            div()
                .debug_selector(|| "message-column".into())
                .w_full()
                .min_w_0()
                .max_w(px(MESSAGE_COLUMN_MAX_WIDTH))
                .child(content),
        )
        .into_any_element()
}

/// 该角色的消息是否渲染成右对齐气泡。
///
/// 规范 S-14 / 红线 10：用户消息是全应用唯一允许给消息本体上底色的地方，
/// 其余角色一律无框无底的纯文本流。抽成纯函数以便单测直接断言。
const fn message_is_bubbled(role: MessageRole) -> bool {
    matches!(role, MessageRole::User)
}

/// 工具状态 → （状态文字, 状态色）。抽成纯函数是为了让「边框不随状态变化」可被单测断言。
fn tool_status_style(status: ToolStatus, cx: &App) -> (&'static str, Hsla) {
    match status {
        ToolStatus::Pending => ("pending", cx.theme().warning),
        ToolStatus::Success => ("success", cx.theme().success),
        ToolStatus::Error => ("error", cx.theme().danger),
        ToolStatus::Empty => ("empty", cx.theme().muted_foreground),
    }
}

/// 子代理状态 → 状态点颜色（规范 S-4：状态色只出现在点上，不描边、不铺底）。
pub(crate) fn subagent_status_color(status: &SubagentStatus, cx: &App) -> Hsla {
    match status {
        SubagentStatus::Queued => cx.theme().muted_foreground,
        SubagentStatus::Running => cx.theme().warning,
        SubagentStatus::Completed => cx.theme().success,
        SubagentStatus::Error => cx.theme().danger,
        // 主动停止与达到轮次上限都是「没跑完但也不是故障」，用 warning 而不是 danger，
        // 免得一屏里正常收尾的任务和真出错的任务看起来一样严重。
        SubagentStatus::Stopped | SubagentStatus::TurnLimit => cx.theme().warning,
        SubagentStatus::Unknown(_) => cx.theme().info,
    }
}

/// 统计摘要，按「有什么显示什么」拼成一行。
///
/// 规范 S-8 限制一行最多 3 个文本片段，所以这里整体只当**一个**片段（自己内部用 ·
/// 连接的短标记不再拆成独立文本节点），完整明细留给展开区。
pub(crate) fn subagent_stats_summary(stats: &SubagentStats) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(turns) = stats.turn_count {
        match stats.max_turns {
            Some(max) => parts.push(format!("{turns}/{max} 轮")),
            None => parts.push(format!("{turns} 轮")),
        }
    }
    if let Some(tools) = stats.tool_uses {
        parts.push(format!("{tools} 次工具"));
    }
    if let Some(duration) = stats.duration_ms {
        parts.push(format_duration_ms(duration));
    }
    if let Some(cost) = stats.cost
        && cost > 0.
    {
        parts.push(format!("${cost:.4}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// 毫秒 → 人读时长。超过一分钟就不再显示小数秒，长任务看分秒比看 `183.4s` 直观。
fn format_duration_ms(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1000;
    if total_seconds >= 60 {
        format!("{}m{:02}s", total_seconds / 60, total_seconds % 60)
    } else {
        format!("{:.1}s", duration_ms as f64 / 1000.)
    }
}

#[derive(Clone)]
struct ChatScrollbarHandle(ListState);

impl ScrollbarHandle for ChatScrollbarHandle {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.0.viewport_bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        self.0.scroll_px_offset_for_scrollbar()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.0.set_offset_from_scrollbar(offset);
    }

    fn content_size(&self) -> Size<Pixels> {
        self.0.viewport_bounds().size + self.0.max_offset_for_scrollbar().into()
    }

    fn start_drag(&self) {
        if self.0.is_scrolled_to_end().is_none() {
            // 未知总高度不能被冻结；下一帧先补齐测量，拖拽沿用新的几何。
            let _ = self.0.clone().measure_all();
        } else {
            self.0.scrollbar_drag_started();
        }
    }

    fn end_drag(&self) {
        self.0.scrollbar_drag_ended();
    }
}

#[derive(Clone, IntoElement)]
pub struct ChatWindow {
    document: Arc<ConversationDocument>,
    model_names: Arc<HashMap<String, String>>,
    list_state: ListState,
    selected_message: Option<String>,
    show_minimap: bool,
    expanded_tools: Arc<HashSet<String>>,
    expanded_processes: Arc<HashSet<String>>,
    on_toggle_tool: Option<DetailToggleHandler>,
    on_toggle_process: Option<ProcessToggleHandler>,
    on_toggle_minimap: Option<MinimapToggleHandler>,
    on_message_pane_bounds: Option<MessagePaneBoundsHandler>,
    on_tail_attachment_change: Option<TailAttachmentHandler>,
    on_tail_detach: Option<TailDetachHandler>,
    on_open_written_file: Option<OpenWrittenFileHandler>,
    on_fork_message: Option<ForkMessageHandler>,
}

impl ChatWindow {
    pub fn new(document: Arc<ConversationDocument>, list_state: ListState) -> Self {
        Self {
            document,
            model_names: Arc::new(HashMap::new()),
            list_state,
            selected_message: None,
            show_minimap: true,
            expanded_tools: Arc::new(HashSet::new()),
            expanded_processes: Arc::new(HashSet::new()),
            on_toggle_tool: None,
            on_toggle_process: None,
            on_toggle_minimap: None,
            on_message_pane_bounds: None,
            on_tail_attachment_change: None,
            on_tail_detach: None,
            on_open_written_file: None,
            on_fork_message: None,
        }
    }

    pub fn model_names(mut self, names: Arc<HashMap<String, String>>) -> Self {
        self.model_names = names;
        self
    }

    pub fn show_minimap(mut self, show: bool) -> Self {
        self.show_minimap = show;
        self
    }

    pub fn expanded_tools(mut self, expanded: Arc<HashSet<String>>) -> Self {
        self.expanded_tools = expanded;
        self
    }

    pub fn expanded_processes(mut self, expanded: Arc<HashSet<String>>) -> Self {
        self.expanded_processes = expanded;
        self
    }

    pub fn on_toggle_tool(mut self, handler: impl Fn(String, String, &mut App) + 'static) -> Self {
        self.on_toggle_tool = Some(Arc::new(handler));
        self
    }

    pub fn on_toggle_process(mut self, handler: impl Fn(String, &mut App) + 'static) -> Self {
        self.on_toggle_process = Some(Arc::new(handler));
        self
    }

    pub fn on_toggle_minimap(mut self, handler: impl Fn(&mut App) + 'static) -> Self {
        self.on_toggle_minimap = Some(Arc::new(handler));
        self
    }

    pub fn on_message_pane_bounds(
        mut self,
        handler: impl Fn(Bounds<Pixels>, &mut App) + 'static,
    ) -> Self {
        self.on_message_pane_bounds = Some(Arc::new(handler));
        self
    }

    pub fn on_tail_attachment_change(
        mut self,
        handler: impl Fn(bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_tail_attachment_change = Some(Arc::new(handler));
        self
    }

    pub fn on_tail_detach(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_tail_detach = Some(Arc::new(handler));
        self
    }

    pub fn selected_message(mut self, id: impl Into<String>) -> Self {
        self.selected_message = Some(id.into());
        self
    }

    pub fn on_open_written_file(mut self, handler: impl Fn(PathBuf, &mut App) + 'static) -> Self {
        self.on_open_written_file = Some(Arc::new(handler));
        self
    }

    pub fn on_fork_message(mut self, handler: impl Fn(String, &mut App) + 'static) -> Self {
        self.on_fork_message = Some(Arc::new(handler));
        self
    }
}

impl gpui::RenderOnce for ChatWindow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let document = self.document.clone();
        let model_names = self.model_names.clone();
        let item_count = document.items.len();
        let selected_message = self.selected_message.clone();
        let expanded_tools = self.expanded_tools.clone();
        let expanded_processes = self.expanded_processes.clone();
        let on_toggle_tool = self.on_toggle_tool.clone();
        let on_toggle_process = self.on_toggle_process.clone();
        let minimap_detach = self.on_tail_detach.clone();
        let minimap_toggle = self.on_toggle_minimap.clone();
        let on_message_pane_bounds = self.on_message_pane_bounds.clone();
        let list_state = self.list_state.clone();
        let items = document.items.clone();
        let on_open_written_file = self.on_open_written_file.clone();
        let on_fork_message = self.on_fork_message.clone();

        div()
            .debug_selector(|| "chat-window".into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .child(
                h_resizable("chat-window-resizable")
                    .child(
                        resizable_panel().child(
                            div()
                                .id("chat-message-list-area")
                                .debug_selector(|| "chat-message-scroll".into())
                                .when_some(on_message_pane_bounds, |pane, handler| {
                                    pane.on_prepaint(move |bounds, _, cx| handler(bounds, cx))
                                })
                                .relative()
                                .h_full()
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .child(
                                    list(list_state.clone(), move |index, _, _| {
                                        items.get(index).map_or_else(
                                            || div().into_any_element(),
                                            |item| {
                                                message_column(match item {
                                                    ConversationItem::Message(message) => {
                                                        MessageView::new(index, message.clone())
                                                            .model_names(model_names.clone())
                                                            .selected(
                                                                selected_message.as_deref()
                                                                    == Some(&message.id),
                                                            )
                                                            .expanded_tools(expanded_tools.clone())
                                                            .on_toggle_tool(on_toggle_tool.clone())
                                                            .on_open_written_file(
                                                                on_open_written_file.clone(),
                                                            )
                                                            .on_fork_message(
                                                                on_fork_message.clone(),
                                                            )
                                                            .into_any_element()
                                                    }
                                                    ConversationItem::Process(group) => {
                                                        ProcessGroupView::new(index, group.clone())
                                                            .model_names(model_names.clone())
                                                            .expanded(
                                                                !group.collapsible
                                                                    || expanded_processes
                                                                        .contains(&group.id),
                                                            )
                                                            .expanded_tools(expanded_tools.clone())
                                                            .on_toggle_tool(on_toggle_tool.clone())
                                                            .on_toggle_process(
                                                                on_toggle_process.clone(),
                                                            )
                                                            .into_any_element()
                                                    }
                                                })
                                            },
                                        )
                                    })
                                    .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
                                    .flex_grow_1()
                                    .size_full(),
                                )
                                .child(
                                    Scrollbar::vertical(&ChatScrollbarHandle(
                                        self.list_state.clone(),
                                    ))
                                    .id("chat-message-scrollbar")
                                    .mode(ScrollbarMode::Hover),
                                ),
                        ),
                    )
                    .when(self.show_minimap && !document.minimap.is_empty(), |this| {
                        this.child(
                            resizable_panel()
                                .size(px(176.))
                                .size_range(px(144.)..px(320.))
                                .flex_none()
                                .child(
                                    ChatMinimap::new(
                                        document.clone(),
                                        self.list_state.clone(),
                                        item_count,
                                    )
                                    .selected(self.selected_message)
                                    .on_navigate(move |window, cx| {
                                        if let Some(handler) = &minimap_detach {
                                            handler(window, cx);
                                        }
                                    })
                                    .on_toggle({
                                        let minimap_toggle = minimap_toggle.clone();
                                        move |cx| {
                                            if let Some(handler) = &minimap_toggle {
                                                handler(cx);
                                            }
                                        }
                                    }),
                                ),
                        )
                    }),
            )
            .when(!self.show_minimap && !document.minimap.is_empty(), |this| {
                let on_toggle = minimap_toggle.clone();
                this.child(
                    v_flex()
                        .debug_selector(|| "chat-minimap-collapsed".into())
                        .w(px(32.))
                        .h_full()
                        .flex_none()
                        .border_l_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().sidebar)
                        .p_1()
                        .child(
                            Button::new("show-chat-minimap")
                                .ghost()
                                .xsmall()
                                .icon(IconName::PanelRightOpen)
                                .tooltip("显示目录")
                                .on_click(move |_, _, cx| {
                                    if let Some(handler) = &on_toggle {
                                        handler(cx);
                                    }
                                }),
                        ),
                )
            })
    }
}

#[derive(Clone, IntoElement)]
pub struct MessageView {
    index: usize,
    message: Arc<Message>,
    model_names: Arc<HashMap<String, String>>,
    selected: bool,
    expanded_tools: Arc<HashSet<String>>,
    on_toggle_tool: Option<DetailToggleHandler>,
    on_open_written_file: Option<OpenWrittenFileHandler>,
    on_fork_message: Option<ForkMessageHandler>,
}

impl MessageView {
    pub fn new(index: usize, message: Arc<Message>) -> Self {
        Self {
            index,
            message,
            model_names: Arc::new(HashMap::new()),
            selected: false,
            expanded_tools: Arc::new(HashSet::new()),
            on_toggle_tool: None,
            on_open_written_file: None,
            on_fork_message: None,
        }
    }

    pub fn model_names(mut self, names: Arc<HashMap<String, String>>) -> Self {
        self.model_names = names;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn expanded_tools(mut self, expanded: Arc<HashSet<String>>) -> Self {
        self.expanded_tools = expanded;
        self
    }

    pub fn on_toggle_tool(mut self, handler: Option<DetailToggleHandler>) -> Self {
        self.on_toggle_tool = handler;
        self
    }

    pub fn on_open_written_file(mut self, handler: Option<OpenWrittenFileHandler>) -> Self {
        self.on_open_written_file = handler;
        self
    }

    pub fn on_fork_message(mut self, handler: Option<ForkMessageHandler>) -> Self {
        self.on_fork_message = handler;
        self
    }
}

impl gpui::RenderOnce for MessageView {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let message_id = self.message.id.clone();
        let expanded_tools = self.expanded_tools.clone();
        let selected = self.selected;
        let blocks = self
            .message
            .blocks
            .iter()
            .enumerate()
            .map(|(block_index, block)| {
                render_block(
                    self.index,
                    block_index,
                    (&message_id, self.message.role),
                    block,
                    &expanded_tools,
                    self.on_toggle_tool.clone(),
                    cx,
                )
            })
            .collect::<Vec<_>>();
        let root = v_flex()
            .id(SharedString::from(format!("message-{message_id}")))
            .debug_selector(|| "chat-message".into())
            .w_full()
            .min_w_0();
        if message_is_bubbled(self.message.role) {
            // 规范 S-14：用户消息是右对齐弱色气泡，不通栏；右对齐 + 底色已完成身份区分，
            // 不再显示 `User` 角色标签（再加就是重复编码，违反 S-8）。
            let style = user_bubble_style(cx);
            let action_group = SharedString::from(format!("message-actions-{message_id}"));
            root.group(action_group.clone())
                .items_end()
                .mb_4()
                .child(
                    v_flex()
                        .debug_selector(|| "user-bubble".into())
                        .min_w_0()
                        .max_w(relative(style.max_w_ratio))
                        .px_3()
                        .py_2()
                        .rounded(style.radius)
                        .border_1()
                        // 选中态（minimap 定位）用同源实色边框表达，不与常规弱边框混淆。
                        .border_color(if selected {
                            style.selected_border
                        } else {
                            style.border
                        })
                        .bg(style.bg)
                        .text_sm()
                        .line_height(relative(BODY_LINE_HEIGHT))
                        .gap_2()
                        .when_some(self.message.label.clone(), |bubble, label| {
                            bubble.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(label),
                            )
                        })
                        .children(blocks),
                )
                .when_some(self.on_fork_message.clone(), |message, handler| {
                    let entry_id = message_id.clone();
                    message.child(
                        h_flex()
                            .mt_1()
                            .justify_end()
                            // Fork 是低频次操作：默认隐藏，仅在整条用户气泡 hover 时显形。
                            .invisible()
                            .group_hover(action_group.clone(), |actions| actions.visible())
                            .child(
                                Button::new(SharedString::from(format!("fork-{entry_id}")))
                                    .debug_selector(|| "fork-user-message".into())
                                    .ghost()
                                    .small()
                                    .icon(IconName::Copy)
                                    .label("分叉")
                                    .tooltip("从此用户消息分叉")
                                    .on_click(move |_, _, cx| handler(entry_id.clone(), cx)),
                            ),
                    )
                })
        } else {
            let model_label = (self.message.role == MessageRole::Assistant)
                .then(|| {
                    self.message.model.as_ref().map(|model| {
                        self.model_names
                            .get(&model_ref_key(&model.provider, &model.id))
                            .cloned()
                            .unwrap_or_else(|| model.id.clone())
                    })
                })
                .flatten();
            let role_color = match self.message.role {
                // 用户消息走上面的气泡分支，此处不可达；给个中性值保持 match 完整。
                MessageRole::User => cx.theme().foreground,
                MessageRole::Assistant => cx.theme().foreground,
                MessageRole::Compaction | MessageRole::BranchSummary => cx.theme().warning,
                // 「渲染不了」不等于「出错了」，Unknown 不与真错误抢 danger 通道（§ 5.1）。
                MessageRole::Unknown => cx.theme().warning,
                MessageRole::Custom => cx.theme().muted_foreground,
            };
            let header_primary = model_label.map_or_else(
                || {
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(role_color)
                        .child(role_label(self.message.role))
                        .into_any_element()
                },
                |model| {
                    div()
                        .debug_selector(|| "assistant-model".into())
                        .text_xs()
                        .text_color(crate::theme::dim_foreground(cx))
                        .child(model)
                        .into_any_element()
                },
            );
            root.gap_2()
                .px_5()
                .py_1p5()
                // 正文字号与行高必须显式声明（S-12 / S-18）：rem 默认 16px，
                // 不显式 `text_sm` 就会整体偏大；行高不给则吃组件库的 1.43，偏挤。
                .text_sm()
                .line_height(relative(BODY_LINE_HEIGHT))
                // 选中态走弱底而不是描边（规范 S-7，0.16 是定值），免得给助手消息加回卡片。
                .when(selected, |view| {
                    view.rounded_md().bg(cx.theme().accent.opacity(0.16))
                })
                .child(h_flex().gap_2().child(header_primary).when_some(
                    self.message.label.clone(),
                    |row, label| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(label),
                        )
                    },
                ))
                .children(blocks)
                .when(!self.message.written_files.is_empty(), |view| {
                    let mut written =
                        crate::TurnWrittenFiles::new(self.message.written_files.clone());
                    if let Some(handler) = self.on_open_written_file.clone() {
                        written = written.on_open(move |path, cx| handler(path, cx));
                    }
                    view.child(written)
                })
        }
    }
}

#[derive(Clone, IntoElement)]
pub struct ProcessGroupView {
    index: usize,
    group: ProcessGroup,
    model_names: Arc<HashMap<String, String>>,
    expanded: bool,
    expanded_tools: Arc<HashSet<String>>,
    on_toggle_tool: Option<DetailToggleHandler>,
    on_toggle_process: Option<ProcessToggleHandler>,
}

impl ProcessGroupView {
    pub fn new(index: usize, group: ProcessGroup) -> Self {
        Self {
            index,
            group,
            model_names: Arc::new(HashMap::new()),
            expanded: false,
            expanded_tools: Arc::new(HashSet::new()),
            on_toggle_tool: None,
            on_toggle_process: None,
        }
    }

    pub fn model_names(mut self, names: Arc<HashMap<String, String>>) -> Self {
        self.model_names = names;
        self
    }

    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self
    }

    pub fn expanded_tools(mut self, expanded: Arc<HashSet<String>>) -> Self {
        self.expanded_tools = expanded;
        self
    }

    pub fn on_toggle_tool(mut self, handler: Option<DetailToggleHandler>) -> Self {
        self.on_toggle_tool = handler;
        self
    }

    pub fn on_toggle_process(mut self, handler: Option<ProcessToggleHandler>) -> Self {
        self.on_toggle_process = handler;
        self
    }
}

impl gpui::RenderOnce for ProcessGroupView {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let group_id = self.group.id.clone();
        let toggle_id = group_id.clone();
        let summary = if self.group.tool_call_count == 0 {
            format!("处理详情 · {} 条消息", self.group.message_count)
        } else {
            format!(
                "处理详情 · {} 条消息 · {} 次工具调用",
                self.group.message_count, self.group.tool_call_count
            )
        };
        let toggle = self.on_toggle_process.clone();
        v_flex()
            .id(SharedString::from(format!("process-group-{group_id}")))
            .debug_selector(|| "process-group".into())
            .w_full()
            .min_w_0()
            .gap_1p5()
            .py_1()
            .child(
                h_flex()
                    .id(SharedString::from(format!("process-toggle-{toggle_id}")))
                    .debug_selector(|| "process-group-toggle".into())
                    // 与助手消息同左缘（规范 3.2 助手消息 px_5），折叠头不自成一栏。
                    .px_5()
                    .py_1()
                    .gap_1p5()
                    .text_color(cx.theme().muted_foreground)
                    .when(self.group.collapsible, |row| {
                        row.cursor_pointer()
                            .hover(|row| row.text_color(cx.theme().foreground))
                            .on_click(move |_, _, cx| {
                                if let Some(handler) = &toggle {
                                    handler(group_id.clone(), cx);
                                }
                            })
                    })
                    .child(Icon::new(disclosure_icon(self.expanded)).size_4())
                    .child(div().text_xs().min_w_0().truncate().child(summary)),
            )
            .when(self.expanded, |view| {
                let model_names = self.model_names.clone();
                let expanded_tools = self.expanded_tools.clone();
                let on_toggle_tool = self.on_toggle_tool.clone();
                view.child(
                    v_flex()
                        .debug_selector(|| "process-group-details".into())
                        // 规范 5.1 缩进消息：左竖线表达从属，内层 MessageView 自带的 px_5 就是竖线后的留白。
                        .min_w_0()
                        .ml_5()
                        .border_l_1()
                        .border_color(card_border(cx))
                        .gap_2()
                        .children(self.group.messages.iter().enumerate().map(
                            move |(message_index, message)| {
                                MessageView::new(
                                    self.index.saturating_mul(10_000) + message_index,
                                    message.clone(),
                                )
                                .model_names(model_names.clone())
                                .expanded_tools(expanded_tools.clone())
                                .on_toggle_tool(on_toggle_tool.clone())
                            },
                        )),
                )
            })
    }
}

#[derive(Clone, IntoElement)]
pub struct MarkdownBody {
    id: SharedString,
    source: String,
}

impl MarkdownBody {
    pub fn new(id: impl Into<SharedString>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
        }
    }
}

impl gpui::RenderOnce for MarkdownBody {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        safe_markdown(self.id, self.source).w_full()
    }
}

fn safe_markdown(id: impl Into<gpui::ElementId>, source: impl Into<SharedString>) -> TextView {
    TextView::markdown(id, source)
        .selectable(true)
        .scrollable(false)
        .on_link_click(|url, _, window, cx| {
            let url: SharedString = url.clone();
            let open_url = url.clone();
            window.open_dialog(cx, move |dialog, _, _| {
                let open_url = open_url.clone();
                dialog
                    .title("打开外部链接？")
                    .child(
                        v_flex()
                            .gap_2()
                            .child("历史会话中的链接可能来自不受信任内容。")
                            .child(div().text_xs().child(url.clone())),
                    )
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("打开")
                            .cancel_text("取消")
                            .show_cancel(true)
                            .on_ok(move |_, _, cx| {
                                cx.open_url(&open_url);
                                true
                            }),
                    )
            });
        })
}

#[derive(Clone, IntoElement)]
pub struct ChatMinimap {
    document: Arc<ConversationDocument>,
    list_state: ListState,
    message_count: usize,
    selected_message: Option<String>,
    on_navigate: Option<TailDetachHandler>,
    on_toggle: Option<MinimapToggleHandler>,
}

impl ChatMinimap {
    pub fn new(
        document: Arc<ConversationDocument>,
        list_state: ListState,
        message_count: usize,
    ) -> Self {
        Self {
            document,
            list_state,
            message_count,
            selected_message: None,
            on_navigate: None,
            on_toggle: None,
        }
    }

    pub fn selected(mut self, selected: Option<String>) -> Self {
        self.selected_message = selected;
        self
    }

    pub fn on_navigate(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_navigate = Some(Arc::new(handler));
        self
    }

    pub fn on_toggle(mut self, handler: impl Fn(&mut App) + 'static) -> Self {
        self.on_toggle = Some(Arc::new(handler));
        self
    }
}

impl gpui::RenderOnce for ChatMinimap {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let message_count = self.message_count;
        let message_indexes = self
            .document
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id(), index))
            .collect::<HashMap<_, _>>();
        let first_minimap_ordinals = self.document.minimap.iter().enumerate().fold(
            HashMap::<String, usize>::new(),
            |mut ordinals, (ordinal, node)| {
                ordinals.entry(node.message_id.clone()).or_insert(ordinal);
                ordinals
            },
        );
        let on_toggle = self.on_toggle.clone();
        v_flex()
            .debug_selector(|| "chat-minimap".into())
            .w_full()
            .h_full()
            .min_h_0()
            .flex_none()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                h_flex().justify_end().p_1().child(
                    Button::new("hide-chat-minimap")
                        .ghost()
                        .xsmall()
                        .icon(IconName::PanelRight)
                        .tooltip("隐藏目录")
                        .on_click(move |_, _, cx| {
                            if let Some(handler) = &on_toggle {
                                handler(cx);
                            }
                        }),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .gap_1()
                    .overflow_y_scrollbar()
                    .children(
                        self.document
                            .minimap
                            .iter()
                            .enumerate()
                            .map(|(ordinal, node)| {
                                let id = node.message_id.clone();
                                let selected = self.selected_message.as_deref()
                                    == Some(id.as_str())
                                    && first_minimap_ordinals.get(id.as_str()) == Some(&ordinal);
                                let message_index = message_indexes
                                    .get(id.as_str())
                                    .copied()
                                    .unwrap_or(0)
                                    .min(message_count.saturating_sub(1));
                                let list_state = self.list_state.clone();
                                let on_navigate = self.on_navigate.clone();
                                div()
                                    .id(minimap_node_id(&id, ordinal))
                                    .debug_selector(|| "chat-minimap-node".into())
                                    .relative()
                                    .ml(px(
                                        f32::from(node.level.unwrap_or(1).saturating_sub(1)) * 7.
                                    ))
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .text_xs()
                                    .truncate()
                                    .cursor_pointer()
                                    .when(selected, |item| {
                                        // 竖条绝对定位：只有选中项才进树，未选中行的排版一像素都不动。
                                        item.bg(cx.theme().accent.opacity(0.16)).child(
                                            div()
                                                .debug_selector(|| "chat-minimap-node-bar".into())
                                                .absolute()
                                                .left_0()
                                                .top_1()
                                                .bottom_1()
                                                .w(px(2.))
                                                .rounded_full()
                                                .bg(cx.theme().accent),
                                        )
                                    })
                                    .hover(|item| item.bg(cx.theme().muted))
                                    .on_click(move |_, window, cx| {
                                        list_state.scroll_to(ListOffset {
                                            item_ix: message_index,
                                            offset_in_item: px(0.),
                                        });
                                        if let Some(handler) = &on_navigate {
                                            handler(window, cx);
                                        }
                                        window.refresh();
                                    })
                                    .child(node.label.clone())
                            }),
                    ),
            )
    }
}

fn minimap_node_id(message_id: &str, ordinal: usize) -> SharedString {
    format!("minimap-{message_id}-{ordinal}").into()
}

fn render_block(
    index: usize,
    block_index: usize,
    message: (&str, MessageRole),
    block: &Block,
    expanded_tools: &HashSet<String>,
    on_toggle_tool: Option<DetailToggleHandler>,
    cx: &App,
) -> gpui::AnyElement {
    let (message_id, role) = message;
    match block {
        Block::Markdown(markdown) => MarkdownBody::new(
            format!("markdown-{index}-{block_index}"),
            markdown.source.clone(),
        )
        .into_any_element(),
        Block::Code(code) => render_code(index, block_index, code.clone(), cx),
        Block::Thinking(text) => {
            let key = detail_key(message_id, block_index, "thinking");
            render_thinking(
                index,
                block_index,
                text.clone(),
                key.clone(),
                expanded_tools.contains(&key),
                on_toggle_tool,
                cx,
            )
        }
        Block::Tool(tool) => {
            let key = tool_key(message_id, block_index, &tool.id);
            render_tool(
                tool.clone(),
                key.clone(),
                expanded_tools.contains(&key),
                on_toggle_tool,
                cx,
            )
        }
        Block::Subagent(card) => {
            let key = detail_key(message_id, block_index, "subagent");
            render_subagent(
                (**card).clone(),
                key.clone(),
                expanded_tools.contains(&key),
                on_toggle_tool,
                cx,
            )
        }
        Block::Diff(diff) => subordinate_column(cx)
            .child(crate::render_diff_block(Arc::new(diff.clone()), cx))
            .into_any_element(),
        Block::Ansi(ansi) => render_ansi(ansi.clone(), cx),
        Block::Image(image) => render_image(image.clone(), role == MessageRole::User, cx),
        Block::Frontmatter(frontmatter) => render_frontmatter(frontmatter.clone(), cx),
        Block::Notice(notice) => v_flex()
            .debug_selector(|| "notice-card".into())
            .min_w_0()
            .gap_1p5()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(card_border(cx))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(status_dot(cx.theme().info))
                    .child(div().text_sm().font_semibold().child(notice.title.clone())),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(notice.text.clone()),
            )
            .into_any_element(),
        Block::Unknown(unknown) => v_flex()
            .debug_selector(|| "unknown-card".into())
            .min_w_0()
            .gap_1p5()
            .p_2()
            .rounded_md()
            .border_1()
            // 状态走 header 圆点，不再拿 warning 描整卡（红线 3）。
            .border_color(card_border(cx))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(status_dot(cx.theme().warning))
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("未知内容 · {}", unknown.kind)),
                    ),
            )
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .child(unknown.text.clone()),
            )
            .into_any_element(),
    }
}

fn model_ref_key(provider: &str, id: &str) -> String {
    format!("{provider}\0{id}")
}

fn message_item_id(detail_key: &str) -> String {
    detail_key.split(':').next().unwrap_or_default().to_owned()
}

fn tool_key(message_id: &str, block_index: usize, tool_id: &str) -> String {
    if tool_id.is_empty() {
        detail_key(message_id, block_index, "tool")
    } else {
        format!("{message_id}:tool:{tool_id}")
    }
}

/// 子代理结果卡片（规范 5.2 的卡片骨架 + 5.3 的左竖线展开区）。
///
/// 与工具卡片同构而不是另起一套视觉：子代理结果在消息流里就是「一次委派的产出」，
/// 读者的扫描路径应该和读工具卡一致 —— 状态点、名字、右侧状态文字、一行摘要，
/// 明细收进展开区。
fn render_subagent(
    card: SubagentCard,
    key: String,
    expanded: bool,
    on_toggle_tool: Option<DetailToggleHandler>,
    cx: &App,
) -> gpui::AnyElement {
    let color = subagent_status_color(&card.status, cx);
    let status_label = card.status.label().to_owned();
    let toggle_key = key.clone();
    let item_id = message_item_id(&toggle_key);
    // 摘要行合计 ≤3 个文本片段（规范 S-8）：描述、短 id、统计。
    let summary = card
        .description
        .clone()
        .unwrap_or_else(|| card.result.lines().next().unwrap_or_default().to_owned());
    let stats_summary = subagent_stats_summary(&card.stats);

    v_flex()
        .id(SharedString::from(format!("subagent-card-{key}")))
        .debug_selector(|| "subagent-card".into())
        .min_w_0()
        .gap_1p5()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(card_border(cx))
        .child(
            h_flex()
                .id(SharedString::from(format!("subagent-toggle-{key}")))
                .debug_selector(|| "subagent-card-toggle".into())
                .gap_1p5()
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    if let Some(handler) = &on_toggle_tool {
                        handler(toggle_key.clone(), item_id.clone(), cx);
                    }
                })
                .child(status_dot(color).debug_selector(|| "subagent-card-status-dot".into()))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_semibold()
                        .child(format!("子代理 · {}", card.agent_type)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(status_label),
                )
                .child(
                    Icon::new(disclosure_icon(expanded))
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                ),
        )
        .child(
            h_flex()
                .min_w_0()
                .gap_1p5()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(summary),
                )
                .when_some(card.short_id.clone(), |row, short_id| {
                    // 短 id 是 dim 级元信息（规范 S-16 第 3 档），用于和面板里的任务对上号。
                    row.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(crate::theme::dim_foreground(cx))
                            .child(short_id),
                    )
                })
                .when_some(stats_summary, |row, stats| {
                    row.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(crate::theme::dim_foreground(cx))
                            .child(stats),
                    )
                }),
        )
        .when(expanded, |view| {
            view.child(
                subordinate_column(cx)
                    .debug_selector(|| "subagent-card-details".into())
                    .gap_2()
                    .when_some(card.stop_reason.clone(), |column, reason| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("结束原因：{reason}")),
                        )
                    })
                    .when_some(card.worktree_path.clone(), |column, path| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(crate::theme::dim_foreground(cx))
                                .child(format!("worktree：{path}")),
                        )
                    })
                    .when_some(card.output_file.clone(), |column, path| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(crate::theme::dim_foreground(cx))
                                .child(format!("transcript：{path}")),
                        )
                    })
                    .when(!card.result.trim().is_empty(), |column| {
                        column.child(
                            MarkdownBody::new(
                                SharedString::from(format!("subagent-result-{key}")),
                                card.result.clone(),
                            )
                            .into_any_element(),
                        )
                    }),
            )
        })
        .into_any_element()
}

fn render_thinking(
    index: usize,
    block_index: usize,
    text: String,
    key: String,
    expanded: bool,
    on_toggle: Option<DetailToggleHandler>,
    cx: &App,
) -> gpui::AnyElement {
    let toggle_key = key.clone();
    let item_id = message_item_id(&toggle_key);
    // 规范 5.4：思考是消息流内的次级内容，不套卡片边框，靠左竖线区分层级。
    v_flex()
        .id(SharedString::from(format!("thinking-card-{key}")))
        .debug_selector(|| "thinking-card".into())
        .min_w_0()
        .gap_1p5()
        .child(
            h_flex()
                .id(SharedString::from(format!("thinking-toggle-{key}")))
                .debug_selector(|| "thinking-card-toggle".into())
                .gap_1p5()
                .cursor_pointer()
                .text_color(cx.theme().muted_foreground)
                .hover(|row| row.text_color(cx.theme().foreground))
                .on_click(move |_, _, cx| {
                    if let Some(handler) = &on_toggle {
                        handler(toggle_key.clone(), item_id.clone(), cx);
                    }
                })
                .child(Icon::new(IconName::Cpu).size_4())
                .child(div().text_sm().font_semibold().child("思考"))
                .child(div().flex_1())
                .child(Icon::new(disclosure_icon(expanded)).size_4()),
        )
        .when(expanded, |view| {
            view.child(
                subordinate_column(cx)
                    .debug_selector(|| "thinking-card-details".into())
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(safe_markdown(
                        format!("thinking-{index}-{block_index}"),
                        text,
                    )),
            )
        })
        .into_any_element()
}

fn render_code(index: usize, block_index: usize, code: CodeBlock, cx: &App) -> gpui::AnyElement {
    let language = code.language.as_deref().unwrap_or("text");
    let display_language = if code.mermaid_source {
        "mermaid · 源码"
    } else {
        language
    };
    v_flex()
        .debug_selector(move || {
            if code.mermaid_source {
                "mermaid-source".into()
            } else {
                "code-block".into()
            }
        })
        .min_w_0()
        .rounded_md()
        .border_1()
        .border_color(card_border(cx))
        .bg(cx.theme().muted.opacity(0.42))
        .child(
            h_flex()
                .justify_between()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(card_border(cx))
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(cx.theme().muted_foreground)
                        .child(display_language.to_owned()),
                )
                .when(code.truncated, |row| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child("已截断"),
                    )
                }),
        )
        .child(
            safe_markdown(
                format!("code-{index}-{block_index}"),
                format!("```{language}\n{}\n```", code.source),
            )
            .p_2(),
        )
        .into_any_element()
}

fn render_tool(
    tool: ToolCard,
    key: String,
    expanded: bool,
    on_toggle_tool: Option<DetailToggleHandler>,
    cx: &App,
) -> gpui::AnyElement {
    let (status, color) = tool_status_style(tool.status, cx);
    let toggle_key = key.clone();
    let item_id = message_item_id(&toggle_key);
    v_flex()
        .id(SharedString::from(format!("tool-card-{key}")))
        .debug_selector(|| "tool-card".into())
        .min_w_0()
        .gap_1p5()
        .p_2()
        .rounded_md()
        .border_1()
        // 状态不上边框（规范 5.2 / 红线 3），只走 header 的状态点。
        .border_color(card_border(cx))
        .child(
            h_flex()
                .id(SharedString::from(format!("tool-toggle-{key}")))
                .debug_selector(|| "tool-card-toggle".into())
                .gap_1p5()
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    if let Some(handler) = &on_toggle_tool {
                        handler(toggle_key.clone(), item_id.clone(), cx);
                    }
                })
                .child(status_dot(color).debug_selector(|| "tool-card-status-dot".into()))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_semibold()
                        .child(tool.name.clone()),
                )
                .child(div().flex_1())
                .when(tool.orphan, |row| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child("orphan"),
                    )
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(status),
                )
                .child(
                    Icon::new(disclosure_icon(expanded))
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                ),
        )
        .child(
            div()
                .text_xs()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(tool.preview),
        )
        .when(expanded, |view| {
            view.child(
                // 展开区走左竖线 + 缩进，卡片里不再嵌卡片（规范 5.3 / 红线 2）。
                subordinate_column(cx)
                    .debug_selector(|| "tool-card-details".into())
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child("Input JSON"),
                    )
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(tool.input_json),
                    )
                    .children(tool.output.into_iter().map(|output| match output {
                        ToolOutput::Text(text) => div().text_xs().child(text).into_any_element(),
                        ToolOutput::Ansi(ansi) => render_ansi(ansi, cx),
                        ToolOutput::Image(image) => render_image(image, false, cx),
                        // Tool card 展开区自身已经是 subordinate_column，不重复套壳。
                        ToolOutput::Diff(diff) => crate::render_diff_block(Arc::new(diff), cx),
                    })),
            )
        })
        .into_any_element()
}

fn render_ansi(ansi: AnsiText, cx: &App) -> gpui::AnyElement {
    let highlights = ansi
        .spans
        .iter()
        .map(|span| (span.range.clone(), ansi_highlight(&span.style, cx)))
        .collect::<Vec<_>>();
    div()
        .debug_selector(|| "ansi-output".into())
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs()
        .child(StyledText::new(ansi.text).with_highlights(highlights))
        .into_any_element()
}

fn ansi_highlight(style: &AnsiStyle, cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: style.foreground.map(|color| ansi_color(color, cx)),
        background_color: style.background.map(|color| ansi_color(color, cx)),
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: style.underline.then_some(gpui::UnderlineStyle {
            color: None,
            thickness: px(1.),
            wavy: false,
        }),
        fade_out: style.dim.then_some(0.45),
        ..HighlightStyle::default()
    }
}

fn ansi_color(color: AnsiColor, cx: &App) -> gpui::Hsla {
    match color {
        AnsiColor::Indexed(index) => match index % 16 {
            0 => cx.theme().muted_foreground,
            1 | 9 => cx.theme().danger,
            2 | 10 => cx.theme().success,
            3 | 11 => cx.theme().warning,
            4 | 12 => cx.theme().accent,
            5 | 13 => cx.theme().secondary_foreground,
            6 | 14 => cx.theme().info,
            _ => cx.theme().foreground,
        },
        AnsiColor::Rgb(red, green, blue) => {
            let max = red.max(green).max(blue);
            let min = red.min(green).min(blue);
            if max.saturating_sub(min) < 24 {
                if max < 96 {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().foreground
                }
            } else if red == max && green > blue {
                cx.theme().warning
            } else if red == max {
                cx.theme().danger
            } else if green == max {
                cx.theme().success
            } else {
                cx.theme().accent
            }
        }
    }
}

fn render_image(image: ImageBlock, user_message: bool, cx: &App) -> gpui::AnyElement {
    if image.state == ImageState::Inline
        && let (Some(bytes), Some(mime)) = (image.bytes, image.mime_type.as_deref())
        && let Some(format) = ImageFormat::from_mime_type(mime)
    {
        let preview = img(Arc::new(Image::from_bytes(format, bytes)));
        let preview = if user_message {
            preview.max_h_56()
        } else {
            preview.max_h_80()
        };
        return div()
            .debug_selector(move || {
                if user_message {
                    "user-inline-image".into()
                } else {
                    "inline-image".into()
                }
            })
            .when(user_message, |view| view.max_w_56().max_h_56())
            .when(!user_message, |view| view.w_full().max_h_80())
            .rounded_md()
            .border_1()
            .border_color(card_border(cx))
            .overflow_hidden()
            .child(preview)
            .into_any_element();
    }
    v_flex()
        .debug_selector(|| "image-placeholder".into())
        .min_w_0()
        .gap_1()
        .p_2()
        .rounded_md()
        // 占位块用背景表达层级而不是描边：它常出现在工具输出的竖线区里（规范 S-1）。
        .bg(cx.theme().muted.opacity(0.42))
        .child(
            div()
                .text_sm()
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .child("图片占位"),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(image.description),
        )
        .when_some(image.remote_url, |view, url| {
            view.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(url),
            )
        })
        .into_any_element()
}

fn render_frontmatter(frontmatter: FrontmatterCard, cx: &App) -> gpui::AnyElement {
    v_flex()
        .debug_selector(|| "frontmatter-card".into())
        .min_w_0()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(card_border(cx))
        .when_some(frontmatter.title, |view, title| {
            view.child(div().text_sm().font_semibold().child(title))
        })
        .when(!frontmatter.tags.is_empty(), |view| {
            view.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().accent)
                    .child(frontmatter.tags.join(" · ")),
            )
        })
        .children(frontmatter.rows.into_iter().map(|(key, value)| {
            h_flex()
                .gap_2()
                .child(
                    div()
                        .w(px(96.))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(key),
                )
                .child(div().flex_1().min_w_0().text_xs().child(value))
        }))
        .into_any_element()
}

fn role_label(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "User",
        MessageRole::Assistant => "Assistant",
        MessageRole::Custom => "Custom",
        MessageRole::Compaction => "Compaction",
        MessageRole::BranchSummary => "Branch summary",
        MessageRole::Unknown => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::PathBuf, rc::Rc};

    use gpui::{
        AppContext as _, Context, ListAlignment, Modifiers, Render, TestAppContext,
        VisualTestContext, size, transparent_black,
    };
    use gpui_component::{Root, ThemeMode};
    use pi_render::{MarkdownBlock, MinimapNode, ModelRef};

    use super::*;

    /// `ChatWindow` 是 `RenderOnce`，测试需要一个持有状态的宿主视图来反复绘制。
    struct ChatHarness {
        document: Arc<ConversationDocument>,
        model_names: Arc<HashMap<String, String>>,
        list_state: ListState,
        expanded_tools: Arc<HashSet<String>>,
        selected: Option<String>,
        forked: Option<Rc<RefCell<Vec<String>>>>,
        show_minimap: bool,
    }

    impl Render for ChatHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let chat = ChatWindow::new(self.document.clone(), self.list_state.clone())
                .model_names(self.model_names.clone())
                .show_minimap(self.show_minimap)
                .expanded_tools(self.expanded_tools.clone());
            let chat = if let Some(forked) = self.forked.clone() {
                chat.on_fork_message(move |entry_id, _| forked.borrow_mut().push(entry_id))
            } else {
                chat
            };
            match &self.selected {
                Some(id) => chat.selected_message(id.clone()),
                None => chat,
            }
        }
    }

    fn message(id: &str, role: MessageRole, blocks: Vec<Block>) -> Arc<Message> {
        Arc::new(Message {
            id: id.to_owned(),
            role,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks,
        })
    }

    fn message_with_model(
        id: &str,
        role: MessageRole,
        model: Option<ModelRef>,
        blocks: Vec<Block>,
    ) -> Arc<Message> {
        Arc::new(Message {
            id: id.to_owned(),
            role,
            timestamp: None,
            label: None,
            model,
            written_files: Vec::new(),
            blocks,
        })
    }

    fn inline_image_block() -> Block {
        Block::Image(ImageBlock {
            mime_type: Some("image/png".to_owned()),
            state: ImageState::Inline,
            bytes: Some(b"\x89PNG\r\n\x1a\nfixture".to_vec()),
            remote_url: None,
            description: "fixture".to_owned(),
        })
    }

    fn tool_block(status: ToolStatus) -> Block {
        Block::Tool(ToolCard {
            id: "call".to_owned(),
            name: "bash".to_owned(),
            arguments: Default::default(),
            input_json: "{}".to_owned(),
            preview: "cargo test".to_owned(),
            status,
            output: vec![ToolOutput::Text("done".to_owned())],
            details: None,
            orphan: false,
        })
    }

    fn fixture_document(status: ToolStatus) -> Arc<ConversationDocument> {
        let user = message(
            "u",
            MessageRole::User,
            vec![Block::Markdown(MarkdownBlock {
                source: "hello".to_owned(),
            })],
        );
        let assistant = message("a", MessageRole::Assistant, vec![tool_block(status)]);
        Arc::new(ConversationDocument {
            session_id: "fixture".to_owned(),
            source_path: std::path::PathBuf::from("fixture.jsonl"),
            cwd: PathBuf::new(),
            messages: Arc::from(vec![user.clone(), assistant.clone()]),
            items: Arc::from(vec![
                ConversationItem::Message(user),
                ConversationItem::Message(assistant),
            ]),
            minimap: Arc::from(vec![
                MinimapNode {
                    message_id: "u".to_owned(),
                    turn: 0,
                    role: MessageRole::User,
                    label: "hello".to_owned(),
                    level: Some(1),
                },
                MinimapNode {
                    message_id: "a".to_owned(),
                    turn: 0,
                    role: MessageRole::Assistant,
                    label: "bash".to_owned(),
                    level: Some(2),
                },
            ]),
            diagnostics: Arc::from(Vec::new()),
        })
    }

    fn render_chat(
        cx: &mut TestAppContext,
        document: Arc<ConversationDocument>,
        expanded_tools: Vec<String>,
        selected: Option<String>,
    ) -> VisualTestContext {
        render_chat_sized(
            cx,
            document,
            expanded_tools,
            selected,
            size(px(640.), px(480.)),
        )
    }

    fn render_chat_with_names(
        cx: &mut TestAppContext,
        document: Arc<ConversationDocument>,
        model_names: Arc<HashMap<String, String>>,
    ) -> VisualTestContext {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init_fonts(cx).expect("font init failed");
        });
        let item_count = document.items.len();
        let handle = cx.open_window(size(px(640.), px(480.)), move |window, cx| {
            let harness = cx.new(|_| ChatHarness {
                document,
                model_names,
                list_state: ListState::new(item_count, ListAlignment::Top, px(1200.)).measure_all(),
                expanded_tools: Arc::new(HashSet::new()),
                selected: None,
                forked: None,
                show_minimap: true,
            });
            Root::new(harness, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        visual
    }

    fn render_chat_sized(
        cx: &mut TestAppContext,
        document: Arc<ConversationDocument>,
        expanded_tools: Vec<String>,
        selected: Option<String>,
        window_size: Size<Pixels>,
    ) -> VisualTestContext {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init_fonts(cx).expect("font init failed");
        });
        let item_count = document.items.len();
        let handle = cx.open_window(window_size, move |window, cx| {
            let harness = cx.new(|_| ChatHarness {
                document,
                model_names: Arc::new(HashMap::new()),
                list_state: ListState::new(item_count, ListAlignment::Top, px(1200.)).measure_all(),
                expanded_tools: Arc::new(expanded_tools.into_iter().collect()),
                selected,
                forked: None,
                show_minimap: true,
            });
            Root::new(harness, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        visual
    }

    fn render_chat_with_fork_handler(
        cx: &mut TestAppContext,
        document: Arc<ConversationDocument>,
    ) -> (VisualTestContext, Rc<RefCell<Vec<String>>>) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init_fonts(cx).expect("font init failed");
        });
        let item_count = document.items.len();
        let forked = Rc::new(RefCell::new(Vec::new()));
        let captured = forked.clone();
        let handle = cx.open_window(size(px(640.), px(480.)), move |window, cx| {
            let harness = cx.new(|_| ChatHarness {
                document,
                model_names: Arc::new(HashMap::new()),
                list_state: ListState::new(item_count, ListAlignment::Top, px(1200.)).measure_all(),
                expanded_tools: Arc::new(HashSet::new()),
                selected: None,
                forked: Some(captured),
                show_minimap: true,
            });
            Root::new(harness, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        (visual, forked)
    }

    /// 消息列宽度断言需要排除 176px 目录面板的占位，用空 minimap 的文档。
    fn fixture_document_without_minimap(status: ToolStatus) -> Arc<ConversationDocument> {
        let base = fixture_document(status);
        Arc::new(ConversationDocument {
            session_id: base.session_id.clone(),
            source_path: base.source_path.clone(),
            cwd: PathBuf::new(),
            messages: base.messages.clone(),
            items: base.items.clone(),
            minimap: Arc::from(Vec::new()),
            diagnostics: Arc::from(Vec::new()),
        })
    }

    #[test]
    fn detail_keys_are_stable_and_separate_thinking_from_tools() {
        assert_eq!(tool_key("m", 2, "call"), "m:tool:call");
        assert_eq!(tool_key("m", 2, ""), "m:tool:2");
        assert_eq!(detail_key("m", 2, "thinking"), "m:thinking:2");
    }

    /// T2 ①：消息根节点不再是「每条一张描边卡」，只有用户消息气泡化（S-14 / 红线 10）。
    #[test]
    fn only_user_messages_are_bubbled() {
        assert!(message_is_bubbled(MessageRole::User));
        for role in [
            MessageRole::Assistant,
            MessageRole::Compaction,
            MessageRole::BranchSummary,
            MessageRole::Custom,
            MessageRole::Unknown,
        ] {
            assert!(
                !message_is_bubbled(role),
                "{role:?} 必须是纯文本流，不能上气泡"
            );
        }
    }

    /// § 5.8 S-13：宽窗口（> 852 = 820 + 32）列宽钉在 820 且居中；
    /// 窄窗口列宽等于窗宽减去列外 16px × 2 留白。
    #[gpui::test]
    fn message_column_is_centered_and_capped(cx: &mut TestAppContext) {
        let mut wide = render_chat_sized(
            cx,
            fixture_document_without_minimap(ToolStatus::Success),
            Vec::new(),
            None,
            size(px(1000.), px(480.)),
        );
        let column = wide.debug_bounds("message-column").expect("消息列必须存在");
        assert_eq!(column.size.width, px(MESSAGE_COLUMN_MAX_WIDTH));
        assert_eq!(
            column.origin.x,
            (px(1000.) - px(MESSAGE_COLUMN_MAX_WIDTH)) / 2.,
            "超宽窗口下消息列必须水平居中"
        );

        let mut narrow = render_chat_sized(
            cx,
            fixture_document_without_minimap(ToolStatus::Success),
            Vec::new(),
            None,
            size(px(640.), px(480.)),
        );
        let column = narrow
            .debug_bounds("message-column")
            .expect("消息列必须存在");
        assert_eq!(column.size.width, px(640.) - px(32.));
        assert_eq!(column.origin.x, px(16.));
    }

    /// § 5.8 S-14：用户气泡右缘贴列右缘，宽度不超过列宽 × 85%。
    #[gpui::test]
    fn user_bubble_hugs_column_right_edge(cx: &mut TestAppContext) {
        let mut visual = render_chat(
            cx,
            fixture_document_without_minimap(ToolStatus::Success),
            Vec::new(),
            None,
        );
        let bubble = visual
            .debug_bounds("user-bubble")
            .expect("用户消息必须渲染为气泡");
        let column = visual
            .debug_bounds("message-column")
            .expect("消息列必须存在");
        let bubble_right = bubble.origin.x + bubble.size.width;
        let column_right = column.origin.x + column.size.width;
        assert!(
            (bubble_right - column_right).abs() <= px(1.),
            "气泡右缘必须贴列右缘：bubble_right={bubble_right:?} column_right={column_right:?}"
        );
        assert!(
            bubble.size.width <= column.size.width * 0.85 + px(1.),
            "气泡宽度不得超过列宽的 85%：bubble={:?} column={:?}",
            bubble.size.width,
            column.size.width
        );
    }

    #[gpui::test]
    fn user_inline_image_preview_is_bounded_to_registered_240_scale(cx: &mut TestAppContext) {
        let user = message("image-user", MessageRole::User, vec![inline_image_block()]);
        let document = Arc::new(ConversationDocument {
            session_id: "image".to_owned(),
            source_path: PathBuf::from("image.jsonl"),
            cwd: PathBuf::new(),
            messages: Arc::from([user.clone()]),
            items: Arc::from([ConversationItem::Message(user)]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        });
        let mut visual =
            render_chat_sized(cx, document, Vec::new(), None, size(px(640.), px(480.)));
        let image = visual
            .debug_bounds("user-inline-image")
            .expect("用户图片预览必须存在");
        assert!(image.size.width <= px(224.));
        assert!(image.size.height <= px(224.));
    }

    /// § 5.8 S-14：气泡样式值全部出自 `user_bubble_style`，渲染与测试同源；
    /// 基色是 base.blue（v2.2 勘误），深浅两种模式下都必须是饱和的身份色，
    /// 防止再次回归成看不见的中性灰（T3 复验意见）。
    #[gpui::test]
    fn user_bubble_style_matches_spec(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                gpui_component::Theme::change(mode, None, cx);
                let style = user_bubble_style(cx);
                assert_eq!(style.bg, cx.theme().blue.opacity(0.10));
                assert_eq!(style.border, cx.theme().blue.opacity(0.2));
                assert_eq!(style.selected_border, cx.theme().blue);
                assert_eq!(style.radius, px(12.), "气泡圆角是 rounded_xl 档（12px）");
                assert!((style.max_w_ratio - 0.85).abs() < f32::EPSILON);
                assert!(
                    style.border.s >= 0.5,
                    "{mode:?} 下气泡边框必须是饱和色（s={}），不许退化成中性灰",
                    style.border.s
                );
                assert_ne!(
                    style.bg,
                    cx.theme().accent.opacity(0.10),
                    "{mode:?} 下气泡底色不得回退到中性 accent"
                );
            }
        });
    }

    /// § 5.8 S-9：用户消息 Fork 默认无命中区，hover 气泡后显示且可点击。
    #[gpui::test]
    fn fork_action_is_hidden_until_message_hover_and_clicks(cx: &mut TestAppContext) {
        let (mut visual, forked) = render_chat_with_fork_handler(
            cx,
            fixture_document_without_minimap(ToolStatus::Success),
        );
        assert!(visual.debug_bounds("fork-user-message").is_none());
        let bubble = visual
            .debug_bounds("user-bubble")
            .expect("用户气泡必须存在");
        visual.simulate_mouse_move(bubble.center(), None, Modifiers::default());
        for _ in 0..2 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        let fork = visual
            .debug_bounds("fork-user-message")
            .expect("hover 用户消息后 Fork 操作必须显形");
        visual.simulate_click(fork.center(), Modifiers::default());
        visual.run_until_parked();
        assert_eq!(forked.borrow().as_slice(), ["u"]);
    }

    /// 源码结构断言作为行为测试的补充，防止回退到每行 hover state。
    #[test]
    fn fork_action_uses_message_group_hover_visibility() {
        let source = include_str!("chat.rs");
        let production = source.split("mod tests {").next().unwrap();
        assert!(production.contains(".group(action_group.clone())"));
        assert!(production.contains(".invisible()"));
        assert!(
            production.contains(".group_hover(action_group.clone(), |actions| actions.visible())")
        );
        assert!(production.contains(".debug_selector(|| \"fork-user-message\".into())"));
        assert!(production.contains(".label(\"分叉\")"));
        assert!(production.contains("root.group(action_group.clone())"));
        assert!(!production.contains(".debug_selector(|| \"user-bubble\".into())\n                        .group(action_group.clone())"));
    }

    /// § 5.8 S-18：行高常量唯一；消息正文与用户气泡都必须引用它，不许出现字面量行高。
    #[test]
    fn body_line_height_is_shared_constant() {
        assert!((BODY_LINE_HEIGHT - 1.7).abs() < f32::EPSILON);
        let source = include_str!("chat.rs");
        // 拼接构造检索串，避免本用例的源码把自己算进去。
        let any_call = format!("line_height(relative{}", "(");
        let const_call = format!("{any_call}BODY_LINE_HEIGHT");
        assert_eq!(
            source.matches(&any_call).count(),
            source.matches(&const_call).count(),
            "所有行高都必须引用 BODY_LINE_HEIGHT，不许写字面量（S-18 / § 5.8）"
        );
        assert!(
            source.matches(&const_call).count() >= 2,
            "消息正文与用户气泡两处都必须显式声明行高"
        );
    }

    #[gpui::test]
    fn assistant_model_is_rendered_only_when_metadata_exists(cx: &mut TestAppContext) {
        let with_model = message_with_model(
            "with-model",
            MessageRole::Assistant,
            Some(ModelRef {
                provider: "provider".to_owned(),
                id: "model-id".to_owned(),
            }),
            vec![Block::Markdown(MarkdownBlock {
                source: "answer".to_owned(),
            })],
        );
        let document = Arc::new(ConversationDocument {
            session_id: "model".to_owned(),
            source_path: PathBuf::from("model.jsonl"),
            cwd: PathBuf::new(),
            messages: Arc::from([with_model.clone()]),
            items: Arc::from([ConversationItem::Message(with_model)]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        });
        let mut visual = render_chat_with_names(
            cx,
            document,
            Arc::new(HashMap::from([(
                model_ref_key("provider", "model-id"),
                "Model Display Name".to_owned(),
            )])),
        );
        assert!(visual.debug_bounds("assistant-model").is_some());

        let without_model = message("without-model", MessageRole::Assistant, vec![]);
        let document = Arc::new(ConversationDocument {
            session_id: "none".to_owned(),
            source_path: PathBuf::from("none.jsonl"),
            cwd: PathBuf::new(),
            messages: Arc::from([without_model.clone()]),
            items: Arc::from([ConversationItem::Message(without_model)]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        });
        let mut visual = render_chat(cx, document, Vec::new(), None);
        assert!(visual.debug_bounds("assistant-model").is_none());
    }

    #[gpui::test]
    fn chat_renders_message_flow_and_tool_card(cx: &mut TestAppContext) {
        let mut visual = render_chat(cx, fixture_document(ToolStatus::Success), Vec::new(), None);
        assert!(visual.debug_bounds("chat-message").is_some());
        assert!(visual.debug_bounds("tool-card").is_some());
        // 未展开时不应渲染展开区。
        assert!(visual.debug_bounds("tool-card-details").is_none());
    }

    /// T2 ②（其一）：工具卡 header 必须有状态点，展开区走左竖线而不是嵌套卡片。
    #[gpui::test]
    fn tool_card_header_has_status_dot(cx: &mut TestAppContext) {
        let mut visual = render_chat(
            cx,
            fixture_document(ToolStatus::Error),
            vec!["a:tool:call".to_owned()],
            None,
        );
        let dot = visual
            .debug_bounds("tool-card-status-dot")
            .expect("工具卡 header 必须有状态点");
        assert!(dot.size.width > px(0.) && dot.size.height > px(0.));
        assert!(visual.debug_bounds("tool-card-details").is_some());
    }

    /// T2 ②（其二）：整卡边框是 border 系，不随工具状态变化。
    #[gpui::test]
    fn tool_card_border_is_neutral_for_every_status(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            let neutral = card_border(cx);
            assert_eq!(neutral, cx.theme().border.opacity(0.8));
            for status in [
                ToolStatus::Pending,
                ToolStatus::Success,
                ToolStatus::Error,
                ToolStatus::Empty,
            ] {
                let (_, color) = tool_status_style(status, cx);
                assert_ne!(
                    neutral, color,
                    "{status:?} 的状态色不得被用作整卡边框（规范红线 3）"
                );
            }
            // 状态点是唯一上状态色的地方，成功/失败必须彼此可区分。
            assert_ne!(
                tool_status_style(ToolStatus::Success, cx).1,
                tool_status_style(ToolStatus::Error, cx).1
            );
        });
    }

    /// T2 ⑤：minimap 选中项有左竖条，未选中项不进树。
    #[gpui::test]
    fn minimap_selected_node_has_accent_bar(cx: &mut TestAppContext) {
        let mut unselected =
            render_chat(cx, fixture_document(ToolStatus::Success), Vec::new(), None);
        assert!(unselected.debug_bounds("chat-minimap-node").is_some());
        assert!(
            unselected.debug_bounds("chat-minimap-node-bar").is_none(),
            "没有选中项时不应出现竖条"
        );

        let mut selected = render_chat(
            cx,
            fixture_document(ToolStatus::Success),
            Vec::new(),
            Some("a".to_owned()),
        );
        let bar = selected
            .debug_bounds("chat-minimap-node-bar")
            .expect("minimap 选中项必须有竖条");
        assert_eq!(bar.size.width, px(2.));
        assert!(bar.size.height > px(0.));
    }

    #[gpui::test]
    fn minimap_h2_only_assistant_selects_its_first_node(cx: &mut TestAppContext) {
        let assistant = message(
            "a-h2",
            MessageRole::Assistant,
            vec![Block::Markdown(MarkdownBlock {
                source: "## 仅有二级标题".to_owned(),
            })],
        );
        let document = Arc::new(ConversationDocument {
            session_id: "h2-only".to_owned(),
            source_path: PathBuf::from("h2-only.jsonl"),
            cwd: PathBuf::new(),
            messages: Arc::from([assistant.clone()]),
            items: Arc::from([ConversationItem::Message(assistant)]),
            minimap: Arc::from([
                MinimapNode {
                    message_id: "a-h2".to_owned(),
                    turn: 0,
                    role: MessageRole::Assistant,
                    label: "仅有二级标题".to_owned(),
                    level: Some(3),
                },
                MinimapNode {
                    message_id: "a-h2".to_owned(),
                    turn: 0,
                    role: MessageRole::Assistant,
                    label: "后续三级标题".to_owned(),
                    level: Some(4),
                },
            ]),
            diagnostics: Arc::from([]),
        });

        let mut selected = render_chat(cx, document, Vec::new(), Some("a-h2".to_owned()));
        assert!(
            selected.debug_bounds("chat-minimap-node-bar").is_some(),
            "首个节点即使从 h2 开始也必须承载消息选中态"
        );
        assert_ne!(
            minimap_node_id("a-h2", 0),
            minimap_node_id("a-h2", 1),
            "同消息的 heading 节点必须保持唯一 ElementId"
        );
    }

    #[gpui::test]
    fn minimap_expanded_and_collapsed_widths_are_bounded(cx: &mut TestAppContext) {
        let document = fixture_document(ToolStatus::Success);
        let mut expanded = render_chat(cx, document.clone(), Vec::new(), None);
        let expanded_bounds = expanded
            .debug_bounds("chat-minimap")
            .expect("展开目录必须存在");
        assert!(expanded_bounds.size.width >= px(144.));
        assert!(expanded_bounds.size.width <= px(320.));

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init_fonts(cx).expect("font init failed");
        });
        let item_count = document.items.len();
        let handle = cx.open_window(size(px(640.), px(480.)), move |window, cx| {
            let harness = cx.new(|_| ChatHarness {
                document,
                model_names: Arc::new(HashMap::new()),
                list_state: ListState::new(item_count, ListAlignment::Top, px(1200.)).measure_all(),
                expanded_tools: Arc::new(HashSet::new()),
                selected: None,
                forked: None,
                show_minimap: false,
            });
            Root::new(harness, window, cx)
        });
        let mut collapsed = VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..4 {
            collapsed.update(|window, cx| window.draw(cx).clear(cx));
            collapsed.run_until_parked();
        }
        let collapsed_bounds = collapsed
            .debug_bounds("chat-minimap-collapsed")
            .expect("折叠目录窄条必须存在");
        assert!(collapsed_bounds.size.width >= px(28.));
        assert!(collapsed_bounds.size.width <= px(32.));
    }

    #[test]
    fn minimap_heading_element_ids_are_unique() {
        assert_ne!(
            minimap_node_id("assistant", 0),
            minimap_node_id("assistant", 1)
        );
        assert_ne!(minimap_node_id("assistant", 0), minimap_node_id("other", 0));
        assert_eq!(
            minimap_node_id("assistant", 2).as_ref(),
            "minimap-assistant-2"
        );
    }

    /// 状态点本身只有一个职责：把状态色点出来，不铺背景。
    #[gpui::test]
    fn status_dot_is_the_only_status_colored_surface(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            assert_ne!(cx.theme().success, transparent_black());
            assert_ne!(card_border(cx), cx.theme().warning);
        });
    }
}
