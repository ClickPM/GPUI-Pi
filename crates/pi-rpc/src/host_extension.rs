//! GPUI-Pi 随包携带的 host 扩展落盘。
//!
//! 目前两份：
//! - `project-command-environment.ts` —— R15 项目命令环境（始终注入）；
//! - `writer-isolation.ts` —— R27 mutating worktree 强制（仅在加载子代理内核时注入）。

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const PROJECT_COMMAND_SOURCE: &str = include_str!("../assets/project-command-environment.ts");
const WRITER_ISOLATION_SOURCE: &str = include_str!("../assets/writer-isolation.ts");
const HOST_EXTENSION_VERSION: &str = env!("CARGO_PKG_VERSION");
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 将项目命令环境扩展落到内容寻址的临时目录，供 `pi -e` 加载。
pub fn materialize_host_extension() -> io::Result<PathBuf> {
    materialize_bundled_extension_in(
        &std::env::temp_dir(),
        "project-command-environment",
        PROJECT_COMMAND_SOURCE,
    )
}

/// 将 R27 writer 隔离扩展落到临时目录。
pub fn materialize_writer_isolation_extension() -> io::Result<PathBuf> {
    materialize_bundled_extension_in(
        &std::env::temp_dir(),
        "writer-isolation",
        WRITER_ISOLATION_SOURCE,
    )
}

fn materialize_bundled_extension_in(
    temp_root: &Path,
    stem: &str,
    source: &str,
) -> io::Result<PathBuf> {
    let digest = content_digest(source.as_bytes());
    let directory = temp_root
        .join("gpui-pi")
        .join("host-extensions")
        .join(format!("v{HOST_EXTENSION_VERSION}-{stem}-{digest:016x}"));
    fs::create_dir_all(&directory)?;
    let file_name = format!("{stem}.ts");
    let target = directory.join(&file_name);

    match fs::read(&target) {
        Ok(existing) if existing == source.as_bytes() => return Ok(target),
        Ok(_) => {
            return existing_fallback(&directory, stem, source)?
                .map_or_else(|| write_unique_fallback(&directory, stem, source), Ok);
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }

    match write_atomic(&directory, &target, source) {
        Ok(()) => Ok(target),
        Err(error) if target.is_file() => {
            let existing = fs::read(&target)?;
            if existing == source.as_bytes() {
                Ok(target)
            } else {
                existing_fallback(&directory, stem, source)?.map_or_else(
                    || write_unique_fallback(&directory, stem, source),
                    Ok,
                )
                .map_err(|fallback_error| {
                    io::Error::new(
                        fallback_error.kind(),
                        format!(
                            "host extension 并发落盘后内容不一致（原错误：{error}；fallback 错误：{fallback_error}）：{}",
                            target.display()
                        ),
                    )
                })
            }
        }
        Err(error) => Err(error),
    }
}

fn existing_fallback(directory: &Path, stem: &str, source: &str) -> io::Result<Option<PathBuf>> {
    let prefix = format!("{stem}.");
    let mut candidates = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".ts"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    for candidate in candidates {
        let Ok(content) = fs::read(&candidate) else {
            continue;
        };
        if content == source.as_bytes() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn write_unique_fallback(directory: &Path, stem: &str, source: &str) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let target = directory.join(format!("{stem}.{}.{}.ts", std::process::id(), sequence));
        match write_atomic(directory, &target, source) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "无法分配唯一 host extension fallback 文件",
    ))
}

fn write_atomic(directory: &Path, target: &Path, source: &str) -> io::Result<()> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "host extension 文件名不是 UTF-8",
            )
        })?;
    let temporary = directory.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(source.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, target)?;
        if fs::read(target)? != source.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("host extension 落盘校验失败：{}", target.display()),
            ));
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn content_digest(content: &[u8]) -> u64 {
    // 固定 FNV-1a，避免 DefaultHasher 实现变化导致缓存路径无谓漂移。
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in content {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_embedded_source_intact_and_stably() {
        let temp = tempfile::tempdir().unwrap();
        let first = materialize_bundled_extension_in(
            temp.path(),
            "project-command-environment",
            PROJECT_COMMAND_SOURCE,
        )
        .unwrap();
        let second = materialize_bundled_extension_in(
            temp.path(),
            "project-command-environment",
            PROJECT_COMMAND_SOURCE,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read_to_string(first).unwrap(), PROJECT_COMMAND_SOURCE);
    }

    #[test]
    fn corrupted_content_addressed_target_uses_a_verified_unique_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let target = materialize_bundled_extension_in(
            temp.path(),
            "project-command-environment",
            PROJECT_COMMAND_SOURCE,
        )
        .unwrap();
        fs::write(&target, "corrupt").unwrap();
        let fallback = materialize_bundled_extension_in(
            temp.path(),
            "project-command-environment",
            PROJECT_COMMAND_SOURCE,
        )
        .unwrap();
        let reused = materialize_bundled_extension_in(
            temp.path(),
            "project-command-environment",
            PROJECT_COMMAND_SOURCE,
        )
        .unwrap();
        assert_ne!(fallback, target);
        assert_eq!(fallback, reused);
        assert_eq!(fs::read_to_string(&target).unwrap(), "corrupt");
        assert_eq!(
            fs::read_to_string(fallback).unwrap(),
            PROJECT_COMMAND_SOURCE
        );
    }

    #[test]
    fn fallback_scan_skips_an_unreadable_shape_and_reuses_a_valid_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("extensions");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("project-command-environment.0.ts")).unwrap();
        let valid = directory.join("project-command-environment.1.ts");
        fs::write(&valid, PROJECT_COMMAND_SOURCE).unwrap();
        assert_eq!(
            existing_fallback(
                &directory,
                "project-command-environment",
                PROJECT_COMMAND_SOURCE
            )
            .unwrap(),
            Some(valid)
        );
    }

    #[test]
    fn source_contains_required_host_environment_contract() {
        for needle in [
            "session_start",
            "resources_discover",
            "user_bash",
            "<builtin:bash>",
            "HOST_EXTENSION_PATH",
            "hostRegistered",
            "PORT",
            "NODE_ENV",
            "NEXT_",
            "getShellPath",
            "getShellCommandPrefix",
        ] {
            assert!(PROJECT_COMMAND_SOURCE.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn writer_isolation_source_enforces_agent_tool_call_contract() {
        for needle in [
            "tool_call",
            "tool_result",
            "Agent",
            "worktree_path",
            "allocateWriterWorktree",
            "block: true",
            "explore",
            "gpui-pi/writer-",
            // 后台 Agent：租约挂到 agentId，避免 tool_result 立刻释放导致并发 writer
            "leaseByAgentId",
            "run_in_background",
            "subagent-result",
        ] {
            assert!(
                WRITER_ISOLATION_SOURCE.contains(needle),
                "writer-isolation missing {needle}"
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let path = materialize_bundled_extension_in(
            temp.path(),
            "writer-isolation",
            WRITER_ISOLATION_SOURCE,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), WRITER_ISOLATION_SOURCE);
    }
}
