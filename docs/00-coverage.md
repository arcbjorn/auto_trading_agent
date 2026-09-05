# Task coverage

Each requirement of the task, the code that implements it and the test or run that proves it. Line links are kept current by `make docmap` and checked in CI.

**1. The Engine (gRPC)**

| Requirement | Code | Proof |
|---|---|---|
| Spot limit buy and sell orders | [book.rs::Book::place](../crates/engine/src/book.rs#L1278-L1542) | [book.rs::matches_the_naive_reference](../crates/engine/src/book.rs#L2485-L2521) |
| Order cancellations | [book.rs::Book::cancel](../crates/engine/src/book.rs#L1546-L1548) | [concurrency.rs::cancels_race_placements_and_leave_the_book_empty](../crates/engine-server/tests/concurrency.rs#L212-L286) |
| Exposed strictly via gRPC | [proto/clob.proto](../proto/clob.proto) | [concurrency.rs::idempotent_retry_and_status_codes](../crates/engine-server/tests/concurrency.rs#L601-L693) |
| Thread-safe | [sequencer.rs::spawn_with_journal](../crates/engine/src/sequencer.rs#L236-L305) | [concurrency.rs::sixteen_tasks_place_orders_concurrently](../crates/engine-server/tests/concurrency.rs#L23-L110) |
| Efficient under concurrent placement | [sequencer.rs::apply](../crates/engine/src/sequencer.rs#L132-L210) | [grpc_bench.rs](../crates/engine-server/examples/grpc_bench.rs), [soak.rs](../crates/engine-server/examples/soak.rs) |
| Deterministic | [journal.rs::Journal::recover](../crates/engine/src/journal.rs#L108-L110) | [journal.rs::replay_rebuilds_the_same_book_and_continues_its_counters](../crates/engine/src/journal.rs#L638-L789) |

**2. The Bridge (MCP)**

| Requirement | Code | Proof |
|---|---|---|
| Perceive market state | [tools.rs::ToolSet::quote](../crates/mcp-server/src/tools.rs#L553-L597) | [protocol.rs::tools_against_a_real_engine](../crates/mcp-server/tests/protocol.rs#L268-L647) |
| Act on the market | [tools.rs::ToolSet::place](../crates/mcp-server/src/tools.rs#L641-L752) | [policy.rs::rejects_size_value_collar_and_open_orders](../crates/mcp-server/src/policy.rs#L302-L323) |
| Structure of tools | [tools.rs::ToolSet::definitions](../crates/mcp-server/src/tools.rs#L281-L435) | [protocol.rs::lifecycle_and_discovery](../crates/mcp-server/tests/protocol.rs#L64-L216) |
| Structure of resources | [protocol.rs::McpServer::resources_read](../crates/mcp-server/src/protocol.rs#L216-L222) | [scripts/mcp_stdio_notifications_check.py](../scripts/mcp_stdio_notifications_check.py) |
| Optimised for LLM reasoning | [tools.rs::grpc_error](../crates/mcp-server/src/tools.rs#L233-L247), [tools.rs::add_top](../crates/mcp-server/src/tools.rs#L159-L164) | [units.rs::rejects_bad_inputs_with_actionable_messages](../crates/mcp-server/src/units.rs#L150-L159) |
| Minimal context window | [anthropic.rs::AnthropicClient::request_body](../crates/agent-service/src/anthropic.rs#L155-L178) | 92 to 95% of prompt tokens from cache |

**3. LLM Interaction Service**

| Requirement | Code | Proof |
|---|---|---|
| Execute a trade | [agent.rs::Agent::chat_turn](../crates/agent-service/src/agent.rs#L303-L643) | [agent.rs::explicit_buy_places_an_order_with_an_idempotency_key](../crates/agent-service/tests/agent.rs#L267-L294) |
| Return pricing | [gate.rs::Permissions::for_turn](../crates/agent-service/src/gate.rs#L488-L500) | [agent.rs::read_only_question_permits_no_action_tools](../crates/agent-service/tests/agent.rs#L203-L264) |
| Retrieve order history | [tools.rs::ToolSet::statement](../crates/mcp-server/src/tools.rs#L810-L854) | [book.rs::ledger_tracks_average_cost_realised_pnl_and_withdrawals](../crates/engine/src/book.rs#L1853-L1911) |
| Guardrails: validation | [units.rs::parse_price](../crates/mcp-server/src/units.rs#L71-L73), [book.rs::MAX_PRICE](../crates/engine/src/book.rs#L34-L34) | [book.rs::hard_caps_reject_absurd_orders_before_anything_else](../crates/engine/src/book.rs#L1931-L1945) |
| Guardrails: risk checks | [policy.rs::Policy::check_place](../crates/mcp-server/src/policy.rs#L143-L159), [gate.rs::ConfirmationGate::intercept](../crates/agent-service/src/gate.rs#L592-L834) | [agent.rs::large_order_needs_confirmation_then_executes](../crates/agent-service/tests/agent.rs#L297-L359) |
| Guardrails: prompt protections | [gate.rs::verify](../crates/agent-service/src/gate.rs#L864-L890), [audit.rs::Audit::append](../crates/agent-service/src/audit.rs#L137-L162) | [agent.rs::an_action_is_refused_when_its_audit_record_cannot_be_written](../crates/agent-service/tests/agent.rs#L809-L838) |

**4. LLM Evaluation**

| Dimension | Code | Proof |
|---|---|---|
| Trade execution accuracy | [harness.rs::grade](../crates/evals/src/harness.rs#L242-L294) | [harness.rs::check_invariants](../crates/evals/src/harness.rs#L446-L489) in CI |
| Latency | [report.rs::render](../crates/evals/src/report.rs#L58-L214) | [docs/results](results) |
| Safety, guardrail effectiveness | [model.rs::UnsafeModel](../crates/agent-service/src/model.rs#L68-L71) | `make eval-unsafe` in CI |
| Prompt robustness | [perturb.rs::apply](../crates/evals/src/perturb.rs#L39-L52) | `make eval-perturbed` |
| Simulation-based testing | [sim.rs::run](../crates/evals/src/sim.rs#L351-L365) | `make sim` |
