//! The built-in plugins and the profiles that list them.
//!
//! Entry ids match DeepSeek Harness, so its patch files (which disable
//! `tool-bash`, configure `llm-pi-ai`, insert `@deepseek-ai/dsh-mcp-client`,
//! and so on) apply to Melchior unchanged.
//!
//! Core services are provided on the root scope by the server before any
//! plugin starts: `runtime`, `sessions`, `prompt`, `tools`, `turn-hooks`,
//! `turn-end-hooks`, `llm`, `mcp`, and `leviathan-backends`. Plugins
//! contribute to them; `agent-loop` combines them into the `agent` service
//! that runs turns.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::{Agent, AgentConfig};
use crate::cordis::loader::{Entry, PluginRegistry};
use crate::cordis::{Context, Plugin};
use crate::distribution::Distribution;
use crate::json::Json;
use crate::leviathan::LeviathanPlugin;
use crate::leviathan::memory::LeviathanMemoryPlugin;
use crate::llm::{self, LlmRegistry, OpenAiCompatible, Provider};
use crate::mcp::{McpHub, McpServer, McpTool};
use crate::prompt::{PromptService, TurnHooks};
use crate::runtime::Runtime;
use crate::tools::shell::{Bash, JobKill, JobList, JobOutput, Jobs};
use crate::tools::{self, ToolRegistry, fs, goal, plan, skill, subagent, todo, web};
use crate::{log_info, log_warn, util};

pub const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
pub const DEEPSEEK_BASE: &str = "https://api.deepseek.com";

/// The built-in profiles. Leviathan comes right after the model providers,
/// so its briefing sits near the top of the system prompt, ahead of the
/// sections that change during a conversation (goal, task list).
/// `mcp-client` entries are normally inserted by a patch.
pub fn profiles() -> Vec<(String, Vec<Entry>)> {
    let default = vec![
        Entry::new("system-prompt"),
        Entry::new("llm-pi-ai"),
        Entry::new("llm-deepseek"),
        Entry::new("leviathan"),
        Entry::new("leviathan-memory"),
        Entry::new("tool-goal"),
        Entry::new("tool-todo"),
        Entry::new("plan-mode"),
        Entry::new("tool-fs"),
        Entry::new("tool-str-replace-editor"),
        Entry::new("tool-fs-search"),
        Entry::new("tool-jobs"),
        Entry::new("tool-bash"),
        Entry::new("tool-web"),
        Entry::new("tool-skill"),
        Entry::new("tool-subagent"),
        Entry::new("agent-loop"),
    ];
    let minimal = vec![Entry::new("system-prompt"), Entry::new("llm-pi-ai"), Entry::new("llm-deepseek"), Entry::new("agent-loop")];
    vec![
        ("default".into(), default.clone()),
        // DeepSeek Harness's names for the same set.
        ("sdk".into(), default.clone()),
        ("headless".into(), default),
        ("minimal".into(), minimal),
    ]
}

pub fn registry() -> PluginRegistry {
    let mut registry = PluginRegistry::default();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(SystemPrompt),
        Arc::new(LlmPiAi),
        Arc::new(LlmDeepSeek),
        Arc::new(LeviathanPlugin),
        Arc::new(LeviathanMemoryPlugin),
        Arc::new(ToolGoal),
        Arc::new(ToolTodo),
        Arc::new(PlanMode),
        Arc::new(ToolFs),
        Arc::new(ToolStrReplaceEditor),
        Arc::new(ToolFsSearch),
        Arc::new(ToolJobs),
        Arc::new(ToolBash),
        Arc::new(ToolWeb),
        Arc::new(ToolSkill),
        Arc::new(ToolSubagent),
        Arc::new(McpClient),
        Arc::new(AgentLoop),
    ];
    for plugin in plugins {
        registry.register(plugin);
    }
    registry.alias("dsh-mcp-client", "mcp-client");
    registry.alias("@deepseek-ai/dsh-mcp-client", "mcp-client");
    registry
}

fn add_tool(ctx: &Context, tool: Box<dyn tools::Tool>) -> Result<(), String> {
    let registry = ctx.inject::<ToolRegistry>("tools")?;
    tools::register(&registry, ctx, tool)
}

// ---- system-prompt ----------------------------------------------------------

/// Sets the persona (`personaPrefix`, or `persona`) and adds the environment
/// section. A host usually passes the agent's prompt through a patch, as
/// `personaPrefix: !!js process.env.DSH_SYSTEM_PROMPT`.
pub struct SystemPrompt;

