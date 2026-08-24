# Code Context

## Files Retrieved
1. `crates/app/src/live_session.rs` (lines 780-917) - 当前用户 Session 的配置、`Client::spawn`、事件订阅与 `ActiveSession` 持有关系；这是 R21 集中化的主入口。
2. `crates/app/src/live_session.rs` (lines 1417-1494) - 历史 HTML 导出的完整短生命周期 RPC 创建、请求、shutdown 路径。
3. `crates/app/src/live_session.rs` (lines 580-625, 1260-1346) - 当前活动 Session 上的 `ExportHtml` 是复用已有 Client 的控制命令，不会另起进程，需与历史导出区分。
4. `crates/app/src/session_sidebar.rs` (lines 631-681) - 历史 HTML 导出的 UI 调用链及后台 executor 接缝。
5. `crates/pi-rpc/src/process.rs` (lines 32-59, 139-177) - `ClientConfig` 默认 `max_restarts = 3` 与唯一公开 `Client::spawn` API。
6. `crates/pi-rpc/src/process.rs` (lines 421-584) - supervisor 的自动重启判定和真正拼装 `pi --mode rpc` 的底层入口。
7. `crates/app/src/model_service.rs` (lines 802-817, 923-949, 1010-1015) - app 中其他 `Command::new(pi_binary)` 路径；属于 provider 登录/一次性 CLI，不是 RPC Runtime。
8. `crates/app/src/workspace.rs` (lines 39-94) - `Workspace::build` 创建 `SessionSidebar` 与 `ChatPanel`，是向两者注入同一个应用级 `Arc<RuntimeManager>` 的直接接线点。
9. `crates/app/src/main.rs` (lines 20-65) - 应用初始化和首窗口创建；适合只创建一次应用级共享 Manager。
10. `Cargo.toml` (lines 1-25) - workspace members 和内部 workspace dependencies，当前没有 `pi-runtime`。
11. `crates/app/Cargo.toml` (lines 1-31) - app 当前直接依赖 `pi-rpc`，新增后需依赖 `pi-runtime`；是否能移除 `pi-rpc` 取决于 R21 是否已迁完协议类型引用。
12. `scripts/validate.ps1` (lines 1-43) - `-Logic` 当前仅包含三个纯逻辑 crate，必须显式加入 `pi-runtime`。
13. `docs/立项文档.md` (lines 88-132) - `pi-runtime` 分层、唯一生产创建入口、maintenance 独立配额和 Manager 重启决策权的权威约束。
14. `docs/立项文档.md` (lines 308-316) - R21 验收明确要求历史 HTML 导出纳入 maintenance job、稳定 Handle/revision，并测试禁止底层自重启。

## Key Code

### 完整创建入口清单

经 `crates/app/src/**/*.rs` 全量搜索，app 生产代码中只有以下两个 `Client::spawn`：

1. **用户会话 Runtime** — `crates/app/src/live_session.rs:851-917`
   - `ActiveSession::spawn_with_pump` 由 fresh、恢复 Session、工具 preset restart 三条上层路径汇合。
   - 实际创建在 `crates/app/src/live_session.rs:890`：

```rust
let client = Client::spawn(config).map_err(|error| error.to_string())?;
let events = client.subscribe();
```

   - `ActiveSession` 随后直接持有 raw `Client`，并向 app 暴露 `client()`、`reducer()`、`reducer_mut()`（`crates/app/src/live_session.rs:917-935`），与 R21 的 `SessionHandle` 边界冲突。
   - `restart_with_tools` 并不直接调用 `Client::spawn`，但会回到 `spawn_with_pump`，因此是**等价创建路径**（`crates/app/src/live_session.rs:1018-1062`）。

2. **历史 HTML 导出 maintenance Runtime** — `crates/app/src/live_session.rs:1457-1488`
   - 创建在 `crates/app/src/live_session.rs:1474`。
   - 配置为恢复历史 session，并使用 `--no-extensions --no-skills --no-prompt-templates --no-context-files --offline`（`crates/app/src/live_session.rs:1462-1473`）。
   - 发 `Command::ExportHtml`，之后显式 shutdown（`crates/app/src/live_session.rs:1475-1487`）。
   - UI 调用从 `SessionSidebar::export_session_html` 经 GPUI background executor 进入该同步函数（`crates/app/src/session_sidebar.rs:631-681`）。当前每个不同 session id 都可同时启动，因此没有全局并发 1。

