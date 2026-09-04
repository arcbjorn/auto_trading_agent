//! Soak: several rounds of a million orders each over gRPC with the write-ahead journal on,
//! restarting the server between rounds so recovery and compaction run, and printing the
//! process's resident memory after every round. Memory must level off rather than grow with the
//! order count: closed orders and trades beyond the retention are archived, and the snapshot
//! holds only what is retained.
//! `SOAK_ORDERS` (default 1,000,000) orders per round, `SOAK_ROUNDS` (default 3).
use clob_proto::v1::engine_client::EngineClient;
use clob_proto::v1::{CancelOrderRequest, DepositRequest, PlaceOrderRequest, Side, TimeInForce};
use engine_server::{EngineConfig, serve};
use std::time::Instant;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn resident_mb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok();
    out.and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        / 1024
}

fn size_mb(path: &std::path::Path) -> f64 {
    std::fs::metadata(path)
        .map(|m| m.len() as f64 / (1 << 20) as f64)
        .unwrap_or(0.0)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let orders = env_u64("SOAK_ORDERS", 1_000_000);
    let rounds = env_u64("SOAK_ROUNDS", 3);
    // With SOAK_JOURNAL set the journal is kept, so `scripts/soak.sh` can run every round in a
    // fresh process, the way a restart happens in practice, and read each process's memory.
    let keep = std::env::var("SOAK_JOURNAL").ok().filter(|s| !s.is_empty());
    let dir = std::env::temp_dir().join(format!("clob-soak-{}", std::process::id()));
    let journal = match &keep {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            std::fs::create_dir_all(&dir)?;
            dir.join("journal.jsonl")
        }
    };
    let snapshot = engine::Journal::snapshot_path(&journal);
    let first_round = env_u64("SOAK_FIRST_ROUND", 1);
    println!(
        "journal {} ; {orders} orders per round, {rounds} rounds",
        journal.display()
    );
    let clients = 16u64;
    for round in first_round..first_round + rounds {
        let cfg = EngineConfig {
            journal_path: Some(journal.clone()),
            ..EngineConfig::default()
        };
        let t0 = Instant::now();
        let (addr, handle) = serve("127.0.0.1:0".parse()?, cfg).await?;
        let recovery_ms = t0.elapsed().as_millis();
        let url = format!("http://{addr}");
        let mut c = EngineClient::connect(url.clone()).await?;
        for t in 0..clients {
            c.deposit(DepositRequest {
                account_id: format!("a{t}"),
                usdc_micro: 1_000_000_000_000_000, // 1e9 USDC per round, far above what a round uses
                eth_lots: 1_000_000_000_000,
            })
            .await?;
        }
        let per = orders / clients;
        let t1 = Instant::now();
        let mut tasks = Vec::new();
        for t in 0..clients {
            let url = url.clone();
            tasks.push(tokio::spawn(async move {
                let mut c = EngineClient::connect(url).await.unwrap();
                // Orders that are still resting 32 placements later are cancelled, as a real
                // participant would: live orders are kept for ever, closed ones are archived.
                let mut recent = std::collections::VecDeque::new();
                for i in 0..per {
                    let buy = (t + i) % 2 == 0;
                    let placed = c
                        .place_order(PlaceOrderRequest {
                            account_id: format!("a{t}"),
                            client_order_id: format!("r{round}-{t}-{i}"),
                            side: if buy { Side::Buy } else { Side::Sell } as i32,
                            price_ticks: 300_000 + ((t * 7 + i) % 40) as i64,
                            quantity_lots: 1 + (i % 9) as i64,
                            tif: if i % 5 == 0 { TimeInForce::Ioc } else { TimeInForce::Gtc } as i32,
                        })
                        .await
                        .unwrap()
                        .into_inner();
                    if let Some(o) = placed.order {
                        recent.push_back(o.order_id);
                    }
                    if recent.len() > 32 {
                        let old = recent.pop_front().unwrap();
                        // Already filled or cancelled is a FAILED_PRECONDITION, which is fine.
                        let _ = c
                            .cancel_order(CancelOrderRequest {
                                account_id: format!("a{t}"),
                                order_id: old,
                            })
                            .await;
                    }
                }
            }));
        }
        for t in tasks {
            t.await?;
        }
        let rate = (per * clients) as f64 / t1.elapsed().as_secs_f64();
        // Placements per second; each placement past the first 32 is followed by a cancel.
        handle.shutdown().await;
        println!(
            "round {round}: recovery {recovery_ms} ms, {rate:.0} orders/s, resident {} MB, journal {:.1} MB, snapshot {:.1} MB",
            resident_mb(),
            size_mb(&journal),
            size_mb(&snapshot)
        );
    }
    if keep.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
}
