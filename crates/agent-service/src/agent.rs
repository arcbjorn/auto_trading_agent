//! The tool loop. One call to [`Agent::chat_turn`] is one user message: the model is called,
//! its tool calls are gated and executed against the MCP server, results are fed back, and the
//! loop ends when the model answers in text. Sessions hold the append-only message history.

use crate::anthropic::ApiError;
use crate::audit::Audit;
use crate::gate::{self, ConfirmationGate, Executed, Intercept, PendingConfirmation, Permissions, ACTION_TOOLS};
use crate::mcp_client::{McpClient, McpError};
use crate::model::ModelClient;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Where the per-turn permission note goes. The operator channel is a `role: "system"` message
/// appended after the user's message: the model cannot mistake it for user text and user text
/// cannot forge it. Not every model accepts it, so the note can also travel as a second text block
/// inside the user message; enforcement in code is the same either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteChannel {
    System,
    User,
}

impl NoteChannel {
    /// Mid-conversation system messages are accepted by Claude Opus 5 and 4.8 and the Fable and
    /// Mythos models; other models get the note inside the user turn.
    pub fn for_model(model: &str) -> Self {
        const SUPPORTED: [&str; 4] = ["claude-opus-5", "claude-opus-4-8", "claude-fable", "claude-mythos"];
        if SUPPORTED.iter().any(|p| model.starts_with(p)) {
            NoteChannel::System
        } else {
            NoteChannel::User
        }
    }
}

#[derive(Debug, Default)]
pub struct Session {
    pub id: String,
    /// Messages API history, append-only.
    pub messages: Vec<Value>,
    pub turns: u32,
    pub pending: Option<PendingConfirmation>,
    pub last_order_id: Option<String>,
    /// Recent `(request_id, response)` pairs, so a retried `POST /chat` returns the same answer
    /// instead of running the turn (and its actions) again.
    pub responses: Vec<(String, Value)>,
    /// Start times of recent turns, for the per-session rate limit.
    pub turn_times: std::collections::VecDeque<Instant>,
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
    /// An order at a price the user never stated needs a confirmation turn, whatever its size.
    pub confirm_unpriced: bool,
    pub confirm_ttl: Duration,
    /// Permit action tools only when the user's words carry the intent. The tool list itself is
    /// always the same; a call outside the turn's permission is refused by the service.
    pub gate_tools: bool,
    /// Cancel an order the verifier flags as unrequested, when it is still open.
    pub compensate: bool,
    /// How the permission note reaches the model; see [`NoteChannel`].
    pub note_channel: NoteChannel,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 8,
            confirm_threshold_lots: 10_000,
            confirm_unpriced: true,
            confirm_ttl: Duration::from_secs(600),
            gate_tools: true,
            compensate: true,
            note_channel: NoteChannel::User,
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
    /// Uncached prompt tokens, billed at the full input price.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
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
    /// Which action tools this turn was allowed to execute.
    pub permitted: Vec<String>,
}

pub struct Agent {
    model: ModelClient,
    mcp: McpClient,
    tools: Vec<Value>,
    cfg: AgentConfig,
    audit: Audit,
    system: String,
    /// Set once the API has rejected a system-role message: every later turn uses the user channel.
    system_channel_rejected: AtomicBool,
}

/// An MCP tool definition has exactly what a Messages API tool needs: name, description, schema.
/// The list is sorted by name so it is byte-identical on every request (it is the cache prefix),
/// and the action tools gain the service's `confirmation_token` field once, here.
pub fn api_tools(mcp_tools: &[Value]) -> Vec<Value> {
    let mut tools: Vec<Value> = mcp_tools
        .iter()
        .map(|t| json!({ "name": t["name"], "description": t["description"].as_str().unwrap_or(""), "input_schema": t["inputSchema"] }))
        .collect();
    tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    for t in &mut tools {
        if ACTION_TOOLS.iter().any(|a| t["name"] == *a) {
            t["input_schema"]["properties"]["confirmation_token"] = json!({
                "type": "string",
                "description": "Only after the user confirmed an action that needed confirmation: the token from the needs_confirmation result."
            });
        }
    }
    tools
}

impl Agent {
    pub async fn new(
        model: impl Into<ModelClient>,
        mcp: McpClient,
        cfg: AgentConfig,
        audit: Audit,
    ) -> Result<Self, AgentError> {
        let tools = api_tools(&mcp.list_tools().await?);
        Ok(Self {
            model: model.into(),
            mcp,
            tools,
            cfg,
            audit,
            system: crate::SYSTEM_PROMPT.to_string(),
            system_channel_rejected: AtomicBool::new(false),
        })
    }

    /// The channel in use right now: the configured one unless the API has rejected it.
    pub fn note_channel(&self) -> NoteChannel {
        if self.cfg.note_channel == NoteChannel::System && !self.system_channel_rejected.load(Ordering::Relaxed) {
            NoteChannel::System
        } else {
            NoteChannel::User
        }
    }

