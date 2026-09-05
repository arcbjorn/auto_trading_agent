# 09 · Runbook

## Prerequisites

Rust stable 1.85 or newer (`rust-toolchain.toml` pins stable with rustfmt and clippy). No system `protoc` is needed. No Python is needed except for the optional interoperability script.

## Build and test

```
cargo build --workspace --release
cargo test --workspace                       # unit, property, concurrency, protocol, HTTP, agent-loop tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Or `make build`, `make test`, `make lint`.

## Run the stack

Three processes, three terminals (or `make run-engine`, `make run-mcp`, `make run-agent`):

```
cargo run --release -p engine-server                      # gRPC on 127.0.0.1:50051
cargo run --release -p mcp-server -- --http               # MCP on 127.0.0.1:8000/mcp
ANTHROPIC_API_KEY=sk-... cargo run --release -p agent-service   # POST /chat on 127.0.0.1:8080
```

For DeepSeek V4 instead of Claude: `MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=... cargo run --release -p agent-service` (add `DEEPSEEK_MODEL=deepseek-v4-pro` for the larger model).

Then:

```
curl -s localhost:8080/chat -H 'content-type: application/json' \
  -d '{"session_id":"me","message":"What is ETH trading at?"}'
curl -s localhost:8080/chat -H 'content-type: application/json' \
  -d '{"session_id":"me","message":"buy 0.5 ETH at 3000"}'
