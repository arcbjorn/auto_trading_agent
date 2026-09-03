//! Single-writer sequencer.
//!
//! The `Book` is moved into one dedicated OS thread. Everything else, including every gRPC handler
//! task, holds only an [`EngineHandle`]: a channel sender plus a lock-free pointer to the latest
//! published snapshot. Commands are applied in arrival order, so the queue order is the market's
//! sequence order; no lock ever guards the book, and the compiler enforces that nothing else can
//! touch it.

use crate::book::*;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

pub enum Command {
    Place(PlaceRequest),
    Cancel {
        account: String,
        id: OrderId,
    },
    Get {
        account: String,
        id: OrderId,
    },
    /// Orders of an account, newest first. `live_only` keeps Open and PartiallyFilled orders.
    Orders {
        account: String,
        status: Option<Status>,
        live_only: bool,
        limit: usize,
    },
    Trades {
        account: Option<String>,
        limit: usize,
    },
}

pub enum Reply {
    Placed(Order, Vec<Trade>),
    Cancelled(Order),
    Order(Order),
    Orders(Vec<Order>),
    Trades(Vec<Trade>),
}

struct Envelope {
    cmd: Command,
    reply: oneshot::Sender<Result<Reply, EngineError>>,
}

#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<Envelope>,
    snapshot: Arc<ArcSwap<Snapshot>>,
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn apply(book: &mut Book, cmd: Command) -> Result<Reply, EngineError> {
    match cmd {
        Command::Place(req) => book.place(req, now_ns()).map(|(o, f)| Reply::Placed(o, f)),
        Command::Cancel { account, id } => book.cancel(&account, id).map(Reply::Cancelled),
        Command::Get { account, id } => match book.order(id) {
            Some(o) if o.account == account => Ok(Reply::Order(o.clone())),
            Some(_) => Err(EngineError::Forbidden(id)),
            None => Err(EngineError::NotFound(id)),
        },
        Command::Orders {
            account,
            status,
            live_only,
            limit,
        } => Ok(Reply::Orders(book.orders_for(
            &account,
            |o| {
                if live_only {
                    o.status.is_live()
                } else {
                    status.is_none_or(|s| o.status == s)
                }
            },
            limit,
        ))),
        Command::Trades { account, limit } => Ok(Reply::Trades(book.trades(account.as_deref(), limit))),
    }
}

/// Starts the matcher thread. `capacity` bounds the command queue (backpressure); `depth` is the
/// number of levels per side kept in the published snapshot.
pub fn spawn(mut book: Book, capacity: usize, depth: usize) -> EngineHandle {
    let (tx, mut rx) = mpsc::channel::<Envelope>(capacity.max(1));
    let snapshot = Arc::new(ArcSwap::from_pointee(book.snapshot(depth)));
    let published = Arc::clone(&snapshot);
    std::thread::Builder::new()
        .name("matcher".into())
        .spawn(move || {
            // The only code that ever touches `book`.
            while let Some(env) = rx.blocking_recv() {
                let reply = apply(&mut book, env.cmd);
                let _ = env.reply.send(reply);
                published.store(Arc::new(book.snapshot(depth)));
            }
        })
        .expect("spawn matcher thread");
    EngineHandle { tx, snapshot }
}

impl EngineHandle {
    /// Queues a command and waits for the matcher's reply. Fails fast with `Busy` when the bounded
    /// queue is full instead of growing memory.
    pub async fn submit(&self, cmd: Command) -> Result<Reply, EngineError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(Envelope { cmd, reply: tx })
            .map_err(|_| EngineError::Busy)?;
        rx.await.map_err(|_| EngineError::Shutdown)?
    }

    /// Latest published top of book. Lock-free: a pointer load, no channel round trip.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }
}
