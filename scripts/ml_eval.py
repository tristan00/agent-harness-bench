"""Hidden-test evaluation for ml_predictor submissions. No model calls."""
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
import urllib.request

from harness import spawn_agent, stop_agent

ROOT = Path(__file__).resolve().parents[1]


def registry():
    return {entry["id"]: entry for entry in json.loads((ROOT / "ml_datasets.json").read_text(encoding="utf-8"))}


def expected_hashes(entry):
    hashes = entry["reference"]["test_sha256"]
    if isinstance(hashes, str):
        return {Path(entry["test"][0]).name: hashes}
    return hashes


def fetch_test(entry, cache):
    cache.mkdir(parents=True, exist_ok=True)
    files = {}
    for url in entry["test"]:
        path = cache / Path(url).name
        if not path.exists():
            request = urllib.request.Request(url, headers={"User-Agent": "agent-harness-bench"})
            with urllib.request.urlopen(request, timeout=120) as response:
                path.write_bytes(response.read())
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if digest != expected_hashes(entry)[path.name]:
            raise ValueError(f"Test file {path.name} does not match its recorded checksum")
        files[path.name] = path
    return files


def idx(data):
    dims = data[3]
    shape = [int.from_bytes(data[4 + 4 * i:8 + 4 * i], "big") for i in range(dims)]
    return shape, data[4 + 4 * dims:]


def test_split(task_id, files, directory):
    directory.mkdir(parents=True, exist_ok=True)
    features = directory / "input"
    if task_id == "adult_income":
        rows = [line.strip() for line in files["adult.test"].read_text(encoding="utf-8").splitlines()[1:] if line.strip()]
        parts = [row.rsplit(",", 1) for row in rows]
        features.write_text("\n".join(head for head, _ in parts) + "\n", encoding="utf-8")
        return features, [1 if label.strip().rstrip(".") == ">50K" else 0 for _, label in parts]
    if task_id == "optical_digits":
        parts = [line.strip().rsplit(",", 1) for line in files["optdigits.tes"].read_text(encoding="utf-8").splitlines() if line.strip()]
        features.write_text("\n".join(head for head, _ in parts) + "\n", encoding="utf-8")
        return features, [int(label) for _, label in parts]
    if task_id == "fashion_mnist":
        features = directory / "t10k-images-idx3-ubyte.gz"
        features.write_bytes(files["t10k-images-idx3-ubyte.gz"].read_bytes())
        _, labels = idx(gzip.decompress(files["t10k-labels-idx1-ubyte.gz"].read_bytes()))
        return features, list(labels)
    raise ValueError(f"Unknown ML task {task_id}")


def roc_auc(labels, scores):
    order = sorted(range(len(scores)), key=lambda i: scores[i])
    ranks = [0.0] * len(scores)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and scores[order[j + 1]] == scores[order[i]]:
            j += 1
        for k in range(i, j + 1):
            ranks[order[k]] = (i + j) / 2 + 1
        i = j + 1
    positives = sum(labels)
    negatives = len(labels) - positives
    if not positives or not negatives:
        raise ValueError("ROC AUC needs both classes")
    return (sum(rank for rank, label in zip(ranks, labels) if label) - positives * (positives + 1) / 2) / (positives * negatives)


def metric(name, labels, lines):
    if len(lines) != len(labels):
        raise ValueError(f"Expected {len(labels)} predictions, got {len(lines)}")
    if name == "roc_auc":
        return roc_auc(labels, [float(line) for line in lines])
    if name == "accuracy":
        return sum(int(float(line)) == label for line, label in zip(lines, labels)) / len(labels)
    raise ValueError(f"Unknown metric {name}")


def normalized(score, reference, trivial):
    if score is None or score <= trivial:
        return 0.0
    if score <= reference:
        return 50 * (score - trivial) / (reference - trivial)
    return min(100.0, 50 + 50 * (score - reference) / (1 - reference))


def materialize(submission, directory):
    directory.mkdir(parents=True, exist_ok=True)
    for file in submission["files"]:
        path = directory / file["path"]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(file["text"].encode("utf-8") if file.get("text") is not None else base64.b64decode(file["base64"]))
    return submission["manifest"]


def evaluate(run, config, data, output):
    assignment = run["ml"]
    entry = registry()[assignment["task"]["id"]]
    reference, trivial = entry["reference"]["score"], entry["reference"]["trivial"]
    result = {"task": entry["id"], "metric": entry["metric"], "reference": reference, "trivial": trivial,
              "reference_model": entry["reference"]["model"], "score": None, "error": None}
    directory = output / "ml" / run["id"]
    try:
        submission = json.loads((data / "submissions" / run["submission_id"] / "submission.json").read_text(encoding="utf-8"))
        features, labels = test_split(entry["id"], fetch_test(entry, data / "ml-data" / entry["id"]), directory / "hidden")
        manifest = materialize(submission, directory / "submission")
        if not manifest.get("predict"):
            raise ValueError("Manifest has no predict command")
        predictions = directory / "predictions.txt"
        argv = [arg.replace("{input}", str(features)).replace("{output}", str(predictions)) for arg in manifest["predict"]]
        python = Path(config["ml_python"])
        env = {**os.environ, **manifest.get("env", {}), "PATH": str(python.parent) + os.pathsep + os.environ.get("PATH", "")}
        env = {key: value for key, value in env.items() if not key.startswith("HARNESS_")}
        argv[0] = shutil.which(argv[0], path=env["PATH"]) or str((directory / "submission" / argv[0]).resolve())
        started = time.monotonic()
        with (directory / "predict.log").open("wb") as log:
            process = spawn_agent(argv, directory / "submission", env, log)
            try:
                process.wait(timeout=config["predict_seconds"])
            except subprocess.TimeoutExpired:
                raise TimeoutError(f"predict exceeded {config['predict_seconds']} seconds")
            finally:
                stop_agent(process)
        result["predict_seconds"] = round(time.monotonic() - started, 3)
        if process.returncode:
            raise RuntimeError(f"predict exited with {process.returncode}")
        lines = [line.strip() for line in predictions.read_text(encoding="utf-8").splitlines() if line.strip()]
        result["rows"] = len(labels)
        result["score"] = metric(entry["metric"], labels, lines)
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
    result["normalized"] = normalized(result["score"], reference, trivial)
    return result
