//! 每 Session 的有界 effect 缓存。
//!
//! R21 把一次性结果放进一条**永不回收**的 `VecDeque`，`snapshot()` 每次还全量 clone，
//! 长会话下内存与 CPU 都随时长线性增长。R22 按语义把 effect 分成三类并给出固定字节上限：
//!
//! | 类别 | 成员 | 背压策略 |
//! |---|---|---|
//! | 可靠 | `RequestFinished` / `ControlFinished` / `ToolRestartFinished` / `Stopped` | 只在计数可见的溢出下淘汰；**运行时终态另存权威字段**，永不丢 |
//! | 可合并 | `Events` / `CommandsLoaded` / `ControlsLoaded` / `ExtensionUiBatch` / `ExtensionUiReset` | 同 key 取最新 |
//! | 尽力而为 | `Diagnostic` | 超限丢弃并计数 |
//!
//! 字节上限是**硬上限**：先剥离可重建的重负载（提交里的图片），再依次淘汰尽力而为、
//! 可合并、可靠条目，每一步都计数并经 `SessionSnapshot.backpressure` 暴露给 UI。
//! 任何一步都不会静默丢失 assistant 正文——正文由 reducer 的 `ConversationDocument`
//! 权威保存，effect 只承载「一次性结果」与「取最新」信号。

use std::collections::VecDeque;

use crate::{RuntimeEffect, RuntimeEffectKind, SessionRuntimeEvent};

/// effect 缓存的有界参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectLimits {
    /// 缓存内所有 effect 的字节硬上限。
    pub max_bytes: usize,
    /// 缓存内 effect 条数上限。
    pub max_effects: usize,
    /// 保留的诊断条数上限。
    pub max_diagnostics: usize,
}

impl Default for EffectLimits {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            max_effects: 512,
            max_diagnostics: 16,
        }
    }
}

impl EffectLimits {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            max_bytes: self.max_bytes.max(64 * 1024),
            max_effects: self.max_effects.max(8),
            max_diagnostics: self.max_diagnostics.min(self.max_effects.max(8)),
        }
    }
}

/// 背压统计，随每个 Snapshot 暴露给 UI。
///
/// 所有非零的淘汰计数都代表「用户可能少看到了一条结果或诊断」，UI 必须能显示，
/// 不允许把它们当作内部实现细节吞掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackpressureStats {
    /// 当前缓存占用字节数。
    pub buffered_bytes: usize,
    /// 当前缓存条数。
    pub buffered_effects: usize,
    /// 同 key 合并次数。
    pub coalesced: u64,
    /// 为满足字节上限而剥离的提交负载（图片/正文）条数。
    pub stripped_submissions: u64,
    /// 被淘汰的诊断条数。
    pub dropped_diagnostics: u64,
    /// 被淘汰的可合并 effect 条数。
    pub dropped_coalescable: u64,
    /// 被淘汰的一次性结果条数；非零意味着 UI 必须提示用户结果不完整。
    pub dropped_results: u64,
    /// 因命令队列已满而被拒绝的命令次数。
    pub rejected_commands: u64,
    /// 因队列已满而未能投递的内部作业（元数据刷新 / 落盘校准）次数。
    pub dropped_jobs: u64,
    /// 合帧时因同族取最新而被丢弃的运行时事件条数（compaction / retry / agent-end）。
    ///
    /// 这些不是「一次性结果」，但确实是用户可见信息，必须计数而不是无声消失。
    pub dropped_runtime_events: u64,
    /// 普通命令通道当前排队深度。
    pub queued_commands: usize,
    /// 控制通道当前排队深度。
    pub queued_controls: usize,
}

/// effect 的背压类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectClass {
    Reliable,
    Coalescable,
    BestEffort,
}

/// 可合并 effect 的 key。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectKey {
    /// 只与队尾相邻条目合并：`Events` 的顺序相对一次性结果有语义。
    Events,
    /// 同上：Extension UI 请求不能被重排到 reset 之前。
    ExtensionUi,
    /// 同上。
    ExtensionUiReset,
    /// 纯赋值型「取最新」，可跨条目淘汰旧值。
    CommandsLoaded,
    /// 同上。
    ControlsLoaded,
}

