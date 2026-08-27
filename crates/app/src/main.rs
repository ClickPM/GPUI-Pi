//! GPUI-Pi 正式桌面入口。

mod file_explorer;
mod live_session;
mod main_panel;
mod model_config;
mod model_service;
mod panels;
mod resource_config;
mod session_sidebar;
mod trust_prompt;
mod workspace;

use gpui::*;
use gpui_component::{Root, TitleBar};
use gpui_component_assets::Assets;
use workspace::Workspace;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            gpui_pi_ui::theme::init_fonts(cx).expect("无法加载内嵌字体");

            let runtime_manager = pi_runtime::RuntimeManager::new(Default::default());

            // R25：窗口全关时**显式**收走全部 pi 进程，不再依赖 Drop 链。
            //
            // 标签持有的 `SessionHandle` 会让 `RuntimeEntry` 比 Manager 活得久，靠 Drop
            // 链回收等于把进程退出推迟到一个不确定的时刻（BACKLOG #19 / #25），多会话
            // 又把它从"最多一个进程"放大到 `total_runtime_slots` 个。
            //
            // 进程真的泄漏不了 —— 每棵树的 Job Object 都带 `KILL_ON_JOB_CLOSE`，本进程
            // 一退它们就没了。这里等的是**优雅**停机：让 pi 收到 stdin EOF、把会话文件
            // 写完再退，而不是被内核直接掐断。
            let shutdown_manager = runtime_manager.clone();
            cx.on_window_closed(move |cx, _| {
                if cx.windows().is_empty() {
                    shutdown_manager.shutdown_all();
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);

            let mut window_size = size(px(1280.), px(820.));
            if let Some(display) = cx.primary_display() {
                let display_size = display.bounds().size;
                window_size.width = window_size.width.min(display_size.width * 0.88);
                window_size.height = window_size.height.min(display_size.height * 0.88);
            }

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    window_size,
                    cx,
                ))),
                window_min_size: Some(size(px(800.), px(560.))),
                kind: WindowKind::Normal,
                #[cfg(target_os = "linux")]
                window_background: WindowBackgroundAppearance::Transparent,
                #[cfg(target_os = "linux")]
                window_decorations: Some(WindowDecorations::Client),
                ..TitleBar::window_options()
            };

            cx.spawn(async move |cx| {
                let window = cx.open_window(options, |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new(runtime_manager, window, cx));
                    cx.new(|cx| Root::new(workspace, window, cx))
                })?;

                window.update(cx, |_, window, _| {
                    window.activate_window();
                    window.set_window_title("GPUI-Pi");
                })?;

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        });
}
