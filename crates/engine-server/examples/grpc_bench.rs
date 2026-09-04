//! gRPC round-trip latency and throughput against an in-process server:
//! `cargo run --release -p engine-server --example grpc_bench`.
//! Set `ENGINE_JOURNAL=/path/file.jsonl` (and `ENGINE_JOURNAL_FSYNC=1`) to measure with the
//! write-ahead journal on.
use clob_proto::v1::engine_client::EngineClient;
use clob_proto::v1::{DepositRequest, PlaceOrderRequest, Side, TimeInForce};
use engine_server::{serve, EngineConfig};
use std::time::Instant;

fn percentile(sorted: &[u128], p: f64) -> u128 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let cfg = EngineConfig {
        journal_path: std::env::var("ENGINE_JOURNAL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Into::into),
        journal_fsync: std::env::var("ENGINE_JOURNAL_FSYNC").is_ok_and(|v| v == "1"),
        ..EngineConfig::default()
    };
    if let Some(p) = &cfg.journal_path {
        let _ = std::fs::remove_file(p);
        println!("journal: {} (fsync {})", p.display(), cfg.journal_fsync);
    }
    let (addr, handle) = serve("127.0.0.1:0".parse()?, cfg).await?;
    let url = format!("http://{addr}");
    let mut c = EngineClient::connect(url.clone()).await?;
    let mut accounts = vec!["b".to_string(), "s".to_string()];
    accounts.extend((0..16u64).map(|t| if t % 2 == 0 { format!("b{t}") } else { format!("s{t}") }));
    for a in accounts {
        c.deposit(DepositRequest {
            account_id: a,
            usdc_micro: 1_000_000_000_000_000,
            eth_lots: 10_000_000_000,
        })
        .await?;
    }
    let n = 5_000u64;
    let mut lat = Vec::with_capacity(n as usize);
    for i in 0..n {
        let t0 = Instant::now();
        c.place_order(PlaceOrderRequest {
            account_id: if i % 2 == 0 { "b".into() } else { "s".into() },
            client_order_id: format!("seq-{i}"),
            side: if i % 2 == 0 { Side::Buy } else { Side::Sell } as i32,
            price_ticks: 300_000 + (i % 50) as i64,
            quantity_lots: 100,
            tif: TimeInForce::Gtc as i32,
        })
        .await?;
        lat.push(t0.elapsed().as_micros());
    }
    lat.sort_unstable();
    println!(
        "sequential PlaceOrder over gRPC, n={n}: p50 {} us, p99 {} us, max {} us",
        percentile(&lat, 0.50),
        percentile(&lat, 0.99),
        lat[lat.len() - 1]
    );
    let clients = 16u64;
    let per = 2_000u64;
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for t in 0..clients {
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = EngineClient::connect(url).await.unwrap();
            for i in 0..per {
                c.place_order(PlaceOrderRequest {
                    account_id: if t % 2 == 0 { format!("b{t}") } else { format!("s{t}") },
                    client_order_id: format!("{t}-{i}"),
                    side: if t % 2 == 0 { Side::Buy } else { Side::Sell } as i32,
                    price_ticks: 300_000 + ((t + i) % 50) as i64,
                    quantity_lots: 100,
                    tif: TimeInForce::Gtc as i32,
                })
                .await
                .unwrap();
            }
        }));
    }
    for t in tasks {
        t.await?;
    }
    let el = t0.elapsed().as_secs_f64();
    println!(
        "{clients} concurrent clients x {per} orders: {:.0} orders/s",
        (clients * per) as f64 / el
    );
    // Pipelined placement: one stream, then four, each sending without waiting for replies.
    for streams in [1u64, 4] {
        let per = 50_000u64;
        let t0 = Instant::now();
        let mut tasks = Vec::new();
        for t in 0..streams {
            let url = url.clone();
            tasks.push(tokio::spawn(async move {
                let mut c = EngineClient::connect(url).await.unwrap();
                let (tx, rx) = tokio::sync::mpsc::channel(1024);
                let producer = tokio::spawn(async move {
                    for i in 0..per {
                        let req = PlaceOrderRequest {
                            account_id: if t % 2 == 0 { format!("b{t}") } else { format!("s{t}") },
                            client_order_id: format!("st{t}-{i}"),
                            side: if t % 2 == 0 { Side::Buy } else { Side::Sell } as i32,
                            price_ticks: 300_000 + ((t + i) % 50) as i64,
                            quantity_lots: 100,
                            tif: TimeInForce::Gtc as i32,
                        };
                        if tx.send(req).await.is_err() {
                            break;
                        }
                    }
                });
                let mut replies = c
                    .place_orders(tokio_stream::wrappers::ReceiverStream::new(rx))
                    .await
                    .unwrap()
                    .into_inner();
                let mut n = 0u64;
                let mut errors = 0u64;
                while let Ok(Some(r)) = replies.message().await {
                    n += 1;
                    if r.placed.is_none() {
                        errors += 1;
                    }
                }
                producer.await.unwrap();
                assert_eq!(n, per, "one result per request, in order");
                errors
            }));
        }
        let mut errors = 0;
        for t in tasks {
            errors += t.await?;
        }
        let el = t0.elapsed().as_secs_f64();
        println!(
            "{streams} pipelined stream(s) x {per} orders: {:.0} orders/s ({errors} refused)",
            (streams * per) as f64 / el
        );
    }
    handle.shutdown().await;
    Ok(())
}
