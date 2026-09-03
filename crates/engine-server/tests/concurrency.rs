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
