//! Agent memory in Leviathan (plugin `leviathan-memory`).
//!
//! Leviathan memory is a small read/write database of typed claims, kept
//! beside the data index and reached through the same backend (`recall`,
//! `remember`, `forget`). A host can scope it to one agent with the
//! `leviathan` plugin's `namespace`, or by forcing the namespace in its own
//! gateway. The session log stays the exact record of a conversation; memory
//! is what carries over between conversations. This plugin:
//!
//! - **Briefs.** On the first message of a session, the memory briefing
//!   (pinned and most important current memories, without journal events)
//!   is attached to the message.
//! - **Recalls.** On every message, memories matching the user's words are
//!   attached too. Both arrive in one `<agent_memory>` block, persisted as
//!   `message/context`, so a resumed session replays what the model saw.
//! - **Journals.** After each answered turn, one `event` memory records the
//!   question and the start of the answer, so a later conversation can find
//!   "what did we discuss about Line 4". Sessions whose id starts with a
//!   `journalSkip` prefix (for example evaluation runs) are not journaled.
//!   It is written on a background thread and never delays or fails the
//!   turn; shutdown waits up to five seconds for writes still in flight.
//! - **Guides.** A system-prompt section tells the model when to `remember`
//!   and `forget` (durable facts, preferences, decisions, corrections).
//!
//! Without a backend that has the memory tools the plugin does nothing.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::index::recall_query;
use super::Leviathan;
use crate::cordis::{Context, Plugin};
use crate::json::Json;
use crate::prompt::{PromptService, TurnContext, TurnCx, TurnEndCx, TurnEndHooks, TurnHooks};
use crate::session::Session;
use crate::{jarr, jobj, log_debug, log_info, util};

pub const RECALL: &str = "recall";
pub const REMEMBER: &str = "remember";
pub const FORGET: &str = "forget";

/// Leviathan's cap on one memory's text.
const MAX_CHARS: usize = 400;

#[derive(Clone)]
pub struct MemoryConfig {
    pub briefing: bool,
    pub recall: bool,
    pub journal: bool,
    pub guide: bool,
    pub briefing_budget: i64,
    pub recall_budget: i64,
    pub min_query_chars: usize,
    pub journal_importance: i64,
    pub journal_expires: Option<String>,
    pub journal_skip: Vec<String>,
}

impl MemoryConfig {
    pub fn from_json(config: &Json) -> MemoryConfig {
        MemoryConfig {
            briefing: config.get("briefing").as_bool().unwrap_or(true),
            recall: config.get("recall").as_bool().unwrap_or(true),
            journal: config.get("journal").as_bool().unwrap_or(true),
            guide: config.get("guide").as_bool().unwrap_or(true),
            briefing_budget: config.get("briefingBudget").as_i64().unwrap_or(500).clamp(50, 4_000),
            recall_budget: config.get("recallBudget").as_i64().unwrap_or(400).clamp(50, 4_000),
            min_query_chars: config.get("minQueryChars").as_i64().unwrap_or(8).max(1) as usize,
            journal_importance: config.get("journalImportance").as_i64().unwrap_or(2).clamp(1, 5),
            journal_expires: match config.get("journalExpires") {
                Json::Null => Some("90d".into()),
                value => value.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            },
            journal_skip: config.get("journalSkip").items().iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        }
    }
}

pub struct Memory {
    leviathan: Arc<Leviathan>,
    config: MemoryConfig,
    /// Journal writes still running.
    in_flight: (Mutex<usize>, Condvar),
}

/// What one recall returned: rendered lines and their memory ids.
struct Recalled {
    lines: Vec<String>,
    ids: Vec<String>,
}

impl Memory {
    pub fn new(leviathan: Arc<Leviathan>, config: MemoryConfig) -> Memory {
        Memory { leviathan, config, in_flight: (Mutex::new(0), Condvar::new()) }
    }

