# auto_trading_agent

A vertical slice of infrastructure for autonomous AI agents trading on a market, in Rust:

1. **A deterministic matching engine** for ETH/USDC behind gRPC (tonic). One matcher thread owns the book; handlers send commands over a bounded channel, so the engine is thread-safe without a lock on the book and every event has a total order.
2. **An MCP server** so a model can perceive the book and act on it. The Model Context Protocol layer is written by hand as JSON-RPC 2.0 on `serde_json`, over stdio (Claude Desktop, Claude Code) and Streamable HTTP, and verified against the official MCP client.
3. **A natural-language service** that runs Claude (or DeepSeek V4, selected by one variable) in a tool loop over those MCP tools, with layered guardrails: schema and unit validation, a deterministic risk policy, per-turn permission of action tools on explicit intent (enforced in code, with a tool list that never changes so the prompt stays cacheable), confirmation of large orders, idempotency keys, a post-turn verifier and an audit log.
4. **An evaluation harness** that seeds a fresh engine per case, drives the real service, and grades the engine's end state, with oracle and null agents bounding the harness from above and below, plus a market simulation.

![User or eval harness posts to agent-service, which calls mcp-server over MCP, which calls engine-server over gRPC; the harness grades the engine's end state](docs/assets/architecture.svg)

The rule that keeps the design honest: **the model may only ever request an action.** Validation, risk limits, identity and ordering are enforced in code below it, so no prompt can bend them.

## Quick start

```
cargo build --workspace --release
cargo test --workspace                                   # 69 tests: unit, property, concurrency, protocol, HTTP, agent loop
cargo run -p evals -- run --agent oracle --assert        # validates the harness without a model (exit code = verdict)
cargo run --release -p engine-server                     # gRPC on 0.0.0.0:50051
cargo run --release -p mcp-server -- --http              # MCP on 127.0.0.1:8000/mcp   (no flag = stdio)
MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... make demo          # the whole stack in one process, an eight-turn conversation
ANTHROPIC_API_KEY=... cargo run --release -p agent-service     # POST /chat on 127.0.0.1:8080
MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... cargo run --release -p agent-service   # same service on DeepSeek V4
MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... cargo run --release -p agent-service   # same service on DeepSeek V4
curl -s localhost:8080/chat -H 'content-type: application/json' -d '{"session_id":"me","message":"buy 0.5 ETH at 3000"}'
```

No system `protoc` is required: `build.rs` uses the vendored one. See the [runbook](docs/09-runbook.md) for environment variables, connecting a desktop MCP host, and troubleshooting.

## Results

Measured on an Apple M1 Pro laptop (release builds, loopback networking, three runs each, the range is reported); rerun with `make bench`, `make soak`, `make eval-oracle`, `make eval-null`, `make sim`.

| Measurement | Result |
|---|---|
| Engine, pure book (1M places + 250k cancels, random prices, 780k trades) | 730k to 840k operations/s, 1.2 to 1.4 µs per operation |
| Engine, list 10 open orders of one account with 1M orders in the book | 0.7 µs per call (was 20 ms: a full scan on the matcher thread, which stalled every other command) |
| Engine, list 10 trades of one account, same book | 0.2 µs per call (was 1 to 7 µs) |
| Engine, memory after the 1M-order benchmark, closed orders and trades archived beyond the last 100k of each | peak 406 MB (was 761 MB), 913k to 837k operations/s for the ordered per-account index |
| Engine soak (`make soak`): 4 rounds × 1M orders over gRPC with cancels, journal on, each round a fresh process recovering the last | resident 110, 108, 107 MB (was 261 to 269 MB from round 2 on: replayed events sat in the broadcast buffer until fixed); snapshot steady at 39.6 MB; recovery of the snapshot plus a 195 MB journal tail 2.2 to 2.5 s |
| Engine, pure book with wallets enforced (reserve, settle, release on every operation) | 650k to 720k operations/s: about 10% for the accounting |
| gRPC `PlaceOrder`, sequential, in-process server | p50 70 to 75 µs, p99 160 to 180 µs |
| gRPC `PlaceOrder`, 16 concurrent clients | 61k to 69k orders/s (unchanged within noise by the batched matcher; the batching buys read-your-writes, not throughput at this load) |
| Same, with the write-ahead journal (flush per batch) | p50 88 µs, 61k orders/s: about 10% for restart durability |
| Same, journal with fsync per batch | p50 4.1 ms, 2.1k orders/s: the price of surviving a power loss on a laptop disk |
| Agent service, 64 sessions × 2 turns at once against a model that answers in 40 ms | 128 turns in 185 ms, turn p50 88 ms, p95 119 ms (5.1 s if sessions waited for each other); the store holds its cap of 16 |
| Concurrency tests: 16 clients × 500 orders; 8 accounts cancelling 150 orders each from parallel tasks | every response OK, sequence numbers unique and contiguous, book never crossed; every cancel succeeds and the book ends empty |
| MCP interoperability (official Python client, stdio and HTTP, also a CI job) | all eleven tools, resources and the prompt, output schemas validated: `INTEROP OK` |
| Reply grounding check (every figure in a reply must come from an input of the turn or arithmetic on two inputs), DeepSeek V4 Flash, 57 cases | 55 replies quoted figures, 172 in all, none unsupported; the check flags a made-up fill price or balance in the unit test |
| Same 57 cases with every turn perturbed before sending (`--perturb all`: typos in ordinary words, filler and doubled spaces, random casing; numbers and intent words untouched) | execution 100% (23/23), paraphrase 100% (19/19), safety 100% (15/15, 13/13 attacks blocked); no unsupported figures in any reply |
| Evaluation harness, oracle agent, 45 cases × 3 reps | execution 100% (51/51), paraphrase 100% (45/45), safety 100% (39/39, 33/33 attacks blocked); `--assert` passes |
| Evaluation harness, null agent | execution 0%, paraphrase 0%, safety 76.9% (10/11 attacks blocked; the one that needs a clarifying question fails, as it must); `--assert` passes |
| Evaluation harness, DeepSeek V4 Flash (thinking mode, effort high), 54 cases × 3 reps, wallets enforced | execution 100% (60/60), paraphrase 100% (57/57), safety 100% (45/45, 39/39 attacks blocked, including a sell the wallet cannot cover); the three reasoning cases added afterwards (top up a holding, cancel the higher bid, sell half) 12/13 runs; Spanish, French, unlisted-verb and side-less requests complete through the confirmation flow; turn p50 3 to 5 s, p95 13 to 19 s; 92% of prompt tokens served from cache; 0.14 USD for the 162 runs |
| Same suites through the Claude Messages-API client, pointed at DeepSeek's Anthropic-compatible endpoint (`ANTHROPIC_BASE_URL`, model `deepseek-v4-flash`) | execution 100% (60/60), paraphrase 100% (57/57), safety 100% (45/45, 39/39 attacks); the Messages-API code path (system block, tool definitions, tool_use and tool_result blocks, stop reasons, usage fields) exercised against a live server without an Anthropic key; 94% cache hits; 0.13 USD |
| Same, reasoning effort low | execution 100% (51/51), paraphrase 93% (53/57: guessed "buy" once on a side-less order, stalled twice on the French confirmation), safety 100% (42/42); 0.10 USD and about the same latency, so low effort buys nothing here |
| Market simulation, DeepSeek V4 Flash, 5 seeds × 8 rounds, 10,000 USDC wallet | goal reached in 5/5 seeds, 0 rule violations, 4 to 6 tool calls per seed, P&L scored from the wallet against the deposit (the scripted baseline: 4/5, 4 to 8 calls, and it overshoots the target) |

A full demo transcript is in [demo-deepseek-v4-flash.md](docs/results/demo-deepseek-v4-flash.md): balances and price, a resting buy, a "sell now" that is held for confirmation and then cancelled by self-trade prevention against the account's own bid, which the assistant explains and offers to fix, open orders, cancel all, trade history, and an injection attempt held for confirmation. Full reports: [oracle](docs/results/report-oracle.md), [null](docs/results/report-null.md), [DeepSeek V4 Flash](docs/results/report-model-deepseek-v4-flash.md), [DeepSeek V4 Flash at low effort](docs/results/report-model-deepseek-v4-flash-low.md), [the Messages-API client via DeepSeek's Anthropic-compatible endpoint](docs/results/report-model-messages-api-deepseek.md), [simulation baseline](docs/results/sim-baseline.md), [simulation with DeepSeek V4 Flash](docs/results/sim-model-deepseek-v4-flash.md). The first model run found one real gap, which is now a code rule: an order at a price the user never stated ("sell 0.5 ETH now") and any order framed as a demo or test require a confirmation turn. Claude runs need `ANTHROPIC_API_KEY`, which was not available in the authoring environment; the Claude request shape (tool list, cache breakpoints, beta headers, permission note) is covered by the mock-model tests in `crates/agent-service/tests/agent.rs`.

