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

type ReplySender = oneshot::Sender<Result<Reply, EngineError>>;

struct Envelope {
    cmd: Command,
    reply: ReplySender,
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
            Some(o) if *o.account == *account => Ok(Reply::Order(o.clone())),
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

/// Largest number of queued commands applied between two snapshot publications.
pub const MAX_BATCH: usize = 256;

/// Starts the matcher thread. `capacity` bounds the command queue (backpressure); `depth` is the
/// number of levels per side kept in the published snapshot.
///
/// Commands are applied in batches: the thread blocks for the first command, then drains whatever
/// else is already queued (up to [`MAX_BATCH`]), publishes one snapshot, and only then sends the
/// replies. Under load this amortises the snapshot over many commands instead of rebuilding it per
/// command; publishing before replying means a client that reads the book after its own reply
/// always sees its own order.
pub fn spawn(mut book: Book, capacity: usize, depth: usize) -> EngineHandle {
    let (tx, mut rx) = mpsc::channel::<Envelope>(capacity.max(1));
    let snapshot = Arc::new(ArcSwap::from_pointee(book.snapshot(depth)));
    let published = Arc::clone(&snapshot);
    std::thread::Builder::new()
        .name("matcher".into())
        .spawn(move || {
            // The only code that ever touches `book`.
            let mut pending: Vec<(ReplySender, Result<Reply, EngineError>)> = Vec::with_capacity(MAX_BATCH);
            while let Some(env) = rx.blocking_recv() {
                pending.push((env.reply, apply(&mut book, env.cmd)));
                while pending.len() < MAX_BATCH {
                    match rx.try_recv() {
                        Ok(env) => pending.push((env.reply, apply(&mut book, env.cmd))),
                        Err(_) => break,
                    }
                }
                published.store(Arc::new(book.snapshot(depth)));
                for (reply, result) in pending.drain(..) {
                    let _ = reply.send(result);
                }
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

    /// Number of commands that can still be queued before `submit` answers `Busy`.
    pub fn free_capacity(&self) -> usize {
        self.tx.capacity()
    }

    /// Latest published top of book. Lock-free: a pointer load, no channel round trip.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }
}
