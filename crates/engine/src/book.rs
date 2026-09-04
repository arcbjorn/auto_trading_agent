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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelReason {
    User,
    Ioc,
    Fok,
    SelfTradePrevention,
}

/// Orders are cloned on every reply and event, so the two strings are shared, not copied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub account: Arc<str>,
    pub client_order_id: Arc<str>,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub remaining: Qty,
    pub status: Status,
    /// Why a cancelled order was cancelled: the user, IOC or FOK time in force, or self-trade
    /// prevention. Callers need this to explain a cancelled order rather than guess.
    pub cancel_reason: Option<CancelReason>,
    pub seq: Seq,
    /// Wall clock at acceptance, for reporting only. Never used for ordering.
    pub created_at_unix_ns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// What an account holds: USDC in micro-USDC (one tick times one lot), ETH in lots. Reserved
/// amounts back the account's live orders and return to available when they fill or cancel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balances {
    pub usdc_available: u128,
    pub usdc_reserved: u128,
    pub eth_available: Qty,
    pub eth_reserved: Qty,
}

/// Per-account trading ledger: what went in and out, what was bought and sold, and the average
/// cost of the ETH still held from purchases on this venue, so realised P&L is a number rather
/// than a guess. ETH that arrived by deposit has no cost basis here: a sell beyond the venue
/// inventory is counted in `sold_from_deposits_lots` and contributes no P&L.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub deposits_usdc: u128,
    pub deposits_eth: Qty,
    pub withdrawals_usdc: u128,
    pub withdrawals_eth: Qty,
    pub bought_lots: Qty,
    pub sold_lots: Qty,
    pub usdc_paid: u128,
    pub usdc_received: u128,
    /// ETH bought here and not yet sold, and what it cost in total (average cost is the ratio).
    pub inventory_lots: Qty,
    pub inventory_cost: u128,
    pub sold_from_deposits_lots: Qty,
    /// Sum over sells of (price minus average cost) times quantity, for the inventory part.
    pub realised_pnl: i128,
    pub trades: u64,
}

/// Books one fill into both ledgers. The seller's inventory is relieved at average cost.
fn record_fill(ledgers: &mut HashMap<Arc<str>, Ledger>, buyer: &Arc<str>, seller: &Arc<str>, px: Price, q: Qty) {
    let paid = px as u128 * q as u128;
    let b = ledgers.entry(Arc::clone(buyer)).or_default();
    b.bought_lots += q;
    b.usdc_paid += paid;
    b.inventory_lots += q;
    b.inventory_cost += paid;
    b.trades += 1;
    let s = ledgers.entry(Arc::clone(seller)).or_default();
    s.sold_lots += q;
    s.usdc_received += paid;
    s.trades += 1;
    let from_inventory = q.min(s.inventory_lots);
    if from_inventory > 0 {
        let cost_removed = s.inventory_cost * from_inventory as u128 / s.inventory_lots as u128;
        s.realised_pnl += (px as u128 * from_inventory as u128) as i128 - cost_removed as i128;
        s.inventory_cost -= cost_removed;
        s.inventory_lots -= from_inventory;
    }
    s.sold_from_deposits_lots += q - from_inventory;
}

/// Append-only history. Order and trade listings are derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Accepted(Order),
    Traded(Trade),
    Deposited {
        account: Arc<str>,
        usdc: u128,
        eth: Qty,
        seq: Seq,
    },
    Withdrawn {
        account: Arc<str>,
        usdc: u128,
        eth: Qty,
        seq: Seq,
    },
    Cancelled {
        id: OrderId,
        account: Arc<str>,
        reason: CancelReason,
        seq: Seq,
    },
    Rejected {
        id: OrderId,
        account: Arc<str>,
        reason: String,
        seq: Seq,
    },
}

