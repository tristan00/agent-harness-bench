#!/usr/bin/env python3
"""Dependency-free client and external-agent adapters for the Rust evaluator."""
import argparse
import base64
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from agent_selection import expand_command, workspace_settings


class ApiError(RuntimeError):
    def __init__(self, status, message):
        self.status = status
        super().__init__(f"HTTP {status}: {message}")


class Client:
    def __init__(self, url="http://127.0.0.1:8741", token=""):
        self.url, self.token = url.rstrip("/"), token

    def request(self, method, path, body=None):
        req = urllib.request.Request(
            self.url + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"},
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise ApiError(error.code, error.read().decode(errors="replace")) from error


DEFAULT_EXCLUDES = {".git", "target", "node_modules", ".venv", "venv", "__pycache__", ".harness", ".env"}


def snapshot(workspace):
    workspace = Path(workspace).resolve()
    manifest = json.loads((workspace / "harness.json").read_text(encoding="utf-8"))
    ignores = []
    ignorefile = workspace / ".harnessignore"
    if ignorefile.exists():
        ignores = [s.strip() for s in ignorefile.read_text().splitlines() if s.strip() and not s.startswith("#")]
    files = []
    # Prune generated dependency/build trees without walking through them.
    for directory, folders, names in os.walk(workspace, followlinks=False):
        folders[:] = sorted(n for n in folders if n not in DEFAULT_EXCLUDES and not (Path(directory) / n).is_symlink())
        for name in sorted(names):
            path = Path(directory) / name
            relative = path.relative_to(workspace).as_posix()
            if name in DEFAULT_EXCLUDES or name.endswith((".pyc", ".log")) or name.startswith(".env.") or any(fnmatch.fnmatch(relative, p) for p in ignores):
                continue
            if path.is_symlink():
                raise ValueError(f"symlinks cannot be submitted: {relative}")
            before = path.stat()
            data = path.read_bytes()
            after = path.stat()
            if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
                raise ValueError(f"file changed while snapshotting: {relative}; retrying next checkpoint")
            item = {"path": relative, "executable": bool(before.st_mode & 0o111)}
            try:
                item["text"] = data.decode("utf-8")
            except UnicodeDecodeError:
                item["base64"] = base64.b64encode(data).decode()
            files.append(item)
    return {"manifest": manifest, "files": files}


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False), encoding="utf-8")
    if os.name != "nt":
        path.chmod(0o600)


def spawn_agent(command, directory, env, log):
    if not isinstance(command, list) or not command or not all(isinstance(s, str) and s for s in command):
        raise ValueError("command-json must be a nonempty argv array")
    kwargs = {"cwd": directory, "env": env, "stdin": subprocess.DEVNULL, "stdout": log, "stderr": subprocess.STDOUT}
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW | 0x00000004  # suspended until assigned to our job
    else:
        kwargs["start_new_session"] = True
    process = subprocess.Popen(command, **kwargs)
    process._harness_stop_lock = threading.Lock()
    process._harness_stopped = False
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes
        class Limits(ctypes.Structure):
            _fields_ = [("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64), ("flags", wintypes.DWORD),
                        ("min_working", ctypes.c_size_t), ("max_working", ctypes.c_size_t), ("active", wintypes.DWORD),
                        ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD), ("scheduling", wintypes.DWORD)]
        class Extended(ctypes.Structure):
            _fields_ = [("basic", Limits), ("io", ctypes.c_uint64 * 6), ("process_memory", ctypes.c_size_t),
                        ("job_memory", ctypes.c_size_t), ("peak_process", ctypes.c_size_t), ("peak_job", ctypes.c_size_t)]
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p]
        kernel.CreateJobObjectW.restype = ctypes.c_void_p
        kernel.SetInformationJobObject.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
        kernel.AssignProcessToJobObject.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        kernel.TerminateJobObject.argtypes = [ctypes.c_void_p, wintypes.UINT]
        kernel.CloseHandle.argtypes = [ctypes.c_void_p]
        resume = ctypes.WinDLL("ntdll").NtResumeProcess
        resume.argtypes, resume.restype = [ctypes.c_void_p], ctypes.c_long
        job = kernel.CreateJobObjectW(None, None)
        limits = Extended()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        try:
            if not job or not kernel.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
                raise ctypes.WinError(ctypes.get_last_error())
            if not kernel.AssignProcessToJobObject(job, int(process._handle)):
                raise ctypes.WinError(ctypes.get_last_error())
            if resume(int(process._handle)) < 0:
                raise OSError("could not resume agent process")
        except Exception:
            process.kill()
            process.wait()
            if job:
                kernel.CloseHandle(job)
            raise
        process._harness_job = (kernel, job)
    return process


