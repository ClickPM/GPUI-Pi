# Round 24 — 有界多用户 Session UI 接线

> 执行方：Windows · 状态：已完成

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
- 视觉审查结论：**PASS**
- 截图验证：已提供（SCREENSHOT_PROVIDED）
- 兜底原因：N/A
- `requested_at`：2026-08-25T16:59:31+08:00 · `deadline`：2026-08-25T17:29:31+08:00（首轮，截止前完整回传 10 张）
- 复核 `requested_at`：2026-08-25T17:35:21+08:00 · `deadline`：2026-08-25T18:05:21+08:00（截止前回传 5 张，17:52）
- 条件：1280×820、缩放 100%、浅色主题
- 审查报告 / 证据：`.pi/visual-review/round-24/evidence/`（首轮 10 张 + 复核 5 张 `recheck-2-*`）、
  `.pi/visual-review/round-24/crops/`（标签条局部放大对比）、`.pi/visual-review/round-24/ui-diff.txt`
- 说明：**审查通道偏离**——项目专用 `visual-reviewer`（`.agents/visual-reviewer.md`）是 pi harness 的
  agent 定义，本轮所处的 Claude Code harness 没有该 agent 类型，且本会话被明确约束不得自行派发子代理。
  因此由主会话依据 `docs/UI设计规范.md` 与用户回传截图逐条核对。如实记录，不冒充「已由独立视觉 reviewer 审查」。

### findings 与整改

| 编号 | 严重度 | 问题 | 证据 | 整改 | 复核 |
|---|---|---|---|---|---|
| V-1 | 阻断 | 挂起成功时，常驻状态说明与绿色成功横幅上下并排说同一件事，白挤一行消息区 | `evidence/07` | 去掉绿条，常驻说明已经把话说完 | `recheck-2-5` 只剩一条灰点说明 ✅ |
| V-2 | 阻断 | 中性状态点（`History` / `Parked`）用 `cx.theme().border`，浅色主题下几乎与标签底色融为一体，放大后只隐约看得出圆形轮廓。状态点是每个标签**唯一**的状态信号 | `crops/tabs-09.png` | 改用 `muted_foreground`（规范 § 1.2 档 2「元信息、图标」，非状态色，不受 S-4/S-5 约束） | `crops/recheck-tabs-2-2.png` 灰点清晰可辨，与绿点并列仍可区分 ✅ |
| V-3 | 阻断 | 排队标签的主操作是**可点的**「恢复运行」，暗示用户必须做点什么；实际轮到就自动启动，切过去本身已抬过优先级 | `evidence/06` | 禁用 + 「排队中…」+ tooltip；文案抽成 `start_action_copy` 并加单测 | `recheck-2-3` 按钮已灰化为「排队中…」✅ |
| V-6 | 阻断 | 挂起过程中闪一条红色「加载会话控制失败：request req_5 timed out」。Park 把在执行的元数据请求连同 client 一起抽走，那几条必然超时——用户刚点了挂起，这正是他要的结果，报成红色故障会让一次正常操作看起来失败了 | `recheck-2-4` | `is_tearing_down()` 期间不把元数据失败当错误上报（`ControlsLoaded` / `CommandsLoaded` 两处） | 见下「V-6 的复核方式」 |
| V-4 | 非阻断 | 「对话」内容标签条与会话标签条上下两行并排，视觉偏重 | `evidence/04` `06` `09` `10` | 不改：内容标签条属 R11/R12 范围，合并两者超出 R24 remit。记 BACKLOG #28 | — |
| V-5 | 非阻断 | 标签多时最后一个标签被 `TabBar` 直接裁切（无省略号） | `evidence/10` | 不改：`.menu(true)` 溢出菜单已兜底，属组件既有行为 | — |

**V-6 的复核方式**：它的整改是让一个**原本会出现的瞬时元素不再出现**，再要一轮截图去拍「一个不存在的横幅」
价值有限。因此改由回归测试钉死（`teardown_metadata_failures_are_not_reported_as_errors`：挂起中与挂起后
两种时机的元数据失败都不出横幅），并结合 `recheck-2-5` —— 挂起落地后的稳态本来就没有该横幅。
这一条如实记为「测试复核，非截图复核」，不声称截图验证过。

### 复核截图确认的行为

