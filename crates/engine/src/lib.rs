//! The matching engine.
//!
//! * [`book`] is the pure order book: no I/O, no threads, no clocks.
//! * [`sequencer`] owns one `Book` on a dedicated thread and serialises every command through a
//!   bounded channel, which is what makes the engine thread-safe without a lock on the book.
//! * [`journal`] is the write-ahead log of commands and its replay: durability for a
//!   deterministic book needs only the inputs.

pub mod book;
pub mod journal;
pub mod sequencer;

pub use book::{
    Balances, Book, BookBuilder, BookState, CancelReason, EngineError, Event, ExposureLimits, Ledger, LevelView,
    MAX_PRICE, MAX_QTY, Order, OrderId, PlaceRequest, Price, Qty, RECENT_EVENTS, RETAINED_CLOSED_ORDERS,
    RETAINED_FOR_NS, RETAINED_TRADES, Retention, Seq, Side, Snapshot, SnapshotHeader, SnapshotLine, Status, Tif, Trade,
    TradeId,
};
pub use journal::{Journal, Record};
pub use sequencer::{Command, EVENT_BUFFER, EngineHandle, Reply, Stats, Top, spawn, spawn_with_journal};
