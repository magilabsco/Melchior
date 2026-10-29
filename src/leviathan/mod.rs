//! Leviathan: the data and memory layer at the centre of Melchior.
//!
//! [Leviathan](https://github.com/elstongun/leviathan) turns records of any
//! size (CSV, JSON/JSONL, SQLite, database exports) into a ranked full-text
//! index that answers in a few hundred tokens, and keeps an agent's memory
//! of typed claims beside it. Melchior builds every turn around it:
//!
//! - **Briefing.** The system prompt says what is indexed (records, groups,
//!   date range, filters) and how to search it.
//! - **Recall.** Each user message is searched before the model sees it; the
//!   top cards arrive with the message as `<workspace_recall>`.
//! - **Tools.** Search, open, resolve and describe the data, and index new
//!   data the user points at (`leviathan_index`), as first-class tools.
//! - **Memory** (plugin `leviathan-memory`, in [`memory`]): briefing and
//!   recall of the agent's own notes, and a journal of answered questions.
//!
//! Leviathan is a separate program, so Melchior keeps no dependencies. It is
//! reached through a [`Backend`]:
//!
//! | Backend | When | How |
//! | --- | --- | --- |
//! | `local` | `index` / `memory` set, or `LEVIATHAN_INDEX` / `LEVIATHAN_MEMORY` | spawns `leviathan mcp` over stdio |
//! | `remote` | `url` set, or `LEVIATHAN_URL` | `leviathan serve --http`, MCP over HTTP with a bearer token |
//! | `mcp` | an `mcp-client` server exposes Leviathan's tools | e.g. a gateway's `leviathan.search` |
//! | (implicit) `local` | nothing configured and `leviathan` is on `PATH` | a per-workspace index and memory in the harness home |
//!
//! Other plugins can register more backends in the `leviathan-backends`
//! registry (a host's permission-checked gateway, for example) and select
//! them with `backend: <name>`.

pub mod index;
pub mod auto;
pub mod memory;
pub mod tools;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cordis::{Context, Plugin, Registry};
use crate::json::{self, Json};
use crate::mcp::{McpHub, McpServer, RawTool, StdioCommand};
use crate::prompt::{PromptService, TurnContext, TurnHooks};
use crate::runtime::Runtime;
use crate::{log_debug, log_info, log_warn, util};

/// Leviathan's data tools, by their own names.
pub const DATA_TOOLS: &[&str] = &["search", "describe", "get", "resolve_group", "aggregate"];
/// Leviathan's memory tools, by their own names.
pub const MEMORY_TOOLS: &[&str] = &["recall", "remember", "forget", "memory_describe"];

pub fn is_memory_tool(tool: &str) -> bool {
    MEMORY_TOOLS.contains(&tool)
}

/// One way to reach Leviathan. Tools are named as Leviathan names them
/// (`search`, `recall`, ...); results are Leviathan's JSON.
pub trait Backend: Send + Sync {
    /// For logs and the load report, e.g. `local index /data/tickets.db`.
    fn label(&self) -> String;
    /// Whether the tool is reachable now.
    fn has(&self, tool: &str) -> bool;
    /// Call a tool for its JSON result.
    fn call(&self, tool: &str, args: Json) -> Result<Json, String>;
    /// The function name the model calls this tool by, if the model has it.
    fn public_name(&self, tool: &str) -> Option<String>;
    /// Runs before each top-level turn: reconnect or refresh state here.
    fn refresh(&self) {}
}

pub type Backends = Registry<Box<dyn Backend>>;

/// Call a Leviathan tool on an MCP server and parse its JSON payload.
pub fn call_json(server: &McpServer, raw_name: &str, mut args: Json) -> Result<Json, String> {
    args.set("format", "json");
    let output = server.call(raw_name, &args);
    if output.is_error {
        return Err(format!("{raw_name} failed: {}", util::truncate(&output.content, 300, "…")));
    }
    json::parse(&output.content)
        .ok()
        .filter(|v| matches!(v, Json::Obj(_)))
        .ok_or_else(|| format!("{raw_name} returned no JSON: {}", util::truncate(&output.content, 200, "…")))
}

