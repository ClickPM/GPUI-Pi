# Round 24 — 有界多用户 Session UI 接线

> 执行方：Windows · 状态：进行中

## 目标

把 R23 的调度器接到界面上：`ChatPanel` 由「一个活会话」变成**一组按 `SessionId` 隔离的会话标签**，
两个用户会话可以真并行且各自独立流式更新，切换标签**不终止**后台 `Running` 会话，
并在标签上如实展示 `Running / Queued / Parked / Failed`。
可证伪判据：同时开两个活会话，切到 B 之后 A 的 pi 进程仍在、文档继续增长；
切回 A 时草稿、附件、展开与滚动位置仍是 A 自己的那一份。

## 前置

- R21（集中化）、R22（有界 Actor + 背压）、R23（Scheduler / 七态 / Park-Resume）已合入 `main`。
- 立项文档 § 七「R22 背压门禁不过不得开放 R24 多会话」已满足（R22 PR #32 已合并且验收全绿）。
- Windows worktree：`D:\variFlight_work\GPUI-Pi\.claude\worktrees\r24-dev-e5b193`。
- 新 worktree 已独立执行 `fetch-pi.ps1` / `fetch-pi-source.ps1` / `fetch-pi-web.ps1` / `check-pins.ps1`，
  vendor 三件齐全、`check-pins` 全绿（2026-08-25）。
- 改动前基线：`.\scripts\validate.ps1 -Logic` → `VALIDATE OK`。

## 现状（改造前）

| # | 位置 | 表现 |
|---|---|---|
| ① | `ChatPanel.session: SessionUiState` | R21 已把会话态收敛成一个结构，但**只有一个实例**；切会话是原地覆写同一份状态 |
| ② | `ChatPanel.active: Option<SessionHandle>` | 全 panel 唯一的运行时绑定；`load_selection` / `start_new_session` 第一件事就是 `stop_user(旧 runtime)` |
| ③ | `RuntimeManager::start_fresh` / `start_session` | R21 的 `single_session_compat`：内部维护唯一 `active_user`，起新会话必然踢掉旧会话 |
| ④ | R23 的 `create_session` / `request_run` / `park` / `session_state` | 已实现且有测试，但**没有任何 app 调用点** |
| ⑤ | — | 调度器的状态变化（队列提升、崩溃回收、TTL 回收）只发生在 `tick()` 里，**没有对外通知**，UI 无从得知一个 `Queued` 会话何时变成 `Running` |

## 设计要点

- **按 `SessionId` 隔离，不是 `RuntimeId`**（BACKLOG #18）：`RuntimeId` 每次 Park/Resume 都换新，
  照立项文档 § 七 R24 的字面表述实现会让一次 Resume 丢掉草稿与滚动位置。
  本轮同步修订立项文档 § 七 R24 的表述。
- **标签的身份分三层**，不混用：
  `tab_id`（app 内单调分配，标签自身身份，永不复用）→ `SessionId`（Manager 会话身份，跨 Park/Resume 恒定）
  → `RuntimeId`（进程宿主身份，每次 Resume 换新，只用于 stale-snapshot 判定）。
  纯历史预览标签只有 `tab_id`，没有 `SessionId`。
- **调度器通知是电平触发不是边沿触发**：新增 `RuntimeManager::subscribe_scheduler()`，
  通道容量 1 且 `try_send` 丢弃满帧——收到通知的一方必须**重新查询**全部关心的会话状态，
  不依赖通知内容。这样合并丢帧不会让 UI 停在旧状态，也不会为 UI 卡顿引入一条无界队列。
- **前台/后台的分工**：后台标签照常消费 effect（否则 R22 的有界 effect 缓存会把它们淘汰掉，
  用户切回来就少了一段），但**不碰 window**——通知、窗口标题、Extension UI 对话框只由前台标签驱动，
  后台标签的对话框请求留在自己的 `ExtensionUiState` 队列里等切回前台。
- **切换标签不启动也不停止任何进程**：只有显式的「启动活会话 / 重试 / 挂起 / 关闭」才动进程。
  切到一个 `Queued` 标签会把它的优先级抬到 `FOREGROUND`（`WaitQueue` 只升不降），但不抢占。
- **释放运行槽后由 app 立刻 `tick()`**：R23 的队列提升只发生在 `tick()`，生产 reaper 的轮询间隔是
  `clamp(idle_ttl/4, 200ms, 5s)`（默认 TTL 180s ⇒ 45s）。关闭/挂起一个会话后立刻 `tick()`，
  排队会话才会马上补位，而不是等到最长 45s 之后。这是 app 侧接线，不改 R23 的调度语义。
- **标签数有界**：`MAX_SESSION_TABS = 8`。进程数由调度器兜底，但每个标签常驻一份
  `ConversationDocument` 与 `ListState`，标签数必须自己有上限；超限时明确拒绝并提示，不静默淘汰。

## 交付物

- `crates/pi-runtime/src/lib.rs`：`SchedulerWatch`（修订号 + 容量 1 的电平触发 watcher）、
  `RuntimeManager::subscribe_scheduler()` / `scheduler_revision()`、`SchedulerChanged`；
  `SessionSlot` 持共享 watch 句柄，状态转移与登记/注销即发布。