- `recheck-2-1`：单个活会话也画标签条（绿点 + 关闭入口），符合「不把一个活着的 pi 进程藏起来」的取舍。
- `recheck-2-2`：4 个标签，1 绿 3 灰，中性点已可辨；选中标签由 gpui-component `Tab` 自带底色区分。
- `recheck-2-3`：排队标签黄点 + 横幅「排队中：同时运行的会话已达上限 2，轮到它就会自动启动」+ 按钮灰化。
- `recheck-2-4`：**崩溃场景（2 活跃 + 1 排队点挂起）不再崩溃、不再卡死**，标签如实显示「挂起中…」。
- `recheck-2-5`：挂起落地——被挂起的标签转灰，**排队中的那个自动转绿开始运行**，
  按钮变「恢复运行」。这一张同时验证了 R24 的核心承诺：切换/挂起不终止其他会话，运行槽一空排队的就补位。

### 首轮已通过、复核未推翻的检查项

- 单会话纯历史预览不画标签条（`evidence/01`），与 R23 一致。
- 两会话并行双绿点（`evidence/04`）。
- 状态色只点不铺（S-10）：只出现在 `size_2` 圆点上，无铺底、无铺边框。
- 一行不超过 3 个文本片段（S-8）：标签 1 个文本节点，完整身份与状态进 tooltip；状态横幅 1 个文本节点。
- 标签标题按字符截断（`evidence/10` 的 `subagent-worker-c3…`），中文不被切成非法 UTF-8。
- 标签放不下时由 `TabBar` 的 chevron 溢出菜单接管（`evidence/09` `10`）。

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

### 人工验收暴露的崩溃与整改

用户在视觉验收时实测：**2 活跃 + 1 排队场景下点「挂起会话」，程序崩溃退出；单活跃场景挂起正常。**

根因不是 UI，是线程归属：`RuntimeManager::park` 收尾时会 `tick()` 一次，把排队会话**就地**提升上来——
连带一次 `switch_session` 往返（复用热进程）或一次冷启动，**全部发生在调用线程上**。
R24 是从 GPUI 主线程直接调它的，于是整个窗口卡死一次进程交接的时间。单会话时 `tick()` 无事可做，
所以那条路径看不出问题——这正好解释了「为什么只有 2+1 场景崩」。

先把根因证成事实再动手：新增 `pi-runtime` 集成用例
`park_finishes_a_queued_handoff_on_the_calling_thread`——`park` 返回时排队会话**已经在跑**，
中间没有任何额外的 `tick()` 或等待。

整改：新增 `ChatPanel::spawn_scheduler_job`，把所有会碰进程的调度器调用移到后台线程，完成后回 UI 线程
reconcile。覆盖 `park`、关标签的 `remove_session` + `tick`、切到排队标签时的优先级抬升、
事件泵终态的 `tick`，以及会话创建/恢复的 `request_run`（`create_session` 是纯内存操作，仍同步做）。
期间标签显示「启动中… / 恢复中… / 挂起中…」并挡住重复点击。

**顺带修掉的一个更早的坑**：`start_live` / `start_new_session` 从 R21 起就是在 UI 线程上同步冷启动的。
单会话时代能忍，多会话之后它会连带冻住**其他会话**的流式渲染，因此一并挪到后台。
这一条严格说属前序轮次行为，但它与本轮崩溃是同一条线程归属问题，分开修只会留半个坑。

回归测试：`parking_hands_the_process_work_to_a_background_task` 钉住「点击同步返回、只立 busy 标记、
挡住重复点击」；关标签用例改为断言「标签立刻消失、进程后台回收」。
复核截图 `recheck-2-4` / `recheck-2-5` 确认：同一场景不再崩溃、不再卡死，挂起落地后排队会话自动补位。

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

### 第二轮独立代码审查与整改

首轮 codex 审查只覆盖到 `c5ba550`，之后又落了 4 个 commit（P1/P2 整改、崩溃整改、两轮视觉整改），
而这些恰恰是最该被独立审的部分——findings 驱动的整改往往自带新假设。
因此在收口前补跑第二轮，覆盖 `main...HEAD` 的完整 diff。

审查通道：`/codex:review --scope branch --base main --background`，Codex thread
`01a0392e-eda4-7331-9a91-a4404a4d0ae2`。结论：**2 项 P2**，核对源码后确认**全部成立**。

