#!/usr/bin/env python3
"""Pass the filled template unchanged to a fresh planner in the selected agent's own harness."""
import os
from pathlib import Path
import subprocess
import sys
from agent_selection import claude_arguments, claude_environment, command_arguments, resolve


def main():
    selection = os.environ["HARNESS_WORKSPACE_PLANNER"]
    agent = resolve(selection)
    env = dict(os.environ)
    env.pop("CODEX_WINDOWS_REGISTERED_CORE", None)
    prompt = Path(env.pop("HARNESS_PLANNER_PROMPT")).read_bytes()
    if agent["harness"] == "codex":
        command = [os.environ.get("CODEX_BIN", "codex"), "exec", "--skip-git-repo-check", "--ephemeral",
                   "--json", "--sandbox", "danger-full-access", "-c", 'approval_policy="never"']
        command.extend(command_arguments(selection))
        command.append("-")
    elif agent["harness"] == "claude_code":
        command = claude_arguments(agent)
        env = claude_environment(env)
    else:
        command = [sys.executable, str(Path(__file__).with_name("local_agent.py")), agent["endpoint"], agent["model"]]
    result = subprocess.run(command, input=prompt, env=env)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
