//! pi 内核的 RPC 客户端。
//!
//! 驱动官方发布的 pi 独立二进制（`pi --mode rpc`），用 JSONL over stdin/stdout
//! 通信。本 crate **不依赖 GPUI**，可在无窗口、无 GPU 的环境完整单测。
//!
pub mod host_extension;
pub mod jsonl;
pub mod process;
pub mod protocol;

pub use host_extension::materialize_host_extension;
pub use process::{
    Client, ClientConfig, ClientError, ClientEvent, DEFAULT_EVENT_BACKLOG_BYTES, EventDetach,
    EventStream, LifecycleEvent, SessionRebindOutcome, kill_process_tree,
};
pub use protocol::*;

/// 钉死的 pi 内核版本，与 `scripts/fetch-pi.*` 下载的版本必须一致。
///
/// 改这里等于换内核版本 —— 必须同步 `docs/立项文档.md` § 二 与两个 fetch 脚本。
pub const PINNED_PI_VERSION: &str = "0.84.2";

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
