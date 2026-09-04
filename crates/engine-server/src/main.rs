//! `engine-server`: the gRPC matching engine.
//!
//! Environment:
//!   ENGINE_BIND   address to listen on (default 127.0.0.1:50051). The engine has no
//!                 authentication and takes the account from the request, so it must not be
//!                 exposed beyond the host without mTLS or a service identity in front of it.
//!   ENGINE_QUEUE  bounded command queue length (default 10000)
//!   ENGINE_JOURNAL       path of the write-ahead journal (default none: in memory only)
//!   ENGINE_JOURNAL_FSYNC 1 to fsync every batch before replying (default 0: flush to the OS)
//!   ENGINE_JOURNAL_COMPACT_MB  compact the journal into a snapshot on start when larger (default 64; 0 never)
//!   ENGINE_BALANCES      0 to run without balance checks (default 1: every order must be funded)
//!   ENGINE_ACCOUNT_RATE_PER_SEC   mutations one account may send per second (default 50, 0 = unlimited)
//!   ENGINE_RETAIN_HOURS           closed orders and trades older than this are archived (default 24)
//!   ENGINE_MAX_OPEN_ORDERS        live orders one account may rest at once (default 20, 0 = unlimited)
//!   ENGINE_MAX_OPEN_NOTIONAL_USDC sum of price x remaining one account may rest, in USDC (default 200000, 0 = unlimited)
//!   ENGINE_FUND          accounts credited on an empty book, whole units: "demo:50000:10,mm:1000000:1000"
//!                        (account:USDC:ETH); journaled, and skipped when a journal was replayed
//!   RUST_LOG      tracing filter (default info)
use engine_server::{serve, EngineConfig};

/// "account:USDC:ETH,..." in whole units -> (account, micro-USDC, lots).
fn parse_funding(spec: &str) -> anyhow::Result<Vec<(String, u64, u64)>> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let parts: Vec<&str> = entry.split(':').collect();
            anyhow::ensure!(parts.len() == 3, "ENGINE_FUND entry {entry:?} is not account:USDC:ETH");
            let usdc: u64 = parts[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("bad USDC amount in {entry:?}"))?;
            let eth: u64 = parts[2]
                .parse()
                .map_err(|_| anyhow::anyhow!("bad ETH amount in {entry:?}"))?;
            Ok((parts[0].to_string(), usdc * 1_000_000, eth * 10_000))
        })
        .collect()
}

/// A numeric setting with a finite default. `0` means unlimited, which reads as `None` so each
/// caller substitutes its own maximum.
fn env_or(name: &str, default: u64) -> Option<u64> {
    let value = std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    (value > 0).then_some(value)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    // Loopback by default: an unauthenticated engine that trusts the account in each request
    // must not be reachable from the network unless an operator says so deliberately.
    let bind = std::env::var("ENGINE_BIND").unwrap_or_else(|_| "127.0.0.1:50051".into());
    if !bind.starts_with("127.") && !bind.starts_with("localhost") && !bind.starts_with("[::1]") {
        tracing::warn!(%bind, "the engine is bound beyond loopback; it has no authentication, so put mTLS or a service identity in front of it");
    }
    let queue_capacity = std::env::var("ENGINE_QUEUE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let cfg = EngineConfig {
        queue_capacity,
        journal_path: std::env::var("ENGINE_JOURNAL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Into::into),
        journal_fsync: matches!(
            std::env::var("ENGINE_JOURNAL_FSYNC").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        ),
        enforce_balances: !matches!(
            std::env::var("ENGINE_BALANCES").as_deref(),
            Ok("0") | Ok("false") | Ok("no")
        ),
        // Finite by default, and matching the MCP policy's own limits, so the matcher is the
        // boundary that actually holds: a direct gRPC client bypasses the MCP checks but not
        // these. Raise them deliberately, or set 0 for unlimited.
        exposure_limits: engine::ExposureLimits {
            max_open_orders: env_or("ENGINE_MAX_OPEN_ORDERS", 20)
                .map(|v| v as u32)
                .unwrap_or(u32::MAX),
            max_open_notional: env_or("ENGINE_MAX_OPEN_NOTIONAL_USDC", 200_000)
                .map(|usdc| usdc as u128 * 1_000_000)
                .unwrap_or(u128::MAX),
        },
        account_rate_per_sec: env_or("ENGINE_ACCOUNT_RATE_PER_SEC", 50).map(|v| v as u32).unwrap_or(0),
        retention: engine::Retention {
            max_age_ns: std::env::var("ENGINE_RETAIN_HOURS")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .map(|h| h * 60 * 60 * 1_000_000_000)
                .unwrap_or(engine::RETAINED_FOR_NS),
            ..engine::Retention::default()
        },
        fund_at_start: parse_funding(&std::env::var("ENGINE_FUND").unwrap_or_default())?,
        journal_compact_bytes: std::env::var("ENGINE_JOURNAL_COMPACT_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|mb| mb << 20)
            .unwrap_or(64 << 20),
        ..EngineConfig::default()
    };
    let (addr, handle) = serve(bind.parse()?, cfg).await?;
    tracing::info!(%addr, "engine listening");
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    handle.shutdown().await;
    Ok(())
}
