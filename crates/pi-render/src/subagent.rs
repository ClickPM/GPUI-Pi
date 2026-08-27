//! 内建子代理任务的可渲染中间模型。
//!
//! R26 的子代理执行内核是钉死的 `pi-subagents-lite`，它**在 pi 进程内**开会话，
//! 自带的 TUI widget 与 `/agents` 菜单在 `--mode rpc` 下全部失效（`ctx.ui.custom()`
//! 返回 `undefined`、`setWidget` 的组件工厂被忽略）。所以桌面端能拿到的事实只有三样，
//! 本模块只从这三样里还原任务状态，不做任何推测：
//!
//! 1. `Agent` / `StopAgent` 工具的调用与结果（`ToolCard`，`details` 已被渲染层保留）；
//! 2. `customType == "subagent-result"` 的 `custom_message` 条目 —— 后台任务完成时由
//!    内核写进**父会话文件**，因此这也是唯一的持久化事实来源，"回看"靠它；
//! 3. `details.outputFile` 指向的 transcript 文件路径（本模块只透传路径，不读文件）。
//!
//! 刻意不覆盖的：子代理自己的会话文件（内核用 `SessionManager.inMemory`，压根不落盘），
//! 以及 `AgentStatus` 工具的文本输出（是给模型看的自然语言列表，解析它只会得到
//! 一个随上游文案漂移的脆弱耦合）。

use serde_json::Value;

use crate::{Block, ConversationDocument, ToolCard, ToolStatus};

/// 子代理执行内核向模型注册的工具名。与 `pi_rpc::SUBAGENT_TOOL_NAMES` 同源，
/// 但这里不依赖 `pi-rpc`：`pi-render` 是 app / ui 与 pi-runtime 共享的类型层。
pub const AGENT_TOOL: &str = "Agent";
pub const STOP_AGENT_TOOL: &str = "StopAgent";

/// 后台任务完成时写进父会话文件的条目类型。
pub const SUBAGENT_RESULT_CUSTOM_TYPE: &str = "subagent-result";

/// 内核 `record.id.slice(0, SHORT_ID_LENGTH)` 的长度，用来把结果条目对回派发它的工具调用。
const SHORT_ID_LENGTH: usize = 8;

/// 子代理任务的生命周期状态。
///
/// 取值来自内核 `record.lifecycle.status`；未知取值原样保留，宁可显示一个陌生词，
/// 也好过把上游新增的状态悄悄归到"完成"里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    /// 已派发但还在等并发槽。
    Queued,
    /// 正在跑。
    Running,
    /// 正常结束。
    Completed,
    /// 内核报错或前台工具抛异常。
    Error,
    /// 被 `StopAgent` 或父会话中断。
    Stopped,
    /// 撞到 `max_turns` 软上限后结束。
    TurnLimit,
    Unknown(String),
}

impl SubagentStatus {
    /// 解析内核 `record.lifecycle.status` 的取值。
    ///
    /// 这七个是**权威全集**，逐字抄自内核 `src/types.ts` 的
    /// `AgentStatus = "queued" | "running" | "completed" | "turn_limited" | "aborted"
    /// | "stopped" | "error"`。不要凭印象加同义拼写：R26 第一版就是照着字段名
    /// `max_turns` 猜了 `turn_limit` / `turnLimit` / `turn-limit` 三个，而内核实际发的是
    /// `turn_limited` —— 三个都没命中，真实的收尾状态全掉进 `Unknown`，
    /// 而 `Unknown` 的 `is_settled()` 为 false，面板会把已经结束的任务永远算成"运行中"。
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            // 两个都是轮次上限终态，别被词义骗了（内核 `src/status-note.ts:5-6`）：
            //   turn_limited = "wrapped up at the turn limit — output may be partial"
            //                  （撞到软上限后被 steer 要求收尾，自己结束）
            //   aborted      = "hit the turn limit before completion; output may be incomplete"
            //                  （连 graceTurns 也耗尽，`agent-runner.ts:614` 直接 session.abort()）
            // `aborted` **不是**"父会话中断" —— 用户中断、StopAgent、watchdog 三条路径
            // 统统落在 `stopped`（`agent-manager.ts` 的 stopAgent，带 stoppedBy 区分）。
            "turn_limited" | "aborted" => Self::TurnLimit,
            "stopped" => Self::Stopped,
            "error" => Self::Error,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// 是否已经不会再变 —— UI 用它决定要不要继续显示进行中的样式。
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Error | Self::Stopped | Self::TurnLimit
        )
    }

    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Queued => "排队中",
            Self::Running => "运行中",
            Self::Completed => "已完成",
            Self::Error => "失败",
            Self::Stopped => "已停止",
            Self::TurnLimit => "达到轮次上限",
            Self::Unknown(raw) => raw,
        }
    }
}

