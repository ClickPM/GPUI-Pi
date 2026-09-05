# Round 27 — mutating 子代理与 worktree writer 隔离

> 执行方：Windows · 状态：进行中

## 目标

mutating 子代理必须在独立 git worktree 内写入；同一 worktree 同一时刻最多一个 writer；完成后 diff/结果由父会话串行审查集成；取消、异常恢复与清理路径在 `remove_worktree` 前全覆盖 reparse-point 安全检查。

## 前置

- R21–R26 已完成（`RuntimeManager`、有界 Actor、Scheduler、多会话 UI、Job Object、内建 `pi-subagents-lite` 内核）。
- 新 round 启动门禁已过：本 worktree 内 `fetch-pi` / `fetch-pi-source` / `fetch-pi-web` / `fetch-pi-subagents-lite` / `check-pins` 全绿。
- R26 已确认：扩展的 `Agent` 工具带可选 `worktree_path` 与 trust 校验；本轮在其上加 **Manager / host 侧强制**，不改 vendor 钉死内核。

## 交付物

### A. 纯逻辑隔离（无 GPUI）
- `crates/pi-runtime/src/writer_isolation.rs` —— agent 读写分类、writer 租约表、串行集成队列、策略判定
- `crates/pi-data/src/git.rs` —— `add_writer_worktree`（独立分支命名约定）+ 既有 `remove_worktree` 的 reparse 门禁保持为清理唯一入口

### B. Host 强制（进程内 `tool_call`）
- `crates/pi-rpc/assets/writer-isolation.ts` —— 对 `Agent` 的 `tool_call`：mutating 缺 `worktree_path`（或指向父 checkout）时自动分配独立 worktree 并改写 `event.input`；同一 path 已有 writer 则 `{ block: true }`；`tool_result` 释放租约
- `crates/pi-rpc/src/host_extension.rs` —— 落盘并注入第二个 `-e`（仅在加载子代理内核的预设下）

### C. Manager API
- `RuntimeManager` / 会话侧：准备 writer worktree、登记/释放租约、入队/出队串行集成、清理（必经 `remove_worktree`）

### D. UI（轻量）
- 子代理面板 / 卡片：展示 writer worktree 路径、租约冲突/策略说明、待集成队列（一次一项）

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | 钉版本 | `.\scripts\check-pins.ps1` 全绿 |
| T1 | 逻辑单测 | `.\scripts\validate.ps1 -Logic` 全绿，含 writer 租约 / 分类 / 集成队列 / worktree 分配 |
| T1 | 全量 | `.\scripts\validate.ps1` 全绿（clippy `-D warnings`） |
| T2 | 强制注入 | mutating `Agent` 无 `worktree_path` 时 host 改写入参为独立 worktree；指向父 checkout 同样被替换 |
| T2 | 单 writer | 同一 worktree 第二个 mutating `Agent` 被 `tool_call` block |
| T2 | 只读放行 | `Explore`（及明确只读工具集）可不带 worktree |
| T2 | 清理安全 | 清理路径只走 `remove_worktree`（含 reparse 扫描）；含目录链接时拒绝移除 |
| T3 | 串行集成 | 两个 mutating 完成后父会话集成队列一次只暴露一项；取消/失败释放租约且不拖垮父会话 |

## 禁止

- 禁止修改 `vendor/pi-subagents-lite-1.13.0/` 钉死内容。
- 禁止代写用户 `~/.pi` / 项目 `.pi/subagents-lite.json`（红线 5）。
- 禁止把配置式并发上限表述为派发式调度。
- 禁止跳过 reparse 检查直接 `git worktree remove` / 删目录。
- 禁止改 `Cargo.lock` 上游 package 的 version / source / checksum。
- 发现前序问题写 `rounds/BACKLOG.md`，不当场改（红线 3）。

## 失败处理

连续 2 次 validation 不过 → 写 `rounds/round-27/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 视觉审查

本轮若 diff 触及 `crates/ui/**` 或 `crates/app/**` 视图代码则触发。

- 视觉审查模式：CODE_ONLY
- 视觉审查结论：CODE_ONLY_PASS
- 截图验证：未提供（SCREENSHOT_NOT_PROVIDED）
- 兜底原因：TIMEOUT_30M
- `requested_at`：2026-09-04T22:46:16+00:00
- `deadline`：2026-09-04T23:21:16+00:00
- 审查报告 / 证据：主会话按 `.agents/visual-reviewer.md` 做 CODE_ONLY；`subagent_panel` 串行集成提示与 worktree tooltip 均走 `cx.theme()` token / `text_xs` / `max_h_40`+scrollbar，未见硬编码色或布局阻断项
- 说明：仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR

## 本轮实测

### 环境

- Cloud Agent（Linux）：vendor 门禁经 `PROCESSOR_ARCHITECTURE=AMD64` + `SystemRoot` tar shim 通过 `check-pins`；`pi.exe` 无法本机自检版本（Exec format error），源码/内核/web 钉死校验全绿。
- 完整 `validate.ps1` / real-pi T2–T3 依赖 Windows CI。

### 逻辑单测（Linux）

| 套件 | 结果 |
|---|---|
| `pi-runtime` `writer_isolation` | 10 passed（含 subdirectory toplevel 独立判定） |
| `pi-runtime` Manager writer API | 1 passed |
| `pi-rpc` `host_extension` | 5 passed（含 writer-isolation 源码契约） |
| `pi-runtime` `active_session_config*` | 相关用例全绿（writer `-e` 排在内核前） |
| `gpui-pi-ui` `serial_integration_hint` | 1 passed |

### PR

https://github.com/ClickPM/GPUI-Pi/pull/1

### 审查整改（本轮续做）

- host `writer-isolation.ts`：后台 Agent 租约挂到 `agentId`，至 `subagent-result` / `StopAgent` / `session_shutdown` 才释放。
- `is_independent_worktree`：改用 `git_toplevel`，修复「cwd 在子目录、path 为仓库根」漏检。
- `prepare_writer_worktree`：acquire 失败立刻 `remove_worktree`；`recover_writer_agent` 先清理再释租约。
- Manager：`track_writer_worktree` / `complete_writer_integration_and_cleanup` / `sync_writer_integrations_from_tasks`；ChatPanel 在 snapshot / 分支预览路径同步集成队列。

