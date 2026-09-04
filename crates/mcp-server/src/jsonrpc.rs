//! JSON-RPC 2.0 envelopes, as used by MCP. Only what the protocol needs.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// MCP-specific: `resources/read` for an unknown URI.
pub const RESOURCE_NOT_FOUND: i64 = -32002;

#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub jsonrpc: Option<String>,
    /// Absent for notifications. MCP forbids `null` ids.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("Method not found: {method}"))
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }
}

/// Validates the envelope. Returns `Err` for anything that is not a JSON-RPC 2.0 request or
/// notification; the caller answers with `INVALID_REQUEST` (or `PARSE_ERROR` before this point).
pub fn parse(msg: &Value) -> Result<Request, RpcError> {
    if !msg.is_object() {
        return Err(RpcError::new(INVALID_REQUEST, "expected a JSON-RPC object"));
    }
    // `id: null` must be told apart from an absent id (a notification) before serde folds both into None.
    if let Some(id) = msg.get("id") {
        if id.is_null() {
            return Err(RpcError::new(INVALID_REQUEST, "id must not be null"));
        }
        if !(id.is_string() || id.is_number()) {
            return Err(RpcError::new(INVALID_REQUEST, "id must be a string or a number"));
        }
    }
    let req: Request = serde_json::from_value(msg.clone())
        .map_err(|e| RpcError::new(INVALID_REQUEST, format!("invalid request: {e}")))?;
    if req.jsonrpc.as_deref() != Some("2.0") {
        return Err(RpcError::new(INVALID_REQUEST, "jsonrpc must be \"2.0\""));
    }
    if let Some(p) = &req.params {
        if !(p.is_object() || p.is_array()) {
            return Err(RpcError::new(INVALID_REQUEST, "params must be an object or an array"));
        }
    }
    Ok(req)
}

pub fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn failure(id: Value, error: RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// Typed access to an object parameter.
pub fn param<'a>(params: &'a Value, name: &str) -> Option<&'a Value> {
    params.get(name)
}

pub fn param_str<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    param(params, name)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("missing or non-string parameter: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_requests_and_notifications() {
        let r = parse(&json!({"jsonrpc":"2.0","id":1,"method":"ping"})).unwrap();
        assert_eq!((r.id, r.method.as_str()), (Some(json!(1)), "ping"));
        let n = parse(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
        assert!(n.id.is_none());
    }

    #[test]
    fn rejects_malformed_envelopes() {
        assert_eq!(parse(&json!([1, 2])).unwrap_err().code, INVALID_REQUEST);
        assert_eq!(
            parse(&json!({"id":1,"method":"ping"})).unwrap_err().code,
            INVALID_REQUEST
        );
        assert_eq!(
            parse(&json!({"jsonrpc":"2.0","id":null,"method":"ping"}))
                .unwrap_err()
                .code,
            INVALID_REQUEST
        );
        assert_eq!(
            parse(&json!({"jsonrpc":"2.0","id":1})).unwrap_err().code,
            INVALID_REQUEST
        );
        assert_eq!(
            parse(&json!({"jsonrpc":"2.0","id":1,"method":"x","params":3}))
                .unwrap_err()
                .code,
            INVALID_REQUEST
        );
    }
}
