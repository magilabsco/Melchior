//! System prompt assembly and per-turn context hooks.
//!
//! The system prompt is rebuilt for every model request: the persona from the
//! `system-prompt` plugin first, then each registered section in plugin order.
//! It is never stored in the session log, so a changed agent prompt applies to
//! restored sessions immediately.

use std::sync::{Arc, Mutex};

use crate::cordis::{App, Registry};
use crate::runtime::Runtime;
use crate::session::Session;

pub struct PromptCx<'a> {
    pub runtime: &'a Runtime,
    pub session: &'a Session,
    pub tool_names: &'a [String],
}

pub type Section = Box<dyn Fn(&PromptCx<'_>) -> Option<String> + Send + Sync>;

pub struct PromptService {
    persona: Mutex<String>,
    pub sections: Arc<Registry<Section>>,
}

impl Default for PromptService {
    fn default() -> Self {
        PromptService { persona: Mutex::new(String::new()), sections: Arc::new(Registry::new()) }
    }
}

impl PromptService {
    pub fn set_persona(&self, persona: &str) {
        *self.persona.lock().unwrap() = persona.trim().to_string();
    }

    pub fn persona(&self) -> String {
        self.persona.lock().unwrap().clone()
    }

    pub fn build(&self, cx: &PromptCx<'_>) -> String {
        let mut parts = Vec::new();
        let persona = self.persona();
        if !persona.is_empty() {
            parts.push(persona);
        }
        for (_, section) in self.sections.all() {
            if let Some(text) = section(cx) {
                let text = text.trim();
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
        parts.join("\n\n")
    }
}

/// Text a plugin attaches to the user's message for this turn only
/// (persisted with the turn so a restored session replays it verbatim).
pub struct TurnContext {
    pub source: String,
    pub text: String,
}

pub struct TurnCx<'a> {
    pub app: &'a App,
    pub runtime: &'a Arc<Runtime>,
    pub session: &'a Arc<Session>,
    pub message_id: &'a str,
    pub text: &'a str,
}

pub type TurnHook = Box<dyn Fn(&TurnCx<'_>) -> Option<TurnContext> + Send + Sync>;
pub type TurnHooks = Registry<TurnHook>;

/// A finished top-level turn, after its `turn/end` record is durable.
pub struct TurnEndCx<'a> {
    pub session: &'a Arc<Session>,
    pub message_id: &'a str,
    /// The user's message.
    pub text: &'a str,
    /// The final answer shown to the user.
    pub answer: &'a str,
    /// `stop`, `max_rounds` or `cancelled`. Turns that failed are not reported.
    pub reason: &'a str,
}

/// Runs on the turn's thread before the prompt request is answered, so a hook
/// that does network work should hand it to a thread of its own.
pub type TurnEndHook = Box<dyn Fn(&TurnEndCx<'_>) + Send + Sync>;
pub type TurnEndHooks = Registry<TurnEndHook>;
