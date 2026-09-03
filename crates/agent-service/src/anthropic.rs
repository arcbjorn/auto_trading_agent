//! Minimal Messages API client: one endpoint, one JSON body, typed just enough to drive the loop.

use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-5";
pub const API_VERSION: &str = "2023-06-01";
/// Beta header required by the scalar `"fallbacks": "default"` form.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Beta header for server-side context editing (clearing old tool results).
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    /// `low`, `medium`, `high`, `xhigh` or `max`; the latency lever. Thinking itself is adaptive.
    pub effort: String,
    /// Route a safety refusal to a fallback model inside the same call.
    pub fallbacks: bool,
    /// Prompt caching: a breakpoint after the system prompt (which caches the tool list with it)
    /// plus automatic caching of the growing conversation.
    pub cache: bool,
    /// Server-side context editing: the API clears old tool results itself, so the history the
    /// service holds stays append-only (a client-side edit would invalidate the cached prefix and,
    /// on the newest models, the thinking blocks bound to it).
    pub context_editing: bool,
    pub timeout: Duration,
    pub max_attempts: u32,
}

/// The server-side fallback parameter is documented for Claude Opus 5 and the Fable and Mythos
/// models; other models get it only when `FALLBACKS=1` is set explicitly.
pub fn fallbacks_default_for(model: &str) -> bool {
    ["claude-opus-5", "claude-fable", "claude-mythos"]
        .iter()
        .any(|p| model.starts_with(p))
}

fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | ""))
        .unwrap_or(default)
}

