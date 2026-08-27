# Round 26 — 内建子代理（集成 `pi-subagents-lite` 为执行内核）

> 执行方：Windows · 状态：进行中

## 目标

GPUI-Pi 随包携带钉死的 `pi-subagents-lite@1.13.0` 作为子代理执行内核，用户无需自行 `pi install` 即可在桌面端派发子代理，并在专用面板里看到排队 / 运行 / 完成 / 失败与统计，历史结果可从父会话文件回看。

## 方向变更说明（本轮前置）

立项文档原定 R26 为「内建**只读**子代理任务与配额调度」，走 Manager 派发 + `new_session { parentSession }` 落盘 + 派发式配额。项目所有者于 **2026-08-27** 裁定改为：

1. 主题去掉「只读」，改为**内建子代理**；
2. **不从 0 自建**，集成 `npm:pi-subagents-lite@1.13.0` 作为执行内核；
3. 依赖走 **vendor 钉死 + 本地路径注入**，不用 `pi install`。

前置调研见 [`调研-pi-subagents-lite.md`](调研-pi-subagents-lite.md)。对应的权威文档改动已落在
`docs/立项文档.md` § 二「R26 新增外部依赖的口径」、§ 三「R26 勘误」、§ 七 R26 行，以及 `CLAUDE.md` / `AGENTS.md` 的镜像条目。

**该内核相对立项文档原假设的三条硬差异**（验收表述必须与之一致）：

- **进程内执行，不 spawn 子进程** —— R25 的 per-Runtime *进程数*配额恒不触发；*内存*却全计入父 Runtime，硬限会连带打死父会话。
- **子代理会话 `SessionManager.inMemory`，不落盘** —— 「可恢复」本轮**拿不到**，禁止在任何验收里声称拿到。「可回看」改由父会话文件的 `subagent-result` `custom_message` 条目承载。
- **自带 UI 在 RPC 模式全失效**（`ctx.ui.custom()` 返回 `undefined`、`setWidget` 组件工厂被忽略，且扩展无 `ctx.mode` 降级分支）—— 进度与结果 UI **必须由 GPUI 侧实现**。

## 前置

- R21–R25 已完成（`RuntimeManager` 为唯一生产创建入口、有界 Actor、Scheduler、多会话 UI、Job Object 与内存治理）。
- 新 round 启动门禁已过：本 worktree 内 `fetch-pi` / `fetch-pi-source` / `fetch-pi-web` / `check-pins` 全绿（2026-08-27 实测）。
- 需要一次联网以建立 `vendor/pi-subagents-lite-1.13.0/` 与 `pins/` 基线；之后走 `GPUI_PI_CACHE` 离线复现。

## 交付物

### A. 依赖钉死
- `scripts/fetch-pi-subagents-lite.ps1` —— 直接下载并解包**两个**钉死 tarball（`pi-subagents-lite@1.13.0` + `@sinclair/typebox@0.34.52`），各按 registry `dist.integrity` 校验；**不调用 npm**，走 `GPUI_PI_CACHE` 缓存
- `pins/pi-subagents-lite-1.13.0.manifest` —— 全量文件校验基线
- `scripts/check-pins.ps1` —— 增加该包的校验段
- `docs/立项文档.md` / `CLAUDE.md` / `AGENTS.md` / `ROUNDS.md` —— 口径同步（**已完成**）

### B. 注入
- `crates/pi-rpc/src/lib.rs` —— `PINNED_SUBAGENTS_LITE_VERSION` / `subagent_kernel_dir_name()` / `SUBAGENT_TOOL_NAMES`
- `crates/pi-runtime/src/lib.rs` —— `official_subagent_kernel()`；`active_session_config_with_sources` 追加第二个 `-e`；`ToolPreset` 新增 `allows_subagents()` 与 `loads_subagent_kernel()`

> **实施中的口径修正**：任务卡初稿写的是「非 `Inherit` 的预设都要放行三个子代理工具」。实现时核对 `pi-subagents-lite` 源码发现
> 子代理的工具集来自 agent 定义的 `tools:` frontmatter 与 pi settings 的 `defaultTools`，**父会话的 `--tools` 传导不过去**。
> 因此 `ReadOnly` 若放行 `Agent`，只读会话就能借一个带 `bash` / `edit` 的 agent 绕开"只读"承诺 —— 这是提权口子，不是便利。
> 最终口径：`Inherit` 注入内核但不下发 `--tools`；`Default` / `Full` 注入并在允许列表里显式列出；`ReadOnly` / `None` 既不注入也不列出。
> `docs/立项文档.md` § 七 R26 行已同步。

### C. 任务模型（纯逻辑，无 GPUI）
- `crates/pi-render/src/subagent.rs` —— `SubagentTask` 与状态归并；输入只认三个事实来源：`tool_execution_*` 事件、`subagent-result` `custom_message` 条目、`details.outputFile`。**刻意不带结果正文与 prompt**：那两个字段 UI 零引用，却会在每帧重算时复制一份刚被负载预算压下去的内容
- 从会话文件重建历史子代理结果（回看路径）

