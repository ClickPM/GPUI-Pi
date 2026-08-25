use std::{path::PathBuf, sync::Arc};

use gpui::{
    AppContext as _, Context, Edges, InteractiveElement as _, IntoElement, ParentElement as _,
    PathPromptOptions, Render, SharedString, Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Root, Sizable as _, StyledExt as _, TitleBar,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    dock::{DockArea, DockItem, DockPlacement},
    h_flex,
};
use gpui_pi_ui::{AppShell, WorkspaceTabBar, theme};

use crate::file_explorer::FileExplorerPanel;
use crate::main_panel::MainPanel;
use crate::model_config::ModelConfigPanel;
#[cfg(test)]
use crate::panels::LayoutProbe;
use crate::panels::{ChatPanel, FocusedSessionChanged, SessionsChanged};
use crate::session_sidebar::{
    NewSessionRequested, SessionSelected, SessionSidebar, WorktreeSelected,
};
use crate::trust_prompt::prompt_project_trust;

const MAIN_DOCK_ID: &str = "gpui-pi-main-dock";
const MAIN_DOCK_VERSION: usize = 1;
pub(crate) const SIDEBAR_WIDTH: f32 = 280.;
pub(crate) const FILES_WIDTH: f32 = 320.;

pub struct Workspace {
    dock_area: gpui::Entity<DockArea>,
    sidebar: gpui::Entity<SessionSidebar>,
    file_explorer: gpui::Entity<FileExplorerPanel>,
    main_panel: gpui::Entity<MainPanel>,
    model_config: gpui::Entity<ModelConfigPanel>,
    selected_directory: Option<PathBuf>,
    /// 前台会话标签的身份，工具栏标题只认它。
    ///
    /// 不再用「侧栏最近选中的那一条」：标签条是第二个切会话入口，走它切换时侧栏
    /// 根本没有事件，标题就会一直停在别的会话上。所有会改变前台标签的路径
    /// （侧栏选择、新建会话、切标签、关标签）都会广播 `FocusedSessionChanged`。
    focused_session: Option<FocusedSessionChanged>,
    _appearance_subscription: Subscription,
    _session_subscription: Subscription,
    _new_session_subscription: Subscription,
    _worktree_subscription: Subscription,
    _sessions_changed_subscription: Subscription,
    /// 会话标签条切换前台会话时，把文件浏览器与工作区根目录一起切过去。
    _focused_session_subscription: Subscription,
}

impl Workspace {
    pub fn new(
        runtime_manager: pi_runtime::RuntimeManager,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(runtime_manager, window, cx, None)
    }

    #[cfg(test)]
    pub(crate) fn new_with_probe(
        window: &mut Window,
        cx: &mut Context<Self>,
        probe: LayoutProbe,
    ) -> Self {
        Self::build(
            pi_runtime::RuntimeManager::new(Default::default()),
            window,
            cx,
            Some(probe),
        )
    }