def stop_agent(process):
    if process is None:
        return
    with process._harness_stop_lock:
        if process._harness_stopped:
            return
        process._harness_stopped = True
        if os.name == "nt":
            kernel, job = process._harness_job
            kernel.TerminateJobObject(job, 1)
            kernel.CloseHandle(job)
        else:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()


def agent_environment(**values):
    env = {k: v for k, v in os.environ.items() if k not in {"HARNESS_API_TOKEN", "HARNESS_JUDGE_TOKEN", "HARNESS_RUN_TOKEN", "HARNESS_JUDGMENT_TOKEN"}}
    env.update({k: str(v) for k, v in values.items()})
    return env


def run_agent(args, admin):
    settings = workspace_settings(json.loads(args.workspace_settings.read_text(encoding="utf-8-sig")))
    argv = expand_command(json.loads(args.command_json), settings["planner"])
    workspace = Path(args.workspace).resolve()
    workspace.mkdir(parents=True, exist_ok=True)
    control = workspace / ".harness"
    control.mkdir(exist_ok=True)
    started = admin.request("POST", "/v1/runs", {"campaign_id": args.campaign, "agent": args.agent})
    write_json(control / "run.json", started)
    prompt = control / "prompt.txt"
    prompt.write_text(started["prompt"] + "\nWrite harness.json in the workspace. The adapter uploads complete snapshots automatically.\n", encoding="utf-8")
    run = started["run"]
    client = Client(args.url, started["run_token"])
    # Use monotonic duration calculated from the server's own timestamps.
    stop_at = time.monotonic() + max(0, run["deadline_ms"] - started["server_time_ms"]) / 1000
    command = [s.replace("{prompt_file}", str(prompt)) for s in argv]
    env = agent_environment(HARNESS_URL=args.url, HARNESS_RUN_ID=run["id"], HARNESS_RUN_TOKEN=started["run_token"],
                            HARNESS_PROMPT_FILE=prompt)
    last_hash = None
    process = None
    watchdog = None
    print(json.dumps({"run_id": run["id"], "deadline_ms": run["deadline_ms"], "workspace": str(workspace)}), flush=True)
    try:
        with (control / "agent.log").open("wb") as log:
            process = spawn_agent(command, workspace, env, log)
            watchdog = threading.Timer(max(0, stop_at - time.monotonic()), stop_agent, args=(process,))
            watchdog.daemon = True
            watchdog.start()
            while time.monotonic() < stop_at:
                try:
                    submission = snapshot(workspace)
                    digest = hashlib.sha256(json.dumps(submission, sort_keys=True).encode()).hexdigest()
                    if digest != last_hash:
                        accepted = client.request("POST", f'/v1/runs/{run["id"]}/submissions', submission)
                        last_hash = digest
                        write_json(control / "last-submission.json", accepted)
                        print(json.dumps({"checkpoint": accepted["submission"]["sha256"]}), flush=True)
                except ApiError as error:
                    if error.status == 409:
                        break
                    print(str(error), file=sys.stderr, flush=True)
                except (OSError, ValueError, urllib.error.URLError) as error:
                    print(f"checkpoint unavailable: {error}", file=sys.stderr, flush=True)
                time.sleep(min(args.interval, max(0, stop_at - time.monotonic())))
    finally:
        if watchdog:
            watchdog.cancel()
        stop_agent(process)
    state = client.request("GET", f'/v1/runs/{run["id"]}')
    write_json(control / "final-state.json", state)
    return state


