//! The MCP methods. Transport-agnostic: both stdio and HTTP hand every message to
//! [`McpServer::handle_message`] and write back whatever it returns.
//!
//! Implemented: `initialize`, `ping`, `tools/list`, `tools/call`, `resources/list`,
//! `resources/templates/list`, `resources/read`, `resources/subscribe`, `resources/unsubscribe`,
//! `prompts/list`, `prompts/get`, and `logging/setLevel` (accepted, no-op). Client notifications
//! are consumed silently. Resource-updated notifications go out over stdio only.

use crate::jsonrpc::{self, PARSE_ERROR, RESOURCE_NOT_FOUND, Request, RpcError, param, param_str};
use crate::tools::ToolSet;
use serde_json::{Value, json};

/// Protocol versions this server speaks. The subset it implements is identical across them.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

pub const SERVER_NAME: &str = "clob-mcp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const INSTRUCTIONS: &str = "ETH/USDC central limit order book for one trading account. \
Prices are USDC per ETH with at most 2 decimals (tick 0.01); quantities are ETH with at most 4 decimals (lot 0.0001). \
Read tools (get_market_summary, get_order_book, get_quote, get_order, get_balances, get_statement, list_orders, list_trades) are always safe. \
place_limit_order, cancel_order and cancel_all_orders change the account's orders: call them only when the user explicitly asked to trade or cancel, \
and call get_quote before placing an order above 1 ETH. There are no market orders; propose a limit price instead. \
Never invent order ids: take them from list_orders or from a previous place_limit_order result.";

pub const PROMPT_NAME: &str = "trading_assistant";

