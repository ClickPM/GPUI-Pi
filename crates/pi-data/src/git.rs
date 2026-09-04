//! Git 状态、diff 与 worktree 的纯逻辑层。
//!
//! 所有命令都逐参数传递，固定英文输出，并限制运行时间与输出体积；UI crate
//! 只消费这里的 owned snapshot，不直接启动 Git。

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::{TEXT_PREVIEW_MAX_BYTES, project_identity_key};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_STATUS_MAX_OUTPUT: usize = 8 * 1024 * 1024;
const GIT_DEFAULT_MAX_OUTPUT: usize = 1024 * 1024;
const GIT_DIFF_MAX_OUTPUT: usize = TEXT_PREVIEW_MAX_BYTES as usize * 4;
const GIT_STATUS_MAX_FILES: usize = 500;
const UNTRACKED_STATS_MAX_FILES: usize = 128;
const UNTRACKED_STATS_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFileStatusKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflict,
}

impl GitFileStatusKind {
    pub const fn code(self) -> char {
        match self {
            Self::Modified => 'M',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Untracked => 'U',
            Self::Conflict => 'C',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileStatus {
    pub relative_path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub kind: GitFileStatusKind,
    pub index_status: char,
    pub worktree_status: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusSnapshot {
    pub is_git_repository: bool,
    pub repository_root: Option<PathBuf>,
    /// 路径统一相对于调用方传入的 cwd，而不是仓库根。
    pub files: Vec<GitFileStatus>,
    pub total_files: usize,
    pub files_truncated: bool,
    pub additions: u64,
    pub deletions: u64,
    pub line_stats_truncated: bool,
}

impl GitStatusSnapshot {
    fn non_git() -> Self {
        Self {
            is_git_repository: false,
            repository_root: None,
            files: Vec::new(),
            total_files: 0,
            files_truncated: false,
            additions: 0,
            deletions: 0,
            line_stats_truncated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitDiffUnsupported {
    NoChanges,
    Binary,
    TooLarge,
    NotAFile,
    MissingHunk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitFileDiff {
    Supported {
        kind: GitFileStatusKind,
        patch: String,
    },
    Unsupported(GitDiffUnsupported),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_main: bool,
    pub is_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    pub project_root: PathBuf,
    pub is_top_level: bool,
    pub worktrees: Vec<WorktreeInfo>,
}

#[derive(Debug, Error)]
pub enum GitError {
    #[error("目录不存在或不是目录：{0}")]
    InvalidDirectory(PathBuf),
    #[error("路径不在当前项目范围内：{0}")]
    PathOutsideProject(PathBuf),
    #[error("Git 命令超时：git {0}")]
    Timeout(String),
    #[error("Git 命令输出超过安全上限：git {0}")]
    OutputTooLarge(String),
    #[error("Git 命令失败：git {args}：{message}")]
    CommandFailed { args: String, message: String },
    #[error("启动 Git 失败：{0}")]
    Spawn(#[source] io::Error),
    #[error("读取 Git 输出失败：{0}")]
    ReadOutput(#[source] io::Error),
    #[error("worktree 含目录链接或 reparse point，拒绝移除：{0}")]
    DirectoryLink(PathBuf),
    #[error("不能移除主 worktree")]
    MainWorktree,
    #[error("目标不是当前仓库的 linked worktree：{0}")]
    UnknownWorktree(PathBuf),
    #[error("worktree 有未提交或未跟踪改动")]
    DirtyWorktree,
    #[error("无效分支名：{0}")]
    InvalidBranch(String),
    #[error("worktree 目标目录已存在：{0}")]
    WorktreePathExists(PathBuf),
    #[error("无法确定仓库根目录")]
    MissingRepositoryRoot,
}

#[derive(Debug)]
struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainEntry {
    path: PathBuf,
    original_path: Option<PathBuf>,
    index_status: char,
    worktree_status: char,
}

pub fn git_status(cwd: impl AsRef<Path>) -> Result<GitStatusSnapshot, GitError> {
    let cwd = checked_directory(cwd.as_ref())?;
    let Some(repository_root) = repository_root(&cwd)? else {
        return Ok(GitStatusSnapshot::non_git());
    };
    let (output, status_output_truncated) = status_porcelain(&repository_root)?;
    let entries = parse_porcelain_v1_z(&output);
    let relative_cwd = cwd
        .strip_prefix(&repository_root)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let mut entries = entries
        .into_iter()
        .filter(|entry| {
            relative_cwd.as_os_str().is_empty() || entry.path.starts_with(&relative_cwd)
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let (mut additions, deletions) = tracked_line_stats(&repository_root, &relative_cwd)?;
    let mut stats_files = 0_usize;
    let mut stats_bytes = 0_u64;
    let mut line_stats_truncated = false;
    for entry in &entries {
        if classify_status(entry.index_status, entry.worktree_status)
            != GitFileStatusKind::Untracked
        {
            continue;
        }
        if stats_files >= UNTRACKED_STATS_MAX_FILES
            || stats_bytes >= UNTRACKED_STATS_MAX_TOTAL_BYTES
        {
            line_stats_truncated = true;
            break;
        }
        let (lines, bytes, truncated) = count_text_lines_budgeted(
            &repository_root.join(&entry.path),
            UNTRACKED_STATS_MAX_TOTAL_BYTES.saturating_sub(stats_bytes),
        );
        additions = additions.saturating_add(lines);
        stats_files += 1;
        stats_bytes = stats_bytes.saturating_add(bytes);
        line_stats_truncated |= truncated;
    }
    let total_files = entries.len();
    let files_truncated = status_output_truncated || total_files > GIT_STATUS_MAX_FILES;
    let files = entries
        .into_iter()
        .take(GIT_STATUS_MAX_FILES)
        .map(|entry| GitFileStatus {
            relative_path: strip_cwd_prefix(&entry.path, &relative_cwd),
            original_path: entry
                .original_path
                .map(|path| strip_cwd_prefix(&path, &relative_cwd)),
            kind: classify_status(entry.index_status, entry.worktree_status),
            index_status: entry.index_status,
            worktree_status: entry.worktree_status,
        })
        .collect();
    Ok(GitStatusSnapshot {
        is_git_repository: true,
        repository_root: Some(repository_root),
        files,
        total_files,
        files_truncated,
        additions,
        deletions,
        line_stats_truncated,
    })
}

pub fn git_file_diff(
    cwd: impl AsRef<Path>,
    requested_path: impl AsRef<Path>,
) -> Result<GitFileDiff, GitError> {
    let cwd = checked_directory(cwd.as_ref())?;
    let Some(repository_root) = repository_root(&cwd)? else {
        return Ok(GitFileDiff::Unsupported(GitDiffUnsupported::NoChanges));
    };
    let requested = if requested_path.as_ref().is_absolute() {
        requested_path.as_ref().to_path_buf()
    } else {
        cwd.join(requested_path)
    };
    let requested = normalize_existing_parent(&requested);
    if !path_within(&cwd, &requested) || !path_within(&repository_root, &requested) {
        return Err(GitError::PathOutsideProject(requested));
    }
    let relative = requested
        .strip_prefix(&repository_root)
        .map_err(|_| GitError::PathOutsideProject(requested.clone()))?
        .to_path_buf();
    let (status_output, _) = status_porcelain(&repository_root)?;
    let Some(entry) = parse_porcelain_v1_z(&status_output)
        .into_iter()
        .find(|entry| same_relative_path(&entry.path, &relative))
    else {
        return Ok(GitFileDiff::Unsupported(GitDiffUnsupported::NoChanges));
    };
    let kind = classify_status(entry.index_status, entry.worktree_status);
    if kind != GitFileStatusKind::Deleted {
        let metadata = match fs::symlink_metadata(&requested) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(GitFileDiff::Unsupported(GitDiffUnsupported::NotAFile)),
        };
        if !metadata.file_type().is_file() {
            return Ok(GitFileDiff::Unsupported(GitDiffUnsupported::NotAFile));
        }
        if metadata.len() > TEXT_PREVIEW_MAX_BYTES {
            return Ok(GitFileDiff::Unsupported(GitDiffUnsupported::TooLarge));
        }
        let bytes = fs::read(&requested).map_err(GitError::ReadOutput)?;
        if bytes.contains(&0) {
            return Ok(GitFileDiff::Unsupported(GitDiffUnsupported::Binary));
        }
        if kind == GitFileStatusKind::Untracked
            || (kind == GitFileStatusKind::Added && !has_head(&repository_root)?)
        {
            let patch = create_added_file_patch(&relative, &String::from_utf8_lossy(&bytes));
            return Ok(if patch.contains("\n@@ ") {
                GitFileDiff::Supported { kind, patch }
            } else {
                GitFileDiff::Unsupported(GitDiffUnsupported::MissingHunk)
            });
        }
    }

    let mut args = vec![
        OsString::from("diff"),
        OsString::from("--no-color"),
        OsString::from("--no-ext-diff"),
        OsString::from("--unified=3"),
    ];
    if has_head(&repository_root)? {
        args.push(OsString::from("HEAD"));
    } else {
        args.push(OsString::from("--cached"));
    }
    args.push(OsString::from("--"));
    if let Some(original) = &entry.original_path
        && !same_relative_path(original, &relative)
    {
        args.push(literal_pathspec(original));
    }
    args.push(literal_pathspec(&relative));
    let patch =
        String::from_utf8_lossy(&run_git(&repository_root, args, GIT_DIFF_MAX_OUTPUT)?.stdout)
            .into_owned();
    Ok(
        if patch.contains("\n@@ ") || (kind == GitFileStatusKind::Renamed && !patch.is_empty()) {
            GitFileDiff::Supported { kind, patch }
        } else {
            GitFileDiff::Unsupported(GitDiffUnsupported::MissingHunk)
        },
    )
}

pub fn list_worktrees(cwd: impl AsRef<Path>) -> Result<WorktreeSnapshot, GitError> {
    let cwd = checked_directory(cwd.as_ref())?;
    let project = crate::resolve_project(&cwd);
    let Some(_) = repository_root(&cwd)? else {
        return Err(command_failed(
            "worktree list --porcelain",
            "not a git repository",
        ));
    };
    let output = run_git(
        &cwd,
        [
            OsString::from("worktree"),
            OsString::from("list"),
            OsString::from("--porcelain"),
        ],
        GIT_DEFAULT_MAX_OUTPUT,
    )?;
    let current_key = project_identity_key(dunce::canonicalize(&cwd).unwrap_or(cwd.clone()));
    let main_key = project_identity_key(common_repository_root(&cwd)?);
    let mut worktrees = parse_worktree_porcelain(&output.stdout)
        .into_iter()
        .filter(|worktree| worktree.0.is_dir() && !worktree.2)
        .map(|(path, branch, _)| {
            let path = dunce::canonicalize(&path).unwrap_or(path);
            let path_key = project_identity_key(&path);
            WorktreeInfo {
                is_current: path_key == current_key,
                is_main: path_key == main_key,
                path,
                branch,
            }
        })
        .collect::<Vec<_>>();
    if !worktrees.iter().any(|worktree| worktree.is_current) {
        for worktree in &mut worktrees {
            worktree.is_current = path_within(&worktree.path, &cwd);
        }
    }
    Ok(WorktreeSnapshot {
        project_root: project.project_root,
        is_top_level: project.is_top_level,
        worktrees,
    })
}

pub fn add_worktree(cwd: impl AsRef<Path>, branch: &str) -> Result<WorktreeInfo, GitError> {
    let cwd = checked_directory(cwd.as_ref())?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(GitError::InvalidBranch(branch.to_owned()));
    }
    if !run_git_allow_failure(
        &cwd,
        [
            OsString::from("check-ref-format"),
            OsString::from("--branch"),
            OsString::from(branch),
        ],
        GIT_DEFAULT_MAX_OUTPUT,
    )?
    .status
    .success()
    {
        return Err(GitError::InvalidBranch(branch.to_owned()));
    }
    let repo_root = common_repository_root(&cwd)?;
    let name = repo_root
        .file_name()
        .ok_or(GitError::MissingRepositoryRoot)?
        .to_string_lossy();
    let base = repo_root
        .parent()
        .ok_or(GitError::MissingRepositoryRoot)?
        .join(format!("{name}-worktrees"));
    let directory_name = sanitize_branch_directory(branch);
    if directory_name.is_empty() {
        return Err(GitError::InvalidBranch(branch.to_owned()));
    }
    let target = base.join(directory_name);
    if target.exists() {
        return Err(GitError::WorktreePathExists(target));
    }
    fs::create_dir_all(&base).map_err(GitError::ReadOutput)?;
    let branch_exists = run_git_allow_failure(
        &repo_root,
        [
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from(format!("refs/heads/{branch}")),
        ],
        GIT_DEFAULT_MAX_OUTPUT,
    )?
    .status
    .success();
    let mut args = vec![OsString::from("worktree"), OsString::from("add")];
    if !branch_exists {
        args.push(OsString::from("-b"));
        args.push(OsString::from(branch));
    }
    args.push(OsString::from("--"));
    args.push(target.as_os_str().to_owned());
    if branch_exists {
        args.push(OsString::from(branch));
    }
    run_git(&repo_root, args, GIT_DEFAULT_MAX_OUTPUT)?;
    Ok(WorktreeInfo {
        path: dunce::canonicalize(&target).unwrap_or(target),
        branch: Some(branch.to_owned()),
        is_main: false,
        is_current: true,
    })
}

pub fn remove_worktree(
    cwd: impl AsRef<Path>,
    target: impl AsRef<Path>,
    force: bool,
) -> Result<(), GitError> {
    let cwd = checked_directory(cwd.as_ref())?;
    let snapshot = list_worktrees(&cwd)?;
    let normalized_target =
        dunce::canonicalize(target.as_ref()).unwrap_or_else(|_| target.as_ref().to_path_buf());
    let target_key = project_identity_key(&normalized_target);
    let Some(worktree) = snapshot
        .worktrees
        .iter()
        .find(|worktree| project_identity_key(&worktree.path) == target_key)
    else {
        return Err(GitError::UnknownWorktree(target.as_ref().to_path_buf()));
    };
    if worktree.is_main {
        return Err(GitError::MainWorktree);
    }
    scan_directory_links(&worktree.path)?;
    let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        args.push(OsString::from("--force"));
    }
    args.push(OsString::from("--"));
    args.push(worktree.path.as_os_str().to_owned());
    let output = run_git_allow_failure(&cwd, args.clone(), GIT_DEFAULT_MAX_OUTPUT)?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.contains("contains modified or untracked files") || message.contains("is dirty") {
        Err(GitError::DirtyWorktree)
    } else {
        Err(command_failed(&format_args(&args), &message))
    }
}

fn checked_directory(path: &Path) -> Result<PathBuf, GitError> {
    if !path.is_dir() {
        return Err(GitError::InvalidDirectory(path.to_path_buf()));
    }
    Ok(dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// 返回 `cwd` 所属 worktree 的 toplevel（`git rev-parse --show-toplevel`）。
///
/// linked worktree 与主 checkout 的 toplevel **不同**；判断「是否同一工作树」应比较本值，
/// 不要用路径前缀（父 cwd 在 `repo/crates/app`、候选是 `repo` 时前缀判断会误判）。
pub fn git_toplevel(cwd: impl AsRef<Path>) -> Result<Option<PathBuf>, GitError> {
    repository_root(cwd.as_ref())
}

fn repository_root(cwd: &Path) -> Result<Option<PathBuf>, GitError> {
    let output = run_git_allow_failure(
        cwd,
        [
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
        GIT_DEFAULT_MAX_OUTPUT,
    )?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("not a git repository") {
            return Ok(None);
        }
        return Err(command_failed("rev-parse --show-toplevel", error.trim()));
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(Some(dunce::canonicalize(&path).unwrap_or(path)))
}

fn common_repository_root(cwd: &Path) -> Result<PathBuf, GitError> {
    let output = run_git(
        cwd,
        [
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--git-common-dir"),
        ],
        GIT_DEFAULT_MAX_OUTPUT,
    )?;
    let common = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    common
        .parent()
        .map(|parent| dunce::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf()))
        .ok_or(GitError::MissingRepositoryRoot)
}

fn status_porcelain(repository_root: &Path) -> Result<(Vec<u8>, bool), GitError> {
    status_porcelain_with_limit(repository_root, GIT_STATUS_MAX_OUTPUT)
}

fn status_porcelain_with_limit(
    repository_root: &Path,
    max_output: usize,
) -> Result<(Vec<u8>, bool), GitError> {
    let args = |mode: &str| {
        [
            OsString::from("status"),
            OsString::from("--porcelain=v1"),
            OsString::from("-z"),
            OsString::from(format!("--untracked-files={mode}")),
        ]
    };
    match run_git(repository_root, args("all"), max_output) {
        Ok(output) => Ok((output.stdout, false)),
        Err(GitError::OutputTooLarge(_)) => {
            let output = run_git(repository_root, args("normal"), max_output)?;
            Ok((output.stdout, true))
        }
        Err(error) => Err(error),
    }
}

fn has_head(repository_root: &Path) -> Result<bool, GitError> {
    Ok(run_git_allow_failure(
        repository_root,
        [
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from("HEAD"),
        ],
        GIT_DEFAULT_MAX_OUTPUT,
    )?
    .status
    .success())
}

fn tracked_line_stats(repository_root: &Path, relative_cwd: &Path) -> Result<(u64, u64), GitError> {
    let pathspec = literal_pathspec(if relative_cwd.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative_cwd
    });
    let mut args = vec![
        OsString::from("diff"),
        OsString::from("--no-color"),
        OsString::from("--no-ext-diff"),
        OsString::from("--numstat"),
    ];
    if has_head(repository_root)? {
        args.push(OsString::from("HEAD"));
    } else {
        args.push(OsString::from("--cached"));
    }
    args.push(OsString::from("--"));
    args.push(pathspec);
    let output = run_git(repository_root, args, GIT_STATUS_MAX_OUTPUT)?;
    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('\t');
        additions =
            additions.saturating_add(fields.next().and_then(|v| v.parse().ok()).unwrap_or(0));
        deletions =
            deletions.saturating_add(fields.next().and_then(|v| v.parse().ok()).unwrap_or(0));
    }
    Ok((additions, deletions))
}

fn count_text_lines_budgeted(path: &Path, budget: u64) -> (u64, u64, bool) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return (0, 0, false);
    };
    if !metadata.file_type().is_file() {
        return (0, 0, false);
    }
    if metadata.len() > TEXT_PREVIEW_MAX_BYTES || metadata.len() > budget {
        return (0, 0, true);
    }
    let Ok(bytes) = fs::read(path) else {
        return (0, 0, false);
    };
    if bytes.is_empty() || bytes.contains(&0) {
        return (0, metadata.len(), false);
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    (
        newlines + u64::from(!bytes.ends_with(b"\n")),
        metadata.len(),
        false,
    )
}

fn strip_cwd_prefix(path: &Path, relative_cwd: &Path) -> PathBuf {
    if relative_cwd.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        path.strip_prefix(relative_cwd)
            .unwrap_or(path)
            .to_path_buf()
    }
}

fn literal_pathspec(path: &Path) -> OsString {
    OsString::from(format!(
        ":(literal){}",
        path.to_string_lossy().replace('\\', "/")
    ))
}

fn parse_porcelain_v1_z(output: &[u8]) -> Vec<PorcelainEntry> {
    let records = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.len() < 4 || record[2] != b' ' {
            continue;
        }
        let index_status = record[0] as char;
        let worktree_status = record[1] as char;
        let path = PathBuf::from(String::from_utf8_lossy(&record[3..]).into_owned());
        let original_path = if uses_rename_path(index_status, worktree_status) {
            let original = records.get(index).copied().unwrap_or_default();
            index += 1;
            (!original.is_empty())
                .then(|| PathBuf::from(String::from_utf8_lossy(original).into_owned()))
        } else {
            None
        };
        entries.push(PorcelainEntry {
            path,
            original_path,
            index_status,
            worktree_status,
        });
    }
    entries
}

fn uses_rename_path(index: char, worktree: char) -> bool {
    matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C')
}

fn classify_status(index: char, worktree: char) -> GitFileStatusKind {
    let pair = [index, worktree];
    if pair == ['?', '?'] {
        return GitFileStatusKind::Untracked;
    }
    let text = pair.iter().collect::<String>();
    if matches!(
        text.as_str(),
        "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU"
    ) || pair.contains(&'U')
    {
        GitFileStatusKind::Conflict
    } else if pair.contains(&'D') {
        GitFileStatusKind::Deleted
    } else if pair.iter().any(|status| matches!(status, 'R' | 'C')) {
        GitFileStatusKind::Renamed
    } else if pair.contains(&'A') {
        GitFileStatusKind::Added
    } else {
        GitFileStatusKind::Modified
    }
}

fn create_added_file_patch(relative: &Path, content: &str) -> String {
    let git_path = relative.to_string_lossy().replace('\\', "/");
    let trailing_newline = content.ends_with('\n');
    let mut lines = content.split('\n').collect::<Vec<_>>();
    if trailing_newline {
        lines.pop();
    }
    let body = lines
        .iter()
        .map(|line| format!("+{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let marker = if !trailing_newline && !lines.is_empty() {
        "\n\\ No newline at end of file"
    } else {
        ""
    };
    format!(
        "diff --git a/{git_path} b/{git_path}\nnew file mode 100644\n--- /dev/null\n+++ b/{git_path}\n@@ -0,0 +1,{} @@\n{body}{marker}",
        lines.len()
    )
}

fn parse_worktree_porcelain(output: &[u8]) -> Vec<(PathBuf, Option<String>, bool)> {
    let text = String::from_utf8_lossy(output);
    let mut result = Vec::new();
    let mut path = None;
    let mut branch = None;
    let mut prunable = false;
    let flush = |result: &mut Vec<_>,
                 path: &mut Option<PathBuf>,
                 branch: &mut Option<String>,
                 prunable: &mut bool| {
        if let Some(path) = path.take() {
            result.push((path, branch.take(), *prunable));
        }
        *prunable = false;
    };
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            flush(&mut result, &mut path, &mut branch, &mut prunable);
            path = Some(PathBuf::from(value.trim()));
        } else if let Some(value) = line.strip_prefix("branch ") {
            branch = Some(value.trim().trim_start_matches("refs/heads/").to_owned());
        } else if line.starts_with("prunable") {
            prunable = true;
        } else if line.is_empty() {
            flush(&mut result, &mut path, &mut branch, &mut prunable);
        }
    }
    flush(&mut result, &mut path, &mut branch, &mut prunable);
    result
}

fn sanitize_branch_directory(branch: &str) -> String {
    branch
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                || ch.is_whitespace()
            {
                '-'
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn scan_directory_links(root: &Path) -> Result<(), GitError> {
    let root_metadata = fs::symlink_metadata(root).map_err(GitError::ReadOutput)?;
    if is_reparse_or_symlink(&root_metadata) {
        return Err(GitError::DirectoryLink(root.to_path_buf()));
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(GitError::ReadOutput)? {
            let entry = entry.map_err(GitError::ReadOutput)?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(GitError::ReadOutput)?;
            // Windows 的 broken junction 无法解析 target；任何 reparse point 都 fail closed。
            if is_reparse_or_symlink(&metadata) {
                return Err(GitError::DirectoryLink(path));
            }
            if metadata.file_type().is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn is_reparse_or_symlink(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn normalize_existing_parent(path: &Path) -> PathBuf {
    if path.exists() {
        return dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    }
    path.parent()
        .and_then(|parent| dunce::canonicalize(parent).ok())
        .and_then(|parent| path.file_name().map(|name| parent.join(name)))
        .unwrap_or_else(|| path.to_path_buf())
}

fn path_within(parent: &Path, target: &Path) -> bool {
    if target.strip_prefix(parent).is_ok() {
        return true;
    }
    let parent = project_identity_key(parent);
    let target = project_identity_key(target);
    if target == parent {
        return true;
    }
    let separator = if cfg!(windows) { '\\' } else { '/' };
    target.starts_with(&format!("{parent}{separator}"))
}

fn same_relative_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    } else {
        left == right
    }
}

fn run_git<I>(cwd: &Path, args: I, max_output: usize) -> Result<GitOutput, GitError>
where
    I: IntoIterator<Item = OsString>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    let output = run_git_allow_failure(cwd, args.clone(), max_output)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failed(
            &format_args(&args),
            String::from_utf8_lossy(&output.stderr).trim(),
        ))
    }
}

fn run_git_allow_failure<I>(cwd: &Path, args: I, max_output: usize) -> Result<GitOutput, GitError>
where
    I: IntoIterator<Item = OsString>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    let display = format_args(&args);
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(&args)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(GitError::Spawn)?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_reader = thread::spawn(move || read_limited(stdout, max_output));
    let stderr_reader = thread::spawn(move || read_limited(stderr, max_output));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(GitError::Spawn)? {
            break status;
        }
        if start.elapsed() >= GIT_TIMEOUT {
            terminate_process_tree(child.id());
            let _ = child.wait();
            // 进程树终止会关闭继承的 pipe；仅在 child 已退出后收束 reader。
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(GitError::Timeout(display));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| GitError::ReadOutput(io::Error::other("stdout reader panicked")))?
        .map_err(GitError::ReadOutput)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| GitError::ReadOutput(io::Error::other("stderr reader panicked")))?
        .map_err(GitError::ReadOutput)?;
    if stdout.1 || stderr.1 {
        return Err(GitError::OutputTooLarge(display));
    }
    Ok(GitOutput {
        status,
        stdout: stdout.0,
        stderr: stderr.0,
    })
}

#[cfg(windows)]
fn terminate_process_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID"])
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn terminate_process_tree(pid: u32) {
    let _ = Command::new("kill")
        .args(["-TERM", "--"])
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(windows, unix)))]
fn terminate_process_tree(_: u32) {}

fn read_limited(mut reader: impl Read, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut total = 0_usize;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if kept.len() < limit {
            let remaining = limit - kept.len();
            kept.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    }
    Ok((kept, total > limit))
}

fn format_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_failed(args: &str, message: &str) -> GitError {
    GitError::CommandFailed {
        args: args.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek as _, Write as _};

    #[test]
    fn porcelain_parser_handles_rename_and_conflict() {
        let output = b" M plain.txt\0R  new name.txt\0old name.txt\0?? unicode-\xe4\xb8\xad.txt\0UU conflict.txt\0";
        let parsed = parse_porcelain_v1_z(output);
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[1].path, PathBuf::from("new name.txt"));
        assert_eq!(parsed[1].original_path, Some(PathBuf::from("old name.txt")));
        assert_eq!(classify_status('?', '?'), GitFileStatusKind::Untracked);
        assert_eq!(classify_status('U', 'U'), GitFileStatusKind::Conflict);
    }

    #[test]
    fn real_repository_status_diff_and_worktrees() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git_ok(&repo, ["init"]);
        git_ok(&repo, ["config", "user.name", "R12 Test"]);
        git_ok(&repo, ["config", "user.email", "r12@example.invalid"]);
        fs::write(repo.join("tracked.txt"), "one\n").unwrap();
        fs::write(repo.join("rename-me.txt"), "rename\n").unwrap();
        fs::write(repo.join("delete-me.txt"), "delete\n").unwrap();
        fs::write(repo.join("unchanged.txt"), "same\n").unwrap();
        fs::write(repo.join("binary.bin"), [0_u8, 1, 2]).unwrap();
        fs::write(repo.join("large.txt"), "small\n").unwrap();
        fs::create_dir(repo.join("sub")).unwrap();
        fs::write(repo.join("sub/inside.txt"), "inside\n").unwrap();
        git_ok(&repo, ["add", "."]);
        git_ok(&repo, ["commit", "-m", "base"]);

        fs::write(repo.join("tracked.txt"), "one\ntwo\n").unwrap();
        fs::write(repo.join("staged.txt"), "staged\n").unwrap();
        git_ok(&repo, ["add", "staged.txt"]);
        git_ok(&repo, ["mv", "rename-me.txt", "renamed.txt"]);
        fs::remove_file(repo.join("delete-me.txt")).unwrap();
        fs::write(repo.join("binary.bin"), [0_u8, 3, 4]).unwrap();
        fs::write(
            repo.join("large.txt"),
            vec![b'x'; TEXT_PREVIEW_MAX_BYTES as usize + 1],
        )
        .unwrap();
        fs::write(repo.join("sub/new 中.txt"), "a\nb").unwrap();
        let snapshot = git_status(&repo).unwrap();
        assert!(snapshot.is_git_repository);
        assert!(snapshot.files.len() >= 7);
        assert!(snapshot.additions >= 3);
        assert!(snapshot.files.iter().any(|file| {
            file.relative_path == Path::new("staged.txt")
                && file.kind == GitFileStatusKind::Added
                && file.index_status == 'A'
        }));
        assert!(snapshot.files.iter().any(|file| {
            file.relative_path == Path::new("tracked.txt")
                && file.kind == GitFileStatusKind::Modified
                && file.worktree_status == 'M'
        }));
        assert!(snapshot.files.iter().any(|file| {
            file.relative_path == Path::new("renamed.txt")
                && file.kind == GitFileStatusKind::Renamed
                && file.original_path.as_deref() == Some(Path::new("rename-me.txt"))
        }));
        assert!(snapshot.files.iter().any(|file| {
            file.relative_path == Path::new("delete-me.txt")
                && file.kind == GitFileStatusKind::Deleted
        }));
        let sub = git_status(repo.join("sub")).unwrap();
        assert_eq!(sub.files.len(), 1);
        assert_eq!(sub.files[0].relative_path, PathBuf::from("new 中.txt"));
        assert_eq!(sub.files[0].original_path, None);
        assert!(matches!(
            git_file_diff(repo.join("sub"), &sub.files[0].relative_path),
            Ok(GitFileDiff::Supported {
                kind: GitFileStatusKind::Untracked,
                ..
            })
        ));

        let diff = git_file_diff(&repo, "tracked.txt").unwrap();
        assert!(matches!(diff, GitFileDiff::Supported { ref patch, .. } if patch.contains("+two")));
        let untracked = git_file_diff(&repo, "sub/new 中.txt").unwrap();
        assert!(matches!(
            untracked,
            GitFileDiff::Supported {
                kind: GitFileStatusKind::Untracked,
                ..
            }
        ));
        assert!(matches!(
            git_file_diff(&repo, "staged.txt").unwrap(),
            GitFileDiff::Supported {
                kind: GitFileStatusKind::Added,
                ..
            }
        ));
        let renamed_diff = git_file_diff(&repo, "renamed.txt").unwrap();
        assert!(
            matches!(
                renamed_diff,
                GitFileDiff::Supported {
                    kind: GitFileStatusKind::Renamed,
                    ..
                }
            ),
            "{renamed_diff:?}"
        );
        assert!(matches!(
            git_file_diff(&repo, "delete-me.txt").unwrap(),
            GitFileDiff::Supported {
                kind: GitFileStatusKind::Deleted,
                ..
            }
        ));
        assert_eq!(
            git_file_diff(&repo, "binary.bin").unwrap(),
            GitFileDiff::Unsupported(GitDiffUnsupported::Binary)
        );
        assert_eq!(
            git_file_diff(&repo, "large.txt").unwrap(),
            GitFileDiff::Unsupported(GitDiffUnsupported::TooLarge)
        );
        assert_eq!(
            git_file_diff(&repo, "unchanged.txt").unwrap(),
            GitFileDiff::Unsupported(GitDiffUnsupported::NoChanges)
        );
        assert!(matches!(
            git_file_diff(repo.join("sub"), repo.join("tracked.txt")),
            Err(GitError::PathOutsideProject(_))
        ));

        assert!(matches!(
            add_worktree(&repo, "bad branch"),
            Err(GitError::InvalidBranch(_))
        ));
        let linked = add_worktree(&repo, "feature/r12").unwrap();
        assert!(linked.path.is_dir());
        assert!(matches!(
            add_worktree(&repo, "feature/r12"),
            Err(GitError::WorktreePathExists(_))
        ));
        let listed = list_worktrees(&repo).unwrap();
        assert_eq!(listed.worktrees.len(), 2);
        assert_eq!(
            listed
                .worktrees
                .iter()
                .filter(|worktree| worktree.is_main)
                .map(|worktree| project_identity_key(&worktree.path))
                .collect::<Vec<_>>(),
            vec![project_identity_key(
                dunce::canonicalize(&repo).unwrap_or(repo.clone())
            )]
        );
        assert!(
            listed
                .worktrees
                .iter()
                .any(|worktree| worktree.branch.as_deref() == Some("feature/r12"))
        );
        fs::write(linked.path.join("dirty.txt"), "dirty").unwrap();
        assert!(matches!(
            remove_worktree(&repo, &linked.path, false),
            Err(GitError::DirtyWorktree)
        ));
        remove_worktree(&repo, &linked.path, true).unwrap();
        assert!(!linked.path.exists());
        let existing_branch = add_worktree(&repo, "feature/r12").unwrap();
        assert_eq!(existing_branch.branch.as_deref(), Some("feature/r12"));
        let current = list_worktrees(&existing_branch.path).unwrap();
        assert!(current.worktrees.iter().any(|item| {
            item.is_current
                && project_identity_key(&item.path) == project_identity_key(&existing_branch.path)
        }));
        remove_worktree(&repo, &existing_branch.path, true).unwrap();
        assert!(matches!(
            remove_worktree(&repo, &repo, true),
            Err(GitError::MainWorktree)
        ));
    }

