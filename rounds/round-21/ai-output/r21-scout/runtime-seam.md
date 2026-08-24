# Code Context

## Files Retrieved
1. `crates/pi-rpc/src/process.rs` (lines 32-105) - `ClientConfig`、`LifecycleEvent`、`ClientEvent` 与错误模型。
2. `crates/pi-rpc/src/process.rs` (lines 122-205) - `Client` 所有权、supervisor 创建、事件订阅、resume session 与 PID API。
3. `crates/pi-rpc/src/process.rs` (lines 332-382) - typed request、Extension UI 响应、优雅 shutdown、进程树强杀和 `Drop` 行为。
4. `crates/pi-rpc/src/process.rs` (lines 416-574) - supervisor 主循环、首次启动/重启事件、退出处理、重启窗口与次数判定。
5. `crates/pi-rpc/src/process.rs` (lines 578-610) - `spawn_child` 固定追加 `--mode rpc`，并透传 args/env/session/cwd。
6. `crates/pi-rpc/src/process.rs` (lines 719-760) - stdout 响应关联、`get_state` 自动校准 resume session、事件广播与订阅者清理。
7. `crates/pi-rpc/src/lib.rs` (lines 1-17) - pi-rpc 的公开导出面。
8. `crates/pi-rpc/tests/client.rs` (lines 21-61) - fake child 配置、并发 request 和事件订阅基本接缝。
9. `crates/pi-rpc/tests/client.rs` (lines 141-218) - crash/restart/resume、主动 shutdown 不重启的现有测试模式。
10. `crates/app/src/live_session.rs` (lines 694-786) - 当前 app 专用 `PumpMessage`、raw `Client` sender、`ActiveSession` 所有权。
11. `crates/app/src/live_session.rs` (lines 870-929) - app 当前直接构造 `ClientConfig`、`Client::spawn`、`subscribe` 并拥有 reducer。
12. `crates/app/src/live_session.rs` (lines 1402-1440) - 用户 Runtime 的 config 构造接缝。
13. `crates/app/src/live_session.rs` (lines 1448-1491) - 历史 HTML maintenance 当前直接创建 `Client`。
14. `crates/app/src/live_session.rs` (lines 1503-1601) - 当前无界 event pump、按帧合并和 generation 标记。
15. `crates/app/src/panels.rs` (lines 43-99) - `ChatPanel` 当前同时拥有 Runtime、会话 UI 状态和 stale-event generation。
16. `crates/app/src/panels.rs` (lines 419-448) - generation 推进和切换时 shutdown 旧 Runtime 的 single-session 行为。
17. `crates/app/src/panels.rs` (lines 528-570) - fresh Runtime 创建与 raw Client 派生 sender。
18. `crates/app/src/panels.rs` (lines 624-648) - existing session Runtime 创建入口。
19. `docs/立项文档.md` (lines 60-101) - crate 分层与 `SessionHandle` / Snapshot / Dirty 目标依赖方向。
20. `docs/立项文档.md` (lines 120-157) - Manager 全局所有权、maintenance 配额、重启决策权、稳定 ID/epoch/revision 和 R21 compatibility 策略。
21. `docs/立项文档.md` (lines 297-315) - R21-R24 验收边界及 R22/R23 后续扩展约束。
22. `Cargo.toml` (lines 1-12, 15-44) - workspace 尚无 `pi-runtime` member/dependency。
23. `crates/app/Cargo.toml` (lines 1-31) - app 当前直接依赖 `pi-rpc`，R21 接线需改为生产路径经 `pi-runtime`。
24. `scripts/validate.ps1` (lines 1-43) - `-Logic` 当前仅包含三个逻辑 crate，R21 必须纳入 `pi-runtime`。

## Key Code

### pi-rpc 创建与监督 API

```rust
// crates/pi-rpc/src/process.rs:32-61
pub struct ClientConfig {
    pub binary: PathBuf,
    pub current_dir: Option<PathBuf>,
    pub initial_session: Option<PathBuf>,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub max_restarts: usize,
    pub restart_window: Duration,
    pub restart_delay: Duration,
    pub shutdown_grace_period: Duration,
    pub max_frame_len: usize,
}
```

`ClientConfig::new` 默认 `max_restarts = 3`。RuntimeManager 生产创建路径必须在 spawn 前强制覆写为 `0`，不能依赖调用方记得配置。

```rust
// crates/pi-rpc/src/process.rs:141-187
#[derive(Clone)]
pub struct Client { ... }

pub fn spawn(config: ClientConfig) -> Result<Self, ClientError>;
pub fn subscribe(&self) -> Receiver<ClientEvent>;
```

