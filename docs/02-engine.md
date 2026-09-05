# 02 · The engine

The book sorts bids highest first and asks lowest first. Incoming limit orders match the opposite side within their limit; any resting remainder joins the queue at its own price.

## Matching rules

| Rule | Behaviour |
|---|---|
| Price-time priority | Best price first; at the same price, first in, first out |
| Crossing | A buy at P matches asks priced at or below P; a sell at P matches bids at or above P |
| Maker price | Trades execute at the resting order's price; the taker gets the price improvement |
| GTC | The unfilled remainder rests at the order's own limit |
| IOC | The unfilled remainder is cancelled |
| FOK | Availability is checked first; insufficient quantity rejects the order without matching |
| Self-trade prevention | "Cancel newest": an incoming order never trades with its own account's resting order; matching stops there and the remainder is cancelled |
| Idempotency | While retained, `client_order_id` is unique per account. Matching retries reconstruct the placement reply with retained fills; different parameters return `ALREADY_EXISTS` |

### Worked example

Resting: asks 3001.00 × 0.5 (A1), 3002.00 × 1.0 (A2); bid 2999.00 × 0.8. Incoming: buy 1.2 @ 3002.00.

1. 0.5 trades at **3001.00** (A1's price, not 3002.00). A1 is filled and removed.
2. 0.7 trades at 3002.00. A2 has 0.3 left.
3. The incoming order is filled: 1.2 ETH for 3601.90 USDC, average 3001.58.

This scenario is the first unit test in `crates/engine/src/book.rs` and is replayed through gRPC and MCP in the other crates' tests.

## Data structures

```rust
pub struct Book {
    bids: BTreeMap<Price, Level>,                      // best bid = last key
    asks: BTreeMap<Price, Level>,                      // best ask = first key
    orders: HashMap<OrderId, Order>,                   // O(1) cancel and lookup
    by_account: HashMap<Arc<str>, BTreeSet<OrderId>>,  // one account's orders, placement order
    by_client_id: HashMap<Arc<str>, HashMap<Arc<str>, OrderId>>, // idempotency: account -> client id
    fill_ids: HashMap<OrderId, (Qty, Vec<TradeId>)>,   // reconstruct placement replies
    events: VecDeque<Event>,                           // bounded event history
    trades: VecDeque<Trade>,                           // retained trades, sequence order
    trades_by_account: HashMap<Arc<str>, VecDeque<TradeId>>,
    last_trade_price: Option<Price>,
    next_order: u64, next_trade: u64, next_seq: u64,   // counters, never clocks or UUIDs
}
struct Level { total: Qty, live: u32, queue: VecDeque<OrderId> }  // FIFO of ids = time priority
```

Each level holds a FIFO of order ids, so an order lives in exactly one place. Cancel is O(1): mark the order, subtract its remaining quantity from the level total, drop the level when the total reaches zero; the matcher skips cancelled ids lazily when it reaches them.

**Atomic cancellation.** `CancelAllOrders` cancels an account's live orders in one matcher command and journal record.

**Rate limiting.** `ENGINE_ACCOUNT_RATE_PER_SEC` uses a token bucket before queue admission: 50 mutations/second/account in the standalone binary, unlimited in the library default. Excess requests receive `RESOURCE_EXHAUSTED` and a retry delay. The same limit applies to pipelined requests.

**Exposure limits.** The matcher checks per-account live-order and open-notional caps before a remainder rests. Standalone defaults are 20 orders and 200000 USDC (`ENGINE_MAX_OPEN_ORDERS`, `ENGINE_MAX_OPEN_NOTIONAL_USDC`); library defaults are unlimited. Fills stand, but a remainder exceeding either cap is cancelled with reason `exposure_limit`. Checking inside the matcher prevents concurrent placements from bypassing the limits.

**Pipelined placement.** `PlaceOrders` is a bidirectional stream. The client sends requests without waiting for replies and gets one result per request, in request order. A refused request comes back as a result carrying its status code, not as the end of the stream.

Two tasks make that work: one queues each request into the matcher as it arrives, waiting for room rather than answering `RESOURCE_EXHAUSTED`; the other forwards replies in order.

Recorded throughput: 955k orders/s on one stream, 673k on four, versus 63k with sixteen unary clients. See [results](results/README.md).

`GetStats` returns the matcher's counters for dashboards: commands, batches, largest batch, events, orders and trades retained, sequence, and queue room.

**Health and reflection.** The server registers the standard gRPC health service and reflection over its descriptor set, so `grpc-health-probe` and `grpcurl` work without the proto file.

**Hard caps.** No order may be priced above 1,000,000.00 USDC or be larger than 10,000 ETH (`MAX_PRICE`, `MAX_QTY`), whatever the wallet mode and whatever the layers above enforce. A rejected request leaves no trace: no id, no sequence number.

**Structural audit.** `Book::check_invariants` checks level totals and queues, live-order placement, an uncrossed book, statuses, account and exposure indices, reservations, contiguous retained trade ids and retention bounds. Property tests run it after each operation; retention and recovery tests also check their results.

**Closed history is bounded.** The engine retains up to 100,000 closed orders and trades (`RETAINED_CLOSED_ORDERS`, `RETAINED_TRADES`), with an age limit of 24 hours (`ENGINE_RETAIN_HOURS`). Successful placements and cancellations advance pruning using journaled command timestamps; reads do not. Live orders never expire. Account count, funded balances, ledgers and journal disk usage have no global cap.

Archiving a closed order removes it from the lookups, from its account's listing, and from the idempotency index. So `GetOrder` answers `NOT_FOUND`, and reusing its `client_order_id` places a new order rather than replaying the old one. Nothing else changes: balances, ledgers and the book itself are untouched, and the account's statement still counts every fill.

The per-account `BTreeSet` supports removal in `O(log n)` and newest-first listings. Price-level queues skip archived ids lazily. Retries reconstruct replies from the order and its retained fill ids, avoiding a separate reply cache. Snapshots preserve pruning order; `Book::with_retention` sets the limits. Tests cover listings, retries, queue cleanup and restart.

`make soak` (`scripts/soak.sh`, `crates/engine-server/examples/soak.rs`) checks the bound end to end. Four rounds, a million orders each, over gRPC from sixteen clients with the journal on, and every order still resting 32 placements later is cancelled. Each round runs in a fresh process that recovers the previous snapshot and journal tail, compacts, and reports its own memory.

On the laptop: round 1 has nothing to recover and peaks at 110 MB. Rounds 2 and 3 each recover a 39 to 40 MB snapshot plus a 195 MB journal in 2.2 to 2.5 s, and peak at 108 and 107 MB. The snapshot stops growing at the retention limit, and so does the process.

**Account-indexed listings.** `ListOrders` walks an account's ordered ids backwards; `ListTrades` walks its trade ids. Both stop at the requested limit and run on the matcher thread.

With a million orders in the book, listing ten open orders of one account takes 0.7 µs. The full scan it replaced took 20 ms, on the matcher thread, and the MCP server does that listing before every order it places (to count open orders for the policy).

Account and client ids use interned `Arc<str>` values to avoid copying strings into events and replies.

A trade records both order ids, both account ids and the taker's side, so a listing can say which side an account traded on and whether it was maker or taker; the gRPC layer blanks the counterparty's account in account-scoped listings.

## Determinism

The same input sequence always produces the same output, which makes the engine replayable and the evaluations reproducible.

* One thread applies commands in arrival order, and that order is the sequence number.
* Order ids, trade ids and sequence numbers are counters.
* Wall-clock timestamps are passed in by the caller (`now_ns`), recorded for reporting, and never used for ordering.

Two property tests cover the book. The first generates random sequences of places (GTC, IOC, FOK) and cancels across four accounts. After every step it asserts that the book is never crossed, that displayed depth equals the open quantity, that every trade is at the maker's price and inside the taker's limit, that no self-trade happens, that sequence numbers are unique, and that replaying the same sequence gives an identical event log.

The second compares the book with a vector-based reference matcher that scans for the best price and earliest id on every fill. Trades and resting orders must agree after every operation, including self-trade prevention and FOK handling.

## Thread safety and concurrency: the single writer

tonic runs each RPC as a tokio task, so many `PlaceOrder` calls arrive at once. Instead of `Arc<Mutex<Book>>`, the book is *moved* into one matcher thread:

![Three tonic handler tasks send commands into a bounded channel; one matcher thread owns the Book, publishes an ArcSwap snapshot and replies over oneshot channels](assets/single-writer.svg)

* Handlers hold an `EngineHandle`: a channel sender and a pointer to the latest snapshot. Nothing else can reach the book, and the compiler enforces it.
* Contention is one channel send. The book itself has no lock and no `unsafe`.
* Reads of the top of book (`GetOrderBook`, `GetMarket`) never enter the queue: readers swap to the latest published snapshot lock-free.
* The matcher drains up to 256 queued commands, applies them in arrival order and publishes one snapshot before replying. A read after a successful reply sees at least that command's effects.
* The channel is bounded (10,000 by default). When it is full, `try_send` fails and the handler answers `RESOURCE_EXHAUSTED` instead of growing memory.
* The matcher and tokio workers can run on separate cores, overlapping matching, encoding and networking.

`crates/engine-server/tests/concurrency.rs` runs a real tonic server with sixteen concurrent clients placing 500 orders each. Every response succeeds, every event's sequence number is unique and contiguous, and the book is never crossed. A second test has eight accounts rest 150 orders each at shared price levels and cancel all of them from parallel tasks while the others are still placing: every cancel succeeds and the book ends empty, which checks the lazily dropped cancelled ids and the per-level totals under interleaving.

## Balances and settlement

Every account has a wallet: USDC in micro-USDC (one tick times one lot, so notionals never need rounding) and ETH in lots. Each is split into *available* and *reserved*.

Placing reserves. A buy reserves price times quantity in USDC, a sell reserves the quantity in ETH. An order the wallet cannot back is refused before it gets an id: `FAILED_PRECONDITION`, with the numbers ("insufficient USDC: the order needs 1500.00, 1000.00 available").

Filling settles both legs at the maker's price. The buyer reserved at its own limit, is charged the trade price, and gets the difference back as available. The seller's reserved ETH moves to the buyer, the buyer's USDC to the seller.

Everything else releases: a cancel, an IOC remainder, a self-trade-prevention cancel, and a FOK that never rested.

Deposits carry a sequence number like any event, so a replayed journal restores wallets as well as orders. `ENGINE_FUND=demo:50000:10` credits accounts when the engine starts on an empty book. It is journaled like any deposit and skipped after a replay, so a restart never funds twice.

The property test funds two buyers and two sellers, one of each tightly, then runs random places and cancels. After every step it checks that no USDC or ETH was created or destroyed, that each account's reserved USDC equals price times remaining over its live buys, that its reserved ETH equals the remaining of its live sells, and that nothing went negative (the unsigned arithmetic would panic). The market maker, the simulation bot and the benchmarks are funded with effectively unlimited balances, so they measure matching rather than funding; `ENGINE_BALANCES=0` turns the checks off entirely.

The accounting costs about 10% of pure-book throughput (650k to 720k operations/s against 690k to 850k without it).

**Withdrawals and the ledger.** `Withdraw` debits what is available; reserved amounts back live orders and cannot leave.

Beside the wallet, every account has a ledger the matcher updates on each fill. It holds deposits and withdrawals, ETH bought and sold with the USDC paid and received, the ETH still held from purchases on this venue and what it cost (average cost is the ratio), and realised P&L.

P&L is booked on every sell, as price minus average cost, over the part that came from that inventory. ETH that arrived by deposit has no cost basis here, so a sell beyond the venue inventory is counted separately and earns no P&L rather than inventing one.

`GetStatement` returns the ledger; the MCP tool adds unrealised P&L at the current market. The property test checks the ledgers are zero-sum across accounts (USDC paid equals USDC received, ETH bought equals ETH sold) and that each account's inventory equals what it bought minus what it sold from it. Deposits and withdrawals are events with sequence numbers and journal records, and the ledger is part of the snapshot.

## Event stream

The matcher broadcasts every event of a batch in sequence order, right after publishing that batch's snapshot: accepted orders, trades, cancels with their reason, rejects, deposits and withdrawals.

`Subscribe` is a server-streaming RPC over that broadcast. With an `account_id` it sends only that account's events, hiding the counterparty on a trade; without one it sends everything.

A subscriber more than 8,192 events behind is not silently skipped. Its stream ends with `DATA_LOSS`, telling it to resynchronise from `GetOrderBook` and `ListOrders` and subscribe again. Events already in the book when a subscriber arrives (from a replayed journal) are history, not news, and are not sent.

`crates/engine-server/tests/concurrency.rs` checks that a subscriber scoped to one account sees its own deposit, orders, trade and cancel in order, and never the other account's orders or id.

## Durability: a journal of commands, replayed

Set `ENGINE_JOURNAL=/path/file.jsonl` to journal placements, cancellations, deposits and withdrawals with their timestamps before applying them. The journal commits once per matcher batch, before replies. Reads are not journaled. A failed commit stops the matcher because its in-memory state may no longer be recoverable.

Startup replays commands through the same matcher, restoring orders, trades, wallets and counters. Retained client ids still deduplicate retries. A corrupt line fails startup.

Two levels of durability, measured on the laptop in the README with the gRPC benchmark:

| Setting | What survives | Sequential p50 | 16 clients |
|---|---|---|---|
| no journal | nothing: in memory only | 79 µs | 68k orders/s |
| journal, flush per batch (default with `ENGINE_JOURNAL`) | a process crash or restart; not a power loss | 88 µs | 61k orders/s |
| journal, `ENGINE_JOURNAL_FSYNC=1` | a power loss, for every reply already sent | 4.1 ms | 2.1k orders/s |

Batching is what makes the fsync variant usable under load: one `fsync` covers every command queued while the previous one ran.

At startup, a journal above `ENGINE_JOURNAL_COMPACT_MB` (64 by default) is compacted into a snapshot plus an empty tail. Compaction does not run during service, so disk usage can grow until restart. Snapshots stream as JSON lines; the older single-object format remains readable.

Compaction durably publishes a generation marker, retires the journal, publishes its snapshot, then durably removes the retired input before removing the marker. Recovery compares generations to avoid replaying deposits twice, and snapshots recovered retired input before processing an active tail. Ambiguous generations fail startup. Tests cover interrupted compaction and repeated restarts.

The snapshot holds the book's durable state: retained orders and trades, the fills each order took at placement (for idempotent retries), the archiving order, wallets and counters. Price levels and indices are derived again on load, with time priority inside a level restored as id order.

A snapshot binds balance mode, exposure limits and retention settings; recovery refuses mismatches. Journal-only recovery has no configuration manifest, so those settings must be preserved by the operator. The in-memory event log starts empty after loading a snapshot.

The recovery tests live in three places.

* `crates/engine/src/journal.rs` has the replay test (2,000 random places and cancels, journaled, replayed into a fresh book, identical event log and snapshot) and the compaction test (snapshot plus tail recovers the same book as never restarting).
* `crates/engine/src/book.rs` round-trips the state through JSON and checks the rebuilt book produces identical fills.
* `crates/engine-server/tests/concurrency.rs` restarts a real server from its journal, then from a compacted snapshot plus tail, checking orders, book, wallets, sequence and idempotency each time.

## gRPC contract

`proto/clob.proto` defines thirteen unary RPCs: `PlaceOrder`, `CancelOrder`, `CancelAllOrders`, `GetOrder`, `ListOrders`, `ListTrades`, `GetOrderBook`, `GetMarket`, `Deposit`, `Withdraw`, `GetBalances`, `GetStatement`, `GetStats`; plus bidirectional `PlaceOrders` and server-streaming `Subscribe`. Generated code lives in `crates/clob-proto`. Its build script uses `PROTOC` when set, otherwise the vendored compiler.

`Trade` carries `taker_side`, `maker_account` and `taker_account`. An account-scoped `ListTrades` fills in only the requesting account's id and leaves the counterparty blank; an unscoped listing (the harness, an operator) carries both.

| Situation | Status |
|---|---|
| Quantity or price not positive, unknown side, non-numeric id, empty account or client id, account or client id longer than 128 bytes | `INVALID_ARGUMENT` |
| Unknown order id | `NOT_FOUND` |
| Cancel of an order that is filled, cancelled or rejected | `FAILED_PRECONDITION` |
| Order not backed by the account's available USDC or ETH | `FAILED_PRECONDITION`, message in human units with what is available |
| Order belongs to another account | `PERMISSION_DENIED` |
| Same `client_order_id` with different parameters | `ALREADY_EXISTS` |
| Command queue full | `RESOURCE_EXHAUSTED` |

`ListOrders` with `status = OPEN` returns every live order (open or partially filled); `STATUS_UNSPECIFIED` returns all.

## Tests and benchmarks

| What | Where | Command |
|---|---|---|
| Unit and property tests: matching, idempotency, retention, balances, ledgers, invariants and reference-matcher agreement | `crates/engine/src/book.rs` | `cargo test -p engine` |
| Concurrency (placements, racing cancels), restart from the journal, and status-code integration tests over a real tonic server | `crates/engine-server/tests/concurrency.rs` | `cargo test -p engine-server` |
| Journal replay identity | `crates/engine/src/journal.rs` | `cargo test -p engine` |
| Pure book throughput and listing cost in a million-order book | `crates/engine/examples/bench.rs` | `cargo run --release -p engine --example bench` |
| gRPC round trip and concurrent throughput, with or without the journal | `crates/engine-server/examples/grpc_bench.rs` | `cargo run --release -p engine-server --example grpc_bench` (`ENGINE_JOURNAL=...`, `ENGINE_JOURNAL_FSYNC=1`) |

Numbers are in the README. The book benchmark deliberately includes the idempotency bookkeeping and the per-account indices; the remaining cost per operation is dominated by the `BTreeMap` walk and the hash maps, not by allocation.
