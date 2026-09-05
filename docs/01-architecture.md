# 01 · Architecture

## The shape of the system

The engine owns market state and matching rules. The MCP server applies risk policy, the agent service manages conversations, and the evaluation harness checks outcomes.

![The browser page and the evaluation harness drive agent-service, mcp-server and engine-server; the harness grades the engine's end state; the model may only ask, code decides](assets/architecture.svg)

* **engine** (library) is the pure order book: no I/O, no threads, no clocks.
* **engine-server** wraps it in tonic. Every handler task sends a command to the one matcher thread and awaits the reply.
* **mcp-server** speaks the Model Context Protocol, written by hand as JSON-RPC 2.0 on `serde_json`, over stdio (for Claude Desktop and Claude Code) and Streamable HTTP (for the service and the MCP Inspector). It holds one gRPC client and the deterministic risk policy.
* **agent-service** turns a sentence into tool calls with Claude or DeepSeek through provider-specific HTTPS clients, and owns the conversation-level guardrails.
* **evals** drives the whole stack in-process, seeds the book, runs a case, and grades what the engine ended up holding.

## Action checks

The model requests an action. The service checks intent and confirmation, the MCP server validates arguments and applies risk policy, and the engine checks funds, ownership and matching rules. Intent recognition uses language heuristics; numeric checks and sequencing are deterministic.

## Data flow of one order

1. The user writes "buy 0.5 ETH at 3000".
2. The service recognises trade intent and appends a permission note to the conversation.
3. The model calls `place_limit_order` with `side`, `price_usdc` and `quantity_eth` as decimal strings.
4. The gate checks the side and figures against the request. Large or unpriced orders require confirmation; a contradictory side is rejected. An allowed call gets a pre-action audit record before submission.
5. The MCP server converts the decimals to ticks and lots exactly, applies the risk policy, and calls `PlaceOrder` over gRPC with an idempotency key derived from the session and turn.
6. The engine queues the command, the matcher applies it, publishes a snapshot, and replies with the order, its fills, and the top of book afterwards.
7. The service audits the turn, checks every figure in the reply against the turn's inputs, and answers the user.

## Units

| Quantity | Wire type | Scale | Example |
|---|---|---|---|
| price | `int64 price_ticks` | 1 tick = 0.01 USDC | 3000.50 USDC = 300050 |
| quantity | `int64 quantity_lots` | 1 lot = 0.0001 ETH | 0.25 ETH = 2500 |
| notional | `u128` ticks × lots | 1 unit = 0.000001 USDC | 3601.90 USDC = 3601900000 |

Prices, quantities and matching use integers: the MCP layer converts decimal strings to integers and back with exact arithmetic (`crates/mcp-server/src/units.rs`). Rate limiting, evaluation statistics and the reply-number diagnostic also use floating-point arithmetic.

## Repository layout

```
proto/clob.proto              the gRPC contract
crates/clob-proto             generated code (build.rs runs the vendored protoc)
crates/engine                 book.rs (pure matching), sequencer.rs (single writer), examples/bench.rs
crates/engine-server          tonic servicer, status mapping, tests/concurrency.rs, examples/grpc_bench.rs
crates/mcp-server             jsonrpc.rs, protocol.rs, tools.rs (11 tools), policy.rs, units.rs, transport/{stdio,http}.rs
crates/agent-service          anthropic.rs, mcp_client.rs, gate.rs (permissions, confirmation, verifier), agent.rs, audit.rs, http.rs, prompts/system.md
crates/evals                  cases.rs, agents.rs, harness.rs, report.rs, sim.rs
evals/cases/{execution,paraphrase,safety}   scenario files
scripts/mcp_interop_check.py  drives the MCP server with the official Python client
docs/                         these pages
```
