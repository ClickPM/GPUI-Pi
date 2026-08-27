//! **只读**解析子代理执行内核（`pi-subagents-lite`）的并发配置。
//!
//! 内核没有任何环境变量或命令行入口，配置只来自两个文件：全局
//! `~/.pi/agent/subagents-lite.json` 与受信任项目的 `<cwd>/.pi/subagents-lite.json`，
//! 项目层覆盖全局层。
//!
//! **这里只读，永不写**。红线 5 要求「能只读就只读」，而这两个文件都是用户自己的
//! pi 配置：全局那份与终端 pi、pi-web-desktop 共享，项目那份会出现在用户仓库的
//! `git status` 里。Manager 替用户改写它们，等于在没被要求的情况下改掉用户的持久配置。
//! 因此本模块的用途是**知道上限是多少**（据此给 Runtime 留内存余量、在 UI 上如实展示），
//! 而不是替用户设定上限 —— 与立项文档 § 三「R26 勘误」里那句「配额只能是配置式上限，
//! 不得表述为派发式调度」是同一件事的两面。

use std::path::Path;

/// 内核内置的默认并发（`DEFAULT_CONCURRENCY = { default: 4 }`）。
pub const DEFAULT_SUBAGENT_CONCURRENCY: u32 = 4;

/// 单次读取能接受的配置文件大小上限。
///
/// 配置文件正常只有几百字节；设一个上限是防止把一个被误写成日志的巨大文件整个读进内存。
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

const CONFIG_FILE_NAME: &str = "subagents-lite.json";

/// 生效的子代理并发上限。
///
/// `agent_dir` 是 pi 的用户数据目录（通常是 `~/.pi/agent`），由调用方解析后传入 ——
/// 与 `pi_data::config` 的一整套 `read_*(agent_dir, ..)` 同一个约定，home 目录归属
/// 保持在应用层一处，这个 crate 就不必再引一份 `dirs`。
///
/// `cwd` 为 `None`（或项目层缺失）时只看全局层；两层都没有就落到内核默认值。
/// 任何一层读不动、不是合法 JSON、或 `concurrency.default` 不是正整数，都**当作该层
/// 未配置**继续往下落，而不是报错 —— 这是展示与预留余量用的参考值，不该因为用户
/// 手改坏了一个配置文件就开不出会话。
#[must_use]
pub fn effective_concurrency(agent_dir: Option<&Path>, cwd: Option<&Path>) -> u32 {
    let global = agent_dir.map(|dir| dir.join(CONFIG_FILE_NAME));
    let project = cwd.map(|dir| dir.join(".pi").join(CONFIG_FILE_NAME));
    effective_concurrency_from(global.as_deref(), project.as_deref())
}

fn effective_concurrency_from(global: Option<&Path>, project: Option<&Path>) -> u32 {
    project
        .and_then(read_default_concurrency)
        .or_else(|| global.and_then(read_default_concurrency))
        .unwrap_or(DEFAULT_SUBAGENT_CONCURRENCY)
}

fn read_default_concurrency(path: &Path) -> Option<u32> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let raw = value.get("concurrency")?.get("default")?.as_u64()?;
    u32::try_from(raw).ok().filter(|slots| *slots > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn falls_back_to_the_kernel_default_when_neither_layer_configures_it() {
        assert_eq!(effective_concurrency_from(None, None), 4);
        assert_eq!(
            effective_concurrency_from(Some(Path::new("nope.json")), None),
            4
        );
    }

    #[test]
    fn the_project_layer_overrides_the_global_layer() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.json",
            r#"{"concurrency":{"default":2}}"#,
        );
        let project = write(
            dir.path(),
            "project.json",
            r#"{"concurrency":{"default":6}}"#,
        );
        assert_eq!(effective_concurrency_from(Some(&global), Some(&project)), 6);
        assert_eq!(effective_concurrency_from(Some(&global), None), 2);
    }

    #[test]
    fn a_broken_layer_falls_through_instead_of_failing() {
        // 用户手改坏配置不该让会话开不出来，只该让这一层"当没配"。
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.json",
            r#"{"concurrency":{"default":3}}"#,
        );
        for (name, body) in [
            ("bad-json.json", "{ not json"),
            ("wrong-shape.json", r#"{"concurrency":"lots"}"#),
            ("zero.json", r#"{"concurrency":{"default":0}}"#),
            ("negative.json", r#"{"concurrency":{"default":-1}}"#),
            ("missing-key.json", r#"{"agent":{}}"#),
        ] {
            let project = write(dir.path(), name, body);
            assert_eq!(
                effective_concurrency_from(Some(&global), Some(&project)),
                3,
                "{name} 应当落回全局层"
            );
        }
    }

    #[test]
    fn an_absurdly_large_config_file_is_refused_rather_than_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.json");
        let filler = "x".repeat(usize::try_from(MAX_CONFIG_BYTES).unwrap() + 1);
        std::fs::write(
            &path,
            format!(r#"{{"pad":"{filler}","concurrency":{{"default":9}}}}"#),
        )
        .unwrap();
        assert_eq!(effective_concurrency_from(None, Some(&path)), 4);
    }

    #[test]
    fn reading_the_config_never_creates_it() {
        // 红线 5：只读。这条钉住"读一次不会把文件或目录顺手建出来"。
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(".pi").join(CONFIG_FILE_NAME);
        assert_eq!(read_default_concurrency(&missing), None);
        assert!(!dir.path().join(".pi").exists());
    }
}
