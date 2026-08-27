//! GPUI-Pi 的应用级 UI 组件层。
//!
//! 只放跨面板复用的外壳、主题和组件封装；窗口状态与 Dock 编排留在 `crates/app`。

mod chat;
mod file_workspace;
mod git_workspace;
mod project_trust_dialog;
mod session_tabs;
mod shell;
mod subagent_panel;
mod tab_bar;
pub mod theme;

pub use chat::{ChatMinimap, ChatWindow, MarkdownBody, MessageView};
pub use file_workspace::{WorkspaceContentTab, WorkspaceContentTabs};
pub use git_workspace::{
    DiffView, GitChangeItem, GitChangeKind, GitChangesModel, GitChangesView, TurnWrittenFiles,
    render_diff_block,
};
pub use project_trust_dialog::project_trust_dialog;
pub use session_tabs::{SessionTabItem, SessionTabState, SessionTabs};
pub use shell::AppShell;
pub use subagent_panel::{render_subagent_tasks, subagent_summary};
pub use tab_bar::WorkspaceTabBar;
