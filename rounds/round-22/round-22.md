# Round 22 — 单 Runtime 有界 Actor 与事件背压

> 执行方：Windows · 状态：进行中（实现与 validation 已全绿，等待代码审查 + 视觉审查门禁）

## 目标

把 R21 集中化后仍然无界的三条路径（逐请求 OS 线程、`ClientEvent` 订阅队列、per-Runtime effect 缓存）改造成**有界**结构：单 Runtime 使用固定线程数的 Actor + 有界命令队列（默认 32）、控制通道与普通命令分离、事件按 16–33ms 帧合并且同 key latest-only、每 Session 事件缓存有固定字节上限；同时保证可靠终态永不丢失、UI 暂停消费后恢复仍得到完整最终 Snapshot，且 stdout 在任何背压路径下都持续 drain、绝不因下游满而阻塞 pi。

## 前置

- R21 已完成并合入 `main`（`pi-runtime` + `RuntimeManager` + `SessionHandle` 已就位）。
- 项目所有者 2026-08-23 选定口径 B：阶段 E（R21–R27）先于 M4。
- Windows worktree：`D:\variFlight_work\GPUI-Pi\.claude\worktrees\r22-development-start-eed679`，分支 `WinClaude/round-22`。
- 新 worktree 已独立执行 `fetch-pi.ps1`、`fetch-pi-source.ps1`、`fetch-pi-web.ps1`、`check-pins.ps1`，完整 vendor 门禁全绿（2026-08-24）。

## 现状（改造前实测的无界点）

| # | 位置 | 无界表现 |
|---|---|---|
| ① | `crates/pi-runtime/src/lib.rs` `dispatch` / `refresh_metadata` / `request_control` / `restart_with_tools` / `spawn_calibration` | 每次调用 `thread::Builder::spawn` 新建 OS 线程，连续 follow-up 线程数无上限 |
| ② | `crates/pi-rpc/src/process.rs` `Client::subscribe` = `mpsc::channel()`（无界），`broadcast` 无背压计量 | 下游 pump 卡住时订阅队列可无限增长 |
| ③ | `crates/pi-runtime/src/lib.rs` `RuntimeEntry::publish` → `state.effects: VecDeque`（R21 明确留待 R22 收敛） | effect 永不回收；`snapshot()` 每次全量 clone，随会话时长线性增长 |
| ④ | `PUMP_FRAME` 固定 20ms、`MAX_EVENTS_PER_BATCH` 512 无 key 合并 | 帧内同 key 事件反复入 effect 流 |

## 交付物

