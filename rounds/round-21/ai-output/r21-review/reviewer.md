我已完整读毕 `crates/pi-runtime/src/lib.rs`（2743 行）、`crates/app/src/panels.rs` 核心路径、`live_session.rs`、`main.rs`、`workspace.rs`、`session_sidebar.rs`、`pi-rpc/src/process.rs`（监督/订阅/shutdown/Drop）、`Cargo.lock`、`validate.ps1` 及三份 scout handoff。以下是审查结论。

---

## Review

### Correct（已核实为正确）

- **唯一生产创建入口成立**：`crates/app/src` 全量搜索 `Client::spawn` / `ClientConfig` / `pi_rpc::Client` 均为 0 匹配；生产 spawn 只存在于 `crates/pi-runtime/src/lib.rs`（`start_user`、`restart_with_tools`、`export_historical_html_impl`），provider 登录等非 RPC CLI 未迁入。
- **`max_restarts=0` 在最终 spawn 前统一钳制**：`clamp_manager_config`（lib.rs:1307-1309）在 `start_user`（1256）、`restart_with_tools`（1135）、`export_historical_html_impl`（1693）三处均在 `Client::spawn` 前调用。
- **`SessionHandle` 不泄漏 raw Client/reducer/pump**：只暴露 `runtime_id/snapshot/subscribe_dirty/refresh_metadata/dispatch/request_control/respond_extension_ui/restart_with_tools`；`Client` 封闭在 `RuntimeState` 内，handle 只有 `Arc<RuntimeEntry>`。
- **epoch/revision/Dirty stale fence 基本正确**：`publish_if_current`（1303）、`client_for_epoch`（946-957）、UI 侧 `apply_snapshot` 按 `effect.epoch == snapshot.epoch && effect.sequence > effect_cursor` 过滤（panels.rs:767-769），旧 epoch 结果被正确拒绝；`ToolRestartFinished`/`ExtensionUiReset` 在新 epoch 发布且 UI 在 epoch 变更时重置 cursor（panels.rs:738-745）。
- **maintenance gate 并发 1 且独立于用户槽**：`MaintenanceGate`（lib.rs:801-838）Condvar 信号量 + RAII `MaintenancePermit`，`Drop` 在 spawn/RPC/shutdown 失败与 panic unwind 时都会释放；`export_historical_html` 不触碰 `active_user`。
- **`Drop for ChatPanel` 已移除**（grep 无匹配），符合"面板 drop 不 shutdown runtime"的约束。
- **Extension UI response 经 epoch 路由**：`HandleExtensionResponseSender`（live_session.rs:12-26）→ `respond_extension_ui` → `client_for_epoch(epoch)`，旧 epoch response 被拒。
- **Cargo.lock 合规**：`gpui-pi` 依赖边 +1 行（2481）、`pi-runtime` 本地 package 12 行（4803-4811），无新增 `source`/`checksum`，与申报的 13 行一致。

---

### Finding 1 — HIGH · 正确性 · dispatch 请求失败恢复 Idle 的 activity_generation fence 已失效

- **位置**：`crates/pi-runtime/src/lib.rs:983-985`（`pending_activity_generation = (...).then_some(activity_generation)` 使用**调用方传入值**）对比 `lib.rs:1016`（`pending_activity_generation == Some(state.activity_generation)` 与**运行时内部计数器**比较）；调用方 `crates/app/src/panels.rs:2049`、`2185` 传入 `self.activity_generation`。
- **问题**：`SessionUiState.activity_generation` 在 panels.rs 中只被赋 0（459/510/598 及初始化 201），**从未递增**；而 `RuntimeState.activity_generation` 在事件泵每次 `AgentStart` 时递增（lib.rs:1820）。两者在第一次 `AgentStart` 后永久分叉。dispatch 里捕获的是面板永远为 0 的旧值，比较的是运行时已递增的值，因此 `Some(0) == Some(1)` 恒为 false。
- **触发场景**：会话完成第一次 agent run（`state.activity_generation` 变为 1）之后，任何一次提交被 pi 拒绝（`Rejected`）或请求失败（`Ambiguous`/超时/进程退出），`restore_phase(LivePhase::Idle)`（lib.rs:1019）都不会执行，reducer 的 `phase` 永远卡在 `Running`。
- **影响**：后续提交被 `submit_composer`（panels.rs:2026-2033）误判为 Steer/FollowUp 而非 Prompt；`set_model`/`set_thinking`/`set_tool_preset`/`begin_control` 因 `phase != Idle` 全部被门禁；"停止"按钮持续显示。现有测试只验证了纯函数 `should_restore_idle_phase`（panels.rs:5973-5976），未覆盖真实 wiring，故 122 passed 未捕获。
- **最小安全修复**：在 `dispatch` 内直接捕获 `state.activity_generation`（`pending_activity_generation = (...).then_some(state.activity_generation)`）并删除该入参；或在 `SessionSnapshot` 暴露 `activity_generation` 供面板在 dispatch 前读取。前者更小且消除跨层漂移。