impl Event {
    pub fn seq(&self) -> Seq {
        match self {
            Event::Accepted(o) => o.seq,
            Event::Traded(t) => t.seq,
            Event::Deposited { seq, .. }
            | Event::Withdrawn { seq, .. }
            | Event::Cancelled { seq, .. }
            | Event::Rejected { seq, .. } => *seq,
        }
    }

    /// Whether `account` is a party to this event.
    pub fn involves(&self, account: &str) -> bool {
        match self {
            Event::Accepted(o) => &*o.account == account,
            Event::Traded(t) => t.side_for(account).is_some(),
            Event::Deposited { account: a, .. }
            | Event::Withdrawn { account: a, .. }
            | Event::Cancelled { account: a, .. }
            | Event::Rejected { account: a, .. } => &**a == account,
        }
    }
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
    /// Raw units: micro-USDC or lots; the gRPC layer renders them for people.
    #[error("insufficient {asset}: needs {needed}, available {available}")]
    InsufficientFunds {
        asset: &'static str,
        needed: u128,
        available: u128,
    },
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
    /// When set, every order must be backed by the account's balance (see [`Book::with_balances`]).
    enforce_balances: bool,
    balances: HashMap<Arc<str>, Balances>,
    ledgers: HashMap<Arc<str>, Ledger>,
}

/// Moves `needed` from available to reserved for a new order (the funds check happened first).
fn reserve(balances: &mut HashMap<Arc<str>, Balances>, account: &Arc<str>, side: Side, price: Price, qty: Qty) {
    let b = balances.entry(Arc::clone(account)).or_default();
    match side {
        Side::Buy => {
            let usdc = price as u128 * qty as u128;
            b.usdc_available -= usdc;
            b.usdc_reserved += usdc;
        }
        Side::Sell => {
            b.eth_available -= qty;
            b.eth_reserved += qty;
        }
    }
}

/// Gives a cancelled or unfilled remainder back to the account.
fn release(balances: &mut HashMap<Arc<str>, Balances>, account: &Arc<str>, side: Side, price: Price, remaining: Qty) {
    let b = balances.entry(Arc::clone(account)).or_default();
    match side {
        Side::Buy => {
            let usdc = price as u128 * remaining as u128;
            b.usdc_reserved -= usdc;
            b.usdc_available += usdc;
        }
        Side::Sell => {
            b.eth_reserved -= remaining;
            b.eth_available += remaining;
        }
    }
}

/// Settles one trade of `q` lots at `px`. The taker reserved at its own limit, so a buy that fills
/// below the limit gets the difference back; the maker's reservation was at `px` already.
#[allow(clippy::too_many_arguments)]
fn settle(
    balances: &mut HashMap<Arc<str>, Balances>,
    taker: &Arc<str>,
    taker_side: Side,
    taker_limit: Price,
    maker: &Arc<str>,
    px: Price,
    q: Qty,
) {
    let paid = px as u128 * q as u128;
    match taker_side {
        Side::Buy => {
            let t = balances.entry(Arc::clone(taker)).or_default();
            let reserved = taker_limit as u128 * q as u128;
            t.usdc_reserved -= reserved;
            t.usdc_available += reserved - paid;
            t.eth_available += q;
            let m = balances.entry(Arc::clone(maker)).or_default();
            m.eth_reserved -= q;
            m.usdc_available += paid;
        }
        Side::Sell => {
            let t = balances.entry(Arc::clone(taker)).or_default();
            t.eth_reserved -= q;
            t.usdc_available += paid;
            let m = balances.entry(Arc::clone(maker)).or_default();
            m.usdc_reserved -= paid;
            m.eth_available += q;
        }
    }
}

/// Everything a book needs to continue exactly where it was: the durable form of the state.
/// Indices and price levels are derived from the orders on load, and the in-memory event log
/// starts empty (the journal, not the event log, is the history).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookState {
    pub orders: Vec<Order>,
    pub trades: Vec<Trade>,
    /// Original replies for idempotent retries: the order as first returned and its fill ids.
    pub original_replies: Vec<(OrderId, Order, Vec<TradeId>)>,
    pub balances: Vec<(String, Balances)>,
    #[serde(default)]
    pub ledgers: Vec<(String, Ledger)>,
    pub last_trade_price: Option<Price>,
    pub next_order: OrderId,
    pub next_trade: TradeId,
    pub next_seq: Seq,
    pub enforce_balances: bool,
}

