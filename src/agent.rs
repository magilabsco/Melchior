//! The agent loop: one user message in, model/tool rounds, one answer out.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::cordis::App;
use crate::json::{self, Json};
use crate::llm::{ChatOutput, ChatRequest, Delta, Provider};
use crate::prompt::{PromptCx, PromptService, TurnCx, TurnEndCx, TurnEndHooks, TurnHooks};
use crate::runtime::Runtime;
use crate::session::Session;
use crate::tools::{self, ToolCx, ToolOutput, ToolRegistry};
use crate::util;
use crate::{jarr, jobj};

#[derive(Clone)]
pub struct AgentConfig {
    pub max_rounds: usize,
    pub context_chars: usize,
    pub tool_output_chars: usize,
    pub stream_deltas: bool,
}

impl AgentConfig {
    pub fn from_json(config: &Json) -> AgentConfig {
        AgentConfig {
            max_rounds: config.get("maxRounds").as_i64().unwrap_or(40).clamp(1, 500) as usize,
            context_chars: config.get("contextChars").as_i64().unwrap_or(600_000).max(20_000) as usize,
            tool_output_chars: config.get("toolOutputChars").as_i64().unwrap_or(40_000).max(1_000) as usize,
            stream_deltas: config.get("streamDeltas").as_bool().unwrap_or(true),
        }
    }
}

pub struct Agent {
    pub app: App,
    pub runtime: Arc<Runtime>,
    pub provider: Arc<Box<dyn Provider>>,
    pub tools: Arc<ToolRegistry>,
    pub prompt: Arc<PromptService>,
    pub hooks: Arc<TurnHooks>,
    pub end_hooks: Arc<TurnEndHooks>,
    pub config: AgentConfig,
}

pub struct TurnOutcome {
    pub text: String,
    pub reason: String,
    pub usage: Json,
}

const INTERRUPTED: &str = "This tool call was interrupted before it finished (the harness restarted). It may or may not have taken effect.";
const FINAL_NUDGE: &str = "(harness) Stop calling tools and write your final answer to the user now, using what you have found so far. If something is still unknown, say so.";

impl Agent {
    fn emit(&self, session: &Session, kind: &str, data: Json) {
        self.runtime.notifier.session_event(
            &session.id,
            jobj! { "id" => util::new_id("evt"), "type" => kind, "ts" => util::now_iso(), "data" => data },
        );
    }

    fn persist(&self, session: &Session, kind: &str, data: Json) -> Result<Json, String> {
        session.append(kind, data)
    }

