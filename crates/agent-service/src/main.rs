//! `agent-service`: natural-language interaction with the order book.
//!
//! Environment:
//!   ANTHROPIC_API_KEY      required (unless ANTHROPIC_BASE_URL points at a local mock)
//!   ANTHROPIC_MODEL        default claude-opus-5
//!   EFFORT                 low | medium | high (default medium)
//!   MAX_TOKENS             default 16000
//!   MCP_URL                default http://127.0.0.1:8000/mcp
//!   AGENT_BIND             default 127.0.0.1:8080
//!   AUDIT_LOG              JSON-lines path (default audit.jsonl; empty string disables)
//!   CONFIRM_THRESHOLD_ETH  orders at or above this size need confirmation (default 1)
//!   GATE_TOOLS             1 (default) offers action tools only on explicit intent
use agent_service::http::{serve, State};
use agent_service::{Agent, AgentConfig, AnthropicClient, AnthropicConfig, Audit, McpClient};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let model_cfg = AnthropicConfig::from_env()?;
    let mcp_url = std::env::var("MCP_URL").unwrap_or_else(|_| "http://127.0.0.1:8000/mcp".into());
    let bind = std::env::var("AGENT_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let audit_path = std::env::var("AUDIT_LOG").unwrap_or_else(|_| "audit.jsonl".into());
    let threshold_eth: u64 = std::env::var("CONFIRM_THRESHOLD_ETH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let cfg = AgentConfig {
        confirm_threshold_lots: threshold_eth * 10_000,
        confirm_ttl: Duration::from_secs(600),
        gate_tools: std::env::var("GATE_TOOLS").map(|v| v != "0").unwrap_or(true),
        ..AgentConfig::default()
    };
    tracing::info!(model = %model_cfg.model, effort = %model_cfg.effort, %mcp_url, ?cfg, "agent-service starting");
    let mcp = McpClient::connect(&mcp_url)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach MCP server at {mcp_url}: {e}"))?;
    let audit = Audit::new(if audit_path.is_empty() {
        None
    } else {
        Some(audit_path.into())
    });
    let agent = Agent::new(AnthropicClient::new(model_cfg)?, mcp, cfg, audit).await?;
    tracing::info!(tools = ?agent.tools().iter().filter_map(|t| t["name"].as_str()).collect::<Vec<_>>(), "tools loaded from MCP");
    let (addr, handle) = serve(bind.parse()?, Arc::new(State::new(agent))).await?;
    tracing::info!(%addr, "POST /chat ready");
    tokio::signal::ctrl_c().await?;
    handle.shutdown().await;
    Ok(())
}