impl Book {
    /// A book that does not check funds: every account may place any order.
    pub fn new() -> Self {
        Self::default()
    }

    /// The durable state of the book, in id order so the output is deterministic.
    pub fn state(&self) -> BookState {
        let mut orders: Vec<Order> = self.orders.values().cloned().collect();
        orders.sort_by_key(|o| o.id);
        let mut original_replies: Vec<(OrderId, Order, Vec<TradeId>)> = self
            .original_replies
            .iter()
            .map(|(id, (o, fills))| (*id, o.clone(), fills.iter().map(|t| t.id).collect()))
            .collect();
        original_replies.sort_by_key(|(id, ..)| *id);
        let mut balances: Vec<(String, Balances)> = self.balances.iter().map(|(a, b)| (a.to_string(), *b)).collect();
        balances.sort_by(|a, b| a.0.cmp(&b.0));
        let mut ledgers: Vec<(String, Ledger)> = self.ledgers.iter().map(|(a, l)| (a.to_string(), *l)).collect();
        ledgers.sort_by(|a, b| a.0.cmp(&b.0));
        BookState {
            orders,
            trades: self.trades.clone(),
            original_replies,
            balances,
            ledgers,
            last_trade_price: self.last_trade_price,
            next_order: self.next_order,
            next_trade: self.next_trade,
            next_seq: self.next_seq,
            enforce_balances: self.enforce_balances,
        }
    }

    /// Rebuilds a book from [`Book::state`]: the same orders, trades, balances and counters, with
    /// the price levels and indices derived again. Time priority within a level is id order,
    /// which is arrival order.
    pub fn from_state(state: BookState) -> Self {
        let mut book = Book {
            enforce_balances: state.enforce_balances,
            last_trade_price: state.last_trade_price,
            next_order: state.next_order,
            next_trade: state.next_trade,
            next_seq: state.next_seq,
            ..Book::default()
        };
        for (account, b) in state.balances {
            book.balances.insert(Arc::from(account.as_str()), b);
        }
        for (account, l) in state.ledgers {
            let key = book.intern(&account);
            book.ledgers.insert(key, l);
        }
        let mut live: Vec<(Side, Price, OrderId, Qty)> = Vec::new();
        for mut o in state.orders {
            o.account = book.intern(&o.account);
            book.by_account.entry(Arc::clone(&o.account)).or_default().push(o.id);
            book.by_client_id
                .entry(Arc::clone(&o.account))
                .or_default()
                .insert(Arc::clone(&o.client_order_id), o.id);
            if o.status.is_live() {
                live.push((o.side, o.price, o.id, o.remaining));
            }
            book.orders.insert(o.id, o);
        }
        live.sort_by_key(|(_, _, id, _)| *id);
        for (side, price, id, remaining) in live {
            let level = match side {
                Side::Buy => book.bids.entry(price).or_default(),
                Side::Sell => book.asks.entry(price).or_default(),
            };
            level.total += remaining;
            level.live += 1;
            level.queue.push_back(id);
        }
        for (pos, mut t) in state.trades.into_iter().enumerate() {
            t.maker_account = book.intern(&t.maker_account);
            t.taker_account = book.intern(&t.taker_account);
            book.trades_by_account
                .entry(Arc::clone(&t.maker_account))
                .or_default()
                .push(pos);
            book.trades_by_account
                .entry(Arc::clone(&t.taker_account))
                .or_default()
                .push(pos);
            book.trades.push(t);
        }
        let by_trade_id: HashMap<TradeId, usize> = book.trades.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
        for (id, order, fill_ids) in state.original_replies {
            let fills = fill_ids
                .iter()
                .filter_map(|t| by_trade_id.get(t).map(|&i| book.trades[i].clone()))
                .collect();
            book.original_replies.insert(id, (order, fills));
        }
        book
    }

