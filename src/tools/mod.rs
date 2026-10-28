//! Tool registry and the built-in tool plugins.

pub mod fs;
pub mod goal;
pub mod plan;
pub mod shell;
pub mod skill;
pub mod subagent;
pub mod todo;
pub mod web;

use std::sync::Arc;

use crate::cordis::{App, Registry};
use crate::jobj;
use crate::json::Json;
use crate::runtime::Runtime;
use crate::session::Session;

pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Json,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Aggregate model usage performed inside the tool, if any. This lets the
    /// parent turn include subagent calls in its billable usage total.
    pub usage: Json,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> ToolOutput {
        ToolOutput { content: content.into(), is_error: false, usage: Json::Null }
    }
    pub fn ok_with_usage(content: impl Into<String>, usage: Json) -> ToolOutput {
        ToolOutput { content: content.into(), is_error: false, usage }
    }
    pub fn err(content: impl Into<String>) -> ToolOutput {
        ToolOutput { content: content.into(), is_error: true, usage: Json::Null }
    }
    pub fn err_with_usage(content: impl Into<String>, usage: Json) -> ToolOutput {
        ToolOutput { content: content.into(), is_error: true, usage }
    }
}

pub struct ToolCx<'a> {
    pub app: &'a App,
    pub runtime: &'a Arc<Runtime>,
    pub session: &'a Arc<Session>,
    pub call_id: &'a str,
    /// Subagent nesting depth (0 for the top-level agent).
    pub depth: usize,
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput;
}

pub type ToolRegistry = Registry<Box<dyn Tool>>;

/// OpenAI `tools` array, sorted by name for stable prompts (and prompt caching).
pub fn definitions(registry: &ToolRegistry, exclude: &[&str]) -> Vec<Json> {
    let mut tools: Vec<(String, Json)> = registry
        .all()
        .into_iter()
        .filter(|(name, _)| !exclude.contains(&name.as_str()))
        .map(|(name, tool)| {
            let spec = tool.spec();
            (
                name,
                jobj! {
                    "type" => "function",
                    "function" => jobj! {
                        "name" => spec.name,
                        "description" => spec.description,
                        "parameters" => spec.parameters,
                    },
                },
            )
        })
        .collect();
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    tools.into_iter().map(|(_, t)| t).collect()
}

pub fn register(registry: &Arc<ToolRegistry>, ctx: &crate::cordis::Context, tool: Box<dyn Tool>) -> Result<(), String> {
    let name = tool.spec().name;
    registry.add(ctx, &name, tool)
}

// ---- argument helpers ------------------------------------------------------

pub fn str_arg<'a>(args: &'a Json, key: &str) -> Result<&'a str, ToolOutput> {
    args.get(key).as_str().ok_or_else(|| ToolOutput::err(format!("missing required string argument {key:?}")))
}

pub fn opt_str<'a>(args: &'a Json, key: &str) -> Option<&'a str> {
    args.get(key).as_str()
}

pub fn opt_i64(args: &Json, key: &str) -> Option<i64> {
    args.get(key).as_i64()
}

pub fn schema(properties: Json, required: &[&str]) -> Json {
    jobj! {
        "type" => "object",
        "properties" => properties,
        "required" => Json::Arr(required.iter().map(|r| Json::from(*r)).collect()),
        "additionalProperties" => false,
    }
}

pub fn prop(kind: &str, description: &str) -> Json {
    jobj! { "type" => kind, "description" => description }
}

macro_rules! try_out {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(out) => return out,
        }
    };
}
pub(crate) use try_out;
