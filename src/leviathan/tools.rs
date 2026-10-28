//! Leviathan as first-class tools.
//!
//! When Melchior connects to Leviathan itself (the `local` and `remote`
//! backends), Leviathan's own tools are registered under stable names:
//! `leviathan_search`, `leviathan_get`, `leviathan_describe`,
//! `leviathan_resolve_group`, and `memory_recall`, `memory_remember`,
//! `memory_forget`, `memory_describe`. Their descriptions and schemas are
//! Leviathan's, so they describe the data that is actually indexed.
//!
//! With a local index, `leviathan_index` builds or refreshes it from files
//! the agent can reach, so a dataset of any size becomes searchable in one
//! call instead of being read or grepped.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Leviathan, OwnedBackend, is_memory_tool};
use crate::cordis::Context;
use crate::json::Json;
use crate::mcp::{McpServer, RawTool};
use crate::tools::fs::checked_path;
use crate::tools::{self, Tool, ToolCx, ToolOutput, ToolRegistry, ToolSpec, prop, schema};
use crate::{jobj, log_warn, util};

pub const INDEX_TOOL: &str = "leviathan_index";

/// The function name for one of Leviathan's tools.
pub fn public_name(tool: &str) -> String {
    if tool.starts_with("memory_") {
        tool.to_string()
    } else if is_memory_tool(tool) {
        format!("memory_{tool}")
    } else {
        format!("leviathan_{tool}")
    }
}

pub fn add(ctx: &Context, tool: Box<dyn Tool>) -> Result<(), String> {
    let registry = ctx.inject::<ToolRegistry>("tools")?;
    tools::register(&registry, ctx, tool)
}

/// Register every tool the server lists that is not registered yet (a
/// server lists data tools only once an index exists).
pub fn register_server_tools(ctx: &Context, server: &Arc<McpServer>, namespace: Option<String>) {
    let Ok(registry) = ctx.inject::<ToolRegistry>("tools") else { return };
    let listed = server.tools.lock().unwrap().clone();
    for raw in listed {
        let name = public_name(&raw.name);
        if registry.get(&name).is_some() {
            continue;
        }
        let tool = LeviathanTool { server: server.clone(), raw, name: name.clone(), namespace: namespace.clone() };
        if let Err(e) = tools::register(&registry, ctx, Box::new(tool)) {
            log_warn!("leviathan", "{name}: {e}");
        }
    }
}

/// One of Leviathan's tools, answered in Leviathan's compact text.
pub struct LeviathanTool {
    server: Arc<McpServer>,
    raw: RawTool,
    name: String,
    namespace: Option<String>,
}

impl Tool for LeviathanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name.clone(), description: self.raw.description.clone(), parameters: self.raw.input_schema.clone() }
    }

    fn call(&self, _cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let mut args = match args {
            Json::Obj(_) => args.clone(),
            _ => Json::obj(),
        };
        if is_memory_tool(&self.raw.name)
            && let Some(ns) = &self.namespace
        {
            args.set("ns", ns.as_str());
        }
        self.server.call(&self.raw.name, &args)
    }
}

/// `leviathan_index`: build or refresh the local index from files.
pub struct IndexTool {
    pub leviathan: Arc<Leviathan>,
    pub backend: Arc<OwnedBackend>,
    pub bin: PathBuf,
    pub index: PathBuf,
    pub timeout: Duration,
}

/// Flags that take one value each, by argument name.
const SINGLE: &[(&str, &str)] = &[("id", "--id"), ("format", "--format"), ("sql", "--sql")];
/// Flags that may repeat, by argument name.
const MULTI: &[(&str, &str)] = &[
    ("title", "--title"),
    ("text", "--text"),
    ("group", "--group"),
    ("group_name", "--group-name"),
    ("date", "--date"),
    ("filter", "--filter"),
    ("display", "--display"),
];

fn strings(value: &Json) -> Vec<String> {
    match value {
        Json::Str(s) => vec![s.clone()],
        other => other.items().iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
    }
}