    /// Wait (at most `limit`) for journal writes still running.
    pub fn flush(&self, limit: Duration) {
        let (count, done) = &self.in_flight;
        let guard = count.lock().unwrap();
        let (guard, _) = done.wait_timeout_while(guard, limit, |n| *n > 0).unwrap();
        if *guard > 0 {
            log_info!("memory", "shutting down with {} journal writes unfinished", *guard);
        }
    }

    fn finish_write(&self) {
        let (count, done) = &self.in_flight;
        *count.lock().unwrap() -= 1;
        done.notify_all();
    }

    fn public_name(&self, tool: &str) -> Option<String> {
        self.leviathan.public_name(tool)
    }

    fn recall_json(&self, args: Json, skip_events: bool) -> Option<Recalled> {
        let result = self.leviathan.call(RECALL, args)?;
        let rows: Vec<&Json> = result
            .get("results")
            .items()
            .iter()
            .filter(|r| !(skip_events && r.get("kind").as_str() == Some("event")))
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(Recalled {
            lines: rows.iter().map(|r| memory_line(r)).collect(),
            ids: rows.iter().filter_map(|r| r.get("id").as_str().map(str::to_string)).collect(),
        })
    }

    /// The `<agent_memory>` block for one user message.
    pub fn context(&self, session: &Session, message: &str) -> Option<TurnContext> {
        let first = session.records().iter().filter(|r| r.get("type").as_str() == Some("message/user")).count() <= 1;
        let briefing = if first && self.config.briefing {
            self.recall_json(jobj! { "budget" => self.config.briefing_budget }, true)
        } else {
            None
        };
        let query = recall_query(message);
        let recalled = if self.config.recall && query.chars().count() >= self.config.min_query_chars {
            self.recall_json(jobj! { "query" => query.as_str(), "budget" => self.config.recall_budget }, false)
                .filter(|r| briefing.as_ref().is_none_or(|b| !r.ids.iter().all(|id| b.ids.contains(id))))
        } else {
            None
        };
        if briefing.is_none() && recalled.is_none() {
            return None;
        }
        let mut out = String::from(
            "<agent_memory source=\"leviathan\">\n\
             Your own notes from earlier conversations in this workspace (current values only). They can be out of \
             date: what the user says now wins, and numbers must be re-checked in the workspace before you rely on them.\n",
        );
        if let Some(briefing) = briefing {
            out.push_str("Standing memories:\n");
            for line in briefing.lines {
                out.push_str(&format!("{line}\n"));
            }
        }
        if let Some(recalled) = recalled {
            out.push_str("Matching this message:\n");
            for line in recalled.lines {
                out.push_str(&format!("{line}\n"));
            }
        }
        out.push_str("</agent_memory>");
        log_debug!("memory", "attached {} chars of memory", out.len());
        Some(TurnContext { source: "leviathan-memory".into(), text: out })
    }

    /// Record an answered turn as one `event` memory, off the turn's thread.
    pub fn journal(self: &Arc<Self>, cx: &TurnEndCx<'_>) {
        if !self.config.journal || cx.reason != "stop" || self.config.journal_skip.iter().any(|p| cx.session.id.starts_with(p.as_str())) {
            return;
        }
        // Resolved now: during shutdown the backend may be gone from the
        // registry by the time the thread runs.
        let Some(backend) = self.leviathan.backend().filter(|b| b.has(REMEMBER)) else { return };
        let question = recall_query(cx.text);
        if question.chars().count() < self.config.min_query_chars {
            return;
        }
        let Some(text) = journal_text(&question, cx.answer) else { return };
        let mut args = jobj! {
            "text" => text.as_str(),
            "kind" => "event",
            "importance" => self.config.journal_importance,
            "tags" => jarr!["conversation"],
            "source" => format!("conversation:{}", cx.session.id),
        };
        if let Some(expires) = &self.config.journal_expires {
            args.set("expires", expires.as_str());
        }
        if let Some(ns) = &self.leviathan.config.namespace {
            args.set("ns", ns.as_str());
        }
        *self.in_flight.0.lock().unwrap() += 1;
        let memory = self.clone();
        let spawned = std::thread::Builder::new().name("memory-journal".into()).spawn(move || {
            match backend.call(REMEMBER, args) {
                Ok(_) => log_debug!("memory", "journaled {} chars", text.len()),
                Err(e) => log_info!("memory", "journal write failed: {e}"),
            }
            memory.finish_write();
        });
        if let Err(error) = spawned {
            log_info!("memory", "journal thread failed to start: {error}");
            self.finish_write();
        }
    }

