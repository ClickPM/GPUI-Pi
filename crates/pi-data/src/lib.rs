//! pi 的本地数据目录（默认 `~/.pi/agent`）读写层。
//!
//! 与终端 `pi`、`pi-web`、pi-web-desktop **共享同一份目录**，因此本 crate 的
//! 写操作必须保守：能只读就只读，必须写时走「临时文件 + rename」。
//!
//! 本 crate 不依赖 GPUI。

use std::ffi::OsString;
use std::path::PathBuf;

pub mod composer;
pub mod config;
pub mod extensions;
pub mod files;
mod fs_util;
pub mod git;
pub mod model_config;
pub mod project;
pub mod resources;
pub mod session;
pub mod session_actions;
pub mod session_view;
pub mod trust;

pub use composer::{
    AT_RESULT_LIMIT, AtInsertion, AtQuery, ComposerDraft, DraftImage, DraftStore, FILE_INDEX_LIMIT,
    FileIndex, FileIndexEntry, ImageValidationError, MAX_ATTACHED_IMAGE_BYTES, MAX_ATTACHED_IMAGES,
    SupportedImageFormat, apply_at_insertion, build_at_insertion, build_entries_from_files,
    build_file_index, detect_image_format, extract_at_query, filter_file_entries, image_from_bytes,
    image_from_clipboard_bytes, merge_restored_submission, validate_image_batch,
};
pub use config::{
    ConfigError, models_path, read_auto_retry_enabled, read_json, read_models, read_settings,
    read_trust, settings_path, trust_path, write_json_atomic, write_models, write_settings,
    write_trust,
};
pub use extensions::{
    ExtensionDiagnostic, ExtensionInfo, ExtensionKind, ExtensionScan, scan_extensions,
};
pub use files::{
    FILE_INDEX_HARD_LIMIT, FILE_INDEX_MAX_DEPTH, FILE_SEARCH_RESULT_LIMIT, FILE_TREE_LIMIT,
    FileAccessError, FileContent, FileNode, FileNodeKind, FileTreeSnapshot,
    IMAGE_PREVIEW_MAX_BYTES, ImageFileContent, ImageKind, MAX_UPLOAD_FILE_BYTES,
    MAX_UPLOAD_TOTAL_BYTES, ProjectFiles, TEXT_PREVIEW_MAX_BYTES, TextFileContent, UploadCandidate,
    UploadConflictStrategy, UploadInspection, UploadItemError, UploadReport, language_for_path,
    validate_upload_name,
};
pub use git::{
    GitDiffUnsupported, GitError, GitFileDiff, GitFileStatus, GitFileStatusKind, GitStatusSnapshot,
    WorktreeInfo, WorktreeSnapshot, add_worktree, git_file_diff, git_status, git_toplevel,
    list_worktrees, remove_worktree,
};
pub use model_config::{
    AuthCapability, AuthKind, AuthSummary, CliAuthStatus, ModelApi, ModelConfigDocument,
    ModelConfigError, ModelEntry, ProviderConfig, ProviderDescriptor, ProviderDraft, SecretString,
    auth_path, built_in_providers, connectivity_request_body, merge_provider_directory,
    parse_cli_auth_status, parse_discovered_models, read_api_key, read_auth_summaries,
    remove_api_key, validate_base_url, write_api_key,
};
pub use project::{
    GroupedSession, PathPlatform, ProjectGroup, ProjectInfo, group_sessions, native_platform,
    project_identity_key, project_identity_key_for, resolve_project,
};
pub use resources::{
    FileRevision, PackageFilters, PluginPackageInfo, PluginScan, ResourceDiagnostic, ResourceScope,
    SkillInfo, SkillScan, SkillWriteError, scan_plugin_packages, scan_skills,
    set_skill_disable_model_invocation, skill_allowed_roots,
};
pub use session::{
    EntryBase, SessionBranchNode, SessionBranchTree, SessionDiagnostic, SessionEntry, SessionError,
    SessionFile, SessionHeader, SessionList, SessionListDiagnostic, SessionMetrics,
    SessionRevision, SessionSummary, list_sessions, load_session, read_session_summary,
    session_metrics,
};
pub use session_actions::{
    SessionActionError, delete_leaf_session, export_session_jsonl, rename_session,
};
pub use session_view::{
    ProjectSessionView, RunningSessionOverlay, SessionView, build_session_view,
};
pub use trust::{
    ProjectTrustStatus, TrustError, has_trust_resources, read_project_trust_status, trust_project,
};

/// pi 用来覆盖数据目录的环境变量，语义与上游一致。
pub const AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// 解析 pi 的 agent 数据目录。
///
/// 优先 `PI_CODING_AGENT_DIR`，否则 `<home>/.pi/agent`。
/// Windows 上 `dirs::home_dir()` 取的是 `USERPROFILE`，不要自己读 `HOME`。
pub fn agent_dir() -> Option<PathBuf> {
    agent_dir_from(std::env::var_os(AGENT_DIR_ENV), dirs::home_dir())
}

/// 当前用户 home，统一由 `dirs` 解析 Windows `USERPROFILE`。
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// [`agent_dir`] 的纯函数版本，便于在不改进程环境的前提下单测。
pub fn agent_dir_from(env_override: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match env_override {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => home.map(|h| h.join(".pi").join("agent")),
    }
}

/// 会话文件所在目录。
pub fn sessions_dir() -> Option<PathBuf> {
    agent_dir().map(|d| d.join("sessions"))
}

/// 扩展目录。
pub fn extensions_dir() -> Option<PathBuf> {
    agent_dir().map(|d| d.join("extensions"))
}

/// 技能目录。GPUI-Pi 只读不写 —— 部署归 pi-web-desktop 管，见立项文档 § 一。
pub fn skills_dir() -> Option<PathBuf> {
    agent_dir().map(|d| d.join("skills"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        let got = agent_dir_from(Some(OsString::from("/tmp/custom")), Some("/home/x".into()));
        assert_eq!(got, Some(PathBuf::from("/tmp/custom")));
    }

    #[test]
    fn empty_env_falls_back_to_home() {
        let got = agent_dir_from(Some(OsString::new()), Some("/home/x".into()));
        assert_eq!(got, Some(PathBuf::from("/home/x/.pi/agent")));
    }

    #[test]
    fn no_home_no_dir() {
        assert_eq!(agent_dir_from(None, None), None);
    }
}
