use std::{
    path::PathBuf,
    process::Command as ProcessCommand,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use pi_rpc::{
    Client, ClientConfig, ClientError, ClientEvent, CloneData, Command, CommandsData,
    ExportPathData, ExtensionUiRequest, ExtensionUiResponse, ForkData, ForkMessagesData,
    ImageContent, ImageKind, LifecycleEvent, RpcEvent, RpcSessionState, SlashCommandSource,
    SwitchSessionData, TreeData,
};
use serde_json::Value;

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_child"))
}

fn config() -> ClientConfig {
    let mut config = ClientConfig::new(fake_binary());
    config.restart_delay = Duration::from_millis(20);
    config
}

#[test]
fn correlates_concurrent_requests_and_drains_stderr() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    let client = Arc::new(client);
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let client = Arc::clone(&client);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            client
                .request(Command::GetMessages, Duration::from_secs(2))
                .unwrap()
        }));
    }
    barrier.wait();
    let first = handles.remove(0).join().unwrap();
    let second = handles.remove(0).join().unwrap();
    assert_ne!(first.id, second.id);

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_stderr = false;
    while Instant::now() < deadline {
        if let Ok(ClientEvent::Lifecycle(LifecycleEvent::Stderr { line })) =
            events.recv_timeout(Duration::from_millis(50))
        {
            saw_stderr = line.contains("fake child ready");
            if saw_stderr {
                break;
            }
        }
    }
    assert!(saw_stderr);
    client.shutdown().unwrap();
}

#[test]
fn initial_session_is_used_by_the_first_spawn() {
    let initial = std::env::temp_dir().join(format!(
        "pi-rpc-initial-session-{}.jsonl",
        std::process::id()
    ));
    let mut child_config = config();
    child_config.initial_session = Some(initial.clone());
    let client = Client::spawn(child_config).unwrap();
    let state: RpcSessionState = client
        .request_data(Command::GetState, Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        state.session_file.as_deref().map(PathBuf::from),
        Some(initial.clone())
    );
    assert_eq!(client.resume_session(), Some(initial));
    client.shutdown().unwrap();
}

#[test]
fn new_client_can_resume_the_same_session_with_a_new_tool_allowlist() {
    let session = std::env::temp_dir().join(format!(
        "pi-rpc-tool-restart-session-{}.jsonl",
        std::process::id()
    ));
    let mut initial = config();
    initial.initial_session = Some(session.clone());
    initial
        .args
        .extend(["--tools".into(), "read,bash,edit,write".into()]);
    let first = Client::spawn(initial).unwrap();
    let first_state: Value = first
        .request_data(Command::GetState, Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        first_state["sessionFile"],
        session.to_string_lossy().as_ref()
    );
    assert_eq!(first_state["toolAllowlist"], "read,bash,edit,write");
    let first_pid = first.pid().unwrap();
    first.shutdown().unwrap();

    let mut restarted = config();
    restarted.initial_session = Some(session.clone());
    restarted
        .args
        .extend(["--tools".into(), "read,grep,find,ls".into()]);
    let second = Client::spawn(restarted).unwrap();
    let second_state: Value = second
        .request_data(Command::GetState, Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        second_state["sessionFile"],
        session.to_string_lossy().as_ref()
    );
    // fake child 的 sessionId 由同一个 --session 路径派生；这里只验证重启参数透传。
    assert_eq!(second_state["sessionId"], first_state["sessionId"]);
    assert_eq!(second_state["toolAllowlist"], "read,grep,find,ls");
    assert_ne!(second.pid().unwrap(), first_pid);
    second.shutdown().unwrap();
}

#[test]
fn empty_tool_allowlist_is_preserved_as_an_explicit_argument() {
    let mut child_config = config();
    child_config.args.extend(["--tools".into(), "".into()]);
    let client = Client::spawn(child_config).unwrap();
    let state: Value = client
        .request_data(Command::GetState, Duration::from_secs(2))
        .unwrap();
    assert_eq!(state["toolAllowlist"], "");
    client.shutdown().unwrap();
}

