# Code Context

## Files Retrieved

1. `crates/app/src/panels.rs` (lines 43-108) - `ChatPanel` 当前字段全集；`active` 与大量会话态平铺在同一面板。
2. `crates/app/src/panels.rs` (lines 419-508) - generation 切换、历史会话加载、旧活会话 shutdown 与会话态清空。
3. `crates/app/src/panels.rs` (lines 520-610) - fresh Session 创建、`ActiveSession` 安装、pump 接线和完整重置序列。
4. `crates/app/src/panels.rs` (lines 623-686) - 历史会话启动与 GPUI 侧 event pump 消费循环。
5. `crates/app/src/panels.rs` (lines 688-949) - `PumpMessage` 总入口；stale generation、工具重启、reducer、controls、校准、Stopped 的生命周期核心。
6. `crates/app/src/panels.rs` (lines 1439-1577) - compaction/retry runtime event、control outcome、fork/clone/switch rebind 与 controls 投影。
7. `crates/app/src/panels.rs` (lines 1600-1722) - model/thinking/tool restart/control command 调用面；工具切换会取走并替换整个 `ActiveSession`。
8. `crates/app/src/panels.rs` (lines 1725-1742) - auto-compaction、auto-retry、abort-retry 控制入口。
9. `crates/app/src/panels.rs` (lines 2033-2071, 2192-2204) - prompt/steer/follow-up/abort 到 `ActiveSession::dispatch` 的调用链。
10. `crates/app/src/panels.rs` (lines 2437-2442) - `ChatPanel::drop` 当前直接决定 RPC 进程 shutdown。
11. `crates/app/src/panels.rs` (lines 2456-2630) - render 对 document、phase、running/stopping、reducer queue、滚动/展开态的直接读取。
12. `crates/app/src/panels.rs` (lines 2701-2708, 2874-2908, 3008-3052) - compaction/retry 状态和控制开关的 UI 投影。
13. `crates/app/src/panels.rs` (lines 5495-5523, 5919-5952, 6006-6067) - 现有 restart stale/failure、restart path、rebind session-list、控制门禁回归测试。
14. `crates/app/src/live_session.rs` (lines 48-382) - `ExtensionUiState` 的队列、去重、限额、取消与 lifecycle reset 状态机。
15. `crates/app/src/live_session.rs` (lines 576-753) - `SessionControls`、control/runtime event、`PumpMessage` 协议。
16. `crates/app/src/live_session.rs` (lines 755-1110) - `ActiveSession` 当前同时拥有 raw `Client`、`LiveSessionReducer`、无界 pump sender、校准路径及 spawn/dispatch/control/restart/shutdown。
17. `crates/app/src/live_session.rs` (lines 1140-1454) - controls 查询、compact/retry/rebind/export 命令实现及 RPC config。
18. `crates/app/src/live_session.rs` (lines 1457-1491) - 历史 HTML 导出直接 `Client::spawn` 的第二个生产创建入口。
19. `crates/app/src/live_session.rs` (lines 1502-1730) - RPC event thread、20ms/512 条 batch、Extension UI reset/batch、runtime event 投影与 settled 校准线程。
20. `crates/app/src/session_sidebar.rs` (lines 24-28, 651-656) - 历史 HTML 导出的 UI 调用点。
21. `crates/pi-rpc/src/process.rs` (lines 32-58, 546-558) - `ClientConfig.max_restarts` 默认 3，底层 supervisor 的自动重启限制。
22. `Cargo.toml` (lines 1-10, 15-20) - 当前 workspace 尚无 `pi-runtime` member/dependency。
23. `crates/app/Cargo.toml` (lines 15-29) - app 当前直接依赖 `pi-rpc`，尚无 `pi-runtime`。
24. `docs/立项文档.md` (lines 118-132) - Manager 唯一创建入口、轻量 `SessionHandle`、maintenance 独立配额和重启决策权。
25. `docs/立项文档.md` (lines 309-315) - R21/R24 边界；R21 单实例收敛，R24 才多实例化。

> 仓库中目前不存在 `rounds/round-21/round-21.md`；勘察仅以权威立项文档的 R21 条目为范围基线。

## Key Code