    pub fn guide(&self) -> Option<String> {
        let remember = self.public_name(REMEMBER)?;
        let recall = self.public_name(RECALL);
        let forget = self.public_name(FORGET);
        let mut out = String::from(
            "# Memory (Leviathan)\n\
             You keep a private memory that lasts across conversations in this workspace. Memories relevant to a \
             message arrive with it in an <agent_memory> block.\n",
        );
        out.push_str(&format!(
            "- Save with `{remember}` when the user tells you something durable: a preference (\"send me tables\"), a \
             decision and why, a correction, a standing fact about a person, machine, supplier or process. One claim per \
             call. Give anything that can change a subject and key (subject \"Sarah\", key \"report.format\") so a later \
             change replaces it.\n"
        ));
        if let Some(recall) = recall {
            out.push_str(&format!(
                "- Search with `{recall}` when the user refers to an earlier conversation or something you may have saved.\n"
            ));
        }
        if let Some(forget) = forget {
            out.push_str(&format!("- Retire a memory that turned out wrong with `{forget}`.\n"));
        }
        out.push_str(
            "- Never save secrets or credentials, and don't copy what the workspace index already holds; save what you \
             learned from the user. Each answered question is also noted automatically as an `event`; don't save those \
             yourself.\n",
        );
        Some(out)
    }
}

/// One memory as Leviathan renders it: `[kind] subject · key: text (date · id)`.
/// Rendered here because Leviathan's own text ends with advice naming its
/// own tool names, which the model does not have.
fn memory_line(row: &Json) -> String {
    let mut out = format!("[{}]", row.get("kind").str_or("note"));
    match (row.get("subject").as_str(), row.get("key").as_str()) {
        (Some(s), Some(k)) => out.push_str(&format!(" {s} · {k}:")),
        (Some(s), None) => out.push_str(&format!(" {s}:")),
        (None, Some(k)) => out.push_str(&format!(" {k}:")),
        (None, None) => {}
    }
    let date = row.get("valid_from").str_or("");
    let mut meta = vec![date[..date.len().min(10)].to_string()];
    if row.get("pinned").as_bool() == Some(true) {
        meta.push("pinned".into());
    }
    meta.push(row.get("id").str_or("?").to_string());
    format!("{out} {} ({})", row.get("text").str_or("").trim(), meta.join(" · "))
}

/// `Asked: "...". Answered: ...`, within Leviathan's 400-character cap. The
/// answer part is the opening prose of the answer: headings, tables, code and
/// markdown emphasis are dropped.
pub fn journal_text(question: &str, answer: &str) -> Option<String> {
    let question = util::truncate(question, 150, "…");
    let mut prose = Vec::new();
    let mut fenced = false;
    for line in answer.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.is_empty() || line.starts_with('|') || line.starts_with('#') || line.starts_with("---") {
            continue;
        }
        prose.push(line.trim_start_matches(['-', '*', '>', ' ']).replace("**", "").replace('`', ""));
        if prose.iter().map(String::len).sum::<usize>() > MAX_CHARS {
            break;
        }
    }
    let prose = prose.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
    if prose.is_empty() {
        return None;
    }
    let head = format!("Asked: \"{question}\". Answered: ");
    // `truncate` appends its suffix after `room` characters.
    let room = MAX_CHARS.saturating_sub(head.chars().count() + 1);
    Some(format!("{head}{}", util::truncate(&prose, room, "…")))
}

