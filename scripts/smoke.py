#!/usr/bin/env python3
"""Exercise both real HTTP lifecycles in isolated temporary data, with no LLM dependency."""
import argparse
import copy
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

from harness import ApiError, Client, snapshot


ROOT = Path(__file__).resolve().parents[1]


def wait_for(fn, timeout=40):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = fn()
            if last:
                return last
        except (OSError, ApiError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"condition did not complete: {last}")


def rejects(client, method, path, body, code):
    try:
        client.request(method, path, body)
    except ApiError as error:
        assert error.status == code, str(error)
    else:
        raise AssertionError(f"{path} should reject with {code}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "debug" / ("harness-bench.exe" if os.name == "nt" else "harness-bench"))
    parser.add_argument("--evidence", type=Path, default=ROOT / "artifacts" / "smoke-results.json")
    parser.add_argument("--with-rust", action="store_true", help="Also build and play the Rust submission example (requires configured Cargo toolchain)")
    args = parser.parse_args()
    binary = args.binary.resolve()
    evidence = {"checks": []}
    with tempfile.TemporaryDirectory(prefix="harness-http-test-") as temp:
        data = Path(temp)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        url = f"http://127.0.0.1:{port}"
        kwargs = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
        log = (data / "service.log").open("wb")
        def launch():
            return subprocess.Popen([str(binary), "serve", "--bind", f"127.0.0.1:{port}", "--data", str(data)], stdout=log, stderr=log, **kwargs)
        process = launch()
        try:
            wait_for(lambda: Client(url).request("GET", "/health"))
            admin = Client(url, (data / "admin-token").read_text())
            judge = Client(url, (data / "judge-token").read_text())
            rejects(Client(url), "POST", "/v1/campaigns", {}, 401)
            config = {"benchmark": "chess960", "development_seconds": 3, "enrollment_seconds": 60,
                      "positions": 1, "max_plies": 8, "seed": 41}
            campaign = admin.request("POST", "/v1/campaigns", config)["campaign"]
            a = admin.request("POST", "/v1/runs", {"campaign_id": campaign["id"], "agent": "valid"})
            b = admin.request("POST", "/v1/runs", {"campaign_id": campaign["id"], "agent": "invalid"})
            ac, bc = Client(url, a["run_token"]), Client(url, b["run_token"])
            submission = snapshot(ROOT / "examples" / "chess_python")
            bad = copy.deepcopy(submission)
            for file in bad["files"]:
                if file["path"] == "picker.py":
                    file["text"] = file["text"].replace("return allowed_moves[0]", 'return "a1a1"')
            ac.request("POST", f'/v1/runs/{a["run"]["id"]}/submissions', bad)
            final_snapshot = ac.request("POST", f'/v1/runs/{a["run"]["id"]}/submissions', submission)["submission"]
            bc.request("POST", f'/v1/runs/{b["run"]["id"]}/submissions', bad)
            rejects(ac, "GET", f'/v1/runs/{b["run"]["id"]}', None, 401)
            admin.request("POST", f'/v1/campaigns/{campaign["id"]}/score', {})
            rejects(admin, "POST", "/v1/runs", {"campaign_id": campaign["id"], "agent": "late"}, 409)
            result = wait_for(lambda: ac.request("GET", f'/v1/runs/{a["run"]["id"]}')["report"])
            bad_result = bc.request("GET", f'/v1/runs/{b["run"]["id"]}')["report"]
            games = admin.request("GET", f'/v1/campaigns/{campaign["id"]}/games')["games"]
            assert len(games) == 12
            assert result["metrics"]["artifact_sha256"] == final_snapshot["sha256"]
            assert result["metrics"]["lines_of_code"] > 0
            assert bad_result["metrics"]["forfeits"] == 6
            assert bad_result["metrics"]["elo_change"] < 0
            assert all(x["initial_elo"] == 1500 for x in result["pool"])
            rejects(ac, "POST", f'/v1/runs/{a["run"]["id"]}/submissions', submission, 409)
            evidence["chess"] = {"games": len(games), "valid": result["metrics"], "invalid": bad_result["metrics"]}
            evidence["checks"] += ["authentication and scoped tokens", "last snapshot frozen", "late enrollment/upload rejected", "automatic color-balanced tournament", "illegal moves forfeit and Elo/LOC persisted"]

            # Exercise the builder adapter and its independent process-tree watchdog.
            adapter_campaign = admin.request("POST", "/v1/campaigns", {"benchmark": "chess960", "development_seconds": 2,
                "enrollment_seconds": 60, "positions": 1, "max_plies": 4, "baselines": False,
                "pool_run_ids": [a["run"]["id"]]})["campaign"]
            selections = data / "workspace-settings.json"
            selections.write_text(json.dumps({"planner":"sol_6_1_light","worker":"sol_6_1_light"}))
            adapter_work = data / "builder-work"
            fake_builder = "import shutil,subprocess,sys,time;shutil.copytree(" + repr(str(ROOT / 'examples' / 'chess_python')) + ",'.',dirs_exist_ok=True);subprocess.Popen([sys.executable,'-c',\"import time,pathlib;time.sleep(3);pathlib.Path('escaped').write_text('bad')\"]);time.sleep(60)"
            started_at = time.monotonic()
            subprocess.run([sys.executable, str(ROOT / "scripts" / "harness.py"), "--url", url, "--token-file", str(data / "admin-token"),
                            "run-agent", adapter_campaign["id"], "adapter-fixture", str(adapter_work), "--interval", "0.2",
                            "--workspace-settings", str(selections), "--command-json", json.dumps([sys.executable, "-c", fake_builder, "{model}", "{effort}"])], check=True, timeout=10, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
            assert time.monotonic() - started_at < 6
            adapter_run = json.loads((adapter_work / ".harness" / "run.json").read_text())
            adapter_client = Client(url, adapter_run["run_token"])
            admin.request("POST", f'/v1/campaigns/{adapter_campaign["id"]}/score', {})
            adapter_report = wait_for(lambda: adapter_client.request("GET", f'/v1/runs/{adapter_run["run"]["id"]}')["report"])
            assert adapter_report["metrics"]["games"] == 2
            assert ac.request("GET", f'/v1/runs/{a["run"]["id"]}')["report"] == result
            evidence["checks"].append("builder adapter checkpoints, enforces development time, and reuses archived opponent without changing old score")
            if args.with_rust:
                rust_campaign = admin.request("POST", "/v1/campaigns", {"benchmark": "chess960", "development_seconds": 1,
                    "enrollment_seconds": 60, "positions": 1, "max_plies": 4, "baselines": False,
                    "pool_run_ids": [a["run"]["id"]]})["campaign"]
                rust_run = admin.request("POST", "/v1/runs", {"campaign_id": rust_campaign["id"], "agent": "rust-fixture"})
                rust_client = Client(url, rust_run["run_token"])
                rust_client.request("POST", f'/v1/runs/{rust_run["run"]["id"]}/submissions', snapshot(ROOT / "examples" / "chess_rust"))
                admin.request("POST", f'/v1/campaigns/{rust_campaign["id"]}/score', {})
                rust_report = wait_for(lambda: rust_client.request("GET", f'/v1/runs/{rust_run["run"]["id"]}')["report"], timeout=140)
                assert rust_report["metrics"]["forfeits"] == 0, rust_client.request("GET", f'/v1/runs/{rust_run["run"]["id"]}')
                evidence["rust"] = rust_report["metrics"]
                evidence["checks"].append("frozen Rust source compiles on host and plays through the executable protocol")

            game_campaign = admin.request("POST", "/v1/campaigns", {"benchmark": "game_build", "development_seconds": 1,
                "enrollment_seconds": 60, "judge_seconds": 2, "score_seconds": 5, "seed": 200})["campaign"]
            built = admin.request("POST", "/v1/runs", {"campaign_id": game_campaign["id"], "agent": "game-fixture"})
            assert len(built["themes"]) == 3 and built["themes"] == built["run"]["inspiration"]["themes"]
            gc = Client(url, built["run_token"])
            gc.request("POST", f'/v1/runs/{built["run"]["id"]}/submissions', snapshot(ROOT / "examples" / "game"))
            admin.request("POST", f'/v1/campaigns/{game_campaign["id"]}/score', {})
            claimed = wait_for(lambda: (r if r["job"] else None) if (r := judge.request("POST", "/v1/judges/claim", {"judge": "fixture-test"})) else None)
            job = claimed["job"]
            assert claimed["themes"] == built["themes"]
            assert job["launch_error"] is None, job
            jc = Client(url, claimed["judgment_token"])
            assert judge.request("POST", "/v1/judges/claim", {"judge": "second"})["job"] is None
            endpoint = f'/v1/judgments/{job["id"]}'
            with urllib.request.urlopen(job["ui_url"]) as page:
                assert b"Harbor Ledger" in page.read()
            observed = jc.request("POST", endpoint + "/action", {"action": "observe", "params": {}})
            assert observed["response"]["result"]["turn"] == 0
            stepped = jc.request("POST", endpoint + "/action", {"action": "step", "params": {"ticks": 1000}})
            assert stepped["response"]["result"]["turn"] == 1000
            with urllib.request.urlopen(job["ui_url"] + "state") as page:
                assert json.load(page)["turn"] == 1000
            score = {key: {"score": 3, "evidence": "Synthetic transport test; not a real subjective assessment."}
                     for key in ["interface", "efficiency", "gameplay", "visuals", "testability"]}
            score.update(compared_games=["Anno 1800"], notes="Fixture-only score, proves storage and validation.")
            rejects(jc, "POST", endpoint + "/score", score, 409)
            wait_for(lambda: jc.request("GET", endpoint)["job"]["status"] == "scoring")
            rejects(jc, "POST", endpoint + "/action", {"action": "observe"}, 409)
            try:
                urllib.request.urlopen(job["ui_url"], timeout=0.5)
            except OSError:
                pass
            else:
                raise AssertionError("game HTTP process still running after cutoff")
            wrong = copy.deepcopy(score)
            wrong["interface"]["score"] = 6
            rejects(jc, "POST", endpoint + "/score", wrong, 400)
            report = jc.request("POST", endpoint + "/score", score)["report"]
            assert report["total_out_of_25"] == 15 and report["mean_out_of_5"] == 3
            assert report["themes"] == built["themes"] and report["inspiration"] == built["run"]["inspiration"]
            rejects(jc, "POST", endpoint + "/score", score, 409)
            evidence["game"] = report
            evidence["checks"] += ["random reproducible themes", "exclusive judge claims", "GUI and API share simulation", "game killed at judge deadline", "early/invalid/duplicate scores rejected", "five scores and telemetry persisted"]

            # Exercise the external judge adapter with an explicitly synthetic fixture.
            jc_campaign = admin.request("POST", "/v1/campaigns", {"benchmark": "game_build", "development_seconds": 1,
                "enrollment_seconds": 60, "judge_seconds": 1, "score_seconds": 5})["campaign"]
            jr = admin.request("POST", "/v1/runs", {"campaign_id": jc_campaign["id"], "agent": "judge-adapter-fixture"})
            jrc = Client(url, jr["run_token"])
            jrc.request("POST", f'/v1/runs/{jr["run"]["id"]}/submissions', snapshot(ROOT / "examples" / "game"))
            admin.request("POST", f'/v1/campaigns/{jc_campaign["id"]}/score', {})
            wait_for(lambda: any(j["run_id"] == jr["run"]["id"] and j["status"] == "pending" for j in admin.request("GET", "/v1/judgments")["judgments"]))
            fake_judge = "import json,os,pathlib;pathlib.Path(os.environ['HARNESS_SCORE_FILE']).write_text(" + repr(json.dumps(score)) + ")"
            subprocess.run([sys.executable, str(ROOT / "scripts" / "harness.py"), "--url", url, "--token-file", str(data / "judge-token"),
                            "judge-worker", "synthetic-adapter-fixture", str(data / "judge-work"), "--once",
                            "--workspace-settings", str(selections), "--command-json", json.dumps([sys.executable, "-c", fake_judge, "{model}", "{effort}"])], check=True, timeout=10, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
            assert jrc.request("GET", f'/v1/runs/{jr["run"]["id"]}')["report"]["total_out_of_25"] == 15
            assert not (adapter_work / "escaped").exists()
            evidence["checks"].append("judge adapter submits rubric after cutoff; builder descendants remain terminated")

            # Persist an active development deadline across a real process restart.
            recovery = admin.request("POST", "/v1/campaigns", {"benchmark": "game_build", "development_seconds": 2, "enrollment_seconds": 60, "judge_seconds": 1, "score_seconds": 1})["campaign"]
            unfinished = admin.request("POST", "/v1/runs", {"campaign_id": recovery["id"], "agent": "missing"})
            admin.request("POST", f'/v1/campaigns/{recovery["id"]}/score', {})
            process.terminate()
            process.wait(timeout=10)
            time.sleep(2.1)
            process = launch()
            wait_for(lambda: Client(url).request("GET", "/health"))
            assert ac.request("GET", f'/v1/runs/{a["run"]["id"]}')["report"] == result
            assert gc.request("GET", f'/v1/runs/{built["run"]["id"]}')["report"] == report
            recovered = wait_for(lambda: (r if r["run"]["status"] == "failed" else None)
                if (r := admin.request("GET", f'/v1/runs/{unfinished["run"]["id"]}')) else None)
            assert recovered["run"]["deadline_ms"] == unfinished["run"]["deadline_ms"]
            assert "no accepted submission" in recovered["run"]["error"].lower()
            assert not any(j["run_id"] == unfinished["run"]["id"] for j in admin.request("GET", "/v1/judgments")["judgments"])
            evidence["checks"].append("restart preserves scores and enforces elapsed deadlines")
            timeout_campaign = admin.request("POST", "/v1/campaigns", {"benchmark":"game_build", "development_seconds":1, "judge_seconds":1, "score_seconds":1})["campaign"]
            timeout_run = admin.request("POST", "/v1/runs", {"campaign_id":timeout_campaign["id"], "agent":"broken-submission"})
            admin.request("POST", f'/v1/runs/{timeout_run["run"]["id"]}/submissions', {"manifest":{"run":["missing-fixture-program"],"ui":"web"},"files":[{"path":"main.py","text":"print('fixture')"}]})
            admin.request("POST", f'/v1/campaigns/{timeout_campaign["id"]}/score', {})
            wait_for(lambda: any(j["run_id"] == timeout_run["run"]["id"] and j["status"] == "pending" for j in admin.request("GET", "/v1/judgments")["judgments"]))
            missing = judge.request("POST", "/v1/judges/claim", {"judge": "timeout-fixture"})
            assert missing["job"]["launch_error"] is not None
            wait_for(lambda: admin.request("GET", f'/v1/runs/{timeout_run["run"]["id"]}')["run"]["status"] == "failed")
            assert admin.request("GET", f'/v1/runs/{timeout_run["run"]["id"]}')["report"] is None
            evidence["checks"].append("missing solution diagnostics and score timeout produce no fabricated scores")
        finally:
            process.terminate()
            process.wait(timeout=10)
            log.close()
            evidence["service_log"] = (data / "service.log").read_text(errors="replace")
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(evidence, indent=2), encoding="utf-8")
    print(f"PASS: {len(evidence['checks'])} HTTP lifecycle checks; evidence: {args.evidence}")


if __name__ == "__main__":
    main()
