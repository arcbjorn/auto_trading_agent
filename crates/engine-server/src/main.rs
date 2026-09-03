//! `engine-server`: the gRPC matching engine.
//!
//! Environment:
//!   ENGINE_BIND   address to listen on (default 0.0.0.0:50051)
//!   ENGINE_QUEUE  bounded command queue length (default 10000)
//!   RUST_LOG      tracing filter (default info)
use engine_server::{serve, EngineConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let bind = std::env::var("ENGINE_BIND").unwrap_or_else(|_| "0.0.0.0:50051".into());
    let queue_capacity = std::env::var("ENGINE_QUEUE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let cfg = EngineConfig {
        queue_capacity,
        ..EngineConfig::default()
    };
    let (addr, handle) = serve(bind.parse()?, cfg).await?;
    tracing::info!(%addr, "engine listening");
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    handle.shutdown().await;
    Ok(())
}