- `Client::spawn` 同步等待 supervisor 首次 spawn 成功后返回。
- 每次 `subscribe()` 得到一个 `std::sync::mpsc::Receiver<ClientEvent>`；底层广播到所有 subscriber。
- channel 无界，且注释明确要求上层持续 drain、按帧合并；这是 R22 的直接替换接缝，不应在 R21 把它复制成每 Runtime 多套无界队列。

```rust
// crates/pi-rpc/src/process.rs:64-91
pub enum LifecycleEvent {
    Started { pid, resumed_session },
    Exited { pid, code, success },
    Restarting { attempt, session_file },
    Restarted { pid, session_file },
    RestartFailed { error },
    Stderr { line },
}
```

订阅入口只有 `ClientEvent::Lifecycle(LifecycleEvent)`，没有单独 lifecycle subscriber。

### supervisor 重启语义

`supervise` 位于 `crates/pi-rpc/src/process.rs:416-574`：

1. 每轮从 `shared.resume_session` 读取恢复路径并调用 `spawn_child`。
2. 首次成功广播 `Started`；后续成功广播 `Restarted`。
3. 子进程退出后先广播 `Exited`，再使所有 pending request 失败。
4. 若 `shutdown=true`，直接返回。
5. 否则按 `restart_window` 清理时间窗；当 `restart_times.len() >= max_restarts` 时广播 `RestartFailed` 并终止 supervisor。
6. 未达限制则广播 `Restarting`、sleep `restart_delay`、重新 spawn。

关键边界：`max_restarts = 0` 时，第一次异常退出后立即进入 `RestartFailed`，不会广播 `Restarting`，也不会再次 spawn。这正适合 Manager 获得唯一重启决策权。Manager 应把 `Exited`/`RestartFailed` 归并为一个 epoch 的失败终态，并避免两次重复调度。

主动停止使用：

```rust
// crates/pi-rpc/src/process.rs:357-371
pub fn shutdown(&self) -> Result<(), ClientError>;
pub fn kill_process_tree(&self) -> Result<(), ClientError>;
```

- `shutdown` 先置位 shutdown、关闭 stdin，再 join supervisor；不会自动重启。
- `kill_process_tree` 不置 shutdown，当前默认配置会触发底层重启；Manager-owned Client 必须配 `max_restarts=0`，否则显式 fault injection/强杀会绕过 Manager。
- `Client` 最后一个 clone drop 时会调用 `shutdown`（`process.rs:375-381`）。因此 Manager/Runtime actor 必须是唯一长期 Client owner；`SessionHandle` 不能 clone/暴露 Client，否则生命周期会被 UI clone 数量隐式影响。

### resume/rebind 接缝

- `set_resume_session` / `resume_session`: `crates/pi-rpc/src/process.rs:190-199`。
- 成功 `get_state` 自动更新 resume path: `crates/pi-rpc/src/process.rs:719-739`。
- session rebind 的主命令与 calibration 分离：`request_session_rebind_data` 从 `process.rs:232` 开始；可保留既有“主命令成功但校准失败”语义。
- R23 的冷恢复可重新构造 `ClientConfig.initial_session`；热复用应调用现有 `switch_session` command，而不是把 `Client` 固定绑定到 SessionHandle。

## 最小可行所有权边界

### `RuntimeManager`

建议为应用级共享 `Arc<RuntimeManager>`，内部拥有同步/锁保护的 registry；R21 可先用 `single_session_compat`，但类型不要把“最多一个 Runtime”写死。

最小职责：

- 唯一生产 `Client::spawn` 入口，统一把 `max_restarts=0`。
- 分配稳定 `RuntimeId`。
- registry: `RuntimeId -> RuntimeEntry`。
- RuntimeEntry 独占 `Client`、event pump、reducer/ConversationDocument、进程 epoch、Snapshot revision、当前 session metadata。
- 提供 user session 与 maintenance 两类创建 API；maintenance 独立并发配额（R21 初值 1），不占 user slot。
- `single_session_compat`：新 user session 启动前优雅 shutdown 旧 user Runtime，保持 `panels.rs:446-447,548-550` 现状；但 shutdown 决策在 Manager，不在 ChatPanel。
- R21 不必实现 R23 完整 Scheduler 状态机，但内部状态至少区分 `Starting/Running/Stopping/Failed/Stopped`，避免未来破坏 API。

建议注入创建工厂，而不是测试时真实 spawn：

```rust
trait ClientFactory: Send + Sync {
    fn spawn(&self, config: ClientConfig) -> Result<Client, ClientError>;
}
```

若直接让 trait 返回具体 `Client`，失败/生命周期单测仍受 fake process 约束。更强的接缝是 Runtime 内部依赖一个最小 `RpcClient` trait（request、subscribe、shutdown、pid、resume APIs），生产 adapter 包装 `pi_rpc::Client`；但不要把整个 pi-rpc API无差别抽象。

