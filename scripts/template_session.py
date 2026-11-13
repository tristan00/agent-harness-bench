#!/usr/bin/env python3
"""Start one planner against the shared coordinator workspace."""
import json
import os
from pathlib import Path
import subprocess
import time

from harness import Client, agent_environment
from agent_selection import planner_command, workspace_settings
from workspace_prompts import documents


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
        "workspace": str(workspace), "group": profile["workspace_group"], "settings": profile["workspace"],
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
    progress = {"mode": "template", "phase": "running", "workspace": str(workspace),
                "coordinator_url": coordinator.url, "run_id": started["run"]["id"]}
    with (root / "planner.log").open("wb") as log:
        planner = subprocess.Popen(command, cwd=workspace, env=env,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, **flags)
    progress["planner_pid"] = planner.pid
    (root / "progress.json").write_text(json.dumps(progress), encoding="utf-8")
    reported = False
    while True:
        if planner.poll() is not None and not reported:
            progress.update(phase="planner_exited", planner_exit_code=planner.returncode)
            (root / "progress.json").write_text(json.dumps(progress), encoding="utf-8")
            reported = True
        time.sleep(0.2)


if __name__ == "__main__":
    main()
