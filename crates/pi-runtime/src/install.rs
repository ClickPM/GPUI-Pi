//! 安装根解析与首启自检。
//!
//! 开发态用编译期仓库根（`CARGO_MANIFEST_DIR/../..`）；打包后的绿色目录把
//! `vendor/pi` 放在 `gpui-pi.exe` 旁边。两者必须共用同一套相对布局，否则会出现
//! 「pi 起得来但子代理内核找不到」这种只在安装包里复现的问题。

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use pi_rpc::{PINNED_PI_VERSION, pi_binary_name, subagent_kernel_dir_name};

/// 覆盖安装根。测试与排障用；生产路径靠 exe 旁 `vendor/` 或仓库根。
pub const INSTALL_ROOT_ENV: &str = "GPUI_PI_ROOT";

/// 绿色包 / 开发树里运行时必须存在的相对路径（相对安装根）。
#[must_use]
pub fn bundled_pi_rel() -> PathBuf {
    Path::new("vendor").join("pi").join(pi_binary_name())
}

/// 子代理内核相对安装根的目录。
#[must_use]
pub fn bundled_kernel_rel() -> PathBuf {
    Path::new("vendor").join(subagent_kernel_dir_name())
}

/// 解析安装根：显式覆盖 → exe 旁已有 `vendor/pi` → 编译期仓库根。
#[must_use]
pub fn resolve_install_root(override_root: Option<PathBuf>, exe_dir: Option<&Path>) -> PathBuf {
    if let Some(root) = override_root {
        return root;
    }
    if let Some(dir) = exe_dir
        && official_binary_in(dir).is_file()
    {
        return dir.to_path_buf();
    }
    workspace_root()
}

/// 当前进程的安装根。
#[must_use]
pub fn install_root() -> PathBuf {
    let override_root = std::env::var_os(INSTALL_ROOT_ENV).map(PathBuf::from);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    resolve_install_root(override_root, exe_dir.as_deref())
}

/// 钉死的官方 pi 二进制。
#[must_use]
pub fn official_binary() -> PathBuf {
    official_binary_in(&install_root())
}

/// 钉死的子代理执行内核目录（`pi -e <该目录>`）。
#[must_use]
pub fn official_subagent_kernel() -> PathBuf {
    official_kernel_in(&install_root())
}

#[must_use]
pub fn official_binary_in(root: &Path) -> PathBuf {
    root.join(bundled_pi_rel())
}

#[must_use]
pub fn official_kernel_in(root: &Path) -> PathBuf {
    root.join(bundled_kernel_rel())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// 文件层自检：不启动 pi 进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleStatus {
    pub root: PathBuf,
    pub pi_binary: PathBuf,
    pub pi_present: bool,
    pub kernel_dir: PathBuf,
    pub kernel_ready: bool,
}

impl BundleStatus {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.pi_present && self.kernel_ready
    }

    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.pi_present {
            problems.push(format!(
                "找不到官方 pi（{}），绿色包需与 vendor\\pi\\ 放在同一目录",
                self.pi_binary.display()
            ));
        }
        if !self.kernel_ready {
            problems.push(format!(
                "内建子代理内核未就绪（{}）",
                self.kernel_dir.display()
            ));
        }
        problems
    }
}

#[must_use]
pub fn inspect_bundle(root: &Path) -> BundleStatus {
    let pi_binary = official_binary_in(root);
    let kernel_dir = official_kernel_in(root);
    BundleStatus {
        root: root.to_path_buf(),
        pi_present: pi_binary.is_file(),
        pi_binary,
        kernel_ready: kernel_dir.join("package.json").is_file(),
        kernel_dir,
    }
}

