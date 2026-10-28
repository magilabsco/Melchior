//! The SDK server: JSON-RPC 2.0 over stdio, one JSON object per line.
//!
//! This is the wire contract of DeepSeek Harness's `--profile sdk`, so its
//! SDK clients drive Melchior unchanged. Requests are read on the main
//! thread; each prompt runs on its own worker thread so `session/cancel`,
//! other sessions, and `shutdown` stay responsive while a turn is running.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::agent::Agent;
use crate::cordis::App;
use crate::cordis::loader::{self, Entry};
use crate::distribution::Distribution;
use crate::json::{self, Json};
use crate::leviathan::Backends;
use crate::llm::LlmRegistry;
use crate::mcp::McpHub;
use crate::plugins;
use crate::prompt::{PromptService, TurnEndHooks, TurnHooks};
use crate::rpc::{self, Notifier, RpcError};
use crate::runtime::Runtime;
use crate::session::{SessionError, SessionStore};
use crate::tools::ToolRegistry;
use crate::{jobj, log_error, log_info, log_warn, util};

struct Booted {
    app: App,
    sessions: Arc<SessionStore>,
    agent: Arc<Agent>,
    report: Json,
}

pub struct Server {
    dist: Arc<Distribution>,
    notifier: Arc<Notifier>,
    entries: Result<Vec<Entry>, String>,
    warnings: Vec<String>,
    booted: Mutex<Option<Arc<Booted>>>,
}

fn session_error(error: SessionError) -> RpcError {
    match error {
        SessionError::Invalid(_) => RpcError::params(error.to_string()),
        other => RpcError::internal(other.to_string()),
    }
}

impl Server {
    pub fn new(dist: Arc<Distribution>, notifier: Arc<Notifier>, entries: Result<(Vec<Entry>, Vec<String>), String>) -> Server {
        let (entries, warnings) = match entries {
            Ok((entries, warnings)) => (Ok(entries), warnings),
            Err(e) => (Err(e), Vec::new()),
        };
        for warning in &warnings {
            log_warn!("loader", "{warning}");
        }
        Server { dist, notifier, entries, warnings, booted: Mutex::new(None) }
    }

