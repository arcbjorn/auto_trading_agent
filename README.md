# auto_trading_agent

A vertical slice of infrastructure for autonomous AI agents trading on a market, in Rust:

1. **A deterministic matching engine** for ETH/USDC behind gRPC (tonic). One matcher thread owns the book; handlers send commands over a bounded channel, so the engine is thread-safe without a lock on the book and every event has a total order.
2. **An MCP server** so a model can perceive the book and act on it. The Model Context Protocol layer is written by hand as JSON-RPC 2.0 on `serde_json`, over stdio (Claude Desktop, Claude Code) and Streamable HTTP, and verified against the official MCP client.
3. **A natural-language service** that runs Claude in a tool loop over those MCP tools through the Messages API, with layered guardrails: schema and unit validation, a deterministic risk policy, per-turn permission of action tools on explicit intent (enforced in code, with a tool list that never changes so the prompt stays cacheable), confirmation of large orders, idempotency keys, a post-turn verifier and an audit log.
4. **An evaluation harness** that seeds a fresh engine per case, drives the real service, and grades the engine's end state, with oracle and null agents bounding the harness from above and below, plus a market simulation.

![User or eval harness posts to agent-service, which calls mcp-server over MCP, which calls engine-server over gRPC; the harness grades the engine's end state](docs/assets/architecture.svg)

The rule that keeps the design honest: **the model may only ever request an action.** Validation, risk limits, identity and ordering are enforced in code below it, so no prompt can bend them.

## Quick start

```
cargo build --workspace --release
cargo test --workspace                                   # 47 tests: unit, property, concurrency, protocol, HTTP, agent loop
cargo run -p evals -- run --agent oracle --assert        # validates the harness without a model (exit code = verdict)
cargo run --release -p engine-server                     # gRPC on 0.0.0.0:50051
cargo run --release -p mcp-server -- --http              # MCP on 127.0.0.1:8000/mcp   (no flag = stdio)
ANTHROPIC_API_KEY=... cargo run --release -p agent-service     # POST /chat on 127.0.0.1:8080
curl -s localhost:8080/chat -H 'content-type: application/json' -d '{"session_id":"me","message":"buy 0.5 ETH at 3000"}'
```

No system `protoc` is required: `build.rs` uses the vendored one. See the [runbook](docs/09-runbook.md) for environment variables, connecting a desktop MCP host, and troubleshooting.

## Results

Measured on an Apple M1 Pro laptop (release builds, loopback networking, three runs each, the range is reported); rerun with `make bench`, `make eval-oracle`, `make eval-null`, `make sim`.

| Measurement | Result |
|---|---|
| Engine, pure book (1M places + 250k cancels, random prices, 780k trades) | 730k to 840k operations/s, 1.2 to 1.4 µs per operation |
| Engine, list 10 open orders of one account with 1M orders in the book | 0.7 µs per call (was 20 ms: a full scan on the matcher thread, which stalled every other command) |
| Engine, list 10 trades of one account, same book | 0.2 µs per call (was 1 to 7 µs) |
| gRPC `PlaceOrder`, sequential, in-process server | p50 70 to 75 µs, p99 160 to 180 µs |
| gRPC `PlaceOrder`, 16 concurrent clients | 61k to 69k orders/s (unchanged within noise by the batched matcher; the batching buys read-your-writes, not throughput at this load) |
| Concurrency tests: 16 clients × 500 orders; 8 accounts cancelling 150 orders each from parallel tasks | every response OK, sequence numbers unique and contiguous, book never crossed; every cancel succeeds and the book ends empty |
| MCP interoperability (official Python client, stdio and HTTP, also a CI job) | all nine tools, resources and the prompt, output schemas validated: `INTEROP OK` |
| Evaluation harness, oracle agent, 45 cases × 3 reps | execution 100% (51/51), paraphrase 100% (45/45), safety 100% (39/39, 33/33 attacks blocked); `--assert` passes |
| Evaluation harness, null agent | execution 0%, paraphrase 0%, safety 76.9% (10/11 attacks blocked; the one that needs a clarifying question fails, as it must); `--assert` passes |

Full reports: [oracle](docs/results/report-oracle.md), [null](docs/results/report-null.md), [simulation baseline](docs/results/sim-baseline.md). Model-driven runs (`--agent model`) need `ANTHROPIC_API_KEY` and were not possible in the authoring environment; the harness, the agent loop and the exact API request shape (tool list, cache breakpoints, beta headers, permission note) are covered by the mock-model tests in `crates/agent-service/tests/agent.rs`.

## Layout

```
proto/clob.proto              the gRPC contract (integer ticks and lots, sequence numbers)
crates/clob-proto             generated code
crates/engine                 book.rs (pure matching, per-account indices, property-tested), sequencer.rs (single writer, batched)
crates/engine-server          tonic servicer, status mapping, concurrency test, gRPC benchmark
crates/mcp-server             jsonrpc.rs, protocol.rs, tools.rs (9 tools), policy.rs, units.rs, transport/{stdio,http}.rs
crates/agent-service          anthropic.rs (caching, context editing), mcp_client.rs, gate.rs (permissions, confirmation, verifier), agent.rs, audit.rs, http.rs, prompts/system.md
crates/evals                  cases.rs, agents.rs, harness.rs (+ CI invariants), report.rs, sim.rs
evals/cases/                  45 scenarios: execution, paraphrase, safety
scripts/mcp_interop_check.py  drives the MCP server with the official Python client
docs/                         architecture, engine, MCP, agent service, guardrails, evaluation, decisions, dependencies, runbook
```

## Design in one screen

* **Integers, never floats.** Price in ticks of 0.01 USDC, quantity in lots of 0.0001 ETH, notionals in `u128`. Decimal strings are converted exactly at the MCP boundary.
* **Single writer.** The book is moved into one thread; the compiler guarantees nothing else touches it. Commands drain in batches, publishing one snapshot before the replies go out, so a client always sees its own order. Reads take that snapshot lock-free. The bounded queue gives backpressure (`RESOURCE_EXHAUSTED`).
* **Nothing on the matcher thread scales with the book.** Orders and trades are indexed per account, so listing one account's open orders costs the same in a million-order book as in an empty one. Account names are interned (`Arc<str>`).
* **Deterministic.** Counters for ids and sequence numbers, clocks for reporting only; the property test replays every generated command list and asserts an identical event log.
* **Idempotent.** `client_order_id` makes retries safe end to end: the engine replays the original reply, the service derives the key from session, turn and tool-call id.
* **MCP designed for the model.** Human units in and out, descriptions that say when to call, precomputed quotes and averages, typed structured output, errors that read as instructions, and three deliberate error channels (protocol, `isError`, structured policy rejection).
* **Guardrails as code.** Policy in the MCP server (size, value, a collar that always has a reference price, open orders, rate, session cap, kill switch); per-turn permission, confirmation, verifier and audit in the service. See [05 Guardrails](docs/05-guardrails.md).
* **A prompt that never rewrites itself.** The system prompt and name-sorted tool list are fixed per session and marked for caching; what a turn may do is appended after the user's message and enforced when a tool is called. The cached prefix stays valid and the history append-only, which the newest models require for the thinking blocks they bind to the conversation.
* **No text reaches the model that anything wrote into the book.** Session ids and client order ids are the only free-form strings that come back in tool results; both are bounded and restricted to a plain character set at every layer.
* **Battle-tested dependencies only.** tokio, hyper, tonic, prost, serde, reqwest, arc-swap, tracing; the MCP SDK, web frameworks and decimal, schema, benchmark and RNG crates were left out on purpose. See [08 Dependencies](docs/08-dependencies.md).

## Next

Persist the command log so the engine recovers by replay; stream book deltas instead of polling; shard by symbol; run the model-driven suites at several effort levels and publish the numbers with the cache hit rate; move the per-turn permission onto the mid-conversation tool-changes beta; add balances and settlement so the simulation scores realised P&L.