/// 一次子代理运行的统计。全部可缺省 —— 后台派发刚确认时内核只给 `status`，
/// 统计要等结果条目回来才有。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubagentStats {
    pub turn_count: Option<u64>,
    pub max_turns: Option<u64>,
    pub tool_uses: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub context_percent: Option<f64>,
    pub duration_ms: Option<u64>,
    pub compactions: Option<u64>,
    pub model_name: Option<String>,
    pub thinking_level: Option<String>,
    pub cost: Option<f64>,
}

impl SubagentStats {
    fn merge_from(&mut self, details: &Value) {
        merge_u64(&mut self.turn_count, details, "turnCount");
        merge_u64(&mut self.max_turns, details, "maxTurns");
        merge_u64(&mut self.tool_uses, details, "toolUses");
        merge_u64(&mut self.input_tokens, details, "input");
        merge_u64(&mut self.output_tokens, details, "output");
        merge_u64(&mut self.duration_ms, details, "durationMs");
        merge_u64(&mut self.compactions, details, "compactions");
        merge_f64(&mut self.context_percent, details, "contextPercent");
        merge_f64(&mut self.cost, details, "cost");
        merge_string(&mut self.model_name, details, "modelName");
        merge_string(&mut self.thinking_level, details, "thinkingLevel");
    }
}

/// 一个子代理任务在 UI 上的完整投影。
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentTask {
    /// 稳定主键。优先用内核的 `agentId`；后台派发确认之前只有工具调用 id，
    /// 那就用 `tool:<toolCallId>`，等 `agentId` 出现再切过去。
    pub key: String,
    /// 内核分配的完整 agent id，只有后台派发的结果里才有。
    pub agent_id: Option<String>,
    /// 派发它的那次 `Agent` 工具调用 id；从会话文件回看后台任务时可能没有。
    pub tool_call_id: Option<String>,
    /// agent 类型（`scout` / `reviewer` / `general-purpose` …）。
    pub agent_type: String,
    /// 一行任务描述，内核缺省时取 prompt 首行。
    pub description: String,
    /// 是否后台任务（前台任务的结果内联在工具结果里）。
    pub background: bool,
    /// 跨仓 worktree 路径（R27 才做 Manager 侧强制，这里只透传）。
    pub worktree_path: Option<String>,
    /// transcript 日志路径（内核的 `output_transcript` 打开时才有）。
    pub output_file: Option<String>,
    pub status: SubagentStatus,
    pub stop_reason: Option<String>,
    pub stats: SubagentStats,
}

// 刻意**不带** `prompt` 与 `result`：
//
// 两者都可能很大（结果正文上限 512KiB），而任务列表是每帧从文档重新扫出来的 —— 带上
// 它们等于每帧深拷贝一遍全部子代理产出。更要命的是那份副本**逃出了文档负载预算**：
// `budget.rs` 刚把 `Block::Subagent` 的正文压成占位文案，任务列表里却还躺着完整原文。
// 需要正文的地方读 `Block::Subagent` 卡片本身，那一份是记账、可释放的。

impl SubagentTask {
    fn new(key: String, agent_type: String, description: String) -> Self {
        Self {
            key,
            agent_id: None,
            tool_call_id: None,
            agent_type,
            description,
            background: false,
            worktree_path: None,
            output_file: None,
            status: SubagentStatus::Running,
            stop_reason: None,
            stats: SubagentStats::default(),
        }
    }

    /// 短 id：内核在结果条目的标题里就是用它指代任务的。
    ///
    /// 按**字符**而不是字节截断。实际的 agent id 是十六进制，怎么切都一样；但这个 id
    /// 可能是从 `details.outputFile` 的文件名反推来的，而 `details` 是扩展给的 JSON ——
    /// 一个非 ASCII 的路径会让按字节切正好落在字符中间，直接 panic 掉整个 UI。
    #[must_use]
    pub fn short_id(&self) -> Option<&str> {
        let id = self.agent_id.as_deref()?;
        let end = id
            .char_indices()
            .nth(SHORT_ID_LENGTH)
            .map_or(id.len(), |(index, _)| index);
        Some(&id[..end])
    }
}

/// 从一份已渲染的会话文档里还原全部子代理任务。
///
/// 之所以以 [`ConversationDocument`] 为输入而不是分别接事件与会话条目：流式路径和
/// 历史回看在这一层已经收敛成同一份文档，任务面板因此不需要维护第二套状态机，
/// 也就不会出现"面板和正文对不上"这种只在切会话时复现的问题。
#[must_use]
pub fn collect_tasks(document: &ConversationDocument) -> Vec<SubagentTask> {
    let mut tasks: Vec<SubagentTask> = Vec::new();
    for message in document.messages.iter() {
        for block in &message.blocks {
            match block {
                Block::Tool(card) if card.name == AGENT_TOOL => ingest_agent_tool(&mut tasks, card),
                Block::Tool(card) if card.name == STOP_AGENT_TOOL => {
                    ingest_stop_agent_tool(&mut tasks, card);
                }
                Block::Subagent(card) => ingest_result_card(&mut tasks, card),
                _ => {}
            }
        }
    }
    tasks
}

