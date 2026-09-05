//! 内建子代理任务面板。
//!
//! 挂在 composer 上方而不是单开一个 dock：子代理任务是**当前会话**的事实（由该会话
//! 的文档还原而来），跟着会话标签走才不会出现「切了标签面板还停在上一个会话」的错位。
//! 折叠态只占一行，展开才铺开任务列表 —— 大多数时候用户只需要知道「还有几个在跑」。

use std::sync::Arc;

use gpui::{
    App, Div, InteractiveElement as _, ParentElement as _, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, StyledExt as _, h_flex, scroll::ScrollableElement as _,
    tooltip::Tooltip, v_flex,
};
use pi_render::{SubagentStatus, SubagentTask};

use crate::theme::dim_foreground;

type ToggleHandler = Arc<dyn Fn(&mut App)>;

/// 折叠态那一行的汇总：行内一个片段 + tooltip 里的分桶明细。
///
/// 规范 S-8 的判定规则明确「`·` 分隔的每一段**各算一个**」，所以不能把
/// 「2 运行 · 1 排队 · 1 失败」拼成一串就当作一个片段。行内只放最该被看见的一项，
/// 完整分桶交给 tooltip（条款：被 tooltip 承载的内容不计入片段数）。
#[must_use]
pub fn subagent_summary(tasks: &[SubagentTask]) -> crate::chat::SubagentStatsText {
    let mut running = 0_usize;
    let mut queued = 0_usize;
    let mut failed = 0_usize;
    let mut halted = 0_usize;
    let mut done = 0_usize;
    for task in tasks {
        match task.status {
            SubagentStatus::Running => running += 1,
            SubagentStatus::Queued => queued += 1,
            SubagentStatus::Error => failed += 1,
            // 「已停止」与「达到轮次上限」都是没跑完就结束的，单列一档。
            // 归进「完成」会让汇总写着「3 完成」而展开后每行都是「已停止」，
            // 汇总与明细互相打脸 —— 与 F-2 被判红的理由同型。
            SubagentStatus::Stopped | SubagentStatus::TurnLimit => halted += 1,
            _ if task.status.is_settled() => done += 1,
            _ => running += 1,
        }
    }
    let buckets = [
        (running, "运行"),
        (queued, "排队"),
        (failed, "失败"),
        (halted, "中止"),
        (done, "完成"),
    ];
    let detail = buckets
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(" · ");

    // 行内优先「还在动的」：运行 > 排队 > 失败 > 完成。用户扫一眼最想知道的是
    // 「还有几个没好」，而不是已经完成了几个。
    let inline = buckets.iter().find(|(count, _)| *count > 0).map_or_else(
        || format!("{} 个任务", tasks.len()),
        |(count, label)| format!("{count} {label}"),
    );
    crate::chat::SubagentStatsText {
        inline,
        detail: if detail.is_empty() {
            format!("{} 个任务", tasks.len())
        } else {
            detail
        },
    }
}

/// 汇总点的颜色。
///
/// 与任务行共用 [`crate::chat::subagent_status_color`]，保证同一语义在两处同色：
/// 还有没结算的 → 按「运行中」取色；全部结算但有失败 → 按「失败」取色；
/// 全部正常收尾 → 中性色（不给"完成"上绿点，一屏全绿会把真正需要注意的点淹掉）。
fn summary_dot_color(tasks: &[SubagentTask], cx: &App) -> gpui::Hsla {
    if has_active(tasks) {
        return crate::chat::subagent_status_color(&SubagentStatus::Running, cx);
    }
    if tasks
        .iter()
        .any(|task| task.status == SubagentStatus::Error)
    {
        return crate::chat::subagent_status_color(&SubagentStatus::Error, cx);
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            SubagentStatus::Stopped | SubagentStatus::TurnLimit
        )
    }) {
        return crate::chat::subagent_status_color(&SubagentStatus::Stopped, cx);
    }
    cx.theme().muted_foreground
}

/// 是否还有没结算的任务 —— 决定折叠态状态点用不用「进行中」的颜色。
fn has_active(tasks: &[SubagentTask]) -> bool {
    tasks.iter().any(|task| !task.status.is_settled())
}