### 1. 当前 seam：`ChatPanel` 同时拥有 Runtime 和 UI 会话态

`crates/app/src/panels.rs:43-98`：

```rust
pub struct ChatPanel {
    status: ChatStatus,
    load_generation: u64,
    active_generation: u64,
    active: Option<ActiveSession>,
    // composer/draft/attachments...
    controls: Option<SessionControls>,
    tool_preset: ToolPreset,
    control_operation: Option<ControlOperation>,
    branch_tree: Option<SessionBranchTree>,
    retry_status: Option<RetryStatus>,
    compacting: bool,
    // list/scroll/expanded...
    activity_generation: u64,
    calibration_generation: u64,
    extension_ui: ExtensionUiState,
    // extension dialog/sender/title...
    fresh_session: bool,
}
```

这里有三类职责混在一起：

- Runtime ownership：`active`、`active_generation`、spawn/pump/shutdown。
- 会话 UI 投影：document/status、controls、branch、retry/compaction、Extension UI、draft/attachments、scroll/expanded。
- 面板/窗口暂态：focus、bounds、popup、GPUI subscriptions、dialog focus handles。

### 2. `ActiveSession` 是 R21 必须拆走的 app 内 Runtime 容器

`crates/app/src/live_session.rs:768-776`：

```rust
pub struct ActiveSession {
    generation: u64,
    client: Client,
    reducer: LiveSessionReducer,
    pump: UnboundedSender<PumpMessage>,
    calibration_path: Arc<Mutex<Option<PathBuf>>>,
    startup_diagnostic: Option<String>,
    agent_dir: Option<PathBuf>,
}
```

它暴露或间接暴露了 R21 设计明确不允许 `SessionHandle` 暴露的三样东西：raw `Client`（`client()`）、可变 reducer（`reducer_mut()`）、app 专用无界 `PumpMessage` receiver/sender。最小迁移不应把 `ActiveSession` 改名成 `SessionHandle`，而应把它的生产职责和 reducer/event ownership 下沉到 `pi-runtime`。

### 3. 当前调用链

#### 创建 fresh Session

`ChatPanel::start_new_session` (`panels.rs:520-610`)
→ `ActiveSession::spawn_fresh` (`live_session.rs:786-817`)
→ `spawn_with_pump` (`live_session.rs:870-913`)
→ `Client::spawn`
→ `Client::subscribe`
→ `spawn_event_pump`
→ 返回 `(ActiveSession, UnboundedReceiver<PumpMessage>)`
→ `ChatPanel.active = Some(active)`
→ `ChatPanel::spawn_pump`。

#### 启动历史 Session

`ChatPanel::start_live` (`panels.rs:623-657`)
→ `ActiveSession::spawn` (`live_session.rs:819-837`)
→ 同上。

#### 事件与 reducer

RPC reader
→ `spawn_event_pump` (`live_session.rs:1502-1604`)
→ `project_pump_event` (`live_session.rs:1606-1684`)
→ `PumpMessage::{Events, ExtensionUiBatch, ExtensionUiReset, Stopped, Calibrated}`
→ GPUI async `ChatPanel::spawn_pump` (`panels.rs:660-686`)
→ `ChatPanel::handle_pump` (`panels.rs:688-949`)
→ `active.reducer_mut().apply_batch(events)`
→ `active.document()`
→ `ChatPanel.status = ChatStatus::Ready(document)`。

#### 发送与停止

Composer submit (`panels.rs:2033-2071`)
→ `ActiveSession::dispatch` (`live_session.rs:952-1014`)
→ 立即修改 reducer phase
→ 每个 request 新建 OS thread
→ `Client::request`
→ `PumpMessage::RequestFinished`。

Abort (`panels.rs:2192-2204`) 走同一 dispatch。

#### tool restart

`ChatPanel::set_tool_preset` (`panels.rs:1658-1700`)
→ `self.active.take()`
→ idle/history/path 校验
→ `begin_active_generation()`
→ `ActiveSession::restart_with_tools` (`live_session.rs:1045-1091`)
→ shutdown 旧 client
→ 使用原 pump sender `spawn_with_pump` 新 client
→ `ToolRestartFinished { generation, active }`
→ `handle_pump` 安装新 `active`、重建 Extension response sender、刷新 metadata (`panels.rs:715-743`)。

