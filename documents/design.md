# Agent Harness Bench design

**Planner and worker agents must not read this document or anything else in this repository.** Benchmark tasks give agents everything they need; reading the harness, its scoring or its judge prompts invalidates a run.

## Purpose

The benchmark compares agent-coordinator configurations: planner and worker models plus the planner and worker prompt documents selected for a workspace. Each workspace builds the same set of tasks under fixed time limits, and its submissions are scored on result quality, source simplicity and how the planner used the coordinator.

## Components

* **Benchmark service** (`src/`): a Rust HTTP service on `http://127.0.0.1:8741`. It owns clocks, run state, submissions, chess tournaments, game and dashboard launches for judges, and score validation. State is SQLite plus submission files under the data directory (normally `../runtime/benchmark-service`).
* **Agent coordinator**: a separate service, normally on port 4242, where planners create tasks that coordinator workers implement and review. The benchmark only starts planners and stops a case's unfinished tasks at its deadline.
* **Batch scheduler** (`src/batch.rs`): queues every benchmark for every workspace, shuffles the cases, runs up to `max_parallel` at once with one case per workspace, and launches each planner through `scripts/template_session.py` and `scripts/template_planner.py`.
* **Scoring runner** (`scripts/score_batch.py`, `scripts/batch_metrics.py`): requests chess tournaments, launches fresh judge agents for games and dashboards, computes framework metrics from the coordinator's saved tasks, and writes one JSON report. Weights and references are in `scoring.json`.

## Benchmarks

| Benchmark | Development | Evaluation |
| --- | --- | --- |
| `chess960` | 600 s | Seeded round robin against the batch's other submissions and two baseline bots; Elo change |
| `game_build` | 1800 s | A judge explores the running game, then rates it with evidence |
| `dataset_dashboard` | 900 s | A judge explores the running dashboard, then rates it with evidence |
| `maze_game` | 1800 s | Hidden follow-up requests arrive one at a time; a judge rates completion of each received request and overall quality |
| `ml_predictor` | 900 s | A judge rates the EDA report; the submission's predict command runs on the official held-out test set and is compared with a precomputed reference model |

Game and maze runs get three random theme words, and game runs also get one to three game types drawn from the 22 in `GAME_TYPES` in `src/prompts.rs` (from economic simulation and 4X to puzzle, racing and narrative adventure); ML runs get one of the three tasks in `ml_datasets.json` (Adult income, Optical digits, Fashion-MNIST), each with an officially separate test file that agents are never given; dashboard runs get a random dataset from `datasets.json`. Maze follow-ups come from a private file named in the batch request and are never shown to agents ahead of time.

## Lifecycle

1. The coordinator's `POST /api/workspaces/generate` creates fresh workspaces with selected prompt documents and agent settings.
2. `harness.py benchmark-batch` submits the batch. Each case gets a run with a fixed deadline and a scoped run token.
3. The planner receives only the filled template: its benchmark name, the HTTP call for its task, and its workspace. The task text holds the build and submission contract.
4. At the deadline the service freezes the last accepted submission and stops the planner tree and its coordinator tasks.
5. `harness.py batch-score` runs scoring once every case has finished.

## Trust model

Everything runs as the same local user, so agents can technically read any file. Agents are kept away from the harness by never pointing them at it: the template and task text contain no repository paths except the chess rules engine, which chess builders may reuse so they spend their time on strategy. Scoring and judge prompts live only in this repository and the service.

## Further documents

[API](api.md), [lifecycle and operations](architecture.md), [batches](batches.md), [coordinator integration](coordinator.md), [scoring](scoring.md), [chess engine provenance](provenance.md), [OpenAPI schema](openapi.json).

## ML predictor

Agents receive only the training URLs and are told not to use the test data. At scoring, `scripts/ml_eval.py` downloads each official test file, checks it against the checksum recorded in `ml_datasets.json`, strips the labels, runs the submission's `predict` command with the ML virtual environment (`../runtime/ml-python`) first on PATH, and computes the metric. Model results are scaled piecewise: the trivial baseline scores 0, the reference model in `ml_reference/` scores 50 and a perfect result scores 100. The ML result is the mean of that and the EDA judge's rating.
