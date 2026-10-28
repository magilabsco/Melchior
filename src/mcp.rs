//! MCP client over streamable HTTP or stdio (plugin `mcp-client`, also loaded
//! by its DeepSeek Harness name `@deepseek-ai/dsh-mcp-client`).
//!
//! Each configured server's tools are exposed to the model as
//! `mcp__<serverName>__<tool>`, with characters outside `[A-Za-z0-9_-]`
//! replaced by `_` (so a gateway's `files.list` becomes `mcp__tools__files_list`).
//! A short hash is appended only when two raw names would collide or the
//! name would exceed 64 characters.
//!
//! A stdio server is a child process speaking newline-delimited JSON-RPC.
//! It is started by `connect`, restarted by the next `connect` if it exits,
//! and killed when its plugin scope is disposed.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http::{self, Request};
use crate::json::{self, Json};
use crate::runtime::Runtime;
use crate::tools::{Tool, ToolCx, ToolOutput, ToolSpec};
use crate::{jobj, log_debug, util};

pub const PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Clone)]
pub struct RawTool {
    pub name: String,
    pub public_name: String,
    pub description: String,
    pub input_schema: Json,
}

struct HttpTransport {
    url: String,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
}

/// How to start a stdio server.
#[derive(Clone)]
pub struct StdioCommand {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

struct StdioProcess {
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl StdioProcess {
    fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct StdioTransport {
    command: StdioCommand,
    process: Mutex<Option<StdioProcess>>,
}

enum Transport {
    Http(HttpTransport),
    Stdio(StdioTransport),
}

pub struct McpServer {
    pub name: String,
    transport: Transport,
    call_timeout: Duration,
    next_id: AtomicU64,
    pub connected: AtomicBool,
    pub tools: Mutex<Vec<RawTool>>,
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn string_pairs(value: &Json) -> Vec<(String, String)> {
    value.entries().iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()
}

impl McpServer {
    fn with_transport(name: &str, transport: Transport, call_timeout: Duration) -> Result<McpServer, String> {
        if !valid_server_name(name) {
            return Err(format!("mcp-client: invalid serverName {name:?}"));
        }
        Ok(McpServer {
            name: name.to_string(),
            transport,
            call_timeout,
            next_id: AtomicU64::new(1),
            connected: AtomicBool::new(false),
            tools: Mutex::new(Vec::new()),
        })
    }

    /// A streamable-HTTP server.
    pub fn http(name: &str, url: &str, headers: Vec<(String, String)>, call_timeout: Duration) -> Result<McpServer, String> {
        let transport = HttpTransport {
            url: url.to_string(),
            headers,
            session_id: Mutex::new(None),
            protocol: Mutex::new(None),
        };
        McpServer::with_transport(name, Transport::Http(transport), call_timeout)
    }

    /// A stdio server, started by the first `connect`.
    pub fn stdio(name: &str, command: StdioCommand, call_timeout: Duration) -> Result<McpServer, String> {
        McpServer::with_transport(name, Transport::Stdio(StdioTransport { command, process: Mutex::new(None) }), call_timeout)
    }

    /// From an `mcp-client` entry's config. A stdio server's environment is
    /// the harness environment without its credentials, plus `env`.
    pub fn from_config(config: &Json, runtime: &Runtime) -> Result<McpServer, String> {
        let name = config.get("serverName").as_str().ok_or("mcp-client: serverName is required")?;
        let timeout_ms = config.get("toolCallTimeoutMs").as_i64().unwrap_or(180_000).clamp(1_000, 900_000);
        let call_timeout = Duration::from_millis(timeout_ms as u64);
        let default_transport = if config.get("command").is_null() { "streamable-http" } else { "stdio" };
        match config.get("transport").str_or(default_transport) {
            "streamable-http" | "http" => {
                let url = config.get("url").as_str().ok_or(format!("mcp-client({name}): url is required"))?;
                McpServer::http(name, url, string_pairs(config.get("headers")), call_timeout)
            }
            "stdio" => {
                let command = config.get("command").as_str().ok_or(format!("mcp-client({name}): command is required for stdio"))?;
                let mut env = runtime.child_env();
                env.extend(string_pairs(config.get("env")));
                let command = StdioCommand {
                    command: command.to_string(),
                    args: config.get("args").items().iter().filter_map(|a| a.as_str().map(str::to_string)).collect(),
                    env,
                    cwd: Some(config.get("cwd").as_str().map(|c| runtime.resolve_path(c)).unwrap_or_else(|| runtime.cwd.clone())),
                };
                McpServer::stdio(name, command, call_timeout)
            }
            other => Err(format!("mcp-client({name}): transport {other:?} is not supported (use streamable-http or stdio)")),
        }
    }

    pub fn is_stdio(&self) -> bool {
        matches!(self.transport, Transport::Stdio(_))
    }

    // ---- streamable HTTP ---------------------------------------------------

    fn post(&self, http: &HttpTransport, body: &Json, timeout: Duration) -> Result<Option<Json>, String> {
        let mut request = Request::new("POST", &http.url)
            .json(body)
            .header("Accept", "application/json, text/event-stream")
            .timeouts(timeout, timeout + Duration::from_secs(5));
        if let Some(id) = http.session_id.lock().unwrap().clone() {
            request = request.header("Mcp-Session-Id", &id);
        }
        if let Some(version) = http.protocol.lock().unwrap().clone() {
            request = request.header("MCP-Protocol-Version", &version);
        }
        for (name, value) in &http.headers {
            request = request.header(name, value);
        }
        let response = http::send(&request).map_err(|e| format!("{}: {e}", self.name))?;
        if let Some(id) = response.header("mcp-session-id") {
            *http.session_id.lock().unwrap() = Some(id.to_string());
        }
        let status = response.status;
        if status == 202 || status == 204 {
            return Ok(None);
        }
        let event_stream = response.header("content-type").map(|v| v.contains("event-stream")).unwrap_or(false);
        if status >= 400 {
            let text = response.text(16 * 1024).unwrap_or_default();
            return Err(format!("{}: MCP server returned HTTP {status}: {}", self.name, util::truncate(text.trim(), 400, "…")));
        }
        let wanted = body.get("id").clone();
        if !event_stream {
            let text = response.text(32 * 1024 * 1024).map_err(|e| e.message)?;
            if text.trim().is_empty() {
                return Ok(None);
            }
            return json::parse(&text).map(Some).map_err(|e| format!("{}: invalid MCP response: {e}", self.name));
        }
        let mut reader = response.body;
        let mut data = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).map_err(|e| format!("{}: MCP stream failed: {e}", self.name))? == 0 {
                return Err(format!("{}: MCP stream ended without a response", self.name));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if let Some(payload) = trimmed.strip_prefix("data:") {
                data.push_str(payload.trim_start());
                continue;
            }
            if trimmed.is_empty() && !data.is_empty() {
                let message = json::parse(&data).map_err(|e| format!("{}: invalid MCP event: {e}", self.name))?;
                data.clear();
                if message.get("id") == &wanted && (!message.get("result").is_null() || !message.get("error").is_null()) {
                    return Ok(Some(message));
                }
            }
        }
    }

    // ---- stdio ---------------------------------------------------------------

    fn spawn(&self, stdio: &StdioTransport) -> Result<StdioProcess, String> {
        let spec = &stdio.command;
        let mut command = Command::new(&spec.command);
        command.args(&spec.args).env_clear().envs(spec.env.iter().cloned()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|e| format!("{}: cannot start {}: {e}", self.name, spec.command))?;
        let stdin = child.stdin.take().ok_or(format!("{}: no stdin", self.name))?;
        let stdout = child.stdout.take().ok_or(format!("{}: no stdout", self.name))?;
        let stderr = child.stderr.take();
        let (sender, lines) = mpsc::channel();
        let reader_name = format!("mcp-{}", self.name);
        std::thread::Builder::new()
            .name(reader_name.clone())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| format!("{}: {e}", self.name))?;
        if let Some(stderr) = stderr {
            let server = self.name.clone();
            let _ = std::thread::Builder::new().name(format!("{reader_name}-stderr")).spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    log_debug!("mcp", "{server}: {line}");
                }
            });
        }
        Ok(StdioProcess { child: crate::proc::track(child), stdin, lines })
    }

    fn stdio_exchange(&self, stdio: &StdioTransport, body: &Json, timeout: Duration) -> Result<Option<Json>, String> {
        let mut slot = stdio.process.lock().unwrap();
        let Some(process) = slot.as_mut() else {
            self.connected.store(false, Ordering::SeqCst);
            return Err(format!("{}: the server is not running", self.name));
        };
        let line = format!("{body}\n");
        if let Err(e) = process.stdin.write_all(line.as_bytes()).and_then(|_| process.stdin.flush()) {
            process.kill();
            *slot = None;
            self.connected.store(false, Ordering::SeqCst);
            return Err(format!("{}: the server exited ({e})", self.name));
        }
        let wanted = body.get("id").clone();
        if wanted.is_null() {
            return Ok(None);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match process.lines.recv_timeout(left) {
                Ok(line) => {
                    let Ok(message) = json::parse(&line) else {
                        log_debug!("mcp", "{}: ignoring non-JSON output: {}", self.name, util::truncate(&line, 200, "…"));
                        continue;
                    };
                    if message.get("id") == &wanted && message.get("method").is_null() {
                        return Ok(Some(message));
                    }
                    // A request from the server: answer pings, refuse the rest.
                    if let Some(method) = message.get("method").as_str()
                        && !message.get("id").is_null()
                    {
                        let reply = if method == "ping" {
                            jobj! { "jsonrpc" => "2.0", "id" => message.get("id").clone(), "result" => jobj! {} }
                        } else {
                            jobj! { "jsonrpc" => "2.0", "id" => message.get("id").clone(),
                                "error" => jobj! { "code" => -32601, "message" => format!("{method} is not supported") } }
                        };
                        let _ = process.stdin.write_all(format!("{reply}\n").as_bytes());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    process.kill();
                    *slot = None;
                    self.connected.store(false, Ordering::SeqCst);
                    return Err(format!("{}: no response within {}s; the server was stopped", self.name, timeout.as_secs()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    process.kill();
                    *slot = None;
                    self.connected.store(false, Ordering::SeqCst);
                    return Err(format!("{}: the server exited", self.name));
                }
            }
        }
    }

    // ---- JSON-RPC ------------------------------------------------------------

    fn send(&self, body: &Json, timeout: Duration) -> Result<Option<Json>, String> {
        match &self.transport {
            Transport::Http(http) => self.post(http, body, timeout),
            Transport::Stdio(stdio) => self.stdio_exchange(stdio, body, timeout),
        }
    }

    fn request(&self, method: &str, params: Json, timeout: Duration) -> Result<Json, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) as i64;
        let body = jobj! { "jsonrpc" => "2.0", "id" => id, "method" => method, "params" => params };
        let response = self.send(&body, timeout)?.ok_or(format!("{}: empty response to {method}", self.name))?;
        if !response.get("error").is_null() {
            return Err(format!("{}: {method} failed: {}", self.name, response.get("error").get("message").str_or("unknown error")));
        }
        Ok(response.get("result").clone())
    }

    /// Initialize the MCP session (starting a stdio server first) and fetch
    /// the full tool list.
    pub fn connect(&self) -> Result<(), String> {
        let timeout = Duration::from_secs(20);
        self.connected.store(false, Ordering::SeqCst);
        match &self.transport {
            Transport::Http(http) => {
                *http.session_id.lock().unwrap() = None;
                *http.protocol.lock().unwrap() = None;
            }
            Transport::Stdio(stdio) => {
                let mut slot = stdio.process.lock().unwrap();
                if let Some(old) = slot.take() {
                    old.kill();
                }
                *slot = Some(self.spawn(stdio)?);
            }
        }
        let result = self.request(
            "initialize",
            jobj! {
                "protocolVersion" => PROTOCOL_VERSION,
                "capabilities" => jobj! {},
                "clientInfo" => jobj! { "name" => "melchior", "version" => env!("CARGO_PKG_VERSION") },
            },
            timeout,
        )?;
        if let Transport::Http(http) = &self.transport {
            *http.protocol.lock().unwrap() = Some(result.get("protocolVersion").str_or(PROTOCOL_VERSION).to_string());
        }
        let _ = self.send(&jobj! { "jsonrpc" => "2.0", "method" => "notifications/initialized" }, timeout);
        let mut raw = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor {
                Some(c) => jobj! { "cursor" => c.as_str() },
                None => jobj! {},
            };
            let page = self.request("tools/list", params, timeout)?;
            raw.extend(page.get("tools").items().iter().cloned());
            match page.get("nextCursor").as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        let mut tools: Vec<RawTool> = Vec::new();
        for tool in &raw {
            let Some(name) = tool.get("name").as_str() else { continue };
            if tools.iter().any(|t| t.name == name) {
                return Err(format!("{}: server listed tool {name:?} more than once", self.name));
            }
            let schema = match tool.get("inputSchema") {
                s @ Json::Obj(_) => s.clone(),
                _ => jobj! { "type" => "object", "additionalProperties" => true },
            };
            tools.push(RawTool {
                name: name.to_string(),
                public_name: String::new(),
                description: tool.get("description").str_or("").to_string(),
                input_schema: schema,
            });
        }
        assign_public_names(&self.name, &mut tools);
        *self.tools.lock().unwrap() = tools;
        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stop a stdio server. Harmless for HTTP servers.
    pub fn close(&self) {
        self.connected.store(false, Ordering::SeqCst);
        if let Transport::Stdio(stdio) = &self.transport
            && let Some(process) = stdio.process.lock().unwrap().take()
        {
            process.kill();
        }
    }

    pub fn call(&self, raw_name: &str, arguments: &Json) -> ToolOutput {
        let params = jobj! { "name" => raw_name, "arguments" => arguments.clone() };
        let result = match self.request("tools/call", params.clone(), self.call_timeout) {
            Ok(result) => result,
            Err(error) => {
                // A gateway restart invalidates the HTTP session, and a stdio
                // server may have exited; reconnect once.
                let retry = error.contains("HTTP 404") || (self.is_stdio() && !self.connected.load(Ordering::SeqCst));
                if retry && self.connect().is_ok() {
                    match self.request("tools/call", params, self.call_timeout) {
                        Ok(result) => result,
                        Err(error) => return ToolOutput::err(error),
                    }
                } else {
                    return ToolOutput::err(error);
                }
            }
        };
        let mut parts = Vec::new();
        for block in result.get("content").items() {
            match block.get("type").as_str() {
                Some("text") => parts.push(block.get("text").str_or("").to_string()),
                Some("image") => parts.push("[image returned by the tool; not shown to the model]".into()),
                Some("resource") => parts.push(block.get("resource").get("text").str_or("[resource]").to_string()),
                _ => {}
            }
        }
        if parts.is_empty() && !result.get("structuredContent").is_null() {
            parts.push(result.get("structuredContent").to_string());
        }
        let text = if parts.is_empty() { format!("({raw_name} returned no model-visible content)") } else { parts.join("\n") };
        if result.get("isError").as_bool() == Some(true) { ToolOutput::err(text) } else { ToolOutput::ok(text) }
    }

    pub fn find(&self, raw_name: &str) -> Option<RawTool> {
        self.tools.lock().unwrap().iter().find(|t| t.name == raw_name).cloned()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.close();
    }
}

fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

pub fn assign_public_names(server: &str, tools: &mut [RawTool]) {
    let base: Vec<String> = tools.iter().map(|t| sanitize(&format!("mcp__{server}__{}", t.name))).collect();
    for (index, tool) in tools.iter_mut().enumerate() {
        let candidate = &base[index];
        let collides = base.iter().enumerate().any(|(j, other)| j != index && other == candidate);
        tool.public_name = if candidate.len() <= 64 && !collides {
            candidate.clone()
        } else {
            let hash = format!("{:012x}", util::fnv1a(format!("{server}\0{}", tool.name).as_bytes()) & 0xffff_ffff_ffff);
            let keep = candidate.len().min(64 - 13);
            format!("{}_{hash}", &candidate[..keep])
        };
    }
}

pub struct McpTool {
    pub server: Arc<McpServer>,
    pub raw: RawTool,
}

impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        let description = if self.raw.description.is_empty() {
            format!("[{}] tool from the {} server.", self.raw.name, self.server.name)
        } else {
            format!("[{}] {}", self.raw.name, self.raw.description)
        };
        ToolSpec { name: self.raw.public_name.clone(), description, parameters: self.raw.input_schema.clone() }
    }

    fn call(&self, _cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        self.server.call(&self.raw.name, args)
    }
}

