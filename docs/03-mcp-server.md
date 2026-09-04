# 03 · The MCP server

MCP is how a host asks a server "what can you do?" and calls those things over JSON-RPC 2.0.
This server is a thin translator: one gRPC client, eleven tools, three resources, one prompt. The
design work is ergonomics for the model — say just enough, in its units, with the arithmetic
already done, and make every error fixable in one retry.

## Why the protocol layer is written by hand

MCP is two years old. No SDK for it, in any language, has the production history of tokio, hyper or serde, and the official Rust SDK still changes its API between minor versions. The subset this server needs is small: nine methods plus notifications. Writing it on `serde_json` keeps the dependency surface to crates that have been in production for years and makes the protocol behaviour fully visible in `crates/mcp-server/src/protocol.rs`. Conformance is checked against the official Python client (see *Interoperability* below).

## Protocol subset

| Method | Behaviour |
|---|---|
| `initialize` | Negotiates the version: echoes the client's version when it is one of `2025-11-25`, `2025-06-18`, `2025-03-26`, otherwise answers with the latest. Advertises `tools`, `resources` and `prompts` capabilities and the standing instructions |
| `notifications/initialized`, other notifications | Consumed, no reply |
| `ping` | `{}` |
| `tools/list` | The nine tool definitions with input schema, output schema and annotations |
| `tools/call` | Runs a tool. Unknown tool: JSON-RPC `-32602`. Invalid arguments and engine errors: a readable `isError` result |
| `resources/list`, `resources/templates/list`, `resources/read` | Three JSON resources and one template; unknown URI: `-32002` |
| `resources/subscribe`, `resources/unsubscribe` | Known URIs only; the server then sends `notifications/resources/updated` when the engine changes. Over stdio only: the stateless HTTP transport refuses a subscription with an explanation, because it has no stream to deliver on |
| `prompts/list`, `prompts/get` | The `trading_assistant` prompt |
| `logging/setLevel` | Accepted as a no-op |
| anything else | `-32601` |

Envelope rules follow JSON-RPC 2.0: `jsonrpc` must be `"2.0"`, an `id` must be a string or number and never `null` (a message without an id is a notification), `params` must be an object or array. Malformed JSON answers `-32700` with a `null` id. Legacy batches (arrays) are processed element by element.

## Transports

**stdio.** One JSON message per line on stdin and stdout. Nothing else is ever written to stdout; logs go to stderr. This is what Claude Desktop and Claude Code expect (`mcp-server` with no flag).

**Streamable HTTP, stateless.** `POST /mcp` with a request answers `application/json`; a notification answers `202 Accepted`. `GET /mcp` answers `405` because the server never opens a server-to-client stream, which the specification allows. No session id is issued, so clients never send one. The `Origin` header, when present, must be a loopback origin (DNS-rebinding protection for a server meant to run locally); an `MCP-Protocol-Version` header, when present, must be a supported version. Bodies are capped at 1 MiB. `GET /healthz` answers `ok`. (`mcp-server --http`, default `127.0.0.1:8000`.)

## The tools

| Tool | Arguments | Returns | Annotations |
|---|---|---|---|
| `get_market_summary` | none | best bid and ask, mid, spread, last trade, tick and lot size, sequence. About 70 tokens | read-only, idempotent |
| `get_order_book` | `depth` 1–20, default 5 | aggregated price levels per side, best first, plus spread, mid, sequence | read-only, idempotent |
| `get_quote` | `side`, `quantity_eth` | walks up to 200 levels: fillable quantity, whether fully fillable, average price, worst price, notional, levels consumed | read-only, idempotent |
| `place_limit_order` | `side`, `price_usdc`, `quantity_eth`, optional `client_order_id` | order id, status, filled and remaining quantity, average fill price, fills; or `{rejected: true, code, message, hint}` from the policy | not read-only, non-destructive, idempotent with a client id |
| `get_balances` | none | available and reserved USDC and ETH of this account in human units, and whether the engine enforces balances. Deposits are an operator action over gRPC, deliberately not a tool | read-only, idempotent |
| `get_statement` | none | how the account has done: deposits and withdrawals, ETH bought and sold with USDC paid and received, venue inventory at its average cost, realised P&L, and unrealised P&L at the current reference price (mid, else last trade, else the quoted side), all in human units with a note on what realised means | read-only, idempotent |
| `get_order` | `order_id` | one of this account's orders with its current status and remaining quantity, whatever its age; another account's id is denied | read-only, idempotent |
| `cancel_order` | `order_id` | final status and cancelled quantity | destructive, idempotent |
| `cancel_all_orders` | none | every open order of the account cancelled in one call: count, the cancelled orders, and any that were already gone. One policy action, however many orders, so "cancel everything" cannot trip the rate limit | destructive, idempotent |
| `list_orders` | `status` open / filled / cancelled / all (default open), `limit` 1–50 | compact rows, newest first | read-only |
| `list_trades` | `limit` 1–50 | compact rows, newest first, each with `side` (what this account did) and `role` (maker or taker) and the account's own order id; counterparty ids are never shown | read-only |

Design rules applied throughout:

