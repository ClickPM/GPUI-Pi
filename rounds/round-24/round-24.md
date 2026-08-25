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

- 视觉审查模式：SCREENSHOT
- 视觉审查结论：**待复核**（首轮 findings 已全部整改，等整改后截图重跑）
- 截图验证：已提供（SCREENSHOT_PROVIDED）
- 兜底原因：N/A
- `requested_at`：2026-08-25T16:59:31+08:00
- `deadline`：2026-08-25T17:29:31+08:00（用户在截止前完整回传 10 张，1280×820、100%、浅色主题）
- 审查报告 / 证据：`.pi/visual-review/round-24/evidence/`（10 张原图）、
  `.pi/visual-review/round-24/crops/`（标签条局部放大）、`.pi/visual-review/round-24/ui-diff.txt`
- 复核窗口：`requested_at` 2026-08-25T17:35:21+08:00 / `deadline` 2026-08-25T18:05:21+08:00
- 说明：审查通道偏离——项目专用 `visual-reviewer`（`.agents/visual-reviewer.md`）是 pi harness 的
  agent 定义，本轮所处的 Claude Code harness 里没有该 agent 类型，且本会话被明确约束不得自行派发
  子代理。因此由主会话依据 `docs/UI设计规范.md` 与用户回传截图逐条核对。这一偏离如实记录，
  不冒充「已由独立视觉 reviewer 审查」。

### 首轮 findings 与整改

| 编号 | 严重度 | 问题 | 证据 | 整改 |
|---|---|---|---|---|
| V-1 | 阻断 | 挂起成功时，常驻状态说明「会话已挂起：pi 进程已让出…」与绿色成功横幅「会话已挂起；进程已让出，可随时恢复」上下并排说同一件事，白白挤掉一行消息区 | `evidence/07` | 去掉绿条：常驻说明已经把话说完了。随崩溃整改一并落地 |
| V-2 | 阻断 | 中性状态点（`History` / `Parked`）用 `cx.theme().border`，浅色主题下几乎与标签底色融为一体，放大后只隐约看得出圆形轮廓。状态点是每个标签**唯一**的状态信号，看不见就等于「展示 Parked」这条验收不成立 | `crops/tabs-09.png` | 改用 `muted_foreground`（规范 § 1.2 档 2「元信息、图标」；非状态色，不受 S-4/S-5 约束）。绿（Running）/ 黄（Queued）实测可读性没问题，不动 |
| V-3 | 阻断 | 排队中的标签，主操作显示**可点的**「恢复运行」，暗示用户必须做点什么；实际上轮到它会自动启动，而且切到该标签本身已经把优先级抬到前台了，点它没有任何可执行的动作 | `evidence/06` | 禁用 + 文案「排队中…」+ tooltip「已在公平队列里等运行槽，轮到它会自动启动」。文案抽成 `start_action_copy` 并加单测 |
| V-4 | 非阻断 | 「对话」内容标签条与会话标签条上下两行并排，两条标签条堆叠、视觉偏重 | `evidence/04` `06` `09` `10` | 不改：内容标签条属 R11/R12 范围，合并两者超出 R24 remit。已记 BACKLOG #28 |
| V-5 | 非阻断 | 标签多时最后一个标签被 `TabBar` 直接裁切（无省略号） | `evidence/10` | 不改：`.menu(true)` 的溢出菜单已兜底，属组件既有行为 |

### 已通过的检查项

- **单会话不画标签条**（`evidence/01`）：纯历史预览与 R23 完全一致，没有多出一行 chrome。
- **两会话并行**（`evidence/04`）：两个标签都是绿点，选中标签由 gpui-component `Tab` 自带的
  底色区分（S-19 复用组件、§ 4.8「用它就吃它的选中色」）。
- **排队态**（`evidence/06`）：第三个标签黄点 + 横幅「排队中：同时运行的会话已达上限 2，轮到它就会自动启动」。
- **挂起态**（`evidence/07`）：灰点 + 横幅「会话已挂起：pi 进程已让出，点「恢复运行」继续」+ 按钮变「恢复运行」。
- **状态色只点不铺**（S-10）：全部状态只出现在 `size_2` 圆点与横幅前的小圆点上，没有铺底、没有铺边框。
- **一行不超过 3 个文本片段**（S-8）：标签只有 1 个文本节点（标题），完整身份 + 状态进 tooltip；
  状态横幅 1 个文本节点。
- **标签标题按字符截断**（`evidence/10` 的 `subagent-worker-c3…`）：18 字符 + 省略号，中文不被切成非法 UTF-8。
- **溢出菜单**（`evidence/09` `10`）：标签放不下时由 `TabBar` 的 chevron 菜单接管。

