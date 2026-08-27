//! 活会话事件的纯逻辑 reducer。
//!
//! RPC wire 类型刻意不泄漏到本 crate；app 只需把事件投影成这里的 `LiveEvent`。

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use serde_json::Value;

use crate::{
    Block, ConversationDocument, ConversationItem, Message, MessageRole, MinimapNode, ModelRef,
    NoticeBlock, RenderDiagnostic, ToolCard, ToolOutput, ToolStatus, parse_ansi,
    parse_unified_diff,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePhase {
    Idle,
    Running,
    Stopping,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveBlockKind {
    Text,
    Thinking,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveAssistantUpdate {
    Start,
    BlockStart {
        index: usize,
        kind: LiveBlockKind,
    },
    BlockDelta {
        index: usize,
        kind: LiveBlockKind,
        delta: String,
    },
    BlockEnd {
        index: usize,
        kind: LiveBlockKind,
        content: Value,
    },
    Done,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveEvent {
    AgentStart,
    AgentEnd,
    AgentSettled,
    MessageStart {
        message: Value,
    },
    MessageUpdate(LiveAssistantUpdate),
    MessageEnd {
        message: Value,
    },
    ToolExecutionStart {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolExecutionUpdate {
        id: String,
        name: String,
        arguments: Value,
        partial_result: Value,
    },
    ToolExecutionEnd {
        id: String,
        name: String,
        result: Value,
        is_error: bool,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    Diagnostic(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReduceOutcome {
    pub changed: bool,
    pub follow_tail: bool,
    pub settled: bool,
}

#[derive(Debug, Clone)]
struct DraftBlock {
    kind: LiveBlockKind,
    accumulated: String,
    complete: Option<Value>,
}

impl DraftBlock {
    fn new(kind: LiveBlockKind) -> Self {
        Self {
            kind,
            accumulated: String::new(),
            complete: None,
        }
    }
}

#[derive(Debug, Clone)]
struct DraftMessage {
    seed: Value,
    blocks: Vec<Option<DraftBlock>>,
}

impl DraftMessage {
    fn new(seed: Value) -> Self {
        Self {
            seed,
            blocks: Vec::new(),
        }
    }

    fn block_mut(&mut self, index: usize, kind: LiveBlockKind) -> &mut DraftBlock {
        if self.blocks.len() <= index {
            self.blocks.resize_with(index + 1, || None);
        }
        let block = self.blocks[index].get_or_insert_with(|| DraftBlock::new(kind));
        if block.kind != kind {
            *block = DraftBlock::new(kind);
        }
        block
    }

    fn snapshot(&self) -> Value {
        let mut message = self.seed.clone();
        let Value::Object(object) = &mut message else {
            message = serde_json::json!({ "role": "assistant" });
            return message;
        };
        object.insert("role".to_owned(), Value::String("assistant".to_owned()));
        let content = self
            .blocks
            .iter()
            .filter_map(|block| block.as_ref())
            .map(|block| {
                if let Some(complete) = &block.complete {
                    return match block.kind {
                        LiveBlockKind::Text => serde_json::json!({"type":"text","text":complete.as_str().unwrap_or_default()}),
                        LiveBlockKind::Thinking => serde_json::json!({"type":"thinking","thinking":complete.as_str().unwrap_or_default()}),
                        LiveBlockKind::ToolCall => complete.clone(),
                    };
                }
                match block.kind {
                    LiveBlockKind::Text => serde_json::json!({"type":"text","text":block.accumulated}),
                    LiveBlockKind::Thinking => serde_json::json!({"type":"thinking","thinking":block.accumulated}),
                    LiveBlockKind::ToolCall => serde_json::from_str(&block.accumulated).unwrap_or_else(|_| {
                        serde_json::json!({
                            "type":"toolCall",
                            "id":format!("streaming-tool-{}", block.accumulated.len()),
                            "name":"tool",
                            "arguments":{},
                            "streamingArguments":block.accumulated
                        })
                    }),
                }
            })
            .collect();
        object.insert("content".to_owned(), Value::Array(content));
        message
    }
}

#[derive(Debug, Clone)]
struct LiveTool {
    name: String,
    arguments: Value,
    result: Option<Value>,
    status: ToolStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MessageIdentity {
    Id(String),
    RunRoleContent {
        run: u64,
        role: String,
        content: String,
    },
}

#[derive(Debug, Clone)]
struct CompletedMessage {
    value: Value,
    rendered: Arc<Message>,
}

#[derive(Debug, Clone)]
pub struct LiveSessionReducer {
    session_id: String,
    source_path: PathBuf,
    history: Arc<ConversationDocument>,
    completed: Vec<CompletedMessage>,
    completed_indexes: HashMap<MessageIdentity, usize>,
    draft: Option<DraftMessage>,
    tools: HashMap<String, LiveTool>,
    phase: LivePhase,
    run_sequence: u64,
    steering: Vec<String>,
    follow_up: Vec<String>,
    diagnostics: Vec<RenderDiagnostic>,
    cached_messages: Arc<[Arc<Message>]>,
    cached_items: Arc<[ConversationItem]>,
    cached_minimap: Arc<[MinimapNode]>,
    cached_diagnostics: Arc<[RenderDiagnostic]>,
    /// 流式段的过程性负载预算。历史段在 `render_session` 里已经各自裁过一次，
    /// 这里只管本次会话新长出来的部分，因此不必对历史做 `Arc` 手术。
    payload_budget: usize,
    /// 流式段负载的估算值；`None` 表示"不确定，下次必须重算"。
    ///
    /// 它只是**快门**：常态下用它 O(1) 判断"离预算还远着呢"，真要动手时再走一遍
    /// 精确统计。没有它，每收一条完成消息都要 O(n) 扫一遍流式段。
    live_payload_hint: Option<usize>,
    /// 触发全量重扫的门槛，常态等于 `payload_budget`。
    ///
    /// 一趟扫完什么都没释放掉时会被抬高：剩下的负载释放不动，再每插一条消息就全量
    /// 重扫一遍纯属白干，长会话会退化成 O(n²)。
    enforce_threshold: usize,
    structure_dirty: bool,
    draft_dirty: bool,
    diagnostics_dirty: bool,
}

impl LiveSessionReducer {
    pub fn new(history: ConversationDocument) -> Self {
        let history = Arc::new(history);
        Self {
            session_id: history.session_id.clone(),
            source_path: history.source_path.clone(),
            cached_messages: history.messages.clone(),
            cached_items: history.items.clone(),
            cached_minimap: history.minimap.clone(),
            cached_diagnostics: history.diagnostics.clone(),
            history,
            completed: Vec::new(),
            completed_indexes: HashMap::new(),
            draft: None,
            tools: HashMap::new(),
            phase: LivePhase::Idle,
            run_sequence: 0,
            steering: Vec::new(),
            follow_up: Vec::new(),
            diagnostics: Vec::new(),
            payload_budget: crate::DEFAULT_PAYLOAD_BUDGET_BYTES,
            enforce_threshold: crate::DEFAULT_PAYLOAD_BUDGET_BYTES,
            live_payload_hint: Some(0),
            structure_dirty: false,
            draft_dirty: false,
            diagnostics_dirty: false,
        }
    }

    pub fn empty(session_id: impl Into<String>, source_path: impl Into<PathBuf>) -> Self {
        let session_id = session_id.into();
        let source_path = source_path.into();
        Self::new(ConversationDocument {
            session_id,
            source_path,
            cwd: PathBuf::new(),
            messages: Arc::from([]),
            items: Arc::from([]),
            minimap: Arc::from([]),
            diagnostics: Arc::from([]),
        })
    }

    /// 覆盖流式段的过程性负载预算（测试与调参用）。
    ///
    /// 立即按新预算裁一次，避免"改小了预算却要等下一条消息才生效"。
    pub fn set_payload_budget(&mut self, budget: usize) {
        self.payload_budget = budget;
        self.enforce_threshold = budget;
        self.live_payload_hint = None;
        self.enforce_payload_budget();
    }

    pub const fn payload_budget(&self) -> usize {
        self.payload_budget
    }

    /// 流式段**原始副本**当前占用的字节估算：`CompletedMessage.value` 与
    /// `tools[*].result` 里所有字符串长度之和。
    ///
    /// 存在的理由是可验收性 —— 渲染结果有界不等于内存有界。只裁 `rendered` 的话
    /// [`crate::payload_bytes`] 会一直报"在预算内"，而原始 JSON 与工具结果照样在涨。
    /// 这条是"原始副本也确实被裁了"的唯一客观判据。
    pub fn live_raw_bytes(&self) -> usize {
        let messages: usize = self
            .completed
            .iter()
            .map(|completed| json_string_bytes(&completed.value))
            .sum();
        let results: usize = self
            .tools
            .values()
            .filter_map(|tool| tool.result.as_ref())
            .map(json_string_bytes)
            .sum();
        messages.saturating_add(results)
    }

    pub const fn phase(&self) -> LivePhase {
        self.phase
    }

    pub fn set_running(&mut self) {
        self.phase = LivePhase::Running;
    }

    pub fn set_stopping(&mut self) {
        self.phase = LivePhase::Stopping;
    }

    pub fn restore_phase(&mut self, phase: LivePhase) {
        self.phase = phase;
    }

    pub fn restore_running_if_stopping(&mut self) -> bool {
        if self.phase != LivePhase::Stopping {
            return false;
        }
        self.phase = LivePhase::Running;
        true
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.phase = LivePhase::Error;
        self.push_diagnostic(message);
    }

    pub fn steering_queue(&self) -> &[String] {
        &self.steering
    }

    pub fn follow_up_queue(&self) -> &[String] {
        &self.follow_up
    }

    /// fresh RPC 启动后，`get_state` 才给出 pi 分配的真实身份。
    /// 只更新身份，不触碰已缓存的消息与流式草稿。
    pub fn set_session_identity(
        &mut self,
        session_id: impl Into<String>,
        source_path: impl Into<PathBuf>,
    ) {
        self.session_id = session_id.into();
        self.source_path = source_path.into();
    }

    /// 用 settled 后从持久文件重读的权威快照替换临时流式状态。
    pub fn calibrate(&mut self, history: ConversationDocument) {
        self.session_id = history.session_id.clone();
        self.source_path = history.source_path.clone();
        self.history = Arc::new(history);
        self.completed.clear();
        self.completed_indexes.clear();
        self.draft = None;
        self.tools.clear();
        self.diagnostics.clear();
        // 流式段被整段丢掉，负载账要跟着归零；留着旧值会让下一条消息白跑一次全量统计。
        // 触发线也必须复位 —— 上一段流若因释放不动被抬高过，留着它会让新的一段在
        // 远超预算之后才开始受管。
        self.live_payload_hint = Some(0);
        self.enforce_threshold = self.payload_budget;
        self.structure_dirty = true;
        self.draft_dirty = false;
        self.diagnostics_dirty = true;
    }

    pub fn apply_batch<I>(&mut self, events: I) -> ReduceOutcome
    where
        I: IntoIterator<Item = LiveEvent>,
    {
        let mut outcome = ReduceOutcome::default();
        for event in events {
            let next = self.apply(event);
            outcome.changed |= next.changed;
            outcome.follow_tail |= next.follow_tail;
            outcome.settled |= next.settled;
        }
        outcome
    }

    pub fn apply(&mut self, event: LiveEvent) -> ReduceOutcome {
        let mut outcome = ReduceOutcome {
            changed: true,
            follow_tail: false,
            settled: false,
        };
        match event {
            LiveEvent::AgentStart => {
                // dispatch 会乐观地把 phase 置为 Running，run 序号不能依赖 phase。
                self.run_sequence = self.run_sequence.wrapping_add(1);
                self.phase = LivePhase::Running;
            }
            // agent_end 之后仍可能 retry/compaction/queued continuation，不能提前 idle。
            LiveEvent::AgentEnd => {}
            LiveEvent::AgentSettled => {
                self.phase = LivePhase::Idle;
                // 活跃尾 turn 在 settled 后需要从展开态切换为已完成折叠态。
                self.structure_dirty = true;
                outcome.settled = true;
            }
            LiveEvent::MessageStart { message } => {
                outcome.follow_tail = true;
                match message.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        self.draft = Some(DraftMessage::new(message));
                        self.draft_dirty = true;
                    }
                    _ => self.upsert_completed(message),
                }
            }
            LiveEvent::MessageUpdate(update) => {
                outcome.follow_tail = true;
                self.apply_update(update);
                self.draft_dirty = true;
            }
            LiveEvent::MessageEnd { message } => {
                outcome.follow_tail = true;
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    self.draft = None;
                    self.draft_dirty = true;
                }
                self.upsert_completed(message);
            }
            LiveEvent::ToolExecutionStart {
                id,
                name,
                arguments,
            } => {
                outcome.follow_tail = true;
                self.tools.insert(
                    id,
                    LiveTool {
                        name,
                        arguments,
                        result: None,
                        status: ToolStatus::Pending,
                    },
                );
                self.rerender_completed_tools();
            }
            LiveEvent::ToolExecutionUpdate {
                id,
                name,
                arguments,
                partial_result,
            } => {
                outcome.follow_tail = true;
                let tool = self.tools.entry(id).or_insert_with(|| LiveTool {
                    name: name.clone(),
                    arguments: arguments.clone(),
                    result: None,
                    status: ToolStatus::Pending,
                });
                tool.name = name;
                tool.arguments = arguments;
                // partialResult 是累计值，必须替换而不是追加。
                tool.result = Some(partial_result);
                self.rerender_completed_tools();
            }
            LiveEvent::ToolExecutionEnd {
                id,
                name,
                result,
                is_error,
            } => {
                outcome.follow_tail = true;
                let tool = self.tools.entry(id).or_insert_with(|| LiveTool {
                    name: name.clone(),
                    arguments: Value::Null,
                    result: None,
                    status: ToolStatus::Pending,
                });
                tool.name = name;
                tool.result = Some(result);
                tool.status = if is_error {
                    ToolStatus::Error
                } else {
                    ToolStatus::Success
                };
                self.rerender_completed_tools();
            }
            LiveEvent::QueueUpdate {
                steering,
                follow_up,
            } => {
                // queue_update 是完整权威快照。
                self.steering = steering;
                self.follow_up = follow_up;
            }
            LiveEvent::Diagnostic(message) => self.push_diagnostic(message),
        }
        outcome
    }

    fn identity(&self, message: &Value) -> MessageIdentity {
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            MessageIdentity::Id(id.to_owned())
        } else {
            MessageIdentity::RunRoleContent {
                run: self.run_sequence,
                role: message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                // message_start/message_end 的字段可不同，但同一消息的可见 content 稳定；
                // 同一 run 的 steer/follow-up 文本不同，因此不会互相覆盖。
                content: canonical_message_content(message),
            }
        }
    }

    fn upsert_completed(&mut self, message: Value) {
        let identity = self.identity(&message);
        if let Some(index) = self.completed_indexes.get(&identity).copied() {
            if let Some(rendered) = render_live_message(&message, index as u64, &self.tools) {
                self.completed[index] = CompletedMessage {
                    value: message,
                    rendered: Arc::new(rendered),
                };
                self.structure_dirty = true;
                // 原地替换时旧负载的账没法只减不加地维护，直接作废快门重算。
                self.live_payload_hint = None;
                self.enforce_payload_budget();
            }
            return;
        }
        let index = self.completed.len();
        if let Some(rendered) = render_live_message(&message, index as u64, &self.tools) {
            self.completed.push(CompletedMessage {
                value: message,
                rendered: Arc::new(rendered),
            });
            self.completed_indexes.insert(identity, index);
            self.structure_dirty = true;
            // 快门必须与全量统计**同源**：手写一份"渲染负载 + 图片原始数据"的加法很容易
            // 漏项（工具结果常常在 `MessageEnd` 之前就到了，只算这条消息自身就会把它漏掉），
            // 于是快门显示"还没到预算"、全量统计其实早就超了。直接复用 `retained_bytes`
            // 就不存在两套算法对不上的可能。
            let added = self.retained_bytes(index);
            self.live_payload_hint = self
                .live_payload_hint
                .map(|hint| hint.saturating_add(added));
            self.enforce_payload_budget();
        }
    }

    /// 把流式段压回预算内。
    ///
    /// 预算算的是**实际驻留内存**：渲染结果的过程性负载 **加上**原始副本
    /// （`CompletedMessage.value` 与它引用的 `tools[*].result`）。只按渲染结果算是不够的 ——
    /// 无效、超限或已脱敏的图片渲染出来根本没有 `bytes`，[`crate::payload_bytes`] 一律
    /// 记 0，可那一大坨 base64 明明还完整躺在原始 JSON 里；只看渲染侧的话，一串这样的
    /// 消息能让预算永远触发不了，而内存一路涨上去。
    fn enforce_payload_budget(&mut self) {
        if self
            .live_payload_hint
            .is_some_and(|hint| hint <= self.enforce_threshold)
        {
            return;
        }
        let mut total: usize = self.total_retained_bytes();
        if total <= self.payload_budget {
            self.live_payload_hint = Some(total);
            return;
        }
        let mut changed = false;
        for index in 0..self.completed.len() {
            if total <= self.payload_budget {
                break;
            }
            let freed = self.release_message_at(index);
            if freed > 0 {
                changed = true;
                total = total.saturating_sub(freed);
            }
        }
        // 已完成的消息都裁过了还超，就轮到那些**没有对应完成消息**的工具结果：
        // 先到一步的、以及消息永远没来的孤儿。放在最后是因为它们可能马上就要被
        // 一条到来的 `MessageEnd` 用上；但真到了这一步，无界增长比丢一条尚未显示的
        // 输出更糟。
        if total > self.payload_budget {
            let orphans = self.orphan_tool_ids();
            for id in orphans {
                if total <= self.payload_budget {
                    break;
                }
                let freed = self.release_tool_result(&id);
                if freed > 0 {
                    changed = true;
                    total = total.saturating_sub(freed);
                }
            }
        }
        self.structure_dirty |= changed;
        let settled: usize = self.total_retained_bytes();
        self.live_payload_hint = Some(settled);
        // 全量扫过一遍仍然超预算，说明剩下的都是释放不动的（例如一整篇都是短到不值得
        // 换占位的输出）。这时把触发线抬到"再涨一个预算"，否则往后每插一条消息都要
        // 白扫一遍全段，长会话直接退化成 O(n²)。有东西被释放掉就说明还推得动，
        // 触发线复位。
        self.enforce_threshold = if changed || settled <= self.payload_budget {
            self.payload_budget
        } else {
            settled.saturating_add(self.payload_budget)
        };
    }

    /// 流式段实际占住的全部内存，**含没有对应完成消息的工具结果**。
    ///
    /// 只按完成消息累加是不够的：工具结果常常先于 `MessageEnd` 到达，消息也可能永远
    /// 不来（run 被取消）。那些结果一直挂在 `tools` 里，既不被计入也永远释放不掉，
    /// 足够让 reducer 在"预算通过"的同时一路涨上去。
    fn total_retained_bytes(&self) -> usize {
        let messages: usize = (0..self.completed.len())
            .map(|index| self.retained_bytes(index))
            .sum();
        let orphans: usize = self
            .orphan_tool_ids()
            .iter()
            .filter_map(|id| self.tools.get(id))
            .filter_map(|tool| tool.result.as_ref())
            .map(json_string_bytes)
            .sum();
        messages.saturating_add(orphans)
    }

    /// 目前没有被任何完成消息引用的工具 id。
    fn orphan_tool_ids(&self) -> Vec<String> {
        let referenced: HashSet<String> = self
            .completed
            .iter()
            .flat_map(|completed| tool_ids(&completed.value))
            .collect();
        self.tools
            .keys()
            .filter(|id| !referenced.contains(*id))
            .cloned()
            .collect()
    }

    /// 一条完成消息实际占住的内存：渲染结果的过程性负载 + 原始副本 + 它引用的工具结果。
    fn retained_bytes(&self, index: usize) -> usize {
        let Some(completed) = self.completed.get(index) else {
            return 0;
        };
        let mut total = crate::payload_bytes(&completed.rendered)
            .saturating_add(releasable_raw_bytes(&completed.value));
        for id in tool_ids(&completed.value) {
            if let Some(result) = self.tools.get(&id).and_then(|tool| tool.result.as_ref()) {
                total = total.saturating_add(json_string_bytes(result));
            }
        }
        total
    }

    /// 同时释放一条消息的渲染结果与原始副本，返回省下的字节数。
    ///
    /// 两侧必须一起裁：只裁渲染结果等于只把统计做小；只裁原始副本又会让下一次重渲染
    /// 把界面上还留着的内容换掉，看起来像是自己变了。
    fn release_message_at(&mut self, index: usize) -> usize {
        let Some(completed) = self.completed.get_mut(index) else {
            return 0;
        };
        let rendered_before = crate::payload_bytes(&completed.rendered);
        // 同一条大小判据也要在这里生效：`release_payload` 自己不判，直接调等于把
        // 一条 `"ok"` 换成一整句更长的占位 —— 有用的输出丢了，内存一个字节没省下，
        // 对应的原始结果那边还会因为"换了不划算"而拒绝替换，两头落空。
        if crate::budget::is_worth_releasing(&completed.rendered) {
            crate::budget::release_payload(Arc::make_mut(&mut completed.rendered));
        }
        let rendered_after = crate::payload_bytes(&completed.rendered);

        let raw_before = json_string_bytes(&completed.value);
        let mut ids = Vec::new();
        release_raw_message(&mut completed.value, &mut ids);
        let raw_after = json_string_bytes(&completed.value);

        let mut freed = rendered_before
            .saturating_sub(rendered_after)
            .saturating_add(raw_before.saturating_sub(raw_after));
        for id in ids {
            freed = freed.saturating_add(self.release_tool_result(&id));
        }
        freed
    }

    /// 把一条工具结果换成与 `rendered` 完全一致的占位，返回省下的字节数。
    fn release_tool_result(&mut self, id: &str) -> usize {
        let Some(tool) = self.tools.get_mut(id) else {
            return 0;
        };
        let Some(result) = tool.result.as_ref() else {
            return 0;
        };
        let before = json_string_bytes(result);
        let placeholder = Value::Object(serde_json::Map::from_iter([(
            "content".to_owned(),
            Value::String(crate::RELEASED_OUTPUT_NOTICE.to_owned()),
        )]));
        let after = json_string_bytes(&placeholder);
        // 比占位还短的结果留着：换掉它是净亏，也白丢了有用的输出。
        if before <= after {
            return 0;
        }
        tool.result = Some(placeholder);
        before.saturating_sub(after)
    }

    fn rerender_completed_tools(&mut self) {
        let mut changed = false;
        for (index, completed) in self.completed.iter_mut().enumerate() {
            if completed
                .rendered
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Tool(_)))
                && let Some(rendered) =
                    render_live_message(&completed.value, index as u64, &self.tools)
            {
                completed.rendered = Arc::new(rendered);
                changed = true;
            }
        }
        self.structure_dirty |= changed;
        self.draft_dirty |= self.draft.is_some();
        // 无论有没有卡片被重渲染，都要重新压一次预算。两条理由，缺一条都会漏：
        //
        // ① 重渲染是从 `value` 重新生成的，会把之前释放掉的负载原样带回来；
        // ② **结果先于 `MessageEnd` 到达、或消息因取消永远不来**时，压根没有卡片可
        //    重渲染，`changed` 恒为 false —— 只在 `changed` 时触发的话，这些孤儿结果
        //    既进不了 `live_payload_hint`、也永远等不到一次全量重算，可以一路涨上去。
        self.live_payload_hint = None;
        self.enforce_payload_budget();
    }

    fn push_diagnostic(&mut self, message: impl Into<String>) {
        self.diagnostics.push(RenderDiagnostic {
            entry_id: None,
            message: message.into(),
        });
        self.diagnostics_dirty = true;
    }

    fn apply_update(&mut self, update: LiveAssistantUpdate) {
        if self.draft.is_none() {
            self.push_diagnostic("message_update 早于 message_start；已创建兼容草稿");
            self.draft = Some(DraftMessage::new(serde_json::json!({"role":"assistant"})));
        }
        if let LiveAssistantUpdate::Error { message } = update {
            self.push_diagnostic(message);
            return;
        }
        let draft = self.draft.as_mut().expect("draft initialized");
        match update {
            LiveAssistantUpdate::Start | LiveAssistantUpdate::Done => {}
            LiveAssistantUpdate::Error { .. } => unreachable!(),
            LiveAssistantUpdate::BlockStart { index, kind } => {
                draft.block_mut(index, kind);
            }
            LiveAssistantUpdate::BlockDelta { index, kind, delta } => {
                draft.block_mut(index, kind).accumulated.push_str(&delta);
            }
            LiveAssistantUpdate::BlockEnd {
                index,
                kind,
                content,
            } => {
                let block = draft.block_mut(index, kind);
                block.complete = Some(content);
            }
        }
    }

    /// 刷新可共享的文档快照。已定稿历史以 Arc 缓存；每帧最多重渲染当前草稿。
    pub fn document(&mut self) -> ConversationDocument {
        let needs_messages = self.structure_dirty || self.draft_dirty;
        if needs_messages {
            let mut live_messages =
                Vec::with_capacity(self.completed.len() + usize::from(self.draft.is_some()));
            live_messages.extend(
                self.completed
                    .iter()
                    .map(|message| message.rendered.clone()),
            );
            if let Some(draft) = &self.draft
                && let Some(message) =
                    render_live_message(&draft.snapshot(), self.completed.len() as u64, &self.tools)
            {
                live_messages.push(Arc::new(message));
            }

            let active_tail = self.phase != LivePhase::Idle;
            // 历史文档已完成 written-files 与 turn 投影。流式帧只投影 live 段，
            // 再复用历史 item/message Arc；draft delta 不扫描或克隆整段历史。
            crate::attach_written_files(&mut live_messages, &self.history.cwd, active_tail);
            let (live_items, live_minimap) =
                crate::project_conversation(&live_messages, active_tail);
            let mut messages =
                Vec::with_capacity(self.history.messages.len() + live_messages.len());
            messages.extend(self.history.messages.iter().cloned());
            messages.extend(live_messages);
            let mut items = Vec::with_capacity(self.history.items.len() + live_items.len());
            items.extend(self.history.items.iter().cloned());
            items.extend(live_items);
            let turn_offset = self
                .history
                .minimap
                .iter()
                .map(|node| node.turn)
                .max()
                .unwrap_or(0);
            let mut minimap = Vec::with_capacity(self.history.minimap.len() + live_minimap.len());
            minimap.extend(self.history.minimap.iter().cloned());
            minimap.extend(live_minimap.into_iter().map(|mut node| {
                node.turn += turn_offset;
                node
            }));

            self.cached_items = items.into();
            self.cached_minimap = minimap.into();
            self.cached_messages = messages.into();
            self.structure_dirty = false;
            self.draft_dirty = false;
        }
        if self.diagnostics_dirty {
            let mut diagnostics =
                Vec::with_capacity(self.history.diagnostics.len() + self.diagnostics.len());
            diagnostics.extend(self.history.diagnostics.iter().cloned());
            diagnostics.extend(self.diagnostics.iter().cloned());
            self.cached_diagnostics = diagnostics.into();
            self.diagnostics_dirty = false;
        }
        ConversationDocument {
            session_id: self.session_id.clone(),
            source_path: self.source_path.clone(),
            cwd: self.history.cwd.clone(),
            messages: self.cached_messages.clone(),
            items: self.cached_items.clone(),
            minimap: self.cached_minimap.clone(),
            diagnostics: self.cached_diagnostics.clone(),
        }
    }
}

/// JSON 里全部字符串字面量的字节数之和。
///
/// 只数字符串：内嵌图片的 base64 与工具输出正文都在那儿，结构本身的开销可以忽略。
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

/// 一条原始消息如果是子代理结果，返回它那份字符串正文。
///
/// 判据用 `customType` 而不是正文形状：内核对这条消息硬编码
/// `customType: "subagent-result"` + `display: true`（`spawn-coordinator.ts`），
/// 比去猜 `[Subagent "..."]` 标题稳。
fn subagent_result_text(message: &Value) -> Option<&str> {
    (message.get("customType").and_then(Value::as_str) == Some(crate::SUBAGENT_RESULT_CUSTOM_TYPE))
        .then(|| message.get("content").and_then(Value::as_str))
        .flatten()
}

/// 一条原始消息里**可以被释放**的原始字节：内嵌图片数据。
///
/// 刻意不数用户 Query 与最终 Answer 的正文 —— 预算从来不会释放它们，把它们算进来
/// 只会让一篇长文本会话永远"超预算"，于是每插一条消息都全量重扫一遍却一个字节也
/// 释放不掉，白白退化成 O(n²)。工具结果不在这里数，由调用方按 id 单独累加。
fn releasable_raw_bytes(message: &Value) -> usize {
    // 子代理结果的 content 是**字符串**（内核 `spawn-coordinator.ts` 直接拼模板串），
    // 走不到下面按数组遍历的图片分支。不单独认它的话，`completed` 里那份原文既不计入
    // 预算、也永远释放不掉 —— 而后台派发是 R26 的主用例，一次长运行能攒下几十条。
    if let Some(text) = subagent_result_text(message) {
        return text.len();
    }
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return 0;
    };
    content
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("image"))
        .map(|item| {
            let inline = item.get("data").and_then(Value::as_str).map_or(0, str::len);
            let nested = item
                .get("source")
                .and_then(|source| source.get("data"))
                .and_then(Value::as_str)
                .map_or(0, str::len);
            inline + nested
        })
        .sum()
}

/// 一条原始消息引用到的全部工具 id。
fn tool_ids(message: &Value) -> Vec<String> {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("toolCall"))
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// 就地裁掉一条原始消息里的重负载，并收集它引用的工具 id。
///
/// 图片数据换成 `<redacted>`：`crate::parse_image` 本来就认这个标记，重渲染会稳定
/// 落到 [`crate::ImageState::Redacted`]，而不是把一段占位当成损坏的 base64。
fn release_raw_message(message: &mut Value, tool_ids: &mut Vec<String>) {
    if subagent_result_text(message).is_some_and(|text| text != crate::RELEASED_OUTPUT_NOTICE) {
        // 与工具输出同一口径：换成占位文案而不是删字段，重渲染时仍是一张结构完整的
        // 子代理卡片（类型、状态、统计都在 details 里），只是正文换成了说明。
        message["content"] = Value::String(crate::RELEASED_OUTPUT_NOTICE.to_owned());
        return;
    }
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("image") => release_raw_image(item),
            Some("toolCall") => {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    tool_ids.push(id.to_owned());
                }
            }
            _ => {}
        }
    }
}

