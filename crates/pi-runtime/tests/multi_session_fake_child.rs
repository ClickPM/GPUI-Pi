//! R24 验收：两个用户 Session 真并行、切前台不动后台、释放运行槽后排队会话立即补位。
//!
//! 全部以 `runtime_fake_child` 为内核，不链接 GPUI、不消耗模型 token。
//! 这里断言的是**调度器对 UI 的契约**（并行、独立、通知）；UI 侧的标签隔离在
//! `crates/app` 的 `--lib` 测试里断言。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use pi_render::ConversationDocument;
use pi_runtime::{
    Clock, ComposerMode, ComposerSubmission, FakeClock, Priority, RpcIntent, RuntimeLimits,
    RuntimeManager, SchedulerChanged, SchedulerLimits, SchedulerState, SessionDescriptor,
    SessionHandle, SessionId, ToolPreset,
};

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_runtime_fake_child"))
}

fn empty_document(id: &str, cwd: &Path) -> ConversationDocument {
    ConversationDocument {
        session_id: id.to_owned(),
        source_path: PathBuf::new(),
        cwd: cwd.to_path_buf(),
        messages: Arc::from([]),
        items: Arc::from([]),
        minimap: Arc::from([]),
        diagnostics: Arc::from([]),
    }
}

fn session_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(format!("{name}.jsonl"));
    std::fs::write(&path, "{\"type\":\"session\"}\n").expect("write fake session file");
    path
}

fn descriptor(cwd: &Path, session_path: Option<PathBuf>) -> SessionDescriptor {
    SessionDescriptor {
        binary: fake_binary(),
        cwd: cwd.to_path_buf(),
        session_path,
        tool_preset: ToolPreset::Inherit,
        agent_dir: None,
    }
}

/// 用 fake clock 构造的 Manager：没有 reaper 线程，一切推进都由显式 `tick()` 触发。
///
/// 这正是「排队会话必须由 app 释放运行槽后立刻补位」这条验收成立的前提——
/// 没有后台线程偷偷帮忙，测出来的就是 app 接线本身的效果。
fn test_manager(scheduler: SchedulerLimits) -> RuntimeManager {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
    RuntimeManager::with_test_clock(
        RuntimeLimits {
            scheduler,
            ..RuntimeLimits::default()
        },
        clock,
    )
}

fn register(manager: &RuntimeManager, workspace: &Path, name: &str) -> SessionId {
    manager.create_session(
        descriptor(workspace, Some(session_file(workspace, name))),
        empty_document(name, workspace),
    )
}

fn run(manager: &RuntimeManager, session: SessionId, priority: Priority) -> SessionHandle {
    manager
        .request_run(session, priority)
        .expect("request_run")
        .expect("slot available")
}

/// 等到 fake child 把一整条 assistant 消息推完。
fn wait_for_message(handle: &SessionHandle, what: &str) -> usize {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let count = handle.snapshot().document.messages.len();
        if count > 0 {
            return count;
        }
        assert!(Instant::now() < deadline, "{what} 没有产出 assistant 消息");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stream(handle: &SessionHandle) {
    handle
        .dispatch(
            RpcIntent::Prompt,
            Some(ComposerSubmission {
                message: "stream".to_owned(),
                images: Vec::new(),
            }),
            ComposerMode::FollowUp,
        )
        .expect("dispatch");
}

#[test]
fn two_user_sessions_run_in_parallel_and_stream_independently() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let limits = SchedulerLimits::default();
    let manager = test_manager(limits);
    let first = register(&manager, workspace.path(), "alpha");
    let second = register(&manager, workspace.path(), "beta");

    let alpha = run(&manager, first, Priority::FOREGROUND);
    let beta = run(&manager, second, Priority::FOREGROUND);

    assert_eq!(manager.session_state(first), Some(SchedulerState::Running));
    assert_eq!(manager.session_state(second), Some(SchedulerState::Running));
    let report = manager.scheduler_report();
    assert_eq!(report.running, 2, "两个用户会话必须同时 Running");
    assert!(
        report.resident_pi <= limits.total_runtime_slots,
        "常驻 pi 数越界：{report:?}"
    );

    // 两个独立进程、两个独立运行时容器：任何一边复用了另一边都会在这里判红。
    let alpha_pid = alpha.process_id().expect("alpha pid");
    let beta_pid = beta.process_id().expect("beta pid");
    assert_ne!(alpha_pid, beta_pid);
    assert_ne!(alpha.runtime_id(), beta.runtime_id());
    assert_ne!(alpha.session_id(), beta.session_id());

    // 只喂 alpha：beta 的文档必须原地不动，证明事件没有串台。
    stream(&alpha);
    let produced = wait_for_message(&alpha, "alpha");
    assert!(
        beta.snapshot().document.messages.is_empty(),
        "beta 收到了 alpha 的流式事件"
    );

    // 再喂 beta：alpha 的产出不受影响，两边各自计数。
    stream(&beta);
    wait_for_message(&beta, "beta");
    assert_eq!(alpha.snapshot().document.messages.len(), produced);

    manager.remove_session(first);
    manager.remove_session(second);
}

