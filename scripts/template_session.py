#!/usr/bin/env python3
"""Start one planner against the shared coordinator workspace."""
import json
import os
from pathlib import Path
import subprocess
import time

from harness import Client, agent_environment
from agent_selection import detect_family, planner_command, workspace_settings
from workspace_prompts import documents

STALL_LIMIT = 3


def next_step(exit_code, state, stalls):
    """Decide what follows a planner exit: hand off, repeat the current request, or stop."""
    if not state["open"]:
        return "finished", stalls
    if exit_code == 0 and state["submitted"]:
        return "advance", 0
    stalls += 1
    return ("stop" if stalls >= STALL_LIMIT else "continue"), stalls


def main():
    launch_path = Path(os.environ["HARNESS_WORKFLOW_REQUEST"])
    launch = json.loads(launch_path.read_text(encoding="utf-8"))
    root, profile, started = launch_path.parent, launch["profile"], launch["started"]
    settings = workspace_settings(profile["workspace"])
    command = planner_command(profile["planner_command"], settings["planner"])
    workspace = Path(profile["workspace_path"])
    if {path.name for path in workspace.iterdir()} != {"PROJECT_PLANNER.md", "WORKER_PROTOCOL.md"}:
        raise ValueError("planner workspace must contain only its selected instruction documents")
    documents(workspace, settings)
    prompt_file = root / "planner-prompt.txt"
    prompt_file.write_bytes(profile["template_prompt"].encode("utf-8"))
    coordinator = Client(profile["coordinator_url"])
    coordinator.request("POST", "/api/benchmark-runs", {
        "workspace": str(workspace), "group": profile["workspace_group"],
        "settings": profile["workspace"],
        "run_id": started["run"]["id"], "deadline_ms": started["run"]["deadline_ms"],
    })
    env = agent_environment(**profile.get("env", {}))
    for key in ["HARNESS_WORKFLOW_REQUEST", "CODEX_WINDOWS_REGISTERED_CORE"]:
        env.pop(key, None)
    env.update(COORDINATOR_URL=coordinator.url,
               HARNESS_URL=launch["url"], HARNESS_RUN_ID=started["run"]["id"],
               HARNESS_RUN_TOKEN=started["run_token"])
    flags = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
    env["HARNESS_PLANNER_PROMPT"] = str(prompt_file)
    env["HARNESS_WORKSPACE_PLANNER"] = profile["workspace"]["planner"]
    env["AGENT_FAMILY"] = detect_family() or ""
    handoff = started.get("handoff_token")
    if handoff:
        env["HARNESS_PLANNER_SESSION"] = str(root / "planner-session.json")
    progress = {"mode": "template", "phase": "running", "workspace": str(workspace),
                "coordinator_url": coordinator.url, "run_id": started["run"]["id"]}

    def save():
        (root / "progress.json").write_text(json.dumps(progress), encoding="utf-8")

    def launch_planner(message=None):
        if message is not None:
            message_file = root / f"planner-message-{len(progress.get('messages', []))}.txt"
            message_file.write_text(message, encoding="utf-8")
            env["HARNESS_PLANNER_MESSAGE"] = str(message_file)
            progress.setdefault("messages", []).append({"file": str(message_file), "at_ms": int(time.time() * 1000)})
        with (root / "planner.log").open("ab") as log:
            planner = subprocess.Popen(command, cwd=workspace, env=env,
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, **flags)
        progress.update(phase="running", planner_pid=planner.pid)
        save()
        return planner

    planner = launch_planner()
    if not handoff:
        reported = False
        while True:
            if planner.poll() is not None and not reported:
                progress.update(phase="planner_exited", planner_exit_code=planner.returncode)
                save()
                reported = True
            time.sleep(0.2)
    supervisor = Client(launch["url"], handoff)
    path = f'/v1/runs/{started["run"]["id"]}/handoff'
    stalls = 0
    while True:
        code = planner.wait()
        progress.update(phase="planner_exited", planner_exit_code=code)
        save()
        state = supervisor.request("POST", path, {"action": "current"})
        action, stalls = next_step(code, state, stalls)
        progress.update(request=state["request"], stalls=stalls, last_action=action)
        if action == "finished":
            break
        if action == "stop":
            supervisor.request("POST", path, {"action": "stop", "reason": f"{STALL_LIMIT} planner exits without a new submission"})
            break
        if action == "advance":
            message = supervisor.request("POST", path, {"action": "advance"})["next"]
            if message is None:
                break
        else:
            message = state["continue"]
        planner = launch_planner(message)
    progress.update(phase="finished")
    save()


if __name__ == "__main__":
    main()