fn release_raw_image(item: &mut Value) {
    for key in ["data"] {
        if item.get(key).and_then(Value::as_str).is_some() {
            item[key] = Value::String("<redacted>".to_owned());
        }
    }
    if let Some(source) = item.get_mut("source")
        && source.get("data").and_then(Value::as_str).is_some()
    {
        source["data"] = Value::String("<redacted>".to_owned());
    }
}

fn canonical_message_content(message: &Value) -> String {
    let content = message.get("content").unwrap_or(&Value::Null);
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("image") => canonical_image_identity(block),
                _ => block
                    .get("text")
                    .or_else(|| block.get("thinking"))
                    .and_then(Value::as_str)
                    .map_or_else(|| block.to_string(), str::to_owned),
            })
            .collect::<Vec<_>>()
            .join("\u{1f}"),
        other => other.to_string(),
    }
}

fn canonical_image_identity(value: &Value) -> String {
    let source = value.get("source");
    let mime = value
        .get("mimeType")
        .or_else(|| value.get("mime_type"))
        .or_else(|| source.and_then(|source| source.get("media_type")))
        .or_else(|| source.and_then(|source| source.get("mimeType")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = value
        .get("data")
        .or_else(|| source.and_then(|source| source.get("data")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!("image:{mime}:{}:{}", data.len(), stable_text_hash(data))
}

fn stable_text_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn render_live_message(
    value: &Value,
    sequence: u64,
    live_tools: &HashMap<String, LiveTool>,
) -> Option<Message> {
    let role = match value.get("role").and_then(Value::as_str) {
        Some("user") => MessageRole::User,
        Some("assistant") => MessageRole::Assistant,
        // 扩展经 `sendMessage` 注入的消息在 agent 层就是 `role: "custom"`，且带着
        // `customType` / `details`。R26 之前它掉进 Unknown 只当纯文本渲染。
        Some("custom") => MessageRole::Custom,
        Some(_) => MessageRole::Unknown,
        None => return None,
    };
    let custom_type = value.get("customType").and_then(Value::as_str);

    // 子代理结果：实时路径必须和从会话文件回看的结果**渲染成同一种块**。
    // 否则同一条结果在流式时是一段纯文本、重开会话后才变成子代理卡片，任务面板也要
    // 等到重新读盘才认得它 —— 正文和面板对不上，而且只在"后台子代理刚完成"这一小段
    // 时间窗里复现，是最难查的那类不一致。
    if custom_type == Some(crate::SUBAGENT_RESULT_CUSTOM_TYPE)
        && let Some(content) = value.get("content")
    {
        let text = crate::content_plain_text(content);
        let card = crate::decode_result_card(&text, value.get("details"));
        return Some(Message {
            id: value
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| format!("live-{sequence}"), ToOwned::to_owned),
            role,
            timestamp: None,
            label: Some(crate::SUBAGENT_RESULT_CUSTOM_TYPE.to_owned()),
            model: None,
            written_files: Vec::new(),
            blocks: vec![Block::Subagent(Box::new(card))],
        });
    }

    let mut blocks = Vec::new();
    match value.get("content") {
        Some(Value::String(text)) => blocks.push(Block::Markdown(crate::MarkdownBlock {
            source: text.clone(),
        })),
        Some(Value::Array(content)) => {
            for item in content {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => blocks.push(Block::Markdown(crate::MarkdownBlock {
                        source: item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    })),
                    Some("thinking") => blocks.push(Block::Thinking(
                        item.get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    )),
                    Some("toolCall") => {
                        blocks.push(Block::Tool(render_live_tool(item, live_tools)))
                    }
                    Some("image") => blocks.push(Block::Image(crate::parse_image(item))),
                    Some(kind) => blocks.push(Block::Unknown(crate::UnknownBlock {
                        kind: kind.to_owned(),
                        text: item.to_string(),
                    })),
                    None => {}
                }
            }
        }
        _ => {}
    }
    if blocks.is_empty() && role == MessageRole::Assistant {
        blocks.push(Block::Notice(NoticeBlock {
            title: "Assistant 正在响应".to_owned(),
            text: "等待流式内容".to_owned(),
        }));
    }
    Some(Message {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("live-{sequence}")),
        role,
        timestamp: value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned),
        label: None,
        model: live_model_ref(value),
        written_files: Vec::new(),
        blocks,
    })
}

