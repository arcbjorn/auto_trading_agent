# 02 · The engine

## In plain terms

An order book is two sorted lists. Bids (buyers) are sorted highest price first, asks (sellers) lowest price first. A new limit order first trades against the other side at any price that satisfies its limit; whatever is left waits in the book at its own price, behind orders already there. Cancel removes a waiting order. That is the whole business logic. Everything else is data structures, determinism and concurrency.

## Matching rules

| Rule | Behaviour |
|---|---|
| Price-time priority | Best price first; at the same price, first in, first out |
| Crossing | A buy at P matches asks priced at or below P; a sell at P matches bids at or above P |
| Maker price | Trades execute at the resting order's price; the taker gets the price improvement |
| GTC | The unfilled remainder rests at the order's own limit |
| IOC | The unfilled remainder is cancelled |
| FOK | Availability is checked first; the order is rejected without touching the book if the full quantity is not there |
| Self-trade prevention | "Cancel newest": an incoming order never trades with its own account's resting order; matching stops there and the remainder is cancelled |
| Idempotency | `client_order_id` is unique per account. The same request again returns the original reply (order and fills); the same key with different parameters is `ALREADY_EXISTS` |

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
    by_client_id: HashMap<(String, String), OrderId>,  // idempotency
    original_replies: HashMap<OrderId, (Order, Vec<Trade>)>,
    events: Vec<Event>,                                // append-only history
    last_trade_price: Option<Price>,
    next_order: u64, next_trade: u64, next_seq: u64,   // counters, never clocks or UUIDs
}
struct Level { total: Qty, queue: VecDeque<OrderId> }  // FIFO of ids = time priority
```

Each level holds a FIFO of order ids, so an order lives in exactly one place. Cancel is O(1): mark the order, subtract its remaining quantity from the level total, drop the level when the total reaches zero; the matcher skips cancelled ids lazily when it reaches them. Order and trade listings are derived from the event vector.

## Determinism

"Deterministic" means the same input sequence always produces the same output, which makes the engine replayable and the evaluations reproducible.

* One thread applies commands in arrival order, and that order is the sequence number.
* Order ids, trade ids and sequence numbers are counters.
* Wall-clock timestamps are passed in by the caller (`now_ns`), recorded for reporting, and never used for ordering.

The property test in `book.rs` generates random sequences of places (GTC, IOC, FOK) and cancels across four accounts and asserts after every step: the book is never crossed, displayed depth equals the open quantity, every trade is at the maker's price and within the taker's limit, no self-trade occurs, sequence numbers are unique, and a replay of the same sequence yields an identical event log.

## Thread safety and concurrency: the single writer

tonic runs each RPC as a tokio task, so many `PlaceOrder` calls arrive at once. Instead of `Arc<Mutex<Book>>`, the book is *moved* into one matcher thread:

```mermaid
flowchart LR
    H1[tonic handler task 1] -- "try_send(cmd, reply)" --> Q
    H2[tonic handler task 2] --> Q
    H3[tonic handler task 3] --> Q
    Q[bounded mpsc channel<br/>arrival order = sequence order] -- "blocking_recv()" --> M[matcher thread<br/>owns the Book]
    M -- "mutates" --> B[(Book)]
    M -- "publishes Arc&lt;Snapshot&gt;" --> S[ArcSwap snapshot<br/>readers load() it, no lock]
    M -. "oneshot reply" .-> H1
```

* Handlers hold an `EngineHandle`: a channel sender and a pointer to the latest snapshot. Nothing else can reach the book; the compiler enforces it.
* Contention is one channel send. The book itself has no lock and no `unsafe`.
* Reads of the top of book (`GetOrderBook`, `GetMarket`) never enter the queue: the matcher publishes a fresh snapshot after every command and readers swap to it lock-free.
* The channel is bounded (10,000 by default). When it is full, `try_send` fails and the handler answers `RESOURCE_EXHAUSTED` instead of growing memory.
* Unlike an interpreter with a global lock, the matcher thread and the tokio worker threads run on different cores, so encoding, networking and matching overlap.

`crates/engine-server/tests/concurrency.rs` runs a real tonic server with sixteen concurrent clients placing 500 orders each. Every response succeeds, every event's sequence number is unique and contiguous, and the book is never crossed.

## gRPC contract

`proto/clob.proto` defines seven unary RPCs: `PlaceOrder`, `CancelOrder`, `GetOrder`, `ListOrders`, `ListTrades`, `GetOrderBook`, `GetMarket`. The generated code lives in `crates/clob-proto`; `build.rs` runs the real `protoc` (a system one when `PROTOC` is set, otherwise the binary vendored by `protoc-bin-vendored`), so a fresh checkout builds with nothing but cargo.

| Situation | Status |
|---|---|
| Quantity or price not positive, unknown side, non-numeric id, empty account or client id | `INVALID_ARGUMENT` |
| Unknown order id | `NOT_FOUND` |
| Cancel of an order that is filled, cancelled or rejected | `FAILED_PRECONDITION` |
| Order belongs to another account | `PERMISSION_DENIED` |
| Same `client_order_id` with different parameters | `ALREADY_EXISTS` |
| Command queue full | `RESOURCE_EXHAUSTED` |

`ListOrders` with `status = OPEN` returns every live order (open or partially filled); `STATUS_UNSPECIFIED` returns all.

## Tests and benchmarks

| What | Where | Command |
|---|---|---|
| 10 unit tests (rules, cancel, idempotency, IOC/FOK, self-trade, listings) and 1 property test | `crates/engine/src/book.rs` | `cargo test -p engine` |
| Concurrency and status-code integration tests over a real tonic server | `crates/engine-server/tests/concurrency.rs` | `cargo test -p engine-server` |
| Pure book throughput | `crates/engine/examples/bench.rs` | `cargo run --release -p engine --example bench` |
| gRPC round trip and concurrent throughput | `crates/engine-server/examples/grpc_bench.rs` | `cargo run --release -p engine-server --example grpc_bench` |

Numbers from the machine the code was written on are in the README. The book benchmark deliberately includes the allocation-heavy parts (string account and client ids, idempotency bookkeeping); a build that interns account ids would be several times faster, and the README says where the time goes.
