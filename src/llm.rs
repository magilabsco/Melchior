//! Model providers. Every provider speaks the OpenAI-compatible
//! `/chat/completions` streaming API, which covers OpenRouter, DeepSeek, and
//! the loopback servers used in tests.

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::cordis::Registry;
use crate::http::{self, Request};
use crate::jobj;
use crate::json::{self, Json};

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct ChatOutput {
    pub text: String,
    pub reasoning: String,
    pub reasoning_details: Vec<Json>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: Json,
    pub model: String,
}

pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Json],
    pub tools: &'a [Json],
    pub max_tokens: Option<i64>,
    pub reasoning_effort: Option<&'a str>,
    pub tool_choice: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct LlmError {
    pub message: String,
    pub retryable: bool,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub enum Delta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
    Retry(u32, &'a str),
}

pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn chat(&self, request: &ChatRequest<'_>, on_delta: &mut dyn FnMut(Delta<'_>), cancel: &AtomicBool)
    -> Result<ChatOutput, LlmError>;
    fn describe(&self) -> Json;
}

pub type LlmRegistry = Registry<Box<dyn Provider>>;

pub struct OpenAiCompatible {
    pub id: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
    pub models: Vec<String>,
    pub max_attempts: u32,
    pub read_timeout: Duration,
    pub total_timeout: Duration,
}

impl OpenAiCompatible {
    pub fn from_config(id: &str, config: &Json, fallback_base: &str, fallback_key_env: &str) -> OpenAiCompatible {
        let headers = config
            .get("headers")
            .entries()
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect();
        let models = config
            .get("models")
            .items()
            .iter()
            .filter_map(|m| m.as_str().or_else(|| m.get("id").as_str()).map(str::to_string))
            .collect();
        OpenAiCompatible {
            id: id.to_string(),
            base_url: config
                .get("baseURL")
                .as_str()
                .or_else(|| config.get("baseUrl").as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback_base)
                .trim_end_matches('/')
                .to_string(),
            api_key_env: Some(config.get("apiKeyEnv").str_or(fallback_key_env).to_string()),
            api_key: config.get("apiKey").as_str().map(str::to_string),
            headers,
            models,
            max_attempts: config.get("maxAttempts").as_i64().unwrap_or(3).clamp(1, 6) as u32,
            read_timeout: Duration::from_secs(config.get("idleTimeoutSeconds").as_i64().unwrap_or(180).max(5) as u64),
            total_timeout: Duration::from_secs(config.get("timeoutSeconds").as_i64().unwrap_or(600).max(10) as u64),
        }
    }

    fn key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| self.api_key_env.as_ref().and_then(|name| std::env::var(name).ok()))
            .filter(|k| !k.trim().is_empty())
    }

    pub fn body(&self, request: &ChatRequest<'_>) -> Json {
        let mut body = jobj! {
            "model" => request.model,
            "messages" => Json::Arr(request.messages.to_vec()),
            "stream" => true,
            "stream_options" => jobj! { "include_usage" => true },
        };
        if !request.tools.is_empty() {
            body.set("tools", Json::Arr(request.tools.to_vec()));
            body.set("tool_choice", request.tool_choice.unwrap_or("auto"));
        }
        if let Some(max) = request.max_tokens {
            body.set("max_tokens", max);
        }
        if let Some(effort) = request.reasoning_effort {
            body.set("reasoning", jobj! { "effort" => effort });
        }
        body
    }

    fn attempt(
        &self,
        body: &Json,
        on_delta: &mut dyn FnMut(Delta<'_>),
        cancel: &AtomicBool,
    ) -> Result<ChatOutput, (LlmError, Option<Duration>)> {
        let mut request = Request::new("POST", &format!("{}/chat/completions", self.base_url))
            .json(body)
            .header("Accept", "text/event-stream")
            .timeouts(self.read_timeout, self.total_timeout);
        if let Some(key) = self.key() {
            request = request.header("Authorization", &format!("Bearer {key}"));
        }
        if self.base_url.contains("openrouter.ai") {
            // OpenRouter's app attribution; a provider's `headers` replace it.
            for (name, value) in [("HTTP-Referer", "https://github.com/magilabsco/Melchior"), ("X-Title", "Melchior")] {
                if !self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                    request = request.header(name, value);
                }
            }
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = http::send(&request)
            .map_err(|e| (LlmError { message: format!("model request failed: {e}"), retryable: e.retryable }, None))?;
        if response.status >= 400 {
            let status = response.status;
            let retry_after = response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.min(30)));
            let text = response.text(64 * 1024).unwrap_or_default();
            let detail = json::parse(&text)
                .ok()
                .and_then(|j| j.get("error").get("message").as_str().map(str::to_string))
                .unwrap_or_else(|| crate::util::truncate(text.trim(), 600, "…"));
            let retryable = matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529);
            return Err((LlmError { message: format!("model provider returned HTTP {status}: {detail}"), retryable }, retry_after));
        }
        let streaming = response.header("content-type").map(|v| v.contains("event-stream")).unwrap_or(false);
        if !streaming {
            let text = response.text(16 * 1024 * 1024).map_err(|e| (LlmError { message: e.message, retryable: true }, None))?;
            let value = json::parse(&text).map_err(|e| {
                (LlmError { message: format!("model provider returned invalid JSON: {e}"), retryable: true }, None)
            })?;
            return parse_complete(&value).map_err(|e| (e, None));
        }
        let mut assembler = Assembler::default();
        let mut data = String::new();
        let mut body = response.body;
        let mut line = String::new();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err((LlmError { message: "cancelled".into(), retryable: false }, None));
            }
            line.clear();
            let n = body.read_line(&mut line).map_err(|e| {
                (LlmError { message: format!("model stream interrupted: {e}"), retryable: true }, None)
            })?;
            if n == 0 {
                if !data.is_empty() {
                    assembler.event(&data, on_delta).map_err(|e| (e, None))?;
                }
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                if !data.is_empty() {
                    let done = assembler.event(&data, on_delta).map_err(|e| (e, None))?;
                    data.clear();
                    if done {
                        break;
                    }
                }
                continue;
            }
            if trimmed.starts_with(':') {
                continue;
            }
            if let Some(payload) = trimmed.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(payload.strip_prefix(' ').unwrap_or(payload));
            }
        }
        if !assembler.saw_any {
            return Err((LlmError { message: "model stream ended without any data".into(), retryable: true }, None));
        }
        Ok(assembler.finish())
    }
}