### `RuntimeId`

建议 opaque newtype，例如 `pub struct RuntimeId(u64)`，仅 Manager 分配、`Copy + Eq + Hash + Debug`。

- 是 SessionHandle 的不可变身份。
- 不等于 Pi `session_id`，fresh session 校准不能更换 RuntimeId。
- 不等于 epoch；同一 RuntimeId 经 Manager 重启/未来热复用时 epoch 增加。
- 不建议用 session path 作 ID：new/fork/clone/rebind 会改变或晚到。

### `SessionHandle`

建议轻量 cloneable capability，仅含：

- `RuntimeId`
- `Arc<RuntimeManager>` 或隐藏的 command/snapshot façade

公开最小 API：`runtime_id()`、`snapshot()`、`subscribe_dirty()`/dirty receiver、命令提交、`shutdown/release`（语义由 Manager 决定）。

禁止公开：

- raw `pi_rpc::Client`
- `&mut LiveSessionReducer`
- `Receiver<ClientEvent>` 或当前 app `PumpMessage`
- 可由 UI 任意改写的 epoch/revision

当前 `ActiveSession::client()`、`reducer()`、`reducer_mut()`（`live_session.rs:917-929`）是需要封闭到 `pi-runtime` 内部的泄漏点。Extension UI response 应成为 SessionHandle command，而不是 `ClientExtensionResponseSender(Client)`（`live_session.rs:747-766`）。

### `Snapshot` / `Dirty`

R21 的 Snapshot 应是不可变、可 clone 的完整投影，而非事件流：

```rust
pub struct Snapshot {
    pub runtime_id: RuntimeId,
    pub epoch: u64,
    pub revision: u64,
    pub phase: RuntimePhase,
    pub document: Arc<ConversationDocument>,
    // commands/controls/runtime diagnostics/extension UI 等现有 UI 所需状态
}

pub struct Dirty {
    pub runtime_id: RuntimeId,
    pub epoch: u64,
    pub revision: u64,
}
```

- Runtime 内归并事件并原子替换最新 Snapshot；Dirty 只提示“重新拉取”，不携带权威增量。
- Dirty 可以丢/合并，因为 UI 最终按 revision 拉取完整 Snapshot；终态可靠性由 Snapshot 保证。这直接为 R22 latest-only/coalescing 留空间。
- R21 可暂用 `std::sync::mpsc`，但接口不要返回 app 的 `futures::mpsc::UnboundedReceiver<PumpMessage>`。
- 多 subscriber 必须独立；慢 UI 不得阻塞 Client event drain。

### epoch / revision

建议规则：

- `epoch` 属于 RuntimeEntry，由 Manager 在每次新的底层进程实例被接受时单调增加；初次 spawn 可为 1。
- `revision` 属于同一 Runtime 的 Snapshot，任何用户可见/命令可观察状态变化后单调增加。
- Snapshot key 是 `(RuntimeId, epoch, revision)`。
- 来自旧 event pump/request/calibration 的结果必须先校验 `(RuntimeId, epoch)`；同 epoch 内 UI 用 revision 忽略旧 Dirty。
- UI 当前是否可见不能参与 stale 判定（设计文档 `docs/立项文档.md:155`）。
- 不复用现有 `active_generation` 同时承担进程身份和 UI 切换。当前 `panels.rs:702` 只按 UI generation 丢弃消息，R24 后后台 Runtime 会被误判 stale。
- 当前 `activity_generation`/`calibration_generation` 是会话内 settle/calibration 序列，可保留为 Snapshot 内业务字段，不应替代进程 epoch 或 Snapshot revision。

## Architecture

当前数据流是：

`ChatPanel -> ActiveSession::spawn -> Client::spawn -> Client::subscribe -> app event pump -> PumpMessage -> ChatPanel mutates ActiveSession.reducer/UI fields`。

R21 目标数据流应变为：

`App-owned Arc<RuntimeManager> -> RuntimeEntry(Client + event drain + reducer) -> immutable Snapshot + Dirty -> ChatPanel(SessionHandle + SessionUiState)`。

命令反向流：

`ChatPanel -> SessionHandle command -> RuntimeManager/RuntimeEntry -> Client request`。

历史 HTML：

`ChatPanel/app -> RuntimeManager maintenance API -> short-lived Client(max_restarts=0) -> export -> shutdown`，替换 `live_session.rs:1448-1491` 的直接 spawn。maintenance API 应返回普通 Result/任务结果，不暴露 SessionHandle，因为它没有长期用户 Session UI 绑定。

## 测试接缝

