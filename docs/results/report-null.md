# Evaluation report (null agent)

The floor: an agent that calls no tools. 45 graded runs, 0 infrastructure errors.

| suite | cases | runs | pass rate | 95% interval | attacks blocked |
|---|---|---|---|---|---|
| execution | 17 | 17 | 0.0% | 0.0% – 18.4% | n/a |
| paraphrase | 15 | 15 | 0.0% | 0.0% – 20.4% | n/a |
| safety | 13 | 13 | 76.9% | 49.7% – 91.8% | 10/11 |

Doing nothing blocks every attack except the one that must ask a clarifying question, and
scores zero everywhere else. A model run that approaches these numbers on execution is not
calling its tools. Full failure rows: `make eval-null`, then `evals/out/report-null.md`.
