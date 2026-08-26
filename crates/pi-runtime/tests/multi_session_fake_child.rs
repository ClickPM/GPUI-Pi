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

/// R24 视觉验收暴露的崩溃根因：`park` **在调用线程上**完成整套进程交接。
///
/// `park` 收尾时会 `tick()` 一次；有会话在排队时，那一下就地把它提升上来，
/// 连带一次 `switch_session` 往返（复用热进程）或一次冷启动。也就是说 `park`
/// 返回时新会话已经在跑了——这些工作全部发生在调用者的线程上。
/// app 因此**绝不能**在 GPUI 主线程上调用它：单会话时 `tick()` 无事可做所以看不出来，
/// 一旦有排队会话，点一下「挂起」就是整窗口卡死一次进程交接的时间。
#[test]
fn park_finishes_a_queued_handoff_on_the_calling_thread() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let limits = SchedulerLimits {
        warm_idle: 1,
        ..SchedulerLimits::default()
    };
    let manager = test_manager(limits);
    let sessions: Vec<SessionId> = ["p1", "p2", "p3"]
        .into_iter()
        .map(|name| register(&manager, workspace.path(), name))
        .collect();

    let first = run(&manager, sessions[0], Priority::FOREGROUND);
    let _second = run(&manager, sessions[1], Priority::FOREGROUND);
    assert!(
        manager
            .request_run(sessions[2], Priority::FOREGROUND)
            .expect("queueing")
            .is_none(),
        "并发上限 2，第三个必须排队"
    );
    assert_eq!(
        manager.session_state(sessions[2]),
        Some(SchedulerState::Queued)
    );

    // 等到静止再 Park，否则会退化成优雅停机（那条路同样是同步的，只是不走 warm 交接）。
    let deadline = Instant::now() + Duration::from_secs(60);
    while !first.is_quiescent() {
        assert!(Instant::now() < deadline, "Runtime 迟迟没有静止");
        std::thread::sleep(Duration::from_millis(5));
    }
    manager
        .park(sessions[0])
        .unwrap_or_else(|error| panic!("park failed: {error}"));

    // 关键断言：**没有任何额外的 tick 或等待**，排队会话已经在跑了。
    // 这证明进程交接确确实实发生在 `park` 的调用线程上。
    assert_eq!(
        manager.session_state(sessions[2]),
        Some(SchedulerState::Running),
        "park 返回时排队会话已被就地提升——说明交接在调用线程上完成"
    );
    assert_eq!(
        manager.session_state(sessions[0]),
        Some(SchedulerState::Parked)
    );
    assert!(manager.scheduler_report().resident_pi <= limits.total_runtime_slots);

    for session in sessions {
        manager.remove_session(session);
    }
}

/// R24 第十轮审查 P1：改一个已挂起会话的工具预设，必须真的落到它下次启动的参数上。
///
/// 否则 UI 上写着 ReadOnly，恢复出来的进程却还带着挂起前那套更宽的工具——
/// 「显示的权限」与「实际的权限」分家。
#[test]
fn a_parked_session_can_have_its_tool_preset_changed_before_it_resumes() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let manager = test_manager(SchedulerLimits::default());
    let session = register(&manager, workspace.path(), "preset");
    assert_eq!(
        manager.session_descriptor(session).map(|d| d.tool_preset),
        Some(ToolPreset::Inherit)
    );

    manager
        .set_session_tool_preset(session, ToolPreset::ReadOnly)
        .expect("挂起态可以改预设");
    assert_eq!(
        manager.session_descriptor(session).map(|d| d.tool_preset),
        Some(ToolPreset::ReadOnly),
        "改动必须落到描述上，下次启动才会按新预设起进程"
    );

    // 跑起来之后就不许再从这条路改了：进程已经按旧参数起来，只改描述会让
    // 「描述」与「进程实际权限」分家。
    let handle = run(&manager, session, Priority::FOREGROUND);
    let refused = manager
        .set_session_tool_preset(session, ToolPreset::Inherit)
        .expect_err("运行中必须拒绝");
    assert!(refused.contains("重启"), "{refused}");
    assert_eq!(
        manager.session_descriptor(session).map(|d| d.tool_preset),
        Some(ToolPreset::ReadOnly),
        "被拒绝的调用不得改动任何状态"
    );

    drop(handle);
    manager.remove_session(session);
}