def judge_worker(args, registry):
    settings = workspace_settings(json.loads(args.workspace_settings.read_text(encoding="utf-8-sig")))
    argv = expand_command(json.loads(args.command_json), settings["worker"])
    root = Path(args.workspace).resolve()
    root.mkdir(parents=True, exist_ok=True)
    while True:
        claimed = registry.request("POST", "/v1/judges/claim", {"judge": args.judge})
        if claimed["job"] is None:
            if args.once:
                return claimed
            time.sleep(2)
            continue
        job = claimed["job"]
        control = root / job["id"]
        control.mkdir(exist_ok=True)
        write_json(control / "job.json", claimed)
        prompt_file = control / "prompt.txt"
        score_file = control / "score.json"
        instructions = claimed["prompt"] + f"\nJudgment ID: {job['id']}\nUI: {job['ui_url']}\nLaunch error: {job['launch_error']}\nUse the HTTP API at {args.url}; your scoped token is in HARNESS_JUDGMENT_TOKEN. Poll GET /v1/judgments/{job['id']}. POST actions to /v1/judgments/{job['id']}/action. GET /v1/judgments/{job['id']}/artifact to inspect submitted code/README.\nWrite your final score object to {score_file} after exploration. The adapter will submit it during scoring.\n"
        prompt_file.write_text(instructions, encoding="utf-8")
        client = Client(args.url, claimed["judgment_token"])
        command = [s.replace("{prompt_file}", str(prompt_file)) for s in argv]
        env = agent_environment(HARNESS_URL=args.url, HARNESS_JUDGMENT_ID=job["id"], HARNESS_JUDGMENT_TOKEN=claimed["judgment_token"],
                                HARNESS_PROMPT_FILE=prompt_file, HARNESS_SCORE_FILE=score_file)
        process = None
        try:
            with (control / "judge.log").open("wb") as log:
                process = spawn_agent(command, control, env, log)
                while True:
                    state = client.request("GET", f'/v1/judgments/{job["id"]}')["job"]
                    if state["status"] in {"completed", "timed_out", "interrupted"}:
                        write_json(control / "final-state.json", state)
                        break
                    if state["status"] == "scoring" and score_file.exists():
                        try:
                            score = json.loads(score_file.read_text(encoding="utf-8"))
                            report = client.request("POST", f'/v1/judgments/{job["id"]}/score', score)
                            write_json(control / "report.json", report)
                            break
                        except (ValueError, ApiError) as error:
                            print(f"score not yet accepted: {error}", file=sys.stderr, flush=True)
                    time.sleep(0.5)
        finally:
            stop_agent(process)
        if args.once:
            return client.request("GET", f'/v1/judgments/{job["id"]}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default=os.environ.get("HARNESS_URL", "http://127.0.0.1:8741"))
    parser.add_argument("--token", default=os.environ.get("HARNESS_RUN_TOKEN") or os.environ.get("HARNESS_API_TOKEN", ""))
    parser.add_argument("--token-file", type=Path)
    sub = parser.add_subparsers(dest="command", required=True)
    create = sub.add_parser("create")
    create.add_argument("benchmark", choices=["chess960", "game_build", "dataset_dashboard", "maze_game"])
    create.add_argument("--config", type=Path, help="Optional campaign JSON configuration")
    start = sub.add_parser("start")
    start.add_argument("campaign")
    start.add_argument("agent")
    start.add_argument("--out", type=Path)
    coordinated = sub.add_parser("run-coordinator", help="Launch a service-owned iterative coordinator run")
    coordinated.add_argument("campaign")
    coordinated.add_argument("agent")
    coordinated.add_argument("--profile", required=True)
    workflow = sub.add_parser("workflow", help="Read coordinator progress and accepted iterations")
    workflow.add_argument("run")
    batch = sub.add_parser("benchmark-batch", help="Queue all three benchmarks for every supplied workspace")
    batch.add_argument("config", type=Path)
    batch_status = sub.add_parser("batch-status")
    batch_status.add_argument("batch")
    batch_stop = sub.add_parser("batch-stop")
    batch_stop.add_argument("batch")
    batch_dashboard = sub.add_parser("batch-add-dashboard", help="Add a dashboard run to every workspace in an existing batch")
    batch_dashboard.add_argument("batch")
    batch_dashboard.add_argument("--config", type=Path)
    task = sub.add_parser("task", help="Fetch this planner's assigned benchmark")
    task.add_argument("run", nargs="?", default=os.environ.get("HARNESS_RUN_ID"))
    submit = sub.add_parser("submit")
    submit.add_argument("run")
    submit.add_argument("workspace", type=Path)
    status = sub.add_parser("status")
    status.add_argument("run")
    close = sub.add_parser("close")
    close.add_argument("campaign")
    score = sub.add_parser("score")
    score.add_argument("campaign")
    request = sub.add_parser("request")
    request.add_argument("method", choices=["GET", "POST"])
    request.add_argument("path")
    request.add_argument("--body", type=Path)
    run = sub.add_parser("run-agent")
    run.add_argument("campaign")
    run.add_argument("agent")
    run.add_argument("workspace")
    run.add_argument("--command-json", required=True, help='Agent argv, e.g. ["my-agent", "--prompt-file", "{prompt_file}"]')
    run.add_argument("--workspace-settings", required=True, type=Path)
    run.add_argument("--interval", type=float, default=1.0)
    judge = sub.add_parser("judge-worker")
    judge.add_argument("judge")
    judge.add_argument("workspace")
    judge.add_argument("--command-json", required=True)
    judge.add_argument("--workspace-settings", required=True, type=Path)
    judge.add_argument("--once", action="store_true")
    args = parser.parse_args()
    token = args.token_file.read_text().strip() if args.token_file else args.token
    client = Client(args.url, token)
    if args.command == "create":
        config = json.loads(args.config.read_text()) if args.config else {}
        result = client.request("POST", "/v1/campaigns", {**config, "benchmark": args.benchmark})
    elif args.command == "start":
        result = client.request("POST", "/v1/runs", {"campaign_id": args.campaign, "agent": args.agent})
        if args.out:
            write_json(args.out, result)
    elif args.command == "run-coordinator":
        result = client.request("POST", "/v1/coordinator-runs", {"campaign_id": args.campaign, "agent": args.agent, "profile": args.profile})
    elif args.command == "workflow":
        result = client.request("GET", f"/v1/runs/{args.run}/coordinator")
        result.update(client.request("GET", f"/v1/runs/{args.run}/iterations"))
    elif args.command == "benchmark-batch":
        result = client.request("POST", "/v1/benchmark-batches", json.loads(args.config.read_text(encoding="utf-8-sig")))
    elif args.command == "batch-status":
        result = client.request("GET", f"/v1/benchmark-batches/{args.batch}")
    elif args.command == "batch-stop":
        result = client.request("POST", f"/v1/benchmark-batches/{args.batch}/stop", {})
    elif args.command == "batch-add-dashboard":
        config = json.loads(args.config.read_text(encoding="utf-8-sig")) if args.config else {}
        result = client.request("POST", f"/v1/benchmark-batches/{args.batch}/dashboard", config)
    elif args.command == "task":
        if not args.run:
            parser.error("task requires a run id or HARNESS_RUN_ID")
        result = client.request("GET", f"/v1/runs/{args.run}/task")
    elif args.command == "submit":
        result = client.request("POST", f"/v1/runs/{args.run}/submissions", snapshot(args.workspace))
    elif args.command == "status":
        result = client.request("GET", f"/v1/runs/{args.run}")
    elif args.command == "close":
        result = client.request("POST", f"/v1/campaigns/{args.campaign}/close", {})
    elif args.command == "score":
        result = client.request("POST", f"/v1/campaigns/{args.campaign}/score", {})
    elif args.command == "request":
        result = client.request(args.method, args.path, json.loads(args.body.read_text()) if args.body else None)
    elif args.command == "run-agent":
        if not 0.1 <= args.interval <= 60:
            parser.error("interval must be 0.1..60 seconds")
        result = run_agent(args, client)
    else:
        if not token:
            client.token = os.environ.get("HARNESS_JUDGE_TOKEN", "")
        result = judge_worker(args, client)
    print(json.dumps(result, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except (ApiError, OSError, ValueError) as error:
        sys.exit(str(error))
