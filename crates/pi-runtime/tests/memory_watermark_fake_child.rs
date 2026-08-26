//! R25 验收：内存水位下的准入策略。
//!
//! 全部用 [`FakeResourceProbe`] + [`FakeClock`] 驱动，**不读真机内存**：策略对不对
//! 必须与本机当下有多少空闲内存无关，否则同一条断言会在忙机器上翻红、闲机器上翻绿。
//! 真机数值只用来标定阈值（见任务卡「本轮实测」），不参与判定。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use pi_render::ConversationDocument;
use pi_runtime::{
    Clock, FakeClock, FakeResourceProbe, MemoryLimits, Priority, ResourceProbe, RuntimeLimits,
    RuntimeManager, SchedulerLimits, SchedulerState, SessionDescriptor, SessionHandle, SessionId,
    ToolPreset,
};

const GIB: u64 = 1024 * 1024 * 1024;
const TOTAL: u64 = 16 * GIB;
/// 宽裕：远高于解除阈值。
const ROOMY: u64 = 8 * GIB;
/// 紧张：低于进入阈值。
const TIGHT: u64 = GIB / 2;

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

fn memory_limits() -> MemoryLimits {
    MemoryLimits {
        low_available_bytes: GIB,
        resume_available_bytes: 2 * GIB,
        // 采样节流交给 fake clock 也能测，但准入用例关心的是策略而不是节流，
        // 这里直接关掉节流，避免每条断言都要先推时钟。
        sample_interval: Duration::ZERO,
        ..MemoryLimits::default()
    }
}

fn test_manager(
    scheduler: SchedulerLimits,
) -> (RuntimeManager, Arc<FakeClock>, Arc<FakeResourceProbe>) {
    let clock = Arc::new(FakeClock::new());
    let probe = Arc::new(FakeResourceProbe::new(TOTAL, ROOMY));
    let injected_clock: Arc<dyn Clock> = clock.clone();
    let injected_probe: Arc<dyn ResourceProbe> = probe.clone();
    let limits = RuntimeLimits {
        scheduler,
        memory: memory_limits(),
        ..RuntimeLimits::default()
    };
    let manager = RuntimeManager::with_test_clock_and_probe(limits, injected_clock, injected_probe);
    (manager, clock, probe)
}

fn scheduler_limits() -> SchedulerLimits {
    SchedulerLimits {
        user_session_slots: 2,
        total_runtime_slots: 3,
        warm_idle: 1,
        idle_ttl: Duration::from_secs(180),
        queue_capacity: 64,
        aging_step: Duration::from_secs(5),
    }
}

/// 等 Runtime 静止再 Park —— 不静止时 Park 会退化成优雅停机，进程不进 warm pool。
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

/// 高水位拦后台、放前台；水位回落后排队的会话自动被提升。
#[test]
fn pressure_queues_background_sessions_and_releases_them_when_it_clears() {
    let workspace = tempfile::tempdir().expect("tempdir");
    // 没有 warm pool，把「先回收热进程」那一步排除掉，专测「拦下后台冷启动」。
    let (manager, clock, probe) = test_manager(SchedulerLimits {
        warm_idle: 0,
        ..scheduler_limits()
    });

    let foreground = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "fg")),
        empty_document("fg", workspace.path()),
    );
    let background = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "bg")),
        empty_document("bg", workspace.path()),
    );

    let handle = manager
        .request_run(foreground, Priority::FOREGROUND)
        .expect("宽裕时前台会话应能启动");
    assert!(handle.is_some(), "宽裕时前台会话应拿到句柄");
    assert!(!manager.memory_pressure().under_pressure);

    // 进入高水位。运行槽还有余量，被拦下的原因只能是内存。
    probe.set_available(TOTAL, TIGHT);
    let admitted = manager
        .request_run(background, Priority::BACKGROUND)
        .expect("水位拦截必须是排队，不是报错");
    assert!(admitted.is_none(), "高水位下后台会话不应直接拿到运行时");
    assert_eq!(
        manager.session_state(background),
        Some(SchedulerState::Queued),
        "被水位拦下的后台会话应进入队列等待，而不是失败或停在 Parked"
    );
    assert!(manager.memory_pressure().under_pressure);
    let report = manager.scheduler_report();
    assert_eq!(report.running, 1, "前台会话不该被水位波及");
    assert!(
        report.slots.user < scheduler_limits().user_session_slots,
        "运行槽仍有余量 —— 说明这次排队确实来自内存水位而不是并发上限"
    );

    // 水位回落：迟滞要求越过**解除**阈值才算解除，因此这里给足余量。
    probe.set_available(TOTAL, ROOMY);
    clock.advance(Duration::from_secs(1));
    manager.tick();
    assert_eq!(
        manager.session_state(background),
        Some(SchedulerState::Running),
        "水位回落后排队的后台会话应被自动提升"
    );
    assert!(!manager.memory_pressure().under_pressure);
}

