# R21 实现结果

## 结论

已在 `D:/variFlight_work/GPUI-Pi-R21` 的 `WinClaude/round-21` 分支完成 R21 实现，未 commit、push、创建 PR 或使用其他 worktree。聚焦验证、Logic validation、全量 validation 均通过。

## Changed files

- `Cargo.toml`：workspace/member dependency 增加 `pi-runtime`。
- `Cargo.lock`：按立项文档 § 二批准的精确例外，仅增加本地 `pi-runtime` package（12 行）及 `gpui-pi` 的 path dependency 边（1 行）。
- `crates/pi-runtime/Cargo.toml`
- `crates/pi-runtime/src/lib.rs`：新增 RuntimeManager / SessionHandle / Snapshot / Dirty / RuntimeEffect / maintenance 实现和测试。
- `crates/app/Cargo.toml`：依赖 `pi-runtime`。
- `crates/app/src/live_session.rs`：缩为 `pi-runtime` 类型重导出及 epoch 校验的 Extension UI response adapter。
- `crates/app/src/main.rs`：应用启动时创建唯一共享 `RuntimeManager`。
- `crates/app/src/workspace.rs`：向 ChatPanel/SessionSidebar 注入同一 Manager。
- `crates/app/src/panels.rs`：`Option<SessionHandle>`、Snapshot/Dirty 消费、单一真实 `SessionUiState`、命令与 effect 投影。
- `crates/app/src/session_sidebar.rs`：历史 HTML 导出迁到共享 Manager maintenance API。
- `crates/app/src/main_panel.rs`：测试构造适配 Manager 注入。
- `scripts/validate.ps1`：`-Logic` 纳入 `pi-runtime`。
- `docs/立项文档.md`：口径 B、R21 Cargo.lock 本地 path crate 精确例外。
- `AGENTS.md` / `CLAUDE.md` / `ROUNDS.md`：保留父会话落盘的口径 B 与 R21 状态。
- `rounds/round-21/round-21.md`：任务卡、精确 lock 例外和本轮实测。

## 架构实现摘要

1. **Runtime 集中化**
   - `pi-runtime` 无 GPUI 依赖，内部独占 `pi_rpc::Client`、`LiveSessionReducer`、Client event drain、calibration、metadata/controls、tool restart 与 lifecycle。
   - app 生产代码不再创建 `Client`。`crates/app/src` 搜索 `Client::spawn` 为零。
   - 所有 Manager 创建配置在最终 spawn 前调用 `clamp_manager_config`，强制 `max_restarts = 0`。

2. **应用级共享 Manager**
   - `main.rs` 在 application run 内创建一个 `RuntimeManager`，经 Workspace 同时注入 ChatPanel 与 SessionSidebar。
   - `single_session_compat`：启动第二个用户 Runtime 前，Manager 优雅 shutdown 旧 Runtime。

3. **稳定 Handle / Snapshot / Dirty**
   - `RuntimeId` 是 Manager 分配的 opaque newtype，不使用 pi `session_id`。
   - `SessionHandle` 轻量 cloneable，只暴露 `runtime_id()`、`snapshot()`、Dirty 订阅和命令 API；不暴露 raw Client、reducer 或 ClientEvent receiver。
   - Snapshot 包含 runtime id、epoch、revision、权威 `ConversationDocument`、phase、队列长度、startup diagnostic 和带 sequence/epoch 的 RuntimeEffect。
   - Dirty 仅提示 UI 拉取 Snapshot；旧 epoch effect/request/calibration/Extension response 由 runtime fence 拒绝。

4. **tool restart 与 identity**
   - tool restart 保持 RuntimeId 不变，成功替换底层进程时 epoch +1；旧 epoch 的异步结果被忽略。
   - fresh `get_state`/controls 后 Runtime 内同步真实 session id/path，但 RuntimeId 不变。
   - calibration 需同时满足当前 epoch、Idle phase、activity generation 一致。

5. **SessionUiState 真实收敛**
   - `ChatPanel.active` 已为 `Option<SessionHandle>`。
   - status、load generation、draft key/attachments、commands/controls/tool/control busy、branch/rebind、retry/compaction、Extension UI、list/scroll/follow-tail/expanded、runtime feedback 与 fresh identity 等会话态真实存储在唯一 `SessionUiState`。
   - ChatPanel 只保留 Manager/Handle 绑定、GPUI Entity/focus/subscription、纯 panel geometry，以及 file popup/index 等面板级暂态。未引入多会话 map/UI。

