//! mutating 子代理的 worktree writer 隔离（纯逻辑）。
//!
//! R26 把执行交给钉死的 `pi-subagents-lite`：Agent 工具已有可选 `worktree_path`，
//! 但内核**不会**强制 mutating 任务离开父 checkout，也不会限制「同一 path 两个 writer」。
//! 本模块是 Manager 侧的权威策略与租约表；进程内真正挡下违规调用的是随包
//! `writer-isolation.ts` host 扩展（`tool_call` 可改写入参 / block）。
//!
//! 事实口径：
//! - `Explore` 与明确只读工具集 → 非 mutating，可不带 worktree；
//! - 默认 `general-purpose`、工具集含 `edit`/`write`、或未声明工具（继承默认 edit/write）→ mutating；
//! - mutating 必须落在**独立** linked worktree（不得等于父会话 cwd / 主 checkout）；
//! - 同一 canonical worktree path 最多一个活跃 writer；
//! - 完成后进入串行集成队列，父会话一次只审查一项。

use pi_data::{
    GitError, WorktreeInfo, add_worktree, project_identity_key, remove_worktree,
};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// 内建只读类型名（大小写不敏感）。与内核 `default-agents.ts` 的 `Explore` 对齐。
const BUILTIN_READ_ONLY_TYPES: &[&str] = &["explore"];

/// 一旦出现即视为 mutating 的工具名。
const MUTATING_TOOL_NAMES: &[&str] = &["edit", "write"];

/// 内核默认活跃工具含 edit/write（`DEFAULT_ACTIVE_TOOL_NAMES`），未声明 tools 时按 mutating 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWriteClass {
    /// 明确只读（Explore 或仅 read/grep/find/bash 等）。
    ReadOnly,
    /// 可能改仓库，必须独立 worktree。
    Mutating,
}

/// 根据 agent 类型名与可选工具声明分类。
///
/// `tools == None` 表示「继承默认活跃集」（含 edit/write）→ Mutating。
/// `tools == Some([])` 表示无工具 → ReadOnly。
#[must_use]
pub fn classify_agent(agent_type: &str, tools: Option<&[String]>) -> AgentWriteClass {
    let type_key = agent_type.trim().to_ascii_lowercase();
    if BUILTIN_READ_ONLY_TYPES
        .iter()
        .any(|name| *name == type_key.as_str())
    {
        return AgentWriteClass::ReadOnly;
    }
    match tools {
        None => AgentWriteClass::Mutating,
        Some(list) if list.is_empty() => AgentWriteClass::ReadOnly,
        Some(list) => {
            if list.iter().any(|tool| {
                MUTATING_TOOL_NAMES
                    .iter()
                    .any(|name| tool.eq_ignore_ascii_case(name))
            }) {
                AgentWriteClass::Mutating
            } else {
                AgentWriteClass::ReadOnly
            }
        }
    }
}

/// 一条 writer 租约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterLease {
    pub agent_id: String,
    pub tool_call_id: Option<String>,
    pub worktree_path: PathBuf,
    pub parent_cwd: PathBuf,
    /// 是否由本 Manager / host 分配（清理时才有权 remove）。
    pub owned: bool,
}

/// 等待父会话串行审查的一项集成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingIntegration {
    pub agent_id: String,
    pub worktree_path: PathBuf,
    pub description: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationError {
    /// mutating 任务缺少独立 worktree。
    MissingWorktree,
    /// 路径落在父 checkout / 主树上，不算独立。
    NotIndependent { path: PathBuf },
    /// 该 worktree 已有活跃 writer。
    WriterConflict {
        path: PathBuf,
        holder: String,
    },
    /// 只读任务不应占用 writer 租约。
    ReadOnlyCannotLease,
    Git(String),
}

impl std::fmt::Display for IsolationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingWorktree => write!(f, "mutating 子代理缺少独立 worktree"),
            Self::NotIndependent { path } => {
                write!(f, "worktree 必须独立于父 checkout：{}", path.display())
            }
            Self::WriterConflict { path, holder } => write!(
                f,
                "worktree 已有活跃 writer（{holder}）：{}",
                path.display()
            ),
            Self::ReadOnlyCannotLease => write!(f, "只读子代理不能占用 writer 租约"),
            Self::Git(message) => write!(f, "git：{message}"),
        }
    }
}

impl std::error::Error for IsolationError {}

impl From<GitError> for IsolationError {
    fn from(value: GitError) -> Self {
        Self::Git(value.to_string())
    }
}