pub struct McpServer {
    tools: ToolSet,
    metrics: crate::metrics::Metrics,
    /// Resource URIs a client subscribed to. Notifications need a server-to-client stream, which
    /// only the stdio transport has; the HTTP transport refuses subscriptions up front.
    subscriptions: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl McpServer {
    pub fn new(tools: ToolSet) -> Self {
        Self {
            tools,
            metrics: crate::metrics::Metrics::default(),
            subscriptions: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        }
    }

    /// The URIs a client asked to be told about, in a stable order.
    pub fn metrics(&self) -> &crate::metrics::Metrics {
        &self.metrics
    }

    pub fn subscriptions(&self) -> Vec<String> {
        self.subscriptions
            .lock()
            .expect("subscriptions")
            .iter()
            .cloned()
            .collect()
    }

    /// `notifications/resources/updated` for every subscribed resource. The transport sends these
    /// after engine events, coalesced so a burst of fills is one notification per resource.
    pub fn updated_notifications(&self) -> Vec<Value> {
        self.subscriptions()
            .into_iter()
            .map(|uri| json!({ "jsonrpc": "2.0", "method": "notifications/resources/updated", "params": { "uri": uri } }))
            .collect()
    }

    fn subscribe(&self, params: &Value, on: bool) -> Result<Value, RpcError> {
        let uri = param_str(params, "uri")?;
        let known =
            self.tools.resource_list().iter().any(|r| r["uri"] == uri) || uri.starts_with("market://ETH-USDC/book/");
        if !known {
            return Err(RpcError::new(RESOURCE_NOT_FOUND, "Resource not found").with_data(json!({ "uri": uri })));
        }
        let mut subs = self.subscriptions.lock().expect("subscriptions");
        if on {
            subs.insert(uri.to_string());
        } else {
            subs.remove(uri);
        }
        Ok(json!({}))
    }

    pub fn tools(&self) -> &ToolSet {
        &self.tools
    }

    /// Parses raw bytes and handles one message (or a legacy batch). Returns the bytes to send
    /// back, or `None` when nothing must be sent (notifications).
    pub async fn handle_bytes(&self, raw: &[u8]) -> Option<Value> {
        match serde_json::from_slice::<Value>(raw) {
            Ok(v) => self.handle_message(v).await,
            Err(e) => Some(jsonrpc::failure(
                Value::Null,
                RpcError::new(PARSE_ERROR, format!("parse error: {e}")),
            )),
        }
    }

    pub async fn handle_message(&self, msg: Value) -> Option<Value> {
        match msg {
            Value::Array(items) => {
                let mut out = Vec::new();
                for item in items {
                    if let Some(r) = self.handle_one(item).await {
                        out.push(r);
                    }
                }
                if out.is_empty() { None } else { Some(Value::Array(out)) }
            }
            other => self.handle_one(other).await,
        }
    }

    async fn handle_one(&self, msg: Value) -> Option<Value> {
        let req = match jsonrpc::parse(&msg) {
            Ok(r) => r,
            Err(e) => {
                let id = msg
                    .get("id")
                    .cloned()
                    .filter(|v| v.is_string() || v.is_number())
                    .unwrap_or(Value::Null);
                return Some(jsonrpc::failure(id, e));
            }
        };
        if req.method.starts_with("notifications/") {
            tracing::debug!(method = %req.method, "notification");
            return None;
        }
        let Some(id) = req.id.clone() else {
            tracing::warn!(method = %req.method, "request without id ignored");
            return None;
        };
        let params = req.params.clone().unwrap_or_else(|| json!({}));
        let started = std::time::Instant::now();
        let result = self.dispatch(&req, params).await;
        tracing::debug!(method = %req.method, ok = result.is_ok(), elapsed_us = started.elapsed().as_micros() as u64, "handled");
        Some(match result {
            Ok(v) => jsonrpc::success(id, v),
            Err(e) => jsonrpc::failure(id, e),
        })
    }

    async fn dispatch(&self, req: &Request, params: Value) -> Result<Value, RpcError> {
        match req.method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools.definitions() })),
            "tools/call" => self.tools_call(&params).await,
            "resources/list" => Ok(json!({ "resources": self.tools.resource_list() })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": self.tools.resource_templates() })),
            "resources/read" => self.resources_read(&params).await,
            "resources/subscribe" => self.subscribe(&params, true),
            "resources/unsubscribe" => self.subscribe(&params, false),
            "prompts/list" => Ok(json!({ "prompts": [prompt_definition()] })),
            "prompts/get" => self.prompts_get(&params),
            "logging/setLevel" => Ok(json!({})),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = param(params, "protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(LATEST_PROTOCOL_VERSION);
        let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION
        };
        let client = params.get("clientInfo").cloned().unwrap_or(Value::Null);
        tracing::info!(requested, negotiated = version, %client, "initialize");
        json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": true, "listChanged": false },
                "prompts": { "listChanged": false }
            },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            "instructions": INSTRUCTIONS
        })
    }

    async fn tools_call(&self, params: &Value) -> Result<Value, RpcError> {
        let name = param_str(params, "name")?;
        let args = match param(params, "arguments") {
            None | Some(Value::Null) => json!({}),
            Some(v) if v.is_object() => v.clone(),
            Some(_) => return Err(RpcError::invalid_params("arguments must be an object")),
        };
        let out = self.tools.call(name, &args).await?;
        let outcome = if out.is_error {
            "error"
        } else if out.structured.as_ref().is_some_and(|s| s["rejected"] == true) {
            if let Some(code) = out.structured.as_ref().and_then(|s| s["code"].as_str()) {
                self.metrics.rejection(code);
            }
            "rejected"
        } else {
            "ok"
        };
        self.metrics.tool_call(name, outcome);
        let mut result = json!({
            "content": [ { "type": "text", "text": out.text } ],
            "isError": out.is_error
        });
        if let Some(s) = out.structured {
            result["structuredContent"] = s;
        }
        Ok(result)
    }

    async fn resources_read(&self, params: &Value) -> Result<Value, RpcError> {
        let uri = param_str(params, "uri")?;
        match self.tools.read_resource(uri).await? {
            Some((mime, text)) => Ok(json!({ "contents": [ { "uri": uri, "mimeType": mime, "text": text } ] })),
            None => Err(RpcError::new(RESOURCE_NOT_FOUND, "Resource not found").with_data(json!({ "uri": uri }))),
        }
    }

    fn prompts_get(&self, params: &Value) -> Result<Value, RpcError> {
        let name = param_str(params, "name")?;
        if name != PROMPT_NAME {
            return Err(RpcError::invalid_params(format!("Unknown prompt: {name}")));
        }
        let account = self.tools.account();
        Ok(json!({
            "description": "Standing instructions for trading ETH/USDC on this account through the CLOB tools.",
            "messages": [ {
                "role": "user",
                "content": { "type": "text", "text": format!(
                    "You are a trading assistant for account {account} on the ETH/USDC order book. {INSTRUCTIONS} \
                     Start by calling get_market_summary, then answer in one or two sentences with the numbers that matter."
                ) }
            } ]
        }))
    }
}

fn prompt_definition() -> Value {
    json!({
        "name": PROMPT_NAME,
        "title": "Trading assistant",
        "description": "Standing instructions for trading ETH/USDC on this account through the CLOB tools.",
        "arguments": []
    })
}