/// `custom_message` / `subagent-result` 条目解码出的结果卡片。
///
/// 单列成一个 Block 而不是塞进普通自定义消息：正文里要按子代理卡片渲染，
/// 任务面板也要按同一份数据聚合，两边共用一个结构才不会各解析各的。
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentCard {
    pub agent_type: String,
    pub description: Option<String>,
    /// 结果条目标题里那个 8 位短 id。
    pub short_id: Option<String>,
    /// 从 `details.outputFile` 的文件名反推出的完整 agent id。
    pub agent_id: Option<String>,
    pub status: SubagentStatus,
    pub stop_reason: Option<String>,
    pub worktree_path: Option<String>,
    pub output_file: Option<String>,
    pub result: String,
    pub stats: SubagentStats,
}

/// 把一条 `subagent-result` 的正文与 `details` 解码成结果卡片。
///
/// `content` 形如 `[Subagent "scout" a1b2c3d4 completed]\n\n<正文>`；标题行的三段
/// 在 `details` 里也都有，但 `details` 是可选的（旧版本内核、或条目被截断），
/// 所以两边都解析、以 `details` 为准、标题行兜底。
#[must_use]
pub fn decode_result_card(content: &str, details: Option<&Value>) -> SubagentCard {
    let header = parse_result_header(content);
    let body = header
        .as_ref()
        .map_or(content, |parsed| parsed.body)
        .trim_start_matches('\n')
        .to_owned();

    let output_file = details.and_then(|d| string_field(d, "outputFile"));
    let mut stats = SubagentStats::default();
    if let Some(details) = details {
        stats.merge_from(details);
    }

    SubagentCard {
        agent_type: details
            .and_then(|d| string_field(d, "type"))
            .or_else(|| header.as_ref().map(|parsed| parsed.agent_type.clone()))
            .unwrap_or_else(|| "subagent".to_owned()),
        description: details.and_then(|d| string_field(d, "description")),
        short_id: header.as_ref().map(|parsed| parsed.short_id.clone()),
        agent_id: output_file.as_deref().and_then(agent_id_from_output_file),
        status: details
            .and_then(|d| string_field(d, "status"))
            .or_else(|| header.as_ref().map(|parsed| parsed.status.clone()))
            .map_or(SubagentStatus::Completed, |raw| SubagentStatus::parse(&raw)),
        stop_reason: details.and_then(|d| string_field(d, "stopReason")),
        worktree_path: details.and_then(|d| string_field(d, "worktreePath")),
        output_file,
        result: body,
        stats,
    }
}

// ── 内部：三个事实来源各自的归并 ─────────────────────────────────────────────

fn ingest_agent_tool(tasks: &mut Vec<SubagentTask>, card: &ToolCard) {
    let details = card.details.as_ref();
    let agent_id = details.and_then(|d| string_field(d, "agentId"));
    let key = agent_id
        .clone()
        .unwrap_or_else(|| format!("tool:{}", card.id));

    let agent_type = details
        .and_then(|d| string_field(d, "type"))
        .or_else(|| string_field(&card.arguments, "agent"))
        .unwrap_or_else(|| "general-purpose".to_owned());
    let description = details
        .and_then(|d| string_field(d, "description"))
        .or_else(|| string_field(&card.arguments, "description"))
        .or_else(|| {
            string_field(&card.arguments, "prompt")
                .as_deref()
                .and_then(first_line)
        })
        .unwrap_or_else(|| "（无描述）".to_owned());

    let index = upsert(tasks, &key, || {
        SubagentTask::new(key.clone(), agent_type.clone(), description.clone())
    });
    let task = &mut tasks[index];
    task.agent_type = agent_type;
    task.description = description;
    task.tool_call_id = Some(card.id.clone());
    if agent_id.is_some() {
        task.agent_id = agent_id;
    }

    // 是否后台，以 `details.agentId` 在不在为准，`run_in_background` 参数只作兜底。
    //
    // 内核的判据是 `runInBackground || store.agent.forceBackground`
    // （`tool-execution.ts:199`）—— 用户在配置里打开 `forceBackground` 后，模型不传
    // 这个参数也会走后台分支。而 `agentId` 只在后台分支被塞进 details（`:229`），
    // 前台那条 `buildAgentDetails(record, { includeStats: true })` 从不含它，
    // 所以它恰好是后台分支的**充分且必要**标记。
    task.background = task.agent_id.is_some()
        || card
            .arguments
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(task.background);

    if let Some(path) = string_field(&card.arguments, "worktree_path")
        .or_else(|| details.and_then(|d| string_field(d, "worktreePath")))
    {
        task.worktree_path = Some(path);
    }
    if let Some(details) = details {
        task.stats.merge_from(details);
        if let Some(file) = string_field(details, "outputFile") {
            task.output_file = Some(file);
        }
    }

    // 状态优先取 details.status（后台派发确认会带 queued / running）。
    //
    // 没有 details 时**不能**凭工具本身的 Success 判成"已完成"：负载预算会把被裁消息的
    // `details` 剪掉（`budget.rs`），而后台派发的工具结果正文只是一句"已受理"，
    // 那次调用本来就是 Success —— 照着它判会把一个还在跑的子代理显示成已完成。
    // 只有能确认是前台任务（结果就在工具正文里）时，工具状态才是可信的终态信号。
    task.status = match details.and_then(|d| string_field(d, "status")) {
        Some(raw) => SubagentStatus::parse(&raw),
        None if task.background => task.status.clone(),
        None => match card.status {
            ToolStatus::Pending => SubagentStatus::Running,
            ToolStatus::Error => SubagentStatus::Error,
            ToolStatus::Success | ToolStatus::Empty => SubagentStatus::Completed,
        },
    };
}