fn live_model_ref(value: &Value) -> Option<ModelRef> {
    Some(ModelRef {
        provider: value.get("provider")?.as_str()?.to_owned(),
        id: value.get("model")?.as_str()?.to_owned(),
    })
}

fn render_live_tool(call: &Value, live_tools: &HashMap<String, LiveTool>) -> ToolCard {
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("streaming-tool")
        .to_owned();
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    let arguments = call.get("arguments").cloned().unwrap_or(Value::Null);
    let live = live_tools.get(&id);
    let arguments = live.map_or(arguments, |tool| tool.arguments.clone());
    let input_json =
        serde_json::to_string_pretty(&arguments).unwrap_or_else(|_| arguments.to_string());
    let mut output = Vec::new();
    let details = live
        .and_then(|tool| tool.result.as_ref())
        .and_then(|result| result.get("details"))
        .cloned();
    if let Some(result) = live.and_then(|tool| tool.result.as_ref()) {
        if let Some(patch) = result
            .get("details")
            .and_then(|details| details.get("patch").or_else(|| details.get("diff")))
            .and_then(Value::as_str)
        {
            output.push(ToolOutput::Diff(parse_unified_diff(patch)));
        }
        append_live_tool_content(result.get("content").unwrap_or(result), &name, &mut output);
    }
    ToolCard {
        id,
        name: live.map_or(name, |tool| tool.name.clone()),
        preview: input_json.chars().take(240).collect(),
        arguments,
        input_json,
        status: live.map_or(ToolStatus::Pending, |tool| tool.status),
        output,
        details,
        orphan: false,
    }
}

fn append_live_tool_content(content: &Value, name: &str, output: &mut Vec<ToolOutput>) {
    match content {
        Value::String(text) => {
            if name.eq_ignore_ascii_case("bash") || text.contains('\u{1b}') {
                output.push(ToolOutput::Ansi(parse_ansi(text)));
            } else {
                output.push(ToolOutput::Text(text.clone()));
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(text) = item.get("text").and_then(Value::as_str)
                {
                    append_live_tool_content(&Value::String(text.to_owned()), name, output);
                }
            }
        }
        Value::Null => {}
        other => output.push(ToolOutput::Text(other.to_string())),
    }
}