| 编号 | 问题 | 整改 |
|---|---|---|
| P2-1 | fresh 会话把 `active_generation` 传给了按 `load_generation` 校验回填的文件索引。新标签上 `load_generation` 恒为 0 而 `active_generation` 已是 1，索引回来**永远**对不上被丢弃——`@` 文件补全在 fresh 会话里从来没工作过。改造前的实现传的是 `load_generation`，是本轮引入的回归 | 不止改传参：把 `load_generation` 提升为 newtype `LoadGeneration`，`start_file_index` / `finish_load` 一起改签名。两个代次都是 `u64` 时混用编译得过、行为却静默错误；类型不同之后这一类错误**编译不过**，比补一条测试更彻底 |
| P2-2 | `is_pristine` 不看附件：在初始空标签上挂了图再「新建会话」，该标签被判为干净而原地复用，`start_new_session` 只清了 composer 文本与草稿，那张无关的图被静默带进新会话 | `is_pristine` 增加 `attachments.is_empty()`。判成不干净就另开一个标签，图留在原处——既不串台，也不丢用户已经做过的操作 |

**为什么 P2-1 没有配回归测试**：它的自然测法是跑一次 `start_new_session`，但那条路径会
`request_run` 真去 spawn `vendor/pi/pi.exe`，并落到真实的 `~/.pi`（红线 5）。为一条测试在生产代码里
开一个二进制注入口不划算，而 newtype 已经把这一类错误挡在编译期。这一条如实记为「类型约束替代测试」。
P2-2 有回归测试 `an_attachment_on_the_empty_tab_keeps_it_from_being_reused`（只走 `open_tab`，不启动会话）。

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **140 passed**。

> 过程记录：这轮整改我一度只跑了 `cargo check` + `cargo test` 就准备收口，
> 漏掉的 `unused_variables` 被 `validate` 的 `clippy -D warnings` 拦下。
> **`cargo check` 不 deny warnings，不能替代 validate。**

### 第三轮独立代码审查与整改

第二轮的整改本身也没被审过，因此再跑一轮闭环。通道同前，Codex thread
`01a03874-8c7e-7f80-b795-d4e80888f355`。结论：**1 项 P1 + 2 项 P2**，核对源码后确认**全部成立**。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P1 | 每个 Runtime 的 dirty 桥仍然用 `mpsc::unbounded()`。多会话之后每个运行中的会话各有一条桥，GPUI 执行器一卡，R22 的有界保证就被会话数乘一遍 | 改成 `mpsc::channel(0)` + `try_send`，与调度器桥同一条合并规则。`Dirty` 只是「有变化了」的信号，`pull_runtime_snapshot` 回来会重读完整 Snapshot，丢掉中间帧无损 | 无新增用例：与首轮 P2 同类的构造性收敛。**首轮我只把调度器桥收敛了，把这条同构的漏掉了**——两条桥形制不一致本身就是这次被抓的原因 |
| P2 | `subscribe_dirty()` 只登记发送端、不补发当前修订号。启动或热接管的元数据可能在 `reconcile_scheduler` 订阅**之前**就跑完，那几条 Dirty 发给了零个订阅者；`install_active` 又不投影 Snapshot，于是这个标签一直没有模型、没有 slash 命令、没有启动诊断，空闲会话可能永远等不到下一次事件 | `attach_runtime` 装上句柄后立刻 `pull_runtime_snapshot` 补一次 | 见下 |
| P2 | 两个标签属于不同工作目录时，切标签只更新了 `ChatPanel`；`Workspace.selected_directory`、文件浏览器根、`MainPanel` 根只认侧栏与新建会话事件。结果是「B 的对话配着 A 的文件树」，工作区操作还打在 A 上 | 新增 `FocusedSessionChanged { cwd }` 事件，`focus_tab` / `close_tab` 广播，`Workspace` 订阅后走既有的 `apply_browsing_root`；目录未变则跳过，切标签是高频操作 | `switching_session_tabs_moves_the_workspace_roots`（workspace 层端到端：切标签后浏览器根与工作区根都跟着走） |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **141 passed**
（`model_service` 的已知 flake BACKLOG #27 偶发，重跑即过）。

> 三轮审查的账：首轮 2 条、二轮 2 条、三轮 3 条，共 7 条 findings 全部成立、全部整改。
> 其中**至少 3 条是我自己引入的回归**（续体串台、fresh 会话文件索引代次、只收敛了一半的桥），
> 说明「实现完 → validate 全绿」远不等于可以收口。

