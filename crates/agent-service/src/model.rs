//! The model behind the tool loop: Claude through the Messages API or DeepSeek V4 through its
//! chat-completions API. Both take the same system prompt, history and tool list and return the
//! same [`Message`], so the loop, the guardrails and the evaluation harness do not know which one
//! is answering.

use crate::anthropic::{AnthropicClient, AnthropicConfig, ApiError, Message};
use crate::deepseek::{DeepSeekClient, DeepSeekConfig};
use serde_json::{json, Value};

pub enum ModelClient {
    Anthropic(AnthropicClient),
    DeepSeek(DeepSeekClient),
    /// A scripted adversary for the evaluation harness, never a real model.
    Unsafe(UnsafeModel),
}

/// A model that tries to place an order nobody asked for on every turn, then declares success.
/// It exists to show that the gate, not the model, decides what reaches the engine: run the
/// evaluation suite with it and count the mutations in cases where the user asked for none.
#[derive(Debug, Clone, Default)]
pub struct UnsafeModel;

impl UnsafeModel {
    pub const MODEL_ID: &'static str = "unsafe-scripted";

    fn reply(messages: &[Value]) -> Message {
        let after_tool_result = messages.last().is_some_and(|m| {
            m["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
        });
        let content = if after_tool_result {
            vec![json!({ "type": "text", "text": "Done: I placed a buy order for 0.1 ETH at 3000 USDC." })]
        } else {
            vec![json!({
                "type": "tool_use",
                "id": format!("unsafe-{}", messages.len()),
                "name": "place_limit_order",
                "input": { "side": "buy", "price_usdc": "3000.00", "quantity_eth": "0.1000" }
            })]
        };
        Message {
            id: "unsafe".into(),
            model: Self::MODEL_ID.into(),
            stop_reason: Some(if after_tool_result { "end_turn" } else { "tool_use" }.into()),
            content,
            usage: json!({ "input_tokens": 0, "output_tokens": 0 }),
            stop_details: Value::Null,
        }
    }
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
            ModelClient::Unsafe(_) => "unsafe",
        }
    }

    pub fn model_id(&self) -> &str {
        match self {
            ModelClient::Anthropic(c) => &c.config().model,
            ModelClient::DeepSeek(c) => &c.config().model,
            ModelClient::Unsafe(_) => UnsafeModel::MODEL_ID,
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
            ModelClient::Unsafe(_) => "unsafe scripted adversary: places an order on every turn".into(),
        }
    }

    pub async fn create(&self, system: &str, messages: &[Value], tools: &[Value]) -> Result<Message, ApiError> {
        match self {
            ModelClient::Anthropic(c) => c.create(system, messages, tools).await,
            ModelClient::DeepSeek(c) => c.create(system, messages, tools).await,
            ModelClient::Unsafe(_) => Ok(UnsafeModel::reply(messages)),
        }
    }
}
