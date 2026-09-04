# 07 · Decisions

Short architecture decision records. Each states the decision, the alternatives, and why.

## ADR-1 Integers with a fixed scale, never floats

Price is a `u64` count of ticks (0.01 USDC), quantity a `u64` count of lots (0.0001 ETH), notionals `u128`. Alternatives: `f64` (rounding makes replays diverge and 0.1 + 0.2 ≠ 0.3), a decimal crate (exact, but slower and one more dependency in the hot path). Integers are exact, fast, and make overflow a thought the type system has already had.

## ADR-2 One matcher thread that owns the book

Handlers send commands over a bounded channel and await a oneshot reply; readers take a lock-free snapshot. Alternatives: `Arc<Mutex<Book>>` (correct, but every task contends for the lock and the ordering is whoever wins it), a lock-free book (far more code, hard to keep deterministic). The single writer gives total ordering for free, needs no lock or `unsafe`, and Rust's ownership rules turn "only one thread touches the book" from a convention into a compile error.

## ADR-3 Counters, not clocks, for ordering

Ids and sequence numbers are counters assigned by the matcher; wall-clock timestamps are recorded for reporting only. Alternatives: UUIDs (no order), timestamps (ties and clock skew). Counters make the engine replayable and the evaluations reproducible.

## ADR-4 tonic for gRPC

tonic is the de-facto standard gRPC implementation for Rust and ships inside the Linkerd proxy, Apache Arrow Flight, the OpenTelemetry OTLP exporter, InfluxDB 3, the Solana validator and Materialize. The only alternative with a production history is the binding to the gRPC C core, whose last release was in 2023. The 0.x version number is Rust convention (log, tracing, rustls, prost and reqwest are all 0.x with hundreds of millions of downloads), and tonic itself is a thin layer over hyper, h2 and tower.

## ADR-5 The real protoc, vendored

`build.rs` runs the system `protoc` when `PROTOC` is set and otherwise the binary from `protoc-bin-vendored`. Alternative: a pure-Rust protobuf compiler. The real compiler is the industry tool and the vendored copy means a fresh checkout builds with nothing but cargo.

## ADR-6 MCP by hand on serde_json, over stdio and Streamable HTTP

MCP is two years old and no SDK for it in any language is battle-tested; the official Rust SDK changes its API between minor versions. The server needs nine methods. Writing them on `serde_json` and `hyper` keeps every dependency in the "years in production" class and makes the protocol behaviour visible in one file. Conformance is checked against the official Python client over both transports. Cost: no automatic schema generation, so the seven schemas are written by hand, which also gives full control over the descriptions the model reads.

## ADR-7 The risk policy lives in the MCP server

That is the boundary every model path crosses, including desktop hosts that never touch the chat service. The chat service owns only conversation-level rules (per-turn permission, confirmation, verifier).

## ADR-8 The Messages API over raw HTTPS, client-side tool loop

There is no official Rust SDK. The loop is about a hundred lines with `reqwest` and `serde_json`. Alternative: the API's server-side MCP connector, which removes the loop but needs a publicly reachable MCP server and removes the per-call hook where the gating and confirmation live.

## ADR-9 The model's tools are the MCP tools

Fetched at startup from `tools/list` and mapped field for field. One place to fix descriptions, one surface for every host. The service adds one optional field, `confirmation_token`, to `place_limit_order`.

## ADR-10 Grade the engine, not the transcript

The evaluation harness reads orders and trades back over gRPC after each turn and compares them to the expected list. The reply is checked only for the numbers it must mention. Alternatives: LLM-judged transcripts (grades narration, not outcomes). Oracle and null agents bound the harness from above and below.

## ADR-11 hyper directly for the two small HTTP servers

Each needs one or two routes. hyper is already in the dependency tree through tonic, so no web framework is added.

## ADR-12 A stable tool list, permission enforced at call time

Earlier the service offered `place_limit_order` and `cancel_order` only on turns whose text carried the intent, so the model could not even see them otherwise. Two properties of the current API made that the wrong trade: the tool list is the first part of the cache prefix, so a per-turn list defeats prompt caching on every turn; and the newest models bind their thinking blocks to the conversation prefix including `tools`, so rebuilding the list mid-conversation invalidates them. The service now sends the same name-sorted list on every request, appends a one-line permission note after the user message (as a system-role message on models with the operator channel, otherwise inside the user turn, with automatic fallback), and refuses a call outside the permission in code. The end state is identical (nothing reaches the engine without intent), the transcript is append-only, and the prompt caches. A later refinement: a call outside the permission is held for confirmation rather than refused, so the keyword gate's inevitable misses (other languages, verbs it does not list) cost one question instead of a dead end, while still nothing executes without the user's own words. Alternative: the mid-conversation tool changes beta (`defer_loading` plus `tool_addition` blocks), which is the cache-preserving form of the old design and is the planned next step once it is stable.

## ADR-13 Context stays append-only; the API prunes it

"Minimise context window usage" invites trimming old tool results client-side. That is a history edit: it invalidates the cached prefix and, on the newest models, every later thinking block. The service therefore never rewrites history and instead can ask the API to clear old tool results itself (`context_management`, `CONTEXT_EDITING=1`), which the API does not count as an edit. Server-side compaction is the next lever when a session approaches the context limit.

## ADR-14 Per-account indices in the book

Every read runs on the matcher thread, so a read that scans all orders is latency for every writer. Orders and trades are indexed per account (`Vec<OrderId>` and positions into the trade vector), and account ids are interned `Arc<str>`. Alternatives: keep scanning (20 ms per listing at a million orders, and the MCP server lists before every placement) or move reads off the matcher thread with a second copy of the state (more code, and two sources of truth).