### 第四轮独立代码审查与整改

Codex thread `01a0388a-369d-7542-ba59-16a05eb67ece`。结论：**3 项 P2**，核对源码后确认**全部成立**。
其中两条是第三轮整改自己带出来的新问题——修一个同步问题时只覆盖了部分入口。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P2 | 标签已达上限时再从侧栏选会话，`load_selection` 只在自己身上留一条错误就返回，而 `Workspace` **已经无条件**把工具栏、文件浏览器和工作区根切到了那个被拒绝的会话——聊天还在旧会话，工作区却指向新的 | `load_selection` 改为返回 `Result`；`Workspace` 先让聊天面板受理，被拒就弹通知并原样返回，不搬任何根目录 | 在 `the_tab_strip_is_bounded_and_refuses_instead_of_evicting` 里补断言 |
| P2 | `open_tab` 换 `focused` 时没有 `apply_window_title`。新标签的 `window_title` 是初值 `GPUI-Pi`，而窗口上挂的可能是上一个会话由 Extension UI 设的标题；`process_extension_ui` 只在「与本标签记录值不同」时才写窗口，两者恰好相等就永远不会纠正，旧标题一直挂着 | `open_tab` 无条件写一次窗口标题。这与第三轮给 `focus_tab` / `close_tab` 补的是同一条修复——当时漏了 `open_tab` 这个入口 | 无：`window.set_window_title` 在 GPUI 测试里没有可读回的观测点。按「所有改 `focused` 的路径都必须 `apply_window_title`」的不变量收敛，三个入口现已齐全 |
| P2 | 两个标签同属一个项目时，`FocusedSessionChanged` 里的 `cwd` 相同，第三轮加的「目录没变就短路」会连**标题状态**一起跳过，于是工具栏一直显示上一个会话的名字 | 标题状态改为无条件跟上，短路只跳过昂贵的文件树重建；同时把工具栏标题的数据源从「侧栏最近选中的那一条」换成「前台会话标签」（`focused_session`），并让事件带上 `title` / `session_key` | `the_toolbar_title_follows_the_focused_tab_within_one_project` |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **142 passed**。

> 四轮共 **10 条 findings，全部成立、全部整改**。分布很说明问题：
> 首轮 2、二轮 2、三轮 3、四轮 3——**数量没有随轮次收敛**，因为每一轮的整改本身都会引入新的
> 待审面。真正收敛的是性质：前两轮是「实现里的洞」，后两轮是「修复覆盖不全」（同一条修复漏了某个入口、
> 同一类结构只收敛了一半）。这一点值得写进后续轮次的经验：**findings 驱动的整改必须重新过审**，
> 而且要专门检查「这条修复的同类入口是不是都覆盖了」。

### 第五轮独立代码审查与整改

Codex thread `01a03897-99fa-7db2-b33d-f937d3544982`。结论：**2 项 P2**，核对源码后确认**全部成立**。
两条又都是前几轮整改的副产物。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P2 | 第二轮为保住附件让「带附件的空标签」不被复用，但那个标签仍然没有 `draft_key`（那是 pi 会话身份），于是 `save_current_draft` 是空操作、`open_tab` 又清空了共享 composer——在空标签上写的字，去开别的标签再切回来就没了 | 拆开两个职责：`draft_key` 仍只表示 pi 会话身份，另加 `SessionUiState::draft_slot_key()`，没有会话身份时退回标签自己的身份。**所有草稿读写路径统一走它**（共 9 处：回填、变更、提交清空、增删附件、fork 回填、Extension UI 写入、提交被拒恢复）。`is_pristine` 同时升级为 `is_focused_tab_pristine`，把「写过字」也算作用过，并要求调用前先存草稿 | `text_typed_on_an_empty_tab_survives_a_detour_to_another_tab` |
| P2 | 关掉最后一个会话标签后，`ChatPanel` 会重置出一个空白标签并广播「没有任何身份」的事件，第四轮的处理照单收下，于是工具栏显示成「新标签」，反而盖掉仍然有效的项目目录回退 | 没有 `cwd` 也没有 `session_key` 的事件按 `None` 处理，恢复既有的目录回退 | `closing_the_last_session_falls_back_to_the_project_directory` |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **144 passed**。

