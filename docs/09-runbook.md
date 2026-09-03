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
cargo run --release -p engine-server                      # gRPC on 0.0.0.0:50051
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

The book starts empty. Seed it by placing orders under another account through gRPC (`grpcurl` works: the server does not enable reflection, so pass `-proto proto/clob.proto`), or run the evaluation harness, which seeds the book for every case.

## Environment variables

| Component | Variable | Default |
|---|---|---|
| engine-server | `ENGINE_BIND` | `0.0.0.0:50051` |
| | `ENGINE_QUEUE` | `10000` |
| mcp-server | `ENGINE_ADDR` | `http://127.0.0.1:50051` |
| | `ACCOUNT_ID` | `demo` |
| | `MCP_BIND` (with `--http`) | `127.0.0.1:8000` |
| | `POLICY_MAX_ORDER_ETH`, `POLICY_MAX_ORDER_USDC`, `POLICY_COLLAR_BPS`, `POLICY_MAX_OPEN_ORDERS`, `POLICY_ACTIONS_PER_MINUTE`, `POLICY_SESSION_CAP_USDC` | 10, 50000, 1000, 20, 10, 200000 |
| | `TRADING_HALTED` | unset |
| agent-service | see [04 Agent service](04-agent-service.md): `MODEL_PROVIDER`, `DEEPSEEK_API_KEY`, `DEEPSEEK_MODEL`, `NOTE_CHANNEL`, `PROMPT_CACHE`, `CONTEXT_EDITING`, `MAX_SESSIONS`, `SESSION_IDLE_SECS` among others | |
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

Both runs end with `INTEROP OK` (`pip install mcp` and plain `python` work too). The CI workflow's `interop` job does the same on every push.

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
| A tool answers `RESOURCE_EXHAUSTED` | The engine's bounded queue is full under load; retry once, or raise `ENGINE_QUEUE` |
| `{"rejected": true, "code": "RATE_LIMIT"}` | More than `POLICY_ACTIONS_PER_MINUTE` actions on one account; wait a minute (`cancel_all_orders` counts as one) |
| A turn's `flags` contain `tool_not_permitted:...` | The model tried an action the user's message did not ask for; the call was refused before the MCP server. Expected on adversarial input; on a benign paraphrase, extend the verbs in `gate.rs` |
| `cache_read_input_tokens` stays 0 across turns | Something rewrites the prompt prefix. The tool list and system prompt must be byte-identical between requests; check `ANTHROPIC_MODEL` did not change mid-session |
| `403 origin not allowed` on `/mcp` | Browser-based hosts must run on localhost; the server refuses foreign origins by design |
| `400 session_id must be ...` from `/chat` | Session ids are limited to 64 plain characters because they become idempotency keys the engine echoes back; use letters, digits, `.`, `_`, `-` |
| A turn's `flags` contain `note_channel_downgraded` | The model rejected a system-role message; the service switched the permission note to the user turn for this process. Set `NOTE_CHANNEL=user` to skip the first failed request |
