//! **只读**解析子代理执行内核（`pi-subagents-lite`）的并发配置。
//!
//! 内核没有任何环境变量或命令行入口，配置只来自两个文件：全局
//! `~/.pi/agent/subagents-lite.json` 与**受信任**项目的 `<cwd>/.pi/subagents-lite.json`。
//!
//! **这里只读，永不写**。红线 5 要求「能只读就只读」，而这两个文件都是用户自己的
//! pi 配置：全局那份与终端 pi、pi-web-desktop 共享，项目那份会出现在用户仓库的
//! `git status` 里。Manager 替用户改写它们，等于在没被要求的情况下改掉用户的持久配置。
//! 因此本模块的用途是**知道上限是多少**（据此给 Runtime 留内存余量），
//! 而不是替用户设定上限 —— 与立项文档 § 三「R26 勘误」里那句「配额只能是配置式上限，
//! 不得表述为派发式调度」是同一件事的两面。

use std::collections::BTreeMap;
use std::path::Path;

/// 内核内置的默认并发（`DEFAULT_CONCURRENCY = { default: 4 }`）。
pub const DEFAULT_SUBAGENT_CONCURRENCY: u32 = 4;

/// 预留余量时假设的「未显式配置的模型池」个数。
///
/// 内核的 `getSlot(modelKey)` 优先级是 per-model 槽 > per-provider 共享槽 >
/// **给这个 modelKey 新建一个 `default` 上限的槽**。也就是说每多用一个未配置的模型，
/// 就多出一整个 `default` 并发池 —— 只按 `concurrency.default` 预留会成倍少算。
/// 真实上界取决于会话里实际用到多少种模型，**在 Runtime 创建时不可知**，所以这里取
/// 一个保守值：一次委派里同时用到 2 种以上不同模型已经不常见。
///
/// 取 2 而不是更大，是为了给 `configured_total` 留出可观察空间：取 4 时默认配置
/// （`default = 4`）算出的 16 正好顶满 [`MAX_RESERVED_SLOTS`]，显式配置的
/// providers / models 就永远被 clamp 吞掉，那套求和逻辑等于白写。
const ASSUMED_UNCONFIGURED_MODEL_POOLS: u32 = 2;

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

/// 生效的子代理并发上限（预留内存余量用的槽数）。
///
/// `agent_dir` 是 pi 的用户数据目录（通常是 `~/.pi/agent`），由调用方解析后传入 ——
/// 与 `pi_data::config` 的一整套 `read_*(agent_dir, ..)` 同一个约定，home 目录归属
/// 保持在应用层一处，这个 crate 就不必再引一份 `dirs`。
///
/// **项目层受 trust 门禁**：内核只在 `ctx.isProjectTrusted()` 为真时加载
/// `<cwd>/.pi/subagents-lite.json`（`src/events.ts:86-89`），未信任的项目那一层
/// 根本不会生效。这里跟着门禁走，否则一个没被 trust 的仓库只要往自己的 `.pi/` 里
/// 写一份配置，就能影响宿主给它开多大的内存上限 —— 仓库内容不该有这个能力。
#[must_use]
pub fn effective_concurrency(agent_dir: Option<&Path>, cwd: Option<&Path>) -> u32 {
    let global = agent_dir.map(|dir| dir.join(CONFIG_FILE_NAME));
    let project = cwd
        .filter(|cwd| project_layer_is_trusted(agent_dir, cwd))
        .map(|dir| dir.join(".pi").join(CONFIG_FILE_NAME));
    effective_concurrency_from(global.as_deref(), project.as_deref())
}

/// 项目层配置是否可信。
///
/// 读不出 trust 状态时**按不可信处理**：宁可少留一点余量（Job Object 上限低一档，
/// 极端情况下子代理会先撞上限），也不让一个来路不明的仓库把宿主的内存上限抬上去。
fn project_layer_is_trusted(agent_dir: Option<&Path>, cwd: &Path) -> bool {
    let Some(agent_dir) = agent_dir else {
        return false;
    };
    pi_data::read_project_trust_status(agent_dir, cwd, None)
        .is_ok_and(|status| !status.requires_trust || status.trusted)
}

/// 按内核 `mergeRawConcurrency` 的语义合并两层，再算出预留槽数。
///
/// 顺序即优先级从低到高。
fn effective_concurrency_from(global: Option<&Path>, project: Option<&Path>) -> u32 {
    let mut merged = RawConcurrency::default();
    for layer in [global, project] {
        if let Some(raw) = layer.and_then(read_raw_concurrency) {
            merged.overlay(raw);
        }
    }
    merged.reserved_slots()
}

