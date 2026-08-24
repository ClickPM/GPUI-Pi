use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

use pi_render::ConversationDocument;
use pi_runtime::{
    ActorLimits, ComposerMode, ControlOperation, ControlOutcome, ControlRequest, Dirty, RpcIntent,
    RuntimeEffectKind, RuntimeId, RuntimeLimits, RuntimeManager, SessionControls, SessionHandle,
    SessionSnapshot, TerminalState, ToolPreset,
};

// 等待窗口必须覆盖 pi 冷启动（Node 进程 + host extension 物化 + 五次串行 metadata RPC）
// 的最坏情况，避免健康但缓慢的首次启动被误报成超时。
const TIMEOUT: Duration = Duration::from_secs(60);

fn configured_binary() -> PathBuf {
    env::var_os("PI_RUNTIME_TEST_BINARY")
        .map(PathBuf::from)
        .expect("PI_RUNTIME_TEST_BINARY must point to the official pi 0.84.2 binary")
}

fn assert_pinned_version(binary: &Path) {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("failed to run {} --version: {error}", binary.display()));
    assert!(
        output.status.success(),
        "{} --version failed: {}",
        binary.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        pi_rpc::PINNED_PI_VERSION
    );
}

fn empty_document(id: &str, cwd: &Path) -> ConversationDocument {
    ConversationDocument {
        session_id: id.to_owned(),
        source_path: PathBuf::new(),
        cwd: cwd.to_path_buf(),
        messages: std::sync::Arc::from([]),
        items: std::sync::Arc::from([]),
        minimap: std::sync::Arc::from([]),
        diagnostics: std::sync::Arc::from([]),
    }
}

fn wait_for_controls(
    handle: &SessionHandle,
    dirty: &Receiver<Dirty>,
    label: &str,
) -> (SessionSnapshot, SessionControls) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = handle.snapshot();
        let mut commands_loaded = false;
        let mut controls = None;
        for effect in snapshot
            .effects
            .iter()
            .filter(|effect| effect.epoch == snapshot.epoch)
        {
            match &effect.kind {
                RuntimeEffectKind::CommandsLoaded(Ok(_)) => commands_loaded = true,
                RuntimeEffectKind::CommandsLoaded(Err(error)) => {
                    panic!("{label}: get_commands failed: {error}")
                }
                RuntimeEffectKind::ControlsLoaded(Ok(loaded)) => controls = Some(loaded.clone()),
                RuntimeEffectKind::ControlsLoaded(Err(error)) => {
                    panic!("{label}: metadata controls failed: {error}")
                }
                RuntimeEffectKind::Stopped(error) => {
                    panic!("{label}: runtime stopped before controls loaded: {error:?}")
                }
                _ => {}
            }
        }
        if commands_loaded && let Some(controls) = controls {
            return (snapshot, controls);
        }

        let now = Instant::now();
        assert!(
            now < deadline,
            "{label}: timed out after {TIMEOUT:?} waiting for CommandsLoaded + ControlsLoaded; \
             runtime_id={}, epoch={}, revision={}, effects={:?}",
            snapshot.runtime_id.get(),
            snapshot.epoch,
            snapshot.revision,
            snapshot.effects
        );
        let remaining = deadline.saturating_duration_since(now);
        let _ = dirty.recv_timeout(remaining.min(Duration::from_millis(250)));
    }
}