#[test]
fn crash_fails_old_pending_then_restarts_with_session() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    let state: RpcSessionState = client
        .request_data(Command::GetState, Duration::from_secs(2))
        .unwrap();
    let session_file = state.session_file.map(PathBuf::from).unwrap();

    let pending_client = client.clone();
    let pending = thread::spawn(move || {
        pending_client.request(
            Command::Prompt {
                message: "ignored".into(),
                images: None,
                streaming_behavior: None,
            },
            Duration::from_secs(5),
        )
    });
    thread::sleep(Duration::from_millis(50));
    client.kill_process_tree().unwrap();
    assert!(matches!(
        pending.join().unwrap(),
        Err(ClientError::ProcessExited { .. })
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut resumed = false;
    while Instant::now() < deadline {
        if let Ok(ClientEvent::Lifecycle(LifecycleEvent::Restarted {
            session_file: actual,
            ..
        })) = events.recv_timeout(Duration::from_millis(100))
        {
            resumed = actual.as_deref() == Some(session_file.as_path());
            if resumed {
                break;
            }
        }
    }
    assert!(resumed);
    let restored: RpcSessionState = client
        .request_data(Command::GetState, Duration::from_secs(2))
        .unwrap();
    assert_eq!(restored.session_id, state.session_id);
    client.shutdown().unwrap();
}

#[test]
fn ephemeral_state_clears_a_previous_resume_target() {
    let mut child_config = config();
    child_config.args.push("--no-session".into());
    let client = Client::spawn(child_config).unwrap();
    client.set_resume_session(Some(PathBuf::from("stale-session.jsonl")));
    let state: RpcSessionState = client
        .request_data(Command::GetState, Duration::from_secs(2))
        .unwrap();
    assert!(state.session_file.is_none());
    assert!(client.resume_session().is_none());
    client.shutdown().unwrap();
}

#[test]
fn active_shutdown_does_not_restart() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    client.shutdown().unwrap();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        if let Ok(ClientEvent::Lifecycle(LifecycleEvent::Restarting { .. })) =
            events.recv_timeout(Duration::from_millis(20))
        {
            panic!("active shutdown restarted the process");
        }
    }
}

#[test]
fn external_tree_kill_works_for_fake_child() {
    let client = Client::spawn(config()).unwrap();
    let pid = client.pid().unwrap();
    #[cfg(windows)]
    let status = ProcessCommand::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .unwrap();
    #[cfg(unix)]
    let status = ProcessCommand::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap();
    assert!(status.status.success());
    client.shutdown().unwrap();
}

#[test]
fn shutdown_kills_a_child_that_does_not_handle_stdin_eof() {
    let mut child_config = config();
    child_config.shutdown_grace_period = Duration::from_millis(50);
    let client = Client::spawn(child_config).unwrap();
    let pending_client = client.clone();
    let pending = thread::spawn(move || {
        pending_client.request(
            Command::Prompt {
                message: "ignored".into(),
                images: None,
                streaming_behavior: None,
            },
            Duration::from_secs(5),
        )
    });
    thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    client.shutdown().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        pending.join().unwrap(),
        Err(ClientError::ProcessExited { .. })
    ));
}

#[test]
fn burst_subscription_keeps_authoritative_tail_events() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    let response = client
        .request(
            Command::Prompt {
                message: "stream".into(),
                images: None,
                streaming_behavior: None,
            },
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(response.success);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut updates = 0;
    let mut saw_message_end = false;
    let mut saw_agent_end = false;
    let mut saw_settled = false;
    while Instant::now() < deadline && !saw_settled {
        if let Ok(ClientEvent::Rpc(event)) = events.recv_timeout(Duration::from_millis(50)) {
            match *event {
                pi_rpc::RpcEvent::MessageUpdate { .. } => updates += 1,
                pi_rpc::RpcEvent::MessageEnd { .. } => saw_message_end = true,
                pi_rpc::RpcEvent::AgentEnd { .. } => saw_agent_end = true,
                pi_rpc::RpcEvent::AgentSettled => saw_settled = true,
                _ => {}
            }
        }
    }
    assert_eq!(updates, 1500);
    assert!(saw_message_end && saw_agent_end && saw_settled);
    client.shutdown().unwrap();
}

#[test]
fn get_commands_decodes_typed_sources_and_image_prompt_preserves_wire() {
    let client = Client::spawn(config()).unwrap();
    let commands: CommandsData = client
        .request_data(Command::GetCommands, Duration::from_secs(2))
        .unwrap();
    assert_eq!(commands.commands.len(), 3);
    assert_eq!(commands.commands[0].source, SlashCommandSource::Extension);
    assert_eq!(commands.commands[1].source, SlashCommandSource::Prompt);
    assert_eq!(commands.commands[2].source, SlashCommandSource::Skill);

    let response = client
        .request(
            Command::Prompt {
                message: "wire-image".into(),
                images: Some(vec![ImageContent {
                    kind: ImageKind::Image,
                    data: "iVBORw0KGgo=".into(),
                    mime_type: "image/png".into(),
                }]),
                streaming_behavior: Some(pi_rpc::StreamingBehavior::Steer),
            },
            Duration::from_secs(2),
        )
        .unwrap();
    assert!(response.success);
    client.shutdown().unwrap();
}