/// 渲染子代理任务面板。`tasks` 为空时返回 `None`，调用方据此整块不画。
#[must_use]
pub fn render_subagent_tasks(
    tasks: &[SubagentTask],
    expanded: bool,
    on_toggle: Option<ToggleHandler>,
    cx: &App,
) -> Option<Div> {
    if tasks.is_empty() {
        return None;
    }
    let summary = subagent_summary(tasks);
    let summary_detail = summary.detail.clone();
    let summary_has_more = summary_detail != summary.inline;

    Some(
        v_flex()
            .debug_selector(|| "subagent-panel".into())
            .min_w_0()
            .px_3()
            .py_1()
            .gap_1()
            .child(
                h_flex()
                    .id(SharedString::from("subagent-panel-toggle"))
                    .debug_selector(|| "subagent-panel-toggle".into())
                    .gap_1p5()
                    .cursor_pointer()
                    // 规范 § 4.4 字面规定的 hover 反馈：`bg(muted)`。
                    //
                    // **不要**照抄 thinking 折叠头的 `hover(text_color(foreground))` 而漏掉
                    // 它的前一句 —— 那种写法要求该行先有 `text_color(muted_foreground)` 基线，
                    // 才有落差可言。本行没有基线色：`AppShell` 根节点已经把环境色设成
                    // `foreground`（`shell.rs`），祖先链上无人改写，于是「hover 时改成
                    // foreground」是把 foreground 改成 foreground，零像素变化 —— 红线 9
                    // 点名的「无状态反馈的 hover 空白」原样成立。R26 第一版就是这么错的。
                    //
                    // S-1 明确 hover 的 `muted` 属「同一表面上的临时叠加，不是新层」，
                    // 因此卡片内使用不违反 S-3 的表面层级限制。
                    .hover(|row| row.bg(cx.theme().muted))
                    .when(summary_has_more, |row| {
                        let detail = summary_detail.clone();
                        row.tooltip(move |window, cx| {
                            Tooltip::new(detail.clone()).build(window, cx)
                        })
                    })
                    .on_click(move |_, _, cx| {
                        if let Some(handler) = &on_toggle {
                            handler(cx);
                        }
                    })
                    // 状态色只点不铺（规范 S-4）。
                    //
                    // 汇总点表达的是「整批任务的当前处境」，与逐条状态是不同维度，
                    // 但取色必须与任务行同源，否则会出现「全部结算且有失败时，汇总点是
                    // 中性灰、行内点是 danger、汇总文字还写着『1 失败』」这种自相矛盾。
                    .child(crate::chat::status_dot(summary_dot_color(tasks, cx)))
                    // 两个文本片段：标题 + 行内汇总。完整分桶在 tooltip 里（S-8）。
                    .child(div().text_xs().font_semibold().child("子代理"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary.inline),
                    )
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                    ),
            )
            .when(expanded, |panel| {
                // 展开区必须有高度上限并可滚动：一个长会话能攒下几十条历史子代理任务，
                // 无约束地把每一行都塞进 composer 上方，会把输入框整个顶出窗口 ——
                // 在 900×700 这种小窗口上尤其明显，而那正是用户最需要 composer 的时候。
                //
                // 高度走 gpui 既有刻度而不是 `px(n)`：红线 4 的 px 白名单里没有这个值，
                // 自造像素尺寸要先改规范文档走评审。滚动条走 `overflow_y_scrollbar()`
                // （规范 S-19），裸 `overflow_y_scroll()` 没有任何「还有内容」的线索。
                let integration_hint = serial_integration_hint(tasks);
                panel
                    .when_some(integration_hint, |panel, hint| {
                        panel.child(
                            div()
                                .debug_selector(|| "subagent-integration-hint".into())
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(hint),
                        )
                    })
                    .child(
                    v_flex()
                        .id(SharedString::from("subagent-task-list"))
                        .debug_selector(|| "subagent-task-list".into())
                        .min_w_0()
                        .max_h_40()
                        .overflow_y_scrollbar()
                        .children(
                            tasks
                                .iter()
                                .map(|task| render_task_row(task, cx))
                                .collect::<Vec<_>>(),
                        ),
                )
            }),
    )
}

/// R27：串行集成提示。队头 = 第一条已结算且带 worktree 的非 Explore 任务。
fn serial_integration_hint(tasks: &[SubagentTask]) -> Option<String> {
    let pending: Vec<_> = tasks
        .iter()
        .filter(|task| {
            task.status.is_settled()
                && task.worktree_path.is_some()
                && !task.agent_type.eq_ignore_ascii_case("explore")
        })
        .collect();
    let head = pending.first()?;
    let rest = pending.len().saturating_sub(1);
    let desc = if head.description.is_empty() {
        head.agent_type.clone()
    } else {
        head.description.clone()
    };
    Some(if rest == 0 {
        format!("待集成：{desc}")
    } else {
        format!("待集成：{desc}（另有 {rest} 项排队）")
    })
}

