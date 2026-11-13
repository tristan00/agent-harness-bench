WORKSPACE_CANDIDATES = {
    "luna_6_high": ("gpt-6-luna", "high"),
    "sol_6_1_light": ("gpt-6.1-sol", "low"),
    "sol_6_1_medium": ("gpt-6.1-sol", "medium"),
}


def model_effort(selection):
    if selection == "sol_light":
        return "gpt-6-sol", "low"
    if selection not in WORKSPACE_CANDIDATES:
        raise ValueError("Explicit supported agent selection is required")
    return WORKSPACE_CANDIDATES[selection]


def workspace_agent(selection):
    if selection not in WORKSPACE_CANDIDATES:
        raise ValueError("Workspace agent must be luna_6_high, sol_6_1_light or sol_6_1_medium")
    return selection


def workspace_settings(value):
    for role in ("planner", "worker"):
        workspace_agent(value.get(role))
    return value


def command_arguments(selection):
    model, effort = model_effort(selection)
    return ["--model", model, "-c", f'model_reasoning_effort="{effort}"']


def expand_command(command, selection):
    workspace_agent(selection)
    model, effort = model_effort(selection)
    if not isinstance(command, list) or not all(isinstance(arg, str) for arg in command):
        raise ValueError("Agent command must be an argv array")
    if "{model}" not in command or not any("{effort}" in arg for arg in command):
        raise ValueError("Agent command must explicitly include {model} and {effort} placeholders")
    return [arg.replace("{model}", model).replace("{effort}", effort) for arg in command]


def planner_command(command, selection):
    from pathlib import Path
    workspace_agent(selection)
    adapters = {Path(__file__).with_name(name).resolve() for name in ("template_planner.py", "codex_planner.py")}
    if len(command) == 2 and Path(command[1]).resolve() in adapters:
        return command
    return expand_command(command, selection)
