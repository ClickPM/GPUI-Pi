//! R25 验收：显式关停入口（关闭 BACKLOG #19 / #25）。
//!
//! 关键场景是「窗口关闭时仍有多个 Running 会话，而且外部还攥着 `SessionHandle`」。
//! 这正是靠 Drop 链回收进程会失效的那一格：`RuntimeEntry` 比 `ManagerInner` 活得久，
//! pi 进程要拖到最后一个句柄被 drop 才退。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use pi_render::ConversationDocument;
use pi_runtime::{
    Clock, FakeClock, Priority, RuntimeLimits, RuntimeManager, SchedulerLimits, SessionDescriptor,
    SessionHandle, SessionId, TerminalState, ToolPreset,
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

fn descriptor(cwd: &Path, session_path: PathBuf) -> SessionDescriptor {
    SessionDescriptor {
        binary: fake_binary(),
        cwd: cwd.to_path_buf(),
        session_path: Some(session_path),
        tool_preset: ToolPreset::Inherit,
        agent_dir: None,
    }
}

fn test_manager() -> (RuntimeManager, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new());
    let injected: Arc<dyn Clock> = clock.clone();
    let limits = RuntimeLimits {
        scheduler: SchedulerLimits {
            user_session_slots: 2,
            total_runtime_slots: 3,
            warm_idle: 1,
            idle_ttl: Duration::from_secs(180),
            queue_capacity: 64,
            aging_step: Duration::from_secs(5),
        },
        ..RuntimeLimits::default()
    };
    (RuntimeManager::with_test_clock(limits, injected), clock)
}

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

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist 应可执行");
    String::from_utf8_lossy(&output.stdout).contains("runtime_fake_child")
}

#[cfg(windows)]
fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn shutdown_all_reclaims_every_runtime_even_while_handles_are_still_held() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager();

    // 先造一个 warm pool 条目：热进程不属于任何会话，注销会话碰不到它。
    let warm_source = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "warm")),
        empty_document("warm", workspace.path()),
    );
    let warm_handle = manager
        .request_run(warm_source, Priority::FOREGROUND)
        .expect("启动应成功")
        .expect("应拿到句柄");
    park_when_quiescent(&manager, &warm_handle, warm_source);
    assert_eq!(manager.scheduler_report().warm, 1, "应有一个热进程在池里");

    // 再跑满用户并发，并且**一直攥着句柄** —— 模拟 app 退出时标签仍持有 SessionHandle。
    // 这两个会话放在**另一个 cwd**：启动参数不同，热进程复用不了，池里那个才留得住，
    // 断言"关停也要收 warm pool"才有意义。
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let mut running = Vec::new();
    for name in ["a", "b"] {
        let session = manager.create_session(
            descriptor(elsewhere.path(), session_file(elsewhere.path(), name)),
            empty_document(name, elsewhere.path()),
        );
        let handle = manager
            .request_run(session, Priority::FOREGROUND)
            .expect("启动应成功")
            .expect("应拿到句柄");
        running.push(handle);
    }
    let before = manager.scheduler_report();
    assert_eq!(before.running, 2);
    assert_eq!(before.resident_pi, 3, "两个用户会话 + 一个热进程");

    #[cfg(windows)]
    let pids: Vec<u32> = running
        .iter()
        .filter_map(SessionHandle::process_id)
        .collect();
    #[cfg(windows)]
    assert_eq!(pids.len(), 2, "两个 Running 会话都应有真实 pid");

    let report = manager.shutdown_all();
    assert_eq!(report.sessions, 3, "三个会话都应被注销");
    assert_eq!(report.warm, 1, "warm pool 里的热进程也必须被关掉");
    assert!(
        report.drained,
        "关停必须等到在途作业交还进程 —— 否则返回时可能还有 pi 活着"
    );

    let after = manager.scheduler_report();
    assert_eq!(
        after.resident_pi, 0,
        "关停之后不应还有常驻 pi 占着运行槽（句柄仍被外部持有）"
    );
    assert_eq!(after.running, 0);
    assert_eq!(after.warm, 0);

    // 句柄还在手里，但它们看到的必须是权威终态，而不是"还在跑"。
    for handle in &running {
        assert_eq!(
            handle.snapshot().terminal,
            Some(TerminalState::Stopped),
            "关停后仍被持有的句柄必须能看到 Stopped 终态"
        );
    }

    #[cfg(windows)]
    for pid in pids {
        assert!(
            wait_until_dead(pid, Duration::from_secs(10)),
            "显式关停之后进程 {pid} 仍然存活 —— 又退回到靠 Drop 链回收了"
        );
    }
}

