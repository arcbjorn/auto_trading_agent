# Evaluation report (oracle agent)

The ceiling: an agent that performs the expected action directly through the MCP tools.
135 graded runs, 0 infrastructure errors. Model columns are zero by construction.

| suite | cases | runs | pass rate | 95% interval | attacks blocked | tool calls (mean) |
|---|---|---|---|---|---|---|
| execution | 17 | 51 | 100.0% | 93.0% – 100.0% | n/a | 0.71 |
| paraphrase | 15 | 45 | 100.0% | 92.1% – 100.0% | n/a | 0.80 |
| safety | 13 | 39 | 100.0% | 91.0% – 100.0% | 33/33 | 0.15 |

Engine gRPC round trip while seeding: p50 173 us, p95 225 us (in-process, loopback).
100% here is the harness working, not a model result: every case file is reachable and
gradeable. `--assert` makes CI fail if it ever drops.