fn ingest_stop_agent_tool(tasks: &mut [SubagentTask], card: &ToolCard) {
    let Some(target) = string_field(&card.arguments, "agent_id") else {
        return;
    };
    if card.status == ToolStatus::Error {
        return;
    }
    // 模型通常写标题里那个 8 位短 id，而完整 id 有 17 位，所以方向是单一的：
    // 完整 id 以模型给的串开头。反向匹配（`target.starts_with(id)`）没有对应场景。
    //
    // 只有**唯一命中**才改写。内核 `stopAgent` 是按 `getRecord(agentId)` 精确中止一个
    // （`tool-execution.ts`），前缀撞车时我们无从知道是哪一个，宁可不改也不能把
    // 几条任务一起标成已停止 —— 那会凭空造出"用户停了三个"的假象。
    let mut hits = tasks
        .iter_mut()
        .filter(|task| {
            task.agent_id
                .as_deref()
                .is_some_and(|id| id.starts_with(&target))
                && !task.status.is_settled()
        })
        .collect::<Vec<_>>();
    if let [task] = hits.as_mut_slice() {
        task.status = SubagentStatus::Stopped;
    }
}

fn ingest_result_card(tasks: &mut Vec<SubagentTask>, card: &SubagentCard) {
    let key = card
        .agent_id
        .clone()
        .or_else(|| find_key_by_short_id(tasks, card.short_id.as_deref()))
        .or_else(|| card.short_id.clone())
        .unwrap_or_else(|| format!("subagent:{}", card.agent_type));

    let index = upsert(tasks, &key, || {
        SubagentTask::new(
            key.clone(),
            card.agent_type.clone(),
            card.description
                .clone()
                .unwrap_or_else(|| "（无描述）".to_owned()),
        )
    });
    let task = &mut tasks[index];
    task.agent_type.clone_from(&card.agent_type);
    if let Some(description) = &card.description {
        task.description.clone_from(description);
    }
    if task.agent_id.is_none() {
        task.agent_id.clone_from(&card.agent_id);
    }
    if card.worktree_path.is_some() {
        task.worktree_path.clone_from(&card.worktree_path);
    }
    if card.output_file.is_some() {
        task.output_file.clone_from(&card.output_file);
    }
    // 结果条目只在后台任务完成时产生，这一点比派发时的参数更可信。
    task.background = true;
    task.status = card.status.clone();
    task.stop_reason.clone_from(&card.stop_reason);
    task.stats.merge_from_stats(&card.stats);
}

impl SubagentStats {
    /// 结果条目带来的统计覆盖派发时的空值，但不用空值抹掉已有数字。
    fn merge_from_stats(&mut self, other: &Self) {
        take_if_some(&mut self.turn_count, other.turn_count);
        take_if_some(&mut self.max_turns, other.max_turns);
        take_if_some(&mut self.tool_uses, other.tool_uses);
        take_if_some(&mut self.input_tokens, other.input_tokens);
        take_if_some(&mut self.output_tokens, other.output_tokens);
        take_if_some(&mut self.duration_ms, other.duration_ms);
        take_if_some(&mut self.compactions, other.compactions);
        take_if_some(&mut self.context_percent, other.context_percent);
        take_if_some(&mut self.cost, other.cost);
        if other.model_name.is_some() {
            self.model_name.clone_from(&other.model_name);
        }
        if other.thinking_level.is_some() {
            self.thinking_level.clone_from(&other.thinking_level);
        }
    }
}

// ── 内部：解析与小工具 ───────────────────────────────────────────────────────

struct ResultHeader<'a> {
    agent_type: String,
    short_id: String,
    status: String,
    body: &'a str,
}