/// 单条任务行：状态点 + 类型 + 描述 + **一项**统计 —— 三个文本片段，卡在 S-8 上限。
fn render_task_row(task: &SubagentTask, cx: &App) -> Stateful<Div> {
    let color = crate::chat::subagent_status_color(&task.status, cx);
    let stats = crate::chat::subagent_stats_summary(&task.stats);
    let row_id = SharedString::from(format!("subagent-task-{}", task.key));
    // tooltip：统计明细 + writer worktree（若有）。路径进 tooltip 不计入 S-8 片段。
    let tooltip = {
        let mut parts = Vec::new();
        if let Some(stats) = stats.as_ref() {
            parts.push(stats.detail.clone());
        }
        if let Some(path) = task.worktree_path.as_ref() {
            parts.push(format!("worktree: {path}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    };
    h_flex()
        .id(row_id)
        .debug_selector(|| "subagent-task-row".into())
        .min_w_0()
        .gap_1p5()
        .py_0p5()
        .when_some(tooltip, |row, detail| {
            row.tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
        })
        .child(crate::chat::status_dot(color))
        .child(
            div()
                // 类型名最终可回落到模型写的 `agent` 工具参数，长度不受控；
                // 不加约束时它会先于描述抢走宽度，把右侧统计挤出可视区。
                .min_w_0()
                .max_w_32()
                .truncate()
                .text_xs()
                .font_semibold()
                .child(task.agent_type.clone()),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(task.description.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(dim_foreground(cx))
                // 未结算的任务还没有统计，那就先显示状态词，别留一片空白。
                .child(stats.map_or_else(|| task.status.label().to_owned(), |stats| stats.inline)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_render::SubagentStats;

    fn task(status: SubagentStatus) -> SubagentTask {
        SubagentTask {
            key: "k".to_owned(),
            agent_id: None,
            tool_call_id: None,
            agent_type: "scout".to_owned(),
            description: "d".to_owned(),
            background: true,
            worktree_path: None,
            output_file: None,
            status,
            stop_reason: None,
            stats: SubagentStats::default(),
        }
    }

    #[test]
    fn serial_integration_hint_shows_queue_head_only() {
        let mut a = task(SubagentStatus::Completed);
        a.key = "a".into();
        a.agent_type = "general-purpose".into();
        a.description = "first writer".into();
        a.worktree_path = Some("/wt/a".into());
        let mut b = task(SubagentStatus::Completed);
        b.key = "b".into();
        b.agent_type = "coder".into();
        b.description = "second".into();
        b.worktree_path = Some("/wt/b".into());
        let mut explore = task(SubagentStatus::Completed);
        explore.agent_type = "Explore".into();
        explore.worktree_path = Some("/wt/e".into());
        let hint = serial_integration_hint(&[explore, a, b]).unwrap();
        assert!(hint.contains("first writer"), "{hint}");
        assert!(hint.contains("另有 1 项排队"), "{hint}");
        assert!(!hint.contains("second"));
    }

    #[test]
    fn summary_counts_each_lifecycle_bucket_separately() {
        let tasks = vec![
            task(SubagentStatus::Running),
            task(SubagentStatus::Running),
            task(SubagentStatus::Queued),
            task(SubagentStatus::Error),
            task(SubagentStatus::Completed),
        ];
        let summary = subagent_summary(&tasks);
        // 行内恒为**一个**片段（S-8：`·` 分隔的每段各算一个，拼串不能规避上限）。
        assert_eq!(summary.inline, "2 运行");
        assert!(!summary.inline.contains('·'));
        // 完整分桶进 tooltip，条款明确 tooltip 内容不计入片段数。
        assert_eq!(summary.detail, "2 运行 · 1 排队 · 1 失败 · 1 完成");
        assert!(has_active(&tasks));
    }

    #[test]
    fn settled_only_tasks_are_not_reported_as_active() {
        let tasks = vec![
            task(SubagentStatus::Completed),
            task(SubagentStatus::Stopped),
            task(SubagentStatus::TurnLimit),
        ];
        assert!(!has_active(&tasks));
        let summary = subagent_summary(&tasks);
        // 「已停止」与「达到轮次上限」都没跑完，不能和正常收尾混在「完成」里 ——
        // 否则汇总写着「3 完成」而展开后每行都是「已停止」。
        assert_eq!(summary.inline, "2 中止");
        assert_eq!(summary.detail, "2 中止 · 1 完成");
    }

    #[test]
    fn an_unknown_status_counts_as_active_rather_than_vanishing() {
        // 上游新增状态时，宁可把它算进"运行"也不能让它从汇总里消失 —— 汇总数字
        // 对不上任务条数，比多算一个正在跑的更难排查。
        let tasks = vec![task(SubagentStatus::Unknown("hibernating".to_owned()))];
        assert!(has_active(&tasks));
        assert_eq!(subagent_summary(&tasks).inline, "1 运行");
    }

    /// 折叠头的 hover 必须真的产生视觉变化（规范 § 4.4 / 红线 9）。
    ///
    /// R26 第一版写的是 `hover(text_color(foreground))`，但那行没有基线文本色 ——
    /// `AppShell` 根节点已经把环境色设成 `foreground`，于是 hover 把 foreground
    /// 改成 foreground，零像素变化，而注释还写着「必须有 hover 反馈」。
    ///
    /// 注意：同样的写法在 thinking 折叠头（`chat.rs` 的 `render_thinking`）里是**合法**的，
    /// 因为那一行先写了 `text_color(muted_foreground)` 基线。所以这条只检查子代理的
    /// 两处折叠头，不做全文件级的否定断言 —— 否则会误伤那处正确用法。
    #[test]
    fn collapse_headers_use_a_hover_effect_that_actually_changes_something() {
        let no_op = [".hover(|row| row.text_color(cx.theme().", "foreground))"].concat();
        let wanted = ".hover(|row| row.bg(cx.theme().muted))";

        let panel = include_str!("subagent_panel.rs")
            .split("mod tests {")
            .next()
            .unwrap();
        assert!(
            panel.contains(wanted),
            "面板折叠头必须用 § 4.4 规定的 bg(muted)"
        );
        assert!(
            !panel.contains(&no_op),
            "面板折叠头不得用会退化成 no-op 的 hover 写法"
        );

        // 子代理卡片的折叠头：只截 render_subagent 这一段来判，避开同文件里
        // thinking 折叠头那处合法用法。
        let chat = include_str!("chat.rs");
        let card = chat
            .split("fn render_subagent(")
            .nth(1)
            .and_then(|rest| {
                rest.split(
                    "
fn ",
                )
                .next()
            })
            .expect("render_subagent 必须存在");
        assert!(card.contains(wanted), "子代理卡片折叠头必须用 bg(muted)");
        assert!(
            !card.contains(&no_op),
            "子代理卡片折叠头不得用会退化成 no-op 的 hover 写法"
        );
    }

    /// 汇总点与任务行状态点必须同源取色，否则同一状态会呈现两种颜色。
    ///
    /// 这条**必须真的调用 `summary_dot_color`**。第一版只断言了汇总文案和
    /// `has_active`，把 `summary_dot_color` 整个回退成旧的两态映射也照样全绿 ——
    /// 一个名字承诺守护 X、实际没碰 X 的守卫，比没有守卫更危险，因为它让人以为有。
    #[gpui::test]
    fn the_summary_dot_never_contradicts_the_rows_it_summarizes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);

            // 全部结算但有失败：汇总点必须与行内的 danger 点同色，
            // 否则会出现「汇总点中性灰、行内点 danger、汇总文字写着 1 失败」的自相矛盾。
            let settled_with_failure =
                vec![task(SubagentStatus::Completed), task(SubagentStatus::Error)];
            assert!(!has_active(&settled_with_failure));
            assert_eq!(
                summary_dot_color(&settled_with_failure, cx),
                crate::chat::subagent_status_color(&SubagentStatus::Error, cx),
            );
            assert_eq!(
                subagent_summary(&settled_with_failure).detail,
                "1 失败 · 1 完成"
            );

            // 还有在跑的：按「运行中」取色，同样与行内同源。
            let still_running = vec![
                task(SubagentStatus::Running),
                task(SubagentStatus::Completed),
            ];
            assert_eq!(
                summary_dot_color(&still_running, cx),
                crate::chat::subagent_status_color(&SubagentStatus::Running, cx),
            );

            // 全部正常收尾：中性色。不给「完成」上绿点 —— 一屏全绿会把真正
            // 需要注意的点淹掉（S-4 的用意）。
            let all_done = vec![task(SubagentStatus::Completed)];
            assert_eq!(
                summary_dot_color(&all_done, cx),
                cx.theme().muted_foreground
            );
        });
    }
}
