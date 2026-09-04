//! Single-writer sequencer.
//!
//! The `Book` is moved into one dedicated OS thread. Everything else, including every gRPC handler
//! task, holds only an [`EngineHandle`]: a channel sender plus a lock-free pointer to the latest
//! published snapshot. Commands are applied in arrival order, so the queue order is the market's
//! sequence order; no lock ever guards the book, and the compiler enforces that nothing else can
//! touch it.

use crate::book::*;
use crate::journal::{Journal, Record};
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Events kept for a slow subscriber before it is told it lagged.
pub const EVENT_BUFFER: usize = 8_192;

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
    Deposit {
        account: String,
        usdc: u128,
        eth: Qty,
    },
    Balances {
        account: String,
    },
    Withdraw {
        account: String,
        usdc: u128,
        eth: Qty,
    },
    Statement {
        account: String,
    },
}

/// The best prices right after a command was applied, returned with the reply so a caller need
/// not read the book again to report the market after its own action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Top {
    pub best_bid: Option<Price>,
    pub best_ask: Option<Price>,
    pub seq: Seq,
}

impl Top {
    pub fn of(book: &Book) -> Self {
        Self {
            best_bid: book.best_bid(),
            best_ask: book.best_ask(),
            seq: book.seq(),
        }
    }
}

pub enum Reply {
    Placed(Order, Vec<Trade>, Top),
    Cancelled(Order, Top),
    Order(Order),
    Orders(Vec<Order>),
    Trades(Vec<Trade>),
    Balances(Balances),
    Statement(Ledger),
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
    events: broadcast::Sender<Arc<Event>>,
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Journals a write command (reads are not journaled) and applies it. The record goes to the
/// journal's buffer first; the batch loop commits the buffer before any reply is sent.
fn apply(book: &mut Book, journal: &mut Option<Journal>, cmd: Command, now: i64) -> Result<Reply, EngineError> {
    if let Some(j) = journal {
        let record = match &cmd {
            Command::Place(req) => Some(Record::Place {
                t: now,
                req: req.clone(),
            }),
            Command::Cancel { account, id } => Some(Record::Cancel {
                t: now,
                account: account.clone(),
                id: *id,
            }),
            Command::Deposit { account, usdc, eth } => Some(Record::Deposit {
                t: now,
                account: account.clone(),
                usdc: *usdc,
                eth: *eth,
            }),
            Command::Withdraw { account, usdc, eth } => Some(Record::Withdraw {
                t: now,
                account: account.clone(),
                usdc: *usdc,
                eth: *eth,
            }),
            _ => None,
        };
        if let Some(r) = record {
            if let Err(e) = j.append(&r) {
                // A journal that cannot be written must not silently become a non-durable engine.
                tracing_error(&e);
                return Err(EngineError::Shutdown);
            }
        }
    }
    match cmd {
        Command::Place(req) => {
            let (o, f) = book.place(req, now)?;
            Ok(Reply::Placed(o, f, Top::of(book)))
        }
        Command::Cancel { account, id } => {
            let o = book.cancel(&account, id)?;
            Ok(Reply::Cancelled(o, Top::of(book)))
        }
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
        Command::Deposit { account, usdc, eth } => book.deposit(&account, usdc, eth).map(Reply::Balances),
        Command::Balances { account } => Ok(Reply::Balances(book.balances(&account))),
        Command::Withdraw { account, usdc, eth } => book.withdraw(&account, usdc, eth).map(Reply::Balances),
        Command::Statement { account } => Ok(Reply::Statement(book.statement(&account))),
    }
}

/// Largest number of queued commands applied between two snapshot publications.
pub const MAX_BATCH: usize = 256;

fn tracing_error(e: &std::io::Error) {
    eprintln!("engine journal write failed: {e}");
}

/// Starts the matcher thread. `capacity` bounds the command queue (backpressure); `depth` is the
/// number of levels per side kept in the published snapshot.
///
/// Commands are applied in batches: the thread blocks for the first command, then drains whatever
/// else is already queued (up to [`MAX_BATCH`]), publishes one snapshot, and only then sends the
/// replies. Under load this amortises the snapshot over many commands instead of rebuilding it per
/// command; publishing before replying means a client that reads the book after its own reply
/// always sees its own order.
pub fn spawn(book: Book, capacity: usize, depth: usize) -> EngineHandle {
    spawn_with_journal(book, capacity, depth, None)
}

/// [`spawn`] with a write-ahead journal: every place and cancel is appended before it is applied
/// and the journal is committed once per batch, before the batch's replies are sent. Replay the
/// journal into the `book` before calling this to recover a previous run.
pub fn spawn_with_journal(mut book: Book, capacity: usize, depth: usize, mut journal: Option<Journal>) -> EngineHandle {
    let (tx, mut rx) = mpsc::channel::<Envelope>(capacity.max(1));
    let snapshot = Arc::new(ArcSwap::from_pointee(book.snapshot(depth)));
    let published = Arc::clone(&snapshot);
    let (events, _) = broadcast::channel::<Arc<Event>>(EVENT_BUFFER);
    let feed = events.clone();
    // Events already in the book (a replayed journal) are history, not news.
    book.take_new_events();
    std::thread::Builder::new()
        .name("matcher".into())
        .spawn(move || {
            // The only code that ever touches `book`.
            let mut pending: Vec<(ReplySender, Result<Reply, EngineError>)> = Vec::with_capacity(MAX_BATCH);
            while let Some(env) = rx.blocking_recv() {
                let now = now_ns();
                pending.push((env.reply, apply(&mut book, &mut journal, env.cmd, now)));
                while pending.len() < MAX_BATCH {
                    match rx.try_recv() {
                        Ok(env) => pending.push((env.reply, apply(&mut book, &mut journal, env.cmd, now))),
                        Err(_) => break,
                    }
                }
                if let Some(j) = journal.as_mut() {
                    if let Err(e) = j.commit() {
                        tracing_error(&e);
                        for (reply, _) in pending.drain(..) {
                            let _ = reply.send(Err(EngineError::Shutdown));
                        }
                        continue;
                    }
                }
                published.store(Arc::new(book.snapshot(depth)));
                // Subscribers see every event of the batch, in sequence order, after the snapshot.
                for e in book.take_new_events() {
                    let _ = feed.send(Arc::new(e));
                }
                for (reply, result) in pending.drain(..) {
                    let _ = reply.send(result);
                }
            }
        })
        .expect("spawn matcher thread");
    EngineHandle { tx, snapshot, events }
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

    /// A live feed of every event from now on, in sequence order. A receiver that falls more than
    /// [`EVENT_BUFFER`] events behind gets `Lagged` and should resynchronise from a snapshot.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }

    /// Latest published top of book. Lock-free: a pointer load, no channel round trip.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }
}
