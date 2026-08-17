//! Versioned request/response protocol.
//!
//! Every frame is one JSON object per line. Clients send [`Request`]s and
//! receive [`Response`]s with the same `id`; the daemon may interleave
//! [`Event`] frames at any time.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped only on breaking protocol changes; reported in `engine.status`.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    /// Namespaced method, e.g. `engine.status`, `project.inspect`.
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    UnknownMethod,
    InvalidParams,
    Internal,
    Busy,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Response { id, result: Some(result), error: None }
    }

    pub fn err(id: u64, code: RpcErrorCode, message: impl Into<String>) -> Self {
        Response {
            id,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}

/// Daemon-initiated notification (`run.started`, `failure.detected`, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Always `"event"`, so clients can tell frames apart from responses.
    pub kind: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

impl Event {
    pub fn new(event: impl Into<String>, payload: Value) -> Self {
        Event { kind: "event".to_owned(), event: event.into(), payload }
    }
}