/// 一层（或合并后）的原始并发设定。
///
/// 字段划分逐字对应内核 `src/config/config-io.ts` 的 `mergeRawConcurrency`：
///
/// ```text
/// for (const layer of layers) {
///   if (layer.default !== undefined) out.default = layer.default;  // 覆盖
///   Object.assign(providers, layer.providers ?? {});               // 并集
///   Object.assign(models,    layer.models    ?? {});               // 并集
/// }
/// ```
///
/// 也就是说**按键合并，不是整层二选一**：项目层只写 `models` 时，全局层的 `default`
/// 照样生效；项目层写了 `default` 时，全局层的 `models` 也不会被丢掉。
/// R26 第一版把它实现成整层择一，会在两层各写一半配置时成倍少算并发槽。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RawConcurrency {
    default_limit: Option<u32>,
    providers: BTreeMap<String, u32>,
    models: BTreeMap<String, u32>,
}

impl RawConcurrency {
    /// 把更高优先级的一层盖上来。
    fn overlay(&mut self, other: Self) {
        if other.default_limit.is_some() {
            self.default_limit = other.default_limit;
        }
        self.providers.extend(other.providers);
        self.models.extend(other.models);
    }

    /// 预留内存余量时使用的槽数。
    ///
    /// **这是启发式，不是精确上界**，而且精确上界本来就不可导出：
    /// 槽数随会话里用到的模型种类增长，且没有 `modelKey` 的子代理压根不占任何槽
    /// （内核 `spawn()` 里 `if (options.modelKey)` 之外没有别的限流）。
    /// 因此这个数只负责把 Job Object 的内存上限抬到"正常并行委派不会误伤父会话"的量级；
    /// 真正的失控由 R25 的进程树硬限与系统内存水位兜底。
    ///
    /// per-model 槽与 per-provider 槽是两组可同时活跃的池（前者优先命中，后者兜住该
    /// provider 下其余模型），所以两边求和是往安全方向偏，不是重复计算。
    fn reserved_slots(&self) -> u32 {
        let configured: u32 = self
            .providers
            .values()
            .chain(self.models.values())
            .fold(0_u32, |sum, limit| sum.saturating_add(*limit));
        self.default_limit
            // 0 不是合法上限。解析层已经滤过一道，这里再兜一次：
            // 让它落回内核默认，而不是把整份预留 clamp 成 1 槽。
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_SUBAGENT_CONCURRENCY)
            .saturating_mul(ASSUMED_UNCONFIGURED_MODEL_POOLS)
            .saturating_add(configured)
            .clamp(1, MAX_RESERVED_SLOTS)
    }
}

/// 读一层配置。
///
/// 缺 `concurrency.default` **不再让整层失效** —— 内核 `mergeDefaults` 会把内置的 4
/// 补上（`config-io.ts:231`），一层只写 `models` 是完全合法的配置。
fn read_raw_concurrency(path: &Path) -> Option<RawConcurrency> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let concurrency = value.get("concurrency")?;
    Some(RawConcurrency {
        default_limit: concurrency
            .get("default")
            .and_then(serde_json::Value::as_u64)
            .and_then(|raw| u32::try_from(raw).ok())
            .filter(|slots| *slots > 0),
        providers: read_limits(concurrency.get("providers")),
        models: read_limits(concurrency.get("models")),
    })
}

