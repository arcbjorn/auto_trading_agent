//! The tool loop. One call to [`Agent::chat_turn`] is one user message: the model is called,
//! its tool calls are gated and executed against the MCP server, results are fed back, and the
//! loop ends when the model answers in text. Sessions hold the append-only message history.

use crate::anthropic::{AnthropicClient, ApiError};
use crate::audit::Audit;
use crate::gate::{self, ConfirmationGate, Intercept, PendingConfirmation, ACTION_TOOLS};
use crate::mcp_client::{McpClient, McpError};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Session {
    pub id: String,
    /// Messages API history, append-only.
    pub messages: Vec<Value>,
    pub turns: u32,
    pub pending: Option<PendingConfirmation>,
    pub last_order_id: Option<String>,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Hard cap on model calls per turn.
    pub max_iterations: usize,
    /// Orders at or above this many lots need a confirmation turn (default 1 ETH).
    pub confirm_threshold_lots: u64,
    pub confirm_ttl: Duration,
    /// Offer action tools only when the user's words carry the intent.
    pub gate_tools: bool,
    /// Cancel an order the verifier flags as unrequested, when it is still open.
    pub compensate: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 8,
            confirm_threshold_lots: 10_000,
            confirm_ttl: Duration::from_secs(600),
            gate_tools: true,
            compensate: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("model: {0}")]
    Model(#[from] ApiError),
    #[error("mcp: {0}")]
    Mcp(#[from] McpError),
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: Value,
    pub result: String,
    pub is_error: bool,
    /// Answered by the service (gate or confirmation) without reaching the MCP server.
    pub intercepted: bool,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnResult {
    pub session_id: String,
    pub turn: u32,
    pub reply: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub usage: Usage,
    pub model: String,
    pub stop_reason: String,
    pub iterations: u32,
    pub latency_ms: u64,
    pub model_latency_ms: u64,
    pub flags: Vec<String>,
}

pub struct Agent {
    claude: AnthropicClient,
    mcp: McpClient,
    tools: Vec<Value>,
    cfg: AgentConfig,
    audit: Audit,
    system: String,
}

/// An MCP tool definition has exactly what a Messages API tool needs: name, description, schema.
pub fn api_tools(mcp_tools: &[Value]) -> Vec<Value> {
    mcp_tools
        .iter()
        .map(|t| json!({ "name": t["name"], "description": t["description"].as_str().unwrap_or(""), "input_schema": t["inputSchema"] }))
        .collect()
}

impl Agent {
    pub async fn new(
        claude: AnthropicClient,
        mcp: McpClient,
        cfg: AgentConfig,
        audit: Audit,
    ) -> Result<Self, AgentError> {
        let tools = api_tools(&mcp.list_tools().await?);
        Ok(Self {
            claude,
            mcp,
            tools,
            cfg,
            audit,
            system: crate::SYSTEM_PROMPT.to_string(),
        })
    }

    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    pub fn config(&self) -> &AgentConfig {
        &self.cfg
    }

    pub async fn chat_turn(&self, session: &mut Session, user_text: &str) -> Result<TurnResult, AgentError> {
        let started = Instant::now();
        session.turns += 1;
        let turn = session.turns;
        let pending_before = session.pending.is_some();
        let offered = gate::offered_tools(&self.tools, user_text, pending_before, self.cfg.gate_tools);
        let offered_names: HashSet<String> = offered
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        let confirmation_turn = pending_before && gate::mentions_confirmation(user_text);
        let confirm = ConfirmationGate {
            threshold_lots: self.cfg.confirm_threshold_lots,
            ttl: self.cfg.confirm_ttl,
        };
        session.messages.push(json!({ "role": "user", "content": user_text }));

        let mut records = Vec::new();
        let mut usage = Usage::default();
        let mut model = String::new();
        let mut model_latency = Duration::ZERO;
        let mut flags = Vec::new();
        let mut executed: Vec<(String, bool)> = Vec::new();
        let mut placed_ids: Vec<String> = Vec::new();
        let mut stop_reason: String;
        let mut reply: String;
        let mut iterations = 0u32;

        loop {
            if iterations as usize >= self.cfg.max_iterations {
                flags.push("max_iterations".into());
                stop_reason = "max_iterations".into();
                reply = "I stopped before finishing this request; please rephrase or try again.".into();
                break;
            }
            iterations += 1;
            let t0 = Instant::now();
            let msg = self.claude.create(&self.system, &session.messages, &offered).await?;
            model_latency += t0.elapsed();
            usage.input_tokens += msg.input_tokens();
            usage.output_tokens += msg.output_tokens();
            model = msg.model.clone();
            stop_reason = msg.stop_reason.clone().unwrap_or_default();
            session
                .messages
                .push(json!({ "role": "assistant", "content": msg.content }));

            match stop_reason.as_str() {
                "tool_use" => {
                    let mut results = Vec::new();
                    for block in msg.tool_uses() {
                        let name = block["name"].as_str().unwrap_or("").to_string();
                        let id = block["id"].as_str().unwrap_or("").to_string();
                        let mut args = if block["input"].is_object() {
                            block["input"].clone()
                        } else {
                            json!({})
                        };
                        let t1 = Instant::now();
                        let (text, is_error, intercepted) = if !offered_names.contains(&name) {
                            flags.push(format!("tool_not_offered:{name}"));
                            (
                                format!("{name} is not available on this turn; the user did not ask for it"),
                                true,
                                true,
                            )
                        } else {
                            if name == "place_limit_order" && args["client_order_id"].as_str().is_none_or(str::is_empty)
                            {
                                // Idempotency key: a retried tool call can never place a second order.
                                args["client_order_id"] = json!(format!("{}-{turn}-{id}", session.id));
                            }
                            match confirm.intercept(&mut session.pending, &name, &args, &session.id, turn) {
                                Intercept::Reply(v) => {
                                    if v["needs_confirmation"] == true {
                                        flags.push("confirmation_requested".into());
                                    }
                                    (v.to_string(), false, true)
                                }
                                Intercept::Proceed(clean) => match self.mcp.call_tool(&name, &clean).await {
                                    Ok(r) => {
                                        let ok =
                                            !r.is_error && r.structured.as_ref().is_none_or(|s| s["rejected"] != true);
                                        if ACTION_TOOLS.contains(&name.as_str()) {
                                            executed.push((name.clone(), ok));
                                        }
                                        if name == "place_limit_order" && ok {
                                            if let Some(oid) =
                                                r.structured.as_ref().and_then(|s| s["order_id"].as_str())
                                            {
                                                placed_ids.push(oid.to_string());
                                                session.last_order_id = Some(oid.to_string());
                                            }
                                        }
                                        (r.text, r.is_error, false)
                                    }
                                    Err(e) => {
                                        flags.push(format!("mcp_error:{name}"));
                                        (format!("tool call failed: {e}"), true, false)
                                    }
                                },
                            }
                        };
                        records.push(ToolCallRecord {
                            name,
                            args,
                            result: text.clone(),
                            is_error,
                            intercepted,
                            latency_ms: t1.elapsed().as_millis() as u64,
                        });
                        results.push(
                            json!({ "type": "tool_result", "tool_use_id": id, "content": text, "is_error": is_error }),
                        );
                    }
                    if results.is_empty() {
                        flags.push("tool_use_without_blocks".into());
                        reply = msg.text();
                        break;
                    }
                    // All results of one assistant turn go back in one user message.
                    session.messages.push(json!({ "role": "user", "content": results }));
                }
                "refusal" => {
                    flags.push("refusal".into());
                    reply = "I can't help with that request.".into();
                    break;
                }
                "max_tokens" => {
                    flags.push("truncated".into());
                    reply = msg.text();
                    break;
                }
                "pause_turn" => continue,
                _ => {
                    reply = msg.text();
                    break;
                }
            }
        }

        flags.extend(gate::verify(user_text, &executed, confirmation_turn));
        if self.cfg.compensate && flags.iter().any(|f| f == "intent_mismatch:place_limit_order") {
            for oid in &placed_ids {
                match self.mcp.call_tool("cancel_order", &json!({ "order_id": oid })).await {
                    Ok(r) if !r.is_error => {
                        flags.push(format!("compensated:cancel:{oid}"));
                        reply.push_str(
                            " (An order was placed without an explicit instruction from you and has been cancelled.)",
                        );
                    }
                    _ => flags.push(format!("compensation_failed:{oid}")),
                }
            }
        }

        let result = TurnResult {
            session_id: session.id.clone(),
            turn,
            reply,
            tool_calls: records,
            usage,
            model,
            stop_reason,
            iterations,
            latency_ms: started.elapsed().as_millis() as u64,
            model_latency_ms: model_latency.as_millis() as u64,
            flags,
        };
        self.audit.write(&json!({
            "ts_unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
            "session": session.id,
            "turn": turn,
            "user": user_text,
            "offered_tools": offered_names.iter().collect::<Vec<_>>(),
            "result": result,
        }));
        Ok(result)
    }
}
