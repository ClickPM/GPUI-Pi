//! R23 验收：Park/Resume 循环不泄漏线程。
//!
//! 必须独立一个集成测试文件 —— `pi_runtime::live_thread_count()` 是进程级计数器，
//! 与 `spawned_thread_count()` 同理：放进 `--lib` 会被并行的其他测试污染，
//! 「回到固定预算」这条断言就不成立了。
//!
//! 与 `thread_budget.rs` 的分工：那条证明「稳态下不再新建线程」（累计计数不涨），
//! 这条证明「换过的线程真的收掉了」（存活计数回到基线）。Park 每一轮都会拆掉一整套
//! Actor + pump 再建一套，只有存活计数能证明旧的那套没留下。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use pi_render::ConversationDocument;
use pi_runtime::{
    Clock, FakeClock, Priority, RuntimeLimits, RuntimeManager, SchedulerLimits, SchedulerState,
    SessionDescriptor, ToolPreset, live_thread_count,
};

const CYCLES: usize = 6;

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

fn wait_for_live_threads(target: u64, what: &str) {
    // 线程退出是异步的：pump 要先被哨兵叫醒，worker 要先跑完手头的作业。
    let deadline = Instant::now() + Duration::from_secs(60);
    while live_thread_count() > target {
        assert!(
            Instant::now() < deadline,
            "{what}：存活线程停在 {}，没有回落到 {target}",
            live_thread_count()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn park_and_resume_cycles_return_to_a_fixed_thread_budget() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let session_path = workspace.path().join("park-resume.jsonl");
    std::fs::write(&session_path, "{\"type\":\"session\"}\n").expect("write fake session file");

    let clock = Arc::new(FakeClock::new());
    let injected: Arc<dyn Clock> = clock.clone();
    let manager = RuntimeManager::with_test_clock(
        RuntimeLimits {
            scheduler: SchedulerLimits::default(),
            ..RuntimeLimits::default()
        },
        injected,
    );
    // 基线在 Manager 构造之后取：`with_test_clock` 不起 reaper 线程，此处应为 0，
    // 但仍然按实测基线比较，避免被同进程内的其他测试目标干扰。
    let baseline = live_thread_count();

    let session = manager.create_session(
        SessionDescriptor {
            binary: fake_binary(),
            cwd: workspace.path().to_path_buf(),
            session_path: Some(session_path),
            tool_preset: ToolPreset::Inherit,
            agent_dir: None,
        },
        empty_document("park-resume", workspace.path()),
    );

    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("cold start")
        .expect("slot available");
    let per_runtime = u64::try_from(handle.live_thread_count()).expect("thread count fits u64");
    let limits = handle.actor_limits();
    assert_eq!(
        per_runtime,
        u64::try_from(limits.command_workers + limits.control_workers + 1).unwrap(),
        "单 Runtime 的常驻线程是固定预算：worker + 事件 pump"
    );
    assert_eq!(live_thread_count(), baseline + per_runtime);

    let mut handle = handle;
    for cycle in 0..CYCLES {
        // 启动/接管之后的元数据刷新还在跑时 Park 只会退化成冷停，热进程复用就断了。
        let deadline = Instant::now() + Duration::from_secs(60);
        while !handle.is_quiescent() {
            assert!(Instant::now() < deadline, "Runtime 迟迟没有静止");
            std::thread::sleep(Duration::from_millis(5));
        }
        manager.park(session).expect("park");
        assert_eq!(manager.session_state(session), Some(SchedulerState::Parked));
        // Park 之后进程还在（热进程池），但这一套 Actor 与 pump 必须全部退出。
        wait_for_live_threads(baseline, &format!("第 {cycle} 轮 Park 之后"));

        let resumed = manager
            .request_run(session, Priority::FOREGROUND)
            .expect("resume")
            .expect("slot available");
        assert_eq!(
            resumed.session_id(),
            session,
            "SessionId 跨 Park/Resume 不变"
        );
        assert_eq!(
            live_thread_count(),
            baseline + per_runtime,
            "第 {cycle} 轮 Resume 之后线程数必须回到固定预算"
        );
        handle = resumed;
    }

    let report = manager.scheduler_report();
    assert_eq!(
        report.warm_resumes,
        u64::try_from(CYCLES).unwrap(),
        "每一轮 Resume 都应复用热进程"
    );
    assert_eq!(report.cold_starts, 1, "只有第一次是冷启动");

    manager.remove_session(session);
    wait_for_live_threads(baseline, "移除会话之后");
}
