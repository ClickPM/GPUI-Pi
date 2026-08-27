use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use pi_rpc::{
    AvailableModelsData, BashResult, Client, ClientConfig, ClientEvent, CloneData, Command,
    CommandsData, EntriesData, ExportPathData, ExtensionUiRequest, ExtensionUiResponse, ForkData,
    LifecycleEvent, MessagesData, PINNED_PI_VERSION, QueueMode, RpcEvent, RpcSessionState,
    StreamingBehavior, ThinkingLevel, ThinkingLevelsData, TreeData, materialize_host_extension,
};
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);

fn configured_binary() -> PathBuf {
    env::var_os("PI_RPC_TEST_BINARY")
        .map(PathBuf::from)
        .expect("PI_RPC_TEST_BINARY must point to the official pi 0.84.2 binary")
}

fn assert_version(binary: &Path) {
    let output = ProcessCommand::new(binary)
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        PINNED_PI_VERSION
    );
}

struct TestClient {
    client: Client,
    _temp: TempDir,
}

fn client_config(temp: &TempDir) -> ClientConfig {
    let binary = configured_binary();
    assert_version(&binary);
    let session_dir = temp.path().join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let agent_dir = temp.path().join("agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let mut config = ClientConfig::new(binary);
    config
        .env
        .push(("PI_CODING_AGENT_DIR".into(), agent_dir.into_os_string()));
    config.args = vec![
        "--no-extensions".into(),
        "--no-skills".into(),
        "--no-prompt-templates".into(),
        "--no-context-files".into(),
        "--offline".into(),
        "--session-dir".into(),
        session_dir.into_os_string(),
    ];
    config.restart_delay = Duration::from_millis(100);
    config
}

fn client() -> TestClient {
    let temp = tempfile::tempdir().unwrap();
    let client = Client::spawn(client_config(&temp)).unwrap();
    TestClient {
        client,
        _temp: temp,
    }
}

fn remove_arg(config: &mut ClientConfig, arg: &str) {
    if let Some(index) = config.args.iter().position(|value| value == arg) {
        config.args.remove(index);
    }
}

fn canonical_path(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase()
}

fn wait_for_entry(client: &Client, custom_type: &str) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let entries: EntriesData = client
            .request_data(Command::GetEntries { since: None }, TIMEOUT)
            .unwrap();
        if let Some(entry) = entries.entries.iter().find(|entry| {
            entry.kind == "custom"
                && entry.fields.get("customType") == Some(&Value::String(custom_type.into()))
        }) {
            return entry
                .fields
                .get("data")
                .cloned()
                .unwrap_or_else(|| panic!("custom entry {custom_type} has no data: {entry:?}"));
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {custom_type}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn host_load_probe(path: &Path, custom_type: &str) {
    fs::write(
        path,
        format!(
            r#"import type {{ ExtensionAPI }} from "@earendil-works/pi-coding-agent";
export default function hostLoadProbe(pi: ExtensionAPI): void {{
  pi.on("resources_discover", () => {{
    const host = pi.getAllTools().find((tool) =>
      tool.name === "bash" && tool.sourceInfo.path.includes("project-command-environment")
    );
    pi.appendEntry("{custom_type}", {{ loaded: host !== undefined }});
  }});
}}
"#
        ),
    )
    .unwrap();
}

fn host_extension_config(temp: &TempDir) -> ClientConfig {
    let mut config = client_config(temp);
    let extension = materialize_host_extension().unwrap();
    config
        .args
        .extend(["-e".into(), extension.into_os_string()]);
    config.env.extend([
        ("Port".into(), "host-port".into()),
        ("node_env".into(), "host-node-env".into()),
        ("Next_Runtime".into(), "host-next".into()),
        ("PI_R15_PRESERVED".into(), "preserved".into()),
    ]);
    config
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn project_command_environment_sanitizes_direct_rpc_bash() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("agent").join("bin")).unwrap();
    let probe_extension = temp.path().join("bash-owner-probe.ts");
    fs::write(
        &probe_extension,
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
export default function bashOwnerProbe(pi: ExtensionAPI): void {
  pi.on("resources_discover", () => {
    const bash = pi.getAllTools().find((tool) => tool.name === "bash");
    pi.appendEntry("r15-bash-owner", {
      sourceInfo: bash?.sourceInfo,
      description: bash?.description,
      promptGuidelines: bash?.promptGuidelines,
    });
  });
}
"#,
    )
    .unwrap();
    let mut config = host_extension_config(&temp);
    config
        .args
        .extend(["-e".into(), probe_extension.into_os_string()]);
    let host_extension = materialize_host_extension().unwrap();
    let client = Client::spawn(config).unwrap();
    let command = r#"printf '%s\n%s\n%s\n%s\n%s' "${Port-unset}" "${node_env-unset}" "${Next_Runtime-unset}" "$PI_R15_PRESERVED" "$PATH""#;
    let result: BashResult = client
        .request_data(
            Command::Bash {
                command: command.into(),
                exclude_from_context: Some(true),
            },
            TIMEOUT,
        )
        .unwrap();
    let mut lines = result.output.lines();
    assert_eq!(lines.next(), Some("unset"));
    assert_eq!(lines.next(), Some("unset"));
    assert_eq!(lines.next(), Some("unset"));
    assert_eq!(lines.next(), Some("preserved"));
    let path = lines.next().expect("PATH output");
    let normalized_path = path.replace('\\', "/").to_lowercase();
    let temp_name = temp
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap()
        .to_lowercase();
    assert!(
        normalized_path.contains(&format!("/{temp_name}/agent/bin")),
        "agent bin missing from PATH: {path}"
    );

    let owner = wait_for_entry(&client, "r15-bash-owner");
    assert_eq!(owner["sourceInfo"]["source"], "cli");
    assert_eq!(
        canonical_path(Path::new(owner["sourceInfo"]["path"].as_str().unwrap())),
        canonical_path(&host_extension)
    );
    assert_eq!(
        owner["description"],
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
    );
    assert_eq!(
        owner["promptGuidelines"][0],
        "You can inspect PI_* environment variables for current model and session details."
    );

    let explicit: BashResult = client
        .request_data(
            Command::Bash {
                command:
                    "export PORT=project-port NODE_ENV=project-mode; printf '%s|%s' \"$PORT\" \"$NODE_ENV\""
                        .into(),
                exclude_from_context: Some(true),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(explicit.output, "project-port|project-mode");
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn user_bash_override_keeps_priority_over_host_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let user_extension = temp.path().join("user-bash.ts");
    fs::write(
        &user_extension,
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
export default function userBash(pi: ExtensionAPI): void {
  pi.registerTool({
    name: "bash",
    label: "user bash",
    description: "user override",
    parameters: Type.Object({ command: Type.String() }),
    async execute() {
      return { content: [{ type: "text", text: "user-tool" }] };
    },
  });
  pi.on("user_bash", () => ({
    result: {
      output: "user-rpc",
      exitCode: 0,
      cancelled: false,
      truncated: false,
    },
  }));
  pi.on("session_start", () => {
    const bash = pi.getAllTools().find((tool) => tool.name === "bash");
    pi.appendEntry("r15-bash-owner", bash?.sourceInfo);
  });
}
"#,
    )
    .unwrap();
    let mut config = host_extension_config(&temp);
    config
        .args
        .extend(["-e".into(), user_extension.clone().into_os_string()]);
    let client = Client::spawn(config).unwrap();

    let result: BashResult = client
        .request_data(
            Command::Bash {
                command: "printf host-must-not-run".into(),
                exclude_from_context: Some(true),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(result.output, "user-rpc");

    let owner = wait_for_entry(&client, "r15-bash-owner");
    assert_eq!(owner["source"], "cli");
    assert_eq!(
        canonical_path(Path::new(owner["path"].as_str().unwrap())),
        canonical_path(&user_extension)
    );
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn readonly_allowlist_leaves_user_bash_handler_in_control() {
    let temp = tempfile::tempdir().unwrap();
    let user_extension = temp.path().join("user-bash-only.ts");
    fs::write(
        &user_extension,
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
export default function userBashOnly(pi: ExtensionAPI): void {
  pi.on("user_bash", () => ({
    result: { output: "readonly-user-rpc", exitCode: 0, cancelled: false, truncated: false },
  }));
}
"#,
    )
    .unwrap();
    let load_probe = temp.path().join("readonly-host-load-probe.ts");
    host_load_probe(&load_probe, "r15-readonly-host-loaded");
    let mut config = host_extension_config(&temp);
    config.args.extend([
        "--tools".into(),
        "read,grep,find,ls".into(),
        "-e".into(),
        user_extension.into_os_string(),
        "-e".into(),
        load_probe.into_os_string(),
    ]);
    let client = Client::spawn(config).unwrap();
    let result: BashResult = client
        .request_data(
            Command::Bash {
                command: "printf host-must-not-run".into(),
                exclude_from_context: Some(true),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(result.output, "readonly-user-rpc");
    assert_eq!(
        wait_for_entry(&client, "r15-readonly-host-loaded")["loaded"],
        false
    );
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn later_session_start_tool_registration_keeps_priority() {
    let temp = tempfile::tempdir().unwrap();
    let user_extension = temp.path().join("dynamic-user-bash.ts");
    fs::write(
        &user_extension,
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
export default function dynamicUserBash(pi: ExtensionAPI): void {
  pi.on("session_start", async () => {
    await new Promise((resolve) => setTimeout(resolve, 10));
    pi.registerTool({
      name: "bash",
      label: "dynamic user bash",
      description: "dynamic user override",
      parameters: Type.Object({ command: Type.String() }),
      async execute() { return { content: [{ type: "text", text: "dynamic-user-tool" }] }; },
    });
  });
  pi.on("resources_discover", () => {
    const bash = pi.getAllTools().find((tool) => tool.name === "bash");
    pi.appendEntry("r15-dynamic-owner", bash?.sourceInfo);
  });
  pi.on("user_bash", () => ({
    result: { output: "dynamic-user-rpc", exitCode: 0, cancelled: false, truncated: false },
  }));
}
"#,
    )
    .unwrap();
    let host_probe = temp.path().join("dynamic-host-load-probe.ts");
    host_load_probe(&host_probe, "r15-dynamic-host-loaded");
    let mut config = host_extension_config(&temp);
    config.args.extend([
        "-e".into(),
        user_extension.clone().into_os_string(),
        "-e".into(),
        host_probe.into_os_string(),
    ]);
    let client = Client::spawn(config).unwrap();
    assert_eq!(
        wait_for_entry(&client, "r15-dynamic-host-loaded")["loaded"],
        false
    );
    let owner = wait_for_entry(&client, "r15-dynamic-owner");
    assert_eq!(owner["source"], "cli");
    assert_eq!(
        canonical_path(Path::new(owner["path"].as_str().unwrap())),
        canonical_path(&user_extension)
    );
    let result: BashResult = client
        .request_data(
            Command::Bash {
                command: "printf host-must-not-run".into(),
                exclude_from_context: Some(true),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(result.output, "dynamic-user-rpc");
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn auto_discovered_user_extension_keeps_priority() {
    let temp = tempfile::tempdir().unwrap();
    let extension_dir = temp.path().join("agent").join("extensions");
    fs::create_dir_all(&extension_dir).unwrap();
    let user_extension = extension_dir.join("user-bash.ts");
    fs::write(
        &user_extension,
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
export default function userBash(pi: ExtensionAPI): void {
  pi.registerTool({
    name: "bash",
    label: "user bash",
    description: "auto user override",
    parameters: Type.Object({ command: Type.String() }),
    async execute() { return { content: [{ type: "text", text: "auto-user-tool" }] }; },
  });
  pi.on("user_bash", () => ({
    result: { output: "auto-user-rpc", exitCode: 0, cancelled: false, truncated: false },
  }));
  pi.on("resources_discover", () => {
    const bash = pi.getAllTools().find((tool) => tool.name === "bash");
    pi.appendEntry("r15-auto-user-owner", bash?.sourceInfo);
  });
}
"#,
    )
    .unwrap();
    let host_probe = temp.path().join("auto-host-load-probe.ts");
    host_load_probe(&host_probe, "r15-auto-host-loaded");
    let mut config = host_extension_config(&temp);
    config
        .args
        .extend(["-e".into(), host_probe.into_os_string()]);
    remove_arg(&mut config, "--no-extensions");
    let client = Client::spawn(config).unwrap();
    let result: BashResult = client
        .request_data(
            Command::Bash {
                command: "printf host-must-not-run".into(),
                exclude_from_context: Some(true),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(result.output, "auto-user-rpc");
    assert_eq!(
        wait_for_entry(&client, "r15-auto-host-loaded")["loaded"],
        false
    );
    let owner = wait_for_entry(&client, "r15-auto-user-owner");
    assert_eq!(owner["source"], "auto");
    assert_eq!(owner["scope"], "user");
    assert_eq!(owner["origin"], "top-level");
    assert_eq!(
        canonical_path(Path::new(owner["path"].as_str().unwrap())),
        canonical_path(&user_extension)
    );
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn third_party_subagent_tool_is_excluded_while_other_extensions_survive() {
    // 关掉 R26 的 T2 盲区：当初的内核加载实测带着 `--no-extensions`，与生产形态
    // （不带该参数、用户全局扩展同场）不同。这里按生产形态复现「两套子代理并存」：
    // 密封 agent 目录里装一个注册三个第三方子代理工具的样板 + 一个注册普通工具的
    // 用户扩展，再显式 `-e` 内核并带上生产恒加的 `--exclude-tools` 拉黑对。
    let kernel = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("vendor")
        .join(pi_rpc::subagent_kernel_dir_name());
    assert!(
        kernel.join("package.json").is_file(),
        "内核未就绪，先跑 scripts/fetch-pi-subagents-lite.ps1：{}",
        kernel.display()
    );
    let temp = tempfile::tempdir().unwrap();
    let extension_dir = temp.path().join("agent").join("extensions");
    fs::create_dir_all(&extension_dir).unwrap();
    // 样板一：模仿 npm:pi-subagents@0.56.0 在父会话注册的**全部三个**工具（派发
    // `subagent`、等待 `subagent_wait`、监督 `subagent_supervisor`；其第四个工具
    // `contact_supervisor` 有子进程门禁、不会出现在父会话，故不模仿），外加一个
    // 命令用来证明「扩展本身照常加载，被屏蔽的只是工具」。
    fs::write(
        extension_dir.join("fake-third-party-subagents.ts"),
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
export default function fakeThirdPartySubagents(pi: ExtensionAPI): void {
  for (const name of ["subagent", "subagent_wait", "subagent_supervisor"]) {
    pi.registerTool({
      name,
      label: `fake third-party ${name}`,
      description: "mimics npm:pi-subagents",
      parameters: Type.Object({ task: Type.String() }),
      async execute() { return { content: [{ type: "text", text: "must-not-be-visible" }] }; },
    });
  }
  pi.registerCommand("r26-fake-subagents-loaded", {
    description: "proves the extension itself still loads",
    handler: async () => {},
  });
}
"#,
    )
    .unwrap();
    // 样板二：用户的其他扩展 —— 注册一个普通工具，并在资源发现后回报全场工具可见性。
    fs::write(
        extension_dir.join("other-user-extension.ts"),
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
export default function otherUserExtension(pi: ExtensionAPI): void {
  pi.registerTool({
    name: "r26_user_other",
    label: "user other tool",
    description: "unrelated user tool that must survive",
    parameters: Type.Object({ value: Type.String() }),
    async execute() { return { content: [{ type: "text", text: "ok" }] }; },
  });
  pi.on("resources_discover", () => {
    const names = new Set(pi.getAllTools().map((tool) => tool.name));
    pi.appendEntry("r26-tool-visibility", {
      subagent: names.has("subagent"),
      subagentWait: names.has("subagent_wait"),
      subagentSupervisor: names.has("subagent_supervisor"),
      agent: names.has("Agent"),
      stopAgent: names.has("StopAgent"),
      agentStatus: names.has("AgentStatus"),
      other: names.has("r26_user_other"),
    });
  });
}
"#,
    )
    .unwrap();
    let mut config = client_config(&temp);
    remove_arg(&mut config, "--no-extensions");
    config.args.extend([
        "-e".into(),
        kernel.into_os_string(),
        "--exclude-tools".into(),
        pi_rpc::THIRD_PARTY_SUBAGENT_TOOL_NAMES.join(",").into(),
    ]);
    let client = Client::spawn(config).unwrap();
    let visibility = wait_for_entry(&client, "r26-tool-visibility");
    assert_eq!(visibility["subagent"], false, "{visibility}");
    assert_eq!(visibility["subagentWait"], false, "{visibility}");
    assert_eq!(visibility["subagentSupervisor"], false, "{visibility}");
    assert_eq!(visibility["agent"], true, "{visibility}");
    assert_eq!(visibility["stopAgent"], true, "{visibility}");
    assert_eq!(visibility["agentStatus"], true, "{visibility}");
    assert_eq!(visibility["other"], true, "{visibility}");
    let commands: CommandsData = client.request_data(Command::GetCommands, TIMEOUT).unwrap();
    assert!(
        commands
            .commands
            .iter()
            .any(|command| command.name == "r26-fake-subagents-loaded"),
        "第三方扩展本身应照常加载，被屏蔽的只是工具"
    );
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn zero_token_command_matrix() {
    let test = client();
    let client = &test.client;
    let state: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    assert!(!state.session_id.is_empty());
    let models: AvailableModelsData = client
        .request_data(Command::GetAvailableModels, TIMEOUT)
        .unwrap();
    assert!(models.models.iter().all(|model| {
        !model.id.is_empty() && !model.provider.is_empty() && !model.name.is_empty()
    }));
    let levels: ThinkingLevelsData = client
        .request_data(Command::GetAvailableThinkingLevels, TIMEOUT)
        .unwrap();
    assert!(!levels.levels.is_empty());
    assert!(levels.levels.contains(&state.thinking_level));

    let commands: CommandsData = client.request_data(Command::GetCommands, TIMEOUT).unwrap();
    assert!(commands.commands.iter().all(|command| {
        !command.name.is_empty()
            && !command.source_info.path.is_empty()
            && !command.source_info.source.is_empty()
    }));

    let can_cycle_model = models.models.len() > 1;
    let can_cycle_thinking = levels.levels.len() > 1;

    for command in [
        Command::GetMessages,
        Command::GetEntries { since: None },
        Command::GetTree,
        Command::GetSessionStats,
        Command::GetLastAssistantText,
        Command::SetThinkingLevel {
            level: ThinkingLevel::Off,
        },
        Command::SetSteeringMode {
            mode: QueueMode::All,
        },
        Command::SetSteeringMode {
            mode: QueueMode::OneAtATime,
        },
        Command::SetFollowUpMode {
            mode: QueueMode::All,
        },
        Command::SetFollowUpMode {
            mode: QueueMode::OneAtATime,
        },
        Command::SetAutoCompaction { enabled: false },
        Command::SetAutoCompaction { enabled: true },
        Command::SetAutoRetry { enabled: false },
        Command::SetAutoRetry { enabled: true },
        Command::Abort,
        Command::AbortRetry,
        Command::AbortBash,
    ] {
        let response = client.request(command, TIMEOUT).unwrap();
        assert!(
            response.success,
            "{}: {:?}",
            response.command, response.error
        );
    }

    if can_cycle_model {
        let response = client.request(Command::CycleModel, TIMEOUT).unwrap();
        assert!(response.success, "cycle_model: {:?}", response.error);
        let state: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
        let selected = state.model.expect("cycled model must be selected");
        assert!(
            models
                .models
                .iter()
                .any(|model| { model.provider == selected.provider && model.id == selected.id })
        );
    }
    if can_cycle_thinking {
        let response = client
            .request(Command::CycleThinkingLevel, TIMEOUT)
            .unwrap();
        assert!(
            response.success,
            "cycle_thinking_level: {:?}",
            response.error
        );
        let state: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
        assert!(levels.levels.contains(&state.thinking_level));
    }

    let invalid = client
        .request(
            Command::SetModel {
                provider: "missing-provider".into(),
                model_id: "missing-model".into(),
            },
            TIMEOUT,
        )
        .unwrap();
    assert!(!invalid.success);

    let invalid_cursor = client
        .request(
            Command::GetEntries {
                since: Some("missing-entry".into()),
            },
            TIMEOUT,
        )
        .unwrap();
    assert!(!invalid_cursor.success);

    let bash_events = client.subscribe();
    let bash_response = client
        .request(
            Command::Bash {
                command: "printf pi-rpc-ok".into(),
                exclude_from_context: Some(true),
            },
            TIMEOUT,
        )
        .unwrap();
    let bash_id = bash_response.id.clone().unwrap();
    let bash: BashResult = bash_response.decode_data().unwrap();
    assert!(bash.output.contains("pi-rpc-ok"));
    assert_eq!(bash.exit_code, Some(0));
    let event_deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_correlated_update = false;
    while Instant::now() < event_deadline {
        if let Ok(ClientEvent::Rpc(event)) = bash_events.recv_timeout(Duration::from_millis(100))
            && let pi_rpc::RpcEvent::BashExecutionUpdate { id, delta } = *event
            && id.as_deref() == Some(&bash_id)
            && delta.contains("pi-rpc-ok")
        {
            saw_correlated_update = true;
            break;
        }
    }
    assert!(saw_correlated_update);

    let before = state.session_id;
    let new_session: Value = client
        .request_data(
            Command::NewSession {
                parent_session: None,
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(new_session["cancelled"], false);
    let after: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    assert_ne!(before, after.session_id);
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn extension_ui_zero_token_fixture_reaches_nine_methods_and_custom_is_unreachable() {
    let temp = tempfile::tempdir().unwrap();
    let extension_path = temp.path().join("extension-ui-fixture.ts");
    fs::write(
        &extension_path,
        r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
export default function (pi: ExtensionAPI) {
  pi.registerCommand("r14-ui", {
    description: "R14 zero-token extension UI fixture",
    handler: async (_args, ctx) => {
      ctx.ui.notify("notify", "warning");
      ctx.ui.setStatus("r14", "status");
      ctx.ui.setWidget("above", ["above"], { placement: "aboveEditor" });
      ctx.ui.setWidget("below", ["below"], { placement: "belowEditor" });
      ctx.ui.setTitle("R14 Fixture");
      ctx.ui.setEditorText("fixture editor text");
      await ctx.ui.select("Select", ["A", "B"]);
      await ctx.ui.confirm("Confirm", "Continue?");
      await ctx.ui.input("Input", "value");
      await ctx.ui.editor("Editor", "prefill");
      const custom = await ctx.ui.custom(() => { throw new Error("must not run"); });
      if (custom !== undefined) throw new Error("custom unexpectedly returned a value");
      ctx.ui.notify("custom:UNSUPPORTED_BY_PINNED_RPC", "info");
    },
  });
}
"#,
    )
    .unwrap();
    let mut config = client_config(&temp);
    config.args.retain(|arg| arg != "--no-extensions");
    config.args.push("--extension".into());
    config.args.push(extension_path.into_os_string());
    let client = Client::spawn(config).unwrap();
    let events = client.subscribe();
    let prompt_client = client.clone();
    let prompt = std::thread::spawn(move || {
        prompt_client.request(
            Command::Prompt {
                message: "/r14-ui".into(),
                images: None,
                streaming_behavior: None,
            },
            TIMEOUT,
        )
    });

    let deadline = Instant::now() + TIMEOUT;
    let mut methods = Vec::new();
    let mut custom_marker = false;
    let mut unknown_events = Vec::new();
    let mut custom_wire_requests = Vec::new();
    while Instant::now() < deadline && (!custom_marker || methods.len() < 10) {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(ClientEvent::Rpc(event)) => {
                if let RpcEvent::ExtensionUiRequest { id, request } = *event {
                    let serialized = serde_json::to_value(&request).unwrap();
                    if serialized.get("method").and_then(Value::as_str) == Some("custom") {
                        custom_wire_requests.push(serialized);
                    }
                    match &request {
                        ExtensionUiRequest::Select { .. }
                        | ExtensionUiRequest::Input { .. }
                        | ExtensionUiRequest::Editor { .. } => client
                            .send_extension_ui_response(&ExtensionUiResponse::value(&id, "fixture"))
                            .unwrap(),
                        ExtensionUiRequest::Confirm { .. } => client
                            .send_extension_ui_response(&ExtensionUiResponse::confirmed(&id, true))
                            .unwrap(),
                        ExtensionUiRequest::Notify { message, .. } => {
                            custom_marker |= message.contains("UNSUPPORTED_BY_PINNED_RPC");
                        }
                        _ => {}
                    }
                    methods.push(request);
                }
            }
            Ok(ClientEvent::Unknown(value)) => unknown_events.push(value),
            Ok(ClientEvent::Lifecycle(_)) | Err(_) => {}
        }
    }
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::Select { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::Confirm { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::Input { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::Editor { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::Notify { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::SetStatus { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::SetWidget { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::SetTitle { .. }))
    );
    assert!(
        methods
            .iter()
            .any(|request| matches!(request, ExtensionUiRequest::SetEditorText { .. }))
    );
    assert!(
        custom_marker,
        "custom() did not prove UNSUPPORTED_BY_PINNED_RPC"
    );
    assert!(
        unknown_events.is_empty(),
        "fixture emitted unknown wire events: {unknown_events:?}"
    );
    assert!(
        custom_wire_requests.is_empty(),
        "custom() unexpectedly emitted extension_ui_request: {custom_wire_requests:?}"
    );
    let response = prompt.join().unwrap().unwrap();
    assert!(response.success, "{:?}", response.error);
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn r13_tree_fork_clone_switch_and_html_are_zero_token() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = env::current_dir().unwrap();
    let fixture_path = temp.path().join("r13-source.jsonl");
    fs::write(
        &fixture_path,
        format!(
            "{}\n{}\n{}\n",
            serde_json::json!({"type":"session","version":3,"id":"r13-source","timestamp":"2026-01-01T00:00:00.000Z","cwd":cwd}),
            serde_json::json!({"type":"message","id":"user-root","parentId":null,"timestamp":"2026-01-01T00:00:00.001Z","message":{"role":"user","content":"fork fixture"}}),
            serde_json::json!({"type":"message","id":"assistant-leaf","parentId":"user-root","timestamp":"2026-01-01T00:00:00.002Z","message":{"role":"assistant","content":[{"type":"text","text":"answer"}],"api":"fixture","provider":"fixture","model":"fixture","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0},"stopReason":"stop","timestamp":0}}})
        ),
    )
    .unwrap();
    let mut config = client_config(&temp);
    config.initial_session = Some(fixture_path.clone());
    let client = Client::spawn(config).unwrap();

    let tree: TreeData = client.request_data(Command::GetTree, TIMEOUT).unwrap();
    let root_id = tree.tree[0].entry.id.clone();
    let leaf_id = tree.leaf_id.clone().expect("fixture leaf");
    assert_ne!(root_id, leaf_id);
    assert!(!tree.tree.is_empty());

    let html = temp.path().join("r13-source.html");
    let exported: ExportPathData = client
        .request_data(
            Command::ExportHtml {
                output_path: Some(html.to_string_lossy().into_owned()),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(Path::new(&exported.path), html);
    assert!(
        fs::read_to_string(&html)
            .unwrap()
            .contains("<!DOCTYPE html>")
    );

    for command in [
        Command::SetAutoCompaction { enabled: false },
        Command::SetAutoCompaction { enabled: true },
        Command::SetAutoRetry { enabled: false },
        Command::SetAutoRetry { enabled: true },
        Command::AbortRetry,
    ] {
        assert!(client.request(command, TIMEOUT).unwrap().success);
    }

    let cloned: CloneData = client.request_data(Command::Clone, TIMEOUT).unwrap();
    assert!(!cloned.cancelled);
    let clone_state: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    assert_ne!(clone_state.session_id, "r13-source");
    assert_eq!(
        client.resume_session().as_deref(),
        clone_state.session_file.as_deref().map(Path::new)
    );

    let switched: serde_json::Value = client
        .request_data(
            Command::SwitchSession {
                session_path: fixture_path.to_string_lossy().into_owned(),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(switched["cancelled"], false);
    let restored: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    assert_eq!(restored.session_id, "r13-source");
    assert_eq!(client.resume_session(), Some(fixture_path.clone()));

    let restored_tree: TreeData = client.request_data(Command::GetTree, TIMEOUT).unwrap();
    let restored_root = restored_tree.tree[0].entry.id.clone();
    let forked: ForkData = client
        .request_data(
            Command::Fork {
                entry_id: restored_root,
            },
            TIMEOUT,
        )
        .unwrap();
    assert!(!forked.cancelled);
    assert_eq!(forked.text, "fork fixture");
    let fork_state: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    assert_ne!(fork_state.session_id, "r13-source");
    assert_eq!(
        client.resume_session().as_deref(),
        fork_state.session_file.as_deref().map(Path::new)
    );
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn first_spawn_resumes_fixture_directly() {
    let temp = tempfile::tempdir().unwrap();
    let fixture_path = temp.path().join("initial-resume.jsonl");
    let cwd = env::current_dir().unwrap();
    fs::write(
        &fixture_path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": "pi-rpc-initial-resume-fixture",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "cwd": cwd
            })
        ),
    )
    .unwrap();
    let mut config = client_config(&temp);
    config.initial_session = Some(fixture_path.clone());
    let client = Client::spawn(config).unwrap();
    let state: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    assert_eq!(state.session_id, "pi-rpc-initial-resume-fixture");
    assert_eq!(
        state.session_file.as_deref().map(Path::new),
        Some(fixture_path.as_path())
    );
    client.shutdown().unwrap();
}

#[test]
#[ignore = "requires PI_RPC_TEST_BINARY=official pi 0.84.2"]
fn kill_restart_and_resume_fixture() {
    let test = client();
    let client = &test.client;
    let events = client.subscribe();
    let fixture_path = test._temp.path().join("resume.jsonl");
    let cwd = env::current_dir().unwrap();
    let fixture = format!(
        "{}\n{}\n",
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "pi-rpc-resume-fixture",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd
        }),
        serde_json::json!({
            "type": "session_info",
            "id": "fixture-entry",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00.001Z",
            "name": "pi-rpc-resume-fixture"
        })
    );
    fs::write(&fixture_path, fixture).unwrap();
    let switched: Value = client
        .request_data(
            Command::SwitchSession {
                session_path: fixture_path.to_string_lossy().into_owned(),
            },
            TIMEOUT,
        )
        .unwrap();
    assert_eq!(switched["cancelled"], false);
    client.set_resume_session(Some(fixture_path.clone()));
    let before: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    let session_file = before
        .session_file
        .clone()
        .expect("persistent session file");
    assert_eq!(Path::new(&session_file), fixture_path);
    client.kill_process_tree().unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_exit = false;
    let mut saw_restart = false;
    while Instant::now() < deadline && !saw_restart {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(ClientEvent::Lifecycle(LifecycleEvent::Exited { .. })) => saw_exit = true,
            Ok(ClientEvent::Lifecycle(LifecycleEvent::Restarted {
                session_file: resumed,
                ..
            })) => {
                saw_restart = resumed.as_deref() == Some(Path::new(&session_file));
            }
            _ => {}
        }
    }
    assert!(saw_exit && saw_restart);
    let after: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.session_name, before.session_name);
    assert_eq!(after.message_count, before.message_count);
    client.shutdown().unwrap();
}

#[test]
#[ignore = "spends real model tokens; requires PI_RPC_R7_LIVE=1 and existing pi credentials"]
fn r7_live_stream_requires_explicit_opt_in() {
    if env::var("PI_RPC_R7_LIVE").as_deref() != Ok("1") {
        eprintln!("skip: set PI_RPC_R7_LIVE=1 explicitly before spending tokens");
        return;
    }
    let test = client();
    let client = &test.client;
    let events = client.subscribe();
    let prompt = env::var("PI_RPC_R7_PROMPT")
        .unwrap_or_else(|_| "Reply with exactly: r7-live-ok".to_owned());
    let response = client
        .request(
            Command::Prompt {
                message: prompt,
                images: None,
                streaming_behavior: None,
            },
            Duration::from_secs(120),
        )
        .unwrap();
    assert!(response.success, "{:?}", response.error);

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut deltas = 0;
    let mut message_end = None;
    let mut settled = false;
    while Instant::now() < deadline && !settled {
        if let Ok(ClientEvent::Rpc(event)) = events.recv_timeout(Duration::from_millis(250)) {
            match *event {
                pi_rpc::RpcEvent::MessageUpdate {
                    assistant_message_event: pi_rpc::AssistantMessageEvent::TextDelta { .. },
                    ..
                } => deltas += 1,
                pi_rpc::RpcEvent::MessageEnd { message } => message_end = Some(message),
                pi_rpc::RpcEvent::AgentSettled => settled = true,
                _ => {}
            }
        }
    }
    assert!(deltas > 0, "no text deltas observed");
    let authoritative = message_end.expect("no authoritative message_end observed");
    assert!(settled, "no agent_settled observed");

    let messages: MessagesData = client.request_data(Command::GetMessages, TIMEOUT).unwrap();
    assert!(
        messages
            .messages
            .iter()
            .any(|message| message == &authoritative),
        "get_messages did not contain authoritative assistant"
    );
    let state: RpcSessionState = client.request_data(Command::GetState, TIMEOUT).unwrap();
    let session_file = state.session_file.expect("persistent live session");
    let session_text = fs::read_to_string(session_file).unwrap();
    let assistant_text = authoritative
        .0
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|block| block.get("text").and_then(Value::as_str))
        })
        .expect("authoritative assistant text");
    assert!(session_text.contains(assistant_text));
    client.shutdown().unwrap();
}

#[test]
#[ignore = "spends real model tokens; requires PI_RPC_R7_QUEUE=1 and existing pi credentials"]
fn r7_live_queue_contract_requires_explicit_opt_in() {
    if env::var("PI_RPC_R7_QUEUE").as_deref() != Ok("1") {
        eprintln!("skip: set PI_RPC_R7_QUEUE=1 explicitly before spending tokens");
        return;
    }
    let test = client();
    let client = &test.client;
    let events = client.subscribe();
    let prompt = env::var("PI_RPC_R7_LONG_PROMPT").unwrap_or_else(|_| {
        "Write a numbered list from 1 to 80, with one short sentence per item.".to_owned()
    });
    assert!(
        client
            .request(
                Command::Prompt {
                    message: prompt,
                    images: None,
                    streaming_behavior: None
                },
                Duration::from_secs(120),
            )
            .unwrap()
            .success
    );
    assert!(
        client
            .request(
                Command::Prompt {
                    message: "Steer: keep every item under eight words.".into(),
                    images: None,
                    streaming_behavior: Some(StreamingBehavior::Steer),
                },
                TIMEOUT,
            )
            .unwrap()
            .success
    );
    assert!(
        client
            .request(
                Command::Prompt {
                    message: "Afterward reply with queue-contract-done.".into(),
                    images: None,
                    streaming_behavior: Some(StreamingBehavior::FollowUp),
                },
                TIMEOUT,
            )
            .unwrap()
            .success
    );
    assert!(client.request(Command::Abort, TIMEOUT).unwrap().success);

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut queue_updates = 0;
    let mut settled = false;
    while Instant::now() < deadline && !settled {
        if let Ok(ClientEvent::Rpc(event)) = events.recv_timeout(Duration::from_millis(250)) {
            match *event {
                pi_rpc::RpcEvent::QueueUpdate { .. } => queue_updates += 1,
                pi_rpc::RpcEvent::AgentSettled => settled = true,
                _ => {}
            }
        }
    }
    assert!(queue_updates > 0, "no queue_update observed");
    assert!(settled, "no agent_settled observed after abort");
    client.shutdown().unwrap();
}
