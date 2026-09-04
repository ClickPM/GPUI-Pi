//! pi 内核的 RPC 客户端。
//!
//! 驱动官方发布的 pi 独立二进制（`pi --mode rpc`），用 JSONL over stdin/stdout
//! 通信。本 crate **不依赖 GPUI**，可在无窗口、无 GPU 的环境完整单测。
//!
pub mod host_extension;
pub mod jsonl;
pub mod platform;
pub mod process;
pub mod protocol;

pub use host_extension::{materialize_host_extension, materialize_writer_isolation_extension};
pub use platform::{JobLimits, JobObject, JobStats, SystemMemory, job_objects_supported};
pub use process::{
    Client, ClientConfig, ClientError, ClientEvent, DEFAULT_EVENT_BACKLOG_BYTES, EventDetach,
    EventStream, LifecycleEvent, SessionRebindOutcome, kill_process_tree,
};
pub use protocol::*;

/// 钉死的 pi 内核版本，与 `scripts/fetch-pi.*` 下载的版本必须一致。
///
/// 改这里等于换内核版本 —— 必须同步 `docs/立项文档.md` § 二 与两个 fetch 脚本。
pub const PINNED_PI_VERSION: &str = "0.84.2";

/// 钉死的子代理执行内核版本（npm 包 `pi-subagents-lite`）。
///
/// 改这里等于换子代理内核 —— 必须同步 `docs/立项文档.md` § 二、
/// `scripts/fetch-pi-subagents-lite.ps1` 与 `pins/` 下的 manifest 基线。
pub const PINNED_SUBAGENTS_LITE_VERSION: &str = "1.13.0";

/// 子代理执行内核在 `vendor/` 下的目录名。
#[must_use]
pub fn subagent_kernel_dir_name() -> String {
    format!("pi-subagents-lite-{PINNED_SUBAGENTS_LITE_VERSION}")
}

/// 子代理执行内核向模型暴露的工具名。
///
/// 顺序与 `pi-subagents-lite` 的 `registerTools()` 注册顺序一致，便于和它的源码对读。
pub const SUBAGENT_TOOL_NAMES: [&str; 3] = ["Agent", "StopAgent", "AgentStatus"];

/// 已知第三方子代理扩展注册的工具名 —— 生产会话恒以 `--exclude-tools` 拉黑。
///
/// 所有者裁定（2026-08-27）：内建内核是本应用唯一的子代理通道；用户全局安装的其他
/// 子代理扩展照常加载（其余功能不受影响），但其工具对模型不可见。逐名来源必须
/// 对着扩展源码核实后才准入列。当前名单覆盖 npm:pi-subagents@0.56.0 在**父会话**
/// 注册的全部三个工具：
/// - `subagent`：派发工具（`src/extension/index.ts:672`，`:716` 注册）；
/// - `subagent_wait`：等待工具，`index.ts:718` 无条件调 `registerWaitTool`
///   （`src/runs/background/wait-tool.ts:10` 定名、`:35` 无条件注册 ——
///   `enabled=false` 只改行为为立即返回，不跳过注册）；
/// - `subagent_supervisor`：监督工具，`session_start` 里 `supervisorChannel.start()`
///   最终在父会话注册（`src/intercom/native-supervisor-channel.ts:22` 定名、`:638` 注册）。
///
/// 刻意**不**入列的：`contact_supervisor` 只在该扩展自行 spawn 的子进程内注册
/// （`native-supervisor-channel.ts:301` 有 `readChildMetadata()` 门禁），
/// 不会出现在本应用拉起的会话里。
///
/// pi 的排除是大小写敏感的精确名集合（`args.ts:128` → `sdk.ts:251` 的
/// `new Set(excludeTools)`），且对扩展注册的工具同样生效（`agent-session.ts:2468-2478`
/// 对 `getAllRegisteredTools()` 应用 `isAllowedTool`），不会误伤内建内核的
/// [`SUBAGENT_TOOL_NAMES`] 三件套。
pub const THIRD_PARTY_SUBAGENT_TOOL_NAMES: [&str; 3] =
    ["subagent", "subagent_wait", "subagent_supervisor"];

/// 当前平台下 pi 可执行文件的文件名。
pub const fn pi_binary_name() -> &'static str {
    if cfg!(windows) { "pi.exe" } else { "pi" }
}

/// 当前平台对应的官方发布包标识（`pi-<target>.<ext>` 里的 `<target>` 部分）。
pub const fn pi_release_target() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x64"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "windows-arm64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else {
        "darwin-x64"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_name_matches_platform() {
        if cfg!(windows) {
            assert_eq!(pi_binary_name(), "pi.exe");
        } else {
            assert_eq!(pi_binary_name(), "pi");
        }
    }

    #[test]
    fn release_target_is_known() {
        const KNOWN: [&str; 6] = [
            "windows-x64",
            "windows-arm64",
            "linux-x64",
            "linux-arm64",
            "darwin-arm64",
            "darwin-x64",
        ];
        assert!(KNOWN.contains(&pi_release_target()));
    }

    /// fetch 脚本与代码里的版本必须同源，防止只改一边。
    #[test]
    fn pinned_version_matches_fetch_scripts() {
        let sh = include_str!("../../../scripts/fetch-pi.sh");
        let ps = include_str!("../../../scripts/fetch-pi.ps1");
        let needle = format!("v{PINNED_PI_VERSION}");
        assert!(sh.contains(&needle), "fetch-pi.sh 未钉 {needle}");
        assert!(ps.contains(&needle), "fetch-pi.ps1 未钉 {needle}");
    }
}
