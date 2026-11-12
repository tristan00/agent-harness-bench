#!/usr/bin/env python3
"""Run both workflows against the actual coordinator using deterministic protocol fixtures."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

from harness import Client, snapshot
from smoke import rejects, wait_for

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--coordinator", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    evidence = {"kind": "deterministic integration fixtures, not model scores", "checks": []}
    with tempfile.TemporaryDirectory(prefix="benchmark-coordinator-") as temporary:
        data = Path(temporary)
        profiles = {}
        for name, example in [("chess960", "chess_python"), ("game_build", "game")]:
            worker = [sys.executable, str(ROOT / "tests/workflow_fixture.py"), "work"]
            profiles[name] = {
                "command": [sys.executable, str(ROOT / "scripts/coordinator.py")],
                "coordinator_command": [str(args.coordinator.resolve())],
                "planner_command": [sys.executable, str(ROOT / "tests/workflow_fixture.py"), "plan"],
                "workers": {"implementation": worker, "review": worker, "replan": worker},
                "capabilities": ["process"],
                "env": {"HARNESS_TEST_EXAMPLE": example, "HARNESS_TEST_PULSE": str(data / f"{name}.pulse")},
                "workspace": {"updates": "events" if name == "chess960" else "poll"},
            }
        (data / "coordinator-profiles.json").write_text(json.dumps(profiles), encoding="utf-8")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        client = Client(f"http://127.0.0.1:{port}")
        options = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
        log = (data / "service.log").open("wb")
        def launch():
            return subprocess.Popen([str(args.binary.resolve()), "serve", "--bind", f"127.0.0.1:{port}", "--data", str(data)], stdout=log, stderr=log, **options)
        service = launch()
        try:
            wait_for(lambda: client.request("GET", "/health"))
            client.token = (data / "admin-token").read_text()
            assert set(client.request("GET", "/v1/coordinator-profiles")["profiles"]) == {"chess960", "game_build"}
            runs = {}
            for benchmark in profiles:
                campaign = client.request("POST", "/v1/campaigns", {"benchmark": benchmark, "development_seconds": 18, "positions": 1, "max_plies": 4})["campaign"]
                started = client.request("POST", "/v1/coordinator-runs", {"campaign_id": campaign["id"], "agent": "fixture", "profile": benchmark})
                assert started["workflow"]["status"] == "running", started
                runs[benchmark] = started["run"]["id"]
                client.request("POST", f'/v1/campaigns/{campaign["id"]}/close', {})
                if benchmark == "chess960":
                    client.request("POST", f'/v1/campaigns/{campaign["id"]}/score', {})
            for benchmark, run in runs.items():
                wait_for(lambda: len(client.request("GET", f"/v1/runs/{run}/iterations")["iterations"]) == 2, timeout=16)
                pulse = data / f"{benchmark}.pulse"
                wait_for(lambda: pulse.exists(), timeout=4)
                stopped = wait_for(lambda: (state if (state := client.request("GET", f"/v1/runs/{run}/coordinator"))["workflow"]["status"] == "deadline" else None), timeout=22)
                before = pulse.read_text()
                time.sleep(0.3)
                assert pulse.read_text() == before, "grandchild survived deadline"
                iterations = client.request("GET", f"/v1/runs/{run}/iterations")["iterations"]
                workspace = Path(stopped["progress"]["workspace"])
                assert (workspace / "unfinished.txt").exists()
                progress = stopped["progress"]
                assert progress["updates"] == profiles[benchmark]["workspace"]["updates"], progress
                assert progress["planner_usage"]["total_tokens"] > 0 and "worker_tokens" in progress, progress
                rejects(client, "POST", f"/v1/runs/{run}/iterations", {"iteration": 3, "submission": snapshot(workspace)}, 409)
                state = client.request("GET", f"/v1/runs/{run}")
                assert state["run"]["submission_id"] == iterations[-1]["submission"]["id"]
                evidence["checks"].append({"benchmark": benchmark, "accepted_iterations": len(iterations), "last_submission": iterations[-1]["submission"]["id"], "deadline_stopped_grandchild": True, "late_iteration_rejected": True, "updates": progress["updates"], "planner_usage": progress["planner_usage"]})
            chess = wait_for(lambda: client.request("GET", f'/v1/runs/{runs["chess960"]}')["report"])
            game = client.request("GET", f'/v1/runs/{runs["game_build"]}')
            assert game["run"]["status"] == "frozen", game
            assert not any(j["run_id"] == runs["game_build"] for j in client.request("GET", "/v1/judgments")["judgments"])
            evidence["chess_scored_after_deadline"] = chess["benchmark"] == "chess960"
            evidence["game_waiting_for_later_judge"] = True
            service.terminate()
            service.wait(timeout=10)
            service = launch()
            wait_for(lambda: client.request("GET", "/health"))
            for run in runs.values():
                assert len(client.request("GET", f"/v1/runs/{run}/iterations")["iterations"]) == 2
            evidence["restart_preserves_iterations"] = True
        except Exception:
            args.evidence.parent.mkdir(parents=True, exist_ok=True)
            import shutil
            shutil.copytree(data, args.evidence.parent / "failed-coordinator-smoke", dirs_exist_ok=True, ignore=shutil.ignore_patterns("*.lock", "benchmark.sqlite3*"))
            raise
        finally:
            service.terminate()
            service.wait(timeout=10)
            log.close()
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(evidence, indent=2), encoding="utf-8")
    print(json.dumps(evidence, indent=2))


if __name__ == "__main__":
    main()