    /// A book where every order must be backed: a buy reserves price times quantity in USDC, a
    /// sell reserves the quantity in ETH, fills settle both legs, cancels release the rest.
    pub fn with_balances() -> Self {
        Self {
            enforce_balances: true,
            ..Self::default()
        }
    }

    pub fn enforces_balances(&self) -> bool {
        self.enforce_balances
    }

    fn intern(&self, account: &str) -> Arc<str> {
        if let Some((k, _)) = self.balances.get_key_value(account) {
            return Arc::clone(k);
        }
        match self.by_account.get_key_value(account) {
            Some((k, _)) => Arc::clone(k),
            None => Arc::from(account),
        }
    }

    /// Credits an account. Recorded as an event so a replay restores balances too.
    pub fn deposit(&mut self, account: &str, usdc: u128, eth: Qty) -> Result<Balances, EngineError> {
        if account.is_empty() || account.len() > MAX_ID_LEN {
            return Err(EngineError::Invalid(format!(
                "account_id must be 1 to {MAX_ID_LEN} bytes"
            )));
        }
        let key = self.intern(account);
        let b = self.balances.entry(Arc::clone(&key)).or_default();
        b.usdc_available = b
            .usdc_available
            .checked_add(usdc)
            .ok_or_else(|| EngineError::Invalid("deposit overflows the balance".into()))?;
        b.eth_available = b
            .eth_available
            .checked_add(eth)
            .ok_or_else(|| EngineError::Invalid("deposit overflows the balance".into()))?;
        let result = *b;
        let ledger = self.ledgers.entry(Arc::clone(&key)).or_default();
        ledger.deposits_usdc += usdc;
        ledger.deposits_eth += eth;
        let seq = self.bump_seq();
        self.events.push(Event::Deposited {
            account: key,
            usdc,
            eth,
            seq,
        });
        Ok(result)
    }

    /// Debits an account from what is available; reserved amounts back live orders and cannot be
    /// withdrawn. Recorded as an event so a replay restores balances too.
    pub fn withdraw(&mut self, account: &str, usdc: u128, eth: Qty) -> Result<Balances, EngineError> {
        if account.is_empty() || account.len() > MAX_ID_LEN {
            return Err(EngineError::Invalid(format!(
                "account_id must be 1 to {MAX_ID_LEN} bytes"
            )));
        }
        let current = self.balances(account);
        if current.usdc_available < usdc {
            return Err(EngineError::InsufficientFunds {
                asset: "USDC",
                needed: usdc,
                available: current.usdc_available,
            });
        }
        if current.eth_available < eth {
            return Err(EngineError::InsufficientFunds {
                asset: "ETH",
                needed: eth as u128,
                available: current.eth_available as u128,
            });
        }
        let key = self.intern(account);
        let b = self.balances.entry(Arc::clone(&key)).or_default();
        b.usdc_available -= usdc;
        b.eth_available -= eth;
        let result = *b;
        let ledger = self.ledgers.entry(Arc::clone(&key)).or_default();
        ledger.withdrawals_usdc += usdc;
        ledger.withdrawals_eth += eth;
        let seq = self.bump_seq();
        self.events.push(Event::Withdrawn {
            account: key,
            usdc,
            eth,
            seq,
        });
        Ok(result)
    }

    /// The account's trading ledger; all zeros for an account that never traded or deposited.
    pub fn statement(&self, account: &str) -> Ledger {
        self.ledgers.get(account).copied().unwrap_or_default()
    }