fn wait_for_control_finished(
    handle: &SessionHandle,
    dirty: &Receiver<Dirty>,
    operation: ControlOperation,
    label: &str,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = handle.snapshot();
        for effect in snapshot
            .effects
            .iter()
            .filter(|effect| effect.epoch == snapshot.epoch)
        {
            if let RuntimeEffectKind::ControlFinished {
                operation: actual,
                result,
            } = &effect.kind
                && *actual == operation
            {
                match result {
                    Ok(ControlOutcome::Controls(_)) => return,
                    Ok(outcome) => panic!("{label}: unexpected control outcome: {outcome:?}"),
                    Err(error) => panic!("{label}: control failed: {error}"),
                }
            }
        }

        let now = Instant::now();
        assert!(
            now < deadline,
            "{label}: timed out after {TIMEOUT:?} waiting for {operation:?}; \
             runtime_id={}, epoch={}, revision={}, effects={:?}",
            snapshot.runtime_id.get(),
            snapshot.epoch,
            snapshot.revision,
            snapshot.effects
        );
        let remaining = deadline.saturating_duration_since(now);
        let _ = dirty.recv_timeout(remaining.min(Duration::from_millis(250)));
    }
}

// canonicalize 失败直接 panic 并带出 IO 错误，禁止静默回退成原始路径——单边回退会把
// \\?\ 长名与 8.3 短名混进同一次比较，产生指向错误方向的断言消息（隔离检查误报越界）。
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("canonicalize {} failed: {error}", path.display()))
}

// 断言失败展开时兜底回收 Runtime：不回收会遗留 pi 子进程占住临时目录（Windows 上
// TempDir 清理会静默失败），反复失败时越积越多。stop_user 对非活跃 id 是幂等空操作，
// 正常路径上显式 stop 之后守卫落空无副作用。
struct StopOnDrop<'a> {
    manager: &'a RuntimeManager,
    id: RuntimeId,
}

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.manager.stop_user(self.id);
    }
}

