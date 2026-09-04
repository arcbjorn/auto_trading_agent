# Results

Apple M1 Pro, release builds, loopback. Reproduce with the `make` targets named in each row; the reports themselves are listed below.

| Measurement | Result |
|---|---|
| Book: 1M places, 250k cancels (`bench`) | 730k to 840k operations/s; 650k to 720k with wallets enforced |
| gRPC `PlaceOrder`, 16 concurrent clients (`bench`) | 61k to 69k orders/s; p50 70 µs per call sequentially |
| Soak: 4 restarts, 1M journaled orders each (`soak`) | resident 110, 108, 107 MB; recovery 2.2 to 2.5 s |
| MCP interoperability, official client (`interop`) | all tools, resources and prompt; schemas validated |
| Harness bounds (`eval-oracle`, `eval-null`) | oracle 100%; null 0% on execution and paraphrase |
| Hostile model against the gate (`eval-unsafe`) | 0 unauthorised mutations in 26 runs; it found 3 before the gate was tightened |
| Accuracy, DeepSeek V4 Flash, 57 cases (`eval-model`) | 57/57 |
| Accuracy, Claude Sonnet 5, 57 cases × 3 reps | 170/171; the first run scored 91% on execution and exposed a gate gap, now fixed |
| Safety, both models | 39/39 attacks blocked; 0 unauthorised mutations across every live run |
| Prompt robustness, every turn perturbed (`eval-perturbed`) | 57/57 on both models |
| Reply grounding | 172 figures quoted, none without a source in the turn's inputs |
| Latency and cost | turn p50 3 to 6 s; 92 to 95% cache hits; 0.05 USD per DeepSeek run, 0.94 USD per Sonnet run of 171 |
| Simulation, 5 seeds × 8 rounds (`sim`) | goal reached 5/5, no rule violations; scripted baseline 4/5 |

## Reports

| File | Run |
|---|---|
| [report-oracle.md](report-oracle.md), [report-null.md](report-null.md), [report-unsafe.md](report-unsafe.md) | the three model-free bounds of the harness |
| [report-model-deepseek-v4-flash.md](report-model-deepseek-v4-flash.md) | DeepSeek V4 Flash, 57 cases, final gate |
| [report-model-deepseek-v4-flash-perturbed.md](report-model-deepseek-v4-flash-perturbed.md) | the same cases with every turn perturbed |
| [report-model-deepseek-v4-flash-low.md](report-model-deepseek-v4-flash-low.md) | reasoning effort low, for the effort comparison |
| [report-model-messages-api-deepseek.md](report-model-messages-api-deepseek.md) | the Claude Messages-API client against DeepSeek's compatible endpoint |
| [report-model-claude-sonnet-5.md](report-model-claude-sonnet-5.md) | Claude Sonnet 5, 57 cases × 3 reps |
| [report-model-claude-sonnet-5-perturbed.md](report-model-claude-sonnet-5-perturbed.md) | Claude Sonnet 5, every turn perturbed |
| [sim-baseline.md](sim-baseline.md), [sim-model-deepseek-v4-flash.md](sim-model-deepseek-v4-flash.md) | the market simulation, scripted baseline and model |
| [demo-deepseek-v4-flash.md](demo-deepseek-v4-flash.md) | the eight-turn `make demo` transcript |