    /// Serve until stdin closes or `shutdown` is received.
    pub fn serve(self: Arc<Self>, input: impl BufRead) {
        for line in input.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let message = match json::parse(&line) {
                Ok(message @ Json::Obj(_)) => message,
                Ok(_) => {
                    self.notifier.respond_error(&Json::Null, rpc::INVALID_REQUEST, "a message must be a JSON object");
                    continue;
                }
                Err(e) => {
                    self.notifier.respond_error(&Json::Null, rpc::PARSE_ERROR, &format!("parse error: {e}"));
                    continue;
                }
            };
            let id = message.get("id").clone();
            let Some(method) = message.get("method").as_str() else {
                continue; // a response to a request we never send
            };
            let params = message.get("params").clone();
            if id.is_null() {
                if method == "session/cancel" {
                    let _ = self.cancel(&params);
                }
                continue;
            }
            if method == "shutdown" {
                self.notifier.respond(&id, jobj! {});
                break;
            }
            match self.dispatch(method, &params) {
                Ok(result) => self.notifier.respond(&id, result),
                Err(error) => self.notifier.respond_error(&id, error.code, &error.message),
            }
        }
        self.stop();
    }

    fn stop(&self) {
        log_info!("server", "shutting down");
        if let Some(booted) = self.booted.lock().unwrap().take() {
            booted.app.dispose_all();
        }
        crate::proc::kill_all();
    }

    fn booted(&self) -> Result<Arc<Booted>, RpcError> {
        self.booted.lock().unwrap().clone().ok_or_else(|| RpcError::internal("the harness is not initialized; call initialize first"))
    }

    fn dispatch(self: &Arc<Self>, method: &str, params: &Json) -> Result<Json, RpcError> {
        match method {
            "initialize" => self.initialize(params),
            "session/prompt" => self.prompt(params),
            "session/resume" => self.resume(params),
            "session/cancel" => self.cancel(params),
            "session/close" => self.close(params),
            "session/list" => {
                let booted = self.booted()?;
                let ids = |v: Vec<String>| Json::Arr(v.into_iter().map(Json::from).collect());
                Ok(jobj! { "open" => ids(booted.sessions.open_ids()), "stored" => ids(booted.sessions.stored_ids()) })
            }
            "harness/plugins" => Ok(self.booted()?.report.clone()),
            "ping" => Ok(jobj! {}),
            other => Err(RpcError { code: rpc::METHOD_NOT_FOUND, message: format!("method {other:?} not found") }),
        }
    }

    fn initialize(&self, params: &Json) -> Result<Json, RpcError> {
        let mut slot = self.booted.lock().unwrap();
        if slot.is_some() {
            return Err(RpcError::internal("the harness is already initialized"));
        }
        let entries = self.entries.clone().map_err(RpcError::internal)?;
        let cwd = params.get("cwd").as_str().ok_or_else(|| RpcError::params("initialize requires cwd"))?;
        let provider = params.get("provider").as_str().ok_or_else(|| RpcError::params("initialize requires provider"))?;
        let model = params.get("model").as_str().filter(|m| !m.is_empty()).ok_or_else(|| RpcError::params("initialize requires model"))?;
        let cwd = PathBuf::from(cwd);
        if !cwd.is_dir() {
            return Err(RpcError::params(format!("cwd {} is not a directory", cwd.display())));
        }
        let home = home_dir().map_err(RpcError::internal)?;
        std::fs::create_dir_all(&home).map_err(|e| RpcError::internal(format!("cannot create the harness home {}: {e}", home.display())))?;

        let runtime = Arc::new(Runtime::new(
            cwd,
            home.clone(),
            provider.to_string(),
            model.to_string(),
            params.get("reasoningEffort").as_str().map(str::to_string),
            params.get("maxTokens").as_i64(),
            self.notifier.clone(),
        ));
        let (app, report) = boot(&self.dist, runtime.clone(), &home, &entries).map_err(RpcError::internal)?;
        let agent = app.get::<Agent>("agent").ok_or_else(|| {
            let failed: Vec<String> = report
                .get("failed")
                .items()
                .iter()
                .map(|f| format!("{}: {}", f.get("id").str_or("?"), f.get("error").str_or("?")))
                .collect();
            app.dispose_all();
            RpcError::internal(if failed.is_empty() {
                "the agent-loop plugin is not active (check the profile and patches)".to_string()
            } else {
                format!("harness plugins failed to start: {}", failed.join("; "))
            })
        })?;
        let sessions = app.get::<SessionStore>("sessions").expect("core service");
        log_info!(
            "server",
            "initialized: provider={} model={} permission={:?} tools={}",
            runtime.provider,
            runtime.model,
            runtime.permission,
            app.get::<ToolRegistry>("tools").map(|t| t.len()).unwrap_or(0)
        );
        let mut report = report;
        report.set("warnings", Json::Arr(self.warnings.iter().map(|w| Json::from(w.as_str())).collect()));
        *slot = Some(Arc::new(Booted { app, sessions, agent, report: report.clone() }));
        Ok(jobj! {
            "serverInfo" => jobj! {
                "name" => self.dist.name.as_str(),
                "version" => self.dist.version.as_str(),
                "melchior" => env!("CARGO_PKG_VERSION"),
            },
            "protocolVersion" => 1,
            "capabilities" => jobj! { "sessions" => jobj! { "resume" => true, "cancel" => true } },
            "plugins" => report,
        })
    }

    fn prompt(&self, params: &Json) -> Result<Json, RpcError> {
        let booted = self.booted()?;
        let session_id = params.get("sessionId").as_str().ok_or_else(|| RpcError::params("session/prompt requires sessionId"))?;
        if !util::valid_session_id(session_id) {
            return Err(session_error(SessionError::Invalid(session_id.to_string())));
        }
        let text = prompt_text(params.get("contentBlocks"))?;
        let session = match booted.sessions.get_open(session_id) {
            Some(session) => session,
            None if booted.sessions.exists(session_id) => {
                // The caller must resume a stored session explicitly;
                // DeepSeek Harness clients match this exact message.
                return Err(session_error(SessionError::Exists(session_id.to_string())));
            }
            None => booted.sessions.create(session_id).map_err(session_error)?,
        };
        if session.busy.swap(true, Ordering::SeqCst) {
            return Err(RpcError::internal(format!("session \"{session_id}\" is busy with another prompt")));
        }
        let message_id = util::new_id("msg");
        let worker_id = message_id.clone();
        let agent = booted.agent.clone();
        let spawned = std::thread::Builder::new().name(format!("turn-{session_id}")).spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| agent.run_turn(&session, &worker_id, &text, 0)));
            if outcome.is_err() {
                log_error!("server", "turn on {} panicked", session.id);
                agent.runtime.notifier.session_event(
                    &session.id,
                    jobj! { "type" => "turn/end", "data" => jobj! { "reason" => jobj! { "kind" => "error", "message" => "harness panic" } } },
                );
                agent.runtime.notifier.session_status(&session.id, "idle");
            }
            session.busy.store(false, Ordering::SeqCst);
        });
        if let Err(e) = spawned {
            return Err(RpcError::internal(format!("cannot start the turn: {e}")));
        }
        Ok(jobj! { "messageId" => message_id })
    }

    fn resume(&self, params: &Json) -> Result<Json, RpcError> {
        let booted = self.booted()?;
        let session_id = params.get("sessionId").as_str().ok_or_else(|| RpcError::params("session/resume requires sessionId"))?;
        let session = booted.sessions.resume(session_id).map_err(session_error)?;
        Ok(jobj! { "sessionId" => session_id, "records" => session.record_count() as i64 })
    }

    fn cancel(&self, params: &Json) -> Result<Json, RpcError> {
        let booted = self.booted()?;
        let session_id = params.get("sessionId").as_str().ok_or_else(|| RpcError::params("session/cancel requires sessionId"))?;
        let session = booted.sessions.get_open(session_id).ok_or_else(|| RpcError::internal(format!("session \"{session_id}\" is not open")))?;
        let running = session.busy.load(Ordering::SeqCst);
        session.cancel.store(true, Ordering::SeqCst);
        Ok(jobj! { "cancelled" => running })
    }

    fn close(&self, params: &Json) -> Result<Json, RpcError> {
        let booted = self.booted()?;
        let session_id = params.get("sessionId").as_str().ok_or_else(|| RpcError::params("session/close requires sessionId"))?;
        if let Some(session) = booted.sessions.get_open(session_id)
            && session.busy.load(Ordering::SeqCst)
        {
            return Err(RpcError::internal(format!("session \"{session_id}\" is busy; cancel it first")));
        }
        Ok(jobj! { "closed" => booted.sessions.close(session_id) })
    }
}

