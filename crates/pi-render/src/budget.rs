//! 文档过程性负载的聚合预算。
//!
//! # 为什么逐条上限不够
//!
//! 本 crate 早就有**逐条**上限：单条文本 512K 字符、单张图 6MB。但一篇会话可以有
//! 几百条消息，每条都合规，加起来照样能把一篇 `ConversationDocument` 撑到几百 MB；
//! R24 开放多会话之后，这个数字还要再乘以并行会话数。R25 因此补一条**聚合**上限。
//!
//! # 为什么只裁过程性负载
//!
//! 用户 Query 与最终 Assistant Answer 是用户真正要看的东西，裁掉它们等于把对话本身
//! 弄坏。真正会把文档撑爆的是图片字节与工具输出 —— 一次 bash 就能吐 512K 字符，
//! 一张截图就是 6MB，而它们正是 R7 默认折叠起来的「过程详情」。因此预算只覆盖这两类，
//! 并且**从最旧的消息开始释放**：越靠后的过程细节越可能还有人回看。
//!
//! 释放不是静默丢弃：图片转成 [`ImageState::Redacted`] 并在说明里写明原因，工具输出
//! 换成一句可见的占位文案。
//!
//! **占位文案不承诺"重开会话就能看到"** —— 重开会走 `render_session`，同一份确定性
//! 预算会把同样那批最旧内容再裁一遍，看到的还是占位。完整内容确实还在会话文件里，
//! 但要真正显示出来需要按需 rehydrate（BACKLOG #31），R25 没有做。

use std::sync::Arc;

use serde_json::Value;

use crate::{AnsiText, Block, DiffBlock, ImageBlock, ImageState, Message, ToolOutput};

/// 单篇文档的过程性负载聚合上限。
///
/// **可配置初值，不是产品契约**：16MiB 约等于两张满额截图，或几十次大工具输出。
/// 历史段与流式段各自适用一份预算，因此一篇文档的过程性负载上界是它的两倍。
pub const DEFAULT_PAYLOAD_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// 工具输出被释放后的占位文案。
///
/// 措辞刻意只陈述事实（原始内容还在磁盘上），**不承诺任何"这样做就能看到"的操作**：
/// 重开会话会走 `render_session`，同一份确定性预算把同样那批最旧内容再裁一遍，
/// 看到的还是占位。真正的回看需要按需 rehydrate（BACKLOG #31），R25 没有做。
pub const RELEASED_OUTPUT_NOTICE: &str =
    "（工具输出已释放以控制内存占用；原始内容保留在磁盘上的会话文件里，界面暂不提供回看）";
const RELEASED_IMAGE_NOTICE: &str = "已释放以控制内存占用；原始图片保留在磁盘上的会话文件里";

/// 一次预算裁剪实际释放了什么。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionOutcome {
    pub released_bytes: usize,
    pub released_images: usize,
    pub released_outputs: usize,
    /// 已经裁过的消息下标上界（不含）。
    ///
    /// 调用方据它同步裁掉自己那份**原始副本** —— 只裁渲染结果等于只把统计做小，
    /// 原始 JSON 与工具结果照样无界增长。
    pub released_upto: usize,
}

impl RetentionOutcome {
    /// 这一趟有没有真的释放掉东西。
    ///
    /// 只看实际释放量，**不看** `released_upto` —— 后者是"扫到哪儿"的覆盖边界，
    /// 一篇已经全部释放过的文档照样会被完整扫一遍，但那趟什么也没释放。
    pub fn is_empty(&self) -> bool {
        self.released_bytes == 0 && self.released_images == 0 && self.released_outputs == 0
    }
}

/// 一条消息当前占用的过程性负载字节数（估算）。
///
/// 只统计**会被释放的那些字段**：统计了却不释放，预算就可能永远降不下来，
/// 于是把每一条消息都白白裁一遍。
pub fn payload_bytes(message: &Message) -> usize {
    message.blocks.iter().map(block_payload_bytes).sum()
}

fn block_payload_bytes(block: &Block) -> usize {
    match block {
        Block::Image(image) => image_bytes(image),
        Block::Tool(tool) => {
            let output: usize = tool.output.iter().map(output_bytes).sum();
            // `details` 是工具结果里 `details` 字段的整份克隆 —— 编辑类工具会把完整
            // patch 塞在这儿。不算它，一篇满是编辑结果的历史能在"预算通过"的同时
            // 大幅超出上限。`crates/ui` / `crates/app` 对它零引用，释放它没有视觉代价。
            output.saturating_add(tool.details.as_ref().map_or(0, json_string_bytes))
        }
        // 子代理结果是**进程产出**，和工具输出同一性质：单条上限 512KiB，一个长会话里
        // 攒几十条就能把文档撑爆。不计入的话，这类负载会绕开整篇文档的总预算 ——
        // 而它恰恰是 R26 新引入的、唯一会随后台任务数线性增长的字段。
        Block::Subagent(card) => card.result.len(),
        _ => 0,
    }
}