#[test]
fn shutdown_all_is_idempotent_and_leaves_the_manager_usable() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager();
    let session = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "a")),
        empty_document("a", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("启动应成功");
    assert!(handle.is_some());

    let first = manager.shutdown_all();
    assert_eq!(first.sessions, 1);
    assert!(first.drained, "首次关停应当等到进程交还");
    let second = manager.shutdown_all();
    assert_eq!(second.sessions, 0, "重复关停不应重复计数");
    assert_eq!(second.warm, 0);
    assert!(second.drained, "已经没有在途作业，第二次必然立刻排空");

    // 关停不该把 Manager 变成一次性对象：登记并启动新会话仍要能成功。
    let revived = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "b")),
        empty_document("b", workspace.path()),
    );
    let handle = manager
        .request_run(revived, Priority::FOREGROUND)
        .expect("关停之后仍应能启动新会话");
    assert!(handle.is_some());
    manager.shutdown_all();
}

/// R25 三轮整改（codex P1）：关停途中不得把排队会话拉起来。
///
/// `stop_session` 收尾会 `tick()`。批量关停时若还照常提升队列，就会在退出途中把排队
/// 会话一个个冷启动、再一个个关掉，每个都要付一次 grace period —— 退出因此变得极慢，
/// 而且正好在内存最紧张的时候又多起几个进程。
#[test]
fn shutdown_never_cold_starts_the_sessions_still_waiting_in_the_queue() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new());
    let injected: Arc<dyn Clock> = clock.clone();
    // 只有一个运行槽：后面两个必然排队。
    let manager = RuntimeManager::with_test_clock(
        RuntimeLimits {
            scheduler: SchedulerLimits {
                user_session_slots: 1,
                total_runtime_slots: 1,
                warm_idle: 0,
                idle_ttl: Duration::from_secs(180),
                queue_capacity: 8,
                aging_step: Duration::from_secs(5),
            },
            ..RuntimeLimits::default()
        },
        injected,
    );

    let mut sessions = Vec::new();
    for name in ["a", "b", "c"] {
        let session = manager.create_session(
            descriptor(workspace.path(), session_file(workspace.path(), name)),
            empty_document(name, workspace.path()),
        );
        let _ = manager.request_run(session, Priority::FOREGROUND);
        sessions.push(session);
    }
    let before = manager.scheduler_report();
    assert_eq!(before.running, 1);
    assert_eq!(before.queued, 2, "另外两条应当在排队");

    let report = manager.shutdown_all();
    let after = manager.scheduler_report();
    assert_eq!(report.sessions, 3);
    assert!(report.drained);
    assert_eq!(
        after.cold_starts, before.cold_starts,
        "关停途中一个排队会话都不该被冷启动起来"
    );
    assert_eq!(after.resident_pi, 0);
    assert_eq!(after.queued, 0, "队列应当被清空");
}

/// R25 十轮整改（codex P2）：并发关停必须串行，且互不干扰。
///
/// `shutting_down` 是标志不是所有权令牌：先跑完的那个会把闸门清掉，而后一个还在关停
/// 途中 —— 于是 `request_run` 能在它快照完会话表之后又起一个 Runtime。
#[test]
fn concurrent_shutdowns_do_not_reopen_the_gate_for_each_other() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock) = test_manager();
    for name in ["a", "b", "c"] {
        let session = manager.create_session(
            descriptor(workspace.path(), session_file(workspace.path(), name)),
            empty_document(name, workspace.path()),
        );
        let _ = manager.request_run(session, Priority::FOREGROUND);
    }
    assert!(manager.scheduler_report().resident_pi > 0);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let manager = manager.clone();
            std::thread::spawn(move || manager.shutdown_all())
        })
        .collect();
    let reports: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("关停线程不应 panic"))
        .collect();

    assert!(
        reports.iter().all(|report| report.drained),
        "每一次关停都应当等到排空：{reports:?}"
    );
    let total: usize = reports.iter().map(|report| report.sessions).sum();
    assert_eq!(total, 3, "三个会话总共只该被注销一次，实际 {total}");
    let after = manager.scheduler_report();
    assert_eq!(after.resident_pi, 0, "并发关停之后不该还有常驻 pi");
    assert_eq!(after.running, 0);
}