impl EffectKey {
    /// 是否允许跨越中间条目淘汰同 key 旧值。
    ///
    /// 只有「应用等价于一次赋值」的 effect 才安全：丢掉旧值不改变最终状态。
    const fn supersedes_across_entries(self) -> bool {
        matches!(self, Self::CommandsLoaded | Self::ControlsLoaded)
    }
}

struct Buffered {
    effect: RuntimeEffect,
    bytes: usize,
    class: EffectClass,
    key: Option<EffectKey>,
}

/// 有界 effect 缓存。
pub(crate) struct EffectBuffer {
    entries: VecDeque<Buffered>,
    bytes: usize,
    limits: EffectLimits,
    stats: BackpressureStats,
    /// 已经交给 UI 的最大 effect 序号。
    ///
    /// 合并**只能**并入 UI 还没看过的条目：一旦某条已经被应用，再往里塞新内容要么
    /// 重复应用（抬序号），要么永远送不出去（不抬序号），两条都是错的。
    delivered: u64,
}

impl EffectBuffer {
    pub(crate) fn new(limits: EffectLimits) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limits: limits.sanitized(),
            stats: BackpressureStats::default(),
            delivered: 0,
        }
    }

    pub(crate) fn push(&mut self, effect: RuntimeEffect) {
        let class = classify(&effect.kind);
        let key = key_of(&effect.kind);
        if let Some(key) = key
            && self.coalesce(key, &effect)
        {
            // 合并后的条目可能比原来更大（例如又并进一批 Extension UI 请求），
            // 因此合并路径同样要过一遍上限，否则字节硬上限在这条路上完全不生效。
            self.enforce_limits();
            return;
        }
        let bytes = kind_bytes(&effect.kind);
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.push_back(Buffered {
            effect,
            bytes,
            class,
            key,
        });
        self.enforce_limits();
    }

    /// 回收 UI 已消费的 effect。稳态下缓存因此保持接近空。
    pub(crate) fn ack(&mut self, epoch: u64, sequence: u64) {
        while let Some(front) = self.entries.front() {
            if front.effect.epoch > epoch
                || (front.effect.epoch == epoch && front.effect.sequence > sequence)
            {
                break;
            }
            let removed = self.entries.pop_front().expect("front exists");
            self.bytes = self.bytes.saturating_sub(removed.bytes);
        }
    }

    /// 取出当前缓存内容，并把「已交付水位」推进到本次交付的最大序号。
    pub(crate) fn snapshot(&mut self) -> Vec<RuntimeEffect> {
        if let Some(last) = self.entries.back() {
            self.delivered = self.delivered.max(last.effect.sequence);
        }
        self.entries
            .iter()
            .map(|buffered| buffered.effect.clone())
            .collect()
    }

    pub(crate) fn stats(&self) -> BackpressureStats {
        BackpressureStats {
            buffered_bytes: self.bytes,
            buffered_effects: self.entries.len(),
            ..self.stats
        }
    }

    pub(crate) fn record_rejected_command(&mut self) {
        self.stats.rejected_commands = self.stats.rejected_commands.saturating_add(1);
    }

    pub(crate) fn record_dropped_job(&mut self) {
        self.stats.dropped_jobs = self.stats.dropped_jobs.saturating_add(1);
    }

    /// 尝试把新 effect 合并进已有同 key 条目。返回是否已合并。
    fn coalesce(&mut self, key: EffectKey, effect: &RuntimeEffect) -> bool {
        if key.supersedes_across_entries() {
            // 纯赋值型：淘汰所有同 epoch 同 key 旧值，新值仍按新序号追加到队尾。
            let mut superseded = 0_u64;
            let mut retained = VecDeque::with_capacity(self.entries.len());
            for buffered in std::mem::take(&mut self.entries) {
                if buffered.key == Some(key) && buffered.effect.epoch == effect.epoch {
                    self.bytes = self.bytes.saturating_sub(buffered.bytes);
                    superseded += 1;
                    continue;
                }
                retained.push_back(buffered);
            }
            self.entries = retained;
            // 每淘汰一条旧值算一次合并，与队尾合并分支的计数口径保持一致。
            self.stats.coalesced = self.stats.coalesced.saturating_add(superseded);
            return false;
        }

        // 顺序敏感型：只与队尾相邻同 key 条目合并，杜绝任何重排。
        let delivered = self.delivered;
        let Some(tail) = self.entries.back_mut() else {
            return false;
        };
        if tail.key != Some(key) || tail.effect.epoch != effect.epoch {
            return false;
        }
        if tail.effect.sequence <= delivered {
            // 队尾已经交给 UI 应用过：再并进去只有两个结果 —— 抬序号会让 UI 把旧内容
            // 重复应用一遍，不抬序号则新内容永远越不过 cursor。两条都错，只能另起一条。
            return false;
        }
        let mut dropped_runtime_events = 0;
        if !merge_kind(
            &mut tail.effect.kind,
            &effect.kind,
            &mut dropped_runtime_events,
        ) {
            return false;
        }
        // 合并后沿用新序号，UI 的 effect cursor 才能单调推进。
        tail.effect.sequence = effect.sequence;
        self.bytes = self.bytes.saturating_sub(tail.bytes);
        tail.bytes = kind_bytes(&tail.effect.kind);
        self.bytes = self.bytes.saturating_add(tail.bytes);
        self.stats.coalesced = self.stats.coalesced.saturating_add(1);
        self.stats.dropped_runtime_events = self
            .stats
            .dropped_runtime_events
            .saturating_add(dropped_runtime_events);
        true
    }

    fn enforce_limits(&mut self) {
        self.trim_diagnostics();
        if self.within_limits() {
            return;
        }
        self.strip_payloads();
        self.evict(EffectClass::BestEffort);
        self.evict(EffectClass::Coalescable);
        self.evict(EffectClass::Reliable);
    }

    fn within_limits(&self) -> bool {
        self.bytes <= self.limits.max_bytes && self.entries.len() <= self.limits.max_effects
    }

    /// 诊断只保留最新若干条，不参与后续的字节淘汰阶梯。
    fn trim_diagnostics(&mut self) {
        let diagnostics = self
            .entries
            .iter()
            .filter(|buffered| buffered.class == EffectClass::BestEffort)
            .count();
        let mut excess = diagnostics.saturating_sub(self.limits.max_diagnostics);
        if excess == 0 {
            return;
        }
        let mut retained = VecDeque::with_capacity(self.entries.len());
        for buffered in std::mem::take(&mut self.entries) {
            if excess > 0 && buffered.class == EffectClass::BestEffort {
                excess -= 1;
                self.bytes = self.bytes.saturating_sub(buffered.bytes);
                self.stats.dropped_diagnostics = self.stats.dropped_diagnostics.saturating_add(1);
                continue;
            }
            retained.push_back(buffered);
        }
        self.entries = retained;
    }

    /// 剥离可重建的重负载：提交里的图片与正文只在「被拒绝」时用于恢复草稿，
    /// 丢掉它们仍然保留错误结果本身，比丢掉整条结果更可接受。
    fn strip_payloads(&mut self) {
        for buffered in self.entries.iter_mut() {
            if self.bytes <= self.limits.max_bytes {
                break;
            }
            let RuntimeEffectKind::RequestFinished { submission, .. } = &mut buffered.effect.kind
            else {
                continue;
            };
            if submission.take().is_none() {
                continue;
            }
            self.bytes = self.bytes.saturating_sub(buffered.bytes);
            buffered.bytes = kind_bytes(&buffered.effect.kind);
            self.bytes = self.bytes.saturating_add(buffered.bytes);
            self.stats.stripped_submissions = self.stats.stripped_submissions.saturating_add(1);
        }
    }

    /// 从最旧开始淘汰指定类别，直到重回上限之内。
    fn evict(&mut self, class: EffectClass) {
        while !self.within_limits() {
            let Some(index) = self
                .entries
                .iter()
                .position(|buffered| buffered.class == class)
            else {
                return;
            };
            let removed = self.entries.remove(index).expect("index from position");
            self.bytes = self.bytes.saturating_sub(removed.bytes);
            match class {
                EffectClass::BestEffort => {
                    self.stats.dropped_diagnostics =
                        self.stats.dropped_diagnostics.saturating_add(1)
                }
                EffectClass::Coalescable => {
                    self.stats.dropped_coalescable =
                        self.stats.dropped_coalescable.saturating_add(1)
                }
                EffectClass::Reliable => {
                    self.stats.dropped_results = self.stats.dropped_results.saturating_add(1)
                }
            }
        }
    }
}