#### fork/clone/switch rebind

UI action
→ `begin_control` (`panels.rs:1705-1722`)
→ `ActiveSession::request_control`
→ `execute_control` (`live_session.rs:1187-1370`)
→ `request_session_rebind_data`
→ calibrated `RpcSessionState`
→ `load_controls_from_state`
→ `ControlFinished`
→ `apply_control_outcome` (`panels.rs:1481-1547`)
→ `apply_session_rebind` (`panels.rs:1549-1564`)
→ 从新 path 同步 `render_path`
→ `active.calibrate(document)` + 更新 draft key/branch/controls/status。

#### compaction/retry

RPC event
→ `project_pump_event` 映射为 `SessionRuntimeEvent` (`live_session.rs:1620-1659`)
→ `PumpMessage::Events.runtime_events`
→ `ChatPanel::apply_runtime_events` (`panels.rs:1439-1475`)
→ `compacting` / `retry_status` / `rpc_error`。

手动 compact/control
→ `execute_control`
→ `ControlOutcome::Compacted/RetryAborted`
→ `apply_control_outcome`
→ 刷新 metadata 或清 retry。

#### Extension UI

`RpcEvent::ExtensionUiRequest`
→ event pump 独立收集并同 key 合并 status/widget (`live_session.rs:1562-1577, 1686-1710`)
→ `ExtensionUiBatch`
→ `ExtensionUiState::apply`
→ `ChatPanel::process_extension_ui` 投影 window/dialog/editor/title并通过 `ClientExtensionResponseSender` 写回。

Lifecycle `Restarted`
→ `ExtensionUiReset` (`live_session.rs:1672-1676`)
→ 取消/清空旧 dialog，防止 response 发给错误 client。

## 状态字段分类

### A. 应进入唯一 `SessionUiState` 的会话级字段

建议 R21 只做单实例 `session_ui: SessionUiState`，不引入 `HashMap<RuntimeId, _>`：

1. **会话内容与身份投影**
   - `status`
   - `fresh_session`
   - `draft_key`
   - `composer_cwd`
   - `activity_generation`
   - `calibration_generation`
   - `controls`
   - `model_names`
   - `slash_commands`

2. **草稿与 composer 会话内容**
   - `composer_mode`
   - `attachments`
   - `pending_draft_restore`
   - 注意：`composer: Entity<TextareaState>` 是 GPUI widget，本轮可继续留在 panel；其值与 draft key 的同步规则归 `SessionUiState` 方法管理。`DraftStore` 可留作 panel/app 级服务，当前会话 key/attachments 属于 state。

3. **分支/rebind 状态**
   - `branch_tree`
   - `branch_preview_leaf`
   - `branch_preview_document`
   - rebind 后 draft key、document、controls 的原子更新应收敛为 `SessionUiState::apply_rebind(...)`。

4. **运行投影与控制 busy**
   - `control_operation`
   - `retry_status`
   - `compacting`
   - `tool_preset`
   - `rpc_success`
   - `rpc_error`
   - `host_extension_degradation`

5. **Extension UI（必须整组迁移，不能只移 `extension_ui`）**
   - `extension_ui`
   - `extension_dialog_open`
   - `extension_dialog_needs_close`
   - `pending_extension_responses`
   - `extension_response_sender` 应删除，改由 `SessionHandle::respond_extension_ui`；若为最小过渡，也必须绑定 handle epoch，不能继续包装 raw Client。
   - `window_title`
   - `next_extension_element_id`
   - `extension_dialog_body_focus/footer_focus` 是窗口 widget handle，可留 panel，但其“当前 dialog id/需关闭”属于 session state。
   - widget ScrollHandle 可留 panel；widget/status/dialog 队列属于 session state。

6. **每会话阅读/布局偏好（为 R24 预先正确归组）**
   - `list_state`
   - `list_items`
   - `tail_attached`
   - `follow_requested`
   - `minimap_visible`
   - `expanded_tools`
   - `expanded_processes`
   - `workspace_bounds`、`message_pane_bounds` 更偏当前 panel geometry；R21 可留 panel，R24 若每 session 恢复 viewport 再迁移。