impl AnthropicConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ if base_url != DEFAULT_BASE_URL => String::new(), // a local mock does not need a key
            _ => anyhow::bail!("ANTHROPIC_API_KEY is not set"),
        };
        let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let fallbacks = env_flag("FALLBACKS", fallbacks_default_for(&model));
        Ok(Self {
            api_key,
            base_url,
            model,
            max_tokens: std::env::var("MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16_000),
            effort: std::env::var("EFFORT").unwrap_or_else(|_| "medium".into()),
            fallbacks,
            cache: env_flag("PROMPT_CACHE", true),
            context_editing: env_flag("CONTEXT_EDITING", false),
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

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub usage: Value,
    #[serde(default)]
    pub stop_details: Value,
}

impl Message {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    }
    pub fn tool_uses(&self) -> impl Iterator<Item = &Value> {
        self.content.iter().filter(|b| b["type"] == "tool_use")
    }
    pub fn input_tokens(&self) -> u64 {
        self.usage["input_tokens"].as_u64().unwrap_or(0)
    }
    pub fn output_tokens(&self) -> u64 {
        self.usage["output_tokens"].as_u64().unwrap_or(0)
    }
    /// Prompt tokens served from the cache (billed at a fraction of the input price).
    pub fn cache_read_tokens(&self) -> u64 {
        self.usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
    }
    /// Prompt tokens written to the cache this call (billed at a premium).
    pub fn cache_creation_tokens(&self) -> u64 {
        self.usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("API returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("could not decode API response: {0}")]
    Decode(String),
}

pub struct AnthropicClient {
    http: reqwest::Client,
    cfg: AnthropicConfig,
}

impl AnthropicClient {
    pub fn new(cfg: AnthropicConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(cfg.timeout).build()?;
        Ok(Self { http, cfg })
    }

    pub fn config(&self) -> &AnthropicConfig {
        &self.cfg
    }

    /// The request body for one call. Render order is tools, system, messages, so the breakpoint on
    /// the system block caches the tool list with it; the top-level `cache_control` then caches the
    /// conversation incrementally. Everything the service varies per turn sits inside `messages`.
    pub fn request_body(&self, system: &str, messages: &[Value], tools: &[Value]) -> Value {
        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "messages": messages,
            "output_config": { "effort": self.cfg.effort },
        });
        if self.cfg.cache {
            body["system"] = json!([{ "type": "text", "text": system, "cache_control": { "type": "ephemeral" } }]);
            body["cache_control"] = json!({ "type": "ephemeral" });
        } else {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if self.cfg.fallbacks {
            body["fallbacks"] = json!("default");
        }
        if self.cfg.context_editing {
            body["context_management"] = json!({ "edits": [ { "type": "clear_tool_uses_20250919" } ] });
        }
        body
    }

    /// The `anthropic-beta` header value, or `None` when no beta feature is enabled.
    pub fn beta_header(&self) -> Option<String> {
        let mut betas = Vec::new();
        if self.cfg.fallbacks {
            betas.push(FALLBACK_BETA);
        }
        if self.cfg.context_editing {
            betas.push(CONTEXT_MANAGEMENT_BETA);
        }
        (!betas.is_empty()).then(|| betas.join(","))
    }

    /// One `POST /v1/messages`, retried with backoff on 429, 529 and 5xx.
    pub async fn create(&self, system: &str, messages: &[Value], tools: &[Value]) -> Result<Message, ApiError> {
        let body = self.request_body(system, messages, tools);
        let beta = self.beta_header();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self
                .http
                .post(format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/')))
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json");
            if !self.cfg.api_key.is_empty() {
                req = req.header("x-api-key", &self.cfg.api_key);
            }
            if let Some(beta) = &beta {
                req = req.header("anthropic-beta", beta);
            }
            let resp = match req.json(&body).send().await {
                Ok(r) => r,
                Err(e) if attempt < self.cfg.max_attempts => {
                    tracing::warn!(attempt, error = %e, "model request failed, retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(e) => return Err(ApiError::Transport(e)),
            };
            let status = resp.status().as_u16();
            let text = resp.text().await?;
            if status == 429 || status == 529 || (500..600).contains(&status) {
                if attempt < self.cfg.max_attempts {
                    tracing::warn!(attempt, status, "model request rejected, retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                return Err(ApiError::Http { status, body: text });
            }
            if !(200..300).contains(&status) {
                return Err(ApiError::Http { status, body: text });
            }
            return serde_json::from_str::<Message>(&text)
                .map_err(|e| ApiError::Decode(format!("{e}: {}", text.chars().take(200).collect::<String>())));
        }
    }
}

pub(crate) fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(500 * 2u64.pow(attempt.saturating_sub(1)));
    let jitter = Duration::from_millis(
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_millis())
            .unwrap_or(0)
            % 250) as u64,
    );
    base + jitter
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(cache: bool, context_editing: bool) -> AnthropicClient {
        AnthropicClient::new(AnthropicConfig {
            api_key: String::new(),
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            max_tokens: 100,
            effort: "medium".into(),
            fallbacks: true,
            cache,
            context_editing,
            timeout: Duration::from_secs(1),
            max_attempts: 1,
        })
        .unwrap()
    }

    #[test]
    fn fallbacks_are_on_by_default_only_where_documented() {
        assert!(fallbacks_default_for("claude-opus-5"));
        assert!(fallbacks_default_for("claude-fable-5-1"));
        assert!(!fallbacks_default_for("claude-sonnet-5"));
        assert!(!fallbacks_default_for("claude-opus-4-8"));
    }

    #[test]
    fn caching_marks_the_system_block_and_the_conversation_tail() {
        let c = client(true, false);
        let body = c.request_body("sys", &[json!({"role":"user","content":"hi"})], &[json!({"name":"t"})]);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["name"], "t");
        assert!(body.get("context_management").is_none());
        assert_eq!(c.beta_header().as_deref(), Some(FALLBACK_BETA));

        let plain = client(false, true);
        let body = plain.request_body("sys", &[], &[]);
        assert_eq!(body["system"], "sys");
        assert!(body.get("cache_control").is_none() && body.get("tools").is_none());
        assert_eq!(
            body["context_management"]["edits"][0]["type"],
            "clear_tool_uses_20250919"
        );
        assert_eq!(
            plain.beta_header().as_deref(),
            Some("server-side-fallback-2026-07-01,context-management-2025-06-27")
        );
    }
}