// ---- the mcp backend -------------------------------------------------------

/// Leviathan's tools on a server connected by an `mcp-client` entry: either
/// Leviathan itself (`leviathan serve --http`, tools named `search`, ...) or
/// a gateway that re-exports them with a prefix (`leviathan.search`).
pub struct McpBackend {
    hub: Arc<McpHub>,
    server: Option<String>,
    prefix: Option<String>,
    /// Last place each tool was found, so calls made while the harness shuts
    /// down (after `mcp-client` left the hub) still reach the server.
    seen: Mutex<HashMap<String, (Arc<McpServer>, RawTool)>>,
}

impl McpBackend {
    pub fn new(hub: Arc<McpHub>, server: Option<String>, prefix: Option<String>) -> McpBackend {
        McpBackend { hub, server, prefix, seen: Mutex::new(HashMap::new()) }
    }

    /// The server and raw tool that answer `tool`, falling back to where it
    /// was last seen.
    pub fn locate(&self, tool: &str) -> Option<(Arc<McpServer>, RawTool)> {
        let found = self.find(tool);
        let mut seen = self.seen.lock().unwrap();
        match found {
            Some(found) => {
                seen.insert(tool.to_string(), found.clone());
                Some(found)
            }
            None => seen.get(tool).filter(|(server, _)| !self.hub.servers().iter().any(|s| Arc::ptr_eq(s, server))).cloned(),
        }
    }

    fn find(&self, tool: &str) -> Option<(Arc<McpServer>, RawTool)> {
        let prefixes: Vec<&str> = match &self.prefix {
            Some(prefix) => vec![prefix.as_str()],
            None => vec!["leviathan.", "leviathan_"],
        };
        for prefix in &prefixes {
            if let Some(found) = self.hub.find(self.server.as_deref(), &format!("{prefix}{tool}")) {
                return Some(found);
            }
        }
        if self.prefix.is_some() {
            return None;
        }
        // Bare names (`search`) only on a server that is Leviathan itself.
        self.hub
            .servers()
            .into_iter()
            .filter(|s| self.server.as_deref().map_or(s.name.contains("leviathan"), |n| s.name == n))
            .find_map(|s| s.find(tool).map(|t| (s.clone(), t)))
    }
}

impl Backend for McpBackend {
    fn label(&self) -> String {
        format!("MCP server {}", self.server.as_deref().unwrap_or("(discovered)"))
    }
    fn has(&self, tool: &str) -> bool {
        self.find(tool).is_some()
    }
    fn call(&self, tool: &str, args: Json) -> Result<Json, String> {
        let (server, raw) = self.locate(tool).ok_or(format!("Leviathan's {tool} is not available"))?;
        call_json(&server, &raw.name, args)
    }
    fn public_name(&self, tool: &str) -> Option<String> {
        self.find(tool).map(|(_, raw)| raw.public_name)
    }
}

// ---- local and remote backends ---------------------------------------------

type OnConnect = Box<dyn Fn(&Arc<McpServer>) + Send + Sync>;

/// A Leviathan Melchior connects to itself: a local `leviathan mcp` child
/// process, or a remote `leviathan serve --http`. Its tools are registered
/// as first-class tools (`leviathan_search`, `memory_remember`, ...).
pub struct OwnedBackend {
    pub server: Arc<McpServer>,
    label: String,
    last_try: AtomicU64,
    attempt: Mutex<()>,
    /// Registers the server's tools after each successful connect.
    on_connect: OnConnect,
}