const fn classify(kind: &RuntimeEffectKind) -> EffectClass {
    match kind {
        RuntimeEffectKind::RequestFinished { .. }
        | RuntimeEffectKind::ControlFinished { .. }
        | RuntimeEffectKind::ToolRestartFinished { .. }
        | RuntimeEffectKind::Stopped(_) => EffectClass::Reliable,
        RuntimeEffectKind::Events { .. }
        | RuntimeEffectKind::ExtensionUiBatch { .. }
        | RuntimeEffectKind::ExtensionUiReset
        | RuntimeEffectKind::CommandsLoaded(_)
        | RuntimeEffectKind::ControlsLoaded(_) => EffectClass::Coalescable,
        RuntimeEffectKind::Diagnostic(_) => EffectClass::BestEffort,
    }
}

const fn key_of(kind: &RuntimeEffectKind) -> Option<EffectKey> {
    match kind {
        RuntimeEffectKind::Events { .. } => Some(EffectKey::Events),
        RuntimeEffectKind::ExtensionUiBatch { .. } => Some(EffectKey::ExtensionUi),
        RuntimeEffectKind::ExtensionUiReset => Some(EffectKey::ExtensionUiReset),
        RuntimeEffectKind::CommandsLoaded(_) => Some(EffectKey::CommandsLoaded),
        RuntimeEffectKind::ControlsLoaded(_) => Some(EffectKey::ControlsLoaded),
        _ => None,
    }
}