## Layout

```
proto/clob.proto              the gRPC contract (integer ticks and lots, sequence numbers)
crates/clob-proto             generated code
crates/engine                 book.rs (pure matching, wallets, per-account indices, property-tested), journal.rs (write-ahead log, replay), sequencer.rs (single writer, batched)
crates/engine-server          tonic servicer, status mapping, concurrency test, gRPC benchmark
crates/mcp-server             jsonrpc.rs, protocol.rs, tools.rs (11 tools), policy.rs, units.rs, transport/{stdio,http}.rs
crates/agent-service          anthropic.rs (caching, context editing), deepseek.rs (V4 chat completions), model.rs (provider switch), mcp_client.rs, gate.rs (permissions, confirmation, verifier), agent.rs, audit.rs, http.rs, prompts/system.md
crates/evals                  cases.rs, agents.rs, harness.rs (+ CI invariants), report.rs, sim.rs
evals/cases/                  57 scenarios: execution, paraphrase, safety
scripts/mcp_interop_check.py  drives the MCP server with the official Python client
scripts/soak.sh               four restarts of a million journaled orders each; memory must level off
docs/                         architecture, engine, MCP, agent service, guardrails, evaluation, decisions, dependencies, runbook
```

## Design in one screen

* **Integers, never floats.** Price in ticks of 0.01 USDC, quantity in lots of 0.0001 ETH, notionals in `u128`. Decimal strings are converted exactly at the MCP boundary.
* **Single writer.** The book is moved into one thread; the compiler guarantees nothing else touches it. Commands drain in batches, publishing one snapshot before the replies go out, so a client always sees its own order. Reads take that snapshot lock-free. The bounded queue gives backpressure (`RESOURCE_EXHAUSTED`).
* **Nothing on the matcher thread scales with the book.** Orders and trades are indexed per account, so listing one account's open orders costs the same in a million-order book as in an empty one. Account names are interned (`Arc<str>`).
* **Deterministic.** Counters for ids and sequence numbers, clocks for reporting only; the property tests replay every generated command list and assert an identical event log, and check every trade and resting order against a naive reference matcher.
* **Idempotent.** `client_order_id` makes retries safe end to end: the engine replays the original reply, the service derives the key from session, turn and tool-call id.
* **Every order is backed.** Accounts have wallets in the engine: a buy reserves its USDC and a sell its ETH at placement, fills settle both legs, cancels release the rest, and the property test proves nothing is created or destroyed. Deposits and withdrawals are gRPC calls, never tools, so no prompt can move funds; `get_balances` shows what the account holds and `get_statement` how it has done: volume, average cost, realised P&L (average-cost basis over ETH bought here) and unrealised P&L at the current market, with the ledger's zero-sum property in the property test.
* **Push, not just poll.** The matcher broadcasts every event; `Subscribe` streams them over gRPC (per account, counterparty hidden, lag reported rather than skipped), and over stdio the MCP server turns them into `resources/updated` notifications for subscribed hosts, coalesced per 100 ms.
* **Durable by replay.** With `ENGINE_JOURNAL` set, every place, cancel and deposit is journaled before it is applied and committed once per batch before the replies go out; a restart replays the file and lands on the same ids, wallets and sequence numbers, and pre-restart idempotency keys still work. A large journal is folded into a snapshot plus tail on the next start. Fsync per batch is a flag, with its cost measured above.
* **MCP designed for the model.** Human units in and out, descriptions that say when to call, precomputed quotes and averages, typed structured output, errors that read as instructions, and three deliberate error channels (protocol, `isError`, structured policy rejection).
* **Guardrails as code.** Policy in the MCP server (size, value, a collar that always has a reference price, open orders, rate, session cap, kill switch); per-turn permission, confirmation, verifier and audit in the service. See [05 Guardrails](docs/05-guardrails.md).
* **A prompt that never rewrites itself.** The system prompt and name-sorted tool list are fixed per session and marked for caching; what a turn may do is appended after the user's message and enforced when a tool is called. The cached prefix stays valid and the history append-only, which the newest models require for the thinking blocks they bind to the conversation.
* **One loop, two providers.** The conversation is kept in Messages API blocks; the DeepSeek client translates to chat-completion messages at the edge, replays each turn's `reasoning_content` (which DeepSeek requires whenever tools are present), and maps its cache-hit accounting onto the same usage fields, so guardrails, evaluation and pricing work unchanged across providers.
* **One loop, two providers.** The conversation is kept in Messages API blocks; the DeepSeek client translates to chat-completion messages at the edge, replays each turn's `reasoning_content` (which DeepSeek requires whenever tools are present), and maps its cache-hit accounting onto the same usage fields, so guardrails, evaluation and pricing work unchanged across providers.
* **No text reaches the model that anything wrote into the book.** Session ids and client order ids are the only free-form strings that come back in tool results; both are bounded and restricted to a plain character set at every layer.
* **Battle-tested dependencies only.** tokio, hyper, tonic, prost, serde, reqwest, arc-swap, tracing; the MCP SDK, web frameworks and decimal, schema, benchmark and RNG crates were left out on purpose. See [08 Dependencies](docs/08-dependencies.md).

## Next

run the Claude suites when a key is available (the harness, pricing and the system-channel note are ready for Opus 5 and Fable 5.1); a per-account rate limit at the gRPC edge so one client cannot starve the others; stream the snapshot into the book on recovery, the last allocation spike at restart; archive by age as well as by count so a quiet venue does not hold old orders; shard by symbol; move the per-turn permission onto the mid-conversation tool-changes beta once it is stable.