    /// Run one user turn to completion. The caller has marked the session busy.
    pub fn run_turn(&self, session: &Arc<Session>, message_id: &str, text: &str, depth: usize) -> TurnOutcome {
        session.cancel.store(false, Ordering::SeqCst);
        self.runtime.notifier.session_status(&session.id, "running");
        let outcome = self.turn(session, message_id, text, depth);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                util::log(util::Level::Error, "agent", &format!("{}: turn failed: {error}", session.id));
                let text = format!("I couldn't finish this answer because the agent runtime hit an error: {error}");
                self.emit(session, "assistant/message", assistant_event(&util::new_id("msg"), &text, &[], true));
                let _ = self.persist(session, "turn/end", jobj! { "reason" => jobj! { "kind" => "error", "message" => error.as_str() } });
                self.emit(session, "turn/end", jobj! { "reason" => jobj! { "kind" => "error", "message" => error } });
                TurnOutcome { text, reason: "error".into(), usage: Json::Null }
            }
        };
        self.runtime.notifier.session_status(&session.id, "idle");
        outcome
    }

    fn turn(&self, session: &Arc<Session>, message_id: &str, text: &str, depth: usize) -> Result<TurnOutcome, String> {
        self.persist(session, "message/user", jobj! { "id" => message_id, "content" => text })?;
        self.emit(
            session,
            "agent/inbox/spliced",
            jobj! { "inserted" => jarr![jobj! {
                "id" => message_id,
                "role" => "user",
                "content" => jarr![jobj! { "type" => "text", "text" => text }],
            }] },
        );
        if depth == 0 {
            for (key, hook) in self.hooks.all() {
                let cx = TurnCx { app: &self.app, runtime: &self.runtime, session, message_id, text };
                let produced = catch_unwind(AssertUnwindSafe(|| hook(&cx))).unwrap_or_else(|_| {
                    util::log(util::Level::Error, "agent", &format!("turn hook {key} panicked"));
                    None
                });
                if let Some(context) = produced {
                    self.persist(
                        session,
                        "message/context",
                        jobj! { "messageId" => message_id, "source" => context.source.as_str(), "text" => context.text.as_str() },
                    )?;
                    self.emit(session, "context/added", jobj! { "source" => context.source, "chars" => context.text.len() });
                }
            }
        }

        let mut usage = Usage::default();
        let mut rounds = 0;
        let mut final_text = String::new();
        let mut reason = "stop".to_string();
        let mut nudged = false;
        let exclude: Vec<&str> = if depth > 0 { vec!["subagent"] } else { vec![] };
        let mut tool_defs = tools::definitions(&self.tools, &exclude);
        let mut tool_count = self.tools.len();
        loop {
            if session.cancel.load(Ordering::SeqCst) {
                reason = "cancelled".into();
                break;
            }
            // A tool call can add tools (indexing data makes search available).
            if self.tools.len() != tool_count {
                tool_defs = tools::definitions(&self.tools, &exclude);
                tool_count = self.tools.len();
            }
            let force_answer = nudged || rounds >= self.config.max_rounds;
            let mut output = self.call_model(session, &tool_defs, force_answer)?;
            usage.add(&output.usage);
            if force_answer {
                output.tool_calls.clear();
            }
            let calls: Vec<Json> = output
                .tool_calls
                .iter()
                .map(|c| jobj! { "id" => c.id.as_str(), "name" => c.name.as_str(), "arguments" => c.arguments.as_str() })
                .collect();
            let assistant_id = util::new_id("msg");
            let mut data = jobj! {
                "id" => assistant_id.as_str(),
                "text" => output.text.as_str(),
                "toolCalls" => Json::Arr(calls.clone()),
                "model" => output.model.as_str(),
                "finishReason" => output.finish_reason.as_str(),
                "usage" => output.usage.clone(),
            };
            if !output.reasoning_details.is_empty() {
                data.set("reasoningDetails", Json::Arr(output.reasoning_details.clone()));
            }
            self.persist(session, "message/assistant", data)?;
            self.emit(session, "assistant/message", assistant_event(&assistant_id, &output.text, &calls, false));
            if !output.text.trim().is_empty() {
                final_text = output.text.clone();
            }
            if output.tool_calls.is_empty() || force_answer {
                if force_answer && !nudged {
                    reason = "max_rounds".into();
                } else if !force_answer && final_text.trim().is_empty() && rounds > 0 {
                    // The model ended on tool output without answering; ask once.
                    nudged = true;
                    continue;
                }
                break;
            }
            for call in &output.tool_calls {
                if session.cancel.load(Ordering::SeqCst) {
                    break;
                }
                self.emit(session, "tool/call", jobj! { "id" => call.id.as_str(), "name" => call.name.as_str(), "arguments" => call.arguments.as_str() });
                let started = util::unix_millis();
                let result = self.execute(session, &call.id, &call.name, &call.arguments, depth);
                usage.add_aggregate(&result.usage);
                let content = util::truncate(
                    &result.content,
                    self.config.tool_output_chars,
                    "\n…[output truncated by the harness; narrow the request to see more]",
                );
                self.persist(
                    session,
                    "tool/result",
                    jobj! { "toolCallId" => call.id.as_str(), "name" => call.name.as_str(), "content" => content.as_str(), "isError" => result.is_error },
                )?;
                self.emit(
                    session,
                    "tool/result",
                    jobj! {
                        "id" => call.id.as_str(),
                        "name" => call.name.as_str(),
                        "isError" => result.is_error,
                        "durationMs" => util::unix_millis().saturating_sub(started),
                        "content" => util::truncate(&content, 2_000, "…"),
                    },
                );
            }
            rounds += 1;
        }
        if final_text.trim().is_empty() && reason == "stop" {
            final_text = "I wasn't able to produce an answer for that. Please try rephrasing the question.".into();
            self.emit(session, "assistant/message", assistant_event(&util::new_id("msg"), &final_text, &[], true));
        }
        let usage = usage.to_json();
        self.persist(session, "turn/end", jobj! { "reason" => jobj! { "kind" => reason.as_str() }, "usage" => usage.clone() })?;
        self.emit(session, "turn/end", jobj! { "reason" => jobj! { "kind" => reason.as_str() }, "usage" => usage.clone() });
        if depth == 0 {
            let cx = TurnEndCx { session, message_id, text, answer: &final_text, reason: &reason };
            for (key, hook) in self.end_hooks.all() {
                if catch_unwind(AssertUnwindSafe(|| hook(&cx))).is_err() {
                    util::log(util::Level::Error, "agent", &format!("turn-end hook {key} panicked"));
                }
            }
        }
        Ok(TurnOutcome { text: final_text, reason, usage })
    }

    fn call_model(&self, session: &Arc<Session>, tool_defs: &[Json], force_answer: bool) -> Result<ChatOutput, String> {
        let tool_names: Vec<String> =
            tool_defs.iter().filter_map(|t| t.get("function").get("name").as_str().map(str::to_string)).collect();
        let system = self.prompt.build(&PromptCx { runtime: &self.runtime, session, tool_names: &tool_names });
        let mut messages = build_messages(&system, &session.records());
        if force_answer {
            messages.push(jobj! { "role" => "user", "content" => FINAL_NUDGE });
        }
        compact(&mut messages, self.config.context_chars);
        let request = ChatRequest {
            model: &self.runtime.model,
            messages: &messages,
            tools: tool_defs,
            max_tokens: self.runtime.max_tokens,
            reasoning_effort: self.runtime.reasoning_effort.as_deref(),
            tool_choice: if force_answer { Some("none") } else { None },
        };
        let stream = self.config.stream_deltas;
        let mut on_delta = |delta: Delta<'_>| match delta {
            Delta::Text(text) if stream => self.emit(session, "assistant/delta", jobj! { "text" => text }),
            Delta::Reasoning(text) if stream => self.emit(session, "assistant/reasoning", jobj! { "text" => text }),
            Delta::Retry(attempt, error) => {
                self.emit(session, "assistant/retry", jobj! { "attempt" => attempt as i64, "error" => error })
            }
            _ => {}
        };
        self.provider.chat(&request, &mut on_delta, &session.cancel).map_err(|e| e.message)
    }

    fn execute(&self, session: &Arc<Session>, call_id: &str, name: &str, arguments: &str, depth: usize) -> ToolOutput {
        let Some(tool) = self.tools.get(name) else {
            return ToolOutput::err(format!("unknown tool {name:?}; use one of the tools you were given"));
        };
        let args = if arguments.trim().is_empty() {
            Json::obj()
        } else {
            match json::parse(arguments) {
                Ok(value @ Json::Obj(_)) => value,
                Ok(_) => return ToolOutput::err("tool arguments must be a JSON object"),
                Err(e) => return ToolOutput::err(format!("tool arguments are not valid JSON ({e}); send a JSON object")),
            }
        };
        let cx = ToolCx { app: &self.app, runtime: &self.runtime, session, call_id, depth };
        catch_unwind(AssertUnwindSafe(|| tool.call(&cx, &args)))
            .unwrap_or_else(|_| ToolOutput::err(format!("tool {name} crashed; this is a harness bug")))
    }
}