7. **会话相关异步代际**
   - `load_generation` 可留 panel（历史文件 load task 防 stale）。
   - `active_generation` 不应继续作为 runtime identity；替换为 `RuntimeId + epoch + snapshot revision`。
   - file index/popup 的 generation 仍可沿用 `load_generation`，它不是 Runtime epoch。

### B. 应留在 `ChatPanel` 的面板级字段

- `focus_handle`
- `composer: Entity<TextareaState>`
- `_composer_subscription`
- `drafts: DraftStore`（持久化服务）
- `popup` / `popup_index`（可争议，但当前紧耦合 widget 输入）
- `file_index`
- `workspace_bounds` / `message_pane_bounds`
- Extension dialog focus handles、widget scroll handles
- `probe`

### C. 应从 app 移入 `pi-runtime` 的字段/职责

- `ActiveSession.client`
- `LiveSessionReducer`
- pump sender/receiver 与 event thread
- calibration path 与 settled 后 render/calibrate 调度
- startup diagnostic
- commands/controls request thread
- dispatch/control/restart/shutdown
- generation stale filtering的 runtime 部分
- Client config 创建；强制 `max_restarts = 0`
- historical HTML maintenance spawn。

## Architecture

### 当前架构

`ChatPanel` 是事实上的 Session controller：它创建/销毁 `Client`，持有 reducer，消费内部 pump，更新 GPUI 状态，并决定工具重启及 rebind 后校准。`ActiveSession` 只是 app 内的进程 facade，不是稳定 handle。`active_generation` 同时承担 UI 绑定代际、进程代际和 stale-message fence，边界过宽。

### R21 最小目标架构

```text
App-level RuntimeManager (shared, no GPUI)
  ├─ owns RuntimeEntry { RuntimeId, epoch, Client, reducer, snapshot, revision }
  ├─ creates all user/maintenance RPC clients, config.max_restarts = 0
  ├─ reduces RPC events and publishes Dirty { runtime_id, epoch, revision }
  ├─ exposes SessionHandle (cloneable lightweight capability)
  └─ handles restart/rebind/stop and Extension UI responses

ChatPanel
  ├─ session: Option<SessionHandle>
  ├─ session_ui: SessionUiState     // exactly one in R21
  ├─ subscribes to Dirty notifications
  └─ pulls immutable Snapshot when revision advances
```

`SessionHandle` 最小 API 建议只暴露：

- stable `runtime_id()`；
- `snapshot() -> Arc<SessionSnapshot>`（含 epoch/revision/document/phase/queues/controls/runtime status/Extension UI deltas或状态）；
- `dispatch(...)`、`request_control(...)`、`restart_with_tools(...)`、`respond_extension_ui(...)`；
- `refresh_metadata()`；
- UI binding subscription/dirty receiver（有界或 GPUI callback；不要暴露现有 `UnboundedReceiver<PumpMessage>`）；
- 显式 `close/stop` 由产品语义调用，`Drop<ChatPanel>` 不自动 shutdown。

### 最小迁移清单（建议顺序）

1. **新增 `pi-runtime` workspace crate**
   - 移入/包装 `ActiveSession` 的 spawn、Client、event pump、reducer、calibration、controls 和 lifecycle。
   - app 仍可暂时保留 command/UI DTO，但 runtime 不应依赖 GPUI。
   - `ClientConfig.max_restarts = 0` 在所有 Manager 创建路径统一设置。

2. **定义稳定身份与快照**
   - `RuntimeId` 在 fresh session 的 pi `session_id` 未校准前已稳定。
   - 每次底层进程替换增加 epoch；每次 snapshot 更新增加 revision。
   - Dirty/command result 必须携带 `RuntimeId + epoch`；UI 只接收当前 handle 的 epoch，revision 单调前进。

3. **用 `SessionHandle` 替换 `ChatPanel.active`**
   - 替换 `is_some/phase/reducer queue/document` 的直接读取为 snapshot。
   - 替换 `dispatch/request_control/restart/shutdown/refresh_metadata` 为 handle API。
   - 删除 app 对 `active.client()`、`reducer_mut()` 的依赖。

