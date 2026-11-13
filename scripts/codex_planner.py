#!/usr/bin/env python3
"""Optional Codex implementation of the replaceable JSON planner contract."""
import json
import os
from pathlib import Path
import subprocess
import sys
from agent_selection import command_arguments, workspace_settings
from workspace_prompts import documents

USAGE_FIELDS = ("input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens")


def main():
    request_file = Path(os.environ["HARNESS_PLANNER_REQUEST"])
    request = json.loads(request_file.read_text(encoding="utf-8"))
    settings = workspace_settings(request["workspace_settings"])
    instructions = documents(request["workspace"], settings)["PROJECT_PLANNER.md"].decode("utf-8")
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
        instructions + "\nReturn an iteration using the supplied JSON schema. The controller executes tasks, waits for implementation and review, "
        "then submits completed source. Use unique key values and after keys. Choose plan with tasks or finish with empty tasks.\n" + json.dumps(request)
    )
    command = [os.environ.get("CODEX_BIN", "codex"), "exec", "--skip-git-repo-check", "--ephemeral", "--json",
               "--sandbox", "read-only", "--output-schema", str(schema_file),
               "--output-last-message", os.environ["HARNESS_PLANNER_RESULT"], "-"]
    command[2:2] = command_arguments(settings["planner"])
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
