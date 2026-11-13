#!/usr/bin/env python3
"""Run the planner in the selected agent's own harness: fresh from the filled template, or resumed with a message."""
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid
from agent_selection import claude_arguments, claude_environment, command_arguments, resolve


def codex_command(selection, persistent, session=None):
    executable = os.environ.get("CODEX_BIN", "codex")
    if session:
        return [executable, "exec", "resume", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox",
                *command_arguments(selection), session, "-"]
    command = [executable, "exec", "--skip-git-repo-check", *([] if persistent else ["--ephemeral"]),
               "--json", "--sandbox", "danger-full-access", "-c", 'approval_policy="never"']
    return [*command, *command_arguments(selection), "-"]


def claude_command(agent, persistent, session=None):
    command = [arg for arg in claude_arguments(agent) if not (persistent and arg == "--no-session-persistence")]
    if persistent:
        command += ["--resume", session] if session else []
    return command


def plan(selection, agent, persistent, saved, message):
    """Return (argv, session record to save before running, whether to read the codex thread id from output)."""
    session = saved.get("session") if message is not None else None
    if agent["harness"] == "codex":
        return codex_command(selection, persistent, session), saved, persistent and session is None
    if agent["harness"] == "claude_code":
        if persistent and session is None:
            saved = {"harness": "claude_code", "session": str(uuid.uuid4())}
            return [*claude_command(agent, True), "--session-id", saved["session"]], saved, False
        return claude_command(agent, persistent, session), saved, False
    command = [sys.executable, str(Path(__file__).with_name("local_agent.py")), agent["endpoint"], agent["model"]]
    if persistent:
        saved = {"harness": "openai_compatible", "session": saved.get("session") or os.environ["HARNESS_PLANNER_SESSION"] + ".messages.json"}
        command += ["--session", saved["session"], *(["--resume"] if message is not None else [])]
    return command, saved, False


def main():
    selection = os.environ["HARNESS_WORKSPACE_PLANNER"]
    agent = resolve(selection)
    env = dict(os.environ)
    env.pop("CODEX_WINDOWS_REGISTERED_CORE", None)
    prompt = Path(env.pop("HARNESS_PLANNER_PROMPT")).read_bytes()
    session_file = env.pop("HARNESS_PLANNER_SESSION", None)
    message_file = env.pop("HARNESS_PLANNER_MESSAGE", None)
    saved = json.loads(Path(session_file).read_text(encoding="utf-8")) if session_file and Path(session_file).exists() else {}
    message = Path(message_file).read_bytes() if message_file else None
    command, saved, capture = plan(selection, agent, bool(session_file), saved, message)
    if saved and session_file:
        Path(session_file).write_text(json.dumps(saved), encoding="utf-8")
    if agent["harness"] == "claude_code":
        env = claude_environment(env)
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE if capture else None, env=env)
    process.stdin.write(message if message is not None else prompt)
    process.stdin.close()
    if capture:
        for line in process.stdout:
            sys.stdout.buffer.write(line)
            sys.stdout.flush()
            if not saved.get("session"):
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if isinstance(event, dict) and event.get("type") == "thread.started" and event.get("thread_id"):
                    saved = {"harness": "codex", "session": event["thread_id"]}
                    Path(session_file).write_text(json.dumps(saved), encoding="utf-8")
    raise SystemExit(process.wait())


if __name__ == "__main__":
    main()
