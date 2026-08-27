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
- `scripts/fetch-pi-subagents-lite.ps1` —— 按 registry `dist.integrity` 校验 tarball、解包、`npm install --omit=dev --legacy-peer-deps --ignore-scripts` 只补 `@sinclair/typebox`，走 `GPUI_PI_CACHE` 缓存
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
- `crates/pi-render/src/subagent.rs` —— `SubagentTask` 与状态归并；输入只认三个事实来源：`tool_execution_*` 事件、`subagent-result` `custom_message` 条目、`details.outputFile`
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

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | vendor 与钉版本 | `.\scripts\fetch-pi-subagents-lite.ps1` 后 `.\scripts\check-pins.ps1` 全绿；`vendor/pi-subagents-lite-1.13.0/node_modules` 只含 `@sinclair/typebox` |
| T1 | 逻辑层单测 | `.\scripts\validate.ps1 -Logic` 全绿，含 `pi-render` 子代理任务模型新增用例 |
| T1 | 全量 | `.\scripts\validate.ps1` 全绿（clippy `-D warnings`） |
| T2 | 扩展确实加载 | `pi --mode rpc -e <vendor 路径>` 的 `get_commands` 含 `agents` 命令，stderr 无错误，且 `~/.pi` 无新增写入 |
| T2 | 预设与子代理的关系 | `Inherit` 注入内核且不下发 `--tools`；`Default` / `Full` 注入且允许列表含 `Agent` / `StopAgent` / `AgentStatus`；`ReadOnly` / `None` 既不注入也不放行（防提权） |
| T2 | 回看 | 父会话文件中的 `subagent-result` 条目能被 `pi-render` 还原成子代理任务并在面板展示 |
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

- 视觉审查模式：<待定 —— 本轮触及 `crates/ui` / `crates/app`，必须触发>
- 视觉审查结论：<待填>
- 截图验证：<待填>
- 兜底原因：<待填>
- `requested_at`：<待填>
- `deadline`：<待填>
- 审查报告 / 证据：<待填>
- 说明：<待填>

## 本轮实测

### 依赖钉死

| 项 | 实测 |
|---|---|
| `pi-subagents-lite` tarball sha512 | `sha512-g6kyfhC762ju1Esr7/6O3WQKrSXPtEsACC+O9UK1jLQ2d5gFTZU1F39mB8AkpuyOMHgVemZVqr4rXbmycr1fIw==`（与 npm registry `dist.integrity` 一致） |
| `@sinclair/typebox` tarball sha512 | `sha512-XiMQh7qqVlxZzcVD+kkGMNGMzcTrDMLWI7S4x7z1MkCkbDPrekpZXEUK0eZqZFMuHQg2a2DZOcDIh9o5v3Gonw==` |
| manifest 文件数 | 1133（59 个包内文件 + 1075 个 typebox 文件 − 1 个 marker） |
| vendor 树最长绝对路径 | 203 字符（仓库前缀 106 + `node_modules/@sinclair/typebox/build/esm/type/constructor-parameters/…` 97） |
| fetch 三条路径 | 冷启动（联网）/ 缓存命中 / vendor 快路径均实测 `exit=0` |

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

### validation

`.\scripts\validate.ps1` 全量，**exit 0**，32 个测试目标共 **551 passed / 0 failed**，clippy `-D warnings` 零警告：

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

本轮新增测试（37 条）分布：`pi-render` 子代理模型 15 + 会话回放 2 + 实时路径 2、
`pi-runtime` 注入与预设 6 + 并发配置 5 + 内存余量 4、`gpui-pi` 面板 2、`gpui-pi-ui` 汇总 3。