    /// Appends the user's words and, with the gate on, the permission note for this turn.
    fn push_user_turn(&self, session: &mut Session, user_text: &str, note: Option<&str>, channel: NoteChannel) {
        match (note, channel) {
            (None, _) => session.messages.push(json!({ "role": "user", "content": user_text })),
            (Some(note), NoteChannel::System) => {
                session.messages.push(json!({ "role": "user", "content": user_text }));
                session.messages.push(json!({ "role": "system", "content": note }));
            }
            (Some(note), NoteChannel::User) => session.messages.push(json!({
                "role": "user",
                "content": [ { "type": "text", "text": user_text }, { "type": "text", "text": note } ]
            })),
        }
    }

    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    pub fn model(&self) -> &ModelClient {
        &self.model
    }

    pub fn config(&self) -> &AgentConfig {
        &self.cfg
    }

    pub async fn chat_turn(&self, session: &mut Session, user_text: &str) -> Result<TurnResult, AgentError> {
        let started = Instant::now();
        session.turns += 1;
        let turn = session.turns;
        let pending_before = session.pending.is_some();
        let pending_tool = session.pending.as_ref().map(|p| p.tool.clone());
        let permissions = Permissions::for_turn(user_text, pending_tool.as_deref(), self.cfg.gate_tools);
        let permitted: Vec<String> = ACTION_TOOLS
            .iter()
            .filter(|t| permissions.allows(t))
            .map(|t| t.to_string())
            .collect();
        let confirmation_turn = pending_before && gate::mentions_confirmation(user_text);
        let confirming = session
            .pending
            .as_ref()
            .filter(|_| confirmation_turn)
            .map(|p| (p.tool.clone(), p.summary.clone(), p.token.clone()));
        let confirm = ConfirmationGate {
            threshold_lots: self.cfg.confirm_threshold_lots,
            confirm_unpriced: self.cfg.confirm_unpriced,
            ttl: self.cfg.confirm_ttl,
        };
        // The user's words, then the service's note on what this turn permits. Both are appended
        // to the history and never edited afterwards.
        let note = self.cfg.gate_tools.then(|| match &confirming {
            Some((tool, summary, token)) => Permissions::note_confirming(tool, summary, token),
            None => permissions.note(),
        });
        let mut channel = self.note_channel();
        self.push_user_turn(session, user_text, note.as_deref(), channel);
        let turn_start = session.messages.len()
            - if note.is_some() && channel == NoteChannel::System {
                2
            } else {
                1
            };

        let mut records = Vec::new();
        let mut usage = Usage::default();
        let mut model = String::new();
        let mut model_latency = Duration::ZERO;
        let mut flags = Vec::new();
        let mut executed: Vec<Executed> = Vec::new();
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
            let msg = match self.model.create(&self.system, &session.messages, &self.tools).await {
                Ok(m) => m,
                // A model that does not accept system-role messages says so with a 400 before
                // producing anything, so the turn can be re-sent on the user channel. The request
                // that failed left no assistant content, so replacing this turn's own messages is
                // not an edit of any history the model has seen.
                Err(ApiError::Http { status: 400, body })
                    if iterations == 1
                        && channel == NoteChannel::System
                        && body.contains("system")
                        && body.contains("role") =>
                {
                    tracing::warn!(model = %self.model.model_id(), "model rejects system-role messages; using the user channel from now on");
                    self.system_channel_rejected.store(true, Ordering::Relaxed);
                    flags.push("note_channel_downgraded".into());
                    channel = NoteChannel::User;
                    session.messages.truncate(turn_start);
                    self.push_user_turn(session, user_text, note.as_deref(), channel);
                    self.model.create(&self.system, &session.messages, &self.tools).await?
                }
                Err(e) => return Err(e.into()),
            };
            model_latency += t0.elapsed();
            usage.input_tokens += msg.input_tokens();
            usage.output_tokens += msg.output_tokens();
            usage.cache_read_input_tokens += msg.cache_read_tokens();
            usage.cache_creation_input_tokens += msg.cache_creation_tokens();
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
                        let (text, is_error, intercepted) = {
                            if name == "place_limit_order" && args["client_order_id"].as_str().is_none_or(str::is_empty)
                            {
                                // Idempotency key: a retried tool call can never place a second order.
                                args["client_order_id"] = json!(format!("{}-{turn}-{id}", session.id));
                            }
                            let permitted = permissions.allows(&name);
                            match confirm.intercept(
                                &mut session.pending,
                                &name,
                                &args,
                                &session.id,
                                turn,
                                user_text,
                                permitted,
                            ) {
                                Intercept::Reply(v) => {
                                    if v["needs_confirmation"] == true {
                                        flags.push(if permitted {
                                            "confirmation_requested".into()
                                        } else {
                                            format!("confirmation_requested:no_intent:{name}")
                                        });
                                    }
                                    (v.to_string(), false, true)
                                }
                                Intercept::Proceed(clean) => match self.mcp.call_tool(&name, &clean).await {
                                    Ok(r) => {
                                        let ok =
                                            !r.is_error && r.structured.as_ref().is_none_or(|s| s["rejected"] != true);
                                        if ACTION_TOOLS.contains(&name.as_str()) {
                                            executed.push(Executed {
                                                tool: name.clone(),
                                                args: clean.clone(),
                                                ok,
                                            });
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
            permitted,
        };
        self.audit.write(&json!({
            "ts_unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
            "session": session.id,
            "turn": turn,
            "user": user_text,
            "result": result,
        }));
        Ok(result)
    }
}
