# Results

Recorded measurements on Apple M1 Pro, release builds, loopback. The DeepSeek runs are against the final gate; the Claude runs and the throughput figures were recorded earlier and are dated in their reports. Reproduce with the `make` targets below.

| Measurement | Result |
|---|---|
| Book: 1M places, 250k cancels (`bench`) | 730k to 840k operations/s; 650k to 720k with wallets enforced |
| gRPC `PlaceOrder`, 16 concurrent clients (`bench`) | 61k to 69k orders/s; p50 70 µs per call sequentially |
| gRPC `PlaceOrders` pipelined stream (`bench`) | 955k orders/s on one stream, 673k on four |
| Soak: 4 restarts, 1M journaled orders each (`soak`) | resident 110, 108, 107 MB; recovery 2.2 to 2.5 s |
| MCP interoperability, official client (`interop`) | all tools, resources and prompt; schemas validated |
| Harness bounds (`eval-oracle`, `eval-null`) | oracle 100%; null 0% on execution and paraphrase |
| Five hostile strategies against the gate (`eval-unsafe`) | 0 unauthorised mutations in 26 runs each; the strategies found 6 gaps between them before reaching zero |
| Accuracy, DeepSeek V4 Flash, 57 cases (`eval-model`) | 57/57 on the final gate; the run before the last gate fix scored 53/57, which is what found the regression |
| Accuracy, Claude Sonnet 5, 57 cases × 3 reps | 170/171; the first run scored 91% on execution and exposed a gate gap, now fixed |
| Safety, both models | 39/39 attacks blocked; 0 unauthorised mutations across every live run |
| Prompt robustness, every turn perturbed (`eval-perturbed`) | 57/57 on both models |
| Reply grounding, both DeepSeek runs | 542 figures quoted across 114 runs, none without a source in the turn's inputs; assistant text no longer counts as evidence |
| Reply-quality judge, DeepSeek V4 Flash judging DeepSeek | clarity 4.81/5, useful 4.39/5, faithful 55/57; two unfaithful replies found that the end-state grade could not see |
| Confirmation burden, DeepSeek V4 Flash, final gate | 6 of 31 legitimate requests held, all in cases written as confirmation flows; 0 needless |
| Latency and cost | turn p50 3.4 to 5.6 s; 93 to 95% cache hits; 0.05 USD per DeepSeek run of 57, 0.94 USD per Sonnet run of 171 |
| Simulation, 5 seeds × 8 rounds (`sim`) | goal reached 5/5, no rule violations; scripted baseline 4/5 |

## Reports

| File | Run |
|---|---|
| [report-oracle.md](report-oracle.md), [report-null.md](report-null.md), [report-unsafe.md](report-unsafe.md) | the three model-free bounds of the harness |
| [report-model-deepseek-v4-flash.md](report-model-deepseek-v4-flash.md) | DeepSeek V4 Flash, 57 cases, final gate |
| [report-model-deepseek-v4-flash-perturbed.md](report-model-deepseek-v4-flash-perturbed.md) | the same cases with every turn perturbed |
| [report-model-deepseek-v4-flash-low.md](report-model-deepseek-v4-flash-low.md) | reasoning effort low, for the effort comparison |
| [report-model-messages-api-deepseek.md](report-model-messages-api-deepseek.md) | the Claude Messages-API client against DeepSeek's compatible endpoint |
| [report-model-deepseek-v4-flash-judged.md](report-model-deepseek-v4-flash-judged.md) | the same cases with every reply scored by the judge |
| [report-model-claude-sonnet-5.md](report-model-claude-sonnet-5.md) | Claude Sonnet 5, 57 cases × 3 reps |
| [report-model-claude-sonnet-5-perturbed.md](report-model-claude-sonnet-5-perturbed.md) | Claude Sonnet 5, every turn perturbed |
| [sim-baseline.md](sim-baseline.md), [sim-model-deepseek-v4-flash.md](sim-model-deepseek-v4-flash.md) | the market simulation, scripted baseline and model |
| [demo-deepseek-v4-flash.md](demo-deepseek-v4-flash.md) | the eight-turn `make demo` transcript |
