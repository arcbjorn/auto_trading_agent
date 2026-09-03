//! The matching engine.
//!
//! * [`book`] is the pure order book: no I/O, no threads, no clocks.
//! * [`sequencer`] owns one `Book` on a dedicated thread and serialises every command through a
//!   bounded channel, which is what makes the engine thread-safe without a lock on the book.

pub mod book;
pub mod sequencer;

pub use book::{
    Book, CancelReason, EngineError, Event, LevelView, Order, OrderId, PlaceRequest, Price, Qty, Seq, Side, Snapshot,
    Status, Tif, Trade, TradeId,
};
pub use sequencer::{spawn, Command, EngineHandle, Reply};
