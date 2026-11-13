import hashlib
from pathlib import Path

DOCUMENTS = {"project_planner": "PROJECT_PLANNER.md", "worker_protocol": "WORKER_PROTOCOL.md"}


def documents(directory, settings):
    result = {}
    for role, filename in DOCUMENTS.items():
        selected = settings.get("prompts", {}).get(role, {})
        candidate = selected.get("candidate", "")
        if not candidate or candidate in {".", ".."} or any(char in candidate for char in "/\\:"):
            raise ValueError("Workspace prompt candidate is required")
        path = Path(directory) / filename
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"Workspace prompt must be a regular file: {path}")
        content = path.read_bytes()
        if not 1 <= len(content.decode("utf-8").split()) <= 200:
            raise ValueError(f"Workspace prompt must contain 1..200 words: {path}")
        if hashlib.sha256(content).hexdigest() != selected.get("sha256"):
            raise ValueError(f"Workspace prompt changed: {path}")
        result[filename] = content
    return result


def copy_documents(source, destination, settings):
    contents = documents(source, settings)
    for filename, content in contents.items():
        with (Path(destination) / filename).open("xb") as file:
            file.write(content)