fn image_bytes(image: &ImageBlock) -> usize {
    image.bytes.as_ref().map_or(0, Vec::len)
}

fn output_bytes(output: &ToolOutput) -> usize {
    match output {
        ToolOutput::Text(text) => text.len(),
        ToolOutput::Ansi(ansi) => ansi_bytes(ansi),
        ToolOutput::Image(image) => image_bytes(image),
        ToolOutput::Diff(diff) => diff_bytes(diff),
    }
}

/// JSON 里全部字符串字面量的字节数之和；结构本身的开销忽略不计。
fn json_string_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().map(json_string_bytes).sum(),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, item)| key.len() + json_string_bytes(item))
            .sum(),
        _ => 0,
    }
}

fn ansi_bytes(ansi: &AnsiText) -> usize {
    // span 是定长结构，按数量折算即可，不必逐个丈量。
    ansi.text.len() + ansi.spans.len() * std::mem::size_of::<crate::AnsiSpan>()
}

fn diff_bytes(diff: &DiffBlock) -> usize {
    let parsed: usize = diff
        .files
        .iter()
        .flat_map(|file| file.hunks.iter())
        .map(|hunk| {
            hunk.header.len() + hunk.lines.iter().map(|line| line.text.len()).sum::<usize>()
        })
        .sum();
    // 原文与解析结果同时驻留，两份都要计。
    diff.raw.len() + parsed
}

/// 按预算从**最旧**的消息开始释放过程性负载，就地改写。
///
/// 已在预算内时不做任何事，也不会克隆任何 `Arc`。
pub fn apply_payload_budget(messages: &mut [Arc<Message>], budget: usize) -> RetentionOutcome {
    let mut total: usize = messages.iter().map(|message| payload_bytes(message)).sum();
    let mut outcome = RetentionOutcome::default();
    if total <= budget {
        return outcome;
    }
    for (index, message) in messages.iter_mut().enumerate() {
        if total <= budget {
            break;
        }
        let before = payload_bytes(message);
        // 判据放在**整条消息**上，而不是逐个输出上：占位文案本身占字节，逐个比大小会
        // 把一条条短输出全部跳过，几万条短输出的总量就能永远超预算而一条都释放不掉，
        // 聚合上限直接失效。按整条消息算，短输出多了照样能一次性省下来。
        if !is_worth_releasing(message) {
            continue;
        }
        outcome.released_upto = index + 1;
        // `Arc::make_mut` 只在这条消息还被别的快照共享时克隆 —— 已经发出去的文档
        // 保持原样，不会在 UI 眼皮底下变内容。
        let released = release_payload(Arc::make_mut(message));
        let after = payload_bytes(message);
        let freed = before.saturating_sub(after);
        total = total.saturating_sub(freed);
        outcome.released_bytes = outcome.released_bytes.saturating_add(freed);
        outcome.released_images = outcome.released_images.saturating_add(released.0);
        outcome.released_outputs = outcome.released_outputs.saturating_add(released.1);
    }
    outcome
}

/// 释放这条消息能不能真的省下内存。
///
/// 占位文案本身占字节：把一条 `"ok"` 换成一整句说明是净亏 —— 既丢了有用的输出，
/// 预算又降不下来。
///
/// **已知残留**：如果一篇文档里全是"单条都短于占位文案"的工具消息，它们谁也换不划算，
/// 总量因此可能停在预算之上。这是结构性下界（每条带工具卡片的消息至少要付一份占位），
/// 不是可以靠调策略消除的：要 16MiB 全由这种消息堆出来，需要约 16 万条。真要压下去
/// 只能整条丢弃消息结构，超出「有界缓存」的 remit。见 BACKLOG #34。
pub(crate) fn is_worth_releasing(message: &Message) -> bool {
    payload_bytes(message) > release_cost(message)
}

/// 释放这条消息要付出的占位文案成本（上界）。
///
/// 每张要释放的图片一份说明、每张要释放的工具卡片一份占位文案。
fn release_cost(message: &Message) -> usize {
    message
        .blocks
        .iter()
        .map(|block| match block {
            Block::Image(image) if image.bytes.is_some() => RELEASED_IMAGE_NOTICE.len(),
            Block::Tool(tool) if !tool.output.is_empty() => RELEASED_OUTPUT_NOTICE.len(),
            Block::Subagent(card) if !card.result.is_empty() => RELEASED_OUTPUT_NOTICE.len(),
            _ => 0,
        })
        .sum()
}