> 第五轮的第一条值得单独记：它暴露的是**一个字段承担了两个职责**——`draft_key` 既是 pi 会话身份
> 又是草稿存放位置。前四轮一直在这个含糊上打补丁（谁该判 pristine、谁该存草稿），直到把两个职责
> 拆开才真正到底。**反复在同一处出 findings，往往说明那里有个没拆开的概念，而不是又一个疏忽。**

### 第六轮独立代码审查与整改

Codex thread `01a038ad-3b00-7cb2-b43e-4455c13b46c5`。结论：**3 项 P2**，核对源码后确认**全部成立**。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P2 | 用项目选择器换项目时，`focused_session` 没被清掉。而工具栏标题优先认它，于是「浏览的是项目 B，标题还写着会话 A」。改造前这条路径本来有一句 `selected_session = None`，是我在第四轮换字段时**连带删掉了** | 在 `choose_directory` 里恢复清理。**不能放进 `apply_browsing_root`**——前台会话变更的处理器正是「先设 `focused_session` 再调它」，放进去会把刚设的值清掉 | 无独立用例：与既有的 `directory_button_opens_native_directory_prompt` 同一路径，断言点在标题回退，已由第五轮的 `closing_the_last_session_falls_back_to_the_project_directory` 覆盖同一回退逻辑 |
| P2 | 侧栏点一个**已经打开**的会话时只切标签，不更新标题、且当它已经是前台时连身份广播都没有。于是「改名 → 点它」之后，标签和工具栏都还是旧名字 | 复用分支里以本次选择的 `title` 覆盖标签标题；已经是前台时显式广播一次身份（`focus_tab` 对「已是前台」直接返回，走不到广播） | `reopening_a_renamed_session_updates_its_tab_and_closing_frees_its_draft` |
| P2 | 第五轮给匿名标签配的 `tab-{id}` 草稿键，在标签关闭后**再也取不到**却一直留在 `DraftStore` 里。反复开关带图片的匿名标签就是一条随次数增长的内存泄漏（一张图可能几十 MB） | `close_tab` 里对没有 `draft_key` 的标签清掉它的兜底草稿键；有会话身份的不动，那份按 pi 会话身份存、重开还要用 | 同上用例的后半段 |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **145 passed**。

> 第三条是典型的「修复自带成本」：第五轮为了不丢草稿给每个标签配了兜底键，
> 却没想清楚这个键**什么时候该消失**。给资源加生命周期起点时必须同时给出终点，
> 否则修好一个丢失问题就换来一个泄漏问题。

### 第七轮独立代码审查与整改

Codex thread `01a038bf-d010-7b11-88df-91353d387c91`。结论：**1 项 P2**，成立。数量首次下降。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P2 | 往空标签里拖一张非法图片：附件没加上、`rpc_error` 却留下了，而这仍然满足「用户没往里放过东西」。复用分支只改标题，那条不相干的红色横幅就跟进了新开的会话 | **改成原地重建整个槽**，而不是再往 `is_focused_tab_pristine` 里加一个条件。逐个字段去清是在追着枚举，以后新增一个瞬时字段就漏一个；换一份全新的 `SessionUiState` 之后，新增字段自动被覆盖。只显式保留三项**用户偏好**（`tool_preset` / `minimap_visible` / `composer_mode`）——在空标签上先挑好工具预设再开会话是正常用法 | `reusing_the_pristine_tab_drops_its_leftover_error` |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **146 passed**。

> 这条与第五轮那条是**同一个模式的两次实例**：前六轮一直在给「什么算干净」这个判据加条件
> （没有会话、没有草稿键、没有附件、没有草稿文字……），每加一条就等下一个字段来打脸。
> 第七轮换了做法——**不再枚举「哪些状态算脏」，而是直接给出一个干净的状态**。
> 判据式修复的成本随字段数线性增长且永远漏一个；构造式修复是常数成本。
> 这是本轮最值得带走的一条工程判断。

### 第八轮独立代码审查与整改