## ADR-15 One internal conversation format, providers translate at the edge

Adding DeepSeek V4 could have meant a second loop or an abstract "message" type. Instead the Messages API block format stays the only internal representation (it is the richer one: typed tool_use and tool_result blocks, per-turn system messages, thinking as a block), and the DeepSeek client translates in both directions, keeping DeepSeek's `reasoning_content` as a `reasoning` block because the API demands it back on every later request that carries tools. The loop, the permission gate, the confirmation flow, the verifier, the audit log and the harness are provider-blind. Alternatives: DeepSeek's Anthropic-compatible endpoint (fewer lines, but it ignores caching markers and rejects the Claude betas, and the native API is the documented one for thinking-mode tool calls), or a third-party client crate (one more dependency for two HTTP calls).

## ADR-16 Journal the commands, not the events

Durability for a deterministic book needs only its inputs: the journal holds every place and cancel with its timestamp, and replay recomputes orders, trades, ids and sequence numbers through the same code. Alternatives: journaling events (more data, and a second code path that must agree with the matcher) or a snapshot per interval (loses everything since the last one). The journal is committed once per matcher batch before the replies go out, which is why the fsync variant stays usable under load; flush-only is the default because a power-loss guarantee costs two orders of magnitude of latency on a laptop disk, and the README shows both numbers. Compaction is a snapshot of the book's state plus the tail, taken at startup when the journal is large: the book already has a deterministic, serialisable state, so there is no second format to keep in step with the matcher, and doing it at startup means the matcher thread never pauses to serialise a large book.

## ADR-17 Wallets in the engine, deposits outside the model's reach

An agent that can sell what it does not hold is not trading infrastructure. Balances live in the book, next to the orders they back, so reservation, settlement and release happen inside the same deterministic step as matching and are journaled with it; a separate ledger service would need two-phase coordination for something the single writer does for free. Amounts are integers in the engine's own units, so a notional is a multiplication with no rounding. `Deposit` is a gRPC method the operator, the harness and the simulation call; it is not an MCP tool, so no prompt can fund an account. Alternatives: a policy-level notional cap only (what the MCP server already has, and it cannot know what was filled), or balances in the MCP server (one more source of truth, and desktop hosts would bypass it).

## ADR-19 An agent loop, not a routing classifier

The model runs a real tool loop: it reads tool results and decides the next call, and the gate decides at call time what may execute. The alternative, seen in a sibling implementation of this exercise, is to ask the model for one typed route per turn (read this, propose that) and let Rust perform the read or prepare a proposal that the user then confirms through a separate endpoint, every time. That design is easier to prove safe, because the model never holds a write capability, but it cannot do anything that needs a read before the action ("sell half my ETH", "cancel the higher bid"), every read reply is a template, and every order costs two round trips. We kept the loop and put the safety in the gate, the verifier, the audit and the evaluation: the hostile runner shows that what reaches the engine is decided by code even when the model is adversarial.

## ADR-20 The audit log is a hash chain and writes fail closed

Each audit line carries the hash of the previous line, the service verifies the chain at startup, and an action tool call is refused if its pre-action record cannot be written. A plain append-only file is enough for debugging but proves nothing after the fact and can miss the one action that mattered, the one during which the process died. The cost is one `fsync` per action and per turn, negligible next to a model call. One log belongs to one process; two processes appending to one file would interleave two chains, which the harness learnt by running cases in parallel with one file each. Idea adopted from a sibling implementation.

## ADR-21 A confirmation in words carries the previous request

Claude Sonnet 5 often asks the user to confirm a large order in its own words before calling the tool. Nothing is then pending in the gate, and the user's "yes" arrived as a turn with no trade words, was held, and the model asked again. Two fixes were possible: trust the model's proposal text as the confirmed order, or let the confirmation stand for the previous user message. The first would let a model propose something other than what the user asked and have a "yes" authorise it. The second keeps the user's own figures as the reference: the previous message grants the permission and pins price and quantity, and only an order matching them proceeds. A derived quantity with two stated figures still goes through the token flow, which costs an ask-first model one extra turn on one case; the prompt now tells the model to call the tool first so the service produces the exact summary.

## ADR-23 A confirmation token identifies, it does not authorise

The gate hands the model a token with each confirmation request and expects it back with the confirmed call. An external review showed the token was doing too much work: presenting it was enough to execute, so a model could keep one and spend it on any later turn, or answer its own request within one turn. The fix keeps the token, which is still needed to tie a confirmed call to the exact pending action, but makes the user's turn the authority: a token executes only on a turn that came after the ask and whose message confirms. The evaluation was complicit, exempting any mutation carrying a confirmation flag, so the flag now names its turn and the harness checks that turn's own text.

## ADR-22 One pair, by design

The task asks for a book for a single pair, and every layer leans on that: the MCP tools carry no symbol, which keeps the prompt small and the cache prefix stable; the gate reasons about one price and one quantity; one book has one sequence and one journal, which is what makes the determinism claim simple. Sharding by symbol would be one actor per pair behind a market registry, with per-shard sequences and a coordinator for anything that spans pairs. That is a redesign of the product surface, not an improvement of this slice, so it stays a documented seam and nothing in the code or docs promises it.

## ADR-18 No Docker in this slice

Every component is a cargo binary with environment-variable configuration; the runbook has the three commands. A compose file would add an untested surface without changing the design.