    #[test]
    fn unborn_head_status_and_diff_use_empty_tree_semantics() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("unborn");
        fs::create_dir(&repo).unwrap();
        git_ok(&repo, ["init"]);
        fs::write(repo.join("staged.txt"), "one\ntwo\n").unwrap();
        fs::write(repo.join("untracked.txt"), "free\n").unwrap();
        git_ok(&repo, ["add", "staged.txt"]);

        let snapshot = git_status(&repo).unwrap();
        assert_eq!(snapshot.additions, 3);
        assert!(snapshot.files.iter().any(|file| {
            file.relative_path == Path::new("staged.txt") && file.kind == GitFileStatusKind::Added
        }));
        assert!(snapshot.files.iter().any(|file| {
            file.relative_path == Path::new("untracked.txt")
                && file.kind == GitFileStatusKind::Untracked
        }));
        assert!(matches!(
            git_file_diff(&repo, "staged.txt").unwrap(),
            GitFileDiff::Supported {
                kind: GitFileStatusKind::Added,
                ref patch,
            } if patch.contains("+one") && patch.contains("+two")
        ));
        assert!(matches!(
            git_file_diff(&repo, "untracked.txt").unwrap(),
            GitFileDiff::Supported {
                kind: GitFileStatusKind::Untracked,
                ..
            }
        ));
    }

    #[test]
    fn status_fallback_runs_normal_mode_and_marks_output_truncated() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("fallback");
        fs::create_dir(&repo).unwrap();
        git_ok(&repo, ["init"]);
        let directory = repo.join("top");
        fs::create_dir(&directory).unwrap();
        for index in 0..8 {
            fs::write(directory.join(format!("long-file-name-{index}.txt")), "x").unwrap();
        }

        // all 会逐文件输出并超过小上限；normal 只返回顶层目录，必须真实回退才会成功。
        let (output, truncated) = status_porcelain_with_limit(&repo, 32).unwrap();
        assert!(truncated);
        let entries = parse_porcelain_v1_z(&output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("top/"));
        assert_eq!(entries[0].index_status, '?');
        assert_eq!(entries[0].worktree_status, '?');
    }

    #[test]
    fn literal_pathspec_escapes_glob_characters() {
        assert_eq!(
            literal_pathspec(Path::new("old*[?].txt")),
            OsString::from(":(literal)old*[?].txt")
        );
    }

    #[test]
    fn non_git_is_distinct_from_command_error() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = git_status(temp.path()).unwrap();
        assert!(!snapshot.is_git_repository);
        assert!(snapshot.files.is_empty());
        assert!(matches!(
            git_status(temp.path().join("missing")),
            Err(GitError::InvalidDirectory(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn worktree_with_junction_is_never_removed() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git_ok(&repo, ["init"]);
        git_ok(&repo, ["config", "user.name", "R12 Test"]);
        git_ok(&repo, ["config", "user.email", "r12@example.invalid"]);
        fs::write(repo.join("base.txt"), "base\n").unwrap();
        git_ok(&repo, ["add", "."]);
        git_ok(&repo, ["commit", "-m", "base"]);
        let linked = add_worktree(&repo, "junction-test").unwrap();
        let external = temp.path().join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("keep.txt"), "keep").unwrap();
        let junction = linked.path.join("linked-dir");
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&external)
            .status()
            .unwrap();
        if !status.success() {
            remove_worktree(&repo, &linked.path, true).unwrap();
            return;
        }
        assert!(matches!(
            remove_worktree(&repo, &linked.path, true),
            Err(GitError::DirectoryLink(_))
        ));
        assert!(external.join("keep.txt").is_file());
        let _ = Command::new("cmd")
            .args(["/C", "rmdir"])
            .arg(&junction)
            .status();
        remove_worktree(&repo, &linked.path, true).unwrap();
    }

    fn git_ok<const N: usize>(cwd: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn added_patch_preserves_no_newline_marker() {
        let patch = create_added_file_patch(Path::new("a b.txt"), "hello");
        assert!(patch.contains("+++ b/a b.txt"));
        assert!(patch.contains("\\ No newline at end of file"));
    }

    #[test]
    fn limited_reader_reports_only_strict_overflow_and_drains() {
        let mut exact = tempfile::tempfile().unwrap();
        exact.write_all(&[b'x'; 16]).unwrap();
        exact.rewind().unwrap();
        let (bytes, exceeded) = read_limited(exact, 16).unwrap();
        assert_eq!(bytes.len(), 16);
        assert!(!exceeded);

        let mut overflow = tempfile::tempfile().unwrap();
        overflow.write_all(&[b'x'; 64]).unwrap();
        overflow.rewind().unwrap();
        let (bytes, exceeded) = read_limited(overflow, 16).unwrap();
        assert_eq!(bytes.len(), 16);
        assert!(exceeded);
    }
}
