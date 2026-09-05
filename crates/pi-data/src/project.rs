//! 会话的项目 identity 与 linked worktree 归并。
//!
//! 只折叠 checkout 顶层的 linked worktree；仓库子目录继续作为独立 cwd，保持
//! 与 pi-web 0.8.9 一致的新会话落点语义。

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{fs_util::hide_console_window, session::SessionSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPlatform {
    Windows,
    Unix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInfo {
    pub project_root: PathBuf,
    pub project_key: String,
    pub branch: Option<String>,
    pub is_worktree: bool,
    pub is_top_level: bool,
}

#[derive(Debug, Clone)]
pub struct GroupedSession {
    pub session: SessionSummary,
    pub project: ProjectInfo,
}

#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub key: String,
    pub root: PathBuf,
    pub sessions: Vec<GroupedSession>,
}

pub fn native_platform() -> PathPlatform {
    if cfg!(windows) {
        PathPlatform::Windows
    } else {
        PathPlatform::Unix
    }
}

pub fn project_identity_key(path: impl AsRef<Path>) -> String {
    project_identity_key_for(path.as_ref(), native_platform())
}

pub fn project_identity_key_for(path: &Path, platform: PathPlatform) -> String {
    let text = path.as_os_str().to_string_lossy();
    match platform {
        PathPlatform::Windows => windows_path_key(&text),
        PathPlatform::Unix => normalize_unix_key(path),
    }
}

pub fn resolve_project(cwd: impl AsRef<Path>) -> ProjectInfo {
    resolve_project_with_git(cwd.as_ref(), OsStr::new("git"))
}

pub fn group_sessions(sessions: impl IntoIterator<Item = SessionSummary>) -> Vec<ProjectGroup> {
    group_sessions_with(sessions, |cwd| resolve_project(cwd))
}

fn group_sessions_with(
    sessions: impl IntoIterator<Item = SessionSummary>,
    mut resolver: impl FnMut(&Path) -> ProjectInfo,
) -> Vec<ProjectGroup> {
    let mut groups = BTreeMap::<String, ProjectGroup>::new();
    let mut projects = BTreeMap::<String, ProjectInfo>::new();
    for session in sessions {
        let cwd_key = project_identity_key(canonical_or_self(&session.cwd));
        let project = projects
            .entry(cwd_key)
            .or_insert_with(|| resolver(&session.cwd))
            .clone();
        groups
            .entry(project.project_key.clone())
            .or_insert_with(|| ProjectGroup {
                key: project.project_key.clone(),
                root: project.project_root.clone(),
                sessions: Vec::new(),
            })
            .sessions
            .push(GroupedSession { session, project });
    }
    let mut groups: Vec<ProjectGroup> = groups.into_values().collect();
    for group in &mut groups {
        group
            .sessions
            .sort_by_key(|item| std::cmp::Reverse(item.session.modified));
    }
    groups.sort_by(|left, right| {
        let left_time = left.sessions.first().map(|item| item.session.modified);
        let right_time = right.sessions.first().map(|item| item.session.modified);
        right_time.cmp(&left_time)
    });
    groups
}

fn resolve_project_with_git(cwd: &Path, git_binary: &OsStr) -> ProjectInfo {
    let fallback = || ProjectInfo {
        project_root: cwd.to_path_buf(),
        project_key: project_identity_key(cwd),
        branch: None,
        is_worktree: false,
        is_top_level: false,
    };
    if !cwd.exists() {
        return fallback();
    }

    let output = match hide_console_window(&mut Command::new(git_binary))
        .arg("-C")
        .arg(cwd)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--git-dir",
            "--show-toplevel",
            "--abbrev-ref",
            "HEAD",
        ])
        .env("LC_ALL", "C")
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return fallback(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let Some(common_dir) = lines.next().map(PathBuf::from) else {
        return fallback();
    };
    let Some(git_dir) = lines.next().map(PathBuf::from) else {
        return fallback();
    };
    let Some(toplevel) = lines.next().map(PathBuf::from) else {
        return fallback();
    };
    let branch = lines
        .next()
        .map(str::trim)
        .filter(|branch| !branch.is_empty() && *branch != "HEAD")
        .map(str::to_owned);

    let real_cwd = canonical_or_self(cwd);
    let real_toplevel = canonical_or_self(&toplevel);
    let real_common = canonical_or_self(&common_dir);
    let real_git = canonical_or_self(&git_dir);
    let is_top_level = same_path(&real_cwd, &real_toplevel);
    let is_worktree = is_top_level && !same_path(&real_common, &real_git);
    let project_root = if is_worktree {
        real_common
            .parent()
            .map_or_else(|| real_toplevel.clone(), canonical_or_self)
    } else if is_top_level {
        real_toplevel
    } else {
        cwd.to_path_buf()
    };
    ProjectInfo {
        project_key: project_identity_key(&project_root),
        project_root,
        branch,
        is_worktree,
        is_top_level,
    }
}

