use std::{
    ffi::OsString,
    io::{Read, Write},
    net::{Shutdown, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use pi_data::{
    CliAuthStatus, ModelApi, ModelConfigError, SecretString, connectivity_request_body,
    parse_cli_auth_status, parse_discovered_models,
};
use std::fmt;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(12);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CLI_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub provider: String,
    pub id: String,
    pub context: String,
    pub max_output: String,
    pub reasoning: bool,
    pub images: bool,
}

pub type AuthCheckStatus = CliAuthStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectivityStatus {
    Reachable,
    AuthenticationRequired,
    RateLimited,
    ServerError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectivityResult {
    pub status: ConnectivityStatus,
    pub http_status: u16,
    pub model: String,
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub enum ModelServiceError {
    InvalidUrl,
    InvalidProviderId,
    InvalidArgument,
    UnsupportedScheme,
    InsecureRemoteHttp,
    Connect,
    Tls,
    Timeout,
    Cancelled,
    ResponseTooLarge,
    InvalidResponse,
    HttpStatus(u16),
    CliTimeout,
    CliOutputTooLarge,
    CliFailed { code: Option<i32> },
    CliSpawn(String),
    Config(ModelConfigError),
}

impl From<ModelConfigError> for ModelServiceError {
    fn from(error: ModelConfigError) -> Self {
        Self::Config(error)
    }
}

impl fmt::Display for ModelServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl => formatter.write_str("请求地址无效"),
            Self::InvalidProviderId => {
                formatter.write_str("provider id 只能包含字母、数字、点、下划线和连字符")
            }
            Self::InvalidArgument => formatter.write_str("请求参数无效"),
            Self::UnsupportedScheme => formatter.write_str("只允许访问 http(s) 模型端点"),
            Self::InsecureRemoteHttp => formatter.write_str("非 loopback 模型端点必须使用 HTTPS"),
            Self::Connect => formatter.write_str("连接模型服务失败"),
            Self::Tls => formatter.write_str("模型服务 TLS 连接或证书校验失败"),
            Self::Timeout => formatter.write_str("模型服务请求超时"),
            Self::Cancelled => formatter.write_str("操作已取消"),
            Self::ResponseTooLarge => {
                write!(formatter, "模型服务响应超过 {MAX_RESPONSE_BYTES} 字节")
            }
            Self::InvalidResponse => formatter.write_str("模型服务响应格式无效"),
            Self::HttpStatus(status) => write!(formatter, "模型服务返回 HTTP {status}"),
            Self::CliTimeout => formatter.write_str("官方 pi 命令超时"),
            Self::CliOutputTooLarge => formatter.write_str("官方 pi 命令输出过大"),
            Self::CliFailed { code } => write!(formatter, "官方 pi 命令执行失败（exit {code:?}）"),
            Self::CliSpawn(error) => write!(formatter, "无法启动官方 pi: {error}"),
            Self::Config(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ModelServiceError {}

#[derive(Debug, Clone)]
pub struct ModelService {
    pi_binary: PathBuf,
    agent_dir: PathBuf,
    timeout: Duration,
    response_limit: usize,
}

impl ModelService {
    pub fn new(pi_binary: PathBuf, agent_dir: PathBuf) -> Self {
        Self {
            pi_binary,
            agent_dir,
            timeout: DEFAULT_TIMEOUT,
            response_limit: MAX_RESPONSE_BYTES,
        }
    }

    #[cfg(test)]
    fn with_limits(mut self, timeout: Duration, response_limit: usize) -> Self {
        self.timeout = timeout;
        self.response_limit = response_limit;
        self
    }

    pub fn list_models(&self) -> Result<Vec<CatalogModel>, ModelServiceError> {
        let output = run_cli(
            &self.pi_binary,
            &[OsString::from("--list-models")],
            &self.agent_dir,
            self.timeout,
        )?;
        parse_model_table(&String::from_utf8_lossy(&output))
    }

    pub fn check_auth(&self, provider: &str) -> Result<AuthCheckStatus, ModelServiceError> {
        validate_provider_arg(provider)?;
        let (status, output) = run_cli_capture_status(
            &self.pi_binary,
            &[
                OsString::from("auth"),
                OsString::from("check"),
                OsString::from("--provider"),
                OsString::from(provider),
                OsString::from("--json"),
                OsString::from("--no-refresh"),
            ],
            &self.agent_dir,
            self.timeout,
        )?;
        if output.is_empty() && !status.success() {
            return Err(ModelServiceError::CliFailed {
                code: status.code(),
            });
        }
        parse_auth_check(&output)
    }

    pub fn discover_models(
        &self,
        base_url: &str,
        api: ModelApi,
        key: Option<&SecretString>,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, ModelServiceError> {
        let endpoint = discovery_endpoint(base_url)?;
        endpoint.validate_transport()?;
        let headers = auth_headers(api, key);
        let response = http_request(
            "GET",
            &endpoint,
            &headers,
            None,
            self.timeout,
            self.response_limit,
            cancel,
        )?;
        if !(200..300).contains(&response.status) {
            return Err(ModelServiceError::HttpStatus(response.status));
        }
        Ok(parse_discovered_models(api, &response.body)?)
    }

    pub fn test_connectivity(
        &self,
        base_url: &str,
        api: ModelApi,
        model: &str,
        key: Option<&SecretString>,
        cancel: &CancellationToken,
    ) -> Result<ConnectivityResult, ModelServiceError> {
        if model.trim().is_empty() {
            return Err(ModelServiceError::InvalidArgument);
        }
        let (endpoint, body) = connectivity_request(base_url, api, model)?;
        endpoint.validate_transport()?;
        let mut headers = auth_headers(api, key);
        headers.push(("Content-Type".into(), "application/json".into()));
        let response = http_request(
            "POST",
            &endpoint,
            &headers,
            Some(&body),
            self.timeout,
            self.response_limit,
            cancel,
        )?;
        let status = match response.status {
            200..=299 => ConnectivityStatus::Reachable,
            401 | 403 => ConnectivityStatus::AuthenticationRequired,
            429 => ConnectivityStatus::RateLimited,
            500..=599 => ConnectivityStatus::ServerError,
            code => return Err(ModelServiceError::HttpStatus(code)),
        };
        Ok(ConnectivityResult {
            status,
            http_status: response.status,
            model: model.to_owned(),
        })
    }

    /// 在新终端中运行官方交互式 TUI；用户需要在终端内手动输入 `/login <provider>`。
    pub fn run_login(
        &self,
        provider: &str,
        cancel: &CancellationToken,
    ) -> Result<ExitStatus, ModelServiceError> {
        validate_provider_arg(provider)?;
        let mut child = login_command(&self.pi_binary, &self.agent_dir, provider)?
            .spawn()
            .map_err(|error| ModelServiceError::CliSpawn(error.to_string()))?;
        let started = Instant::now();
        loop {
            if cancel.is_cancelled() {
                let _ = pi_rpc::kill_process_tree(child.id());
                return Err(ModelServiceError::Cancelled);
            }
            if started.elapsed() >= LOGIN_TIMEOUT {
                let _ = pi_rpc::kill_process_tree(child.id());
                return Err(ModelServiceError::CliTimeout);
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(error) => return Err(ModelServiceError::CliSpawn(error.to_string())),
            }
        }
    }
}

fn validate_provider_arg(provider: &str) -> Result<(), ModelServiceError> {
    if provider.is_empty()
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ModelServiceError::InvalidProviderId);
    }
    Ok(())
}

fn parse_model_table(output: &str) -> Result<Vec<CatalogModel>, ModelServiceError> {
    let output = output.strip_prefix('\u{feff}').unwrap_or(output);
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let Some(header) = lines.next() else {
        return Ok(Vec::new());
    };
    if header.starts_with("No models available.") {
        return Ok(Vec::new());
    }
    if !header.starts_with("provider") || !header.contains("model") {
        return Err(ModelServiceError::InvalidResponse);
    }
    let mut models = Vec::new();
    for line in lines {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        if columns.len() < 6 {
            continue;
        }
        models.push(CatalogModel {
            provider: columns[0].to_owned(),
            id: columns[1].to_owned(),
            context: columns[2].to_owned(),
            max_output: columns[3].to_owned(),
            reasoning: columns[4] == "yes",
            images: columns[5] == "yes",
        });
    }
    Ok(models)
}

fn parse_auth_check(output: &[u8]) -> Result<AuthCheckStatus, ModelServiceError> {
    Ok(parse_cli_auth_status(output)?)
}

fn discovery_endpoint(base_url: &str) -> Result<ParsedUrl, ModelServiceError> {
    ParsedUrl::parse(&format!("{}/models", base_url.trim_end_matches('/')))
}

fn connectivity_request(
    base_url: &str,
    api: ModelApi,
    model: &str,
) -> Result<(ParsedUrl, Vec<u8>), ModelServiceError> {
    let base = base_url.trim_end_matches('/');
    let url = match api {
        ModelApi::OpenAiCompletions => format!("{base}/chat/completions"),
        ModelApi::OpenAiResponses => format!("{base}/responses"),
        ModelApi::AnthropicMessages => format!("{base}/messages"),
        ModelApi::GoogleGenerativeAi => format!("{base}/models/{model}:generateContent"),
    };
    Ok((
        ParsedUrl::parse(&url)?,
        connectivity_request_body(api, model),
    ))
}

fn auth_headers(api: ModelApi, key: Option<&SecretString>) -> Vec<(String, String)> {
    let Some(key) = key else {
        return Vec::new();
    };
    match api {
        ModelApi::AnthropicMessages => vec![
            ("x-api-key".into(), key.expose().into()),
            ("anthropic-version".into(), "2023-06-01".into()),
        ],
        ModelApi::GoogleGenerativeAi => vec![("x-goog-api-key".into(), key.expose().into())],
        ModelApi::OpenAiCompletions | ModelApi::OpenAiResponses => {
            vec![("Authorization".into(), format!("Bearer {}", key.expose()))]
        }
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct ParsedUrl {
    host: String,
    port: u16,
    path: String,
    secure: bool,
}

impl ParsedUrl {
    fn parse(value: &str) -> Result<Self, ModelServiceError> {
        let (secure, rest, default_port) = if let Some(rest) = value.strip_prefix("http://") {
            (false, rest, 80)
        } else if value.starts_with("https://") {
            // 本项目不引入新的 HTTP/TLS 依赖；Windows HTTPS 走系统 curl，密钥仍仅在 header。
            let rest = value.strip_prefix("https://").unwrap();
            (true, rest, 443)
        } else {
            return Err(ModelServiceError::UnsupportedScheme);
        };
        if rest.contains('@') || rest.contains(['\r', '\n', '\0']) {
            return Err(ModelServiceError::InvalidUrl);
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = if authority.starts_with('[') {
            let end = authority.find(']').ok_or(ModelServiceError::InvalidUrl)?;
            let host = &authority[..=end];
            let port = match authority.get(end + 1..) {
                Some("") => default_port,
                Some(value) => value
                    .strip_prefix(':')
                    .ok_or(ModelServiceError::InvalidUrl)?
                    .parse()
                    .map_err(|_| ModelServiceError::InvalidUrl)?,
                None => return Err(ModelServiceError::InvalidUrl),
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) if !host.contains(':') => (
                    host,
                    port.parse().map_err(|_| ModelServiceError::InvalidUrl)?,
                ),
                _ => (authority, default_port),
            }
        };
        if host.is_empty() {
            return Err(ModelServiceError::InvalidUrl);
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            path: format!("/{path}"),
            secure,
        })
    }

    fn validate_transport(&self) -> Result<(), ModelServiceError> {
        if self.secure || is_loopback_host(&self.host) {
            Ok(())
        } else {
            Err(ModelServiceError::InsecureRemoteHttp)
        }
    }

    fn sanitized(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        let default = (self.secure && self.port == 443) || (!self.secure && self.port == 80);
        if default {
            format!("{scheme}://{}{}", self.host, self.path)
        } else {
            format!("{scheme}://{}:{}{}", self.host, self.port, self.path)
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("[::1]") {
        return true;
    }
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| address.octets()[0] == 127)
}

fn http_request(
    method: &str,
    url: &ParsedUrl,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
    response_limit: usize,
    cancel: &CancellationToken,
) -> Result<HttpResponse, ModelServiceError> {
    if cancel.is_cancelled() {
        return Err(ModelServiceError::Cancelled);
    }
    if url.secure {
        return curl_request(method, url, headers, body, timeout, response_limit, cancel);
    }
    tcp_request(method, url, headers, body, timeout, response_limit, cancel)
}

fn tcp_request(
    method: &str,
    url: &ParsedUrl,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
    response_limit: usize,
    cancel: &CancellationToken,
) -> Result<HttpResponse, ModelServiceError> {
    let started = Instant::now();
    let addresses = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|_| ModelServiceError::Connect)?
        .collect::<Vec<_>>();
    let mut stream = None;
    for address in addresses {
        if cancel.is_cancelled() {
            return Err(ModelServiceError::Cancelled);
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(ModelServiceError::Timeout);
        }
        if let Ok(connection) = TcpStream::connect_timeout(&address, remaining) {
            stream = Some(connection);
            break;
        }
    }
    let mut stream = stream.ok_or(ModelServiceError::Connect)?;
    let poll = Duration::from_millis(100).min(timeout);
    stream
        .set_read_timeout(Some(poll))
        .map_err(|_| ModelServiceError::Connect)?;
    stream
        .set_write_timeout(Some(poll))
        .map_err(|_| ModelServiceError::Connect)?;
    let body = body.unwrap_or_default();
    // 本地明文端点固定 HTTP/1.0，避免服务端用 chunked 编码而误把分块标记交给 JSON 解析。
    let mut request = format!(
        "{method} {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\nContent-Length: {}\r\n",
        url.path,
        tcp_host_header(url),
        body.len()
    )
    .into_bytes();
    for (name, value) in headers {
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(ModelServiceError::InvalidArgument);
        }
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(body);
    write_with_deadline(&mut stream, &request, started, timeout, cancel)?;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        if cancel.is_cancelled() {
            let _ = stream.shutdown(Shutdown::Both);
            return Err(ModelServiceError::Cancelled);
        }
        if started.elapsed() >= timeout {
            return Err(ModelServiceError::Timeout);
        }
        match stream.read(&mut buffer) {
            Ok(0) => {
                if response.is_empty() {
                    return Err(ModelServiceError::Connect);
                }
                break;
            }
            Ok(count) => {
                response.extend_from_slice(&buffer[..count]);
                if response.len() > response_limit + 16 * 1024 {
                    return Err(ModelServiceError::ResponseTooLarge);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(ModelServiceError::Connect),
        }
    }
    parse_http_response(&response, response_limit)
}

fn tcp_host_header(url: &ParsedUrl) -> String {
    if url.port == 80 {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    }
}

fn write_with_deadline(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    started: Instant,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(), ModelServiceError> {
    while !bytes.is_empty() {
        if cancel.is_cancelled() {
            return Err(ModelServiceError::Cancelled);
        }
        if started.elapsed() >= timeout {
            return Err(ModelServiceError::Timeout);
        }
        match stream.write(bytes) {
            Ok(0) => return Err(ModelServiceError::Connect),
            Ok(count) => bytes = &bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(ModelServiceError::Connect),
        }
    }
    Ok(())
}

fn parse_http_response(
    response: &[u8],
    response_limit: usize,
) -> Result<HttpResponse, ModelServiceError> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(ModelServiceError::InvalidResponse)?;
    let header = std::str::from_utf8(&response[..header_end])
        .map_err(|_| ModelServiceError::InvalidResponse)?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or(ModelServiceError::InvalidResponse)?;
    let raw_body = &response[header_end + 4..];
    let body = if header.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
        })
    }) {
        decode_chunked(raw_body, response_limit)?
    } else {
        if raw_body.len() > response_limit {
            return Err(ModelServiceError::ResponseTooLarge);
        }
        raw_body.to_vec()
    };
    Ok(HttpResponse { status, body })
}

fn decode_chunked(bytes: &[u8], response_limit: usize) -> Result<Vec<u8>, ModelServiceError> {
    let mut output = Vec::new();
    let mut remaining = bytes;
    loop {
        let line_end = remaining
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or(ModelServiceError::InvalidResponse)?;
        let size_text = std::str::from_utf8(&remaining[..line_end])
            .map_err(|_| ModelServiceError::InvalidResponse)?
            .split(';')
            .next()
            .ok_or(ModelServiceError::InvalidResponse)?;
        let size = usize::from_str_radix(size_text.trim(), 16)
            .map_err(|_| ModelServiceError::InvalidResponse)?;
        remaining = &remaining[line_end + 2..];
        if size == 0 {
            return Ok(output);
        }
        if size > remaining.len() || remaining.get(size..size + 2) != Some(b"\r\n") {
            return Err(ModelServiceError::InvalidResponse);
        }
        if output.len().saturating_add(size) > response_limit {
            return Err(ModelServiceError::ResponseTooLarge);
        }
        output.extend_from_slice(&remaining[..size]);
        remaining = &remaining[size + 2..];
    }
}

fn curl_request(
    method: &str,
    url: &ParsedUrl,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
    response_limit: usize,
    cancel: &CancellationToken,
) -> Result<HttpResponse, ModelServiceError> {
    let (mut command, config) = curl_command(method, url, headers, body, timeout)?;
    let mut child = command
        .spawn()
        .map_err(|error| ModelServiceError::CliSpawn(error.to_string()))?;
    let mut stdin = child.stdin.take().ok_or(ModelServiceError::Connect)?;
    stdin
        .write_all(config.as_bytes())
        .map_err(|_| ModelServiceError::Connect)?;
    drop(stdin);
    let output_limit = response_limit + 16 * 1024;
    let reader = spawn_bounded_stdout_reader(
        child
            .stdout
            .take()
            .ok_or(ModelServiceError::InvalidResponse)?,
        output_limit,
    );
    let (status, output) = wait_for_child_output(
        &mut child,
        reader,
        timeout + Duration::from_secs(1),
        Some(cancel),
        ModelServiceError::Timeout,
        ModelServiceError::ResponseTooLarge,
    )?;
    if !status.success() {
        return Err(curl_exit_error(status.code()));
    }
    parse_http_response(&output, response_limit)
}

fn curl_exit_error(code: Option<i32>) -> ModelServiceError {
    match code {
        Some(28) => ModelServiceError::Timeout,
        Some(35 | 58 | 59 | 60) => ModelServiceError::Tls,
        _ => ModelServiceError::Connect,
    }
}

fn hide_console_window(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(pi_rpc::platform::no_window_creation_flags());
    }
    command
}

