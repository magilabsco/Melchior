//! Drives the real binary over stdio against a loopback OpenAI-compatible
//! model:
//!
//! - with a loopback MCP gateway that re-exports Leviathan's tools as
//!   `leviathan.*` beside a tool of its own, the way a host platform does;
//! - with a real `leviathan` binary (from `LEVIATHAN_BIN` or `PATH`; skipped
//!   when there is none): the agent indexes a CSV, searches it, and answers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use melchior::json::{self, Json};

// ---- loopback HTTP ---------------------------------------------------------

type Request = (String, Vec<(String, String)>, String);
type Reply = (&'static str, &'static str, String);
type Handler = Arc<dyn Fn(&str, &[(String, String)], &Json) -> Reply + Send + Sync>;

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let length: usize = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((path, headers, String::from_utf8(body).ok()?))
}

/// Serve `handler` on a loopback port.
fn serve(handler: Handler) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || {
                let Some((path, headers, body)) = read_request(&mut stream) else { return };
                let request = json::parse(&body).unwrap_or(Json::Null);
                let (status, content_type, body) = handler(&path, &headers, &request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

fn sse(chunks: &[String]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

/// A streamed model reply: one tool call, or a final answer.
fn model_reply(tool: Option<(&str, &str)>, answer: &str) -> Reply {
    let delta = match tool {
        Some((name, arguments)) => format!(
            r#"{{"choices":[{{"index":0,"delta":{{"role":"assistant","tool_calls":[{{"index":0,"id":"call-{name}","type":"function","function":{{"name":"{name}","arguments":{}}}}}]}},"finish_reason":"tool_calls"}}]}}"#,
            Json::from(arguments)
        ),
        None => format!(r#"{{"choices":[{{"index":0,"delta":{{"role":"assistant","content":{}}},"finish_reason":"stop"}}]}}"#, Json::from(answer)),
    };
    let usage = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#.to_string();
    ("200 OK", "text/event-stream", sse(&[delta, usage]))
}

fn tool_names(request: &Json) -> Vec<String> {
    request.get("tools").items().iter().filter_map(|t| t.get("function").get("name").as_str().map(str::to_string)).collect()
}

fn tool_messages(request: &Json) -> Vec<String> {
    request.get("messages").items().iter().filter(|m| m.get("role").as_str() == Some("tool")).map(|m| m.get("content").to_string()).collect()
}

// ---- the harness process ---------------------------------------------------

struct Harness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    messages: Vec<Json>,
}

impl Harness {
    fn start(args: &[&str], env: &[(&str, String)], cwd: &Path) -> Harness {
        let mut command = Command::new(env!("CARGO_BIN_EXE_melchior"));
        command.args(args).env_clear().env("PATH", "/usr/local/bin:/usr/bin:/bin");
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child =
            command.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Harness { child, stdin, stdout, messages: Vec::new() }
    }

    fn send(&mut self, line: String) {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    fn read_until(&mut self, done: &dyn Fn(&Json) -> bool) -> Json {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(Instant::now() < deadline, "timed out; saw {:?}", self.messages);
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "harness exited");
            let message = json::parse(&line).unwrap();
            self.messages.push(message.clone());
            if done(&message) {
                return message;
            }
        }
    }

    fn initialize(&mut self, workspace: &Path, model: &str) -> Json {
        self.send(format!(
            r#"{{"jsonrpc":"2.0","id":"init","method":"initialize","params":{{"cwd":{},"provider":"openrouter","model":"{model}"}}}}"#,
            Json::from(workspace.display().to_string())
        ));
        self.read_until(&|m| m.get("id").as_str() == Some("init"))
    }

    fn prompt(&mut self, id: &str, session: &str, text: &str) {
        self.send(format!(
            r#"{{"jsonrpc":"2.0","id":"{id}","method":"session/prompt","params":{{"sessionId":"{session}","contentBlocks":[{{"type":"text","text":{}}}]}}}}"#,
            Json::from(text)
        ));
        self.read_until(&|m| m.get("method").as_str() == Some("session.status") && m.get("params").get("status").as_str() == Some("idle"));
    }

    fn shutdown(mut self) -> Vec<Json> {
        self.send(r#"{"jsonrpc":"2.0","id":"bye","method":"shutdown"}"#.to_string());
        self.read_until(&|m| m.get("id").as_str() == Some("bye"));
        assert!(self.child.wait().unwrap().success());
        self.messages
    }
}

fn events(messages: &[Json]) -> Vec<&Json> {
    messages.iter().filter(|m| m.get("method").as_str() == Some("session.event")).map(|m| m.get("params").get("event")).collect()
}

fn last_answer(messages: &[Json]) -> String {
    events(messages)
        .iter()
        .rev()
        .find(|e| e.get("type").as_str() == Some("assistant/message"))
        .map(|e| e.get("data").get("message").get("content").at(0).get("text").str_or("").to_string())
        .unwrap_or_default()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("melchior-{name}-{}-{}", std::process::id(), melchior::util::random_hex(4)));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("workspace")).unwrap();
    dir
}

// ---- a host gateway that re-exports Leviathan ------------------------------

#[derive(Default)]
struct Seen {
    model_requests: Vec<Json>,
    mcp_calls: Vec<(String, Json)>,
    auth: Vec<String>,
}

fn mcp_result(id: &Json, result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#)
}

fn text_result(id: &Json, text: &str) -> String {
    mcp_result(id, &format!(r#"{{"content":[{{"type":"text","text":{}}}]}}"#, Json::from(text)))
}

const DESCRIBE: &str = r#"{"about":{"record":"record","group":"source"},"record_count":42,"group_count":2,"largest_groups":[{"key":"plant_work_orders","records":30}],"filters":[{"field":"kind","distinct":2,"top":[["table_row",40],["document",2]]}]}"#;
const BRIEFING: &str = r#"{"status":"ok","total_matches":2,"results":[{"id":"m-pin","kind":"preference","subject":"Sarah","key":"report.format","text":"Sarah wants answers as tables","valid_from":"2026-10-20T09:00:00Z"},{"id":"m-old","kind":"event","text":"Asked: \"old question\"","valid_from":"2026-10-01"}],"text":"Leviathan memory: 1 of 1 current memories.\n[preference] Sarah · report.format: Sarah wants answers as tables (2026-10-20 · m-pin)\nUse `recall` with words before acting on a subject.\n"}"#;
const RECALLED: &str = r#"{"status":"ok","total_matches":1,"results":[{"id":"m-ev","kind":"event","text":"Asked: \"Is filler 3 running?\". Answered: Filler 3 was down for a bearing.","valid_from":"2026-10-18"}],"text":"[event] Asked: \"Is filler 3 running?\"\n"}"#;
const SEARCH: &str = r#"{"status":"ok","total_matches":1,"results":[{"id":"wo-17","title":"Filler 3 bearing failure","group":"plant_work_orders","date":"2026-09-30","fields":{"status":"open"},"snippet":"Replaced the drive-side bearing on filler 3","relevance":1.0}]}"#;

fn gateway(seen: Arc<Mutex<Seen>>) -> Handler {
    Arc::new(move |path, headers, request| {
        if path == "/v1/chat/completions" {
            let tools = tool_names(request);
            let answered = !tool_messages(request).is_empty();
            seen.lock().unwrap().model_requests.push(request.clone());
            let call = tools.iter().any(|t| t == "mcp__tools__files_list") && !answered;
            return model_reply(call.then_some(("mcp__tools__files_list", "{}")), "Filler 3 had a bearing failure (wo-17).");
        }
        if path != "/mcp" {
            return ("404 Not Found", "text/plain", String::new());
        }
        let auth = headers.iter().find(|(n, _)| n == "authorization").map(|(_, v)| v.clone()).unwrap_or_default();
        seen.lock().unwrap().auth.push(auth);
        let id = request.get("id").clone();
        let body = match request.get("method").as_str().unwrap_or("") {
            "initialize" => mcp_result(&id, r#"{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"loopback"}}"#),
            m if m.starts_with("notifications/") => return ("202 Accepted", "application/json", String::new()),
            "tools/list" => mcp_result(
                &id,
                r#"{"tools":[
                    {"name":"files.list","description":"List uploaded files.","inputSchema":{"type":"object","properties":{}}},
                    {"name":"leviathan.describe","description":"Describe the index.","inputSchema":{"type":"object","properties":{}}},
                    {"name":"leviathan.search","description":"Search the index.","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}},
                    {"name":"leviathan.recall","description":"Your memory.","inputSchema":{"type":"object","properties":{"query":{"type":"string"}}}},
                    {"name":"leviathan.remember","description":"Save a memory.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
                    {"name":"leviathan.forget","description":"Retire a memory.","inputSchema":{"type":"object","properties":{"id":{"type":"string"}}}}
                ]}"#,
            ),
            "tools/call" => {
                let name = request.get("params").get("name").str_or("").to_string();
                let arguments = request.get("params").get("arguments").clone();
                seen.lock().unwrap().mcp_calls.push((name.clone(), arguments.clone()));
                match name.as_str() {
                    "leviathan.describe" => text_result(&id, DESCRIBE),
                    "leviathan.search" => text_result(&id, SEARCH),
                    "leviathan.recall" if arguments.get("query").is_null() => text_result(&id, BRIEFING),
                    "leviathan.recall" => text_result(&id, RECALLED),
                    "leviathan.remember" => text_result(&id, r#"{"status":"created","memory":{"id":"m-new"},"text":"remembered m-new\n"}"#),
                    "files.list" => text_result(&id, "maintenance_log.csv (table)"),
                    _ => mcp_result(&id, r#"{"content":[{"type":"text","text":"unknown"}],"isError":true}"#),
                }
            }
            _ => format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32601,"message":"nope"}}}}"#),
        };
        ("200 OK", "application/json", body)
    })
}

#[test]
fn serves_a_turn_with_leviathan_through_a_gateway() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let port = serve(gateway(seen.clone()));
    let root = temp_dir("gateway");
    let (home, workspace) = (root.join("home"), root.join("workspace"));
    let patch = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/data-assistant.patch.yml");
    let tool_patch = workspace.join(".tools.patch.yml");
    std::fs::write(
        &tool_patch,
        format!(
            "- insert:\n    - id: mcp-tools\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: tools\n        url: http://127.0.0.1:{port}/mcp\n        headers:\n          Authorization: !!js '`Bearer ${{process.env.GATEWAY_TOKEN}}`'\n"
        ),
    )
    .unwrap();
    let tool_patch = tool_patch.display().to_string();
    let mut harness = Harness::start(
        &["--serve", "--patch", patch, "--patch", &tool_patch],
        &[
            ("MELCHIOR_HOME", home.display().to_string()),
            ("AGENT_PROMPT", "You are a plant analyst. Use leviathan.search for documents.".into()),
            ("AGENT_MODEL", "loopback-model".into()),
            ("MODEL_API_KEY", "model-key".into()),
            ("MODEL_BASE_URL", format!("http://127.0.0.1:{port}/v1")),
            ("GATEWAY_TOKEN", "gateway-token".into()),
        ],
        &workspace,
    );
    let init = harness.initialize(&workspace, "loopback-model");
    assert_eq!(init.get("result").get("serverInfo").get("name").as_str(), Some("melchior"), "{init}");
    let tools: Vec<&str> = init.get("result").get("plugins").get("tools").items().iter().filter_map(|t| t.as_str()).collect();
    assert!(tools.contains(&"mcp__tools__leviathan_search"), "{tools:?}");
    assert!(!tools.contains(&"bash") && !tools.contains(&"leviathan_index"), "the patch leaves only gateway tools: {tools:?}");

    harness.prompt("p1", "s1", "[Today is Sunday, October 25, 2026.]\n\nWhat failed on filler 3?");
    let messages = harness.shutdown();

    let response = messages.iter().find(|m| m.get("id").as_str() == Some("p1")).unwrap();
    let message_id = response.get("result").get("messageId").as_str().unwrap();
    let events = events(&messages);
    let spliced = events.iter().find(|e| e.get("type").as_str() == Some("agent/inbox/spliced")).unwrap();
    assert_eq!(spliced.get("data").get("inserted").at(0).get("id").as_str(), Some(message_id));
    assert_eq!(last_answer(&messages), "Filler 3 had a bearing failure (wo-17).");
    assert!(events.iter().any(|e| e.get("type").as_str() == Some("turn/end") && e.get("data").get("reason").get("kind").as_str() == Some("stop")));

    let seen = seen.lock().unwrap();
    assert!(seen.auth.iter().all(|a| a == "Bearer gateway-token"), "{:?}", seen.auth);
    let names: Vec<&str> = seen.mcp_calls.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"leviathan.describe") && names.contains(&"leviathan.search") && names.contains(&"files.list"), "{names:?}");
    let (_, search_args) = seen.mcp_calls.iter().find(|(n, _)| n == "leviathan.search").unwrap();
    assert_eq!(search_args.get("query").as_str(), Some("What failed on filler 3?"), "a leading context line is not part of the query");

    let first = &seen.model_requests[0];
    assert_eq!(first.get("model").as_str(), Some("loopback-model"));
    let system = first.get("messages").at(0).get("content").str_or("");
    assert!(system.starts_with("You are a plant analyst."), "persona first: {system}");
    assert!(system.contains("42 records across 2 sources"), "briefing: {system}");
    assert!(system.contains("leviathan.search → mcp__tools__leviathan_search"), "name map: {system}");
    assert!(system.find("# Workspace index").unwrap() < system.find("# Tool names").unwrap(), "the briefing leads: {system}");
    let user = first.get("messages").at(1).to_string();
    assert!(user.contains("<workspace_recall") && user.contains("Filler 3 bearing failure [id: wo-17]"), "recall: {user}");
    assert!(user.contains("<agent_memory") && user.contains("[preference] Sarah · report.format: Sarah wants answers as tables (2026-10-20 · m-pin)"), "memory briefing: {user}");
    assert!(!user.contains("old question"), "journal events stay out of the briefing: {user}");
    assert!(user.contains("Is filler 3 running?"), "memory recall: {user}");
    assert!(!user.contains("Use `recall`"), "Leviathan's own tool advice is not passed on: {user}");
    assert!(system.contains("# Memory (Leviathan)") && system.contains("`mcp__tools__leviathan_remember`"), "memory guide: {system}");

    let recalls: Vec<&Json> = seen.mcp_calls.iter().filter(|(n, _)| n == "leviathan.recall").map(|(_, a)| a).collect();
    assert_eq!(recalls.len(), 2, "one briefing and one query recall on the first turn");
    assert!(recalls.iter().all(|a| a.get("format").as_str() == Some("json")));
    assert!(recalls.iter().all(|a| a.get("ns").is_null()), "no namespace unless one is configured");
    let (_, journal) = seen.mcp_calls.iter().find(|(n, _)| n == "leviathan.remember").expect("the turn is journaled before shutdown");
    assert_eq!(journal.get("kind").as_str(), Some("event"));
    assert_eq!(journal.get("source").as_str(), Some("conversation:s1"));
    assert_eq!(
        journal.get("text").as_str(),
        Some("Asked: \"What failed on filler 3?\". Answered: Filler 3 had a bearing failure (wo-17).")
    );
    let second = &seen.model_requests[1];
    assert!(second.get("messages").to_string().contains("maintenance_log.csv"), "tool result reached the model");

    let log = std::fs::read_to_string(home.join("melchior/sessions/s1/events.jsonl")).unwrap();
    for kind in ["session/created", "message/user", "message/context", "message/assistant", "tool/result", "turn/end"] {
        assert!(log.contains(&format!(r#""type":"{kind}""#)), "{kind} missing from the session log");
    }
    assert!(log.contains(r#""source":"leviathan-memory""#), "memory context is persisted with the turn");
    assert!(!log.contains("gateway-token") && !log.contains("model-key"), "secrets never reach the session log");
    let _ = std::fs::remove_dir_all(&root);
}

// ---- a real Leviathan --------------------------------------------------------

fn leviathan_binary() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("LEVIATHAN_BIN") {
        return Some(PathBuf::from(bin)).filter(|p| p.is_file());
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("leviathan")).find(|p| p.is_file())
}

const TICKETS: &str = "id,customer,created,title,body\n\
T-1,Acme,2026-09-01,Invoice totals wrong,The October invoice double counts seats\n\
T-2,Acme,2026-09-03,Login loops after password reset,Users who reset their password get sent back to sign-in in an endless loop; cleared stale session cookies in 4.2.1\n\
T-3,Globex,2026-09-05,Export times out,CSV export of 2M rows times out after 30 seconds\n";

#[test]
fn indexes_and_searches_with_a_local_leviathan() {
    let Some(bin) = leviathan_binary() else {
        eprintln!("skipped: no leviathan binary (set LEVIATHAN_BIN or put leviathan on PATH)");
        return;
    };
    let requests = Arc::new(Mutex::new(Vec::<Json>::new()));
    let seen = requests.clone();
    let port = serve(Arc::new(move |path, _headers, request| {
        if path != "/v1/chat/completions" {
            return ("404 Not Found", "text/plain", String::new());
        }
        seen.lock().unwrap().push(request.clone());
        match tool_messages(request).len() {
            0 => model_reply(Some(("leviathan_index", r#"{"paths":["tickets.csv"],"id":"id","title":["title"],"group":["customer"],"date":["created"]}"#)), ""),
            1 => model_reply(Some(("leviathan_search", r#"{"query":"login loop password reset"}"#)), ""),
            _ => model_reply(None, "T-2: the login loop after a password reset, fixed in 4.2.1."),
        }
    }));
    let root = temp_dir("local");
    let (home, workspace) = (root.join("home"), root.join("workspace"));
    std::fs::write(workspace.join("tickets.csv"), TICKETS).unwrap();
    let patch = root.join("model.patch.yml");
    std::fs::write(
        &patch,
        "- id: llm-pi-ai\n  config:\n    providers:\n      openrouter:\n        apiKeyEnv: MODEL_API_KEY\n        baseURL: !!js process.env.MODEL_BASE_URL\n- id: llm-deepseek\n  disabled: true\n",
    )
    .unwrap();
    let (index, memory) = (root.join("data/tickets.db"), root.join("data/memory.db"));
    std::fs::create_dir_all(root.join("data")).unwrap();
    let args = [
        "--serve".to_string(),
        "--patch".into(),
        patch.display().to_string(),
        "--index".into(),
        index.display().to_string(),
        "--memory".into(),
        memory.display().to_string(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut harness = Harness::start(
        &args,
        &[
            ("MELCHIOR_HOME", home.display().to_string()),
            ("LEVIATHAN_BIN", bin.display().to_string()),
            ("MODEL_API_KEY", "model-key".into()),
            ("MODEL_BASE_URL", format!("http://127.0.0.1:{port}/v1")),
        ],
        &workspace,
    );
    let init = harness.initialize(&workspace, "loopback-model");
    let tools: Vec<&str> = init.get("result").get("plugins").get("tools").items().iter().filter_map(|t| t.as_str()).collect();
    assert!(tools.contains(&"leviathan_index") && tools.contains(&"memory_remember"), "{tools:?}");

    harness.prompt("p1", "s1", "Which ticket is about the login loop after a password reset?");
    harness.prompt("p2", "s1", "And which customer reported the export timeout?");
    let messages = harness.shutdown();
    assert!(last_answer(&messages).starts_with("T-2"), "{}", last_answer(&messages));
    assert!(index.is_file(), "the index was built");

    let requests = requests.lock().unwrap();
    let first_system = requests[0].get("messages").at(0).get("content").str_or("").to_string();
    assert!(first_system.contains("No dataset is indexed yet") && first_system.contains("`leviathan_index`"), "{first_system}");
    let after_index = &requests[1];
    let results = tool_messages(after_index).join("\n");
    assert!(results.contains("searchable now"), "index result: {results}");
    assert!(tool_names(after_index).contains(&"leviathan_search".to_string()), "search appears once data is indexed");
    let after_search = tool_messages(&requests[2]).join("\n");
    assert!(after_search.contains("T-2"), "search found the ticket: {after_search}");

    // The second message: briefing on the indexed data, recall, and memory.
    let second = requests.iter().rev().find(|r| r.get("messages").to_string().contains("export timeout")).unwrap();
    let system = second.get("messages").at(0).get("content").str_or("");
    assert!(system.contains("3 records across 2"), "briefing on the indexed data: {system}");
    assert!(second.get("messages").to_string().contains("<workspace_recall"), "recall ran on the second message");
    assert!(memory.is_file(), "answered turns are journaled to memory");
    let _ = std::fs::remove_dir_all(&root);
}

/// Data in the workspace is indexed before the first model call: the first
/// request already has search, a briefing on the data, and recall, and the
/// model spends no turn on indexing.
#[test]
fn workspace_data_is_indexed_before_the_first_model_call() {
    let Some(bin) = leviathan_binary() else {
        eprintln!("skipped: no leviathan binary (set LEVIATHAN_BIN or put leviathan on PATH)");
        return;
    };
    let requests = Arc::new(Mutex::new(Vec::<Json>::new()));
    let seen = requests.clone();
    let port = serve(Arc::new(move |path, _headers, request| {
        if path != "/v1/chat/completions" {
            return ("404 Not Found", "text/plain", String::new());
        }
        seen.lock().unwrap().push(request.clone());
        model_reply(None, "Globex (T-3).")
    }));
    let root = temp_dir("autoindex");
    let (home, workspace) = (root.join("home"), root.join("workspace"));
    std::fs::write(workspace.join("tickets.csv"), TICKETS).unwrap();
    std::fs::write(
        home.join("settings.yaml"),
        format!("llm-pi-ai:\n  providers:\n    openrouter:\n      baseURL: http://127.0.0.1:{port}/v1\nleviathan:\n  autoIndexMinBytes: 1\n"),
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.parent().unwrap().display());
    let output = Command::new(env!("CARGO_BIN_EXE_melchior"))
        .arg("Which customer reported that the export times out?")
        .env_clear()
        .env("PATH", path)
        .env("MELCHIOR_HOME", &home)
        .env("OPENROUTER_API_KEY", "model-key")
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "answered in one model call");
    let first = &requests[0];
    assert!(tool_names(first).contains(&"leviathan_search".to_string()), "{:?}", tool_names(first));
    let system = first.get("messages").at(0).get("content").str_or("");
    assert!(system.contains("It holds 3 ") && system.contains("indexed tickets.csv for you"), "{system}");
    assert!(first.get("messages").at(1).to_string().contains("<workspace_recall"), "recall on the first message");
    let _ = std::fs::remove_dir_all(&root);
}

/// One headless task with nothing configured: `leviathan` on `PATH` is
/// enough for a per-workspace index the agent can fill.
#[test]
fn headless_task_starts_leviathan_without_configuration() {
    let Some(bin) = leviathan_binary() else {
        eprintln!("skipped: no leviathan binary (set LEVIATHAN_BIN or put leviathan on PATH)");
        return;
    };
    let port = serve(Arc::new(move |path, _headers, request| {
        if path != "/v1/chat/completions" {
            return ("404 Not Found", "text/plain", String::new());
        }
        match tool_messages(request).len() {
            0 => model_reply(Some(("leviathan_index", r#"{"paths":["tickets.csv"],"id":"id","group":["customer"]}"#)), ""),
            1 => model_reply(Some(("leviathan_search", r#"{"query":"export times out"}"#)), ""),
            _ => {
                let found = tool_messages(request).join("\n");
                model_reply(None, if found.contains("T-3") { "Globex (T-3)." } else { "not found" })
            }
        }
    }));
    let root = temp_dir("headless");
    let (home, workspace) = (root.join("home"), root.join("workspace"));
    std::fs::write(workspace.join("tickets.csv"), TICKETS).unwrap();
    std::fs::write(
        home.join("settings.yaml"),
        format!("llm-pi-ai:\n  providers:\n    openrouter:\n      baseURL: http://127.0.0.1:{port}/v1\n"),
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.parent().unwrap().display());
    let output = Command::new(env!("CARGO_BIN_EXE_melchior"))
        .arg("Which customer reported the export timeout?")
        .env_clear()
        .env("PATH", path)
        .env("MELCHIOR_HOME", &home)
        .env("OPENROUTER_API_KEY", "model-key")
        .current_dir(&workspace)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stdout.trim(), "Globex (T-3).");
    let indexes: Vec<PathBuf> = std::fs::read_dir(home.join("leviathan")).unwrap().flatten().map(|e| e.path()).collect();
    assert!(indexes.iter().any(|p| p.extension().is_some_and(|e| e == "db") && !p.to_string_lossy().contains("memory")), "{indexes:?}");
    let _ = std::fs::remove_dir_all(&root);
}
