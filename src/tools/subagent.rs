//! `subagent` (plugin `tool-subagent`): delegate a self-contained task to a
//! fresh agent with the same tools and an empty, in-memory conversation.

use std::sync::atomic::Ordering;

use super::{Tool, ToolCx, ToolOutput, ToolSpec, prop, schema, str_arg, try_out};
use crate::agent::Agent;
use crate::jobj;
use crate::json::Json;
use crate::session::Session;
use crate::util;

pub struct Subagent;

impl Tool for Subagent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "subagent".into(),
            description: "Delegate a self-contained task to a subagent that starts with no conversation history. Give it everything it needs in prompt; it returns its final answer.".into(),
            parameters: schema(
                jobj! {
                    "description" => prop("string", "Three to five words naming the task."),
                    "prompt" => prop("string", "Complete instructions for the subagent."),
                },
                &["prompt"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let prompt = try_out!(str_arg(args, "prompt"));
        if cx.depth >= 1 {
            return ToolOutput::err("subagents cannot start further subagents");
        }
        let Some(agent) = cx.app.get::<Agent>("agent") else {
            return ToolOutput::err("the agent service is unavailable");
        };
        let child = Session::ephemeral(&util::new_id("sub"));
        let notifier = &cx.runtime.notifier;
        let started = jobj! {
            "parentSessionId" => cx.session.id.as_str(),
            "childSessionId" => child.id.as_str(),
            "toolCallId" => cx.call_id,
            "description" => args.get("description").str_or("subagent task"),
        };
        notifier.notify("subagent.started", started);
        let cancel_watch = cx.session.cancel.load(Ordering::SeqCst);
        if cancel_watch {
            return ToolOutput::err("cancelled");
        }
        let outcome = agent.run_turn(&child, &util::new_id("msg"), prompt, cx.depth + 1);
        notifier.notify(
            "subagent.finished",
            jobj! {
                "parentSessionId" => cx.session.id.as_str(),
                "childSessionId" => child.id.as_str(),
                "reason" => outcome.reason.as_str(),
            },
        );
        if outcome.reason == "error" {
            ToolOutput::err_with_usage(outcome.text, outcome.usage)
        } else {
            ToolOutput::ok_with_usage(outcome.text, outcome.usage)
        }
    }
}
