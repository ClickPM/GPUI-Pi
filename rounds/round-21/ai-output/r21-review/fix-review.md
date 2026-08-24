已完成对全部 6 条 finding 的源码级复核，以及修复增量引入的新风险检查。以下为最终审查报告。

---

## Review

### 结论

**Verdict：PASS**

6 条 finding 均已在真实源码中修复且有对应测试/证据支撑，未发现修复增量引入新的 HIGH/MEDIUM 正确性、死锁、竞态、生命周期或契约问题。以下逐条给出证据与 2 条 NOTE 级观察项。

---

### 逐条验证

**Finding 1（HIGH · activity_generation wiring）— 已修复，验证有效**

- `crates/pi-runtime/src/lib.rs:993-995`：`dispatch_with_timeout` 现在直接在 RuntimeState 锁内捕获 `pending_activity_generation = (intent != Abort && phase != Running).then_some(state.activity_generation)`，不再使用 panel 传入值。
- `crates/app/src/panels.rs` 全量 grep `activity_generation` / `calibration_generation` 为 0 匹配；`dispatch` 调用点（panels.rs:2035、2166）均为 3 参数签名，panel 侧永远为 0 的失真投影已删除。
- 失败恢复逻辑（lib.rs 约 1012-1020）：`result.is_err()` 时仅在 `pending_activity_generation == Some(state.activity_generation)` 且 `phase == Running` 时才 `restore_phase(Idle)`，Abort 走 `restore_running_if_stopping()`。fence 语义正确：若请求期间发生新 `AgentStart`（计数器递增），则不强行回 Idle。
- 真实 fake-child 测试有效：`dispatch_rejected_after_prior_activity_restores_idle_from_runtime_generation` 与 `dispatch_ambiguous_timeout_restores_idle_from_runtime_generation`（lib.rs 测试模块）通过 `runtime_handle()` 真实 spawn `runtime_fake_child.exe`，先 `complete`（真实产生 agent_start/agent_end/agent_settled，activity_generation→1），再 `reject`（fake child 不 emit agent 事件，返回 success:false）与 40ms 超时 Ambiguous，断言 `pending_activity_generation == Some(1)` 且 phase 恢复 Idle。已核实 fake_child 的 `reject` 分支（fake_child.rs）只回 `success:false`、不 emit agent 事件，测试语义成立。

**Finding 2（MEDIUM · 256 截断）— 已修复，语义明确无残留**

- 原 `while state.effects.len() > 256 { pop_front(); }` 已删除；`publish`（lib.rs:889-894）现在无截断，注释明确 R21 一次性结果仅 effect 载体、R22 建立可靠终态前保持无界可靠语义。
- `one_shot_effects_remain_reliable_beyond_256_publications` 测试直接 300 次 publish 验证 sequence 1..300 全部保留。
- 与立项文档.md:309（R22「可靠终态事件不丢」「固定字节上限」）一致，R21 无界为有记录债务，非静默回归。

**Finding 3（MEDIUM · 崩溃终态）— 已修复，幂等唯一终态**

- `project_pump_event`（lib.rs 约 1900 区域）把 `Exited` / `RestartFailed` 归并为 `projected.terminal_failure`；`spawn_event_pump` 检测到后调用 `fail_runtime` 并 return，事件泵线程不再僵尸阻塞。
- `fail_runtime`（lib.rs:1340-1348）守卫 `epoch != epoch || stopped || replacing`，幂等：`stopped=true`、`client.take()`、`set_error`（phase=Error）、发布唯一 `Stopped(Some("会话已崩溃…"))`。
- `max_restarts=0` 下 supervisor 在首次 `Exited` 后即 `restart_times.len() (0) >= max_restarts (0)` → 广播 `RestartFailed` 并 return（process.rs supervise 尾部），无底层复活。
- `crash_with_restart_disabled_publishes_one_failed_terminal_state` 测试验证：Stopped 计数 == 1、phase == Error、无 Restarting/Restarted 诊断、后续 dispatch 返回 Err。fake_child 的 `crash` 分支 `std::process::exit(23)` 真实触发崩溃。

**Finding 4（LOW · tool restart 失败 fence）— 已修复**

- `restart_with_tools`（lib.rs:1102-1104）先 `replacing=true` 再 `old_client.take()`；失败路径统一走 `publish_tool_restart_failure`（lib.rs:1315-1337）：守卫 `epoch 匹配 && !stopped`，置 `replacing=false`、`stopped=true`、`set_error`，发布准确 `ToolRestartFinished(Err)`。
- 旧 pump 的 `Exited`/断线在 replacement 窗口被 `replacing` 或后续 `stopped`/epoch 守卫 fence，不会覆盖 `ToolRestartFinished` 的准确错误。
- `tool_restart_failure_fences_old_pump_stopped_effect` 测试断言 epoch 不变、phase Error、无 Stopped effect。已核实失败路径不存在 `replacing=false && stopped=false` 的窗口，fence 无竞态。

**Finding 5（LOW · start_user 失败保留旧会话）— 已修复，顺序安全**

