//! Real tonic server, sixteen concurrent clients. Proves the single-writer design under load:
//! every response succeeds, every event has a unique sequence number with no gaps, and the book
//! is never crossed.
use clob_proto::v1::engine_client::EngineClient;
use clob_proto::v1::{
    CancelOrderRequest, GetOrderBookRequest, ListTradesRequest, PlaceOrderRequest, Side, TimeInForce,
};
use engine_server::{serve, EngineConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_tasks_place_orders_concurrently() {
    let (addr, handle) = serve("127.0.0.1:0".parse().unwrap(), EngineConfig::default())
        .await
        .unwrap();
    let url = format!("http://{addr}");
    let per_task = 500u64;
    let mut tasks = Vec::new();
    for t in 0..16u64 {
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = EngineClient::connect(url).await.unwrap();
            // Even tasks buy, odd tasks sell, so no account ever trades with itself.
            let (side, account) = if t % 2 == 0 {
                (Side::Buy, format!("buyer-{t}"))
            } else {
                (Side::Sell, format!("seller-{t}"))
            };
            let mut seqs = Vec::new();
            for i in 0..per_task {
                let r = c
                    .place_order(PlaceOrderRequest {
                        account_id: account.clone(),
                        client_order_id: format!("{t}-{i}"),
                        side: side as i32,
                        price_ticks: 300_000 + ((t * 7 + i * 3) % 40) as i64,
                        quantity_lots: 100,
                        tif: TimeInForce::Gtc as i32,
                    })
                    .await
                    .unwrap()
                    .into_inner();
                seqs.push(r.order.unwrap().sequence);
                seqs.extend(r.fills.iter().map(|f| f.sequence));
            }
            seqs
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    all.sort_unstable();
    assert!(all.len() >= 16 * per_task as usize);
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "sequence numbers must be unique and contiguous"
    );

    let mut c = EngineClient::connect(url).await.unwrap();
    let book = c
        .get_order_book(GetOrderBookRequest { depth: 1 })
        .await
        .unwrap()
        .into_inner();
    if let (Some(b), Some(a)) = (book.bids.first(), book.asks.first()) {
        assert!(b.price_ticks < a.price_ticks, "book is crossed");
    }
    let trades = c
        .list_trades(ListTradesRequest {
            account_id: String::new(),
            limit: 10_000,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        trades.trades.len(),
        all.len() - 16 * per_task as usize,
        "every non-accept event is a trade"
    );
    handle.shutdown().await;
}

/// Eight accounts rest orders at shared price levels, then every account cancels all of its own
/// orders from parallel tasks while the others are still placing. Every cancel must succeed
/// (nothing crosses, so nothing fills) and the book must end empty: the lazily dropped cancelled
/// ids and the per-level totals stay consistent under interleaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancels_race_placements_and_leave_the_book_empty() {
    let (addr, handle) = serve("127.0.0.1:0".parse().unwrap(), EngineConfig::default())
        .await
        .unwrap();
    let url = format!("http://{addr}");
    let per_account = 150u64;
    let mut tasks = Vec::new();
    for a in 0..8u64 {
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = EngineClient::connect(url.clone()).await.unwrap();
            let (side, base) = if a % 2 == 0 {
                (Side::Buy, 290_000)
            } else {
                (Side::Sell, 310_000)
            };
            let mut ids = Vec::new();
            for i in 0..per_account {
                let r = c
                    .place_order(PlaceOrderRequest {
                        account_id: format!("acct-{a}"),
                        client_order_id: format!("{a}-{i}"),
                        side: side as i32,
                        price_ticks: base + (i % 5) as i64, // five shared levels per side
                        quantity_lots: 10 + i as i64,
                        tif: TimeInForce::Gtc as i32,
                    })
                    .await
                    .unwrap()
                    .into_inner();
                ids.push(r.order.unwrap().order_id);
            }
            let mut cancels = Vec::new();
            for id in ids {
                let url = url.clone();
                cancels.push(tokio::spawn(async move {
                    let mut c = EngineClient::connect(url).await.unwrap();
                    c.cancel_order(CancelOrderRequest {
                        account_id: format!("acct-{a}"),
                        order_id: id,
                    })
                    .await
                    .map(|r| r.into_inner().order.unwrap().remaining_lots)
                }));
            }
            let mut cancelled = 0u64;
            for cancel in cancels {
                let remaining = cancel.await.unwrap().expect("cancel of a resting order succeeds");
                assert!(remaining > 0, "nothing can fill: the sides never cross");
                cancelled += 1;
            }
            cancelled
        }));
    }
    let mut total = 0;
    for t in tasks {
        total += t.await.unwrap();
    }
    assert_eq!(total, 8 * per_account);
    let mut c = EngineClient::connect(url).await.unwrap();
    let book = c
        .get_order_book(GetOrderBookRequest { depth: 200 })
        .await
        .unwrap()
        .into_inner();
    assert!(
        book.bids.is_empty() && book.asks.is_empty(),
        "book must be empty: {book:?}"
    );
    handle.shutdown().await;
}

