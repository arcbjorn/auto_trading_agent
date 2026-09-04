# auto_trading_agent

Infrastructure for autonomous AI agents trading on a market, in Rust. Four parts, one rule: **the model may only request an action.** Validation, risk limits, funds, identity and ordering are enforced in code beneath it.

1. **Matching engine.** A deterministic ETH/USDC central limit order book behind gRPC (tonic). One thread owns the book, commands arrive over a bounded channel, every event carries a sequence number. Wallets, a write-ahead journal with snapshot compaction, and an event stream are part of the engine.
2. **MCP server.** Eleven tools, resources and a prompt, written by hand as JSON-RPC 2.0 over stdio and Streamable HTTP, verified against the official MCP client in CI. Human units in and out, a deterministic risk policy, errors that read as instructions.
3. **Natural-language service.** Claude or DeepSeek V4 (one variable) in a tool loop over the MCP tools, with per-turn permission of action tools on explicit intent, confirmation of large or unpriced orders, idempotency keys, a post-turn verifier that also checks every figure in a reply against the turn's inputs, and an audit log.
4. **Evaluation harness.** A fresh engine per case, the real service, grading on the engine's end state. Oracle and null agents bound the harness from above and below; prompts can be perturbed for robustness; a market simulation scores P&L from the wallet.

![Architecture](docs/assets/architecture.svg)

## Quick start

```
cargo build --workspace --release
cargo test --workspace                                  # 69 tests: unit, property, concurrency, protocol, HTTP, agent loop
cargo run -p evals -- run --agent oracle --assert       # validates the harness without a model
cargo run --release -p engine-server                    # gRPC on 0.0.0.0:50051
cargo run --release -p mcp-server -- --http             # MCP on 127.0.0.1:8000/mcp (no flag: stdio)
ANTHROPIC_API_KEY=... cargo run --release -p agent-service                          # POST /chat on 127.0.0.1:8080
MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... cargo run --release -p agent-service   # same service on DeepSeek V4
MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... make demo                              # whole stack in one process, eight turns
curl -s localhost:8080/chat -H 'content-type: application/json' -d '{"session_id":"me","message":"buy 0.5 ETH at 3000"}'
```

No system `protoc` is needed. Environment variables, desktop MCP hosts and troubleshooting are in the [runbook](docs/09-runbook.md).

## Results

Apple M1 Pro, release builds, loopback. Reproduce with `make bench`, `make soak`, `make eval-oracle`, `make eval-null`, `make eval-model`, `make eval-perturbed`, `make sim`.

| Measurement | Result |
|---|---|
| Book, 1M places and 250k cancels, 780k trades | 730k to 840k operations/s; 650k to 720k with wallets enforced |
| List 10 orders or 10 trades of one account, 1M-order book | 0.7 µs and 0.2 µs per call (indexed per account) |
| Memory, same benchmark, closed history archived beyond the last 100k orders and trades | peak 406 MB, was 761 MB |
| Soak: 4 restarts, 1M journaled orders each with cancels | resident 110, 108, 107 MB; snapshot steady at 39.6 MB; recovery 2.2 to 2.5 s |
| gRPC `PlaceOrder`, sequential | p50 70 to 75 µs, p99 160 to 180 µs |
| gRPC `PlaceOrder`, 16 concurrent clients | 61k to 69k orders/s; 61k with the journal; 2.1k with fsync per batch |
| Agent service, 64 sessions × 2 turns at once, 40 ms mock model | 128 turns in 185 ms; sessions do not wait for each other |
| MCP interoperability, official Python client, stdio and HTTP | all tools, resources and the prompt; output schemas validated |
| Harness bounds, 3 reps | oracle 100% on every suite; null 0% execution, 0% paraphrase, 10/11 attacks blocked |
| DeepSeek V4 Flash, 54 cases × 3 reps, wallets enforced | execution 60/60, paraphrase 57/57, safety 45/45 with 39/39 attacks blocked; turn p50 3 to 5 s; 92% cache hits; 0.14 USD |
| Same suites via the Claude Messages-API client on DeepSeek's compatible endpoint | 162/162; the Claude request path exercised live |
| Reasoning cases added later (top up a holding, cancel the higher bid, sell half) | 16/17 runs |
| All 57 cases with every turn perturbed (typos, filler, casing) | 57/57 |
| Tool calls per turn with the post-action book in results, DeepSeek V4 Flash, 57 cases | execution 2.00 to 1.83, paraphrase 2.00 to 1.47, safety unchanged; 57/57 |
| Reply grounding, 57 cases | 172 figures quoted, none without a source in the turn's inputs |
| Market simulation, 5 seeds × 8 rounds | goal reached 5/5, no rule violations; scripted baseline 4/5 |

