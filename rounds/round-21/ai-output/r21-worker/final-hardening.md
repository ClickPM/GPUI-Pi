# R21 最终生命周期窄加固

## 结论

已完成第二轮 reviewer PASS 后要求的窄加固，未扩大范围，未 commit、push、创建 PR、创建其他 worktree 或启动子代理。

`fail_runtime` 现在只在持有 `RuntimeState` mutex 时完成以下原子状态变更：

1. 校验 `epoch / stopped / replacing` fence；
2. 设置 `stopped = true`；
3. 从 state 中取出 `Client`；
4. 将 reducer 置为准确的 `Error`；
5. 发布唯一 `RuntimeEffectKind::Stopped(Some(error))`。

随后离开锁作用域，再在锁外显式调用 `Client::shutdown()`。shutdown 错误按任务要求忽略，不改变已经发布的准确终态。这样保留了幂等 Stopped、禁止底层自动重启和崩溃错误语义，同时避免 supervisor/stdout join 时长期占有 RuntimeState 锁、阻塞 UI `snapshot()`。

## Changed files（本次窄增量）

- `crates/pi-runtime/src/lib.rs`
  - 重构 `fail_runtime`：锁内取出 Client 并发布终态，锁外 shutdown。
  - 增加中文注释说明为何不能持 RuntimeState 锁等待进程线程退出。
- `rounds/round-21/round-21.md`
  - 回填本次生命周期加固与验证结果。
  - 记录 `runtime_fake_child` 是测试 helper target；R17 打包必须只收录 `gpui-pi.exe` 与 `vendor/` 所需运行时，不得收录 helper。
- `ai-output/r21-worker/final-hardening.md`
  - 本报告。

未修改 app/workspace，因此按任务授权未重复全量 validation；上一轮全量 validation 刚完成且全绿。

## Tests

未新增或修改测试代码。现有真实 fake-child 崩溃测试 `tests::crash_with_restart_disabled_publishes_one_failed_terminal_state` 直接覆盖本次路径，验证：

- 崩溃后 Snapshot phase 为 `Error`；
- 只发布一个准确 `Stopped`；
- `max_restarts=0` 下不会底层复活；
- 终态后 Handle 命令被拒绝。

## Validation

| 命令 | Exit / 结果 |
|---|---|
| `cargo fmt --all -- --check` | 0 |
| `cargo test -p pi-runtime crash_with_restart_disabled_publishes_one_failed_terminal_state` | 0；目标测试 1 passed / 0 failed |
| `cargo test -p pi-runtime` | 0；lib 24 passed / 0 failed / 1 ignored，integration 1 passed |
| `cargo clippy -p pi-runtime --all-targets -- -D warnings` | 0 |
| `powershell.exe -NoProfile -ExecutionPolicy Bypass -File ./scripts/validate.ps1 -Logic` | 0；`VALIDATE OK`，范围包含 pi-runtime |
| `git diff --check` | 0 |
| `git diff --cached --name-only` | 空；无 staged 文件 |

Logic validation 同时完成 pins、fmt、四个逻辑 crate clippy/test/release build，最终输出 `VALIDATE OK`。

## Review disposition

本次改动精确落实第二轮 reviewer PASS 报告中的 NOTE 级生命周期加固：`fail_runtime` 不再持锁 drop/shutdown Client。人工复核确认：

- fence 仍位于同一锁作用域，不存在检查后竞态；
- `stopped=true` 在 publish 前设置，`Exited` / `RestartFailed` / channel disconnect 的重复路径会被幂等拒绝；
- `entry.publish` 仍在锁内使用同一个 state，Stopped sequence/revision/Dirty 语义不变；
- Client 在 state 中先被移除，后续命令无法取得 raw Client；
- 锁外 shutdown 不会触发新的 Manager spawn，且 Manager config 仍强制 `max_restarts=0`；
- shutdown 失败不会覆盖准确崩溃终态。

未发现 blocker。

## Residual risks

- R21 为保证一次性 effect 可靠，effect 历史仍无界；按既定计划由 R22 实现可靠终态权威字段、流式 latest-only、固定字节上限与背压。
- `runtime_fake_child` 作为测试 helper target 会在对应 release build 中产出辅助 exe；本轮不处理该正常构建噪声。R17 打包必须显式只收录产品 `gpui-pi.exe` 与 `vendor/` 运行时。
- Windows 测试仍可能输出既有 `taskkill` stderr；本次所有命令 exit 均为 0。

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "仅修改 fail_runtime 的锁/Client shutdown 生命周期顺序及 R21 任务卡实测；未触碰 app/workspace、未处理 fake-child 构建设施、未扩大到其他轮次。"
    },
    {
      "id": "criterion-2",
      "status": "satisfied",
      "evidence": "提供精确 changed files、目标崩溃测试、pi-runtime 全测/clippy、Logic VALIDATE OK、git diff --check 与无 staged 文件证据。"
    }
  ],
  "changedFiles": [
    "crates/pi-runtime/src/lib.rs",
    "rounds/round-21/round-21.md",
    "ai-output/r21-worker/final-hardening.md"
  ],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {
      "command": "cargo fmt --all -- --check",
      "result": "passed",
      "summary": "exit 0"
    },
    {
      "command": "cargo test -p pi-runtime crash_with_restart_disabled_publishes_one_failed_terminal_state",
      "result": "passed",
      "summary": "目标崩溃终态测试 1 passed / 0 failed"
    },
    {
      "command": "cargo test -p pi-runtime",
      "result": "passed",
      "summary": "lib 24 passed / 0 failed / 1 ignored；integration 1 passed"
    },
    {
      "command": "cargo clippy -p pi-runtime --all-targets -- -D warnings",
      "result": "passed",
      "summary": "exit 0"
    },
    {
      "command": "powershell.exe -NoProfile -ExecutionPolicy Bypass -File ./scripts/validate.ps1 -Logic",
      "result": "passed",
      "summary": "VALIDATE OK；pins/fmt/clippy/test/release build 全绿，范围含 pi-runtime"
    },
    {
      "command": "git diff --check",
      "result": "passed",
      "summary": "exit 0，无 whitespace error"
    },
    {
      "command": "git diff --cached --name-only",
      "result": "passed",
      "summary": "输出为空，无 staged 文件"
    }
  ],
  "validationOutput": [
    "目标崩溃测试：1 passed / 0 failed",
    "pi-runtime：24 passed / 0 failed / 1 ignored；integration 1 passed",
    "pi-runtime clippy -D warnings：通过",
    "Logic validation：VALIDATE OK",
    "git diff --check：通过"
  ],
  "residualRisks": [
    "R21 effect 历史为保证一次性结果可靠仍无界，R22 负责有界背压与可靠终态权威化。",
    "runtime_fake_child 是测试 helper target；R17 打包必须排除它，只收录 gpui-pi.exe 与 vendor 运行时。"
  ],
  "noStagedFiles": true,
  "diffSummary": "fail_runtime 在 RuntimeState 锁内完成幂等失败终态和 Client take，释放锁后显式 shutdown，避免进程 join 阻塞 UI Snapshot；任务卡记录验证与 R17 helper 排除要求。",
  "reviewFindings": [
    "no blockers；第二轮 reviewer NOTE 所述持锁 Client shutdown 风险已按最小方案关闭"
  ],
  "manualNotes": "未重复全量 validate，因为本次只修改纯逻辑 crate 与任务卡，且上一轮全量 validation 刚全绿；按任务要求完成 Logic validation。"
}
```
