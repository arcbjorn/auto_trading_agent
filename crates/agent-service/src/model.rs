//! The model behind the tool loop: Claude through the Messages API or DeepSeek V4 through its
//! chat-completions API. Both take the same system prompt, history and tool list and return the
//! same [`Message`], so the loop, the guardrails and the evaluation harness do not know which one
//! is answering.

use crate::anthropic::{AnthropicClient, AnthropicConfig, ApiError, Message};
use crate::deepseek::{DeepSeekClient, DeepSeekConfig};
use serde_json::Value;

pub enum ModelClient {
    Anthropic(AnthropicClient),
    DeepSeek(DeepSeekClient),
}

impl From<AnthropicClient> for ModelClient {
    fn from(c: AnthropicClient) -> Self {
        ModelClient::Anthropic(c)
    }
}

impl From<DeepSeekClient> for ModelClient {
    fn from(c: DeepSeekClient) -> Self {
        ModelClient::DeepSeek(c)
    }
}

impl ModelClient {
    /// `MODEL_PROVIDER=anthropic` (default) or `deepseek`; each provider reads its own variables.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("MODEL_PROVIDER").as_deref().map(str::trim) {
            Ok("deepseek") => Ok(ModelClient::DeepSeek(DeepSeekClient::new(DeepSeekConfig::from_env()?)?)),
            Ok("anthropic") | Ok("") | Err(_) => Ok(ModelClient::Anthropic(AnthropicClient::new(
                AnthropicConfig::from_env()?,
            )?)),
            Ok(other) => anyhow::bail!("unknown MODEL_PROVIDER {other:?}; use anthropic or deepseek"),
        }
    }

    pub fn provider(&self) -> &'static str {
        match self {
            ModelClient::Anthropic(_) => "anthropic",
            ModelClient::DeepSeek(_) => "deepseek",
        }
    }

    pub fn model_id(&self) -> &str {
        match self {
            ModelClient::Anthropic(c) => &c.config().model,
            ModelClient::DeepSeek(c) => &c.config().model,
        }
    }

    /// A one-line description for the startup log: model and the settings that shape a request.
    pub fn describe(&self) -> String {
        match self {
            ModelClient::Anthropic(c) => {
                let cfg = c.config();
                format!(
                    "anthropic model={} effort={} cache={} context_editing={} fallbacks={}",
                    cfg.model, cfg.effort, cfg.cache, cfg.context_editing, cfg.fallbacks
                )
            }
            ModelClient::DeepSeek(c) => {
                let cfg = c.config();
                format!(
                    "deepseek model={} thinking={} reasoning_effort={}",
                    cfg.model, cfg.thinking, cfg.reasoning_effort
                )
            }
        }
    }

    pub async fn create(&self, system: &str, messages: &[Value], tools: &[Value]) -> Result<Message, ApiError> {
        match self {
            ModelClient::Anthropic(c) => c.create(system, messages, tools).await,
            ModelClient::DeepSeek(c) => c.create(system, messages, tools).await,
        }
    }
}