#[test]

fn extension_ui_wire_decodes_all_nine_requests_and_writes_four_responses() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    assert!(
        client
            .request(
                Command::Prompt {
                    message: "extension-ui".into(),
                    images: None,
                    streaming_behavior: None,
                },
                Duration::from_secs(2),
            )
            .unwrap()
            .success
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut requests = Vec::new();
    while Instant::now() < deadline && requests.len() < 9 {
        if let Ok(ClientEvent::Rpc(event)) = events.recv_timeout(Duration::from_millis(50))
            && let RpcEvent::ExtensionUiRequest { id, request } = *event
        {
            requests.push((id, request));
        }
    }
    assert_eq!(requests.len(), 9);
    assert!(matches!(requests[0].1, ExtensionUiRequest::Select { .. }));
    assert!(matches!(requests[1].1, ExtensionUiRequest::Confirm { .. }));
    assert!(matches!(requests[2].1, ExtensionUiRequest::Input { .. }));
    assert!(matches!(requests[3].1, ExtensionUiRequest::Editor { .. }));
    assert!(matches!(requests[4].1, ExtensionUiRequest::Notify { .. }));
    assert!(matches!(
        requests[5].1,
        ExtensionUiRequest::SetStatus { .. }
    ));
    assert!(matches!(
        requests[6].1,
        ExtensionUiRequest::SetWidget { .. }
    ));
    assert!(matches!(requests[7].1, ExtensionUiRequest::SetTitle { .. }));
    assert!(matches!(
        requests[8].1,
        ExtensionUiRequest::SetEditorText { .. }
    ));

    client
        .send_extension_ui_response(&ExtensionUiResponse::value("ui-select", "Alpha"))
        .unwrap();
    client
        .send_extension_ui_response(&ExtensionUiResponse::confirmed("ui-confirm", true))
        .unwrap();
    client
        .send_extension_ui_response(&ExtensionUiResponse::cancelled("ui-input"))
        .unwrap();
    client
        .send_extension_ui_response(&ExtensionUiResponse::value("ui-editor", "edited"))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut acknowledgements = 0;
    while Instant::now() < deadline && acknowledgements < 4 {
        if let Ok(ClientEvent::Rpc(event)) = events.recv_timeout(Duration::from_millis(50))
            && let RpcEvent::ExtensionError { error, .. } = *event
            && error.starts_with("received:ui-")
        {
            acknowledgements += 1;
        }
    }
    assert_eq!(acknowledgements, 4);
    assert!(
        client
            .request(
                Command::Prompt {
                    message: "extension-ui-pending".into(),
                    images: None,
                    streaming_behavior: None,
                },
                Duration::from_secs(2),
            )
            .unwrap()
            .success
    );
    client.shutdown().unwrap();
}

#[test]
fn session_rebind_calibration_failure_preserves_main_success_and_never_retries_command() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("initial.jsonl");
    let switched = temp.path().join("switched.jsonl");
    let command_log = temp.path().join("commands.log");
    let mut child_config = config();
    child_config.initial_session = Some(initial.clone());
    child_config.env.extend([
        ("PI_RPC_FAKE_GET_STATE_FAILURES".into(), "3".into()),
        (
            "PI_RPC_FAKE_COMMAND_LOG".into(),
            command_log.as_os_str().to_owned(),
        ),
    ]);
    let client = Client::spawn(child_config).unwrap();

    let switched_outcome = client
        .request_session_rebind_data::<SwitchSessionData>(
            Command::SwitchSession {
                session_path: switched.to_string_lossy().into_owned(),
            },
            Duration::from_secs(2),
        )
        .unwrap();
    assert!(!switched_outcome.data.cancelled);
    assert!(switched_outcome.calibration.unwrap().is_err());
    assert_eq!(
        client.resume_session().as_deref(),
        Some(switched.as_path()),
        "switch 的已知目标在校准失败时仍可安全恢复"
    );

    let cloned_outcome = client
        .request_session_rebind_data::<CloneData>(Command::Clone, Duration::from_secs(2))
        .unwrap();
    assert!(!cloned_outcome.data.cancelled);
    assert!(cloned_outcome.calibration.unwrap().is_ok());
    assert_eq!(
        client.resume_session().unwrap().file_name().unwrap(),
        "cloned-session.jsonl"
    );

    let commands = std::fs::read_to_string(command_log).unwrap();
    assert_eq!(
        commands
            .lines()
            .filter(|line| *line == "switch_session")
            .count(),
        1
    );
    assert_eq!(commands.lines().filter(|line| *line == "clone").count(), 1);
    assert_eq!(
        commands.lines().filter(|line| *line == "get_state").count(),
        4
    );
    client.shutdown().unwrap();
}