/// All MCP servers known to this harness.
#[derive(Default)]
pub struct McpHub {
    servers: Mutex<Vec<Arc<McpServer>>>,
}

impl McpHub {
    pub fn add(&self, server: Arc<McpServer>) -> Result<(), String> {
        let mut servers = self.servers.lock().unwrap();
        if servers.iter().any(|s| s.name == server.name) {
            return Err(format!("mcp-client: serverName {:?} is already in use", server.name));
        }
        servers.push(server);
        Ok(())
    }

    pub fn remove(&self, name: &str) {
        self.servers.lock().unwrap().retain(|s| s.name != name);
    }

    pub fn servers(&self) -> Vec<Arc<McpServer>> {
        self.servers.lock().unwrap().clone()
    }

    /// Find a tool by its raw MCP name, optionally on one server.
    pub fn find(&self, server: Option<&str>, raw_name: &str) -> Option<(Arc<McpServer>, RawTool)> {
        self.servers()
            .into_iter()
            .filter(|s| server.is_none_or(|n| s.name == n))
            .find_map(|s| s.find(raw_name).map(|t| (s.clone(), t)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str) -> RawTool {
        RawTool { name: name.into(), public_name: String::new(), description: String::new(), input_schema: Json::Null }
    }

    #[test]
    fn public_names_are_clean_unless_they_collide() {
        let mut tools = vec![raw("files.list"), raw("leviathan.search"), raw("a.b"), raw("a_b")];
        assign_public_names("tools", &mut tools);
        assert_eq!(tools[0].public_name, "mcp__tools__files_list");
        assert_eq!(tools[1].public_name, "mcp__tools__leviathan_search");
        assert_ne!(tools[2].public_name, tools[3].public_name);
        assert!(tools[2].public_name.starts_with("mcp__tools__a_b_"));
        let mut long = vec![raw(&"x".repeat(80))];
        assign_public_names("tools", &mut long);
        assert_eq!(long[0].public_name.len(), 64);
    }

    /// A stdio server written in sh: answers initialize, tools/list and
    /// tools/call by echoing the request id.
    fn sh_server() -> StdioCommand {
        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26"}}\n' "$id" ;;
    *'"tools/list"'*) printf 'not json\n{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"die"'*) exit 0 ;;
    *) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
  esac
done"#;
        StdioCommand {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            cwd: None,
        }
    }

    #[test]
    fn stdio_servers_connect_call_and_restart() {
        let server = McpServer::stdio("local", sh_server(), Duration::from_secs(10)).unwrap();
        server.connect().unwrap();
        assert_eq!(server.find("echo").unwrap().public_name, "mcp__local__echo");
        let out = server.call("echo", &jobj! {});
        assert!(!out.is_error && out.content == "pong", "{out:?}");
        // The server exits mid-call; the next call restarts it.
        let died = server.call("die", &jobj! {});
        assert!(died.is_error, "{died:?}");
        let again = server.call("echo", &jobj! {});
        assert!(!again.is_error && again.content == "pong", "{again:?}");
        server.close();
        assert!(!server.connected.load(Ordering::SeqCst));
    }
}
