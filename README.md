# auto_trading_agent

Infrastructure for autonomous AI agents trading on a market, in Rust. Four parts, one rule: **the model may only request an action.** Validation, risk limits, funds, identity and ordering are enforced in code beneath it.

1. **Matching engine.** A deterministic ETH/USDC limit order book behind gRPC. One thread owns the book, commands arrive over a bounded channel, every event carries a sequence number. Wallets, a write-ahead journal with snapshot compaction, and an event stream.
2. **MCP server.** Eleven tools, resources and a prompt over stdio and Streamable HTTP, verified against the official MCP client in CI. Human units in and out, a deterministic risk policy, errors that read as instructions.
3. **Natural-language service.** Claude or DeepSeek V4 in a tool loop over the MCP tools. Permission per turn from the user's own words, confirmation of large or unpriced orders, a post-turn verifier, reply grounding, a hash-chained audit log that fails closed.
4. **Evaluation harness.** A fresh engine per case, the real service, grading on the engine's end state. Oracle, null and hostile agents bound the harness; every turn can be perturbed; a market simulation scores P&L from the wallet.

![Architecture](docs/assets/architecture.svg)

## Quick start

```
cargo build --workspace --release
cargo test --workspace                          # 77 tests
cp .env.example .env                            # add ANTHROPIC_API_KEY or DEEPSEEK_API_KEY; make loads it
make demo                                       # whole stack in one process, eight turns
make run-engine  /  make run-mcp  /  make run-agent      # the three services, then POST /chat on :8080
```

No system `protoc` is needed. Environment variables and troubleshooting are in the [runbook](docs/09-runbook.md).

## Where to look

* [Task coverage](docs/00-coverage.md): each requirement of the task, the code that implements it, the test that proves it.
* [Walkthrough](docs/10-walkthrough.md): every part running, with the output to expect.
* [Results](docs/results/README.md): all measurements and the reports behind them.

## Results

Apple M1 Pro, release builds, loopback. Full table and every report: [docs/results](docs/results/README.md).

| Measurement | Result |
|---|---|
| Book throughput | 730k to 840k operations/s; 61k to 69k orders/s over gRPC with 16 clients |
| Restart soak, 4 × 1M journaled orders | memory flat at about 110 MB; recovery 2.2 to 2.5 s |
| Accuracy | DeepSeek V4 Flash 57/57; Claude Sonnet 5 170/171 over three reps |
| Safety | 39/39 attacks blocked; a hostile model causes 0 unauthorised mutations |
| Prompt robustness | 57/57 with every turn perturbed, on both models |
| Cost | 0.05 USD per DeepSeek run of the suite; 92 to 95% of prompt tokens from cache |

## Layout

```
proto/clob.proto        gRPC contract: integer ticks and lots, sequence numbers
crates/engine           book.rs, journal.rs, sequencer.rs
crates/engine-server    tonic service, concurrency tests, gRPC benchmark, soak
crates/mcp-server       jsonrpc.rs, protocol.rs, tools.rs, policy.rs, units.rs, transport/
crates/agent-service    anthropic.rs, deepseek.rs, gate.rs, agent.rs, audit.rs, http.rs, prompts/
crates/evals            cases.rs, agents.rs, harness.rs, perturb.rs, report.rs, sim.rs, demo.rs
evals/cases/            57 scenarios: execution, paraphrase, safety
scripts/                MCP interoperability checks, soak, docmap
docs/                   architecture, engine, MCP, service, guardrails, evaluation, decisions, runbook, walkthrough
```

## Design

* **Integers only.** Ticks of 0.01 USDC, lots of 0.0001 ETH, notionals in `u128`; decimals convert exactly at the MCP boundary.
* **Single writer, batched.** One thread owns the book; a snapshot is published before replies go out, so a client always sees its own order.
* **Deterministic, durable, bounded.** Counters, not clocks; the journal replays to the same state; closed history is archived past fixed counts.
* **A prompt that never rewrites itself.** Fixed system prompt and tool list, cached; the turn's permissions travel as a note and are enforced when a tool is called.
* **Guardrails as code.** Risk policy in the MCP server; in the service, permission from the user's own words, confirmation, a verifier, reply grounding and a fail-closed audit chain.
* **Few dependencies.** tokio, hyper, tonic, prost, serde, reqwest, arc-swap, tracing, sha2. No MCP SDK, web framework, decimal or RNG crate.

## Documentation

[00 Task coverage](docs/00-coverage.md) · [01 Architecture](docs/01-architecture.md) · [02 Engine](docs/02-engine.md) · [03 MCP server](docs/03-mcp-server.md) · [04 Agent service](docs/04-agent-service.md) · [05 Guardrails](docs/05-guardrails.md) · [06 Evaluation](docs/06-evaluation.md) · [07 Decisions](docs/07-decisions.md) · [08 Dependencies](docs/08-dependencies.md) · [09 Runbook](docs/09-runbook.md) · [10 Walkthrough](docs/10-walkthrough.md)

## Next

A confirmation-burden metric per model; more hostile strategies for the unsafe runner; exposure limits inside the matcher; a client-streaming placement RPC; a token budget per session; metrics endpoints.
