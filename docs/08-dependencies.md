# 08 · Dependencies

## Toolchain

Rust edition 2024 on a pinned stable toolchain; `rust-version` states the minimum the code needs
rather than the version it was built with. Edition 2024 is what makes `std::env::set_var` unsafe,
since it races with any thread reading the environment, which is why the protobuf build script
configures `protoc` directly instead of setting a variable. The workspace forbids `unsafe_code`,
denies `todo!` and `dbg!`, and the four crates holding market state deny unchecked numeric casts
(see ADR-25).

## Policy

Every runtime dependency must have years of production use at scale, judged by adoption and
maintenance rather than version number, since Rust convention keeps many foundational crates
below 1.0. Young or fast-moving crates are replaced by hand-written code when the surface is
small enough (the MCP protocol layer). Nothing here is `unsafe`.

## Runtime dependencies

Download counts are from crates.io on 2026-09-03; "since" is the first release.

| Crate | Version | Since | Total downloads | Last 90 days | Used for |
|---|---|---|---|---|---|
| tokio | 1.53 | 2016 | 932M | 213M | async runtime, channels, I/O |
| tokio-stream | 0.1 | 2020 | 451M | 99M | `TcpListenerStream` for tonic's incoming |
| hyper | 1.11 | 2014 | 886M | 192M | HTTP/1.1 servers for the MCP endpoint and the chat API |
| hyper-util | 0.1 | 2022 | 451M | 133M | tokio adapter for hyper |
| http-body-util | 0.1 | 2022 | 435M | 132M | body collection with a size limit |
| bytes | 1.12 | 2015 | 999M | 232M | byte buffers |
| tonic, tonic-prost | 0.14 | 2018 | 377M | 84M | gRPC server and client |
| prost | 0.14 | 2017 | 564M | 125M | protobuf runtime |
| tonic-prost-build, protoc-bin-vendored | 0.14, 3.2 | 2018, 2020 | – | – | build-time code generation with the real protoc |
| serde, serde_json | 1.0 | 2015 | 1.25B (json) | 292M | every JSON boundary |
| reqwest | 0.13 | 2016 | 686M | 172M | HTTPS client for the Messages API and the MCP endpoint |
| arc-swap | 1.9 | 2018 | 311M | 77M | lock-free snapshot publication |
| thiserror, anyhow | 2.0, 1.0 | 2019 | – | – | error types |
| tracing, tracing-subscriber | 0.1, 0.3 | 2017 | 816M | 180M | structured logging |
| proptest (dev) | 1.11 | 2017 | 180M | 45M | property-based tests |

## Vendored browser asset

The web demo (`evals web`) serves one third-party file: htmx 2.0.10 (`crates/evals/web/htmx.min.js`, 51 KB, BSD Zero Clause licence), so the page has no build step and works offline. The page's own stylesheet and script are a few hundred lines, and every panel is an HTML fragment rendered by the server. The only other runtime fetch is the IBM Plex Mono font from Google Fonts, with a system monospace fallback when offline.

## What was deliberately left out

| Crate | Why not |
|---|---|
| rmcp (official MCP SDK) | First published March 2025; API changes between minor versions, deprecations within months. Replaced by ~400 lines of hand-written JSON-RPC |
| axum, actix-web | Two routes per server do not justify a framework; hyper is already present |
| rust_decimal | Exact decimal parsing and formatting for two fixed scales is 60 lines in `units.rs` |
| schemars | Eleven hand-written schemas give full control over what the model reads |
| criterion | Two small benchmark binaries with `std::time::Instant` avoid a heavy dev dependency |
| governor | The per-account rate limit is a sliding window in `policy.rs` |
| rand | The benchmarks and the simulation use a seeded xorshift generator |
| askama, maud | The web demo renders its fragments with `format!` and one escape function |
| pulldown-cmark | The result reports use headings, paragraphs, lists and tables; a small renderer in `markdown.rs` covers them |
| protox | A pure-Rust protobuf compiler; the real protoc is vendored instead |

## gRPC in Rust, for the record

| Crate | First release | Total downloads | Last 90 days | Latest release |
|---|---|---|---|---|
| tonic | 2018 | 377M | 84M | 2026-05 |
| grpcio (gRPC C-core binding) | 2017 | 1.6M | 0.17M | 2023-08 |
| grpc (pure Rust) | 2016 | 1.0M | 0.03M | 2026-05 |

Verified from their manifests and lockfiles: the Linkerd2 proxy, Apache Arrow Flight, the OpenTelemetry Rust OTLP exporter, InfluxDB 3, the Solana Agave validator and Materialize all ship tonic.
