//! gRPC round-trip latency and throughput against an in-process server:
//! `cargo run --release -p engine-server --example grpc_bench`.
use clob_proto::v1::engine_client::EngineClient;
use clob_proto::v1::{PlaceOrderRequest, Side, TimeInForce};
use engine_server::{serve, EngineConfig};
use std::time::Instant;

fn percentile(sorted: &[u128], p: f64) -> u128 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let (addr, handle) = serve("127.0.0.1:0".parse()?, EngineConfig::default()).await?;
    let url = format!("http://{addr}");
    let mut c = EngineClient::connect(url.clone()).await?;
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
    handle.shutdown().await;
    Ok(())
}
