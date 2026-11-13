#!/usr/bin/env python3
"""Dependency-free client for the Rust evaluator and process helpers for the scoring runner."""
import argparse
import base64
import fnmatch
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import urllib.error
import urllib.request


class ApiError(RuntimeError):
    def __init__(self, status, message):
        self.status = status
        super().__init__(f"HTTP {status}: {message}")


class Client:
    def __init__(self, url="http://127.0.0.1:8741", token=""):
        self.url, self.token = url.rstrip("/"), token

    def request(self, method, path, body=None, timeout=30):
        req = urllib.request.Request(
            self.url + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"},
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as response:
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default=os.environ.get("HARNESS_URL", "http://127.0.0.1:8741"))
    parser.add_argument("--token", default=os.environ.get("HARNESS_RUN_TOKEN") or os.environ.get("HARNESS_API_TOKEN", ""))
    parser.add_argument("--token-file", type=Path)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("benchmark-batch", help="Queue every benchmark for every supplied workspace").add_argument("config", type=Path)
    for name in ["batch-status", "batch-stop"]:
        sub.add_parser(name).add_argument("batch")
    batch_score = sub.add_parser("batch-score", help="Run scoring through one API call and print the complete JSON report")
    batch_score.add_argument("batch")
    batch_score.add_argument("--max-judges", type=int, default=2)
    sub.add_parser("task", help="Fetch this planner's assigned benchmark").add_argument("run", nargs="?", default=os.environ.get("HARNESS_RUN_ID"))
    submit = sub.add_parser("submit")
    submit.add_argument("run")
    submit.add_argument("workspace", type=Path)
    args = parser.parse_args()
    client = Client(args.url, args.token_file.read_text().strip() if args.token_file else args.token)
    if args.command == "benchmark-batch":
        result = client.request("POST", "/v1/benchmark-batches", json.loads(args.config.read_text(encoding="utf-8-sig")))
    elif args.command == "batch-status":
        result = client.request("GET", f"/v1/benchmark-batches/{args.batch}")
    elif args.command == "batch-stop":
        result = client.request("POST", f"/v1/benchmark-batches/{args.batch}/stop", {})
    elif args.command == "batch-score":
        result = client.request("POST", f"/v1/benchmark-batches/{args.batch}/score", {"max_judges": args.max_judges}, timeout=None)
    elif args.command == "task":
        if not args.run:
            parser.error("task requires a run id or HARNESS_RUN_ID")
        result = client.request("GET", f"/v1/runs/{args.run}/task")
    else:
        result = client.request("POST", f"/v1/runs/{args.run}/submissions", snapshot(args.workspace))
    sys.stdout.reconfigure(encoding="utf-8")
    print(json.dumps(result, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except (ApiError, OSError, ValueError) as error:
        sys.exit(str(error))