#[test]
fn switching_the_foreground_session_never_stops_the_background_one() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let manager = test_manager(SchedulerLimits::default());
    let background = register(&manager, workspace.path(), "background");
    let foreground = register(&manager, workspace.path(), "foreground");

    let background_handle = run(&manager, background, Priority::FOREGROUND);
    let background_pid = background_handle.process_id().expect("pid");
    let foreground_handle = run(&manager, foreground, Priority::FOREGROUND);

    // UI 切到另一个标签 = 把新标签抬到前台优先级；**不碰**任何进程。
    let switched = manager
        .request_run(foreground, Priority::FOREGROUND)
        .expect("focus")
        .expect("already running");
    assert_eq!(switched.runtime_id(), foreground_handle.runtime_id());

    assert_eq!(
        manager.session_state(background),
        Some(SchedulerState::Running),
        "切前台不得让后台会话离开 Running"
    );
    assert_eq!(
        background_handle.process_id(),
        Some(background_pid),
        "后台会话的 pi 进程必须原地不动"
    );
    assert!(background_handle.snapshot().terminal.is_none());

    // 后台会话仍然可用：切走之后照样能接受提交并产出结果。
    stream(&background_handle);
    wait_for_message(&background_handle, "background");
    assert_eq!(background_handle.process_id(), Some(background_pid));

    manager.remove_session(background);
    manager.remove_session(foreground);
}

#[test]
fn closing_a_running_session_lets_a_queued_one_take_the_slot_immediately() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let limits = SchedulerLimits::default();
    let manager = test_manager(limits);
    let mut subscription = manager.subscribe_scheduler();
    let notices = subscription.take_receiver().expect("receiver");
    let sessions: Vec<SessionId> = ["one", "two", "three"]
        .into_iter()
        .map(|name| register(&manager, workspace.path(), name))
        .collect();

    for session in &sessions {
        let _ = manager
            .request_run(*session, Priority::FOREGROUND)
            .expect("request_run");
    }
    assert_eq!(
        manager.session_state(sessions[2]),
        Some(SchedulerState::Queued),
        "并发上限是 {}，第三个必须排队",
        limits.user_session_slots
    );

    // 把待处理通知取空，确保后面观察到的是「补位」这一次变化。
    while notices.try_recv().is_ok() {}

    manager.remove_session(sessions[0]);
    // app 释放运行槽后立刻 tick：不这样做就得等 reaper 轮询（默认 TTL 下最长 45s）。
    manager.tick();

    assert_eq!(
        manager.session_state(sessions[2]),
        Some(SchedulerState::Running),
        "运行槽空出来之后排队会话必须马上补位"
    );
    assert_eq!(
        notices.try_recv(),
        Ok(SchedulerChanged),
        "补位是可见变化，必须唤醒订阅者"
    );
    let report = manager.scheduler_report();
    assert_eq!(report.queued, 0);
    assert!(report.resident_pi <= limits.total_runtime_slots);

    for session in sessions.iter().skip(1) {
        manager.remove_session(*session);
    }
}