Codex thread `01a038d2-574e-71b3-8f30-8ec3c496e231`。结论：**1 项 P2 + 1 项 P3**，均成立。
首次出现 P3（最低档），且两条都不再是「用户能直接撞上的错」。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P2 | 第三轮加的「装上句柄后补拉一次 Snapshot」忽略了返回值。若子进程在「Manager 发布 `Running`」与「订阅装好」之间就退出了，订阅不补发之前的 Dirty，**这次补拉就是唯一一次能看到终态的机会**；忽略它，运行槽会一直挂在 `Running` 上，排队会话要等 reaper 轮询才补位 | 接住返回值，终态时按事件泵的同一条路径收口（后台 `tick()`）。放后台顺带避免了「reconcile → attach → reconcile」的递归 | 无独立用例：需要「进程在两个时刻之间恰好退出」的竞态窗口，app 层没有可注入的接缝。与事件泵终态路径同一处理，后者已由 `pi-runtime` 的终态用例覆盖 |
| P3 | fresh 会话先塞了一个 `fresh-{tab}-{generation}` 当 `draft_key`，而该字段是**对外的 pi 会话身份**（标签去重认它、工作区 tooltip 显示它），于是那个编出来的值会以「真实身份」的名义漏到界面上，直到 `ControlsLoaded` 才被换掉 | 干脆不编：fresh 会话的 `draft_key` 保持 `None`，草稿走第五轮引入的 `draft_slot_key()` 兜底；`ControlsLoaded` 拿到真 id 时从兜底键迁移过去，并在前台标签上补广播一次身份 | `a_fresh_session_publishes_no_identity_until_it_is_calibrated` |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **146 passed**。

> P3 这条是第五轮那次拆分的**红利**：`draft_slot_key()` 一旦存在，「给 fresh 会话编一个身份」
> 这件事就没有必要了，删掉即可。把概念拆对之后，后面的问题往往不是「再修一处」而是「少做一件事」。

### 第九轮独立代码审查与整改

Codex thread `01a038e6-ebbf-75d2-b71f-d2b7658b3a1a`。结论：**1 项 P2**，成立。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P2 | `reset_extension_ui` 里混着两个层级的状态：标签自己的（待回响应、`ExtensionUiState`）和窗口级的（对话框焦点句柄、窗口标题）。关**后台**标签时我把它整个投影进那一槽，于是前台正开着的对话框被清掉了焦点句柄——此后 `extension_dialog_is_topmost` 认不出它是最上层，只置 `needs_close` 却关不掉，一个关不掉的模态浮在别的会话上，它那条请求也永远回不去 | 按层级拆成两个函数：`reset_extension_ui_slot`（只碰标签自己，任何标签都能安全调用）与 `reset_foreground_extension_ui`（前台专用，额外收拾窗口级状态）。`close_tab` 按「关的是不是前台」二选一 | `closing_a_background_tab_leaves_the_foreground_dialog_alone` |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **147 passed**。

> 又一次「一个函数担了两个层级」。与第五轮的 `draft_key`、第七轮的 pristine 判据同源：
> **多会话把原本只有一份的状态劈成了「每标签一份」和「整窗口一份」两类，
> 而所有单会话时代写的代码都默认这两类是同一回事。** 本轮的 findings 有相当一部分
> 就是这条默认假设在各个角落的残留。

### 第十轮独立代码审查与整改

