import json
from contextlib import closing
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


@unittest.skipUnless(os.environ.get("HARNESS_TEST_BINARY"), "HARNESS_TEST_BINARY is required")
class ScoringApi(unittest.TestCase):
    def test_single_request_returns_complete_json_and_reuses_results(self):
        with tempfile.TemporaryDirectory(prefix="harness-score-api-") as temporary:
            root = Path(temporary)

            class Coordinator(BaseHTTPRequestHandler):
                def do_GET(self):
                    payload = json.dumps({"dataDirectory":str(root)}).encode()
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.write(payload)

                def log_message(self, *args):
                    pass

            coordinator = ThreadingHTTPServer(("127.0.0.1", 0), Coordinator)
            threading.Thread(target=coordinator.serve_forever, daemon=True).start()
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            url = f"http://127.0.0.1:{port}"
            env = {**os.environ, "AGENT_FAMILY":"codex"}
            flags = {"creationflags":subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
            with (root / "service.log").open("wb") as log:
                process = subprocess.Popen([os.environ["HARNESS_TEST_BINARY"], "serve", "--bind", f"127.0.0.1:{port}", "--data", str(root)],
                                           stdout=log, stderr=log, env=env, **flags)
            try:
                deadline = time.monotonic() + 20
                while True:
                    try:
                        with urllib.request.urlopen(url + "/health", timeout=1):
                            break
                    except OSError:
                        if process.poll() is not None or time.monotonic() >= deadline:
                            self.fail((root / "service.log").read_text())
                        time.sleep(.05)
                token = (root / "admin-token").read_text().strip()
                names = ["chess960", "game_build", "dataset_dashboard", "maze_game", "ml_predictor"]
                batch = {"id":"fixture", "status":"completed", "profile":"fixture",
                         "profile_config":{"coordinator_url":f"http://127.0.0.1:{coordinator.server_port}"},
                         "campaigns":{name:name for name in names},
                         "cases":[{"benchmark":name, "workspace":"fixture-workspace", "status":"failed", "run_id":None,
                                   "campaign_id":name, "settings":{}, "error":"fixture: never launched"} for name in names]}

                def save():
                    with closing(sqlite3.connect(root / "benchmark.sqlite3")) as db:
                        db.execute("INSERT OR REPLACE INTO batches VALUES (?, ?)", (batch["id"], json.dumps(batch)))
                        db.commit()

                def request(auth=token, body=None):
                    req = urllib.request.Request(url + "/v1/benchmark-batches/fixture/score", data=json.dumps(body or {}).encode(),
                                                 headers={"Authorization":"Bearer " + auth,"Content-Type":"application/json"})
                    with urllib.request.urlopen(req, timeout=30) as response:
                        self.assertIn("application/json", response.headers["Content-Type"])
                        return json.load(response)

                save()
                with self.assertRaises(urllib.error.HTTPError) as unauthorized:
                    request(auth="invalid")
                self.assertEqual(unauthorized.exception.code, 401)
                unauthorized.exception.close()
                batch["cases"][0]["status"] = "running"
                save()
                with self.assertRaises(urllib.error.HTTPError) as premature:
                    request()
                self.assertEqual(premature.exception.code, 409)
                premature.exception.close()
                batch["cases"][0]["status"] = "failed"
                save()
                with self.assertRaises(urllib.error.HTTPError) as invalid:
                    request(body={"max_judges":0})
                self.assertEqual(invalid.exception.code, 400)
                invalid.exception.close()
                report = request()
                self.assertEqual(report["schema_version"], "benchmark-score-report.v2")
                self.assertEqual(len(report["cases"]), 5)
                self.assertEqual(len(report["ranking"][0]["calculation"]["terms"]), 5)
                self.assertTrue(all(case["score"] == 0 and case["status"] == "no_submission" for case in report["cases"]))
                self.assertEqual(report["ranking"], request()["ranking"])
                saved = root / "batch-scores/fixture/workspace-scores.v5/scores.json"
                self.assertEqual(json.loads(saved.read_text())["ranking"], report["ranking"])
                self.assertFalse(list(root.rglob("*.html")))
            finally:
                process.terminate()
                process.wait(timeout=10)
                coordinator.shutdown()
                coordinator.server_close()


if __name__ == "__main__":
    unittest.main()
