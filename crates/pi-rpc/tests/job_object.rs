//! R25：Job Object 整树纳管、硬限制与整树终止。
//!
//! 这些用例专门盯住 `taskkill /T` 做不到的那一格：**孙进程**。fixture 会 spawn 一个
//! 独立的孙进程（三个标准流接 null，不继承 stdout 管道），它不是 pi 的子进程链上
//! "顺手会被带走"的那种，只有真的进了 job 才管得住。

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::{Duration, Instant},
};

use pi_rpc::{Client, ClientConfig, JobLimits};

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_child"))
}

/// 让 fixture 生 `count` 个孙进程，并把 pid 写到 `report`。
fn config_spawning_grandchildren(count: usize, report: &Path) -> ClientConfig {
    let mut config = ClientConfig::new(fake_binary());
    config.restart_delay = Duration::from_millis(20);
    // 崩溃自动重启会在测试中途再拉起一棵树，干扰整树断言。
    config.max_restarts = 0;
    config.env.push((
        OsString::from("PI_RPC_FAKE_SPAWN_GRANDCHILD"),
        OsString::from(count.to_string()),
    ));
    config.env.push((
        OsString::from("PI_RPC_FAKE_GRANDCHILD_PIDS"),
        report.as_os_str().to_owned(),
    ));
    config
}

/// 等 fixture 把孙进程结果写完；返回文件里的每一行。
fn wait_for_report(report: &Path, lines: usize, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(body) = fs::read_to_string(report) {
            let collected: Vec<String> = body
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect();
            if collected.len() >= lines {
                return collected;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "fixture 未在 {timeout:?} 内写出 {lines} 行孙进程结果：{}",
        report.display()
    );
}

/// 进程是否仍然存活。用镜像名一起过滤，避免 pid 复用造成假阳性。
#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let output = ProcessCommand::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist 应可执行");
    String::from_utf8_lossy(&output.stdout).contains("fake_child")
}

#[cfg(windows)]
fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 整树纳管 + 句柄关闭即整树终止。
///
/// 这条覆盖的是"关闭 Runtime 不泄漏工具子进程"：测试全程**不调用**任何显式整树终止，
/// 只是让宿主正常 shutdown —— 孙进程必须仍然死掉，兜底完全来自 `KILL_ON_JOB_CLOSE`。
#[cfg(windows)]
#[test]
fn grandchildren_join_the_job_and_die_when_the_runtime_closes() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("grandchildren.txt");
    let client = Client::spawn(config_spawning_grandchildren(1, &report)).unwrap();

    let lines = wait_for_report(&report, 1, Duration::from_secs(10));
    let grandchild: u32 = lines[0]
        .parse()
        .unwrap_or_else(|_| panic!("孙进程未能启动：{}", lines[0]));
    assert!(process_is_alive(grandchild), "孙进程应当已经跑起来");

    let stats = client
        .process_tree_stats()
        .expect("Windows 上整树统计必须可用");
    assert!(
        stats.active_processes >= 2,
        "job 内应同时含 pi 与孙进程，实际活跃 {}",
        stats.active_processes
    );
    assert!(
        stats.private_bytes > 0,
        "整树私有内存不应为 0，实际采样到 {} 个进程",
        stats.sampled_processes
    );

    // 孙进程继承着 stdout 管道的写端，所以整树回收必须发生在 reader join 之前。
    // 这条计时断言就是那个次序的回归护栏：次序一错，shutdown 会一直等到孙进程
    // 自己活满 `GRANDCHILD_MAX_LIFETIME`（120s），测试仍会"通过"但慢得离谱。
    let started = Instant::now();
    client.shutdown().unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "shutdown 耗时 {elapsed:?} —— 整树回收多半排到了 reader join 之后，被孙进程持有的管道拖住"
    );
    assert!(
        wait_until_dead(grandchild, Duration::from_secs(10)),
        "宿主退出后孙进程 {grandchild} 仍然存活 —— 整树回收没有生效"
    );
}

/// 显式整树终止走 Job Object，孙进程一并带走。
#[cfg(windows)]
#[test]
fn kill_process_tree_terminates_the_whole_job() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("grandchildren.txt");
    let client = Client::spawn(config_spawning_grandchildren(2, &report)).unwrap();

    let lines = wait_for_report(&report, 2, Duration::from_secs(10));
    let grandchildren: Vec<u32> = lines
        .iter()
        .map(|line| {
            line.parse()
                .unwrap_or_else(|_| panic!("孙进程未能启动：{line}"))
        })
        .collect();

    client.kill_process_tree().unwrap();
    for pid in grandchildren {
        assert!(
            wait_until_dead(pid, Duration::from_secs(10)),
            "整树终止后孙进程 {pid} 仍然存活"
        );
    }
    let _ = client.shutdown();
}

/// 进程数硬限制对"扩展自行 spawn 的子代理"同样有效。
///
/// 这正是立项文档说的、Manager 无法按任务语义限流、但 R25 能兜住的那一类进程。
#[cfg(windows)]
#[test]
fn active_process_limit_stops_the_extension_from_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("grandchildren.txt");
    let mut config = config_spawning_grandchildren(1, &report);
    // 只允许 pi 自己一个进程：孙进程创建必须在内核层就失败。
    config.job_limits = JobLimits {
        max_active_processes: Some(1),
        job_memory_bytes: None,
    };
    let client = Client::spawn(config).unwrap();

    let lines = wait_for_report(&report, 1, Duration::from_secs(10));
    assert!(
        lines[0].starts_with("ERROR:"),
        "活跃进程数上限为 1 时孙进程不应创建成功，实际得到：{}",
        lines[0]
    );

    // 硬限制不该误伤宿主自己。CREATE_NO_WINDOW 时 Windows 可能再挂一个隐藏
    // conhost，它会计入 job，但不能把宿主挤掉。
    let stats = client.process_tree_stats().unwrap();
    assert!(
        (1..=2).contains(&stats.active_processes),
        "宿主自身必须仍在 job 内正常运行，实际活跃 {}",
        stats.active_processes
    );
    client.shutdown().unwrap();
}

/// 非 Windows 平台明确报 `Unsupported`，绝不把"查不到"伪装成 0。
#[cfg(not(windows))]
#[test]
fn process_tree_stats_are_unsupported_off_windows() {
    use std::io::ErrorKind;

    let job = pi_rpc::JobObject::create(JobLimits::default()).unwrap();
    let error = job.stats().expect_err("非 Windows 平台不应返回统计值");
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    let error = job
        .terminate()
        .expect_err("非 Windows 平台不应声称终止成功");
    assert_eq!(error.kind(), ErrorKind::Unsupported);
}