#[test]
#[ignore = "requires PI_RUNTIME_TEST_BINARY=official pi 0.84.2"]
fn runtime_manager_fresh_stop_and_resume_is_zero_token() {
    let binary = configured_binary();
    assert_pinned_version(&binary);
    // 隔离只覆盖 PI_CODING_AGENT_DIR；钉死 pi 的 PI_CODING_AGENT_SESSION_DIR 优先级
    // 更高（main.ts 677-681），会把 session 写到 agent 目录之外并击穿本测试的隔离
    // 前提，必须先清掉再跑。
    assert!(
        env::var_os("PI_CODING_AGENT_SESSION_DIR").is_none(),
        "unset PI_CODING_AGENT_SESSION_DIR before running this isolation test"
    );

    let temp = tempfile::tempdir().expect("failed to create isolated runtime test root");
    let agent_dir = temp.path().join("agent");
    let cwd = temp.path().join("project");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();

    let manager = RuntimeManager::new(RuntimeLimits::default());

    // ---- fresh 启动：get_state 只预分配 session 路径，不落盘 ----
    // 钉死 pi 0.84.2 的权威契约（session-manager.ts:1015-1027 `_persist` 的 hasAssistant
    // 闸门 + `newSession()` 置 flushed=false）：出现首条 assistant 消息前 fresh 会话不
    // 创建 JSONL；fresh 态 flushed=false，bash / thinking 等零 token entry 同样不落盘。
    // 这里把懒持久化当契约断言，而不是试图在禁止 prompt 的前提下强迫 fresh 落盘。
    // 前提：环境需有可解析的模型凭据——pi 在 RPC 模式下无模型会直接退出（main.ts
    // 906-909），与 pi-rpc 真实 pi 测试的运行前提一致。
    let fresh = manager
        .start_fresh(
            binary.clone(),
            cwd.clone(),
            empty_document("fresh-real-pi", &cwd),
            ToolPreset::Inherit,
            Some(agent_dir.clone()),
        )
        .unwrap_or_else(|error| panic!("fresh RuntimeManager start failed: {error}"));
    let fresh_runtime_id = fresh.runtime_id();
    let _fresh_guard = StopOnDrop {
        manager: &manager,
        id: fresh_runtime_id,
    };
    let fresh_dirty = fresh.subscribe_dirty();
    let (fresh_snapshot, fresh_controls) = wait_for_controls(&fresh, &fresh_dirty, "fresh runtime");

    assert_eq!(fresh_snapshot.runtime_id, fresh_runtime_id);
    assert!(!fresh_controls.session_id.is_empty());
    let fresh_session_file = fresh_controls
        .session_file
        .clone()
        .expect("fresh get_state must return a preallocated session_file");
    // 预分配路径尚无文件，无法整体 canonicalize；钉死 pi 在 SessionManager 构造期就
    // mkdirSync 了 sessions 目录（session-manager.ts:879-881），用该父目录做隔离包含检查。
    let fresh_session_parent = fresh_session_file
        .parent()
        .expect("preallocated session_file must have a parent directory");
    assert!(
        canonical(fresh_session_parent).starts_with(canonical(&agent_dir)),
        "session file escaped isolated PI_CODING_AGENT_DIR: {}",
        fresh_session_file.display()
    );
    assert!(
        !fresh_session_file
            .try_exists()
            .expect("probe preallocated session file"),
        "pinned pi must not persist a fresh session before the first assistant message \
         (session-manager.ts:1015-1027 hasAssistant gate): {}",
        fresh_session_file.display()
    );
    // 身份校准发生在 ControlsLoaded 发布前的同一把锁内；这里验证 wrapper 确实把
    // controls 身份应用进了文档投影。路径尚不存在，直接比较原始值。
    assert_eq!(
        fresh_snapshot.document.session_id,
        fresh_controls.session_id
    );
    assert_eq!(fresh_snapshot.document.source_path, fresh_session_file);

    manager.stop_user(fresh_runtime_id);
    assert!(
        !fresh_session_file
            .try_exists()
            .expect("probe preallocated session file"),
        "graceful stop must not invent a session file for an unpersisted fresh session"
    );

    // ---- stop/resume 接缝：走钉死 pi 的显式空文件分支实现零 token 持久化 ----
    // `--session` 指向已存在的 0 字节文件时（session-manager.ts:902-911 空文件分支），
    // pi 立即写入 session header 并置 flushed=true，是唯一不消耗模型 token 就能让真实
    // pi 自己落盘的权威路径。flushed=true 后启动期的 model/thinking 变更 entry 会随之
    // 追加（sdk.ts 371-377），文件不止 header 一行，但 header 恒为首行。
    let seam_dir = agent_dir.join("sessions");
    fs::create_dir_all(&seam_dir).unwrap();
    let seam_file = seam_dir.join("seam-session.jsonl");
    fs::write(&seam_file, "").unwrap();

    let seeded = manager
        .start_session(
            binary.clone(),
            seam_file.clone(),
            cwd.clone(),
            empty_document("seam-real-pi", &cwd),
            ToolPreset::Inherit,
            Some(agent_dir.clone()),
        )
        .unwrap_or_else(|error| panic!("seeded RuntimeManager start failed: {error}"));
    let seeded_runtime_id = seeded.runtime_id();
    let _seeded_guard = StopOnDrop {
        manager: &manager,
        id: seeded_runtime_id,
    };
    assert_ne!(
        seeded_runtime_id, fresh_runtime_id,
        "a new resident Runtime must receive a new RuntimeId"
    );
    let seeded_dirty = seeded.subscribe_dirty();
    let (seeded_snapshot, seeded_controls) =
        wait_for_controls(&seeded, &seeded_dirty, "seeded runtime");

    assert_eq!(seeded_snapshot.runtime_id, seeded_runtime_id);
    assert!(!seeded_controls.session_id.is_empty());
    assert_eq!(
        canonical(
            seeded_controls
                .session_file
                .as_deref()
                .expect("seeded get_state must return a session_file")
        ),
        canonical(&seam_file)
    );
    let header_text = fs::read_to_string(&seam_file)
        .unwrap_or_else(|error| panic!("real pi did not persist the seeded session: {error}"));
    let header: serde_json::Value = header_text
        .lines()
        .next()
        .and_then(|line| serde_json::from_str(line).ok())
        .unwrap_or_else(|| panic!("seeded session file has no parseable header: {header_text:?}"));
    assert_eq!(header["type"], "session");
    assert_eq!(
        header["id"].as_str(),
        Some(seeded_controls.session_id.as_str()),
        "on-disk session header must match get_state session_id"
    );
    assert_eq!(
        seeded_snapshot.document.session_id,
        seeded_controls.session_id
    );
    assert_eq!(
        canonical(&seeded_snapshot.document.source_path),
        canonical(&seam_file)
    );

    // 经 SessionHandle 走一次零 token 控制往返：同值 set_thinking_level 不写盘、不耗
    // token（rpc-mode.ts:495-497 恒 success，同值不追加 entry），保住 request_control
    // → execute_control → ControlFinished 在真实 pi 上的端到端覆盖。
    seeded
        .request_control(
            ControlOperation::Thinking,
            ControlRequest::SetThinking(seeded_controls.thinking_level),
        )
        .unwrap_or_else(|error| panic!("zero-token control dispatch failed: {error}"));
    wait_for_control_finished(
        &seeded,
        &seeded_dirty,
        ControlOperation::Thinking,
        "seeded control round-trip",
    );

    manager.stop_user(seeded_runtime_id);
    assert!(
        seam_file.is_file(),
        "graceful stop removed the seeded session file"
    );

    // 从同一 session file resume，历史加载走与生产一致的 pi-render 路径。resume 并非
    // 只读：钉死 pi 对无消息会话按新会话处理，启动期会再追加 model/thinking 变更
    // entry（sdk.ts 190-191、371-377），因此这里只断言身份与路径稳定，不断言字节不变。
    let history = pi_render::render_path(&seam_file)
        .unwrap_or_else(|error| panic!("failed to render stopped session for resume: {error}"));
    assert_eq!(history.session_id, seeded_controls.session_id);
    let resumed = manager
        .start_session(
            binary,
            seam_file.clone(),
            cwd.clone(),
            history,
            ToolPreset::Inherit,
            Some(agent_dir.clone()),
        )
        .unwrap_or_else(|error| panic!("resume RuntimeManager start failed: {error}"));
    let resumed_runtime_id = resumed.runtime_id();
    let _resumed_guard = StopOnDrop {
        manager: &manager,
        id: resumed_runtime_id,
    };
    assert_ne!(
        resumed_runtime_id, seeded_runtime_id,
        "a new resident Runtime must receive a new RuntimeId"
    );
    let resumed_dirty = resumed.subscribe_dirty();
    let (resumed_snapshot, resumed_controls) =
        wait_for_controls(&resumed, &resumed_dirty, "resumed runtime");

    assert_eq!(resumed_snapshot.runtime_id, resumed_runtime_id);
    assert_eq!(resumed_controls.session_id, seeded_controls.session_id);
    assert_eq!(
        canonical(
            resumed_controls
                .session_file
                .as_deref()
                .expect("resumed get_state must return a session_file")
        ),
        canonical(&seam_file)
    );
    assert_eq!(
        resumed_snapshot.document.session_id,
        seeded_controls.session_id
    );
    assert_eq!(
        canonical(&resumed_snapshot.document.source_path),
        canonical(&seam_file)
    );

    manager.stop_user(resumed_runtime_id);

    assert!(
        fs::read_dir(&cwd).unwrap().next().is_none(),
        "zero-token runtime metadata test wrote into the temporary cwd"
    );
}

