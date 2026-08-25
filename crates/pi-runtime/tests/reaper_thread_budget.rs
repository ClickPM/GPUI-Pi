//! R23 验收：Manager 的后台线程预算是常数，且测试构造完全没有后台线程。
//!
//! 独立一个集成测试文件、且**只放一条用例**：`live_thread_count()` 与
//! `spawned_thread_count()` 都是进程级计数器，只要同进程内还有第二条并行用例，
//! 「前后差值」这种断言就会被随机顶掉（R23 首次实现时正是这样红过一次）。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use pi_runtime::{
    Clock, FakeClock, RuntimeLimits, RuntimeManager, SchedulerLimits, live_thread_count,
};

#[test]
fn the_manager_costs_exactly_one_background_thread_and_zero_under_a_test_clock() {
    let baseline = live_thread_count();

    // ---- 测试构造：不起 reaper，TTL 与队列提升完全由显式 tick 驱动 ----
    let clock = Arc::new(FakeClock::new());
    let injected: Arc<dyn Clock> = clock.clone();
    let quiet = RuntimeManager::with_test_clock(RuntimeLimits::default(), injected);
    assert_eq!(
        live_thread_count(),
        baseline,
        "with_test_clock 不得起 reaper 线程，否则它会和断言抢状态"
    );
    quiet.tick();
    assert_eq!(live_thread_count(), baseline, "tick 本身也不创建线程");
    drop(quiet);

    // ---- 生产构造：整个 Manager 只有一条 reaper 线程，与 Runtime 数量无关 ----
    let manager = RuntimeManager::new(RuntimeLimits {
        scheduler: SchedulerLimits {
            // 短 TTL → 200ms 巡检间隔，Drop 之后能很快观察到线程退出。
            idle_ttl: Duration::from_millis(400),
            ..SchedulerLimits::default()
        },
        ..RuntimeLimits::default()
    });
    assert_eq!(
        live_thread_count(),
        baseline + 1,
        "生产构造只应多出一条 reaper 线程"
    );
    // 登记会话是纯内存操作：Parked 会话零进程零线程。
    for _ in 0..20 {
        manager.tick();
    }
    assert_eq!(
        live_thread_count(),
        baseline + 1,
        "反复 tick 不得让线程数增长"
    );

    // Drop 之后 reaper 必须自己退出，不能靠进程结束兜底。
    drop(manager);
    let deadline = Instant::now() + Duration::from_secs(30);
    while live_thread_count() > baseline {
        assert!(
            Instant::now() < deadline,
            "Manager 已 Drop，reaper 线程仍未退出（存活 {}）",
            live_thread_count()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
