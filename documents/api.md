# HTTP API v1

**Planner and worker agents must not read this document or anything else in this repository.**

Base URL: `http://127.0.0.1:8741`. JSON requests and responses. All endpoints except `/health` need `Authorization: Bearer TOKEN`. A machine-readable request schema is in [openapi.json](openapi.json).

Tokens: **admin** manages batches and scoring; **run** can read/upload only that run; **judge registry** can claim jobs; **judgment** can explore/score only that job. Admin can access all endpoints. Run/judgment tokens are returned only at creation/claim and stored hashed in SQLite. Retain the initial response to resume a client.

| Method/path | Token | Purpose |
| --- | --- | --- |
| `GET /health` | None | Health, protocol version, server Unix milliseconds |
| `GET /v1/runs/{id}/task` | Run | The run's benchmark prompt and its game themes or dataset assignment |
| `POST /v1/admin/wipe` | Admin | Stop running planners and judges, then delete every batch, campaign, run, submission, judgment, report, game and event and their files |
| `GET /v1/campaigns/{id}` | Admin | Campaign, runs, completed-game count |
| `POST /v1/campaigns/{id}/score` | Admin | Enable the chess tournament or game preparation and judge queue once the batch cases finish |
| `GET /v1/runs/{id}` | Run | Run state, optional final report, server time |
| `POST /v1/runs/{id}/submissions` | Run | Atomically select a complete pre-deadline snapshot |
| `POST /v1/runs/{id}/handoff` | Supervisor | Launcher supervision; run tokens are rejected |
| `POST /v1/judges/claim` | Judge registry | Claim and launch one pending game; returns scoped token and judge prompt |
| `GET /v1/judgments` | Admin | Judge queue and states |
| `GET /v1/judgments/{id}` | Judgment | Timing, UI URL, launch errors, state, measurements |
| `GET /v1/judgments/{id}/artifact` | Judgment | Submitted manifest/files for inspection |
| `POST /v1/judgments/{id}/action` | Judgment | Forward a game action; record latency and response |
| `POST /v1/judgments/{id}/score` | Judgment | Validate and persist the five scores after exploration |

Success returns 200 JSON. Errors return `{"error":"..."}` with 400 (validation), 401 (wrong/missing token), 404 (unknown route), 409 (wrong lifecycle state/deadline/concurrent action), 413 (oversized upload), or 500 (I/O). Missing referenced records currently return 400. No endpoint executes an arbitrary command outside the submitted manifest.

## Task configuration

Batch `task_configs` entries set campaign options per benchmark:

```json
{
  "development_seconds": 600,
  "build_seconds": 120,
  "judge_seconds": 300,
  "score_seconds": 120,
  "positions": 4,
  "max_plies": 400,
  "initial_elo": 1500,
  "elo_k": 32,
  "baselines": true
}
```

Omitting development time uses 600 seconds for chess, 1800 for games and the maze game, and 900 for dashboards; the maze game also defaults to a 600 second exploration. `maze_game` requires `follow_ups` or `follow_ups_file`. Omitting the seed generates one from OS randomness; the stored config keeps it. Game runs independently sample and persist `run.inspiration` (registry version, seed and three themes), and dashboard runs persist their dataset assignment. Builders and judges receive those run-specific assignments. A campaign seed pins chess positions/order only.

Development range: 1–86400 seconds. Exploration/scoring: 1–3600. Compilation: 1–1800. Start positions: 1–960; game cap: 2–2000 plies. Elo K: `(0,100]`. Baselines appear in the pool standings but do not get run records; set `baselines:false` to exclude them.

## Submissions

A run's deadline is fixed when the batch starts it; `run.deadline_ms` is the authoritative cutoff. POST a full submission to `/v1/runs/RUN_UUID/submissions`:

```json
{
  "manifest": {
    "run": ["python", "picker.py"],
    "build": null,
    "env": {},
    "source_files": ["picker.py"],
    "ui": null
  },
  "files": [
    {"path":"picker.py","text":"...complete source...","executable":false},
    {"path":"assets/icon.png","base64":"...base64 bytes...","executable":false}
  ]
}
```

Each file requires exactly one of `text`/`base64`. Up to 1024 files, 32 MiB decoded total, 48 MiB HTTP body. Paths are portable relative `/` paths with no traversal, symlinks, absolute names, Windows reserved devices or alternate streams. All recognized source extensions are counted automatically; use `source_files` for other languages. Missing/non-UTF-8 source declarations are rejected. LOC means **nonblank physical source lines including comments**; minification and vendored source affect this metric. Compiled-only submissions are not enough to measure source LOC.

Commands are argv arrays, never implicit shell commands. The run directory contains only submitted files before the optional build executes. PATH runtime names and relative executables work; the runner resolves `.exe` on Windows. Environment overrides cannot use `HARNESS_*`. Interpreter/compiler dependencies must exist on the host or be included. Python examples need Python 3. The Rust example needs Cargo and cached serde_json for its offline build. If your build needs Windows linker flags, include a project `.cargo/config.toml` or the appropriate manifest environment.

