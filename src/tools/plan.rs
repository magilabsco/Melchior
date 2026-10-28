//! `exit_plan_mode` (plugin `plan-mode`): record a plan, then proceed.

use super::{Tool, ToolCx, ToolOutput, ToolSpec, prop, schema, str_arg, try_out};
use crate::jobj;
use crate::json::Json;
use crate::util;

pub struct ExitPlanMode;

impl Tool for ExitPlanMode {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exit_plan_mode".into(),
            description: "Present a complete plan before doing multi-step work. The plan is recorded in the session and work continues from the next step.".into(),
            parameters: schema(
                jobj! { "plan" => prop("string", "The complete plan, as markdown, starting with a # heading that names it.") },
                &["plan"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let plan = try_out!(str_arg(args, "plan"));
        if plan.trim().is_empty() {
            return ToolOutput::err("plan must not be empty");
        }
        if let Err(e) = cx.session.set_state("plan", jobj! { "plan" => plan, "recordedAt" => util::now_iso() }) {
            return ToolOutput::err(e);
        }
        ToolOutput::ok("Plan recorded. Carry it out from the next step.")
    }
}
