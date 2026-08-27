# Round 25 — Windows Job Object + 进程树与内存治理

<!-- 保存为 rounds/round-25/round-25.md；该轮其他管理产出也放在同一目录。 -->

> 执行方：Windows · 状态：已完成 · PR [#35](https://github.com/cking000bigdemon/GPUI-Pi/pull/35) · CI 阻断 job 通过

## 目标

每个 Runtime 的 pi 进程树被内核级 Job Object 兜住：进程数与内存有硬上限、整树可统计可终止、关闭 Runtime 不残留任何工具/扩展孙进程；内存水位由**可注入的 `ResourceProbe`** 驱动，高水位下先回收 IdleWarm、再阻止后台会话启动，策略层用 fake probe 做确定性测试，不依赖真机内存曲线。

## 前置

- R21–R24 已完成：`pi-runtime` 的 `RuntimeManager` 是 app 内建 RPC Runtime 的唯一生产创建入口，Scheduler / warm pool / Park-Resume / 多会话 UI 均已接线。
- 新 round 启动门禁已过：`vendor/pi/pi.exe`(0.84.2)、`vendor/upstream/pi-0.84.2/`、`vendor/upstream/pi-web-0.8.9/` 均在本 worktree 独立准备完成，`check-pins.ps1` 全绿（2026-08-26）。
- **`unsafe` 口径（本轮特批）**：项目所有者于 2026-08-26 批准**限定豁免** —— Win32 FFI 所需的 `unsafe` 只允许集中在单一平台 FFI 模块内，每处写明安全性理由；模块之外一律安全 Rust。此前仓库已有同类先例（`crates/pi-data/src/fs_util.rs`、`crates/app/src/model_service.rs`）。不引入安全封装 crate（会改 `Cargo.lock`，触红线 2），也不降级为 `taskkill /T` 方案（拿不到内核级硬限制，等于放弃本轮核心增量）。
- 依赖边界：Job Object 与进程内存查询 API 均在已钉定的 `windows-sys 0.61` 内，只增加 feature。

## 交付物

### A. Job Object 平台层（`crates/pi-rpc/`）

- `crates/pi-rpc/src/platform.rs`（新增）—— 本轮**新增 `unsafe` 的唯一落点**：
  - 匿名、不可继承的 Job Object；`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 永远打开，句柄关闭即整树终止；
  - `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` / `JOB_OBJECT_LIMIT_JOB_MEMORY` 硬上限；**不开 breakaway**，子孙进程无法逃逸；
  - `QueryInformationJobObject`：活跃进程数、峰值 job 内存、整树 pid 列表 + 逐进程私有提交内存；
  - `TerminateJobObject` 整树终止，取代按 PPID 链走的 `taskkill /T`；
  - `GlobalMemoryStatusEx` 系统内存采样（放在同一模块，以维持"unsafe 只有一处"）；
  - **assign 竞态已消除**：`CREATE_SUSPENDED` 创建 → `AssignProcessToJobObject` → Toolhelp 线程快照 `ResumeThread`。挂起期间 pi 一条指令都没执行，因此不是"概率很小"，是不可能。
  - 非 Windows：`stats` / `terminate` / `process_ids` 明确报 `Unsupported`，由调用方回退 `kill_process_tree`，不假装成功。
- `crates/pi-rpc/src/process.rs`：`ClientConfig::job_limits`；`Shared` 持当前 job；`Client::kill_process_tree` 优先走 job；新增 `Client::process_tree_stats()` / `process_tree_pids()`；进程退出后**先整树回收再 join reader**（见「本轮实测」的缺陷 ①）。
- `crates/pi-rpc/tests/fixtures/fake_child.rs`：新增 `--grandchild-sleep` 与 `PI_RPC_FAKE_SPAWN_GRANDCHILD`，模拟"扩展自己 spawn 子代理"。

### B. `ResourceProbe` 抽象与内存水位策略（`crates/pi-runtime/`）

- `crates/pi-runtime/src/resource.rs`（新增）：`ResourceProbe` trait、`SystemResourceProbe`、`FakeResourceProbe`、`MemoryLimits`、带**迟滞 + 采样节流**的 `MemoryGovernor`、`MemoryPressureReport`。
- `crates/pi-runtime/src/lib.rs`：`RuntimeLimits::memory`；`RuntimeTuning` 携带 `JobLimits` 并在 `clamp_manager_config` 这个**唯一落点**透传给 `pi-rpc`（用户会话 / 内建子代理 / maintenance 导出一视同仁）；`RuntimeManager::with_test_clock_and_probe`；`memory_limits()` / `memory_pressure()`；`admit` 接入水位判定（采样在**调度锁之外**完成）。
- 策略：高水位 → 先回收最久空闲的 IdleWarm；无热进程可回收时后台会话转 `Queued`，前台仍放行；采样拿不到数据一律放行。

### C. 显式关停入口（关闭 BACKLOG #19 / #25）

- `RuntimeManager::shutdown_all() -> ShutdownReport`：注销全部会话 Runtime + 清空 warm pool，幂等，关停后 Manager 仍可用。沿用既有 `remove_session` 路径，**未新增**"把 client 交给在途作业同时置空 `state.client`"的捷径，因此不触发 BACKLOG #23 的 `ClientOwnerGuard` 记账要求。
- `crates/app/src/main.rs`：窗口全关时显式调用，再 `cx.quit()`。

### D. 文档聚合有界缓存（`crates/pi-render/`）

- `crates/pi-render/src/budget.rs`（新增）：`apply_payload_budget` / `payload_bytes` / `DEFAULT_PAYLOAD_BUDGET_BYTES`（16MiB）。
- 只裁**过程性负载**（图片字节 + 工具输出），从最旧的消息开始释放；对话正文（用户 Query、最终 Answer、thinking 文本）一概不动。
- 释放**不是静默丢弃**：图片转 `ImageState::Redacted` 并在说明里写明原因，工具输出换成可见占位文案，卡片的 `preview` 与 `status` 保留。
- 接入两处：`render_session`（静态加载，**在投影之前**，否则 `items`/`minimap` 里留的还是没裁过的 `Arc`）与 `LiveSessionReducer`（流式段，含 `rerender_completed_tools` 之后的重新压制）。

### E. 轮次文档

- 本文件；`rounds/BACKLOG.md` 关闭 #19 / #25，新增 #29–#31；收口时更新仓库根 `ROUNDS.md`。

## 验收

| 级别 | 检查 | 结果 |
|---|---|---|
| T1 | 全量 validation | ✅ `.\scripts\validate.ps1` → `VALIDATE OK`（check-pins / fmt / clippy -D warnings / test / build --release 全绿） |
| T2 | Job Object：孙进程入 job、整树终止、句柄关闭即整树终止、active-process 硬限制拦住扩展 spawn | ✅ `crates/pi-rpc/tests/job_object.rs` 3/3 |
| T2 | 水位策略确定性验收（fake probe，不读真机内存） | ✅ `crates/pi-runtime/tests/memory_watermark_fake_child.rs` 5/5 |
| T2 | 显式关停（外部仍持 3 个 `SessionHandle`）+ 幂等 | ✅ `crates/pi-runtime/tests/shutdown_all_fake_child.rs` 2/2 |
| T2 | 文档聚合预算：最旧优先释放、正文不动、幂等、旧快照不被改写 | ✅ `crates/pi-render/src/budget.rs` 6 条单测 + `tests/live_reducer.rs` 2 条流式用例 |
| T3 | 真实 pi 整树治理 + 显式关停无残留 | ✅ `real_pi_trees_are_governed_and_shutdown_all_leaves_no_survivors`（零 token），数字见下 |
| T3 | 多会话内存实测 → 标定水位初值 | ✅ 见「本轮实测」 |

## 禁止

- 不改 `Cargo.lock` 里的任何版本、不新增 crate（只给 `windows-sys` 加 feature）。
- 新增 `unsafe` 只允许出现在 `crates/pi-rpc/src/platform.rs`。
- 不开放 R26 的内建子代理任务与配额调度，不做 R27 的 worktree writer 隔离。
- 不顺手修 BACKLOG 中非本轮（#19 / #25）的条目。
- 不改 RPC 协议、会话文件格式与钉死上游身份。
- 不把配置初值硬编码成不可变产品契约。

## 失败处理

连续 2 次 validation 不过 → 写 `rounds/round-25/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 代码审查

- 通道：**codex 插件**（`codex@openai-codex` 1.0.6，`disable-model-invocation: false`，主会话直接调用 `/codex:review --background --scope working-tree`）。Claude Code harness 的首选路径，未降级。
- 前置：发起时本轮 diff 已收敛，全量 validation 已两次全绿。
- 第一轮结论：**5 条 findings（3×P1、2×P2）**，逐条核对后**全部成立**，全部整改，无一条以「不成立」结案。

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 1 | P1 | 只裁 `rendered`，`CompletedMessage.value` 与 `tools[*].result` 的原始副本仍无界 —— 等于只把统计做小 | **推翻我自己的判断**：原本记进 BACKLOG #29 推迟，实际是欠范围。当轮修复：按 `RetentionOutcome::released_upto` 同步裁原始副本；新增 `LiveSessionReducer::live_raw_bytes()` 作为客观判据，补 `releasing_payload_also_drops_the_raw_copies_behind_it` |
| 2 | P1 | `shutdown_all` 可能在在途 `restart_with_tools` 仍攥着 `Client` 时就返回，紧接着 `cx.quit()` | 新增 `wait_until_drained`（预算 10s，真实时钟），`ShutdownReport.drained` 如实上报；超时不死等，靠 `KILL_ON_JOB_CLOSE` 兜底但记明少了一次优雅收尾 |
| 3 | P1 | 水位只在 admit 时采样：外部负载压高内存且无人申请运行时，热进程等到 TTL 才回收、`memory_pressure()` 停在旧值 | `tick()` 每次都在锁外采样；高水位下热进程不等 TTL 直接回收，新增 `SchedulerReport::pressure_reclaimed` 区分回收原因；补 `rising_pressure_reclaims_warm_runtimes_without_any_admission` |
| 4 | P2 | 被水位挡下的后台会话堵在队首时，`promote_queued` 就此收手，本该绕过水位的前台会话跟着饿死 | 新增 `WaitQueue::peek_next_excluding`（跳过而非出队再入队，不动 aging）；`promote_queued` 记录本轮推不动的条目并继续看下一条；补 `a_pressure_blocked_background_entry_does_not_starve_a_queued_foreground_one` |
| 5 | P2 | 占位文案承诺「重新打开会话即可查看」，但重开会走同一份确定性预算再裁一遍 —— 假承诺 | 改写文案为只陈述事实（原始内容在磁盘会话文件里、界面暂不提供回看），并在模块文档写明真正的回看要按需 rehydrate（BACKLOG #31） |

### 第二轮 codex 审查

第一轮整改后重跑 `/codex:review`，**又出 3 条（1×P1、2×P2），全部成立**，且三条都是我自己整改引入或未覆盖到的 —— 记在这里而不是轻描淡写带过：

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 6 | P1 | 渲染后没有 `bytes` 的图片（无效 / 超限 / 已脱敏）在 `payload_bytes` 里记 0，可那坨 base64 还完整躺在 `value` 里 —— 一串这种消息能让预算永远触发不了 | 预算改为按**实际驻留内存**计：渲染负载 + 原始副本 + 引用到的工具结果（`retained_bytes`）；`upsert_completed` 的快门也连原始副本一起记。补 `raw_image_payload_alone_is_enough_to_trigger_a_release` |
| 7 | P2 | 占位文案本身占字节，把一条 `"ok"` 换成一整句说明反而更大：`freed` 饱和成 0，既丢了有用输出、`total` 又降不下来 | 释放前先比大小，比占位还短的输出/图片一律留着（`release_payload` / `release_image` / `release_tool_result` 三处都加了判据） |
| 8 | P2 | 高水位下 `tick()` 清空 warm pool，而 `request_run` 先 tick 再 admit —— 把「Resume 优先复用 `switch_session`」整条路径打掉了。**这是第一轮修 P1-3 引入的回归** | `tick_inner(reclaim_under_pressure)` 拆开：`request_run` 前那趟传 `false`；周期性 tick 里把 `promote_queued` 排到水位回收**之前**，让排队会话先有机会复用。补 `pressure_does_not_defeat_warm_reuse_on_resume`（断言 pid 不变 + `warm_resumes` 递增 + `cold_starts` 不变） |

### 第三轮 codex 审查

第二轮整改后再跑，**又出 6 条（3×P1、3×P2）**：

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 9 | P1 | `stop_session` 收尾会 `tick()`，批量关停途中照常提升队列 —— 会把排队会话一个个冷启动再一个个关掉，每个付一次 grace period | `ManagerInner.shutting_down` 置位期间 `promote_queued` 直接返回；`shutdown_all` 动任何会话**之前**先清空队列。补 `shutdown_never_cold_starts_the_sessions_still_waiting_in_the_queue` |
| 10 | P1 | 抢到槽但 `finish_start` 还没挂进调度器的那段窗口里，entry 与 lease 只活在局部 `Admission` 里，`draining` 看不到 → 屏障会提前宣布已排空 | 新增 `PendingStartGuard`（**闭包外调用点持有**，与 BACKLOG #23 同构），屏障加等 `pending_starts == 0` |
| 11 | P1 | Cargo.lock 那一行违反红线 2 / 立项文档依赖边界 | **提请项目所有者裁定**：codex 的前提「只加 feature 不改 lock」对新增依赖边不可实现（lock 按 package 记依赖清单）。所有者 2026-08-26 选择修订立项文档措辞为「不新增 package、不改任何版本」，已在 § 七加勘误块。现状零版本漂移、`check-pins` 全绿 |
| 12 | P2 | `retained_bytes` 把用户 Query / 最终 Answer 的正文也算进预算，而释放从不碰它们 → 长文本会话永远超预算，每插一条就全量重扫，退化成 O(n²) | 预算只数**可释放**的原始字节（`releasable_raw_bytes`：内嵌图片数据 + 按 id 累加的工具结果）；另加"卡死护栏"：一趟什么都没释放就把触发线抬到「再涨一个预算」。补 `text_only_conversations_do_not_drag_the_budget_over` |
| 13 | P2 | 二轮加的"逐条比大小"判据会把每条短输出都跳过，几万条的总量永远超预算却一条都释放不掉 | 判据从逐个输出上移到**整条消息**（`release_cost`）。补 `many_short_tool_outputs_are_still_released_in_aggregate` |
| 14 | P2 | 历史 HTML 导出这类 maintenance Runtime 只由 `MaintenanceGate` 表示，不在会话 / warm / draining 里 → 屏障会在导出还在写盘时宣布已排空 | 屏障加等 `MaintenanceGate::in_flight() == 0` |

### 第四轮 codex 审查

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 15 | P1 | 工具结果常在 `MessageEnd` 之前就到，`upsert_completed` 的快门只加了「渲染负载 + 本条图片数据」，漏了 `tools[*].result` —— 快门说「还没到预算」，全量统计其实早就超了 | 快门改为**直接复用** `retained_bytes(index)`，与全量统计同源，杜绝两套算法对不上 |
| 16 | P1 | `shutting_down` 只拦队列提升，不拦直接准入：并发 `request_run` 能赶在快照之后把新 Runtime 起来，不在关停名单里 | `admit_and_start` 开头即拒绝准入并返回明确错误；`shutdown_all` 改为**循环**到「会话表与 warm pool 都空且已排空」或撞上同一个 deadline |
| 17 | P1 | `ResourceProbe` 只能注入系统内存，per-Runtime 进程数与整树内存仍直连 `JobObject::stats()` —— 立项文档要求的是「内存**与进程数**采样经可注入抽象」 | trait 增加 `process_tree(&dyn Fn() -> Option<JobStats>)`（默认转发真实取数口）；`RuntimeEntry` 持 probe，`SessionHandle::process_tree_stats` 经它转发；`FakeResourceProbe` 可预置采样值、也可模拟采样失败。补 `process_tree_sampling_goes_through_the_injectable_probe` |
| 18 | P2 | `calibrate` 只复位 `live_payload_hint`，没复位被抬高过的 `enforce_threshold`，新一段流会在远超预算之后才开始受管 | 一并复位为 `payload_budget` |

### 第五轮 codex 审查

**4 条，全部 P2、无 P1** —— 严重度开始收敛。

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 19 | P2 | `ToolCard.details`（编辑类工具的整份 patch）既不计入预算也不释放，常常比输出本身还大 | 计入 `payload_bytes` 并随输出一起清空。确认 `crates/ui` / `crates/app` 对 `details` **零引用**，释放它没有视觉代价。补 `tool_details_are_counted_and_released_with_the_output` |
| 20 | P2 | 跨消息的短输出聚合仍可能停在预算之上 | **部分不采纳并写明理由**：这是结构性下界（每条带工具卡片的消息至少要付一份占位），强行替换只会既丢有用输出又不省内存。触到 16MiB 需要约 16 万条此类消息，实际不可达。已记 BACKLOG #34，含缩短占位文案这条真正的改进方向 |
| 21 | P2 | 流式段的 `release_message_at` 直接调 `release_payload`，**绕过**了整条消息的大小判据 —— 一条 `"ok"` 被换成更长的占位，输出丢了、内存没省，原始侧还因"换了不划算"拒绝替换，两头落空 | 抽出 `is_worth_releasing`，静态与流式两条路径共用同一判据。补 `short_live_outputs_survive_when_releasing_them_would_not_save_anything` |
| 22 | P2 | `export_historical_html` 不受 `shutting_down` 约束：赶在最后一次采样之后拿到 permit 的导出，会在 `drained = true` 之后才 spawn 进程 | 关停期直接拒绝新的 maintenance 作业 |

### 第六轮 codex 审查

**3 条（1×P1、2×P2），三条都是我自己修复里的精确竞态/遗漏。**

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 23 | P1 | `PendingStartGuard` 在**放开调度锁之后**才记账：`admit` 标 `Starting` 到守卫加一之间有一道缝，`shutdown_all` 恰好跑完就会同时看到 `draining` 与 `pending_starts` 都空，宣布已排空 —— 而这次准入还攥着常驻 lease | 守卫改到**取锁之前**获取，并把关停标志的检查放到守卫之后：要么关停看得见我们、等我们交还，要么我们看得见关停、立刻退出 |
| 24 | P2 | `export_historical_html` 是「先查标志、再去 acquire」，两步之间的缝足够让一次导出在屏障采完样之后才开工 | `MaintenanceGate` 的 state 改为 `(在跑数, 是否关闭)` 同锁保护，`acquire` 在锁内判定并返回 `Option`；`shutdown_all` 单独 `set_closed`。一旦返回 `Some`，在跑数**已经**加过，之后任何 `in_flight()` 都看得见 |
| 25 | P2 | 「输出为空、详情很大」的工具卡片（合法空结果 + 非 patch 结构化详情）在 `output.is_empty()` 的 `continue` 上溜走：预算算得到它、却永远释放不掉 | `details` 改为**无条件先放掉**，再判断要不要动 `output`。补 `tool_details_are_released_even_when_the_output_is_empty` |

### 第七轮 codex 审查

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 26 | P1 | `park` 的第 ③ 步之后 entry 与 lease 已从 `SchedulerCore` 摘走、client 还攥在栈上，这一刻谁也看不见它；关停屏障恰在此时采样就会宣布已排空，随后 park 把一个**活着的** client 塞进 warm pool | 新增 `PendingHandoffGuard`（与启动守卫同构，分开计数便于诊断），屏障加等 `pending_handoffs == 0` |
| 27 | P1 | 工具结果常先于 `MessageEnd` 到达，消息也可能永远不来（run 被取消）；这类结果挂在 `tools` 里既不被计入也永远释放不掉 | 预算改为 `total_retained_bytes()` = 完成消息 + **孤儿工具结果**；释放顺序把孤儿排在最后（它们可能马上被一条到来的 `MessageEnd` 用上）。补 `orphan_tool_results_are_counted_and_released` |
| 28 | P2 | `with_test_clock` 把 fake clock 与**真实**内存探针配在一起，而水位默认启用 —— 宿主可用内存不足 1.5GiB 时，R23/R24 那批用 BACKGROUND 优先级的既有用例会变成看宿主脸色的 flake | 改为注入 `FakeResourceProbe::unavailable()`：采样恒报"拿不到数据"，水位恒不生效，注入时钟的用例回到"只依赖 fake clock"。要测水位请显式用 `with_test_clock_and_probe` |
| 29 | P2 | 准入路径上因水位淘汰热进程时不加 `pressure_reclaimed`，统计口径只覆盖周期性回收那一路 | 在该分支补计数 |

七轮合计 **29 条 findings**：27 条自行核实成立并整改，1 条（#11）提请所有者裁定后按裁定执行，1 条（#20）经核算为结构性下界、**部分不采纳并写明理由与不可达性论证**。

> #28 值得单独记一笔：它是**我的改动让别人家的测试变脆**——给 `with_test_clock` 默认挂上真实探针，等于把 R23/R24 那批本来只依赖 fake clock 的用例悄悄绑上了宿主内存。这类"不改前序代码却改坏前序验收"的回归，比改坏自己写的东西更难发现。

### 第八轮 codex 审查

**1 条，P1** —— 第七轮修复的精确缺口。

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 30 | P1 | 孤儿工具结果虽已计入预算，**触发路径却没接上**：`rerender_completed_tools` 只在 `changed` 为真时才作废快门，而结果先于 `MessageEnd` 到达、或消息因取消永远不来时压根没有卡片可重渲染，`changed` 恒为 false —— 计入了却永远等不到一次压制，等于没有上界 | 改为**无论是否重渲染过**都作废快门并压一次预算。`rerender_completed_tools` 本就是 O(n) 遍历，不改变量级。测试同步加强：预算一开始就设好、之后不再碰，证明是孤儿结果**自己**触发的压制，而不是外部又调了一次 `set_payload_budget` 顺带裁掉的 |

### 第九轮 codex 审查

**1 条，P1** —— 与第七轮 park 那条同类。

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 31 | P1 | `stop_session` 摘走 `slot.entry` 之后、lease 进 `core.draining` 之前，进程句柄只活在调用栈上；reaper 关热进程时同样如此。屏障此刻采样会宣布已排空，退出于是跳过优雅收尾、改由 Job Object 直接终止，pi 可能来不及把会话文件写完 | `stop_session` 全程持 `PendingHandoffGuard`；`tick_inner` 的 TTL 回收与水位回收两个关进程循环、以及 `shutdown_all` 的 warm 关闭循环各自加同样的记账 |

**顺手做了一次穷尽核对**，不再等审查一条条挑：`pi-runtime` 里 11 个进程拆除点逐个过了一遍，其余各自已被覆盖 —— 准入/淘汰/接管失败在 `PendingStartGuard` 作用域内（`pending` 在 `Evict` 分支仍然存活），换进程作业走 `ClientOwnerGuard` + `draining`，历史导出走 maintenance permit，崩溃路径的进程本就已经死了、由 `reap_terminated` 收 lease。这一族至此完整。

### 第十轮 codex 审查

**1 条，P2、无 P1。**

| # | 级别 | findings | 处理 |
|---|---|---|---|
| 32 | P2 | `shutting_down` 是**标志不是所有权令牌**：两个 Manager clone 并发关停时，先跑完的那个把闸门清掉，而后一个还在关停途中 —— `request_run` 于是能在它快照完会话表之后又起一个 Runtime。maintenance 闸门的开合同理 | 加 `shutdown_lock`，`shutdown_all` 全程持有，把整个关停串行化，一次性消掉这一类（两个闸门的开合都被它覆盖）。补 `concurrent_shutdowns_do_not_reopen_the_gate_for_each_other`：4 线程并发关停，断言每次都 `drained`、三个会话总共只被注销一次、结束后零常驻 |

### 第十一轮 codex 审查 —— 收敛

> No discrete, actionable defects were found in the current staged, unstaged, or untracked changes.

### 审查总账

| 轮 | findings | 严重度 |
|---|---|---|
| 1 | 5 | 3×P1 + 2×P2 |
| 2 | 3 | 1×P1 + 2×P2 |
| 3 | 6 | 3×P1 + 3×P2 |
| 4 | 4 | 3×P1 + 1×P2 |
| 5 | 4 | 0×P1 + 4×P2 |
| 6 | 3 | 1×P1 + 2×P2 |
| 7 | 4 | 2×P1 + 2×P2 |
| 8 | 1 | 1×P1 |
| 9 | 1 | 1×P1 |
| 10 | 1 | 0×P1 + 1×P2 |
| **11** | **0** | **收敛** |

合计 **32 条 findings**：30 条自行核实成立并整改，1 条（#11）提请所有者裁定后按裁定执行，1 条（#20）经核算为结构性下界、**部分不采纳并写明理由与不可达性论证**。**无一条以「不成立」草草结案**。

其中 **6 条是我自己整改引入或欠范围的**（#1 欠范围、#8 第三轮修复引入的回归、#21 绕过自己刚加的判据、#23 / #26 / #31 同一族的在途记账盲区、#28 让前序轮次的测试变脆）。这些都单列在各轮表格里，没有混在"审查发现的问题"里含糊过去。

> **未以确定性测试覆盖的部分（如实记录）**：#16 的并发准入拦截、#10 的 `pending_starts` 屏障、#14 的 maintenance 屏障都是**竞态护栏**，要稳定复现需要在生产路径里插同步点。本轮以「构造上成立 + 审查确认」结案，没有为它们写会 flake 的压力测试 —— 写一个时灵时不灵的用例，比不写更糟（红线 4 防的正是这种反复不过）。三者的正确性依据写在各自代码注释里。

> 顺带记两条工程教训：
> 1. 整改期间一次批量文本替换因 `cargo fmt` 已把长常量折行而静默落空，而脚本里的 `assert s != o` 只校验「整个文件有变化」，被同批其他替换掩盖过去了。后续改为**逐条替换逐条断言**。
> 2. 第一轮修完 P1-3（tick 采样水位）直接引入了 finding 8 的回归。教训是：给一条周期性路径**加**副作用时，要把所有调用点（这里是 `request_run` 的前置 tick）都过一遍，而不只看新加的那段逻辑自洽。

## 视觉审查

- 视觉审查模式：`CODE_ONLY`
- 视觉审查结论：`CODE_ONLY_PASS`（**非独立**，见下方「审查器独立性」）
- 截图验证：未提供（`SCREENSHOT_NOT_PROVIDED`）
- 兜底原因：`USER_DECLINED`
- `requested_at`：N/A（用户在截图请求窗口开启前即选定 CODE_ONLY 路径，按 CLAUDE.md「用户明确拒绝时立即进入 CODE_ONLY，无需等待截止」）
- `deadline`：N/A
- 审查报告 / 证据：本节
- 说明：**仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR。**
- 复核：视觉审查在第 6 轮代码审查后完成，此后第 7–10 轮又改了代码，已逐条复核对可见表现的影响 ——
  `details` 释放（`crates/ui` / `crates/app` 零引用，不可见）、孤儿结果处理（不可见）、在途记账与关停串行化（不可见）；
  唯一有可见影响的是 `is_worth_releasing` 现在会**保留**短输出，占位文案变少，方向上只会更好。结论不变。

### 审查器独立性（必须如实记录）

本轮**没有**按 CLAUDE.md 派发 `.agents/visual-reviewer.md` 子代理，原因有二：
① 本会话（Claude Code harness）被明确约束「未经用户要求不得调用 Agent 工具」；
② 该 harness 未注册 `visual-reviewer` 代理类型。

项目所有者 2026-08-26 裁定：由主会话按 `.agents/visual-reviewer.md` 的口径执行 CODE_ONLY，并写明非独立。
因此本结论**不满足协议要求的「审查器与 writer 隔离」**，其证据强度低于标准 `CODE_ONLY_PASS`。
若后续需要标准结论，应在支持该代理类型的 harness 内重跑。

### Compared Evidence

- fallback reason：`USER_DECLINED`；`requested_at` / `deadline`：N/A
- 当前 diff：本 worktree 工作区 diff（`crates/ui/**` **零 diff**；`crates/app/**` 仅 `main.rs` 窗口全关回调，非视图/布局/样式代码）
- 变更文件清单：见「交付物」
- 规范：`docs/UI设计规范.md`（S-1、S-8、S-16、§ 5.2、§ 5.3）
- 任务卡：本文件
- 相关渲染代码（只读）：`crates/ui/src/chat.rs`（`subordinate_column`、工具卡片展开区、`render_image` 占位分支）
- 截图验证：**未提供**

### Findings

1. **（非阻断）释放后的工具输出与普通工具输出在视觉上无法区分** —— `crates/ui/src/chat.rs:1301`
   - 可见状态：工具卡片展开态，某条旧输出已被预算释放
   - 规范依据：`docs/UI设计规范.md` § 5.3 要求工具输出区整体 `text_color(cx.theme().muted_foreground)`；S-16 把「元信息 / placeholder」归到 2 次文本档
   - 静态风险：`ToolOutput::Text` 分支是 `div().text_xs().child(text)`，**未设 `text_color`**，继承卡片默认前景色。因此本轮新增的占位文案会以正文色渲染，读起来像一条真实工具输出而非系统说明
   - 最小纯 UI 修复建议：给该分支补 `text_color(cx.theme().muted_foreground)`
   - **为什么本轮不改**：该分支是**前序轮次既有代码**，改它会同时改变所有普通工具输出的呈现（这正是规范要求的方向，但属于前序偏差的整改），触红线 3。已记入 BACKLOG #32 交后续 UI 打磨轮
   - 缓解：占位文案以「（…）」包裹并自述原因，`preview` 与 `status` 均原样保留，用户仍能看出这张卡片跑的是什么、结果是成是败

### Non-UI Dependencies

- **按需 rehydrate 被释放的内容**：要让用户真正重新看到被释放的图片 / 工具输出，需要从 `source_path` 会话文件按条回读，涉及 `pi-render` 的数据通路与会话文件索引 —— **非 UI 依赖，视觉修复阶段禁止处理**，已记 BACKLOG #31。

### Matches（仅代码层已确认，不代表真实画面已还原）

- 本轮 `crates/ui/**` 零 diff，**未引入任何新组件、新布局或新 Theme token**；两条可见变化都走既有渲染路径。
- 图片释放后落到 `render_image` 的 `image-placeholder` 分支：`bg(cx.theme().muted.opacity(0.42))` + `muted_foreground` 文本 + `min_w_0()`，**用背景与留白表达层级、不描边**，符合 S-1；颜色全部走 `cx.theme()` token，无硬编码。
- 工具卡片释放后仍位于 `subordinate_column`（`ml_1p5 + pl_3p5 + border_l_1`）内，符合 § 5.3「左竖线 + 缩进、不嵌套卡片」。
- 占位文案是**编译期常量**（工具输出约 40 个汉字、图片说明约 25 个），长度有界；与 BACKLOG #17 记录的「pi 回传 `{error}` 长度不受控」不同类，不存在无界撑高风险。
- `tool.status` 与 `tool.preview` 在释放路径中被显式保留（`released_tool_output_leaves_a_visible_placeholder` 有断言），§ 5.2 的状态点语义与「这次跑的是什么」都不受影响。
- 未触碰任何状态色使用，S-8「一行不超过 3 片段」不受影响（占位是单个文本块，不是多片段行）。

## 本轮实测

### 真实 pi 进程树实测（2026-08-26，Windows 11 Pro 26200，pi 0.84.2，零 token）

| 项 | 实测 |
|---|---|
| 单个静止 Runtime 整树进程数 | **1** |
| 单个静止 Runtime 整树私有提交内存 | **377.5 MiB / 377.3 MiB**（两个并行会话） |
| 单个 Runtime job 峰值内存 | 380.9 MiB / 382.8 MiB |
| 本机系统内存 | 31.3 GiB 总量 / 13.7 GiB 可用 / 占用 56% |
| `shutdown_all` 后残留进程 | **0**（句柄仍被持有的情况下） |

**与立项文档的偏离（须记录）**：立项文档 § 三写「一个 Resident Runtime 实测常驻约 203MB」，本轮实测私有提交约 **377MiB**，接近两倍。两者量纲多半不同（工作集 vs 私有提交），本轮统一采用**私有提交**（`PROCESS_MEMORY_COUNTERS_EX.PrivateUsage`）——它才是"这棵树向系统要了多少内存"的口径，也是水位该盯的量。水位初值按此标定，未回改立项文档正文（属跨轮次改动，红线 3）。

### 水位初值及其依据

| 参数 | 初值 | 依据 |
|---|---|---|
| `low_available_bytes` | 1.5 GiB | ≈ 4 × 单 Runtime 实测占用。低于它再起一个后台会话，剩余量就掉到一个 Runtime 的量级以下 |
| `resume_available_bytes` | 2.5 GiB | 比低水位再高约一个 Runtime 的余量，迟滞带足够宽，一次冷启动的抖动不会来回翻状态 |
| `max_processes_per_runtime` | 64 | 静止实测只有 1 个进程；64 是"跑飞了"的护栏，不是日常配额 |
| `runtime_memory_bytes` | 4 GiB | ≈ 实测占用的 10 倍，只拦真正的失控 |
| `sample_interval` | 500ms | 采样是一次系统调用，且在调度锁之外；节流足以让每次 admit 不各打一次 |
| 文档过程性负载预算 | 16 MiB（历史段与流式段各一份） | 约等于两张满额截图；对话正文不占用该预算 |

### 实现过程中发现并修复的缺陷

**① 整树回收排在 reader join 之后，shutdown 会被孙进程拖住（本轮引入 Job Object 时暴露，已修）**

首版把 job 句柄的释放放在 `stdout_handle.join()` **之后**。Windows 的 `CreateProcess` 一旦开继承，会把父进程所有可继承句柄一并带给子进程 —— pi 的孙进程因此攥着 stdout 管道的写端，reader 永远等不到 EOF，`shutdown()` 被一个早已与会话无关的进程拖到它自己退出为止。

实测：`grandchildren_join_the_job_and_die_when_the_runtime_closes` **跑了 120.61 秒**（正好是 fixture 孙进程的自毁时限），而且**测试是"通过"的** —— 断言只看孙进程最终有没有死。把整树回收前移到 join 之前后，同一用例 **0.78 秒**。

已加计时断言（`shutdown` 必须在 15s 内完成）作为次序回归护栏：只靠"孙进程最终死了"这条断言，次序退化会表现为"通过但慢 150 倍"，翻不了红。

**② `rerender_completed_tools` 会把已释放的负载复活（设计聚合预算时发现，已处理）**

流式段的重渲染是从原始 `value` 重建的，会把预算释放掉的工具输出原样带回来。不在重渲染之后重新压一次，预算就只在"没有工具结果回来"时有效。已加 `re_rendering_completed_tools_does_not_resurrect_released_payload` 覆盖。

### `Cargo.lock` 的实际变化（须记录）

立项文档 § 七「依赖边界」写的是「只需增加 feature，**不改 `Cargo.lock`**」。实际结果是 lock **新增了一行**：

```
 [[package]] name = "pi-rpc"
   ...
+ "windows-sys 0.61.2",
```

这是**依赖边**的记录（pi-rpc 现在也依赖 windows-sys），不是版本漂移：`windows-sys 0.61.2` 早已在 lock 中（`pi-data` 在用），本轮没有新增任何 package、没有改动任何版本号，`check-pins.ps1` 全绿。立项文档那句表述略理想化 —— 给一个此前不依赖某 crate 的成员新增依赖，必然会在 lock 里留下这一行。已按事实记录，不视为触红线 2。

### CI

PR [#35](https://github.com/cking000bigdemon/GPUI-Pi/pull/35)，唯一阻断 job `windows (阻断)` 的
`T1 全量验收` **通过**（[run 32962261545](https://github.com/cking000bigdemon/GPUI-Pi/actions/runs/32962261545)，20m11s）。
CI runner 关闭本机缓存（`GPUI_PI_CACHE: OFF`）、每次全新联网拉取 vendor，因此这也顺带验证了
本轮的 `vendor` 门禁与钉版本在干净机器上成立。

### 一次 validation 失败及其判定（如实记录）

第七轮整改后的那次全量 validation **失败一次**，失败项是
`crates/app/src/model_service.rs` 的 `timeout_oversize_and_malformed_json_are_bounded`。

判定为 **BACKLOG #27 记录的既有时序 flake**，不是本轮回归，依据：

- 本轮对 `crates/app` 的全部改动只有 `main.rs`（14 行），`model_service.rs` **零 diff**；
- 该用例单跑 **3/3 通过**，与 BACKLOG #27 写的「6 次完整跑复现 2 次，单跑必过」完全一致；
- 失败发生在本机负载最高的时段（真实 pi 实测那几轮把可用内存从 13.7GiB 压到 7.4GiB、
  占用 76%），正是该 flake 的触发条件（整个用例共用一个 50ms 超时预算）。

按 CLAUDE.md 流程「不过就改，重跑」处理：本轮没有可改的东西，重跑后通过。**未触发红线 4**
（该条要求的是「同一验收项经针对性整改后连续 2 次仍不过」）。

### 测试计数

| 套件 | 结果 |
|---|---|
| `pi-rpc` | 16 + 20 + 3（新增 job_object）+ 12 ignored |
| `pi-render` | 27（含 budget 7 条）+ 24（含流式预算与释放 9 条）+ 其余 |
| `pi-runtime` | 88 + 9（新增 memory_watermark）+ 4（新增 shutdown_all）+ 24 + 其余 |
| `pi-runtime` real_pi | **5/5 通过**（手动实跑，含 R21–R24 既有四条 + R25 新增一条），确认本轮改动未回退前序轮次的真机行为 |
