//! JSON-RPC client for an MCP server's Streamable HTTP endpoint (stateless mode).

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("MCP server answered HTTP {0}")]
    Http(u16),
    #[error("JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("malformed MCP response: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub text: String,
    pub structured: Option<Value>,
    pub is_error: bool,
}

pub struct McpClient {
    http: reqwest::Client,
    url: String,
    protocol_version: String,
    next_id: AtomicU64,
}

impl McpClient {
    /// Runs the initialize handshake and returns a ready client.
    pub async fn connect(url: &str) -> Result<Self, McpError> {
        let client = Self {
            // A stalled MCP server must surface as a tool error, not a hung turn.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(
                    std::env::var("MCP_TIMEOUT_SECS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(30),
                ))
                .build()?,
            url: url.to_string(),
            protocol_version: "2025-11-25".into(),
            next_id: AtomicU64::new(1),
        };
        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "agent-service", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        let mut client = client;
        if let Some(v) = init["protocolVersion"].as_str() {
            client.protocol_version = v.to_string();
        }
        client.notify("notifications/initialized").await?;
        Ok(client)
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    async fn post(&self, body: Value) -> Result<(u16, String), McpError> {
        let resp = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", &self.protocol_version)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        Ok((status, resp.text().await?))
    }

    async fn notify(&self, method: &str) -> Result<(), McpError> {
        let (status, _) = self.post(json!({ "jsonrpc": "2.0", "method": method })).await?;
        if status == 202 || status == 200 {
            Ok(())
        } else {
            Err(McpError::Http(status))
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (status, text) = self
            .post(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        if status != 200 {
            return Err(McpError::Http(status));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| McpError::Malformed(e.to_string()))?;
        if let Some(err) = v.get("error") {
            return Err(McpError::Rpc {
                code: err["code"].as_i64().unwrap_or(0),
                message: err["message"].as_str().unwrap_or("").into(),
            });
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| McpError::Malformed("response without result".into()))
    }

    pub async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let r = self.request("tools/list", json!({})).await?;
        r["tools"]
            .as_array()
            .cloned()
            .ok_or_else(|| McpError::Malformed("tools/list without tools".into()))
    }

    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<ToolResult, McpError> {
        let r = self
            .request("tools/call", json!({ "name": name, "arguments": arguments }))
            .await?;
        let text = r["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(ToolResult {
            text,
            structured: r.get("structuredContent").cloned(),
            is_error: r["isError"].as_bool().unwrap_or(false),
        })
    }
}
