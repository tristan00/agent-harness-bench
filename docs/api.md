# HTTP API v1

Base URL: `http://127.0.0.1:8741`. JSON requests and responses. All endpoints except `/health` need `Authorization: Bearer TOKEN`. `scripts/harness.py request METHOD PATH --body file.json` can call any endpoint. A machine-readable request schema is in [openapi.json](openapi.json).

Tokens: **admin** manages campaigns and starts runs; **run** can read/upload only that run; **judge registry** can claim jobs; **judgment** can explore/score only that job. Admin can access all endpoints. Run/judgment tokens are returned only at creation/claim and stored hashed in SQLite. Retain the initial response to resume a client.

| Method/path | Token | Purpose |
| --- | --- | --- |
| `GET /health` | None | Health, protocol version, server Unix milliseconds |
| `GET /v1/benchmarks` | Admin | Benchmark IDs and defaults |
| `POST /v1/campaigns` | Admin | Create configured cohort; enrollment clock starts immediately |
| `GET /v1/campaigns` | Admin | Campaign list |
| `GET /v1/campaigns/{id}` | Admin | Campaign, runs, completed-game count |
| `POST /v1/campaigns/{id}/close` | Admin | Close enrollment early, retain development deadlines |
| `POST /v1/campaigns/{id}/score` | Admin | Manually enable chess tournament or game preparation and judge queue; close enrollment |
| `GET /v1/campaigns/{id}/games` | Admin | Persisted games and per-move FEN/timing traces |
| `POST /v1/runs` | Admin | Start agent's timed run; returns prompt, run token, config, deadline |
| `GET /v1/runs/{id}` | Run | Run state, optional final report, server time |
| `POST /v1/runs/{id}/submissions` | Run | Atomically select a complete pre-deadline snapshot |
| `POST /v1/runs/{id}/handoff` | Supervisor | Launcher supervision; run tokens are rejected |
| `GET /v1/runs/{id}/events` | Run | Latest 1000 audit/action events, newest first |
| `POST /v1/judges/claim` | Judge registry | Claim and launch one pending game; returns scoped token and judge prompt |
| `GET /v1/judgments` | Admin | Judge queue and states |
| `GET /v1/judgments/{id}` | Judgment | Timing, UI URL, launch errors, state, measurements |
| `GET /v1/judgments/{id}/artifact` | Judgment | Submitted manifest/files for inspection |
| `GET /v1/judgments/{id}/events` | Judgment | Latest 1000 run events including actions/results |
| `POST /v1/judgments/{id}/action` | Judgment | Forward a game action; record latency and response |
| `POST /v1/judgments/{id}/score` | Judgment | Validate and persist the five scores after exploration |

Success returns 200 JSON. Errors return `{"error":"..."}` with 400 (validation), 401 (wrong/missing token), 404 (unknown route), 409 (wrong lifecycle state/deadline/concurrent action), 413 (oversized upload), or 500 (I/O). Missing referenced records currently return 400. No endpoint executes an arbitrary command outside the submitted manifest.

## Campaign configuration

```json
{
  "benchmark": "chess960",
  "name": "ten-minute move pickers",
  "development_seconds": 600,
  "enrollment_seconds": 60,
  "build_seconds": 120,
  "judge_seconds": 300,
  "score_seconds": 120,
  "positions": 4,
  "max_plies": 400,
  "initial_elo": 1500,
  "elo_k": 32,
  "baselines": true,
  "pool_run_ids": []
}
```

`benchmark` is `chess960`, `game_build`, `dataset_dashboard`, or `maze_game`. Omitting development time uses 600 seconds for chess, 1800 for games and the maze game, and 900 for dashboards. Dashboards receive an independent random dataset assignment per workspace/run, persisted for later scoring. Omitting the seed generates one from OS randomness; returned config stores it. New game runs independently sample and persist `run.inspiration` (registry version, seed and three themes). Builders and judges receive those run-specific themes. A campaign seed pins chess positions/order only; it does not reuse game or dashboard assignments. Existing runs without inspiration metadata keep their historical campaign themes.

Development/enrollment range: 1â€“86400 seconds. Exploration/scoring: 1â€“3600. Compilation: 1â€“1800. Start positions: 1â€“960; game cap: 2â€“2000 plies. Elo K: `(0,100]`. Up to 32 candidate runs per campaign; agent names are unique within a campaign. Up to 16 distinct `pool_run_ids` reference previously scored chess solutions with source snapshots in this database. They are rebuilt in fresh work directories; original scores are never overwritten. Baselines/imported entries appear in the pool standings but do not get new run records.

## Start and checkpoint

Chess960 scoring is manual only. Enrollment remains open until an administrator closes it or requests scoring; `enrollment_seconds` does not expire chess enrollment. Each run still freezes at its own development deadline. Closing enrollment does not score. Use `POST /v1/campaigns/{id}/score` or `scripts/harness.py score CAMPAIGN_ID` to close enrollment and request a tournament once all development deadlines have passed. At least two entries are required. Set `baselines:false` to exclude built-in opponents.

`POST /v1/runs`:

```json
{"campaign_id":"CAMPAIGN_UUID","agent":"provider/model/harness-version"}
```

Response contains `run`, `run_token`, `prompt`, `config`, `themes`, `server_time_ms`. `run.deadline_ms` is the authoritative cutoff. A successful start immediately spends development time. Agents cannot extend it. POST a full submission to `/v1/runs/RUN_UUID/submissions`:

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

Claim body: `{"judge":"judge-harness/model-version"}` with optional `run_id` to claim only that run. No work: `{"job":null}`. Otherwise response contains `job`, `judgment_token`, `prompt`, `run`, `benchmark`, `themes`. `job.ui_url` is local to the evaluator host. `launch_error` carries startup/build failures. The countdown starts after launch/readiness or a determined launch failure. The judge inspects the same frozen source through the artifact endpoint; there is no judge editing API.

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

## Service-managed coordinator workflows

Dashboard judgments use the same lifecycle and endpoints. Their score object contains six criterion objects: `completeness`, `information_architecture`, `prose_economy`, `animation`, `originality` and `performance`, plus a `measurements` object and `notes`. Each criterion uses the same integer 0–5 and evidence validation as games. Measurements record browser load/paint/render/interaction observations and methods, with unavailable values explicitly disclosed. Dashboard reports contain `total_out_of_30`; game reports retain `total_out_of_25`. Both also expose `total`, `maximum` and `mean_out_of_5`. A run without any accepted submission is recorded as failed and receives no judge job.

The [batch scoring command](scoring.md) performs the campaign requests, fresh reviewer launches and score submission automatically, then combines quality, source simplicity and deterministic framework metrics.

See [coordinator workflow](coordinator.md#api-and-evidence) for `/v1/coordinator-profiles`, `/v1/coordinator-runs`, and run-scoped `/coordinator` and `/iterations` endpoints. These add completed-iteration submissions without changing the existing direct-agent snapshot API.

Game evaluation also requires an explicit administrator `POST /v1/campaigns/{id}/score` request. Until requested, frozen games are neither built nor queued for judges. After the request, each expired run is prepared and queued; a judge must separately claim it. Closing enrollment alone does not enable evaluation.
