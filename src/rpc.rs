//! JSON-RPC 2.0 framing: one JSON object per line on stdout.

use std::io::Write;
use std::sync::Mutex;

use crate::jobj;
use crate::json::Json;

pub const INTERNAL_ERROR: i64 = -32603;
pub const INVALID_PARAMS: i64 = -32602;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_REQUEST: i64 = -32600;
pub const PARSE_ERROR: i64 = -32700;

type Sink = Box<dyn Fn(&str) + Send + Sync>;

/// Writes protocol messages. In SDK mode the sink is stdout; headless mode
/// and tests install their own sink.
pub struct Notifier {
    sink: Mutex<Sink>,
}

impl Notifier {
    pub fn stdout() -> Notifier {
        Notifier::with_sink(|line| {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        })
    }

    pub fn with_sink(sink: impl Fn(&str) + Send + Sync + 'static) -> Notifier {
        Notifier { sink: Mutex::new(Box::new(sink)) }
    }

    pub fn silent() -> Notifier {
        Notifier::with_sink(|_| {})
    }

    pub fn send(&self, message: &Json) {
        let line = message.to_string();
        let sink = self.sink.lock().unwrap();
        sink(&line);
    }

    pub fn respond(&self, id: &Json, result: Json) {
        self.send(&jobj! { "jsonrpc" => "2.0", "id" => id.clone(), "result" => result });
    }

    pub fn respond_error(&self, id: &Json, code: i64, message: &str) {
        self.send(&jobj! {
            "jsonrpc" => "2.0",
            "id" => id.clone(),
            "error" => jobj! { "code" => code, "message" => message },
        });
    }

    pub fn notify(&self, method: &str, params: Json) {
        self.send(&jobj! { "jsonrpc" => "2.0", "method" => method, "params" => params });
    }

    /// `session.event` carries one session event: `{sessionId, event: {type, data, ...}}`.
    pub fn session_event(&self, session_id: &str, event: Json) {
        self.notify("session.event", jobj! { "sessionId" => session_id, "event" => event });
    }

    pub fn session_status(&self, session_id: &str, status: &str) {
        self.notify("session.status", jobj! { "sessionId" => session_id, "status" => status });
    }
}

pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn internal(message: impl Into<String>) -> RpcError {
        RpcError { code: INTERNAL_ERROR, message: message.into() }
    }
    pub fn params(message: impl Into<String>) -> RpcError {
        RpcError { code: INVALID_PARAMS, message: message.into() }
    }
}

impl From<String> for RpcError {
    fn from(message: String) -> Self {
        RpcError::internal(message)
    }
}
