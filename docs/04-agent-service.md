# 04 · The agent service

## In plain terms

A small HTTP API, `POST /chat`, runs Claude in a loop: Claude reads the message, decides which tools to call, the service runs them against the MCP server and feeds the results back, and the loop ends when Claude answers in plain text. The service also owns the conversation-level guardrails: which tools a turn may use, a confirmation step for large orders, idempotency keys for retries, a post-turn verifier and an audit log.

## Calling the model

There is no official Anthropic SDK for Rust, so `crates/agent-service/src/anthropic.rs` talks to the Messages API directly with `reqwest` and `serde_json`: one endpoint, one JSON body.

| Request field | Value | Why |
|---|---|---|
| `model` | `claude-opus-5` (env `ANTHROPIC_MODEL`) | the current default model |
| `max_tokens` | 16000 (env `MAX_TOKENS`) | thinking tokens count against it; a low cap truncates answers |
| `system` | `crates/agent-service/src/prompts/system.md` | units, the "only on explicit instruction" rule, style |
| `tools` | the MCP tool definitions, mapped field for field | an MCP tool has exactly `name`, `description`, `inputSchema` |
| `output_config.effort` | `medium` (env `EFFORT`) | the latency lever; thinking itself is adaptive by default |
| `fallbacks` | `"default"` with header `anthropic-beta: server-side-fallback-2026-07-01` | a safety refusal is routed to a fallback model inside the same call |

Headers: `x-api-key`, `anthropic-version: 2023-06-01`. Transient failures (429, 529, 5xx, connection errors) are retried up to three times with jittered exponential backoff. `temperature` is not sent: current Opus models reject sampling parameters, which is why the evaluation harness measures variance with repeated runs instead.

## The loop

```
offered = read tools + action tools when the user's words carry the intent
push user message
loop (at most 8 iterations):
    msg = POST /v1/messages
    push assistant content (append-only history)
    match stop_reason:
        tool_use  -> for each tool_use block: gate -> confirm -> MCP call -> tool_result
                     push ONE user message with every tool_result, continue
        refusal   -> "I can't help with that request."
        max_tokens-> return the text, flag "truncated"
        otherwise -> return the text
verifier -> flags, compensating cancel when warranted
audit line
```

All tool results of one assistant turn go back in a single user message, as the API requires for parallel tool use. A tool the model names but that was not offered on this turn is answered with an error result and flagged, so a misbehaving model call can never reach the engine through a closed gate.

## Sessions and the API

Sessions are in-memory: an append-only message history, the turn counter, a pending confirmation if any, and the last order id. `POST /chat` takes `{"session_id": optional, "message": string}` and answers:

```json
{
  "session_id": "web-1", "turn": 1,
  "reply": "Placed order 5: buy 0.5000 ETH at 3000.00, resting.",
  "tool_calls": [ { "name": "place_limit_order", "args": {...}, "result": "...", "is_error": false, "intercepted": false, "latency_ms": 3 } ],
  "usage": { "input_tokens": 1834, "output_tokens": 212 },
  "model": "claude-opus-5", "stop_reason": "end_turn", "iterations": 2,
  "latency_ms": 4210, "model_latency_ms": 4180, "flags": []
}
```

`GET /healthz` and `GET /sessions/{id}` (turns, messages, pending confirmation) complete the API. Errors are JSON with an `error` field: 400 for a missing or oversized message, 502 when the model or the MCP server fails.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `ANTHROPIC_API_KEY` | required | unless `ANTHROPIC_BASE_URL` points at a local mock |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | the tests point this at a scripted mock |
| `ANTHROPIC_MODEL` | `claude-opus-5` | |
| `EFFORT` | `medium` | `low`, `medium`, `high`, `xhigh`, `max` |
| `MAX_TOKENS` | `16000` | |
| `MCP_URL` | `http://127.0.0.1:8000/mcp` | |
| `AGENT_BIND` | `127.0.0.1:8080` | |
| `AUDIT_LOG` | `audit.jsonl` | empty string disables |
| `CONFIRM_THRESHOLD_ETH` | `1` | orders at or above this size need a confirmation turn |
| `GATE_TOOLS` | `1` | offer action tools only on explicit intent |

## Testing without the model

`crates/agent-service/tests/agent.rs` runs the real engine and MCP server in-process and replaces the model with a scripted mock of the Messages API. Every scenario asserts the engine's end state: a price question offers no action tools; an explicit buy places an order carrying an idempotency key; a 2 ETH buy needs a confirmation turn before it is placed; an unrequested placement is refused when the gate is on and cancelled by the verifier when the gate is off; a refusal and the iteration cap are handled; the HTTP API round-trips. The mock also asserts the request shape: headers, `output_config`, `fallbacks`, and that all tool results return in one message.