### 待复核项（整改后截图未覆盖）

首轮证据拍摄于整改之前，以下四处的**真实渲染**尚未验证：挂起态（绿条已移除）、
中性状态点新配色、排队态按钮新形态、以及三个 busy 文案（「启动中… / 恢复中… / 挂起中…」）。
复核截图回传并核对通过后，本节结论方可升级为 `PASS`。

## 本轮实测

### 门禁与基线

- 新 worktree vendor 门禁（2026-08-25）：`fetch-pi.ps1` / `fetch-pi-source.ps1` / `fetch-pi-web.ps1`
  全部 cache hit 并自检通过，`check-pins.ps1` 全绿；`vendor/pi/pi.exe`、`vendor/upstream/pi-0.84.2/`、
  `vendor/upstream/pi-web-0.8.9/` 三件齐全。
- 改动前基线：`.\scripts\validate.ps1 -Logic` → `VALIDATE OK`。

### 关键设计决定与依据

| # | 决定 | 依据 |
|---|---|---|
| ① | **按 `SessionId` 隔离，不按 `RuntimeId`** | BACKLOG #18。`RuntimeId` 每次 Park/Resume 换新，照立项文档原字面实现会让一次唤醒丢掉草稿与滚动位置。立项文档 § 七 R24 已改写并附勘误。 |
| ② | 标签身份分三层：`tab_id` → `SessionId` → `RuntimeId` | 三者生命周期不同：`tab_id` 是 app 内的标签身份（关掉再开必须换新，否则 GPUI 元素 id 复用会串状态）；`SessionId` 是会话；`RuntimeId` 是进程宿主，只用于 stale-snapshot 判定与重绑。合并任意两个都会在某条路径上串台。 |
| ③ | **投影游标 `cursor` + `project()`，而不是把目标下标逐层传参** | 整套会话态投影（`apply_snapshot` / `apply_runtime_effect` / `sync_list_document` / `apply_control_outcome`…）约一千行都写成 `self.xxx`。改成传参要动遍每一个分支，且此后每加一条分支都可能忘记带下标；投影游标把「写进哪一槽」收敛成一个入口。`render` 第一行无条件 `cursor = focused` 兜底自愈。 |
| ④ | 调度器通知**电平触发**、容量 1、`try_send` 丢满帧 | 通知不带内容，收到就重新查状态：合并掉的帧不会让 UI 停在旧状态。容量 1 保证 UI 卡顿不会长出一条无界队列（R22 要消灭的正是这个）。发布发生在持调度锁期间，`try_send` 永不阻塞。 |
| ⑤ | 订阅做成 `SchedulerSubscription` 守卫，**析构即退订** | UI 侧必须把接收端交给一条阻塞线程。如果只靠「通道另一端没人了」收尾，那条线程会一直卡在 `recv` 上直到整个 Manager 析构——关一次窗口漏一条线程。退订当场摘掉名册项并断开通道，阻塞中的 `recv` 立刻返回。 |
| ⑥ | 桥接线程**只在真的登记了会话之后才起** | 从没启动过活会话的面板不需要监听调度器；顺带避开 GPUI 测试调度器的「检测到其他线程活动」断言——它是确定性测试的一部分，不该为了绕过它给生产代码加 `#[cfg(test)]`。 |
| ⑦ | 释放运行槽后由 **app 立刻 `tick()`** | R23 的队列提升只发生在 `tick()`，生产 reaper 的轮询间隔是 `clamp(idle_ttl/4, 200ms, 5s)`（默认 TTL 180s ⇒ 45s）。关闭/挂起会话、以及事件泵看到终态时立刻 `tick()`，排队会话才会马上补位。这是 app 侧接线，没有改 R23 的调度语义。 |
| ⑧ | 终态判定取 `SessionSnapshot.terminal`，**不看 `Stopped` effect** | effect 会被背压淘汰，终态不会（R22 刻意如此）。只看 effect 的话，崩溃的 Runtime 不会再产生 Dirty，事件泵就永远阻塞在下一条通知上，运行槽拖到 reaper 轮询才回收。 |
| ⑨ | 后台标签照常消费 effect，但**不碰 window** | 不消费 effect，R22 的有界 effect 缓存会把后台会话的增量淘汰掉，用户切回来就少一段。反过来，通知、窗口标题和 Extension UI 对话框是全局资源，后台会话去动它们等于替用户抢屏幕。 |
| ⑩ | 切换标签**不启动也不停止**任何进程；只有 `Queued` 标签被切到前台时抬一次优先级 | 这是本轮的核心承诺。`WaitQueue::push` 对已排队条目取 `min`，只升不降，抬优先级不会误伤别人。Parked 会话切过去不自动恢复：它可能是用户主动挂起的，也可能是在别处被停掉的，替用户重新拉起一个约 203MB 的进程不是切标签该有的副作用。 |
| ⑪ | 标签数上限 `MAX_SESSION_TABS = 8`，到顶明确拒绝 | 进程数由调度器兜底，但每个标签常驻一份 `ConversationDocument` + `ListState` + 整套会话态，与进程无关。静默淘汰会让用户正在用的标签凭空消失。8 与队列容量 64 相容：最坏 2 运行 + 6 排队。 |
| ⑫ | 单个标签只要登记了会话就画标签条 | 状态点是用户唯一能看到会话在不在跑的地方，关闭入口也只在标签上；不画等于把一个活着的 pi 进程藏起来。纯历史预览（没有会话）仍然不画，避免为一条无信息的横条占掉一行消息区。 |

