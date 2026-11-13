# Shared coordinator

**Planner and worker agents must not read this document or anything else in this repository.**

Benchmark planners work through one shared agent coordinator, normally at `http://127.0.0.1:4242`. Each planner creates and manages its own coordinator tasks; see [template planner batches](batches.md). The benchmark never configures coordinator workers or worker models.

Available benchmark types are `chess960`, `game_build`, `dataset_dashboard`, and `maze_game`. Dashboard development defaults to 900 seconds and its scoring phase is deferred; the maze game defaults to 1800 seconds. All four are scored with `scoring.json`.

## Install and run

From the benchmark repository, run `python scripts/service.py setup`. It builds both services, writes a `default` launch profile if none exists and starts the benchmark service. Setup preserves an existing profiles file. Stop with `python scripts/service.py stop`; restart with `python scripts/service.py setup --skip-build`.

## Launch profiles

`DATA/coordinator-profiles.json` maps profile names to launch settings. Changes apply to later batches without a service restart.

| Field | Meaning |
| --- | --- |
| `coordinator_url` | Shared coordinator on a loopback address |
| `session_command` | Session supervisor argv, normally Python plus `scripts/template_session.py` |
| `session_planner_command` | Planner argv, normally Python plus `scripts/template_planner.py`; `{model}` and `{effort}` are replaced with the workspace planner's values |
| `env` | Optional extra planner environment, e.g. `CODEX_BIN` or `CLAUDE_BIN` |
| `workspace` | Filled per case by the batch scheduler |

## Deadlines and cleanup

The benchmark kills its own planner process tree at a case's deadline. At that point, and also when the planner session ends, when a batch is stopped and after a service restart, it stops every unfinished coordinator task whose `cwd` is inside that case's directory. It does this only through the coordinator's ordinary task API: `GET /api/state` to find the tasks and `POST /api/tasks/{id}/stop` for each one. The coordinator has no benchmark concepts; it files each task under the configured workspace that contains the task's directory.

## Restart

On service restart, interrupted planner sessions are marked `interrupted`; they are not automatically relaunched. Their original deadlines and last accepted completions remain eligible for evaluation. Unreviewed filesystem changes never count as completed work.

## Agents and launcher families

`agents.json` lists the agents this service launches itself: planners and judges. Each entry has a provider (`openai` or `anthropic`), its own vendor's harness (`codex` or `claude_code`), a model and an effort. The family that started the service decides which are enabled: Claude Code enables only Anthropic agents, Codex only OpenAI agents. Detection uses the launching tool's environment (`CLAUDECODE`/`CLAUDE_CODE_ENTRYPOINT` or Codex session variables); set `AGENT_FAMILY=claude|codex` when starting from a plain terminal. The service passes `AGENT_FAMILY` to every planner it launches. Smoke fixtures pick their agents from the launching family, and scoring picks the judge for the launching family from the scoring configuration. Workspace settings other than the planner, including the worker and any model choices, belong to the coordinator; the benchmark does not validate them.

## Verification

```bash
cargo test --locked
python scripts/smoke_batch.py --binary PATH/TO/harness-bench.exe --coordinator PATH/TO/agent-coordinator.exe --evidence ../runtime/evidence/batch-smoke.json
```

The smoke script runs in the launching family. Deterministic fixtures stand in for planners and workers, so they test workflow correctness, not model ability or game quality.