    fn build(
        runtime_manager: pi_runtime::RuntimeManager,
        window: &mut Window,
        cx: &mut Context<Self>,
        #[cfg(test)] probe: Option<LayoutProbe>,
        #[cfg(not(test))] _probe: Option<()>,
    ) -> Self {
        theme::sync_system_theme(window, cx);

        let dock_area =
            cx.new(|cx| DockArea::new(MAIN_DOCK_ID, Some(MAIN_DOCK_VERSION), window, cx));
        let sidebar = cx.new(|cx| {
            #[cfg(test)]
            let panel = SessionSidebar::new_empty(window, cx);
            #[cfg(not(test))]
            let panel = SessionSidebar::new(runtime_manager.clone(), window, cx);
            #[cfg(test)]
            if let Some(probe) = probe.clone() {
                return panel.with_probe(probe);
            }
            panel
        });
        let chat = cx.new(|cx| {
            let panel = ChatPanel::new(runtime_manager.clone(), window, cx);
            #[cfg(test)]
            if let Some(probe) = probe.clone() {
                return panel.with_probe(probe);
            }
            panel
        });
        let file_explorer = cx.new(|cx| {
            let panel = FileExplorerPanel::new(window, cx);
            #[cfg(test)]
            if let Some(probe) = probe.clone() {
                return panel.with_probe(probe);
            }
            panel
        });
        let workspace = cx.new(|cx| MainPanel::new(chat.clone(), &file_explorer, window, cx));
        let model_config = cx.new(|cx| ModelConfigPanel::new(window, cx));

        dock_area.update(cx, |dock_area, cx| {
            dock_area.set_center(DockItem::panel(Arc::new(workspace.clone())), window, cx);
            dock_area.set_left_dock(
                DockItem::panel(Arc::new(sidebar.clone())),
                Some(px(SIDEBAR_WIDTH)),
                true,
                window,
                cx,
            );
            dock_area.set_right_dock(
                DockItem::panel(Arc::new(file_explorer.clone())),
                Some(px(FILES_WIDTH)),
                false,
                window,
                cx,
            );
            dock_area.set_dock_collapsible(
                Edges {
                    left: true,
                    right: true,
                    ..Default::default()
                },
                window,
                cx,
            );
            // 收展入口由应用工具栏统一承载，避免同时显示组件库内置按钮。
            dock_area.set_toggle_button_visible(false, cx);
        });

        let appearance_subscription = cx.observe_window_appearance(window, |_, window, cx| {
            theme::sync_system_theme(window, cx);
            cx.notify();
        });
        let chat_panel = chat.clone();
        let file_panel = file_explorer.clone();
        let main_content = workspace.clone();
        let session_subscription = cx.subscribe_in(
            &sidebar,
            window,
            move |workspace, _, event: &SessionSelected, window, cx| {
                // 先让聊天面板受理；被拒（例如标签已达上限）就不能搬工作区，
                // 否则会出现「聊天还在旧会话、文件树已经跳到被拒绝的那个」。
                if let Err(error) = chat_panel.update(cx, |panel, cx| {
                    panel.load_selection(event.clone(), window, cx)
                }) {
                    window.push_notification(
                        gpui_component::notification::Notification::error(format!(
                            "打开会话失败：{error}"
                        )),
                        cx,
                    );
                    return;
                }
                workspace.selected_directory = Some(event.cwd.clone());
                file_panel.update(cx, |panel, cx| {
                    panel.set_root(Some(event.cwd.clone()), window, cx);
                });
                main_content.update(cx, |panel, cx| {
                    panel.set_root(Some(event.cwd.clone()), cx);
                });
                cx.notify();
            },
        );

        let fresh_chat = chat.clone();
        let fresh_files = file_explorer.clone();
        let fresh_main = workspace.clone();
        let new_session_subscription = cx.subscribe_in(
            &sidebar,
            window,
            move |workspace, _, event: &NewSessionRequested, window, cx| {
                match fresh_chat.update(cx, |panel, cx| {
                    panel.start_new_session(event.cwd.clone(), window, cx)
                }) {
                    Ok(()) => {
                        workspace.selected_directory = Some(event.cwd.clone());
                        fresh_files.update(cx, |panel, cx| {
                            panel.set_root(Some(event.cwd.clone()), window, cx);
                        });
                        fresh_main.update(cx, |panel, cx| {
                            panel.set_root(Some(event.cwd.clone()), cx);
                        });
                    }
                    Err(error) => window.push_notification(
                        gpui_component::notification::Notification::error(format!(
                            "新建会话失败：{error}"
                        )),
                        cx,
                    ),
                }
                cx.notify();
            },
        );

        let worktree_subscription = cx.subscribe_in(
            &sidebar,
            window,
            move |workspace, _, event: &WorktreeSelected, window, cx| {
                workspace.apply_browsing_root(event.cwd.clone(), window, cx);
            },
        );
        let sessions_sidebar = sidebar.clone();
        let sessions_changed_subscription = cx.subscribe_in(
            &chat,
            window,
            move |_, _, _: &SessionsChanged, window, cx| {
                sessions_sidebar.update(cx, |sidebar, cx| sidebar.refresh(window, cx));
            },
        );

        let focused_session_subscription = cx.subscribe_in(
            &chat,
            window,
            move |workspace, _, event: &FocusedSessionChanged, window, cx| {
                // 标题状态**无条件**跟上：两个标签同属一个项目时下面的目录短路会生效，
                // 但前台会话确实换了，标题不跟就会一直显示另一个会话的名字。
                //
                // 没有任何身份的事件（关掉最后一个标签后重置出的空白标签）不算「有会话」，
                // 存下来会让标题显示成「新标签」，反而盖掉仍然有效的项目目录回退。
                workspace.focused_session =
                    (event.cwd.is_some() || event.session_key.is_some()).then(|| event.clone());
                let Some(cwd) = event.cwd.clone() else {
                    cx.notify();
                    return;
                };
                // 目录没变就别再重建一次文件树：切标签是高频操作。
                if workspace.selected_directory.as_deref() == Some(cwd.as_path()) {
                    cx.notify();
                    return;
                }
                workspace.apply_browsing_root(cwd, window, cx);
            },
        );

        Self {
            dock_area,
            sidebar,
            file_explorer,
            main_panel: workspace,
            model_config,
            selected_directory: None,
            focused_session: None,
            _appearance_subscription: appearance_subscription,
            _session_subscription: session_subscription,
            _new_session_subscription: new_session_subscription,
            _worktree_subscription: worktree_subscription,
            _sessions_changed_subscription: sessions_changed_subscription,
            _focused_session_subscription: focused_session_subscription,
        }
    }

