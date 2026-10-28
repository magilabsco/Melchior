//! The command line, shared by every distribution.
//!
//! ```text
//! melchior [options] "task"                         one headless task, answer on stdout
//! melchior --serve [options]                        JSON-RPC over stdio
//! melchior --profile sdk [--patch FILE]...          the same, as DeepSeek Harness is launched
//! melchior --version
//!
//! options: --profile NAME  --patch FILE (repeatable)
//!          --index FILE    a Leviathan index to work over (local `leviathan` binary)
//!          --memory FILE   a Leviathan memory store to keep across runs
//! ```

use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use crate::agent::Agent;
use crate::cordis::loader::Entry;
use crate::distribution::Distribution;
use crate::rpc::Notifier;
use crate::runtime::Runtime;
use crate::server::{self, Server};
use crate::session::Session;
use crate::{jarr, jobj, json, plugins, util};

struct Args {
    profile: Option<String>,
    serve: bool,
    patches: Vec<PathBuf>,
    index: Option<String>,
    memory: Option<String>,
    task: Vec<String>,
}

fn usage(name: &str) -> String {
    format!(
        "usage: {name} [options] \"task\"\n       {name} --serve [options]\n       {name} --profile sdk [--patch FILE]...\n       {name} --version\n\n\
         options:\n  --profile NAME   plugin profile (default, minimal, ...)\n  --patch FILE     Cordis patch file, applied in order (repeatable)\n  \
         --index FILE     Leviathan index to work over\n  --memory FILE    Leviathan memory store kept across runs\n  --serve          speak JSON-RPC on stdio"
    )
}

fn parse_args(dist: &Distribution) -> Result<Option<Args>, String> {
    let mut args = Args { profile: None, serve: false, patches: Vec::new(), index: None, memory: None, task: Vec::new() };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_string(), Some(value.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| inline.clone().or_else(|| iter.next()).ok_or(format!("{flag} needs {what}"));
        match flag.as_str() {
            "--version" | "-V" => {
                println!("{} {}", dist.name, dist.version);
                return Ok(None);
            }
            "--help" | "-h" => {
                println!("{}", usage(&dist.name));
                return Ok(None);
            }
            "--serve" => args.serve = true,
            "--profile" => args.profile = Some(value("a value")?),
            "--patch" => args.patches.push(PathBuf::from(value("a file")?)),
            "--index" => args.index = Some(value("a file")?),
            "--memory" => args.memory = Some(value("a file")?),
            other if other.starts_with("--") => return Err(format!("unknown option {other}\n{}", usage(&dist.name))),
            _ => args.task.push(arg),
        }
    }
    Ok(Some(args))
}

/// Run the command line for a distribution.
pub fn main(dist: Distribution) -> ExitCode {
    util::init_logging();
    let args = match parse_args(&dist) {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", dist.name);
            return ExitCode::from(2);
        }
    };
    // DeepSeek Harness serves JSON-RPC when launched with `--profile sdk`.
    let serve = args.serve || (args.task.is_empty() && args.profile.as_deref() == Some("sdk"));
    let profile = args.profile.clone().unwrap_or_else(|| dist.default_profile.clone());
    let dist = Arc::new(dist);
    if serve {
        return serve_stdio(dist, &profile, &args);
    }
    headless(dist, &profile, &args)
}

/// Point the `leviathan` entry at the files given on the command line.
fn apply_leviathan_flags(entries: &mut [Entry], args: &Args) {
    let Some(entry) = entries.iter_mut().find(|e| e.id == "leviathan") else { return };
    if let Some(index) = &args.index {
        entry.config.set("index", index.as_str());
    }
    if let Some(memory) = &args.memory {
        entry.config.set("memory", memory.as_str());
    }
}

fn serve_stdio(dist: Arc<Distribution>, profile: &str, args: &Args) -> ExitCode {
    let entries = plugins::build_entries(&dist, profile, None, &args.patches).map(|(mut entries, warnings)| {
        apply_leviathan_flags(&mut entries, args);
        (entries, warnings)
    });
    if let Err(e) = &entries {
        // Report it on stderr now and as the initialize error later, so the
        // SDK client shows it either way.
        eprintln!("{}: {e}", dist.name);
    }
    let server = Arc::new(Server::new(dist, Arc::new(Notifier::stdout()), entries));
    server.serve(BufReader::new(io::stdin()));
    ExitCode::SUCCESS
}

fn headless(dist: Arc<Distribution>, profile: &str, args: &Args) -> ExitCode {
    let name = dist.name.clone();
    let task = args.task.join(" ");
    if task.trim().is_empty() {
        eprintln!("{name}: give a task, or use --serve\n{}", usage(&name));
        return ExitCode::from(2);
    }
    let home = server::env_home()
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(&dist.home_dir_name)))
        .unwrap_or_else(|| PathBuf::from(&dist.home_dir_name));
    let provider = std::env::var("MELCHIOR_PROVIDER").unwrap_or_else(|_| "openrouter".into());
    let model = std::env::var("MELCHIOR_MODEL").unwrap_or_else(|_| "deepseek/deepseek-chat-v3.1".into());

    let entries = dist.profile(profile).and_then(|mut entries| {
        if let Some(llm) = entries.iter_mut().find(|e| e.id == "llm-pi-ai") {
            let mut config = jobj! { "providers" => jobj! { "openrouter" => jobj! {
                "apiKeyEnv" => "OPENROUTER_API_KEY",
                "models" => jarr![model.as_str()],
            } } };
            config.deep_merge(&llm.config);
            llm.config = config;
        }
        let (mut patched, warnings) = plugins::build_entries(&dist, profile, Some(&home), &args.patches)?;
        for warning in warnings {
            eprintln!("{name}: {warning}");
        }
        // Keep the headless provider default unless settings replaced it.
        for entry in &mut patched {
            if entry.id == "llm-pi-ai"
                && let Some(base) = entries.iter().find(|e| e.id == "llm-pi-ai")
            {
                let mut config = base.config.clone();
                config.deep_merge(&entry.config);
                entry.config = config;
            }
        }
        apply_leviathan_flags(&mut patched, args);
        Ok(patched)
    });
    let entries = match entries {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("{name}: {e}");
            return ExitCode::from(2);
        }
    };

    let notifier = Arc::new(Notifier::with_sink(|line| {
        let Ok(message) = json::parse(line) else { return };
        let event = message.get("params").get("event");
        match event.get("type").as_str() {
            Some("tool/call") => eprintln!("· {} {}", event.get("data").get("name").str_or("?"), util::truncate(event.get("data").get("arguments").str_or(""), 160, "…")),
            Some("context/added") => eprintln!("· context from {}", event.get("data").get("source").str_or("?")),
            Some("assistant/retry") => eprintln!("· retrying: {}", event.get("data").get("error").str_or("")),
            _ => {}
        }
    }));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let runtime = Arc::new(Runtime::new(cwd, home.clone(), provider, model, None, None, notifier));
    let (app, report) = match server::boot(&dist, runtime, &home, &entries) {
        Ok(booted) => booted,
        Err(e) => {
            eprintln!("{name}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(agent) = app.get::<Agent>("agent") else {
        eprintln!("{name}: the agent did not start: {}", report.get("failed"));
        return ExitCode::FAILURE;
    };
    let session = Session::ephemeral(&util::new_id("headless"));
    let outcome = agent.run_turn(&session, &util::new_id("msg"), &task, 0);
    println!("{}", outcome.text);
    app.dispose_all();
    crate::proc::kill_all();
    if outcome.reason == "error" { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
