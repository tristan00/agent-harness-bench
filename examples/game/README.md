# Harbor Ledger protocol fixture

A deliberately small game for testing the benchmark pipeline, not a reference answer to the game benchmark. Requires Python 3; no packages.

Run `python game.py` and open `http://127.0.0.1:8765`. In the evaluator, use the judgment's `ui_url` instead. The evaluator chooses the port.

Click map tiles to build/upgrade farms or markets. Space advances a turn, S saves, L loads. Buttons buy/sell grain, step 100 turns, reset and toggle fullscreen. Saves persist to `save.json` in this evaluation's working copy.

JSONL actions: `describe`, `observe`, `reset`, `step` with `ticks` up to 100000, `build` with `tile` and `kind`, `trade` with signed `amount`, `save`, `load`. GUI and API share the same state. Invalid gameplay actions are represented as an error inside `result`; transport failures are separate.