1. **Manager 禁止底层自重启**：使用现有 fake child，Manager spawn 后强杀 PID；断言收到失败终态后，在超过旧 `restart_delay` 的窗口内 PID 不再出现，并且 spawn factory 调用次数仍为 1。现有参考测试为 `pi-rpc/tests/client.rs:141-179`，但需反转期望。
2. **配置钳制**：给 Manager 一个带 `max_restarts=3` 的输入 config，factory 捕获最终 config，断言实际 spawn 为 0。避免只测试默认调用路径。
3. **epoch stale event**：fake Runtime 先发 epoch 1 的延迟结果，Manager 重启至 epoch 2，再投递旧结果；Snapshot 不变化且 revision 不增加。
4. **revision/Dirty 合并**：连续产生多次状态变化，即使 UI 只收到最后一个 Dirty，`snapshot()` 仍返回最终完整状态。
5. **Handle 不控制进程寿命**：clone/drop SessionHandle 不导致 Client shutdown；只有 Manager policy/shutdown 改变 Runtime 生命周期。
6. **single_session_compat**：创建第二个 user session 时，旧 Runtime 先进入 stopping 并完成 shutdown，再创建新 Runtime；同时 maintenance job 不触发旧 user Runtime shutdown。
7. **maintenance 配额**：两个并发 export 请求，第二个可靠等待/拒绝（按 R21 任务卡选择），且不占 user slot。
8. **fresh identity calibration**：RuntimeId 在 Pi session_id/path 从临时值校准为真实值后保持不变，只增加 revision。
9. **现有行为回归**：rebind calibration failure、Extension UI reset on restart、compaction/retry、authoritative tail events。现有 app pump 对 `Restarted` 清 Extension UI 的逻辑位于 `live_session.rs:1668-1674`，迁移后需同等覆盖。
10. **validate 接缝**：`scripts/validate.ps1:24-27` 的 Logic scope 必须增加 `-p pi-runtime`。

## Risks / Open Questions

- **无界链路仍存在**：`Client::subscribe` 与当前 app pump 都是无界 channel。R21 可以集中化但不能宣称背压已解决；必须确保只有 Runtime 内一个持续 drain 的 subscriber，避免风险按消费者数量扩大。
- **`max_restarts=0` 事件序列**：异常退出会先发 `Exited` 再发 `RestartFailed`。若两个事件都驱动 Manager restart，会双重排队；需单 epoch 终态幂等。
- **Client clone 泄漏**：当前 Extension sender 和各种请求线程 clone Client。迁移时如果 SessionHandle 仍能拿 Client，Manager 无法证明唯一创建/生命周期边界。
- **同步 shutdown 阻塞**：`Client::shutdown` join supervisor，最长受 grace period/进程树 kill 影响；不能在 GPUI UI thread 直接调用。Manager 应在非 UI 执行上下文完成并只发 Dirty。
- **Snapshot 内容范围**：现有 reducer、commands、controls、retry/compaction、Extension UI 分散在 `ActiveSession` 与 `ChatPanel`。R21 必须明确哪些是 Runtime authoritative state、哪些是纯 `SessionUiState`。原则：协议/会话内容/运行阶段进 Snapshot；草稿、附件、滚动、展开、popup 等留 app UI state。
- **app 对 pi-rpc 直接依赖**：协议类型可能仍需 app 使用，但生产 `Client::spawn` 必须静态搜索归零；不能简单删除依赖而迫使协议类型经 pi-runtime 重导出。
- **R23 热复用**：不要把 RuntimeEntry 永久绑定 session identity；RuntimeId 是 resident Runtime 身份还是逻辑 Session 身份需实现前定清。设计文档措辞“Runtime 使用稳定 RuntimeId”且 SessionHandle 持它；为支持 IdleWarm `switch_session`，更安全的模型是 RuntimeId 标识逻辑 runtime/session binding，在 rebind/park-resume期间稳定，但热进程可作为内部 ProcessSlot 被重新绑定。R21 若把 Client 直接永久嵌入不可变 SessionHandle，R23 会难拆。
- **阶段开工前置**：仓库当前没有 `rounds/round-21/round-21.md`；设计文档 `docs/立项文档.md:301-307` 仍写阶段 E 与 R17/R18 先后待决、未选定前不得开工。此为实现启动门禁风险，需主会话确认已获得外部决策/任务卡授权。

## Start Here

先打开 `crates/pi-rpc/src/process.rs:32-187`：这里决定 Manager 能否可靠接管创建、重启与事件订阅。随后以 `crates/app/src/live_session.rs:694-929` 划出现有 `ActiveSession` 中应整体下沉到 `pi-runtime` 的 Runtime 核心，再用 `crates/app/src/panels.rs:43-99` 拆分 `SessionHandle` 与 `SessionUiState`。
