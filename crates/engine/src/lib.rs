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
    Balances, Book, BookState, CancelReason, EngineError, Event, Ledger, LevelView, Order, OrderId, PlaceRequest,
    Price, Qty, Seq, Side, Snapshot, Status, Tif, Trade, TradeId,
};
pub use journal::{Journal, Record};
pub use sequencer::{spawn, spawn_with_journal, Command, EngineHandle, Reply, EVENT_BUFFER};