```

The book starts empty and unfunded. `make run-engine` funds the demo account (50,000 USDC, 10 ETH) and a market maker through `ENGINE_FUND`; without it, deposit over gRPC (`grpcurl -plaintext -proto proto/clob.proto -d '{"account_id":"demo","usdc_micro":50000000000,"eth_lots":100000}' localhost:50051 clob.v1.Engine/Deposit`; the server enables gRPC reflection, so `grpcurl -plaintext localhost:50051 list` works without the proto, and the standard health service answers `grpc.health.v1.Health/Check`). Seed liquidity by placing orders under a funded market-maker account, or run the evaluation harness, which funds and seeds for every case. Withdrawals go the same way (`clob.v1.Engine/Withdraw` with the same fields) and only ever take what is available.

## Demo conversation

`make demo` (or `cargo run --release -p evals -- demo`) starts the engine, the MCP server and the agent in one process. It funds the demo account with 50,000 USDC and 10 ETH, seeds two levels on each side, and runs an eight-turn scripted conversation through the configured model: balances and price, a resting buy, "sell now" with its confirmation, open orders, cancel all, trade history, and an injection attempt. Every turn prints the tool calls with their outcome, the reply, latency and tokens, and the engine's final state follows. It needs a model: `MODEL_PROVIDER=deepseek DEEPSEEK_API_KEY=...` or `ANTHROPIC_API_KEY=...`. A recorded transcript is in `docs/results/demo-deepseek-v4-flash.md`.

## Web demo

`make demo-web` (or `cargo run --release -p evals -- web [--addr HOST:PORT]`) starts the same stack behind a page at http://127.0.0.1:8080. The page is server-rendered HTML with htmx, served by the process itself; every panel is a fragment built from the same gRPC, MCP and HTTP calls a client would make. The chat sits in the middle with the live book, the last trades, the session and the audit log beside it, and a ticker under the header shows the market and the demo wallet. Four tabs under it, engine, MCP server, evaluation and results, hold the parts under the hood. Every panel carries a `?` that explains what it shows and which interface it reads from; `explain` in the header opens every explanation at once and is remembered, and `?explain=1` or `?theme=light` in the URL do the same for a projector.

* Engine: the live order book, trades, wallets and statement, event stream and statistics, polled once a second and refreshed at once after any action on the page. A load test places orders from N gRPC connections inside the spread, then checks that the book is not crossed and that USDC and ETH are conserved across every account. A reset cancels every order and rests the demo levels again.
* MCP: the tool list as the model receives it, a form that calls any tool and shows the JSON-RPC exchange, presets that show an accepted order, a fill and each kind of policy rejection, the policy limits, the resources and the prompt.
* Chat: through `POST /chat`, with every tool call, the gate's decision on it and the verifier's flags next to the reply; the session's state; the audit log's tail with a verify button and a tamper button.
* Give the agent a goal (experimental, beyond the brief): the simulation's market-maker bot and taker trade on the live book while the model (or the scripted baseline) pursues the goal alone, round by round; the book beside the chat moves as it happens, and the run ends with fills, average cost, P&L marked at the final mid and the rule-break count. Six buttons next to it run the hostile scripted models against the gate over the whole suite.
* Evaluation: suite runs with the oracle, null or model agent shown as a live case grid, the simulation, a perturbation preview, and every stored report under `docs/results` rendered on the page.

Chat needs a model key (`DEEPSEEK_API_KEY`, `ANTHROPIC_API_KEY`, or both; `make` loads `.env`). With both keys the chat panel offers a model dropdown, one agent-service per model behind it; `MODEL_PROVIDER` picks the default, and changing the model starts a new session on it. Everything else, the hostile-model runs included, works without a key. A model-driven suite run from the page takes minutes; the page polls its progress.

## Environment variables

| Component | Variable | Default |
|---|---|---|
| engine-server | `ENGINE_BIND` | `127.0.0.1:50051`; the engine has no authentication and trusts the account in each request, so binding it beyond loopback needs mTLS or a service identity in front of it |
| | `ENGINE_QUEUE` | `10000` |
| | `ENGINE_JOURNAL` | unset (in memory only); a path enables the write-ahead journal and replay on start |
| | `ENGINE_JOURNAL_FSYNC` | `0`; `1` fsyncs every batch before replying |
| | `ENGINE_JOURNAL_COMPACT_MB` | `64`; on start, a journal larger than this is folded into `<journal>.snapshot` and emptied; `0` never |
| | `ENGINE_BALANCES` | `1`; `0` runs without wallet checks |
| | `ENGINE_FUND` | unset; `demo:50000:10,mm:1000000:1000` credits accounts (whole USDC and ETH) on an empty book |
| any | `.env` in the repository root | git-ignored; `make` loads it, or `set -a; source .env; set +a` |
| mcp-server | `ENGINE_ADDR` | `http://127.0.0.1:50051` |
| | `ACCOUNT_ID` | `demo` |
| | `MCP_BIND` (with `--http`) | `127.0.0.1:8000` |
| | `ENGINE_TIMEOUT_MS` | `2000`, deadline for each call to the engine |
| engine-server | `ENGINE_MAX_OPEN_ORDERS`, `ENGINE_MAX_OPEN_NOTIONAL_USDC` | per-account caps enforced in the matcher; unlimited by default |
| engine-server | `ENGINE_ACCOUNT_RATE_PER_SEC` | mutations one account may send per second; unlimited by default |
| engine-server | `ENGINE_RETAIN_HOURS` | `24`, closed orders and trades older than this are archived |
| evals | `--judge` (`make eval-judged`) | score every reply with a second model call; needs a key |
| agent-service | `MAX_CONTEXT_TOKENS` | `150000`, prompt tokens a session's history may reach |
| mcp-server, agent-service | `GET /metrics` | Prometheus text: tool calls, rejections, turns, latency histogram, engine counters |
| | `POLICY_MAX_ORDER_ETH`, `POLICY_MAX_ORDER_USDC`, `POLICY_COLLAR_BPS`, `POLICY_MAX_OPEN_ORDERS`, `POLICY_ACTIONS_PER_MINUTE`, `POLICY_SESSION_CAP_USDC` | 10, 50000, 1000, 20, 10, 200000 |
| | `TRADING_HALTED` | unset |
| agent-service | see [04 Agent service](04-agent-service.md): `MODEL_PROVIDER`, `DEEPSEEK_API_KEY`, `DEEPSEEK_MODEL`, `NOTE_CHANNEL`, `PROMPT_CACHE`, `CONTEXT_EDITING`, `MAX_SESSIONS`, `SESSION_IDLE_SECS`, `TURNS_PER_MINUTE` among others | |
| all | `RUST_LOG` | `info` (`warn` for evals) |

The MCP server's stdio mode logs to stderr only; stdout is reserved for the protocol.

## Connect Claude Desktop or Claude Code

```json
{ "mcpServers": { "clob": { "command": "/abs/path/target/release/mcp-server",
                            "env": { "ENGINE_ADDR": "http://127.0.0.1:50051", "ACCOUNT_ID": "demo" } } } }
```

```
claude mcp add clob -e ENGINE_ADDR=http://127.0.0.1:50051 -e ACCOUNT_ID=demo -- /abs/path/target/release/mcp-server
```