4. **把 reducer application 移至 runtime**
   - `PumpMessage::Events` 不再把 `LiveEvent` 交给 app reducer。
   - runtime 产出 immutable snapshot；ChatPanel 只在 revision 更新时同步 `status/list document/follow tail`。
   - follow-tail 可在 snapshot/change summary 中携带 `follow_tail`、`settled`，保留当前“一 batch 最多滚一次”语义。

5. **建立单一 `SessionUiState`**
   - 先机械搬迁上述 A 类字段与 reset/apply 方法，不改变 UI。
   - 至少提供 `reset_for_selection`、`reset_for_fresh`、`apply_snapshot`、`apply_runtime_event`、`apply_control_outcome`、`apply_rebind`、`clear_extension_lifecycle`。
   - `ChatPanel` 保持只有一个 state 实例；禁止提前做多 session map/UI。

6. **保留并重做 restart fence**
   - tool restart 不再 `take()` handle；Manager 在同一 `RuntimeId` 下切 epoch。
   - restart 完成前 UI 保留 handle 与 `ControlOperation::Tools`。
   - 旧 epoch 的 Dirty、request result、Extension response、calibration 必须忽略。
   - restart failure 进入可恢复 Failed/snapshot error，但 handle 仍稳定；不要以 `session = None` 模拟失败。

7. **保留 rebind 原子语义**
   - fork/clone/switch 成功后 Runtime 更新当前 session identity、calibration path、reducer document、controls，发布同一 revision 或可判定顺序的 revisions。
   - UI 的 `draft_key`、branch preview、document、controls 应在 `SessionUiState::apply_rebind` 一次提交，避免当前多字段半更新。
   - `RebindCalibrationFailed` 仍必须视为操作可能已成功，禁止自动重试；保留 `SessionsChanged` 现有规则。

8. **Extension UI 穿过 handle，不穿 raw Client**
   - runtime 负责接收、合并、reset 与 response 路由 epoch。
   - UI state 继续保留 sanitization/queue/dialog projection也可作为最小方案；关键是 response 调用 `handle.respond_extension_ui(epoch, response)`。
   - restart/rebind/Stopped 时取消旧 dialogs；旧 epoch response 必须被 runtime 拒绝而非发给新 client。

9. **compaction/retry 保持可观测顺序**
   - runtime snapshot/change 中保留 `CompactionStarted/Ended`、`RetryStarted/Ended`、`AgentEnded(will_retry)` 的可靠状态效果。
   - `SessionUiState` 继续维护 `compacting` 与 `RetryStatus`；controls 的 `is_compacting` 是 metadata 校准来源，event 是实时来源，需规定 revision 后写覆盖顺序。
   - `AbortRetry` 继续允许在非 Idle phase 发出，保持 `begin_control` 特例。

10. **迁移历史 HTML 导出**
    - `session_sidebar.rs:651-656` 改调应用共享 Manager maintenance API。
    - 删除 `live_session.rs:1457-1491` 的直接 `Client::spawn`。
    - maintenance 配额独立于用户 slot，并同样 `max_restarts=0`。

11. **改变 UI drop 语义**
    - 删除 `ChatPanel::drop` 对 runtime 的直接 shutdown。
    - R21 仍单实例，可在应用/Manager shutdown 时统一清理；显式选择另一个 Session 是否 stop 旧 runtime由 Manager API执行，不由 handle Drop 隐式决定。

### 风险与约束

- **最大风险是“双 reducer”**：如果 runtime 和 app 同时 apply `LiveEvent`，document/phase/queue 必然漂移。迁移点必须一次切断。
- **generation 不能简单删除**：`load_generation`、Runtime epoch、activity/calibration generation 是三种不同 fence，应拆清，不能全用 `RuntimeId` 代替。
- **Extension UI response 串台**：当前 sender 直接绑定 Client；重启时已有专门清队列注释。新 API 必须把 epoch 纳入校验。
- **fresh identity 校准**：临时 `fresh-N` 只是 UI/draft key；稳定 handle 主键必须是 `RuntimeId`，pi `session_id/path` 后置写入 snapshot。
- **rebind 已成功但 metadata 失败**：不能把它当普通 command failure，也不能重放 fork/clone/switch。
- **底层自动重启**：当前 app config 未覆写默认值，`ClientConfig` 默认 `max_restarts=3`。R21 Manager 路径必须显式置 0并测试。
- **R22 边界**：当前每 request/control 都 spawn thread、pump 是 unbounded；R21 可集中化但不必完成完整有界 Actor/背压，避免跨轮次。但 app 不应继续拿无界 receiver。
- **R24 边界**：只收敛一个 `SessionUiState`，不开放后台多 Session 或多实例 UI。