6. **maintenance HTML**
   - 历史 HTML 导出经共享 Manager 的独立 `MaintenanceGate`，默认可配置并发 1，不影响用户 Runtime 生命周期。
   - 保留成功但 shutdown 失败返回 cleanup warning、导出失败附带清理失败信息的语义。
   - 活会话 ExportHtml 继续走 SessionHandle control，不进入 maintenance。

## Tests added/updated

`pi-runtime` 新增/迁移覆盖：
- RuntimeId 与 clone handle 稳定；revision/effect sequence 单调。
- 旧 epoch effect fence。
- Manager config 强制 `max_restarts=0`。
- maintenance gate 共享调用者串行并发 1。
- historical export cleanup warning/error 语义。
- active config、Extension UI sanitize/coalesce/reset、compaction/retry event projection、typed controls/rebind 等迁移回归。

app 回归补回：
- host extension degradation 的 generation/reset/可见反馈。
- tool restart failure 清 busy、释放失效 handle、显示可恢复错误。

## Validation

| 命令 | Exit | 关键结果 |
|---|---:|---|
| `cargo fmt --all -- --check` | 0 | 无格式 diff |
| `cargo test -p pi-runtime` | 0 | 18 passed / 0 failed / 1 ignored |
| `cargo test -p gpui-pi` | 0 | 122 passed / 0 failed |
| `cargo clippy -p pi-runtime -p gpui-pi --all-targets -- -D warnings` | 0 | 通过 |
| PowerShell Parser ParseFile(`scripts/validate.ps1`) | 0 | `PARSER_OK` |
| `powershell.exe ... ./scripts/validate.ps1 -Logic` | 0 | `VALIDATE OK`，范围含 pi-runtime |
| `powershell.exe ... ./scripts/validate.ps1` | 0 | `VALIDATE OK`，全 workspace release 构建通过 |
| `powershell.exe ... ./scripts/check-pins.ps1` | 0 | zed/gpui-component/pi/pi-web 全绿 |
| `git diff --check` | 0 | 无 whitespace error |
| `rg -n 'Client::spawn' crates/app/src --glob '*.rs'` | 0（无匹配） | app 生产创建入口为零 |

说明：测试日志中存在既有 Windows `taskkill` 的非阻断 stderr，以及上游 `proc-macro-error2` future-incompat/linker informational warning；所有测试、clippy 和 validation exit 均为 0。

## Cargo.lock 合规证明

Lock diff 共 13 行：
- `gpui-pi` dependencies 增加 `"pi-runtime"` 1 行；
- 新增 `[[package]] name = "pi-runtime" version = "0.1.0"` 和既有 workspace dependency 列表 12 行。
- 无新增/修改任何 `source =`、`checksum =`；除新本地 package 的 `version = "0.1.0"` 外无 version 变化。
- 未执行 `cargo update`；最终 `check-pins.ps1` 全绿。

## Static search

- `crates/app/src`: `Client::spawn` **0 matches**。
- `crates/pi-runtime/src/lib.rs`: 3 个生产 spawn（user initial、tool restart、maintenance）及 1 个 `#[cfg(test)]` fake-child 测试 spawn。

## 未完成项

- 未运行需要显式 `GPUI_PI_TEST_FAKE_CHILD` 的 ignored typed-controls 测试；同类 pi-rpc fake-child 与 app 全量测试已通过。
- 未做人工启动后的真实 GUI 对话/截图；本轮最终 diff 没有视觉样式/布局设计变化，任务卡视觉审查维持 N/A。
- 未 commit（按任务要求）。

## 残余风险

- R21 按计划仍使用无界 Client subscription 和线程式请求；背压/有界 actor 明确留给 R22。
- `SessionUiState` 通过 ChatPanel 的 Deref/DerefMut 保持既有调用面，字段已物理集中，但后续 R24 多实例化前应将方法逐步收口到 state 自身 API。
- tool restart spawn 失败后旧 client 已停止，UI 清 handle；用户需要点击重新启动活会话，保持旧行为可恢复语义。
- RuntimeEffect 缓存固定保留最近 256 项，这是为了避免无界 effect 历史；Snapshot 本身仍为权威最终状态，一次性 effect 的慢消费者极端溢出策略可在 R22 背压轮进一步形式化。