#[test]
fn fork_calibration_failure_is_structured_and_clears_stale_resume_target() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("initial.jsonl");
    let command_log = temp.path().join("commands.log");
    let mut child_config = config();
    child_config.initial_session = Some(initial);
    child_config.env.extend([
        ("PI_RPC_FAKE_GET_STATE_FAILURES".into(), "3".into()),
        (
            "PI_RPC_FAKE_COMMAND_LOG".into(),
            command_log.as_os_str().to_owned(),
        ),
    ]);
    let client = Client::spawn(child_config).unwrap();
    let outcome = client
        .request_session_rebind_data::<ForkData>(
            Command::Fork {
                entry_id: "user-root".into(),
            },
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(outcome.data.text, "fork me");
    assert!(outcome.calibration.unwrap().is_err());
    assert_eq!(client.resume_session(), None, "不能静默回到旧会话");
    let commands = std::fs::read_to_string(command_log).unwrap();
    assert_eq!(commands.lines().filter(|line| *line == "fork").count(), 1);
    assert_eq!(
        commands.lines().filter(|line| *line == "get_state").count(),
        3
    );
    client.shutdown().unwrap();
}

#[test]
fn r13_commands_update_resume_target_and_export_html() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("initial.jsonl");
    let switched = temp.path().join("switched.jsonl");
    let exported = temp.path().join("session.html");
    let mut child_config = config();
    child_config.initial_session = Some(initial);
    let client = Client::spawn(child_config).unwrap();

    let tree: TreeData = client
        .request_data(Command::GetTree, Duration::from_secs(2))
        .unwrap();
    assert_eq!(tree.leaf_id.as_deref(), Some("assistant-leaf"));
    assert_eq!(tree.tree[0].entry.id, "user-root");
    let messages: ForkMessagesData = client
        .request_data(Command::GetForkMessages, Duration::from_secs(2))
        .unwrap();
    assert_eq!(messages.messages[0].text, "fork me");

    let switched_data: SwitchSessionData = client
        .request_data(
            Command::SwitchSession {
                session_path: switched.to_string_lossy().into_owned(),
            },
            Duration::from_secs(2),
        )
        .unwrap();
    assert!(!switched_data.cancelled);
    assert_eq!(client.resume_session(), Some(switched));

    let forked: ForkData = client
        .request_data(
            Command::Fork {
                entry_id: "user-root".into(),
            },
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(forked.text, "fork me");
    assert_eq!(
        client.resume_session().unwrap().file_name().unwrap(),
        "forked-session.jsonl"
    );
    let cloned: CloneData = client
        .request_data(Command::Clone, Duration::from_secs(2))
        .unwrap();
    assert!(!cloned.cancelled);
    assert_eq!(
        client.resume_session().unwrap().file_name().unwrap(),
        "cloned-session.jsonl"
    );

    for command in [
        Command::SetAutoCompaction { enabled: false },
        Command::SetAutoRetry { enabled: false },
        Command::AbortRetry,
    ] {
        assert!(
            client
                .request(command, Duration::from_secs(2))
                .unwrap()
                .success
        );
    }
    let path: ExportPathData = client
        .request_data(
            Command::ExportHtml {
                output_path: Some(exported.to_string_lossy().into_owned()),
            },
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(PathBuf::from(path.path), exported);
    assert!(
        std::fs::read_to_string(exported)
            .unwrap()
            .contains("doctype html")
    );
    client.shutdown().unwrap();
}

#[test]
fn fake_queue_and_abort_emit_settled_tails() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    client
        .request(
            Command::Prompt {
                message: "queue".into(),
                images: None,
                streaming_behavior: Some(pi_rpc::StreamingBehavior::FollowUp),
            },
            Duration::from_secs(2),
        )
        .unwrap();
    let mut queue_snapshots = Vec::new();
    let mut settled = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && settled == 0 {
        if let Ok(ClientEvent::Rpc(event)) = events.recv_timeout(Duration::from_millis(50)) {
            match *event {
                pi_rpc::RpcEvent::QueueUpdate {
                    steering,
                    follow_up,
                } => {
                    queue_snapshots.push((steering, follow_up));
                }
                pi_rpc::RpcEvent::AgentSettled => settled += 1,
                _ => {}
            }
        }
    }
    assert_eq!(queue_snapshots.len(), 2);
    assert_eq!(queue_snapshots[1].0, ["replacement"]);
    assert!(queue_snapshots[1].1.is_empty());

    client
        .request(Command::Abort, Duration::from_secs(2))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut abort_message_end = false;
    while Instant::now() < deadline {
        if let Ok(ClientEvent::Rpc(event)) = events.recv_timeout(Duration::from_millis(50)) {
            match *event {
                pi_rpc::RpcEvent::MessageEnd { .. } => abort_message_end = true,
                pi_rpc::RpcEvent::AgentSettled => break,
                _ => {}
            }
        }
    }
    assert!(abort_message_end);
    client.shutdown().unwrap();
}

