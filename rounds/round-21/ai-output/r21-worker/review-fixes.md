# R21 独立审查修复报告

## 结论

已修复 `ai-output/r21-review/reviewer.md` 的全部 6 项 findings，未扩大到 R22–R27，未 commit/push/PR，未创建其他 worktree 或启动子代理。最终 Logic 与全量 validation 均为 `VALIDATE OK`。

## Finding disposition

1. **HIGH activity_generation wiring — FIXED**
   - `SessionHandle::dispatch` 删除 panel `activity_generation` 入参，直接在 RuntimeState 锁内捕获 `state.activity_generation`。
   - 删除 app 中永远为 0 的 `activity_generation` / `calibration_generation` 投影和失真的纯函数测试。
   - 新增真实 fake-child dispatch 测试：先通过 `complete` 真实产生 `AgentStart/AgentSettled`，再分别触发 RPC rejected 与 timeout ambiguous，验证 `pending_activity_generation == Some(1)` 且 Snapshot phase 恢复 `Idle`。

2. **MEDIUM 256 effect 截断 — FIXED（R21 最小安全方案）**
   - 删除 `VecDeque` 的 256 条 `pop_front` 截断。
   - 注释明确：R21 一次性结果只有 effect 载体，R22 建立“可靠终态权威字段 + 流式 latest-only + 固定字节上限”前必须保持原有无界可靠语义。
   - 新增 300 条 `ControlFinished` 一次性 effect 测试，验证 sequence 1..300 全部仍可消费。

3. **MEDIUM 崩溃终态 — FIXED**
   - event pump 将 `LifecycleEvent::Exited` / `RestartFailed` 幂等归并到 `fail_runtime`。
   - `fail_runtime` 设置 `stopped=true`、移除 Client、reducer phase=`Error`，发布唯一 `Stopped(Some("会话已崩溃，请重新启动…"))` 后退出 pump。
   - Manager spawn 仍强制 `max_restarts=0`；fake-child `crash` 测试验证崩溃后 Error + 单一 Stopped，handle 后续命令失败，未观察到 Restarting/Restarted 语义。

4. **LOW tool restart spawn 失败顺序 — FIXED**
   - restart 开始设置 `replacing=true`；旧 pump 的断线/Exited 在 replacement 窗口被 fence。
   - shutdown/spawn 失败统一走 `publish_tool_restart_failure`：先清 replacement、设置 stopped/Error，再发布准确 `ToolRestartFinished(Err)`。
   - 顺序回归验证 Snapshot 有准确 tool restart failure，且没有旧 pump `Stopped` 覆盖。

5. **LOW start_user 失败销毁旧会话 — FIXED**
   - 先成功 `Client::spawn` 并构造新 entry，再在 `active_user` 锁内 `replace`，随后优雅 shutdown 旧 entry。
   - spawn failure 测试验证旧 active RuntimeId 保持、旧 entry 未 stopped。
   - UI 和 `active_user` 仍只绑定一个 Runtime；替换的极短窗口可有两个 OS 进程并存，但没有持久多会话状态或 UI。

6. **LOW 未使用依赖 — FIXED**
   - 删除 `pi-runtime` 的 `futures` 与 `thiserror`。
   - fake-child binary 复用现有 fixture，需要实际使用的既有 `serde_json`；`Cargo.lock` 本地 stanza 因此为实际依赖 `pi-data/pi-render/pi-rpc/serde_json/tempfile`。

## Changed files（本次 reviewer 修复增量）

- `crates/pi-runtime/src/lib.rs`
- `crates/pi-runtime/Cargo.toml`
- `crates/pi-runtime/tests/runtime_fake_child.rs`
- `crates/app/src/panels.rs`
- `crates/pi-rpc/tests/fixtures/fake_child.rs`
- `Cargo.lock`
- `docs/立项文档.md`
- `rounds/round-21/round-21.md`
- `ai-output/r21-worker/review-fixes.md`

