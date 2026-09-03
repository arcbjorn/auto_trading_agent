//! DeepSeek V4 through its native chat-completions API (OpenAI wire format).
//!
//! The service keeps one internal conversation format, the Messages API content blocks, and this
//! client translates at the edge: system and user text, `tool_use` and `tool_result` blocks and
//! the per-turn `role: system` notes go out as chat-completion messages, and the reply comes back
//! as blocks. Thinking-mode reasoning is kept as a `reasoning` block on the assistant turn because
//! DeepSeek requires every earlier turn's `reasoning_content` to be sent back whenever the request
//! carries tools; dropping it is a 400.
//!
//! Endpoint: `POST {base_url}/chat/completions`, bearer authentication. Models: `deepseek-v4-pro`
//! and `deepseek-v4-flash` (1M context, up to 384K output, tool calls in thinking mode).

use crate::anthropic::{ApiError, Message};
use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Debug)]
pub struct DeepSeekConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    /// Thinking mode on or off. On, the model reasons before every answer and tool call.
    pub thinking: bool,
    /// `low`, `high` or `max` (DeepSeek's scale); only sent in thinking mode.
    pub reasoning_effort: String,
    pub timeout: Duration,
    pub max_attempts: u32,
}

/// Maps the service's five-level `EFFORT` onto DeepSeek's three levels.
pub fn reasoning_effort_for(effort: &str) -> &'static str {
    match effort {
        "low" => "low",
        "xhigh" | "max" => "max",
        _ => "high",
    }
}

impl DeepSeekConfig {
    /// Environment: `DEEPSEEK_API_KEY` (required unless `DEEPSEEK_BASE_URL` points at a mock),
    /// `DEEPSEEK_BASE_URL`, `DEEPSEEK_MODEL`, `DEEPSEEK_THINKING` (default 1), `EFFORT`
    /// (mapped: low -> low, medium/high -> high, xhigh/max -> max), `MAX_TOKENS`, `MODEL_TIMEOUT_SECS`.
    pub fn from_env() -> anyhow::Result<Self> {
        let base_url = std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        let api_key = match std::env::var("DEEPSEEK_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ if base_url != DEFAULT_BASE_URL => String::new(),
            _ => anyhow::bail!("DEEPSEEK_API_KEY is not set"),
        };
        let effort = std::env::var("EFFORT").unwrap_or_else(|_| "medium".into());
        Ok(Self {
            api_key,
            base_url,
            model: std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            max_tokens: std::env::var("MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16_000),
            thinking: std::env::var("DEEPSEEK_THINKING")
                .map(|v| !matches!(v.trim(), "0" | "false" | "no" | ""))
                .unwrap_or(true),
            reasoning_effort: std::env::var("DEEPSEEK_REASONING_EFFORT")
                .unwrap_or_else(|_| reasoning_effort_for(&effort).to_string()),
            timeout: Duration::from_secs(
                std::env::var("MODEL_TIMEOUT_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(600),
            ),
            max_attempts: 3,
        })
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Content blocks -> chat-completion messages. One block list can expand to several messages:
/// every `tool_result` becomes its own `tool` message, which the API requires directly after the
/// assistant turn that made the calls, and any text in the same user turn follows as a user message.
pub fn to_chat_messages(system: &str, messages: &[Value]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        let content = &m["content"];
        match role {
            "assistant" => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut tool_calls = Vec::new();
                if let Some(blocks) = content.as_array() {
                    for b in blocks {
                        match b["type"].as_str().unwrap_or("") {
                            "text" => text.push_str(b["text"].as_str().unwrap_or("")),
                            "reasoning" => reasoning.push_str(b["text"].as_str().unwrap_or("")),
                            "tool_use" => tool_calls.push(json!({
                                "id": b["id"],
                                "type": "function",
                                "function": { "name": b["name"], "arguments": b["input"].to_string() }
                            })),
                            _ => {}
                        }
                    }
                } else {
                    text = text_of(content);
                }
                let mut msg =
                    json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
                if !reasoning.is_empty() {
                    msg["reasoning_content"] = json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            "system" => out.push(json!({ "role": "system", "content": text_of(content) })),
            _ => {
                let mut texts = Vec::new();
                if let Some(blocks) = content.as_array() {
                    for b in blocks {
                        match b["type"].as_str().unwrap_or("") {
                            "tool_result" => out.push(json!({
                                "role": "tool",
                                "tool_call_id": b["tool_use_id"],
                                "content": text_of(&b["content"])
                            })),
                            "text" => texts.push(b["text"].as_str().unwrap_or("").to_string()),
                            _ => {}
                        }
                    }
                } else {
                    texts.push(text_of(content));
                }
                if !texts.is_empty() {
                    out.push(json!({ "role": "user", "content": texts.join("\n\n") }));
                }
            }
        }
    }
    out
}

/// Messages API tool definitions -> chat-completion function tools.
pub fn to_chat_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": { "name": t["name"], "description": t["description"], "parameters": t["input_schema"] }
            })
        })
        .collect()
}

