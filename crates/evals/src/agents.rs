//! The three drivers a case can be run with. All of them act through the MCP server, so the
//! oracle and null agents exercise the same path as the model minus the model.

use crate::cases::Case;
use agent_service::{Agent, AgentConfig, AnthropicClient, AnthropicConfig, Audit, McpClient, Session};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Default, Serialize)]
pub struct TurnOutcome {
    pub reply: String,
    pub tool_calls: usize,
    pub tool_call_records: Vec<Value>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model_latency_ms: u64,
    pub latency_ms: u64,
    pub model: String,
    pub flags: Vec<String>,
    pub stop_reason: String,
}

pub enum Driver {
    Model {
        cfg: AnthropicConfig,
        agent_cfg: AgentConfig,
        audit: Option<PathBuf>,
    },
    Oracle,
    Null,
}

impl Driver {
    pub fn from_name(name: &str, out_dir: &std::path::Path) -> anyhow::Result<Self> {
        Ok(match name {
            "model" => Driver::Model {
                cfg: AnthropicConfig::from_env()?,
                agent_cfg: AgentConfig::default(),
                audit: Some(out_dir.join("audit.jsonl")),
            },
            "oracle" => Driver::Oracle,
            "null" => Driver::Null,
            other => anyhow::bail!("unknown agent {other}; use model, oracle or null"),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Driver::Model { .. } => "model",
            Driver::Oracle => "oracle",
            Driver::Null => "null",
        }
    }

    /// Runs every turn of the case and returns the outcome of the last one (tool calls summed).
    pub async fn run_case(
        &self,
        case: &Case,
        mcp_url: &str,
        orders_after_setup: &[Value],
    ) -> anyhow::Result<TurnOutcome> {
        match self {
            Driver::Model { cfg, agent_cfg, audit } => {
                let claude = AnthropicClient::new(cfg.clone())?;
                let mcp = McpClient::connect(mcp_url).await?;
                let agent = Agent::new(claude, mcp, agent_cfg.clone(), Audit::new(audit.clone())).await?;
                let mut session = Session::new(format!("eval-{}", case.id));
                let mut total = TurnOutcome::default();
                for text in &case.turns {
                    let t = agent.chat_turn(&mut session, text).await?;
                    total.reply = t.reply;
                    total.tool_calls += t.tool_calls.len();
                    total
                        .tool_call_records
                        .extend(t.tool_calls.iter().map(|c| serde_json::to_value(c).unwrap_or_default()));
                    total.input_tokens += t.usage.input_tokens;
                    total.output_tokens += t.usage.output_tokens;
                    total.model_latency_ms += t.model_latency_ms;
                    total.latency_ms += t.latency_ms;
                    total.model = t.model;
                    total.flags.extend(t.flags);
                    total.stop_reason = t.stop_reason;
                }
                Ok(total)
            }
            Driver::Oracle => oracle(case, mcp_url, orders_after_setup).await,
            Driver::Null => Ok(TurnOutcome {
                reply: "I'm not able to help with that.".into(),
                model: "null".into(),
                ..TurnOutcome::default()
            }),
        }
    }
}

/// Performs exactly the expected outcome through the MCP tools: places orders that should exist
/// and cancels orders that should end cancelled. The harness must give this agent 100%.
async fn oracle(case: &Case, mcp_url: &str, orders_after_setup: &[Value]) -> anyhow::Result<TurnOutcome> {
    let started = Instant::now();
    let mcp = McpClient::connect(mcp_url).await?;
    let mut outcome = TurnOutcome {
        model: "oracle".into(),
        ..TurnOutcome::default()
    };
    if case.expect.reply_asks_question {
        outcome.reply = "Could you tell me the quantity and the limit price you want?".into();
        outcome.latency_ms = started.elapsed().as_millis() as u64;
        return Ok(outcome);
    }
    if !case.expect.no_action {
        let mut remaining: Vec<Value> = orders_after_setup.to_vec();
        for want in &case.expect.orders {
            // An order that already exists from setup with the same side/price/qty: keep it, cancel it if it must end cancelled.
            if let Some(pos) = remaining
                .iter()
                .position(|o| o["side"] == want.side && o["price_usdc"] == want.price && o["quantity_eth"] == want.qty)
            {
                let existing = remaining.remove(pos);
                if want.status == "cancelled" && existing["status"] != "cancelled" {
                    let r = mcp
                        .call_tool("cancel_order", &json!({ "order_id": existing["order_id"] }))
                        .await?;
                    outcome.tool_calls += 1;
                    outcome
                        .tool_call_records
                        .push(json!({ "name": "cancel_order", "result": r.text }));
                }
                continue;
            }
            let r = mcp
                .call_tool(
                    "place_limit_order",
                    &json!({ "side": want.side, "price_usdc": want.price, "quantity_eth": want.qty }),
                )
                .await?;
            outcome.tool_calls += 1;
            outcome
                .tool_call_records
                .push(json!({ "name": "place_limit_order", "result": r.text }));
            if want.status == "cancelled" {
                if let Some(id) = r.structured.as_ref().and_then(|s| s["order_id"].as_str()) {
                    let c = mcp.call_tool("cancel_order", &json!({ "order_id": id })).await?;
                    outcome.tool_calls += 1;
                    outcome
                        .tool_call_records
                        .push(json!({ "name": "cancel_order", "result": c.text }));
                }
            }
        }
    }
    outcome.reply = if case.expect.reply_mentions.is_empty() {
        if case.attack {
            "I can't do that.".into()
        } else {
            "Done.".into()
        }
    } else {
        format!("Done: {}.", case.expect.reply_mentions.join(", "))
    };
    outcome.latency_ms = started.elapsed().as_millis() as u64;
    Ok(outcome)
}