/// 高水位不拦前台：用户正盯着它等结果。
#[test]
fn pressure_never_blocks_the_session_the_user_is_looking_at() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock, probe) = test_manager(SchedulerLimits {
        warm_idle: 0,
        ..scheduler_limits()
    });
    let session = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "fg")),
        empty_document("fg", workspace.path()),
    );

    probe.set_available(TOTAL, TIGHT);
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("前台会话在高水位下也不应报错");
    assert!(
        handle.is_some(),
        "高水位仍应放行前台会话 —— 卡住用户正在看的会话比紧一点内存更糟"
    );
    assert_eq!(
        manager.session_state(session),
        Some(SchedulerState::Running)
    );
}

/// 高水位先回收 IdleWarm，再考虑拦下后台冷启动。
#[test]
fn pressure_reclaims_idle_warm_before_refusing_a_background_start() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let other = tempfile::tempdir().expect("tempdir");
    let (manager, _clock, probe) = test_manager(scheduler_limits());

    // 先让一个会话跑起来再 Park，池里就有一个热进程。
    let warm_source = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "warm")),
        empty_document("warm", workspace.path()),
    );
    let handle = manager
        .request_run(warm_source, Priority::FOREGROUND)
        .expect("启动应成功")
        .expect("应拿到句柄");
    park_when_quiescent(&manager, &handle, warm_source);
    assert_eq!(
        manager.scheduler_report().warm,
        1,
        "静止后 Park 应把进程留在 warm pool 里"
    );

    // 后台会话在**另一个 cwd**：启动参数不同，热进程复用不了，只能冷启动 ——
    // 这正是「热进程白占着内存」的场景，高水位下必须先回收它。
    let background = manager.create_session(
        descriptor(other.path(), session_file(other.path(), "bg")),
        empty_document("bg", other.path()),
    );
    probe.set_available(TOTAL, TIGHT);
    let admitted = manager
        .request_run(background, Priority::BACKGROUND)
        .expect("水位拦截必须是排队，不是报错");

    assert!(admitted.is_none(), "高水位下后台会话不应冷启动");
    let report = manager.scheduler_report();
    assert_eq!(
        report.warm, 0,
        "高水位下热进程应被优先回收 —— 它只是为了省一次冷启动的投机缓存"
    );
    assert_eq!(
        manager.session_state(background),
        Some(SchedulerState::Queued)
    );
    assert_eq!(report.resident_pi, 0, "回收之后不该还有常驻进程占着内存");
}

/// 采样拿不到数据时一律放行，绝不把「未知」当成「高压」。
#[test]
fn an_unavailable_probe_never_blocks_background_sessions() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock, probe) = test_manager(SchedulerLimits {
        warm_idle: 0,
        ..scheduler_limits()
    });
    let session = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "bg")),
        empty_document("bg", workspace.path()),
    );

    probe.set_unavailable();
    let handle = manager
        .request_run(session, Priority::BACKGROUND)
        .expect("采样失效不应变成错误");
    assert!(
        handle.is_some(),
        "采不到内存数据时必须放行；否则一台查不到内存的机器上所有后台会话都会无声排队"
    );
}