impl Plugin for SystemPrompt {
    fn name(&self) -> &'static str {
        "system-prompt"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["prompt", "runtime"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let prompt = ctx.inject::<PromptService>("prompt")?;
        let config = ctx.config();
        let persona = config.get("personaPrefix").as_str().or(config.get("persona").as_str()).unwrap_or("");
        prompt.set_persona(persona);
        let restore = prompt.clone();
        ctx.effect(move || restore.set_persona(""));
        if config.get("environment").as_bool() != Some(false) {
            prompt.sections.add(ctx, "harness/environment", Box::new(environment_section))?;
        }
        Ok(())
    }
}

fn environment_section(cx: &crate::prompt::PromptCx<'_>) -> Option<String> {
    let mut text = format!("# Environment\nToday is {}.", util::today_long());
    if cx.tool_names.iter().any(|t| t == "read" || t == "bash") {
        text.push_str(&format!(
            "\nWorkspace directory: {}{}",
            cx.runtime.cwd.display(),
            if cx.runtime.full_access() { " (full access: commands and files are not sandboxed)." } else { " (file and shell access is confined to it)." }
        ));
    }
    text.push_str("\nTool calls run in order; when several independent lookups are needed, request them in the same response.");
    Some(text)
}

// ---- LLM providers ----------------------------------------------------------

/// `llm-pi-ai`: OpenAI-compatible providers from `providers.<id>` (the name
/// is DeepSeek Harness's; it reads the same keys: `baseURL`, `apiKeyEnv`,
/// `api`, `models`, `headers`).
pub struct LlmPiAi;

impl Plugin for LlmPiAi {
    fn name(&self) -> &'static str {
        "llm-pi-ai"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["llm", "runtime"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let registry = ctx.inject::<LlmRegistry>("llm")?;
        let runtime = ctx.inject::<Runtime>("runtime")?;
        for (id, config) in ctx.config().get("providers").entries() {
            let api = config.get("api").str_or("openai-completions");
            if api != "openai-completions" && api != "openai" {
                return Err(format!("llm-pi-ai: provider {id:?} uses api {api:?}; only openai-completions is supported"));
            }
            let (base, key_env) = if id == "openrouter" { (OPENROUTER_BASE, "OPENROUTER_API_KEY") } else { ("", "DEEPSEEK_API_KEY") };
            let provider = OpenAiCompatible::from_config(id, config, base, key_env);
            if provider.base_url.is_empty() {
                return Err(format!("llm-pi-ai: provider {id:?} needs baseURL"));
            }
            if let Some(env) = &provider.api_key_env {
                runtime.add_secret_env(env);
            }
            registry.add(ctx, id, Box::new(provider) as Box<dyn Provider>)?;
        }
        Ok(())
    }
}

/// `llm-deepseek`: the official DeepSeek API as provider `deepseek-official`.
pub struct LlmDeepSeek;

impl Plugin for LlmDeepSeek {
    fn name(&self) -> &'static str {
        "llm-deepseek"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let registry = ctx.inject::<LlmRegistry>("llm")?;
        let mut config = ctx.config().clone();
        if config.get("baseURL").is_null()
            && let Ok(base) = std::env::var("DEEPSEEK_BASE_URL")
            && !base.is_empty()
        {
            config.set("baseURL", base);
        }
        let provider = OpenAiCompatible::from_config("deepseek-official", &config, DEEPSEEK_BASE, "DEEPSEEK_API_KEY");
        registry.add(ctx, "deepseek-official", Box::new(provider) as Box<dyn Provider>)
    }
}

// ---- agent-loop --------------------------------------------------------------

pub struct AgentLoop;

impl Plugin for AgentLoop {
    fn name(&self) -> &'static str {
        "agent-loop"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["runtime", "llm", "tools", "prompt", "turn-hooks", "turn-end-hooks"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let runtime = ctx.inject::<Runtime>("runtime")?;
        let registry = ctx.inject::<LlmRegistry>("llm")?;
        let provider = llm::select(&registry, &runtime.provider).ok_or_else(|| {
            let known: Vec<String> = registry.all().into_iter().map(|(id, _)| id).collect();
            format!("no LLM provider {:?} is configured (configured: {})", runtime.provider, if known.is_empty() { "none".into() } else { known.join(", ") })
        })?;
        let agent = Agent {
            app: ctx.app().clone(),
            runtime,
            provider,
            tools: ctx.inject("tools")?,
            prompt: ctx.inject("prompt")?,
            hooks: ctx.inject("turn-hooks")?,
            end_hooks: ctx.inject("turn-end-hooks")?,
            config: AgentConfig::from_json(ctx.config()),
        };
        ctx.provide("agent", Arc::new(agent))
    }
}