### 与设计的偏离

- 立项文档 § 七 R24 原写「按 `RuntimeId` 多实例隔离」，本轮按 BACKLOG #18 改为 `SessionId`，
  已同步修订立项文档并在该处附勘误说明，不是静默改语义。
- `RuntimeManager::start_fresh` / `start_session` / `stop_user`（R21 单会话兼容通道）**在 app 侧全部下线**，
  改走 `create_session` + `request_run`。三个 API 仍保留供 `pi-runtime` 自身测试使用，未删除，
  R23 的相关用例不受影响。BACKLOG #21 描述的「单槽回滚导致句柄失效」路径因此在 app 侧不再可达。

### 实测结果

- `cargo test -p pi-runtime --lib` → **80 passed / 0 failed / 1 ignored**（R23 收口时 77，本轮新增 3：
  电平触发合并、订阅析构退订、发布方不被停滞订阅者阻塞）。
- `cargo test -p pi-runtime --test multi_session_fake_child` → **3 passed**（新增）。
- `cargo test -p pi-runtime --test scheduler_fake_child` → 11 passed（R23 既有用例未受影响）。
- `cargo test -p gpui-pi-ui --lib` → **32 passed**（R23 收口时 30，本轮新增 2：状态文案与标签项）。
- `cargo test -p gpui-pi` → **133 passed**（R23 收口时 124；净增 9：新增 10 条 R24 用例，
  删除 1 条已失去被测对象的 `fresh_session_reset_clears_project_scoped_index_and_popup_state`——
  它测的自由函数 `reset_session_scoped_state` 随「新标签天生干净」一起消失，
  等价不变量改由 `session_tabs_isolate_draft_expansion_and_scroll_state` 断言）。
- 真实 pi 零 token：`PI_RUNTIME_TEST_BINARY=<abs>\vendor\pi\pi.exe cargo test -p pi-runtime --test real_pi -- --ignored --test-threads=1`
  → **4 passed**（R21/R22/R23 原有三项 + 本轮新增
  `two_real_pi_sessions_run_in_parallel_and_survive_foreground_switches`）。该用例证实：
  真实 pi 0.84.2 下两个会话同时 `Running`、pid 不同、`resident_pi == 2 <= total_runtime_slots`；
  第三个会话如实 `Queued`；反复切前台后两个会话的 pid 与 `Running` 状态都不变、`terminal` 仍为 `None`；
  关掉其中一个后排队会话补位，并唤醒调度器订阅。
- `.\scripts\validate.ps1 -Logic` → `VALIDATE OK`；完整 `.\scripts\validate.ps1` → **`VALIDATE OK`**
  （release 构建 2m47s，全部测试目标零失败；仅有既存的 linker stdout 与 `proc-macro-error2` future-incompat 警告）。

### 独立代码审查与整改

审查通道：Claude Code harness → **codex 插件**（`/codex:review --scope branch --base main --background`，
Codex thread `01a03809-c451-71a1-b82e-ef46baf1cfce`）。只读、与 writer 隔离。
本轮由主会话实现，因此由主会话修复，writer 归属未变。

> 记一笔：发起前 `CLAUDE.md` 还写着「这两个命令标了 `disable-model-invocation`，主会话无法自行触发」。
> 实际生效的插件版本是 **1.0.6**（`installed_plugins.json` 的 `installPath`），其
> `commands/review.md` 已是 `disable-model-invocation: false`；只有旧版 1.0.1 与
> `marketplaces/` 下的副本仍是 `true`，我最初读错了副本。已按用户指示同步修正
> `CLAUDE.md` / `AGENTS.md`，改为「以实际生效的 `installPath` 下那份命令定义为准」。

