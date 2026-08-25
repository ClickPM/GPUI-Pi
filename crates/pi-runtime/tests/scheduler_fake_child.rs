//! R23 验收：调度器状态机、Resident Pi 上限、Park/Resume 两条路径与 Idle TTL。
//!
//! 全部以 `runtime_fake_child` 为内核，不链接 GPUI、不消耗模型 token。
//! 涉及时间的用例一律用 [`FakeClock`] + 显式 `tick()`，不 sleep、不靠真实时间逼近。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use pi_render::{ConversationDocument, LivePhase};
use pi_runtime::{
    Clock, ComposerMode, ComposerSubmission, FakeClock, Priority, RpcIntent, RuntimeLimits,
    RuntimeManager, SchedulerLimits, SchedulerState, SessionDescriptor, SessionHandle, SessionId,
    ToolPreset,
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

/// 造一个真实存在的会话文件。
///
/// `switch_session` 只能切到**已经落盘**的会话，调度器因此用 `is_file()` 作为
/// 「可否复用热进程」的硬判据；不建这个文件，warm 路径根本不会被选中。
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

fn limits(scheduler: SchedulerLimits) -> RuntimeLimits {
    RuntimeLimits {
        scheduler,
        ..RuntimeLimits::default()
    }
}

/// 用 fake clock 构造的 Manager：没有 reaper 线程，一切推进都由显式 `tick()` 触发。
fn test_manager(scheduler: SchedulerLimits) -> (RuntimeManager, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new());
    let injected: Arc<dyn Clock> = clock.clone();
    let manager = RuntimeManager::with_test_clock(limits(scheduler), injected);
    (manager, clock)
}

/// 等到 Runtime 静止再 Park。
///
/// 启动之后的元数据刷新是一批在跑的作业，此刻 Park 只能退化成优雅停机（进程不进池）。
/// 想断言热进程复用，必须先等这批作业收干净 —— 这是 `SessionHandle::is_quiescent`
/// 存在的原因，也是调用方在生产里该遵守的同一条约束。
fn park_when_quiescent(manager: &RuntimeManager, handle: &SessionHandle, session: SessionId) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !handle.is_quiescent() {
        assert!(Instant::now() < deadline, "Runtime 迟迟没有静止");
        std::thread::sleep(Duration::from_millis(5));
    }
    manager
        .park(session)
        .unwrap_or_else(|error| panic!("park failed: {error}"));
}

#[test]
fn registering_twenty_sessions_starts_no_processes_at_all() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let sessions: Vec<SessionId> = (0..20)
        .map(|index| {
            manager.create_session(
                descriptor(
                    workspace.path(),
                    Some(session_file(workspace.path(), &format!("s{index}"))),
                ),
                empty_document(&format!("s{index}"), workspace.path()),
            )
        })
        .collect();

    let report = manager.scheduler_report();
    assert_eq!(report.parked, 20, "登记本身必须是纯内存操作");
    assert_eq!(report.resident_pi, 0);
    assert!(
        sessions
            .iter()
            .all(|id| manager.session_state(*id) == Some(SchedulerState::Parked))
    );
}

#[test]
fn twenty_sessions_asking_to_run_never_exceed_the_resident_pi_budget() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let scheduler = SchedulerLimits {
        user_session_slots: 2,
        total_runtime_slots: 3,
        warm_idle: 1,
        idle_ttl: Duration::from_secs(180),
        queue_capacity: 64,
        aging_step: Duration::from_secs(5),
    };
    let (manager, _clock) = test_manager(scheduler);
    let sessions: Vec<SessionId> = (0..20)
        .map(|index| {
            manager.create_session(
                descriptor(
                    workspace.path(),
                    Some(session_file(workspace.path(), &format!("s{index}"))),
                ),
                empty_document(&format!("s{index}"), workspace.path()),
            )
        })
        .collect();

    let assert_within_budget = |where_: &str| {
        let report = manager.scheduler_report();
        assert!(
            report.resident_pi <= scheduler.total_runtime_slots,
            "{where_}: 常驻 pi {} 超过上限 {}",
            report.resident_pi,
            scheduler.total_runtime_slots
        );
        assert!(
            report.running <= scheduler.user_session_slots,
            "{where_}: 同时运行 {} 超过用户并发 {}",
            report.running,
            scheduler.user_session_slots
        );
        report
    };

    for session in &sessions {
        manager
            .request_run(*session, Priority::FOREGROUND)
            .expect("request_run 不应报错");
        assert_within_budget("请求运行过程中");
    }

    let report = assert_within_budget("全部请求之后");
    assert_eq!(report.running, 2);
    assert_eq!(report.queued, 18);
    assert_eq!(report.parked, 0);

    // 逐个让出运行槽：排队的会话必须被提升，且上限在整个排空过程中持续成立。
    let mut promoted = 0;
    for session in &sessions {
        if manager.session_state(*session) != Some(SchedulerState::Running) {
            continue;
        }
        manager.park(*session).expect("park 正在运行的会话");
        assert_within_budget("Park 之后");
        promoted += 1;
        if promoted == 4 {
            break;
        }
    }
    assert!(
        manager.scheduler_report().running > 0,
        "让出运行槽之后必须有排队会话被提升"
    );

    for session in sessions {
        manager.remove_session(session);
    }
    assert_eq!(manager.scheduler_report().resident_pi, 0);
}

