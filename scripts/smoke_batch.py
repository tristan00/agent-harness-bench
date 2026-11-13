#!/usr/bin/env python3
"""Exercise the batch scheduler and workspace deadlines on a shared board."""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from harness import Client
from agent_selection import CATALOG, WORKSPACE_CANDIDATES
from smoke import rejects, wait_for

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from prompt_fixture import selected_documents


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--coordinator", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    evidence = {}
    with tempfile.TemporaryDirectory(prefix="benchmark-batch-") as temporary:
        data = Path(temporary)
        workspace_root = data / "workspaces"
        workspace_root.mkdir()
        template = data / "template.txt"
        template.write_text("Task {benchmark_task}\nWorkspace: " + str(workspace_root / "{workspace_name}") + "\n", encoding="utf-8")
        worker = [sys.executable, str(ROOT / "tests/template_fixture.py"), "worker", "--model", "{model}", "--effort", "{effort}"]
        profile = {"command": ["unused"], "planner_command": ["unused"],
                   "coordinator_command": [str(args.coordinator.resolve())],
                   "workers": {k: worker for k in ["implementation", "review", "replan"]}, "capabilities": ["process"],
                   "session_command": [sys.executable, str(ROOT / "scripts/template_session.py")],
                   "session_planner_command": [sys.executable, str(ROOT / "tests/template_fixture.py"), "plan", "--model", "{model}", "--effort", "{effort}"]}
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            board_port = listener.getsockname()[1]
        profile["coordinator_url"] = f"http://127.0.0.1:{board_port}"
        (data / "coordinator-profiles.json").write_text(json.dumps({"fixture": profile}))
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        client = Client(f"http://127.0.0.1:{port}")
        log = (data / "service.log").open("wb")
        flags = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
        def launch():
            return subprocess.Popen([str(args.binary.resolve()), "serve", "--bind", f"127.0.0.1:{port}", "--data", str(data)], stdout=log, stderr=log, **flags)
        def config(prefix, cap=2, seconds=8, count=2):
            workspaces = []
            for index in range(count):
                path = workspace_root / f"{prefix}-{index}"
                path.mkdir()
                candidates = list(WORKSPACE_CANDIDATES)
                planners = [name for name in candidates if not CATALOG[name].get("workerOnly")]
                settings = selected_documents(path, {"planner": planners[index % len(planners)], "worker": candidates[(index + 1) % 3], "updates": "events" if index else "poll", "limits": {"words": 140 + index}})
                workspaces.append({"path": str(path), "settings": settings})
            return {"profile": "fixture", "template_path": str(template), "workspace_root": str(workspace_root),
                    "workspaces": workspaces, "max_parallel": cap, "order_seed":960,
                    "benchmarks": ["chess960", "game_build", "dataset_dashboard"],
                    "task_configs": {k: {"development_seconds": seconds} for k in ["chess960", "game_build", "dataset_dashboard"]}}
        def status(batch):
            return client.request("GET", f'/v1/benchmark-batches/{batch["id"]}')["batch"]
        def frozen_pulses(paths):
            before = {p: p.read_text() for p in paths}
            time.sleep(0.25)
            assert all(p.read_text() == text for p, text in before.items()), "descendant survived termination"
        board = subprocess.Popen([str(args.coordinator.resolve())], env={**os.environ, "COORDINATOR_DATA": str(data / "shared-board"), "PORT": str(board_port)}, stdout=log, stderr=log, **flags)
        board_client = Client(profile["coordinator_url"])
        wait_for(lambda: board_client.request("GET", "/api/state"))
        unrelated = data / "unrelated"
        unrelated.mkdir()
        board_client.request("PUT", "/api/config", {**profile["workers"], "capabilities": ["process"], "notify": [], "workspaces": {str(unrelated): selected_documents(unrelated)}})
        board_client.request("POST", "/api/tasks", {"title":"Pending", "outcome":"Keep fixture running", "cwd":str(unrelated), "standalone":True, "acceptance":["Running"], "attributes":{"lane":"code"}})
        service = launch()
        try:
            wait_for(lambda: client.request("GET", "/health"))
            client.token = (data / "admin-token").read_text()
            wait_for(lambda: (unrelated / "worker.pulse").exists())
            request = config("deadline")
            rejects(client, "POST", "/v1/benchmark-batches", {**request, "max_parallel": 0}, 400)
            invalid = {**request, "workspaces": [{"path": request["workspaces"][0]["path"], "settings": {"planner": "astra_xhigh"}}]}
            rejects(client, "POST", "/v1/benchmark-batches", invalid, 400)
            rejects(client, "POST", "/v1/benchmark-batches", {**request, "workspaces": request["workspaces"] * 2}, 400)
            first_workspace = Path(request["workspaces"][0]["path"])
            selected = (first_workspace / "PROJECT_PLANNER.md").read_bytes()
            (first_workspace / "PROJECT_PLANNER.md").write_text("Changed instructions")
            rejects(client, "POST", "/v1/benchmark-batches", request, 400)
            (first_workspace / "PROJECT_PLANNER.md").write_text("word " * 201)
            rejects(client, "POST", "/v1/benchmark-batches", request, 400)
            (first_workspace / "PROJECT_PLANNER.md").write_bytes(selected)
            assert not client.request("GET", "/v1/campaigns")["campaigns"]
            batch = client.request("POST", "/v1/benchmark-batches", request)["batch"]
            assert "profile_config" not in batch and "template" not in batch
            (data / "coordinator-profiles.json").write_text("{}")
            first = wait_for(lambda: (b if (b := status(batch)) and sum(c["status"] == "running" for c in b["cases"]) == 2 else None))
            assert all("run_id" not in c and not Path(c["directory"]).exists() for c in first["cases"] if c["status"] == "queued")
            for campaign in first["campaigns"].values():
                rejects(client, "POST", f"/v1/campaigns/{campaign}/score", {}, 409)
            for case in [case for case in first["cases"] if case["status"] == "running"]:
                wait_for(lambda: (Path(case["directory"]) / "worker.pulse").exists(), timeout=6)
                wait_for(lambda: client.request("GET", f'/v1/runs/{case["run_id"]}/coordinator')["progress"]["phase"] == "planner_exited", timeout=3)
                assert status(batch)["cases"][first["cases"].index(case)]["status"] == "running"
            peak = 0
            until = time.monotonic() + 34
            while time.monotonic() < until:
                current = status(batch)
                running = [c for c in current["cases"] if c["status"] == "running"]
                peak = max(peak, len(running))
                assert len(running) <= 2 and len({c["workspace"] for c in running}) == len(running)
                if current["status"] == "completed": break
                time.sleep(0.05)
            assert current["status"] == "completed", current
            assert len(current["cases"]) == 6 and all(c["status"] == "deadline" for c in current["cases"]), current
            pids = set()
            assert client.request("POST", f'/v1/benchmark-batches/{batch["id"]}/dashboard', {})["batch"]["cases"] == current["cases"]
            for case in current["cases"]:
                directory = Path(case["directory"])
                observed = json.loads((directory / "observed.json").read_text())
                pids.add(observed["pid"])
                assert observed["prompt"] == template.read_text().replace("{workspace_name}", case["workspace_name"]).replace("{benchmark_task}", case["benchmark"])
                settings = next(v for k, v in observed["config"]["workspaces"].items() if Path(k).samefile(case["workspace"]))
                assert settings["limits"]["words"] == case["settings"]["limits"]["words"] and settings["updates"] == case["settings"]["updates"]
                assert settings["prompts"] == case["settings"]["prompts"]
                for name in ["PROJECT_PLANNER.md", "WORKER_PROTOCOL.md"]:
                    assert (directory / name).read_bytes() == (Path(case["workspace"]) / name).read_bytes()
                frozen_pulses([directory / "planner.pulse", directory / "worker.pulse"])
                state = client.request("GET", f'/v1/runs/{case["run_id"]}')
                assert state["run"]["status"] == "frozen" and state["run"]["submission_id"] and state["report"] is None
                if case["benchmark"] == "game_build": assert state["run"]["inspiration"] == case["inspiration"]
                if case["benchmark"] == "dataset_dashboard": assert state["run"]["dataset"] == case["dataset"]
                rejects(client, "POST", f'/v1/runs/{case["run_id"]}/submissions', {}, 409)
            assert len(pids) == 6 and not client.request("GET", "/v1/judgments")["judgments"]
            evidence.update(cases=6, peak_parallel=peak, exact_prompts=True, only_selected_documents_at_start=True, prompt_hashes_verified=True, settings_preserved=True,
                            per_run_inspiration_preserved=True, per_run_dataset_preserved=True,
                            fresh_planners=True, queued_clocks_not_started=True, exited_planner_workers_preserved=True,
                            deadline_kills_planner_and_worker_descendants=True, late_upload_rejected=True, scoring_deferred=True)
            (data / "coordinator-profiles.json").write_text(json.dumps({"fixture": profile}))
            batch = client.request("POST", "/v1/benchmark-batches", config("restart", 1, 20))["batch"]
            first = wait_for(lambda: (b if (b := status(batch))["cases"][0]["status"] == "running" else None))
            pulse = Path(first["cases"][0]["directory"]) / "worker.pulse"
            wait_for(pulse.exists, timeout=6)
            service.terminate(); service.wait(timeout=5)
            service = launch()
            wait_for(lambda: client.request("GET", "/health"))
            resumed = wait_for(lambda: (b if (b := status(batch))["cases"][1]["status"] == "running" else None))
            assert [c["directory"] for c in resumed["cases"]] == [c["directory"] for c in first["cases"]]
            assert resumed["order_seed"] == first["order_seed"] == 960
            assert resumed["cases"][0]["status"] == "interrupted"
            time.sleep(0.5)
            frozen_pulses([pulse])
            second_pulse = Path(resumed["cases"][1]["directory"]) / "planner.pulse"
            wait_for(second_pulse.exists, timeout=6)
            stopped = client.request("POST", f'/v1/benchmark-batches/{batch["id"]}/stop', {})["batch"]
            assert stopped["status"] == "stopped" and all(c["status"] not in {"running", "queued"} for c in stopped["cases"])
            frozen_pulses([second_pulse])
            original = (unrelated / "worker.pulse").read_text()
            wait_for(lambda: (unrelated / "worker.pulse").read_text() != original, timeout=3)
            evidence["unrelated_workspace_kept_running"] = True
            evidence.update(service_restart_stops_scoped_workers=True, restart_preserves_queue_without_replaying=True, stop_cancels_queue_and_workers=True)
            request = config("eight", seconds=20, count=10)
            del request["max_parallel"]
            batch = client.request("POST", "/v1/benchmark-batches", request)["batch"]
            assert batch["max_parallel"] == 8
            full = wait_for(lambda: (b if (b := status(batch)) and sum(c["status"] == "running" for c in b["cases"]) == 8 else None))
            running = [c for c in full["cases"] if c["status"] == "running"]
            assert len({c["workspace"] for c in running}) == 8
            assert len({c["benchmark"] for c in running}) > 1
            wait_for(lambda: all((Path(c["directory"]) / "observed.json").exists() for c in running), timeout=10)
            for _ in range(5):
                assert sum(c["status"] == "running" for c in status(batch)["cases"]) <= 8
                time.sleep(0.1)
            client.request("POST", f'/v1/benchmark-batches/{batch["id"]}/stop', {})
            evidence.update(default_parallel_cap=8, eight_concurrent_distinct_workspaces=True, mixed_benchmarks_dispatched=True, shuffled_order_survives_restart=True)
        except Exception:
            args.evidence.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(data, args.evidence.parent / "failed-batch-smoke", dirs_exist_ok=True, ignore=shutil.ignore_patterns("*.lock", "benchmark.sqlite3*"))
            raise
        finally:
            service.terminate(); service.wait(timeout=5)
            for task in board_client.request("GET", "/api/state")["tasks"]:
                if task["active"]:
                    board_client.request("POST", f'/api/tasks/{task["id"]}/stop', {"actor":"planner", "reason":"Fixture cleanup"})
            time.sleep(0.3)
            board.terminate(); board.wait(timeout=5); log.close()
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(evidence, indent=2))
    print(json.dumps(evidence, indent=2))


if __name__ == "__main__":
    main()
