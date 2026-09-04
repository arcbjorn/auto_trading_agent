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
cargo test --workspace                                  # 77 tests: unit, property, concurrency, protocol, HTTP, agent loop
cp .env.example .env                                    # then put ANTHROPIC_API_KEY or DEEPSEEK_API_KEY in it; make loads it
cargo run -p evals -- run --agent oracle --assert       # validates the harness without a model
cargo run --release -p engine-server                    # gRPC on 0.0.0.0:50051
cargo run --release -p mcp-server -- --http             # MCP on 127.0.0.1:8000/mcp (no flag: stdio)
ANTHROPIC_API_KEY=... cargo run --release -p agent-service                          # POST /chat on 127.0.0.1:8080
MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... cargo run --release -p agent-service   # same service on DeepSeek V4
MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... make demo                              # whole stack in one process, eight turns
curl -s localhost:8080/chat -H 'content-type: application/json' -d '{"session_id":"me","message":"buy 0.5 ETH at 3000"}'
```

No system `protoc` is needed. [10 Walkthrough](docs/10-walkthrough.md) runs every part end to end with the output to expect; environment variables, desktop MCP hosts and troubleshooting are in the [runbook](docs/09-runbook.md).

## Where each requirement lives

The task's four parts, in its own words. Each row links the code that does it, the test that proves it, and the number that measures it. Line anchors are rewritten by `make docmap` and checked in CI, so they stay right.

**1. The Engine (gRPC)**

| Requirement | What we built | Code | Proof | Measured |
|---|---|---|---|---|
| Support Spot Limit Buy and Sell orders | Price-time priority, fills at the maker's price, GTC, IOC and FOK, self-trade prevention, wallets that back every order | [book.rs::Book::place](crates/engine/src/book.rs#L948-L1200) | [book.rs::matches_the_naive_reference](crates/engine/src/book.rs#L1987-L2023), [book.rs::worked_example_trades_at_maker_price_best_first](crates/engine/src/book.rs#L1362-L1378) | 730k to 840k operations/s in the pure book |
| Support Order cancellations | O(1) cancel: the level total drops now, the matcher skips the order lazily | [book.rs::Book::cancel](crates/engine/src/book.rs#L1204-L1245) | [concurrency.rs::cancels_race_placements_and_leave_the_book_empty](crates/engine-server/tests/concurrency.rs#L117-L188) | 8 accounts cancelling 150 orders each from parallel tasks, book ends empty |
| Expose the engine strictly via gRPC | Twelve RPCs in one proto, integer ticks and lots, sequence numbers on everything; no other interface | [proto/clob.proto](proto/clob.proto), [lib.rs::Svc::place_order](crates/engine-server/src/lib.rs#L280-L306) | [concurrency.rs::idempotent_retry_and_status_codes](crates/engine-server/tests/concurrency.rs#L503-L595) | p50 70 to 75 µs per call |
| The engine must be thread-safe | One thread owns the book; handlers send commands over a bounded channel, reads take a lock-free snapshot | [sequencer.rs::spawn_with_journal](crates/engine/src/sequencer.rs#L204-L247) | [concurrency.rs::sixteen_tasks_place_orders_concurrently](crates/engine-server/tests/concurrency.rs#L23-L110) | 16 clients, sequence numbers unique and contiguous |
| Efficiently handle concurrent order placement | Commands drain in batches, one snapshot published per batch before the replies | [sequencer.rs::apply](crates/engine/src/sequencer.rs#L110-L180) | [grpc_bench.rs](crates/engine-server/examples/grpc_bench.rs), [soak.rs](crates/engine-server/examples/soak.rs) | 61k to 69k orders/s over gRPC; memory flat across restarts |
| Deterministic | Counters, not clocks; a journal of commands replays to the identical book | [journal.rs::Journal::recover](crates/engine/src/journal.rs#L92-L126) | [journal.rs::replay_rebuilds_the_same_book_and_continues_its_counters](crates/engine/src/journal.rs#L209-L346), [book.rs::invariants_hold_and_replay_is_identical](crates/engine/src/book.rs#L2028-L2068) | 1M orders recover in 2.2 to 2.5 s |

**2. The Bridge (MCP)**

| Requirement | What we built | Code | Proof | Measured |
|---|---|---|---|---|
| Perceive market state | Summary, depth, quote, balances, statement, listings; resources with subscriptions over stdio | [tools.rs::ToolSet::market](crates/mcp-server/src/tools.rs#L463-L470), [tools.rs::ToolSet::quote](crates/mcp-server/src/tools.rs#L527-L571), [stdio.rs::pump](crates/mcp-server/src/transport/stdio.rs#L31-L65) | [protocol.rs::tools_against_a_real_engine](crates/mcp-server/tests/protocol.rs#L217-L586) | official client: `INTEROP OK` on stdio and HTTP |
| Act on the market | Place and cancel behind a deterministic risk policy: size, value, collar, open orders, rate, session cap, kill switch | [tools.rs::ToolSet::place](crates/mcp-server/src/tools.rs#L587-L679), [policy.rs::Policy::check_place](crates/mcp-server/src/policy.rs#L143-L159) | [policy.rs::rejects_size_value_collar_and_open_orders](crates/mcp-server/src/policy.rs#L277-L298) | every rejection a structured code and hint |
| How to structure MCP Tools | Eleven tools; descriptions say when to call; closed input schemas; typed output schemas | [tools.rs::ToolSet::definitions](crates/mcp-server/src/tools.rs#L259-L409) | [protocol.rs::lifecycle_and_discovery](crates/mcp-server/tests/protocol.rs#L64-L214) | catalog checked exactly at service startup |
| How to structure MCP Resources | Market spec, book, open orders, statement; `resources/subscribe` pushes updates from the engine's event stream | [protocol.rs::McpServer::resources_read](crates/mcp-server/src/protocol.rs#L203-L209) | [scripts/mcp_stdio_notifications_check.py](scripts/mcp_stdio_notifications_check.py) | notifications coalesced per 100 ms |
| How to optimize for LLM reasoning | Human units in and out, arithmetic done server side, errors written as instructions, the book after every action | [units.rs::parse_price](crates/mcp-server/src/units.rs#L71-L73), [tools.rs::grpc_error](crates/mcp-server/src/tools.rs#L212-L226), [tools.rs::add_top](crates/mcp-server/src/tools.rs#L142-L147) | [units.rs::rejects_bad_inputs_with_actionable_messages](crates/mcp-server/src/units.rs#L138-L147) | tool calls per turn 2.00 to 1.47 after the post-action book |
| How to minimize context window usage | A tool list that never changes so the prompt caches; bounded listings and fills; compact results | [anthropic.rs::AnthropicClient::request_body](crates/agent-service/src/anthropic.rs#L155-L178), [tools.rs::MAX_FILLS](crates/mcp-server/src/tools.rs#L55-L55) | [agent.rs::context_editing_is_requested_server_side_when_enabled](crates/agent-service/tests/agent.rs#L706-L745) | 92 to 95% of prompt tokens served from cache |

**3. LLM Interaction Service**

| Requirement | What we built | Code | Proof | Measured |
|---|---|---|---|---|
| Execute a trade | A real tool loop: the model reads, decides, acts; the service decides at call time what may execute | [agent.rs::Agent::chat_turn](crates/agent-service/src/agent.rs#L293-L569) | [agent.rs::explicit_buy_places_an_order_with_an_idempotency_key](crates/agent-service/tests/agent.rs#L263-L288) | execution suite 100% on both models |
| Return pricing for a trading pair | Read tools permitted on every turn; action tools only on explicit intent | [gate.rs::Permissions::for_turn](crates/agent-service/src/gate.rs#L234-L246) | [agent.rs::read_only_question_permits_no_action_tools](crates/agent-service/tests/agent.rs#L203-L260) | turn p50 3 to 6 s |
| Retrieve order history | Orders, trades and a statement with realised and unrealised P&L | [tools.rs::ToolSet::statement](crates/mcp-server/src/tools.rs#L737-L779) | [book.rs::ledger_tracks_average_cost_realised_pnl_and_withdrawals](crates/engine/src/book.rs#L1478-L1536) | statement case: a 0.60 USDC loss reported exactly |
| Bonus: validation | Exact decimal parsing, bounded ids, closed schemas, hard caps in the engine itself | [units.rs::parse_qty](crates/mcp-server/src/units.rs#L75-L77), [book.rs::MAX_PRICE](crates/engine/src/book.rs#L34-L34) | [book.rs::hard_caps_reject_absurd_orders_before_anything_else](crates/engine/src/book.rs#L1539-L1553) | |
| Bonus: risk checks | Policy in the MCP server, wallets in the engine, confirmation of large or unpriced orders | [gate.rs::ConfirmationGate::intercept](crates/agent-service/src/gate.rs#L310-L449) | [agent.rs::large_order_needs_confirmation_then_executes](crates/agent-service/tests/agent.rs#L291-L349) | safety suite 100%, 39/39 attacks blocked |
| Bonus: prompt protections | Permission from the user's own words, two stated figures pin the order, a post-turn verifier, reply grounding, a hash-chained audit that fails closed | [gate.rs::verify](crates/agent-service/src/gate.rs#L477-L503), [gate.rs::unsupported_numbers](crates/agent-service/src/gate.rs#L512-L547), [audit.rs::Audit::append](crates/agent-service/src/audit.rs#L86-L98) | [agent.rs::unrequested_action_becomes_a_confirmation_request](crates/agent-service/tests/agent.rs#L352-L375), [agent.rs::an_action_is_refused_when_its_audit_record_cannot_be_written](crates/agent-service/tests/agent.rs#L748-L777) | hostile model: 0 unauthorised mutations in 26 runs |

**4. LLM Evaluation**

| Dimension | What we built | Code | Proof | Measured |
|---|---|---|---|---|
| Trade execution accuracy | A fresh engine per case, the real service, grading on the engine's end state; oracle and null agents bound the harness | [harness.rs::grade](crates/evals/src/harness.rs#L214-L266), [agents.rs::Driver](crates/evals/src/agents.rs#L27-L42) | [harness.rs::check_invariants](crates/evals/src/harness.rs#L373-L413) in CI | DeepSeek 57/57; Sonnet 5 170/171 |
| Latency | Turn, model and gRPC hop timed separately; tokens and list-price cost from the API's usage | [report.rs::render](crates/evals/src/report.rs#L58-L195) | report per run | p50 3 to 6 s per turn, 70 µs per engine call |
| Safety, guardrail effectiveness | Fifteen attack cases, plus a hostile scripted model that tries to trade on every turn | [model.rs::UnsafeModel](crates/agent-service/src/model.rs#L21-L21) | `make eval-unsafe` in CI | 39/39 attacks blocked; hostile 0/26; it found 3 leaks first |
| Prompt robustness | Nineteen paraphrases in five languages, and every turn perturbable: typos, filler, casing | [perturb.rs::apply](crates/evals/src/perturb.rs#L36-L49) | `make eval-perturbed` | 57/57 perturbed on both models |
| Simulation-based testing | A seeded bot moves the book while the agent pursues a goal; P&L scored from the wallet | [sim.rs::run](crates/evals/src/sim.rs#L167-L295) | `make sim` | goal reached 5/5 seeds, 0 rule violations |

## Results

Apple M1 Pro, release builds, loopback. Reproduce with `make bench`, `make soak`, `make eval-oracle`, `make eval-null`, `make eval-unsafe`, `make eval-model`, `make eval-perturbed`, `make sim`. Full reports and a demo transcript are under [docs/results](docs/results).

**Engine and protocol**

| Measurement | Result |
|---|---|
| Book, 1M places and 250k cancels, 780k trades | 730k to 840k operations/s; 650k to 720k with wallets enforced |
| List 10 orders or 10 trades of one account, 1M-order book | 0.7 µs and 0.2 µs per call |
| Memory, same benchmark, closed history archived beyond the last 100k orders and trades | peak 406 MB, was 761 MB |
| Soak: 4 restarts, 1M journaled orders each with cancels, fresh process per round | resident 110, 108, 107 MB; snapshot steady at 39.6 MB; recovery 2.2 to 2.5 s |
| gRPC `PlaceOrder`, sequential | p50 70 to 75 µs, p99 160 to 180 µs (health and reflection enabled) |
| gRPC `PlaceOrder`, 16 concurrent clients | 61k to 69k orders/s; 61k with the journal; 2.1k with fsync per batch |
| Agent service, 64 sessions × 2 turns at once, 40 ms mock model | 128 turns in 185 ms |
| MCP interoperability, official Python client, stdio and HTTP | all tools, resources, prompt; output schemas validated |

**Trade execution accuracy**

| Measurement | Result |
|---|---|
| Harness bounds, 3 reps | oracle 100% on every suite; null 0% execution, 0% paraphrase |
| DeepSeek V4 Flash, 57 cases, final gate | 57/57; earlier 54 cases × 3 reps 162/162 |
| Claude Sonnet 5, 57 cases × 3 reps | execution 98.6% (68/69), paraphrase 100%, safety 100%; the first Sonnet run scored 91% on execution and exposed a gate gap, now fixed |
| Reasoning cases (top up a holding, cancel the higher bid, sell half) | 16/17 runs on DeepSeek, 3/3 each on Sonnet after the fix |
| Post-action book in results | tool calls per turn 2.00 to 1.83 and 1.47 on DeepSeek; 1.26 to 1.62 on Sonnet |

**Latency and cost**

| Measurement | Result |
|---|---|
| DeepSeek V4 Flash, thinking on | turn p50 3 to 5 s, p95 13 to 19 s; 92% cache hits; 0.05 USD for 57 runs |
| Claude Sonnet 5 | turn p50 4.7 to 5.7 s, p95 10 to 13 s; 95% cache hits; 0.94 USD for 171 runs |
| Reasoning effort low (DeepSeek) | same latency, 93% paraphrase: low effort buys nothing here |

**Safety and guardrail effectiveness**

| Measurement | Result |
|---|---|
| Safety suite, both models | 100%; 39/39 attacks blocked, including a sell the wallet cannot cover |
| Hostile model against the gate (`make eval-unsafe`) | 0 unauthorised mutations in the 26 runs that asked for no order or cancel; its first run found 3, which led to the two-figure rule |
| Unauthorised mutations across every live run | 0 |
| Reply grounding, 57 cases | 172 figures quoted, none without a source in the turn's inputs |

**Prompt robustness**

| Measurement | Result |
|---|---|
| Paraphrase suite, 19 cases in five languages | 100% on both models |
| Every turn perturbed (typos, filler, casing), 57 cases | 57/57 on DeepSeek, 57/57 on Sonnet |

**Simulation-based testing**

| Measurement | Result |
|---|---|
| Market simulation, DeepSeek V4 Flash, 5 seeds × 8 rounds | goal reached 5/5, no rule violations; scripted baseline 4/5 |

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
* **Guardrails as code.** Policy in the MCP server (size, value, collar, open orders, rate, session cap, kill switch); permission from the user's own words in six languages, confirmation, a rule that two stated figures pin the order, a confirmation in words that carries the previous request, verifier and reply grounding in the service. Every call to the engine has a deadline; the service refuses to start against a server whose tool catalog differs from the eleven it expects. Details in [05 Guardrails](docs/05-guardrails.md).
* **An audit log that fails closed.** Hash-chained JSON lines, verified at startup; a pre-action record is flushed before every action tool call, and the call is refused if it cannot be written. A hostile scripted model runs the whole suite in CI and must cause no unauthorised mutation.
* **Few dependencies.** tokio, hyper, tonic, prost, serde, reqwest, arc-swap, tracing. No MCP SDK, web framework, decimal or RNG crate. Rationale in [08 Dependencies](docs/08-dependencies.md).

## Documentation

[01 Architecture](docs/01-architecture.md) · [02 Engine](docs/02-engine.md) · [03 MCP server](docs/03-mcp-server.md) · [04 Agent service](docs/04-agent-service.md) · [05 Guardrails](docs/05-guardrails.md) · [06 Evaluation](docs/06-evaluation.md) · [07 Decisions](docs/07-decisions.md) · [08 Dependencies](docs/08-dependencies.md) · [09 Runbook](docs/09-runbook.md) · [10 Walkthrough](docs/10-walkthrough.md)

## Next

A confirmation-burden metric (how often a legitimate order needed a confirmation turn, per model); more hostile strategies for the unsafe runner (cancel everything, replay a token, ask first then swap); exposure limits inside the matcher; a client-streaming placement RPC for throughput; a token budget per session; metrics endpoints; a per-account rate limit at the gRPC edge; streaming the snapshot on recovery; archiving by age as well as by count; sharding by symbol.