impl OwnedBackend {
    /// Connect now (or reconnect, restarting a local server).
    pub fn connect(&self) -> Result<(), String> {
        let _one = self.attempt.lock().unwrap();
        self.last_try.store(util::unix_millis(), Ordering::SeqCst);
        self.server.connect()?;
        (self.on_connect)(&self.server);
        log_info!("leviathan", "connected to {} ({} tools)", self.label, self.server.tools.lock().unwrap().len());
        Ok(())
    }

    /// Connect if not connected, at most every 15 seconds.
    pub fn ensure(&self) -> bool {
        if self.server.connected.load(Ordering::SeqCst) {
            return true;
        }
        if util::unix_millis().saturating_sub(self.last_try.load(Ordering::SeqCst)) < 15_000 {
            return false;
        }
        if let Err(e) = self.connect() {
            log_warn!("leviathan", "{}: {e}; will retry before the next turn", self.label);
        }
        self.server.connected.load(Ordering::SeqCst)
    }
}

impl Backend for OwnedBackend {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn has(&self, tool: &str) -> bool {
        self.server.find(tool).is_some()
    }
    fn call(&self, tool: &str, args: Json) -> Result<Json, String> {
        if !self.ensure() {
            return Err(format!("{} is not connected", self.label));
        }
        call_json(&self.server, tool, args)
    }
    fn public_name(&self, tool: &str) -> Option<String> {
        self.server.find(tool).map(|_| tools::public_name(tool))
    }
    fn refresh(&self) {
        self.ensure();
    }
}

// ---- configuration ---------------------------------------------------------

#[derive(Clone)]
pub struct LeviathanConfig {
    /// `auto`, `local`, `remote`, `mcp`, or a backend another plugin registered.
    pub backend: String,
    pub bin: String,
    pub index: Option<PathBuf>,
    pub memory: Option<PathBuf>,
    pub url: Option<String>,
    pub token: Option<String>,
    pub server: Option<String>,
    pub prefix: Option<String>,
    /// Memory namespace forced on every memory call.
    pub namespace: Option<String>,
    pub auto_start: bool,
    pub tools: bool,
    pub index_tool: bool,
    /// Index the workspace's large data files before the first model call.
    pub auto_index: bool,
    pub auto_index_min_bytes: u64,
    pub briefing: bool,
    pub recall: bool,
    pub recall_limit: i64,
    pub min_relevance: f64,
    pub min_query_chars: usize,
    pub briefing_ttl: Duration,
    pub call_timeout: Duration,
    pub index_timeout: Duration,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl LeviathanConfig {
    /// Config keys first, then `LEVIATHAN_*` environment variables.
    pub fn from_json(config: &Json, runtime: &Runtime) -> LeviathanConfig {
        let text = |key: &str, env: &str| non_empty(config.get(key).as_str().map(str::to_string)).or_else(|| non_empty(std::env::var(env).ok()));
        let memory = match config.get("memory") {
            Json::Bool(false) => None,
            _ => text("memory", "LEVIATHAN_MEMORY").map(|p| runtime.resolve_path(&p)),
        };
        let token_env = config.get("tokenEnv").str_or("LEVIATHAN_TOKEN").to_string();
        runtime.add_secret_env(&token_env);
        LeviathanConfig {
            backend: config.get("backend").str_or("auto").to_string(),
            bin: text("bin", "LEVIATHAN_BIN").unwrap_or_else(|| "leviathan".into()),
            index: text("index", "LEVIATHAN_INDEX").map(|p| runtime.resolve_path(&p)),
            memory,
            url: text("url", "LEVIATHAN_URL"),
            token: non_empty(config.get("token").as_str().map(str::to_string)).or_else(|| non_empty(std::env::var(&token_env).ok())),
            server: config.get("server").as_str().map(str::to_string),
            prefix: config.get("prefix").as_str().map(str::to_string),
            namespace: non_empty(config.get("namespace").as_str().map(str::to_string)),
            auto_start: config.get("autoStart").as_bool().unwrap_or(true),
            tools: config.get("tools").as_bool().unwrap_or(true),
            index_tool: config.get("indexTool").as_bool().unwrap_or(true),
            auto_index: config.get("autoIndex").as_bool().unwrap_or(true),
            auto_index_min_bytes: config.get("autoIndexMinBytes").as_i64().unwrap_or(1 << 20).max(0) as u64,
            briefing: config.get("briefing").as_bool().unwrap_or(true),
            recall: config.get("recall").as_bool().unwrap_or(true),
            recall_limit: config.get("recallLimit").as_i64().unwrap_or(5).clamp(1, 20),
            min_relevance: config.get("minRelevance").as_f64().unwrap_or(0.15),
            min_query_chars: config.get("minQueryChars").as_i64().unwrap_or(8).max(1) as usize,
            briefing_ttl: Duration::from_secs(config.get("briefingTtlSeconds").as_i64().unwrap_or(120).max(0) as u64),
            call_timeout: Duration::from_secs(config.get("timeoutSeconds").as_i64().unwrap_or(60).clamp(5, 900) as u64),
            index_timeout: Duration::from_secs(config.get("indexTimeoutSeconds").as_i64().unwrap_or(3600).clamp(10, 86_400) as u64),
        }
    }
}

/// Find an executable the way a shell would.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|p| p.is_file()))
}

