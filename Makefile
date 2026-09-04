# Local secrets (API keys) live in .env, which is git-ignored; every recipe sees them.
-include .env
export

.PHONY: build test lint fmt bench soak run-engine run-mcp run-mcp-stdio run-agent eval-oracle eval-null eval-unsafe eval-model eval-perturbed sim demo interop

build:
	cargo build --workspace --release

test:
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo fmt --all -- --check

fmt:
	cargo fmt --all

bench:
	cargo run --release -p engine --example bench
	cargo run --release -p engine-server --example grpc_bench

# Three million orders over gRPC with the journal on, restarting between rounds: memory must level off.
soak:
	scripts/soak.sh 4

run-engine:
	ENGINE_FUND=demo:50000:10,mm:1000000:1000 cargo run --release -p engine-server

run-mcp:
	cargo run --release -p mcp-server -- --http

run-mcp-stdio:
	cargo run --release -p mcp-server

run-agent:
	cargo run --release -p agent-service

eval-oracle:
	cargo run --release -p evals -- run --agent oracle --reps 1 --assert

eval-null:
	cargo run --release -p evals -- run --agent null --reps 1 --assert

# A hostile scripted model that tries to place an order on every turn: the gate must let none through
# where the user asked for no order or cancel.
eval-unsafe:
	cargo run --release -p evals -- run --agent unsafe --reps 1 --assert

eval-model:
	cargo run --release -p evals -- run --agent model --reps 3

eval-perturbed:
	cargo run --release -p evals -- run --agent model --perturb all --reps 3

demo:
	cargo run --release -p evals -- demo

sim:
	cargo run --release -p evals -- sim --agent baseline --seeds 5 --rounds 8

interop:
	uv run --with mcp python scripts/mcp_interop_check.py --http http://127.0.0.1:8000/mcp
