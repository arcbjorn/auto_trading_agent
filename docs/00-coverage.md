# Task coverage

Each requirement of the task, the code that implements it and the test or run that proves it. Line links are kept current by `make docmap` and checked in CI.

**1. The Engine (gRPC)**

| Requirement | Code | Proof |
|---|---|---|
| Spot limit buy and sell orders | [book.rs::Book::place](../crates/engine/src/book.rs#L1270-L1534) | [book.rs::matches_the_naive_reference](../crates/engine/src/book.rs#L2477-L2513) |
| Order cancellations | [book.rs::Book::cancel](../crates/engine/src/book.rs#L1538-L1540) | [concurrency.rs::cancels_race_placements_and_leave_the_book_empty](../crates/engine-server/tests/concurrency.rs#L212-L283) |
| Exposed strictly via gRPC | [proto/clob.proto](../proto/clob.proto) | [concurrency.rs::idempotent_retry_and_status_codes](../crates/engine-server/tests/concurrency.rs#L598-L690) |
| Thread-safe | [sequencer.rs::spawn_with_journal](../crates/engine/src/sequencer.rs#L236-L305) | [concurrency.rs::sixteen_tasks_place_orders_concurrently](../crates/engine-server/tests/concurrency.rs#L23-L110) |
| Efficient under concurrent placement | [sequencer.rs::apply](../crates/engine/src/sequencer.rs#L132-L210) | [grpc_bench.rs](../crates/engine-server/examples/grpc_bench.rs), [soak.rs](../crates/engine-server/examples/soak.rs) |
| Deterministic | [journal.rs::Journal::recover](../crates/engine/src/journal.rs#L108-L181) | [journal.rs::replay_rebuilds_the_same_book_and_continues_its_counters](../crates/engine/src/journal.rs#L475-L626) |

**2. The Bridge (MCP)**

| Requirement | Code | Proof |
|---|---|---|
| Perceive market state | [tools.rs::ToolSet::quote](../crates/mcp-server/src/tools.rs#L550-L594) | [protocol.rs::tools_against_a_real_engine](../crates/mcp-server/tests/protocol.rs#L260-L639) |
| Act on the market | [tools.rs::ToolSet::place](../crates/mcp-server/src/tools.rs#L638-L736) | [policy.rs::rejects_size_value_collar_and_open_orders](../crates/mcp-server/src/policy.rs#L277-L298) |
| Structure of tools | [tools.rs::ToolSet::definitions](../crates/mcp-server/src/tools.rs#L278-L432) | [protocol.rs::lifecycle_and_discovery](../crates/mcp-server/tests/protocol.rs#L64-L216) |
| Structure of resources | [protocol.rs::McpServer::resources_read](../crates/mcp-server/src/protocol.rs#L216-L222) | [scripts/mcp_stdio_notifications_check.py](../scripts/mcp_stdio_notifications_check.py) |
| Optimised for LLM reasoning | [tools.rs::grpc_error](../crates/mcp-server/src/tools.rs#L231-L245), [tools.rs::add_top](../crates/mcp-server/src/tools.rs#L157-L162) | [units.rs::rejects_bad_inputs_with_actionable_messages](../crates/mcp-server/src/units.rs#L139-L148) |
| Minimal context window | [anthropic.rs::AnthropicClient::request_body](../crates/agent-service/src/anthropic.rs#L155-L178) | 92 to 95% of prompt tokens from cache |

**3. LLM Interaction Service**

| Requirement | Code | Proof |
|---|---|---|
| Execute a trade | [agent.rs::Agent::chat_turn](../crates/agent-service/src/agent.rs#L298-L603) | [agent.rs::explicit_buy_places_an_order_with_an_idempotency_key](../crates/agent-service/tests/agent.rs#L267-L294) |
| Return pricing | [gate.rs::Permissions::for_turn](../crates/agent-service/src/gate.rs#L424-L436) | [agent.rs::read_only_question_permits_no_action_tools](../crates/agent-service/tests/agent.rs#L203-L264) |
| Retrieve order history | [tools.rs::ToolSet::statement](../crates/mcp-server/src/tools.rs#L794-L838) | [book.rs::ledger_tracks_average_cost_realised_pnl_and_withdrawals](../crates/engine/src/book.rs#L1845-L1903) |
| Guardrails: validation | [units.rs::parse_price](../crates/mcp-server/src/units.rs#L71-L73), [book.rs::MAX_PRICE](../crates/engine/src/book.rs#L34-L34) | [book.rs::hard_caps_reject_absurd_orders_before_anything_else](../crates/engine/src/book.rs#L1923-L1937) |
| Guardrails: risk checks | [policy.rs::Policy::check_place](../crates/mcp-server/src/policy.rs#L143-L159), [gate.rs::ConfirmationGate::intercept](../crates/agent-service/src/gate.rs#L524-L749) | [agent.rs::large_order_needs_confirmation_then_executes](../crates/agent-service/tests/agent.rs#L297-L359) |
| Guardrails: prompt protections | [gate.rs::verify](../crates/agent-service/src/gate.rs#L779-L805), [audit.rs::Audit::append](../crates/agent-service/src/audit.rs#L124-L138) | [agent.rs::an_action_is_refused_when_its_audit_record_cannot_be_written](../crates/agent-service/tests/agent.rs#L764-L793) |

**4. LLM Evaluation**

| Dimension | Code | Proof |
|---|---|---|
| Trade execution accuracy | [harness.rs::grade](../crates/evals/src/harness.rs#L229-L281) | [harness.rs::check_invariants](../crates/evals/src/harness.rs#L434-L477) in CI |
| Latency | [report.rs::render](../crates/evals/src/report.rs#L58-L214) | [docs/results](results) |
| Safety, guardrail effectiveness | [model.rs::UnsafeModel](../crates/agent-service/src/model.rs#L68-L71) | `make eval-unsafe` in CI |
| Prompt robustness | [perturb.rs::apply](../crates/evals/src/perturb.rs#L39-L52) | `make eval-perturbed` |
| Simulation-based testing | [sim.rs::run](../crates/evals/src/sim.rs#L167-L300) | `make sim` |
