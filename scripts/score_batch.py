"""Score final submissions and execution evidence for a completed benchmark batch."""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager
import hashlib
import html
import json
import math
import os
from pathlib import Path
import time

from agent_selection import command_arguments, model_effort
from batch_metrics import aggregate, collect, read, score
from harness import ApiError, Client, agent_environment, spawn_agent, stop_agent, write_json

ROOT = Path(__file__).resolve().parents[1]


@contextmanager
def output_lock(path):
    with path.open("a+b") as handle:
        if not path.stat().st_size:
            handle.write(b"0")
            handle.flush()
        handle.seek(0)
        try:
            if os.name == "nt":
                import msvcrt
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            raise RuntimeError("Another scoring command already owns this output directory") from error
        try:
            yield
        finally:
            handle.seek(0)
            if os.name == "nt":
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def validate_config(config):
    groups = {"category_weights": {"result", "simplicity", "framework"},
              "benchmark_weights": {"chess960", "game_build", "dataset_dashboard"},
              "framework_weights": {"task_count", "relations", "parallelism", "token_efficiency"},
              "references": {"elo_scale", "source_lines", "task_count", "relation_types", "edges_per_task", "tokens"}}
    for name, keys in groups.items():
        values = config[name]
        if set(values) != keys or any(not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0 for value in values.values()):
            raise ValueError(f"Invalid scoring configuration: {name}")
    if config["incomplete_token_fallback"] != "agent_seconds_per_development_second":
        raise ValueError("Unsupported token fallback")
    model_effort(config["judge_selection"])


def judge_command(selection, prompt_path, output_path, browser_version):
    browser_args = ["-y", f"@playwright/mcp@{browser_version}", "--isolated", "--headless", "--browser", "chrome",
                    "--output-dir", str(prompt_path.parent / "browser")]
    return [os.environ.get("CODEX_BIN", "codex"), "exec", "--ephemeral", "--skip-git-repo-check", "--json",
            "--sandbox", "danger-full-access", "-c", 'approval_policy="never"', "--disable", "memories",
            *command_arguments(selection), "-c", "mcp_servers.chrome-devtools.enabled=false",
            "-c", 'mcp_servers.playwright.command="npx"', "-c", "mcp_servers.playwright.enabled=true",
            "-c", "mcp_servers.playwright.args=" + json.dumps(browser_args),
            "--output-last-message", str(output_path), prompt_path.read_text(encoding="utf-8")]


def review_case(args, case, client, output, config):
    run_id = case["run_id"]
    if not run_id:
        return None
    state = client.request("GET", f"/v1/runs/{run_id}")
    if state["report"]:
        return state["report"]
    if not state["run"].get("submission_id"):
        return None
    directory = output / "judges" / run_id
    directory.mkdir(parents=True, exist_ok=True)
    claimed = None
    until = time.monotonic() + args.timeout
    while time.monotonic() < until:
        claimed = client.request("POST", "/v1/judges/claim", {"judge": f"batch:{args.batch}:{args.judge}", "run_id":run_id})
        if claimed["job"]:
            break
        jobs = client.request("GET", "/v1/judgments")["judgments"]
        previous = next((job for job in jobs if job["run_id"] == run_id), None)
        if previous and previous["status"] == "completed":
            return client.request("GET", f"/v1/runs/{run_id}")["report"]
        if previous and previous["status"] in {"timed_out", "interrupted", "completed"}:
            raise RuntimeError(f"Judge session already ended for {run_id}: {previous['status']}")
        time.sleep(0.25)
    if not claimed or not claimed["job"]:
        raise TimeoutError(f"No judge job became available for {run_id}")
    job = claimed["job"]
    scoped = Client(args.url, claimed["judgment_token"])
    score_path = directory / "score.json"
    artifact = scoped.request("GET", f'/v1/judgments/{job["id"]}/artifact')
    write_json(directory / "submission.json", artifact)
    prompt_path = directory / "prompt.txt"
    details = (
        f"\nReview only this submission. Benchmark: {case['benchmark']}. UI: {job['ui_url']}. "
        f"Launch error: {job['launch_error']}. Use the Playwright MCP tools for an isolated browser. "
        "Capture actual screenshots and exercise the UI before rating it. Save screenshots in your current directory. "
        "Use browser evaluation to record navigation/paint timings and requestAnimationFrame intervals during interaction. "
        "Use the frozen submission.json here for source/data verification. Do not inspect other workspaces or solutions. "
        "Do not change implementation or repair launch failures. Agent framework and code simplicity are scored by code elsewhere. "
        f"Your scoped benchmark API URL is {args.url}; token is in HARNESS_JUDGMENT_TOKEN. "
        f"GET /v1/judgments/{job['id']} reports exploration/scoring status. "
        f"POST /v1/judgments/{job['id']}/action takes {{\"action\":\"describe\",\"params\":{{}}}} or observe and other actions. "
        f"Write your score object to {score_path} after collecting evidence; the runner submits it when scoring opens. "
        "Do not spend your whole session waiting for scoring to open. You may finish after writing score.json. "
        "Give concrete evidence for every criterion and disclose unavailable measurements. "
        "The runner records your session and will not invent missing ratings.\n"
    )
    prompt_path.write_text(claimed["prompt"] + details, encoding="utf-8")
    command = judge_command(args.judge, prompt_path, directory / "last-message.txt", config["playwright_mcp_version"])
    write_json(directory / "review-metadata.json", {"run_id":run_id, "judgment_id":job["id"],
               "model":model_effort(args.judge)[0], "effort":model_effort(args.judge)[1],
               "prompt_sha256":hashlib.sha256(prompt_path.read_bytes()).hexdigest(),
               "artifact_id":state["run"]["submission_id"], "fresh_session":True, "browser_isolated":True})
    env = agent_environment(HARNESS_JUDGMENT_TOKEN=claimed["judgment_token"], HARNESS_URL=args.url)
    env.pop("CODEX_WINDOWS_REGISTERED_CORE", None)
    process = None
    try:
        with (directory / "agent.jsonl").open("wb") as log:
            process = spawn_agent(command, directory, env, log)
        while time.monotonic() < until:
            job_state = scoped.request("GET", f'/v1/judgments/{job["id"]}')["job"]
            if job_state["status"] == "completed":
                return client.request("GET", f"/v1/runs/{run_id}")["report"]
            if job_state["status"] in {"timed_out", "interrupted"}:
                raise RuntimeError(f"Judge {run_id} ended with {job_state['status']}; inspect {directory}")
            if job_state["status"] == "scoring" and score_path.exists():
                try:
                    result = scoped.request("POST", f'/v1/judgments/{job["id"]}/score', read(score_path))
                    write_json(directory / "report.json", result["report"])
                    return result["report"]
                except (ValueError, ApiError) as error:
                    (directory / "score-error.txt").write_text(str(error), encoding="utf-8")
            if process.poll() is not None and not score_path.exists():
                raise RuntimeError(f"Judge exited {process.returncode} without scores; inspect {directory}")
            time.sleep(0.5)
        raise TimeoutError(f"Judge timeout for {run_id}")
    finally:
        stop_agent(process)


