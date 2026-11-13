import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from harness import Client, snapshot
from agent_selection import model_effort, workspace_agent


def pulse(path):
    while True:
        Path(path).write_text(str(time.time()))
        time.sleep(0.05)


def main():
    if sys.argv[1] == "pulse":
        return pulse(sys.argv[2])
    if sys.argv[1] == "worker":
        request = json.loads(Path(os.environ["COORDINATOR_REQUEST"]).read_text(encoding="utf-8"))
        model, effort = model_effort(workspace_agent(request["worker"]))
        assert request["task"]["model"] == model
        assert request["task"]["effort"] == effort
        protocol = request["workerProtocol"]
        assert Path(protocol["path"]).read_text() == protocol["text"]
        assert "Selected worker_protocol fixture" in protocol["text"]
        workspace = Path(request["task"]["cwd"])
        if request["stage"] == "implementation":
            if request["task"]["title"] == "Pending":
                subprocess.Popen([sys.executable, __file__, "pulse", str(workspace / "worker.pulse")])
                time.sleep(120)
            (workspace / "x.py").write_text("print('ready')\n")
            (workspace / "harness.json").write_text(json.dumps({"run": [sys.executable, "x.py"]}))
        result = {"status": "accepted" if request["stage"] == "review" else "completed",
                  "summary": "Fixture ready", "criteria": ["passed"], "findings": []}
        Path(os.environ["COORDINATOR_RESULT"]).write_text(json.dumps(result))
        return
    assert {path.name for path in Path.cwd().iterdir()} == {"PROJECT_PLANNER.md", "WORKER_PROTOCOL.md"}, "planner did not start with only selected documents"
    assert not any(key in os.environ for key in ["HARNESS_API_TOKEN", "HARNESS_WORKFLOW_REQUEST", "HARNESS_JUDGE_TOKEN"])
    prompt = Path(os.environ["HARNESS_PLANNER_PROMPT"]).read_text(encoding="utf-8")
    assert str(Path.cwd()) in prompt
    assert "{workspace_name}" not in prompt and "{benchmark_task}" not in prompt
    client = Client(os.environ["HARNESS_URL"], os.environ["HARNESS_RUN_TOKEN"])
    run_id = os.environ["HARNESS_RUN_ID"]
    task = client.request("GET", f"/v1/runs/{run_id}/task")
    assert {"run_id", "benchmark", "prompt"} <= set(task)
    if task["benchmark"] == "dataset_dashboard":
        assert task["dataset"]["dataset"]["id"] in task["prompt"]
    assert task["benchmark"] in prompt
    coordinator = Client(os.environ["COORDINATOR_URL"])
    state = coordinator.request("GET", "/api/state")
    settings = next(v for k, v in state["config"]["workspaces"].items() if Path(k).samefile(Path.cwd().parent))
    assert os.environ["HARNESS_WORKSPACE_PLANNER"] == settings["planner"]
    (Path.cwd() / "observed.json").write_text(json.dumps({"prompt": prompt, "pid": os.getpid(), "config": state["config"]}))
    payload = {"title": "Baseline", "outcome": "Write fixture files", "cwd": str(Path.cwd()),
               "standalone": True, "acceptance": ["Fixture files exist"], "attributes": {"lane": "code"}}
    coordinator.request("POST", "/api/tasks", payload)
    while True:
        tasks = coordinator.request("GET", "/api/state")["tasks"]
        for item in tasks:
            if Path(item["cwd"]).samefile(Path.cwd()):
                assert Path(item["workspace"]).samefile(Path.cwd().parent)
        if any(t["title"] == "Baseline" and t["stage"] == "done" and Path(t["cwd"]).resolve() == Path.cwd() for t in tasks):
            break
        time.sleep(0.05)
    client.request("POST", f"/v1/runs/{run_id}/submissions", snapshot(Path.cwd()))
    subprocess.Popen([sys.executable, __file__, "pulse", str(Path.cwd() / "planner.pulse")])
    coordinator.request("POST", "/api/tasks", {**payload, "title": "Pending"})
    while not (Path.cwd() / "worker.pulse").exists():
        time.sleep(0.05)


if __name__ == "__main__":
    main()