/// 把 `incoming` 合并进 `target`；两者必须是同 key。
///
/// `dropped_runtime_events` 累加因同族取最新而被丢弃的运行时事件条数。
fn merge_kind(
    target: &mut RuntimeEffectKind,
    incoming: &RuntimeEffectKind,
    dropped_runtime_events: &mut u64,
) -> bool {
    match (target, incoming) {
        (
            RuntimeEffectKind::Events {
                follow_tail,
                settled,
                runtime_events,
            },
            RuntimeEffectKind::Events {
                follow_tail: next_follow_tail,
                settled: next_settled,
                runtime_events: next_runtime_events,
            },
        ) => {
            *follow_tail |= *next_follow_tail;
            *settled |= *next_settled;
            runtime_events.extend(next_runtime_events.iter().cloned());
            let before = runtime_events.len();
            keep_latest_per_family(runtime_events);
            *dropped_runtime_events =
                dropped_runtime_events.saturating_add((before - runtime_events.len()) as u64);
            true
        }
        (
            RuntimeEffectKind::ExtensionUiBatch { requests },
            RuntimeEffectKind::ExtensionUiBatch {
                requests: incoming_requests,
            },
        ) => {
            let mut merged = std::mem::take(requests);
            merged.extend(incoming_requests.iter().cloned());
            *requests = fold_extension_requests(merged);
            true
        }
        (RuntimeEffectKind::ExtensionUiReset, RuntimeEffectKind::ExtensionUiReset) => true,
        _ => false,
    }
}

