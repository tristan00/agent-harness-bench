#!/usr/bin/env python3
"""Minimal tool loop for a local OpenAI-compatible model: prompt on stdin, JSONL transcript on stdout."""
import json
import os
from pathlib import Path
import subprocess
import sys
import urllib.request

SYSTEM = ("You are an autonomous agent. Reply with exactly one JSON object per turn. To use a tool reply "
          '{"tool":NAME,"args":{...}}. Tools: read_file {path}; list_files {path}; write_file {path, content}; '
          "run_command {argv: [program, args...], timeout_seconds}. Relative paths resolve against the working directory. "
          'When your work is complete reply {"final": SUMMARY}.')
LIMIT = 12000


def clip(text):
    return text if len(text) <= LIMIT else text[:LIMIT] + f"\n[truncated {len(text) - LIMIT} characters]"


def run_tool(call, cwd):
    args = call.get("args") or {}
    path = (Path(cwd) / str(args.get("path", "."))).resolve()
    try:
        name = call.get("tool")
        if name == "read_file":
            return clip(path.read_text(encoding="utf-8", errors="replace"))
        if name == "list_files":
            return "\n".join(item.name + ("/" if item.is_dir() else "") for item in sorted(path.iterdir()))
        if name == "write_file":
            if not path.is_relative_to(Path(cwd).resolve()):
                return "error: path is outside the working directory"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(str(args.get("content", "")), encoding="utf-8")
            return "written"
        if name == "run_command":
            argv = args.get("argv")
            if not isinstance(argv, list) or not argv or not all(isinstance(a, str) for a in argv):
                return "error: argv must be a nonempty string array"
            done = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, errors="replace",
                                  timeout=float(args.get("timeout_seconds") or 120), stdin=subprocess.DEVNULL)
            return clip(f"exit {done.returncode}\n{done.stdout}{done.stderr}")
        return f"error: unknown tool {name}"
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        return f"error: {error}"


def parse(content):
    start, end = content.find("{"), content.rfind("}")
    if start < 0 or end <= start:
        return None
    try:
        value = json.loads(content[start:end + 1])
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def chat(endpoint, body):
    request = urllib.request.Request(endpoint.rstrip("/") + "/chat/completions", json.dumps(body).encode(),
                                     {"Content-Type": "application/json"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=900) as response:
        return json.load(response)


def main(endpoint, model, prompt, cwd, steps, emit, send=chat, execute=run_tool):
    messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": f"{prompt}\nWorking directory: {cwd}"}]
    usage = {"input_tokens": 0, "output_tokens": 0, "turns": 0}
    for _ in range(steps):
        reply = send(endpoint, {"model": model, "messages": messages, "temperature": 0.2, "response_format": {"type": "json_object"}})
        content = reply["choices"][0]["message"].get("content") or ""
        usage["input_tokens"] += int((reply.get("usage") or {}).get("prompt_tokens") or 0)
        usage["output_tokens"] += int((reply.get("usage") or {}).get("completion_tokens") or 0)
        usage["turns"] += 1
        emit({"type": "turn", "content": content, "usage": reply.get("usage")})
        messages.append({"role": "assistant", "content": content})
        call = parse(content)
        if call and "final" in call:
            emit({"type": "result", "final": call["final"], "usage": usage})
            return 0
        observation = execute(call, cwd) if call else "error: reply with one JSON object"
        messages.append({"role": "user", "content": f"Tool result:\n{observation}"})
    emit({"type": "result", "error": f"no final answer within {steps} turns", "usage": usage})
    return 1


if __name__ == "__main__":
    def emit(event):
        sys.stdout.write(json.dumps(event) + "\n")
        sys.stdout.flush()
    raise SystemExit(main(sys.argv[1], sys.argv[2], sys.stdin.buffer.read().decode("utf-8"), os.getcwd(),
                          int(os.environ.get("HARNESS_LOCAL_STEPS", "200")), emit))