/// Manager 侧租约表 + 串行集成队列。
#[derive(Debug, Default)]
pub struct WriterIsolation {
    /// canonical path key → lease
    leases: HashMap<String, WriterLease>,
    /// 已完成、待父会话审查（FIFO；前台一次只暴露队头）。
    integrations: VecDeque<PendingIntegration>,
}

impl WriterIsolation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn active_leases(&self) -> impl Iterator<Item = &WriterLease> {
        self.leases.values()
    }

    #[must_use]
    pub fn pending_integrations(&self) -> &VecDeque<PendingIntegration> {
        &self.integrations
    }

    /// 队头（当前应审查的一项）；空则 None。
    #[must_use]
    pub fn current_integration(&self) -> Option<&PendingIntegration> {
        self.integrations.front()
    }

    /// 判定 mutating 调用是否允许在该 path 上取得租约。
    pub fn check_mutating_path(
        &self,
        parent_cwd: &Path,
        worktree_path: Option<&Path>,
    ) -> Result<PathBuf, IsolationError> {
        let Some(path) = worktree_path else {
            return Err(IsolationError::MissingWorktree);
        };
        if !is_independent_worktree(parent_cwd, path) {
            return Err(IsolationError::NotIndependent {
                path: path.to_path_buf(),
            });
        }
        let key = project_identity_key(path);
        if let Some(existing) = self.leases.get(&key) {
            return Err(IsolationError::WriterConflict {
                path: path.to_path_buf(),
                holder: existing.agent_id.clone(),
            });
        }
        Ok(dunce_canonical(path))
    }

    /// 取得 writer 租约。调用方须先 `check_mutating_path` 或传入已校验 path。
    pub fn acquire(
        &mut self,
        class: AgentWriteClass,
        parent_cwd: &Path,
        worktree_path: &Path,
        agent_id: impl Into<String>,
        tool_call_id: Option<String>,
        owned: bool,
    ) -> Result<&WriterLease, IsolationError> {
        if class == AgentWriteClass::ReadOnly {
            return Err(IsolationError::ReadOnlyCannotLease);
        }
        let path = self.check_mutating_path(parent_cwd, Some(worktree_path))?;
        let key = project_identity_key(&path);
        let lease = WriterLease {
            agent_id: agent_id.into(),
            tool_call_id,
            worktree_path: path,
            parent_cwd: dunce_canonical(parent_cwd),
            owned,
        };
        self.leases.insert(key.clone(), lease);
        Ok(self.leases.get(&key).expect("just inserted"))
    }

    /// 按 agent_id 或 tool_call_id 释放。
    pub fn release(&mut self, agent_id: &str) -> Option<WriterLease> {
        let key = self
            .leases
            .iter()
            .find(|(_, lease)| {
                lease.agent_id == agent_id
                    || lease
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|id| id == agent_id)
            })
            .map(|(key, _)| key.clone())?;
        self.leases.remove(&key)
    }

    pub fn release_by_tool_call(&mut self, tool_call_id: &str) -> Option<WriterLease> {
        let key = self
            .leases
            .iter()
            .find(|(_, lease)| lease.tool_call_id.as_deref() == Some(tool_call_id))
            .map(|(key, _)| key.clone())?;
        self.leases.remove(&key)
    }

    /// 子代理结束后入队串行集成（仅 mutating + 有 worktree 时有意义）。
    pub fn enqueue_integration(&mut self, item: PendingIntegration) {
        if self
            .integrations
            .iter()
            .any(|pending| pending.agent_id == item.agent_id)
        {
            return;
        }
        self.integrations.push_back(item);
    }

    /// 父会话审查完成（接受或丢弃）后弹出队头。
    pub fn complete_integration(&mut self, agent_id: &str) -> Option<PendingIntegration> {
        let front = self.integrations.front()?;
        if front.agent_id != agent_id {
            return None;
        }
        self.integrations.pop_front()
    }

    /// 取消 / 失败恢复：释放租约；若在队列里也摘掉。
    pub fn recover_agent(&mut self, agent_id: &str) -> Option<WriterLease> {
        self.integrations.retain(|item| item.agent_id != agent_id);
        self.release(agent_id)
    }
}

/// 父 cwd 与候选 path 是否同一 checkout（不算独立）。
#[must_use]
pub fn is_independent_worktree(parent_cwd: &Path, worktree_path: &Path) -> bool {
    let parent = dunce_canonical(parent_cwd);
    let candidate = dunce_canonical(worktree_path);
    if project_identity_key(&parent) == project_identity_key(&candidate) {
        return false;
    }
    // 候选是父路径的子目录（未单独 worktree）也不算独立。
    if candidate.starts_with(&parent) {
        return false;
    }
    true
}

