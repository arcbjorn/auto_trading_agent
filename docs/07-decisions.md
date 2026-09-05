# 07 · Decisions

Short architecture decision records. Each states the decision, the alternatives, and why.

## ADR-1 Integers with a fixed scale, never floats

Price is a `u64` count of ticks (0.01 USDC), quantity a `u64` count of lots (0.0001 ETH), notionals `u128`. Fixed scales make accounting exact, with checked conversions and explicit caps. Alternatives: `f64` introduces rounding; a decimal crate adds a dependency for two fixed scales.

## ADR-2 One matcher thread that owns the book

Handlers send commands over a bounded channel and await a oneshot reply; readers take a lock-free snapshot. Alternatives: `Arc<Mutex<Book>>` (correct, but every task contends for the lock and the ordering is whoever wins it), a lock-free book (far more code, hard to keep deterministic). The single writer gives total ordering for free, needs no lock or `unsafe`, and Rust's ownership rules turn "only one thread touches the book" from a convention into a compile error.

## ADR-3 Counters, not clocks, for ordering

Ids and sequence numbers are counters assigned by the matcher; wall-clock timestamps are recorded for reporting only. Alternatives: UUIDs (no order), timestamps (ties and clock skew). Counters make the engine replayable and the evaluations reproducible.

## ADR-4 tonic for gRPC