// ---- the service -----------------------------------------------------------

/// The `leviathan` service: whichever backend is selected, plus the index
/// briefing and recall built on it.
pub struct Leviathan {
    backends: Arc<Backends>,
    pub config: LeviathanConfig,
    /// The local backend started without configuration; used only when no
    /// other backend has Leviathan's tools.
    implicit: Option<Arc<OwnedBackend>>,
    briefing: Mutex<Option<(Instant, Option<index::Briefing>)>>,
    /// Set when `leviathan_index` is available.
    pub index_tool: AtomicBool,
    /// What automatic indexing did, appended to the briefing.
    pub note: Mutex<Option<String>>,
}

impl Leviathan {
    pub fn new(backends: Arc<Backends>, config: LeviathanConfig) -> Leviathan {
        Leviathan { backends, config, implicit: None, briefing: Mutex::new(None), index_tool: AtomicBool::new(false), note: Mutex::new(None) }
    }

    /// Whether the implicit per-workspace backend is the one in use (no
    /// configured or discovered backend answers).
    pub fn implicit_in_use(&self) -> bool {
        let Some(implicit) = &self.implicit else { return false };
        self.backend().is_some_and(|b| b.label() == implicit.label())
    }

    fn reachable(backend: &dyn Backend) -> bool {
        backend.has("search") || backend.has("recall") || backend.has("describe")
    }

    /// The backend in use now, if any.
    pub fn backend(&self) -> Option<Arc<Box<dyn Backend>>> {
        if self.config.backend != "auto" {
            return self.backends.get(&self.config.backend);
        }
        let all = self.backends.all();
        for name in ["local", "remote"] {
            if let Some((_, backend)) = all.iter().find(|(n, _)| n == name) {
                return Some(backend.clone());
            }
        }
        if let Some((_, backend)) = all.iter().find(|(n, b)| n != "mcp" && n != "implicit" && Self::reachable(b.as_ref().as_ref())) {
            return Some(backend.clone());
        }
        if let Some((_, backend)) = all.iter().find(|(n, b)| n == "mcp" && Self::reachable(b.as_ref().as_ref())) {
            return Some(backend.clone());
        }
        let implicit = self.implicit.as_ref()?;
        implicit.ensure();
        self.backends.get("implicit")
    }

    pub fn has(&self, tool: &str) -> bool {
        self.backend().is_some_and(|b| b.has(tool))
    }

    pub fn public_name(&self, tool: &str) -> Option<String> {
        self.backend()?.public_name(tool)
    }