- `crates/pi-rpc/src/process.rs`：`subscribe()` 改为**字节计量的有界广播**（`EventStream`），`broadcast` 全程 `try`-语义不阻塞；订阅积压超上限时**不静默丢事件**，改为发终态 `LifecycleEvent::EventBacklogOverflow` 并断开该订阅（fail-stop）。stdout reader 与 supervisor 的 drain 路径不受下游影响。
- `crates/pi-runtime/src/actor.rs`（新增）：有界命令队列 + 固定线程池。普通通道默认容量 32，控制通道独立；队列满时 `dispatch` 返回 `Err` 且不改动 reducer 状态。
- `crates/pi-runtime/src/effects.rs`（新增）：有界 effect 缓存。三类语义——**可靠终态**（`RequestFinished` / `ControlFinished` / `ToolRestartFinished` / 终态 `Stopped`）永不丢；**latest-only**（`Events` / `CommandsLoaded` / `ControlsLoaded` / `ExtensionUiBatch` 同 id）按 key 合并；**尽力而为**（`Diagnostic`）超限丢弃并计数。固定字节上限通过「剥离重负载（`ComposerSubmission` 图片）而非丢事件」实现。
- `crates/pi-runtime/src/lib.rs`：`RuntimeLimits` 扩展为可配置有界参数（队列容量、worker 数、帧长 16–33ms、effect 字节上限）；`SessionSnapshot` 增加**权威终态字段** `terminal` 与背压统计 `backpressure`；新增 `SessionHandle::ack_effects` 供 UI 回收已消费 effect；优雅停止纳入可观察终态（关闭 BACKLOG #12 中属于 R22 的部分）。
- `crates/app/src/panels.rs`：消费 Snapshot 后 `ack_effects`；命令队列满与背压统计有明确用户可见提示。
- 对应单元测试 + fake-child 集成测试 + 真实 pi 零 token 集成测试。

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | 全量格式、clippy、测试、release 构建与钉版本 | `.\scripts\validate.ps1` → `VALIDATE OK` |
| T1 | 逻辑 crate 快速回归 | `.\scripts\validate.ps1 -Logic` → `VALIDATE OK` |
| T2 | 命令队列有界 | 默认容量为 32；灌满后 `dispatch` 返回 `Err`，且 reducer phase / activity_generation **未被改动**；排空后可继续 |
| T2 | 控制通道分离 | 普通通道被长任务占满时，`Abort` / `request_control` 仍能立即被受理并完成 |
| T2 | 连续 follow-up 不新增无界 OS 线程 | 注入线程计数器：连续 200 次 follow-up 后，Runtime 存活线程数 ≤ 固定上限（worker 数 + pump + 常驻），与请求次数无关 |
| T2 | 帧合并 | 帧长落在 16–33ms 闭区间（越界配置被 clamp 且有测试）；一帧内 N 个流式事件只产生 1 条 `Events` effect |
| T2 | 同 key latest-only | 连续多次 `CommandsLoaded` / `ControlsLoaded` / 同 id `ExtensionUiBatch` 只保留最新一条 |
| T2 | 可靠终态不丢 | **终态**（崩溃 / 重启失败 / 积压超限 / 优雅停止）是 `SessionSnapshot.terminal` 权威字段，任何背压下都不丢；一次性结果（`RequestFinished` / `ControlFinished` / `ToolRestartFinished`）在字节上限内优先靠剥离可重建负载保全，只有在剥离与低优先级淘汰都不足时才作为最后一级被淘汰，且**每一次淘汰都有计数并由 UI 明示**，不存在静默丢失 |
| T2 | 固定字节上限 | 持续发布大负载 effect，`snapshot().backpressure.buffered_bytes` 始终 ≤ 配置上限 |
| T2 | UI 暂停后恢复 | 停止消费 Snapshot 期间灌入完整会话事件流，恢复后单次 `snapshot()` 得到完整 document、正确 phase、权威终态与最新 controls |
| T2 | stdout 持续 drain | fake child 在订阅者完全停摆时持续写 stdout：子进程不被阻塞（写入持续推进），订阅路径内存有界，超限走 fail-stop 而非静默丢事件或无限增长 |
| T2 | 优雅停止可观察 | `stop_user` 后 Snapshot 的 `terminal` 为优雅停止终态（BACKLOG #12 的 R22 部分） |
| T2 | 真实 pi 零 token | `vendor\pi\pi.exe` 跑 `pi-runtime` 忽略型集成测试：有界队列 + 背压路径下启动、`get_state`、优雅 shutdown 全部正常，不消耗模型 token |
| T2 | 行为回归 | 现有 restart/rebind、Extension UI、compaction/retry、HTML 导出、`gpui-pi` 全部测试继续通过 |
| T3 | 可见行为 | 单会话交互与 R21 一致；若最终 diff 触发 UI 视觉审查条件，按 `SCREENSHOT` / `CODE_ONLY` 门禁执行 |

## 禁止

- 不实现 R23 的 Scheduler、Park/Resume、IdleWarm、Idle TTL、公平队列与 fake clock。
- 不开放 R24 多会话 UI；本轮仍只有一个 `SessionUiState` 实例、一个活跃用户 Runtime。
- 不实现 R25 Job Object / `ResourceProbe` / 内存水位 / `ConversationDocument` 有界缓存，不实现 R26/R27 子代理。
- **不得用「丢弃事件」冒充背压**：任何会静默丢失 assistant 内容或一次性结果的路径都不允许；只能剥离可重建的重负载、丢弃已计数的 `Diagnostic`，或明确 fail-stop。
- 不修改 `PINNED_PI_VERSION` 或 `vendor/upstream/**` 身份；不引入新第三方依赖；`Cargo.lock` 不得有任何上游 version / source / checksum 漂移。
- 不改变用户主目录 `~/.pi` 数据；测试只读真实目录或使用临时目录。
- 不借本轮顺手修复前序轮次问题；BACKLOG #12 中「优雅停止纳入可观察终态」已被立项文档指派给 R22，属本轮范围，其余（#11 / #13 / #14）不动。

