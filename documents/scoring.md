# JSON batch scoring

**Planner and worker agents must not read this document or anything else in this repository.**

After every development case finishes, send one authenticated request:

```http
POST /v1/benchmark-batches/BATCH_ID/score
Authorization: Bearer ADMIN_TOKEN
Content-Type: application/json

{"max_judges": 4}
```

The request remains open while the service runs the scoring utility and returns the complete JSON report. Use a client without a short HTTP read timeout. No supervising agent composes or edits the report. The service runs the chess tournament, launches independent quality judges, submits their ratings, computes the metrics and aggregates every score. An equivalent CLI makes this same single API call:

```bash
python scripts/harness.py --token-file ../runtime/benchmark-service/admin-token batch-score BATCH_ID --max-judges 4 > scores.json
```

An unfinished batch or concurrent scoring request returns HTTP 409. Authentication is required. Scoring errors remain errors; missing judge ratings are never invented. Repeated requests reuse completed native reports and the saved metric snapshot. They do not silently rerun failed judges. The service records execution details in its batch-scores directory.

The report is JSON only. It includes raw task and dependency evidence, per-file source line counts, execution intervals, token coverage, judge criteria and evidence, chess Elo metrics, formulas, intermediate values, and every weighted aggregation. Each weighted mean lists raw weights, normalized weights, weighted values, contributions, numerator, denominator, and result. `cases[].calculation` explains benchmark scores; `ranking[].calculation` explains workspace scores. `token_mode_decision` lists exactly which incomplete cases forced the common runtime fallback.

Scoring uses `scoring.json` (version `workspace-scores.v4`). Its five equally weighted framework components are task count, relationships, parallelism, token efficiency, and classification. Classification is `100 * attribute_fill_share * classified_dependency_share`. `attribute_fill_share` is filled attribute slots divided by `task_count * len(classification_attributes)`, where `scoring.json` lists `languages`, `functions` and `priority`. An attribute is unfilled when it is null or empty. The coordinator saves `lane`, `size`, `artifact` and `deploy` with defaults, and `needs` and `model` have meaningful empty values, so saved tasks cannot show whether a planner set them and they are not counted. Dependency reasons that are missing or `unclassified` reduce dependency coverage. Only in-scope dependency edges count, matching the relationship graph. Containers count as tasks. No dependencies means no dependency penalty; no tasks earns zero. The report lists filled counts and missing task IDs per attribute and all unclassified edges. Attributes are read from saved tasks, never inferred from text or filled in by the scorer.

Result averages maze request completion and quality; maze source lines and token cost are divided by `max(sum(request completion fractions), 1)`. Earlier scoring versions are no longer supported; reports already saved under their version directories are unchanged.

Independent reviewers use the configured model and effort in fresh sessions with isolated headless browsers. They inspect frozen submissions; they cannot repair them. Arithmetic and aggregations are deterministic code. Reviewer judgment remains subjective and its individual ratings and evidence are preserved in `quality_report`.

## Formulas

All category scores are on a 0–100 scale. Each benchmark is the mean of **result, simplicity and framework efficiency**. A workspace is the mean of its **four benchmark scores**. Framework efficiency is the mean of **task count, relations, parallelism, token efficiency and classification**. These equal weights are configured in `scoring.json`.

Normalization anchors are initial implementation choices, recorded in that versioned configuration. They are fixed across batches, rather than rescaled against whichever competitors happen to appear. Raw metrics remain available for analysis and future formula changes.

| Metric | Score |
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

Results are saved in `DATA/batch-scores/BATCH_ID/workspace-scores.v4/`:

- `scores.json`: all scores, raw metrics, historical workspace settings, datasets and native quality reports.
- `metrics.json`, `batch.json`, `config.json`: captured inputs, case identities and formulas. Reruns reuse the captured metrics and reject changed settings or selected attempts in the same output directory.
- `judges/RUN_ID/`: exact prompt, frozen source artifact, explicit model/effort metadata, session log, screenshots, reviewer score and report.

Use `--config` and a new version/output directory for deliberate formula changes. The command reuses completed quality reports; it does not rerun the chess tournament or produce new subjective ratings simply to recalculate scores. A process lock prevents simultaneous commands against the same output directory and releases automatically when the process exits. The report records scorer source hashes and invocation settings. Judge outcomes are stochastic despite fixed model settings. Different dashboard datasets and this small batch limit causal comparisons between workspace settings.