impl Provider for OpenAiCompatible {
    fn id(&self) -> &str {
        &self.id
    }

    fn describe(&self) -> Json {
        jobj! {
            "id" => self.id.as_str(),
            "baseURL" => self.base_url.as_str(),
            "apiKeyEnv" => self.api_key_env.clone(),
            "models" => Json::Arr(self.models.iter().map(|m| Json::from(m.as_str())).collect()),
        }
    }

    fn chat(
        &self,
        request: &ChatRequest<'_>,
        on_delta: &mut dyn FnMut(Delta<'_>),
        cancel: &AtomicBool,
    ) -> Result<ChatOutput, LlmError> {
        let body = self.body(request);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.attempt(&body, on_delta, cancel) {
                Ok(output) => return Ok(output),
                Err((error, retry_after)) => {
                    if !error.retryable || attempt >= self.max_attempts || cancel.load(Ordering::SeqCst) {
                        return Err(error);
                    }
                    let wait = retry_after.unwrap_or(Duration::from_millis(800 * 3u64.pow(attempt - 1)));
                    crate::log_warn!("llm", "{} attempt {attempt} failed ({error}); retrying in {wait:?}", self.id);
                    on_delta(Delta::Retry(attempt, &error.message));
                    std::thread::sleep(wait);
                }
            }
        }
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct Assembler {
    text: String,
    reasoning: String,
    details: Vec<Json>,
    calls: Vec<PartialCall>,
    finish: String,
    usage: Json,
    model: String,
    saw_any: bool,
}

impl Assembler {
    /// Feed one SSE `data:` payload. Returns true on `[DONE]`.
    fn event(&mut self, data: &str, on_delta: &mut dyn FnMut(Delta<'_>)) -> Result<bool, LlmError> {
        if data.trim() == "[DONE]" {
            return Ok(true);
        }
        let chunk = json::parse(data)
            .map_err(|e| LlmError { message: format!("model stream sent invalid JSON: {e}"), retryable: true })?;
        self.saw_any = true;
        if !chunk.get("error").is_null() {
            let error = chunk.get("error");
            let code = error.get("code").as_i64().unwrap_or(0);
            return Err(LlmError {
                message: format!("model provider error: {}", error.get("message").str_or("unknown error")),
                retryable: code == 429 || code >= 500 || self.text.is_empty() && self.calls.is_empty(),
            });
        }
        if let Some(model) = chunk.get("model").as_str() {
            self.model = model.to_string();
        }
        if !chunk.get("usage").is_null() {
            self.usage = chunk.get("usage").clone();
        }
        let choice = chunk.get("choices").at(0);
        let delta = choice.get("delta");
        if let Some(text) = delta.get("content").as_str()
            && !text.is_empty() {
                self.text.push_str(text);
                on_delta(Delta::Text(text));
            }
        let reasoning = delta.get("reasoning").as_str().or_else(|| delta.get("reasoning_content").as_str());
        if let Some(reasoning) = reasoning
            && !reasoning.is_empty() {
                self.reasoning.push_str(reasoning);
                on_delta(Delta::Reasoning(reasoning));
            }
        for detail in delta.get("reasoning_details").items() {
            self.merge_detail(detail);
        }
        for call in delta.get("tool_calls").items() {
            let index = call.get("index").as_i64().unwrap_or(self.calls.len() as i64).max(0) as usize;
            while self.calls.len() <= index {
                self.calls.push(PartialCall::default());
            }
            let slot = &mut self.calls[index];
            if let Some(id) = call.get("id").as_str()
                && !id.is_empty() {
                    slot.id = id.to_string();
                }
            let function = call.get("function");
            if let Some(name) = function.get("name").as_str() {
                slot.name.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").as_str() {
                slot.arguments.push_str(arguments);
            }
        }
        if let Some(reason) = choice.get("finish_reason").as_str() {
            self.finish = reason.to_string();
        }
        Ok(false)
    }

    /// Streamed reasoning details arrive as fragments keyed by `index`;
    /// text-bearing fields are concatenated, everything else is kept as sent.
    fn merge_detail(&mut self, detail: &Json) {
        let index = detail.get("index").as_i64();
        let existing = index.and_then(|i| self.details.iter_mut().find(|d| d.get("index").as_i64() == Some(i)));
        match existing {
            Some(current) => {
                for (key, value) in detail.entries() {
                    match (key.as_str(), value) {
                        ("text" | "summary" | "data", Json::Str(fragment)) => {
                            let merged = format!("{}{fragment}", current.get(key).str_or(""));
                            current.set(key, merged);
                        }
                        (_, value) if !value.is_null() => {
                            current.set(key, value.clone());
                        }
                        _ => {}
                    }
                }
            }
            None => self.details.push(detail.clone()),
        }
    }

    fn finish(self) -> ChatOutput {
        let tool_calls = self
            .calls
            .into_iter()
            .filter(|c| !c.name.is_empty())
            .map(|c| ToolCall {
                id: if c.id.is_empty() { crate::util::new_id("call") } else { c.id },
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        ChatOutput {
            text: self.text,
            reasoning: self.reasoning,
            reasoning_details: self.details,
            tool_calls,
            finish_reason: self.finish,
            usage: self.usage,
            model: self.model,
        }
    }
}

fn parse_complete(value: &Json) -> Result<ChatOutput, LlmError> {
    if !value.get("error").is_null() {
        return Err(LlmError {
            message: format!("model provider error: {}", value.get("error").get("message").str_or("unknown error")),
            retryable: false,
        });
    }
    let choice = value.get("choices").at(0);
    let message = choice.get("message");
    Ok(ChatOutput {
        text: message.get("content").str_or("").to_string(),
        reasoning: message.get("reasoning").as_str().or_else(|| message.get("reasoning_content").as_str()).unwrap_or("").into(),
        reasoning_details: message.get("reasoning_details").items().to_vec(),
        tool_calls: message
            .get("tool_calls")
            .items()
            .iter()
            .map(|c| ToolCall {
                id: c.get("id").as_str().map(str::to_string).unwrap_or_else(|| crate::util::new_id("call")),
                name: c.get("function").get("name").str_or("").to_string(),
                arguments: c.get("function").get("arguments").str_or("").to_string(),
            })
            .filter(|c| !c.name.is_empty())
            .collect(),
        finish_reason: choice.get("finish_reason").str_or("stop").to_string(),
        usage: value.get("usage").clone(),
        model: value.get("model").str_or("").to_string(),
    })
}

/// Pick the provider for `initialize({provider})`.
pub fn select(registry: &Arc<LlmRegistry>, provider: &str) -> Option<Arc<Box<dyn Provider>>> {
    registry.get(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_streamed_tool_calls_and_usage() {
        let mut assembler = Assembler::default();
        let mut seen = String::new();
        let mut on = |d: Delta<'_>| {
            if let Delta::Text(t) = d {
                seen.push_str(t);
            }
        };
        for chunk in [
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hi "}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"mcp__tools__files_list","arguments":"{\"a\""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":"think "}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":"more"}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"total_tokens":12}}"#,
        ] {
            assert!(!assembler.event(chunk, &mut on).unwrap());
        }
        assert!(assembler.event("[DONE]", &mut on).unwrap());
        let output = assembler.finish();
        assert_eq!(seen, "Hi ");
        assert_eq!(output.tool_calls[0].arguments, "{\"a\":1}");
        assert_eq!(output.reasoning_details[0].get("text").as_str(), Some("think more"));
        assert_eq!(output.finish_reason, "tool_calls");
        assert_eq!(output.usage.get("total_tokens").as_i64(), Some(12));
    }
}