    /// Call a Leviathan tool for JSON. `None` when it is unavailable or
    /// failed (logged). Memory calls carry the configured namespace.
    pub fn call(&self, tool: &str, mut args: Json) -> Option<Json> {
        let backend = self.backend()?;
        if !backend.has(tool) {
            return None;
        }
        if is_memory_tool(tool)
            && let Some(ns) = &self.config.namespace
        {
            args.set("ns", ns.as_str());
        }
        match backend.call(tool, args) {
            Ok(value) => Some(value),
            Err(e) => {
                log_info!("leviathan", "{e}");
                None
            }
        }
    }

    /// Run each backend's refresh (reconnects), before a top-level turn.
    pub fn refresh(&self) {
        if let Some(backend) = self.backend() {
            backend.refresh();
        }
    }

    /// Forget the cached briefing (after the index changed).
    pub fn invalidate(&self) {
        *self.briefing.lock().unwrap() = None;
    }

    pub fn briefing(&self) -> Option<index::Briefing> {
        let mut cached = self.briefing.lock().unwrap();
        if let Some((at, briefing)) = cached.as_ref()
            && at.elapsed() < self.config.briefing_ttl
        {
            return briefing.clone();
        }
        let names = self.tool_names();
        let fresh = match self.call("describe", Json::obj()) {
            Some(describe) => Some(index::Briefing {
                text: index::render_briefing(&describe, &names),
                record_count: describe.get("record_count").as_i64().unwrap_or(0),
            }),
            // No data index yet: say how to make one, if the agent can.
            None if names.index.is_some() => Some(index::Briefing { text: index::render_empty(&names), record_count: 0 }),
            None => None,
        };
        let fresh = match (fresh, self.note.lock().unwrap().clone()) {
            (Some(mut b), Some(note)) => {
                b.text = format!("{}\n{note}\n", b.text.trim_end());
                Some(b)
            }
            (fresh, _) => fresh,
        };
        *cached = Some((Instant::now(), fresh.clone()));
        fresh
    }

    pub fn tool_names(&self) -> index::ToolNames {
        let backend = self.backend();
        let name = |tool: &str| backend.as_ref().and_then(|b| b.public_name(tool));
        index::ToolNames {
            search: name("search"),
            get: name("get"),
            resolve: name("resolve_group"),
            aggregate: name("aggregate"),
            index: self.index_tool.load(Ordering::SeqCst).then(|| tools::INDEX_TOOL.to_string()),
            renamed: backend.is_some_and(|b| b.public_name("search").is_some_and(|n| n != "search")),
        }
    }

    pub fn recall(&self, message: &str) -> Option<TurnContext> {
        let query = index::recall_query(message);
        if query.chars().count() < self.config.min_query_chars {
            return None;
        }
        if self.config.briefing && self.briefing().is_some_and(|b| b.record_count == 0) {
            return None;
        }
        let result = self.call("search", crate::jobj! { "query" => query.as_str(), "limit" => self.config.recall_limit })?;
        let text = index::render_recall(&result, self.config.min_relevance, &self.tool_names())?;
        log_debug!("leviathan", "recalled {} chars for {:?}", text.len(), util::truncate(&query, 80, "…"));
        Some(TurnContext { source: "leviathan".into(), text })
    }
}

// ---- the plugin ------------------------------------------------------------

/// Where the implicit local backend keeps one workspace's index and memory.
fn implicit_paths(runtime: &Runtime) -> (PathBuf, PathBuf) {
    let key = format!("{:012x}", util::fnv1a(runtime.cwd.to_string_lossy().as_bytes()) & 0xffff_ffff_ffff);
    let dir = runtime.home.join("leviathan");
    (dir.join(format!("{key}.db")), dir.join(format!("{key}.memory.db")))
}