    fn apply_browsing_root(&mut self, cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_directory = Some(cwd.clone());
        self.file_explorer.update(cx, |panel, cx| {
            panel.set_root(Some(cwd.clone()), window, cx)
        });
        self.main_panel
            .update(cx, |panel, cx| panel.set_root(Some(cwd), cx));
        cx.notify();
    }

    fn toggle_sidebar(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dock_area.update(cx, |dock_area, cx| {
            dock_area.toggle_dock(DockPlacement::Left, window, cx);
        });
        // Dock 自身的 notify 不会重绘 Dock 外的工具栏；这里同步刷新图标和 tooltip。
        cx.notify();
    }

    fn toggle_files(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.dock_area.update(cx, |dock_area, cx| {
            dock_area.toggle_dock(DockPlacement::Right, window, cx);
        });
        cx.notify();
    }

    fn open_resources(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cwd) = self.selected_directory.clone() else {
            return;
        };
        crate::resource_config::open_resource_config(cwd, window, cx);
    }

    fn open_model_config(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_sheet(cx) {
            return;
        }
        self.model_config
            .update(cx, |panel, cx| panel.open(window, cx));
    }

    fn choose_directory(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择项目目录".into()),
        });

        cx.spawn_in(window, async move |workspace, cx| {
            let Some(paths) = receiver.await.ok().and_then(Result::ok).flatten() else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = cx.update(|window, cx| {
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.selected_directory = Some(path.clone());
                    workspace.file_explorer.update(cx, |panel, cx| {
                        panel.set_root(Some(path.clone()), window, cx);
                    });
                    workspace.main_panel.update(cx, |panel, cx| {
                        panel.set_root(Some(path.clone()), cx);
                    });
                    workspace.sidebar.update(cx, |sidebar, cx| {
                        sidebar.set_browsing_cwd(Some(path.clone()), cx);
                    });
                    cx.notify();
                });
                prompt_project_trust(pi_data::agent_dir(), &path, window, cx);
            });
        })
        .detach();
    }

    fn tab_label(&self) -> SharedString {
        self.focused_session.as_ref().map_or_else(
            || {
                self.selected_directory
                    .as_deref()
                    .map_or_else(|| "未选择项目".into(), tab_label_for_path)
            },
            |session| session.title.clone().into(),
        )
    }

    fn directory_tooltip(&self) -> SharedString {
        self.selected_directory.as_deref().map_or_else(
            || "选择项目目录".into(),
            |path| path.display().to_string().into(),
        )
    }

    fn tab_tooltip(&self) -> SharedString {
        self.focused_session.as_ref().map_or_else(
            || self.directory_tooltip(),
            |session| match (&session.session_key, &session.cwd) {
                (Some(key), Some(cwd)) => format!("{key}\n{}", cwd.display()).into(),
                (Some(key), None) => key.clone().into(),
                (None, Some(cwd)) => cwd.display().to_string().into(),
                (None, None) => session.title.clone().into(),
            },
        )
    }
}