/// A chat-completion reply -> the service's `Message`: `reasoning`, `text` and `tool_use` blocks,
/// a Messages API stop reason, and usage with DeepSeek's cache split mapped onto the same fields.
pub fn from_chat_completion(v: &Value) -> Result<Message, ApiError> {
    let choice = v["choices"]
        .get(0)
        .ok_or_else(|| ApiError::Decode("chat completion without choices".into()))?;
    let msg = &choice["message"];
    let mut content = Vec::new();
    if let Some(r) = msg["reasoning_content"].as_str().filter(|s| !s.is_empty()) {
        content.push(json!({ "type": "reasoning", "text": r }));
    }
    if let Some(t) = msg["content"].as_str().filter(|s| !s.is_empty()) {
        content.push(json!({ "type": "text", "text": t }));
    }
    let mut calls = 0;
    if let Some(tool_calls) = msg["tool_calls"].as_array() {
        for c in tool_calls {
            let arguments = c["function"]["arguments"].as_str().unwrap_or("{}");
            // Arguments arrive as a JSON string; anything unparsable becomes an empty object so
            // the tool layer reports the missing fields as a readable error.
            let input = serde_json::from_str::<Value>(arguments)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            content.push(json!({ "type": "tool_use", "id": c["id"], "name": c["function"]["name"], "input": input }));
            calls += 1;
        }
    }
    let stop_reason = match choice["finish_reason"].as_str().unwrap_or("stop") {
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        "content_filter" => "refusal",
        "stop" if calls > 0 => "tool_use",
        _ => "end_turn",
    };
    let u = &v["usage"];
    let hit = u["prompt_cache_hit_tokens"].as_u64().unwrap_or(0);
    let miss = u["prompt_cache_miss_tokens"]
        .as_u64()
        .unwrap_or_else(|| u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(hit));
    Ok(Message {
        id: v["id"].as_str().unwrap_or("").to_string(),
        model: v["model"].as_str().unwrap_or("").to_string(),
        stop_reason: Some(stop_reason.to_string()),
        content,
        usage: json!({
            "input_tokens": miss,
            "output_tokens": u["completion_tokens"].as_u64().unwrap_or(0),
            "cache_read_input_tokens": hit,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": u["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0)
        }),
        stop_details: Value::Null,
    })
}

pub struct DeepSeekClient {
    http: reqwest::Client,
    cfg: DeepSeekConfig,
}

impl DeepSeekClient {
    pub fn new(cfg: DeepSeekConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(cfg.timeout).build()?;
        Ok(Self { http, cfg })
    }

    pub fn config(&self) -> &DeepSeekConfig {
        &self.cfg
    }