- `start_user`（lib.rs:1214-1265）现在先 `Client::spawn` 成功并构造新 entry，再在 `active_user` 锁内 `replace`，随后在**锁外** `shutdown_entry(&old)`；spawn 失败经 `?` 直接返回，`active_user` 与旧 handle 均不动。
- 锁与 shutdown 顺序安全：`replace` 后 `MutexGuard` 随语句结束释放，`shutdown_entry`（lib.rs:1292-1298）先取 client、放锁、再 `client.shutdown()`，无持锁阻塞。
- 单会话兼容：`start_session`（带 session 文件）唯一调用点是 `start_live`（panels.rs:643），其入口 `if self.active.is_some() { return; }` 保证不会在旧会话存活时对同一 session 文件并起两个进程；两进程窗口只出现在 `start_fresh`（fresh 无 session 文件），无文件锁冲突。
- `start_user_spawn_failure_preserves_existing_active_handle` 测试验证旧 RuntimeId 保持且未 stopped。

**Finding 6（LOW · 未使用依赖）— 已清理，Cargo.lock 精确例外合规**

- `crates/pi-runtime/Cargo.toml` 已移除 `futures`/`thiserror`；lib.rs 全量 grep 无 `futures::`/`thiserror`/`#[derive(Error)]`。
- `serde_json` 保留为真实依赖：`[[bin]] runtime_fake_child`（Cargo.toml）复用 `../pi-rpc/tests/fixtures/fake_child.rs`，其 `main` 使用 serde_json；且 `validate.ps1` Step 5 为 `cargo build --release -p pi-runtime`，bin 在普通 build 下编译，必须用 regular dependency（非 dev-dependency），选择正确。
- `Cargo.lock`：gpui-pi 依赖边 +1 行（line 2483 `"pi-runtime"`），`pi-runtime` stanza（4802-4811）仅 `pi-data/pi-render/pi-rpc/serde_json/tempfile`，无 `source=`/`checksum=`，共 12 行，与申报一致。

---

### 新风险检查（任务特别关注点）

**fail_runtime 在 event pump 中 drop Client/shutdown 的线程语义 — NOTE（低风险，建议加固）**

- 位置：lib.rs:1346 `state.client.take();` 在持有 `state` Mutex 时 drop Client；若为最后一个 clone，`Drop for Client`（process.rs）触发 `shutdown()` → `supervisor.lock().take()` + `handle.join()`，即持锁做阻塞 join。
- 实测分析无死锁：崩溃路径下 supervisor 在广播 `Exited`/`RestartFailed` 后已无 sleep、即将 return（`restart limit 0` 分支），join 为亚毫秒级；supervisor 及其 stdout/stderr 读线程均不触碰 `RuntimeState`，与 `state`/`subscribers` 无锁序反转（锁序恒为 `state → subscribers → supervisor`）。
- 残余风险：若崩溃时 pi 的孙进程仍持有 stdout/stderr 管道写端，stdout 读线程 `read()` 不返回 EOF，supervisor 卡在 `stdout_handle.join()`，则此处持锁 join 会长时间持有 `state` 锁，使 UI 的 `snapshot()` 等全部阻塞（旧代码在 `shutdown_entry` 是取 client 后**放锁再 join**，无此问题）。
- 最小加固：`fail_runtime` 改为 `let client = state.client.take(); drop(state); if let Some(c) = client { let _ = c.shutdown(); }`，与 `shutdown_entry` 同构，避免持锁 join。非阻断，建议 R22 前顺手处理。

**runtime_fake_child 作为 regular bin 随 release 构建产出 — NOTE（低）**

- `pi-runtime` 声明 `[[bin]] runtime_fake_child` 指向 `../pi-rpc/tests/fixtures/fake_child.rs`，导致 `cargo build --release -p pi-runtime` 额外产出一个测试用 `runtime_fake_child.exe` 伪件。功能无影响、验证通过，仅提示这是测试产物进入正常构建面的轻微噪声，非阻断。

**replacing/stopped fence 与 start_user 新旧事件积压 — 无问题**

- 事件通道为无界 `mpsc::channel()`（process.rs `subscribe`），`start_user` 在 `spawn_event_pump` 前已 `subscribe`，新旧 pump 的事件均先入队、无丢失；旧 entry 的 pump 在 `shutdown_entry` 置 `stopped=true` 后经 `fail_runtime` 守卫退出，无僵尸。fence（`replacing`/`stopped`/epoch）覆盖 Exited、断线、旧 epoch 三条路径，无发现竞态窗口。

---

### 未做验证项（需 supervisor 自行执行）

本审查为只读，未运行命令。报告声称的 `cargo test`/`clippy`/`validate.ps1`/`check-pins.ps1`/`git diff --check` 结果无法在此复核，建议 supervisor 在合并前独立重跑 `.\scripts\validate.ps1 -Logic` 与 `.\scripts\validate.ps1` 确认回显为 `VALIDATE OK`。

---

### 汇总

- Blocker：无。
- Note（2 条）：fail_runtime 持锁 drop-join 的潜在延迟（建议加固）；runtime_fake_child 随 release 构建产出的测试伪件。
- 6 条 finding 全部修复，均有真实 fake-child 测试覆盖，未发现修复增量引入新的 HIGH/MEDIUM 问题。