# Round 21 — 权威设计同步 + 单会话 Runtime 集中化 + 会话态收敛

> 执行方：Windows · 状态：已完成（BLOCKED 已解除，见 `BLOCKED.md` 解除记录）

## 目标

在不开放多会话 UI、不改变现有单会话外部行为的前提下，引入无 GPUI 的应用级 `pi-runtime`，使所有 app 内建 RPC Runtime 经共享 `RuntimeManager` 创建，并把 `ChatPanel` 的单会话状态收敛为 `SessionHandle` + 单一 `SessionUiState`。

## 前置

- R2、R3、R7、R13、R14、R16、R20 已完成并合入 `main`。
- 项目所有者于 2026-08-23 选择立项文档 § 七阶段 E 的**口径 B**：先实施 R21–R27，M4 顺延到 M5 之后；附录 A 验收基线在 R24 后重建。
- Windows worktree：`D:\variFlight_work\GPUI-Pi-R21`，分支：`WinClaude/round-21`。
- 新 worktree 已独立执行 `fetch-pi.ps1`、`fetch-pi-source.ps1`、`fetch-pi-web.ps1`、`check-pins.ps1`，完整 vendor 门禁全绿。
- 改动前 `./scripts/validate.ps1 -Logic` 已全绿。

## 交付物

- `docs/立项文档.md`、`AGENTS.md`、`CLAUDE.md`、`ROUNDS.md`：记录口径 B 与 R21 状态。
- `Cargo.toml`、`crates/pi-runtime/**`：新增纯逻辑 crate，提供应用级 `RuntimeManager`、稳定 `RuntimeId`、`SessionHandle`、Snapshot revision / Dirty 通知与 maintenance 配额入口。
- `crates/pi-rpc/**`：仅允许补充上层集中化所需的窄接口与测试；Manager 创建的 Runtime 必须关闭底层自动重启。
- `crates/app/**`：共享注入一个 `RuntimeManager`；生产会话与历史 HTML 导出不再直接创建 `pi_rpc::Client`；`ChatPanel.active` 替换为 `SessionHandle`；会话态收敛为单一 `SessionUiState`。
- `scripts/validate.ps1`：逻辑验证范围纳入 `pi-runtime`。
- R21 对应单元测试与真实 pi 零 token 集成测试。

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | 全量格式、clippy、测试、release 构建与钉版本 | `.\scripts\validate.ps1` → `VALIDATE OK` |
| T1 | 逻辑 crate 快速回归包含 `pi-runtime` | `.\scripts\validate.ps1 -Logic` → 输出范围含 `pi-runtime` 且 `VALIDATE OK` |
| T2 | Runtime 核心契约 | `cargo test -p pi-runtime`：稳定 `RuntimeId`、epoch/revision 单调、Dirty/Snapshot、single-session compatibility、maintenance 并发上限、Manager 重启所有权全部通过 |
| T2 | 唯一生产创建入口 | 搜索 `crates/app` 生产代码，除明确的非 RPC CLI 外不得直接调用 `Client::spawn`；用户会话与 HTML 导出均经 `RuntimeManager` |
| T2 | 重启归属 | Manager 创建配置强制 `max_restarts = 0`；测试证明底层崩溃后不会绕过 Manager 自行拉起进程 |
| T2 | 真实 pi 零 token | 用 `vendor/pi/pi.exe` 跑 `pi-runtime` 忽略型集成测试，至少覆盖启动、`get_state`、优雅 shutdown / resume 接缝，不消耗模型 token |
| T2 | 行为回归 | 现有 restart/rebind、Extension UI、compaction/retry、HTML 导出相关测试继续通过；不开放多会话 UI |
| T3 | 可见行为 | 本轮不设计视觉变化；若最终 diff 触发项目 UI 视觉审查条件，按 `SCREENSHOT` / `CODE_ONLY` 门禁执行 |

## 禁止

