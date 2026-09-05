# 05 · Guardrails

The service checks intent and confirmation, the MCP server applies risk policy, and the engine
enforces funds and matching rules. Intent recognition and reply grounding use heuristics.

## Enforced boundaries

| Layer | Implementation | Behavior and limits |
|---|---|---|
| Schema and units | `mcp-server/src/tools.rs`, `units.rs` | Rejects unknown fields, invalid types and precision. Price/quantity conversion uses integer arithmetic. |
| Engine validation | `engine/src/book.rs`, `engine-server` | Requires positive values, bounded identifiers, valid sides, and hard per-order caps of 1000000 USDC price and 10000 ETH quantity. |
| Wallets | `engine/src/book.rs` | Reserves funds at placement, settles fills and releases cancelled remainders. The default service requires funding; explicit benchmark configuration can disable wallet checks. |
| MCP policy | `mcp-server/src/policy.rs`, `tools.rs` | Defaults: 10 ETH/order, 50000 USDC/order, 10% price collar, 20 live orders, 10 actions/minute and 200000 USDC submitted notional per account/process. `TRADING_HALTED=1` blocks writes. All are startup configuration, not chat settings. |
| Policy accounting | `mcp-server/src/tools.rs` | Serializes placement checks and submission per `ToolSet`. Definitive rejection refunds submitted notional; ambiguous failures retain it. Accounting is process-local. |
| Price reference | `mcp-server/src/policy.rs` | Uses midpoint, else last trade, else the quoted side. A completely empty market has no reference and therefore no collar. |
| Account binding | `mcp-server` | Tools use the configured `ACCOUNT_ID`. All chat sessions on that service share it. The engine checks ownership against the caller-supplied account; it does not authenticate that account. |
| Local browser boundary | `mcp-server/src/transport/http.rs`, chat and demo handlers | Requires a loopback Host and, if present, one explicit loopback HTTP(S) Origin. Rejects opaque `null`, foreign and malformed origins before processing requests. This does not authenticate local callers. |
| Turn permission | `agent-service/src/gate.rs` | Trade/cancel vocabulary and order shapes grant permission; unknown intent is held for confirmation. Recognized negations and questions grant neither permission nor numeric parameters. Quoted, conditional and multilingual text is not fully parsed. |
| Confirmation | `gate.rs`, `agent.rs` | At least 1 ETH, an unstated side or price, an adjusted quantity, or recognized demo/test framing requires confirmation. The pending action must be disclosed with its side and figures; cancelling all must disclose cancellation of all orders. |
| Confirmation release | `gate.rs` | Requires a later affirmative user turn, an unexpired token, the same tool and matching arguments. The token identifies the action; the user's turn authorizes it. |
| Confirmation in words | `agent.rs` | A bare “yes” can carry an unexecuted previous request only if the preceding reply explicitly asked for confirmation and disclosed the action. |
| Action budget | `agent.rs`, `gate.rs` | At most one action attempt crosses MCP per turn, including a confirming turn. A transport error spends that attempt because it can follow execution. |
| Contradictions | `gate.rs` | A recognized opposite side, an unstated price when figures pin the order, or cancelling another explicitly named id is rejected. Cancelling all requires the cancellation clause itself to say all; otherwise it is held. |
| Request replay | `agent-service/src/http.rs` | The last sixteen request ids per resident session retain the exact message, status and response, including failed turns. A different message conflicts; an interrupted handler leaves an unknown-outcome conflict. No durability across restart/eviction. |
| Engine replay keys | `engine/src/book.rs` | `client_order_id` deduplicates matching placements while the order is retained. Archived ids can create new orders. MCP's policy-free retry lookup only searches its latest 200 orders. |
| Session concurrency | `agent-service/src/http.rs` | Per-session locks serialize turns; active sessions cannot be evicted. A store containing only busy sessions returns 429. |
| Bounded history | session store and engine retention | Default chat limits: 1000 sessions, 20 turns/minute/session, 200 turns/session and 150000 reported prompt tokens. Engine queues and closed history are bounded. Account count, funded ledgers, journal disk growth and total HTTP connections are not globally bounded. |
| Engine exposure | `engine-server/src/main.rs`, matcher | Standalone defaults: 20 live orders, 200000 USDC resting notional, 50 mutations/second/account. Library defaults are unlimited. Exposure caps apply to resting remainders after fills. |
| Journal recovery | `engine/src/journal.rs` | Generation-based compaction preserves recovery inputs; ambiguous generations fail startup. Snapshots bind balance mode, exposure and retention. See [engine durability](02-engine.md#durability-a-journal-of-commands-replayed). |
| Audit before actions | `agent-service/src/audit.rs`, `agent.rs` | With auditing enabled, pre-action records are synced before submission, including compensating cancels. An append error blocks later appends until verified reopen. Unterminated final records refuse reopen. Audit disabling is an explicit configuration option. |

## Diagnostics and compensation

The post-turn verifier shares the gate's heuristics. It flags successful actions without recognized
intent and placements whose price and quantity both lack support in a numbered request.

`unsupported_number:<n>` flags figures absent from non-assistant inputs, tool arguments/results
and simple arithmetic on them; small counts are ignored. A supported number can still be used
incorrectly. Flags are diagnostic and do not block the reply.

Compensation attempts to cancel an unrequested order but cannot reverse fills. Policy rejection,
audit failure or network error can prevent the cancel; the failure is reported.

## What the audit chain establishes

Pre-action records are required before submission; final turn records are best effort. A pre-action
record establishes an attempt, not execution. The chain detects corruption, but a file writer can
recompute an unkeyed chain, and deleting a complete suffix leaves a valid shorter log.

`AUDIT_KEY` uses legacy secret-prefix SHA-256, not HMAC. Tamper evidence requires an authenticated
external sink and retained chain heads. Each file must have one writer. The web demo resets its
log at boot and exposes a tamper button.

## Evaluation scope

The [evaluation suites](06-evaluation.md) check engine state and include benign requests, so blocking
everything cannot pass. Some judgments reuse gate vocabulary and can share its blind spots.
Deployment assumptions are in the [runbook](09-runbook.md#local-trust-boundary).