## Git state

- staged files：无（`git diff --cached --name-only` 空）。
- `Cargo.lock`：有批准的 13 行必要变化，除此之外无漂移。

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "新增 pi-runtime 并完成 app 集中化、共享 Manager、SessionHandle/Snapshot/Dirty、真实 SessionUiState、maintenance HTML；Logic 和全量 validation 均为 VALIDATE OK，app Client::spawn 搜索为零。"
    }
  ],
  "changedFiles": [
    "Cargo.toml",
    "Cargo.lock",
    "crates/pi-runtime/Cargo.toml",
    "crates/pi-runtime/src/lib.rs",
    "crates/app/Cargo.toml",
    "crates/app/src/live_session.rs",
    "crates/app/src/main.rs",
    "crates/app/src/main_panel.rs",
    "crates/app/src/panels.rs",
    "crates/app/src/session_sidebar.rs",
    "crates/app/src/workspace.rs",
    "scripts/validate.ps1",
    "docs/立项文档.md",
    "AGENTS.md",
    "CLAUDE.md",
    "ROUNDS.md",
    "rounds/round-21/round-21.md"
  ],
  "testsAddedOrUpdated": [
    "crates/pi-runtime/src/lib.rs: RuntimeId/revision/effect sequence, old epoch fence, max_restarts clamp, maintenance gate, export cleanup and migrated runtime tests",
    "crates/app/src/panels.rs: host extension degradation and tool restart failure regression tests"
  ],
  "commandsRun": [
    {
      "command": "cargo fmt --all -- --check",
      "result": "passed",
      "summary": "exit 0"
    },
    {
      "command": "cargo test -p pi-runtime",
      "result": "passed",
      "summary": "18 passed, 0 failed, 1 ignored"
    },
    {
      "command": "cargo test -p gpui-pi",
      "result": "passed",
      "summary": "122 passed, 0 failed"
    },
    {
      "command": "cargo clippy -p pi-runtime -p gpui-pi --all-targets -- -D warnings",
      "result": "passed",
      "summary": "exit 0"
    },
    {
      "command": "PowerShell Parser::ParseFile scripts/validate.ps1",
      "result": "passed",
      "summary": "PARSER_OK"
    },
    {
      "command": "powershell.exe -File ./scripts/validate.ps1 -Logic",
      "result": "passed",
      "summary": "VALIDATE OK"
    },
    {
      "command": "powershell.exe -File ./scripts/validate.ps1",
      "result": "passed",
      "summary": "VALIDATE OK"
    },
    {
      "command": "powershell.exe -File ./scripts/check-pins.ps1",
      "result": "passed",
      "summary": "all pinned identities and manifests OK"
    },
    {
      "command": "rg -n 'Client::spawn' crates/app/src --glob '*.rs'",
      "result": "passed",
      "summary": "0 matches"
    },
    {
      "command": "git diff --check",
      "result": "passed",
      "summary": "exit 0"
    }
  ],
  "validationOutput": [
    "Logic validation: VALIDATE OK",
    "Full workspace validation: VALIDATE OK",
    "pi-runtime: 18 passed / 0 failed / 1 ignored",
    "gpui-pi: 122 passed / 0 failed",
    "check-pins: all OK"
  ],
  "residualRisks": [
    "R22 才实现有界 Actor/背压；R21 仍沿用无界底层 subscription，但仅 runtime 单点持续 drain。",
    "未做人工真实 GUI 启动/对话；自动化全量验证通过。",
    "RuntimeEffect 最近 256 项的溢出形式化策略留待 R22，Snapshot 最终状态始终权威。"
  ],
  "noStagedFiles": true,
  "diffSummary": "新增 pi-runtime 并把用户 RPC Runtime、reducer/event pump、tool restart、maintenance HTML 从 app 下沉；app 使用共享 Manager、SessionHandle/Snapshot/Dirty 和真实单一 SessionUiState。Cargo.lock 仅含批准的 13 行本地 path crate 变化。",
  "reviewFindings": [
    "no blockers found by implementation validation; independent parent review still required by project flow"
  ],
  "manualNotes": "未 commit/push/PR/merge。"
}
```
