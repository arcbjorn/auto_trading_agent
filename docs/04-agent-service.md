# 04 · The agent service

`POST /chat` runs a model in a loop (Claude by default, DeepSeek V4 with `MODEL_PROVIDER=deepseek`): it reads the message, decides which tools to call, the
service runs them against the MCP server and feeds the results back, and the loop ends when
Claude answers in text. The service owns the conversation-level guardrails: per-turn tool
permission, confirmation of large orders, idempotency keys, a post-turn verifier, an audit log.

## Calling the model

There is no official Anthropic SDK for Rust, so `crates/agent-service/src/anthropic.rs` talks to the Messages API directly with `reqwest` and `serde_json`: one endpoint, one JSON body.

| Request field | Value | Why |
|---|---|---|
| `model` | `claude-opus-5` (env `ANTHROPIC_MODEL`) | the current default model |
| `max_tokens` | 16000 (env `MAX_TOKENS`) | thinking tokens count against it; a low cap truncates answers |
| `system` | `crates/agent-service/src/prompts/system.md`, as one text block with `cache_control: {type: "ephemeral"}` | units, the "only on explicit instruction" rule, style; the breakpoint caches the tool list and the prompt together (tools render first) |
| `tools` | the MCP tool definitions, mapped field for field, sorted by name, identical on every request of a session | an MCP tool has exactly `name`, `description`, `inputSchema`; a stable list is the cache prefix |
| `cache_control` (top level) | `{type: "ephemeral"}` | automatic caching of the growing conversation, so each call reads everything before the last turn from cache |
| `output_config.effort` | `medium` (env `EFFORT`) | the latency lever; thinking itself is adaptive by default |
| `fallbacks` | `"default"` with header `anthropic-beta: server-side-fallback-2026-07-01` | a safety refusal is routed to a fallback model inside the same call |
| `context_management` | `{edits: [{type: "clear_tool_uses_20250919"}]}` with header `context-management-2025-06-27`, only with `CONTEXT_EDITING=1` | the API clears old tool results itself; the history the service holds stays append-only |

Headers: `x-api-key`, `anthropic-version: 2023-06-01`, and one `anthropic-beta` header listing the enabled betas. Transient failures (429, 529, 5xx, connection errors) are retried up to three times with jittered exponential backoff. `temperature` is not sent: current Opus models reject sampling parameters, which is why the evaluation harness measures variance with repeated runs instead.

Usage is recorded per turn as uncached input, cache reads, cache writes and output tokens, so the evaluation report can show the cache hit rate and price the run correctly.

## DeepSeek V4 as a second provider

`crates/agent-service/src/deepseek.rs` speaks DeepSeek's native chat-completions API (`POST https://api.deepseek.com/chat/completions`, bearer authentication) for `deepseek-v4-flash` (default) and `deepseek-v4-pro`: 1M context, up to 384K output, tool calls in thinking mode. `crates/agent-service/src/model.rs` is the switch; everything above it sees one `create` call and one `Message` type.

