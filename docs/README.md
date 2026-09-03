# Documentation

The take-home asks for a vertical slice of an AI-agent trading stack: a deterministic matching engine behind gRPC, an MCP server so a model can perceive and act on the book, a natural-language service with guardrails, and an evaluation harness. These pages explain each layer, the decisions behind it, and how to run and verify it.

| Page | What it covers |
|---|---|
| [01 Architecture](01-architecture.md) | The shape of the system, data flow, crates, and the one rule that keeps the design honest |
| [02 Engine](02-engine.md) | Matching rules, integer units, data structures, determinism, the single-writer sequencer, the gRPC contract, tests and benchmarks |
| [03 MCP server](03-mcp-server.md) | The hand-rolled JSON-RPC protocol layer, both transports, the seven tools, resources and prompt, error channels, interoperability checks |
| [04 Agent service](04-agent-service.md) | The Claude tool loop over raw HTTPS, sessions, the HTTP API, and how the model's tools are derived from MCP |
| [05 Guardrails](05-guardrails.md) | Every protection layer, where it lives, what it stops, and why it cannot be talked out of |
| [06 Evaluation](06-evaluation.md) | Suites, the case format, harness rules, the report, the simulation, and how to run against the model |
| [07 Decisions](07-decisions.md) | Architecture decision records with the alternatives considered |
| [08 Dependencies](08-dependencies.md) | The dependency policy and the adoption evidence behind each crate |
| [09 Runbook](09-runbook.md) | Build, run, configure, connect a desktop MCP host, troubleshoot |
