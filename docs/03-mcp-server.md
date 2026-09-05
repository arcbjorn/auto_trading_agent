# 03 · The MCP server

The MCP server exposes eleven tools, three resources and one prompt over JSON-RPC 2.0. It translates human units to gRPC, computes market summaries and applies risk policy before submitting actions.

## Why the protocol layer is written by hand

The protocol subset is small enough to implement directly on `serde_json` and `hyper`, keeping dependencies limited and dispatch visible in `crates/mcp-server/src/protocol.rs`. The tradeoff is maintaining schemas and protocol behavior ourselves. CI checks interoperability with the official Python client.

## Protocol subset

| Method | Behaviour |
|---|---|
| `initialize` | Negotiates the version: echoes the client's version when it is one of `2025-11-25`, `2025-06-18`, `2025-03-26`, otherwise answers with the latest. Advertises `tools`, `resources` and `prompts` capabilities and the standing instructions |
| `notifications/initialized`, other notifications | Consumed, no reply |
| `ping` | `{}` |
| `tools/list` | Eleven tool definitions with input schema, output schema and annotations |
| `tools/call` | Runs a tool. Unknown tool: JSON-RPC `-32602`. Invalid arguments and engine errors: a readable `isError` result |
| `resources/list`, `resources/templates/list`, `resources/read` | Three JSON resources and one template; unknown URI: `-32002` |
| `resources/subscribe`, `resources/unsubscribe` | Known URIs only; the server then sends `notifications/resources/updated` when the engine changes. Over stdio only: the stateless HTTP transport refuses a subscription with an explanation, because it has no stream to deliver on |
| `prompts/list`, `prompts/get` | The `trading_assistant` prompt |
| `logging/setLevel` | Accepted as a no-op |
| anything else | `-32601` |

Envelope rules follow JSON-RPC 2.0: `jsonrpc` must be `"2.0"`, an `id` must be a string or number and never `null` (a message without an id is a notification), `params` must be an object or array. Malformed JSON answers `-32700` with a `null` id. Legacy batches (arrays) are processed element by element.

## Transports

**stdio.** One JSON message per line on stdin and stdout. Nothing else is ever written to stdout; logs go to stderr. This is what Claude Desktop and Claude Code expect (`mcp-server` with no flag).