/// 分配一条 Manager 拥有的 writer worktree（分支 `gpui-pi/writer-<label>`）。
pub fn allocate_writer_worktree(
    parent_cwd: &Path,
    label: &str,
) -> Result<WorktreeInfo, IsolationError> {
    let branch = writer_branch_name(label);
    let info = add_worktree(parent_cwd, &branch)?;
    if !is_independent_worktree(parent_cwd, &info.path) {
        let _ = remove_worktree(parent_cwd, &info.path, true);
        return Err(IsolationError::NotIndependent {
            path: info.path,
        });
    }
    Ok(info)
}

/// 清理 Manager 拥有的 writer worktree；**必须**走 `remove_worktree`（内含 reparse 扫描）。
pub fn cleanup_writer_worktree(
    parent_cwd: &Path,
    worktree_path: &Path,
    force: bool,
) -> Result<(), IsolationError> {
    remove_worktree(parent_cwd, worktree_path, force).map_err(IsolationError::from)
}

/// 从已结算且带 worktree 的 mutating 任务推导串行集成队列。
///
/// Manager 内存队列是权威写入路径；文档回看时没有 Manager 状态，用本函数从
/// `collect_tasks` 结果还原「还没被父会话消化」的 writer 结果。队头 = 当前应审查项。
#[must_use]
pub fn pending_writer_integrations(
    tasks: &[pi_render::SubagentTask],
) -> Vec<&pi_render::SubagentTask> {
    tasks
        .iter()
        .filter(|task| {
            task.status.is_settled()
                && task.worktree_path.is_some()
                && classify_agent(&task.agent_type, None) == AgentWriteClass::Mutating
        })
        .collect()
}

fn writer_branch_name(label: &str) -> String {
    let sanitized = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_ascii_lowercase();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let base = if sanitized.is_empty() {
        format!("gpui-pi/writer-{stamp}")
    } else {
        format!("gpui-pi/writer-{sanitized}-{stamp}")
    };
    // git branch 名不宜过长
    if base.len() > 80 {
        format!("gpui-pi/writer-{stamp}")
    } else {
        base
    }
}