- 不实现 R22 的有界 Actor、command queue、事件背压或 reducer 定时合并。
- 不实现 R23 的 Scheduler、Park/Resume、IdleWarm、TTL、公平队列。
- 不开放 R24 多会话 UI；本轮始终只有一个 `SessionUiState` 实例。
- 不实现 R25 Job Object / 内存治理，不实现 R26/R27 子代理与 writer worktree。
- 不修改 `PINNED_PI_VERSION` 或 `vendor/upstream/**` 身份，不引入新第三方依赖；`Cargo.lock` 仅允许立项文档 § 二授权的本地 path crate 例外：增加 `pi-runtime` package 记录及 `gpui-pi` 对它的依赖边，禁止任何上游 version / source / checksum 漂移。
- 不改变用户主目录 `~/.pi` 数据；测试只读真实目录或使用临时目录。
- 不借本轮顺手修复前序轮次问题；发现后只记 `rounds/BACKLOG.md`。

## 失败处理

同一验收项经针对性整改后连续 2 次 validation 仍不过 → 写 `rounds/round-21/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 视觉审查

- 视觉审查模式：CODE_ONLY
- 视觉审查结论：CODE_ONLY_PASS
- 截图验证：未提供（SCREENSHOT_NOT_PROVIDED）
- 兜底原因：USER_DECLINED
- `requested_at`：2026-08-23T12:06:15+08:00
- `deadline`：2026-08-23T12:36:15+08:00
- 审查报告 / 证据：`rounds/round-21/ai-output/r21-review/visual-code-only.md`；辅助图片 manifest：`.pi/visual-review/round-21/evidence/manifest-088acca8cc21ed9b.json`。
- 说明：仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR。用户在截止前回传的 3 张图片均为 2880×1716 浅色主题，且第 2 张有通知遮挡，不符合 1280×820 / 100% / 深色的截图请求元数据；按用户明确拒绝补图处理，仅作为非阻断辅助旁证，不回写为已完成截图验证。

## 本轮实测

- 新 worktree vendor 门禁：`fetch-pi.ps1`、`fetch-pi-source.ps1`、`fetch-pi-web.ps1`、`check-pins.ps1` 全绿；最终再次运行 `check-pins.ps1`，两份上游 manifest 与钉 commit 均通过。
- 新增 `pi-runtime` 纯逻辑 crate：review 修复后 `cargo test -p pi-runtime` 为 **24 passed / 0 failed / 1 ignored**，附加 fake-child binary availability integration test **1 passed**；ignored typed-controls 项另以显式 `GPUI_PI_TEST_FAKE_CHILD` 运行并 **1 passed**。
- app 回归：`cargo test -p gpui-pi` 为 **121 passed / 0 failed**；删除的是迁移后失真的 panel activity-generation 纯函数测试，真实 dispatch wiring 已由 `pi-runtime` fake-child 测试覆盖 Rejected 与 Ambiguous 两条恢复路径。
- 聚焦 clippy：`cargo clippy -p pi-runtime -p gpui-pi --all-targets -- -D warnings` 通过。
- PowerShell 语法：`validate.ps1` 经 `[System.Management.Automation.Language.Parser]::ParseFile` 检查，输出 `PARSER_OK`。
- `scripts/validate.ps1 -Logic` 已纳入 `pi-runtime` 并输出 `VALIDATE OK`；最终全量 `scripts/validate.ps1` 输出 `VALIDATE OK`，release 构建通过。
- app 生产源码搜索 `rg -n 'Client::spawn' crates/app/src --glob '*.rs'` 无结果；生产创建入口集中在 `crates/pi-runtime`。provider 登录等非 RPC CLI 保持原路径。
- `Cargo.lock` 按立项文档 § 二的 R21 精确例外增加 **12 行**：`gpui-pi` 对 `pi-runtime` 的依赖边和只列实际既有依赖的 `pi-runtime 0.1.0` 本地 package；移除未使用的 `futures` / `thiserror`，测试 fake child 复用现有 `serde_json`；无任何上游 source/checksum/version 变化，未运行 `cargo update`。
- `ChatPanel.active` 已替换为 `Option<SessionHandle>`；会话态真实存储于单一 `SessionUiState`，`ChatPanel` 仅保留 Runtime 绑定、GPUI Entity/focus/subscription、面板几何与文件 popup/index 等面板态。
- `RuntimeManager` 在应用启动时创建一次，并注入 `Workspace`、`ChatPanel`、`SessionSidebar`；历史 HTML 导出与用户 Runtime 共享该 Manager，maintenance 配额默认 1。
- review 修复：dispatch 在 Runtime 锁内捕获 `activity_generation`；一次性 effect 在 R21 保持无界可靠，不再截断 256 条；崩溃归并为幂等 `Error + Stopped("会话已崩溃，请重新启动…")`；tool restart spawn 失败 fence 旧 pump；新 Client spawn 成功后才替换 active user，失败保留旧 handle。
- 第二轮 review PASS 后加固：`fail_runtime` 在锁内完成 epoch/stopped/replacing fence、Error/Stopped 发布并取出 Client，释放 `RuntimeState` 锁后才显式 shutdown，避免 supervisor/stdout join 时阻塞 UI Snapshot；聚焦崩溃测试、`pi-runtime` 全测/clippy 与 Logic validation 均通过。
- 残余风险：R21 为保证一次性结果可靠，effect 历史仍无界；R22 必须实现可靠终态权威字段、流式状态 latest-only 与固定字节上限后才能重新有界。旧会话只在新 Client 成功 spawn 后才 shutdown，替换窗口内两个进程可短暂并存，但 UI 与 `active_user` 始终只绑定一个，未开放持久多会话。
- `runtime_fake_child` 是 `pi-runtime` 测试 helper target，正常 release build 会产出该辅助 exe；R17 打包必须只收录 `gpui-pi.exe` 与 `vendor/` 所需运行时，不得把该 helper 纳入安装包。
- 未开放多会话 UI；R22 Actor/背压、R23 Scheduler/Park、R25 Job Object、R26/R27 子代理均未提前实现。
- **BLOCKED（已解除，2026-08-24）**：任务卡 T2 的真实 pi 零 token 集成测试曾连续两次失败并按红线停止（详见 `BLOCKED.md` 原始记录）。项目所有者指示接手后，依据钉死 pi 0.84.2 源码查明根因：`session-manager.ts` `_persist()` 的 `hasAssistant` 闸门决定 fresh 会话在首条 assistant 消息前**不落盘**（上游权威契约，原方向 1 的 `bash` entry 也会被同一闸门拦住）。整改仅改测试层：fresh 段把懒持久化当契约断言（预分配路径隔离且不存在，优雅 stop 后仍不存在）；stop/resume 接缝改走钉死 pi 的 `--session` 显式 0 字节文件分支（session-manager.ts:902-911，零 token 即写入 header 且 `flushed=true`），校验磁盘 header 与 `get_state` 一致 → 优雅 stop → 经 `pi_render::render_path` 从**同一文件** resume → `session_id` 保持。验收标准原文未变。实测：`PI_RUNTIME_TEST_BINARY=vendor\pi\pi.exe cargo test -p pi-runtime --test real_pi -- --ignored` → **1 passed / 0 failed**；`.\scripts\validate.ps1 -Logic` → `VALIDATE OK`。
- 独立代码审查：`claude-code-review` 因当前 Wi-Fi 非 `Variflight` 被工具门禁拒绝，按仓库路由降级为 `deepseek/deepseek-v4-pro` reviewer；首轮发现 1 HIGH、2 MEDIUM、3 LOW，全部修复并补真实 fake-child 测试，第二轮结论 **PASS**。随后关闭了 reviewer 的 NOTE 级持锁 shutdown 风险。
- BLOCKED 解除后的增量审查（Claude Code harness 自身流程，10 角度多代理 + 钉死源码逐条核验，范围限 `crates/pi-runtime/tests/real_pi.rs`）：确认测试的 4 项上游契约假设全部成立；修复 10 项（`canonical()` 失败即 panic 消除 `\\?\`/8.3 短名混比误报、负向断言改 `try_exists`、恢复 `request_control → ControlFinished` 真实 pi 端到端覆盖、`StopOnDrop` 兜底回收防 panic 泄漏 pi 进程、`PI_CODING_AGENT_SESSION_DIR` 前提断言、seam 文件移入 agent sessions 目录、删除恒真断言并补 seeded `snapshot.runtime_id` 校验、TIMEOUT 60s 冷启动余量、模型凭据前提与 resume 非只读行为文档化）；4 项跨轮次/生产层发现按红线 3 记 `rounds/BACKLOG.md` #11–#14。修复后 `cargo test -p pi-runtime --test real_pi -- --ignored` 仍 **1 passed**，聚焦 clippy 通过。
- 视觉审查：最终 diff 触及 app 视图接线而触发门禁；用户明确拒绝按元数据要求补图，项目专用 `visual-reviewer` 以 `CODE_ONLY` 模式审查，结论 **CODE_ONLY_PASS**。仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR。