/// 跨帧折叠 Extension UI 请求。
///
/// 必须和 pump 内单帧合并用同一口径：`setStatus` / `setWidget` 按 `statusKey` /
/// `widgetKey` 折叠，其余按请求 id 折叠，两级都保留**首次出现的位置**、取最新的值。
/// 只按 id 去重是不够的 —— pi 每次调用都生成新 id，同一个状态栏 key 会无限堆积。
fn fold_extension_requests(
    requests: Vec<(String, pi_rpc::ExtensionUiRequest)>,
) -> Vec<(String, pi_rpc::ExtensionUiRequest)> {
    let folded = crate::coalesce_extension_ui_requests(requests);
    let mut result = Vec::with_capacity(folded.len());
    let mut index_by_id = std::collections::HashMap::<String, usize>::new();
    for (id, request) in folded {
        if let Some(index) = index_by_id.get(&id).copied() {
            result[index] = (id, request);
        } else {
            index_by_id.insert(id.clone(), result.len());
            result.push((id, request));
        }
    }
    result
}

/// 每个运行时事件族只保留最新一条（compaction / retry / agent-end）。
fn keep_latest_per_family(events: &mut Vec<SessionRuntimeEvent>) {
    let mut seen = Vec::new();
    let mut kept = Vec::with_capacity(events.len());
    for event in events.drain(..).rev() {
        let family = runtime_event_family(&event);
        if seen.contains(&family) {
            continue;
        }
        seen.push(family);
        kept.push(event);
    }
    kept.reverse();
    *events = kept;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeEventFamily {
    Compaction,
    Retry,
    AgentEnded,
}

const fn runtime_event_family(event: &SessionRuntimeEvent) -> RuntimeEventFamily {
    match event {
        SessionRuntimeEvent::CompactionStarted | SessionRuntimeEvent::CompactionEnded { .. } => {
            RuntimeEventFamily::Compaction
        }
        SessionRuntimeEvent::RetryStarted { .. } | SessionRuntimeEvent::RetryEnded { .. } => {
            RuntimeEventFamily::Retry
        }
        SessionRuntimeEvent::AgentEnded { .. } => RuntimeEventFamily::AgentEnded,
    }
}

/// 单条 effect 的记账字节数。
///
/// 只需要是**保守上界**：高估只会让缓存更早开始回收，低估才会破坏字节上限。
fn kind_bytes(kind: &RuntimeEffectKind) -> usize {
    const ENTRY_OVERHEAD: usize = 64;
    let payload = match kind {
        RuntimeEffectKind::Events { runtime_events, .. } => runtime_events
            .iter()
            .map(runtime_event_bytes)
            .sum::<usize>(),
        RuntimeEffectKind::ExtensionUiBatch { requests } => requests
            .iter()
            .map(|(id, request)| id.len() + extension_request_bytes(request) + ENTRY_OVERHEAD)
            .sum(),
        RuntimeEffectKind::ExtensionUiReset => 0,
        RuntimeEffectKind::RequestFinished {
            submission, result, ..
        } => {
            submission
                .as_ref()
                .map(|submission| {
                    submission.message.len()
                        + submission
                            .images
                            .iter()
                            .map(|image| image.data.len() + image.mime_type.len())
                            .sum::<usize>()
                })
                .unwrap_or(0)
                + result
                    .as_ref()
                    .err()
                    .map(|(_, error)| error.len())
                    .unwrap_or(0)
        }
        RuntimeEffectKind::CommandsLoaded(result) => match result {
            Ok(commands) => debug_bytes(commands),
            Err(error) => error.len(),
        },
        RuntimeEffectKind::ControlsLoaded(result) => match result {
            Ok(controls) => controls_bytes(controls),
            Err(error) => error.len(),
        },
        RuntimeEffectKind::ControlFinished { result, .. } => match result {
            Ok(outcome) => control_outcome_bytes(outcome),
            Err(error) => error.len(),
        },
        RuntimeEffectKind::ToolRestartFinished { result, .. } => {
            result.as_ref().err().map(String::len).unwrap_or(0)
        }
        RuntimeEffectKind::Diagnostic(message) => message.len(),
        RuntimeEffectKind::Stopped(reason) => reason.as_ref().map(String::len).unwrap_or(0),
    };
    payload.saturating_add(ENTRY_OVERHEAD)
}

const fn runtime_event_bytes(event: &SessionRuntimeEvent) -> usize {
    match event {
        SessionRuntimeEvent::RetryStarted { error, .. } => 64 + error.len(),
        SessionRuntimeEvent::RetryEnded { error, .. } => match error {
            Some(error) => 64 + error.len(),
            None => 64,
        },
        SessionRuntimeEvent::CompactionEnded { error } => match error {
            Some(error) => 64 + error.len(),
            None => 64,
        },
        SessionRuntimeEvent::CompactionStarted | SessionRuntimeEvent::AgentEnded { .. } => 64,
    }
}

fn extension_request_bytes(request: &pi_rpc::ExtensionUiRequest) -> usize {
    use pi_rpc::ExtensionUiRequest as Request;
    match request {
        Request::Select { title, options, .. } => {
            title.len() + options.iter().map(String::len).sum::<usize>()
        }
        Request::Confirm { title, message, .. } => title.len() + message.len(),
        Request::Input {
            title, placeholder, ..
        } => title.len() + placeholder.as_ref().map(String::len).unwrap_or(0),
        Request::Editor { title, prefill } => {
            title.len() + prefill.as_ref().map(String::len).unwrap_or(0)
        }
        Request::Notify { message, .. } => message.len(),
        Request::SetStatus {
            status_key,
            status_text,
        } => status_key.len() + status_text.as_ref().map(String::len).unwrap_or(0),
        Request::SetWidget {
            widget_key,
            widget_lines,
            ..
        } => {
            widget_key.len()
                + widget_lines
                    .as_ref()
                    .map(|lines| lines.iter().map(String::len).sum::<usize>())
                    .unwrap_or(0)
        }
        Request::SetTitle { title } => title.len(),
        Request::SetEditorText { text } => text.len(),
    }
}

fn controls_bytes(controls: &crate::SessionControls) -> usize {
    debug_bytes(&controls.models)
        + debug_bytes(&controls.thinking_levels)
        + debug_bytes(&controls.tree)
        + controls.model.as_ref().map(debug_bytes).unwrap_or(0)
        + controls
            .session_file
            .as_ref()
            .map(|path| path.as_os_str().len())
            .unwrap_or(0)
        + controls.session_id.len()
}

fn control_outcome_bytes(outcome: &crate::ControlOutcome) -> usize {
    use crate::ControlOutcome as Outcome;
    match outcome {
        Outcome::Controls(controls) | Outcome::Switched(controls) => controls_bytes(controls),
        Outcome::Compacted(result) => debug_bytes(result),
        Outcome::Forked { data, controls } => debug_bytes(data) + controls_bytes(controls),
        Outcome::ForkCancelled(data) => debug_bytes(data),
        Outcome::Cloned { data, controls } => debug_bytes(data) + controls_bytes(controls),
        Outcome::RebindCalibrationFailed {
            message, fork_data, ..
        } => message.len() + fork_data.as_ref().map(debug_bytes).unwrap_or(0),
        Outcome::Exported(path) => debug_bytes(path),
        Outcome::CloneCancelled | Outcome::SwitchCancelled | Outcome::RetryAborted => 0,
    }
}

/// 用 `Debug` 渲染长度作为协议负载的**保守上界**。
///
/// 这里不需要精确大小，只需要「绝不低估」：`Debug` 会连字段名、引号和结构一起打印，
/// 长度必然大于其中所有字符串的字节和。选它而不是 JSON 序列化，是为了不让 `pi-runtime`
/// 为了记账新增一条 `serde` 直接依赖（会改动 `Cargo.lock`）。
/// 这些负载都是 latest-only 或受控制通道深度约束的稀有 effect，格式化开销可接受。
fn debug_bytes<T: std::fmt::Debug>(value: &T) -> usize {
    format!("{value:?}").len()
}