---

### Finding 2 — MEDIUM · 契约 · 256 条 effect 截断会静默丢弃一次性结果

- **位置**：`crates/pi-runtime/src/lib.rs:890-892`（`while state.effects.len() > 256 { pop_front(); }`）。
- **问题**：`Snapshot.effects` 是唯一的 effect 载体，而一次性结果——`RequestFinished`（草稿恢复）、`ControlsLoaded`（fresh 会话 draft-key 迁移 + `SessionsChanged`）、`ToolRestartFinished`（清 busy/更新 preset）、`ControlFinished`（rebind 结果）——**都不在快照的权威字段（document/phase）里**，只存在于这个会被 `pop_front` 截断的 ring 中。UI 侧靠 `effect.sequence > effect_cursor` 消费（panels.rs:769），一旦 UI 主线程停滞期间累积超过 256 条，最旧的一次性结果被弹出且 sequence 落入已消费区间，永远不会被 apply。
- **触发场景**：UI 线程长时间阻塞（巨量文档渲染/同步卡顿/窗口最小化节流）期间，事件泵与各 request 线程持续 `publish`，超过 256 条。
- **影响**：草稿静默丢失（Rejected 后不恢复）、fresh 会话 draft-key 不迁移且 `SessionsChanged` 不发、`control_operation`/`tool_preset` 卡住。实现 handoff 自认这是"留待 R22 的残余风险"，但它是**可靠一次性投递契约**的缺口。
- **最小安全修复**：只对流式 kind（`Events`/`ExtensionUiBatch`）施加 256 上限；对一次性 kind 保留"每 kind 最近一条"的小型槽，或把它们下沉为 `Snapshot` 权威字段（如 `Snapshot.controls`/`rpc_error`/`tool_preset`），仅用 effect 触发增量刷新。

---

### Finding 3 — MEDIUM · 正确性 · pi 崩溃（`RestartFailed`）未被作为终态失败呈现

- **位置**：`crates/pi-runtime/src/lib.rs:1986-1996`（`project_event` 把 `LifecycleEvent::RestartFailed` 映射为 `LiveEvent::Diagnostic`，`Exited` → `None`）。
- **问题**：`max_restarts=0` 下进程每次崩溃即触发 `Exited`（被忽略）+ `RestartFailed("restart limit 0 reached...")`（变成文档诊断，`live.rs:440` `push_diagnostic` 不改 phase）。而 `Stopped` effect 只在事件泵 `recv()` 断开时发布（lib.rs:1756-1762、1840-1842），崩溃后 `state.client` 仍持有 Client，`shared` 未释放，`recv()` 持续阻塞，`Stopped` 永不触发。
- **触发场景**：pi 进程崩溃（OOM、段错误、被强杀）。
- **影响**：UI 无"会话已停止/失败"的终态信号——phase 保持 Idle/Running，模型/思考等控制仍可点，点后只得到"pi RPC process is not running"的写入错误；用户只看到一条晦涩的 `restart limit 0 reached` 诊断。相比旧代码（默认 `max_restarts=3` 透明自动重启），这是行为回归；scout 已明确警示"需单 epoch 终态幂等/Manager 判 Failed"，实现未落地。事件泵线程在崩溃后成为僵尸（阻塞在 `recv()`），直到下次 `shutdown_entry` 才退出。
- **最小安全修复**：在事件泵或 Manager 内把 `RestartFailed`（limit-0）归并为终态 `Stopped(Some(明确错误))`（或新增 `Failed` 变体），让 UI 清句柄并展示"会话已崩溃，请重新启动"；同时确保该终态在 Manager 记录 Failed 后不再被旧进程复活。

