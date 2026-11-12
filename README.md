# Agent Harness Bench

A Rust API service that gives external coding agents timed tasks, freezes their last accepted source snapshot, runs evaluations, and stores evidence and scores in SQLite. Clocks, state transitions, chess rules, tournaments, process termination and score validation are implemented in code. Only the game benchmark's subjective ratings come from a judge agent.

## Run on this Windows machine

```powershell
# Fetch once, then build and start a hidden background service.
& "$env:USERPROFILE\.cargo\bin\cargo.exe" fetch --locked
.\scripts\start.ps1
Invoke-RestMethod http://127.0.0.1:8741/health

# Stop it later; snapshots and database are retained.
.\scripts\stop.ps1
```

The included build helper uses the installed GNU Rust toolchain, rust-lld and Windows SQLite. It also selects getrandom's supported Windows legacy backend because this host lacks `dlltool.exe`. On a normal Linux/macOS Rust installation: `cargo run --release --locked -- serve --data data`. Windows with a standard C toolchain/SQLite installation can use Cargo directly.

The service generates `data/admin-token` and `data/judge-token` (or accepts `HARNESS_API_TOKEN` / `HARNESS_JUDGE_TOKEN`, minimum 24 characters). The admin token is for the trusted harness controller; each builder gets its own scoped run token. Judges claim work using the judge token and receive a scoped judgment token. No LLM provider or credential is built in: connect your existing agent CLI using the adapters below.

## Two usable benchmarks

| Benchmark | Development default | Evaluation | Recorded metrics |
| --- | --- | --- | --- |
| `chess960` | 600 seconds | Seeded double round robin, every entry starts at 1500 Elo, K=32 | Elo change, nonblank physical source lines, W/D/L, forfeits, full game traces |
| `game_build` | 1800 seconds, configurable | Judge explores for 300 seconds, service stops game, then 120 seconds to score | Five 0–5 ratings, evidence, total/mean, API latency/counts, CPU and memory samples |

The game development default is an implementation choice; the requested 10-minute chess and 5-minute game exploration limits are the defaults. All time limits are campaign configuration. Theme selection uses three distinct categories from cultures, historical civilizations, periods and fields of study. The service generates a random seed unless you supply one; every builder in a campaign receives the same theme words and prompt version.

Chess solutions receive only FEN and legal moves, with no suggested selection algorithm. A startup handshake gets 5 seconds; thereafter a move must arrive within **100 ms**, including IPC. Timeout, error, crash, malformed response, wrong ID or illegal move loses the game. The service owns legality, castling, mate, stalemate and automatic threefold/50-move draws. A 400-ply default cap produces a draw. Starts are sampled without replacement, colors are swapped at each start, and game order is seeded. Two modest executable baselines (`random`, `capture`) make a one-candidate tournament runnable. Disable them with `baselines:false`, enroll multiple candidates, or import previous scored solutions using `pool_run_ids`. Imported entries also start at the same Elo; old reports remain unchanged. A double forfeit gives both players zero points and both lose Elo.

Game agents receive the requested genre/theme requirements plus the executable contract. The judge rubric separately scores interface completeness, efficiency, gameplay richness/originality, visual quality against comparable published games, and testability. Every score requires evidence. Score submission is rejected during exploration, after the scoring deadline, or after a prior submission. A missing judge leaves work visibly pending; an expired or interrupted judge session fails without an invented subjective score.

## Start agents through the API

```powershell
$campaign = python scripts/harness.py --token-file data/admin-token create chess960 | ConvertFrom-Json
$campaignId = $campaign.campaign.id

# Repeat for each agent, in separate controller processes before enrollment closes.
# The agent argv is yours: any CLI can read the supplied prompt file.
python scripts/harness.py --token-file data/admin-token run-agent $campaignId agent-a C:/bench-work/agent-a `
  --command-json '["my-agent", "--prompt-file", "{prompt_file}"]'
