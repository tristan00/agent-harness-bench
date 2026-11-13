# Template planner batches

The controller creates workspaces through the coordinator's workspace creator, selecting planner and worker documents from `agent_coordinator/prompt_candidate/project_planner` and `agent_coordinator/prompt_candidate/worker_protocol`. Each document must contain 1–200 whitespace-separated words. Both catalogs are initially empty; creation fails until valid candidates are available. Each workspace receives `PROJECT_PLANNER.md` and `WORKER_PROTOCOL.md`, with candidate names and SHA-256 hashes recorded in its `settings.prompts`. No repository-wide instruction fallback is used.

The controller then submits one batch request. `chess960`, `game_build` and `dataset_dashboard` are queued for every workspace. Each fresh benchmark child directory receives copies of those two selected documents before its development clock starts. The planner template points to that child's `PROJECT_PLANNER.md`; implementation, review and replan workers receive its `WORKER_PROTOCOL.md` in `request.workerProtocol`. The template and launch profile are snapshotted at enqueue time. Changed workspace document hashes prevent launch.

All planners connect to the shared coordinator at the profile's `coordinator_url` (normally port 4242). Each case registers its directory under the parent workspace, so board dropdown filtering groups its tasks there. The coordinator persists the case deadline and stops its scoped tasks and worker trees at cutoff. The benchmark owns and terminates the planner tree. Working directories start with only the two selected documents; launch prompts and logs live under `DATA/coordinator/RUN_ID` and task history lives in the shared coordinator data directory.

The scheduler dispatches cases and enforces deadlines. Planners choose tasks, monitor work and submit completed iterations. An early planner exit leaves workers their remaining allowance. New batches shuffle all workspace/benchmark pairs once before enqueueing, mixing benchmark types and workspace order. Optional `order_seed` reproduces that shuffle; otherwise a random seed is generated. The seed, algorithm version and resulting queue are persisted, so restart does not reshuffle. Benchmarks run sequentially within each workspace, with `max_parallel` limiting active cases per batch (default **8**). The scheduler skips busy workspaces while filling available slots. Queued cases have no ticking clock. Service restart marks active cases interrupted and stops their shared-board tasks while retaining accepted submissions. `batch-stop` stops active planners and scoped tasks and cancels queued cases.

Dashboard cases receive independent random dataset assignments when enqueued. Repeats are possible. The selected metadata, registry version and selection seed are saved in the case and run, so restart and later scoring retain the assignment. The default allowance is 900 seconds, including downloading. The five-source rotation is in `datasets.json`. USGS was excluded because it needs pagination, and World Bank was excluded after a timeout. All five retained sources passed two complete downloads and checksum comparisons.

Scoring is an explicit, separate step. The [batch scoring command](scoring.md) starts the chess tournament, launches fresh game/dashboard reviewers, calculates execution metrics and writes the combined workspace ranking without a supervising agent.

## Configuration

Run `scripts/setup-coordinator.ps1` to add the template session commands to the existing `codex` profile. Existing worker commands, model settings and other profiles are preserved. For another provider, set `session_planner_command` to an argv array whose process reads the exact UTF-8 prompt from `HARNESS_PLANNER_PROMPT`. It inherits `HARNESS_URL`, `HARNESS_RUN_ID`, `HARNESS_RUN_TOKEN` and `COORDINATOR_URL`. No admin token or deadline is supplied in that planner environment. `session_command` normally runs `scripts/template_session.py`.

The Codex adapter uses a fresh `codex exec --ephemeral` invocation, passes the template unchanged through stdin and uses the required workspace setting `planner`. The creator samples `planner` and `worker` independently from `luna_6_high` (`gpt-6-luna` / `high`), `sol_6_1_light` (`gpt-6.1-sol` / `low`) and `sol_6_1_medium` (`gpt-6.1-sol` / `medium`). The selected pair is always explicit in workspace settings and every launch. Workers use it for implementation, review and replanning, taking precedence over task overrides and global CLI defaults. The CLI runs with unattended access to this trusted local workspace; process ownership is not a filesystem security boundary.

Create workspaces with `agent_coordinator/scripts/create-workspace.ps1 -Workspace PATH`. Optional `-Planner` and `-Worker` pin either role to one eligible model/effort candidate; otherwise each is sampled independently. `-ProjectPlanner FILENAME` and `-WorkerProtocol FILENAME` select explicit prompt candidates; omitted choices are independently randomized per workspace, reproducibly with `-Seed`. `GET /api/prompt-candidates` lists valid candidates and invalid-file diagnostics; `POST /api/workspaces` accepts `path`, `settings`, `project_planner` and `worker_protocol` filenames and creates/copies/registers the workspace. Candidate edits affect future creations only. Candidate lists are discovered from the coordinator; empty prompt pools prevent creation.

Example `batch.json` (directories must contain only their selected documents; copy the actual hashes returned by workspace creation):

```json
{
  "profile": "codex",
  "template_path": "D:/harness_work/agent-harness-bench/planner_agent_benchmark_prompt_template",
  "workspace_root": "D:/harness_work/workspaces",
  "max_parallel": 8,
  "workspaces": [
    {"path": "D:/harness_work/workspaces/experiment_1", "settings": {"planner": "luna_6_high", "worker": "sol_6_1_light", "updates": "poll", "prompts": {"project_planner": {"candidate": "planner.md", "sha256": "ACTUAL_HASH"}, "worker_protocol": {"candidate": "worker.md", "sha256": "ACTUAL_HASH"}}}}
  ],
  "task_configs": {
    "chess960": {"development_seconds": 1200, "seed": 960},
    "game_build": {"development_seconds": 1800, "seed": 960},
    "dataset_dashboard": {"development_seconds": 900}
  }
}
```

Copy each workspace's settings from the coordinator configuration into the request. The same settings apply to all three benchmarks. Parameters, benchmark seed and prompts are retained for later analysis. Fresh runs require fresh workspace directories; the utility never deletes or reuses previous solutions.

```powershell
$token = '../runtime/benchmark-service/admin-token'
python scripts/harness.py --token-file $token benchmark-batch batch.json
python scripts/harness.py --token-file $token batch-status BATCH_ID
python scripts/harness.py --token-file $token batch-stop BATCH_ID
```

Admin routes: `POST /v1/benchmark-batches`, `GET /v1/benchmark-batches`, `GET /v1/benchmark-batches/ID`, `POST /v1/benchmark-batches/ID/stop`. The scoped planner route `GET /v1/runs/ID/task` returns the benchmark prompt without timing metadata. Existing snapshot and iteration APIs are unchanged. The development scheduler does not automatically submit or score; run the scoring command separately after development finishes.

Run `scripts/smoke_batch.py --binary PATH --coordinator PATH --evidence PATH` to verify the actual HTTP service, coordinators, scoped task access, exact prompts, copied workspace documents, settings, queue cap, deadline termination, late submissions, restart recovery and cancellation using deterministic local fixtures.

`harness.py batch-add-dashboard BATCH_ID` adds one dashboard case per workspace to an older batch, preserving existing cases. Repeating it returns the original assignments. `POST /v1/benchmark-batches/ID/retry` with `{ "benchmark": "game_build" }` explicitly retries terminal cases in fresh directories and preserves prior attempts in `previous_cases`.

Retry preserves the selected planner and worker values; it never substitutes a default model. Retired model selections remain readable in historical records but cannot start new workspace runs.