fn assistant_event(id: &str, text: &str, calls: &[Json], transient: bool) -> Json {
    let mut message = jobj! {
        "id" => id,
        "role" => "assistant",
        "content" => jarr![jobj! { "type" => "text", "text" => text }],
    };
    if !calls.is_empty() {
        message.set("toolCalls", Json::Arr(calls.to_vec()));
    }
    let mut data = jobj! { "message" => message };
    if transient {
        data.set("transient", true);
    }
    data
}

#[derive(Default)]
struct Usage {
    prompt: i64,
    completion: i64,
    total: i64,
    cost: f64,
    calls: i64,
    token_usage_reported: bool,
    cost_reported: bool,
}

impl Usage {
    fn add(&mut self, usage: &Json) {
        self.calls += 1;
        self.token_usage_reported |= usage.get("prompt_tokens").as_i64().is_some()
            || usage.get("completion_tokens").as_i64().is_some()
            || usage.get("total_tokens").as_i64().is_some();
        self.cost_reported |= usage.get("cost").as_f64().is_some();
        self.prompt += usage.get("prompt_tokens").as_i64().unwrap_or(0);
        self.completion += usage.get("completion_tokens").as_i64().unwrap_or(0);
        self.total += usage.get("total_tokens").as_i64().unwrap_or(0);
        self.cost += usage.get("cost").as_f64().unwrap_or(0.0);
    }