工作区仍包含上一阶段完整 R21 的其他改动，未在本次误删或重置。

## Tests added/updated

- `dispatch_rejected_after_prior_activity_restores_idle_from_runtime_generation`
- `dispatch_ambiguous_timeout_restores_idle_from_runtime_generation`
- `one_shot_effects_remain_reliable_beyond_256_publications`
- `crash_with_restart_disabled_publishes_one_failed_terminal_state`
- `tool_restart_failure_fences_old_pump_stopped_effect`
- `start_user_spawn_failure_preserves_existing_active_handle`
- `crates/pi-runtime/tests/runtime_fake_child.rs` binary availability integration check
- fake child 新增 `complete`、`reject`、`crash` 场景
- 删除 app 中仅测试失真 panel generation 纯函数的 1 项测试；行为由上述真实 wiring 测试替代。

## Validation commands

| Command | Exit/result |
|---|---|
| `cargo fmt --all -- --check` | 0 |
| `cargo test -p pi-runtime` | 0；lib 24 passed / 0 failed / 1 ignored，integration 1 passed |
| `cargo test -p gpui-pi` | 0；121 passed / 0 failed |
| `cargo clippy -p pi-runtime -p gpui-pi --all-targets -- -D warnings` | 0 |
| `cargo build -p pi-rpc --bin fake_child` | 0 |
| `GPUI_PI_TEST_FAKE_CHILD=... cargo test -p pi-runtime session_controls_and_switches_use_typed_rpc_state -- --ignored` | 0；1 passed |
| `powershell.exe ... ./scripts/validate.ps1 -Logic` | 0；`VALIDATE OK` |
| `powershell.exe ... ./scripts/validate.ps1` | 0；`VALIDATE OK` |
| `powershell.exe ... ./scripts/check-pins.ps1` | 0；全部 pins/manifests OK |
| `git diff --check` | 0 |
| `rg -n 'Client::spawn' crates/app/src --glob '*.rs'` | 0 matches |
| `git diff --cached --name-only` | empty |

非阻断信息：Windows 测试中的既有 `taskkill` stderr、release MSVC linker informational warning、上游 `proc-macro-error2` future-incompat warning；所有命令 exit 为 0。

## Cargo.lock diff

相对 HEAD 共新增 **12 行**：
- `gpui-pi` dependency 增加 `pi-runtime`：1 行。
- 新本地 `pi-runtime 0.1.0` stanza：11 行，实际依赖为 `pi-data`、`pi-render`、`pi-rpc`、`serde_json`、`tempfile`。
- 无任何 `source=` / `checksum=` 变化；除新本地 package 自身 `version=0.1.0` 外，无 version 变化。
- 未运行 `cargo update`；`check-pins.ps1` 全绿。

## Residual risks

- R21 为不丢一次性终态，effect history 保持无界；R22 必须将可靠终态放入权威 Snapshot、流式状态 latest-only，并实施固定字节上限与背压测试后才能有界。
- `start_user` 替换时先 spawn 新 Client、再 swap/stop 旧 Client，因此可短暂存在两个 OS 进程；但 Manager active slot 与 UI 始终只有一个，新 spawn 失败保留旧 handle。真正全局资源预算与并发槽属于 R23/R25，不在 R21 扩围。
- 本次没有执行或宣称视觉审查。

## Git state