另有 `crates/app/src/live_session.rs:1888` 的 `Client::spawn`，位于 `#[cfg(test)]` 测试模块，仅是 app 单测，不属于生产入口。

### 不是新 Runtime 的 HTML 导出

当前活动会话的“导出 HTML”走已有 `ActiveSession.client`：`ControlRequest::ExportHtml` 在 `crates/app/src/live_session.rs:1334-1345` 直接发 RPC，不创建第二个进程。UI 入口在 `crates/app/src/panels.rs:1843-1870`。它应继续作为 SessionHandle 命令；不要错误计入 maintenance 并发槽。

### 等价 CLI 创建检查

`crates/app/src/model_service.rs` 中存在直接创建官方 pi binary 的命令：

- `spawn_cli`：`crates/app/src/model_service.rs:802-817`
- Windows provider 登录控制台包装：`crates/app/src/model_service.rs:923-949`
- `login_command`：`crates/app/src/model_service.rs:1010-1015`

这些路径没有传 `--mode rpc`，属于 provider 登录等一次性 CLI，按 `docs/立项文档.md:123-124` 明确不计入 Session Runtime，不应强行迁入 RuntimeManager。curl 路径同样无关。

真正添加 `--mode rpc` 的唯一底层位置是 `crates/pi-rpc/src/process.rs:577-584`：

```rust
let mut command = ProcessCommand::new(&config.binary);
command.args(["--mode", "rpc"]);
```

因此 app 侧只需封死所有生产 `Client::spawn`；不应复制或绕过 `pi-rpc` 的进程树清理封装。

## 建议的 R21 maintenance 最小 API

建议把**配额、配置收口、spawn、请求和 shutdown 全部留在 `pi-runtime`**，避免给 app 一个通用 closure/raw `Client`，否则“唯一入口”只是形式迁移，app 仍可任意操纵 Client。

```rust
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    pub user_slots: usize,
    pub maintenance_slots: usize, // Default 初值 1，不写死为产品契约
}

#[derive(Clone)]
pub struct RuntimeManager { /* Arc<Inner> */ }

pub struct HistoricalHtmlExportRequest {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub session_path: PathBuf,
    pub output_path: PathBuf,
}

pub struct HistoricalHtmlExportResult {
    pub path: PathBuf,
    pub cleanup_warning: Option<String>,
}

impl RuntimeManager {
    pub fn new(limits: RuntimeLimits) -> Self;

    // 同步阻塞 API 与当前 background_executor 调用方式直接兼容。
    // 内部等待独立 maintenance permit；不消耗 user slot。
    pub fn export_historical_html(
        &self,
        request: HistoricalHtmlExportRequest,
    ) -> Result<HistoricalHtmlExportResult, RuntimeError>;
}
```

内部实现建议：

1. `MaintenanceGate` 使用 `Mutex<State> + Condvar` 和 RAII `MaintenancePermit`；`active < maintenance_slots` 才放行，第二个请求等待而非另起进程。permit 在 spawn 失败、RPC 失败、shutdown 失败及正常返回时均自动释放。
2. `export_historical_html` 内部构造 `ClientConfig`，**无条件覆盖 `config.max_restarts = 0`**，再调用 crate-private factory。
3. 用一个私有/`#[cfg(test)]` 可注入的工厂形成测试接缝，而不是公开 raw Client：

```rust
trait RuntimeFactory: Send + Sync {
    type Runtime: RpcRuntime;
    fn spawn(&self, config: ClientConfig) -> Result<Self::Runtime, RuntimeError>;
}

trait RpcRuntime: Send {
    fn export_html(&self, output: &Path) -> Result<PathBuf, RuntimeError>;
    fn shutdown(self) -> Result<(), RuntimeError>;
}
```