    pub fn request_body(&self, system: &str, messages: &[Value], tools: &[Value]) -> Value {
        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "messages": to_chat_messages(system, messages),
            "thinking": { "type": if self.cfg.thinking { "enabled" } else { "disabled" } },
        });
        if self.cfg.thinking {
            body["reasoning_effort"] = json!(self.cfg.reasoning_effort);
        }
        if !tools.is_empty() {
            body["tools"] = json!(to_chat_tools(tools));
        }
        body
    }

    /// One `POST /chat/completions`, retried with backoff on 429, 5xx, connection errors and the
    /// API's `insufficient_system_resource` finish reason.
    pub async fn create(&self, system: &str, messages: &[Value], tools: &[Value]) -> Result<Message, ApiError> {
        let body = self.request_body(system, messages, tools);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self
                .http
                .post(format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/')))
                .header("content-type", "application/json");
            if !self.cfg.api_key.is_empty() {
                req = req.bearer_auth(&self.cfg.api_key);
            }
            let resp = match req.json(&body).send().await {
                Ok(r) => r,
                Err(e) if attempt < self.cfg.max_attempts => {
                    tracing::warn!(attempt, error = %e, "model request failed, retrying");
                    tokio::time::sleep(crate::anthropic::backoff(attempt)).await;
                    continue;
                }
                Err(e) => return Err(ApiError::Transport(e)),
            };
            let status = resp.status().as_u16();
            let text = resp.text().await?;
            if status == 429 || (500..600).contains(&status) {
                if attempt < self.cfg.max_attempts {
                    tracing::warn!(attempt, status, "model request rejected, retrying");
                    tokio::time::sleep(crate::anthropic::backoff(attempt)).await;
                    continue;
                }
                return Err(ApiError::Http { status, body: text });
            }
            if !(200..300).contains(&status) {
                return Err(ApiError::Http { status, body: text });
            }
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| ApiError::Decode(format!("{e}: {}", text.chars().take(200).collect::<String>())))?;
            if v["choices"][0]["finish_reason"] == "insufficient_system_resource" && attempt < self.cfg.max_attempts {
                tracing::warn!(attempt, "model reported insufficient system resources, retrying");
                tokio::time::sleep(crate::anthropic::backoff(attempt)).await;
                continue;
            }
            return from_chat_completion(&v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_become_chat_messages_and_back() {
        let history = vec![
            json!({ "role": "user", "content": "buy 0.5 eth at 3000" }),
            json!({ "role": "system", "content": "[service] This turn permits: place orders = yes" }),
            json!({ "role": "assistant", "content": [
                { "type": "reasoning", "text": "The user wants a limit buy." },
                { "type": "text", "text": "Placing it." },
                { "type": "tool_use", "id": "call_1", "name": "place_limit_order", "input": { "side": "buy", "price_usdc": "3000", "quantity_eth": "0.5" } }
            ] }),
            json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "{\"order_id\":\"7\"}", "is_error": false },
                { "type": "text", "text": "[service] note" }
            ] }),
        ];
        let chat = to_chat_messages("SYS", &history);
        assert_eq!(chat[0], json!({ "role": "system", "content": "SYS" }));
        assert_eq!(chat[1], json!({ "role": "user", "content": "buy 0.5 eth at 3000" }));
        assert_eq!(chat[2]["role"], "system");
        assert_eq!(chat[3]["reasoning_content"], "The user wants a limit buy.");
        assert_eq!(chat[3]["content"], "Placing it.");
        assert_eq!(chat[3]["tool_calls"][0]["function"]["name"], "place_limit_order");
        let args: Value =
            serde_json::from_str(chat[3]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["price_usdc"], "3000");
        assert_eq!(
            chat[4],
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "{\"order_id\":\"7\"}" })
        );
        assert_eq!(chat[5], json!({ "role": "user", "content": "[service] note" }));

        let tools = to_chat_tools(&[json!({ "name": "t", "description": "d", "input_schema": { "type": "object" } })]);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");

        let reply = json!({
            "id": "c1", "model": "deepseek-v4-flash",
            "choices": [ { "index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null, "reasoning_content": "Need the market first.",
                "tool_calls": [ { "id": "call_2", "type": "function", "function": { "name": "get_market_summary", "arguments": "{}" } },
                                { "id": "call_3", "type": "function", "function": { "name": "get_quote", "arguments": "not json" } } ] } } ],
            "usage": { "prompt_tokens": 1200, "completion_tokens": 80, "prompt_cache_hit_tokens": 1000, "prompt_cache_miss_tokens": 200,
                       "completion_tokens_details": { "reasoning_tokens": 60 } }
        });
        let m = from_chat_completion(&reply).unwrap();
        assert_eq!(m.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(m.content[0]["type"], "reasoning");
        assert_eq!(m.tool_uses().count(), 2);
        assert_eq!(m.content[2]["input"], json!({}));
        assert_eq!(
            (m.input_tokens(), m.cache_read_tokens(), m.output_tokens()),
            (200, 1000, 80)
        );
        // The reply's blocks go straight back into the history and carry the reasoning with them.
        let next = to_chat_messages("SYS", &[json!({ "role": "assistant", "content": m.content })]);
        assert_eq!(next[1]["reasoning_content"], "Need the market first.");
        assert!(next[1]["content"].is_null());
        assert_eq!(next[1]["tool_calls"].as_array().unwrap().len(), 2);

        let done = json!({ "choices": [ { "finish_reason": "stop", "message": { "content": "Done." } } ], "usage": { "prompt_tokens": 5, "completion_tokens": 1 } });
        let m = from_chat_completion(&done).unwrap();
        assert_eq!(
            (m.stop_reason.as_deref(), m.text().as_str(), m.input_tokens()),
            (Some("end_turn"), "Done.", 5)
        );
        assert!(from_chat_completion(&json!({ "error": "x" })).is_err());
        assert_eq!(reasoning_effort_for("medium"), "high");
        assert_eq!(reasoning_effort_for("xhigh"), "max");
    }

    #[test]
    fn request_carries_thinking_and_function_tools() {
        let c = DeepSeekClient::new(DeepSeekConfig {
            api_key: String::new(),
            base_url: DEFAULT_BASE_URL.into(),
            model: "deepseek-v4-pro".into(),
            max_tokens: 100,
            thinking: true,
            reasoning_effort: "high".into(),
            timeout: Duration::from_secs(1),
            max_attempts: 1,
        })
        .unwrap();
        let body = c.request_body(
            "sys",
            &[json!({ "role": "user", "content": "hi" })],
            &[json!({ "name": "t", "description": "d", "input_schema": {} })],
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["tools"][0]["function"]["name"], "t");
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body.get("cache_control").is_none() && body.get("output_config").is_none());
    }
}
