import hashlib


def selected_documents(path, settings=None):
    if settings is None:
        from agent_selection import planner_candidates
        candidate = next(iter(planner_candidates()))
        settings = {"planner": candidate, "worker": candidate}
    settings = dict(settings)
    settings["prompts"] = {}
    for role, filename in [("project_planner", "PROJECT_PLANNER.md"), ("worker_protocol", "WORKER_PROTOCOL.md")]:
        content = f"Selected {role} fixture for {path.name}.".encode()
        (path / filename).write_bytes(content)
        settings["prompts"][role] = {"candidate":"fixture.md", "sha256":hashlib.sha256(content).hexdigest()}
    return settings
