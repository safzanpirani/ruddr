//! Line-delimited JSON-RPC as `codex app-server` speaks it: one JSON object
//! per line, no `"jsonrpc"` field required. Ruddr-originated calls use string
//! IDs; responses with string or numeric IDs are both accepted.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Str(String),
    Num(i64),
}

impl Id {
    /// The ID as text, so `"7"` and `7` correlate to the same call.
    pub fn key(&self) -> String {
        match self {
            Id::Str(s) => s.clone(),
            Id::Num(n) => n.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC error codes.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Message {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    Notification,
    Response,
    Invalid,
}

impl Message {
    pub fn request(id: impl Into<String>, method: &str, params: Value) -> Message {
        Message { id: Some(Id::Str(id.into())), method: Some(method.into()), params: Some(params), ..Default::default() }
    }
    pub fn notification(method: &str, params: Value) -> Message {
        Message { method: Some(method.into()), params: Some(params), ..Default::default() }
    }
    pub fn response(id: Id, result: Value) -> Message {
        Message { id: Some(id), result: Some(result), ..Default::default() }
    }
    pub fn error_response(id: Id, code: i64, message: impl Into<String>) -> Message {
        Message { id: Some(id), error: Some(RpcError { code, message: message.into(), data: None }), ..Default::default() }
    }

    pub fn kind(&self) -> Kind {
        match (&self.id, &self.method) {
            (Some(_), Some(_)) => Kind::Request,
            (None, Some(_)) => Kind::Notification,
            (Some(_), None) if self.result.is_some() || self.error.is_some() => Kind::Response,
            _ => Kind::Invalid,
        }
    }

    /// One line of JSON, newline-terminated.
    pub fn to_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).expect("JSON-RPC messages always serialize");
        line.push(b'\n');
        line
    }

    pub fn parse(line: &str) -> Result<Message, serde_json::Error> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_and_round_trips() {
        let request = Message::request("ruddr-1", "thread/start", json!({"cwd": "/w"}));
        assert_eq!(request.kind(), Kind::Request);
        let line = String::from_utf8(request.to_line()).unwrap();
        assert!(line.ends_with('\n') && !line.contains("jsonrpc"));
        assert_eq!(Message::parse(line.trim()).unwrap(), request);
        assert_eq!(Message::parse(r#"{"id":7,"result":{}}"#).unwrap().kind(), Kind::Response);
        assert_eq!(Message::parse(r#"{"method":"turn/completed","params":{}}"#).unwrap().kind(), Kind::Notification);
        assert_eq!(Id::Num(7).key(), Id::Str("7".into()).key());
    }
}
