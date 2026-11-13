"""Score final submissions and execution evidence for a completed benchmark batch."""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager
import hashlib
import json
import math
import os
import sys
from pathlib import Path
import time

from agent_selection import claude_arguments, claude_environment, command_arguments, model_effort, resolve, detect_family
from batch_metrics import TRACKED_ATTRIBUTES, aggregate, collect, read, score
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
              "benchmark_weights": {"chess960", "game_build", "dataset_dashboard", "maze_game"},
              "framework_weights": {"task_count", "relations", "parallelism", "token_efficiency", "classification"},
              "references": {"elo_scale", "source_lines", "task_count", "relation_types", "edges_per_task", "tokens"},
              "maze": {"completion_weight", "quality_weight"}}
    for name, keys in groups.items():
        values = config[name]
        if set(values) != keys or any(not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0 for value in values.values()):
            raise ValueError(f"Invalid scoring configuration: {name}")
    attributes = config["classification_attributes"]
    if not attributes or len(set(attributes)) != len(attributes) or not set(attributes) <= set(TRACKED_ATTRIBUTES):
        raise ValueError("Invalid scoring configuration: classification_attributes")
    if config["incomplete_token_fallback"] != "agent_seconds_per_development_second":
        raise ValueError("Unsupported token fallback")
    model_effort(judge_selection(config))


def judge_selection(config):
    family = detect_family()
    if family not in config["judge_selections"]:
        raise ValueError(f"Scoring configuration has no judge for the {family or 'undetected'} launcher family")
    return config["judge_selections"][family]


def judge_command(selection, prompt_path, output_path, browser_version):
    browser_args = ["-y", f"@playwright/mcp@{browser_version}", "--isolated", "--headless", "--browser", "chrome",
                    "--output-dir", str(prompt_path.parent / "browser")]
    agent = resolve(selection, retired=True)
    if agent["harness"] == "claude_code":
        servers = {"mcpServers": {"playwright": {"command": "npx", "args": browser_args}}}
        return [*claude_arguments(agent), "--strict-mcp-config", "--mcp-config", json.dumps(servers),
                prompt_path.read_text(encoding="utf-8")]
    if agent["harness"] != "codex":
        raise ValueError(f"Judge {selection} needs a browser-capable harness")
    return [os.environ.get("CODEX_BIN", "codex"), "exec", "--ephemeral", "--skip-git-repo-check", "--json",
            "--sandbox", "danger-full-access", "-c", 'approval_policy="never"', "--disable", "memories",
            *command_arguments(selection, retired=True), "-c", "mcp_servers.chrome-devtools.enabled=false",
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
    if resolve(args.judge, retired=True)["harness"] == "claude_code":
        env = claude_environment(env)
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("batch")
    parser.add_argument("--data", type=Path, required=True)
    parser.add_argument("--coordinator-data", type=Path)
    parser.add_argument("--coordinator-url")
    parser.add_argument("--url", default="http://127.0.0.1:8741")
    parser.add_argument("--max-judges", type=int, default=2)
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument("--config", type=Path, default=ROOT / "scoring.json")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.max_judges < 1 or args.timeout < 1:
        parser.error("max-judges and timeout must be positive")
    args.data = args.data.resolve()
    config = read(args.config)
    validate_config(config)
    args.judge = judge_selection(config)
    output = (args.output or args.data / "batch-scores" / args.batch / config["version"]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    with output_lock(output / ".scoring.lock"):
        score_batch(args, output, config)


def score_batch(args, output, config):
    client = Client(args.url, (args.data / "admin-token").read_text().strip())
    batch = client.request("GET", f"/v1/benchmark-batches/{args.batch}")["batch"]
    if args.coordinator_data is None:
        coordinator_url = args.coordinator_url
        if not coordinator_url:
            profiles = read(args.data / "coordinator-profiles.json")
            coordinator_url = profiles[batch["profile"]]["coordinator_url"]
        args.coordinator_data = Path(Client(coordinator_url).request("GET", "/api/state")["dataDirectory"])
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
                                  "benchmark":case["benchmark"],"workspace":case["workspace"]}), file=sys.stderr, flush=True)
            except Exception as error:
                errors.append({"run_id":case["run_id"],"error":str(error)})
                print(json.dumps(errors[-1]), file=sys.stderr, flush=True)
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
                         "completed_requests":run.get("completed_requests", [])})
    report = {"schema_version":"benchmark-score-report.v2", "producer":"agent-harness-bench scoring utility",
              "score_generation":{"arithmetic":"deterministic code", "quality_ratings":"independent judge sessions",
                                  "aggregation":"deterministic weighted means", "human_or_supervising_agent_edits":False},
              "batch_id":args.batch,"config":config,"created_at_ms":int(time.time()*1000),"cases":scored,"unscored":unscored,
              "scoring_execution":{"judge":args.judge, "max_judges":args.max_judges,
                                   "timeout_seconds":args.timeout, "implementation_sha256":implementation},
              "token_scoring_mode":mode,"ranking":aggregate(scored,config),
              "token_mode_decision":{"rule":"Use measured tokens only when every launch in every case has complete usage; otherwise use the same runtime proxy for all cases.",
                  "selected":mode,"incomplete_cases":[{"run_id":item["run_id"],"workspace":item["workspace"],"benchmark":item["benchmark"],
                      "measured_launches":item["tokens"]["measured_launches"],"launches":item["tokens"]["launches"]} for item in metrics if not item["tokens"]["complete"]]},
              "notes":["Only selected final attempts are scored; previous aborted attempts are excluded.",
                       "Historical workspace settings are preserved without reinterpreting model or prompt selections.",
                       "Dataset dashboard assignments differ across workspaces; this is one observation per assignment, not a causal estimate.",
                       "Source lines include comments and vendored source, matching the existing artifact counter."]}
    write_json(output / "scores.json", report)
    print(json.dumps(report, allow_nan=False), flush=True)


if __name__ == "__main__":
    main()