/// 释放一条消息的过程性负载，返回 (释放的图片数, 释放的工具输出数)。
///
/// **只在真能省下内存时才动手**：占位文案本身也占字节，把一条 `"ok"` 换成一整句说明
/// 反而更大 —— 那样既丢了有用的输出，`total` 又降不下来，预算白扣一轮。
pub(crate) fn release_payload(message: &mut Message) -> (usize, usize) {
    let mut images = 0;
    let mut outputs = 0;
    for block in &mut message.blocks {
        match block {
            Block::Image(image) => {
                if release_image(image) {
                    images += 1;
                }
            }
            Block::Tool(tool) => {
                // 结构化详情先放掉。它是被计入预算的，而一张"输出为空、详情很大"的卡片
                // （合法的空结果 + 非 patch 结构化详情）会在下面那个 `continue` 上原样
                // 溜走 —— 预算算得到它、却永远释放不掉。
                //
                // 唯一的例外是内建子代理的派发卡片：后台派发的 `agentId` **只**存在于
                // details 里（内核 `tool-execution.ts:229`），它同时是任务面板归并同一次
                // 派发的主键。整份抹掉会让一次派发在面板里裂成两条，其中一条还会因为
                // 丢了 `status` 而把在跑的子代理显示成已完成。所以这里按名字保留一小撮
                // **定长**身份字段，其余照旧释放。
                tool.details = retained_tool_details(&tool.name, tool.details.take());
                if tool.output.is_empty() {
                    continue;
                }

                // 已经只剩占位文案的卡片不再重复计数，否则反复调用会把统计越加越大。
                // 也要认 Ansi 形态：bash 类工具的结果重渲染时会把同一句占位解析成
                // `ToolOutput::Ansi`，只认 Text 的话每次重渲染都会重新计一次数。
                let already_released = tool.output.len() == 1
                    && match &tool.output[0] {
                        ToolOutput::Text(text) => text == RELEASED_OUTPUT_NOTICE,
                        ToolOutput::Ansi(ansi) => ansi.text == RELEASED_OUTPUT_NOTICE,
                        _ => false,
                    };
                if already_released {
                    continue;
                }
                images += tool
                    .output
                    .iter()
                    .filter(|output| matches!(output, ToolOutput::Image(_)))
                    .count();
                outputs += tool.output.len();
                tool.output = vec![ToolOutput::Text(RELEASED_OUTPUT_NOTICE.to_owned())];
            }
            Block::Subagent(card) => {
                // 只放掉正文，卡片本身（类型、状态、统计、transcript 路径）留着 ——
                // 与工具卡片同一口径：结构可见、负载可释放。完整正文仍在磁盘上的
                // 会话文件里，也在 `details.outputFile` 指向的 transcript 里。
                if card.result.is_empty() || card.result == RELEASED_OUTPUT_NOTICE {
                    continue;
                }
                card.result = RELEASED_OUTPUT_NOTICE.to_owned();
                outputs += 1;
            }
            _ => {}
        }
    }
    (images, outputs)
}

/// 释放工具 details 时需要保留的身份字段。
///
/// 只保留短小且不可再生的标识：`agentId` 是 17 字符，`status` / `type` 是短枚举词，
/// 三者加起来不到 100 字节，不构成负载；而它们一旦丢失就无法从别处恢复。
const RETAINED_SUBAGENT_DETAIL_KEYS: [&str; 3] = ["agentId", "status", "type"];

fn retained_tool_details(tool_name: &str, details: Option<Value>) -> Option<Value> {
    if tool_name != crate::AGENT_TOOL {
        return None;
    }
    let details = details?;
    let mut kept = serde_json::Map::new();
    for key in RETAINED_SUBAGENT_DETAIL_KEYS {
        if let Some(value) = details.get(key) {
            kept.insert(key.to_owned(), value.clone());
        }
    }
    (!kept.is_empty()).then(|| Value::Object(kept))
}