fn local_server(name: &str, bin: &Path, index: Option<&Path>, memory: Option<&Path>, runtime: &Runtime, timeout: Duration) -> Result<McpServer, String> {
    let mut args = Vec::new();
    if let Some(index) = index {
        args.push("--index".to_string());
        args.push(index.display().to_string());
    }
    if let Some(memory) = memory {
        args.push(format!("--memory={}", memory.display()));
    }
    args.push("mcp".into());
    let command = StdioCommand { command: bin.display().to_string(), args, env: runtime.child_env(), cwd: Some(runtime.cwd.clone()) };
    McpServer::stdio(name, command, timeout)
}

/// `leviathan`: connects Melchior to Leviathan, registers its tools, the
/// briefing section, and per-message recall. Provides the `leviathan` service.
pub struct LeviathanPlugin;

impl LeviathanPlugin {
    /// A backend Melchior connects itself; its server stops with the scope.
    fn owned(ctx: &Context, config: &LeviathanConfig, server: McpServer, label: String) -> Arc<OwnedBackend> {
        let (tool_ctx, register_tools, namespace) = (ctx.clone(), config.tools, config.namespace.clone());
        let backend = Arc::new(OwnedBackend {
            server: Arc::new(server),
            label,
            last_try: AtomicU64::new(0),
            attempt: Mutex::new(()),
            on_connect: Box::new(move |server| {
                if register_tools {
                    tools::register_server_tools(&tool_ctx, server, namespace.clone());
                }
            }),
        });
        let closing = backend.server.clone();
        ctx.effect(move || closing.close());
        backend
    }
}

/// Lets a registry hold the same backend the plugin keeps a handle to.
struct Shared(Arc<OwnedBackend>);

impl Backend for Shared {
    fn label(&self) -> String {
        self.0.label()
    }
    fn has(&self, tool: &str) -> bool {
        self.0.has(tool)
    }
    fn call(&self, tool: &str, args: Json) -> Result<Json, String> {
        self.0.call(tool, args)
    }
    fn public_name(&self, tool: &str) -> Option<String> {
        self.0.public_name(tool)
    }
    fn refresh(&self) {
        self.0.refresh()
    }
}

impl Plugin for LeviathanPlugin {
    fn name(&self) -> &'static str {
        "leviathan"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["runtime", "mcp", "prompt", "turn-hooks", "tools", "leviathan-backends"]
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let runtime = ctx.inject::<Runtime>("runtime")?;
        let backends = ctx.inject::<Backends>("leviathan-backends")?;
        let config = LeviathanConfig::from_json(ctx.config(), &runtime);
        let mut leviathan = Leviathan::new(backends.clone(), config.clone());
        let wants = |name: &str| config.backend == "auto" || config.backend == name;
        let bin = find_binary(&config.bin).unwrap_or_else(|| PathBuf::from(&config.bin));

