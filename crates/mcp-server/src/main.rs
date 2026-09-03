//! `mcp-server`: exposes the engine to a model over MCP.
//!
//! Usage: `mcp-server` (stdio, for Claude Desktop / Claude Code) or `mcp-server --http`.
//!
//! Environment:
//!   ENGINE_ADDR   gRPC endpoint of the engine (default http://127.0.0.1:50051)
//!   ACCOUNT_ID    the trading account this server acts for (default demo)
//!   MCP_BIND      HTTP bind address with --http (default 127.0.0.1:8000)
//!   POLICY_*      risk limits, see policy.rs; TRADING_HALTED=1 rejects every action
//!   RUST_LOG      tracing filter (default info); logs go to stderr
use clob_proto::v1::engine_client::EngineClient;
use mcp_server::{run_stdio, serve_http, McpServer, Policy, PolicyConfig, ToolSet};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let engine_addr = std::env::var("ENGINE_ADDR").unwrap_or_else(|_| "http://127.0.0.1:50051".into());
    let account = std::env::var("ACCOUNT_ID").unwrap_or_else(|_| "demo".into());
    let engine = EngineClient::connect(engine_addr.clone())
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach engine at {engine_addr}: {e}"))?;
    let policy = Arc::new(Policy::new(PolicyConfig::from_env()));
    tracing::info!(%engine_addr, %account, policy = ?policy.config(), "mcp-server starting");
    let server = Arc::new(McpServer::new(ToolSet::new(engine, account, policy)));
    if std::env::args().any(|a| a == "--http") {
        let bind = std::env::var("MCP_BIND").unwrap_or_else(|_| "127.0.0.1:8000".into());
        let (addr, handle) = serve_http(bind.parse()?, server).await?;
        tracing::info!(%addr, "MCP over Streamable HTTP at /mcp");
        tokio::signal::ctrl_c().await?;
        handle.shutdown().await;
    } else {
        run_stdio(server).await?;
    }
    Ok(())
}
