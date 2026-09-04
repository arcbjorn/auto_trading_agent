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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Price in ticks. 1 tick = 0.01 USDC per ETH.
pub type Price = u64;
/// Quantity in lots. 1 lot = 0.0001 ETH.
pub type Qty = u64;
pub type OrderId = u64;
pub type TradeId = u64;
/// Position in the engine's total order of events.
pub type Seq = u64;
/// Events kept in memory for inspection (tests, debugging). The journal is the history; the
/// broadcast takes its events from a separate buffer that the sequencer drains every batch.
pub const RECENT_EVENTS: usize = 100_000;

/// Backstops that hold whatever the layers above do: no order may be priced above
/// 1,000,000.00 USDC or be larger than 10,000 ETH.
pub const MAX_PRICE: Price = 100_000_000;
pub const MAX_QTY: Qty = 100_000_000;

/// Closed orders kept in memory (lookups, listings, idempotent retries): when more have closed,
/// the oldest closed order is archived. Live orders are always kept.
pub const RETAINED_CLOSED_ORDERS: usize = 100_000;
/// Trades kept in memory (listings, the fills of a retry reply): the oldest is dropped when more
/// have happened. The ledgers and balances they settled are unaffected.
pub const RETAINED_TRADES: usize = 100_000;

/// How much closed history the book keeps; see [`Book::with_retention`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    pub closed_orders: usize,
    pub trades: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            closed_orders: RETAINED_CLOSED_ORDERS,
            trades: RETAINED_TRADES,
        }
    }
}

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
    /// Resting the remainder would have taken the account past its open-order or open-notional
    /// limit; the fills stand, the remainder does not rest.
    ExposureLimit,
}

/// Per-account limits on what may rest in the book at once, enforced inside the matcher so two
/// concurrent placements cannot both pass a check made outside it. Unlimited by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposureLimits {
    pub max_open_orders: u32,
    /// Sum over live orders of price times remaining, in micro-USDC.
    pub max_open_notional: u128,
}