    fn add_aggregate(&mut self, usage: &Json) {
        if usage.is_null() {
            return;
        }
        self.calls += usage.get("modelCalls").as_i64().unwrap_or(0);
        self.prompt += usage.get("promptTokens").as_i64().unwrap_or(0);
        self.completion += usage.get("completionTokens").as_i64().unwrap_or(0);
        self.total += usage.get("totalTokens").as_i64().unwrap_or(0);
        self.cost += usage.get("cost").as_f64().unwrap_or(0.0);
        self.token_usage_reported |= usage.get("tokenUsageReported").as_bool().unwrap_or(false);
        self.cost_reported |= usage.get("costReported").as_bool().unwrap_or(false);
    }

    fn to_json(&self) -> Json {
        let mut usage = jobj! {
            "modelCalls" => self.calls,
            "promptTokens" => self.prompt,
            "completionTokens" => self.completion,
            "totalTokens" => self.total,
            "tokenUsageReported" => self.token_usage_reported,
            "costReported" => self.cost_reported,
        };
        usage.set("cost", if self.cost_reported { Json::from(self.cost) } else { Json::Null });
        usage
    }
}

/// Rebuild the provider conversation from the session log.
///
/// Tool calls without a recorded result (the process died mid-call) get a
/// synthetic "interrupted" result, so the message sequence is always valid.
/// Reasoning details are returned only for the current turn.
pub fn build_messages(system: &str, records: &[Json]) -> Vec<Json> {
    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(jobj! { "role" => "system", "content" => system });
    }
    let last_user = records.iter().rposition(|r| r.get("type").as_str() == Some("message/user"));
    let mut contexts: Vec<(&str, &str)> = Vec::new();
    for record in records {
        if record.get("type").as_str() == Some("message/context") {
            let data = record.get("data");
            contexts.push((data.get("messageId").str_or(""), data.get("text").str_or("")));
        }
    }
    let mut pending: Vec<String> = Vec::new();
    let close_pending = |messages: &mut Vec<Json>, pending: &mut Vec<String>| {
        for id in pending.drain(..) {
            messages.push(jobj! { "role" => "tool", "tool_call_id" => id, "content" => INTERRUPTED });
        }
    };
    for (index, record) in records.iter().enumerate() {
        let data = record.get("data");
        match record.get("type").as_str().unwrap_or("") {
            "message/user" => {
                close_pending(&mut messages, &mut pending);
                let id = data.get("id").str_or("");
                let text = data.get("content").str_or("");
                let attached: Vec<&str> = contexts.iter().filter(|(m, _)| *m == id).map(|(_, t)| *t).collect();
                if attached.is_empty() {
                    messages.push(jobj! { "role" => "user", "content" => text });
                } else {
                    let mut parts = vec![jobj! { "type" => "text", "text" => text }];
                    parts.extend(attached.iter().map(|t| jobj! { "type" => "text", "text" => *t }));
                    messages.push(jobj! { "role" => "user", "content" => Json::Arr(parts) });
                }
            }
            "message/assistant" => {
                close_pending(&mut messages, &mut pending);
                let text = data.get("text").str_or("");
                let calls = data.get("toolCalls").items();
                let mut message = jobj! { "role" => "assistant" };
                if calls.is_empty() {
                    message.set("content", text);
                } else {
                    message.set("content", if text.is_empty() { Json::Null } else { Json::from(text) });
                    message.set(
                        "tool_calls",
                        Json::Arr(
                            calls
                                .iter()
                                .map(|c| {
                                    jobj! {
                                        "id" => c.get("id").clone(),
                                        "type" => "function",
                                        "function" => jobj! {
                                            "name" => c.get("name").clone(),
                                            "arguments" => if c.get("arguments").str_or("").is_empty() { Json::from("{}") } else { c.get("arguments").clone() },
                                        },
                                    }
                                })
                                .collect(),
                        ),
                    );
                    pending = calls.iter().filter_map(|c| c.get("id").as_str().map(str::to_string)).collect();
                }
                let current_turn = last_user.is_some_and(|u| index > u);
                if current_turn && !data.get("reasoningDetails").is_null() {
                    message.set("reasoning_details", data.get("reasoningDetails").clone());
                }
                messages.push(message);
            }
            "tool/result" => {
                let id = data.get("toolCallId").str_or("");
                if let Some(position) = pending.iter().position(|p| p == id) {
                    pending.remove(position);
                    let mut content = data.get("content").str_or("").to_string();
                    if data.get("isError").as_bool() == Some(true) && !content.starts_with("Error") {
                        content = format!("Error: {content}");
                    }
                    messages.push(jobj! { "role" => "tool", "tool_call_id" => id, "content" => content });
                }
            }
            _ => {}
        }
    }
    close_pending(&mut messages, &mut pending);
    messages
}