        // The backend Melchior connects itself, if any: its registry name,
        // the backend, and the index file `leviathan_index` builds into.
        let mut owned: Option<(&str, Arc<OwnedBackend>, Option<PathBuf>)> = None;
        if wants("local") && (config.index.is_some() || config.memory.is_some()) {
            let label = match (&config.index, &config.memory) {
                (Some(index), _) => format!("local index {}", index.display()),
                (None, Some(memory)) => format!("local memory {}", memory.display()),
                (None, None) => unreachable!("checked above"),
            };
            let server = local_server("leviathan", &bin, config.index.as_deref(), config.memory.as_deref(), &runtime, config.call_timeout)?;
            owned = Some(("local", Self::owned(ctx, &config, server, label), config.index.clone()));
        } else if wants("remote")
            && let Some(url) = &config.url
        {
            let base = url.trim_end_matches('/');
            let endpoint = if base.ends_with("/mcp") { base.to_string() } else { format!("{base}/mcp") };
            let headers = config.token.iter().map(|t| ("Authorization".to_string(), format!("Bearer {t}"))).collect();
            let server = McpServer::http("leviathan", &endpoint, headers, config.call_timeout)?;
            owned = Some(("remote", Self::owned(ctx, &config, server, format!("remote {url}")), None));
        } else if config.backend == "auto" && config.auto_start && find_binary(&config.bin).is_some() {
            let (index, memory) = implicit_paths(&runtime);
            if let Some(dir) = index.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("leviathan: cannot create {}: {e}", dir.display()))?;
            }
            let server = local_server("leviathan", &bin, Some(&index), Some(&memory), &runtime, config.call_timeout)?;
            let backend = Self::owned(ctx, &config, server, format!("workspace index {}", index.display()));
            leviathan.implicit = Some(backend.clone());
            owned = Some(("implicit", backend, Some(index)));
        } else if config.backend == "auto" {
            log_debug!("leviathan", "no backend configured and {} is not on PATH", config.bin);
        }
        if let Some((name, backend, _)) = &owned {
            backends.add(ctx, name, Box::new(Shared(backend.clone())))?;
        }
        if backends.get("mcp").is_none() {
            let hub = ctx.inject::<McpHub>("mcp")?;
            backends.add(ctx, "mcp", Box::new(McpBackend::new(hub, config.server.clone(), config.prefix.clone())))?;
        }
        let indexer = owned.as_ref().and_then(|(_, backend, index)| index.clone().map(|index| (backend.clone(), index)));
        leviathan.index_tool.store(config.index_tool && indexer.is_some(), Ordering::SeqCst);
        let leviathan = Arc::new(leviathan);
        ctx.provide("leviathan", leviathan.clone())?;

        // The implicit index is Melchior's own; a configured one only while
        // it does not exist yet.
        let implicit = owned.as_ref().is_some_and(|(name, _, _)| *name == "implicit");
        let auto = match &indexer {
            Some((backend, index)) if config.auto_index => {
                let managed = implicit || !index.exists();
                let exclude = vec![runtime.home.clone()];
                Some(Arc::new(auto::AutoIndex::new(
                    leviathan.clone(),
                    backend.clone(),
                    bin.clone(),
                    index.clone(),
                    config.auto_index_min_bytes,
                    config.index_timeout,
                    managed,
                    exclude,
                )))
            }
            _ => None,
        };
        if let Some((backend, index)) = indexer
            && config.index_tool
        {
            tools::add(ctx, Box::new(tools::IndexTool { leviathan: leviathan.clone(), backend, bin, index, timeout: config.index_timeout }))?;
        }
        // A configured backend connects now, so its tools are in the load
        // report; the implicit one waits until nothing else answers.
        if let Some((name, backend, _)) = &owned
            && *name != "implicit"
            && let Err(e) = backend.connect()
        {
            log_warn!("leviathan", "{}: {e}; will retry before the next turn", backend.label());
        }

        let hooks = ctx.inject::<TurnHooks>("turn-hooks")?;
        if let Some(auto) = auto {
            let source = leviathan.clone();
            hooks.add(
                ctx,
                "leviathan/auto-index",
                Box::new(move |cx| {
                    // The implicit index only matters when nothing else answers.
                    if implicit && !source.implicit_in_use() {
                        return None;
                    }
                    auto.run_once(cx.runtime);
                    if let Some(note) = auto.note(&cx.runtime.cwd) {
                        *source.note.lock().unwrap() = Some(note);
                        source.invalidate();
                    }
                    None
                }),
            )?;
        }
        let source = leviathan.clone();
        hooks.add(
            ctx,
            "leviathan/refresh",
            Box::new(move |_cx| {
                source.refresh();
                None
            }),
        )?;
        if config.briefing {
            let prompt = ctx.inject::<PromptService>("prompt")?;
            let source = leviathan.clone();
            prompt.sections.add(ctx, "leviathan/briefing", Box::new(move |_cx| source.briefing().map(|b| b.text)))?;
        }
        if config.recall {
            let source = leviathan.clone();
            hooks.add(ctx, "leviathan/recall", Box::new(move |cx| source.recall(cx.text)))?;
        }
        Ok(())
    }
}