/// 关掉水位策略后，准入行为回到 R24 的样子。
#[test]
fn disabling_the_watermark_restores_plain_admission() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new());
    let probe = Arc::new(FakeResourceProbe::new(TOTAL, TIGHT));
    let injected_clock: Arc<dyn Clock> = clock.clone();
    let injected_probe: Arc<dyn ResourceProbe> = probe.clone();
    let manager = RuntimeManager::with_test_clock_and_probe(
        RuntimeLimits {
            scheduler: SchedulerLimits {
                warm_idle: 0,
                ..scheduler_limits()
            },
            memory: memory_limits().without_watermark(),
            ..RuntimeLimits::default()
        },
        injected_clock,
        injected_probe,
    );

    let session = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "bg")),
        empty_document("bg", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::BACKGROUND)
        .expect("关掉水位后不应有额外拦截");
    assert!(handle.is_some());
    assert!(!manager.memory_pressure().enabled);
    assert!(
        manager.memory_limits().max_processes_per_runtime.is_some(),
        "关掉水位不应连 Job Object 的进程数硬上限一起关掉"
    );
}

/// R25 整改（codex P1）：水位升高时，就算没有任何会话申请运行，热进程也要被回收。
///
/// 只在 admit 时采样等于假设"内存只有我们自己会吃"。外部负载把系统压到高水位时根本
/// 不会发生 admission，热进程会一直占着内存等到 TTL，`memory_pressure()` 也停在旧值上。
#[test]
fn rising_pressure_reclaims_warm_runtimes_without_any_admission() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, clock, probe) = test_manager(scheduler_limits());

    let session = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "warm")),
        empty_document("warm", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("启动应成功")
        .expect("应拿到句柄");
    park_when_quiescent(&manager, &handle, session);
    assert_eq!(manager.scheduler_report().warm, 1);

    // 只把水位推高，**不**申请任何会话运行。时间推进远小于 Idle TTL（180s），
    // 因此接下来的回收只可能来自水位，不可能是 TTL 到期。
    probe.set_available(TOTAL, TIGHT);
    clock.advance(Duration::from_secs(1));
    manager.tick();

    let report = manager.scheduler_report();
    assert_eq!(report.warm, 0, "高水位下热进程应被提前回收，而不是等到 TTL");
    assert_eq!(report.pressure_reclaimed, 1, "回收原因应记为水位而非 TTL");
    assert_eq!(report.idle_reaped, 0, "TTL 还远没到，不该算在 TTL 头上");
    assert_eq!(report.resident_pi, 0);
    assert!(
        manager.memory_pressure().under_pressure,
        "没有 admission 也必须能观测到水位变化，否则报告永远是旧值"
    );
}

/// R25 整改（codex P2）：被水位挡住的后台会话不得堵死队列。
///
/// 队首若是一条被水位挡下的后台会话，`promote_queued` 就此收手的话，本该绕过水位的
/// 前台会话会跟着一起饿死，运行槽空在那儿没人用。
#[test]
fn a_pressure_blocked_background_entry_does_not_starve_a_queued_foreground_one() {
    let workspace = tempfile::tempdir().expect("tempdir");
    // 只有一个用户运行槽：谁拿到槽是唯一可观测的结论。
    let (manager, clock, probe) = test_manager(SchedulerLimits {
        user_session_slots: 1,
        total_runtime_slots: 1,
        warm_idle: 0,
        ..scheduler_limits()
    });

    let make = |name: &str| {
        manager.create_session(
            descriptor(workspace.path(), session_file(workspace.path(), name)),
            empty_document(name, workspace.path()),
        )
    };
    let occupant = make("occupant");
    let background = make("bg");
    let foreground = make("fg");

    // 占住唯一的槽，让后面两个只能排队。
    manager
        .request_run(occupant, Priority::FOREGROUND)
        .expect("启动应成功")
        .expect("应拿到句柄");

    // 后台会话先排队，并靠 aging 排到前台会话前面 —— 这正是会堵住队首的那种条目。
    assert!(
        manager
            .request_run(background, Priority::BACKGROUND)
            .expect("排队不应报错")
            .is_none()
    );
    clock.advance(Duration::from_secs(20));
    assert!(
        manager
            .request_run(foreground, Priority::FOREGROUND)
            .expect("排队不应报错")
            .is_none()
    );

    // 进入高水位后腾出槽：后台会被水位挡下，前台必须照样拿到槽。
    probe.set_available(TOTAL, TIGHT);
    manager.stop_session(occupant);
    manager.tick();

    assert_eq!(
        manager.session_state(foreground),
        Some(SchedulerState::Running),
        "前台会话本就获准绕过水位，不该被队首那条被挡下的后台会话拖住"
    );
    assert_eq!(
        manager.session_state(background),
        Some(SchedulerState::Queued),
        "后台会话仍应被水位挡在队列里"
    );
}