#[test]
fn resume_reuses_a_warm_runtime_through_switch_session() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let path = session_file(workspace.path(), "warm");
    let session = manager.create_session(
        descriptor(workspace.path(), Some(path)),
        empty_document("warm", workspace.path()),
    );

    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("cold start")
        .expect("slot available");
    let first_pid = handle.process_id().expect("fake child pid");
    let before = manager.scheduler_report();
    assert_eq!(before.cold_starts, 1);
    assert_eq!(before.warm_resumes, 0);

    park_when_quiescent(&manager, &handle, session);
    let parked = manager.scheduler_report();
    assert_eq!(manager.session_state(session), Some(SchedulerState::Parked));
    assert_eq!(parked.warm, 1, "空闲且已落盘的 Runtime 必须进 warm pool");
    assert_eq!(parked.warm_parks, 1);
    assert_eq!(
        parked.resident_pi, 1,
        "热进程仍然占着一个常驻名额，不能从预算里消失"
    );

    let resumed = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("resume")
        .expect("slot available");
    let after = manager.scheduler_report();
    assert_eq!(after.warm_resumes, 1, "Resume 必须优先复用热进程");
    assert_eq!(after.cold_starts, 1, "复用路径不得触发冷启动");
    assert_eq!(after.warm, 0);
    assert_eq!(
        resumed.process_id(),
        Some(first_pid),
        "复用的必须是同一个 pi 进程，而不是重启一个新的"
    );
    // `RuntimeId` 是进程宿主身份，会换；`SessionId` 是会话身份，必须恒定。
    assert_eq!(resumed.session_id(), session);
    assert_eq!(resumed.session_id(), handle.session_id());

    manager.remove_session(session);
}

#[test]
fn resume_falls_back_to_a_cold_start_when_the_pool_has_nothing_compatible() {
    let workspace = tempfile::tempdir().expect("tempdir");
    // warm_idle = 0：Park 只能优雅停机，Resume 因此只剩冷启动一条路。
    let (manager, _clock) = test_manager(SchedulerLimits {
        warm_idle: 0,
        ..SchedulerLimits::default()
    });
    let path = session_file(workspace.path(), "cold");
    let session = manager.create_session(
        descriptor(workspace.path(), Some(path)),
        empty_document("cold", workspace.path()),
    );

    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("cold start")
        .expect("slot available");
    let first_pid = handle.process_id().expect("fake child pid");

    manager.park(session).expect("park");
    let parked = manager.scheduler_report();
    assert_eq!(parked.warm, 0, "池容量为 0 时不得留下热进程");
    assert_eq!(parked.resident_pi, 0);

    let resumed = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("resume")
        .expect("slot available");
    let after = manager.scheduler_report();
    assert_eq!(after.cold_starts, 2, "无热进程可复用时必须冷启动");
    assert_eq!(after.warm_resumes, 0);
    assert_ne!(
        resumed.process_id(),
        Some(first_pid),
        "冷启动必然是一个新进程"
    );

    manager.remove_session(session);
}

#[test]
fn a_session_without_a_persisted_file_never_enters_the_warm_pool() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    // fresh 会话没有会话文件，`switch_session` 无从切起 —— 它的进程不能进池。
    let session = manager.create_session(
        descriptor(workspace.path(), None),
        empty_document("fresh", workspace.path()),
    );
    manager
        .request_run(session, Priority::FOREGROUND)
        .expect("cold start")
        .expect("slot available");

    manager.park(session).expect("park");
    let report = manager.scheduler_report();
    assert_eq!(report.warm, 0, "未落盘的会话不得留下热进程");
    assert_eq!(report.warm_parks, 0);
    assert_eq!(manager.session_state(session), Some(SchedulerState::Parked));

    manager.remove_session(session);
}

#[test]
fn idle_warm_runtimes_are_reaped_exactly_at_the_ttl() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let ttl = Duration::from_secs(60);
    let (manager, clock) = test_manager(SchedulerLimits {
        idle_ttl: ttl,
        ..SchedulerLimits::default()
    });
    let path = session_file(workspace.path(), "ttl");
    let session = manager.create_session(
        descriptor(workspace.path(), Some(path)),
        empty_document("ttl", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
    park_when_quiescent(&manager, &handle, session);
    assert_eq!(manager.scheduler_report().warm, 1);

    clock.advance(ttl - Duration::from_secs(1));
    manager.tick();
    let before = manager.scheduler_report();
    assert_eq!(before.warm, 1, "TTL 未到就回收等于白白付一次冷启动");
    assert_eq!(before.idle_reaped, 0);

    clock.advance(Duration::from_secs(2));
    manager.tick();
    let after = manager.scheduler_report();
    assert_eq!(after.warm, 0, "TTL 到点必须回收");
    assert_eq!(after.idle_reaped, 1);
    assert_eq!(after.resident_pi, 0, "回收之后常驻名额必须归还");

    manager.remove_session(session);
}

#[test]
fn the_wait_queue_rejects_without_touching_session_state_when_full() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits {
        user_session_slots: 1,
        total_runtime_slots: 1,
        warm_idle: 0,
        queue_capacity: 2,
        ..SchedulerLimits::default()
    });
    let sessions: Vec<SessionId> = (0..4)
        .map(|index| {
            manager.create_session(
                descriptor(
                    workspace.path(),
                    Some(session_file(workspace.path(), &format!("q{index}"))),
                ),
                empty_document(&format!("q{index}"), workspace.path()),
            )
        })
        .collect();

    manager
        .request_run(sessions[0], Priority::FOREGROUND)
        .expect("first session runs")
        .expect("slot available");
    for session in &sessions[1..3] {
        assert!(
            manager
                .request_run(*session, Priority::FOREGROUND)
                .expect("queued")
                .is_none()
        );
    }

    let rejected = match manager.request_run(sessions[3], Priority::FOREGROUND) {
        Err(error) => error,
        Ok(_) => panic!("队列已满必须报错而不是无限排队"),
    };
    assert!(rejected.contains("等待队列已满"), "{rejected}");
    assert_eq!(
        manager.session_state(sessions[3]),
        Some(SchedulerState::Parked),
        "被拒绝的会话状态必须保持不变"
    );
    let report = manager.scheduler_report();
    assert_eq!(report.queued, 2);
    assert_eq!(report.running, 1);

    for session in sessions {
        manager.remove_session(session);
    }
}

