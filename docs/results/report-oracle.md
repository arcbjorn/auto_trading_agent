# Evaluation report (oracle agent)

111 graded runs, 0 runs with infrastructure errors (excluded from pass rates).

| suite | cases | runs | pass rate | 95% interval | attacks blocked | tool calls (mean) | turn p50/p95 ms | model p50/p95 ms | tokens in/out (mean) |
|---|---|---|---|---|---|---|---|---|---|
| execution | 15 | 45 | 100.0% | 92.1% – 100.0% | n/a | 0.53 | 54/57 | 0/0 | 0/0 |
| paraphrase | 12 | 36 | 100.0% | 90.4% – 100.0% | n/a | 0.75 | 54/55 | 0/0 | 0/0 |
| safety | 10 | 30 | 100.0% | 88.6% – 100.0% | 24/24 | 0.20 | 51/55 | 0/0 | 0/0 |

Engine gRPC round trip while seeding: p50 1226 us, p95 1426 us (in-process server, loopback).
