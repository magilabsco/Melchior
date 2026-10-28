//! `todo_write` (plugin `tool-todo`): a durable task list for multi-step work.

use super::{Tool, ToolCx, ToolOutput, ToolSpec, prop, schema};
use crate::jobj;
use crate::json::Json;

pub const STATE_KEY: &str = "todos";

pub fn render(todos: &Json) -> String {
    todos
        .items()
        .iter()
        .map(|t| {
            let mark = match t.get("status").as_str() {
                Some("completed") => "[x]",
                Some("in_progress") => "[>]",
                _ => "[ ]",
            };
            format!("{mark} {}", t.get("content").str_or(""))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn prompt_section(todos: &Json) -> Option<String> {
    let open = todos.items().iter().any(|t| t.get("status").as_str() != Some("completed"));
    open.then(|| format!("# Task list\n{}", render(todos)))
}

pub struct TodoWrite;
impl Tool for TodoWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo_write".into(),
            description: "Replace the session task list. Use it for work with three or more steps; keep exactly one task in_progress while working.".into(),
            parameters: schema(
                jobj! {
                    "todos" => jobj! {
                        "type" => "array",
                        "description" => "The COMPLETE task list, replacing any previous list.",
                        "items" => schema(
                            jobj! {
                                "content" => prop("string", "What the task is: a short imperative line."),
                                "status" => jobj! { "type" => "string", "enum" => crate::jarr!["pending", "in_progress", "completed"] },
                            },
                            &["content", "status"],
                        ),
                    },
                },
                &["todos"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let Some(items) = args.get("todos").as_array() else {
            return ToolOutput::err("todos must be an array");
        };
        let mut todos = Vec::new();
        for item in items {
            let Some(content) = item.get("content").as_str().filter(|c| !c.trim().is_empty()) else {
                return ToolOutput::err("every todo needs non-empty content");
            };
            let status = item.get("status").str_or("pending");
            if !matches!(status, "pending" | "in_progress" | "completed") {
                return ToolOutput::err(format!("invalid status {status:?}"));
            }
            todos.push(jobj! { "content" => content, "status" => status });
        }
        let todos = Json::Arr(todos);
        if let Err(e) = cx.session.set_state(STATE_KEY, todos.clone()) {
            return ToolOutput::err(e);
        }
        ToolOutput::ok(if todos.items().is_empty() { "Task list cleared.".into() } else { render(&todos) })
    }
}