/// R22：任何背压路径下 stdout 都必须持续 drain，绝不因下游满而阻塞 pi。
///
/// 订阅者完全停摆，且积压额度被压到远小于本次事件量。要求同时成立：
/// 1. 子进程写完 1500 条事件后仍能返回 prompt 响应 —— 证明它从未被写阻塞；
/// 2. 订阅队列字节数始终不超过配置额度 —— 证明这条链路有界；
/// 3. 溢出以显式终态事件收场，而不是静默丢事件 —— 丢事件会破坏正文完整性。
#[test]
fn stdout_keeps_draining_while_a_subscriber_stalls_and_the_backlog_stays_bounded() {
    const BACKLOG_LIMIT: usize = 8 * 1024;
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
    let mut config = config();
    config.event_backlog_bytes = BACKLOG_LIMIT;
    let client = Client::spawn(config).unwrap();
    let events = client.subscribe();

    let started = Instant::now();
    let response = client
        .request(
            Command::Prompt {
                message: "stream".into(),
                images: None,
                streaming_behavior: None,
            },
            REQUEST_TIMEOUT,
        )
        .unwrap();
    // 这条断言本身就是「stdout 未被阻塞」的证明：fake child 先写完 1500 条事件、
    // 最后才写 response，只有 supervisor 全程持续 drain 才可能收到成功响应。
    assert!(
        response.success,
        "订阅者停摆时子进程仍必须能写完 stdout 并应答"
    );
    assert!(
        started.elapsed() < REQUEST_TIMEOUT,
        "子进程被下游背压拖慢即视为阻塞 pi"
    );
    assert!(
        events.queued_bytes() <= BACKLOG_LIMIT,
        "订阅积压必须有界：{} > {BACKLOG_LIMIT}",
        events.queued_bytes()
    );

    // 排空订阅：溢出前送达的必须是事件流的**连续前缀**（无空洞），
    // 末尾必须是显式的溢出终态，之后流关闭。
    let mut delivered = Vec::new();
    let mut overflow = None;
    loop {
        match events.try_recv() {
            Ok(ClientEvent::Lifecycle(LifecycleEvent::EventBacklogOverflow {
                queued_bytes,
                limit,
            })) => {
                assert!(overflow.is_none(), "溢出终态只应出现一次");
                overflow = Some((queued_bytes, limit));
            }
            Ok(event) => {
                assert!(
                    overflow.is_none(),
                    "溢出终态之后不应再有事件：这条流已经断开"
                );
                delivered.push(event);
            }
            Err(_) => break,
        }
    }
    let (queued_bytes, limit) = overflow.expect("积压超限必须以显式终态事件收场，不能静默丢事件");
    assert_eq!(limit, BACKLOG_LIMIT);
    assert!(queued_bytes <= BACKLOG_LIMIT);

    // fake child 的 "stream" 序列固定为 agent_start → message_start → 1500×message_update
    // → message_end → agent_end → agent_settled。校验 RPC 前缀逐条对得上，才能证明
    // 「溢出前的事件完整送达」，而不是零散丢了几条。
    // （stderr 的启动横幅走 Lifecycle，与 RPC 事件流无关，这里先滤掉。）
    let rpc_events = delivered
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Rpc(rpc) => Some(rpc.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        rpc_events.len() > 2,
        "溢出前至少应送达开头几条 RPC 事件，实际 {}",
        rpc_events.len()
    );
    assert!(matches!(rpc_events[0], RpcEvent::AgentStart));
    assert!(matches!(rpc_events[1], RpcEvent::MessageStart { .. }));
    for (index, event) in rpc_events.iter().enumerate().skip(2) {
        assert!(
            matches!(event, RpcEvent::MessageUpdate { .. }),
            "第 {index} 条应仍在连续的 message_update 前缀内，实际 {event:?}"
        );
    }
    assert!(
        rpc_events.len() < 1502,
        "本用例的额度必须小到真的触发溢出，否则等于没测背压"
    );

    client.shutdown().unwrap();
}

