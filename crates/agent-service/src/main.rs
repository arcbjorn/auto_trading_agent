//! `agent-service`: natural-language interaction with the order book.
//!
//! Environment:
//!   MODEL_PROVIDER         anthropic (default) | deepseek
//!   ANTHROPIC_API_KEY      required with the anthropic provider (unless ANTHROPIC_BASE_URL points at a local mock)
//!   ANTHROPIC_MODEL        default claude-opus-5
//!   DEEPSEEK_API_KEY       required with the deepseek provider
//!   DEEPSEEK_MODEL         default deepseek-v4-flash (or deepseek-v4-pro)
//!   DEEPSEEK_THINKING      1 (default) enables thinking mode; EFFORT maps onto low/high/max
//!   EFFORT                 low | medium | high (default medium)
//!   MAX_TOKENS             default 16000
//!   MCP_URL                default http://127.0.0.1:8000/mcp
//!   AGENT_BIND             default 127.0.0.1:8080
//!   AUDIT_LOG              JSON-lines path (default audit.jsonl; empty string disables)
//!   CONFIRM_THRESHOLD_ETH  orders at or above this size need confirmation (default 1)
//!   CONFIRM_UNPRICED       1 (default) confirms any order at a price the user did not state
//!   GATE_TOOLS             1 (default) permits action tools only on explicit intent
//!   NOTE_CHANNEL           system | user: how the per-turn permission note is sent (default: by model)
//!   PROMPT_CACHE           1 (default) marks the system prompt and conversation for caching
//!   CONTEXT_EDITING        1 lets the API clear old tool results server-side (default 0)
//!   MAX_SESSIONS           sessions kept in memory (default 1000)
//!   SESSION_IDLE_SECS      idle sessions are dropped first when the store is full (default 3600)
//!   TURNS_PER_MINUTE       turns one session may start per minute (default 20)
//!   MAX_TURNS              turns one session may hold in total (default 200)
use agent_service::http::{serve, SessionLimits, State};
use agent_service::{Agent, AgentConfig, Audit, McpClient, ModelClient, NoteChannel};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let model = ModelClient::from_env()?;
    let mcp_url = std::env::var("MCP_URL").unwrap_or_else(|_| "http://127.0.0.1:8000/mcp".into());
    let bind = std::env::var("AGENT_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let audit_path = std::env::var("AUDIT_LOG").unwrap_or_else(|_| "audit.jsonl".into());
    let threshold_eth: u64 = std::env::var("CONFIRM_THRESHOLD_ETH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let note_channel = match std::env::var("NOTE_CHANNEL").as_deref() {
        Ok("system") => NoteChannel::System,
        Ok("user") => NoteChannel::User,
        _ => NoteChannel::for_model(model.model_id()),
    };
    let cfg = AgentConfig {
        confirm_threshold_lots: threshold_eth * 10_000,
        confirm_unpriced: std::env::var("CONFIRM_UNPRICED").map(|v| v != "0").unwrap_or(true),
        confirm_ttl: Duration::from_secs(600),
        gate_tools: std::env::var("GATE_TOOLS").map(|v| v != "0").unwrap_or(true),
        note_channel,
        ..AgentConfig::default()
    };
    let limits = SessionLimits {
        max_sessions: std::env::var("MAX_SESSIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1_000),
        idle_ttl: Duration::from_secs(
            std::env::var("SESSION_IDLE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3_600),
        ),
        turns_per_minute: std::env::var("TURNS_PER_MINUTE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20),
        max_turns: std::env::var("MAX_TURNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200),
    };
    tracing::info!(model = %model.describe(), %mcp_url, ?cfg, ?limits, "agent-service starting");
    let mcp = McpClient::connect(&mcp_url)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach MCP server at {mcp_url}: {e}"))?;
    let audit = Audit::new(if audit_path.is_empty() {
        None
    } else {
        Some(audit_path.into())
    })
    .map_err(|e| anyhow::anyhow!("audit log: {e}"))?;
    let agent = Agent::new(model, mcp, cfg, audit).await?;
    tracing::info!(tools = ?agent.tools().iter().filter_map(|t| t["name"].as_str()).collect::<Vec<_>>(), "tools loaded from MCP");
    let (addr, handle) = serve(bind.parse()?, Arc::new(State::with_limits(agent, limits))).await?;
    tracing::info!(%addr, "POST /chat ready");
    tokio::signal::ctrl_c().await?;
    handle.shutdown().await;
    Ok(())
}