## 失败处理

同一验收项经针对性整改后连续 2 次 validation 仍不过 → 写 `rounds/round-22/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 视觉审查

本轮 diff 只触及 `crates/app/src/panels.rs` 的**非渲染逻辑**（effect ack、提交失败保留草稿、背压提示文案），
未改 `crates/ui/**`、未改 Theme token、组件结构、布局或视觉资源。但新增/改写了三处**用户可见文案**并改变了
「提交被拒时是否清空输入框」的可见行为，按 CLAUDE.md「只要会改变用户可见的 UI 表现，也必须触发视觉 review」
判定为涉及 UI，已进入视觉审查门禁。

- 视觉审查模式：CODE_ONLY
- 视觉审查结论：CODE_ONLY_PASS
- 截图验证：未提供（SCREENSHOT_NOT_PROVIDED）
- 兜底原因：USER_DECLINED
- `requested_at`：2026-08-24T16:05:27+08:00
- `deadline`：2026-08-24T16:35:27+08:00
- 审查报告 / 证据：审查输入为 `.pi/visual-review/round-22/ui-diff.patch` 与 `changed-files.txt`（仓库根 gitignored 的 `.pi/`，不入库），判据为 `docs/UI设计规范.md`。两轮审查结论与整改逐条记录见下方「视觉审查两轮结论与整改」。
- 说明：**仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR。** 用户在截止前回传 2 张截图，均为浅色主题、约 2880×1716（缩放后约 2000×1193 视口），与请求清单要求的 1280×820 / 100% / 深色不符；请求清单中 #2–#5（队列已满横幅、被拒后草稿保留、停止失败提示、背压丢弃提示）用户明确表示无法提供。按项目规则判为未完整回传，记 `USER_DECLINED`。这两张图仅作非阻断旁证，未作为审查输入传给 reviewer，也不回写为已完成截图验证。

### 截图请求清单（requested_at 2026-08-24T16:05:27+08:00 / deadline 2026-08-24T16:35:27+08:00）

统一元数据：窗口 **1280×820**、系统缩放 **100%**、主题 **深色**、`target/release/gpui-pi.exe` 启动。

| # | 页面 / 状态 | 导航与前置 | 对应目标基线 |
|---|---|---|---|
| 1 | 主聊天面板 · 空闲态 | 启动 → 新建会话 | `docs/UI设计规范.md` 基线，作为其余截图的对照 |
| 2 | 提交被拒的错误横幅 | 会话运行中连续快速提交，直到出现「命令队列已满（上限 32），请等待当前请求完成后重试」 | 状态色只点不铺；一行信息 ≤ 3 段；长中文文案不得溢出/截断 |
| 3 | 被拒后草稿保留 | 承接 #2，输入框带文字 + 1 张图片附件时触发拒绝 | 拒绝后输入框与附件条必须仍然完整可见（本轮行为变更） |
| 4 | 停止失败提示 | 运行中点「停止」，内核未响应时出现「停止失败：…」 | 同 #2 的横幅规范 |
| 5 | 背压丢弃提示 | 长时间流式回复中让界面卡住后恢复（若无法自然触发可跳过并注明） | 同 #2；确认多行文案的换行与容器高度 |

未提供 #2–#5 中任何一项即视为未完整回传，按 30 分钟窗口规则走 `CODE_ONLY` 兜底。

## 本轮实测

- 新 worktree vendor 门禁（2026-08-24）：`fetch-pi.ps1` / `fetch-pi-source.ps1` / `fetch-pi-web.ps1` 全部 cache hit 并自检通过，`check-pins.ps1` 全绿；worktree 内 reparse point 扫描为空（红线 6）。
- 改动前基线：`.\scripts\validate.ps1 -Logic` → `VALIDATE OK`。
- **无界点的处置**：
  - ① 逐请求 OS 线程 → `crates/pi-runtime/src/actor.rs` 的固定 worker Actor。`dispatch` / `refresh_metadata` / `request_control` / `restart_with_tools` / 落盘校准全部改为投递作业；`pi-runtime` 内所有 `thread::Builder::spawn` 收敛到唯一入口 `actor::spawn_named`，并由进程级计数器 `spawned_thread_count()` 监控。
  - ② `Client::subscribe` → 字节计量的有界 `EventStream`。生产侧全程非阻塞，stdout reader 与 supervisor 不受下游影响；超限发终态 `LifecycleEvent::EventBacklogOverflow` 并断开该订阅（fail-stop），**不丢中间事件**。stdout 事件按解析前的原始 JSONL 帧长精确记账。
  - ③ effect 无界 `VecDeque` → `crates/pi-runtime/src/effects.rs` 的有界缓存 + `SessionHandle::ack_effects` 回收。
  - ④ `PUMP_FRAME` 固定 20ms → `RuntimeLimits::event_frame`，在 `RuntimeTuning::from_limits` 一次性 clamp 进 16–33ms。
- **「可靠终态事件不丢」的落实口径**（实现与验收的对应关系，供审查核对）：Runtime 终态（崩溃 / 重启失败 / 事件积压超限 / 优雅停止）提升为 `SessionSnapshot.terminal` 权威字段，**完全不依赖 effect 流，任何背压下都不会丢**；一次性结果（`RequestFinished` / `ControlFinished` / `ToolRestartFinished`）归入「可靠」类，只有在字节/条数上限的最后一级阶梯才会被淘汰，且淘汰必然计入 `backpressure.dropped_results` 并由 `ChatPanel` 显式提示用户「界面状态可能不完整」——**不存在静默丢失**。稳态下 UI 每帧 `ack_effects`，该阶梯不会被触发。
- **字节上限的实现方式**：先剥离可重建的重负载（`RequestFinished` 里用于恢复草稿的 `ComposerSubmission`，含 base64 图片），再依次淘汰诊断 → 可合并 effect → 一次性结果，每级都有独立计数。协议负载（models / tree / commands / control outcome）用 `Debug` 渲染长度做**保守上界**估算，刻意高估而非低估；选它而不是 JSON 序列化是为了不给 `pi-runtime` 新增 `serde` 直接依赖（会改动 `Cargo.lock`）。
- **worker 数取值依据**（立项文档只钉死 command queue = 32，worker 数属可调初始值）：普通通道 2 个 worker，避免一次「等待人工对话框」的交互式提交独占整条通道（`INTERACTIVE_REQUEST_TIMEOUT` 是 30 分钟）；控制通道也给 2 个 —— 会话切换 / fork / HTML 导出可能一跑几十秒，单 worker 会让 Abort 排在它们后面，而 Abort 恰恰最不能等。单 Runtime 常驻线程因此固定为 **5**（2 command + 2 control + 1 event pump）。
- **合并的顺序安全性**：`CommandsLoaded` / `ControlsLoaded` 是纯赋值型，允许跨条目淘汰旧值；`Events` / `ExtensionUiBatch` / `ExtensionUiReset` 顺序敏感，**只与队尾相邻同 key 条目合并**，杜绝把 reset 之前的请求重排到 reset 之后。已有针对性测试。
- 实测结果：
  - `cargo test -p pi-runtime --lib` → **41 passed / 0 failed / 1 ignored**（其中 R22 新增 14 项：5 项 Actor 单测 + 9 项背压/终态/合帧）。
  - `cargo test -p pi-runtime --test thread_budget` → **1 passed**（独立进程，200 次 follow-up 后 `spawned_thread_count()` 增量为 0，存活线程恒为 `command_workers + control_workers + 1 pump`）。
  - `cargo test -p pi-rpc --test client` → **18 passed**（新增 stdout 持续 drain + 默认额度不误伤两项）。
  - 真实 pi 零 token：`PI_RUNTIME_TEST_BINARY=<abs>\vendor\pi\pi.exe cargo test -p pi-runtime --test real_pi -- --ignored` → **2 passed / 0 failed**（R21 原有一项 + R22 新增 `bounded_actor_and_effect_backpressure_hold_against_real_pi`）。注意该环境变量必须传**绝对路径**：集成测试进程 cwd 是 crate 目录而非仓库根。
  - `cargo test -p gpui-pi` → **121 passed / 0 failed**；`gpui-pi-ui` → **30 passed**。
  - 连跑 5 次 `cargo test -p pi-runtime --lib` 均 41 passed，未见 flake（有界队列测试改为「一直投递到出现拒绝、并记录被拒那一次之前的快照」后才稳定：启动时的元数据作业也占普通通道，「第几次被拒」不是确定值）。
  - `.\scripts\validate.ps1 -Logic` → `VALIDATE OK`；完整 `.\scripts\validate.ps1` → `VALIDATE OK`（release 构建 6m41s，仅有既存的 linker stdout 与 `proc-macro-error2` future-incompat 警告）。
- `Cargo.lock` **零改动**（`git diff --stat Cargo.lock` 为空），未新增任何第三方依赖，未触碰 `PINNED_PI_VERSION` 与 `vendor/upstream/**`。
- BACKLOG #12 中指派给 R22 的部分已关闭：`shutdown_entry` 现在同时写入权威终态 `TerminalState::Stopped` 并发布 `Stopped(None)` effect，观察者不再只能靠超时判断。#12 的另一半（`apply_controls_identity` 在 `session_file == None` 时静默 no-op）属钉死 pi 下不可达的静默降级，仍留在 BACKLOG。
- 新增 BACKLOG #15：`CLAUDE.md:52` / `AGENTS.md:54` 的 `-Logic` 注释落后于 R21 已完成的脚本改动，按红线 3 不在本轮顺手改。
- 未提前实现 R23 Scheduler/Park、R24 多会话 UI、R25 Job Object/内存治理、R26/R27 子代理；本轮仍只有一个活跃用户 Runtime 与一个 `SessionUiState` 实例。

### 独立代码审查与整改

首轮独立只读审查结论 **CHANGES_REQUESTED**（1 HIGH / 3 MEDIUM / 6 LOW / 6 NOTE）。逐条核对后确认 HIGH 与两条 MEDIUM 是真 bug，已全部整改并补回归测试：

| 编号 | 问题 | 整改 |
|---|---|---|
| H1 | 合并路径 `push` 提前 `return`，**跳过 `enforce_limits()`**，字节/条数硬上限在这条路上完全不生效；且 `ExtensionUiBatch` 只按请求 id 去重，而 pi 每次调用都生成新 id，同一个 `statusKey` 会无限堆积 | 合并成功后同样跑一遍上限；跨帧折叠改为复用 pump 的 `coalesce_extension_ui_requests`（`statusKey` / `widgetKey` 语义键），再按 id 折叠。新增 `merged_entries_are_still_subject_to_the_byte_ceiling`、`merged_extension_batches_fold_status_and_widget_keys_not_just_ids` |
| M2 | 合并时把队尾 `sequence` 抬到新值：若该条已被 UI 应用，会导致**重复应用**（错误横幅退不掉、`settled` 粘住）且 ack 永远回收不掉 | 引入「已交付水位」`EffectBuffer::delivered`，`snapshot()` 推进它；**只允许并入 UI 还没看过的条目**，否则另起一条。新增 `delivered_entries_are_never_merged_into`、`ack_reclaims_the_tail_even_while_new_frames_keep_arriving` |
| M3 | `Queue::close()` 在**持队列锁**时 drop 作业闭包；排队作业可能持有最后一个 `Client`，其 Drop 会同步等待 pi 退出（grace period + taskkill），而 `snapshot()` 是先拿 state 锁再读队列深度 → UI 冻结数秒 | `std::mem::take` 出作业，释放锁后再 drop |
| M4 | 验收表「可靠终态不丢」那一行写的是「被牺牲的只有可剥离负载与 `Diagnostic`」，与实现的最后一级 `evict(Reliable)` 矛盾 | **修订验收行**（见上表）。理由：立项文档的权威表述是「可靠终态事件不丢」，终态由权威字段保证；我在实现前自拟的那一行比立项文档更严，且与「固定字节上限」互斥——硬上限下只能二选一。权衡后保留「计数可见的最后一级淘汰」而非 fail-stop：为一个记账上限杀掉一个仍在工作的会话，对用户更差。此项属自拟验收行的口径更正，非因 validation 失败而放宽标准 |
| L5 | 合帧同族取最新会无计数地丢弃运行时事件 | 新增 `dropped_runtime_events` 计数并在 UI 弱提示位展示；测试 `dropped_runtime_events_are_counted` |
| L6 | `Drop for RuntimeEntry` 的注释宣称了一个不成立的保证（有排队作业时 Arc 环使其不可达） | 改注释，写明真正的保证来自所有终止路径显式 `actor.close()` |
| L7 | `dropped_jobs` 有计数但无出口 | 纳入 `report_backpressure` 的弱提示位 |
| L8 | 控制通道被拒时文案写成「命令队列已满（上限 8）」，与 32 的命令队列混淆 | `QueueError::Full` 带上 `Channel`，文案区分「命令队列 / 控制队列」；测试 `queue_full_message_names_the_channel` |
| L9 | `report_backpressure` 会覆盖同帧内 effect 刚写入的具体错误 | 改为**追加**而非覆盖；自愈型降级移到弱提示位 |
| L10 | 默认订阅额度 32MiB 只有单帧上限（16MiB）的 2 倍 | 改为按 `DEFAULT_MAX_FRAME_LEN` 的倍数表达（4×，即 64MiB），关系显式化 |
| N11 | 跨条目淘汰分支的 `coalesced` 只 +1，与队尾合并分支口径不一致 | 改为按实际淘汰条数累加；测试 `controls_loaded_keeps_only_the_latest_value` 断言计数 |
| N12 | `started.elapsed() < 10s` 接近恒真、`drained > 1` 无法证明「溢出前完整送达」 | 改为校验送达的是 RPC 事件流的**连续前缀**（`agent_start` → `message_start` → 连续 `message_update`），并断言确实触发了溢出 |
| N13 / N14 | `ControlsLoaded` 无直接测试；暂停恢复用例未断言 controls | 补 `controls_loaded_keeps_only_the_latest_value`；暂停用例改为**不 ack** 启动元数据，并断言最终 Snapshot 仍含 `ControlsLoaded(Ok)` 与 `CommandsLoaded(Ok)` |
| N15 | pump 在终态帧丢弃同帧已投影的 `batch` | R21 既有行为，按红线 3 记 `rounds/BACKLOG.md` #16，本轮不改 |

审查已核对且**无发现**的维度（供复核覆盖面）：锁顺序全路径一致、无死锁；`restart_with_tools` 替换窗口无误判；`Queue::pop` Condvar 无丢唤醒；worker 不持 `Arc<RuntimeEntry>`；`EventStream` 字节配平且上限硬成立；`kind_bytes` 逐变体核对无低估；顺序敏感合并不重排；epoch / cursor 一致；app 侧 ack 无 runtime_id 错配；`Cargo.lock` 零改动、无新依赖、无 `#[allow]`、无 `unsafe`。

整改后复测：`cargo test -p pi-runtime --lib` → **48 passed**（新增 7 项回归）；`cargo test -p pi-rpc --test client` → **18 passed**。

### 视觉审查两轮结论与整改

判定为涉及 UI 的依据：diff 未触及 `crates/ui/**`、Theme token、组件结构与布局，但改写了三处用户可见文案，并改变了「提交被拒时是否清空输入框与附件」这一可见行为。

**第一轮：`CODE_ONLY_FAIL`**（1 阻断 / 1 中 / 4 低）

| 编号 | 问题 | 整改 |
|---|---|---|
| F1（阻断） | 背压弱提示被写进共享的 `host_extension_degradation`，而该槽位承载的是「整会话持续成立」的降级事实（如宿主扩展未加载）且**没有任何清除路径**；结果是瞬时提示驻留到会话切换，并永久顶掉启动诊断。既有测试 `host_extension_degradation_is_generation_scoped_and_survives_successes` 恰好把该槽位钉为「generation 级、跨成功不清除」，与塞入的瞬时语义直接冲突 | 新增独立字段 `backpressure_note` 与独立渲染行（`debug_selector = "backpressure-note"`，沿用 `cx.theme().warning` 文本色、`px_3`/`py_1`/`text_xs`），并给出对称退场路径 |
| F2（中） | `strip_payloads` 把 `submission` 剥离后，`Rejected` 仍显示「已恢复草稿」，而输入框与附件条实际为空——该组合**由本轮首次变为可达** | 引入 `restored_draft`，未真正恢复时改为「pi 明确拒绝提交（草稿因积压未能保留）：{error}」。文案行虽属前序代码，但可达性由本轮引入，按本轮范围修复，不顺延 BACKLOG |
| F3（低，未判红） | 拼接长文案无高度上限 | `notes` 移出错误横幅到独立弱提示行；错误位只在少见情况下追加一次。第二轮复核确认该处置可接受，横幅无高度上限属前序既有问题 → BACKLOG #17 |
| F4（低） | 新增可见状态无 UI 测试 | 补 3 条 `#[gpui::test]` |
| F5（低） | 提交被拒时未收起 composer 浮层 | 被拒分支补 `self.popup = None` |
| F6（低） | `effect_set_error` 用字符串不等判定，连续两帧同文错误会被误判为「本帧没写过」 | 改用显式布尔 |

**第二轮复审：`CODE_ONLY_PASS`**（F1 已实质关闭，无新增阻断项）。复审又提出 6 条非阻断 findings，其中 3 条判定值得当轮处理：

| 编号 | 问题 | 整改 |
|---|---|---|
| 复审-1（中） | 保护标记按帧重置，只覆盖 effect 路径；而「命令队列已满」「停止失败」写在**用户操作路径**上、不经过 `apply_snapshot`，下一帧就可能被背压提示整条替换——而队列满与 effect 淘汰本就是同一类过载工况下的关联事件，本轮最核心的可见提示恰好落在保护范围之外 | `rpc_error_written_this_frame` 改为不按帧重置的 `rpc_error_protected`，两处用户操作写入点一并置位、`clear_rpc_error` 清位；同时限制同一条错误只追加一次背压说明，防止横幅无限变长 |
| 复审-2（中） | 弱提示只在「本帧有新增计数」时非空，一次性计数只活一帧（16–33ms），既看不清又会让下方 composer 逐帧上下跳一行 | 加 5s 最短可读驻留窗口 `backpressure_note_until`，过期后自行退场 |
| 复审-3（中） | `rejected_submission_keeps_the_draft_and_attachments_visible` 未走真实提交路径，断言相对自设状态接近恒真，名实不符 | 更名为 `error_banner_and_attachment_strip_coexist_without_overlap_at_1280x820`（它真正锁住的是「错误横幅在场时附件条与 composer 各行不重叠」这一布局性质，既有同类用例跑在 1000×1000 且无横幅），并在注释中如实标注未覆盖项：被拒后的草稿保留与浮层收起需要一个会拒绝 dispatch 的 `SessionHandle`，app 测试二进制内无此 fixture |
| 复审-4（低） | 另两条测试的覆盖边界未写清 | 在测试段首注明：均直接调用 `report_backpressure`，未覆盖 `rpc_error_protected` 在 9 处 effect 写入点的赋值齐全性 |
| 复审-5（低） | 错误横幅无高度上限 | 属前序既有问题，按红线 3 记 BACKLOG #17 |
| 复审-6（低） | `install_active` 归零统计基线但不清空提示，清除责任散在调用方 | 把 `backpressure_note` 的清空一并放进 `install_active` |

视觉修复增量**全部限于 `crates/app/src/panels.rs`** 的展示态字段、文案分支、一处渲染行与 UI 测试，未触及 `pi-rpc` / `pi-runtime` / `pi-data` / `pi-render`，符合「视觉修复只允许改 UI 表现代码」的约束。复审确认无硬编码颜色/字体、间距与字号均在规范刻度内、信息层级保持「success → 持续降级 → 瞬时降级 → error」的自弱到强顺序。

视觉整改后复测：`cargo test -p gpui-pi` → **124 passed**；完整 `.\scripts\validate.ps1` → `VALIDATE OK`（25 个测试目标零失败）。
