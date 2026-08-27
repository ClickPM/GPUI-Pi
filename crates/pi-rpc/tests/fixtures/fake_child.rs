use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// 孙进程的最长存活时间：测试一旦漏杀，它自己也会退，不至于在开发机上长期赖着。
const GRANDCHILD_MAX_LIFETIME: Duration = Duration::from_secs(120);

fn main() {
    // R25：模拟"pi 的扩展自己 spawn 子代理"。孙进程重执行本二进制，不依赖任何系统命令。
    if env::args_os().any(|arg| arg == "--grandchild-sleep") {
        let started = Instant::now();
        while started.elapsed() < GRANDCHILD_MAX_LIFETIME {
            thread::sleep(Duration::from_millis(50));
        }
        return;
    }
    spawn_requested_grandchildren();

    let ephemeral = env::args_os().any(|arg| arg == "--no-session");
    let mut session_file = env::args_os()
        .collect::<Vec<_>>()
        .windows(2)
        .find(|args| args[0] == "--session")
        .map(|args| PathBuf::from(&args[1]));
    if session_file.is_none() {
        session_file = env::var_os("PI_RPC_FAKE_SESSION").map(PathBuf::from);
    }
    let mut session_file =
        session_file.unwrap_or_else(|| env::temp_dir().join("pi-rpc-fake-session.jsonl"));
    let mut session_id = session_file
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("fake-session")
        .to_owned();
    let available_models = json!([
        {
            "id":"model-one",
            "name":"Model One",
            "api":"fixture",
            "provider":"provider-one",
            "baseUrl":"https://fixture.invalid",
            "reasoning":true,
            "input":["text"],
            "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0},
            "contextWindow":100000,
            "maxTokens":4096
        },
        {
            "id":"model-two",
            "name":"Model Two",
            "api":"fixture",
            "provider":"provider-two",
            "baseUrl":"https://fixture.invalid",
            "reasoning":true,
            "input":["text","image"],
            "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0},
            "contextWindow":200000,
            "maxTokens":8192
        }
    ]);
    let mut current_model = available_models[0].clone();
    let mut thinking_level = "off".to_owned();

    let mut pending_extension_ui = Vec::<String>::new();
    let mut auto_compaction = true;
    let mut leaf_id = Some("assistant-leaf".to_owned());
    let mut get_state_failures = env::var("PI_RPC_FAKE_GET_STATE_FAILURES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let command_log = env::var_os("PI_RPC_FAKE_COMMAND_LOG").map(PathBuf::from);
    let tool_allowlist = env::args_os()
        .collect::<Vec<_>>()
        .windows(2)
        .find(|args| args[0] == "--tools")
        .map(|args| args[1].to_string_lossy().into_owned());

    eprintln!(
        "fake child ready tools={}",
        tool_allowlist.as_deref().unwrap_or("<unset>")
    );
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in BufReader::new(stdin.lock()).lines() {
        let line = line.unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let command = value["type"].as_str().unwrap();

        if command == "extension_ui_response" {
            let response_id = value["id"].as_str().unwrap_or_default();
            let valid = if value["cancelled"] == true {
                true
            } else {
                match response_id {
                    "ui-select" | "ui-input" | "ui-editor" => value.get("value").is_some(),
                    "ui-confirm" => value.get("confirmed").is_some(),
                    _ => false,
                }
            };
            if valid {
                pending_extension_ui.retain(|id| id != response_id);
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"extension_error","extensionPath":"fake-ui","event":"response","error":format!("received:{response_id}")})
                )
                .unwrap();
            }
            stdout.flush().unwrap();
            continue;
        }
        if let Some(path) = command_log.as_ref() {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            writeln!(options.open(path).unwrap(), "{command}").unwrap();
        }
        if command == "prompt" {
            let message = value["message"].as_str().unwrap_or_default();
            if message == "complete" {
                writeln!(stdout, "{}", json!({"type":"agent_start"})).unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"agent_end","messages":[],"willRetry":false})
                )
                .unwrap();
                writeln!(stdout, "{}", json!({"type":"agent_settled"})).unwrap();
                let response = json!({"id":id,"type":"response","command":command,"success":true});
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
            if message == "stream" {
                writeln!(stdout, "{}", json!({"type":"agent_start"})).unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"message_start","message":{"role":"assistant","content":[]}})
                )
                .unwrap();
                for index in 0..1500 {
                    writeln!(stdout, "{}", json!({
                        "type":"message_update",
                        "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
                        "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":if index == 1499 {"done"} else {"x"}}
                    })).unwrap();
                }
                writeln!(stdout, "{}", json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"authoritative"}]}})).unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"agent_end","messages":[],"willRetry":false})
                )
                .unwrap();
                writeln!(stdout, "{}", json!({"type":"agent_settled"})).unwrap();
                let response = json!({"id":id,"type":"response","command":command,"success":true});
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
            if message == "extension-ui" {
                let requests = [
                    json!({"type":"extension_ui_request","id":"ui-select","method":"select","title":"Select","options":["Alpha","Beta"],"timeout":1000}),
                    json!({"type":"extension_ui_request","id":"ui-confirm","method":"confirm","title":"Confirm","message":"Continue?","timeout":1000}),
                    json!({"type":"extension_ui_request","id":"ui-input","method":"input","title":"Input","placeholder":"value","timeout":1000}),
                    json!({"type":"extension_ui_request","id":"ui-editor","method":"editor","title":"Editor","prefill":"line 1\nline 2"}),
                    json!({"type":"extension_ui_request","id":"ui-notify","method":"notify","message":"Notice","notifyType":"warning"}),
                    json!({"type":"extension_ui_request","id":"ui-status","method":"setStatus","statusKey":"fixture","statusText":"Running"}),
                    json!({"type":"extension_ui_request","id":"ui-widget","method":"setWidget","widgetKey":"fixture","widgetLines":["one","two"]}),
                    json!({"type":"extension_ui_request","id":"ui-title","method":"setTitle","title":"Fixture Title"}),
                    json!({"type":"extension_ui_request","id":"ui-editor-text","method":"set_editor_text","text":"composer text"}),
                ];
                pending_extension_ui.extend(
                    ["ui-select", "ui-confirm", "ui-input", "ui-editor"]
                        .into_iter()
                        .map(str::to_owned),
                );
                for request in requests {
                    writeln!(stdout, "{request}").unwrap();
                }
                let response = json!({"id":id,"type":"response","command":command,"success":true});
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
            if message == "extension-ui-pending" {
                let response = json!({"id":id,"type":"response","command":command,"success":pending_extension_ui.is_empty(),"error":(!pending_extension_ui.is_empty()).then(|| format!("pending:{}", pending_extension_ui.join(",")))});
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
            if message == "wire-image" {
                let valid = value["streamingBehavior"] == "steer"
                    && value["images"][0]["type"] == "image"
                    && value["images"][0]["data"] == "iVBORw0KGgo="
                    && value["images"][0]["mimeType"] == "image/png";
                let response = if valid {
                    json!({"id":id,"type":"response","command":command,"success":true})
                } else {
                    json!({"id":id,"type":"response","command":command,"success":false,"error":"image wire mismatch"})
                };
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
            if message == "reject" {
                let response = json!({"id":id,"type":"response","command":command,"success":false,"error":"injected rejection"});
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
            if message == "crash" {
                stdout.flush().unwrap();
                std::process::exit(23);
            }
            if message == "queue" {
                writeln!(stdout, "{}", json!({"type":"agent_start"})).unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"queue_update","steering":["first"],"followUp":["later"]})
                )
                .unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"queue_update","steering":["replacement"],"followUp":[]})
                )
                .unwrap();
                writeln!(stdout, "{}", json!({"type":"agent_settled"})).unwrap();
                let response = json!({"id":id,"type":"response","command":command,"success":true});
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
            thread::sleep(Duration::from_secs(60));
            continue;
        }
        if command == "abort" {
            writeln!(stdout, "{}", json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"aborted tail"}],"stopReason":"aborted"}})).unwrap();
            writeln!(
                stdout,
                "{}",
                json!({"type":"agent_end","messages":[],"willRetry":false})
            )
            .unwrap();
            writeln!(stdout, "{}", json!({"type":"agent_settled"})).unwrap();
            let response = json!({"id":id,"type":"response","command":command,"success":true});
            writeln!(stdout, "{response}").unwrap();
            stdout.flush().unwrap();
            continue;
        }
        if command == "set_session_name" && value["name"] == "emit_many" {
            for _ in 0..2000 {
                writeln!(stdout, "{}", json!({"type":"agent_start"})).unwrap();
            }
            let response = json!({"id":id,"type":"response","command":command,"success":true});
            writeln!(stdout, "{response}").unwrap();
            stdout.flush().unwrap();
            continue;
        }
        if command == "emit" {
            writeln!(stdout, "{}", json!({"type":"agent_start"})).unwrap();
        }
        let response = match command {
            "get_state" if get_state_failures > 0 => {
                get_state_failures -= 1;
                json!({"id":id,"type":"response","command":"get_state","success":false,"error":"injected get_state failure"})
            }
            "get_state" => json!({
                "id": id,
                "type": "response",
                "command": "get_state",
                "success": true,
                "data": {
                    "model": current_model,
                    "thinkingLevel": thinking_level,
                    "toolAllowlist": tool_allowlist,
                    "isStreaming": false,
                    "isCompacting": false,
                    "steeringMode": "all",
                    "followUpMode": "all",
                    "sessionFile": (!ephemeral).then_some(&session_file),
                    "sessionId": session_id,
                    "autoCompactionEnabled": auto_compaction,
                    "messageCount": 0,
                    "pendingMessageCount": 0
                }
            }),
            "get_messages" => {
                json!({"id":id,"type":"response","command":"get_messages","success":true,"data":{"messages":[]}})
            }
            "get_available_models" => json!({
                "id":id,
                "type":"response",
                "command":"get_available_models",
                "success":true,
                "data":{"models":available_models}
            }),
            "get_available_thinking_levels" => json!({
                "id":id,
                "type":"response",
                "command":"get_available_thinking_levels",
                "success":true,
                "data":{"levels":["off","low","high"]}
            }),
            "set_model" => {
                let target = available_models.as_array().unwrap().iter().find(|model| {
                    model["provider"] == value["provider"] && model["id"] == value["modelId"]
                });
                if let Some(target) = target {
                    current_model = target.clone();
                    json!({"id":id,"type":"response","command":"set_model","success":true,"data":current_model})
                } else {
                    json!({"id":id,"type":"response","command":"set_model","success":false,"error":"model not found"})
                }
            }
            "cycle_model" => {
                current_model = if current_model["id"] == "model-one" {
                    available_models[1].clone()
                } else {
                    available_models[0].clone()
                };
                json!({"id":id,"type":"response","command":"cycle_model","success":true,"data":{"model":current_model,"thinkingLevel":thinking_level,"isScoped":false}})
            }
            "set_thinking_level" => {
                thinking_level = value["level"].as_str().unwrap_or("off").to_owned();
                json!({"id":id,"type":"response","command":"set_thinking_level","success":true})
            }
            "set_auto_compaction" => {
                auto_compaction = value["enabled"].as_bool().unwrap_or(true);
                json!({"id":id,"type":"response","command":command,"success":true})
            }
            "set_auto_retry" => {
                let enabled = value["enabled"].as_bool().unwrap_or(true);
                json!({"id":id,"type":"response","command":command,"success":true,"data":{"enabled":enabled}})
            }
            "abort_retry" => json!({"id":id,"type":"response","command":command,"success":true}),
            "get_tree" => json!({
                "id":id,"type":"response","command":command,"success":true,
                "data":{"tree":[{"entry":{"type":"message","id":"user-root","parentId":null,"timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"fork me"}},"children":[{"entry":{"type":"message","id":"assistant-leaf","parentId":"user-root","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":"answer"}},"children":[]}]}],"leafId":leaf_id}
            }),
            "get_fork_messages" => {
                json!({"id":id,"type":"response","command":command,"success":true,"data":{"messages":[{"entryId":"user-root","text":"fork me"}]}})
            }
            "switch_session" => {
                session_file = PathBuf::from(value["sessionPath"].as_str().unwrap());
                session_id = session_file
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("switched")
                    .to_owned();
                leaf_id = Some("assistant-leaf".to_owned());
                json!({"id":id,"type":"response","command":command,"success":true,"data":{"cancelled":false}})
            }
            "fork" => {
                let source = value["entryId"].as_str().unwrap_or("user-root");
                session_file = session_file.with_file_name("forked-session.jsonl");
                session_id = "forked-session".to_owned();
                leaf_id = Some(source.to_owned());
                json!({"id":id,"type":"response","command":command,"success":true,"data":{"text":"fork me","cancelled":false}})
            }
            "clone" => {
                session_file = session_file.with_file_name("cloned-session.jsonl");
                session_id = "cloned-session".to_owned();
                json!({"id":id,"type":"response","command":command,"success":true,"data":{"cancelled":false}})
            }
            "compact" => {
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"compaction_start","reason":"manual"})
                )
                .unwrap();
                writeln!(stdout, "{}", json!({"type":"compaction_end","reason":"manual","result":{"summary":"compact fixture","firstKeptEntryId":"user-root","tokensBefore":10,"estimatedTokensAfter":3},"aborted":false,"willRetry":false})).unwrap();
                json!({"id":id,"type":"response","command":command,"success":true,"data":{"summary":"compact fixture","firstKeptEntryId":"user-root","tokensBefore":10,"estimatedTokensAfter":3}})
            }
            "export_html" => {
                let path = PathBuf::from(value["outputPath"].as_str().unwrap());
                std::fs::write(&path, "<!doctype html><title>fixture</title>").unwrap();
                json!({"id":id,"type":"response","command":command,"success":true,"data":{"path":path}})
            }
            "get_commands" => json!({
                "id":id,
                "type":"response",
                "command":"get_commands",
                "success":true,
                "data":{"commands":[
                    {"name":"fixture-extension","description":"Extension fixture","source":"extension","sourceInfo":{"path":"/fixture/ext.ts","source":"fixture","scope":"user","origin":"top-level"}},
                    {"name":"fixture-prompt","description":"Prompt fixture","source":"prompt","sourceInfo":{"path":"/fixture/prompt.md","source":"fixture","scope":"project","origin":"top-level"}},
                    {"name":"skill:fixture","description":"Skill fixture","source":"skill","sourceInfo":{"path":"/fixture/SKILL.md","source":"fixture","scope":"user","origin":"top-level"}}
                ]}
            }),
            _ => json!({"id":id,"type":"response","command":command,"success":true}),
        };
        writeln!(stdout, "{response}").unwrap();
        stdout.flush().unwrap();
    }
}

/// 按 `PI_RPC_FAKE_SPAWN_GRANDCHILD` 生若干孙进程，把结果写进
/// `PI_RPC_FAKE_GRANDCHILD_PIDS` 指定的文件：成功写 pid，失败写 `ERROR:<原因>`。
///
/// 孙进程的三个标准流一律接 null —— 若让它继承 stdout 管道，父进程退出后管道仍有写端，
/// 宿主的 stdout reader 就永远等不到 EOF。
fn spawn_requested_grandchildren() {
    let Some(count) = env::var("PI_RPC_FAKE_SPAWN_GRANDCHILD")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    let report = env::var_os("PI_RPC_FAKE_GRANDCHILD_PIDS").map(PathBuf::from);
    let Ok(self_exe) = env::current_exe() else {
        return;
    };
    let mut lines = Vec::new();
    for _ in 0..count {
        let spawned = ProcessCommand::new(&self_exe)
            .arg("--grandchild-sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => lines.push(child.id().to_string()),
            Err(error) => lines.push(format!("ERROR:{error}")),
        }
    }
    if let Some(report) = report {
        let mut body = lines.join("\n");
        body.push('\n');
        let _ = fs::write(report, body);
    }
}
