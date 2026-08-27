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

/// 预留余量时假设的「未显式配置的模型池」个数。
///
/// 内核的 `getSlot(modelKey)` 优先级是 per-model 槽 > per-provider 共享槽 >
/// **给这个 modelKey 新建一个 `default` 上限的槽**。也就是说每多用一个未配置的模型，
/// 就多出一整个 `default` 并发池 —— 只按 `concurrency.default` 预留会成倍少算。
/// 真实上界取决于会话里实际用到多少种模型，**在 Runtime 创建时不可知**，所以这里取一个
/// 有依据的保守值：一次委派里同时用到 4 种以上不同模型已经很罕见。
const ASSUMED_UNCONFIGURED_MODEL_POOLS: u32 = 4;

/// 预留槽数的硬上限。
///
/// 上一条是启发式，必须有个封顶：否则一份把 `concurrency.models` 写了几十条的配置，
/// 会让 Job Object 的内存上限被推到形同虚设，等于把 R25 的兜底关掉。
const MAX_RESERVED_SLOTS: u32 = 16;

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
        .and_then(read_concurrency)
        .or_else(|| global.and_then(read_concurrency))
        .unwrap_or_else(Concurrency::kernel_default)
        .reserved_slots()
}

/// 一份配置里的并发设定。
///
/// 内核的槽是**互斥**的（每个子代理只占一个），所以"同时最多几个"等于所有已存在的槽
/// 上限之和。难点在于槽的**个数**不可知：未配置的模型每用一个就新建一个 `default` 槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Concurrency {
    /// `concurrency.default`。
    default_limit: u32,
    /// `concurrency.providers` 与 `concurrency.models` 里显式配置的上限之和。
    configured_total: u32,
}

impl Concurrency {
    const fn kernel_default() -> Self {
        Self {
            default_limit: DEFAULT_SUBAGENT_CONCURRENCY,
            configured_total: 0,
        }
    }

    /// 预留内存余量时使用的槽数。
    ///
    /// **这是启发式，不是精确上界**，而且精确上界本来就不可导出：
    /// 槽数随会话里用到的模型种类增长，且没有 `modelKey` 的子代理压根不占任何槽
    /// （内核 `spawn()` 里 `if (options.modelKey)` 之外没有别的限流）。
    /// 因此这个数只负责把 Job Object 的内存上限抬到"正常并行委派不会误伤父会话"的量级；
    /// 真正的失控由 R25 的进程树硬限与系统内存水位兜底。
    fn reserved_slots(self) -> u32 {
        self.default_limit
            .saturating_mul(ASSUMED_UNCONFIGURED_MODEL_POOLS)
            .saturating_add(self.configured_total)
            .clamp(1, MAX_RESERVED_SLOTS)
    }
}

fn read_concurrency(path: &Path) -> Option<Concurrency> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let concurrency = value.get("concurrency")?;
    let default_limit = concurrency
        .get("default")
        .and_then(serde_json::Value::as_u64)
        .and_then(|raw| u32::try_from(raw).ok())
        .filter(|slots| *slots > 0)?;
    Some(Concurrency {
        default_limit,
        configured_total: sum_limits(concurrency.get("providers"))
            .saturating_add(sum_limits(concurrency.get("models"))),
    })
}

/// 把 `{ "<key>": <limit> }` 这样一张表里的上限加起来；非法项按 0 计。
fn sum_limits(table: Option<&serde_json::Value>) -> u32 {
    table
        .and_then(serde_json::Value::as_object)
        .map_or(0, |entries| {
            entries
                .values()
                .filter_map(serde_json::Value::as_u64)
                .filter_map(|raw| u32::try_from(raw).ok())
                .fold(0_u32, u32::saturating_add)
        })
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

    /// 内核默认 `{ default: 4 }` 对应的预留槽数。
    const DEFAULT_RESERVED: u32 = DEFAULT_SUBAGENT_CONCURRENCY * ASSUMED_UNCONFIGURED_MODEL_POOLS;

    #[test]
    fn falls_back_to_the_kernel_default_when_neither_layer_configures_it() {
        assert_eq!(effective_concurrency_from(None, None), DEFAULT_RESERVED);
        assert_eq!(
            effective_concurrency_from(Some(Path::new("nope.json")), None),
            DEFAULT_RESERVED
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
            r#"{"concurrency":{"default":3}}"#,
        );
        assert_eq!(
            effective_concurrency_from(Some(&global), Some(&project)),
            12
        );
        assert_eq!(effective_concurrency_from(Some(&global), None), 8);
    }

    #[test]
    fn per_provider_and_per_model_limits_add_their_own_pools() {
        // 内核 getSlot() 的槽是互斥的，但**每个**配置项都是一个独立池：
        // 只看 concurrency.default 会成倍少算同时在跑的子代理数，
        // 从而把 Job Object 的内存余量留得不够，正常并行委派就会误伤父会话。
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "multi.json",
            r#"{"concurrency":{"default":1,"providers":{"anthropic":2},"models":{"anthropic/x":3}}}"#,
        );
        // 1 * 4（未配置模型池的保守假设）+ 2 + 3
        assert_eq!(effective_concurrency_from(None, Some(&path)), 9);
    }

    #[test]
    fn a_pathological_config_cannot_push_the_reservation_past_the_cap() {
        // 没有封顶的话，一份写了几十条 models 的配置会把 Job Object 的内存上限
        // 抬到形同虚设，等于把 R25 的兜底关掉。
        let dir = tempfile::tempdir().unwrap();
        let models = (0..50)
            .map(|index| format!(r#""p/m{index}":99"#))
            .collect::<Vec<_>>()
            .join(",");
        let path = write(
            dir.path(),
            "huge-concurrency.json",
            &format!(r#"{{"concurrency":{{"default":99,"models":{{{models}}}}}}}"#),
        );
        assert_eq!(
            effective_concurrency_from(None, Some(&path)),
            MAX_RESERVED_SLOTS
        );
    }

    #[test]
    fn reserved_slots_never_collapse_to_zero() {
        // 0 会让 job_limits_with_subagent_slots 完全不加余量，静默退回没有子代理的口径。
        assert_eq!(
            Concurrency {
                default_limit: 0,
                configured_total: 0,
            }
            .reserved_slots(),
            1
        );
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
                12,
                "{name} 应当落回全局层"
            );
        }
    }

    #[test]
    fn malformed_entries_inside_the_limit_tables_count_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "junk-entries.json",
            r#"{"concurrency":{"default":1,"providers":{"a":"lots","b":2},"models":"nope"}}"#,
        );
        assert_eq!(effective_concurrency_from(None, Some(&path)), 6);
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
        assert_eq!(
            effective_concurrency_from(None, Some(&path)),
            DEFAULT_RESERVED
        );
    }

    #[test]
    fn reading_the_config_never_creates_it() {
        // 红线 5：只读。这条钉住"读一次不会把文件或目录顺手建出来"。
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(".pi").join(CONFIG_FILE_NAME);
        assert_eq!(read_concurrency(&missing), None);
        assert!(!dir.path().join(".pi").exists());
    }
}