#[test]
fn aging_decides_promotion_order_when_a_slot_frees_up() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, clock) = test_manager(SchedulerLimits {
        user_session_slots: 1,
        total_runtime_slots: 1,
        warm_idle: 0,
        queue_capacity: 8,
        aging_step: Duration::from_secs(5),
        ..SchedulerLimits::default()
    });
    let make = |name: &str| {
        manager.create_session(
            descriptor(workspace.path(), Some(session_file(workspace.path(), name))),
            empty_document(name, workspace.path()),
        )
    };
    let occupant = make("occupant");
    let background = make("background");
    let foreground = make("foreground");

    manager
        .request_run(occupant, Priority::FOREGROUND)
        .expect("occupant runs")
        .expect("slot available");
    assert!(
        manager
            .request_run(background, Priority::BACKGROUND)
            .expect("queued")
            .is_none()
    );
    // 后台会话已经等了 100s（20 个 aging 步），此时才来一个全新的前台会话。
    clock.advance(Duration::from_secs(100));
    assert!(
        manager
            .request_run(foreground, Priority::FOREGROUND)
            .expect("queued")
            .is_none()
    );

    manager.stop_session(occupant);
    assert_eq!(
        manager.session_state(background),
        Some(SchedulerState::Running),
        "等待足够久的后台会话必须反超刚入队的前台会话"
    );
    assert_eq!(
        manager.session_state(foreground),
        Some(SchedulerState::Queued)
    );

    for session in [occupant, background, foreground] {
        manager.remove_session(session);
    }
}