| Concern | How it is handled |
|---|---|
| Conversation format | The service keeps Messages API blocks as its only internal format. Going out: system and user text become `system`/`user` messages, each `tool_result` block becomes its own `tool` message with the `tool_call_id`, the per-turn note becomes a `user` text (the note channel is `user` for DeepSeek), `tool_use` blocks become `tool_calls` with the input serialised as the JSON string the API expects. Coming back: `reasoning_content`, `content` and `tool_calls` become `reasoning`, `text` and `tool_use` blocks; arguments are parsed from the JSON string, anything unparsable becomes `{}` so the tool layer reports the missing fields |
| Thinking mode | `thinking: {type: "enabled"}` plus top-level `reasoning_effort` (`low`, `high`, `max`; the service's `EFFORT` maps low to low, medium and high to high, xhigh and max to max). DeepSeek requires the `reasoning_content` of every earlier assistant turn to be sent back whenever the request carries tools, and answers 400 otherwise; the `reasoning` block in the history is exactly that, replayed on every request |
| Stop reasons | `tool_calls` to `tool_use`, `stop` to `end_turn`, `length` to `max_tokens`, `content_filter` to `refusal`; `insufficient_system_resource` is retried with backoff like a 5xx |
| Usage and cost | `prompt_cache_miss_tokens` is the uncached input, `prompt_cache_hit_tokens` the cache read (DeepSeek caches prefixes automatically, no markers), `completion_tokens` the output; the evaluation report prices DeepSeek runs at its peak-hour list rates |
| Not sent | `cache_control`, `fallbacks`, `context_management`, `output_config` and system-role messages mid-conversation are Claude features; the DeepSeek request carries none of them |

DeepSeek also offers an Anthropic-compatible endpoint (`https://api.deepseek.com/anthropic`, mapping `claude-opus-*` to V4-Pro and the others to V4-Flash). Pointing `ANTHROPIC_BASE_URL` there works for a quick look, but it ignores `cache_control` and rejects the beta features the Claude path uses, so the native client is the supported route.

Environment for the DeepSeek provider: `DEEPSEEK_API_KEY`, `DEEPSEEK_MODEL` (default `deepseek-v4-flash`), `DEEPSEEK_BASE_URL`, `DEEPSEEK_THINKING` (default `1`), `DEEPSEEK_REASONING_EFFORT` (overrides the `EFFORT` mapping), plus the shared `EFFORT`, `MAX_TOKENS` and `MODEL_TIMEOUT_SECS`.

## Why the tool list never changes

Two facts about the current API decide this. Prompt caching is a prefix match over `tools`, then `system`, then `messages`: a tool list that differs between turns invalidates everything, every turn. And the newest models bind each thinking block to the exact conversation prefix that produced it; rebuilding `tools` or `system` between requests of one conversation invalidates those blocks (an error on organisations created after August 2026). So the service sends the same system prompt and the same ten tools on every request of a session, and expresses "what this turn may do" inside `messages`, plus enforcement in code when a tool is called. The history is only ever appended to.

The note itself travels on the operator channel where the model has one: Claude Opus 5, Opus 4.8 and the Fable and Mythos models accept a `{"role": "system", ...}` message appended after the user's message, which the model cannot mistake for user text and which user text cannot forge. Other models get the same note as a second text block inside the user message (`[service] This turn permits: ...`). The channel is chosen from the model name (`NOTE_CHANNEL=system|user` overrides it), and if the API answers a system-role message with a 400 the service re-sends that turn on the user channel and stays there for the rest of the process. Either way the permission is enforced in code, so a forged note can only mislead the model, never the engine. The alternative of declaring action tools with `defer_loading` and surfacing them with `tool_addition` blocks is the cache-preserving form of per-turn tool lists; it is a beta and was left for later.

## The loop

```
permissions = trade? cancel?   from the user's own words (or a confirmation of a pending order)
push user message, then the note "[service] This turn permits: place orders = yes/no, cancel orders = yes/no ..."
    (a role: system message, or a second text block in the user message)
loop (at most 8 iterations):
    msg = POST /v1/messages   (same system, same tools, every time)
    push assistant content (append-only history)
    match stop_reason:
        tool_use  -> for each tool_use block: permission -> confirm -> MCP call -> tool_result
                     push ONE user message with every tool_result, continue
        refusal   -> "I can't help with that request."
        max_tokens-> return the text, flag "truncated"
        otherwise -> return the text
verifier -> flags, compensating cancel when warranted
audit line
```

All tool results of one assistant turn go back in a single user message, as the API requires for parallel tool use. A tool the model calls outside the turn's permission is not refused; it is turned into a confirmation request (`needs_confirmation`, flagged `confirmation_requested:no_intent`) and held until the user says so in a turn of their own, so a misbehaving model call can never reach the engine on its own, and a request the keyword gate does not recognise ("compra medio ETH a 3000", "get me half an eth at 3000") still works after one question. The same token flow covers cancels: the token is bound to the tool and its arguments, and a pending cancel permits cancels, not placements, on the confirming turn.

Permission comes from the user's words: a trade verb (buy, sell, bid, offer, go long, grab, dump, and so on) or the shape of an order (the asset plus at least two numbers, as in "0.5 ETH @ 3000"), a cancel verb for `cancel_order` and `cancel_all_orders`, or a confirmation word while an order is pending. These are heuristics, and the paraphrase suite is where they are measured; the verifier applies the same rules after the fact and additionally flags a placed order whose price and quantity both fail to appear in a message that did contain numbers (`params_not_in_request`).

On the turn in which the user confirms a pending action, the note changes shape: it names the action and its token ("The user confirmed the pending action (cancel order 5). Call cancel_order now with the same arguments plus confirmation_token ..."). The first live runs showed a model occasionally asking a second time instead of completing the flow; the explicit note removes that ambiguity without changing what is enforced.

## Sessions and the API

Sessions are in-memory and bounded: an append-only message history, the turn counter, a pending confirmation if any, and the last order id. A client-supplied `session_id` must be 1 to 64 characters of letters, digits, `.`, `_` or `-`, because it becomes part of every idempotency key the engine echoes back in listings the model reads; anything else is a 400. When the store holds `MAX_SESSIONS`, sessions idle for longer than `SESSION_IDLE_SECS` are dropped, then the least recently used one. `POST /chat` takes `{"session_id": optional, "message": string}` and answers:

A `request_id` in the chat request (`{"session_id", "request_id", "message"}`) makes a retried POST, from a client that lost the response to a network error, return the earlier answer instead of running the turn, and its actions, again; the last sixteen answers per session are kept. Each session may start twenty turns per minute (`TURNS_PER_MINUTE`); the twenty-first within a minute is answered 429 without touching the model.

```json
{
  "session_id": "web-1", "turn": 1,
  "reply": "Placed order 5: buy 0.5000 ETH at 3000.00, resting.",
  "tool_calls": [ { "name": "place_limit_order", "args": {...}, "result": "...", "is_error": false, "intercepted": false, "latency_ms": 3 } ],
  "usage": { "input_tokens": 61, "output_tokens": 212, "cache_read_input_tokens": 2310, "cache_creation_input_tokens": 140 },
  "model": "claude-opus-5", "stop_reason": "end_turn", "iterations": 2,
  "latency_ms": 4210, "model_latency_ms": 4180, "flags": [], "permitted": ["place_limit_order"]
}
```

`GET /healthz` and `GET /sessions/{id}` (turns, messages, pending confirmation) complete the API. Errors are JSON with an `error` field: 400 for a missing or oversized message, 502 when the model or the MCP server fails.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `MODEL_PROVIDER` | `anthropic` | `anthropic` or `deepseek`; the DeepSeek variables are listed above |
| `ANTHROPIC_API_KEY` | required | unless `ANTHROPIC_BASE_URL` points at a local mock |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | the tests point this at a scripted mock |
| `ANTHROPIC_MODEL` | `claude-opus-5` | |
| `EFFORT` | `medium` | `low`, `medium`, `high`, `xhigh`, `max` |
| `MAX_TOKENS` | `16000` | |
| `MCP_URL` | `http://127.0.0.1:8000/mcp` | |
| `AGENT_BIND` | `127.0.0.1:8080` | |
| `AUDIT_LOG` | `audit.jsonl` | empty string disables |
| `CONFIRM_THRESHOLD_ETH` | `1` | orders at or above this size need a confirmation turn |
| `CONFIRM_UNPRICED` | `1` | an order whose price or side the user never stated (the model chose it, as for "sell now" or "0.5 ETH @ 3000 please") needs a confirmation turn whatever its size |
| `TURNS_PER_MINUTE` | `20` | turns one session may start per rolling minute; beyond it `POST /chat` answers 429 |
| `GATE_TOOLS` | `1` | permit action tools only on explicit intent (`0`: everything permitted, the verifier still runs) |
| `NOTE_CHANNEL` | by model | `system` or `user`: how the per-turn permission note is sent |
| `PROMPT_CACHE` | `1` | cache breakpoint on the system prompt plus automatic caching of the conversation |
| `CONTEXT_EDITING` | `0` | ask the API to clear old tool results server-side (beta) |
| `MAX_SESSIONS` | `1000` | sessions kept in memory |
| `SESSION_IDLE_SECS` | `3600` | idle sessions are dropped first when the store is full |

## Testing without the model

`crates/agent-service/tests/agent.rs` runs the real engine and MCP server in-process and replaces the model with a scripted mock of the Messages API. Every scenario asserts the engine's end state: a price question permits no action tools and the request still carries the full, name-sorted tool list with the cache breakpoints; an explicit buy places an order carrying an idempotency key; a 2 ETH buy needs a confirmation turn before it is placed and the tool list is identical across all three requests; an unrequested placement becomes a confirmation request when the gate is on and is cancelled by the verifier when the gate is off; a cancel in Spanish is held, confirmed with "sí" and executed with the token bound to that cancel; a refusal and the iteration cap are handled; context editing is requested when enabled; a DeepSeek-shaped mock (chat completions with `reasoning_content` and OpenAI-style `tool_calls`) drives a placement through the same loop and the second request replays the reasoning and answers the call by id; the permission note is a system-role message on supporting models and falls back to the user turn when a mock model rejects it with a 400; the session store evicts the least recently used session; the HTTP API round-trips and refuses a session id that could carry text. The mock also asserts the request shape: headers, `output_config`, `fallbacks`, the permission note, and that all tool results return in one message.
