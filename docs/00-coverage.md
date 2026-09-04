# Task coverage

Each requirement of the task, the code that implements it and the test or run that proves it. Line links are kept current by `make docmap` and checked in CI.

**1. The Engine (gRPC)**

| Requirement | Code | Proof |
|---|---|---|
| Spot limit buy and sell orders | [book.rs::Book::place](../crates/engine/src/book.rs#L948-L1200) | [book.rs::matches_the_naive_reference](../crates/engine/src/book.rs#L1987-L2023) |
| Order cancellations | [book.rs::Book::cancel](../crates/engine/src/book.rs#L1204-L1245) | [concurrency.rs::cancels_race_placements_and_leave_the_book_empty](../crates/engine-server/tests/concurrency.rs#L117-L188) |
| Exposed strictly via gRPC | [proto/clob.proto](../proto/clob.proto) | [concurrency.rs::idempotent_retry_and_status_codes](../crates/engine-server/tests/concurrency.rs#L503-L595) |
| Thread-safe | [sequencer.rs::spawn_with_journal](../crates/engine/src/sequencer.rs#L204-L247) | [concurrency.rs::sixteen_tasks_place_orders_concurrently](../crates/engine-server/tests/concurrency.rs#L23-L110) |
| Efficient under concurrent placement | [sequencer.rs::apply](../crates/engine/src/sequencer.rs#L110-L180) | [grpc_bench.rs](../crates/engine-server/examples/grpc_bench.rs), [soak.rs](../crates/engine-server/examples/soak.rs) |
| Deterministic | [journal.rs::Journal::recover](../crates/engine/src/journal.rs#L92-L126) | [journal.rs::replay_rebuilds_the_same_book_and_continues_its_counters](../crates/engine/src/journal.rs#L209-L346) |

**2. The Bridge (MCP)**

| Requirement | Code | Proof |
|---|---|---|
| Perceive market state | [tools.rs::ToolSet::quote](../crates/mcp-server/src/tools.rs#L527-L571) | [protocol.rs::tools_against_a_real_engine](../crates/mcp-server/tests/protocol.rs#L217-L586) |
| Act on the market | [tools.rs::ToolSet::place](../crates/mcp-server/src/tools.rs#L587-L679) | [policy.rs::rejects_size_value_collar_and_open_orders](../crates/mcp-server/src/policy.rs#L277-L298) |
| Structure of tools | [tools.rs::ToolSet::definitions](../crates/mcp-server/src/tools.rs#L259-L409) | [protocol.rs::lifecycle_and_discovery](../crates/mcp-server/tests/protocol.rs#L64-L214) |
| Structure of resources | [protocol.rs::McpServer::resources_read](../crates/mcp-server/src/protocol.rs#L203-L209) | [scripts/mcp_stdio_notifications_check.py](../scripts/mcp_stdio_notifications_check.py) |
| Optimised for LLM reasoning | [tools.rs::grpc_error](../crates/mcp-server/src/tools.rs#L212-L226), [tools.rs::add_top](../crates/mcp-server/src/tools.rs#L142-L147) | [units.rs::rejects_bad_inputs_with_actionable_messages](../crates/mcp-server/src/units.rs#L138-L147) |
| Minimal context window | [anthropic.rs::AnthropicClient::request_body](../crates/agent-service/src/anthropic.rs#L155-L178) | 92 to 95% of prompt tokens from cache |

**3. LLM Interaction Service**

| Requirement | Code | Proof |
|---|---|---|
| Execute a trade | [agent.rs::Agent::chat_turn](../crates/agent-service/src/agent.rs#L293-L569) | [agent.rs::explicit_buy_places_an_order_with_an_idempotency_key](../crates/agent-service/tests/agent.rs#L263-L288) |
| Return pricing | [gate.rs::Permissions::for_turn](../crates/agent-service/src/gate.rs#L234-L246) | [agent.rs::read_only_question_permits_no_action_tools](../crates/agent-service/tests/agent.rs#L203-L260) |
| Retrieve order history | [tools.rs::ToolSet::statement](../crates/mcp-server/src/tools.rs#L737-L779) | [book.rs::ledger_tracks_average_cost_realised_pnl_and_withdrawals](../crates/engine/src/book.rs#L1478-L1536) |
| Guardrails: validation | [units.rs::parse_price](../crates/mcp-server/src/units.rs#L71-L73), [book.rs::MAX_PRICE](../crates/engine/src/book.rs#L34-L34) | [book.rs::hard_caps_reject_absurd_orders_before_anything_else](../crates/engine/src/book.rs#L1539-L1553) |
| Guardrails: risk checks | [policy.rs::Policy::check_place](../crates/mcp-server/src/policy.rs#L143-L159), [gate.rs::ConfirmationGate::intercept](../crates/agent-service/src/gate.rs#L310-L449) | [agent.rs::large_order_needs_confirmation_then_executes](../crates/agent-service/tests/agent.rs#L291-L349) |
| Guardrails: prompt protections | [gate.rs::verify](../crates/agent-service/src/gate.rs#L477-L503), [audit.rs::Audit::append](../crates/agent-service/src/audit.rs#L86-L98) | [agent.rs::an_action_is_refused_when_its_audit_record_cannot_be_written](../crates/agent-service/tests/agent.rs#L748-L777) |

**4. LLM Evaluation**

| Dimension | Code | Proof |
|---|---|---|
| Trade execution accuracy | [harness.rs::grade](../crates/evals/src/harness.rs#L214-L266) | [harness.rs::check_invariants](../crates/evals/src/harness.rs#L373-L413) in CI |
| Latency | [report.rs::render](../crates/evals/src/report.rs#L58-L195) | [docs/results](results) |
| Safety, guardrail effectiveness | [model.rs::UnsafeModel](../crates/agent-service/src/model.rs#L21-L21) | `make eval-unsafe` in CI |
| Prompt robustness | [perturb.rs::apply](../crates/evals/src/perturb.rs#L36-L49) | `make eval-perturbed` |
| Simulation-based testing | [sim.rs::run](../crates/evals/src/sim.rs#L167-L295) | `make sim` |
