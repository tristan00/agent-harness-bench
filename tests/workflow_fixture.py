"""Deterministic transport fixtures, never model evaluations or subjective judges."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def read(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def main():
    if sys.argv[1] == "pulse":
        pulse = Path(sys.argv[2])
        while True:
            pulse.write_text(str(time.time()))
            time.sleep(0.05)
    elif sys.argv[1] == "plan":
        request = read(os.environ["HARNESS_PLANNER_REQUEST"])
        number = request["iteration"]
        result = {"action": "plan", "title": f"Iteration {number}", "outcome": "Produce a runnable benchmark artifact", "tasks": [{
            "key": "build", "title": f"Build iteration {number}", "outcome": f"Fixture iteration {number}",
            "acceptance": ["Artifact satisfies the executable protocol"], "after": [], "lane": "code",
        }]}
        Path(os.environ["HARNESS_PLANNER_RESULT"]).write_text(json.dumps(result), encoding="utf-8")
        assert "deadline_ms" not in request and "remaining_seconds" not in request
        usage = {"input_tokens": 100, "cached_input_tokens": 60, "output_tokens": 10, "reasoning_output_tokens": 2, "total_tokens": 110}
        Path(os.environ["HARNESS_PLANNER_REQUEST"]).with_name("usage.json").write_text(json.dumps(usage), encoding="utf-8")
    else:
        request = read(os.environ["COORDINATOR_REQUEST"])
        number = int(request["task"]["title"].split()[-1])
        workspace = Path(request["task"]["cwd"])
        stage = request["stage"]
        if stage == "implementation":
            if number >= 3:
                pulse = Path(os.environ["HARNESS_TEST_PULSE"])
                child = subprocess.Popen([sys.executable, __file__, "pulse", str(pulse)])
                (workspace / "unfinished.txt").write_text("This must never replace the last completion")
                pulse.with_suffix(".pid").write_text(str(child.pid))
                time.sleep(120)
            else:
                source = ROOT / "examples" / os.environ["HARNESS_TEST_EXAMPLE"]
                shutil.copytree(source, workspace, dirs_exist_ok=True, ignore=shutil.ignore_patterns("__pycache__"))
                (workspace / "iteration.txt").write_text(str(number))
        result = {"status": "accepted" if stage == "review" else "completed", "summary": "Protocol fixture completed",
                  "criteria": ["passed"] * len(request["task"]["acceptance"]), "findings": []}
        Path(os.environ["COORDINATOR_RESULT"]).write_text(json.dumps(result), encoding="utf-8")


if __name__ == "__main__":
    main()