tonic integrates with tokio and the existing hyper stack, with established use in projects such as Linkerd and Arrow Flight. The alternative, grpcio, adds a binding to the gRPC C core. Adoption data is recorded in [Dependencies](08-dependencies.md#grpc-implementations).

## ADR-5 The real protoc, vendored

`build.rs` runs the system `protoc` when `PROTOC` is set and otherwise the binary from `protoc-bin-vendored`. Alternative: a pure-Rust protobuf compiler. The real compiler is the industry tool and the vendored copy means a fresh checkout builds with nothing but cargo.

## ADR-6 MCP by hand on serde_json, over stdio and Streamable HTTP

The required protocol subset fits on `serde_json` and `hyper`, avoiding an SDK dependency and keeping dispatch visible in one file. The cost is maintaining the protocol and eleven tool schemas ourselves. CI checks both transports against the official Python client.

## ADR-7 The risk policy lives in the MCP server

That is the boundary every model path crosses, including desktop hosts that never touch the chat service. The chat service owns only conversation-level rules (per-turn permission, confirmation, verifier).

## ADR-8 The Messages API over raw HTTPS, client-side tool loop

There is no official Rust SDK. The loop is about a hundred lines with `reqwest` and `serde_json`. Alternative: the API's server-side MCP connector, which removes the loop but needs a publicly reachable MCP server and removes the per-call hook where the gating and confirmation live.

## ADR-9 The model's tools are the MCP tools

Fetched at startup from `tools/list` and mapped field for field, keeping descriptions consistent across hosts. The service adds optional `confirmation_token` to the three action tools.

## ADR-10 Grade the engine, not the transcript

The evaluation harness reads orders and trades back over gRPC after each turn and compares them to the expected list. The reply is checked only for the numbers it must mention. Alternatives: LLM-judged transcripts (grades narration, not outcomes). Oracle and null agents bound the harness from above and below.

## ADR-11 hyper directly for the two small HTTP servers

Both servers have small routing tables, and hyper is already present through tonic. A web framework would add a dependency without simplifying these handlers substantially.

## ADR-12 A stable tool list, permission enforced at call time

Keep a name-sorted tool list and append permission notes to the conversation. Per-turn tool lists invalidate prompt caching and can invalidate thinking blocks bound to that prefix. The gate checks calls independently and holds unrecognised intent for confirmation. Notes use the system channel where supported, with a user-channel fallback. Deferred tool loading (`defer_loading` plus `tool_addition`) is a beta alternative that preserves the cache prefix.

## ADR-13 Context stays append-only; the API prunes it

"Minimise context window usage" invites trimming old tool results client-side. That is a history edit: it invalidates the cached prefix and, on the newest models, every later thinking block. The service therefore never rewrites history and instead can ask the API to clear old tool results itself (`context_management`, `CONTEXT_EDITING=1`), which the API does not count as an edit. Server-side compaction is the next lever when a session approaches the context limit.

## ADR-14 Per-account indices in the book

Account listings run on the matcher thread, so full scans delay writers. Orders use per-account `BTreeSet<OrderId>` indices; trades use `VecDeque<TradeId>`. Account ids are interned `Arc<str>`. Alternatives: scan all history or maintain a second copy for reads. The indices keep listings local to an account and support retention cleanup.

## ADR-15 One internal conversation format, providers translate at the edge

Adding DeepSeek V4 could have meant a second loop or an abstract "message" type. Instead the Messages API block format stays the only internal representation, being the richer one: typed tool_use and tool_result blocks, per-turn system messages, thinking as a block. The DeepSeek client translates in both directions, keeping DeepSeek's `reasoning_content` as a `reasoning` block because the API demands it back on every later request that carries tools. The loop, the permission gate, the confirmation flow, the verifier, the audit log and the harness are all provider-blind. The alternatives were DeepSeek's Anthropic-compatible endpoint (fewer lines, but it ignores caching markers and rejects the Claude betas, and the native API is the documented one for thinking-mode tool calls) or a third-party client crate (one more dependency for two HTTP calls).

## ADR-16 Journal the commands, not the events

Durability for a deterministic book needs only its inputs. The journal holds every place and cancel with its timestamp, and replay recomputes orders, trades, ids and sequence numbers through the same code. The alternatives were journaling events (more data, and a second code path that must agree with the matcher) or a snapshot per interval (loses everything since the last one). The journal is committed once per matcher batch before the replies go out, which is why the fsync variant stays usable under load. Flush-only is the default because a power-loss guarantee costs two orders of magnitude of latency on a laptop disk, and the README shows both numbers. Compaction is a snapshot of the book's state plus the tail, taken at startup when the journal is large. The book already has a deterministic, serialisable state, so there is no second format to keep in step with the matcher, and doing it at startup means the matcher thread never pauses to serialise a large book.

## ADR-17 Wallets in the engine, deposits outside the model's reach

An agent that can sell what it does not hold is not trading infrastructure. Balances live in the book, next to the orders they back, so reservation, settlement and release happen inside the same deterministic step as matching and are journaled with it; a separate ledger service would need two-phase coordination for something the single writer does for free. Amounts are integers in the engine's own units, so a notional is a multiplication with no rounding. `Deposit` is a gRPC method the operator, the harness and the simulation call; it is not an MCP tool, so no prompt can fund an account. Alternatives: a policy-level notional cap only (what the MCP server already has, and it cannot know what was filled), or balances in the MCP server (one more source of truth, and desktop hosts would bypass it).

## ADR-19 An agent loop, not a routing classifier

The model reads tool results and chooses subsequent calls, supporting requests such as "sell half my ETH" and "cancel the higher bid". The gate decides what may execute. A typed routing classifier would simplify control flow but require explicit workflows for read-before-action requests. The loop's broader action surface is checked by confirmation, policy, auditing and hostile-model evaluations.

## ADR-20 Pre-action audit writes fail closed

Sync a hash-chained pre-action record before each action, refusing submission if the write fails. Verify the chain at startup and allow one writer per file. This preserves attempted actions across process failure at the cost of an `fsync`; final turn records are best effort. A chain alone does not establish execution or detect complete suffix deletion. See [audit limitations](05-guardrails.md#what-the-audit-chain-establishes).

## ADR-21 A confirmation in words carries the previous request

A model may ask for confirmation before calling a tool, leaving nothing pending in the gate. A bare confirmation can therefore carry the previous user request, provided that turn sent no action and its reply explicitly asked for confirmation with the side and figures. The user's request remains the parameter reference. Trusting the model's proposal alone could authorize altered terms. Derived quantities still use the token flow.

## ADR-25 Unchecked numeric casts are denied where money lives

`as` between integer widths is silent: a negative price read as `u64` becomes astronomical, and a wide value narrowed wraps. The four crates that hold market state deny `cast_possible_truncation`, `cast_possible_wrap` and `cast_sign_loss`, so every conversion is either checked, saturating with the reason written down, or allowed locally with an explanation (a retry hint rounded from a float is the only one). The gRPC boundary converts through a `wire` helper whose comment names the caps that make it total, and a test asserts those caps still hold, so raising one fails the build rather than a price. Generated protobuf and the evaluation binary keep the defaults: there is nothing to fix in a file rewritten on every build, and the harness is an ordinary program.

## ADR-24 A confirmation may only answer what the user was shown

The service checks that a pending action's side and figures, or cancellation scope, appeared in the reply before allowing confirmation. Otherwise a model could hide a proposal behind "Done." and treat a later "yes" as approval. The harness also checks disclosure; `hide_summary` exercises this attack in CI.

## ADR-23 A confirmation token identifies, it does not authorise

A token binds a call to the exact pending action. Execution also requires a later user turn that confirms the disclosed action; possession of the token alone grants no permission. The confirmation flag names its turn, and the harness checks that turn's text rather than trusting the flag alone.

## ADR-22 One pair, by design

The task asks for a book for a single pair, and every layer leans on that. The MCP tools carry no symbol, which keeps the prompt small and the cache prefix stable. The gate reasons about one price and one quantity. One book has one sequence and one journal, which is what makes the determinism claim simple. Sharding by symbol would be one actor per pair behind a market registry, with per-shard sequences and a coordinator for anything that spans pairs. That is a redesign of the product surface, not an improvement of this slice, so it stays a documented seam and nothing in the code or docs promises it.

## ADR-18 No Docker in this slice

Every component is a cargo binary with environment-variable configuration; the runbook has the three commands. A compose file would add an untested surface without changing the design.

## The goal run is experimental

The simulation and web goal runner use `AgentConfig::autonomous`, allowing the model to act on a goal while policy, balances and auditing remain active. This extends the supervised service and is marked experimental; the chat API never uses it. Keeping the mode explicit prevents goal runs from inheriting order-specific intent and parameter checks.