impl Tool for IndexTool {
    fn spec(&self) -> ToolSpec {
        let list = |what: &str| jobj! { "type" => "array", "items" => jobj! { "type" => "string" }, "description" => what };
        ToolSpec {
            name: INDEX_TOOL.into(),
            description: "Index data into Leviathan so it can be searched at any size: CSV/TSV, JSON/JSONL, SQLite, or a \
                directory of them. Use it before working through any dataset too large to read whole (more than a few \
                hundred lines), then answer from leviathan_search instead of reading or grepping the files. Rebuilds \
                atomically and skips sources that did not change. Map fields so searches can be scoped and filtered: a \
                unique `id`, a `title`, a `group` (customer, machine, account...), a `date`, and exact-match `filter` \
                fields. Fields are paths such as `a.b` or `items[].name`. Indexing replaces the previous contents of \
                the index, so pass every source that should stay searchable."
                .into(),
            parameters: schema(
                jobj! {
                    "paths" => list("Files or directories, relative to the workspace."),
                    "id" => prop("string", "Unique id field."),
                    "title" => list("Title field(s); the first present wins."),
                    "text" => list("Fields to search (default: every string field)."),
                    "group" => list("Group field for scoped search."),
                    "group_name" => list("Human-readable group name field."),
                    "date" => list("Date field(s); the first present wins."),
                    "filter" => list("Exact-match filter fields."),
                    "display" => list("Fields shown on result cards."),
                    "format" => jobj! { "type" => "string", "enum" => crate::jarr!["auto", "jsonl", "json", "csv", "tsv", "sqlite"] },
                    "sql" => prop("string", "Query for a SQLite source."),
                    "config" => prop("string", "A leviathan.toml mapping to use instead of the field arguments."),
                    "force" => prop("boolean", "Rebuild even when nothing changed."),
                },
                &["paths"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let paths = strings(args.get("paths"));
        if paths.is_empty() {
            return ToolOutput::err("paths must list at least one file or directory");
        }
        let mut command = Command::new(&self.bin);
        command.arg("index");
        for raw in &paths {
            match checked_path(cx, raw) {
                Ok(path) => command.arg(path),
                Err(out) => return out,
            };
        }
        command.arg("--index").arg(&self.index);
        if let Some(config) = args.get("config").as_str() {
            match checked_path(cx, config) {
                Ok(path) => command.arg("--config").arg(path),
                Err(out) => return out,
            };
        }
        for (key, flag) in SINGLE {
            if let Some(value) = args.get(key).as_str().filter(|v| !v.is_empty()) {
                command.arg(flag).arg(value);
            }
        }
        for (key, flag) in MULTI {
            for value in strings(args.get(key)) {
                command.arg(flag).arg(value);
            }
        }
        if args.get("force").as_bool() == Some(true) {
            command.arg("--force");
        }
        command
            .env_clear()
            .envs(cx.runtime.child_env())
            .current_dir(&cx.runtime.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let output = match run(command, self.timeout) {
            Ok(output) => output,
            Err(e) => return ToolOutput::err(format!("leviathan index failed: {e}")),
        };
        let text = util::truncate(output.1.trim(), 4_000, "…").to_string();
        if !output.0 {
            return ToolOutput::err(format!("leviathan index failed:\n{text}"));
        }
        // The server lists search tools only once an index exists, and
        // caches what it described; restart it and drop the briefing.
        self.leviathan.invalidate();
        if let Err(e) = self.backend.connect() {
            return ToolOutput::err(format!("indexed, but Leviathan did not restart: {e}\n{text}"));
        }
        let records = self.leviathan.briefing().map(|b| b.record_count).unwrap_or(0);
        ToolOutput::ok(format!(
            "{text}\n\nIndexed in {:.1}s; {records} records are searchable now with leviathan_search.",
            started.elapsed().as_secs_f64()
        ))
    }
}

/// Run to completion within `timeout`: (succeeded, stdout + stderr).
pub(super) fn run(mut command: Command, timeout: Duration) -> Result<(bool, String), String> {
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read = |stream: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_string(&mut text);
            }
            text
        })
    };
    let out = read(stdout.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let err = read(stderr.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let child = crate::proc::track(child);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.lock().unwrap().try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() > deadline {
            let mut child = child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut text = out.join().unwrap_or_default();
    let errors = err.join().unwrap_or_default();
    if !errors.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&errors);
    }
    Ok((status.success(), text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_names_are_stable() {
        assert_eq!(public_name("search"), "leviathan_search");
        assert_eq!(public_name("resolve_group"), "leviathan_resolve_group");
        assert_eq!(public_name("remember"), "memory_remember");
        assert_eq!(public_name("memory_describe"), "memory_describe");
    }

    #[test]
    fn index_arguments_accept_strings_or_lists() {
        assert_eq!(strings(&Json::from("a")), vec!["a".to_string()]);
        assert_eq!(strings(&crate::jarr!["a", "b"]), vec!["a".to_string(), "b".to_string()]);
        assert!(strings(&Json::Null).is_empty());
    }
}