fn release_image(image: &mut ImageBlock) -> bool {
    if image.bytes.is_none() {
        return false;
    }
    image.bytes = None;
    image.state = ImageState::Redacted;
    if image.description.is_empty() {
        image.description = RELEASED_IMAGE_NOTICE.to_owned();
    } else if !image.description.contains(RELEASED_IMAGE_NOTICE) {
        image.description = format!("{} · {RELEASED_IMAGE_NOTICE}", image.description);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MarkdownBlock, MessageRole, ToolCard, ToolStatus};
    use serde_json::Value;

    fn subagent_message(id: &str, result: &str) -> Arc<Message> {
        Arc::new(Message {
            id: id.to_owned(),
            role: MessageRole::Custom,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks: vec![Block::Subagent(Box::new(crate::SubagentCard {
                agent_type: "scout".to_owned(),
                description: Some("找调用点".to_owned()),
                short_id: Some("a1b2c3d4".to_owned()),
                agent_id: Some("a1b2c3d4e5f6".to_owned()),
                status: crate::SubagentStatus::Completed,
                stop_reason: None,
                worktree_path: None,
                output_file: Some("/tmp/pi-agent-outputs/a1b2c3d4e5f6.log".to_owned()),
                result: result.to_owned(),
                stats: crate::SubagentStats::default(),
            }))],
        })
    }

    fn image_message(id: &str, bytes: usize) -> Arc<Message> {
        Arc::new(Message {
            id: id.to_owned(),
            role: MessageRole::Assistant,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks: vec![Block::Image(ImageBlock {
                mime_type: Some("image/png".to_owned()),
                state: ImageState::Inline,
                bytes: Some(vec![0_u8; bytes]),
                remote_url: None,
                description: "截图".to_owned(),
            })],
        })
    }

    fn tool_message(id: &str, output_len: usize) -> Arc<Message> {
        Arc::new(Message {
            id: id.to_owned(),
            role: MessageRole::Assistant,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks: vec![Block::Tool(ToolCard {
                id: format!("{id}-tool"),
                name: "bash".to_owned(),
                arguments: Value::Null,
                input_json: "{}".to_owned(),
                preview: "ls".to_owned(),
                status: ToolStatus::Success,
                output: vec![ToolOutput::Text("x".repeat(output_len))],
                details: None,
                orphan: false,
            })],
        })
    }

    fn answer_message(id: &str, text: &str) -> Arc<Message> {
        Arc::new(Message {
            id: id.to_owned(),
            role: MessageRole::Assistant,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks: vec![Block::Markdown(MarkdownBlock {
                source: text.to_owned(),
            })],
        })
    }

    #[test]
    fn documents_within_budget_are_left_untouched() {
        let mut messages = vec![image_message("a", 1024), tool_message("b", 1024)];
        let outcome = apply_payload_budget(&mut messages, 1024 * 1024);
        assert!(outcome.is_empty(), "预算之内不应释放任何东西");
        assert!(matches!(
            &messages[0].blocks[0],
            Block::Image(image) if image.bytes.is_some()
        ));
    }

    #[test]
    fn the_oldest_payloads_are_released_first_and_stay_visible() {
        let mut messages = vec![
            image_message("old", 4096),
            image_message("mid", 4096),
            image_message("new", 4096),
        ];
        // 只放得下最后一张。
        let outcome = apply_payload_budget(&mut messages, 4096);
        assert_eq!(outcome.released_images, 2);
        assert!(outcome.released_bytes >= 8192);

        for (index, expect_released) in [(0, true), (1, true), (2, false)] {
            let Block::Image(image) = &messages[index].blocks[0] else {
                panic!("应仍是图片块");
            };
            assert_eq!(image.bytes.is_none(), expect_released, "第 {index} 张");
            if expect_released {
                assert_eq!(image.state, ImageState::Redacted);
                assert!(
                    image.description.contains(RELEASED_IMAGE_NOTICE),
                    "释放必须在界面上留痕，不能静默消失：{}",
                    image.description
                );
            }
        }
    }

    #[test]
    fn released_tool_output_leaves_a_visible_placeholder() {
        // 预算要留出占位文案自身的体积 —— 释放不是把负载归零，而是换成一句短文案。
        let mut messages = vec![tool_message("old", 8192), tool_message("new", 1024)];
        let outcome = apply_payload_budget(&mut messages, 4096);
        assert_eq!(outcome.released_outputs, 1, "留得下的那条不该被牵连");
        let Block::Tool(tool) = &messages[0].blocks[0] else {
            panic!("应仍是工具卡片");
        };
        assert_eq!(tool.output.len(), 1);
        assert!(
            matches!(&tool.output[0], ToolOutput::Text(text) if text == RELEASED_OUTPUT_NOTICE)
        );
        assert_eq!(tool.preview, "ls", "卡片仍要说明这次跑的是什么");
        assert_eq!(tool.status, ToolStatus::Success, "状态不该被释放改写");
    }

    #[test]
    fn conversation_text_is_never_released() {
        let long_answer = "答".repeat(64 * 1024);
        let mut messages = vec![
            answer_message("answer", &long_answer),
            image_message("shot", 64 * 1024),
        ];
        // 预算小到必须释放，正文仍然必须完整保留。
        apply_payload_budget(&mut messages, 0);
        let Block::Markdown(markdown) = &messages[0].blocks[0] else {
            panic!("正文块应原样保留");
        };
        assert_eq!(
            markdown.source, long_answer,
            "预算只覆盖过程性负载，不得裁掉对话正文"
        );
    }

    #[test]
    fn releasing_twice_is_idempotent() {
        let mut messages = vec![tool_message("old", 8192), image_message("shot", 8192)];
        let first = apply_payload_budget(&mut messages, 0);
        let second = apply_payload_budget(&mut messages, 0);
        assert!(!first.is_empty());
        assert!(
            second.is_empty(),
            "已经释放过的负载不该被重复计数：{second:?}"
        );
    }

    /// 输出为空、详情很大的工具卡片：预算算得到它，就必须释放得掉它。
    #[test]
    fn tool_details_are_released_even_when_the_output_is_empty() {
        let patch = "+line
"
        .repeat(4096);
        let mut messages = vec![Arc::new(Message {
            id: "edit".to_owned(),
            role: MessageRole::Assistant,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks: vec![Block::Tool(ToolCard {
                id: "edit-tool".to_owned(),
                name: "edit".to_owned(),
                arguments: Value::Null,
                input_json: "{}".to_owned(),
                preview: "edit".to_owned(),
                status: ToolStatus::Success,
                // 合法的空结果：内容为空，但结构化详情很大。
                output: Vec::new(),
                details: Some(serde_json::json!({ "summary": patch })),
                orphan: false,
            })],
        })];
        let counted = payload_bytes(&messages[0]);
        assert!(counted >= 4096, "详情必须被计入，实际只数出 {counted}");

        let outcome = apply_payload_budget(&mut messages, 0);
        assert!(!outcome.is_empty(), "算得到就必须释放得掉");
        let Block::Tool(tool) = &messages[0].blocks[0] else {
            panic!("应仍是工具卡片");
        };
        assert!(tool.details.is_none());
        assert_eq!(payload_bytes(&messages[0]), 0);
    }

    #[test]
    fn shared_snapshots_are_not_mutated_behind_the_reader_back() {
        let mut messages = vec![image_message("shot", 8192)];
        let snapshot = messages[0].clone();
        apply_payload_budget(&mut messages, 0);
        let Block::Image(kept) = &snapshot.blocks[0] else {
            panic!("旧快照应仍是图片块");
        };
        assert!(
            kept.bytes.is_some(),
            "已经发出去的文档快照必须原样不动，不能在 UI 眼皮底下变内容"
        );
    }

    #[test]
    fn subagent_results_are_counted_and_released_like_tool_output() {
        // R26 新增的这条负载会随后台任务数线性增长；不入账就会整个绕开文档总预算。
        let big = "x".repeat(200 * 1024);
        let mut messages = vec![
            subagent_message("s1", &big),
            subagent_message("s2", &big),
            subagent_message("s3", &big),
        ];
        let before: usize = messages.iter().map(|m| payload_bytes(m)).sum();
        assert!(before >= 600 * 1024, "实际 {before}");

        let outcome = apply_payload_budget(&mut messages, 128 * 1024);
        assert!(outcome.released_outputs > 0, "必须真的释放掉一些正文");
        let after: usize = messages.iter().map(|m| payload_bytes(m)).sum();
        assert!(after < before, "释放后总量必须下降：{before} -> {after}");

        // 卡片结构留着，只有正文被换成占位文案 —— 与工具卡片同一口径。
        let Block::Subagent(card) = &messages[0].blocks[0] else {
            panic!("子代理块不该被替换成别的类型");
        };
        assert_eq!(card.result, RELEASED_OUTPUT_NOTICE);
        assert_eq!(card.agent_type, "scout");
        assert_eq!(card.status, crate::SubagentStatus::Completed);
        assert_eq!(
            card.output_file.as_deref(),
            Some("/tmp/pi-agent-outputs/a1b2c3d4e5f6.log"),
            "transcript 路径是找回完整正文的唯一线索，不能一起丢掉"
        );
    }

    #[test]
    fn releasing_a_subagent_result_twice_does_not_double_count() {
        let mut messages = vec![subagent_message("s1", &"y".repeat(300 * 1024))];
        let first = apply_payload_budget(&mut messages, 1024);
        let second = apply_payload_budget(&mut messages, 1024);
        assert!(first.released_outputs > 0);
        assert_eq!(
            second.released_outputs, 0,
            "已经只剩占位文案的卡片不该被重复计数"
        );
    }
}
