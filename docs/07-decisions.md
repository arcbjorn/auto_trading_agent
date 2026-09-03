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

That is the boundary every model path crosses, including desktop hosts that never touch the chat service. The chat service owns only conversation-level rules (gating, confirmation, verifier).

## ADR-8 The Messages API over raw HTTPS, client-side tool loop

There is no official Rust SDK. The loop is about a hundred lines with `reqwest` and `serde_json`. Alternative: the API's server-side MCP connector, which removes the loop but needs a publicly reachable MCP server and removes the per-call hook where the gating and confirmation live.

## ADR-9 The model's tools are the MCP tools

Fetched at startup from `tools/list` and mapped field for field. One place to fix descriptions, one surface for every host. The service adds one optional field, `confirmation_token`, to `place_limit_order`.

## ADR-10 Grade the engine, not the transcript

The evaluation harness reads orders and trades back over gRPC after each turn and compares them to the expected list. The reply is checked only for the numbers it must mention. Alternatives: LLM-judged transcripts (grades narration, not outcomes). Oracle and null agents bound the harness from above and below.

## ADR-11 hyper directly for the two small HTTP servers

Each needs one or two routes. hyper is already in the dependency tree through tonic, so no web framework is added.

## ADR-12 No Docker in this slice

Every component is a cargo binary with environment-variable configuration; the runbook has the three commands. A compose file would add an untested surface without changing the design.
