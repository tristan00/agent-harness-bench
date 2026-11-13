#!/usr/bin/env python3
"""Pass the filled template unchanged to a fresh authenticated Codex process."""
import os
from pathlib import Path
import subprocess
from agent_selection import command_arguments, workspace_agent


def main():
    command = [os.environ.get("CODEX_BIN", "codex"), "exec", "--skip-git-repo-check", "--ephemeral",
               "--json", "--sandbox", "danger-full-access", "-c", 'approval_policy="never"']
    command.extend(command_arguments(workspace_agent(os.environ["HARNESS_WORKSPACE_PLANNER"])))
    command.append("-")
    env = dict(os.environ)
    env.pop("CODEX_WINDOWS_REGISTERED_CORE", None)
    prompt = Path(env.pop("HARNESS_PLANNER_PROMPT")).read_bytes()
    result = subprocess.run(command, input=prompt, env=env)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