fn tab_label_for_path(path: &std::path::Path) -> SharedString {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
        .into()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);

        // TitleBar 的内容区在 Windows 会被标为原生 Drag hitbox，交互控件必须放在其下方。
        let title_bar = TitleBar::new().child(
            h_flex()
                .size_full()
                .min_w_0()
                .child(div().flex_none().font_semibold().child("GPUI-Pi")),
        );
        let sidebar_open = self
            .dock_area
            .read(cx)
            .is_dock_open(DockPlacement::Left, cx);
        let sidebar_toggle_icon = if sidebar_open {
            IconName::PanelLeft
        } else {
            IconName::PanelLeftOpen
        };
        let sidebar_toggle_tooltip = if sidebar_open {
            "收起侧栏"
        } else {
            "展开侧栏"
        };
        let files_open = self
            .dock_area
            .read(cx)
            .is_dock_open(DockPlacement::Right, cx);
        let files_toggle_icon = if files_open {
            IconName::PanelRight
        } else {
            IconName::PanelRightOpen
        };
        let files_toggle_tooltip = if files_open {
            "收起文件面板"
        } else {
            "展开文件面板"
        };
        let toolbar = h_flex()
            .debug_selector(|| "workspace-toolbar".into())
            .h(gpui::px(38.))
            // 150% DPI 下仍给左侧控制留出明确边距，避免图标贴住窗口边缘。
            .px_3()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().title_bar)
            .child(
                div().flex_none().child(
                    Button::new("toggle-left-sidebar")
                        .debug_selector(|| "sidebar-toggle".into())
                        .ghost()
                        .small()
                        .icon(sidebar_toggle_icon)
                        .tooltip(sidebar_toggle_tooltip)
                        .on_click(cx.listener(Self::toggle_sidebar)),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(WorkspaceTabBar::new(self.tab_label()).tooltip(self.tab_tooltip())),
            )
            .child(
                Button::new("toggle-files-panel")
                    .debug_selector(move || format!("files-toggle-{files_open}"))
                    .ghost()
                    .small()
                    .icon(files_toggle_icon)
                    .tooltip(files_toggle_tooltip)
                    .on_click(cx.listener(Self::toggle_files)),
            )
            .child(
                Button::new("open-resource-config")
                    .ghost()
                    .small()
                    .icon(IconName::Settings)
                    .label("资源")
                    .tooltip("查看 Skills / Plugins")
                    .disabled(self.selected_directory.is_none())
                    .on_click(cx.listener(Self::open_resources)),
            )
            .child(
                Button::new("open-model-config")
                    .debug_selector(|| "open-model-config".into())
                    .ghost()
                    .small()
                    .icon(IconName::Settings)
                    .tooltip("打开模型与认证设置")
                    .on_click(cx.listener(Self::open_model_config)),
            )
            .child(
                Button::new("choose-project-directory")
                    .debug_selector(|| "choose-project-directory".into())
                    .ghost()
                    .small()
                    .icon(IconName::FolderOpen)
                    .label("打开目录")
                    .tooltip(self.directory_tooltip())
                    .on_click(cx.listener(Self::choose_directory)),
            );

        div()
            .id("workspace-root")
            .size_full()
            .child(AppShell::new(title_bar, toolbar, self.dock_area.clone()))
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};

    fn render_workspace(
        cx: &mut TestAppContext,
        window_size: gpui::Size<gpui::Pixels>,
        probe: LayoutProbe,
    ) -> VisualTestContext {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("test font init failed");
        });
        let handle = cx.open_window(window_size, move |window, cx| {
            let workspace = cx.new(|cx| Workspace::new_with_probe(window, cx, probe));
            Root::new(workspace, window, cx)
        });
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        visual
    }

    #[gpui::test]
    fn worktree_selection_updates_explorer_and_main_roots(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("test font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let window = cx.open_window(size(px(1000.), px(700.)), move |window, cx| {
            let workspace =
                cx.new(|cx| Workspace::new_with_probe(window, cx, LayoutProbe::default()));
            *output.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let workspace = captured.borrow().clone().unwrap();
        let selected = PathBuf::from("C:/fixture/worktree-b");
        let _ = window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_browsing_root(selected.clone(), window, cx);
                assert_eq!(workspace.selected_directory.as_ref(), Some(&selected));
                assert_eq!(
                    workspace.file_explorer.read(cx).root_for_test(),
                    Some(selected.as_path())
                );
                assert_eq!(
                    workspace.main_panel.read(cx).root_for_test(),
                    Some(selected.as_path())
                );
            });
        });
    }

    /// 第三轮独立审查 P2：标签条切前台会话时，文件浏览器与工作区根目录必须跟着切。
    ///
    /// 多会话之后标签条成了第二个「切会话」入口，而这些面板原本只认侧栏事件。
    /// 不同步就会出现「B 的对话配着 A 的文件树」，工作区操作还打在 A 上。
    #[gpui::test]
    fn switching_session_tabs_moves_the_workspace_roots(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("test font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let window = cx.open_window(size(px(1200.), px(800.)), move |window, cx| {
            let workspace =
                cx.new(|cx| Workspace::new_with_probe(window, cx, LayoutProbe::default()));
            *output.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let workspace = captured.borrow().clone().unwrap();
        let first = PathBuf::from("C:/fixture/project-a");
        let second = PathBuf::from("C:/fixture/project-b");

        let chat = workspace.read_with(cx, |workspace, cx| {
            workspace.main_panel.read(cx).chat_for_test().clone()
        });
        let _ = window.update(cx, |_, window, cx| {
            chat.update(cx, |chat, cx| {
                chat.open_two_tabs_for_test(first.clone(), second.clone(), window, cx);
                // 开完之后前台是第二个标签。
                chat.focus_tab(0, window, cx);
            });
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.selected_directory.as_ref(), Some(&first));
            assert_eq!(
                workspace.file_explorer.read(cx).root_for_test(),
                Some(first.as_path()),
                "文件浏览器必须跟着前台会话走"
            );
            assert_eq!(
                workspace.main_panel.read(cx).root_for_test(),
                Some(first.as_path())
            );
        });

        let _ = window.update(cx, |_, window, cx| {
            chat.update(cx, |chat, cx| chat.focus_tab(1, window, cx));
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.selected_directory.as_ref(), Some(&second));
            assert_eq!(
                workspace.file_explorer.read(cx).root_for_test(),
                Some(second.as_path())
            );
        });
    }

    /// 第四轮独立审查 P2：两个标签同属一个项目时，工具栏标题也必须跟着前台会话走。
    ///
    /// 目录没变会走「跳过重建文件树」的短路，但前台会话确实换了；标题不跟就会一直
    /// 显示另一个会话的名字，而下面已经是别人的对话了。
    #[gpui::test]
    fn the_toolbar_title_follows_the_focused_tab_within_one_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("test font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let window = cx.open_window(size(px(1200.), px(800.)), move |window, cx| {
            let workspace =
                cx.new(|cx| Workspace::new_with_probe(window, cx, LayoutProbe::default()));
            *output.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let workspace = captured.borrow().clone().unwrap();
        let shared = PathBuf::from("C:/fixture/one-project");
        let chat = workspace.read_with(cx, |workspace, cx| {
            workspace.main_panel.read(cx).chat_for_test().clone()
        });

        let _ = window.update(cx, |_, window, cx| {
            chat.update(cx, |chat, cx| {
                // 两个标签、同一个目录、不同标题。
                chat.open_two_tabs_for_test(shared.clone(), shared.clone(), window, cx);
                chat.set_tab_titles_for_test("会话甲", "会话乙");
                chat.focus_tab(0, window, cx);
            });
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.tab_label().as_ref(), "会话甲");
        });

        let _ = window.update(cx, |_, window, cx| {
            chat.update(cx, |chat, cx| chat.focus_tab(1, window, cx));
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.tab_label().as_ref(),
                "会话乙",
                "同项目内切标签，标题也必须跟着走"
            );
        });
    }

    /// 第五轮独立审查 P2：关掉最后一个会话标签后，标题要回落到项目目录。
    ///
    /// `ChatPanel` 会把最后一个标签重置成空白标签并广播一个「没有任何身份」的事件；
    /// 照单收下会让标题显示成「新标签」，反而盖掉仍然有效的项目目录回退。
    #[gpui::test]
    fn closing_the_last_session_falls_back_to_the_project_directory(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("test font init failed");
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = captured.clone();
        let window = cx.open_window(size(px(1200.), px(800.)), move |window, cx| {
            let workspace =
                cx.new(|cx| Workspace::new_with_probe(window, cx, LayoutProbe::default()));
            *output.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let workspace = captured.borrow().clone().unwrap();
        let project = PathBuf::from("C:/fixture/only-project");
        let chat = workspace.read_with(cx, |workspace, cx| {
            workspace.main_panel.read(cx).chat_for_test().clone()
        });

        let _ = window.update(cx, |_, window, cx| {
            chat.update(cx, |chat, cx| {
                chat.open_two_tabs_for_test(project.clone(), project.clone(), window, cx);
                chat.set_tab_titles_for_test("会话甲", "会话乙");
                chat.focus_tab(0, window, cx);
            });
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.tab_label().as_ref(), "会话甲");
        });

        // 关掉两个标签；第二次会把最后一个标签重置成空白标签。
        let _ = window.update(cx, |_, window, cx| {
            chat.update(cx, |chat, cx| {
                chat.close_tab(1, window, cx);
                chat.close_tab(0, window, cx);
            });
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.tab_label().as_ref(),
                "only-project",
                "没有会话时标题应回落到项目目录，而不是显示空白标签的名字"
            );
        });
    }

    #[gpui::test]
    fn default_layout_places_sidebar_left_of_workspace(cx: &mut TestAppContext) {
        let probe = LayoutProbe::default();
        let mut cx = render_workspace(cx, size(px(1200.), px(800.)), probe.clone());

        let shell = cx.debug_bounds("app-shell").expect("app shell missing");
        let title = cx.debug_bounds("app-title-bar").expect("title bar missing");
        let sidebar = probe.sidebar.get();
        let workspace = probe.workspace.get();

        assert!(shell.size.width > px(0.) && shell.size.height > px(0.));
        assert!(sidebar.size.width > px(0.) && workspace.size.width > px(0.));
        assert!(title.origin.y <= sidebar.origin.y);
        assert!(sidebar.origin.x < workspace.origin.x);
        assert!(sidebar.size.width >= px(240.) && sidebar.size.width <= px(360.));
        assert!(workspace.size.width > px(500.));
        let toolbar = cx.debug_bounds("app-toolbar").expect("toolbar missing");
        assert!(sidebar.origin.y >= toolbar.origin.y + toolbar.size.height);
        assert!(workspace.origin.y >= toolbar.origin.y + toolbar.size.height);
    }

    #[gpui::test]
    fn minimum_window_keeps_both_dock_regions_usable(cx: &mut TestAppContext) {
        let probe = LayoutProbe::default();
        let mut cx = render_workspace(cx, size(px(800.), px(560.)), probe.clone());
        let shell = cx.debug_bounds("app-shell").expect("app shell missing");
        let title = cx.debug_bounds("app-title-bar").expect("title bar missing");
        let toolbar = cx.debug_bounds("app-toolbar").expect("toolbar missing");
        let body = cx.debug_bounds("app-dock-body").expect("dock body missing");
        let sidebar = probe.sidebar.get();
        let workspace = probe.workspace.get();

        assert!(shell.size.width > px(0.) && shell.size.height > px(0.));
        assert!(title.size.height > px(0.) && toolbar.size.height > px(0.));
        assert!(body.size.width > px(0.) && body.size.height > px(0.));
        assert!(sidebar.size.height > px(0.) && workspace.size.height > px(0.));
        assert!(sidebar.size.width >= px(200.));
        assert!(workspace.size.width >= px(360.));
        // 左 Dock 的 panel 会包含 tab/title 内边距，布局 contract 只要求中心起点在其右侧。
        assert!(sidebar.origin.x < workspace.origin.x);
        assert!(toolbar.origin.y >= title.origin.y + title.size.height);
        assert!(body.origin.y >= toolbar.origin.y + toolbar.size.height);
        assert!(sidebar.origin.y >= body.origin.y && workspace.origin.y >= body.origin.y);
    }

    #[gpui::test]
    fn sidebar_toggle_collapses_and_reopens_left_dock(cx: &mut TestAppContext) {
        let probe = LayoutProbe::default();
        let mut visual = render_workspace(cx, size(px(1200.), px(800.)), probe.clone());
        let toggle = visual
            .debug_bounds("sidebar-toggle")
            .expect("sidebar toggle missing");
        let expanded_workspace_width = probe.workspace.get().size.width;

        visual.simulate_click(toggle.center(), Default::default());
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert!(visual.debug_bounds("session-sidebar").is_none());
        assert!(probe.workspace.get().size.width > expanded_workspace_width);

        let toggle = visual
            .debug_bounds("sidebar-toggle")
            .expect("sidebar expand toggle missing");
        let prepaints_before_reopen = probe.sidebar_prepaints.get();
        visual.simulate_click(toggle.center(), Default::default());
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert!(probe.sidebar_prepaints.get() > prepaints_before_reopen);
        let reopened_workspace = probe.workspace.get();
        assert!(reopened_workspace.origin.x >= px(200.));
        assert!(reopened_workspace.size.width < px(1000.));
    }

    #[gpui::test]
    fn files_panel_defaults_closed_and_toolbar_toggle_opens_it(cx: &mut TestAppContext) {
        let probe = LayoutProbe::default();
        let mut visual = render_workspace(cx, size(px(1200.), px(800.)), probe.clone());
        assert!(visual.debug_bounds("file-explorer-panel").is_none());
        let toggle = visual
            .debug_bounds("files-toggle-false")
            .expect("files toggle missing");
        visual.simulate_click(toggle.center(), Default::default());
        for _ in 0..8 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        assert!(visual.debug_bounds("files-toggle-true").is_some());
        assert!(probe.files.get().size.width > px(0.));
    }

    #[gpui::test]
    fn directory_button_opens_native_directory_prompt(cx: &mut TestAppContext) {
        let probe = LayoutProbe::default();
        let mut visual = render_workspace(cx, size(px(800.), px(560.)), probe);
        let button = visual
            .debug_bounds("choose-project-directory")
            .expect("directory button missing");

        visual.simulate_click(button.center(), Default::default());
        assert!(visual.did_prompt_for_paths());
        visual.simulate_path_prompt_response(|options| {
            assert!(!options.files);
            assert!(options.directories);
            assert!(!options.multiple);
            None
        });
        visual.run_until_parked();
        assert!(!visual.did_prompt_for_paths());
    }

    #[test]
    fn directory_labels_cover_roots_and_unicode_names() {
        assert_eq!(
            tab_label_for_path(std::path::Path::new("C:\\")).as_ref(),
            "C:\\"
        );
        assert_eq!(
            tab_label_for_path(std::path::Path::new("C:/项目/会话")).as_ref(),
            "会话"
        );
    }
}
