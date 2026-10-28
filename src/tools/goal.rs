//! Goal tracking (plugin `tool-goal`): `get_goal`, `create_goal`, `update_goal`.
//!
//! A goal is durable session state with optimistic revisions, so the model can
//! keep a multi-turn objective across restarts. The active goal is shown in
//! the system prompt.

use super::{Tool, ToolCx, ToolOutput, ToolSpec, opt_i64, opt_str, prop, schema, str_arg, try_out};
use crate::jobj;
use crate::json::Json;
use crate::util;

pub const STATE_KEY: &str = "goal";

fn render(goal: &Json) -> String {
    if goal.is_null() {
        return "No goal is set for this session.".into();
    }
    let mut text = format!(
        "Goal {} (revision {}): {}\nStatus: {}",
        goal.get("id").str_or("?"),
        goal.get("revision").as_i64().unwrap_or(0),
        goal.get("objective").str_or(""),
        goal.get("status").str_or("active"),
    );
    if let Some(max) = goal.get("maxRounds").as_i64() {
        text.push_str(&format!("\nRound cap: {max}"));
    }
    if let Some(reason) = goal.get("blockedReason").as_str() {
        text.push_str(&format!("\nBlocked: {reason}"));
    }
    text
}

pub fn prompt_section(goal: &Json) -> Option<String> {
    match goal.get("status").as_str() {
        Some("active") => Some(format!("# Current goal\n{}", render(goal))),
        _ => None,
    }
}

pub struct GetGoal;
impl Tool for GetGoal {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_goal".into(),
            description: "Return the current session goal with its id, revision, and status.".into(),
            parameters: schema(jobj! {}, &[]),
        }
    }
    fn call(&self, cx: &ToolCx<'_>, _args: &Json) -> ToolOutput {
        ToolOutput::ok(render(&cx.session.state(STATE_KEY)))
    }
}

pub struct CreateGoal;
impl Tool for CreateGoal {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "create_goal".into(),
            description: "Set a completion objective for this session when the user asks for a multi-step outcome. Fails if an unfinished goal exists.".into(),
            parameters: schema(
                jobj! {
                    "objective" => prop("string", "The concrete completion objective inferred from the direct human request."),
                    "max_rounds" => prop("integer", "Optional positive limit on continuation rounds."),
                },
                &["objective"],
            ),
        }
    }
    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let objective = try_out!(str_arg(args, "objective"));
        let current = cx.session.state(STATE_KEY);
        if matches!(current.get("status").as_str(), Some("active" | "paused")) {
            return ToolOutput::err(format!("an unfinished goal already exists:\n{}", render(&current)));
        }
        let mut goal = jobj! {
            "id" => util::new_id("goal"),
            "revision" => 1,
            "objective" => objective,
            "status" => "active",
            "createdAt" => util::now_iso(),
        };
        if let Some(max) = opt_i64(args, "max_rounds").filter(|m| *m > 0) {
            goal.set("maxRounds", max);
        }
        if let Err(e) = cx.session.set_state(STATE_KEY, goal.clone()) {
            return ToolOutput::err(e);
        }
        ToolOutput::ok(format!("Created.\n{}", render(&goal)))
    }
}

pub struct UpdateGoal;
impl Tool for UpdateGoal {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_goal".into(),
            description: "Update the exact current goal revision: edit, pause, resume, complete, or blocked (with blocked_reason).".into(),
            parameters: schema(
                jobj! {
                    "goal_id" => prop("string", "Exact id returned by get_goal."),
                    "revision" => prop("integer", "Exact revision returned by get_goal."),
                    "action" => jobj! { "type" => "string", "enum" => crate::jarr!["edit", "pause", "resume", "complete", "blocked"] },
                    "objective" => prop("string", "Replacement objective; only with action edit."),
                    "max_rounds" => prop("integer", "Replacement cap; only with action edit."),
                    "blocked_reason" => prop("string", "Concrete blocking condition; required with action blocked."),
                },
                &["goal_id", "revision", "action"],
            ),
        }
    }
    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let id = try_out!(str_arg(args, "goal_id"));
        let action = try_out!(str_arg(args, "action"));
        let revision = opt_i64(args, "revision").unwrap_or(-1);
        let mut goal = cx.session.state(STATE_KEY);
        if goal.get("id").as_str() != Some(id) {
            return ToolOutput::err(format!("goal {id:?} is not the current goal; call get_goal"));
        }
        if goal.get("revision").as_i64() != Some(revision) {
            return ToolOutput::err("stale revision; call get_goal and retry with the current revision");
        }
        let status = match action {
            "edit" => {
                if let Some(objective) = opt_str(args, "objective") {
                    goal.set("objective", objective);
                }
                if let Some(max) = opt_i64(args, "max_rounds") {
                    goal.set("maxRounds", max);
                }
                goal.get("status").str_or("active").to_string()
            }
            "pause" => "paused".into(),
            "resume" => "active".into(),
            "complete" => "complete".into(),
            "blocked" => {
                let Some(reason) = opt_str(args, "blocked_reason").filter(|r| !r.trim().is_empty()) else {
                    return ToolOutput::err("blocked requires blocked_reason");
                };
                goal.set("blockedReason", reason);
                "blocked".into()
            }
            other => return ToolOutput::err(format!("unknown action {other:?}")),
        };
        goal.set("status", status);
        goal.set("revision", revision + 1);
        goal.set("updatedAt", util::now_iso());
        if let Err(e) = cx.session.set_state(STATE_KEY, goal.clone()) {
            return ToolOutput::err(e);
        }
        ToolOutput::ok(format!("Updated.\n{}", render(&goal)))
    }
}