- staged files：无。
- 未 commit、push、PR、merge。
- app 生产 `Client::spawn`：0 matches。

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "6 项 reviewer findings 均以 R21 最小范围修复；未实现 R22 Actor/背压或多会话 UI。Logic 与全量 validation 均为 VALIDATE OK。"
    },
    {
      "id": "criterion-2",
      "status": "satisfied",
      "evidence": "提供真实 fake-child dispatch/crash/restart/spawn-failure 测试、完整命令 exit、Cargo.lock 精确 diff、pins 与静态搜索证据。"
    }
  ],
  "changedFiles": [
    "crates/pi-runtime/src/lib.rs",
    "crates/pi-runtime/Cargo.toml",
    "crates/pi-runtime/tests/runtime_fake_child.rs",
    "crates/app/src/panels.rs",
    "crates/pi-rpc/tests/fixtures/fake_child.rs",
    "Cargo.lock",
    "docs/立项文档.md",
    "rounds/round-21/round-21.md",
    "ai-output/r21-worker/review-fixes.md"
  ],
  "testsAddedOrUpdated": [
    "dispatch_rejected_after_prior_activity_restores_idle_from_runtime_generation",
    "dispatch_ambiguous_timeout_restores_idle_from_runtime_generation",
    "one_shot_effects_remain_reliable_beyond_256_publications",
    "crash_with_restart_disabled_publishes_one_failed_terminal_state",
    "tool_restart_failure_fences_old_pump_stopped_effect",
    "start_user_spawn_failure_preserves_existing_active_handle",
    "crates/pi-runtime/tests/runtime_fake_child.rs",
    "crates/pi-rpc/tests/fixtures/fake_child.rs complete/reject/crash scenarios"
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
      "summary": "24 passed, 0 failed, 1 ignored; integration 1 passed"
    },
    {
      "command": "cargo test -p gpui-pi",
      "result": "passed",
      "summary": "121 passed, 0 failed"
    },
    {
      "command": "cargo clippy -p pi-runtime -p gpui-pi --all-targets -- -D warnings",
      "result": "passed",
      "summary": "exit 0"
    },
    {
      "command": "cargo build -p pi-rpc --bin fake_child && GPUI_PI_TEST_FAKE_CHILD=target/debug/fake_child.exe cargo test -p pi-runtime session_controls_and_switches_use_typed_rpc_state -- --ignored",
      "result": "passed",
      "summary": "ignored typed-controls test 1 passed"
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
      "command": "git diff --check",
      "result": "passed",
      "summary": "exit 0"
    },
    {
      "command": "rg -n 'Client::spawn' crates/app/src --glob '*.rs'",
      "result": "passed",
      "summary": "0 matches"
    },
    {
      "command": "git diff --cached --name-only",
      "result": "passed",
      "summary": "empty"
    }
  ],
  "validationOutput": [
    "Logic validation exit 0: VALIDATE OK",
    "Full validation exit 0: VALIDATE OK",
    "pi-runtime: 24 passed, 0 failed, 1 ignored; integration 1 passed",
    "gpui-pi: 121 passed, 0 failed",
    "typed-controls ignored test: 1 passed",
    "check-pins: all OK"
  ],
  "residualRisks": [
    "R21 effect history 为保证一次性结果可靠而无界；R22 需实现权威可靠终态、latest-only 流式状态和固定字节上限。",
    "start_user 成功替换的短窗口可能有两个 OS 进程，但 active_user/UI 始终仅一个；新 spawn 失败保留旧 handle。",
    "未执行或宣称视觉审查。"
  ],
  "noStagedFiles": true,
  "diffSummary": "修复 Runtime activity-generation wiring、一次性 effect 截断、崩溃失败终态、tool restart 失败 fencing、start_user 原子替换，并清理未使用依赖；Cargo.lock 仅 12 行批准的本地 path crate 变化。",
  "reviewFindings": [
    "HIGH activity_generation wiring: fixed and covered by real rejected/ambiguous dispatch tests",
    "MEDIUM effect truncation: fixed by restoring reliable unbounded R21 semantics with >256 test",
    "MEDIUM crash terminal state: fixed with idempotent Error + Stopped and fake-child crash test",
    "LOW tool restart failure ordering: fixed with replacement fence and regression test",
    "LOW start_user spawn failure: fixed by spawn-before-swap and preservation test",
    "LOW unused futures/thiserror: removed"
  ],
  "manualNotes": "No commit/push/PR/merge; no visual review claimed."
}
```
