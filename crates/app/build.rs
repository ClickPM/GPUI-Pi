use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-env-changed=RC");

    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo 未提供 CARGO_MANIFEST_DIR"));
    let assets_dir = manifest_dir.join("assets");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo 未提供 OUT_DIR"))
        .join("gpui-pi-icon.res");
    let status = Command::new(find_resource_compiler())
        .args(["/nologo", "/c65001", "/i"])
        .arg(&assets_dir)
        .arg("/fo")
        .arg(&output)
        .arg(assets_dir.join("app.rc"))
        .status()
        .expect("无法启动 Windows SDK rc.exe");
    assert!(status.success(), "Windows 应用图标资源编译失败");

    println!("cargo:rustc-link-arg-bin=gpui-pi={}", output.display());

    // Windows 主线程默认 1MB 栈对 GPUI 的 debug 构建不够：元素树布局的调用深度在
    // debug（无内联、帧膨胀）下会一次性吃穿 1MB，启动即 0xc00000fd（R26 实测：
    // release 正常、debug 空 agent 目录也必崩，故障与会话内容无关，纯阈值问题——
    // 早晨的 debug 构建侥幸在阈值下，一次重编译后被推过线）。与 Zed 对 zed.exe 的
    // 处置一致：把主线程栈抬到 8MB。只影响链接参数，不动依赖。
    println!("cargo:rustc-link-arg-bin=gpui-pi=/STACK:8388608");
}

fn find_resource_compiler() -> PathBuf {
    if let Some(path) = env::var_os("RC") {
        return PathBuf::from(path);
    }

    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return PathBuf::from("rc.exe");
    }

    let program_files = env::var_os("ProgramFiles(x86)")
        .or_else(|| env::var_os("ProgramFiles"))
        .expect("无法定位 Program Files");
    let bin_root = Path::new(&program_files).join("Windows Kits/10/bin");
    // rc.exe 是构建机工具，交叉编译时也必须选择 host 架构版本。
    let host = env::var("HOST").unwrap_or_default();
    let architecture = if host.starts_with("aarch64") {
        "arm64"
    } else if host.starts_with("i586") || host.starts_with("i686") {
        "x86"
    } else {
        "x64"
    };

    let mut candidates = fs::read_dir(&bin_root)
        .unwrap_or_else(|error| panic!("无法读取 {}：{error}", bin_root.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let compiler = path.join(architecture).join("rc.exe");
            compiler
                .is_file()
                .then(|| (sdk_version_key(&path), compiler))
        })
        .collect::<Vec<_>>();

    let legacy_compiler = bin_root.join(architecture).join("rc.exe");
    if legacy_compiler.is_file() {
        candidates.push((Vec::new(), legacy_compiler));
    }

    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    candidates
        .pop()
        .map(|(_, path)| path)
        .unwrap_or_else(|| panic!("Windows SDK 中未找到 {architecture}/rc.exe"))
}

fn sdk_version_key(path: &Path) -> Vec<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|version| {
            version
                .split('.')
                .map(|part| part.parse::<u32>().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}
