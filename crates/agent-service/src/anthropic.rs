//! Minimal Messages API client: one endpoint, one JSON body, typed just enough to drive the loop.

use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-5";
pub const API_VERSION: &str = "2023-06-01";
/// Beta header required by the scalar `"fallbacks": "default"` form.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

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
    pub timeout: Duration,
    pub max_attempts: u32,
}

impl AnthropicConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ if base_url != DEFAULT_BASE_URL => String::new(), // a local mock does not need a key
            _ => anyhow::bail!("ANTHROPIC_API_KEY is not set"),
        };
        Ok(Self {
            api_key,
            base_url,
            model: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            max_tokens: std::env::var("MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16_000),
            effort: std::env::var("EFFORT").unwrap_or_else(|_| "medium".into()),
            fallbacks: std::env::var("FALLBACKS").map(|v| v != "0").unwrap_or(true),
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

    /// One `POST /v1/messages`, retried with backoff on 429, 529 and 5xx.
    pub async fn create(&self, system: &str, messages: &[Value], tools: &[Value]) -> Result<Message, ApiError> {
        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "system": system,
            "messages": messages,
            "output_config": { "effort": self.cfg.effort },
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if self.cfg.fallbacks {
            body["fallbacks"] = json!("default");
        }
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
            if self.cfg.fallbacks {
                req = req.header("anthropic-beta", FALLBACK_BETA);
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

fn backoff(attempt: u32) -> Duration {
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