## 回归测试建议

### `pi-runtime` 纯逻辑/注入式测试

1. **唯一生产 spawn seam**：fake factory 记录 user与maintenance创建；app 无直接 `Client::spawn`。
2. **重启权归 Manager**：所有 Manager config `max_restarts == 0`；模拟进程失败并 Manager 标记 Failed 后，fake factory 未自行再次 spawn。
3. **稳定 handle**：tool restart 前后 `RuntimeId` 不变、epoch +1、revision 单调；pi session id 可从 temporary 校准为真实值。
4. **stale epoch**：旧 epoch 的 event、request result、calibration、restart completion 全部不改变 snapshot。
5. **reducer 单一所有权**：event batch 后仅 runtime document/phase/queue 更新，UI snapshot 与 reducer revision一致。
6. **calibration fence**：settled 后旧 calibration 不覆盖随后开始的新 run；复刻当前 `phase == Idle && activity_generation == calibration` 条件。
7. **restart failure**：handle 保持有效并暴露 Failed/error，旧 Client 已 shutdown，metadata refresh 不发往旧进程。
8. **maintenance 配额**：maintenance 并发 1且不占 user slot；失败/成功都 shutdown。

### app/`SessionUiState` 测试

1. **selection/fresh reset 等价**：覆盖 `panels.rs:438-508` 与 `520-610` 当前所有字段，尤其 file index、popup、draft、branch、retry/compaction、Extension UI、scroll/expanded。
2. **snapshot revision gate**：重复 revision 幂等；旧 revision/旧 epoch 不更新 status/list/controls。
3. **follow-tail batch 语义**：一个 snapshot change 最多一次 scroll；detached tail 不自动跟随。
4. **request failure 草稿语义**：Rejected 恢复草稿与附件；Ambiguous 不恢复；Abort 失败恢复 Running；无新 AgentStart 时恢复 Idle。
5. **fresh calibration**：真实 session id/path 到达后迁移 draft key、更新 document、只 emit 一次 `SessionsChanged`。
6. **rebind 原子性**：fork/clone/switch 后 document、draft key、branch preview、controls 一致；calibration warning 保留 draft且不重复命令。
7. **Extension UI restart**：旧 dialog 全部取消/关闭，旧 epoch response 不发给新 runtime；重复请求、timeout、status/widget coalesce 保持现有行为。
8. **Extension UI hidden chat**：保留 `main_panel.rs:849-1005`“文件 tab active 时仍驱动 dialog/title/editor/status/widget”的测试。
9. **compaction/retry**：Started/Ended、AgentEnded(will_retry=false)、AbortRetry 与 metadata refresh 顺序；错误反馈仍压过 success。
10. **tool restart 现有回归迁移**：把 `panels.rs:5495-5523` 改为 fake `SessionHandle`，验证 stale failure ignored、当前 failure recoverable、control busy 清除。
11. **restart path**：保留 `panels.rs:5919-5952`，existing controls path 优先、未落盘 path 不使用。
12. **session list refresh**：保留 `panels.rs:6006-6050`，仅成功/部分成功 fork/clone触发，cancel/retry 不触发。
13. **drop 生命周期**：drop `ChatPanel` 不 stop runtime；应用/Manager shutdown 才 stop，确保 R24 前置语义正确。

## Start Here

先打开 `crates/app/src/panels.rs:43-98`，建立 `SessionUiState` 字段表；随后紧接着看 `crates/app/src/live_session.rs:755-1110`。这两处正好定义了 R21 的切割线：前者应只保留轻量 handle + 单一 UI state，后者的大部分 Runtime ownership 应进入新 `pi-runtime`。