/// 跑 `pi --version`，应等于钉死的 [`PINNED_PI_VERSION`]。
pub fn read_pi_version(binary: &Path) -> Result<String, String> {
    if !binary.is_file() {
        return Err(format!("pi 不存在：{}", binary.display()));
    }
    let mut command = Command::new(binary);
    command.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(pi_rpc::platform::no_window_creation_flags());
    }
    let output = command
        .output()
        .map_err(|error| format!("无法启动 pi --version：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pi --version 失败（status {}）",
            output.status.code().unwrap_or(-1)
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        return Err("pi --version 没有输出".to_owned());
    }
    Ok(version)
}

#[must_use]
pub fn pi_version_matches_pin(observed: &str) -> bool {
    observed.trim() == PINNED_PI_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_root_wins_over_exe_and_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("pretend-exe-dir");
        std::fs::create_dir_all(exe.join("vendor").join("pi")).unwrap();
        std::fs::write(exe.join(bundled_pi_rel()), b"not-used").unwrap();
        let override_root = dir.path().join("override");
        std::fs::create_dir_all(&override_root).unwrap();
        let resolved = resolve_install_root(Some(override_root.clone()), Some(&exe));
        assert_eq!(resolved, override_root);
    }

    #[test]
    fn exe_dir_wins_when_vendor_pi_is_beside_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor").join("pi")).unwrap();
        std::fs::write(dir.path().join(bundled_pi_rel()), b"pi").unwrap();
        let resolved = resolve_install_root(None, Some(dir.path()));
        assert_eq!(resolved, dir.path());
        assert!(inspect_bundle(&resolved).pi_present);
    }

    #[test]
    fn workspace_fallback_uses_the_compile_time_repo_layout() {
        let resolved = resolve_install_root(None, None);
        let binary = official_binary_in(&resolved);
        assert_eq!(
            binary.file_name().and_then(|name| name.to_str()),
            Some(pi_binary_name())
        );
        assert!(
            binary.ends_with(bundled_pi_rel()),
            "workspace fallback 必须保持 vendor/pi 相对布局，实际 {}",
            binary.display()
        );
    }

    #[test]
    fn inspect_bundle_reports_missing_runtime_files() {
        let dir = tempfile::tempdir().unwrap();
        let status = inspect_bundle(dir.path());
        assert!(!status.pi_present);
        assert!(!status.kernel_ready);
        assert!(!status.is_ready());
        assert_eq!(status.problems().len(), 2);
    }

    #[test]
    fn inspect_bundle_is_ready_when_pi_and_kernel_exist() {
        let dir = tempfile::tempdir().unwrap();
        let pi_dir = dir.path().join("vendor").join("pi");
        std::fs::create_dir_all(&pi_dir).unwrap();
        std::fs::write(pi_dir.join(pi_binary_name()), b"").unwrap();
        let kernel = dir.path().join("vendor").join(subagent_kernel_dir_name());
        std::fs::create_dir_all(&kernel).unwrap();
        std::fs::write(kernel.join("package.json"), "{}").unwrap();
        let status = inspect_bundle(dir.path());
        assert!(status.is_ready(), "{status:?}");
        assert!(status.problems().is_empty());
    }

    #[test]
    fn kernel_directory_name_embeds_the_pinned_version() {
        assert!(
            subagent_kernel_dir_name().ends_with(pi_rpc::PINNED_SUBAGENTS_LITE_VERSION),
            "{}",
            subagent_kernel_dir_name()
        );
    }

    #[test]
    fn pin_matcher_accepts_only_the_exact_pinned_version() {
        assert!(pi_version_matches_pin(PINNED_PI_VERSION));
        assert!(pi_version_matches_pin(&format!(" {PINNED_PI_VERSION}\n")));
        assert!(!pi_version_matches_pin("0.0.0"));
    }

    #[test]
    fn official_vendor_pi_reports_the_pinned_version_when_present() {
        let binary = official_binary();
        if !binary.is_file() {
            return;
        }
        let version = read_pi_version(&binary).expect("pi --version");
        assert!(
            pi_version_matches_pin(&version),
            "expected {PINNED_PI_VERSION}, got {version}"
        );
    }
}