fn curl_command(
    method: &str,
    url: &ParsedUrl,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<(Command, String), ModelServiceError> {
    let mut command = Command::new(system_curl_path()?);
    hide_console_window(&mut command);
    // 所有敏感 header 都通过 stdin config 传入；argv 仅包含固定开关、超时和无凭据 URL。
    command
        .args(["--disable", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut config = String::new();
    push_curl_config(&mut config, "silent", None)?;
    push_curl_config(&mut config, "show-error", None)?;
    push_curl_config(&mut config, "include", None)?;
    // --include 会保留 Transfer-Encoding；--raw 保证 body 也保持原始分块，由统一解析器限额解码。
    push_curl_config(&mut config, "raw", None)?;
    push_curl_config(&mut config, "request", Some(method))?;
    push_curl_config(
        &mut config,
        "max-time",
        Some(&timeout.as_secs_f64().to_string()),
    )?;
    push_curl_config(&mut config, "url", Some(&url.sanitized()))?;
    for (name, value) in headers {
        push_curl_config(&mut config, "header", Some(&format!("{name}: {value}")))?;
    }
    if let Some(body) = body {
        let body = std::str::from_utf8(body).map_err(|_| ModelServiceError::InvalidArgument)?;
        push_curl_config(&mut config, "data-raw", Some(body))?;
    }
    Ok((command, config))
}

#[cfg(windows)]
fn system_curl_path() -> Result<PathBuf, ModelServiceError> {
    let windows = trusted_windows_directory()?;
    system_file(&windows, "curl.exe")
        .ok_or_else(|| ModelServiceError::CliSpawn("Windows 系统 curl.exe 不存在".into()))
}

#[cfg(not(windows))]
fn system_curl_path() -> Result<PathBuf, ModelServiceError> {
    Err(ModelServiceError::CliSpawn(
        "HTTPS 模型端点仅支持 Windows 系统 curl".into(),
    ))
}

fn push_curl_config(
    config: &mut String,
    option: &str,
    value: Option<&str>,
) -> Result<(), ModelServiceError> {
    if !option
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(ModelServiceError::InvalidArgument);
    }
    config.push_str(option);
    if let Some(value) = value {
        if value.contains(['\r', '\n', '\0']) {
            return Err(ModelServiceError::InvalidArgument);
        }
        config.push_str(" = \"");
        for character in value.chars() {
            if matches!(character, '\\' | '"') {
                config.push('\\');
            }
            config.push(character);
        }
        config.push('"');
    }
    config.push('\n');
    Ok(())
}

fn run_cli(
    binary: &Path,
    args: &[OsString],
    agent_dir: &Path,
    timeout: Duration,
) -> Result<Vec<u8>, ModelServiceError> {
    let (status, output) = run_cli_capture_status(binary, args, agent_dir, timeout)?;
    if status.success() {
        Ok(output)
    } else {
        Err(ModelServiceError::CliFailed {
            code: status.code(),
        })
    }
}

fn run_cli_capture_status(
    binary: &Path,
    args: &[OsString],
    agent_dir: &Path,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>), ModelServiceError> {
    let mut command = Command::new(binary);
    hide_console_window(&mut command);
    let mut child = command
        .args(args)
        .env(pi_data::AGENT_DIR_ENV, agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ModelServiceError::CliSpawn(error.to_string()))?;
    let reader = spawn_bounded_stdout_reader(
        child
            .stdout
            .take()
            .ok_or(ModelServiceError::InvalidResponse)?,
        MAX_CLI_OUTPUT_BYTES,
    );
    wait_for_child_output(
        &mut child,
        reader,
        timeout,
        None,
        ModelServiceError::CliTimeout,
        ModelServiceError::CliOutputTooLarge,
    )
}

enum OutputRead {
    Complete(Vec<u8>),
    TooLarge,
    Failed,
}

fn spawn_bounded_stdout_reader(
    mut stdout: ChildStdout,
    limit: usize,
) -> mpsc::Receiver<OutputRead> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    let _ = sender.send(OutputRead::Complete(output));
                    return;
                }
                Ok(count) if output.len().saturating_add(count) <= limit => {
                    output.extend_from_slice(&buffer[..count]);
                }
                Ok(_) => {
                    let _ = sender.send(OutputRead::TooLarge);
                    return;
                }
                Err(_) => {
                    let _ = sender.send(OutputRead::Failed);
                    return;
                }
            }
        }
    });
    receiver
}