/// 把 `{ "<key>": <limit> }` 这样一张表读成映射；非法项跳过。
fn read_limits(table: Option<&serde_json::Value>) -> BTreeMap<String, u32> {
    table
        .and_then(serde_json::Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    let limit = value.as_u64().and_then(|raw| u32::try_from(raw).ok())?;
                    (limit > 0).then(|| (key.clone(), limit))
                })
                .collect()
        })
        .unwrap_or_default()
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
    fn layers_merge_per_key_the_way_the_kernel_does() {
        // 内核 mergeRawConcurrency：default 后层覆盖，providers / models 取并集。
        // R26 第一版实现成「整层二选一」，这条把正确语义钉住。
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.json",
            r#"{"concurrency":{"default":4,"models":{"a/b":6}}}"#,
        );
        let project = write(
            dir.path(),
            "project.json",
            r#"{"concurrency":{"default":1}}"#,
        );
        // default 取项目层的 1，但全局层的 models 必须保住：1*2 + 6 = 8
        assert_eq!(effective_concurrency_from(Some(&global), Some(&project)), 8);
    }

    #[test]
    fn a_layer_without_default_still_contributes_its_limit_tables() {
        // 只写 models 的项目层在内核里完全合法（mergeDefaults 会补上内置的 4）。
        // 旧实现对整层返回 None，把这份配置整个丢掉。
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.json",
            r#"{"concurrency":{"default":2}}"#,
        );
        let project = write(
            dir.path(),
            "project.json",
            r#"{"concurrency":{"models":{"a/b":5}}}"#,
        );
        // default 仍是全局的 2，models 来自项目层：2*2 + 5 = 9
        assert_eq!(effective_concurrency_from(Some(&global), Some(&project)), 9);
    }

    #[test]
    fn same_key_in_both_layers_takes_the_project_value() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.json",
            r#"{"concurrency":{"default":1,"models":{"a/b":9}}}"#,
        );
        let project = write(
            dir.path(),
            "project.json",
            r#"{"concurrency":{"models":{"a/b":2}}}"#,
        );
        // 1*2 + 2（项目层同键覆盖，不是相加）
        assert_eq!(effective_concurrency_from(Some(&global), Some(&project)), 4);
    }

    #[test]
    fn per_provider_and_per_model_limits_both_count() {
        // getSlot 的 per-model 槽与 per-provider 槽是两组可同时活跃的池，
        // 求和是往安全方向偏。
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "multi.json",
            r#"{"concurrency":{"default":1,"providers":{"anthropic":2},"models":{"anthropic/x":3}}}"#,
        );
        assert_eq!(effective_concurrency_from(None, Some(&path)), 2 + 2 + 3);
    }

    #[test]
    fn the_default_configuration_leaves_room_below_the_cap() {
        // 取 ASSUMED_UNCONFIGURED_MODEL_POOLS = 4 时，默认配置算出的 16 正好顶满
        // MAX_RESERVED_SLOTS，显式配置的 providers/models 就永远被 clamp 吞掉。
        // 这条钉住「默认值之上仍有可观察空间」。
        // 用配置实际算一遍，而不是直接比两个常量 —— 后者是编译期恒真式，
        // clippy 会判 `assertions_on_constants`，而且也测不到读取路径。
        let reserved = effective_concurrency_from(None, None);
        assert_eq!(reserved, DEFAULT_RESERVED);
        assert!(
            reserved < MAX_RESERVED_SLOTS,
            "默认预留 {reserved} 不该顶满上限 {MAX_RESERVED_SLOTS}"
        );
    }

    #[test]
    fn a_pathological_config_cannot_push_the_reservation_past_the_cap() {
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
        assert_eq!(RawConcurrency::default().reserved_slots(), DEFAULT_RESERVED);
        assert_eq!(
            RawConcurrency {
                default_limit: Some(0),
                ..RawConcurrency::default()
            }
            .reserved_slots(),
            DEFAULT_RESERVED,
            "0 不是合法上限，应落回内核默认而不是变成 0 槽"
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
                6,
                "{name} 应当落回全局层的 default=3"
            );
        }
    }

    #[test]
    fn malformed_entries_inside_the_limit_tables_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "junk-entries.json",
            r#"{"concurrency":{"default":1,"providers":{"a":"lots","b":2},"models":"nope"}}"#,
        );
        assert_eq!(effective_concurrency_from(None, Some(&path)), 2 + 2);
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
    fn an_untrusted_project_layer_is_ignored() {
        // 内核只在 isProjectTrusted() 为真时加载项目层。跟着门禁走，否则一个没被
        // trust 的仓库只要写一份 .pi/subagents-lite.json 就能抬高宿主的内存上限。
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        // pi 判定「该项目是否需要 trust」看的是 `.pi/settings.json` / `extensions` /
        // `skills` 等资源，**不含** subagents-lite.json —— 只放一份并发配置的项目按
        // pi 自己的规则根本不需要 trust，因而算受信任。要构造未信任场景就得放一个
        // 真正触发 trust 的资源，再让 trust store 里没有它的决定。
        std::fs::write(cwd.join(".pi").join("settings.json"), "{}").unwrap();
        std::fs::write(
            cwd.join(".pi").join(CONFIG_FILE_NAME),
            r#"{"concurrency":{"default":16}}"#,
        )
        .unwrap();
        assert_eq!(
            effective_concurrency(Some(&agent_dir), Some(&cwd)),
            DEFAULT_RESERVED,
            "未信任项目的配置不该生效"
        );
    }

    #[test]
    fn reading_the_config_never_creates_it() {
        // 红线 5：只读。这条钉住"读一次不会把文件或目录顺手建出来"。
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(".pi").join(CONFIG_FILE_NAME);
        assert_eq!(read_raw_concurrency(&missing), None);
        assert!(!dir.path().join(".pi").exists());
    }

    #[test]
    fn a_project_that_does_not_require_trust_still_gets_its_config_applied() {
        // 与上一条互补：没有任何触发 trust 的资源时，pi 视该项目为受信任，
        // 内核会照常加载项目层，我们也必须跟着加载 —— 否则又是一处与上游不一致。
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let cwd = dir.path().join("plain-project");
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        std::fs::write(
            cwd.join(".pi").join(CONFIG_FILE_NAME),
            r#"{"concurrency":{"default":1}}"#,
        )
        .unwrap();
        assert_eq!(
            effective_concurrency(Some(&agent_dir), Some(&cwd)),
            // default=1 -> 1 * ASSUMED_UNCONFIGURED_MODEL_POOLS
            ASSUMED_UNCONFIGURED_MODEL_POOLS
        );
    }
}