生产 `PiRuntimeFactory` 是 `pi-runtime` 内唯一调用 `Client::spawn` 的实现；fake factory 可记录配置、同时存活数、调用顺序和 shutdown。若关联类型使存储不便，可改为 `Box<dyn RpcRuntime>`。
4. 保留现有“导出成功但 shutdown 失败返回 warning；导出失败时附加 cleanup 错误”的语义，当前基线在 `finish_historical_export`（`crates/app/src/live_session.rs:1433-1455`）。该逻辑应迁到 runtime crate 并继续单测。
5. `RuntimeLimits::default()` 可给 `maintenance_slots = 1`，但构造参数必须可配置，以符合 `AGENTS.md:134`。

### 推荐测试接缝与最小测试集

放在 `crates/pi-runtime` 的纯逻辑测试，不依赖 GPUI 或真实 pi：

1. **独立配额 1**：fake runtime 的第一个 maintenance job 阻塞；并发提交第二个；断言 `max_live_maintenance == 1` 且第二个尚未调用 factory；释放第一个后第二个开始。
2. **不占用户槽**：占满配置的 user slot 后，maintenance 仍能获得 permit 并启动；反向也验证 maintenance 正在运行不减少 user 可用槽。即使 R21 user manager 很薄，也应把两个计数器分开测试。
3. **配置强制收口**：调用方传入/构造后的 fake 记录必须显示 `max_restarts == 0`；并核对历史导出的 offline/no-extension 参数、cwd、initial_session。
4. **错误路径释放 permit**：factory spawn 失败、export RPC 失败、shutdown 失败三种情况下，后续 maintenance 都能开始，防止配额永久泄漏。
5. **清理语义**：导出成功 + shutdown 失败 => `Ok` 带 `cleanup_warning`；导出失败 + shutdown 失败 => 主错误和清理错误都可观测。迁移现有 `crates/app/src/live_session.rs:1930-1954` 一带测试。
6. **禁止底层复活**：fake factory 捕获 config 并断言 0；另可复用 `pi-rpc` fake child 做集成测试，令进程崩溃后确认只有 `Exited/RestartFailed(limit 0)`、没有 `Restarting/Restarted`，满足 `docs/立项文档.md:126-131`。
7. **全局而非每窗口配额**：两个调用者 clone 同一个 `RuntimeManager` 并发导出，仍只能活跃 1 个。此测试能防止未来把 Manager 错建到 `SessionSidebar`/`ChatPanel` 内。

## Architecture

当前数据流：

```text
main -> Workspace::build
  ├─ SessionSidebar -> export_historical_html -> Client::spawn (短生命周期)
  └─ ChatPanel -> ActiveSession::spawn_with_pump -> Client::spawn (用户会话)
                                      └─ Client supervisor -> ProcessCommand("pi", "--mode", "rpc")
```

R21 最小目标流：

```text
main 创建一次 Arc<RuntimeManager>
  -> 注入 Workspace
     ├─ SessionSidebar clone Manager -> manager.export_historical_html(...)
     └─ ChatPanel clone Manager -> manager.start/resume Session -> SessionHandle

pi-runtime::PiRuntimeFactory -> 唯一生产 Client::spawn
pi-rpc -> 仍只负责单进程监督、JSONL、进程树终止
```

应用级实例建议在 `crates/app/src/main.rs:26-65` 的 application run 闭包内创建一次，然后传给 `Workspace::new`；`Workspace::build` 在 `crates/app/src/workspace.rs:65-94` 同时创建 sidebar/chat，天然可把同一个 `Arc` 分发给二者。不要在 `ChatPanel::new` 或 `SessionSidebar::new` 内各自 `RuntimeManager::new`，否则 maintenance 配额会变成每窗口/每组件配额。

## Cargo / validate 所需改动

1. 根 `Cargo.toml:3-9` 的 `members` 增加 `"crates/pi-runtime"`。
2. 根 `Cargo.toml:17-22` 的 `[workspace.dependencies]` 增加：

```toml
pi-runtime = { path = "crates/pi-runtime" }
```