## Interoperability check with the official MCP client

```
cargo build -p engine-server -p mcp-server
cargo run -p engine-server &                                    # or ENGINE_BIND=127.0.0.1:50051
ENGINE_ADDR=http://127.0.0.1:50051 uv run --with mcp python scripts/mcp_interop_check.py --stdio target/debug/mcp-server
cargo run -p mcp-server -- --http &
uv run --with mcp python scripts/mcp_interop_check.py --http http://127.0.0.1:8000/mcp
```

Both runs end with `INTEROP OK` (`pip install mcp` and plain `python` work too). The CI workflow's `interop` job does the same on every push. `ENGINE_ADDR=http://127.0.0.1:50051 python3 scripts/mcp_stdio_notifications_check.py target/debug/mcp-server` checks the stdio push path: it subscribes to the market summary, places an order through the tools and expects a `resources/updated` notification (no client library needed).

## Evaluation

See [06 Evaluation](06-evaluation.md). `make eval-oracle` and `make eval-null` need no key; `make eval-model` needs `ANTHROPIC_API_KEY`.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `Address family not supported` on start | The host has no IPv6; bind to an IPv4 address (`ENGINE_BIND=0.0.0.0:50051`) |
| `cannot reach engine` from mcp-server or evals | Start `engine-server` first, or fix `ENGINE_ADDR` |
| `ANTHROPIC_API_KEY is not set` | Export the key, or set `ANTHROPIC_BASE_URL` to a local mock for tests |
| `DEEPSEEK_API_KEY is not set` | `MODEL_PROVIDER=deepseek` needs the DeepSeek key; `DEEPSEEK_MODEL` picks `deepseek-v4-flash` (default) or `deepseek-v4-pro` |
| DeepSeek answers 400 mentioning `reasoning_content` | The history lost a turn's reasoning; the service replays it from the `reasoning` block, so this points at a hand-edited session or a proxy that strips fields |
| `insufficient USDC` or `insufficient ETH` from a tool | The wallet cannot back the order; `get_balances` shows what is available. Fund the account with `Deposit` (or `ENGINE_FUND` on a fresh engine) |
| `cannot recover journal ... snapshot ... balance checks` on start | The snapshot was taken with the other `ENGINE_BALANCES` setting; keep the setting the journal was written with |
| `cannot replay journal` on start | The journal file is corrupt or unreadable; the engine refuses to start on partial data. Move the file aside to start empty, or repair the bad line |
| A tool answers `RESOURCE_EXHAUSTED` | The engine's bounded queue is full under load; retry once, or raise `ENGINE_QUEUE` |
| `{"rejected": true, "code": "RATE_LIMIT"}` | More than `POLICY_ACTIONS_PER_MINUTE` actions on one account; wait a minute (`cancel_all_orders` counts as one) |
| A turn's `flags` contain `confirmation_requested:no_intent:...` | The model tried an action the user's message did not clearly ask for; it was held for confirmation before the MCP server. Expected on adversarial input and on phrasings the keyword gate does not know; the user's "yes" (or "sí", "oui", "ja") releases it. To make a phrasing execute at once, extend the verbs in `gate.rs` |
| `cache_read_input_tokens` stays 0 across turns | Something rewrites the prompt prefix. The tool list and system prompt must be byte-identical between requests; check `ANTHROPIC_MODEL` did not change mid-session |
| `403 origin not allowed` on `/mcp` | Browser-based hosts must run on localhost; the server refuses foreign origins by design |
| `429` from `/chat` | The session started more than `TURNS_PER_MINUTE` turns in the last minute; wait, or raise the limit |
| `409` from `/chat` | The session reached `MAX_TURNS`; start a new session id |
| `NOT_FOUND` for an order that did exist | It closed more than 100,000 closings ago and was archived (`RETAINED_CLOSED_ORDERS`); the journal and the account's statement still have it |
| `400 session_id must be ...` from `/chat` | Session ids are limited to 64 plain characters because they become idempotency keys the engine echoes back; use letters, digits, `.`, `_`, `-` |
| A turn's `flags` contain `note_channel_downgraded` | The model rejected a system-role message; the service switched the permission note to the user turn for this process. Set `NOTE_CHANNEL=user` to skip the first failed request |