pub struct LeviathanMemoryPlugin;

impl Plugin for LeviathanMemoryPlugin {
    fn name(&self) -> &'static str {
        "leviathan-memory"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["leviathan", "prompt", "turn-hooks", "turn-end-hooks"]
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let config = MemoryConfig::from_json(ctx.config());
        let memory = Arc::new(Memory::new(ctx.inject::<Leviathan>("leviathan")?, config.clone()));
        ctx.provide("leviathan-memory", memory.clone())?;
        let flushing = memory.clone();
        ctx.effect(move || flushing.flush(Duration::from_secs(5)));
        if config.guide {
            let source = memory.clone();
            ctx.inject::<PromptService>("prompt")?.sections.add(
                ctx,
                "leviathan-memory/guide",
                Box::new(move |_cx| source.guide()),
            )?;
        }
        if config.briefing || config.recall {
            let source = memory.clone();
            ctx.inject::<TurnHooks>("turn-hooks")?.add(
                ctx,
                "leviathan-memory/recall",
                Box::new(move |cx: &TurnCx<'_>| source.context(cx.session, cx.text)),
            )?;
        }
        if config.journal {
            let source = memory.clone();
            ctx.inject::<TurnEndHooks>("turn-end-hooks")?.add(
                ctx,
                "leviathan-memory/journal",
                Box::new(move |cx: &TurnEndCx<'_>| source.journal(cx)),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    #[test]
    fn journal_keeps_the_question_and_the_opening_prose() {
        let answer = "## Line 4 bearing\n\n**Bearing ZK-4471-B** failed on Line 4; Priya Ramaswamy signed off the repair.\n\n\
                      | part | qty |\n|---|---|\n| ZK-4471-B | 2 |\n\n```sql\nselect 1\n```\n- Kestrel PO 88213 covers the spares.";
        let text = journal_text("Which bearing failed on Line 4?", answer).unwrap();
        assert_eq!(
            text,
            "Asked: \"Which bearing failed on Line 4?\". Answered: Bearing ZK-4471-B failed on Line 4; Priya Ramaswamy \
             signed off the repair. Kestrel PO 88213 covers the spares."
        );
        assert!(journal_text("question here", "| only | a table |\n|---|---|").is_none());
    }

    #[test]
    fn journal_stays_within_leviathans_cap() {
        let text = journal_text(&"why ".repeat(100), &"because the spindle overheated. ".repeat(40)).unwrap();
        assert!(text.chars().count() <= MAX_CHARS, "{}", text.chars().count());
        assert!(text.ends_with('…'));
    }

    #[test]
    fn memory_lines_match_leviathans_format() {
        let row = json::parse(
            r#"{"id":"m1","kind":"preference","subject":"Sarah","key":"report.format","text":"Wants tables","valid_from":"2026-10-25T10:00:00Z","pinned":true}"#,
        )
        .unwrap();
        assert_eq!(memory_line(&row), "[preference] Sarah · report.format: Wants tables (2026-10-25 · pinned · m1)");
        let row = json::parse(r#"{"id":"m2","kind":"event","text":"Asked: \"x\"","valid_from":"2026-10-24"}"#).unwrap();
        assert_eq!(memory_line(&row), "[event] Asked: \"x\" (2026-10-24 · m2)");
    }

    #[test]
    fn config_defaults_and_overrides() {
        let config = MemoryConfig::from_json(&Json::Null);
        assert!(config.briefing && config.recall && config.journal && config.guide);
        assert_eq!(config.journal_expires.as_deref(), Some("90d"));
        assert!(config.journal_skip.is_empty());
        let config = MemoryConfig::from_json(&json::parse(r#"{"journal":false,"journalExpires":"","journalSkip":["eval-"]}"#).unwrap());
        assert!(!config.journal);
        assert_eq!(config.journal_expires, None);
        assert_eq!(config.journal_skip, vec!["eval-".to_string()]);
    }
}