/// `MELCHIOR_HOME`, or DeepSeek Harness's `DSH_HOME`.
pub fn env_home() -> Option<PathBuf> {
    ["MELCHIOR_HOME", "DSH_HOME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|home| !home.trim().is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> Result<PathBuf, String> {
    env_home().ok_or_else(|| "MELCHIOR_HOME (or DSH_HOME) is not set; the server never picks a home directory implicitly".into())
}

/// Provide the core services and start the plugins.
pub fn boot(dist: &Distribution, runtime: Arc<Runtime>, home: &Path, entries: &[Entry]) -> Result<(App, Json), String> {
    let app = App::new();
    let root = app.root();
    root.provide("runtime", runtime)?;
    root.provide("sessions", Arc::new(SessionStore::new(home.join(&dist.sessions_dir))))?;
    root.provide("prompt", Arc::new(PromptService::default()))?;
    root.provide("tools", Arc::new(ToolRegistry::new()))?;
    root.provide("turn-hooks", Arc::new(TurnHooks::new()))?;
    root.provide("turn-end-hooks", Arc::new(TurnEndHooks::new()))?;
    root.provide("llm", Arc::new(LlmRegistry::new()))?;
    root.provide("mcp", Arc::new(McpHub::default()))?;
    root.provide("leviathan-backends", Arc::new(Backends::new()))?;
    let report = loader::start(&root, &dist.registry, entries);
    let mut json = report.to_json();
    json.set("entries", plugins::describe_entries(entries));
    if let Some(tools) = app.get::<ToolRegistry>("tools") {
        let mut names: Vec<String> = tools.all().into_iter().map(|(name, _)| name).collect();
        names.sort();
        json.set("tools", Json::Arr(names.into_iter().map(Json::from).collect()));
    }
    Ok((app, json))
}

/// Join the text of a prompt's content blocks.
pub fn prompt_text(blocks: &Json) -> Result<String, RpcError> {
    let items = blocks.as_array().ok_or_else(|| RpcError::params("contentBlocks must be an array"))?;
    let mut parts = Vec::new();
    for block in items {
        match block.get("type").as_str() {
            Some("text") => parts.push(block.get("text").str_or("").to_string()),
            Some("image") => parts.push("[the user attached an image, which this harness does not pass to the model]".into()),
            Some(other) => parts.push(format!("[unsupported {other} content omitted]")),
            None => return Err(RpcError::params("every content block needs a type")),
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return Err(RpcError::params("the prompt has no text"));
    }
    Ok(text)
}
