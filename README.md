# auto_trading_agent

A local ETH/USDC trading system in Rust. Ask Claude or DeepSeek to check prices, place limit orders or cancel them, then inspect the tool calls, fills and balances in the browser.

The demo runs its own order book with simulated balances. **The model requests actions; code checks funds, order parameters and risk limits.** Intent recognition uses heuristics, and the services are intended for local use without caller authentication.

1. **Matching engine:** processes orders, settles balances and journals state, exposed over gRPC.
2. **MCP server:** gives the model eleven market and account tools with risk checks.
3. **Chat service:** manages conversations, order confirmations and audit records.
4. **Evaluation harness:** checks actual orders and trades across execution, safety and paraphrase cases.

![Architecture](docs/assets/architecture.svg)

## Quick start

Requires Rust 1.88 or newer and `make`. No system `protoc` is needed.

```sh
make demo-web
```

Open [the local demo](http://127.0.0.1:8080). It builds and starts the stack with a funded demo account and a seeded book. Explore the market, call MCP tools, run scripted evaluations and read reports without an API key.

To enable chat, copy [.env.example](.env.example) to `.env`, add `ANTHROPIC_API_KEY` or `DEEPSEEK_API_KEY`, then restart the demo. `make` loads the file automatically. Try “What is ETH trading at?”, “Buy 0.5 ETH at 3000” or “Cancel that order”.

Other useful commands:

```sh
make demo                              # eight-turn terminal conversation; needs a model key
make eval-oracle                       # check expected outcomes through MCP; no key needed
cargo test --workspace --locked        # unit, property and integration tests
```

For DeepSeek terminal demos and model evaluations, set `MODEL_PROVIDER=deepseek` in `.env`. To run separate services, use `make run-engine`, `make run-mcp` and `make run-agent` in three terminals. See the [runbook](docs/09-runbook.md) for configuration and troubleshooting.

## Where to look

* [Walkthrough](docs/10-walkthrough.md): commands and expected output for each part of the stack.
* [Results](docs/results/README.md): measurements and the reports behind them.
* [Task coverage](docs/00-coverage.md): requirements mapped to code and tests.

## Results

Recorded measurements on Apple M1 Pro, release builds, loopback. See [results](docs/results/README.md) for reports and reproduction commands; live-model scores and throughput predate the latest changes.

| Measurement | Result |
|---|---|
| Throughput | 730k to 840k book operations/s; 955k orders/s on one pipelined gRPC stream, 61k to 69k with 16 unary clients |
| Restart soak, 4 × 1M journaled orders | memory flat at about 110 MB; recovery 2.2 to 2.5 s |
| Accuracy | DeepSeek V4 Flash 57/57; Claude Sonnet 5 170/171 over three reps |
| Safety | 39/39 attacks blocked; five hostile model strategies cause 0 unauthorised mutations |
| Prompt robustness | 57/57 with every turn perturbed, on both models |
| Reply quality, judged | clarity 4.81/5, faithful 55/57; the judge found two replies the end state could not fault |
| Cost | 0.05 USD per DeepSeek run of the suite; 92 to 95% of prompt tokens from cache |

## Layout

```
proto/clob.proto        gRPC API contract
crates/engine           matching, wallets, journaling and replay
crates/engine-server    gRPC service and concurrency tests
crates/mcp-server       tools, risk policy and protocol transports
crates/agent-service    model clients, chat loop, confirmations and audit
crates/evals            evaluation harness, simulation and browser demo
evals/cases/            57 scenarios: execution, paraphrase, safety
scripts/                MCP interoperability checks, soak, docmap
docs/                   design, setup guides and recorded results
```

## Design

* **Integer accounting.** Ticks of 0.01 USDC, lots of 0.0001 ETH, notionals in `u128`; decimals convert exactly at the MCP boundary.
* **Single writer, batched.** One thread owns the book; a snapshot is published before replies go out, so a client always sees its own order.
* **Deterministic replay.** Counter-based ordering, a command journal and bounded closed history.
* **Stable prompts.** A fixed system prompt and tool list support caching; per-turn permissions are enforced when a tool is called.
* **Guardrails as code.** Risk policy in the MCP server; in the service, permission from the user's own words, confirmation, a verifier, reply grounding and a fail-closed audit chain.
* **Few dependencies.** tokio, hyper, tonic, prost, serde, reqwest, arc-swap, tracing, sha2. No MCP SDK, web framework, decimal or RNG crate.

## Documentation

[Documentation index](docs/README.md) · [Architecture](docs/01-architecture.md) · [Guardrails](docs/05-guardrails.md) · [Evaluation](docs/06-evaluation.md) · [Design decisions](docs/07-decisions.md)

## Next

Metrics dashboards and alerts on the counters now exposed; a second live model comparison at three reps once a key allows it; time-in-force post-only orders. Sharding by symbol is out of scope for a single-pair slice by design (ADR-22).