**Streamable HTTP, stateless.** `POST /mcp` returns `application/json` for requests and `202 Accepted` for notifications. `GET /mcp` returns `405`; there is no server-to-client stream or session id. Host and optional Origin headers must be loopback; a supplied `MCP-Protocol-Version` must be supported. Bodies are capped at 1 MiB. `GET /healthz` answers `ok`. Run with `mcp-server --http` (default `127.0.0.1:8000`); see the [local trust boundary](09-runbook.md#local-trust-boundary).

## The tools

| Tool | Arguments | Returns | Annotations |
|---|---|---|---|
| `get_market_summary` | none | best bid and ask, mid, spread, last trade, tick and lot size, sequence. About 70 tokens | read-only, idempotent |
| `get_order_book` | `depth` 1–20, default 5 | aggregated price levels per side, best first, plus spread, mid, sequence | read-only, idempotent |
| `get_quote` | `side`, `quantity_eth` | walks up to 200 levels: fillable quantity, whether fully fillable, average price, worst price, notional, levels consumed | read-only, idempotent |
| `place_limit_order` | `side`, `price_usdc`, `quantity_eth`, optional `client_order_id` | order id, status, filled and remaining quantity, average fill price, fills; or `{rejected: true, code, message, hint}` from the policy | destructive, not idempotent (client id is optional) |
| `get_balances` | none | available and reserved USDC and ETH of this account in human units, and whether the engine enforces balances. Deposits are an operator action over gRPC, deliberately not a tool | read-only, idempotent |
| `get_statement` | none | deposits, withdrawals, bought/sold totals, venue inventory and average cost, realised P&L and unrealised P&L at the current reference price; all in human units | read-only, idempotent |
| `get_order` | `order_id` | a retained order's status and remaining quantity; another account's id is denied | read-only, idempotent |
| `cancel_order` | `order_id` | final status and cancelled quantity | destructive, idempotent |
| `cancel_all_orders` | none | atomically cancels the account's open orders; returns the count and cancelled orders. Counts as one policy action | destructive, idempotent |
| `list_orders` | `status` open / filled / cancelled / all (default open), `limit` 1–50 | compact rows, newest first | read-only |
| `list_trades` | `limit` 1–50 | compact rows, newest first, each with `side` (what this account did) and `role` (maker or taker) and the account's own order id; counterparty ids are never shown | read-only |

Design rules applied throughout:

* **Human units in, human units out.** Prices and quantities are decimal strings (`"3000.50"`, `"0.2500"`) converted with exact integer arithmetic. JSON numbers are accepted too, rendered with their shortest exact representation. The model never sees `3000.0000000001`.
* **Descriptions say when to call, not only what.** For example: "Call before placing an order above 1 ETH, and whenever the user asks what something would cost."
* **Constrained schemas.** Enums for side and status, minimum and maximum for depth and limit, `additionalProperties: false`. The schema steers the model; the code enforces it anyway.
* **The arithmetic is done here.** Quotes, notionals, average prices and the side of each trade are computed here so the model never sums seven price levels or works out which of two order ids was its own.
* **Errors are instructions.** `price_usdc "3000.123" has more than 2 decimals; it must be a multiple of 0.01. Round it and retry`, or `NotFound: order 99 not found. Check the id with list_orders.`
* **Identity is bound server-side.** The account comes from `ACCOUNT_ID`, never from a tool argument, so the model cannot act on another account's orders.
* **Cancellation reasons.** Placements and listings carry `cancel_reason` (`user`, `ioc`, `fok`, `self_trade_prevention`, `exposure_limit`), with an explanatory `note` when the engine cancels a remainder.
* **A refused order says what is available.** An order the wallet cannot back comes back as a readable error with the needed and available amounts and a hint to report the balance, not to retry.
* **Ids are not a text channel.** A `client_order_id` comes back in every listing the model reads, so it is limited to 128 characters of letters, digits, `.`, `_`, `:` and `-`; anything else is refused with a readable error. The engine enforces the length again.
* **The collar always has a reference.** The fat-finger check measures the limit price against the midpoint when both sides of the book exist, else the last trade, else the one quoted side, so a one-sided or freshly traded-through book does not switch the check off. Only an empty market with no trade yet has no reference.
* **Submission accounting.** Placements serialize policy checks and submission within the MCP process. Submitted notional is charged before gRPC, refunded on definitive rejection and retained on ambiguous transport failures. Matching client ids deduplicate placements while the engine retains them; MCP's policy-free retry lookup covers the latest 200 orders.
* **The book after the action comes with the result.** `place_limit_order`, `cancel_order` and `cancel_all_orders` return the best bid and ask as they stood right after the command. The engine attaches them to its own reply, from the same batch, so they are exact rather than a second read. The system prompt tells the model to report the market from them.
* **Bounded results.** A placement lists at most 50 fills and says so with `fills_truncated`; the totals above the list always cover every fill. Every call to the engine carries a deadline (`ENGINE_TIMEOUT_MS`, 2 s), so a stalled engine is a tool error the model can report, not a hung turn.
* **Metrics.** `GET /metrics` on the HTTP transport renders tool calls by tool and outcome, policy rejections by code and HTTP outcomes in the Prometheus text format, and fetches the engine's counters over `GetStats` on each scrape.
* **Typed output.** Every tool declares an `outputSchema` and returns `structuredContent` plus the same JSON as text, as the specification recommends.

## Three error channels, used deliberately

| Channel | When | What the model sees |
|---|---|---|
| JSON-RPC error (`-32602`, `-32601`, `-32002`) | Unknown tool, method or resource: a programming error in the host | A protocol error; hosts show it as a bare failure |
| Result with `isError: true` | Invalid arguments, engine status errors, engine unreachable | The message as text, and most hosts prompt the model to retry with a fix |
| Result with `{rejected: true, code, message, hint}` and `isError: false` | A risk-policy decision | A definitive answer to relay to the user, not something to retry |

## Resources and prompt

The eleven tool definitions occupy about a thousand tokens. The chat service keeps the list stable for prompt caching (see [04](04-agent-service.md)).

Resources are application-controlled: a host may attach them to context without the model asking. `market://ETH-USDC/summary`, `market://ETH-USDC/book` (5 levels), the template `market://ETH-USDC/book/{depth}` and `orders://me/open`, all `application/json`. The `trading_assistant` prompt carries the unit rules and the "only trade on explicit instruction" rule for hosts that support prompts. Hosts use tools far more reliably than resources, so the tools are self-sufficient.

Over stdio the server also pushes. A pump subscribes to the engine's event stream (`Subscribe` over gRPC) and, whenever events arrive, sends one `notifications/resources/updated` per subscribed resource, coalesced over 100 ms so a burst of fills is one update rather than hundreds. The notification carries no data; the host re-reads the resource. The pump reconnects after a dropped stream or a lag, since nothing is lost that a re-read would not recover. `scripts/mcp_stdio_notifications_check.py` drives this end to end with raw JSON-RPC: subscribe, place an order through the tools, expect the notification.

## Interoperability

`scripts/mcp_interop_check.py` drives the server with the official Python MCP client, over both transports, and checks the whole surface: the eleven tools with their input and output schemas, the resources and their templates, the prompt, and a placement round trip.

It runs in CI against a freshly built server, so a change that breaks a real client fails the build rather than the demo. `scripts/mcp_stdio_notifications_check.py` covers the subscription path, which HTTP cannot carry.

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
