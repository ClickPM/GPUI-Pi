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
- Manager 侧把并发上限投影为 `.pi/subagents-lite.json` 的 `concurrency`
- 有活跃子代理的 Runtime 单独内存口径

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

<!-- 完成后回填：实际数字、踩的坑、与设计的偏离及原因 -->