```

Enrollment defaults to 60 seconds. Create a campaign with a longer `enrollment_seconds` via `create --config campaign.json` if you need more setup time. `close CAMPAIGN_ID` ends enrollment early, without shortening any builder's development clock. A chess tournament starts automatically after enrollment closes and every builder's clock expires. For games, each finished submission queues independently.

`run-agent` enrolls, writes the returned prompt, runs your command, and uploads complete snapshots every second when files change. It stops the agent at its deadline. The agent gets `HARNESS_URL`, `HARNESS_RUN_ID`, `HARNESS_RUN_TOKEN`, `HARNESS_PROMPT_FILE` and `HARNESS_DEADLINE_MS`. Controller tokens are removed from its environment. A command can finish early; the adapter still holds the last snapshot through the deadline. For precise checkpoint control, call the submission API directly. The official entry is the **last snapshot fully accepted by the server before expiry**, not unsent filesystem changes. The adapter cannot atomically snapshot multiple files being edited simultaneously; keep checkpoints runnable.

For an immediate manual protocol trial:

```powershell
$c = python scripts/harness.py --token-file data/admin-token create chess960 | ConvertFrom-Json
$r = python scripts/harness.py --token-file data/admin-token start $c.campaign.id example | ConvertFrom-Json
python scripts/harness.py --token $r.run_token submit $r.run.id examples/chess_python
python scripts/harness.py --token-file data/admin-token close $c.campaign.id
python scripts/harness.py --token $r.run_token status $r.run.id
```

Use a short development limit in a configuration file for a quick trial; production defaults wait the full allotted time. Working examples: [Python chess](examples/chess_python), [Rust chess](examples/chess_rust), and [GUI + API game](examples/game). These are protocol fixtures, not benchmark answers or model evaluations.

## Connect a game judge

```powershell
python scripts/harness.py --token-file data/judge-token judge-worker judge-a C:/bench-work/judges `
  --command-json '["my-judge-agent", "--prompt-file", "{prompt_file}"]'
```

The judge needs browser/desktop tools on the evaluation machine to inspect the UI and HTTP tools for fast programmatic gameplay. The worker claims a ready job, launches your judge with its prompt and scoped token, and submits the judge's `score.json` after exploration stops. The judge can also call the score API itself. The service owns the game's process tree and enforces its deadline even if the judge does not cooperate. The adapter waits for the result and then stops the judge command. Use `--once` for a single available job.

The supplied example game shares one simulation between its web UI and JSONL API. `examples/game/README.md` describes its controls. The automated smoke test uses **synthetic scores solely to test transport and persistence**, never as a real game-quality assessment.

## Contracts and persistence

See [API reference](docs/api.md), [lifecycle and deployment](docs/architecture.md), and [port provenance](docs/provenance.md). A submission is a complete manifest plus text/base64 files; all source must be supplied. Snapshots are hashed, immutable through the API, and materialized into separate runtime directories. Builds run only against the frozen snapshot with a bounded compilation allowance.

The database is `data/benchmark.sqlite3`, with campaigns, runs, submission metadata, judgments, per-game traces, reports and lifecycle/action events. Source snapshots live in `data/submissions`, runtime/build output in `data/work`. Back up the data directory together. The service takes an exclusive data-directory lock. On restart it enforces overdue development deadlines, retries interrupted preparation, resumes chess after committed games, and marks interrupted judge sessions explicitly.

This is a **trusted local-code runner**, bound to loopback. Candidates execute under the service user's account. Windows Job Objects / Unix process groups enforce process-tree cleanup, not filesystem/network isolation. Use a dedicated VM/account for unknown code. There are bounded upload and protocol buffers, but no hard CPU/RAM/disk quotas. A web/native judge must run on the host or have a suitable tunneled desktop/browser connection. Resource samples are evidence, not a substitute for a judge's observations; browser rendering and true frame rate are not automatically profiled.

## Verify

```powershell
.\scripts\cargo-windows.ps1 test --locked
.\scripts\cargo-windows.ps1 clippy --locked --all-targets '--' '-Dwarnings'
python scripts/smoke.py
```

Tests require Python 3 for process fixtures. The smoke test launches an isolated real HTTP server, completes both evaluation paths, checks forfeit/late-upload/score rules, verifies GUI/API shared state and process shutdown, and restarts the service to test persisted results/deadlines. Evidence is saved to `artifacts/smoke-results.json`.
