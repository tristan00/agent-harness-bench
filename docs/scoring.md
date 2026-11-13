# One-command workspace scoring

With the benchmark service running against the specified data directory, execute from this repository:

```powershell
python scripts/score_batch.py BATCH_ID --data D:/harness_work/runtime/benchmark-batches-20261112 --coordinator-data COORDINATOR_DATA_DIRECTORY --judge sol_light --max-judges 2
```

The shared coordinator's `/api/state` response gives `dataDirectory`. `--url` defaults to `http://127.0.0.1:8741`. Development must have finished. The command reads the admin token from the benchmark data directory and never passes it to reviewers. It requests all three campaign evaluations, runs the chess tournament through the service, starts and terminates reviewer processes, submits their ratings when scoring opens, computes metrics, and emits JSON plus an HTML ranking. No agent supervises scoring. Source submissions are frozen; scoring does not restart development or repair submissions.

Reviewers require a working, authenticated Codex CLI, Node/npx and Chrome on the evaluation host. Every reviewer is a new ephemeral session with explicit `gpt-6-sol` / `low` settings. `--judge sol_light` is required; no model or effort is inherited from defaults. Playwright MCP is version-pinned in `scoring-v1.json` and uses an isolated, headless browser. `--max-judges` caps concurrent reviewer sessions. Each uses the campaign's exploration and scoring deadlines (normally 300 + 120 seconds). The service stops the submitted application at exploration cutoff; the command owns and terminates the reviewer process tree.

Reviewers inspect the UI, screenshots, controls, the frozen submission and browser timings. Game reviewers use the existing interface, efficiency, gameplay, visuals and testability rubric. Dashboard reviewers separately score completeness, information architecture, prose economy, animation, originality and performance. Each score is an integer 0–5 with concrete evidence; dashboards also record observed measurements and methods. Missing or invalid reviewer results fail the command explicitly instead of inventing ratings. Completed native reports are reused on rerun. A failed reviewer session remains an explicit failure; the command does not silently retry it with a more favorable judge.

## Version 1 formulas

All category scores are on a 0–100 scale. Each benchmark is the mean of **result, simplicity and framework efficiency**. A workspace is the mean of its **three benchmark scores**. Framework efficiency is the mean of **task count, relations, parallelism and token efficiency**. These equal weights are configured in `scoring-v1.json`.

Normalization anchors are initial implementation choices, recorded in that versioned configuration. They are fixed across batches, rather than rescaled against whichever competitors happen to appear. Raw metrics remain available for analysis and future formula changes.

| Metric | Version 1 score |
| --- | --- |
| Chess result | `100 / (1 + 10^(-EloChange / 400))`; zero Elo change maps to 50 |
| Game/dashboard result | `20 × mean criterion rating` |
| Simplicity | `100 / (1 + sourceLines / 1000)` |
| Task count | `100 × tasks / (tasks + 10)` |
| Relations | `100 / 3 × (connectivity + types/(types+4) + density/(density+2))` |
| Parallelism | `50 × (overlapFraction + 1 - 1/effectiveParallelism)`; zero with no attempts |
| Token efficiency | `100 / (1 + cost)`; zero with no tasks |

Source lines are the existing artifact counter's nonblank physical source lines, including comments and vendored source. A run without an accepted submission receives zero result and simplicity scores. Its framework activity is still measured. Build or launch failures with an accepted submission follow the tournament's forfeit rules or the review rubric, rather than being replaced with an earlier submission.

The framework calculation uses code only. Tasks are matched by the exact benchmark case directory and its descendants, including archived tasks; earlier aborted attempts are excluded. Container tasks count and are reported separately. Relations count parent, after, conflicts, related, supersedes and each typed dependency reason. Symmetric edges are deduplicated; edges outside the selected case are excluded. `connectivity` is the mean of `min(uniqueNeighbors/2, 1)`, penalizing both orphan and single-neighbor tasks. `density` is typed edges per task. These metrics measure recorded structure, not whether a dependency was useful.

Worker implementation, review and replan attempt intervals are clipped to the development window. `overlapFraction` is time with at least two distinct tasks active divided by time with any active task. `effectiveParallelism` is distinct-task active-seconds divided by that busy time. Overlapping attempts for the same task do not inflate parallelism. Planner time is excluded from task parallelism.

When every worker launch and planner has a complete usage record, `cost` is total input-plus-output tokens / 100000. Cached input tokens are already included in input counts and are not added again. If any usage records are incomplete, **every case in the batch** uses the same fallback: `(worker agent-seconds + planner seconds) / development allowance`. This is a runtime proxy, not a token estimate. An early planner exit uses its last recorded log timestamp; otherwise the full allowance is used. Partial measured token totals and coverage remain visible. Scoring agents' usage is excluded from development efficiency.

## Outputs and repeatability

Results are saved in `DATA/batch-scores/BATCH_ID/workspace-scores.v1/`:

- `scores.html`: workspace ranking and category breakdown.
- `scores.json`: all scores, raw metrics, historical workspace settings, datasets and native quality reports.
- `metrics.json`, `batch.json`, `config.json`: captured inputs, case identities and formulas. Reruns reuse the captured metrics and reject changed settings or selected attempts in the same output directory.
- `judges/RUN_ID/`: exact prompt, frozen source artifact, explicit model/effort metadata, session log, screenshots, reviewer score and report.

Use `--config` and a new version/output directory for deliberate formula changes. The command reuses completed quality reports; it does not rerun the chess tournament or produce new subjective ratings simply to recalculate scores. A process lock prevents simultaneous commands against the same output directory and releases automatically when the process exits. The report records scorer source hashes and invocation settings. Judge outcomes are stochastic despite fixed model settings. Different dashboard datasets and this small batch limit causal comparisons between workspace settings.
