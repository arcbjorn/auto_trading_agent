//! tonic servicer for `clob.v1.Engine`. Every RPC is a thin translation: protobuf in, a command to
//! the sequencer, protobuf out. Engine errors map one-to-one onto gRPC status codes.

use clob_proto::v1 as pb;
use clob_proto::v1::engine_server::{Engine, EngineServer};
use engine::{spawn, Book, Command, EngineError, EngineHandle, PlaceRequest, Reply};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Request, Response, Status};

pub const SYMBOL: &str = "ETH-USDC";
pub const MAX_DEPTH: u32 = 200;
pub const MAX_LIST: u32 = 10_000;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Bounded command queue; a full queue answers RESOURCE_EXHAUSTED instead of growing memory.
    pub queue_capacity: usize,
    /// Levels per side in the published snapshot (the maximum `GetOrderBook` depth).
    pub snapshot_depth: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 10_000,
            snapshot_depth: MAX_DEPTH as usize,
        }
    }
}

pub struct Svc {
    engine: EngineHandle,
}

impl Svc {
    pub fn new(engine: EngineHandle) -> Self {
        Self { engine }
    }
}

pub fn to_status(e: EngineError) -> Status {
    match e {
        EngineError::Invalid(m) => Status::invalid_argument(m),
        EngineError::NotFound(_) => Status::not_found(e.to_string()),
        EngineError::Precondition(..) => Status::failed_precondition(e.to_string()),
        EngineError::Forbidden(_) => Status::permission_denied(e.to_string()),
        EngineError::AlreadyExists => Status::already_exists(e.to_string()),
        EngineError::Busy => Status::resource_exhausted(e.to_string()),
        EngineError::Shutdown => Status::unavailable(e.to_string()),
    }
}

fn side_from_pb(v: i32) -> Result<engine::Side, Status> {
    match pb::Side::try_from(v) {
        Ok(pb::Side::Buy) => Ok(engine::Side::Buy),
        Ok(pb::Side::Sell) => Ok(engine::Side::Sell),
        _ => Err(Status::invalid_argument("side must be BUY or SELL")),
    }
}

fn tif_from_pb(v: i32) -> engine::Tif {
    match pb::TimeInForce::try_from(v) {
        Ok(pb::TimeInForce::Ioc) => engine::Tif::Ioc,
        Ok(pb::TimeInForce::Fok) => engine::Tif::Fok,
        _ => engine::Tif::Gtc,
    }
}

fn status_to_pb(s: engine::Status) -> pb::OrderStatus {
    match s {
        engine::Status::Open => pb::OrderStatus::Open,
        engine::Status::PartiallyFilled => pb::OrderStatus::PartiallyFilled,
        engine::Status::Filled => pb::OrderStatus::Filled,
        engine::Status::Cancelled => pb::OrderStatus::Cancelled,
        engine::Status::Rejected => pb::OrderStatus::Rejected,
    }
}

pub fn order_to_pb(o: &engine::Order) -> pb::Order {
    pb::Order {
        order_id: o.id.to_string(),
        account_id: o.account.to_string(),
        client_order_id: o.client_order_id.to_string(),
        side: side_to_pb(o.side) as i32,
        price_ticks: o.price as i64,
        quantity_lots: o.qty as i64,
        remaining_lots: o.remaining as i64,
        status: status_to_pb(o.status) as i32,
        sequence: o.seq,
        created_at_unix_ns: o.created_at_unix_ns,
    }
}

fn side_to_pb(s: engine::Side) -> pb::Side {
    match s {
        engine::Side::Buy => pb::Side::Buy,
        engine::Side::Sell => pb::Side::Sell,
    }
}

/// `viewer` is the account the listing is scoped to: the other party's id is left blank.
pub fn trade_to_pb(t: &engine::Trade, viewer: Option<&str>) -> pb::Trade {
    let show = |a: &str| viewer.is_none_or(|v| v == a);
    pb::Trade {
        trade_id: t.id.to_string(),
        maker_order_id: t.maker.to_string(),
        taker_order_id: t.taker.to_string(),
        price_ticks: t.price as i64,
        quantity_lots: t.qty as i64,
        sequence: t.seq,
        executed_at_unix_ns: t.executed_at_unix_ns,
        taker_side: side_to_pb(t.taker_side) as i32,
        maker_account: if show(&t.maker_account) {
            t.maker_account.to_string()
        } else {
            String::new()
        },
        taker_account: if show(&t.taker_account) {
            t.taker_account.to_string()
        } else {
            String::new()
        },
    }
}

fn level_to_pb(l: &engine::LevelView) -> pb::PriceLevel {
    pb::PriceLevel {
        price_ticks: l.price as i64,
        quantity_lots: l.qty as i64,
        order_count: l.orders,
    }
}

fn parse_id(s: &str) -> Result<u64, Status> {
    s.trim()
        .parse()
        .map_err(|_| Status::invalid_argument(format!("order_id {s:?} is not a numeric id")))
}

fn limit(v: u32) -> usize {
    if v == 0 {
        10
    } else {
        v.min(MAX_LIST) as usize
    }
}