* **Human units in, human units out.** Prices and quantities are decimal strings (`"3000.50"`, `"0.2500"`) converted with exact integer arithmetic. JSON numbers are accepted too, rendered with their shortest exact representation. The model never sees `3000.0000000001`.
* **Descriptions say when to call, not only what.** For example: "Call before placing an order above 1 ETH, and whenever the user asks what something would cost."
* **Constrained schemas.** Enums for side and status, minimum and maximum for depth and limit, `additionalProperties: false`. The schema steers the model; the code enforces it anyway.
* **The arithmetic is done here.** Quotes, notionals, average prices and the side of each trade are computed here so the model never sums seven price levels or works out which of two order ids was its own.
* **Errors are instructions.** `price_usdc "3000.123" has more than 2 decimals; it must be a multiple of 0.01. Round it and retry`, or `NotFound: order 99 not found. Check the id with list_orders.`
* **Identity is bound server-side.** The account comes from `ACCOUNT_ID`, never from a tool argument, so the model cannot act on another account's orders.
* **A cancelled order says why.** `place_limit_order` and the listings carry `cancel_reason` (`user`, `ioc`, `fok`, `self_trade_prevention`), and a placement whose remainder was cancelled by the engine comes with a one-sentence `note` the model can relay, for example that the order would have traded against the account's own resting order and what to do about it. The demo conversation found this gap: a confirmed "sell now" came back cancelled and neither the tool result nor the model could explain it.
* **A refused order says what is available.** An order the wallet cannot back comes back as a readable error with the needed and available amounts and a hint to report the balance, not to retry.
* **Ids are not a text channel.** A `client_order_id` comes back in every listing the model reads, so it is limited to 128 characters of letters, digits, `.`, `_`, `:` and `-`; anything else is refused with a readable error. The engine enforces the length again.
* **The collar always has a reference.** The fat-finger check measures the limit price against the midpoint when both sides of the book exist, else the last trade, else the one quoted side, so a one-sided or freshly traded-through book does not switch the check off. Only an empty market with no trade yet has no reference.
* **The session cap counts what the engine took.** The per-session value cap is charged before the gRPC call and released again if the engine rejects the order, so a retry after a transient failure is not double-counted.
* **Typed output.** Every tool declares an `outputSchema` and returns `structuredContent` plus the same JSON as text, as the specification recommends.

## Three error channels, used deliberately

| Channel | When | What the model sees |
|---|---|---|
| JSON-RPC error (`-32602`, `-32601`, `-32002`) | Unknown tool, method or resource: a programming error in the host | A protocol error; hosts show it as a bare failure |
| Result with `isError: true` | Invalid arguments, engine status errors, engine unreachable | The message as text, and most hosts prompt the model to retry with a fix |
| Result with `{rejected: true, code, message, hint}` and `isError: false` | A risk-policy decision | A definitive answer to relay to the user, not something to retry |

## Resources and prompt

A tool the model does not need on a turn still costs context: the eleven definitions are about a thousand tokens on the Messages API side. The chat service keeps that cost to one cache write per session by never changing the list (see [04](04-agent-service.md)).

Resources are application-controlled: a host may attach them to context without the model asking. `market://ETH-USDC/summary`, `market://ETH-USDC/book` (5 levels), the template `market://ETH-USDC/book/{depth}` and `orders://me/open`, all `application/json`. The `trading_assistant` prompt carries the unit rules and the "only trade on explicit instruction" rule for hosts that support prompts. Hosts use tools far more reliably than resources, so the tools are self-sufficient.

Over stdio the server also pushes. A pump subscribes to the engine's event stream (`Subscribe` over gRPC) and, whenever events arrive, sends one `notifications/resources/updated` per subscribed resource, coalesced over 100 ms so a burst of fills is one update rather than hundreds. The notification carries no data; the host re-reads the resource. The pump reconnects after a dropped stream or a lag, since nothing is lost that a re-read would not recover. `scripts/mcp_stdio_notifications_check.py` drives this end to end with raw JSON-RPC: subscribe, place an order through the tools, expect the notification.

## Interoperability

`scripts/mcp_interop_check.py` drives the server with the official Python MCP client (`pip install mcp`) over both transports: initialize, `tools/list`, calls to every tool including an invalid one, `resources/list`, `resources/read` and `prompts/get`. The Python client validates every structured result against the advertised output schema, so a passing run also checks the schemas. The Rust integration tests in `crates/mcp-server/tests/protocol.rs` cover the same ground with raw JSON-RPC messages, plus the HTTP rules (405 on GET, 403 on a foreign origin, 400 on an unknown protocol version, 202 for notifications, 413 for oversized bodies), `cancel_all_orders`, `get_order`, client id validation, trade sides and roles, and the collar's reference fallback on a one-sided book. The CI workflow runs the Python interoperability check on every push, over both transports.

## Connecting a desktop host

Claude Desktop (`claude_desktop_config.json`) or Claude Code (`claude mcp add`) run the stdio binary:

```json
{
  "mcpServers": {
    "clob": {
      "command": "/path/to/target/release/mcp-server",
      "env": { "ENGINE_ADDR": "http://127.0.0.1:50051", "ACCOUNT_ID": "demo" }
    }
  }
}
```

The MCP Inspector (`npx @modelcontextprotocol/inspector`) can connect to `http://127.0.0.1:8000/mcp` when the server runs with `--http`.
