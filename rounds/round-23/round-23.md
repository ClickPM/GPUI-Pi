# Round 23 — 纯逻辑 Scheduler、状态机与 Park/Resume

> 执行方：Windows · 状态：已完成（validation 全绿；四轮独立代码审查的 findings 全部整改并补回归测试）

## 目标

在 `pi-runtime` 内实现**不依赖 GPUI 的**会话调度器：七态状态机（`Parked/Queued/Starting/IdleWarm/Running/Stopping/Failed`）、
RAII 运行槽 lease、带 aging 的公平等待队列、可注入时钟驱动的 Idle TTL 回收，以及 Park/Resume——
其中 Resume **优先复用空闲 Runtime 的 `switch_session`**，冷启动只是无空闲进程时的回退。
可证伪判据：登记 20 个 Session 并全部请求运行后，常驻 pi 进程数始终不超过配置上限，且两条 Resume 路径各有独立测试。

## 前置

- R21（集中化）、R22（有界 Actor + 事件背压）已合入 `main`。
- 项目所有者 2026-08-23 选定口径 B：阶段 E（R21–R27）先于 M4。
- Windows worktree：`D:\variFlight_work\GPUI-Pi\.claude\worktrees\r23-development-8027ed`。
- 新 worktree 已独立执行 `fetch-pi.ps1` / `fetch-pi-source.ps1` / `fetch-pi-web.ps1` / `check-pins.ps1`，
  vendor 三件齐全、`check-pins` 全绿、reparse point 扫描为空（2026-08-25）。

## 现状（改造前）

| # | 位置 | 表现 |
|---|---|---|
| ① | `RuntimeManager.active_user: Mutex<Option<Arc<RuntimeEntry>>>` | R21 的 `single_session_compat`：只有「当前唯一活跃 Runtime」概念，没有 Session 注册表，也没有运行槽预算 |
| ② | — | 没有 `Parked` 概念：一个会话要么有进程，要么不存在；无法登记 N 个会话而只跑 M 个 |
| ③ | — | 没有等待队列，没有公平性/aging，没有 Idle TTL，没有可注入时钟 |
| ④ | `execute_control(ControlRequest::SwitchSession)` | `switch_session` 已实现且在用，但只服务「用户在 UI 里切会话」，未被当作 Resume 的复用路径 |
| ⑤ | `pi_rpc::EventStream` | 订阅只能随 Client 关闭而断开；无法在保留进程的前提下确定性地停掉 pump 线程（Park 到 warm pool 的前提） |

## 设计要点

- **两个 id，各司其职**：`SessionId` 是 Manager 分配的**稳定会话身份**，跨 Park/Resume 不变；
  `RuntimeId` 仍然是**运行时容器（进程宿主）身份**，一次 Resume 会得到新的 `RuntimeId`。
  立项文档 § 七 R24 写「`SessionUiState` 按 `RuntimeId` 隔离」是在两 id 拆分之前的表述，R24 应改按 `SessionId` 隔离——
  本轮在「本轮实测」显式记录该偏离，不静默改语义。
- **`IdleWarm` 属于进程池，不属于某个 Session**（立项文档 § 三）：七态枚举 `SchedulerState` 同时描述 Session 与池内热进程，
  因为二者共享同一份 Resident Pi 预算，调度器必须在同一张表里看到它们。
- **Park 有两种落点**：Runtime 静止、会话已落盘且 warm pool 有空位 → 交出进程进池（`IdleWarm`）；否则优雅停机。两种都让 Session 变 `Parked`。
  Runtime **正在执行请求**时 `park` 直接返回 `Err` 且不改动任何状态 —— Park 是「让出进程」，不是「打断请求」。
- **Resume 优先 warm**：池内存在 `WarmKey`（binary / cwd / tool_preset / agent_dir）一致的热进程且会话已落盘 → `switch_session` 接管；
  否则冷启动。`SchedulerReport` 用 `warm_resumes` / `cold_starts` 计数把两条路径分开，可被测试直接断言。
- **Idle TTL 与 aging 只在 `tick(now)` 里发生**，时钟经 `Clock` trait 注入，`FakeClock` 让测试完全确定；
  生产由 Manager 内**唯一一条** reaper 线程按有界间隔调用 `tick()`，`with_test_clock` 构造的 Manager 不启动该线程。
- **本轮不改变 app 行为**：`start_fresh` / `start_session` / `stop_user` 签名与外部语义保持不变，
  内部改走调度器的 compat 通道（先占槽再替换旧会话，spawn 失败时旧会话仍在），`crates/app/**` 与 `crates/ui/**` 零改动。

## 交付物

- `crates/pi-rpc/src/process.rs`：新增 `LifecycleEvent::Detached` 与 `EventStream::detach()`。
  detach 把自己从订阅表摘除并投递哨兵事件唤醒阻塞中的 `recv`，**不影响其他订阅者、不影响 stdout drain**；
  这是「保留 pi 进程、确定性停掉 pump 线程」的唯一前提。
- `crates/pi-runtime/src/clock.rs`（新增）：`Clock` trait + `SystemClock` + `FakeClock`（`advance` / `set`）。
- `crates/pi-runtime/src/scheduler.rs`（新增）：`SessionId`、`SchedulerState`、`SchedulerLimits`、`SlotPool` + RAII `RunLease`、
  带 aging 的有界公平队列 `WaitQueue`、`SchedulerReport`。全部纯逻辑，可脱离进程单测。