    /// The account's balances; zero for an account that was never funded.
    pub fn balances(&self, account: &str) -> Balances {
        self.balances.get(account).copied().unwrap_or_default()
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

        if self.enforce_balances {
            let bal = self.balances(&req.account);
            match req.side {
                Side::Buy => {
                    let needed = req.price as u128 * req.qty as u128;
                    if bal.usdc_available < needed {
                        return Err(EngineError::InsufficientFunds {
                            asset: "USDC",
                            needed,
                            available: bal.usdc_available,
                        });
                    }
                }
                Side::Sell => {
                    if bal.eth_available < req.qty {
                        return Err(EngineError::InsufficientFunds {
                            asset: "ETH",
                            needed: req.qty as u128,
                            available: bal.eth_available as u128,
                        });
                    }
                }
            }
        }
        // Reuse the account's interned name so every order of an account shares one allocation.
        let account: Arc<str> = self.intern(&req.account);
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
            cancel_reason: None,
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
                account: Arc::clone(&o.account),
                reason: "FOK: insufficient quantity available at this price".into(),
                seq: o.seq,
            });
            self.orders.insert(o.id, o.clone());
            self.original_replies.insert(o.id, (o.clone(), Vec::new()));
            return Ok((o, Vec::new()));
        }
        self.events.push(Event::Accepted(o.clone()));
        if self.enforce_balances {
            reserve(&mut self.balances, &o.account, o.side, o.price, o.qty);
        }

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
            if self.enforce_balances {
                settle(&mut self.balances, &o.account, o.side, o.price, &maker_account, px, q);
            }
            match o.side {
                Side::Buy => record_fill(&mut self.ledgers, &o.account, &maker_account, px, q),
                Side::Sell => record_fill(&mut self.ledgers, &maker_account, &o.account, px, q),
            }
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
        o.cancel_reason = cancel;
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
            if self.enforce_balances {
                release(&mut self.balances, &o.account, o.side, o.price, o.remaining);
            }
            let seq = self.bump_seq();
            self.events.push(Event::Cancelled {
                id: o.id,
                account: Arc::clone(&o.account),
                reason,
                seq,
            });
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
        o.cancel_reason = Some(CancelReason::User);
        let (side, price, remaining) = (o.side, o.price, o.remaining);
        let owner = Arc::clone(&o.account);
        if self.enforce_balances {
            release(&mut self.balances, &owner, side, price, remaining);
        }
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
            account: owner,
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
    fn balances_reserve_settle_and_release() {
        let mut b = Book::with_balances();
        assert!(matches!(
            b.place(gtc("t", "x", Side::Buy, 300_000, 1_000), 0),
            Err(EngineError::InsufficientFunds { asset: "USDC", .. })
        ));
        b.deposit("m", 0, 20_000).unwrap(); // 2 ETH
        b.deposit("t", 4_000_000_000, 0).unwrap(); // 4,000 USDC
        assert_eq!(b.balances("nobody"), Balances::default());
        let (a1, _) = b.place(gtc("m", "a1", Side::Sell, 300_100, 5_000), 0).unwrap(); // 0.5 @ 3001
        assert_eq!(
            b.balances("m"),
            Balances {
                usdc_available: 0,
                usdc_reserved: 0,
                eth_available: 15_000,
                eth_reserved: 5_000
            }
        );
        assert!(matches!(
            b.place(gtc("m", "a2", Side::Sell, 300_200, 20_000), 0),
            Err(EngineError::InsufficientFunds {
                asset: "ETH",
                needed: 20_000,
                available: 15_000
            })
        ));
        // Buy 1.2 @ 3002: reserves 3602.40, fills 0.5 at 3001 (refund 0.50), rests 0.7 at 3002.
        let (t, fills) = b.place(gtc("t", "t1", Side::Buy, 300_200, 12_000), 0).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!((t.status, t.remaining), (Status::PartiallyFilled, 7_000));
        let tb = b.balances("t");
        assert_eq!(tb.eth_available, 5_000);
        assert_eq!(tb.usdc_reserved, 300_200 * 7_000); // the resting remainder at its limit
        assert_eq!(
            tb.usdc_available,
            4_000_000_000 - 300_200 * 12_000 + (300_200 - 300_100) * 5_000
        );
        let mb = b.balances("m");
        assert_eq!(
            (mb.eth_available, mb.eth_reserved, mb.usdc_available),
            (15_000, 0, 300_100 * 5_000)
        );
        assert_eq!(b.order(a1.id).unwrap().status, Status::Filled);
        // Cancel the remainder: the reservation comes back.
        b.cancel("t", t.id).unwrap();
        let tb = b.balances("t");
        assert_eq!(
            (tb.usdc_reserved, tb.usdc_available),
            (0, 4_000_000_000 - 300_100 * 5_000)
        );
        // An IOC remainder is released too, and a FOK that cannot fill reserves nothing.
        b.place(gtc("m", "a3", Side::Sell, 300_500, 1_000), 0).unwrap();
        b.place(req("t", "i", Side::Buy, 300_500, 3_000, Tif::Ioc), 0).unwrap();
        assert_eq!(b.balances("t").usdc_reserved, 0);
        b.place(req("t", "f", Side::Buy, 300_500, 3_000, Tif::Fok), 0).unwrap();
        assert_eq!(b.balances("t").usdc_reserved, 0);
        assert!(matches!(b.events().first(), Some(Event::Deposited { .. })));
    }

    #[test]
    fn state_round_trips_and_the_rebuilt_book_behaves_identically() {
        let mut a = Book::with_balances();
        a.deposit("m", 0, 100_000).unwrap();
        a.deposit("t", 10_000_000_000, 0).unwrap();
        a.place(gtc("m", "a1", Side::Sell, 300_100, 5_000), 1).unwrap();
        a.place(gtc("m", "a2", Side::Sell, 300_100, 3_000), 2).unwrap(); // same level, behind a1
        a.place(gtc("m", "a3", Side::Sell, 300_300, 1_000), 3).unwrap();
        let (t1, _) = a.place(gtc("t", "t1", Side::Buy, 300_100, 6_000), 4).unwrap(); // fills a1, part of a2
        a.place(gtc("t", "t2", Side::Buy, 299_000, 2_000), 5).unwrap();
        let (c, _) = a.place(gtc("t", "t3", Side::Buy, 298_000, 100), 6).unwrap();
        a.cancel("t", c.id).unwrap();
        let state = a.state();
        let json = serde_json::to_string(&state).unwrap();
        let mut b = Book::from_state(serde_json::from_str(&json).unwrap());
        assert_eq!(b.snapshot(100), a.snapshot(100));
        assert_eq!(b.seq(), a.seq());
        assert_eq!(b.balances("m"), a.balances("m"));
        assert_eq!(b.balances("t"), a.balances("t"));
        assert_eq!(b.orders_for("t", |_| true, 100), a.orders_for("t", |_| true, 100));
        assert_eq!(b.trades(Some("m"), 100), a.trades(Some("m"), 100));
        assert_eq!(b.open_quantity(), a.open_quantity());
        // Idempotent retries replay the original reply after a rebuild too.
        let replay = b.place(gtc("t", "t1", Side::Buy, 300_100, 6_000), 99).unwrap();
        assert_eq!(replay.0.id, t1.id);
        assert_eq!(replay.1.len(), 2);
        // From here on both books produce identical events: time priority within a level survived.
        let next = gtc("t", "t4", Side::Buy, 300_300, 4_000);
        let (oa, fa) = a.place(next.clone(), 7).unwrap();
        let (ob, fb) = b.place(next, 7).unwrap();
        assert_eq!((&oa, &fa), (&ob, &fb));
        assert_eq!(fb.iter().map(|f| f.maker).collect::<Vec<_>>(), vec![2, 3]); // a2's remainder first
        assert_eq!(b.snapshot(100), a.snapshot(100));
        assert_eq!(b.balances("t"), a.balances("t"));
    }

    #[test]
    fn ledger_tracks_average_cost_realised_pnl_and_withdrawals() {
        let mut b = Book::with_balances();
        b.deposit("m", 10_000_000_000, 100_000).unwrap();
        b.deposit("t", 10_000_000_000, 0).unwrap();
        b.place(gtc("m", "a1", Side::Sell, 300_100, 5_000), 0).unwrap();
        b.place(gtc("m", "b1", Side::Buy, 299_900, 8_000), 0).unwrap();
        // t buys 0.5 at 3001.00, then sells 0.3 at 2999.00: realised (2999 - 3001) * 0.3 = -0.60 USDC.
        b.place(gtc("t", "t1", Side::Buy, 300_100, 5_000), 0).unwrap();
        let after_buy = b.statement("t");
        assert_eq!(
            (
                after_buy.bought_lots,
                after_buy.inventory_lots,
                after_buy.inventory_cost
            ),
            (5_000, 5_000, 300_100 * 5_000)
        );
        b.place(gtc("t", "t2", Side::Sell, 299_900, 3_000), 0).unwrap();
        let t = b.statement("t");
        assert_eq!(
            (t.sold_lots, t.inventory_lots, t.sold_from_deposits_lots, t.trades),
            (3_000, 2_000, 0, 2)
        );
        assert_eq!(t.realised_pnl, -600_000); // -0.60 USDC in micro-USDC
        assert_eq!(t.inventory_cost, 300_100 * 2_000);
        // The maker sold ETH it deposited (no cost basis) and then bought: no P&L, inventory 0.3.
        let m = b.statement("m");
        assert_eq!(
            (m.sold_from_deposits_lots, m.inventory_lots, m.realised_pnl),
            (5_000, 3_000, 0)
        );
        assert_eq!(
            (m.deposits_eth, m.deposits_usdc, t.deposits_usdc),
            (100_000, 10_000_000_000, 10_000_000_000)
        );
        // Withdrawals come out of what is available; reserved stays put.
        b.place(gtc("t", "t3", Side::Buy, 290_000, 1_000), 0).unwrap(); // reserves 290 USDC
        let bal = b.balances("t");
        assert!(matches!(
            b.withdraw("t", bal.usdc_available + 1, 0),
            Err(EngineError::InsufficientFunds { asset: "USDC", .. })
        ));
        let left = b.withdraw("t", bal.usdc_available, 2_000).unwrap();
        assert_eq!(
            (left.usdc_available, left.eth_available, left.usdc_reserved),
            (0, 0, 290_000 * 1_000)
        );
        let t = b.statement("t");
        assert_eq!((t.withdrawals_usdc, t.withdrawals_eth), (bal.usdc_available, 2_000));
        assert!(matches!(
            b.withdraw("t", 0, 1),
            Err(EngineError::InsufficientFunds { asset: "ETH", .. })
        ));
        assert!(matches!(b.events().last(), Some(Event::Withdrawn { .. })));
        // The ledger survives a state round trip.
        let rebuilt = Book::from_state(b.state());
        assert_eq!(rebuilt.statement("t"), b.statement("t"));
        assert_eq!(rebuilt.statement("m"), m);
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
        assert_eq!(
            (c.status, c.cancel_reason),
            (Status::Cancelled, Some(CancelReason::User))
        );
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
        assert_eq!(ioc.cancel_reason, Some(CancelReason::Ioc));
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
        assert_eq!(o.cancel_reason, Some(CancelReason::SelfTradePrevention));
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
                let seqs: Vec<u64> = b.events().iter().map(Event::seq).collect();
                let mut sorted = seqs.clone();
                sorted.sort_unstable();
                sorted.dedup();
                proptest::prop_assert_eq!(sorted.len(), seqs.len(), "duplicate sequence numbers");
                Ok(b.events().iter().map(|e| format!("{e:?}")).collect::<Vec<_>>())
            };
            proptest::prop_assert_eq!(run(&ops)?, run(&ops)?, "replay produced a different event log");
        }

        /// With balances enforced: nothing is created or destroyed, reservations equal the live
        /// orders, and an account can never go negative. One buyer and one seller are funded
        /// tightly so rejections and partial capacity are exercised.
        #[test]
        fn balances_are_conserved_and_back_every_live_order(
            ops in proptest::collection::vec((0u8..2, 1u64..40, 1u64..500, 0u8..3), 1..300)
        ) {
            let mut b = Book::with_balances();
            let funding = [("b0", 1_000_000_000_000u128, 0u64), ("b1", 3_000_000_000u128, 0), ("s0", 0, 1_000_000), ("s1", 0, 700)];
            for (a, usdc, eth) in funding { b.deposit(a, usdc, eth).unwrap(); }
            let total_usdc: u128 = funding.iter().map(|f| f.1).sum();
            let total_eth: u64 = funding.iter().map(|f| f.2).sum();
            let mut accepted = 0;
            let mut refused = 0;
            for (i, (side, price, qty, tif)) in ops.iter().enumerate() {
                let side = if *side == 0 { Side::Buy } else { Side::Sell };
                let tif = match tif { 0 => Tif::Gtc, 1 => Tif::Ioc, _ => Tif::Fok };
                let account = format!("{}{}", if side == Side::Buy { "b" } else { "s" }, i % 2);
                match b.place(req(&account, &i.to_string(), side, 299_000 + price * 10, *qty, tif), 0) {
                    Ok(_) => accepted += 1,
                    Err(EngineError::InsufficientFunds { .. }) => refused += 1,
                    Err(e) => return Err(proptest::test_runner::TestCaseError::fail(format!("{e}"))),
                }
                if i % 5 == 4 {
                    let victim = (i as u64 / 2).max(1);
                    if let Some(o) = b.order(victim).cloned() { let _ = b.cancel(&o.account, victim); }
                }
                let mut usdc = 0u128;
                let mut eth = 0u64;
                for (a, _, _) in funding {
                    let bal = b.balances(a);
                    usdc += bal.usdc_available + bal.usdc_reserved;
                    eth += bal.eth_available + bal.eth_reserved;
                    let live = b.open_orders_for(a);
                    let usdc_backing: u128 = live.iter().filter(|o| o.side == Side::Buy).map(|o| o.price as u128 * o.remaining as u128).sum();
                    let eth_backing: u64 = live.iter().filter(|o| o.side == Side::Sell).map(|o| o.remaining).sum();
                    proptest::prop_assert_eq!(bal.usdc_reserved, usdc_backing, "USDC reserved != live buys of {}", a);
                    proptest::prop_assert_eq!(bal.eth_reserved, eth_backing, "ETH reserved != live sells of {}", a);
                }
                proptest::prop_assert_eq!(usdc, total_usdc, "USDC was created or destroyed");
                proptest::prop_assert_eq!(eth, total_eth, "ETH was created or destroyed");
            }
            proptest::prop_assert!(accepted + refused == ops.len());
            // The ledgers are zero-sum across accounts and each one's inventory adds up.
            let (mut paid, mut received, mut bought, mut sold) = (0u128, 0u128, 0u64, 0u64);
            for (a, _, _) in funding {
                let l = b.statement(a);
                paid += l.usdc_paid;
                received += l.usdc_received;
                bought += l.bought_lots;
                sold += l.sold_lots;
                proptest::prop_assert_eq!(l.inventory_lots, l.bought_lots - (l.sold_lots - l.sold_from_deposits_lots), "inventory of {}", a);
                proptest::prop_assert!(l.inventory_lots > 0 || l.inventory_cost == 0, "cost without inventory for {}", a);
                proptest::prop_assert!(l.sold_from_deposits_lots <= l.deposits_eth, "{} sold ETH it never had", a);
            }
            proptest::prop_assert_eq!(paid, received, "USDC paid != USDC received");
            proptest::prop_assert_eq!(bought, sold, "ETH bought != ETH sold");
        }
    }
}
