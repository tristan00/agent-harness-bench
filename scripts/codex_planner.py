#!/usr/bin/env python3
"""Optional Codex implementation of the replaceable JSON planner contract."""
import json
import os
from pathlib import Path
import subprocess
import sys

USAGE_FIELDS = ("input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens")


def main():
    request_file = Path(os.environ["HARNESS_PLANNER_REQUEST"])
    request = json.loads(request_file.read_text(encoding="utf-8"))
    text = {"type": "string"}
    task = {"type": "object", "additionalProperties": False, "properties": {
        "key": text, "title": text, "outcome": text,
        "acceptance": {"type": "array", "items": text, "minItems": 1, "maxItems": 3},
        "after": {"type": "array", "items": text},
        "lane": {"type": "string", "enum": ["code", "ui", "data", "docs", "audit", "cleanup"]},
    }, "required": ["key", "title", "outcome", "acceptance", "after", "lane"]}
    schema = {"type": "object", "additionalProperties": False, "properties": {
        "action": {"type": "string", "enum": ["plan", "finish"]}, "title": text, "outcome": text,
        "tasks": {"type": "array", "items": task},
    }, "required": ["action", "title", "outcome", "tasks"]}
    schema_file = request_file.with_name("schema.json")
    schema_file.write_text(json.dumps(schema), encoding="utf-8")
    prompt = (
        "You manage a benchmark through an agent coordinator. Plan only; do not edit files or launch workers. "
        "Return a small executable iteration of tasks. The controller launches your plan, waits for implementation "
        "AND independent review, submits the completed source, then calls you again with acceptance and task evidence. "
        "Do not add a generic review task: the coordinator already independently reviews every task. "
        "First deliver a minimal working solution with harness.json, "
        "then improve it in later iterations. Inspect the workspace if useful. No score is reported during development. "
        "Every task has one concern, a title of at most 12 words, at most 3 measurable acceptance criteria, "
        "and fewer than 110 words total across title, outcome and acceptance. Do not put task IDs, hashes or AGENTS.md "
        "in prose. Use unique key values and after keys for ordering. Workers automatically read .harness/benchmark.txt. "
        "Do not request git init, cloning, commits, identity changes, attribution, deployment or external messaging. "
        "Only use Codex or local models if a task itself needs a model. Choose finish only when no useful improvement "
        "remains; return empty tasks then. Otherwise choose plan with at least one task.\n" + json.dumps(request)
    )
    command = [os.environ.get("CODEX_BIN", "codex"), "exec", "--skip-git-repo-check", "--ephemeral", "--json",
               "--sandbox", "read-only", "--output-schema", str(schema_file),
               "--output-last-message", os.environ["HARNESS_PLANNER_RESULT"], "-"]
    extra = json.loads(os.environ.get("HARNESS_CODEX_ARGS", "[]"))
    command[2:2] = extra
    env = dict(os.environ)
    env.pop("CODEX_WINDOWS_REGISTERED_CORE", None)
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env, cwd=request["workspace"])
    process.stdin.write(prompt.encode())
    process.stdin.close()
    usage = {key: 0 for key in USAGE_FIELDS}
    turns = 0
    for line in process.stdout:
        sys.stdout.buffer.write(line)
        sys.stdout.flush()
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if isinstance(event, dict) and event.get("type") == "turn.completed":
            turns += 1
            for key in USAGE_FIELDS:
                usage[key] += int((event.get("usage") or {}).get(key) or 0)
    code = process.wait()
    if turns:
        usage["total_tokens"] = usage["input_tokens"] + usage["output_tokens"]
        usage.update(turns=turns, source="codex exec --json turn.completed")
        request_file.with_name("usage.json").write_text(json.dumps(usage), encoding="utf-8")
    raise SystemExit(code)


if __name__ == "__main__":
    main()