fn dunce_canonical(path: &Path) -> PathBuf {
    // 与 pi-data 一样优先 canonicalize；失败时退回原 path（测试里的相对路径）。
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_render::{SubagentStats, SubagentStatus, SubagentTask};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        run(dir.path(), &["git", "init", "-b", "main"]);
        run(
            dir.path(),
            &["git", "config", "user.email", "r27@example.com"],
        );
        run(dir.path(), &["git", "config", "user.name", "r27"]);
        fs::write(dir.path().join("README"), "r27\n").unwrap();
        run(dir.path(), &["git", "add", "README"]);
        run(dir.path(), &["git", "commit", "-m", "init"]);
        dir
    }

    fn run(cwd: &Path, args: &[&str]) {
        let status = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    }

    #[test]
    fn pending_writer_integrations_skip_explore_and_keep_document_order() {
        let explore = SubagentTask {
            key: "e".into(),
            agent_id: Some("explore1".into()),
            tool_call_id: None,
            agent_type: "Explore".into(),
            description: "scan".into(),
            background: true,
            worktree_path: Some("/wt/e".into()),
            output_file: None,
            status: SubagentStatus::Completed,
            stop_reason: None,
            stats: SubagentStats::default(),
        };
        let first = SubagentTask {
            key: "a".into(),
            agent_id: Some("a1".into()),
            tool_call_id: None,
            agent_type: "general-purpose".into(),
            description: "one".into(),
            background: true,
            worktree_path: Some("/wt/a".into()),
            output_file: None,
            status: SubagentStatus::Completed,
            stop_reason: None,
            stats: SubagentStats::default(),
        };
        let second = SubagentTask {
            key: "b".into(),
            agent_id: Some("b1".into()),
            tool_call_id: None,
            agent_type: "coder".into(),
            description: "two".into(),
            background: true,
            worktree_path: Some("/wt/b".into()),
            output_file: None,
            status: SubagentStatus::Completed,
            stop_reason: None,
            stats: SubagentStats::default(),
        };
        let running = SubagentTask {
            key: "c".into(),
            agent_id: Some("c1".into()),
            tool_call_id: None,
            agent_type: "general-purpose".into(),
            description: "live".into(),
            background: true,
            worktree_path: Some("/wt/c".into()),
            output_file: None,
            status: SubagentStatus::Running,
            stop_reason: None,
            stats: SubagentStats::default(),
        };
        let tasks = [explore, first, second, running];
        let pending = pending_writer_integrations(&tasks);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].key, "a");
        assert_eq!(pending[1].key, "b");
    }

    #[test]
    fn explore_and_explicit_readonly_tools_are_not_mutating() {
        assert_eq!(
            classify_agent("Explore", None),
            AgentWriteClass::ReadOnly
        );
        assert_eq!(
            classify_agent(
                "scout",
                Some(&["read".into(), "grep".into(), "bash".into()])
            ),
            AgentWriteClass::ReadOnly
        );
    }

    #[test]
    fn general_purpose_and_edit_tools_are_mutating() {
        assert_eq!(
            classify_agent("general-purpose", None),
            AgentWriteClass::Mutating
        );
        assert_eq!(
            classify_agent("coder", Some(&["read".into(), "edit".into()])),
            AgentWriteClass::Mutating
        );
    }

    #[test]
    fn lease_table_enforces_single_writer_and_independence() {
        let repo = init_repo();
        let mut iso = WriterIsolation::new();
        let parent = repo.path();
        assert!(matches!(
            iso.check_mutating_path(parent, None),
            Err(IsolationError::MissingWorktree)
        ));
        assert!(matches!(
            iso.check_mutating_path(parent, Some(parent)),
            Err(IsolationError::NotIndependent { .. })
        ));

        let linked = allocate_writer_worktree(parent, "a1").unwrap();
        iso.acquire(
            AgentWriteClass::Mutating,
            parent,
            &linked.path,
            "agent-1",
            Some("tc-1".into()),
            true,
        )
        .unwrap();
        let err = iso
            .check_mutating_path(parent, Some(&linked.path))
            .unwrap_err();
        assert!(matches!(err, IsolationError::WriterConflict { .. }));

        iso.release("agent-1").unwrap();
        iso.check_mutating_path(parent, Some(&linked.path))
            .unwrap();
        cleanup_writer_worktree(parent, &linked.path, true).unwrap();
    }

    #[test]
    fn integration_queue_is_serial_by_front() {
        let mut iso = WriterIsolation::new();
        iso.enqueue_integration(PendingIntegration {
            agent_id: "a".into(),
            worktree_path: PathBuf::from("/wt/a"),
            description: "one".into(),
            status: "completed".into(),
        });
        iso.enqueue_integration(PendingIntegration {
            agent_id: "b".into(),
            worktree_path: PathBuf::from("/wt/b"),
            description: "two".into(),
            status: "completed".into(),
        });
        // 重复入队忽略
        iso.enqueue_integration(PendingIntegration {
            agent_id: "a".into(),
            worktree_path: PathBuf::from("/wt/a"),
            description: "one".into(),
            status: "completed".into(),
        });
        assert_eq!(iso.pending_integrations().len(), 2);
        assert_eq!(iso.current_integration().unwrap().agent_id, "a");
        assert!(iso.complete_integration("b").is_none());
        assert_eq!(iso.complete_integration("a").unwrap().agent_id, "a");
        assert_eq!(iso.current_integration().unwrap().agent_id, "b");
    }

    #[test]
    fn recover_releases_lease_and_drops_queue_item() {
        let repo = init_repo();
        let mut iso = WriterIsolation::new();
        let linked = allocate_writer_worktree(repo.path(), "x").unwrap();
        iso.acquire(
            AgentWriteClass::Mutating,
            repo.path(),
            &linked.path,
            "agent-x",
            None,
            true,
        )
        .unwrap();
        iso.enqueue_integration(PendingIntegration {
            agent_id: "agent-x".into(),
            worktree_path: linked.path.clone(),
            description: "d".into(),
            status: "error".into(),
        });
        let lease = iso.recover_agent("agent-x").unwrap();
        assert!(lease.owned);
        assert!(iso.current_integration().is_none());
        cleanup_writer_worktree(repo.path(), &linked.path, true).unwrap();
    }

    #[test]
    fn cleanup_rejects_directory_symlink_inside_worktree() {
        let repo = init_repo();
        let linked = allocate_writer_worktree(repo.path(), "link").unwrap();
        let target = repo.path().join("outside-target");
        fs::create_dir(&target).unwrap();
        let link = linked.path.join("bad-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();

        // Unix 上 is_reparse_or_symlink 认 symlink；Windows 认 reparse。
        // Linux CI / 本机单测都应拒绝清理。
        let err = cleanup_writer_worktree(repo.path(), &linked.path, true).unwrap_err();
        assert!(
            matches!(err, IsolationError::Git(_)),
            "expected git/reparse failure, got {err:?}"
        );
        // 手动拆掉链接后应能清掉，避免 TempDir 残留
        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&link);
        cleanup_writer_worktree(repo.path(), &linked.path, true).unwrap();
    }
}