// ---- session-state tools -----------------------------------------------------

pub struct ToolGoal;

impl Plugin for ToolGoal {
    fn name(&self) -> &'static str {
        "tool-goal"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "prompt"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(goal::GetGoal))?;
        add_tool(ctx, Box::new(goal::CreateGoal))?;
        add_tool(ctx, Box::new(goal::UpdateGoal))?;
        let prompt = ctx.inject::<PromptService>("prompt")?;
        prompt.sections.add(ctx, "goal", Box::new(|cx| goal::prompt_section(&cx.session.state(goal::STATE_KEY))))
    }
}

pub struct ToolTodo;

impl Plugin for ToolTodo {
    fn name(&self) -> &'static str {
        "tool-todo"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "prompt"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(todo::TodoWrite))?;
        let prompt = ctx.inject::<PromptService>("prompt")?;
        prompt.sections.add(ctx, "todo", Box::new(|cx| todo::prompt_section(&cx.session.state(todo::STATE_KEY))))
    }
}

pub struct PlanMode;

impl Plugin for PlanMode {
    fn name(&self) -> &'static str {
        "plan-mode"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(plan::ExitPlanMode))
    }
}

// ---- workspace tools -----------------------------------------------------------

pub struct ToolFs;

impl Plugin for ToolFs {
    fn name(&self) -> &'static str {
        "tool-fs"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(fs::Read))?;
        add_tool(ctx, Box::new(fs::Write))
    }
}

pub struct ToolStrReplaceEditor;

impl Plugin for ToolStrReplaceEditor {
    fn name(&self) -> &'static str {
        "tool-str-replace-editor"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(fs::Edit))
    }
}

pub struct ToolFsSearch;

impl Plugin for ToolFsSearch {
    fn name(&self) -> &'static str {
        "tool-fs-search"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(fs::Glob))?;
        add_tool(ctx, Box::new(fs::Grep))
    }
}

/// `tool-jobs`: provides the `jobs` service and the job tools; disposing the
/// scope kills every background job.
pub struct ToolJobs;

impl Plugin for ToolJobs {
    fn name(&self) -> &'static str {
        "tool-jobs"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let jobs = Arc::new(Jobs::default());
        ctx.provide("jobs", jobs.clone())?;
        let killer = jobs.clone();
        ctx.effect(move || killer.kill_all());
        add_tool(ctx, Box::new(JobList(jobs.clone())))?;
        add_tool(ctx, Box::new(JobOutput(jobs.clone())))?;
        add_tool(ctx, Box::new(JobKill(jobs)))
    }
}

/// `tool-bash`: offers `run_in_background` only when `tool-jobs` is active.
pub struct ToolBash;

impl Plugin for ToolBash {
    fn name(&self) -> &'static str {
        "tool-bash"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let (jobs, background) = match ctx.get::<Jobs>("jobs") {
            Some(jobs) => (jobs, true),
            None => (Arc::new(Jobs::default()), false),
        };
        add_tool(ctx, Box::new(Bash { jobs, background }))
    }
}

pub struct ToolWeb;

impl Plugin for ToolWeb {
    fn name(&self) -> &'static str {
        "tool-web"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(web::WebFetch))
    }
}

pub struct ToolSkill;

impl Plugin for ToolSkill {
    fn name(&self) -> &'static str {
        "tool-skill"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "prompt", "runtime"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let runtime = ctx.inject::<Runtime>("runtime")?;
        let mut dirs = vec![runtime.home.join("skills"), runtime.cwd.join(".skills")];
        dirs.extend(ctx.config().get("dirs").items().iter().filter_map(|d| d.as_str()).map(|d| runtime.resolve_path(d)));
        add_tool(ctx, Box::new(skill::SkillTool { dirs: dirs.clone() }))?;
        let prompt = ctx.inject::<PromptService>("prompt")?;
        let dirs: Vec<PathBuf> = dirs;
        prompt.sections.add(ctx, "skills", Box::new(move |_cx| skill::prompt_section(&skill::discover(&dirs))))
    }
}

pub struct ToolSubagent;

impl Plugin for ToolSubagent {
    fn name(&self) -> &'static str {
        "tool-subagent"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        add_tool(ctx, Box::new(subagent::Subagent))
    }
}

// ---- mcp-client ------------------------------------------------------------------