impl Default for ExposureLimits {
    fn default() -> Self {
        Self {
            max_open_orders: u32::MAX,
            max_open_notional: u128::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Exposure {
    open: u32,
    notional: u128,
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

/// Appends an event to the bounded inspection log and to the broadcast buffer. A free function so
/// it can run while a price level is mutably borrowed.
fn record_into(events: &mut VecDeque<Event>, pending: &mut Vec<Event>, e: Event) {
    if events.len() == RECENT_EVENTS {
        events.pop_front();
    }
    events.push_back(e.clone());
    pending.push(e);
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
    /// Order ids per account (id order is placement order), so listings never scan the whole
    /// book and an archived order leaves in O(log n).
    by_account: HashMap<Arc<str>, BTreeSet<OrderId>>,
    /// account -> client_order_id -> order: the idempotency index.
    by_client_id: HashMap<Arc<str>, HashMap<Arc<str>, OrderId>>,
    /// Quantity filled and fill ids of every order that filled at placement, so an idempotent
    /// retry can be answered with the original reply without keeping a second copy of the order.
    fill_ids: HashMap<OrderId, (Qty, Vec<TradeId>)>,
    /// Closed orders in closing order; the front is archived first. See [`Retention`].
    closed: VecDeque<OrderId>,
    retention: Retention,
    /// The most recent events, bounded; see [`RECENT_EVENTS`].
    events: VecDeque<Event>,
    /// Events since the sequencer last drained them, for the broadcast.
    pending_events: Vec<Event>,
    /// Retained trades in id order (the front is the oldest kept), plus the ids each account
    /// took part in.
    trades: VecDeque<Trade>,
    trades_by_account: HashMap<Arc<str>, VecDeque<TradeId>>,
    last_trade_price: Option<Price>,
    next_order: OrderId,
    next_trade: TradeId,
    next_seq: Seq,
    /// When set, every order must be backed by the account's balance (see [`Book::with_balances`]).
    enforce_balances: bool,
    balances: HashMap<Arc<str>, Balances>,
    ledgers: HashMap<Arc<str>, Ledger>,
    exposure_limits: ExposureLimits,
    /// Live orders and their notional per account, kept incrementally for the limits.
    exposure: HashMap<Arc<str>, Exposure>,
}

/// One more live order for the account: `qty` at `price` now rests.
fn exposure_add(exposure: &mut HashMap<Arc<str>, Exposure>, account: &Arc<str>, price: Price, qty: Qty) {
    let e = exposure.entry(Arc::clone(account)).or_default();
    e.open += 1;
    e.notional += price as u128 * qty as u128;
}

/// Removes `qty` at `price` from the account's exposure, and one order when it closed. A free
/// function, like the wallet helpers, so it can run while a price level is borrowed.
fn exposure_sub(exposure: &mut HashMap<Arc<str>, Exposure>, account: &Arc<str>, price: Price, qty: Qty, closed: bool) {
    if let Some(e) = exposure.get_mut(account) {
        e.notional = e.notional.saturating_sub(price as u128 * qty as u128);
        if closed {
            e.open = e.open.saturating_sub(1);
        }
        if *e == Exposure::default() {
            exposure.remove(account);
        }
    }
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
    /// Quantity filled and fill ids per order that filled at placement, for idempotent retries.
    #[serde(default)]
    pub fill_ids: Vec<(OrderId, Qty, Vec<TradeId>)>,
    /// Closed orders in closing order, so archiving continues identically after a restart.
    #[serde(default)]
    pub closed: Vec<OrderId>,
    pub balances: Vec<(String, Balances)>,
    #[serde(default)]
    pub ledgers: Vec<(String, Ledger)>,
    pub last_trade_price: Option<Price>,
    pub next_order: OrderId,
    pub next_trade: TradeId,
    pub next_seq: Seq,
    pub enforce_balances: bool,
    /// The limits in force when the state was written; replay under other limits would differ.
    #[serde(default)]
    pub exposure_limits: ExposureLimits,
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
        let mut fill_ids: Vec<(OrderId, Qty, Vec<TradeId>)> = self
            .fill_ids
            .iter()
            .map(|(id, (filled, fills))| (*id, *filled, fills.clone()))
            .collect();
        fill_ids.sort_by_key(|(id, _, _)| *id);
        let mut balances: Vec<(String, Balances)> = self.balances.iter().map(|(a, b)| (a.to_string(), *b)).collect();
        balances.sort_by(|a, b| a.0.cmp(&b.0));
        let mut ledgers: Vec<(String, Ledger)> = self.ledgers.iter().map(|(a, l)| (a.to_string(), *l)).collect();
        ledgers.sort_by(|a, b| a.0.cmp(&b.0));
        BookState {
            orders,
            trades: self.trades.iter().cloned().collect(),
            fill_ids,
            closed: self.closed.iter().copied().collect(),
            balances,
            ledgers,
            last_trade_price: self.last_trade_price,
            next_order: self.next_order,
            next_trade: self.next_trade,
            next_seq: self.next_seq,
            enforce_balances: self.enforce_balances,
            exposure_limits: self.exposure_limits,
        }
    }

    /// Rebuilds a book from [`Book::state`]: the same orders, trades, balances and counters, with
    /// the price levels and indices derived again. Time priority within a level is id order,
    /// which is arrival order.
    pub fn from_state(state: BookState) -> Self {
        let mut book = Book {
            enforce_balances: state.enforce_balances,
            exposure_limits: state.exposure_limits,
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
            book.by_account.entry(Arc::clone(&o.account)).or_default().insert(o.id);
            book.by_client_id
                .entry(Arc::clone(&o.account))
                .or_default()
                .insert(Arc::clone(&o.client_order_id), o.id);
            if o.status.is_live() {
                live.push((o.side, o.price, o.id, o.remaining));
            }
            book.orders.insert(o.id, o);
        }
        for o in book.orders.values().filter(|o| o.status.is_live()) {
            let e = book.exposure.entry(Arc::clone(&o.account)).or_default();
            e.open += 1;
            e.notional += o.price as u128 * o.remaining as u128;
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
        for mut t in state.trades {
            t.maker_account = book.intern(&t.maker_account);
            t.taker_account = book.intern(&t.taker_account);
            book.trades_by_account
                .entry(Arc::clone(&t.maker_account))
                .or_default()
                .push_back(t.id);
            book.trades_by_account
                .entry(Arc::clone(&t.taker_account))
                .or_default()
                .push_back(t.id);
            book.trades.push_back(t);
        }
        for (id, filled, fills) in state.fill_ids {
            book.fill_ids.insert(id, (filled, fills));
        }
        if state.closed.is_empty() {
            // A state written before closing order was recorded: id order is the best guess.
            let mut closed: Vec<OrderId> = book
                .orders
                .values()
                .filter(|o| !o.status.is_live())
                .map(|o| o.id)
                .collect();
            closed.sort_unstable();
            book.closed = closed.into();
        } else {
            book.closed = state.closed.into();
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

    /// Sets the per-account exposure limits. Part of the durable state, since matching under
    /// other limits would rest different orders.
    pub fn with_exposure_limits(mut self, limits: ExposureLimits) -> Self {
        self.exposure_limits = limits;
        self
    }

    pub fn exposure_limits(&self) -> ExposureLimits {
        self.exposure_limits
    }

    /// Whether resting `qty` more at `price` keeps the account within its limits.
    fn exposure_allows(&self, account: &str, price: Price, qty: Qty) -> bool {
        let e = self.exposure.get(account).copied().unwrap_or_default();
        e.open < self.exposure_limits.max_open_orders
            && e.notional + price as u128 * qty as u128 <= self.exposure_limits.max_open_notional
    }

    /// Sets how much closed history is kept and applies it at once. Everything an order or
    /// trade changed (balances, ledgers, the book) is kept; only the records themselves leave.
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self.enforce_retention();
        self
    }

    pub fn retention(&self) -> Retention {
        self.retention
    }

    /// Orders currently in memory: every live order plus the retained closed ones.
    pub fn retained_orders(&self) -> usize {
        self.orders.len()
    }

    pub fn retained_trades(&self) -> usize {
        self.trades.len()
    }

    /// Archives the oldest closed orders and drops the oldest trades beyond the retention.
    fn enforce_retention(&mut self) {
        while self.closed.len() > self.retention.closed_orders {
            let id = self.closed.pop_front().expect("closed is not empty");
            self.archive(id);
        }
        while self.trades.len() > self.retention.trades {
            let t = self.trades.pop_front().expect("trades is not empty");
            for account in [&t.maker_account, &t.taker_account] {
                if let Some(ids) = self.trades_by_account.get_mut(account) {
                    // Per-account ids are in id order, so the oldest trade is at the front.
                    if ids.front() == Some(&t.id) {
                        ids.pop_front();
                    }
                    if ids.is_empty() {
                        self.trades_by_account.remove(account);
                    }
                }
            }
        }
    }

    /// Forgets a closed order: it leaves the lookups, its account's listing and the idempotency
    /// index (a retry of its client id after this point is a new order). A cancelled order that
    /// still sits in a level's queue is skipped by the matcher when reached.
    fn archive(&mut self, id: OrderId) {
        let Some(o) = self.orders.remove(&id) else { return };
        debug_assert!(!o.status.is_live(), "only closed orders are archived");
        if let Some(ids) = self.by_account.get_mut(&o.account) {
            ids.remove(&id);
            if ids.is_empty() {
                self.by_account.remove(&o.account);
            }
        }
        if let Some(m) = self.by_client_id.get_mut(&o.account) {
            m.remove(&o.client_order_id);
            if m.is_empty() {
                self.by_client_id.remove(&o.account);
            }
        }
        self.fill_ids.remove(&id);
    }

    /// Structural audit for tests and soaks, returning the first violation: every kept level
    /// matches its queue, every live order rests exactly once on its own side and price, the
    /// book is not crossed, statuses agree with quantities, the account index is exact,
    /// reservations back the live orders when balances are enforced, retained trade ids are
    /// contiguous, and every retention bound holds.
    pub fn check_invariants(&self) -> Result<(), String> {
        let mut resting: HashSet<OrderId> = HashSet::new();
        for (name, side, levels) in [("bids", Side::Buy, &self.bids), ("asks", Side::Sell, &self.asks)] {
            for (&price, level) in levels {
                if level.total == 0 || level.live == 0 {
                    return Err(format!("{name} level {price} is empty but kept"));
                }
                let (mut total, mut live) = (0, 0u32);
                for id in &level.queue {
                    // Lazily cancelled orders stay queued until reached; archived ones are gone.
                    let Some(o) = self.orders.get(id) else { continue };
                    if !o.status.is_live() {
                        continue;
                    }
                    if o.side != side || o.price != price {
                        return Err(format!("order {id} rests on the wrong side or price"));
                    }
                    if !resting.insert(*id) {
                        return Err(format!("order {id} rests twice"));
                    }
                    total += o.remaining;
                    live += 1;
                }
                if total != level.total || live != level.live {
                    return Err(format!(
                        "{name} level {price} says {} lots in {} orders but its queue holds {total} in {live}",
                        level.total, level.live
                    ));
                }
            }
        }
        if let (Some(b), Some(a)) = (self.best_bid(), self.best_ask()) {
            if b >= a {
                return Err(format!("crossed book: bid {b} >= ask {a}"));
            }
        }
        let mut backing: HashMap<&Arc<str>, (u128, Qty)> = HashMap::new();
        for o in self.orders.values() {
            let consistent = match o.status {
                Status::Open => o.remaining == o.qty,
                Status::PartiallyFilled => o.remaining > 0 && o.remaining < o.qty,
                Status::Filled => o.remaining == 0,
                Status::Cancelled | Status::Rejected => o.remaining > 0,
            };
            if !consistent {
                return Err(format!(
                    "order {} is {:?} with {} of {} remaining",
                    o.id, o.status, o.remaining, o.qty
                ));
            }
            if o.status.is_live() != resting.contains(&o.id) {
                return Err(format!(
                    "order {} is live={} but resting={}",
                    o.id,
                    o.status.is_live(),
                    !o.status.is_live()
                ));
            }
            if !self.by_account.get(&o.account).is_some_and(|ids| ids.contains(&o.id)) {
                return Err(format!("order {} is missing from its account index", o.id));
            }
            if o.status.is_live() {
                let e = backing.entry(&o.account).or_default();
                match o.side {
                    Side::Buy => e.0 += o.price as u128 * o.remaining as u128,
                    Side::Sell => e.1 += o.remaining,
                }
            }
        }
        for ids in self.by_account.values() {
            if let Some(id) = ids.iter().find(|id| !self.orders.contains_key(id)) {
                return Err(format!("the account index holds archived order {id}"));
            }
        }
        let mut expected_exposure: HashMap<&Arc<str>, Exposure> = HashMap::new();
        for o in self.orders.values().filter(|o| o.status.is_live()) {
            let e = expected_exposure.entry(&o.account).or_default();
            e.open += 1;
            e.notional += o.price as u128 * o.remaining as u128;
        }
        for (account, e) in &self.exposure {
            if expected_exposure.get(account) != Some(e) {
                return Err(format!("{account}: exposure {e:?} does not match its live orders"));
            }
        }
        if expected_exposure.len() != self.exposure.len() {
            return Err("an account with live orders has no exposure entry".into());
        }
        if self.enforce_balances {
            for (account, b) in &self.balances {
                let (usdc, eth) = backing.get(account).copied().unwrap_or_default();
                if b.usdc_reserved != usdc || b.eth_reserved != eth {
                    return Err(format!(
                        "{account} reserves {} micro-USDC and {} lots but its live orders need {usdc} and {eth}",
                        b.usdc_reserved, b.eth_reserved
                    ));
                }
            }
        }
        if let (Some(first), Some(last)) = (self.trades.front(), self.trades.back()) {
            if last.id + 1 - first.id != self.trades.len() as u64 {
                return Err("retained trade ids are not contiguous".into());
            }
        }
        if self.events.len() > RECENT_EVENTS
            || self.closed.len() > self.retention.closed_orders
            || self.trades.len() > self.retention.trades
        {
            return Err("a retention bound is exceeded".into());
        }
        Ok(())
    }

    /// A retained trade by id.
    pub fn trade(&self, id: TradeId) -> Option<&Trade> {
        let first = self.trades.front()?.id;
        if id < first {
            return None;
        }
        self.trades.get((id - first) as usize)
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
        record_into(
            &mut self.events,
            &mut self.pending_events,
            Event::Deposited {
                account: key,
                usdc,
                eth,
                seq,
            },
        );
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
        record_into(
            &mut self.events,
            &mut self.pending_events,
            Event::Withdrawn {
                account: key,
                usdc,
                eth,
                seq,
            },
        );
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

    /// Events recorded since the last call, oldest first: what the sequencer broadcasts.
    pub fn take_new_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending_events)
    }

    /// The reply an order got when it was placed, rebuilt from its current record and its fills:
    /// the same order fields, the remaining quantity after the placement fills, the status and
    /// cancel reason as of then (a later user cancel is not part of the original reply).
    fn original_reply(&self, id: OrderId) -> (Order, Vec<Trade>) {
        let stored = &self.orders[&id];
        // Fills that have left the trade window are no longer listed; the quantity is kept.
        let (filled, fills): (Qty, Vec<Trade>) = self
            .fill_ids
            .get(&id)
            .map(|(filled, ids)| (*filled, ids.iter().filter_map(|t| self.trade(*t)).cloned().collect()))
            .unwrap_or_default();
        let mut o = stored.clone();
        if stored.status != Status::Rejected {
            o.remaining = o.qty - filled;
            let at_placement = matches!(
                stored.cancel_reason,
                Some(CancelReason::Ioc) | Some(CancelReason::Fok) | Some(CancelReason::SelfTradePrevention)
            );
            o.cancel_reason = if at_placement { stored.cancel_reason } else { None };
            o.status = if o.remaining == 0 {
                Status::Filled
            } else if at_placement {
                Status::Cancelled
            } else if filled > 0 {
                Status::PartiallyFilled
            } else {
                Status::Open
            };
        }
        (o, fills)
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
                let Some(o) = self.orders.get(id) else { continue };
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
        if req.price > MAX_PRICE || req.qty > MAX_QTY {
            return Err(EngineError::Invalid(format!(
                "price must be at most {MAX_PRICE} ticks and quantity at most {MAX_QTY} lots"
            )));
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
            let (original, fills) = self.original_reply(id);
            return if original.side == req.side && original.price == req.price && original.qty == req.qty {
                Ok((original, fills))
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
        self.by_account.entry(account).or_default().insert(o.id);

        if tif == Tif::Fok && self.available(o.side, o.price, &o.account, o.qty) < o.qty {
            o.status = Status::Rejected;
            record_into(
                &mut self.events,
                &mut self.pending_events,
                Event::Rejected {
                    id: o.id,
                    account: Arc::clone(&o.account),
                    reason: "FOK: insufficient quantity available at this price".into(),
                    seq: o.seq,
                },
            );
            self.orders.insert(o.id, o.clone());
            self.closed.push_back(o.id);
            self.enforce_retention();
            return Ok((o, Vec::new()));
        }
        record_into(&mut self.events, &mut self.pending_events, Event::Accepted(o.clone()));
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
            let maker = match self.orders.get_mut(&maker_id) {
                Some(m) if m.status.is_live() => m,
                _ => {
                    // Lazily dropped: cancel only marks the order (and archiving may have
                    // removed it since); the matcher removes it from the queue here.
                    level.queue.pop_front();
                    continue;
                }
            };
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
                self.closed.push_back(maker_id);
            }
            let maker_closed = maker.remaining == 0;
            let level_empty = level.total == 0;
            let maker_account = Arc::clone(&maker.account);
            exposure_sub(&mut self.exposure, &maker_account, px, q, maker_closed);
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
            record_into(&mut self.events, &mut self.pending_events, Event::Traded(trade.clone()));
            self.trades_by_account
                .entry(maker_account)
                .or_default()
                .push_back(trade.id);
            self.trades_by_account
                .entry(Arc::clone(&o.account))
                .or_default()
                .push_back(trade.id);
            self.trades.push_back(trade.clone());
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
                    Tif::Gtc if !self.exposure_allows(&o.account, o.price, o.remaining) => {
                        cancel = Some(CancelReason::ExposureLimit)
                    }
                    Tif::Gtc => {
                        let same = match o.side {
                            Side::Buy => &mut self.bids,
                            Side::Sell => &mut self.asks,
                        };
                        let level = same.entry(o.price).or_default();
                        level.total += o.remaining;
                        level.live += 1;
                        level.queue.push_back(o.id);
                        exposure_add(&mut self.exposure, &o.account, o.price, o.remaining);
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
            record_into(
                &mut self.events,
                &mut self.pending_events,
                Event::Cancelled {
                    id: o.id,
                    account: Arc::clone(&o.account),
                    reason,
                    seq,
                },
            );
        }
        self.orders.insert(o.id, o.clone());
        if !fills.is_empty() {
            self.fill_ids
                .insert(o.id, (o.qty - o.remaining, fills.iter().map(|t| t.id).collect()));
        }
        if !o.status.is_live() {
            self.closed.push_back(o.id);
        }
        self.enforce_retention();
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
        exposure_sub(&mut self.exposure, &owner, price, remaining, true);
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
        record_into(
            &mut self.events,
            &mut self.pending_events,
            Event::Cancelled {
                id,
                account: owner,
                reason: CancelReason::User,
                seq,
            },
        );
        let cancelled = self.orders[&id].clone();
        self.closed.push_back(id);
        self.enforce_retention();
        Ok(cancelled)
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
                .map(|ids| {
                    ids.iter()
                        .rev()
                        .take(limit)
                        .filter_map(|id| self.trade(*id))
                        .cloned()
                        .collect()
                })
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

    /// The most recent events (bounded by [`RECENT_EVENTS`]), oldest first.
    pub fn events(&self) -> Vec<Event> {
        self.events.iter().cloned().collect()
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
    fn hard_caps_reject_absurd_orders_before_anything_else() {
        let mut b = Book::new();
        assert!(matches!(
            b.place(req("a", "p", Side::Buy, MAX_PRICE + 1, 1, Tif::Gtc), 0),
            Err(EngineError::Invalid(_))
        ));
        assert!(matches!(
            b.place(req("a", "q", Side::Sell, 1, MAX_QTY + 1, Tif::Gtc), 0),
            Err(EngineError::Invalid(_))
        ));
        assert_eq!(b.seq(), 0, "a rejected request leaves no trace");
        b.place(req("a", "ok", Side::Buy, MAX_PRICE, MAX_QTY, Tif::Gtc), 0)
            .unwrap();
        b.check_invariants().unwrap();
    }

    #[test]
    fn exposure_limits_stop_the_remainder_from_resting() {
        let limits = ExposureLimits {
            max_open_orders: 2,
            max_open_notional: 2_000 * 300_000, // 600 USDC at 3000: two small orders, not three
        };
        let mut b = Book::new().with_exposure_limits(limits);
        b.place(req("a", "1", Side::Buy, 300_000, 1_000, Tif::Gtc), 0).unwrap();
        b.place(req("a", "2", Side::Buy, 299_000, 1_000, Tif::Gtc), 0).unwrap();
        // A third resting order is over the count: cancelled with the reason, nothing rests.
        let (third, _) = b.place(req("a", "3", Side::Buy, 298_000, 1_000, Tif::Gtc), 0).unwrap();
        assert_eq!(
            (third.status, third.cancel_reason),
            (Status::Cancelled, Some(CancelReason::ExposureLimit))
        );
        assert_eq!(b.open_orders_for("a").len(), 2);
        // A crossing order still fills: the limit is on what rests, not on what trades.
        let (hit, fills) = b.place(req("s", "s1", Side::Sell, 300_000, 500, Tif::Gtc), 0).unwrap();
        assert_eq!((hit.status, fills.len()), (Status::Filled, 1));
        // Another account has its own room.
        assert_eq!(
            b.place(req("z", "z1", Side::Buy, 297_000, 1_000, Tif::Gtc), 0)
                .unwrap()
                .0
                .status,
            Status::Open
        );
        // Cancelling makes room again; the notional limit then bites before the count does.
        b.cancel("a", 2).unwrap();
        let (big, _) = b.place(req("a", "4", Side::Buy, 300_000, 1_600, Tif::Gtc), 0).unwrap();
        assert_eq!(
            big.cancel_reason,
            Some(CancelReason::ExposureLimit),
            "500 + 1600 lots at 3000 exceeds 600 USDC"
        );
        let (fits, _) = b.place(req("a", "5", Side::Buy, 300_000, 1_400, Tif::Gtc), 0).unwrap();
        assert_eq!(fits.status, Status::Open);
        b.check_invariants().unwrap();
        // The limits travel with the state.
        let r = Book::from_state(b.state());
        assert_eq!(r.exposure_limits(), limits);
        r.check_invariants().unwrap();
    }

    #[test]
    fn closed_history_is_bounded_and_live_orders_are_kept() {
        let keep = Retention {
            closed_orders: 20,
            trades: 5,
        };
        let mut b = Book::new().with_retention(keep);
        // Two bids at 298000 from m; c1 is cancelled but stays in the level's queue behind c2.
        let (c1, _) = b.place(req("m", "c1", Side::Buy, 298_000, 5, Tif::Gtc), 0).unwrap();
        let (c2, _) = b.place(req("m", "c2", Side::Buy, 298_000, 5, Tif::Gtc), 0).unwrap();
        b.cancel("m", c1.id).unwrap();
        // Thirty bids from m, each lifted by a sell from t: 30 trades, 60 more closed orders.
        let mut pairs = Vec::new();
        for i in 0..30u64 {
            let (bid, _) = b
                .place(req("m", &format!("b{i}"), Side::Buy, 300_000, 10, Tif::Gtc), i as i64)
                .unwrap();
            let (ask, fills) = b
                .place(req("t", &format!("s{i}"), Side::Sell, 300_000, 10, Tif::Gtc), i as i64)
                .unwrap();
            assert_eq!(fills.len(), 1);
            pairs.push((bid.id, ask.id));
        }
        b.check_invariants().unwrap();
        assert_eq!(b.retained_orders(), 21, "20 closed orders and the live c2");
        assert_eq!(b.retained_trades(), 5);
        assert!(b.order(c1.id).is_none(), "the cancelled order was archived");
        assert!(b.order(pairs[0].0).is_none());
        assert_eq!(b.order(c2.id).map(|o| o.status), Some(Status::Open));
        assert_eq!(
            b.orders_for("t", |_| true, 100).len(),
            10,
            "only the newest closed are listed"
        );
        assert_eq!(b.orders_for("m", |_| true, 100).len(), 11);
        assert_eq!(b.open_orders_for("m").len(), 1);
        assert_eq!(
            b.trades(None, 100).iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![30, 29, 28, 27, 26]
        );
        assert_eq!(b.trades(Some("t"), 100).len(), 5);
        assert!(b.trades(Some("nobody"), 100).is_empty());
        // The balances and the ledger remember every fill, retained or not.
        assert_eq!(b.statement("t").sold_lots, 300);

        // A retry of a retained order whose fill left the window still says what happened.
        let (again, fills) = b.place(req("t", "s20", Side::Sell, 300_000, 10, Tif::Gtc), 1).unwrap();
        assert_eq!(
            (again.id, again.status, again.remaining),
            (pairs[20].1, Status::Filled, 0)
        );
        assert!(fills.is_empty(), "trade 21 is no longer retained");
        // A retry of an archived client id is a new order.
        let (fresh, _) = b.place(req("t", "s0", Side::Sell, 300_000, 10, Tif::Gtc), 2).unwrap();
        assert!(fresh.id > pairs[29].1);
        assert_eq!(fresh.status, Status::Open);
        // The matcher skips the archived c1 in the queue and fills against c2.
        let (hit, fills) = b.place(req("t", "hit", Side::Sell, 298_000, 5, Tif::Gtc), 3).unwrap();
        assert_eq!((hit.status, fills.len(), fills[0].maker), (Status::Filled, 1, c2.id));

        // A restart keeps archiving in the same order as the never-restarted book.
        let mut r = Book::from_state(b.state()).with_retention(keep);
        assert_eq!(r.orders_for("m", |_| true, 100), b.orders_for("m", |_| true, 100));
        assert_eq!(r.trades(None, 100), b.trades(None, 100));
        for (book, i) in [(&mut b, 0i64), (&mut r, 0i64)] {
            book.place(req("m", "after", Side::Buy, 300_000, 1, Tif::Gtc), i)
                .unwrap();
            book.place(req("t", "after", Side::Sell, 300_000, 1, Tif::Gtc), i)
                .unwrap();
        }
        assert_eq!(r.retained_orders(), b.retained_orders());
        r.check_invariants().unwrap();
        b.check_invariants().unwrap();
        assert_eq!(r.orders_for("t", |_| true, 100), b.orders_for("t", |_| true, 100));
        assert_eq!(r.trades(Some("m"), 100), b.trades(Some("m"), 100));
    }

    #[test]
    fn original_replies_survive_later_changes_and_the_event_log_is_bounded() {
        let mut b = Book::new();
        b.place(gtc("m", "a1", Side::Sell, 300_000, 100), 0).unwrap();
        let (o, f) = b.place(gtc("t", "k", Side::Buy, 300_000, 300), 0).unwrap();
        assert_eq!((o.status, o.remaining, f.len()), (Status::PartiallyFilled, 200, 1));
        // The order changes afterwards (a user cancel); the replay still describes the placement.
        b.cancel("t", o.id).unwrap();
        let (again, fills) = b.place(gtc("t", "k", Side::Buy, 300_000, 300), 0).unwrap();
        assert_eq!((again, fills), (o, f));
        // An IOC remainder is a placement-time cancel and stays in the replay.
        let (ioc, _) = b.place(req("t", "i", Side::Buy, 300_000, 10, Tif::Ioc), 0).unwrap();
        assert_eq!(
            (ioc.status, ioc.cancel_reason),
            (Status::Cancelled, Some(CancelReason::Ioc))
        );
        assert_eq!(
            b.place(req("t", "i", Side::Buy, 300_000, 10, Tif::Ioc), 0).unwrap().0,
            ioc
        );
        // The broadcast buffer drains; the inspection log stays bounded.
        assert!(b.take_new_events().len() >= 5);
        assert!(b.take_new_events().is_empty());
        for i in 0..(RECENT_EVENTS + 10) {
            b.deposit("x", 1, 0).unwrap();
            if i % 50_000 == 0 {
                b.take_new_events();
            }
        }
        assert_eq!(b.events().len(), RECENT_EVENTS);
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

    /// A deliberately naive matcher: resting orders in a vector, best price then lowest id chosen
    /// by a scan on every fill. Slow and obviously right, so the book can be checked against it.
    #[derive(Default)]
    struct Reference {
        resting: Vec<(OrderId, String, Side, Price, Qty)>,
        trades: Vec<(OrderId, OrderId, Price, Qty)>,
        next_id: OrderId,
    }

    impl Reference {
        fn best_opposite(&self, side: Side, limit: Price) -> Option<usize> {
            let mut best: Option<usize> = None;
            for (i, (id, _, s, px, _)) in self.resting.iter().enumerate() {
                if *s == side {
                    continue;
                }
                let crosses = match side {
                    Side::Buy => *px <= limit,
                    Side::Sell => *px >= limit,
                };
                if !crosses {
                    continue;
                }
                best = match best {
                    None => Some(i),
                    Some(j) => {
                        let (jid, _, _, jpx, _) = &self.resting[j];
                        let better = match side {
                            Side::Buy => px < jpx || (px == jpx && id < jid),
                            Side::Sell => px > jpx || (px == jpx && id < jid),
                        };
                        Some(if better { i } else { j })
                    }
                };
            }
            best
        }

        /// Quantity that could fill before the walk reaches one of the account's own orders.
        fn available(&self, account: &str, side: Side, limit: Price) -> Qty {
            let mut candidates: Vec<&(OrderId, String, Side, Price, Qty)> = self
                .resting
                .iter()
                .filter(|(_, _, s, px, _)| {
                    *s != side
                        && match side {
                            Side::Buy => *px <= limit,
                            Side::Sell => *px >= limit,
                        }
                })
                .collect();
            candidates.sort_by(|a, b| match side {
                Side::Buy => a.3.cmp(&b.3).then(a.0.cmp(&b.0)),
                Side::Sell => b.3.cmp(&a.3).then(a.0.cmp(&b.0)),
            });
            let mut got = 0;
            for (_, acct, _, _, q) in candidates {
                if acct == account {
                    break;
                }
                got += q;
            }
            got
        }

        /// Returns (order id, remaining, rejected).
        fn place(&mut self, account: &str, side: Side, price: Price, qty: Qty, tif: Tif) -> (OrderId, Qty, bool) {
            self.next_id += 1;
            let id = self.next_id;
            if tif == Tif::Fok && self.available(account, side, price) < qty {
                return (id, qty, true);
            }
            let mut remaining = qty;
            while remaining > 0 {
                let Some(i) = self.best_opposite(side, price) else {
                    break;
                };
                if self.resting[i].1 == account {
                    break; // self-trade prevention: the newest order stops here
                }
                let q = remaining.min(self.resting[i].4);
                self.trades.push((self.resting[i].0, id, self.resting[i].3, q));
                remaining -= q;
                self.resting[i].4 -= q;
                if self.resting[i].4 == 0 {
                    self.resting.remove(i);
                }
            }
            if remaining > 0 && tif == Tif::Gtc && self.available(account, side, price) == 0 {
                // Rests only when nothing more could fill: the loop stopped at an own order or
                // ran out of crossing orders. A stop at an own order cancels instead.
                let own_ahead = self.best_opposite(side, price).is_some();
                if !own_ahead {
                    self.resting.push((id, account.to_string(), side, price, remaining));
                }
            }
            (id, remaining, false)
        }

        fn cancel(&mut self, id: OrderId) {
            self.resting.retain(|o| o.0 != id);
        }
    }

    proptest::proptest! {
        /// The book agrees with the naive reference on every trade (maker, taker, price,
        /// quantity, in order) and on the resting orders after every operation.
        #[test]
        fn matches_the_naive_reference(
            ops in proptest::collection::vec((0u8..2, 1u64..40, 1u64..500, 0u8..3), 1..300)
        ) {
            let mut b = Book::new();
            let mut r = Reference::default();
            for (i, (side, price, qty, tif)) in ops.iter().enumerate() {
                let side = if *side == 0 { Side::Buy } else { Side::Sell };
                let tif = match tif { 0 => Tif::Gtc, 1 => Tif::Ioc, _ => Tif::Fok };
                let account = format!("{}{}", if side == Side::Buy { "b" } else { "s" }, i % 3);
                let px = 299_000 + price * 10;
                let (o, _) = b.place(req(&account, &i.to_string(), side, px, *qty, tif), 0).unwrap();
                let (rid, remaining, rejected) = r.place(&account, side, px, *qty, tif);
                proptest::prop_assert_eq!(o.id, rid);
                proptest::prop_assert_eq!(o.status == Status::Rejected, rejected, "FOK decision differs at op {}", i);
                proptest::prop_assert_eq!(o.remaining, remaining, "remaining differs at op {}", i);
                if i % 7 == 3 {
                    let victim = (i as u64 / 2).max(1);
                    if let Some(v) = b.order(victim).cloned() {
                        let _ = b.cancel(&v.account, victim);
                        r.cancel(victim);
                    }
                }
                proptest::prop_assert!(b.check_invariants().is_ok(), "{:?}", b.check_invariants());
                let book_trades: Vec<(OrderId, OrderId, Price, Qty)> =
                    b.trades(None, usize::MAX).iter().rev().map(|t| (t.maker, t.taker, t.price, t.qty)).collect();
                proptest::prop_assert_eq!(&book_trades, &r.trades, "trades differ after op {}", i);
                let mut resting: Vec<(OrderId, Qty)> = r.resting.iter().map(|o| (o.0, o.4)).collect();
                resting.sort_unstable();
                let mut live: Vec<(OrderId, Qty)> = ["b0", "b1", "b2", "s0", "s1", "s2"]
                    .iter()
                    .flat_map(|a| b.open_orders_for(a))
                    .map(|o| (o.id, o.remaining))
                    .collect();
                live.sort_unstable();
                proptest::prop_assert_eq!(live, resting, "resting orders differ after op {}", i);
            }
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
                    proptest::prop_assert!(b.check_invariants().is_ok(), "{:?}", b.check_invariants());
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
                proptest::prop_assert!(b.check_invariants().is_ok(), "{:?}", b.check_invariants());
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