### D. UI
- `crates/ui` / `crates/app` —— 子代理任务面板 + `Agent` 工具卡片专用渲染

### E. 配额与内存口径
- `crates/pi-runtime/src/subagent_config.rs` —— **只读**解析内核并发配置（全局 + 项目两层）
- `MemoryLimits::subagent_slot_bytes` + `job_limits_with_subagent_slots()`；`clamp_manager_config_with_subagent_slots` 按预设决定是否预留

> **实施中的口径修正（二）**：任务卡初稿写的是「Manager 把并发上限**投影为** `.pi/subagents-lite.json`」，即由 GPUI-Pi 代写该文件。
> 实现时确认内核**没有任何环境变量或命令行入口**，配置只来自全局 `~/.pi/agent/subagents-lite.json` 与项目 `<cwd>/.pi/subagents-lite.json` 两个文件。
> 这两份都是用户自己的 pi 配置：全局那份与终端 pi、pi-web-desktop 共享（红线 5），项目那份会出现在用户仓库的 `git status` 里。
> 替用户改写它们属于未经请求地修改其持久配置，因此最终口径改为**只读**：Manager 读取生效上限，据此为加载内核的 Runtime 预留内存余量；
> 真正的强制仍来自 R25 的 Job Object 兜底。`docs/立项文档.md` § 三勘误与 § 七 R26 行已同步。

### F. 唯一子代理通道（2026-08-27 所有者补充裁定，当轮实施）

- 背景：BACKLOG #41 —— 生产路径不传 `--no-extensions`，用户全局自装的 `npm:pi-subagents`（工具名 `subagent`）与内核同场加载，真实派发时模型挑走了第三方工具，内建面板整块未被触发。所有者当日改判：不再留给独立产品决策轮，当轮采用核实过的出路 ①。
- `pi_rpc::THIRD_PARTY_SUBAGENT_TOOL_NAMES` —— 已知第三方子代理工具名名单（`subagent` / `subagent_wait` / `subagent_supervisor`，即 npm:pi-subagents@0.56.0 在父会话注册的全部三个工具，逐名对着扩展源码核实后才准入列；`contact_supervisor` 有子进程门禁、刻意不入列）；
- `active_session_config_with_sources` 对**每个** `ToolPreset` 恒追加 `--exclude-tools <名单>`：对四个显式预设是 allowlist 之外的冗余保险，真正的洞在不下发 `--tools` 的 `Inherit`；
- 边界：进程级参数只作用于本应用拉起的 `pi --mode rpc`，不碰用户终端 pi、不写任何用户配置；第三方扩展**本身**照常加载（仅其子代理工具被过滤），用户其他扩展与工具不受影响。

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | vendor 与钉版本 | `.\scripts\fetch-pi-subagents-lite.ps1` 后 `.\scripts\check-pins.ps1` 全绿；`vendor/pi-subagents-lite-1.13.0/node_modules` 只含 `@sinclair/typebox` |
| T1 | 逻辑层单测 | `.\scripts\validate.ps1 -Logic` 全绿，含 `pi-render` 子代理任务模型新增用例 |
| T1 | 全量 | `.\scripts\validate.ps1` 全绿（clippy `-D warnings`） |
| T2 | 扩展确实加载 | `pi --mode rpc -e <vendor 路径>` 的 `get_commands` 含 `agents` 命令，stderr 无错误，且 `~/.pi` 无新增写入 |
| T2 | 预设与子代理的关系 | `Inherit` 注入内核且不下发 `--tools`；`Default` / `Full` 注入且允许列表含 `Agent` / `StopAgent` / `AgentStatus`；`ReadOnly` / `None` 既不注入也不放行（防提权） |
| T2 | 回看 | 父会话文件中的 `subagent-result` 条目能被 `pi-render` 还原成子代理任务并在面板展示 |
| T2 | 唯一通道（生产形态） | 不带 `--no-extensions` 时：`subagent` / `subagent_wait` / `subagent_supervisor` 对模型均不可见，内核 `Agent` 三件套与用户其他扩展工具可见，第三方扩展本身照常加载 |
| T3 | 真实派发 | 桌面端发起一次子代理任务，面板显示状态流转与统计，失败时父会话仍可继续 |

## 禁止

- 禁止用 `pi install` 或 `pi -e npm:<spec>` 形式引入依赖（会写 `~/.pi/agent/tmp`，触红线 5）。
- 禁止声称本轮实现了子代理会话落盘 / 崩溃恢复 —— 内核是 in-memory。
- 禁止把配置式并发上限表述为「Manager 派发式调度」。
- 禁止实现 R27 的 mutating worktree 隔离强制（`worktree_path` 参数透传可以，Manager 侧强制留给 R27）。
- 禁止修改 `Cargo.lock` 中任何上游 package 的 version / source / checksum；本轮不执行 `cargo update`。
- 发现前序轮次问题写 `rounds/BACKLOG.md`，不当场改（红线 3）。

## 失败处理