---

### Finding 4 — LOW · 可维护性 · tool restart 失败路径会以旧 epoch 发布误导性 `Stopped` 覆盖准确错误

- **位置**：`crates/pi-runtime/src/lib.rs:1135-1147`（`restart_with_tools` 失败分支不置 `stopped`/不增 epoch）结合 lib.rs:1756-1762（旧事件泵断开后 `publish_if_current` 以旧 epoch 成功发布 `Stopped("pi RPC 事件泵意外停止")`）。
- **问题**：restart 失败时 `state.client` 已被 `take()`、`state.stopped` 仍为 false、epoch 仍为旧值；重启闭包结束时 `old_client` 被 drop，旧事件泵 `recv()` 断开并以**当前仍有效的旧 epoch** 发布 `Stopped`。UI 依序列先 apply `ToolRestartFinished(Err)`（`self.active=None`、`rpc_error="工具预设重启失败…"`），再 apply `Stopped`，把 `rpc_error` 覆盖成误导性的"事件泵意外停止"（panels.rs:846-855）。
- **触发场景**：tool preset 切换时新进程 spawn 失败。
- **影响**：用户看到错误归因错误（"事件泵意外停止"而非"重启失败"），掩盖可恢复语义。
- **最小安全修复**：restart 失败分支在返回前把 `state.stopped = true`（或推进一个失效标记）以 fence 旧泵的 `Stopped`；或让 `Stopped` 不覆盖已有 `rpc_error`。

---

### Finding 5 — LOW · 健壮性 · `start_user` 先 shutdown 旧会话再 spawn 新会话，spawn 失败会丢失旧会话

- **位置**：`crates/pi-runtime/src/lib.rs:1225-1227`（`if let Some(old) = active_user.take() { shutdown_entry(&old); }` 先于 1240 `Client::spawn`）与 panels.rs:565（`start_fresh(...)?`）。
- **问题**：新会话 spawn 失败（pi 二进制缺失/无法拉起）时，旧 entry 已被 shutdown 且从 `active_user` 移除，`start_new_session` 经 `?` 返回 Err，但 `self.active` 仍指向已停止的旧句柄（僵尸），UI 显示旧文档为"活跃"。
- **影响**：失败的新建会话销毁旧会话；会话文件仍在磁盘（非数据丢失），但交互态混乱。scout 建议的"先停旧再启新"顺序存在此代价，未被识别。
- **最小安全修复**：先预检/创建新 client（成功后再 shutdown 旧 entry 并换入），失败时保留旧 entry。

---

### Finding 6 — LOW · 可维护性 · `pi-runtime` 声明未使用依赖 `futures`、`thiserror`

- **位置**：`crates/pi-runtime/Cargo.toml:10-11`。
- **问题**：lib.rs 全文件无 `futures::` 与 `thiserror`（`#[derive(Error)]`）引用，错误类型全部为 `String`。
- **最小安全修复**：从 `[dependencies]` 移除 `futures` 与 `thiserror`。

---

### 结论

**Verdict：FAIL**

存在一条 HIGH 阻断 finding（Finding 1：dispatch 的 activity_generation fence 失效，导致首次 agent run 之后任何被拒/失败提交都会把会话 phase 永久卡在 `Running`，破坏提交意图判定与所有 Idle 门禁）。该问题有直接代码证据且未被现有 122 项 app 测试覆盖（测试只验证了脱离 wiring 的纯函数）。合并前必须先修复 Finding 1 并补上覆盖真实 dispatch→失败→恢复 phase 的测试；Finding 2/3 建议在收口前修复或明确降级为有记录的 R22 债务。