/// The engine restarts from its journal with the same orders, the same ids and a continuing
/// sequence, and a retry of an old client id after the restart is still recognised.
#[tokio::test]
async fn restart_replays_the_journal() {
    let path = std::env::temp_dir().join(format!(
        "clob-restart-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&path);
    let cfg = EngineConfig {
        journal_path: Some(path.clone()),
        journal_fsync: true,
        ..EngineConfig::default()
    };
    let (addr, handle) = serve("127.0.0.1:0".parse().unwrap(), cfg.clone()).await.unwrap();
    let mut c = EngineClient::connect(format!("http://{addr}")).await.unwrap();
    let order = |cid: &str, side: Side, price: i64, lots: i64| PlaceOrderRequest {
        account_id: "a".into(),
        client_order_id: cid.into(),
        side: side as i32,
        price_ticks: price,
        quantity_lots: lots,
        tif: TimeInForce::Gtc as i32,
    };
    let first = c
        .place_order(order("k1", Side::Buy, 299_000, 500))
        .await
        .unwrap()
        .into_inner();
    c.place_order(order("k2", Side::Buy, 298_000, 300)).await.unwrap();
    c.place_order(PlaceOrderRequest {
        account_id: "b".into(),
        ..order("k3", Side::Sell, 299_000, 200)
    })
    .await
    .unwrap(); // trades 200 against k1
    let id2 = c
        .place_order(order("k4", Side::Buy, 297_000, 100))
        .await
        .unwrap()
        .into_inner()
        .order
        .unwrap()
        .order_id;
    c.cancel_order(CancelOrderRequest {
        account_id: "a".into(),
        order_id: id2,
    })
    .await
    .unwrap();
    let before = c
        .list_orders(clob_proto::v1::ListOrdersRequest {
            account_id: "a".into(),
            status: 0,
            limit: 100,
        })
        .await
        .unwrap()
        .into_inner();
    let book_before = c
        .get_order_book(GetOrderBookRequest { depth: 10 })
        .await
        .unwrap()
        .into_inner();
    handle.shutdown().await;

    let (addr, handle) = serve("127.0.0.1:0".parse().unwrap(), cfg).await.unwrap();
    let mut c = EngineClient::connect(format!("http://{addr}")).await.unwrap();
    let after = c
        .list_orders(clob_proto::v1::ListOrdersRequest {
            account_id: "a".into(),
            status: 0,
            limit: 100,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        after, before,
        "orders, statuses, remaining quantities and sequences survive"
    );
    let book_after = c
        .get_order_book(GetOrderBookRequest { depth: 10 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (book_after.bids, book_after.asks, book_after.sequence),
        (book_before.bids, book_before.asks, book_before.sequence)
    );
    // Idempotency survives too: the same client id replays the original order rather than a new one.
    let again = c
        .place_order(order("k1", Side::Buy, 299_000, 500))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.order.unwrap().order_id, first.order.unwrap().order_id);
    // And new orders continue the id and sequence counters.
    let next = c
        .place_order(order("k5", Side::Buy, 296_000, 100))
        .await
        .unwrap()
        .into_inner();
    assert!(next.order.unwrap().sequence > book_before.sequence);
    handle.shutdown().await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn idempotent_retry_and_status_codes() {
    let (addr, handle) = serve("127.0.0.1:0".parse().unwrap(), EngineConfig::default())
        .await
        .unwrap();
    let mut c = EngineClient::connect(format!("http://{addr}")).await.unwrap();
    let req = PlaceOrderRequest {
        account_id: "a".into(),
        client_order_id: "k1".into(),
        side: Side::Buy as i32,
        price_ticks: 300_000,
        quantity_lots: 500,
        tif: TimeInForce::Gtc as i32,
    };
    let first = c.place_order(req.clone()).await.unwrap().into_inner();
    let again = c.place_order(req.clone()).await.unwrap().into_inner();
    assert_eq!(
        first.order.as_ref().unwrap().order_id,
        again.order.as_ref().unwrap().order_id
    );
    let changed = c
        .place_order(PlaceOrderRequest {
            quantity_lots: 600,
            ..req.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(changed.code(), tonic::Code::AlreadyExists);
    let bad = c
        .place_order(PlaceOrderRequest {
            quantity_lots: 0,
            ..req.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(bad.code(), tonic::Code::InvalidArgument);
    let missing = c
        .cancel_order(CancelOrderRequest {
            account_id: "a".into(),
            order_id: "99".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(missing.code(), tonic::Code::NotFound);
    let id = first.order.unwrap().order_id;
    let foreign = c
        .cancel_order(CancelOrderRequest {
            account_id: "b".into(),
            order_id: id.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(foreign.code(), tonic::Code::PermissionDenied);
    c.cancel_order(CancelOrderRequest {
        account_id: "a".into(),
        order_id: id.clone(),
    })
    .await
    .unwrap();
    let twice = c
        .cancel_order(CancelOrderRequest {
            account_id: "a".into(),
            order_id: id,
        })
        .await
        .unwrap_err();
    assert_eq!(twice.code(), tonic::Code::FailedPrecondition);
    handle.shutdown().await;
}
