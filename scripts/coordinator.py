#!/usr/bin/env python3
"""Service-owned, provider-neutral planner/coordinator iteration driver."""
import json
import os
from pathlib import Path
import socket
import sys
import time
import urllib.parse

from harness import ApiError, Client, agent_environment, snapshot, spawn_agent, stop_agent
from agent_selection import workspace_settings, planner_command
from workspace_prompts import copy_documents


def save(path, value):
    path = Path(path)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2, ensure_ascii=False), encoding="utf-8")
    temporary.replace(path)


def read(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def batch(plan, workspace, key):
    tasks = plan.get("tasks")
    if not isinstance(tasks, list) or not tasks:
        raise ValueError("plan requires nonempty tasks")
    keys = [task["key"] for task in tasks]
    if any(not isinstance(k, str) or not k for k in keys) or len(set(keys)) != len(keys):
        raise ValueError("task keys must be nonempty and unique")
    children = []
    for task in tasks:
        after = task.get("after", [])
        if not isinstance(after, list) or any(k not in keys or k == task["key"] for k in after):
            raise ValueError("after must name other tasks in this plan")
        if not isinstance(task.get("acceptance"), list) or not task["acceptance"]:
            raise ValueError("each task requires acceptance criteria")
        children.append({
            "localKey": task["key"], "title": task["title"],
            "outcome": "Read .harness/benchmark.txt. " + task["outcome"],
            "acceptance": task["acceptance"], "writable": [str(workspace)],
            "after": [{"local": k} for k in after],
            "attributes": {"lane": task.get("lane", "code"), "size": "small", "artifact": "none"},
        })
    return {"requestKey": key, "container": {
        "title": plan["title"], "outcome": plan["outcome"], "cwd": str(workspace),
        "standalone": True, "acceptance": [], "attributes": {"size": "large"},
    }, "children": children}


def main():
    launch = read(os.environ["HARNESS_WORKFLOW_REQUEST"])
    root = Path(os.environ["HARNESS_WORKFLOW_REQUEST"]).resolve().parent
    started, profile = launch["started"], launch["profile"]
    settings = workspace_settings(profile["workspace"])
    planner_argv = planner_command(profile["planner_command"], settings["planner"])
    run = started["run"]
    client = Client(launch["url"], started["run_token"])
    # Fetch current server time: startup latency must spend the development allowance.
    state = client.request("GET", f'/v1/runs/{run["id"]}')
    stop_at = time.monotonic() + max(0, run["deadline_ms"] - state["server_time_ms"]) / 1000
    workspace = root / "workspace"
    workspace.mkdir(exist_ok=True)
    copy_documents(profile["prompt_workspace"], workspace, settings)
    control = workspace / ".harness"
    control.mkdir(exist_ok=True)
    (control / "benchmark.txt").write_text(started["prompt"], encoding="utf-8")
    coordinator_data = root / "board"
    coordinator_data.mkdir(exist_ok=True)
    # This board is private to this run; never attach to or stop a user's shared board.
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    coordinator = Client(f"http://127.0.0.1:{port}")
    env = agent_environment(COORDINATOR_DATA=coordinator_data, PORT=port, HARNESS_RUN_ID=run["id"])
    env.pop("HARNESS_WORKFLOW_REQUEST", None)
    env.update({str(k): str(v) for k, v in profile.get("env", {}).items()})
    progress = {"run_id": run["id"], "iteration": 1, "phase": "starting", "coordinator_url": coordinator.url,
                "workspace": str(workspace), "deadline_ms": run["deadline_ms"]}
    process = planner = None
    tasks, cursor = {}, None
    planner_usage = {}

    def watch(wait):
        query = urllib.parse.urlencode({"workspace": str(workspace), "wait": wait, **({} if cursor is None else {"after": cursor})})
        return coordinator.request("GET", f"/api/updates?{query}")

    def update(phase, **extra):
        progress.update(phase=phase, updated_at_ms=int(time.time() * 1000), **extra)
        save(root / "progress.json", progress)

    def remaining():
        return max(0, stop_at - time.monotonic())

    def pause():
        time.sleep(min(0.2, remaining()))

    try:
        with (root / "coordinator.log").open("wb") as log:
            process = spawn_agent(profile["coordinator_command"], workspace, env, log)
        while remaining():
            if process.poll() is not None:
                raise RuntimeError("coordinator exited; inspect coordinator.log")
            try:
                board = coordinator.request("GET", "/api/state")
                if board.get("dataDirectory", "").replace("\\\\?\\", "") != str(coordinator_data):
                    # resolve handles Windows extended path prefixes and case normalization.
                    if Path(board["dataDirectory"]).resolve() != coordinator_data.resolve():
                        raise RuntimeError("port belongs to another coordinator")
                break
            except (OSError, ApiError):
                pause()
        if not remaining():
            return
        coordinator.request("PUT", "/api/config", {
            **profile["workers"], "capabilities": profile.get("capabilities", ["process"]),
            "notify": [], "workspaces": {str(workspace): settings},
        })
        iteration, attempt, previous, error = 1, 0, None, None
        while remaining():
            attempt += 1
            directory = root / f"plan-{iteration}-{attempt}"
            directory.mkdir()
            request_file, result_file = directory / "request.json", directory / "result.json"
            request = {"protocol": 1, "workspace_settings": settings, "benchmark": started["config"]["benchmark"], "prompt": started["prompt"],
                       "workspace": str(workspace), "iteration": iteration, "previous_completion": previous, "error": error}
            save(request_file, request)
            update("planning", iteration=iteration, attempt=attempt, error=error)
            planner_env = {**env, "HARNESS_PLANNER_REQUEST": str(request_file), "HARNESS_PLANNER_RESULT": str(result_file)}
            with (directory / "planner.log").open("wb") as log:
                planner = spawn_agent(planner_argv, workspace, planner_env, log)
            while remaining() and planner.poll() is None:
                pause()
            if not remaining():
                break
            code = planner.returncode
            stop_agent(planner)
            planner = None
            if (directory / "usage.json").exists():
                for key, value in read(directory / "usage.json").items():
                    if isinstance(value, int):
                        planner_usage[key] = planner_usage.get(key, 0) + value
                update("planned", planner_usage=planner_usage)
            try:
                if code:
                    raise ValueError(f"planner exited {code}; inspect {directory / 'planner.log'}")
                plan = read(result_file)
                if plan.get("action") == "finish":
                    update("completed", reason=plan.get("outcome"))
                    return
                if plan.get("action") != "plan":
                    raise ValueError("planner action must be plan or finish")
                payload = batch(plan, workspace, f'{run["id"]}:{iteration}:{attempt}')
                save(directory / "batch.json", payload)
                receipt = coordinator.request("POST", "/api/tasks/batch", payload)
            except (ValueError, KeyError, ApiError, OSError) as exc:
                error = str(exc)
                pause()
                continue
            save(directory / "receipt.json", receipt)
            update("working", tasks=receipt, error=None)
            while remaining():
                if process.poll() is not None:
                    raise RuntimeError("coordinator exited during iteration")
                wait = max(0, min(20, int(remaining())))
                changes = watch(wait)
                cursor = changes["sequence"]
                tasks.update((t["id"], t) for t in changes["tasks"])
                tasks = {key: value for key, value in tasks.items() if key in set(changes["ids"])}
                container = tasks.get(receipt["container"])
                if container and container["stage"] == "done" and all(not t["active"] and t["stage"] == "done" for t in tasks.values()):
                    break
                held = [t["id"] for t in tasks.values() if t["blocked"] or t["stopped"]]
                update("blocked" if held or changes["holdUntil"] > int(time.time()*1000) else "working", held_tasks=held,
                       worker_tokens=sum(t["tokens"] for t in tasks.values()), updates=changes["updates"])
                if changes["updates"] != "events" or wait == 0:
                    pause()
            if not remaining():
                break
            update("submitting")
            try:
                payload = {"iteration": iteration, "tasks": list(receipt["children"].values()), "submission": snapshot(workspace)}
            except (ValueError, OSError) as exc:
                error = f"Completed tasks did not produce a submittable artifact: {exc}"
                continue
            save(directory / "submission.json", payload)
            # Retry exactly the same completion; the service deduplicates by iteration and content.
            accepted = None
            while remaining() and accepted is None:
                try:
                    accepted = client.request("POST", f'/v1/runs/{run["id"]}/iterations', payload)
                except ApiError as exc:
                    if exc.status == 409:
                        if remaining() < 1:
                            return
                        raise
                    if exc.status < 500:
                        error = str(exc)
                        break
                    pause()
                except OSError:
                    pause()
            if accepted is None:
                continue
            save(directory / "accepted.json", accepted)
            previous = {"accepted": accepted, "tasks": list(tasks.values())}
            update("accepted", last_completion=accepted)
            iteration, attempt, error = accepted["next_iteration"], 0, None
        update("deadline")
    except Exception as exc:
        update("failed", error=str(exc))
        raise
    finally:
        stop_agent(planner)
        stop_agent(process)


if __name__ == "__main__":
    main()
