"""Small shared-state GUI/API economy for exercising the evaluation pipeline."""
import copy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sys
import threading

LOCK = threading.RLock()
SAVE = None


def initial():
    return {"turn": 0, "coins": 100.0, "grain": 20.0, "price": 3.0,
            "tiles": [{"kind": "port" if i == 0 else "plain", "level": 1 if i == 0 else 0} for i in range(36)]}


STATE = initial()


def act(action, params):
    global STATE, SAVE
    with LOCK:
        if action == "describe":
            return {"actions": {"observe": {}, "reset": {}, "step": {"ticks": "integer 1..100000"},
                                "build": {"tile": "integer 0..35", "kind": "farm or market"},
                                "trade": {"amount": "positive to sell grain, negative to buy"}, "save": {}, "load": {}},
                    "controls": "Click tile to build; Space advances one turn; S saves; L loads. GUI and JSONL share state."}
        if action == "reset":
            STATE = initial()
        elif action == "step":
            ticks = max(1, min(100000, int(params.get("ticks", 1))))
            for _ in range(ticks):
                farms = sum(t["level"] for t in STATE["tiles"] if t["kind"] == "farm")
                markets = sum(t["level"] for t in STATE["tiles"] if t["kind"] == "market")
                STATE["grain"] += farms * 2
                STATE["coins"] += markets * 0.8
                STATE["turn"] += 1
                STATE["price"] = round(2.0 + (STATE["turn"] % 11) * 0.2, 2)
        elif action == "build":
            index = int(params["tile"])
            kind = params.get("kind", "farm")
            if not 0 <= index < 36 or kind not in {"farm", "market"}:
                raise ValueError("invalid tile or building")
            tile = STATE["tiles"][index]
            cost = 15 * (tile["level"] + 1)
            if tile["kind"] == "port" or STATE["coins"] < cost:
                raise ValueError("cannot build here or insufficient coins")
            STATE["coins"] -= cost
            tile.update(kind=kind, level=tile["level"] + 1)
        elif action == "trade":
            amount = float(params.get("amount", 1))
            if amount > STATE["grain"] or (amount < 0 and -amount * STATE["price"] > STATE["coins"]):
                raise ValueError("insufficient resources")
            STATE["grain"] -= amount
            STATE["coins"] += amount * STATE["price"]
        elif action == "save":
            SAVE = copy.deepcopy(STATE)
            Path("save.json").write_text(json.dumps(SAVE))
        elif action == "load":
            STATE = json.loads(Path("save.json").read_text())
        elif action != "observe":
            raise ValueError("unknown action")
        return copy.deepcopy(STATE)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        if self.path == "/state":
            data, content_type = json.dumps(act("observe", {})).encode(), "application/json"
        else:
            data, content_type = Path("index.html").read_bytes(), "text/html; charset=utf-8"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            data = json.dumps({"result": act(request["action"], request.get("params", {}))}).encode()
            code = 200
        except Exception as error:
            data, code = json.dumps({"error": str(error)}).encode(), 400
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", int(os.environ.get("HARNESS_PORT", "8765"))), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print(json.dumps({"ready": True}), flush=True)
    for line in sys.stdin:
        request = json.loads(line)
        try:
            response = {"id": request["id"], "result": act(request["action"], request.get("params", {}))}
        except Exception as error:
            # Application-level invalid actions are ordinary results; a top-level error is fatal in the harness protocol.
            response = {"id": request["id"], "result": {"error": str(error)}}
        print(json.dumps(response), flush=True)