fn canonical_or_self(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_path(left: &Path, right: &Path) -> bool {
    project_identity_key(left) == project_identity_key(right)
}

fn windows_path_key(path: &str) -> String {
    let mut path = path.replace('/', "\\");
    let prefix = if path.starts_with("\\\\") { "\\\\" } else { "" };
    let mut parts = Vec::new();
    for part in path.trim_start_matches('\\').split('\\') {
        match part {
            "" | "." => {}
            ".." if !parts.is_empty() => {
                parts.pop();
            }
            ".." => parts.push(".."),
            _ => parts.push(part),
        }
    }
    path = format!("{prefix}{}", parts.join("\\"));
    while path.ends_with('\\') && !is_windows_root(&path) {
        path.pop();
    }
    path.to_lowercase()
}

fn is_windows_root(path: &str) -> bool {
    (path.len() == 3 && path.as_bytes().get(1) == Some(&b':'))
        || (path.starts_with("\\\\") && path.matches('\\').count() <= 3)
}

fn normalize_unix_key(path: &Path) -> String {
    // 测试可能在 Windows 主机上验证 Unix 语义，因此不能借用宿主 `Path::components()`；
    // Unix identity 只把 `/` 当分隔符，反斜杠必须保留为普通字符。
    let text = path.as_os_str().to_string_lossy();
    let absolute = text.starts_with('/');
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." if !parts.is_empty() => {
                parts.pop();
            }
            ".." => parts.push(".."),
            _ => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if absolute {
        if joined.is_empty() {
            "/".to_owned()
        } else {
            format!("/{joined}")
        }
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_identity_ignores_case_separator_and_dot() {
        let expected = project_identity_key_for(
            Path::new(r"C:\Users\Alex\Project\Study\ELM"),
            PathPlatform::Windows,
        );
        assert_eq!(
            project_identity_key_for(
                Path::new("c:/users/ALEX/project/study/elm"),
                PathPlatform::Windows
            ),
            expected
        );
        assert_eq!(
            project_identity_key_for(
                Path::new(r"c:\Users\Alex\Project\Study\.\ELM\"),
                PathPlatform::Windows
            ),
            expected
        );
    }

    #[test]
    fn group_sessions_resolves_each_normalized_cwd_once() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let first = SessionSummary {
            path: PathBuf::from("first.jsonl"),
            revision: crate::SessionRevision {
                len: 0,
                modified: std::time::SystemTime::UNIX_EPOCH,
                fingerprint: 0,
            },
            id: "first".to_owned(),
            cwd: cwd.clone(),
            name: None,
            created: std::time::SystemTime::UNIX_EPOCH,
            modified: std::time::SystemTime::UNIX_EPOCH,
            message_count: 0,
            first_message: String::new(),
            parent_session_path: None,
            parent_session_id: None,
            metrics: crate::SessionMetrics::default(),
        };
        let mut second = first.clone();
        second.id = "second".to_owned();
        second.path = PathBuf::from("second.jsonl");
        let mut calls = 0;
        let groups = group_sessions_with([first, second], |cwd| {
            calls += 1;
            ProjectInfo {
                project_root: cwd.to_path_buf(),
                project_key: project_identity_key(cwd),
                branch: None,
                is_worktree: false,
                is_top_level: false,
            }
        });
        assert_eq!(calls, 1);
        assert_eq!(groups[0].sessions.len(), 2);
    }

    #[test]
    fn unix_identity_preserves_case_and_backslash() {
        assert_ne!(
            project_identity_key_for(Path::new("/Users/Alex"), PathPlatform::Unix),
            project_identity_key_for(Path::new("/users/alex"), PathPlatform::Unix)
        );
        assert_ne!(
            project_identity_key_for(Path::new(r"/a\b"), PathPlatform::Unix),
            project_identity_key_for(Path::new("/a/b"), PathPlatform::Unix)
        );
    }
}