/// 解析 `[Subagent "scout" a1b2c3d4 completed]` 这一行。
///
/// 手写而不是上正则：`pi-render` 现在没有 regex 依赖，为一行固定格式的标题引入一个
/// crate 不划算（红线 2 下新增依赖还得先改立项文档 § 二）。
fn parse_result_header(content: &str) -> Option<ResultHeader<'_>> {
    let rest = content.strip_prefix("[Subagent ")?;
    let close = rest.find(']')?;
    let (head, tail) = rest.split_at(close);
    let body = tail.strip_prefix(']')?;

    let head = head.strip_prefix('"')?;
    let quote = head.find('"')?;
    let (agent_type, after_type) = head.split_at(quote);
    let mut parts = after_type.trim_start_matches('"').split_whitespace();
    let short_id = parts.next()?.to_owned();
    let status = parts.next()?.to_owned();
    Some(ResultHeader {
        agent_type: agent_type.to_owned(),
        short_id,
        status,
        body,
    })
}

/// transcript 路径形如 `/tmp/pi-agent-outputs/<agentId>.log`，文件名就是完整 agent id。
fn agent_id_from_output_file(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next()?;
    let stem = name.strip_suffix(".log").unwrap_or(name);
    (!stem.is_empty()).then(|| stem.to_owned())
}

fn find_key_by_short_id(tasks: &[SubagentTask], short_id: Option<&str>) -> Option<String> {
    let short_id = short_id?;
    tasks
        .iter()
        .find(|task| task.short_id() == Some(short_id))
        .map(|task| task.key.clone())
}

