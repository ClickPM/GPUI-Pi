//! 内建子代理任务面板。
//!
//! 挂在 composer 上方而不是单开一个 dock：子代理任务是**当前会话**的事实（由该会话
//! 的文档还原而来），跟着会话标签走才不会出现「切了标签面板还停在上一个会话」的错位。
//! 折叠态只占一行，展开才铺开任务列表 —— 大多数时候用户只需要知道「还有几个在跑」。

use std::sync::Arc;

use gpui::{
    App, Div, InteractiveElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, StyledExt as _, h_flex, v_flex};
use pi_render::{SubagentStatus, SubagentTask};

use crate::theme::dim_foreground;

type ToggleHandler = Arc<dyn Fn(&mut App)>;

/// 展开态任务列表的最大高度（约 7 行）。
///
/// 面板挂在 composer 正上方，它长多高就等于从输入区抢走多少。取 7 行是因为一屏内
/// 同时在跑的任务通常只有个位数；更多的是历史，滚动看即可。
const EXPANDED_LIST_MAX_HEIGHT: f32 = 168.;

/// 折叠态那一行的汇总文案。
///
/// 规范 S-8 限制一行最多 3 个文本片段，所以整段汇总只当**一个**片段：内部的
/// 「2 运行 · 1 排队」不拆成独立文本节点。
#[must_use]
pub fn subagent_summary(tasks: &[SubagentTask]) -> String {
    let mut running = 0_usize;
    let mut queued = 0_usize;
    let mut failed = 0_usize;
    let mut done = 0_usize;
    for task in tasks {
        match task.status {
            SubagentStatus::Running => running += 1,
            SubagentStatus::Queued => queued += 1,
            SubagentStatus::Error => failed += 1,
            _ if task.status.is_settled() => done += 1,
            _ => running += 1,
        }
    }
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} 运行"));
    }
    if queued > 0 {
        parts.push(format!("{queued} 排队"));
    }
    if failed > 0 {
        parts.push(format!("{failed} 失败"));
    }
    if done > 0 {
        parts.push(format!("{done} 完成"));
    }
    if parts.is_empty() {
        format!("{} 个任务", tasks.len())
    } else {
        parts.join(" · ")
    }
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
    let active = has_active(tasks);
    let summary = subagent_summary(tasks);

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
                    .on_click(move |_, _, cx| {
                        if let Some(handler) = &on_toggle {
                            handler(cx);
                        }
                    })
                    .child(
                        div()
                            .size_2()
                            .flex_none()
                            .rounded_full()
                            // 状态色只点不铺（规范 S-4）。
                            .bg(if active {
                                cx.theme().warning
                            } else {
                                cx.theme().muted_foreground
                            }),
                    )
                    .child(div().text_xs().font_semibold().child("子代理"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
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
                panel.child(
                    v_flex()
                        .id(SharedString::from("subagent-task-list"))
                        .debug_selector(|| "subagent-task-list".into())
                        .min_w_0()
                        .max_h(px(EXPANDED_LIST_MAX_HEIGHT))
                        .overflow_y_scroll()
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

/// 单条任务行：状态点 + 类型/描述 + 右侧统计。三个文本片段，正好卡在 S-8 上限。
fn render_task_row(task: &SubagentTask, cx: &App) -> Div {
    let color = crate::chat::subagent_status_color(&task.status, cx);
    let stats = crate::chat::subagent_stats_summary(&task.stats);
    h_flex()
        .debug_selector(|| "subagent-task-row".into())
        .min_w_0()
        .gap_1p5()
        .py_0p5()
        .child(div().size_2().flex_none().rounded_full().bg(color))
        .child(
            div()
                .flex_none()
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
                .child(stats.unwrap_or_else(|| task.status.label().to_owned())),
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
    fn summary_counts_each_lifecycle_bucket_separately() {
        let tasks = vec![
            task(SubagentStatus::Running),
            task(SubagentStatus::Running),
            task(SubagentStatus::Queued),
            task(SubagentStatus::Error),
            task(SubagentStatus::Completed),
        ];
        assert_eq!(
            subagent_summary(&tasks),
            "2 运行 · 1 排队 · 1 失败 · 1 完成"
        );
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
        assert_eq!(subagent_summary(&tasks), "3 完成");
    }

    #[test]
    fn an_unknown_status_counts_as_active_rather_than_vanishing() {
        // 上游新增状态时，宁可把它算进"运行"也不能让它从汇总里消失 —— 汇总数字
        // 对不上任务条数，比多算一个正在跑的更难排查。
        let tasks = vec![task(SubagentStatus::Unknown("hibernating".to_owned()))];
        assert!(has_active(&tasks));
        assert_eq!(subagent_summary(&tasks), "1 运行");
    }
}