Codex thread `01a038f4-7bd9-71e1-8943-797c05468e92`。结论：**2 项 P1**，均成立。
严重度在第 8–9 轮降到 P2/P3 之后**回升**——这两条都不是「显示不对」，而是权限与落盘正确性。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P1 | 会话处于 `Parked` / `Failed` 时工具选择器仍然可用，但 `set_tool_preset` 因为 `active` 是 `None` 只改了 `SessionUiState`。「恢复运行」是拿 **Manager 的会话描述**去起进程的，于是挂起一个 Full/Inherit 会话、改成 ReadOnly、再恢复 —— 界面写着 ReadOnly，进程却按旧的宽权限起来了 | `pi-runtime` 新增 `RuntimeManager::set_session_tool_preset`，**只允许改没有 Runtime 的会话**（有 Runtime 必须走 `restart_with_tools` 重启进程，否则「描述」与「进程实际权限」又会分家——R23 审查 P1-1 修的就是这条，不能再开后门）；app 在无 Runtime 分支里同步过去，失败就报错并不改 UI | `a_parked_session_can_have_its_tool_preset_changed_before_it_resumes`（含「运行中必须拒绝且不改动任何状态」） |
| P1 | 「切换会话」走原生选择器，绕开了侧栏选择时的 `slot_index_for_key` 去重，也没做路径撞车检查。多标签并行时可以选中**另一个标签已经占着**的 JSONL，于是两个 pi 进程绑同一份文件各自追加，内存历史分叉、落盘历史交错甚至写坏 | 切换前按「已登记会话的标签」查一遍路径撞车，命中就明确拒绝并指出是谁占着（`该会话已在标签「X」中打开，请直接切过去`），不发起任何控制操作。路径比较走 `same_session_file`：Windows 大小写不敏感、还可能差一个 `\\?\` 前缀，直接比 `PathBuf` 会漏判 | `switching_into_a_session_another_tab_owns_is_refused` |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **149 passed**；
`cargo test -p pi-runtime --test multi_session_fake_child` → **5 passed**。

> 严重度回升值得记一笔：前九轮把「状态该归谁」这条线基本理顺了，第十轮换了一个方向 ——
> **多会话让原本互斥的资源变得可以被同时争抢**（同一份会话文件、同一个会话的启动参数）。
> 这是与「状态分层」并列的第二类多会话问题，之前几轮完全没触及。
> 说明 findings 数量下降不等于面已经覆盖完，只说明**当前这条线**挖到底了。

### 第十一轮独立代码审查与整改

Codex thread `01a03b94-a822-73a3-abf8-5834c6c74b06`。结论：**2 项 P1 + 1 项 P2**；
逐条核源码后 **2 条成立、1 条不成立**（那 2 条其实是同一个洞的两端）。

| 编号 | 问题 | 判定与整改 | 回归测试 |
|---|---|---|---|
| P1 | `set_session_tool_preset` 只看 `slot.entry.is_some()`。但 `admit` 在进入 `Starting` 时就把描述**克隆**给了正在冷启动的那个进程，`entry` 要到 `finish_start` 才装上——整个启动窗口里这个判据都是空的（`Stopping` 同理）。于是改预设被接受、写进描述，而进程已经拿着旧预设出发了 | **成立**。判据从 `entry` 换成已有的 `SchedulerState::holds_process()`（`Starting \| IdleWarm \| Running \| Stopping`）：它就是「这个状态占着（或正在占）一个进程」，与运行槽会计同源，将来加状态不必回来补一遍 | `tool_preset_edits_are_refused_in_every_state_that_holds_a_process`（七态逐条钉死：三态允许并落到描述，四态拒绝且不得改描述） |
| P2 | app 侧 `tools_enabled` 漏了 `scheduler_busy`。`request_run_in_background` 期间 `active` 是 `None`，选择器可点、`set_tool_preset` 直接走进「只改会话描述」那条分支；park 作业在飞时 `active` 还在，又能对同一个 Runtime 发起 `restart_with_tools` | **成立**，与上一条合起来才是完整链条：挂起 → 点「恢复运行」→ 在它启动的那几百毫秒里改成 ReadOnly → 界面写 ReadOnly、进程按 Full/Inherit 跑着。新增 `SessionUiState::control_busy()` = `control_operation.is_some() \|\| scheduler_job.is_some()`，**按钮的 `disabled` 与 handler 的早退共用这一个**；删掉原先散在 `can_park` / 启动按钮 / `controls_enabled` / `tools_enabled` / `abort_retry_disabled` 五处的组合判据 | `runtime_controls_are_locked_while_a_scheduler_job_owns_the_session`（busy 期间 UI 与 Manager 描述都不动、也不留误导性错误横幅；落地后照常可改并同步到描述） |
| P1 | 切标签时若旧对话框不是最上层，`suspend_foreground_dialog` 只在**旧槽**记下 `extension_dialog_needs_close` 就返回，而 `process_extension_ui` 此后只为新前台标签跑，对话框会留在别的会话上 | **不成立**。链条要求「Extension UI 对话框开着时切标签 / 关标签」，但它是 `overlay_closable(false)` 的真模态：gpui-component 的 `Dialog` 用 `anchored().snap_to_window()` 套全窗口 `.occlude()` 背板，`dialog_layer` 在 `workspace.rs` 里渲染在 `AppShell` 之后，整窗口点击都被挡住；而 `focus_tab` / `close_tab` 的全部入口都是鼠标点击（标签条 `on_select` / `on_close`、侧栏选中经 `load_selection`），`crates/app` 与 `crates/ui` 里没有任何 `actions!` / `KeyBinding` / `on_action`，没有键盘路径。该分支在产品里到不了，本轮不改代码 | — |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **150 passed**；
`cargo test -p pi-runtime --lib` → **81 passed**。

> 两条成立的 findings 落在第十轮那条线的延长线上：**多会话让原本互斥的资源可以被同时争抢**。
> 第十轮是两个标签抢同一份会话文件，这一轮是 UI 与调度器抢同一个会话的启动参数。
> 更值得记的是整改形态：第一反应是往 `tools_enabled` 再加一个 `&& !scheduler_busy`、
> 往 `set_session_tool_preset` 再加一个状态判断——那正是第七轮认定会「永远漏一个」的判据式修复。
> 两处最后都换成了**已有的单一概念**（`holds_process()` / `control_busy()`），
> 各调用点共用同一个判据，新增状态或新增按钮都不必回来补。

### 第十二轮独立代码审查与整改

Codex thread `01a03bbc-8074-76b1-a7c7-e46a492bcb43`。结论：**1 项 P1 + 2 项 P2，三条全部成立**。
三条都在第十轮开的那条线上：**多会话让原本互斥的资源可以被同时争抢**。

| 编号 | 问题 | 整改 | 回归测试 |
|---|---|---|---|
| P1 | 第十一轮加的切换守卫只查 `slot.session.is_some()` 的标签。只读历史标签没登记会话，于是不算占用者：别的标签可以切进它显示的那份 JSONL，两个标签共用同一个 `draft_key`（草稿互相覆盖），随后把历史标签启起来就是两个 pi 进程写同一份文件。另外切换在飞时 `bound_session_file()` 报的还是**旧**文件，两个标签能同时切到同一份 | 归属判据收敛成 `ChatPanel::session_file_owner()` 一个函数，主张由 `SessionUiState::claimed_session_files()` 给出：已经绑上的 + 正在切过去的（`pending_switch_target`，只在 `control_operation == SwitchSession` 期间有意义，因此不需要单独清理）。**并让真正会起进程的另一条入口 `start_live` 也问它**——此前只守了切换，没守启动 | `a_history_only_tab_still_owns_its_session_file`（切换与启动两条入口都拒绝，且被拒时不留主张） |
| P2 | `close_tab` 立刻摘标签、把 `remove_session` 放后台（它要等优雅停机）。这段时间里那份 JSONL 的主张随标签一起消失，而进程还活着——「关掉 → 重新打开同一段历史 → 启动活会话」会在旧进程退干净之前起第二个进程写它 | `ChatPanel` 新增 `closing_session_files` 墓碑（按 `tab_id` 记账），`close_tab` 压入、后台作业落地时摘除；`session_file_owner` 一并查它 | `a_closing_tab_keeps_its_session_file_claimed_until_the_process_is_gone`（含「落地后必须摘除，否则那份文件就永远打不开了」） |
| P2 | `request_run` 的第一件事是 `tick()`，而 `promote_queued` 按队列里**现有的**优先级挑人；`slot.priority = priority` 要到随后的 `admit` 才执行。于是「切到一个 Queued 标签」这次调用会先用旧优先级把刚空出来的槽让给别的后台会话，自己才被抬成 FOREGROUND | `pi-runtime` 新增 `RuntimeManager::reprioritize()`：只更新调度优先级、不做任何状态转移、不入队，队列侧沿用 `WaitQueue::push` 既有的「只升不降 + 不刷新 `enqueued_at`」；app 在 `request_run` 之前先调它。**没有改动 `request_run` 自身的语义**，是加一步而不是改一步 | `reprioritize_raises_a_queued_entry_before_the_next_promotion_tick`（抬前抬后 `peek_next` 各挑谁、只升不降、不把未排队会话塞进队列） |

整改后：`.\scripts\validate.ps1` → `VALIDATE OK`；`cargo test -p gpui-pi` → **152 passed**；
`cargo test -p pi-runtime --lib` → **82 passed**；`--test multi_session_fake_child` → **5 passed**。

> 第十一轮刚把「谁能改工具预设」收敛成一个判据，这一轮同样的形状又出现在「谁占着这份会话文件」上：
> 第十轮建立守卫、第十一轮没动它、第十二轮发现它从一开始就只覆盖了**一类**占用者，
> 而且**另一条会起进程的入口根本没被守**。教训不是「再补一个条件」，而是
> **凡是「某资源归谁」的问题，判据要有唯一的落点，并且所有会取得该资源的入口都从那里问**——
> 这次把 `session_file_owner()` 做成那个落点，切换与启动两条入口共用。

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