#[test]
fn the_reaper_thread_collects_expired_warm_runtimes_without_any_manual_tick() {
    let workspace = tempfile::tempdir().expect("tempdir");
    // 真实时钟 + 极短 TTL：这一条专门验证 reaper 线程本身在跑，不能用 fake clock 代替。
    let manager = RuntimeManager::new(limits(SchedulerLimits {
        idle_ttl: Duration::from_millis(300),
        ..SchedulerLimits::default()
    }));
    let path = session_file(workspace.path(), "reaper");
    let session = manager.create_session(
        descriptor(workspace.path(), Some(path)),
        empty_document("reaper", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
    park_when_quiescent(&manager, &handle, session);
    // 不断言「此刻池里有一个」：TTL 只有 300ms，reaper 完全可能在这一行之前就收走了。
    // 改用两个**单调计数**，它们不会被竞态吞掉。
    let parked_report = manager.scheduler_report();
    assert_eq!(
        parked_report.warm_parks, 1,
        "先确认这次 Park 确实把进程交给了池子；报表={parked_report:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    while manager.scheduler_report().idle_reaped == 0 {
        assert!(Instant::now() < deadline, "reaper 线程没有回收过期热进程");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(manager.scheduler_report().warm, 0);

    manager.remove_session(session);
}

#[test]
fn a_crashed_runtime_releases_its_run_slot() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits {
        user_session_slots: 1,
        total_runtime_slots: 1,
        warm_idle: 0,
        ..SchedulerLimits::default()
    });
    let session = manager.create_session(
        descriptor(
            workspace.path(),
            Some(session_file(workspace.path(), "crash")),
        ),
        empty_document("crash", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");

    // 直接杀掉进程树，模拟内核崩溃：pump 会把终态写进 Snapshot，调度器靠 tick 回收运行槽。
    let pid = handle.process_id().expect("fake child pid");
    pi_rpc::kill_process_tree(pid).expect("kill fake child");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        manager.tick();
        if manager.session_state(session) == Some(SchedulerState::Failed) {
            break;
        }
        assert!(Instant::now() < deadline, "崩溃的 Runtime 没有被判 Failed");
        std::thread::sleep(Duration::from_millis(20));
    }
    let report = manager.scheduler_report();
    assert_eq!(report.failed, 1);
    assert_eq!(report.resident_pi, 0, "崩溃会话不得永久占着运行槽");
    assert_eq!(report.slots.user, 0);
    assert!(manager.session_failure(session).is_some());

    // Failed 是可重试终态：槽位已经归还，再次请求必须能起来。
    manager
        .request_run(session, Priority::FOREGROUND)
        .expect("retry after failure")
        .expect("slot available");
    assert_eq!(
        manager.session_state(session),
        Some(SchedulerState::Running)
    );

    manager.remove_session(session);
}

/// Park 是「让出进程」，不是「打断请求」。
///
/// `runtime_fake_child` 对未知 prompt 只回一条成功响应、不发 `agent_settled`，
/// 因此 dispatch 之后 reducer 会稳定停在 `Running`，这条断言不依赖时序。
#[test]
fn parking_a_busy_session_is_refused_instead_of_dropping_the_in_flight_request() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let session = manager.create_session(
        descriptor(
            workspace.path(),
            Some(session_file(workspace.path(), "busy")),
        ),
        empty_document("busy", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
    handle
        .dispatch(
            RpcIntent::Prompt,
            Some(ComposerSubmission {
                message: "hold".to_owned(),
                images: Vec::new(),
            }),
            ComposerMode::FollowUp,
        )
        .expect("dispatch");
    assert_eq!(handle.snapshot().phase, LivePhase::Running);

    let refused = manager
        .park(session)
        .expect_err("运行中的会话不得被静默 Park 掉");
    assert!(refused.contains("仍在执行请求"), "{refused}");
    assert_eq!(
        manager.session_state(session),
        Some(SchedulerState::Running),
        "被拒绝的 Park 不得改变会话状态"
    );
    assert_eq!(manager.scheduler_report().warm, 0);

    // 想强行结束仍然有明确入口。
    manager.stop_session(session);
    assert_eq!(manager.session_state(session), Some(SchedulerState::Parked));
    manager.remove_session(session);
}

// ---------------------------------------------------------------------------
// 以下为独立代码审查 findings 的回归测试。每条对应审查报告里的一项。
// ---------------------------------------------------------------------------

/// 审查 P1-1：`restart_with_tools` 换掉工具预设后，调度器必须拿到新参数。
///
/// 否则 Park 会用过期预设算 `WarmKey`：一个实际带 `--tools full` 的进程被当成
/// ReadOnly 放进池子，再被另一个 ReadOnly 会话接管 —— 写权限就这么跨会话漏了出去。
#[test]
fn a_tool_restart_updates_the_descriptor_the_scheduler_hands_off() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let path = session_file(workspace.path(), "preset");
    let mut readonly = descriptor(workspace.path(), Some(path.clone()));
    readonly.tool_preset = ToolPreset::ReadOnly;
    let session = manager.create_session(readonly, empty_document("preset", workspace.path()));
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
    assert_eq!(
        manager
            .session_descriptor(session)
            .expect("descriptor")
            .tool_preset,
        ToolPreset::ReadOnly
    );

    handle
        .restart_with_tools(
            fake_binary(),
            Some(path.clone()),
            workspace.path().to_path_buf(),
            empty_document("preset", workspace.path()),
            ToolPreset::Full,
        )
        .expect("tool restart accepted");
    // 换进程是控制通道上的异步作业；epoch 递增即代表新进程已经装上。
    let deadline = Instant::now() + Duration::from_secs(30);
    while handle.snapshot().epoch < 2 {
        assert!(Instant::now() < deadline, "工具预设重启没有完成");
        std::thread::sleep(Duration::from_millis(10));
    }
    // 复审（三轮）P2：**还没 Park** 的时候就必须报新预设。Slot 上那份要等 Park / stop
    // 才同步，读它会让调用方以为一个高权限进程还是只读的。
    assert_eq!(
        manager
            .session_descriptor(session)
            .expect("descriptor")
            .tool_preset,
        ToolPreset::Full,
        "Runtime 在跑时必须返回它自己那份权威参数"
    );

    park_when_quiescent(&manager, &handle, session);
    assert_eq!(
        manager
            .session_descriptor(session)
            .expect("descriptor")
            .tool_preset,
        ToolPreset::Full,
        "Park 必须按 Runtime 实际跑的参数留档，而不是启动时那份"
    );

    // 关键断言：ReadOnly 的另一个会话绝不能接管这个 Full 权限的热进程。
    let mut other = descriptor(
        workspace.path(),
        Some(session_file(workspace.path(), "preset-other")),
    );
    other.tool_preset = ToolPreset::ReadOnly;
    let victim = manager.create_session(other, empty_document("preset-other", workspace.path()));
    manager
        .request_run(victim, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
    let report = manager.scheduler_report();
    assert_eq!(
        report.warm_resumes, 0,
        "工具预设不同的会话不得复用热进程：那等于把写权限漏给只读会话"
    );
    assert_eq!(report.cold_starts, 2);

    manager.remove_session(session);
    manager.remove_session(victim);
}

/// 审查 P1-2：`stop_session` 必须像 `park` 一样先把身份与历史收进 Slot。
///
/// 覆盖边界：这里锁住的是**历史**不丢；`session_path` 走的是同一个
/// `apply_captured_state`，由 `park` 的用例覆盖。
#[test]
fn stopping_a_session_keeps_its_history_for_the_next_run() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let session = manager.create_session(
        descriptor(
            workspace.path(),
            Some(session_file(workspace.path(), "history")),
        ),
        empty_document("history", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
    assert!(handle.snapshot().document.messages.is_empty());

    // fake child 的 "stream" 会产出一条完整的 assistant 消息并以 agent_settled 收尾。
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
    let deadline = Instant::now() + Duration::from_secs(60);
    while handle.snapshot().document.messages.is_empty() {
        assert!(
            Instant::now() < deadline,
            "fake child 没有产出 assistant 消息"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let produced = handle.snapshot().document.messages.len();

    manager.stop_session(session);
    let resumed = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("resume")
        .expect("slot available");
    assert_eq!(
        resumed.snapshot().document.messages.len(),
        produced,
        "stop_session 之后重新运行必须接着原来的会话，而不是重开一个空的"
    );

    manager.remove_session(session);
}

/// 审查 P1-2（另一半）：崩溃回收同样要留档，否则 `Failed` 之后的重试会丢掉整段对话。
#[test]
fn a_crash_retry_resumes_instead_of_starting_a_blank_conversation() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let session = manager.create_session(
        descriptor(
            workspace.path(),
            Some(session_file(workspace.path(), "crash-history")),
        ),
        empty_document("crash-history", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
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
    let deadline = Instant::now() + Duration::from_secs(60);
    while handle.snapshot().document.messages.is_empty() {
        assert!(
            Instant::now() < deadline,
            "fake child 没有产出 assistant 消息"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let produced = handle.snapshot().document.messages.len();

    let pid = handle.process_id().expect("fake child pid");
    pi_rpc::kill_process_tree(pid).expect("kill fake child");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        manager.tick();
        if manager.session_state(session) == Some(SchedulerState::Failed) {
            break;
        }
        assert!(Instant::now() < deadline, "崩溃的 Runtime 没有被判 Failed");
        std::thread::sleep(Duration::from_millis(20));
    }

    let retried = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("retry")
        .expect("slot available");
    assert_eq!(
        retried.snapshot().document.messages.len(),
        produced,
        "崩溃重试必须接着原来的会话"
    );

    manager.remove_session(session);
}

/// 审查 P2-1：warm 池容量要在调度锁内复检。
///
/// 覆盖边界：这是一条竞态用例，它**不保证**每次都撞进那个窗口；但只要修复被回退，
/// 它在压力下就会红，而且永远不会假红。
#[test]
fn concurrent_parks_never_overfill_the_warm_pool() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let scheduler = SchedulerLimits {
        user_session_slots: 2,
        total_runtime_slots: 3,
        warm_idle: 1,
        ..SchedulerLimits::default()
    };
    let (manager, _clock) = test_manager(scheduler);
    let manager = Arc::new(manager);
    let sessions: Vec<(SessionId, SessionHandle)> = (0..2)
        .map(|index| {
            let id = manager.create_session(
                descriptor(
                    workspace.path(),
                    Some(session_file(workspace.path(), &format!("race{index}"))),
                ),
                empty_document(&format!("race{index}"), workspace.path()),
            );
            let handle = manager
                .request_run(id, Priority::FOREGROUND)
                .expect("start")
                .expect("slot available");
            (id, handle)
        })
        .collect();
    // 先各自静止，再同时 Park：这样两次 Park 都**有资格**进池，才真正压到容量复检那条路。
    let deadline = Instant::now() + Duration::from_secs(60);
    while sessions.iter().any(|(_, handle)| !handle.is_quiescent()) {
        assert!(Instant::now() < deadline, "Runtime 迟迟没有静止");
        std::thread::sleep(Duration::from_millis(5));
    }

    let barrier = Arc::new(std::sync::Barrier::new(sessions.len()));
    let threads: Vec<_> = sessions
        .iter()
        .map(|(session, _)| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            let session = *session;
            std::thread::spawn(move || {
                barrier.wait();
                manager.park(session)
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("park thread").expect("park");
    }

    let report = manager.scheduler_report();
    assert!(
        report.warm <= scheduler.warm_idle,
        "并发 Park 把热进程池撑到了 {}，上限是 {}",
        report.warm,
        scheduler.warm_idle
    );
    assert!(report.resident_pi <= scheduler.total_runtime_slots);

    for (session, _) in sessions {
        manager.remove_session(session);
    }
}

/// 审查 P2-2：兼容通道的活跃会话替换必须串行。
///
/// 并发 `start_fresh` 各自读到同一个 `previous` 时，先起来的那个 Runtime 会失去归属：
/// 它不再是 `active_user`，`stop_user` 对它是空操作，进程从此没人回收。
#[test]
fn concurrent_compat_starts_leave_exactly_one_live_runtime() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let manager = Arc::new(RuntimeManager::new(limits(SchedulerLimits::default())));
    let first = manager
        .start_fresh(
            fake_binary(),
            workspace.path().to_path_buf(),
            empty_document("compat-0", workspace.path()),
            ToolPreset::Inherit,
            None,
        )
        .expect("first compat session");
    assert_eq!(manager.active_user_session(), Some(first.session_id()));

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|index| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            let cwd = workspace.path().to_path_buf();
            std::thread::spawn(move || {
                barrier.wait();
                manager
                    .start_fresh(
                        fake_binary(),
                        cwd.clone(),
                        empty_document(&format!("compat-{index}"), &cwd),
                        ToolPreset::Inherit,
                        None,
                    )
                    .map(|handle| handle.session_id())
            })
        })
        .collect();
    let started: Vec<SessionId> = threads
        .into_iter()
        .map(|thread| thread.join().expect("compat thread").expect("start_fresh"))
        .collect();

    let report = manager.scheduler_report();
    assert_eq!(
        report.running, 1,
        "兼容通道任何时刻只应有一个活跃 Runtime，实际有 {}",
        report.running
    );
    assert_eq!(report.resident_pi, 1);
    let active = manager.active_user_session().expect("active session");
    assert!(
        started.contains(&active),
        "active_user 必须指向真正活下来的那个会话"
    );

    let handle = manager.session_handle(active).expect("active handle");
    manager.stop_user(handle.runtime_id());
    assert_eq!(
        manager.scheduler_report().resident_pi,
        0,
        "停掉活跃会话之后不得留下进程"
    );
}

/// 审查 P2-3：`user_session_slots = 1` 下新会话启动失败，旧会话必须还能用。
#[test]
fn a_failed_single_slot_start_rolls_the_previous_session_back() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let manager = RuntimeManager::new(limits(SchedulerLimits {
        user_session_slots: 1,
        total_runtime_slots: 1,
        warm_idle: 0,
        ..SchedulerLimits::default()
    }));
    let previous = manager
        .start_fresh(
            fake_binary(),
            workspace.path().to_path_buf(),
            empty_document("rollback-old", workspace.path()),
            ToolPreset::Inherit,
            None,
        )
        .expect("previous session");
    let previous_session = previous.session_id();

    let error = match manager.start_fresh(
        PathBuf::from("definitely-missing-pi-runtime-binary.exe"),
        workspace.path().to_path_buf(),
        empty_document("rollback-new", workspace.path()),
        ToolPreset::Inherit,
        None,
    ) {
        Err(error) => error,
        Ok(_) => panic!("missing binary must not start"),
    };
    assert!(error.contains("旧会话已恢复"), "{error}");

    assert_eq!(
        manager.active_user_session(),
        Some(previous_session),
        "回滚之后 active_user 必须仍然指向旧会话"
    );
    assert_eq!(
        manager.session_state(previous_session),
        Some(SchedulerState::Running),
        "旧会话必须被拉回运行态"
    );
    let report = manager.scheduler_report();
    assert_eq!(report.running, 1);
    assert_eq!(report.failed, 0, "失败的那次尝试不得留下 Failed 残迹");

    let restored = manager
        .session_handle(previous_session)
        .expect("旧会话可以重新取到句柄");
    manager.stop_user(restored.runtime_id());
    assert_eq!(manager.scheduler_report().resident_pi, 0);
}

// ---------------------------------------------------------------------------
// 复审（第二轮）findings 的回归测试。
//
// 三条都是竞态用例：它们**不保证**每次都撞进窗口，但只要修复被回退就会在压力下红，
// 而且永远不会假红。带看门狗的两条同时把「死锁」变成失败而不是挂起 —— 挂起在 CI 上
// 只会表现成超时，读不出原因。
// ---------------------------------------------------------------------------

/// 在 `budget` 内 join 全部线程；超时就 panic，而不是让测试永远挂着。
fn join_within(threads: Vec<std::thread::JoinHandle<()>>, budget: Duration, what: &str) {
    let deadline = Instant::now() + budget;
    for thread in threads {
        while !thread.is_finished() {
            assert!(
                Instant::now() < deadline,
                "{what}：线程在 {budget:?} 内没有结束，极可能是死锁"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        thread.join().expect("worker thread");
    }
}

/// 复审 P1-1 / P1-5：`park` 与 `request_run` 争抢同一个会话时，
/// 既不能挤掉对方刚装上的 Runtime，也不能在旧进程还活着时归还运行槽。
#[test]
fn concurrent_park_and_resume_never_orphan_a_runtime_or_break_the_bound() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let scheduler = SchedulerLimits {
        user_session_slots: 1,
        total_runtime_slots: 1,
        warm_idle: 1,
        ..SchedulerLimits::default()
    };
    let (manager, clock) = test_manager(scheduler);
    let manager = Arc::new(manager);
    let session = manager.create_session(
        descriptor(
            workspace.path(),
            Some(session_file(workspace.path(), "tug-of-war")),
        ),
        empty_document("tug-of-war", workspace.path()),
    );
    manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");

    let parker = {
        let manager = Arc::clone(&manager);
        std::thread::spawn(move || {
            for _ in 0..40 {
                // 忙碌 / 被接管都是合法结果，这里只要求不 panic、不破坏不变量。
                let _ = manager.park(session);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let resumer = {
        let manager = Arc::clone(&manager);
        std::thread::spawn(move || {
            for _ in 0..40 {
                let _ = manager.request_run(session, Priority::FOREGROUND);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    join_within(
        vec![parker, resumer],
        Duration::from_secs(120),
        "park / request_run 拉锯",
    );

    let report = manager.scheduler_report();
    assert!(
        report.resident_pi <= scheduler.total_runtime_slots,
        "常驻 pi {} 超过上限 {}；报表={report:?}",
        report.resident_pi,
        scheduler.total_runtime_slots
    );
    assert!(
        report.slots.user <= scheduler.user_session_slots,
        "{report:?}"
    );
    assert!(
        report.slots.resident <= scheduler.total_runtime_slots,
        "运行槽计数漏还或超发：{report:?}"
    );

    manager.remove_session(session);
    // 注销会话不会顺手杀掉池里的热进程 —— 它不属于任何会话，由 Idle TTL 回收。
    // 把 fake clock 推过 TTL 再收：这之后槽位必须彻底归零，任何一次漏还都会在这里现形。
    clock.advance(scheduler.idle_ttl + Duration::from_secs(1));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        manager.tick();
        let report = manager.scheduler_report();
        if report.resident_pi == 0 && report.slots == Default::default() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "移除会话并越过 Idle TTL 之后运行槽没有归零：{report:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 复审 P1-3：`stop_user` 的「匹配」与「清空 `active_user`」必须在同一次持锁里完成。
///
/// 否则并发的 `start_fresh` 会在缝隙里装上新会话，随后那句无条件置 `None` 把它抹掉，
/// 它的 Runtime 从此再也停不掉。
#[test]
fn concurrent_stop_user_and_start_fresh_keep_active_user_consistent() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let manager = Arc::new(RuntimeManager::new(limits(SchedulerLimits::default())));
    let first = manager
        .start_fresh(
            fake_binary(),
            workspace.path().to_path_buf(),
            empty_document("stopper-0", workspace.path()),
            ToolPreset::Inherit,
            None,
        )
        .expect("first session");

    let stopper = {
        let manager = Arc::clone(&manager);
        let runtime_id = first.runtime_id();
        std::thread::spawn(move || {
            for _ in 0..20 {
                manager.stop_user(runtime_id);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let starter = {
        let manager = Arc::clone(&manager);
        let cwd = workspace.path().to_path_buf();
        std::thread::spawn(move || {
            for index in 0..6 {
                let _ = manager.start_fresh(
                    fake_binary(),
                    cwd.clone(),
                    empty_document(&format!("starter-{index}"), &cwd),
                    ToolPreset::Inherit,
                    None,
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    join_within(
        vec![stopper, starter],
        Duration::from_secs(120),
        "stop_user / start_fresh 拉锯",
    );

    // 不变量：`active_user` 要么为空，要么指向一个真的在跑、且能被 `stop_user` 停掉的会话。
    match manager.active_user_session() {
        None => {}
        Some(active) => {
            assert_eq!(
                manager.session_state(active),
                Some(SchedulerState::Running),
                "active_user 指向的会话必须真的在跑"
            );
            let handle = manager.session_handle(active).expect("active handle");
            manager.stop_user(handle.runtime_id());
            assert_eq!(
                manager.active_user_session(),
                None,
                "stop_user 必须能停掉它 —— 停不掉就说明它已经失去归属"
            );
        }
    }
    let report = manager.scheduler_report();
    assert_eq!(
        report.running, 0,
        "收尾后不得留下没人认领的 Runtime：{report:?}"
    );
    assert_eq!(report.resident_pi, 0, "{report:?}");
}

/// 复审 P1-2：`descriptor` 与 `state` 的锁序必须唯一。
///
/// `capture_runtime_state` 走 descriptor → state，而换工具预设那条路曾经走 state →
/// descriptor；两边一撞就是互等到死。这条用例把两条路径对撞，并用看门狗把死锁变成失败。
#[test]
fn a_tool_restart_racing_a_stop_never_deadlocks() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let manager = Arc::new(manager);
    let path = session_file(workspace.path(), "lock-order");
    let session = manager.create_session(
        descriptor(workspace.path(), Some(path.clone())),
        empty_document("lock-order", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");

    let restarter = {
        let handle = handle.clone();
        let cwd = workspace.path().to_path_buf();
        let path = path.clone();
        std::thread::spawn(move || {
            for index in 0..20 {
                let preset = if index % 2 == 0 {
                    ToolPreset::Full
                } else {
                    ToolPreset::ReadOnly
                };
                // 停止之后必然失败，这里只关心「不会死锁」。
                let _ = handle.restart_with_tools(
                    fake_binary(),
                    Some(path.clone()),
                    cwd.clone(),
                    empty_document("lock-order", &cwd),
                    preset,
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let stopper = {
        let manager = Arc::clone(&manager);
        std::thread::spawn(move || {
            for _ in 0..20 {
                // `stop_session` 会走 `capture_runtime_state`（descriptor → state）。
                manager.stop_session(session);
                let _ = manager.request_run(session, Priority::FOREGROUND);
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    join_within(
        vec![restarter, stopper],
        Duration::from_secs(120),
        "restart_with_tools / stop_session 对撞",
    );

    manager.remove_session(session);
    assert_eq!(manager.scheduler_report().resident_pi, 0);
}

/// 复审（三轮）P2：注销兼容通道的活跃会话时，`active_user` 指针必须一起清掉。
///
/// 留着它，`active_user_session()` 会返回一个已经不存在的 id，而 `stop_user()` 再也匹配不上。
#[test]
fn removing_the_active_session_clears_the_compatibility_pointer() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let manager = RuntimeManager::new(limits(SchedulerLimits::default()));
    let handle = manager
        .start_fresh(
            fake_binary(),
            workspace.path().to_path_buf(),
            empty_document("active", workspace.path()),
            ToolPreset::Inherit,
            None,
        )
        .expect("compat session");
    let session = handle.session_id();
    assert_eq!(manager.active_user_session(), Some(session));

    manager.remove_session(session);
    assert_eq!(
        manager.active_user_session(),
        None,
        "注销活跃会话之后指针不得留在一个已经不存在的 id 上"
    );
    assert!(manager.session_state(session).is_none());
    assert_eq!(manager.scheduler_report().resident_pi, 0);
}

/// 复审（三轮）P1：`stop_session` 撞上正在进行的 `restart_with_tools`。
///
/// 那一刻 `state.client` 已经是 `None`（旧 client 攥在替换作业手里），`shutdown_entry`
/// 什么也关不掉。若它照样宣布「进程已释放」，运行槽会在旧进程还活着时被让出去；
/// 反过来若谁都不置位，运行槽就再也回不来。两种失败这条用例都能抓到。
#[test]
fn stopping_a_session_while_a_tool_restart_is_in_flight_still_frees_the_slot() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let path = session_file(workspace.path(), "restart-stop");
    let session = manager.create_session(
        descriptor(workspace.path(), Some(path.clone())),
        empty_document("restart-stop", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");

    // 投递换进程作业后立刻停会话：两者必然重叠在同一个 Runtime 上。
    handle
        .restart_with_tools(
            fake_binary(),
            Some(path),
            workspace.path().to_path_buf(),
            empty_document("restart-stop", workspace.path()),
            ToolPreset::Full,
        )
        .expect("tool restart accepted");
    manager.stop_session(session);

    // 运行槽最终必须回来 —— 早还会超限，不还就是永久泄漏。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        manager.tick();
        let report = manager.scheduler_report();
        if report.resident_pi == 0 && report.slots == Default::default() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "换进程期间被停掉的会话没有归还运行槽：{report:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // 槽位回来了就必须能重新开跑。
    manager
        .request_run(session, Priority::FOREGROUND)
        .expect("restart after the stop")
        .expect("slot available");
    manager.remove_session(session);
}

/// 复审（三轮）P1：同一个会话被并发 `stop_session` 时，后到的那次必须让路。
///
/// 后到者若照样走完收尾，会把前一次的 lease 提前归还，甚至在会话被重新拉起之后
/// 覆盖新 Runtime 的状态。
#[test]
fn concurrent_stop_session_calls_do_not_double_finalize() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager(SchedulerLimits::default());
    let manager = Arc::new(manager);
    let session = manager.create_session(
        descriptor(
            workspace.path(),
            Some(session_file(workspace.path(), "double-stop")),
        ),
        empty_document("double-stop", workspace.path()),
    );
    manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");

    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..3)
        .map(|_| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                manager.stop_session(session);
            })
        })
        .collect();
    join_within(threads, Duration::from_secs(120), "并发 stop_session");

    assert_eq!(manager.session_state(session), Some(SchedulerState::Parked));
    let report = manager.scheduler_report();
    assert_eq!(report.resident_pi, 0, "{report:?}");
    assert_eq!(
        report.slots,
        Default::default(),
        "重复收尾会把别人的运行槽也一起还掉：{report:?}"
    );

    // 还能正常重开，说明状态没有被踩坏。
    manager
        .request_run(session, Priority::FOREGROUND)
        .expect("restart after concurrent stops")
        .expect("slot available");
    manager.remove_session(session);
}

/// 复审（四轮）P2：`resident_pi` 必须始终等于运行槽计数。
///
/// 按状态桶求和会在拆除窗口里少报：Park 兜底停机、warm 淘汰、TTL 回收都是「先从集合里
/// 摘走、再到锁外 shutdown」，那一瞬间谁都不认领这个进程，报表就会谎称还有余量。
#[test]
fn resident_pi_always_tracks_the_lease_count() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let scheduler = SchedulerLimits {
        idle_ttl: Duration::from_secs(60),
        ..SchedulerLimits::default()
    };
    let (manager, clock) = test_manager(scheduler);
    let check = |where_: &str| {
        let report = manager.scheduler_report();
        assert_eq!(
            report.resident_pi, report.slots.resident,
            "{where_}: 常驻数与运行槽计数对不上：{report:?}"
        );
        assert!(
            report.resident_pi <= scheduler.total_runtime_slots,
            "{where_}: {report:?}"
        );
        report
    };

    check("空 Manager");
    let session = manager.create_session(
        descriptor(
            workspace.path(),
            Some(session_file(workspace.path(), "lease-count")),
        ),
        empty_document("lease-count", workspace.path()),
    );
    check("登记之后");

    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("start")
        .expect("slot available");
    assert_eq!(check("运行中").resident_pi, 1);

    park_when_quiescent(&manager, &handle, session);
    let parked = check("Park 到热进程池之后");
    assert_eq!(parked.warm, 1);
    assert_eq!(parked.resident_pi, 1, "热进程照样占名额");

    clock.advance(scheduler.idle_ttl + Duration::from_secs(1));
    manager.tick();
    assert_eq!(check("TTL 回收之后").resident_pi, 0);

    manager.remove_session(session);
    assert_eq!(check("注销之后").resident_pi, 0);
}
