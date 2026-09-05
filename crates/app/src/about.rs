//! 关于 / 版本 / 首启自检。
//!
//! 绿色包免安装、不做自动更新；这里只展示当前版本与内核是否就绪，
//! 并把用户送到 GitHub Releases 自行下载新包。

use gpui::{
    App, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogClose, DialogFooter},
    h_flex, v_flex,
};
use pi_rpc::{PINNED_PI_VERSION, PINNED_SUBAGENTS_LITE_VERSION};

const RELEASES_URL: &str = "https://github.com/ClickPM/GPUI-Pi/releases";

#[must_use]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn open_about_dialog(window: &mut Window, cx: &mut App) {
    let status = pi_runtime::inspect_bundle(&pi_runtime::install_root());
    let observed = status
        .pi_present
        .then(|| pi_runtime::read_pi_version(&status.pi_binary).ok())
        .flatten();
    present_about(window, cx, status, observed, false);
}

#[cfg(not(test))]
pub fn warn_if_runtime_missing(window: &mut Window, cx: &mut App) {
    let status = pi_runtime::inspect_bundle(&pi_runtime::install_root());
    if status.is_ready() {
        return;
    }
    present_about(window, cx, status, None, true);
}

fn present_about(
    window: &mut Window,
    cx: &mut App,
    status: pi_runtime::BundleStatus,
    observed_pi: Option<String>,
    missing: bool,
) {
    let title = if missing {
        "运行时未就绪"
    } else {
        "关于 GPUI-Pi"
    };
    window.open_dialog(cx, move |dialog, _, cx| {
        dialog
            .title(title)
            .overlay_closable(true)
            .child(about_body(&status, observed_pi.as_deref(), missing, cx))
            .footer(
                h_flex()
                    .debug_selector(|| "about-dialog-footer".into())
                    .w_full()
                    .justify_between()
                    .child(
                        Button::new("open-releases")
                            .debug_selector(|| "about-open-releases".into())
                            .ghost()
                            .small()
                            .icon(IconName::ExternalLink)
                            .label("打开 Releases")
                            .tooltip("本应用不做自动更新，请自行下载绿色包")
                            .on_click(|_, _, cx| {
                                cx.open_url(RELEASES_URL);
                            }),
                    )
                    .child(
                        DialogFooter::new().child(
                            DialogClose::new().child(
                                Button::new("close-about")
                                    .debug_selector(|| "about-close".into())
                                    .primary()
                                    .label("关闭"),
                            ),
                        ),
                    ),
            )
    });
}

fn about_body(
    status: &pi_runtime::BundleStatus,
    observed_pi: Option<&str>,
    missing: bool,
    cx: &App,
) -> impl IntoElement {
    let pi_line = match observed_pi {
        Some(version) if pi_runtime::pi_version_matches_pin(version) => {
            format!("pi 内核 {version}（与钉死版本一致）")
        }
        Some(version) => format!("pi 内核 {version}（期望 {PINNED_PI_VERSION}）"),
        None if status.pi_present => {
            format!("pi 内核已找到，版本未探测（钉死 {PINNED_PI_VERSION}）")
        }
        None => format!("pi 内核缺失（钉死 {PINNED_PI_VERSION}）"),
    };
    let kernel_line = if status.kernel_ready {
        format!("子代理内核 {PINNED_SUBAGENTS_LITE_VERSION} 已就绪")
    } else {
        format!("子代理内核 {PINNED_SUBAGENTS_LITE_VERSION} 未就绪")
    };
    let status_color =
        if status.is_ready() && observed_pi.is_none_or(pi_runtime::pi_version_matches_pin) {
            cx.theme().success
        } else {
            cx.theme().warning
        };

    v_flex()
        .debug_selector(|| "about-dialog-body".into())
        .gap_2()
        .p_2()
        .child(
            div()
                .font_semibold()
                .child(format!("GPUI-Pi {}", app_version())),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("免安装绿色包：解压后双击 gpui-pi.exe。不写注册表、不装服务、不依赖 WebView2。"),
        )
        .when(missing, |column| {
            column.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().warning)
                    .child("旁边缺少 vendor\\pi 或子代理内核。请使用 package.ps1 打出来的完整目录，不要只拷贝 exe。"),
            )
        })
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(status_color)
                        .child(if status.is_ready() {
                            "运行时自检通过"
                        } else {
                            "运行时自检未通过"
                        }),
                )
                .child(meta_line(&pi_line, cx))
                .child(meta_line(&kernel_line, cx))
                .child(meta_line(
                    &format!("安装根 {}", status.root.display()),
                    cx,
                )),
        )
        .children(status.problems().into_iter().map(|problem| {
            div()
                .text_xs()
                .text_color(cx.theme().warning)
                .child(problem)
        }))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("本应用不做自动更新。新版本请到 GitHub Releases 下载对应绿色包覆盖本目录。"),
        )
}

fn meta_line(text: &str, cx: &App) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_owned())
}
