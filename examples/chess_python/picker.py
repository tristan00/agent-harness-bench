"""Minimal protocol example, not a competitive chess solution."""
import json
import sys


def pick_move(fen, allowed_moves):
    return allowed_moves[0]


print(json.dumps({"ready": True}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    move = pick_move(request["fen"], request["allowed_moves"])
    print(json.dumps({"id": request["id"], "move": move}), flush=True)
