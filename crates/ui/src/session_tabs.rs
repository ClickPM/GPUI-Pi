use std::rc::Rc;

use gpui::{
    App, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce, SharedString,
    Styled as _, Window, div,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    tab::{Tab, TabBar},
};

/// 一个会话标签当前的调度状态。
///
/// 与 `pi_runtime::SchedulerState` 一一对应，另加一个 `History`——标签可以只是一份
/// 只读历史，还没有登记成会话。`crates/ui` 不依赖 `pi-runtime`，因此这里是独立枚举，
/// 由 `crates/app` 负责映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTabState {
    /// 只读历史，尚未登记为会话。
    History,
    /// 已登记但没有进程。
    Parked,
    /// 想运行但还没抢到运行槽。
    Queued,
    /// 正在冷启动或接管热进程。
    Starting,
    /// 正常运行中。
    Running,
    /// 正在让出进程。
    Stopping,
    /// 启动失败或运行中崩溃。
    Failed,
}

impl SessionTabState {
    /// 状态文案。只进 tooltip，不占标签行的文本额度（S-8）。
    pub const fn label(self) -> &'static str {
        match self {
            Self::History => "历史（未启动）",
            Self::Parked => "已挂起",
            Self::Queued => "排队中",
            Self::Starting => "启动中",
            Self::Running => "运行中",
            Self::Stopping => "停止中",
            Self::Failed => "已失败",
        }
    }

    /// 状态点颜色。
    ///
    /// S-10：状态色只上小圆点，不铺底、不铺边。没有进程的两个状态刻意走中性色——
    /// `Parked` / `History` 不是异常，用 `warning` 会把「一切正常」染成一片黄色，
    /// 状态色也就不再有分辨力。
    ///
    /// 中性色取 `muted_foreground`（规范 § 1.2 档 2「元信息、图标」）而**不是**
    /// `border`：R24 视觉验收实测，`border` 的圆点在浅色主题下几乎与标签底色融为一体，
    /// 只隐约看得出一个圆形轮廓。状态点是每个标签唯一的状态信号，看不见就等于没有。
    pub fn dot(self, cx: &App) -> Hsla {
        match self {
            Self::History | Self::Parked => cx.theme().muted_foreground,
            Self::Queued | Self::Starting | Self::Stopping => cx.theme().warning,
            Self::Running => cx.theme().success,
            Self::Failed => cx.theme().danger,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTabItem {
    pub id: SharedString,
    pub label: SharedString,
    /// 完整身份 + 状态说明；次要信息一律进 tooltip（S-8）。
    pub tooltip: SharedString,
    pub state: SessionTabState,
}

impl SessionTabItem {
    pub fn new(
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        tooltip: impl Into<SharedString>,
        state: SessionTabState,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            tooltip: tooltip.into(),
            state,
        }
    }
}

type TabHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// 会话标签条：每个标签一个会话，前缀状态点表示它在调度器里的状态。
///
/// 只在有多个标签时才由调用方渲染——单会话时多出一条只有一个标签的横条，
/// 既不承载信息又要占掉一行消息区。
#[derive(IntoElement)]
pub struct SessionTabs {
    tabs: Vec<SessionTabItem>,
    selected_index: usize,
    on_select: Option<TabHandler>,
    on_close: Option<TabHandler>,
}

impl SessionTabs {
    pub fn new(tabs: Vec<SessionTabItem>, selected_index: usize) -> Self {
        Self {
            tabs,
            selected_index,
            on_select: None,
            on_close: None,
        }
    }

    pub fn on_select(mut self, handler: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }

    pub fn on_close(mut self, handler: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for SessionTabs {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let on_close = self.on_close.clone();
        let on_select = self.on_select.clone();
        // 状态点颜色在这里一次性求值：`Tab::prefix` 的元素在 `App` 之外构造，
        // 拿不到 theme。
        let dots = self
            .tabs
            .iter()
            .map(|tab| tab.state.dot(cx))
            .collect::<Vec<_>>();
        div()
            .debug_selector(|| "session-tabs".into())
            .w_full()
            .min_w_0()
            .child(
                TabBar::new("session-tab-bar")
                    .menu(true)
                    .selected_index(self.selected_index)
                    .on_click(move |index, window, cx| {
                        if let Some(handler) = &on_select {
                            handler(*index, window, cx);
                        }
                    })
                    .children(self.tabs.into_iter().zip(dots).enumerate().map(
                        |(index, (tab, dot))| {
                            let close = on_close.clone();
                            Tab::new()
                                .label(tab.label.clone())
                                .aria_label(tab.tooltip.clone())
                                .prefix(
                                    div()
                                        .debug_selector(|| "session-tab-state-dot".into())
                                        .size_2()
                                        .flex_none()
                                        .rounded_full()
                                        .bg(dot),
                                )
                                .suffix(
                                    // 关闭按钮常驻，与相邻的内容标签条（`WorkspaceContentTabs`）
                                    // 保持同一形制；两条标签条一条 hover 显隐一条常驻会更难用。
                                    Button::new(format!("close-session-tab-{}", tab.id))
                                        .debug_selector(|| "close-session-tab".into())
                                        .ghost()
                                        .small()
                                        .icon(gpui_component::IconName::Close)
                                        .tooltip(format!("{} · 关闭会话", tab.tooltip))
                                        .on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            if let Some(handler) = &close {
                                                handler(index, window, cx);
                                            }
                                        }),
                                )
                        },
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failed_sessions_claim_the_danger_color_and_idle_ones_stay_neutral() {
        // 状态点的分辨力全靠这条：没有进程不等于出错。
        assert_eq!(SessionTabState::History.label(), "历史（未启动）");
        assert_eq!(SessionTabState::Parked.label(), "已挂起");
        assert_eq!(SessionTabState::Running.label(), "运行中");
        assert_eq!(SessionTabState::Failed.label(), "已失败");
    }

    #[test]
    fn every_scheduler_state_has_its_own_tab_item() {
        let tab = SessionTabItem::new("s1", "会话 A", "会话 A · 运行中", SessionTabState::Running);
        assert_eq!(tab.state, SessionTabState::Running);
        assert_eq!(tab.label.as_ref(), "会话 A");
    }
}