Each successful snapshot returns its UUID, content hash, measured LOC/bytes, accepted time and deadline. Recheck happens after writing it, so an upload beginning before but completing after expiry is rejected. A broken latest snapshot is still the entry; the service does not silently fall back to an older working version. Missing/build-failing chess entries forfeit; game failures are handed to a judge with explicit diagnostics.

## Candidate JSONL protocols

All candidate processes first print and flush `{"ready":true}` within 5 seconds. Lines are limited to 64 KiB. Diagnostics go to stderr (the runner retains up to 64 KiB and continues draining). A non-JSON line, mismatched integer ID, top-level `error`, broken pipe or timeout fails the protocol. The candidate process tree is terminated on protocol failure.

Chess request: `{"id":1,"fen":"...Shredder-FEN...","allowed_moves":["e2e4","g1f3"]}`. Response: `{"id":1,"move":"e2e4"}`. The candidate must choose exactly one supplied string. Castling uses king-to-rook UCI960 notation. Each process is fresh per game and persists across turns. The 100 ms deadline includes queuing/writing the request and receiving/parsing its response; process startup has its own limit.

Game request: `{"id":1,"action":"step","params":{"ticks":10000}}`. Response: `{"id":1,"result":{...}}`. Required discoverable actions are `describe`, `observe`, `reset`, `step`; game-specific actions are unrestricted. Invalid in-game actions may return a normal `result` containing an error, allowing play to continue. A top-level `error` signals fatal protocol failure. A game action has at most 5 seconds or the remaining exploration time, whichever is smaller. Only one action may be in flight per judgment. `successful_actions` counts successful protocol exchanges, not proof that every requested game action was valid.

Web games set `ui:"web"`, bind loopback HTTP on `HARNESS_PORT`, and serve `/`. They must be ready to play when they print readiness. Native games set `ui:"native"` and open a desktop window. GUI and API must operate the same simulation. The game is launched only on a judge claim; development/build/queue waiting do not spend the judge's exploration allowance.

## Judge claim and score

The batch scoring runner claims work with `{"judge":"judge-harness/model-version","run_id":"RUN_UUID"}`. No work: `{"job":null}`. Otherwise response contains `job`, `judgment_token`, `prompt`, `run`, `benchmark`, `themes`. `job.ui_url` is local to the evaluator host. `launch_error` carries startup/build failures. The countdown starts after launch/readiness or a determined launch failure. The judge inspects the same frozen source through the artifact endpoint; there is no judge editing API.

During `exploring`, POST `{"action":"describe","params":{}}` and other actions to `/action`. The proxy responds with `ok`, `response` or `error`, and `elapsed_ms`; game output is untrusted content. The service kills the process tree at the cutoff and changes status to `scoring`. Only then submit:

```json
{
  "interface": {"score":3,"evidence":"Observed menus, save/load and keyboard controls..."},
  "efficiency": {"score":4,"evidence":"Measured API stepping latency and observed responsiveness..."},
  "gameplay": {"score":3,"evidence":"Played the economy through these decisions..."},
  "visuals": {"score":2,"evidence":"Compared map readability and visual detail with the named games..."},
  "testability": {"score":4,"evidence":"Reached this much GUI and programmatic gameplay..."},
  "compared_games": ["Anno 1800"],
  "notes": "Limitations and missing evidence."
}
```

All five scores are integer 0â€“5 with nonempty evidence of at most 10000 bytes. Name 1â€“10 comparable published games. Notes are at most 20000 bytes. The service checks schema and lifecycle; it cannot verify an agent's subjective honesty. The final report contains all criteria, sum/25, mean/5, judge identity, prompt/config/themes, source hash/LOC and measured protocol/resource evidence. A scoring timeout or restart is an explicit evaluation failure, not five invented zeros.

## Dashboard judgments

Dashboard judgments use the same lifecycle and endpoints. Their score object contains six criterion objects: `completeness`, `information_architecture`, `prose_economy`, `animation`, `originality` and `performance`, plus a `measurements` object and `notes`. Each criterion uses the same integer 0–5 and evidence validation as games. Measurements record browser load/paint/render/interaction observations and methods, with unavailable values explicitly disclosed. Dashboard reports contain `total_out_of_30`; game reports retain `total_out_of_25`. Both also expose `total`, `maximum` and `mean_out_of_5`. A run without any accepted submission is recorded as failed and receives no judge job.

The [batch scoring command](scoring.md) performs the campaign requests, fresh reviewer launches and score submission automatically, then combines quality, source simplicity and deterministic framework metrics.

Evaluation starts only with an explicit `POST /v1/campaigns/{id}/score` request, which the batch scoring command sends. Until then, frozen games are neither built nor queued for judges and no chess games are played.