结论：**1 项 P1 + 1 项 P2**。逐条核对源码后确认**全部成立**，且都落在本轮新写的代码里，
已全部整改并补回归测试。P1 的影响面比审查点名的更大——顺着它给的判据把
`panels.rs` 里所有跨 await 点的续体扫了一遍，共 **6 处**，不是 2 处。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P1 | 多标签投影之后，既有的异步续体仍然靠 `Deref` 落到「回来时的前台标签」，而不是发起它的那个标签。跨 await 点的写入因此会串台：附件挂到别人身上（或因对错 `load_generation` 被静默丢弃）、Extension UI 超时去取消别人的同名请求 | 新增 `ChatPanel::project_tab(tab_id, body)`：发起时捕获 `tab_id`，回来先定位再投影；标签已关就整段跳过。逐处钉住 **6 条**续体：`start_attach_paths`、`choose_images`（原生选择器）、`send_extension_response` 的错误写回、Extension UI 超时定时器，以及自查追加的 `choose_session_switch`、`export_html` | `an_attachment_started_on_one_tab_never_lands_on_another`、`an_extension_dialog_timeout_only_cancels_its_own_tabs_request`、`a_session_switch_choice_lands_on_the_tab_that_opened_the_picker` |
| P2 | 调度器桥接线程把上游「容量 1、满帧即丢」的订阅原样倒进一条 `mpsc::unbounded`。GPUI 执行器一卡住，这条线程就会把上游取空并在桥接侧堆成无界队列，电平触发的合并保证到这里作废 | 桥接侧改为 `mpsc::channel(0)` + `try_send`，满帧即丢、只在 `is_disconnected()` 时退出，与上游同一条合并规则。端到端上限因此是「上游 1 + 桥接 1」，与 UI 卡多久无关 | 无新增用例：这是 3 行的构造性收敛，桥接线程与私有通道在 app 层没有可注入的观测点，硬造一个 stall 只会测到 GPUI 执行器而不是这段代码。退订即断链那一半已由 `pi-runtime` 的 `dropping_a_subscription_unregisters_it_and_disconnects_the_bridge` 覆盖 |

整改中额外发现并修掉的一个缺口（由 P1 的超时用例逼出来）：切走标签时对话框会被收起
（`extension_dialog_open` 清空），而 `finish_extension_dialog` 要求「这个对话框此刻正开着」
才肯收口——于是后台标签的超时只留下一条错误，请求本身悬在队列里，pi 那头一直等。
这是本轮 suspend-on-switch 引入的缺口，新增 `expire_extension_dialog`：
**截止时间属于请求、不属于窗口**，无论此刻显不显示都回一个 cancelled。

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **136 passed**
（较整改前 133 增 3 条 P1 回归用例）。

### 踩到的坑

- **GPUI 的测试调度器会把「其他线程唤醒任务」判成不确定性测试**：调度器桥接线程最初在
  `ChatPanel::new` 里无条件起，线程退出时 `UnboundedSender` 析构唤醒了 GPUI 任务，
  `test_scheduler` 直接 panic（`Detected activity on thread ...`），一次跑挂 13 个既有用例。
  改成「只在真的登记了会话之后才起」——既是正确的资源惯例（没有会话就不需要监听调度器），
  也让测试环境天然不起这条线程，不必给生产代码加 `#[cfg(test)]`。
- **`ListState` 的滚动回调是 `cx.defer` 之后才执行的**：回调里原本走 `Deref` 写 `tail_attached`，
  多标签下那一刻投影游标可能正指着别的标签，等于把 A 的滚动状态写进 B。改为按 `tab_id` 找槽，
  并把两处重复的 `ListState` 构造合并成 `new_list_state(tab_id, weak)`。
- **同样的问题在两条后台回填路径上各有一份**：历史渲染（`finish_load`）与文件索引
  （`start_file_index`）都是后台任务，回来时用户可能已经切走。两处都改成按 `tab_id` 定位，
  且文件索引只在**前台**标签上用 composer 内容重算补全——composer 全窗口只有一个，
  拿它的内容去刷新后台标签的补全面板等于用别人的输入。
- **`process_extension_ui` 的标题写入是「与本标签记录值比较」**，换标签时两个值可能恰好相等，
  而窗口上挂的还是上一个标签的标题。切标签与关标签都必须无条件写一次（`apply_window_title`）。
- **一次不可复现的 flake**：某一轮完整 validation 里
  `model_service::tests::timeout_oversize_and_malformed_json_are_bounded` 失败一次
  （loopback HTTP fixture，与本轮改动无交集）。单独重跑 4 次全过，随后完整 validation 亦全绿。
  未观察到第二次，暂不登记 BACKLOG；若后续复现再单独立项。