- `crates/pi-runtime/src/lib.rs`：`RuntimeManager` 增加 Session 注册表与调度 API
  （`create_session` / `request_run` / `park` / `stop_session` / `session_state` / `handle` / `scheduler_report` / `tick`），
  warm pool（`WarmRuntime`）、Park/Resume 实现、reaper 线程；`RuntimeLimits` 增加 `scheduler` 字段。
- `crates/pi-runtime/tests/scheduler_fake_child.rs`（新增）：以 `runtime_fake_child` 为内核的调度器集成测试。
- `crates/pi-runtime/tests/park_resume_threads.rs`（新增）：Park/Resume 循环的线程存活预算（独立进程）。
- `crates/pi-runtime/tests/reaper_thread_budget.rs`（新增）：Manager 后台线程预算（独立进程）。
- 新增对外 API：`SessionHandle::session_id()` / `process_id()` / `is_quiescent()`、
  `RuntimeManager::active_user_session()`、`pi_runtime::live_thread_count()`、`pi_rpc::EventDetach`。
- 对应 `--lib` 单测（状态机、lease、aging、TTL、队列有界）与真实 pi 零 token 集成测试。

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | 全量格式、clippy、测试、release 构建与钉版本 | `.\scripts\validate.ps1` → `VALIDATE OK` |
| T1 | 逻辑 crate 快速回归 | `.\scripts\validate.ps1 -Logic` → `VALIDATE OK` |
| T2 | 七态状态机 | `Parked/Queued/Starting/IdleWarm/Running/Stopping/Failed` 全部可达且有测试；非法转移被拒绝而不是静默改状态 |
| T2 | RAII lease | `RunLease` drop 即归还运行槽（含 panic 路径）；槽位计数在任何路径下都不泄漏，测试用泄漏检测断言 |
| T2 | 公平队列带 aging | 先入队的低优先级条目在等待超过 aging 阈值后，会**先于**后入队的高优先级条目被调度；无 aging 时则相反。两种情形各有测试 |
| T2 | 队列有界 | 等待队列达 `queue_capacity` 后 `request_run` 返回 `Err` 且不改动任何 Session 状态 |
| T2 | Idle TTL + fake clock | `FakeClock` 推进到 TTL 之前热进程仍在池中，推进到 TTL 之后一次 `tick()` 即回收；测试不依赖真实时间 |
| T2 | Resident Pi 上限 | 登记 20 个 Session 并全部 `request_run`：任一时刻 `scheduler_report().resident_pi <= total_runtime_slots`，`running <= user_session_slots`，其余为 `Queued`；排空过程中上限持续成立 |
| T2 | Resume 走 warm（首选路径） | Park 到 warm pool 后 Resume：`warm_resumes` +1、`cold_starts` 不变，且新 Runtime 的 pi **pid 与 Park 前相同**（证明确实复用了进程而非重启） |
| T2 | Resume 走冷启动（回退路径） | 池空 / `WarmKey` 不匹配 / 会话未落盘时 Resume：`cold_starts` +1、`warm_resumes` 不变，pid 与 Park 前不同 |
| T2 | Park 不丢弃已受理的后台作业 | Actor 队列里还有排队或在执行的作业（如 Compact / Fork / SwitchSession）时 `park` 返回 `Err` 且不改动状态；`park` 会先在锁外给 500ms 收尾时间 |
| T2 | 运行槽不早于进程归还 | 任一终态（崩溃 / 停止 / 重启失败）都只在 `shutdown` 返回后才归还运行槽；每条拆除路径都置 `released`，漏置会被专门用例判红 |
| T2 | Park 不打断运行中的请求 | 会话正在执行请求时 `park` 返回 `Err` 且**不改动会话状态**（想强行结束走 `stop_session`）；空闲与已崩溃的会话可以 Park |
| T2 | Park 不泄漏线程 | Park→Resume 循环若干次后，存活线程数回到固定预算（每 Runtime = `command_workers + control_workers + 1`），与循环次数无关 |
| T2 | `EventStream::detach` | detach 后 `recv` 依次拿到 `Lifecycle(Detached)` 再 `Err`；其他订阅者不受影响；子进程 stdout 仍被持续 drain |
| T2 | 核心测试不依赖 GPUI | 上述 T2 全部位于 `pi-runtime` / `pi-rpc`，`cargo test -p pi-runtime -p pi-rpc` 可独立通过，不链接 GPUI |
| T2 | 真实 pi 零 token | `vendor\pi\pi.exe` 跑忽略型集成测试：Park→warm→`switch_session` Resume 全链路成立，不消耗模型 token |
| T2 | 行为回归 | `start_fresh` / `start_session` / `stop_user` 外部语义不变；`gpui-pi`、`gpui-pi-ui`、`pi-rpc`、`pi-render`、`pi-data` 既有测试全绿 |
| T3 | 可见行为 | 单会话交互与 R22 一致；`crates/app/**`、`crates/ui/**` 零 diff |

## 禁止

