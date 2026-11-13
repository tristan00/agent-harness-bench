# Template planner batches

**Planner and worker agents must not read this document or anything else in this repository.**

The controller creates workspaces through the coordinator's workspace creator, selecting planner and worker documents from `agent_coordinator/prompt_candidate/project_planner` and `agent_coordinator/prompt_candidate/worker_protocol`. Each document must contain 1–200 whitespace-separated words. Both catalogs are initially empty; creation fails until valid candidates are available. Each workspace receives `PROJECT_PLANNER.md` and `WORKER_PROTOCOL.md`, with candidate names and SHA-256 hashes recorded in its `settings.prompts`. No repository-wide instruction fallback is used.

The controller then submits one batch request. `chess960`, `game_build`, `dataset_dashboard` and `maze_game` are queued for every workspace unless the request lists `benchmarks`; `maze_game` also needs its private configuration in `task_configs`. Each fresh benchmark child directory receives copies of those two selected documents before its development clock starts. The planner template points to that child's `PROJECT_PLANNER.md`; implementation, review and replan workers receive its `WORKER_PROTOCOL.md` in `request.workerProtocol`. The template and launch profile are snapshotted at enqueue time. Changed workspace document hashes prevent launch.

All planners connect to the shared coordinator at the profile's `coordinator_url` (normally port 4242). Each case directory sits inside its workspace, and the coordinator files its tasks under that workspace, so board filtering groups them there. At cutoff the benchmark terminates the planner tree and stops the case's unfinished coordinator tasks through the coordinator's task API. Working directories start with only the two selected documents; launch prompts and logs live under `DATA/coordinator/RUN_ID` and task history lives in the shared coordinator data directory.

The scheduler dispatches cases and enforces deadlines. Planners choose tasks, monitor work and submit complete versions. An early planner exit leaves workers their remaining allowance; once the planner has exited and none of the case's tasks can still run (every task is done, stopped or held), the case ends and frees its slot. Maze cases keep their session, which hands over follow-up requests. New batches shuffle all workspace/benchmark pairs once before enqueueing, mixing benchmark types and workspace order. Optional `order_seed` reproduces that shuffle; otherwise a random seed is generated. The seed, algorithm version and resulting queue are persisted, so restart does not reshuffle. Benchmarks run sequentially within each workspace, with `max_parallel` limiting active cases per batch (default **8**). The scheduler skips busy workspaces while filling available slots. Queued cases have no ticking clock. Service restart marks active cases interrupted and stops their shared-board tasks while retaining accepted submissions. `batch-stop` stops active planners and scoped tasks and cancels queued cases.

Game cases receive independent three-word inspiration assignments when enqueued, stored as `case.inspiration` and copied into `run.inspiration`. Builder prompts, reviewer prompts and reports use that run’s own words. Dashboard cases receive independent random dataset assignments when enqueued. Repeats are possible. The selected metadata, registry version and selection seed are saved in the case and run, so restart and later scoring retain the assignment. The default allowance is 900 seconds, including downloading. The five-source rotation is in `datasets.json`. USGS was excluded because it needs pagination, and World Bank was excluded after a timeout. All five retained sources passed two complete downloads and checksum comparisons.

Scoring is an explicit, separate step. The [batch scoring command](scoring.md) starts the chess tournament, launches fresh game/dashboard reviewers, calculates execution metrics and writes the combined workspace ranking without a supervising agent.

## Configuration

Run `python scripts/service.py setup` to add the template session commands to the `default` profile. Other profiles are preserved. It inherits `HARNESS_URL`, `HARNESS_RUN_ID`, `HARNESS_RUN_TOKEN` and `COORDINATOR_URL`. No admin token or deadline is supplied in that planner environment. `session_command` normally runs `scripts/template_session.py`.

The planner launcher runs the required workspace setting `planner` in its own vendor's harness: a fresh `codex exec --ephemeral` invocation for OpenAI agents or headless `claude -p` for Anthropic agents, passing the template unchanged through stdin. The creator samples `planner` and `worker` independently from the agents the launching family allows. The selected pair is always explicit in workspace settings and every launch. Workers use it for implementation, review and replanning, taking precedence over task overrides and global CLI defaults. The CLI runs with unattended access to this trusted local workspace; process ownership is not a filesystem security boundary.

Create workspaces with the coordinator's `POST /api/workspaces/generate` and body `{"paths": [ABSOLUTE_NEW_DIRECTORIES], "seed": OPTIONAL_INTEGER}`. In one call it deals the valid `project_planner` and `worker_protocol` candidates from shuffled decks, so every candidate is used once before any repeats. It samples the planner from the launching family's planner-capable agents, the worker from its enabled agents, and limits and update mode from the same seeded generator. The response returns the seed. `GET /api/prompt-candidates` lists valid candidates and invalid-file diagnostics; `POST /api/workspaces` creates one workspace from explicit `path`, `settings`, `project_planner` and `worker_protocol`. Candidate edits affect future creations only; empty prompt pools prevent creation.

Example `batch.json` (directories must contain only their selected documents; copy the actual hashes returned by workspace creation):

```json
{
  "profile": "default",
  "template_path": "D:/harness_work/agent-harness-bench/planner_agent_benchmark_prompt_template",
  "workspace_root": "D:/harness_work/workspaces",
  "max_parallel": 8,
  "workspaces": [
    {"path": "D:/harness_work/workspaces/experiment_1", "settings": {"planner": "PLANNER_ID", "worker": "WORKER_ID", "updates": "poll", "prompts": {"project_planner": {"candidate": "planner.md", "sha256": "ACTUAL_HASH"}, "worker_protocol": {"candidate": "worker.md", "sha256": "ACTUAL_HASH"}}}}
  ],
  "task_configs": {
    "chess960": {"development_seconds": 1200},
    "game_build": {"development_seconds": 1800},
    "dataset_dashboard": {"development_seconds": 900}
  }
}
```

Copy each workspace's settings from the coordinator configuration into the request. The same settings apply to all three benchmarks. Parameters, benchmark seed and prompts are retained for later analysis. Fresh runs require fresh workspace directories; the utility never deletes or reuses previous solutions.

```bash
python scripts/harness.py --token-file ../runtime/benchmark-service/admin-token benchmark-batch batch.json
python scripts/harness.py --token-file ../runtime/benchmark-service/admin-token batch-status BATCH_ID
python scripts/harness.py --token-file ../runtime/benchmark-service/admin-token batch-stop BATCH_ID
```

Admin routes: `POST /v1/benchmark-batches`, `GET /v1/benchmark-batches`, `GET /v1/benchmark-batches/ID`, `POST /v1/benchmark-batches/ID/stop`. The scoped planner route `GET /v1/runs/ID/task` returns the benchmark prompt without timing metadata. The development scheduler does not automatically submit or score; run the scoring command separately after development finishes.

Run `scripts/smoke_batch.py --binary PATH --coordinator PATH --evidence PATH` in the launching family to verify the actual HTTP service, coordinators, scoped task access, exact prompts, copied workspace documents, settings, queue cap, deadline termination, late submissions, restart recovery and cancellation using deterministic local fixtures.

New chess campaigns draw a fresh random seed by default, producing four random starts sampled without replacement and played in both colors. Supply `task_configs.chess960.seed` only to intentionally replay a tournament schedule. That seed does not control game inspiration or dashboard datasets. Restart never resamples saved assignments.

To stage a compiled update without interrupting a batch, write its absolute executable path to `DATA/next-service-binary`. The next `python scripts/service.py start` without an explicit `--binary` uses it and consumes the marker after successful startup. The helper refuses to start over an existing service. This does not schedule or perform a restart.