/// 默认额度足够宽，正常会话不会被溢出终态误伤。
#[test]
fn default_backlog_budget_delivers_a_full_streaming_burst() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    assert!(
        client
            .request(
                Command::Prompt {
                    message: "stream".into(),
                    images: None,
                    streaming_behavior: None,
                },
                Duration::from_secs(10),
            )
            .unwrap()
            .success
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut updates = 0;
    let mut settled = false;
    while Instant::now() < deadline && !settled {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(ClientEvent::Rpc(event)) => match *event {
                RpcEvent::MessageUpdate { .. } => updates += 1,
                RpcEvent::AgentSettled => settled = true,
                _ => {}
            },
            Ok(ClientEvent::Lifecycle(LifecycleEvent::EventBacklogOverflow { .. })) => {
                panic!("默认额度不应在一次普通流式回复中溢出")
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert_eq!(updates, 1500);
    assert!(settled);
    assert_eq!(events.queued_bytes(), 0, "全部消费后额度必须完全归还");
    client.shutdown().unwrap();
}

/// R23：`detach` 必须在**保留进程**的前提下确定性地结束一条订阅。
///
/// Park 的前提就是这一条：pump 线程阻塞在 `recv` 上，而空闲会话不会再产生任何事件，
/// 靠等下一条业务事件唤醒等于永远等下去；靠丢 `Client` 唤醒又会把进程一起杀掉。
#[test]
fn detach_wakes_a_blocked_subscriber_and_leaves_the_process_running() {
    let client = Client::spawn(config()).unwrap();
    let events = client.subscribe();
    let detach = events.detach_handle();

    // 消费线程阻塞在 recv 上：没有哨兵它就不会返回。
    let consumer = thread::spawn(move || {
        let mut tail = Vec::new();
        while let Ok(event) = events.recv() {
            tail.push(event);
        }
        tail
    });

    // 先把启动横幅之类的既有事件放过去，再断开，确保唤醒的确实是一次阻塞中的 recv。
    thread::sleep(Duration::from_millis(50));
    detach.detach();
    let tail = consumer.join().expect("consumer thread");
    assert!(
        matches!(
            tail.last(),
            Some(ClientEvent::Lifecycle(LifecycleEvent::Detached))
        ),
        "断开必须以显式哨兵收场，最后收到的是 {:?}",
        tail.last()
    );

    // 进程仍然活着：这正是 Park 到 warm pool 的价值所在。
    assert!(client.pid().is_some());
    let response = client
        .request(Command::GetMessages, Duration::from_secs(5))
        .expect("detach 之后进程必须仍然可用");
    assert!(response.success);

    // 幂等：重复 detach 只是找不到自己。
    detach.detach();
    client.shutdown().unwrap();
}

/// 断开一条订阅不得影响其他订阅者，也不得影响 stdout 的持续 drain。
#[test]
fn detaching_one_subscriber_does_not_disturb_the_others() {
    let client = Client::spawn(config()).unwrap();
    let parked = client.subscribe();
    let live = client.subscribe();

    parked.detach();
    assert!(
        matches!(
            parked.recv(),
            Ok(ClientEvent::Lifecycle(LifecycleEvent::Detached))
        ),
        "被断开的订阅先收到哨兵"
    );
    assert!(parked.recv().is_err(), "哨兵之后本订阅即关闭");

    let response = client
        .request(
            Command::Prompt {
                message: "complete".into(),
                images: None,
                streaming_behavior: None,
            },
            Duration::from_secs(10),
        )
        .unwrap();
    assert!(response.success, "另一条订阅在场时 stdout 必须继续被 drain");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_agent_event = false;
    while Instant::now() < deadline {
        match live.recv_timeout(Duration::from_millis(100)) {
            Ok(ClientEvent::Rpc(event)) => {
                if matches!(*event, RpcEvent::AgentStart) {
                    saw_agent_event = true;
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_agent_event, "未被断开的订阅必须照常收到事件");
    assert_eq!(parked.queued_bytes(), 0);
    client.shutdown().unwrap();
}