Reports and a full demo transcript are under [docs/results](docs/results). Claude runs are pending an Anthropic key; the harness, pricing and request shape are ready (`make eval-model`), and the Claude request path is covered by the mock-model tests and the live run above.

## Layout

```
proto/clob.proto              gRPC contract: integer ticks and lots, sequence numbers
crates/engine                 book.rs (matching, wallets, indices, retention), journal.rs, sequencer.rs
crates/engine-server          tonic service, status mapping, concurrency tests, gRPC benchmark, soak
crates/mcp-server             jsonrpc.rs, protocol.rs, tools.rs, policy.rs, units.rs, transport/{stdio,http}.rs
crates/agent-service          anthropic.rs, deepseek.rs, model.rs, gate.rs, agent.rs, http.rs, prompts/system.md
crates/evals                  cases.rs, agents.rs, harness.rs, perturb.rs, report.rs, sim.rs, demo.rs
evals/cases/                  57 scenarios: execution, paraphrase, safety
scripts/                      MCP interoperability checks (official client), soak
docs/                         architecture, engine, MCP, agent service, guardrails, evaluation, decisions, dependencies, runbook
```

## Design

* **Integers only.** Prices in ticks of 0.01 USDC, quantities in lots of 0.0001 ETH, notionals in `u128`; decimals convert exactly at the MCP boundary.
* **Single writer, batched.** The book lives on one thread. Commands drain in batches and a snapshot is published before replies go out, so a client always sees its own order; reads are lock-free; the bounded queue gives backpressure.
* **Bounded memory.** Events, closed orders and trades are retained up to fixed counts and archived beyond them; live orders, wallets and ledgers are never touched. Sessions in the service are capped by count, idle time, turns per minute and total turns.
* **Deterministic and durable.** Ids and sequence numbers are counters; property tests replay every generated command list to an identical event log, check every trade against a naive reference matcher, and audit the book's structure after every operation. Hard caps on price and size hold whatever the layers above do. The journal is committed once per batch before replies; a restart replays it, or a snapshot plus tail, onto the same state.
* **Idempotent end to end.** `client_order_id` replays the original reply; the service derives it from session, turn and tool call; `request_id` makes `POST /chat` retries safe.
* **Funds in the engine.** A buy reserves USDC and a sell reserves ETH at placement; fills settle, cancels release; deposits and withdrawals are gRPC calls, never tools. Statements report volume, average cost and realised P&L.
* **A prompt that never rewrites itself.** The system prompt and name-sorted tool list are fixed and cached; the turn's permissions travel as a note after the user's message and are enforced when a tool is called.
* **Two providers, one loop.** History is kept as Messages API blocks; the DeepSeek client translates at the edge, replays reasoning content, and maps cache accounting onto the same usage fields.
* **The book travels with the action.** Placement and cancel results carry the best bid and ask afterwards, so the model reports the market without another call: tool calls per turn fell from 2.00 to 1.83 and 1.47 on the execution and paraphrase suites.
* **Guardrails as code.** Policy in the MCP server (size, value, collar, open orders, rate, session cap, kill switch); permission, confirmation, verifier, reply grounding and audit in the service. Details in [05 Guardrails](docs/05-guardrails.md).
* **Few dependencies.** tokio, hyper, tonic, prost, serde, reqwest, arc-swap, tracing. No MCP SDK, web framework, decimal or RNG crate. Rationale in [08 Dependencies](docs/08-dependencies.md).

## Documentation

[01 Architecture](docs/01-architecture.md) · [02 Engine](docs/02-engine.md) · [03 MCP server](docs/03-mcp-server.md) · [04 Agent service](docs/04-agent-service.md) · [05 Guardrails](docs/05-guardrails.md) · [06 Evaluation](docs/06-evaluation.md) · [07 Decisions](docs/07-decisions.md) · [08 Dependencies](docs/08-dependencies.md) · [09 Runbook](docs/09-runbook.md)

## Next

Run the Claude suites once a key is available. Then: a per-account rate limit at the gRPC edge, streaming the snapshot on recovery, archiving by age as well as by count, sharding by symbol.
