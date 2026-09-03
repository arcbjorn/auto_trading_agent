//! Pure matching engine for one trading pair.
//!
//! Nothing here performs I/O, spawns threads, or reads a clock. Given the same sequence of calls
//! (including the `now_ns` arguments) the book produces the same sequence of events, which is what
//! makes the engine replayable and the evaluations reproducible.
//!
//! Matching rules:
//! * price-time priority: best price first, first-in-first-out within a price;
//! * a buy at P crosses asks priced at or below P, a sell at P crosses bids at or above P;
//! * trades execute at the resting (maker) order's price;
//! * the unfilled remainder of a GTC order rests; IOC cancels it; FOK checks availability first and
//!   rejects without touching the book;
//! * self-trade prevention, "cancel newest": an incoming order never matches its own account's
//!   resting order; matching stops there and the remainder is cancelled.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

/// Price in ticks. 1 tick = 0.01 USDC per ETH.
pub type Price = u64;
/// Quantity in lots. 1 lot = 0.0001 ETH.
pub type Qty = u64;
pub type OrderId = u64;
pub type TradeId = u64;
/// Position in the engine's total order of events.
pub type Seq = u64;
/// Longest account or client order id accepted. Ids are echoed back in listings, so a bound keeps
/// them from becoming a channel for arbitrary text.
pub const MAX_ID_LEN: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tif {
    Gtc,
    Ioc,
    Fok,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

impl Status {
    /// Live orders sit in the book and can still trade.
    pub fn is_live(self) -> bool {
        matches!(self, Status::Open | Status::PartiallyFilled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    User,
    Ioc,
    Fok,
    SelfTradePrevention,
}

/// Orders are cloned on every reply and event, so the two strings are shared, not copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub account: Arc<str>,
    pub client_order_id: Arc<str>,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub remaining: Qty,
    pub status: Status,
    pub seq: Seq,
    /// Wall clock at acceptance, for reporting only. Never used for ordering.
    pub created_at_unix_ns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trade {
    pub id: TradeId,
    pub maker: OrderId,
    pub taker: OrderId,
    pub maker_account: Arc<str>,
    pub taker_account: Arc<str>,
    /// The aggressor's side; the maker took the other side.
    pub taker_side: Side,
    /// Always the maker's price.
    pub price: Price,
    pub qty: Qty,
    pub seq: Seq,
    pub executed_at_unix_ns: i64,
}

impl Trade {
    /// The side `account` traded on, if it took part.
    pub fn side_for(&self, account: &str) -> Option<Side> {
        if &*self.taker_account == account {
            Some(self.taker_side)
        } else if &*self.maker_account == account {
            Some(self.taker_side.opposite())
        } else {
            None
        }
    }
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

/// Append-only history. Order and trade listings are derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Accepted(Order),
    Traded(Trade),
    Cancelled {
        id: OrderId,
        reason: CancelReason,
        seq: Seq,
    },
    Rejected {
        id: OrderId,
        reason: String,
        seq: Seq,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("order {0} not found")]
    NotFound(OrderId),
    #[error("order {0} is already {1:?}")]
    Precondition(OrderId, Status),
    #[error("order {0} belongs to another account")]
    Forbidden(OrderId),
    #[error("client_order_id was already used with different parameters")]
    AlreadyExists,
    #[error("engine busy, retry")]
    Busy,
    #[error("engine shut down")]
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceRequest {
    pub account: String,
    /// Idempotency key, unique per account.
    pub client_order_id: String,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub tif: Tif,
}

#[derive(Default)]
struct Level {
    /// Open quantity at this price: what the book displays.
    total: Qty,
    /// Number of live orders at this price, maintained incrementally so snapshots never scan queues.
    live: u32,
    /// Order ids in arrival order. Time priority is this queue.
    queue: VecDeque<OrderId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelView {
    pub price: Price,
    pub qty: Qty,
    pub orders: u32,
}

/// Aggregated view of the top of the book.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Highest price first.
    pub bids: Vec<LevelView>,
    /// Lowest price first.
    pub asks: Vec<LevelView>,
    pub last_trade_price: Option<Price>,
    pub seq: Seq,
}

impl Snapshot {
    pub fn best_bid(&self) -> Option<Price> {
        self.bids.first().map(|l| l.price)
    }
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.first().map(|l| l.price)
    }
}

#[derive(Default)]
pub struct Book {
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
    orders: HashMap<OrderId, Order>,
    /// Order ids per account in placement order, so listings never scan the whole book.
    by_account: HashMap<Arc<str>, Vec<OrderId>>,
    /// account -> client_order_id -> order: the idempotency index.
    by_client_id: HashMap<Arc<str>, HashMap<Arc<str>, OrderId>>,
    /// The reply given when an order was first placed, replayed for idempotent retries.
    original_replies: HashMap<OrderId, (Order, Vec<Trade>)>,
    events: Vec<Event>,
    /// Every trade in sequence order, plus the positions each account took part in.
    trades: Vec<Trade>,
    trades_by_account: HashMap<Arc<str>, Vec<usize>>,
    last_trade_price: Option<Price>,
    next_order: OrderId,
    next_trade: TradeId,
    next_seq: Seq,
}

impl Book {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump_seq(&mut self) -> Seq {
        self.next_seq += 1;
        self.next_seq
    }

    /// Quantity an incoming order could take right now, walking price-time priority and stopping
    /// at the first resting order from the same account (where matching would stop too).
    fn available(&self, side: Side, price: Price, account: &str, wanted: Qty) -> Qty {
        match side {
            Side::Buy => self.walk(self.asks.range(..=price).map(|(_, l)| l), account, wanted),
            Side::Sell => self.walk(self.bids.range(price..).rev().map(|(_, l)| l), account, wanted),
        }
    }

    fn walk<'a>(&self, levels: impl Iterator<Item = &'a Level>, account: &str, wanted: Qty) -> Qty {
        let mut got = 0;
        for level in levels {
            for id in &level.queue {
                let o = &self.orders[id];
                if !o.status.is_live() {
                    continue;
                }
                if &*o.account == account {
                    return got;
                }
                got += o.remaining;
                if got >= wanted {
                    return got;
                }
            }
        }
        got
    }

    /// Places a limit order. Returns the order's state after matching and the fills it caused.
    pub fn place(&mut self, req: PlaceRequest, now_ns: i64) -> Result<(Order, Vec<Trade>), EngineError> {
        if req.price == 0 || req.qty == 0 {
            return Err(EngineError::Invalid("price and quantity must be positive".into()));
        }
        if req.account.is_empty() {
            return Err(EngineError::Invalid("account_id is required".into()));
        }
        if req.client_order_id.is_empty() {
            return Err(EngineError::Invalid("client_order_id is required".into()));
        }
        if req.account.len() > MAX_ID_LEN || req.client_order_id.len() > MAX_ID_LEN {
            return Err(EngineError::Invalid(format!(
                "account_id and client_order_id must be at most {MAX_ID_LEN} bytes"
            )));
        }
        if let Some(&id) = self
            .by_client_id
            .get(req.account.as_str())
            .and_then(|m| m.get(req.client_order_id.as_str()))
        {
            let (original, fills) = &self.original_replies[&id];
            return if original.side == req.side && original.price == req.price && original.qty == req.qty {
                Ok((original.clone(), fills.clone()))
            } else {
                Err(EngineError::AlreadyExists)
            };
        }

        // Reuse the account's interned name so every order of an account shares one allocation.
        let account: Arc<str> = match self.by_account.get_key_value(req.account.as_str()) {
            Some((k, _)) => Arc::clone(k),
            None => Arc::from(req.account.as_str()),
        };
        let client_order_id: Arc<str> = Arc::from(req.client_order_id.as_str());
        let tif = req.tif;
        self.next_order += 1;
        let seq = self.bump_seq();
        let mut o = Order {
            id: self.next_order,
            account: Arc::clone(&account),
            client_order_id: Arc::clone(&client_order_id),
            side: req.side,
            price: req.price,
            qty: req.qty,
            remaining: req.qty,
            status: Status::Open,
            seq,
            created_at_unix_ns: now_ns,
        };
        self.by_client_id
            .entry(Arc::clone(&account))
            .or_default()
            .insert(client_order_id, o.id);
        self.by_account.entry(account).or_default().push(o.id);

        if tif == Tif::Fok && self.available(o.side, o.price, &o.account, o.qty) < o.qty {
            o.status = Status::Rejected;
            self.events.push(Event::Rejected {
                id: o.id,
                reason: "FOK: insufficient quantity available at this price".into(),
                seq: o.seq,
            });
            self.orders.insert(o.id, o.clone());
            self.original_replies.insert(o.id, (o.clone(), Vec::new()));
            return Ok((o, Vec::new()));
        }
        self.events.push(Event::Accepted(o.clone()));

        let mut fills = Vec::new();
        let mut self_trade = false;
        while o.remaining > 0 {
            let best = match o.side {
                Side::Buy => self.asks.keys().next().copied(),
                Side::Sell => self.bids.keys().next_back().copied(),
            };
            let Some(px) = best else { break };
            let crosses = match o.side {
                Side::Buy => px <= o.price,
                Side::Sell => px >= o.price,
            };
            if !crosses {
                break;
            }
            let opposite = match o.side {
                Side::Buy => &mut self.asks,
                Side::Sell => &mut self.bids,
            };
            let level = opposite.get_mut(&px).expect("best level exists");
            let Some(&maker_id) = level.queue.front() else {
                opposite.remove(&px);
                continue;
            };
            let maker = self.orders.get_mut(&maker_id).expect("queued order exists");
            if !maker.status.is_live() {
                // Lazily dropped: cancel only marks the order, the matcher removes it here.
                level.queue.pop_front();
                continue;
            }
            if maker.account == o.account {
                self_trade = true;
                break;
            }
            let q = o.remaining.min(maker.remaining);
            maker.remaining -= q;
            o.remaining -= q;
            level.total -= q;
            maker.status = if maker.remaining == 0 {
                Status::Filled
            } else {
                Status::PartiallyFilled
            };
            if maker.remaining == 0 {
                level.queue.pop_front();
                level.live -= 1;
            }
            let level_empty = level.total == 0;
            let maker_account = Arc::clone(&maker.account);
            self.next_trade += 1;
            self.next_seq += 1;
            let trade = Trade {
                id: self.next_trade,
                maker: maker_id,
                taker: o.id,
                maker_account: Arc::clone(&maker_account),
                taker_account: Arc::clone(&o.account),
                taker_side: o.side,
                price: px,
                qty: q,
                seq: self.next_seq,
                executed_at_unix_ns: now_ns,
            };
            self.last_trade_price = Some(px);
            self.events.push(Event::Traded(trade.clone()));
            let pos = self.trades.len();
            self.trades_by_account.entry(maker_account).or_default().push(pos);
            self.trades_by_account
                .entry(Arc::clone(&o.account))
                .or_default()
                .push(pos);
            self.trades.push(trade.clone());
            fills.push(trade);
            if level_empty {
                opposite.remove(&px);
            }
        }

        let mut cancel = None;
        if o.remaining > 0 {
            if self_trade {
                cancel = Some(CancelReason::SelfTradePrevention);
            } else {
                match tif {
                    Tif::Gtc => {
                        let same = match o.side {
                            Side::Buy => &mut self.bids,
                            Side::Sell => &mut self.asks,
                        };
                        let level = same.entry(o.price).or_default();
                        level.total += o.remaining;
                        level.live += 1;
                        level.queue.push_back(o.id);
                    }
                    Tif::Ioc => cancel = Some(CancelReason::Ioc),
                    Tif::Fok => cancel = Some(CancelReason::Fok),
                }
            }
        }
        o.status = if o.remaining == 0 {
            Status::Filled
        } else if cancel.is_some() {
            Status::Cancelled
        } else if o.remaining < o.qty {
            Status::PartiallyFilled
        } else {
            Status::Open
        };
        if let Some(reason) = cancel {
            let seq = self.bump_seq();
            self.events.push(Event::Cancelled { id: o.id, reason, seq });
        }
        self.orders.insert(o.id, o.clone());
        self.original_replies.insert(o.id, (o.clone(), fills.clone()));
        Ok((o, fills))
    }

    /// Cancels a live order owned by `account`. O(1): the level total is adjusted now and the
    /// matcher drops the order lazily when it reaches it.
    pub fn cancel(&mut self, account: &str, id: OrderId) -> Result<Order, EngineError> {
        let o = self.orders.get_mut(&id).ok_or(EngineError::NotFound(id))?;
        if &*o.account != account {
            return Err(EngineError::Forbidden(id));
        }
        if !o.status.is_live() {
            return Err(EngineError::Precondition(id, o.status));
        }
        o.status = Status::Cancelled;
        let (side, price, remaining) = (o.side, o.price, o.remaining);
        let same = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        if let Some(level) = same.get_mut(&price) {
            level.total -= remaining;
            level.live -= 1;
            if level.total == 0 {
                same.remove(&price);
            }
        }
        let seq = self.bump_seq();
        self.events.push(Event::Cancelled {
            id,
            reason: CancelReason::User,
            seq,
        });
        Ok(self.orders[&id].clone())
    }

    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// Orders of one account matching `filter`, newest first. Walks only that account's orders.
    pub fn orders_for(&self, account: &str, filter: impl Fn(&Order) -> bool, limit: usize) -> Vec<Order> {
        self.by_account
            .get(account)
            .map(|ids| {
                ids.iter()
                    .rev()
                    .map(|id| &self.orders[id])
                    .filter(|o| filter(o))
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Live orders of one account, oldest first.
    pub fn open_orders_for(&self, account: &str) -> Vec<Order> {
        self.by_account
            .get(account)
            .map(|ids| {
                ids.iter()
                    .map(|id| &self.orders[id])
                    .filter(|o| o.status.is_live())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Trades, newest first, optionally restricted to those where `account` was maker or taker.
    /// Walks only that account's trades.
    pub fn trades(&self, account: Option<&str>, limit: usize) -> Vec<Trade> {
        match account {
            None => self.trades.iter().rev().take(limit).cloned().collect(),
            Some(a) => self
                .trades_by_account
                .get(a)
                .map(|idx| idx.iter().rev().take(limit).map(|&i| self.trades[i].clone()).collect())
                .unwrap_or_default(),
        }
    }

    pub fn snapshot(&self, depth: usize) -> Snapshot {
        let view = |(&price, l): (&Price, &Level)| LevelView {
            price,
            qty: l.total,
            orders: l.live,
        };
        Snapshot {
            bids: self.bids.iter().rev().take(depth).map(view).collect(),
            asks: self.asks.iter().take(depth).map(view).collect(),
            last_trade_price: self.last_trade_price,
            seq: self.next_seq,
        }
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next_back().copied()
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn seq(&self) -> Seq {
        self.next_seq
    }

    /// Sum of the remaining quantity of every live order. Equals the displayed depth.
    pub fn open_quantity(&self) -> Qty {
        self.orders
            .values()
            .filter(|o| o.status.is_live())
            .map(|o| o.remaining)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(account: &str, cid: &str, side: Side, price: Price, qty: Qty, tif: Tif) -> PlaceRequest {
        PlaceRequest {
            account: account.into(),
            client_order_id: cid.into(),
            side,
            price,
            qty,
            tif,
        }
    }

    fn gtc(account: &str, cid: &str, side: Side, price: Price, qty: Qty) -> PlaceRequest {
        req(account, cid, side, price, qty, Tif::Gtc)
    }

    #[test]
    fn worked_example_trades_at_maker_price_best_first() {
        let mut b = Book::new();
        let (a1, _) = b.place(gtc("m", "a1", Side::Sell, 300_100, 5_000), 0).unwrap(); // 3001.00 x 0.5
        let (a2, _) = b.place(gtc("m", "a2", Side::Sell, 300_200, 10_000), 0).unwrap(); // 3002.00 x 1.0
        b.place(gtc("m", "b1", Side::Buy, 299_900, 8_000), 0).unwrap(); // 2999.00 x 0.8
        let (t, fills) = b.place(gtc("t", "t1", Side::Buy, 300_200, 12_000), 0).unwrap(); // buy 1.2 @ 3002.00
        assert_eq!(fills.len(), 2);
        assert_eq!((fills[0].maker, fills[0].price, fills[0].qty), (a1.id, 300_100, 5_000));
        assert_eq!((fills[1].maker, fills[1].price, fills[1].qty), (a2.id, 300_200, 7_000));
        assert_eq!((t.status, t.remaining), (Status::Filled, 0));
        let s = b.snapshot(5);
        assert_eq!((s.asks[0].price, s.asks[0].qty, s.asks[0].orders), (300_200, 3_000, 1));
        assert_eq!((s.bids[0].price, s.bids[0].qty), (299_900, 8_000));
        assert_eq!(s.last_trade_price, Some(300_200));
        assert_eq!(b.order(a1.id).unwrap().status, Status::Filled);
        assert_eq!(b.order(a2.id).unwrap().status, Status::PartiallyFilled);
    }

    #[test]
    fn fifo_within_a_price_level() {
        let mut b = Book::new();
        let (first, _) = b.place(gtc("m1", "x", Side::Sell, 300_000, 100), 0).unwrap();
        let (second, _) = b.place(gtc("m2", "y", Side::Sell, 300_000, 100), 0).unwrap();
        let (_, fills) = b.place(gtc("t", "z", Side::Buy, 300_000, 150), 0).unwrap();
        assert_eq!(
            fills.iter().map(|f| (f.maker, f.qty)).collect::<Vec<_>>(),
            vec![(first.id, 100), (second.id, 50)]
        );
    }

    #[test]
    fn partial_fill_rests_the_remainder() {
        let mut b = Book::new();
        b.place(gtc("m", "a", Side::Sell, 300_000, 100), 0).unwrap();
        let (o, fills) = b.place(gtc("t", "b", Side::Buy, 300_500, 300), 0).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!((o.status, o.remaining), (Status::PartiallyFilled, 200));
        let s = b.snapshot(1);
        assert_eq!((s.bids[0].price, s.bids[0].qty), (300_500, 200)); // rests at its own limit
        assert!(s.asks.is_empty());
    }

    #[test]
    fn cancel_removes_depth_and_is_final() {
        let mut b = Book::new();
        let (o, _) = b.place(gtc("a", "c1", Side::Buy, 300_000, 1_000), 0).unwrap();
        assert_eq!(b.snapshot(1).bids[0].qty, 1_000);
        let c = b.cancel("a", o.id).unwrap();
        assert_eq!(c.status, Status::Cancelled);
        assert!(b.snapshot(1).bids.is_empty());
        assert_eq!(
            b.cancel("a", o.id),
            Err(EngineError::Precondition(o.id, Status::Cancelled))
        );
        assert_eq!(b.cancel("a", 999), Err(EngineError::NotFound(999)));
        let (o2, _) = b.place(gtc("a", "c2", Side::Buy, 300_000, 1_000), 0).unwrap();
        assert_eq!(b.cancel("someone-else", o2.id), Err(EngineError::Forbidden(o2.id)));
    }

    #[test]
    fn cancelled_order_is_skipped_by_the_matcher() {
        let mut b = Book::new();
        let (stale, _) = b.place(gtc("m1", "a", Side::Sell, 300_000, 100), 0).unwrap();
        let (fresh, _) = b.place(gtc("m2", "b", Side::Sell, 300_000, 100), 0).unwrap();
        b.cancel("m1", stale.id).unwrap();
        let (_, fills) = b.place(gtc("t", "c", Side::Buy, 300_000, 100), 0).unwrap();
        assert_eq!(fills.iter().map(|f| f.maker).collect::<Vec<_>>(), vec![fresh.id]);
        assert!(b.snapshot(1).asks.is_empty());
    }

    #[test]
    fn duplicate_client_id_replays_the_original_reply() {
        let mut b = Book::new();
        b.place(gtc("m", "a", Side::Sell, 300_000, 100), 0).unwrap();
        let (o1, f1) = b.place(gtc("a", "k", Side::Buy, 300_000, 100), 0).unwrap();
        let (o2, f2) = b.place(gtc("a", "k", Side::Buy, 300_000, 100), 0).unwrap();
        assert_eq!((o1.id, f1.len()), (o2.id, f2.len()));
        assert_eq!(f1, f2);
        assert_eq!(
            b.place(gtc("a", "k", Side::Buy, 300_000, 200), 0),
            Err(EngineError::AlreadyExists)
        );
        assert_eq!(b.events().len(), 3); // accept, trade, accept: no second placement
    }

    #[test]
    fn ioc_never_rests_and_fok_rejects_without_touching_the_book() {
        let mut b = Book::new();
        b.place(gtc("m", "a", Side::Sell, 300_000, 100), 0).unwrap();
        let (ioc, fills) = b.place(req("t", "i", Side::Buy, 300_000, 300, Tif::Ioc), 0).unwrap();
        assert_eq!((fills.len(), ioc.status, ioc.remaining), (1, Status::Cancelled, 200));
        assert!(b.snapshot(1).bids.is_empty());
        b.place(gtc("m", "b", Side::Sell, 300_000, 100), 0).unwrap();
        let before = b.snapshot(5);
        let (fok, fills) = b.place(req("t", "f", Side::Buy, 300_000, 300, Tif::Fok), 0).unwrap();
        assert_eq!((fills.len(), fok.status), (0, Status::Rejected));
        assert_eq!(b.snapshot(5).asks, before.asks);
        let (fok_ok, fills) = b.place(req("t", "g", Side::Buy, 300_000, 100, Tif::Fok), 0).unwrap();
        assert_eq!((fills.len(), fok_ok.status), (1, Status::Filled));
    }

    #[test]
    fn self_trade_prevention_cancels_the_newest_order() {
        let mut b = Book::new();
        b.place(gtc("other", "o", Side::Sell, 300_000, 50), 0).unwrap();
        b.place(gtc("me", "resting", Side::Sell, 300_100, 100), 0).unwrap();
        let (o, fills) = b.place(gtc("me", "taker", Side::Buy, 300_200, 200), 0).unwrap();
        assert_eq!(fills.len(), 1); // takes the other account's order first
        assert_eq!((o.status, o.remaining), (Status::Cancelled, 150)); // stops at its own order
        assert_eq!(b.snapshot(1).asks[0].qty, 100); // own resting order untouched
        assert!(b.snapshot(1).bids.is_empty()); // remainder did not rest
        assert!(matches!(
            b.events().last(),
            Some(Event::Cancelled {
                reason: CancelReason::SelfTradePrevention,
                ..
            })
        ));
    }

    #[test]
    fn invalid_arguments_are_rejected() {
        let mut b = Book::new();
        assert!(matches!(
            b.place(gtc("a", "x", Side::Buy, 0, 1), 0),
            Err(EngineError::Invalid(_))
        ));
        assert!(matches!(
            b.place(gtc("a", "x", Side::Buy, 1, 0), 0),
            Err(EngineError::Invalid(_))
        ));
        assert!(matches!(
            b.place(gtc("", "x", Side::Buy, 1, 1), 0),
            Err(EngineError::Invalid(_))
        ));
        assert!(matches!(
            b.place(gtc("a", "", Side::Buy, 1, 1), 0),
            Err(EngineError::Invalid(_))
        ));
        let long = "x".repeat(MAX_ID_LEN + 1);
        assert!(matches!(
            b.place(gtc("a", &long, Side::Buy, 1, 1), 0),
            Err(EngineError::Invalid(_))
        ));
        assert!(matches!(
            b.place(gtc(&long, "k", Side::Buy, 1, 1), 0),
            Err(EngineError::Invalid(_))
        ));
        assert!(b.place(gtc("a", &"y".repeat(MAX_ID_LEN), Side::Buy, 1, 1), 0).is_ok());
    }

    #[test]
    fn listings_are_newest_first_and_scoped_to_the_account() {
        let mut b = Book::new();
        b.place(gtc("a", "1", Side::Buy, 299_000, 10), 0).unwrap();
        b.place(gtc("b", "1", Side::Sell, 301_000, 10), 0).unwrap();
        b.place(gtc("a", "2", Side::Buy, 299_500, 10), 0).unwrap();
        let (_, fills) = b.place(gtc("a", "3", Side::Buy, 301_000, 10), 0).unwrap();
        assert_eq!(fills.len(), 1);
        let a = b.orders_for("a", |_| true, 10);
        assert_eq!(
            a.iter().map(|o| &*o.client_order_id).collect::<Vec<_>>(),
            vec!["3", "2", "1"]
        );
        assert_eq!(b.orders_for("a", |_| true, 2).len(), 2);
        assert_eq!(b.orders_for("a", |o| o.status.is_live(), 10).len(), 2);
        assert_eq!(b.open_orders_for("a").len(), 2);
        assert_eq!(b.trades(Some("a"), 10).len(), 1);
        assert_eq!(b.trades(Some("b"), 10).len(), 1);
        assert_eq!(b.trades(Some("nobody"), 10).len(), 0);
        assert_eq!(b.trades(None, 10).len(), 1);
        let t = &b.trades(None, 10)[0];
        assert_eq!(
            (t.taker_side, &*t.taker_account, &*t.maker_account),
            (Side::Buy, "a", "b")
        );
        assert_eq!(
            (t.side_for("a"), t.side_for("b"), t.side_for("x")),
            (Some(Side::Buy), Some(Side::Sell), None)
        );
    }

    #[test]
    fn listings_follow_the_same_order_as_the_event_log() {
        // Many orders across two accounts: the indexed listing must equal a full scan of the log.
        let mut b = Book::new();
        for i in 0..200u64 {
            let account = if i % 3 == 0 { "x" } else { "y" };
            let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
            let _ = b.place(
                gtc(account, &i.to_string(), side, 299_900 + (i % 7) * 50, 10 + i % 5),
                0,
            );
            if i % 5 == 4 {
                let _ = b.cancel("y", i.max(1) - 1);
            }
        }
        for account in ["x", "y"] {
            let mut from_log: Vec<OrderId> = b
                .events()
                .iter()
                .filter_map(|e| match e {
                    Event::Accepted(o) if &*o.account == account => Some(o.id),
                    Event::Rejected { id, .. } if &*b.order(*id).unwrap().account == account => Some(*id),
                    _ => None,
                })
                .collect();
            from_log.reverse();
            let listed: Vec<OrderId> = b
                .orders_for(account, |_| true, usize::MAX)
                .iter()
                .map(|o| o.id)
                .collect();
            assert_eq!(listed, from_log);
            let trades_from_log: Vec<TradeId> = b
                .events()
                .iter()
                .rev()
                .filter_map(|e| match e {
                    Event::Traded(t) if t.side_for(account).is_some() => Some(t.id),
                    _ => None,
                })
                .collect();
            let listed: Vec<TradeId> = b.trades(Some(account), usize::MAX).iter().map(|t| t.id).collect();
            assert_eq!(listed, trades_from_log);
        }
    }

    proptest::proptest! {
        #[test]
        fn invariants_hold_and_replay_is_identical(
            ops in proptest::collection::vec((0u8..2, 1u64..40, 1u64..500, 0u8..3), 1..300)
        ) {
            let run = |ops: &[(u8, u64, u64, u8)]| {
                let mut b = Book::new();
                for (i, (side, price, qty, tif)) in ops.iter().enumerate() {
                    let side = if *side == 0 { Side::Buy } else { Side::Sell };
                    let tif = match tif { 0 => Tif::Gtc, 1 => Tif::Ioc, _ => Tif::Fok };
                    // Two accounts per side so self-trade prevention is exercised without dominating.
                    let account = format!("{}{}", if side == Side::Buy { "b" } else { "s" }, i % 2);
                    let _ = b.place(req(&account, &i.to_string(), side, 299_000 + price * 10, *qty, tif), 0);
                    if i % 7 == 3 {
                        let victim = (i as u64 / 2).max(1);
                        if let Some(o) = b.order(victim).cloned() { let _ = b.cancel(&o.account, victim); }
                    }
                    if let (Some(bb), Some(ba)) = (b.best_bid(), b.best_ask()) {
                        proptest::prop_assert!(bb < ba, "crossed book: bid {bb} >= ask {ba}");
                    }
                    let s = b.snapshot(1_000);
                    let shown: u64 = s.bids.iter().chain(s.asks.iter()).map(|l| l.qty).sum();
                    proptest::prop_assert_eq!(b.open_quantity(), shown, "displayed depth != open quantity");
                    for e in b.events() {
                        if let Event::Traded(t) = e {
                            let (maker, taker) = (b.order(t.maker).unwrap(), b.order(t.taker).unwrap());
                            proptest::prop_assert_eq!(t.price, maker.price, "trade not at maker price");
                            let ok = match taker.side { Side::Buy => t.price <= taker.price, Side::Sell => t.price >= taker.price };
                            proptest::prop_assert!(ok, "trade outside taker limit");
                            proptest::prop_assert!(maker.account != taker.account, "self trade");
                        }
                    }
                }
                let seqs: Vec<u64> = b.events().iter().map(|e| match e {
                    Event::Accepted(o) => o.seq,
                    Event::Traded(t) => t.seq,
                    Event::Cancelled { seq, .. } | Event::Rejected { seq, .. } => *seq,
                }).collect();
                let mut sorted = seqs.clone();
                sorted.sort_unstable();
                sorted.dedup();
                proptest::prop_assert_eq!(sorted.len(), seqs.len(), "duplicate sequence numbers");
                Ok(b.events().iter().map(|e| format!("{e:?}")).collect::<Vec<_>>())
            };
            proptest::prop_assert_eq!(run(&ops)?, run(&ops)?, "replay produced a different event log");
        }
    }
}