3. 新建 `crates/pi-runtime/Cargo.toml`，最小依赖至少 `pi-rpc.workspace = true`、`thiserror.workspace = true`；若把 historical session cwd 加载也迁入 runtime，则需 `pi-data.workspace = true`。更清晰的边界是 app 先读 session 得到 cwd，runtime 接收完整 request，这样 maintenance 核心无需依赖 `pi-data`。
4. `crates/app/Cargo.toml:15-29` 增加 `pi-runtime.workspace = true`。
5. app 目前大量直接使用 `pi_rpc` 协议类型；R21 不应为追求依赖图美观强行经 `pi-runtime` re-export。只有当所有直接使用都迁走后才能删除 `pi-rpc.workspace = true`，否则保留是合理的；硬性禁止的是 app 持有/创建 raw Client，不是禁止协议类型依赖。
6. `scripts/validate.ps1:3-4` 注释从“三个纯逻辑 crate”更新为四个。
7. `scripts/validate.ps1:23-27` 的 Logic scope 增加 `"-p", "pi-runtime"`，范围提示同步为 `pi-rpc / pi-data / pi-render / pi-runtime`。全量 `--workspace` 会因 workspace member 自动纳入。
8. 修改 PowerShell 后按项目约定至少做 Parser 语法验证，并实际运行 `./scripts/validate.ps1 -Logic`。

## 风险与约束

- **最大风险：默认自动重启仍为 3。** `ClientConfig::new` 在 `crates/pi-rpc/src/process.rs:46-59` 默认 `max_restarts = 3`；任何 Manager 创建路径忘记置 0，崩溃后都会绕过配额。应在唯一 factory 内最终覆盖，而不只依赖各调用点自觉设置。
- **`max_restarts = 0` 的现实现语义正确但会发 `RestartFailed`。** supervisor 在首次退出后于 `crates/pi-rpc/src/process.rs:547-559` 立即命中 limit 并返回，不会发 `Restarting`。Manager 的生命周期归并要避免把这条 limit 事件误认为一次新的启动失败。
- **同步等待的线程占用。** 当前 sidebar 已把导出放到 GPUI background executor，故同步 Condvar 最小改动可行；禁止在 UI 线程直接调用。若 background executor 线程池很小，排队 job 会占一个 worker，R22/R23 可再升级为显式异步队列，但不必在 R21先造完整 scheduler。
- **取消语义。** 当前用户取消仅发生在文件选择前；导出开始后没有取消。R21 最小 API 可保持现状，但排队期间窗口关闭不会自动取消。RAII 必须保证调用 future/task 被丢弃或线程异常时不泄漏 permit；若仍是同步 worker，至少 panic unwind 会 drop permit。
- **shutdown 所有权。** 不要把 `Client` clone 泄露到 app，否则 `shutdown(self)` 后仍可能有 clone，且唯一入口/配额生命周期变得不可证明。maintenance wrapper 应独占 runtime 并在返回前完成清理。
- **输出路径语义。** 当前代码忽略 RPC 返回的 `data.path`，成功时固定返回调用方 `output_path`（`crates/app/src/live_session.rs:1481-1487`）。迁移时要么保持兼容，要么明确验证并采用 RPC path；不要无意改变通知内容。
- **测试重复入口。** app 测试中直接 `Client::spawn`（`crates/app/src/live_session.rs:1888`）虽不违反“生产入口”，但迁移 `load_controls` 后可能自然失效。纯 `pi-rpc` 隔离测试允许直 spawn；app 测试优先改走 fake RuntimeManager，避免架构测试继续绑定 raw Client。
- **Cargo.lock 红线。** 新增本地 workspace crate/path dependency通常只增加本地 package 记录，不应更新上游版本；必须检查 lock diff，绝不能运行 `cargo update` 或接受钉版本漂移。

## Start Here

先打开 `crates/app/src/live_session.rs:780-917`。这里是用户 Session 所有创建路径的汇合点，也是 raw `Client`、事件泵、reducer 和 restart 行为耦合最深的位置；先定义 `pi-runtime::SessionHandle`/factory 边界，再迁历史导出，能避免为 maintenance 单独设计一套与主 Runtime 不兼容的创建抽象。