#[tonic::async_trait]
impl Engine for Svc {
    async fn place_order(
        &self,
        req: Request<pb::PlaceOrderRequest>,
    ) -> Result<Response<pb::PlaceOrderResponse>, Status> {
        let r = req.into_inner();
        if r.price_ticks <= 0 || r.quantity_lots <= 0 {
            return Err(Status::invalid_argument(
                "price_ticks and quantity_lots must be positive",
            ));
        }
        let cmd = Command::Place(PlaceRequest {
            account: r.account_id,
            client_order_id: r.client_order_id,
            side: side_from_pb(r.side)?,
            price: r.price_ticks as u64,
            qty: r.quantity_lots as u64,
            tif: tif_from_pb(r.tif),
        });
        match self.engine.submit(cmd).await.map_err(to_status)? {
            Reply::Placed(order, fills) => Ok(Response::new(pb::PlaceOrderResponse {
                fills: fills.iter().map(|t| trade_to_pb(t, Some(&order.account))).collect(),
                order: Some(order_to_pb(&order)),
            })),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn cancel_order(
        &self,
        req: Request<pb::CancelOrderRequest>,
    ) -> Result<Response<pb::CancelOrderResponse>, Status> {
        let r = req.into_inner();
        let cmd = Command::Cancel {
            account: r.account_id,
            id: parse_id(&r.order_id)?,
        };
        match self.engine.submit(cmd).await.map_err(to_status)? {
            Reply::Cancelled(o) => Ok(Response::new(pb::CancelOrderResponse {
                order: Some(order_to_pb(&o)),
            })),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn get_order(&self, req: Request<pb::GetOrderRequest>) -> Result<Response<pb::Order>, Status> {
        let r = req.into_inner();
        let cmd = Command::Get {
            account: r.account_id,
            id: parse_id(&r.order_id)?,
        };
        match self.engine.submit(cmd).await.map_err(to_status)? {
            Reply::Order(o) => Ok(Response::new(order_to_pb(&o))),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn list_orders(
        &self,
        req: Request<pb::ListOrdersRequest>,
    ) -> Result<Response<pb::ListOrdersResponse>, Status> {
        let r = req.into_inner();
        let (status, live_only) = match pb::OrderStatus::try_from(r.status) {
            Ok(pb::OrderStatus::Open) => (None, true), // OPEN filter = every live order
            Ok(pb::OrderStatus::PartiallyFilled) => (Some(engine::Status::PartiallyFilled), false),
            Ok(pb::OrderStatus::Filled) => (Some(engine::Status::Filled), false),
            Ok(pb::OrderStatus::Cancelled) => (Some(engine::Status::Cancelled), false),
            Ok(pb::OrderStatus::Rejected) => (Some(engine::Status::Rejected), false),
            _ => (None, false),
        };
        let cmd = Command::Orders {
            account: r.account_id,
            status,
            live_only,
            limit: limit(r.limit),
        };
        match self.engine.submit(cmd).await.map_err(to_status)? {
            Reply::Orders(v) => Ok(Response::new(pb::ListOrdersResponse {
                orders: v.iter().map(order_to_pb).collect(),
            })),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn list_trades(
        &self,
        req: Request<pb::ListTradesRequest>,
    ) -> Result<Response<pb::ListTradesResponse>, Status> {
        let r = req.into_inner();
        let account = if r.account_id.is_empty() {
            None
        } else {
            Some(r.account_id)
        };
        let cmd = Command::Trades {
            account: account.clone(),
            limit: limit(r.limit),
        };
        match self.engine.submit(cmd).await.map_err(to_status)? {
            Reply::Trades(v) => Ok(Response::new(pb::ListTradesResponse {
                trades: v.iter().map(|t| trade_to_pb(t, account.as_deref())).collect(),
            })),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn get_order_book(&self, req: Request<pb::GetOrderBookRequest>) -> Result<Response<pb::OrderBook>, Status> {
        let depth = req.into_inner().depth;
        let depth = if depth == 0 { 5 } else { depth.min(MAX_DEPTH) } as usize;
        let snap = self.engine.snapshot(); // lock-free: no channel round trip for reads
        Ok(Response::new(pb::OrderBook {
            symbol: SYMBOL.into(),
            bids: snap.bids.iter().take(depth).map(level_to_pb).collect(),
            asks: snap.asks.iter().take(depth).map(level_to_pb).collect(),
            sequence: snap.seq,
        }))
    }

    async fn get_market(&self, _req: Request<pb::GetMarketRequest>) -> Result<Response<pb::Market>, Status> {
        let snap = self.engine.snapshot();
        Ok(Response::new(pb::Market {
            symbol: SYMBOL.into(),
            tick_size_ticks: 1,
            lot_size_lots: 1,
            best_bid_ticks: snap.best_bid().map(|p| p as i64).unwrap_or(0),
            best_ask_ticks: snap.best_ask().map(|p| p as i64).unwrap_or(0),
            last_trade_price_ticks: snap.last_trade_price.map(|p| p as i64).unwrap_or(0),
            sequence: snap.seq,
        }))
    }
}

/// A running server. Dropping the handle does not stop the server; call [`ServerHandle::shutdown`].
pub struct ServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl ServerHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
    }
}

/// Binds `addr` (port 0 picks a free port), starts a fresh engine and serves it. Returns the bound
/// address so tests and the evaluation harness can run the real server in-process.
pub async fn serve(addr: SocketAddr, cfg: EngineConfig) -> anyhow::Result<(SocketAddr, ServerHandle)> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let engine = spawn(Book::new(), cfg.queue_capacity, cfg.snapshot_depth);
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(
        Server::builder()
            .add_service(EngineServer::new(Svc::new(engine)))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = rx.await;
            }),
    );
    Ok((
        bound,
        ServerHandle {
            shutdown: Some(tx),
            task,
        },
    ))
}
