# Round 21 — BLOCKED（已解除）

> **状态：RESOLVED（2026-08-24）**。项目所有者指示接手继续未完成工作后，按下方「解除记录」完成整改，T2 真实 pi 零 token 验收已通过；验收标准未做任何放宽。

## 阻塞项（原始记录）

任务卡 T2「用钉死真实 `vendor/pi/pi.exe` 验证 RuntimeManager fresh 启动、零 token controls、优雅 stop、从同一 session file resume」在针对性整改后连续两次失败，按项目红线停止。

## 实测（原始记录）

环境：Windows，`vendor/pi/pi.exe --version` = `0.84.2`；测试使用 `tempfile` 隔离 `PI_CODING_AGENT_DIR` 与 cwd，未调用 prompt/模型请求。

1. 第一次：fresh Runtime 等待 `CommandsLoaded + ControlsLoaded` 成功，`get_state` 返回非空 `session_id` 与 `session_file`，但该 `session_file` 尚不存在。失败：
   - `missing <temp>/agent/sessions/...jsonl`
2. 针对性整改：通过 RuntimeManager 发送 pi-rpc 既有零 token矩阵已覆盖的 `SetThinkingLevel(current_level)`，等待对应 `ControlFinished(Controls)` 成功，再检查路径。
   - 结果：官方 pi 仍未创建 JSONL。
   - 失败：`zero-token metadata mutation did not create <temp>/agent/sessions/...jsonl`

两次均说明钉死 pi 对 fresh metadata 查询/同值设置只预分配 session path，不落盘；因此当前无法在禁止 prompt 的条件下执行“从同一真实 session file resume”。

## 需要决策（原始记录）

可选方向必须由 supervisor 批准后继续：

1. 允许使用确定不烧模型 token但会写会话 entry 的命令（候选：`bash` 且 `exclude_from_context=true`）来促使 JSONL 落盘，再验证 stop/resume；或
2. 提供钉死 pi 可无 token持久化 fresh session 的权威命令/fixture 方案；或
3. 明确调整验收，不要求 fresh 流程先落盘（当前不建议，属于标准变化）。

## 解除记录（2026-08-24）

- 决策来源：项目所有者于 2026-08-24 指示接手 R21 未完成工作。经查钉死上游源码后按**方向 2 的权威方案**解除，验收标准（T2「至少覆盖启动、`get_state`、优雅 shutdown / resume 接缝，不消耗模型 token」）原文未变。
- 根因（钉死 pi 0.84.2 源码证据，`vendor/upstream/pi-0.84.2/packages/coding-agent/src/core/session-manager.ts`）：
  - `_persist()` 有 `hasAssistant` 闸门：出现首条 `assistant` 消息前任何 entry 都不写盘，仅置 `flushed=false`；`newSession()` 初始即 `flushed=false`。**fresh 会话零 token 不落盘是上游权威契约，不是缺陷。**
  - 由此判定原方向 1 不可行：`bash` + `exclude_from_context=true` 的 `recordBashResult` 同样经 `sessionManager.appendMessage` → `_persist`，在 fresh 会话上会被同一闸门拦住，第三次尝试也必然失败。
  - 权威零 token 持久化路径：`--session` 指向**已存在的 0 字节文件**时（`_setSessionFile` 空文件分支，session-manager.ts:902-911），pi 立即 `newSession()` + `_rewriteFile()` 写入 session header 并置 `flushed=true`，之后所有 entry 直接追加落盘。该分支经 `SessionManager.open` 由 `pi --mode rpc --session <path>` 直达。
- 整改内容（仅测试层，未改任何生产逻辑）：重写 `crates/pi-runtime/tests/real_pi.rs` 的 `runtime_manager_fresh_stop_and_resume_is_zero_token`：
  1. fresh 段改为把懒持久化当契约断言：`get_state` 预分配路径在隔离 `PI_CODING_AGENT_DIR` 内、磁盘上不存在、优雅 stop 后仍不存在；
  2. 新增 stop/resume 接缝段：显式 0 字节文件 `start_session` → 真实 pi 零 token 写入 header（校验磁盘 header 的 `type`/`id` 与 `get_state` 一致）→ 优雅 stop 后文件保留 → 经生产同款 `pi_render::render_path` 从**同一文件** resume → `session_id` 保持一致 → 优雅 stop；
  3. 全程无 prompt、无模型 token 消耗；临时 cwd 保持为空的清洁性断言保留。
- 实测：`PI_RUNTIME_TEST_BINARY=vendor\pi\pi.exe cargo test -p pi-runtime --test real_pi -- --ignored` → **1 passed / 0 failed**；`.\scripts\validate.ps1 -Logic` → `VALIDATE OK`。