- `crates/ui/src/session_tabs.rs`（新增）：`SessionTabs` / `SessionTabItem` / `SessionTabState`，
  基于 gpui-component `TabBar` + `Tab`（S-19），状态点走 `prefix`，关闭按钮走 `suffix`。
- `crates/app/src/panels.rs`：`SessionUiState` 吸收运行时绑定（`session` / `active` / epoch /
  revision / effect cursor / backpressure / `model_names` / `popup` / `file_index`）；
  `ChatPanel` 改持 `sessions: Vec<SessionUiState>` + `focused` / `cursor` 两个下标与 `project()` 投影；
  多标签开关与调度器 reconcile；标签条渲染与状态展示。
- 对应 `--lib` 单测：pi-runtime 侧 watcher 语义，app 侧标签隔离、状态投影与渲染断言。
- `crates/pi-runtime/tests/multi_session_fake_child.rs`（新增）：两个会话真并行、各自独立推进的集成测试。
- `docs/立项文档.md` § 七 R24 表述按 BACKLOG #18 修订为 `SessionId`。
- `rounds/BACKLOG.md`：关闭 #18，并登记本轮新发现的问题。

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | 全量格式、clippy、测试、release 构建与钉版本 | `.\scripts\validate.ps1` → `VALIDATE OK` |
| T1 | 逻辑 crate 快速回归 | `.\scripts\validate.ps1 -Logic` → `VALIDATE OK` |
| T2 | 两个用户 Session 真并行 | 两个会话同时 `Running`，各自的 Snapshot `revision` 独立增长、文档互不污染；`resident_pi <= total_runtime_slots` 持续成立 |
| T2 | 切换 UI 不终止后台 Running | 切换前台标签后，后台会话的 `SchedulerState` 仍为 `Running`、`process_id()` 不变、进程数不变 |
| T2 | 后台会话不丢 effect | 后台标签持续 ack effect；切回前台后文档与控制态是最新的，不依赖「切回来再拉一次」 |
| T2 | `SessionUiState` 按 `SessionId` 多实例隔离 | 草稿、附件、展开集合、滚动/跟随状态、Extension UI、分支预览、错误/成功横幅在标签间互不串写；切走再切回原样恢复 |
| T2 | 四态展示 | `Running` / `Queued` / `Parked` / `Failed` 各有对应标签状态点与文案，且由 `session_state()` 驱动而非 app 自行猜测 |
| T2 | 调度器通知电平触发 | 连续多次状态变化只留一条待处理通知；订阅者断开后发布方自动摘除；通知不阻塞持锁的发布方 |
| T2 | 释放运行槽后排队会话立即补位 | 关闭一个 `Running` 会话后，`Queued` 会话在同一次调用链内变成 `Running`，不等 reaper 轮询 |
| T2 | 标签数有界 | 达到 `MAX_SESSION_TABS` 后再开标签明确失败并提示，已开标签与运行中会话不受影响 |
| T2 | 行为回归 | 单会话下的提交/停止/restart/rebind/compaction/retry/Extension UI/导出与 R23 一致；既有 `gpui-pi` / `gpui-pi-ui` / `pi-*` 测试全绿 |
| T3 | 可见行为 | 标签条符合 `docs/UI设计规范.md`（S-19 复用 `TabBar`、S-10 状态色只点不铺、S-8 一行不超过 3 片段、S-9 低频操作 hover/菜单） |

## 禁止

- 不实现 R25 的 Job Object / `ResourceProbe` / 内存水位 / `ConversationDocument` 有界缓存。
- 不实现 R26/R27 的子代理任务、`parentSession`、worktree writer 隔离。
- 不改 R23 的调度语义（七态转移表、aging、TTL、warm 复用判据）；app 只调用既有 API，
  新增的只有对外通知与查询，不改变任何既有转移路径。
- 不放大无界路径：新增通道必须有界；不得为「后台会话也要实时渲染」给每个标签常驻新线程或新定时器。
- 不修改 `PINNED_PI_VERSION` 或 `vendor/upstream/**` 身份；不引入新第三方依赖；`Cargo.lock` 零漂移。
- 不写用户主目录 `~/.pi`；测试只读真实目录或用临时目录。
- 不借本轮顺手修前序问题：BACKLOG #11 / #13 / #14 / #15 / #16 / #17 / #19 / #20 均不动
  （#18 是立项文档明确指派给本轮的修订，不在此列）。

## 失败处理

同一验收项经针对性整改后连续 2 次 validation 仍不过 → 写 `rounds/round-24/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 视觉审查

- 视觉审查模式：<待填>
- 视觉审查结论：<待填>
- 截图验证：<待填>
- 兜底原因：<待填>
- `requested_at`：<待填>
- `deadline`：<待填>
- 审查报告 / 证据：<待填>
- 说明：本轮触及 `crates/ui/**` 与 `crates/app/**` 的视图代码，按 CLAUDE.md 必须触发视觉审查。

## 本轮实测

<!-- 完成后回填 -->