/// R22：用真实 pi 验证有界 Actor 与背压路径，全程零 token。
///
/// 只跑 `get_commands` / `get_state` 这类元数据 RPC 与优雅停止，不发任何 prompt，
/// 因此不消耗模型 token；覆盖的是「换成固定线程 + 有界队列 + 有界 effect 缓存之后，
/// 真实内核仍能正常启动、刷新元数据并留下权威终态」。
#[test]
#[ignore = "requires PI_RUNTIME_TEST_BINARY=official pi 0.84.2"]
fn bounded_actor_and_effect_backpressure_hold_against_real_pi() {
    let binary = configured_binary();
    assert_pinned_version(&binary);
    assert!(
        env::var_os("PI_CODING_AGENT_SESSION_DIR").is_none(),
        "unset PI_CODING_AGENT_SESSION_DIR before running this isolation test"
    );

    let temp = tempfile::tempdir().expect("failed to create isolated runtime test root");
    let agent_dir = temp.path().join("agent");
    let cwd = temp.path().join("project");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();

    let actor = ActorLimits {
        command_capacity: 4,
        control_capacity: 2,
        command_workers: 1,
        control_workers: 1,
    };
    let manager = RuntimeManager::new(RuntimeLimits {
        actor,
        // 故意配一个越界的合帧窗口，验证它被收敛进 16–33ms 而不是被原样信任。
        event_frame: Duration::from_millis(1),
        ..RuntimeLimits::default()
    });
    let handle = manager
        .start_fresh(
            binary.clone(),
            cwd.clone(),
            empty_document("bounded-real-pi", &cwd),
            ToolPreset::Inherit,
            Some(agent_dir.clone()),
        )
        .unwrap_or_else(|error| panic!("bounded RuntimeManager start failed: {error}"));
    let runtime_id = handle.runtime_id();
    let _guard = StopOnDrop {
        manager: &manager,
        id: runtime_id,
    };
    let dirty = handle.subscribe_dirty();

    assert_eq!(handle.actor_limits(), actor, "有界参数必须原样生效");
    assert_eq!(
        handle.event_frame(),
        pi_runtime::EVENT_FRAME_MIN,
        "越界合帧窗口必须被 clamp 进 16–33ms"
    );
    let expected_threads = actor.command_workers + actor.control_workers + 1;
    assert_eq!(
        handle.live_thread_count(),
        expected_threads,
        "线程预算固定为 worker + 事件 pump"
    );

    // ---- 启动元数据经普通通道完成（零 token 的 get_commands / get_state） ----
    let (snapshot, controls) = wait_for_controls(&handle, &dirty, "bounded runtime");
    assert_eq!(snapshot.runtime_id, runtime_id);
    assert!(!controls.session_id.is_empty());
    assert_eq!(snapshot.terminal, None, "运行中的会话不该有终态");
    assert!(
        snapshot.backpressure.buffered_bytes <= handle.effect_limits().max_bytes,
        "effect 缓存越过字节上限：{} > {}",
        snapshot.backpressure.buffered_bytes,
        handle.effect_limits().max_bytes
    );
    assert_eq!(snapshot.backpressure.dropped_results, 0);

    // ---- ack 回收：消费过的 effect 立刻从运行时缓存中释放 ----
    let last = snapshot
        .effects
        .last()
        .expect("metadata effects present")
        .sequence;
    handle.ack_effects(snapshot.epoch, last);
    let acked = handle.snapshot();
    assert_eq!(acked.backpressure.buffered_effects, 0);
    assert_eq!(acked.backpressure.buffered_bytes, 0);

    // ---- 再刷一次元数据：仍走同一批 worker，线程数不变 ----
    handle.refresh_metadata();
    let (refreshed, refreshed_controls) =
        wait_for_controls(&handle, &dirty, "bounded runtime refresh");
    assert_eq!(refreshed_controls.session_id, controls.session_id);
    assert_eq!(
        handle.live_thread_count(),
        expected_threads,
        "刷新元数据不得新建线程"
    );
    assert!(refreshed.backpressure.buffered_bytes <= handle.effect_limits().max_bytes);

    // ---- 优雅停止留下权威终态（BACKLOG #12 的 R22 部分） ----
    manager.stop_user(runtime_id);
    let stopped = handle.snapshot();
    assert_eq!(
        stopped.terminal,
        Some(TerminalState::Stopped),
        "优雅停止必须是可观察终态"
    );
    assert!(
        handle
            .dispatch(RpcIntent::Prompt, None, ComposerMode::Steer)
            .is_err(),
        "停止后不再接受任何命令"
    );
}