fn message_chars(message: &Json) -> usize {
    message.to_string().len()
}

/// Keep the request under `budget` characters: first elide old tool
/// outputs, then drop whole old exchanges. The system prompt and the most
/// recent user exchange are always kept.
pub fn compact(messages: &mut Vec<Json>, budget: usize) {
    let total = |m: &Vec<Json>| m.iter().map(message_chars).sum::<usize>();
    if total(messages) <= budget {
        return;
    }
    let last_user = messages.iter().rposition(|m| m.get("role").as_str() == Some("user")).unwrap_or(0);
    for index in 0..last_user {
        if total(messages) <= budget {
            return;
        }
        let message = &mut messages[index];
        if message.get("role").as_str() == Some("tool") && message.get("content").str_or("").len() > 500 {
            message.set("content", "[older tool output elided by the harness to fit the context window]");
        }
    }
    while total(messages) > budget {
        let has_system = messages.first().map(|m| m.get("role").as_str() == Some("system")).unwrap_or(false);
        let start = usize::from(has_system);
        let last_user = messages.iter().rposition(|m| m.get("role").as_str() == Some("user")).unwrap_or(0);
        let next_user = messages[start + 1..].iter().position(|m| m.get("role").as_str() == Some("user")).map(|p| p + start + 1);
        match next_user {
            Some(end) if end <= last_user && end > start => {
                messages.drain(start..end);
            }
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, data: Json) -> Json {
        jobj! { "v" => 1, "seq" => 1, "type" => kind, "data" => data }
    }

    #[test]
    fn rebuilds_interrupted_history_and_context() {
        let records = vec![
            rec("session/created", jobj! { "sessionId" => "s" }),
            rec("message/user", jobj! { "id" => "u1", "content" => "held turn" }),
            rec("message/context", jobj! { "messageId" => "u1", "source" => "leviathan", "text" => "recall" }),
            rec(
                "message/assistant",
                jobj! { "text" => "", "toolCalls" => jarr![jobj! { "id" => "c1", "name" => "x", "arguments" => "{}" }], "reasoningDetails" => jarr![jobj! { "text" => "t" }] },
            ),
            rec("message/user", jobj! { "id" => "u2", "content" => "recovery-probe" }),
        ];
        let messages = build_messages("sys", &records);
        let roles: Vec<&str> = messages.iter().map(|m| m.get("role").as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
        assert_eq!(messages[1].get("content").at(1).get("text").as_str(), Some("recall"));
        assert_eq!(messages[3].get("content").as_str(), Some(INTERRUPTED));
        assert!(messages[2].get("reasoning_details").is_null(), "old-turn reasoning is not replayed");
    }

    #[test]
    fn compaction_keeps_latest_exchange() {
        let big = "x".repeat(5_000);
        let mut messages = vec![
            jobj! { "role" => "system", "content" => "sys" },
            jobj! { "role" => "user", "content" => "old" },
            jobj! { "role" => "assistant", "content" => big.as_str() },
            jobj! { "role" => "user", "content" => "new" },
        ];
        compact(&mut messages, 1_000);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].get("content").as_str(), Some("new"));
    }

    #[test]
    fn usage_combines_root_and_nested_model_calls_without_inventing_cost() {
        let mut usage = Usage::default();
        usage.add(&jobj! {
            "prompt_tokens" => 10,
            "completion_tokens" => 2,
            "total_tokens" => 12,
        });
        usage.add_aggregate(&jobj! {
            "modelCalls" => 2,
            "promptTokens" => 20,
            "completionTokens" => 4,
            "totalTokens" => 24,
            "cost" => Json::Null,
            "tokenUsageReported" => true,
            "costReported" => false,
        });
        let output = usage.to_json();
        assert_eq!(output.get("modelCalls").as_i64(), Some(3));
        assert_eq!(output.get("promptTokens").as_i64(), Some(30));
        assert_eq!(output.get("completionTokens").as_i64(), Some(6));
        assert_eq!(output.get("totalTokens").as_i64(), Some(36));
        assert_eq!(output.get("tokenUsageReported").as_bool(), Some(true));
        assert!(output.get("cost").is_null());
    }
}
