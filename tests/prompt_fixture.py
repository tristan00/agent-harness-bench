import hashlib


def selected_documents(path, settings=None):
    settings = dict(settings or {"planner":"sol_6_1_light", "worker":"sol_6_1_light"})
    settings["prompts"] = {}
    for role, filename in [("project_planner", "PROJECT_PLANNER.md"), ("worker_protocol", "WORKER_PROTOCOL.md")]:
        content = f"Selected {role} fixture for {path.name}.".encode()
        (path / filename).write_bytes(content)
        settings["prompts"][role] = {"candidate":"fixture.md", "sha256":hashlib.sha256(content).hexdigest()}
    return settings
