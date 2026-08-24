//! R22 验收：连续 follow-up 不新增无界 OS 线程。
//!
//! 这一条必须单独一个集成测试文件 —— 每个集成测试文件是独立进程，
//! `pi_runtime::spawned_thread_count()` 这个进程级计数器因此只被本测试触碰，
//! 「前后差值为 0」的断言才成立。放进 `--lib` 单测里会被并行的其他测试污染。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use pi_render::ConversationDocument;
use pi_runtime::{
    ActorLimits, ComposerMode, ComposerSubmission, RpcIntent, RuntimeLimits, RuntimeManager,
    ToolPreset, spawned_thread_count,
};

/// 连续投递多少次 follow-up。远大于命令队列容量（32），才能证明线程数与请求次数无关。
const FOLLOW_UPS: usize = 200;

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

#[test]
fn sustained_follow_ups_never_grow_the_runtime_thread_budget() {
    let cwd = std::env::temp_dir();
    let manager = RuntimeManager::new(RuntimeLimits::default());
    let before_start = spawned_thread_count();
    let handle = manager
        .start_fresh(
            fake_binary(),
            cwd.clone(),
            empty_document("thread-budget", &cwd),
            ToolPreset::Inherit,
            None,
        )
        .expect("fake child runtime starts");

    let limits: ActorLimits = handle.actor_limits();
    let expected_threads = limits.command_workers + limits.control_workers + 1;
    let after_start = spawned_thread_count();
    assert_eq!(
        after_start - before_start,
        expected_threads as u64,
        "启动只应创建固定的 worker + 事件 pump 线程"
    );
    assert_eq!(handle.live_thread_count(), expected_threads);

    // fake child 对 "complete" 会立刻回完整的 agent 生命周期，队列因此持续排空。
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut accepted = 0_usize;
    while accepted < FOLLOW_UPS {
        assert!(
            Instant::now() < deadline,
            "timed out dispatching follow-ups"
        );
        let dispatched = handle.dispatch(
            RpcIntent::FollowUp,
            Some(ComposerSubmission {
                message: "complete".to_owned(),
                images: Vec::new(),
            }),
            ComposerMode::FollowUp,
        );
        match dispatched {
            Ok(()) => accepted += 1,
            // 队列满是设计内的背压结果，不是失败：等 worker 排空后继续。
            Err(error) => {
                assert!(error.contains("命令队列已满"), "{error}");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    // 等待全部作业执行完，确保断言覆盖的是「执行过 200 次请求之后」的稳态。
    let drained = Instant::now() + Duration::from_secs(60);
    while handle.snapshot().backpressure.queued_commands > 0 {
        assert!(Instant::now() < drained, "timed out draining command queue");
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        spawned_thread_count(),
        after_start,
        "{FOLLOW_UPS} 次 follow-up 之后不得新建任何线程"
    );
    assert_eq!(
        handle.live_thread_count(),
        expected_threads,
        "存活线程数必须与请求次数无关"
    );

    manager.stop_user(handle.runtime_id());
}