连续 2 次 validation 不过 → 写 `rounds/round-26/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 视觉审查

本轮 diff 触及 `crates/ui/src/{chat.rs,subagent_panel.rs}` 与 `crates/app/src/panels.rs` 的视图代码，必须触发视觉 review。

- 视觉审查模式：`CODE_ONLY`
- 视觉审查结论：`CODE_ONLY_PASS`（第 3 轮；前两轮均为 `CODE_ONLY_FAIL`，阻断项已逐条整改）
- 截图验证：未提供（`SCREENSHOT_NOT_PROVIDED`）
- 兜底原因：`TIMEOUT_30M`
- `requested_at`：`2026-08-27T10:45:33+08:00`
- `deadline`：`2026-08-27T11:15:33+08:00`
- 审查报告 / 证据：diff 导出于 `.pi/visual-review/round-26/ui-diff.txt`（gitignored）
- 说明：仅完成纯代码层视觉审查，**未验证真实渲染**；不阻塞 PR 的前提是结论为 `CODE_ONLY_PASS`

> **追加 `SCREENSHOT` 审查（2026-08-27 下午，截图后补）**：用户当日在真实派发中逐张回传 15 张截图
> （无法另存文件，由主会话从会话 transcript 提取 base64 落盘至
> `.pi/visual-review/round-26/evidence/`，含 `MANIFEST.md` 角色清单）。`SCREENSHOT` 模式审查结论：
> **已提供证据的用例范围内 PASS（用例 1 变体 / 3 / 4 / 5 / 6 / 9），无确证阻断项**。
> 覆盖事实包括：S-8 三处行内片段收敛在真实渲染中全部成立、状态色只点不铺（失败卡与失败行均无
> 红边框红底）、左竖线 + 缩进从属区、展开区小号弱色正文、行/头两级 tooltip 内容与代码一致、
> 汇总点与行点同源取色（全结算含失败正确转红）。
>
> **本追加结论不等于完整 `PASS`，不得宣称完成全部截图视觉验证**。未验证清单：用例 2（hover 底色，
> 判 `INSUFFICIENT_EVIDENCE`，若坐实无反馈须按红线 9 立阻断）、用例 7（900×700 窄窗）、用例 8
> （无子代理会话回归）、深色主题整组、用例 1 严格版（「1 运行」橙点）、排队/停止/未知三种点色、
> 用例 5 的 header 悬停。环境偏差如实记录：浅色主题（清单原要求深色优先）、窗口 ~2000×1281
> （非 1440×900）、两张含用户手绘红框。审查器另核实出 MANIFEST 初版 img-07/08 标注互换
> （主会话按时间戳推断角色未逐张验证——同根因清单再 +1；「逐张核实」防线按设计接住），已就地勘误。
> 本轮正式门禁结论维持 `CODE_ONLY_PASS` 不回写。

### 视觉审查过程

| 轮 | 结论 | 阻断项 | 处置 |
|---|---|---|---|
| 1 | `CODE_ONLY_FAIL` | **S-8 一行片段超限**：面板折叠行 5 段、任务行 6 段、卡片摘要行 6 段，且都是常态输入 | 按条款重构：统计与汇总各返回 `{ inline, detail }`，行内只放**一项**，完整明细进 tooltip（条款明确 tooltip 内容不计入片段）。改后 2 / 3 / 3 段。另采纳 8 条建议、2 条记 backlog |
| 2 | `CODE_ONLY_FAIL` | **F-1 hover 零像素变化**：为补 § 4.4 反馈加的 `hover(text_color(foreground))` 在代码层可证明无效——该行无基线文本色，`AppShell` 已把环境色设为 `foreground` | 改用 § 4.4 字面规定的 `hover(bg(muted))`；新增源码级回归测试；并更正 backlog #38 里被写失实的那句 |
| 3 | **`CODE_ONLY_PASS`** | 无 | 复核确认 S-8 与 hover 两个阻断项均已在代码层关闭。新提 4 条建议级：**F3-1 我给 F-2 写的回归测试从未调用过 `summary_dot_color`**（守卫是空的，把被守卫的函数整个回退掉测试照样全绿）、F3-2 `Stopped`/`TurnLimit` 归进「完成」桶与行内标签互相打脸、F3-3 § 4.4 的 `text_sm` 与汇总条实际用的 `text_xs` 对不上（判为不改代码、交规范维护轮）、F3-4 我在 backlog 里引入的未转义竖线切断了表格。F3-1 / F3-2 / F3-4 已修，F3-3 记 backlog #40 |

**这两轮阻断项的共同点**：我都写了一条断言性注释（"整段汇总只当一个片段" / "折叠头必须有 hover 反馈"），
而那个断言从未被任何东西验证过。第 1 轮是对规范条款的自我解读，第 2 轮是对自己代码效果的自我判断。
两次都是**注释在陈述"我以为会发生什么"，而不是"已经验证会发生什么"**——与本轮代码审查中那 4 次
"从字段名反推上游行为"是同一个根因。

**而第 3 轮又抓到同一模式的第 3 次**：我为 F-2 写的回归测试名叫
「汇总点永远不与它汇总的行矛盾」，却从头到尾没调用过 `summary_dot_color` ——
一个名字承诺守护 X、实际没碰 X 的守卫，比没有守卫更糟，因为它让人以为有。
这条已改成真正调用被守卫函数并逐分支断言。

因此本轮新增的两条测试刻意选了**源码级断言**（`collapse_headers_use_a_hover_effect_that_actually_changes_something`
断言不出现会退化成 no-op 的写法；`subagent_stats_keep_the_inline_row_to_a_single_fragment` 断言 `inline` 不含 `·`）：
这类问题靠渲染测试很难发现（no-op 的 hover 渲染出来"看着没问题"），只有把**禁止的写法本身**钉死才挡得住回退。
注意第一版这条测试做了全文件否定断言，误伤了 thinking 折叠头那处合法用法（它有基线色），收窄后才对——
说明连"防止犯错的机制"本身也需要验证。

### 截图请求清单

**已迁出并重写**：见同目录 [`截图用例.md`](截图用例.md)。

原清单（`requested_at` 10:45 那份）写在 S-8 与 hover 两个视觉阻断项整改**之前**，
其中的预期文案已与当前实现不符（例如行内汇总当时写的是 `1 运行 · 1 完成`，
而按 S-8 整改后行内只保留一段、明细进 tooltip），继续照它比对会得出错误结论，因此作废。

新清单共 8 个必需用例 + 2 个可选，覆盖：面板折叠/hover/tooltip/展开、卡片折叠/展开、
窄窗口 900×700 溢出、无子代理会话回归。

## 本轮实测

### 依赖钉死

| 项 | 实测 |
|---|---|
| `pi-subagents-lite` tarball sha512 | `sha512-g6kyfhC762ju1Esr7/6O3WQKrSXPtEsACC+O9UK1jLQ2d5gFTZU1F39mB8AkpuyOMHgVemZVqr4rXbmycr1fIw==`（与 npm registry `dist.integrity` 一致） |
| `@sinclair/typebox` tarball sha512 | `sha512-XiMQh7qqVlxZzcVD+kkGMNGMzcTrDMLWI7S4x7z1MkCkbDPrekpZXEUK0eZqZFMuHQg2a2DZOcDIh9o5v3Gonw==` |
| manifest 文件数 | 1133（59 个包内文件 + 1075 个 typebox 文件 − 1 个 marker） |
| vendor 树最长绝对路径 | 203 字符（仓库前缀 106 + `node_modules/@sinclair/typebox/build/esm/type/constructor-parameters/…` 97） |
| fetch 四条路径 | 缓存命中 / vendor 快路径 / 冷启动建缓存 / **无缓存兜底（`GPUI_PI_CACHE=OFF`，CI 走的正是这条）** 均实测 `exit=0` |
| 兜底路径长度 | 缩短临时目录名与解包标签后 267 → **209**（`MaxPathLimit` = 259） |

**踩的坑（一）：manifest 被静默截断成 62 个文件。** 第一版 bootstrap 跑在很深的 scratchpad 路径下，
`Directory.EnumerateFiles` 枚举出的长路径超过 `MAX_PATH`，`File.OpenRead` 逐个抛异常却没有中断循环，
最后生成了一份"每行都对、只是少了 1071 行"的假基线 —— 而逐行比对只会报 count mismatch，看不出是采集失败。
修法：`check-pi-subagents-lite-pin.ps1` 里给读文件套 try/catch 并**抛错而不是跳过**，加零文件护栏，
再加显式 `MAX_PATH` 预检。（原本想用 `\\?\` 扩展长度路径绕过，实测 Windows PowerShell 5.1 的
.NET Framework `EnumerateFiles` 直接拒收：`Illegal characters in path`。）

**踩的坑（二）：`npm install` 会把 peerDependencies 一起装进来。** 不加 `--legacy-peer-deps` 时实测多装
**90 个包 / 75MB**（`@earendil-works/*`、`@aws-sdk/*`、`openai` 等），而那些必须由 pi 二进制自身提供。
最终干脆不用 npm：直接下载并解包两个钉死 tarball，确定性最好，也顺带避开 typebox `^0.34.52` 这个
caret 区间会随时间漂移的问题。

### T2 验收实测

| 检查 | 实测 |
|---|---|
| 扩展从 vendor 真实路径加载 | `pi --mode rpc --no-extensions -e <vendor 绝对路径>` 的 `get_commands` 返回 `agents` 命令，`sourceInfo.path` 指向 `vendor/pi-subagents-lite-1.13.0/src/index.ts`，**stderr 全空**，exit 0 |
| `~/.pi` 无新增写入 | 运行前后对 `~/.pi` 递归 `find` 取快照逐条 diff：**24884 → 24884，新增 0 条**（红线 5） |
| 预设与子代理工具的关系 | 由 `crates/pi-runtime` 的 `active_session_config_always_loads_host_extension_without_changing_tool_presets` 等 4 条用例覆盖五个预设 |
| 回看 | `subagent_result_entries_decode_into_subagent_blocks_for_replay` 走完整 `render_path` 链路，从磁盘 JSONL 还原成子代理卡片并归并成任务 |
| 唯一通道（生产形态） | real-pi 测试 `third_party_subagent_tool_is_excluded_while_other_extensions_survive`：**不带** `--no-extensions`，密封 agent 目录装「注册 `subagent` / `subagent_wait` / `subagent_supervisor` 三个工具的第三方样板 + 注册普通工具的用户扩展」，`-e` 内核 + `--exclude-tools <三名单>` 后由 probe 扩展回报 `getAllTools()` —— 三个第三方工具名均不可见，`Agent` / `StopAgent` / `AgentStatus` 与 `r26_user_other` 可见，`get_commands` 仍含第三方样板注册的命令（扩展本身照常加载）。真实 pi 0.84.2 实跑 exit 0 |

> ~~**T3（桌面端真实派发）未做** —— 需要真实模型调用与人工操作，本轮未执行，不得视为已验收。~~
> → **2026-08-27 下午已完成**，见下方「T3 验收实测」。

### T3 验收实测（2026-08-27 下午，真实派发）

用户以 `cargo run -p gpui-pi`（debug 构建）+ 「跟随 pi」预设 + DeepSeek V4 Flash 真实派发，
全程截图证据落盘 `.pi/visual-review/round-26/evidence/`（15 张 + `MANIFEST.md`）。共 6 个
`Agent` 工具后台子代理（4 完成 / 2 失败），T3 三要素逐一有实证：

| 要素 | 实证 |
|---|---|
| 派发 | 模型在用户全局 `npm:pi-subagents` 照常加载的条件下调用内核 `Agent` 工具（img-02 派发表格 + 面板出现）；对照 img-01（改动前旧构建，模型挑走第三方 `subagent`，面板整块不渲染）——**「唯一通道」在生产环境实证生效**，与 real-pi 自动化测试互为印证 |
| 状态流转与统计 | 面板行「运行中（橙点）→ 耗时（绿点）/ 失败（红点）」全程可见（img-02→03→05→10）；行 tooltip 完整统计两次实拍（`11 轮 · 28 次工具 · 2m48s · $0.0490`、`4 轮 · 10 次工具 · 1m37s · $0.0148`）；折叠头 tooltip `2 失败 · 4 完成`（img-15）；结果卡折叠/展开均正常（img-06/12/14），完成卡展开区完整渲染 12m06s 长任务的 Markdown 报告 |
| 失败不拖垮父会话 | 两个子代理被用户侧 DeepSeek 网关中止（卡片展开可见 `Error: Explore (variflight-ticket/virtual_deepseek): The operation was aborted.`，img-12）；父会话未受影响，模型自行识别失败并重派，重派任务完成（img-10 四绿两红全结算） |

过程中另暴露并当轮修复一个**非本功能**的阻断问题：debug 构建启动即主线程栈溢出
（`0xc00000fd`，与会话内容无关，空 agent 目录也必崩；release 正常）。定位为 Windows 主线程
默认 1MB 栈不够 GPUI debug 布局深度，`crates/app/build.rs` 追加 `/STACK:8388608` 修复
（commit `108c685`），修复后 debug 版沙箱与真实数据均稳定。用户测试期间的「黑屏/崩溃」
均源于此，非子代理功能缺陷。

> **T2 的一处实测盲区（2026-08-27 由一次真实截图暴露）**：上表「扩展确实加载」用的命令是
> `pi --mode rpc --no-extensions -e <vendor 路径>`，把内核**隔离**出来验；而生产路径
> （`active_session_config_with_sources`）**不传** `--no-extensions`，用户 `~/.pi` 下自装的扩展会同时加载。
>
> 本机真实运行时，用户装有 `npm:pi-subagents`（工具名 **`subagent`**，与 lite 版的 `Agent` 不同）。
> 发出「后台并行派两个 scout」后，模型选了**那一套**：会话里出现它的工作流 ID 与
> `pi-subagents.async-status-snapshot` widget，但没有任何 `Agent` 工具调用，
> 于是 `collect_tasks` 找不到任务、子代理面板整块不渲染 —— **看起来像功能坏了，实则是没被调用**。
>
> 这不是本轮代码的缺陷（面板正确地只认自己内核的任务，与立项文档 § 三「两类子代理」的边界一致），
> 但它意味着「R26 能用」与「R26 在装了其他子代理扩展的机器上能用」是两回事。
> 已记 `rounds/BACKLOG.md` #41，两条可选出路（定向 `--exclude-tools` / `--no-extensions`）
> 均属产品级行为变更、超出本轮范围，经所有者裁定留给独立产品决策轮。
>
> **同日后续（所有者改判，当轮收口）**：上一段的「留给独立产品决策轮」当日被推翻 ——
> 所有者裁定当轮实施出路 ①，见交付物 F。该盲区随之被真正关掉：新增的生产形态
> real-pi 测试不带 `--no-extensions`、带第三方样板扩展同场，见上表「唯一通道（生产形态）」行。
> 此后截图取证**不再依赖**「显式点名 `Agent` 工具」的临时办法：`subagent` 已对模型不可见。

### 与内核的三处口径修正

1. **`ReadOnly` 预设不放行子代理**（任务卡初稿要求"非 `Inherit` 都放行"）。核对内核源码后确认：
   子代理的工具集来自 agent 定义的 `tools:` frontmatter 与 pi settings 的 `defaultTools`，
   **父会话的 `--tools` 传导不过去**。只读会话若能派发子代理，就能借一个带 `bash` / `edit` 的
   agent 绕开"只读"承诺 —— 这是提权口子。
2. **并发配置只读不写**（初稿要求 Manager"投影为" `.pi/subagents-lite.json`）。内核没有任何环境变量或
   命令行入口，配置只来自用户自己的两份 pi 配置文件；代写它们越权且与红线 5 冲突。
3. **子代理会话不落盘**，`SessionManager.inMemory` 是内核源码里唯一的 `SessionManager` 构造。
   "可恢复"本轮拿不到；"可回看"改由父会话文件的 `subagent-result` `custom_message` 条目承载。

### 实现中自查发现并修掉的两处

- **`Block` 膨胀**：`SubagentCard` 内联让 `Block` 从 192 字节涨到 408（clippy `large_enum_variant`）。
  按建议装箱，未用 `#[allow]` 糊过去。
- **每帧克隆全部子代理结果**：`collect_tasks` 会克隆每个任务的完整结果文本（单条上限 512KB），
  而 `ChatPanel::render` 每帧都要问一次。改为按**文档 Arc 身份**记忆化，并连文档一起持有以免
  "旧文档释放后新文档复用同一地址"的误命中；文档消失时缓存一并释放（有测试钉住）。

### 代码审查

**第 1 轮：codex 插件（`/codex:review --base main --scope branch`，插件 1.0.6）** —— 4 条 findings，逐条对着内核源码核实后**全部成立、全部整改**：

| # | 级别 | finding | 核实 | 整改 |
|---|---|---|---|---|
| 1 | P1 | 只读 `concurrency.default` 会少算并发槽 | **成立**。内核 `agent-manager.ts` 的 `getSlot(modelKey)` 优先级是 per-model 槽 > per-provider 共享槽 > **给该 modelKey 新建一个 `default` 槽**；槽互斥但个数随用到的模型种类增长，且 `spawn()` 里 `if (options.modelKey)` 之外没有别的限流——精确上界**不可导出** | `subagent_config` 改为聚合 `default × 未配置模型池假设(4) + Σproviders + Σmodels`，封顶 16 槽；并把 `subagent_slot_bytes` 从 1GiB 下调到 256MiB，否则 16 槽 × 1GiB 会把整树上限推到 20GiB，等于关掉 R25 兜底 |
| 2 | P2 | 子代理结果绕过文档内存预算 | **成立**。`budget.rs` 只统计并释放 `Block::Image` / `Block::Tool`，而 `Block::Subagent` 的正文单条上限 512KiB，且是本轮唯一随后台任务数线性增长的字段 | `block_payload_bytes` / `release_cost` / `release_payload` 纳入 `Block::Subagent`；只放正文、保留卡片结构与 transcript 路径，与工具卡片同一口径 |
| 3 | P2 | 不认识内核的 `turn_limited` | **成立**。内核 `types.ts:95` 的权威全集是 `queued\|running\|completed\|turn_limited\|aborted\|stopped\|error`，而我照着字段名 `max_turns` 猜的 `turn_limit` / `turnLimit` / `turn-limit` **一个都没命中**，真实收尾状态全掉进 `Unknown`，面板会永远算成"运行中" | 解析改为逐字对齐那 7 个取值，删掉全部臆造拼写，并加测试把七个取值逐一钉住 |
| 4 | P2 | 展开的任务列表无高度上限 | **成立**。`panel.children(rows)` 无 `max_h`、不可滚动，长会话的历史任务会把 composer 顶出窗口 | 展开区加 `max_h(168px)` + `overflow_y_scroll` |

第 1、3 条尤其值得记：两条都是**我自己声称要防的失败模式**——第 1 条正是「一次正常的并行委派把父会话一起撞死」，
第 3 条正是「宁可显示陌生词也不把上游状态悄悄归错」。写了注释、也写了测试，但测试用的是我猜的拼写，
所以自测全绿而实际全错。**凡是照抄上游取值集的地方，必须回源码逐字核对，不能从字段名反推。**

**第 2 轮：Claude 子代理审查**（项目所有者 2026-08-27 指定，本任务内改变 CLAUDE.md § 代码审查工具路由的默认选择）。
三个只读子代理并行，视角互不重叠：上游契约一致性 / 资源边界与安全 / 红线与约定符合度。
隔离已验证：审查结束后 `git log`、`git stash list`、`git reflog` 均无审查器留下的任何改动。
整改仍由本轮 writer（主会话）承担 —— 审查工具切换不改变 writer 归属。

共 15 条 findings，**12 条成立并整改，3 条经核对判为可接受并写明理由**：

| # | 级别 | finding | 处理 |
|---|---|---|---|
| 1 | P1 | 实时路径的 `subagent-result` 原始副本既不计入预算也释放不掉 | **成立**。内核发的 `content` 是**字符串**，而 `releasable_raw_bytes` / `release_raw_message` 第一行都在 `as_array()` 上早退 → `completed` 里那份原文随后台任务数无界增长。这正是 R25 给工具结果补三件套要堵的洞，子代理走的不是 `toolCall` item，一条都没覆盖。新增 `subagent_result_text()`，两条路径都按 `customType` 认它，释放口径与工具输出一致 |
| 2 | P2 | 预算释放 `tool.details = None` 抹掉 `agentId`，任务在面板里裂成两条且状态错 | **成立**。后台派发的 `agentId` 只存在于 details。改为按名字保留 `agentId`/`status`/`type` 三个定长身份字段；并让缺 `details` 的后台任务**保持未结算**，不再凭工具 `Success` 判成已完成 |
| 3 | P2 | 记忆化在运行期恒不命中，等于每帧深拷贝全部子代理产出 | **成立**。`snapshot()` 每次 `Arc::new(...)` 都是新 Arc，`ptr_eq` 缓存恰好在自己声称要优化的场景里永远失效，只剩「钉住过期文档不让释放」的副作用。**根治办法是改数据形状**：`SubagentTask` 去掉 UI 零引用的 `prompt` / `result` 两个大字段，现算即变廉价，缓存连同其副作用一起删掉 |
| 4 | P2 | 并发配置分层语义与内核不符（两个子代理独立发现） | **成立**，见下方「同一类错误的第 3、4 例」 |
| 5 | P2 | `aborted` 被当成「父会话中断」 | **成立**，见下方 |
| 6 | P2 | 立项文档 § 二 描述的是被放弃的 `npm install` 方案 | **成立**。§ 二 那段写于切换到双 tarball 方案**之前**，切换后只在任务卡「踩的坑」里记了，没回头改授权基线，导致同一份任务卡自相矛盾。已改写 |
| 7 | P2 | 无缓存兜底路径超 MAX_PATH，且「三条路径均实测」不实 | **成立**。兜底路径 267 > 259 限制，在本 worktree 必然抛错；而 CI 恰好设 `GPUI_PI_CACHE: OFF` 走的就是这条，只因 CI 仓库前缀短才没暴露。缩短临时目录名与解包标签（267 → 209），并**实际执行**了 `GPUI_PI_CACHE=OFF` 全流程 |
| 8 | P3 | 项目层配置缺 trust 门禁 | **成立**。内核只在 `isProjectTrusted()` 为真时读项目层；无条件读等于让仓库内容影响宿主内存上限。改为经 `pi_data::read_project_trust_status` 门禁，读不出状态时按不可信处理 |
| 9 | P3 | `forceBackground` 开启时前台调用也走后台，只看工具参数会误判 | **成立**。改用 `details.agentId` 判定 —— 内核只在后台分支塞它，是充分必要标记 |
| 10 | P3 | `details.outputFile` 默认不存在，三处 fixture 全覆盖开启态 | **成立**。默认 `outputTranscript = false`，且 `outputFile` 只在 nudge 路径出现。归并逻辑经核对本就靠短 id 兜住，但补了默认路径的测试 |
| 11 | P3 | `StopAgent` 双向前缀匹配会一次改写多条 | **成立**。改为单向（完整 id 以模型给的串开头）且**仅唯一命中时**才改写 |
| 12 | P3 | 默认配置顶满 `MAX_RESERVED_SLOTS`，`configured_total` 恒被吞 | **成立**。`ASSUMED_UNCONFIGURED_MODEL_POOLS` 由 4 下调到 2，让默认值之上仍有可观察空间 |
| 13 | P3 | 实时路径无 `display` 过滤，回放路径有 | **不改，记录理由**。子代理结果内核硬编码 `display: true`，不受影响；该不对称在 R26 之前就存在（原先落 `Unknown` 也照样渲染），属前序轮次问题，按红线 3 记入 backlog 而非本轮顺手改 |
| 14 | P3 | `SubagentCard` 里 `result` 之外的字符串不计入预算 | **不改，记录理由**。它们都来自 `details` 的短字段（类型名、状态词、路径），唯一不可控的 `description` 来自模型写的工具参数，量级是一行摘要。计入却不释放反而会让预算永远降不下来（`budget.rs` 注释里写明的坑） |
| 15 | P3 | 后台标签的任务缓存会钉住过期文档 | **随 #3 一并消失**（缓存已整体删除） |

### 同一类错误，一轮之内出现了 4 次

| # | 我的错假设 | 内核实际 | 出处 |
|---|---|---|---|
| 1 | `turn_limit` / `turnLimit` / `turn-limit` | `turn_limited` | 从字段名 `max_turns` 反推 |
| 2 | `aborted` = 父会话中断 | `aborted` = **轮次上限硬杀**，与 `turn_limited` 同族；`stopped` 才是被谁停掉 | 从英文词义反推 |
| 3 | 并发配置整层替换 | **按键合并**：`default` 覆盖、`providers`/`models` 取并集 | 没读 `mergeRawConcurrency` |
| 4 | `details.outputFile` 总是存在 | 只在 nudge 路径 + `outputTranscript`（默认 false）时才有 | 拿开启态当默认态 |

第 2 例最值得记：我在第 1 轮**刚因为 `turn_limited` 被抓**，整改时却把它的孪生状态又归错了桶 ——
而 `aborted` 与 `turn_limited` 的权威注释就并列写在内核 `status-note.ts` 的相邻两行。
第 4 例则是「自测与实现共用同一个非默认假设」的第三次重演：三处 fixture 全塞了 `outputFile`，
默认配置下的真实路径零覆盖。

**结论性教训（已写进本轮，后续轮次沿用）**：凡是照抄上游取值集、字段存在性、或合并语义的地方，
必须**回源码逐字核对并在注释里引出处**，不得从字段名、英文词义或"看起来合理"反推 ——
自己造的测试挡不住自己造的错误假设，只会把它钉得更牢。

**第 3 轮：Claude 子代理审查（交付物 F 增量，2026-08-27）** —— 单个只读审查器，首轮结论 **BLOCK**，
3 条 findings 全部成立并整改（复核结论见表后）：

| # | 级别 | finding | 处理 |
|---|---|---|---|
| 1 | P1 | denylist 注释「`subagent` 是 npm:pi-subagents 唯一注册的工具」不实 | **成立，且正是上表教训的又一例**：我只 grep 到 `index.ts:672` 就写下「唯一」，审查器对着**同一份源码**往下读 46 行即推翻 —— `:718` 无条件 `registerWaitTool`（`subagent_wait`），`session_start` 里 `supervisorChannel.start()` 在父会话注册 `subagent_supervisor`。已亲验后把两名补入名单（`contact_supervisor` 有子进程门禁、刻意不入列并注明），fixture 补齐三工具样板，所有断言与六份文档同步 |
| 2 | P3 | BACKLOG #41 同格里「均不采用，留给独立产品决策轮」未划线，与文末收口结论矛盾 | **成立**。补划线并指向文末改判 |
| 3 | P3 | 截图用例「第三方子代理扩展的工具对模型已不可见」是全称 overclaim | **成立**（与 #1 同根）。名单补齐后该句成立，措辞改为点名扩展与三个工具名 |

审查器同时逐条核对了五处上游行号断言（四处半准确）、生产 spawn 路径覆盖（含 warm pool
`switch_session` 复用后排除集仍生效、历史导出带 `--no-extensions` 不构成漏洞）、
单测无放宽（红线 4）、镜像一致性 —— 均无问题。

**复核（同一审查器，第二轮）：最终结论 PASS**。三条 findings 的整改逐条与源码证据吻合、
无新的未验证断言、断言强度全部变强或持平、镜像逐字节一致。另提 4 条不阻断残留（N1 validation
节数字过期、N2 测试头注释仍写单数、N3 门禁行号 300 应为 301、N4 验收表 T2 行未同步三名单），
已全部处理——N3 的行号我重新数过源码确认为 301。审查器声明未复跑测试（只读约束），
测试通过口径以本节 validation 记录为准。

### validation

`.\scripts\validate.ps1` 全量（第 3 轮审查整改后重跑，含交付物 F 新增用例），**exit 0**，`VALIDATE OK`，
共 **572 passed / 0 failed**，clippy `-D warnings` 零警告；real-pi 集成测试
`third_party_subagent_tool_is_excluded_while_other_extensions_survive` 另以真实 pi 0.84.2 实跑通过
（`--ignored` 单跑，1 passed）。下面保留两轮审查整改后那次（567 passed）的原始回显：

```
### 范围：全工作区（含 gpui / gpui-component 编译）
### [1/5] 上游钉版本
OK   zed (gpui / gpui_platform)
OK   gpui-component
OK   zed 无杂散 sha
OK   gpui-component 无杂散 sha
OK   pi source content matches baseline manifest (1373 files)
OK   pi-web source content matches baseline manifest (380 files)
OK   pi-subagents-lite directory has no .git
OK   node_modules holds only @sinclair/typebox
OK   pi-subagents-lite marker (version/package_sha512/typebox_version/typebox_sha512/source)
OK   pi-subagents-lite content matches baseline manifest (1133 files)
### [2/5] cargo fmt
### [3/5] cargo clippy
### [4/5] cargo test
### [5/5] cargo build --release
    Finished `release` profile [optimized] target(s) in 3m 31s
validate exit=0
```

本轮新增测试覆盖：`pi-render` 子代理模型（含四条专为「上游假设」设的回归：内核状态全集、默认无 transcript 路径、details 被预算裁剪后的后台任务、StopAgent 前缀歧义）、会话回放、
实时路径与回放路径一致性、负载预算纳入子代理正文；`pi-runtime` 注入与预设、并发配置按键合并与 trust 门禁、内存余量；`gpui-pi` 面板显隐与任务足迹；`gpui-pi-ui` 汇总计数。

