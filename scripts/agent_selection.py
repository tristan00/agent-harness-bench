import json
import os
import shutil
from pathlib import Path

CATALOG = json.loads((Path(__file__).resolve().parents[1] / "agents.json").read_text(encoding="utf-8"))
PAIRS = {("openai", "codex"), ("anthropic", "claude_code"), ("local", "openai_compatible")}
VENDOR = {"anthropic": "claude", "openai": "codex"}
CODEX_MARKERS = ["CODEX_WINDOWS_REGISTERED_CORE", "CODEX_THREAD_ID", "CODEX_SANDBOX", "CODEX_SANDBOX_NETWORK_DISABLED",
                 "CODEX_MANAGED_BY_NPM", "CODEX_MANAGED_BY_BUN"]
WORKSPACE_CANDIDATES = {name: (agent["model"], agent["effort"]) for name, agent in CATALOG.items()
                        if agent["provider"] == "openai" and not agent.get("retired")}


def detect_family(environ=None):
    environ = os.environ if environ is None else environ
    explicit = (environ.get("AGENT_FAMILY") or "").strip().lower()
    if explicit:
        return explicit if explicit in {"claude", "codex"} else None
    claude = environ.get("CLAUDECODE") == "1" or bool(environ.get("CLAUDE_CODE_ENTRYPOINT"))
    codex = any(environ.get(name) for name in CODEX_MARKERS)
    return "claude" if claude and not codex else "codex" if codex and not claude else None


def allowed(agent, family):
    return agent["provider"] == "local" or VENDOR.get(agent["provider"]) == family


def resolve(selection, family=None, defined=None, retired=False):
    family = detect_family() if family is None else family
    agent = (defined or {}).get(selection) or CATALOG.get(selection)
    if not agent:
        raise ValueError(f"Unknown agent {selection}")
    if (agent["provider"], agent["harness"]) not in PAIRS or not agent.get("model", "").strip():
        raise ValueError(f"Agent {selection} must run in its own vendor's harness")
    if agent["provider"] == "local" and not str(agent.get("endpoint", "")).startswith(("http://127.0.0.1:", "http://localhost:", "http://[::1]:")):
        raise ValueError(f"Local agent {selection} needs a loopback endpoint")
    if agent.get("retired") and not retired:
        raise ValueError(f"Agent {selection} is retired")
    if not allowed(agent, family):
        raise ValueError(f"Agent {selection} is not allowed when started by {family or 'an undetected launcher'}; "
                         f"only {family or 'no hosted'} and local agents are enabled")
    return dict(agent)


def model_effort(selection):
    agent = resolve(selection, retired=True)
    return agent["model"], agent.get("effort", "")


def workspace_agent(selection):
    resolve(selection)
    return selection


def workspace_settings(value):
    for role in ("planner", "worker"):
        resolve(value.get(role), defined=value.get("agents"))
    if resolve(value.get("planner"), defined=value.get("agents")).get("workerOnly"):
        raise ValueError(f'Agent {value.get("planner")} can only be a worker')
    listed = value.get("enabled_agents")
    if listed is not None and (not isinstance(listed, list) or not all(isinstance(name, str) for name in listed)):
        raise ValueError("enabled_agents must be a list of agent ids")
    for name in listed or []:
        resolve(name, defined=value.get("agents"))
    return value


def enabled_agents(settings, family=None):
    family = detect_family() if family is None else family
    listed = settings.get("enabled_agents")
    if listed is None:
        listed = [name for name, agent in CATALOG.items() if not agent.get("retired") and allowed(agent, family)]
    return list(dict.fromkeys([*listed, settings["planner"], settings["worker"]]))


def agent_definitions(settings, family=None):
    return {name: resolve(name, family, settings.get("agents")) for name in enabled_agents(settings, family)}


def coordinator_settings(settings, family=None):
    registered = {key: value for key, value in settings.items() if key != "enabled_agents"}
    registered["agents"] = agent_definitions(settings, family)
    return registered


def command_arguments(selection, retired=False):
    agent = resolve(selection, retired=retired)
    if agent["harness"] != "codex":
        raise ValueError(f"Agent {selection} does not run in Codex")
    return ["--model", agent["model"], "-c", f'model_reasoning_effort="{agent["effort"]}"']


def hosted_claude_variable(name):
    return name in {"CLAUDECODE", "CLAUDE_PID", "CLAUDE_EFFORT"} or name.startswith(("CLAUDE_CODE_", "CLAUDE_AGENT_SDK", "CLAUDE_PREVIEW"))


def claude_environment(env):
    return {key: value for key, value in env.items() if not hosted_claude_variable(key)}


def claude_executable(found=None):
    found = found or shutil.which("claude")
    if found and Path(found).suffix.lower() == ".cmd":
        native = Path(found).parent / "node_modules" / "@anthropic-ai" / "claude-code" / "bin" / "claude.exe"
        if native.is_file():
            return str(native)
    return found or "claude"


def claude_arguments(agent, read_only=False):
    argv = [os.environ.get("CLAUDE_BIN") or claude_executable(), "-p", "--model", agent["model"], "--output-format", "stream-json", "--verbose",
            "--no-session-persistence", "--dangerously-skip-permissions"]
    if agent.get("effort"):
        argv += ["--effort", agent["effort"]]
    if read_only:
        argv += ["--disallowedTools", "Edit", "Write", "NotebookEdit"]
    return argv


def expand_command(command, selection):
    model, effort = model_effort(selection)
    if not isinstance(command, list) or not all(isinstance(arg, str) for arg in command):
        raise ValueError("Agent command must be an argv array")
    if "{model}" not in command or not any("{effort}" in arg for arg in command):
        raise ValueError("Agent command must explicitly include {model} and {effort} placeholders")
    return [arg.replace("{model}", model).replace("{effort}", effort) for arg in command]


def planner_command(command, selection):
    workspace_agent(selection)
    adapters = {Path(__file__).with_name(name).resolve() for name in ("template_planner.py", "codex_planner.py")}
    if len(command) == 2 and Path(command[1]).resolve() in adapters:
        return command
    return expand_command(command, selection)