fn upsert(tasks: &mut Vec<SubagentTask>, key: &str, make: impl FnOnce() -> SubagentTask) -> usize {
    if let Some(index) = tasks.iter().position(|task| task.key == key) {
        return index;
    }
    tasks.push(make());
    tasks.len() - 1
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn merge_u64(slot: &mut Option<u64>, value: &Value, key: &str) {
    if let Some(found) = value.get(key).and_then(Value::as_u64) {
        *slot = Some(found);
    }
}

fn merge_f64(slot: &mut Option<f64>, value: &Value, key: &str) {
    if let Some(found) = value.get(key).and_then(Value::as_f64) {
        *slot = Some(found);
    }
}

fn merge_string(slot: &mut Option<String>, value: &Value, key: &str) {
    if let Some(found) = string_field(value, key) {
        *slot = Some(found);
    }
}

fn take_if_some<T: Copy>(slot: &mut Option<T>, other: Option<T>) {
    if other.is_some() {
        *slot = other;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolOutput};
    use serde_json::json;

    fn tool_card(id: &str, name: &str, arguments: Value, details: Option<Value>) -> ToolCard {
        ToolCard {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
            input_json: String::new(),
            preview: String::new(),
            status: ToolStatus::Success,
            output: Vec::new(),
            details,
            orphan: false,
        }
    }

    fn document(blocks: Vec<Block>) -> ConversationDocument {
        let message = Message {
            id: "m1".to_owned(),
            role: crate::MessageRole::Assistant,
            timestamp: None,
            label: None,
            model: None,
            written_files: Vec::new(),
            blocks,
        };
        ConversationDocument {
            session_id: "s".to_owned(),
            source_path: std::path::PathBuf::new(),
            cwd: std::path::PathBuf::new(),
            messages: std::sync::Arc::from(vec![std::sync::Arc::new(message)]),
            items: std::sync::Arc::from(Vec::new()),
            minimap: std::sync::Arc::from(Vec::new()),
            diagnostics: std::sync::Arc::from(Vec::new()),
        }
    }

    #[test]
    fn parses_the_result_header_the_kernel_writes() {
        let header =
            parse_result_header("[Subagent \"scout\" a1b2c3d4 completed]\n\n找到了三处调用点")
                .unwrap();
        assert_eq!(header.agent_type, "scout");
        assert_eq!(header.short_id, "a1b2c3d4");
        assert_eq!(header.status, "completed");
        assert_eq!(header.body.trim_start(), "找到了三处调用点");
    }

    #[test]
    fn result_header_parse_degrades_instead_of_panicking_on_junk() {
        // 内核换了文案、条目被截断、或者根本不是这个 customType —— 都不许 panic，
        // 也不许把整段正文吞掉。
        for junk in [
            "",
            "[Subagent",
            "[Subagent ]",
            "[Subagent \"scout\"]",
            "[Subagent \"scout\" a1b2c3d4]",
            "普通文本",
        ] {
            assert!(parse_result_header(junk).is_none(), "junk={junk:?}");
            let card = decode_result_card(junk, None);
            assert_eq!(card.result, junk);
        }
    }

    #[test]
    fn a_non_ascii_agent_id_truncates_on_a_char_boundary_instead_of_panicking() {
        // agent id 可能是从 details.outputFile 的文件名反推的，而 details 是扩展给的
        // JSON —— 一个中文路径按字节切会正好落在字符中间，直接把 UI panic 掉。
        let mut task = SubagentTask::new("k".to_owned(), "scout".to_owned(), "d".to_owned());
        task.agent_id = Some("子代理标识符很长很长".to_owned());
        let short = task.short_id().expect("必须有值");
        assert_eq!(short.chars().count(), 8);
        assert_eq!(short, "子代理标识符很长");

        task.agent_id = Some("短".to_owned());
        assert_eq!(task.short_id(), Some("短"));

        task.agent_id = Some(String::new());
        assert_eq!(task.short_id(), Some(""));
    }

    #[test]
    fn recovers_the_full_agent_id_from_the_transcript_path() {
        assert_eq!(
            agent_id_from_output_file("/tmp/pi-agent-outputs/a1b2c3d4e5f6.log").as_deref(),
            Some("a1b2c3d4e5f6")
        );
        assert_eq!(
            agent_id_from_output_file(r"D:\tmp\pi-agent-outputs\a1b2c3d4e5f6.log").as_deref(),
            Some("a1b2c3d4e5f6")
        );
        assert_eq!(agent_id_from_output_file("").as_deref(), None);
    }

    #[test]
    fn details_win_over_the_header_when_both_are_present() {
        let card = decode_result_card(
            "[Subagent \"scout\" a1b2c3d4 completed]\n\n正文",
            Some(&json!({
                "type": "reviewer",
                "status": "error",
                "stopReason": "max turns",
                "outputFile": "/tmp/pi-agent-outputs/a1b2c3d4e5f6.log",
                "turnCount": 7,
                "cost": 0.25
            })),
        );
        assert_eq!(card.agent_type, "reviewer");
        assert_eq!(card.status, SubagentStatus::Error);
        assert_eq!(card.stop_reason.as_deref(), Some("max turns"));
        assert_eq!(card.short_id.as_deref(), Some("a1b2c3d4"));
        assert_eq!(card.agent_id.as_deref(), Some("a1b2c3d4e5f6"));
        assert_eq!(card.stats.turn_count, Some(7));
        assert_eq!(card.stats.cost, Some(0.25));
        assert_eq!(card.result, "正文");
    }

    #[test]
    fn background_dispatch_and_its_result_entry_collapse_into_one_task() {
        // 后台派发时工具结果给完整 agentId 但没有统计；完成时的结果条目反过来 ——
        // 有统计、有 status，却只在标题里留了 8 位短 id。两者必须并成一条任务。
        let dispatch = tool_card(
            "call_1",
            AGENT_TOOL,
            json!({"prompt": "找出所有鉴权代码", "agent": "scout", "run_in_background": true}),
            Some(json!({
                "type": "scout",
                "description": "找鉴权",
                "agentId": "a1b2c3d4e5f6",
                "status": "queued"
            })),
        );
        let result = decode_result_card(
            "[Subagent \"scout\" a1b2c3d4 completed]\n\n三处",
            Some(&json!({
                "type": "scout",
                "status": "completed",
                "turnCount": 4,
                "toolUses": 9,
                "outputFile": "/tmp/pi-agent-outputs/a1b2c3d4e5f6.log"
            })),
        );
        let tasks = collect_tasks(&document(vec![
            Block::Tool(dispatch),
            Block::Subagent(Box::new(result)),
        ]));

        assert_eq!(tasks.len(), 1, "{tasks:#?}");
        let task = &tasks[0];
        assert_eq!(task.key, "a1b2c3d4e5f6");
        assert_eq!(task.agent_id.as_deref(), Some("a1b2c3d4e5f6"));
        assert_eq!(task.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(task.agent_type, "scout");
        assert_eq!(task.description, "找鉴权");
        assert!(task.background);
        assert_eq!(task.status, SubagentStatus::Completed);
        assert_eq!(task.stats.turn_count, Some(4));
        assert_eq!(task.stats.tool_uses, Some(9));
        assert_eq!(
            task.output_file.as_deref(),
            Some("/tmp/pi-agent-outputs/a1b2c3d4e5f6.log")
        );
    }

    #[test]
    fn a_result_entry_alone_still_reconstructs_the_task_for_replay() {
        // 回看历史会话时，派发那次工具调用可能已经被压缩掉，只剩结果条目。
        let result = decode_result_card(
            "[Subagent \"reviewer\" 99887766 completed]\n\n没问题",
            Some(&json!({"type": "reviewer", "status": "completed"})),
        );
        let tasks = collect_tasks(&document(vec![Block::Subagent(Box::new(result))]));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].key, "99887766");
        assert_eq!(tasks[0].agent_type, "reviewer");
        assert!(tasks[0].background);
    }

    #[test]
    fn foreground_task_takes_its_result_from_the_tool_output() {
        let mut card = tool_card(
            "call_2",
            AGENT_TOOL,
            json!({"prompt": "审一下 diff"}),
            Some(json!({"type": "reviewer", "turnCount": 2})),
        );
        card.output = vec![ToolOutput::Text("两处小问题".to_owned())];
        let tasks = collect_tasks(&document(vec![Block::Tool(card)]));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].key, "tool:call_2");
        assert!(!tasks[0].background);
        assert_eq!(tasks[0].status, SubagentStatus::Completed);
        assert_eq!(tasks[0].description, "审一下 diff");
    }

    #[test]
    fn a_failing_foreground_tool_call_marks_the_task_failed() {
        // 前台任务出错时内核是抛异常的，没有 details.status 可读，只能看工具状态。
        let mut card = tool_card(
            "call_3",
            AGENT_TOOL,
            json!({"prompt": "跑测试"}),
            Some(json!({"type": "worker"})),
        );
        card.status = ToolStatus::Error;
        card.output = vec![ToolOutput::Text("Agent failed: boom".to_owned())];
        let tasks = collect_tasks(&document(vec![Block::Tool(card)]));
        assert_eq!(tasks[0].status, SubagentStatus::Error);
    }

    #[test]
    fn stop_agent_matches_on_the_short_id_the_model_actually_writes() {
        let dispatch = tool_card(
            "call_4",
            AGENT_TOOL,
            json!({"prompt": "长任务", "run_in_background": true}),
            Some(json!({"agentId": "a1b2c3d4e5f6", "status": "running"})),
        );
        let stop = tool_card(
            "call_5",
            STOP_AGENT_TOOL,
            json!({"agent_id": "a1b2c3d4"}),
            None,
        );
        let tasks = collect_tasks(&document(vec![Block::Tool(dispatch), Block::Tool(stop)]));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, SubagentStatus::Stopped);
    }

    #[test]
    fn stop_agent_never_rewrites_an_already_settled_task() {
        // 对一个刚好完成的任务调 StopAgent 会返回成功，但那不代表它是被停掉的。
        let dispatch = tool_card(
            "call_6",
            AGENT_TOOL,
            json!({"prompt": "短任务", "run_in_background": true}),
            Some(json!({"agentId": "ffeeddccbbaa", "status": "completed"})),
        );
        let stop = tool_card(
            "call_7",
            STOP_AGENT_TOOL,
            json!({"agent_id": "ffeeddcc"}),
            None,
        );
        let tasks = collect_tasks(&document(vec![Block::Tool(dispatch), Block::Tool(stop)]));
        assert_eq!(tasks[0].status, SubagentStatus::Completed);
    }

    #[test]
    fn every_status_the_kernel_can_emit_is_recognized() {
        // 逐字对齐内核 src/types.ts 的 AgentStatus 全集。任何一个落进 Unknown 都会让
        // 面板把已结束的任务永远算成"运行中"（Unknown 的 is_settled() 为 false）。
        let expected = [
            ("queued", SubagentStatus::Queued, false),
            ("running", SubagentStatus::Running, false),
            ("completed", SubagentStatus::Completed, true),
            // 两个轮次上限终态，别按词义归类：aborted 是「graceTurns 也耗尽后被硬杀」，
            // 不是「父会话中断」——后者走 stopped（内核 status-note.ts:5-6）。
            ("turn_limited", SubagentStatus::TurnLimit, true),
            ("aborted", SubagentStatus::TurnLimit, true),
            ("stopped", SubagentStatus::Stopped, true),
            ("error", SubagentStatus::Error, true),
        ];
        for (raw, status, settled) in expected {
            assert_eq!(SubagentStatus::parse(raw), status, "raw={raw}");
            assert_eq!(
                SubagentStatus::parse(raw).is_settled(),
                settled,
                "raw={raw}"
            );
            assert!(
                !matches!(SubagentStatus::parse(raw), SubagentStatus::Unknown(_)),
                "raw={raw} 不该落进 Unknown"
            );
        }
    }

    #[test]
    fn unknown_status_is_preserved_instead_of_being_folded_into_completed() {
        assert_eq!(
            SubagentStatus::parse("hibernating"),
            SubagentStatus::Unknown("hibernating".to_owned())
        );
        assert!(!SubagentStatus::parse("hibernating").is_settled());
    }

    #[test]
    fn stats_from_the_result_entry_never_erase_numbers_already_collected() {
        let mut stats = SubagentStats {
            turn_count: Some(3),
            cost: Some(0.5),
            ..SubagentStats::default()
        };
        stats.merge_from_stats(&SubagentStats {
            tool_uses: Some(7),
            ..SubagentStats::default()
        });
        assert_eq!(stats.turn_count, Some(3));
        assert_eq!(stats.cost, Some(0.5));
        assert_eq!(stats.tool_uses, Some(7));
    }

    #[test]
    fn non_subagent_tools_are_ignored_entirely() {
        let bash = tool_card("call_8", "bash", json!({"command": "ls"}), None);
        assert!(collect_tasks(&document(vec![Block::Tool(bash)])).is_empty());
    }

    #[test]
    fn the_default_configuration_has_no_transcript_path_and_still_merges_by_short_id() {
        // outputTranscript 默认是 false（内核 config-io.ts:65），而 outputFile 只在
        // nudge 那条路径的 details 里出现（tool-execution.ts:46-50）—— 也就是说
        // **默认配置下结果条目根本没有 outputFile**，agent_id 恒为 None。
        // 之前几处 fixture 全都塞了 outputFile，把这条真实默认路径漏成了零覆盖。
        let dispatch = tool_card(
            "call_a",
            AGENT_TOOL,
            json!({"prompt": "查一下", "run_in_background": true}),
            Some(json!({"type": "scout", "agentId": "abcdef0123456789a", "status": "running"})),
        );
        let result = decode_result_card(
            "[Subagent \"scout\" abcdef01 completed]\n\n查到了",
            // 没有 outputFile，也没有 agentId —— nudge 的 details 从不含 agentId。
            Some(&json!({"type": "scout", "status": "completed", "turnCount": 2})),
        );
        assert_eq!(result.agent_id, None, "默认配置下反推不出完整 agent id");
        assert_eq!(result.short_id.as_deref(), Some("abcdef01"));

        let tasks = collect_tasks(&document(vec![
            Block::Tool(dispatch),
            Block::Subagent(Box::new(result)),
        ]));
        assert_eq!(tasks.len(), 1, "必须靠短 id 归并成一条：{tasks:#?}");
        assert_eq!(tasks[0].agent_id.as_deref(), Some("abcdef0123456789a"));
        assert_eq!(tasks[0].status, SubagentStatus::Completed);
        assert_eq!(tasks[0].output_file, None);
    }

    #[test]
    fn a_background_dispatch_whose_details_were_released_is_not_reported_as_completed() {
        // 负载预算会把被裁消息的 details 剪掉，只留身份字段。后台派发的工具调用本身
        // 是 Success，照着工具状态判会把一个还在跑的子代理显示成已完成 —— 那正是
        // 「面板说完成了、其实还在跑」这类最难发现的错误。
        let dispatch = tool_card(
            "call_b",
            AGENT_TOOL,
            json!({"prompt": "长任务", "run_in_background": true}),
            // budget.rs 释放后保留下来的那一小撮身份字段。
            Some(json!({"agentId": "0011223344556677a", "status": "running"})),
        );
        let tasks = collect_tasks(&document(vec![Block::Tool(dispatch)]));
        assert_eq!(tasks[0].status, SubagentStatus::Running);

        // 连 status 都没了的极端情况：仍然不能凭工具 Success 判成完成。
        let stripped = tool_card(
            "call_c",
            AGENT_TOOL,
            json!({"prompt": "长任务", "run_in_background": true}),
            Some(json!({"agentId": "0011223344556677a"})),
        );
        let tasks = collect_tasks(&document(vec![Block::Tool(stripped)]));
        assert_eq!(
            tasks[0].status,
            SubagentStatus::Running,
            "后台任务缺 status 时应保持未结算，而不是被工具状态判成完成"
        );
    }

    #[test]
    fn force_background_is_recognized_from_details_rather_than_the_tool_argument() {
        // 用户打开 agent.forceBackground 后，模型不传 run_in_background 也会走后台
        // 分支；此时唯一可靠的标记是 details.agentId（内核只在后台分支塞它）。
        let card = tool_card(
            "call_d",
            AGENT_TOOL,
            json!({"prompt": "任务"}),
            Some(json!({"agentId": "aabbccddeeff00112", "status": "queued"})),
        );
        let tasks = collect_tasks(&document(vec![Block::Tool(card)]));
        assert!(tasks[0].background, "有 agentId 就必须按后台任务处理");
        assert_eq!(tasks[0].status, SubagentStatus::Queued);
    }

    #[test]
    fn stop_agent_leaves_everything_alone_when_the_prefix_is_ambiguous() {
        // 内核只按精确 id 中止一个；前缀撞车时我们无从知道是哪一个，
        // 宁可不改也不能一次把几条都标成已停止。
        let make = |call: &str, id: &str| {
            tool_card(
                call,
                AGENT_TOOL,
                json!({"prompt": "p", "run_in_background": true}),
                Some(json!({"agentId": id, "status": "running"})),
            )
        };
        let tasks = collect_tasks(&document(vec![
            Block::Tool(make("c1", "ab00000000000000a")),
            Block::Tool(make("c2", "ab11111111111111b")),
            Block::Tool(tool_card(
                "c3",
                STOP_AGENT_TOOL,
                json!({"agent_id": "ab"}),
                None,
            )),
        ]));
        assert_eq!(tasks.len(), 2);
        assert!(
            tasks
                .iter()
                .all(|task| task.status == SubagentStatus::Running),
            "前缀有歧义时不该改写任何一条：{tasks:#?}"
        );
    }
}