- **不开放 R24 多会话 UI**：`crates/app/**` 与 `crates/ui/**` 不得出现多 Session 接线；本轮 app 侧应为零 diff。
- 不实现 R25 的 Job Object / `ResourceProbe` / 内存水位 / `ConversationDocument` 有界缓存。
- 不实现 R26/R27 的子代理任务、`parentSession`、worktree writer 隔离。
- 不把 Park/Resume 实现成 kill + 冷启动：warm 复用必须走已有的 `switch_session`（立项文档 § 三明确要求复用而非重写）。
- 不新建每 Session 常驻线程：Parked Session 必须零线程零进程。
- 不修改 `PINNED_PI_VERSION` 或 `vendor/upstream/**` 身份；不引入新第三方依赖；`Cargo.lock` 不得有任何上游 version / source / checksum 漂移。
- 不写用户主目录 `~/.pi`；测试只读真实目录或使用临时目录。
- 不借本轮顺手修前序问题：BACKLOG #11 / #13 / #14 / #15 / #16 / #17 均不动。

## 失败处理

同一验收项经针对性整改后连续 2 次 validation 仍不过 → 写 `rounds/round-23/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 视觉审查

- 视觉审查模式：N/A
- 视觉审查结论：N/A
- 截图验证：N/A
- 兜底原因：N/A
- `requested_at`：N/A
- `deadline`：N/A
- 审查报告 / 证据：N/A
- 说明：本轮为纯逻辑轮。最终 diff 对 `crates/app/**` 与 `crates/ui/**` **零改动**
  （`git diff --stat -- crates/app crates/ui` 为空），未触及任何视图/组件/布局/样式渲染代码、
  Theme token、图标或视觉资源，也未改变任何用户可见文案或可见行为，因此不满足 CLAUDE.md
  「视觉还原度审查」的触发条件。调度器与 Park/Resume 的 UI 接线属 R24 范围。

## 本轮实测

### 门禁与基线

- 新 worktree vendor 门禁（2026-08-25）：`fetch-pi.ps1` / `fetch-pi-source.ps1` / `fetch-pi-web.ps1`
  全部 cache hit 并自检通过，`check-pins.ps1` 全绿；`vendor/pi/pi.exe`、`vendor/upstream/pi-0.84.2/`、
  `vendor/upstream/pi-web-0.8.9/` 三件齐全；worktree 内 reparse point 扫描为空（红线 6）。
- 改动前基线：`.\scripts\validate.ps1 -Logic` → `VALIDATE OK`。

### 关键设计决定与依据

| # | 决定 | 依据 |
|---|---|---|
| ① | **`SessionId` 与 `RuntimeId` 拆成两个身份** | `RuntimeId` 标识运行时容器（进程宿主），一次 Resume 必然换新（可能接管的是**另一个会话留下的**热进程）；`SessionId` 标识用户会话，跨 Park/Resume 恒定。已加 `SessionHandle::session_id()`。**这与立项文档 § 七 R24「`SessionUiState` 按 `RuntimeId` 隔离」的字面表述冲突**，该表述成文于两 id 拆分之前；照字面实现会让一次 Park/Resume 丢掉草稿与滚动位置。已记 `rounds/BACKLOG.md` #18，R24 须改按 `SessionId` 隔离并同步修订立项文档。 |
| ② | **Park 保留 `RuntimeEntry` 的做法被否掉，改成「摘出裸 `Client` 进池 + Resume 时新建容器」** | 前者要么让 Parked 会话继续背着 5 条线程（20 个会话就是 100 条），要么无法确定性结束 pump——pump 阻塞在 `recv` 上，空闲会话不会再产生任何事件。 |
| ③ | 为 ② 在 `pi-rpc` 增加 `LifecycleEvent::Detached` + `EventStream::detach()` / `EventDetach` | 这是「**保留 pi 进程**的前提下确定性停掉一条订阅」的唯一干净手段：丢 `Client` 会连进程一起杀掉，等下一条业务事件则在空闲会话上永远等不到。`EventStream` 内含 `Receiver` 因而不是 `Sync`，所以另拆一个 `Send + Sync` 的 `EventDetach` 句柄存在 `RuntimeEntry` 上。 |
| ④ | **`IdleWarm` 归进程池，不归 Session** | 立项文档 § 三：「可被任意 Session 复用的热进程，不是某个 Session 的预热副本」。七态枚举 `SchedulerState` 同时描述二者，因为它们共享同一份常驻 Runtime 预算——不把热进程算进同一张表，「Resident Pi 不超上限」就是假的。真实 pi 测试特意让 A 留下的热进程去接管 **B**，把这条语义钉死。 |
| ⑤ | **Idle TTL / aging 只在 `tick(now)` 里发生**，时钟经 `Clock` trait 注入 | `Instant` 无法构造任意时刻，fake 实现写不出来，所以时间统一表示为「自时钟原点起的 `Duration`」。生产由 Manager 内**唯一一条** reaper 线程按有界间隔调用 `tick()`；`with_test_clock` 构造的 Manager **不起** reaper，否则后台线程会抢在断言之前改状态。 |
| ⑥ | **Park 拒绝正在执行请求的会话** | Park 的语义是「让出进程」，不是「打断请求」；直接 Park 会把 in-flight 的 assistant 输出连同工具调用一起丢掉，而调用方并没有表达这个意思。想强行结束有明确入口 `stop_session`。 |
| ⑦ | 交出进程前等 `Actor::in_flight()` 归零（上限 500ms，超时退化为优雅停机） | `Actor::close()` 只丢得掉**排队中**的作业，丢不掉正在跑的那一条，而它手里往往攥着一个 `Client` 克隆——不等它返回，上一会话的 `get_state` 会和下一会话的 `switch_session` 撞在同一个内核上。为此在 `actor.rs` 新增在执行计数（RAII 配平）。 |

### 配置取值

立项文档 § 七阶段 E 钉死的初值原样落地：用户会话并发 **2**、总 Runtime **3**、Warm Idle **1**、Idle TTL **3 分钟**。
立项文档未规定、由本轮选定并记录依据的两项：

- `queue_capacity = 64`：队列只存轻量条目（id + 优先级 + 时刻），足够覆盖「一次性打开一整个项目的会话列表」，又不至于让用户排到看不见尽头；
- `aging_step = 5s`：比一次冷启动（秒级）长一档，避免刚入队就被 aging 抬到抢占前台；
- reaper 轮询间隔 = `clamp(idle_ttl / 4, 200ms, 5s)`：过期热进程最多多活 25% 的 TTL，同时极短 TTL 不会把 CPU 烧在轮询上。

### 七态可达性的证据分布

`Parked` / `Queued` / `Running` / `Failed` 与 `IdleWarm`（`SchedulerReport::warm`）在
`tests/scheduler_fake_child.rs` 有端到端断言；`Starting` 与 `Stopping` 按设计是**放开调度锁之前的短暂中间态**
（重活必须在锁外做），端到端观察它们必然是竞态的，因此在状态机层由
`every_scheduler_state_is_reachable_through_the_transition_table` 与
`illegal_slot_transitions_leave_the_state_untouched` 逐条钉死。这一分工是刻意的，不是覆盖缺口。

### 实测结果

- `cargo test -p pi-runtime --lib` → **70 passed / 0 failed / 1 ignored**（R22 收口时为 48，本轮新增 22：
  `scheduler.rs` 13 项、`clock.rs` 3 项、manager 层 6 项）。
- `cargo test -p pi-runtime --test scheduler_fake_child` → **11 passed**。
- `cargo test -p pi-runtime --test park_resume_threads` → **1 passed**（6 轮 Park/Resume 后存活线程回到基线，
  `warm_resumes == 6`、`cold_starts == 1`）。
- `cargo test -p pi-runtime --test reaper_thread_budget` → **1 passed**（生产构造恰好多一条 reaper 线程，
  `with_test_clock` 零线程，Manager Drop 后 reaper 自行退出）。
- `cargo test -p pi-runtime --test thread_budget` → **1 passed**（R22 既有用例未受影响）。
- `cargo test -p pi-rpc --test client` → **20 passed**（新增 detach 两项）。
- 真实 pi 零 token：`PI_RUNTIME_TEST_BINARY=<abs>\vendor\pi\pi.exe cargo test -p pi-runtime --test real_pi -- --ignored`
  → **3 passed**（R21 / R22 原有两项 + R23 新增 `park_to_warm_and_resume_via_switch_session_hold_against_real_pi`）。
  该用例证实：真实 pi 0.84.2 的进程被 Park 进池后，**pid 不变**地经 `switch_session` 接管了另一个会话，
  `get_state` 返回的 `session_id` / `session_file` 都切到了新会话；随后 fake clock 推过 TTL，一次 `tick()` 即回收。
- 连跑 4 次 `cargo test -p pi-runtime` 全部 70 + 11 + 1 + 1 + 1 通过，未见 flake。
- `.\scripts\validate.ps1 -Logic` → `VALIDATE OK`；完整 `.\scripts\validate.ps1` → **`VALIDATE OK`**
  （release 构建 6m59s，25 个测试目标零失败；`gpui-pi` 124 passed、`gpui-pi-ui` 30 passed、`pi-data` 86 passed，
  仅有既存的 linker stdout 与 `proc-macro-error2` future-incompat 警告）。

### 踩到的坑

- **进程级计数器不能在并行 lib 测试里做差值断言**：`a_parked_session_holds_no_process_and_no_thread`
  用 `spawned_thread_count()` 前后比较，被同进程内并行的其他用例顶掉，validation 红过一次（`left: 17, right: 16`）。
  这正是 `thread_budget.rs` 注释里写过的坑。已把两条线程预算断言移进各自独立进程的集成测试目标
  （`park_resume_threads.rs` / `reaper_thread_budget.rs`），lib 里只留与线程无关的断言。
- **`EventStream` 不是 `Sync`**（内含 `Receiver`），第一版把 `Arc<EventStream>` 存进 `RuntimeEntry` 直接让
  `RuntimeEntry` 失去 `Send`，所有 Actor 作业闭包一起编译失败。改为单拆一个 `EventDetach` 句柄。
- **公平队列的 aging 会被「pop 之后放回」清零**：`promote_queued` 初版先 `pop_next` 再交给 `admit`，
  抢不到槽时 `admit` 重新 `push`，等于把这条条目辛苦攒下的等待时间一次清零——恰好惩罚等得最久的那个。
  改为 `peek_next` 选人、由 `admit` 在**抢到槽之后**才出队。
- **warm pool 不能假设按空闲时长有序**：复用失败时热进程会被原样放回队尾，因此 TTL 回收改为全扫（池容量本就很小），
  淘汰也改为按 `idle_since` 取最老的，而不是取队首。

### 独立代码审查与整改

审查通道：Claude Code harness → **codex 插件**（`/codex:review --scope working-tree --base main --background`，
job `review-mt826lui-1icp07`，Codex session `01a036cb-7558-7033-923d-78056d531112`）。只读、与 writer 隔离。

结论：**4 项 P1 + 3 项 P2**。逐条核对源码后确认**全部成立**，且全部落在本轮新写的代码里，已全部整改并补回归测试。
writer 归属未变（本轮由主会话实现，因此由主会话修复）。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P1-1 | `restart_with_tools` 换掉工具预设后只改进程、不改 `SessionSlot::descriptor`。Park 于是用**过期预设**算 `WarmKey`：一个实际带 `--tools full` 的进程会被当成 ReadOnly 放进池子，再被另一个 ReadOnly 会话接管 —— 写权限跨会话漏出；冷启动恢复时也会退回旧配置 | 把「这个进程实际是用什么参数起来的」提升为 `RuntimeEntry::descriptor`（权威副本），`restart_with_tools` **成功之后**才更新它；Park / stop / 崩溃回收统一经 `apply_captured_state` 用它覆盖 Slot | `a_tool_restart_updates_the_descriptor_the_scheduler_hands_off`（含「ReadOnly 会话不得接管 Full 热进程」的正面断言） |
| P1-2 | `stop_session` 与 `reap_terminated` 不像 `park` 那样把 `calibration_path` 与 reducer 文档收进 Slot。fresh 会话落盘后被停掉或崩溃，下一次 `request_run` 会拿着 `session_path=None` + 最初的历史**重开一个空会话**，整段对话消失 | 抽出 `capture_runtime_state` / `apply_captured_state`，三处丢弃 Runtime 的路径全部先留档 | `stopping_a_session_keeps_its_history_for_the_next_run`、`a_crash_retry_resumes_instead_of_starting_a_blank_conversation` |
| P1-3 | `queue.pop()` 返回与 `in_flight` 自增之间有缝隙。worker 恰好在缝隙里被调度出去时，Park 会看到「队列空 + 没人在跑」，把进程交给下一个会话，随后那条陈旧作业醒来把 RPC 发到已被复用的 pi 上 | 在执行计数下沉进 `Queue`，由 `pop()` 在**出队锁内**自增，守卫在作业返回（含 panic 展开）时自减；`Actor::in_flight()` 改为两条通道求和 | `dequeue_accounts_for_the_job_before_pop_returns`、`in_flight_is_balanced_even_when_a_job_panics` |
| P1-4 | `park` 的「是否空闲」检查与 `detach_entry_for_warm` 之间没有栅栏。中间被接受的一次 `dispatch` 会把 reducer 推到 Running，detach 随即返回 `None`，而 `park` 把它当成兜底停机路径**照样杀掉进程并返回 `Ok`** —— 无声掐掉一个已受理的请求 | `park` 重构为「① 只读取 Runtime（不改状态）→ ② `begin_park` 在 Runtime 自己那把锁里一次完成『确认空闲 + 置停止位 + 摘走进程』→ ③ 落调度器状态」。`dispatch` 全程持同一把 `state` 锁，因此它要么排在前面（我们看到 Running，拒绝并**原样返回**），要么排在后面（看到 `stopped`，自己被拒），中间不存在缝隙 | `begin_park_refuses_a_busy_runtime_and_leaves_it_untouched`（断言拒绝时不置停止位、不写终态）＋ 既有的 `parking_a_busy_session_is_refused_instead_of_dropping_the_in_flight_request` |
| P2-1 | 并发 `park` 各自在锁外判断「池里还有位置」，`warm_idle = 1` 的池会装进两个进程、多占几百 MB | 容量复检移进 ③ 的同一把调度锁；装不下的进程在锁外关掉，`lease` 就地 Drop 归还整个运行槽 | `concurrent_parks_never_overfill_the_warm_pool`（竞态用例，**不保证每次撞进窗口**，但永不假红） |
| P2-2 | 并发 `start_fresh` 各自读到同一个 `previous`，最后一个覆盖 `active_user`；先起来的那个 Runtime 失去归属，`stop_user` 对它是空操作，进程再没人回收 | 「读 previous → 起新的 → 改 active → 收旧的」整段持 `active_user` 锁串行；关旧进程移到放锁之后 | `concurrent_compat_starts_leave_exactly_one_live_runtime` |
| P2-3 | `user_session_slots = 1` 时新会话先把旧会话 `remove_session` 掉再冷启动，冷启动失败则两个都没了，与「失败时旧会话原样还在」的承诺矛盾 | 让位改用 `park`（会拒绝忙碌的旧会话，那时旧会话**完全没被动过**）；新会话起不来则 `request_run(previous)` 原地拉回，错误文案明确写「旧会话已恢复，请重新获取会话句柄」；新增 `RuntimeManager::active_user_session()` 供调用方重新取句柄 | `a_failed_single_slot_start_rolls_the_previous_session_back` |

**整改过程中暴露的一条真实约束**（不是测试问题）：`the_reaper_thread_...` 在整套并行跑时偶发红，
定位是 `begin_park` 的 500ms 排空预算被撑爆 —— **启动后的元数据刷新（`get_commands` / `get_state` / models / tree）
是一批在执行的作业，此刻 Park 只能退化成优雅停机**，真实 pi 上这批作业要跑好几秒，degradation 会更明显。
处置是把判据暴露成 API `SessionHandle::is_quiescent()`（而不是把测试调松或把预算调大）：
调用方想稳定拿到热进程复用就先等它为真，R24 的 UI 也可以据它决定是否提供 Park 入口。
所有断言热进程复用的用例改为先等静止再 Park；`parking_a_busy_session_is_refused_*` 刻意不等（它就是要撞忙碌态）。

整改后复测：`cargo test -p pi-runtime` → lib **73 passed** / `scheduler_fake_child` **17 passed** /
`park_resume_threads` **1** / `reaper_thread_budget` **1** / `thread_budget` **1**；
`cargo test -p pi-rpc` → **32 passed**（`client` 20 + 其余）；连跑 **5 次**零失败；
真实 pi 零 token `--ignored` → **3 passed**。

### 第二轮独立代码审查与整改

首轮整改改动面很大（`park` 整体重构、Actor 计数下沉、compat 通道串行化 + 回滚），因此**主动再跑了一轮**同通道审查
（job `review-mt850iv8-dcarue`，Codex session `01a036cb-…`，同样 review-only、与 writer 隔离）。

结论：**6 项 P1**，全部落在首轮整改后的新代码里。逐条核对源码后确认**全部成立**，已全部整改并补回归测试。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| R2-1 | `park` 与 `request_run` 争抢同一会话：`begin_park` 先发布了 `Stopped` 终态，而槽位此时还是 `Running`；中途放开调度锁的话，并发的 `request_run` 会先 `tick()` 把这个 entry 当成「被外部停掉的」回收、再装上一个**新 Runtime**，随后 `park` 回来无条件改状态，把新 Runtime 连同进程一起挤掉 | `begin_park` 拆成 `reserve_park`（Runtime 侧原子段，**在调度锁内**调用）+ `finish_park`（锁外拆线程）。预留时复核「槽位仍是 `Running` 且 `slot.entry` 仍是同一个 `Arc`」，随后在同一把锁里转 `Stopping` 并把 entry 与 lease 一起摘下 —— 窗口不复存在 | `concurrent_park_and_resume_never_orphan_a_runtime_or_break_the_bound` |
| R2-2 | **锁序倒置**：`capture_runtime_state` 走 descriptor → state，而 `restart_with_tools` 两处走 state → descriptor。一次「停会话」撞上一次「换工具预设」就互等到死 | 钉死唯一锁序 **descriptor 先于 state**（写进 `RuntimeEntry::descriptor` 字段注释）：`restarted_descriptor` 移到取 `state` 之前构造；成功段改为先拿 descriptor 再拿 state，两把锁一起持有，让「装上新进程」与「更新权威参数」对外是一次原子变更 | `a_tool_restart_racing_a_stop_never_deadlocks`（带看门狗，把死锁变成失败而不是挂起） |
| R2-3 | `stop_user` 在一次持锁里匹配、放锁、再无条件置 `active_user = None`。并发的 `start_fresh` 在缝隙里装上新会话后被抹掉，它的 Runtime 从此再也停不掉 | 匹配与清空放进**同一次持锁**，且只在匹配成功时才清 | `concurrent_stop_user_and_start_fresh_keep_active_user_consistent` |
| R2-4 | 控制类作业（Compact / Fork / SwitchSession）**不改 reducer 的 phase**，只看 phase 会把「队列里还压着一次 Fork」当成空闲；`actor.close()` 随即把排队作业丢掉，正在跑的那条又被抽掉 client —— 一次用户点过的 Compact、一次不可重放的 Fork 就这么无声消失 | Park 准入改为「phase 空闲 **且** Actor 完全静止」（新增 `Actor::is_idle()`）。拒绝原因细分为 `Busy` / `PendingJobs` / `Replacing`，文案各不相同；`park` 先在**所有锁之外**给后台作业 500ms 收尾时间（这段等待不改任何状态），等不到就明确拒绝，绝不丢作业。原先「排空超时就退化成停机」的兜底随之删除 —— 那条路径本身就是在丢作业 | `reserve_park_refuses_while_actor_jobs_are_outstanding`（构造「phase 空闲但作业在跑」，并断言拒绝时不置停止位、不写终态） |
| R2-5 | warm 池满 / 会话未落盘 / 不可复用时，`lease` 在匹配分支里就地 Drop，而 `surplus` 进程是**放锁之后**才 shutdown。并发 `request_run` 于是能在旧进程还活着时拿到常驻槽，突破 `total_runtime_slots` 硬上限 | lease 跟着进程走：`surplus` 改为 `(Option<Client>, SlotLease)`，先 `shutdown()`、再 `mark_released()`、最后才 Drop lease。同时把提交段里一处 `?` 改成 `let _ =` —— 那条早退路径同样会让 lease 先于进程归还 | 同 R2-1 的拉锯用例（断言 `slots.resident` 与 `resident_pi` 全程不越界，收尾后归零） |
| R2-6 | `fail_runtime` 先发布终态、后 `Client::shutdown`（R22 刻意如此：崩溃要立刻可见）。`reap_terminated` 一看到终态就归还运行槽，此刻进程可能还在退出（典型是事件积压 fail-stop 正在杀进程），新会话补位即超限 | 新增 `RuntimeEntry::released` 标记，只有 `shutdown` 真的返回之后才置位；`reap_terminated` 未置位就跳过、留到下一次 tick（reaper 每轮都会回来）。UI 侧不受影响 —— 终态仍然经 Snapshot 立即可见 | `every_teardown_path_marks_the_runtime_as_released` |

**整改自身引入、并在补测时抓出来的一个洞**：`publish_tool_restart_failure` 会写 `Failed` 终态却从不 `mark_released`
（它的旧进程在进入函数前就已 shutdown、新进程压根没起来）。加了 R2-6 的闸门之后，这条路径会让 `reap_terminated`
**永远**跳过该会话，运行槽被永久扣住 —— 比原问题更糟。已补上标记，并把「每一条拆除路径都必须置位」写成逐路径断言的
用例（`every_teardown_path_marks_the_runtime_as_released`），防止以后新增拆除路径时再漏。

**一处是我的测试断言错了、不是代码错了**：拉锯用例原本断言 `remove_session` 之后运行槽立即归零，实际留下 `warm: 1`。
这是正确设计 —— 热进程**不属于任何会话**（立项文档 § 三），注销一个会话不该顺手杀掉别的会话还能复用的进程，它由
Idle TTL 回收。测试改为推过 TTL 再断言归零，而不是去改代码迎合断言。

第二轮整改后复测：`cargo test -p pi-runtime --lib` → **75 passed**；`scheduler_fake_child` → **20 passed**；
`park_resume_threads` / `reaper_thread_budget` / `thread_budget` / `runtime_fake_child` 各 **1 passed**；
`cargo test -p pi-rpc` → **32 passed**；连跑 **5 次**零失败；真实 pi 零 token `--ignored` → **3 passed**。

### 第三轮独立代码审查与整改

前两轮各查出 6–7 个真问题，说明自查兜不住这块并发面，因此又跑了一轮同通道审查
（job `review-mt86gqjx-qx048n`）。结论：**2 项 P1 + 3 项 P2**，全部落在第二轮整改后的新代码里，
逐条核对后确认成立。其中 **4 条即使调用方是单线程也会发生** —— 竞争来自 pi-runtime 自己的
pump / actor 线程，不需要外部并发。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| R3-1 | `stop_session` 撞上进行中的 `restart_with_tools`：那一刻 `state.client` 已是 `None`（旧 client 攥在替换作业手里），`shutdown_entry` 什么也关不掉却照样宣布「进程已释放」，调用方随即让出运行槽，而旧进程还在退出、甚至已经 spawn 出替身 —— 突破 `total_runtime_slots` | 引入**进程句柄持有者计数** `RuntimeEntry::client_owners`，由替换作业**捕获**的 RAII 守卫维护；`is_released()` 要求「拆除流程走完 **且** 持有者为 0」。`shutdown_entry` 回到无条件置位，不再靠启发式猜 | `stopping_a_session_while_a_tool_restart_is_in_flight_still_frees_the_slot` |
| R3-2 | 同一会话被并发 `stop_session`：后到者看到 `Stopping` 且无 entry，仍走完收尾，把前一次的 lease 提前归还；会话被重新拉起后，前一次再收尾一遍，又把新 Runtime 的 lease 和状态一起覆盖 | 收尾与「本次调用摘下的那个 Runtime」绑定：后到者发现 `Stopping` 且 `entry` 已被摘走就**完全不动**，由拥有者收口 | `concurrent_stop_session_calls_do_not_double_finalize` |
| R3-3 | pump 被 detach 唤醒时，手上那一帧已收进来的事件被直接丢弃。若含扩展 UI 请求，pi 里那个扩展可能正等着回应，而这个进程随后进了 warm pool 交给下一个会话 | `finish_park` 改为**等 pump 真的退出**再决定进程去留，并在 pump 丢过帧时把进程标记为不可复用（宁可多付一次冷启动）。正文增量丢一点无妨 —— pi 自己的会话文件才是权威，Resume 后的落盘校准会补回来 | `a_runtime_whose_pump_dropped_a_frame_is_never_pooled` |
| R3-4 | `remove_session` 注销的若是兼容通道的活跃会话，`active_user` 会留在一个已经不存在的 id 上：`active_user_session()` 返回无效值，`stop_user()` 再也匹配不上 | 注销时一并清掉指针 | `removing_the_active_session_clears_the_compatibility_pointer` |
| R3-5 | `session_descriptor` 读 Slot 上那份，而 `restart_with_tools` 只更新 `RuntimeEntry` 上的权威副本 —— 运行中的会话会报出**过期的工具预设**，调用方以为一个高权限进程还是只读的 | Runtime 在跑时改读它自己那份 | 强化 `a_tool_restart_updates_the_descriptor_the_scheduler_hands_off`：**Park 之前**就断言返回新预设 |

**整改中我自己制造并抓出的两个问题**（都被新测试判红，不是靠事后 review 发现的）：

1. **自锁**：给 `remove_session` 加「清 `active_user`」之后，`start_user` 的失败分支仍持着同一把锁调它，
   std `Mutex` 不可重入 —— 整个 lib 测试挂死。既有用例
   `start_user_spawn_failure_preserves_existing_active_handle` 直接卡住，等于现成的回归测试。
   修法是先放锁再 `remove_session`，重新取锁时只清「我们认得的那个」id。
2. **RAII 守卫放错位置**：R3-1 第一版把守卫写在**闭包体内**，于是「作业还在队列里就被 `close()` 丢掉」
   那条路根本不会执行它；当时又用 `actor.in_flight() == 0` 做补充判据，而启动期的元数据作业会让它非零 ——
   两个洞叠加成运行槽永久泄漏，新用例约 50% 复现。正解是让守卫由闭包**捕获**：闭包被丢弃时守卫照样 Drop。
   这条经验写进了 `ClientOwnerGuard` 的注释，防止以后再踩。

第三轮整改后复测：`cargo test -p pi-runtime --lib` → **76 passed**；`scheduler_fake_child` → **23 passed**；
`park_resume_threads` / `reaper_thread_budget` / `thread_budget` / `runtime_fake_child` 各 **1 passed**；
`cargo test -p pi-rpc` → **32 passed**；`scheduler_fake_child` 单独连跑 **10 次**、全套连跑 **5 次**均零失败；
真实 pi 零 token `--ignored` → **3 passed**。

**关于收口**：三轮下来的问题密度是从 7 → 6 → 5，且第三轮已有 3 条是 P2、2 条是「返回过期数据 / 指针悬空」
这类非并发的简单缺陷，而不再是新的结构性竞态。`SchedulerReport` 新增 `draining` 字段把「拆除中仍占名额」
如实计入 `resident_pi`，硬上限在报表层也不再有盲区。

### 第四轮独立代码审查与整改

第三轮改动同样不小，因此再跑一轮确认收敛（job `review-mt898isy-zdpgwu`）。
结论：**2 项 P1 + 1 项 P2**，核对后全部成立。三条同属一个主题 —— 「运行槽必须活得比进程久」
与「报表不得少报」，没有出现新的结构性竞态。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| R4-1 | `fail_runtime` 取走 client 后要花到 grace period 才关完，这段时间进程还活着；并发的 `park` 看到「有终态 + 没有 client」就一路走到 `mark_released`，运行槽在旧进程退出前被让出去。`stop_session` 经 `shutdown_entry` 有同样的窗口 | 把第三轮引入的 `ClientOwnerGuard` 铺到**每一条**「持有 client 并在锁外关它」的路径：`fail_runtime`、`shutdown_entry`、`park` 的兜底停机。`is_released()` 因此要求「拆除流程走完 **且** 持有者为 0」 | `a_runtime_is_not_released_while_a_job_still_owns_its_process` |
| R4-2 | pump 的丢帧标记只挂在哨兵那条路上。若这一帧被 frame deadline 或 512 条上限先截断，而 Park 已经置了 `stopped`，pump 会从 `epoch/stopped` 那个分支退出，帧照样丢、却不标记 —— `finish_park` 于是认为进程可复用 | 把「这一帧还有没有内容」提成 `frame_has_payload`，两条退出路径共用 | 消费侧由 `a_runtime_whose_pump_dropped_a_frame_is_never_pooled` 覆盖；**边界如实说明**：产生侧（pump 在哪条分支置位）没有独立用例，它需要一个能精确卡在 frame deadline 上的 fixture |
| R4-3 | `resident_pi` 按状态桶求和，而 Park 兜底停机 / warm 淘汰 / TTL 回收都是「先从集合里摘走、再到锁外 shutdown」，那一瞬间谁都不认领这个进程，报表谎称还有余量 | `resident_pi` 直接取运行槽计数 —— lease 从 spawn 前占用、到 shutdown 后归还，是唯一没有窗口的权威计数 | `resident_pi_always_tracks_the_lease_count` |

第四轮整改后复测：`cargo test -p pi-runtime --lib` → **77 passed**；`scheduler_fake_child` → **24 passed**；
其余各 **1 passed**；`cargo test -p pi-rpc` → **32 passed**；全套连跑 **5 次**零失败；
真实 pi 零 token `--ignored` → **3 passed**。

### 四轮审查的收敛判断

| 轮次 | findings | 性质 |
|---|---|---|
| 一 | 4 P1 + 3 P2 | 权限跨会话泄漏、状态丢失、Park 无声掐请求 —— 结构性 |
| 二 | 6 P1 | 锁序倒置、多处竞态窗口 —— 结构性 |
| 三 | 2 P1 + 3 P2 | 2 条竞态 + 3 条「返回过期数据 / 指针悬空」 |
| 四 | 2 P1 + 1 P2 | 全部是同一主题的收尾（lease 生命周期、报表口径） |

密度 7 → 6 → 5 → 3，性质从「新的结构性问题」收敛到「同一条不变量的最后几个窗口」。
`resident_pi` 改取 lease 计数之后，硬上限在报表层不再有盲区；`ClientOwnerGuard` 铺到全部
持有路径之后，「运行槽早于进程归还」这一类也没有剩余入口。据此判断可以收口。

### 范围与边界

- `crates/app/**`、`crates/ui/**` **零 diff**；`Cargo.lock` **零改动**（`git diff --stat Cargo.lock` 为空），
  未新增任何第三方依赖，未触碰 `PINNED_PI_VERSION` 与 `vendor/upstream/**`。
- `start_fresh` / `start_session` / `stop_user` 签名与外部语义未变：内部改走调度器 compat 通道，
  仍然是**先把新会话拉起来、成功之后才停旧的**，spawn 失败时旧会话原样还在
  （`start_user_spawn_failure_preserves_existing_active_handle` 已扩展为同时断言调度器报表回到只剩旧会话）。
- 未提前实现 R24 多会话 UI、R25 Job Object / `ResourceProbe` / 内存水位、R26/R27 子代理。
- 未借本轮修前序问题：BACKLOG #11 / #13 / #14 / #15 / #16 / #17 均未动。
- 本轮新增 BACKLOG #18（R24 必须按 `SessionId` 隔离 UI 态）、#19（R25 应提供显式关停入口）、
  #20（`admit` 在调度锁内做了一次 `is_file()`）。