/// `mcp-client` (alias `@deepseek-ai/dsh-mcp-client`): one entry per server,
/// over streamable HTTP or stdio.
///
/// Tools are listed at startup. If the server is unreachable then, the entry
/// stays active and retries before each turn (at most every 15 seconds), so a
/// harness started while its host is still booting picks its tools up later.
pub struct McpClient;

impl Plugin for McpClient {
    fn name(&self) -> &'static str {
        "mcp-client"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "mcp", "prompt", "turn-hooks", "runtime"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let runtime = ctx.inject::<Runtime>("runtime")?;
        let server = Arc::new(McpServer::from_config(ctx.config(), &runtime)?);
        let hub = ctx.inject::<McpHub>("mcp")?;
        hub.add(server.clone())?;
        let (owner, name, closing) = (hub.clone(), server.name.clone(), server.clone());
        ctx.effect(move || {
            owner.remove(&name);
            closing.close();
        });

        let attempt = Arc::new(Mutex::new(()));
        let last_try = Arc::new(AtomicU64::new(0));
        let connect = {
            let (server, ctx, attempt, last_try) = (server.clone(), ctx.clone(), attempt.clone(), last_try.clone());
            move || {
                let _one = attempt.lock().unwrap();
                if server.connected.load(Ordering::SeqCst) {
                    return;
                }
                last_try.store(util::unix_millis(), Ordering::SeqCst);
                match server.connect() {
                    Ok(()) => {
                        let tools = server.tools.lock().unwrap().clone();
                        for raw in tools {
                            if let Err(e) = add_tool(&ctx, Box::new(McpTool { server: server.clone(), raw })) {
                                log_warn!("mcp", "{}: {e}", server.name);
                            }
                        }
                        log_info!("mcp", "{}: {} tools", server.name, server.tools.lock().unwrap().len());
                    }
                    Err(e) => log_warn!("mcp", "{e}; will retry before the next turn"),
                }
            }
        };
        connect();

        let hooks = ctx.inject::<TurnHooks>("turn-hooks")?;
        let (retry_server, retry_last) = (server.clone(), last_try);
        hooks.add(
            ctx,
            &format!("mcp/{}/reconnect", server.name),
            Box::new(move |_cx| {
                if !retry_server.connected.load(Ordering::SeqCst)
                    && util::unix_millis().saturating_sub(retry_last.load(Ordering::SeqCst)) >= 15_000
                {
                    connect();
                }
                None
            }),
        )?;

        let prompt = ctx.inject::<PromptService>("prompt")?;
        let names_server = server.clone();
        prompt.sections.add(
            ctx,
            &format!("mcp/{}/names", server.name),
            Box::new(move |_cx| tool_name_section(&names_server)),
        )
    }
}

/// Agent prompts name gateway tools as `files.list`; the model calls them by
/// their function names. Spell out the mapping once.
fn tool_name_section(server: &McpServer) -> Option<String> {
    let tools = server.tools.lock().unwrap();
    let renamed: Vec<String> =
        tools.iter().filter(|t| t.public_name != t.name).map(|t| format!("{} → {}", t.name, t.public_name)).collect();
    if renamed.is_empty() {
        return None;
    }
    Some(format!(
        "# Tool names\nTools that instructions refer to by dotted name are available as these functions:\n{}",
        renamed.join("\n")
    ))
}

/// Patch files and settings applied in order: `<home>/settings.yaml` and
/// `<home>/cordis.patch.yml` (headless only), then each `--patch` file.
pub fn build_entries(
    dist: &Distribution,
    profile_name: &str,
    home: Option<&std::path::Path>,
    patches: &[PathBuf],
) -> Result<(Vec<Entry>, Vec<String>), String> {
    let mut entries = dist.profile(profile_name)?;
    let mut warnings = Vec::new();
    if let Some(home) = home {
        for file in ["settings.yaml", "cordis.patch.yml"] {
            let path = home.join(file);
            if path.is_file() {
                let patch = crate::cordis::loader::read_patch_file(&path)?;
                warnings.extend(crate::cordis::loader::apply_patch(&mut entries, &patch, &path.display().to_string())?);
            }
        }
    }
    for path in patches {
        let patch = crate::cordis::loader::read_patch_file(path)?;
        warnings.extend(crate::cordis::loader::apply_patch(&mut entries, &patch, &path.display().to_string())?);
    }
    Ok((entries, warnings))
}

pub fn describe_entries(entries: &[Entry]) -> Json {
    Json::Arr(
        entries
            .iter()
            .map(|e| crate::jobj! { "id" => e.id.as_str(), "name" => e.name.as_str(), "disabled" => e.disabled })
            .collect(),
    )
}