def write_html(path, report):
    rows = "".join("<tr><td>" + html.escape(Path(row["workspace"]).name) + "</td>" +
                   "".join(f"<td>{row['benchmarks'][name]:.2f}</td>" for name in report["config"]["benchmark_weights"]) +
                   f"<td><strong>{row['score']:.2f}</strong></td></tr>" for row in report["ranking"])
    cases = "".join(f"<tr><td>{html.escape(Path(case['workspace']).name)}</td><td>{case['benchmark']}</td>" +
                    "".join(f"<td>{case['categories'][key]:.2f}</td>" for key in ["result","simplicity","framework"]) +
                    f"<td>{case['source_lines'] if case['source_lines'] is not None else 'Missing'}</td><td>{case['status']}</td></tr>"
                    for case in report["cases"])
    path.write_text("""<!doctype html><html lang="en"><meta charset="utf-8"><title>Workspace benchmark scores</title>
<style>body{font:16px system-ui;max-width:1200px;margin:40px auto;padding:0 24px;background:#f5f6f8;color:#17202c}table{border-collapse:collapse;width:100%;background:white;margin:24px 0}td,th{text-align:left;padding:12px;border-bottom:1px solid #ddd}h1{font-size:28px}small{color:#526273}</style>
<h1>Workspace benchmark scores</h1><p>Each benchmark has equal weight. Within each benchmark, result, simplicity, and framework efficiency have equal weight. Scores are out of 100.</p>
<table><thead><tr><th>Workspace</th><th>Chess960</th><th>Game</th><th>Dashboard</th><th>Overall</th></tr></thead><tbody>""" + rows +
                    "</tbody></table><h2>Components</h2><table><tr><th>Workspace</th><th>Benchmark</th><th>Result</th><th>Simplicity</th><th>Framework</th><th>Source lines</th><th>Status</th></tr>" + cases +
                    "</table><p>Token scoring mode: " + html.escape(report["token_scoring_mode"]) +
                    ". Recorded partial token totals and full execution metrics are in scores.json.</p><small>" + html.escape(report["config"]["version"]) + "</small></html>", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("batch")
    parser.add_argument("--data", type=Path, required=True)
    parser.add_argument("--coordinator-data", type=Path, required=True)
    parser.add_argument("--url", default="http://127.0.0.1:8741")
    parser.add_argument("--judge", required=True, choices=["sol_light"])
    parser.add_argument("--max-judges", type=int, default=2)
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument("--config", type=Path, default=ROOT / "scoring-v1.json")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.max_judges < 1 or args.timeout < 1:
        parser.error("max-judges and timeout must be positive")
    args.data = args.data.resolve()
    config = read(args.config)
    validate_config(config)
    if config["judge_selection"] != args.judge:
        parser.error("judge selection must match scoring config")
    output = (args.output or args.data / "batch-scores" / args.batch / config["version"]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    with output_lock(output / ".scoring.lock"):
        score_batch(args, output, config)


def score_batch(args, output, config):
    client = Client(args.url, (args.data / "admin-token").read_text().strip())
    batch = client.request("GET", f"/v1/benchmark-batches/{args.batch}")["batch"]
    deferred = [case for case in batch["cases"] if case["benchmark"] not in config["benchmark_weights"]]
    batch["cases"] = [case for case in batch["cases"] if case["benchmark"] in config["benchmark_weights"]]
    batch["campaigns"] = {name: campaign for name, campaign in batch["campaigns"].items() if name in config["benchmark_weights"]}
    if any(case["status"] in {"queued","running"} for case in batch["cases"]):
        raise ValueError("Development must finish before scoring")
    if (output / "config.json").exists() and read(output / "config.json") != config:
        raise ValueError("Scoring config changed; use a new version or output directory")
    if (output / "batch.json").exists() and read(output / "batch.json")["cases"] != batch["cases"]:
        raise ValueError("Selected batch attempts changed; use a new output directory")
    write_json(output / "batch.json", batch)
    write_json(output / "config.json", config)
    if (output / "metrics.json").exists():
        metrics = read(output / "metrics.json")
        identity = lambda item: (item["workspace"], item["benchmark"], item["run_id"] or "")
        if sorted(map(identity, metrics)) != sorted(map(identity, batch["cases"])):
            raise ValueError("Saved metrics do not match the selected batch attempts")
    else:
        metrics = []
        for case in batch["cases"]:
            run = client.request("GET", f'/v1/runs/{case["run_id"]}')["run"] if case["run_id"] else None
            metrics.append(collect(case, run, args.data, args.coordinator_data))
        write_json(output / "metrics.json", metrics)
    for campaign_id in batch["campaigns"].values():
        if not any(case["campaign_id"] == campaign_id and case["run_id"] for case in batch["cases"]):
            continue
        status = client.request("GET", f"/v1/campaigns/{campaign_id}")["campaign"]["status"]
        if status in {"enrolling","awaiting_score","waiting"}:
            client.request("POST", f"/v1/campaigns/{campaign_id}/score", {})
    reviewed = {}
    errors = []
    cases = [case for case in batch["cases"] if case["benchmark"] != "chess960"]
    with ThreadPoolExecutor(max_workers=args.max_judges) as pool:
        futures = {pool.submit(review_case, args, case, client, output, config): case for case in cases}
        for future in as_completed(futures):
            case = futures[future]
            try:
                reviewed[case["run_id"]] = future.result()
                print(json.dumps({"run_id":case["run_id"], "status":"reviewed" if reviewed[case["run_id"]] else "no_submission",
                                  "benchmark":case["benchmark"],"workspace":case["workspace"]}), flush=True)
            except Exception as error:
                errors.append({"run_id":case["run_id"],"error":str(error)})
                print(json.dumps(errors[-1]), flush=True)
    if errors:
        write_json(output / "errors.json", errors)
        raise RuntimeError(f"Incomplete judge results: {output / 'errors.json'}")
    until = time.monotonic() + args.timeout
    chess = [case for case in batch["cases"] if case["benchmark"] == "chess960" and case["run_id"]]
    while time.monotonic() < until:
        for case in chess:
            report = client.request("GET", f'/v1/runs/{case["run_id"]}')["report"]
            if report:
                reviewed[case["run_id"]] = report
        if all(case["run_id"] in reviewed for case in chess):
            break
        time.sleep(1)
    mode = "measured_tokens" if all(item["tokens"]["complete"] for item in metrics) else config["incomplete_token_fallback"]
    scored = [score(item, reviewed.get(item["run_id"]), config, mode) for item in metrics]
    implementation = {name:hashlib.sha256((ROOT / "scripts" / name).read_bytes()).hexdigest()
                      for name in ["score_batch.py", "batch_metrics.py", "agent_selection.py"]}
    unscored = []
    for case in deferred:
        run = client.request("GET", f'/v1/runs/{case["run_id"]}') if case.get("run_id") else {}
        unscored.append({"metrics":collect(case, run.get("run"), args.data, args.coordinator_data),
                         "stages":run.get("stages", []),"stages_reached":len(run.get("stages", []))})
    report = {"batch_id":args.batch,"config":config,"created_at_ms":int(time.time()*1000),"cases":scored,"unscored":unscored,
              "scoring_execution":{"judge":args.judge, "max_judges":args.max_judges,
                                   "timeout_seconds":args.timeout, "implementation_sha256":implementation},
              "token_scoring_mode":mode,"ranking":aggregate(scored,config),
              "notes":["Only selected final attempts are scored; previous aborted attempts are excluded.",
                       "Recorded historical workspace settings are preserved. Later Sol-light settings do not rewrite this batch.",
                       "Dataset dashboard assignments differ across workspaces; this is one observation per assignment, not a causal estimate.",
                       "Source lines include comments and vendored source, matching the existing artifact counter."]}
    write_json(output / "scores.json", report)
    write_html(output / "scores.html", report)
    print(json.dumps({"scores":str(output / "scores.json"),"ranking":report["ranking"]},indent=2),flush=True)


if __name__ == "__main__":
    main()