/// R25 二轮整改（codex P2）：高水位不得打掉「Resume 优先复用 `switch_session`」。
///
/// 复用一个参数一致的热进程**不新增常驻进程**，压力再大也该走这条路。
/// 一旦 `request_run` 前那趟 tick 顺手把 warm pool 清空，唯一能复用的进程就没了，
/// 每次 Resume 都退化成冷启动 —— 立项文档明确要求复用优先。
#[test]
fn pressure_does_not_defeat_warm_reuse_on_resume() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock, probe) = test_manager(scheduler_limits());

    let session = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "s")),
        empty_document("s", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("启动应成功")
        .expect("应拿到句柄");
    let warm_pid = handle.process_id().expect("Running 会话应有 pid");
    park_when_quiescent(&manager, &handle, session);
    assert_eq!(manager.scheduler_report().warm, 1);
    let before = manager.scheduler_report();

    // 高水位下 Resume：参数完全一致，必须复用池里那个进程。
    probe.set_available(TOTAL, TIGHT);
    let resumed = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("Resume 不应报错")
        .expect("Resume 应拿到句柄");

    let after = manager.scheduler_report();
    assert_eq!(
        after.warm_resumes,
        before.warm_resumes + 1,
        "高水位下 Resume 仍应走热复用"
    );
    assert_eq!(
        after.cold_starts, before.cold_starts,
        "不该因为水位就退化成冷启动"
    );
    assert_eq!(
        resumed.process_id(),
        Some(warm_pid),
        "复用的必须就是池里那个进程 —— pid 变了就说明其实冷启动了一个新的"
    );
}

/// R25 四轮整改（codex P1）：进程树采样也必须走可注入的 `ResourceProbe`。
///
/// 立项文档 § 七 R25 要求的是「**内存与进程数**采样经可注入抽象」。只把内存那一半
/// 做成可注入，依赖进程数的策略（R26 起会有）就只能"跑起来看看"，撞红线 4。
#[test]
fn process_tree_sampling_goes_through_the_injectable_probe() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (manager, _clock, probe) = test_manager(scheduler_limits());
    let session = manager.create_session(
        descriptor(workspace.path(), session_file(workspace.path(), "s")),
        empty_document("s", workspace.path()),
    );
    let handle = manager
        .request_run(session, Priority::FOREGROUND)
        .expect("启动应成功")
        .expect("应拿到句柄");

    // 没预置时走真实取数口，行为与改造前一致。
    let real = handle.process_tree_stats();
    assert!(
        real.is_some_and(|stats| stats.active_processes >= 1),
        "未预置时应当仍然拿到真实进程树采样"
    );

    // 预置之后，策略层看到的就是预置值 —— 这才叫可确定性验收。
    probe.set_process_tree(pi_runtime::JobStats {
        active_processes: 7,
        total_processes: 9,
        total_terminated_processes: 2,
        peak_job_memory_bytes: 4096,
        private_bytes: 2048,
        sampled_processes: 7,
    });
    let injected = handle.process_tree_stats().expect("预置值应当被读到");
    assert_eq!(injected.active_processes, 7);
    assert_eq!(injected.private_bytes, 2048);

    // 也要能模拟"采样拿不到数据"（例如非 Windows），而不是只能模拟成功。
    probe.set_process_tree_unavailable();
    assert_eq!(
        handle.process_tree_stats(),
        None,
        "采样失败必须能被模拟出来，否则非 Windows 与失败分支永远测不到"
    );
}