fn wait_for_child_output(
    child: &mut Child,
    receiver: mpsc::Receiver<OutputRead>,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
    timeout_error: ModelServiceError,
    too_large_error: ModelServiceError,
) -> Result<(ExitStatus, Vec<u8>), ModelServiceError> {
    let started = Instant::now();
    let mut exit_status = None;
    let mut completed_output = None;
    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            kill_and_reap(child);
            return Err(ModelServiceError::Cancelled);
        }
        if started.elapsed() >= timeout {
            kill_and_reap(child);
            return Err(timeout_error);
        }
        if completed_output.is_none() {
            match receiver.try_recv() {
                Ok(OutputRead::Complete(output)) => completed_output = Some(output),
                Ok(OutputRead::TooLarge) => {
                    kill_and_reap(child);
                    return Err(too_large_error);
                }
                Ok(OutputRead::Failed) => return Err(ModelServiceError::InvalidResponse),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(ModelServiceError::InvalidResponse);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exit_status = Some(status),
                Ok(None) => {}
                Err(error) => return Err(ModelServiceError::CliSpawn(error.to_string())),
            }
        }
        if let Some(status) = exit_status
            && let Some(output) = completed_output.take()
        {
            return Ok((status, output));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn kill_and_reap(child: &mut Child) {
    let _ = pi_rpc::kill_process_tree(child.id());
    let _ = child.wait();
}

#[cfg(windows)]
fn login_command(
    pi_binary: &Path,
    agent_dir: &Path,
    provider: &str,
) -> Result<Command, ModelServiceError> {
    validate_provider_arg(provider)?;
    let pi_binary = pi_binary
        .canonicalize()
        .map_err(|error| ModelServiceError::CliSpawn(error.to_string()))?;
    validate_cmd_path(&pi_binary)?;
    let cmd = system_executable("cmd.exe")?;
    let mut command = Command::new(cmd);
    // `start /wait` 创建拥有正常标准句柄的新控制台；参数逐项传入，provider 仍受白名单约束。
    // 这里故意不挂 CREATE_NO_WINDOW：包装进程若无窗口，`start` 拉起的官方 TUI 也可能被一起藏掉。
    command
        .args(["/d", "/s", "/c", "start", "", "/wait", "/d"])
        .arg(pi_binary.parent().unwrap_or_else(|| Path::new(".")))
        .arg(&pi_binary)
        .env(pi_data::AGENT_DIR_ENV, agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

#[cfg(windows)]
fn validate_cmd_path(path: &Path) -> Result<(), ModelServiceError> {
    let text = path.to_string_lossy();
    if text.contains(['&', '|', '<', '>', '^', '%', '!', '(', ')', '\r', '\n']) {
        Err(ModelServiceError::CliSpawn(
            "官方 pi 路径包含 cmd.exe 不安全字符".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn system_executable(name: &str) -> Result<PathBuf, ModelServiceError> {
    let windows = trusted_windows_directory()?;
    system_file(&windows, name)
        .ok_or_else(|| ModelServiceError::CliSpawn(format!("Windows 系统 {name} 不存在")))
}

#[cfg(windows)]
fn trusted_windows_directory() -> Result<PathBuf, ModelServiceError> {
    unsafe extern "system" {
        fn GetWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }

    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: 缓冲区可写且长度按 u32 传入；返回长度交给边界检查后才读取和构造 OsString。
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    windows_directory_from_buffer(&buffer, length)
}

#[cfg(windows)]
fn windows_directory_from_buffer(
    buffer: &[u16],
    length: usize,
) -> Result<PathBuf, ModelServiceError> {
    use std::os::windows::ffi::OsStringExt as _;

    if length == 0 || length >= buffer.len() {
        return Err(ModelServiceError::CliSpawn(
            "无法定位受信任的 Windows 系统目录".into(),
        ));
    }
    let path = PathBuf::from(OsString::from_wide(&buffer[..length]));
    path.is_absolute()
        .then_some(path)
        .ok_or_else(|| ModelServiceError::CliSpawn("Windows 系统目录不是绝对路径".into()))
}

#[cfg(windows)]
fn system_file(windows: &Path, name: &str) -> Option<PathBuf> {
    let sysnative = windows.join("Sysnative").join(name);
    if sysnative.is_file() {
        return Some(sysnative);
    }
    let system32 = windows.join("System32").join(name);
    system32.is_file().then_some(system32)
}

#[cfg(not(windows))]
fn login_command(
    pi_binary: &Path,
    agent_dir: &Path,
    provider: &str,
) -> Result<Command, ModelServiceError> {
    validate_provider_arg(provider)?;
    let mut command = Command::new(pi_binary);
    command.env(pi_data::AGENT_DIR_ENV, agent_dir);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::{fs, net::TcpListener};

    use tempfile::tempdir;

    use super::*;

    fn mock_server(response: Vec<u8>, delay: Duration) -> (String, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        request.extend_from_slice(&buffer[..count]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            let body_offset = request
                                .windows(4)
                                .position(|window| window == b"\r\n\r\n")
                                .unwrap()
                                + 4;
                            let content_length = String::from_utf8_lossy(&request[..body_offset])
                                .lines()
                                .find_map(|line| {
                                    line.strip_prefix("Content-Length: ")?.parse::<usize>().ok()
                                })
                                .unwrap_or(0);
                            if request.len() >= body_offset + content_length {
                                break;
                            }
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(_) => break,
                }
            }
            let _ = sender.send(request);
            thread::sleep(delay);
            let _ = stream.write_all(&response);
        });
        (format!("http://{address}/v1"), receiver)
    }

    fn response(status: u16, body: &[u8]) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect()
    }

    #[test]
    fn discovery_uses_authorization_header_and_never_url() {
        let body = br#"{"data":[{"id":"model-a"}]}"#;
        let (base, request) = mock_server(response(200, body), Duration::ZERO);
        let service = ModelService::new(PathBuf::new(), PathBuf::new());
        let secret = SecretString::new("not-in-url-secret".into()).unwrap();
        let models = service
            .discover_models(
                &base,
                ModelApi::OpenAiCompletions,
                Some(&secret),
                &CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(models, vec!["model-a"]);
        let request = String::from_utf8(request.recv().unwrap()).unwrap();
        assert!(request.starts_with("GET /v1/models HTTP/1.0"));
        assert!(request.contains("Authorization: Bearer not-in-url-secret"));
        assert!(
            !request
                .lines()
                .next()
                .unwrap()
                .contains("not-in-url-secret")
        );
    }

    #[test]
    fn provider_errors_map_without_exposing_response_body() {
        for status in [401, 403, 429, 500, 503] {
            let secret = format!("secret-{status}");
            let (base, _) = mock_server(
                response(status, format!("{{\"error\":\"{secret}\"}}").as_bytes()),
                Duration::ZERO,
            );
            let service = ModelService::new(PathBuf::new(), PathBuf::new());
            let error = service
                .discover_models(
                    &base,
                    ModelApi::AnthropicMessages,
                    None,
                    &CancellationToken::default(),
                )
                .unwrap_err();
            assert_eq!(error.to_string(), format!("模型服务返回 HTTP {status}"));
            assert!(!error.to_string().contains(&secret));
        }
    }

    #[test]
    fn timeout_oversize_and_malformed_json_are_bounded() {
        let (base, _) = mock_server(response(200, br#"{"data":[]}"#), Duration::from_millis(250));
        let service = ModelService::new(PathBuf::new(), PathBuf::new())
            .with_limits(Duration::from_millis(50), 1024);
        assert!(matches!(
            service.discover_models(
                &base,
                ModelApi::OpenAiCompletions,
                None,
                &CancellationToken::default()
            ),
            Err(ModelServiceError::Timeout)
        ));

        let (base, _) = mock_server(response(200, &[b'x'; 2048]), Duration::ZERO);
        assert!(matches!(
            service.discover_models(
                &base,
                ModelApi::OpenAiCompletions,
                None,
                &CancellationToken::default()
            ),
            Err(ModelServiceError::ResponseTooLarge)
        ));

        let (base, _) = mock_server(response(200, b"not-json"), Duration::ZERO);
        assert!(matches!(
            service.discover_models(
                &base,
                ModelApi::GoogleGenerativeAi,
                None,
                &CancellationToken::default()
            ),
            Err(ModelServiceError::Config(_))
        ));
    }

    #[test]
    fn plaintext_http_is_limited_to_loopback_hosts() {
        for url in [
            "http://localhost:8080/v1",
            "http://127.0.0.1/v1",
            "http://127.42.0.9/v1",
            "http://[::1]:8080/v1",
        ] {
            ParsedUrl::parse(url).unwrap().validate_transport().unwrap();
        }
        for url in ["http://192.168.1.2/v1", "http://example.com/v1"] {
            assert!(matches!(
                ParsedUrl::parse(url).unwrap().validate_transport(),
                Err(ModelServiceError::InsecureRemoteHttp)
            ));
        }
        ParsedUrl::parse("https://example.com/v1")
            .unwrap()
            .validate_transport()
            .unwrap();
    }

    #[test]
    fn connectivity_uses_minimal_body_and_status_model() {
        let (base, request) = mock_server(response(429, b"{}"), Duration::ZERO);
        let service = ModelService::new(PathBuf::new(), PathBuf::new());
        let result = service
            .test_connectivity(
                &base,
                ModelApi::OpenAiResponses,
                "model-a",
                None,
                &CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(result.status, ConnectivityStatus::RateLimited);
        assert_eq!(result.http_status, 429);
        let request = String::from_utf8(request.recv().unwrap()).unwrap();
        assert!(request.starts_with("POST /v1/responses HTTP/1.0"));
        assert!(request.contains("\"max_output_tokens\":1"));
    }

    #[test]
    fn cancelled_request_does_not_connect() {
        let token = CancellationToken::default();
        token.cancel();
        let service = ModelService::new(PathBuf::new(), PathBuf::new());
        assert!(matches!(
            service.discover_models(
                "http://127.0.0.1:9/v1",
                ModelApi::OpenAiCompletions,
                None,
                &token
            ),
            Err(ModelServiceError::Cancelled)
        ));
    }

    #[test]
    fn semantic_argument_errors_have_clear_messages() {
        assert_eq!(
            ModelServiceError::InvalidProviderId.to_string(),
            "provider id 只能包含字母、数字、点、下划线和连字符"
        );
        assert_eq!(
            ModelServiceError::InvalidArgument.to_string(),
            "请求参数无效"
        );
        assert!(validate_provider_arg("openai & whoami").is_err());
    }

    #[test]
    fn cli_parsers_cover_catalog_and_auth_json() {
        let table = "provider  model  context  max-out  thinking  images\nopenai  gpt-x  128K  16K  yes  no\n";
        assert_eq!(
            parse_model_table(table).unwrap(),
            vec![CatalogModel {
                provider: "openai".into(),
                id: "gpt-x".into(),
                context: "128K".into(),
                max_output: "16K".into(),
                reasoning: true,
                images: false,
            }]
        );
        assert_eq!(
            parse_auth_check(br#"{"status":"ready","provider":"openai","authType":"api_key"}"#)
                .unwrap(),
            AuthCheckStatus::Ready {
                auth_type: Some("api_key".into())
            }
        );
        assert!(
            parse_model_table(
                "No models available. Configure an API key or run /login <provider>.\n"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn official_pi_cli_uses_temporary_agent_dir_without_tokens() {
        let binary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("vendor")
            .join("pi")
            .join(pi_rpc::pi_binary_name());
        if !binary.is_file() {
            return;
        }
        let dir = tempdir().unwrap();
        let service = ModelService::new(binary, dir.path().to_path_buf());
        let models = service.list_models().unwrap_or_default();
        assert!(models.iter().all(|model| !model.provider.is_empty()));
        let status = service.check_auth("openai").unwrap();
        assert!(matches!(
            status,
            AuthCheckStatus::NotReady { .. } | AuthCheckStatus::Ready { .. }
        ));
        if let Ok(auth) = fs::read_to_string(dir.path().join("auth.json")) {
            assert!(!auth.contains("api_key"));
            assert!(!auth.contains("oauth"));
        }
    }

    #[test]
    fn official_pi_resolves_client_written_secret_as_original_literal() {
        let binary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("vendor")
            .join("pi")
            .join(pi_rpc::pi_binary_name());
        if !binary.is_file() {
            return;
        }
        let dir = tempdir().unwrap();
        let secret = "!literal-$R16_SHOULD_NOT_EXPAND-${R16_SHOULD_NOT_EXPAND}-$$-$!";
        pi_data::write_api_key(
            dir.path(),
            "openai",
            SecretString::new(secret.into()).unwrap(),
        )
        .unwrap();
        let output = run_cli(
            &binary,
            &[
                OsString::from("auth"),
                OsString::from("print-api-key"),
                OsString::from("--provider"),
                OsString::from("openai"),
            ],
            dir.path(),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(String::from_utf8(output).unwrap().trim(), secret);
    }

    #[test]
    fn curl_raw_mode_keeps_chunked_body_consistent_with_shared_parser() {
        let url = ParsedUrl::parse("https://api.example.test/v1/models").unwrap();
        let (_, config) = curl_command("GET", &url, &[], None, Duration::from_secs(3)).unwrap();
        assert!(config.lines().any(|line| line == "raw"));

        // curl 未使用 --raw 时会留下 chunked header，却交付已解码 JSON；共享解析器必须不接受该矛盾形态。
        let decoded_curl_shape =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{\"data\":[]}";
        assert!(matches!(
            parse_http_response(decoded_curl_shape, 1024),
            Err(ModelServiceError::InvalidResponse)
        ));

        let raw_body = b"B\r\n{\"data\":[]}\r\n0\r\n\r\n";
        let mut raw_response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        raw_response.extend_from_slice(raw_body);
        let parsed = parse_http_response(&raw_response, 1024).unwrap();
        assert_eq!(parsed.body, br#"{"data":[]}"#);
    }

    #[test]
    fn curl_sensitive_headers_are_only_in_stdin_config() {
        let url = ParsedUrl::parse("https://api.example.test/v1/models").unwrap();
        let secret = "secret\\\"value";
        let (command, config) = curl_command(
            "GET",
            &url,
            &[("Authorization".into(), format!("Bearer {secret}"))],
            None,
            Duration::from_secs(3),
        )
        .unwrap();
        let argv = format!("{command:?}");
        assert!(!argv.contains(secret));
        assert!(!argv.contains("Authorization"));
        assert!(argv.contains("--config"));
        assert!(config.contains("header = \"Authorization: Bearer secret\\\\\\\"value\""));
        assert!(!config.contains('\r'));
    }

    #[test]
    fn tcp_host_header_includes_only_non_default_port_and_preserves_ipv6_brackets() {
        assert_eq!(
            tcp_host_header(&ParsedUrl::parse("http://localhost:11434/v1").unwrap()),
            "localhost:11434"
        );
        assert_eq!(
            tcp_host_header(&ParsedUrl::parse("http://127.0.0.1/v1").unwrap()),
            "127.0.0.1"
        );
        assert_eq!(
            tcp_host_header(&ParsedUrl::parse("http://[::1]:8080/v1").unwrap()),
            "[::1]:8080"
        );
    }

    #[test]
    fn curl_exit_codes_map_without_response_or_secret_details() {
        assert!(matches!(
            curl_exit_error(Some(28)),
            ModelServiceError::Timeout
        ));
        for code in [35, 58, 59, 60] {
            let error = curl_exit_error(Some(code));
            assert!(matches!(error, ModelServiceError::Tls));
            assert_eq!(error.to_string(), "模型服务 TLS 连接或证书校验失败");
        }
        for code in [1, 6, 7, 22] {
            assert!(matches!(
                curl_exit_error(Some(code)),
                ModelServiceError::Connect
            ));
        }
    }

    #[test]
    fn curl_config_rejects_newlines_and_nul() {
        let mut config = String::new();
        assert!(push_curl_config(&mut config, "header", Some("x: a\nb")).is_err());
        assert!(push_curl_config(&mut config, "header", Some("x: a\0b")).is_err());
    }

    #[test]
    fn chunked_http_response_is_decoded_with_limit() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"da\r\n7\r\nta\":[]}\r\n0\r\n\r\n";
        let parsed = parse_http_response(response, 1024).unwrap();
        assert_eq!(parsed.body, br#"{"data":[]}"#);
        assert!(matches!(
            parse_http_response(response, 4),
            Err(ModelServiceError::ResponseTooLarge)
        ));
    }

    #[test]
    fn stdout_eof_does_not_disable_child_timeout_or_cancellation() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("close-stdout.ps1");
        fs::write(
            &script,
            r#"$signature = @'
using System;
using System.Runtime.InteropServices;
public static class StdoutCloser {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr GetStdHandle(int nStdHandle);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool CloseHandle(IntPtr hObject);
}
'@
Add-Type -TypeDefinition $signature
[Console]::Out.Flush()
[StdoutCloser]::CloseHandle([StdoutCloser]::GetStdHandle(-11)) | Out-Null
Start-Sleep -Seconds 10
"#,
        )
        .unwrap();

        let spawn = || {
            let mut child = hide_console_window(&mut Command::new("powershell.exe"))
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    script.to_str().unwrap(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let reader = spawn_bounded_stdout_reader(child.stdout.take().unwrap(), 1024);
            (child, reader)
        };

        let (mut timed_child, timed_reader) = spawn();
        let started = Instant::now();
        assert!(matches!(
            wait_for_child_output(
                &mut timed_child,
                timed_reader,
                Duration::from_millis(500),
                None,
                ModelServiceError::CliTimeout,
                ModelServiceError::CliOutputTooLarge,
            ),
            Err(ModelServiceError::CliTimeout)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));

        let (mut cancelled_child, cancelled_reader) = spawn();
        let cancel = CancellationToken::default();
        let cancel_later = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            cancel_later.cancel();
        });
        assert!(matches!(
            wait_for_child_output(
                &mut cancelled_child,
                cancelled_reader,
                Duration::from_secs(5),
                Some(&cancel),
                ModelServiceError::CliTimeout,
                ModelServiceError::CliOutputTooLarge,
            ),
            Err(ModelServiceError::Cancelled)
        ));
    }

    #[test]
    fn cli_stdout_is_drained_while_child_runs_and_is_bounded() {
        let dir = tempdir().unwrap();
        let fixture_dir = dir.path().join("output fixtures with spaces");
        fs::create_dir(&fixture_dir).unwrap();

        // 使用一次 type 批量输出固定字节，避免共享 CI runner 上逐字符 set /p 先撞到 timeout。
        // `%~dp0` 由 cmd 从脚本自身路径解析，不拼接外部输入，且覆盖含空格路径。
        let bounded_payload = fixture_dir.join("bounded payload.txt");
        let bounded_script = fixture_dir.join("bounded output.cmd");
        fs::write(&bounded_payload, vec![b'x'; MAX_CLI_OUTPUT_BYTES]).unwrap();
        fs::write(
            &bounded_script,
            "@echo off\r\ntype \"%~dp0bounded payload.txt\"\r\n",
        )
        .unwrap();
        let bounded =
            run_cli_capture_status(&bounded_script, &[], &fixture_dir, Duration::from_secs(5));
        let (status, bytes) = bounded
            .unwrap_or_else(|error| panic!("恰好达到 CLI 输出上限应成功，实际错误：{error:?}"));
        assert!(status.success(), "fixture 退出状态异常：{status:?}");
        assert_eq!(
            bytes.len(),
            MAX_CLI_OUTPUT_BYTES,
            "恰好达到上限的输出必须完整保留"
        );
        assert!(
            bytes.iter().all(|byte| *byte == b'x'),
            "fixture 输出内容不得被 cmd 文本处理改变"
        );

        let oversized_payload = fixture_dir.join("oversized payload.txt");
        let oversized_script = fixture_dir.join("oversized output.cmd");
        fs::write(&oversized_payload, vec![b'y'; MAX_CLI_OUTPUT_BYTES + 1]).unwrap();
        fs::write(
            &oversized_script,
            "@echo off\r\ntype \"%~dp0oversized payload.txt\"\r\n",
        )
        .unwrap();
        match run_cli_capture_status(&oversized_script, &[], &fixture_dir, Duration::from_secs(5)) {
            Err(ModelServiceError::CliOutputTooLarge) => {}
            Err(error) => {
                panic!("超过 CLI 输出上限 1 字节应返回 CliOutputTooLarge，实际错误：{error:?}")
            }
            Ok((status, bytes)) => panic!(
                "超过 CLI 输出上限 1 字节不应成功：status={status:?}, bytes={}",
                bytes.len()
            ),
        }
    }

    #[test]
    fn system_curl_path_is_absolute_and_ignores_current_directory() {
        let path = system_curl_path().unwrap();
        assert!(path.is_absolute());
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("curl.exe")
        );
        let normalized = path.to_string_lossy().to_ascii_lowercase();
        assert!(normalized.contains("system32") || normalized.contains("sysnative"));
    }

    #[test]
    fn login_provider_is_validated_and_never_shell_concatenated() {
        let dir = tempdir().unwrap();
        let binary = dir.path().join("pi.exe");
        fs::write(&binary, b"").unwrap();
        assert!(validate_provider_arg("openai-codex").is_ok());
        let command = login_command(&binary, dir.path(), "openai-codex").unwrap();
        let debug = format!("{command:?}");
        assert!(!debug.contains("/login"));
        assert!(!debug.contains("openai-codex"));
        assert!(debug.contains("cmd.exe"));
        assert!(debug.contains("start"));
        assert!(debug.contains("/wait"));
        assert!(!debug.contains("CREATE_NEW_CONSOLE"));
        assert!(validate_provider_arg("openai & whoami").is_err());
    }
}
