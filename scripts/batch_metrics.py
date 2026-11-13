"""Deterministic metrics and scoring. No model calls."""
import base64
import hashlib
import json
from collections import Counter, defaultdict
from pathlib import Path


def read(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def path_key(path):
    return str(path).removeprefix("\\\\?\\").replace("\\", "/").rstrip("/").casefold()


def inside(path, root):
    return path_key(path) == path_key(root) or path_key(path).startswith(path_key(root) + "/")


def event_usage(path):
    usage = Counter()
    turns = 0
    if Path(path).exists():
        for line in Path(path).open(encoding="utf-8", errors="replace"):
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") == "turn.completed" and isinstance(event.get("usage"), dict):
                turns += 1
                usage.update({key: value for key, value in event["usage"].items() if isinstance(value, int)})
    return dict(usage), turns


def measured_tokens(attempt, board_root):
    usage = (attempt.get("evidence") or {}).get("usage") or {}
    total = usage.get("totalTokens", usage.get("total_tokens"))
    if total is not None:
        return total
    directory = board_root / "attempts" / attempt["id"]
    fields, turns = event_usage(directory / "events.jsonl")
    if turns:
        return fields.get("input_tokens", 0) + fields.get("output_tokens", 0)
    file = directory / "usage.json"
    if file.exists():
        data = read(file)
        return data.get("total_tokens", data.get("totalTokens"))
    return None


def parallel_metrics(intervals):
    events = defaultdict(list)
    total_seconds = 0.0
    for task, start, end in intervals:
        if end <= start:
            continue
        events[start].append((task, 1))
        events[end].append((task, -1))
        total_seconds += (end - start) / 1000
    active = Counter()
    previous = None
    busy = overlap = task_seconds = 0.0
    peak = 0
    for at, changes in sorted(events.items()):
        if previous is not None:
            seconds = (at - previous) / 1000
            count = sum(value > 0 for value in active.values())
            busy += seconds if count else 0
            overlap += seconds if count >= 2 else 0
            task_seconds += seconds * count
        for task, delta in changes:
            active[task] += delta
        peak = max(peak, sum(value > 0 for value in active.values()))
        previous = at
    return {"worker_agent_seconds": total_seconds, "distinct_task_seconds": task_seconds, "busy_seconds": busy, "parallel_seconds": overlap,
            "peak_parallel_tasks": peak, "effective_parallelism": task_seconds / busy if busy else 0,
            "parallel_fraction": overlap / busy if busy else 0}


def supersedes(task):
    targets = set(task.get("supersedes", []))
    for event in task.get("history", []):
        if event.get("kind") == "created":
            targets.update((event.get("evidence") or {}).get("supersedes", []))
    return sorted(targets)


def graph_metrics(tasks):
    ids = {task["id"] for task in tasks}
    edges = set()
    external = 0
    for task in tasks:
        source = task["id"]
        candidates = [("parent", task.get("parent"))]
        candidates.extend(("supersedes", target) for target in supersedes(task))
        for relation in ["after", "conflicts", "related"]:
            candidates.extend((relation, target) for target in task.get(relation, []))
        candidates.extend(("dependency:" + str(link.get("reason") or "unclassified"), link.get("target"))
                          for link in task.get("dependencies", []) if isinstance(link, dict))
        for relation, target in candidates:
            if not target or target == source:
                continue
            if target not in ids:
                external += 1
                continue
            pair = sorted((source, target)) if relation in {"related", "conflicts"} else (source, target)
            edges.add((pair[0], pair[1], relation))
    neighbors = {task: set() for task in ids}
    for source, target, _ in edges:
        neighbors[source].add(target)
        neighbors[target].add(source)
    degrees = [len(value) for value in neighbors.values()]
    types = Counter(relation for _, _, relation in edges)
    return {"task_count": len(tasks), "runnable_tasks": sum(bool(t.get("acceptance")) for t in tasks),
            "containers": sum(not t.get("acceptance") for t in tasks), "edge_count": len(edges),
            "relation_types": dict(sorted(types.items())), "distinct_relation_types": len(types),
            "dependency_edges": sum(count for relation, count in types.items() if relation.startswith("dependency:")),
            "orphans": degrees.count(0), "near_orphans": degrees.count(1),
            "connectivity": sum(min(degree / 2, 1) for degree in degrees) / len(tasks) if tasks else 0,
            "external_edges_excluded": external, "edges": sorted(edges),
            "connectivity_by_task": {key:{"neighbors":sorted(value),"degree":len(value),"contribution":min(len(value)/2, 1)} for key,value in sorted(neighbors.items())},
            "classification": classification_metrics(tasks, edges)}


TRACKED_ATTRIBUTES = ("languages", "functions", "priority")


def attribute_missing(task, name):
    value = (task.get("attributes") or {}).get(name)
    return value is None or value == [] or value == "" or value == {}


def classification_metrics(tasks, edges):
    missing = {name: sorted(task["id"] for task in tasks if attribute_missing(task, name)) for name in TRACKED_ATTRIBUTES}
    dependencies = sorted(edge for edge in edges if edge[2].startswith("dependency:"))
    unclassified = [edge for edge in dependencies if edge[2] == "dependency:unclassified"]
    n, d = len(tasks), len(dependencies)
    return {"task_count":n, "missing_by_attribute":missing, "filled_by_attribute":{name: n - len(ids) for name, ids in missing.items()},
            "dependency_count":d, "unclassified_edges":unclassified, "unclassified_dependency_share":len(unclassified)/d if d else 0,
            "classified_dependency_share":(d-len(unclassified))/d if d else 1}


def attribute_fill(classification, attributes):
    filled = sum(classification["filled_by_attribute"][name] for name in attributes)
    slots = classification["task_count"] * len(attributes)
    return {"attributes":list(attributes), "filled_slots":filled, "total_slots":slots,
            "filled_by_attribute":{name: classification["filled_by_attribute"][name] for name in attributes},
            "missing_by_attribute":{name: classification["missing_by_attribute"][name] for name in attributes},
            "attribute_fill_share":filled / slots if slots else 0}


def source_line_details(benchmark_root, artifact):
    if not artifact:
        return {}
    submission = read(benchmark_root / "submissions" / artifact["id"] / "submission.json")
    source_files = set(artifact["source_files"])
    lines = {}
    for file in submission["files"]:
        if file["path"] in source_files:
            text = file.get("text")
            if text is None:
                text = base64.b64decode(file["base64"]).decode("utf-8")
            lines[file["path"]] = sum(bool(line.strip()) for line in text.split("\n"))
    if sum(lines.values()) != artifact["lines_of_code"]:
        raise ValueError("Frozen source line breakdown disagrees with the accepted artifact")
    return dict(sorted(lines.items()))


def collect(case, run, benchmark_root, shared_board):
    if run is None:
        return {"run_id":None, "workspace":case["workspace"], "benchmark":case["benchmark"],
                "settings_at_run":case["settings"], "dataset":None, "artifact":None, "source_lines":None, "source_lines_by_file":{},
                "graph":graph_metrics([]), "parallel":parallel_metrics([]),
                "tokens":{"recorded_total":0, "measured_launches":0, "launches":0, "complete":True,
                          "planner_usage":{}, "proxy_agent_seconds":0, "development_seconds":0},
                "evidence":{"error":case.get("error"), "tasks":[]}}
    private = benchmark_root / "coordinator" / run["id"] / "board"
    board_root = private if (private / "state.json").exists() else shared_board
    state_path = board_root / "state.json"
    state = read(state_path)
    tasks = {task["id"]: task for task in state.get("archived", []) + state.get("tasks", [])
             if inside(task.get("cwd", ""), case["directory"])}
    intervals = []
    launches = []
    records = []
    for task in tasks.values():
        attempts = []
        for attempt in task.get("attempts", []):
            start = max(attempt["started"], run["started_at_ms"])
            end = min(attempt.get("ended") or run["deadline_ms"], run["deadline_ms"])
            intervals.append((task["id"], start, end))
            total = measured_tokens(attempt, board_root)
            item = {"id": attempt["id"], "task_id": task["id"], "stage": attempt["stage"],
                    "start_ms": start, "end_ms": end, "tokens": total,
                    "observed": (attempt.get("evidence") or {}).get("observed")}
            attempts.append(item)
            launches.append(item)
        records.append({**{key: task.get(key) for key in ["id", "title", "cwd", "parent", "after", "dependencies", "conflicts", "related", "acceptance", "stage", "attributes"]}, "supersedes":supersedes(task), "attempts": attempts})
    progress_root = benchmark_root / "coordinator" / run["id"]
    fields, turns = event_usage(progress_root / "planner.log")
    planner_tokens = fields.get("input_tokens", 0) + fields.get("output_tokens", 0) if turns else None
    planner_seconds = (run["deadline_ms"] - run["started_at_ms"]) / 1000
    log = progress_root / "planner.log"
    progress = progress_root / "progress.json"
    if progress.exists() and read(progress).get("phase") == "planner_exited":
        planner_seconds = max(0, min(planner_seconds, log.stat().st_mtime - run["started_at_ms"] / 1000))
    launches.append({"stage":"planner", "tokens":planner_tokens})
    parallel = parallel_metrics(intervals)
    known = [item["tokens"] for item in launches if item["tokens"] is not None]
    artifact = read(benchmark_root / "submissions" / run["submission_id"] / "artifact.json") if run.get("submission_id") else None
    return {"run_id": run["id"], "workspace": case["workspace"], "benchmark": case["benchmark"],
            "settings_at_run": case["settings"], "dataset": run.get("dataset"), "artifact": artifact,
            "source_lines": artifact["lines_of_code"] if artifact else None, "source_lines_by_file":source_line_details(benchmark_root, artifact),
            "graph": graph_metrics(list(tasks.values())), "parallel": parallel,
            "tokens": {"recorded_total": sum(known), "measured_launches": len(known), "launches": len(launches),
                       "complete": len(known) == len(launches), "planner_usage": fields,
                       "proxy_agent_seconds": parallel["worker_agent_seconds"] + planner_seconds, "planner_seconds":planner_seconds,
                       "development_seconds": (run["deadline_ms"] - run["started_at_ms"]) / 1000},
            "evidence": {"state_path":str(state_path), "state_sha256":hashlib.sha256(state_path.read_bytes()).hexdigest(),
                         "planner_log":str(log), "tasks":records}}


def weighted(values, weights):
    return sum(values[key] * weights[key] for key in weights) / sum(weights.values())


def aggregation(values, weights):
    denominator = sum(weights.values())
    terms = {key: {"value": values[key], "weight": weights[key],
                   "weighted_value": values[key] * weights[key],
                   "normalized_weight": weights[key] / denominator,
                   "contribution": values[key] * weights[key] / denominator} for key in weights}
    return {"operation": "weighted_mean", "terms": terms, "weight_sum": denominator,
            "weighted_sum": sum(term["weighted_value"] for term in terms.values()),
            "value": weighted(values, weights)}


def calculation(formula, inputs, value, **details):
    return {"formula": formula, "inputs": inputs, "value": value, **details}


def score(metrics, report, config, token_mode):
    refs = config["references"]
    graph, parallel, tokens = metrics["graph"], metrics["parallel"], metrics["tokens"]
    n = graph["task_count"]
    relation_types = graph["distinct_relation_types"]
    richness = graph["edge_count"] / n if n else 0
    relations = 100 * (graph["connectivity"] + relation_types / (relation_types + refs["relation_types"]) + richness / (richness + refs["edges_per_task"])) / 3
    concurrency = parallel["effective_parallelism"]
    parallelism = 50 * (parallel["parallel_fraction"] + (1 - 1 / concurrency if concurrency else 0))
    maze = metrics["benchmark"] == "maze_game"
    completed = report["progress"]["fraction_sum"] if maze and report else 0
    per_request = max(completed, 1) if maze else 1
    token_cost = (tokens["recorded_total"] / refs["tokens"] if token_mode == "measured_tokens" else tokens["proxy_agent_seconds"] / tokens["development_seconds"]) / per_request if n else 0
    parts = {"task_count":100*n/(n+refs["task_count"]), "relations":relations, "parallelism":parallelism,
             "token_efficiency":100/(1+token_cost) if n else 0}
    classification = graph["classification"]
    fill = attribute_fill(classification, config["classification_attributes"])
    parts["classification"] = 100 * fill["attribute_fill_share"] * classification["classified_dependency_share"]
    artifact = metrics["artifact"]
    if not artifact:
        result = config["missing_submission"]["result"]
        simplicity = config["missing_submission"]["simplicity"]
    else:
        simplicity = 100 / (1 + metrics["source_lines"] / per_request / refs["source_lines"])
        if report is None:
            raise ValueError(f'Missing quality report for {metrics["run_id"]}')
        if metrics["benchmark"] == "chess960":
            delta = report["metrics"]["elo_change"]
            result = 100 / (1 + 10 ** (-delta / refs["elo_scale"]))
        elif maze:
            weights = config["maze"]
            completion = completed / report["progress"]["requests_total"]
            quality = report["mean_out_of_5"] / 5
            result = 100 * (weights["completion_weight"] * completion + weights["quality_weight"] * quality) / (weights["completion_weight"] + weights["quality_weight"])
        else:
            result = 20 * report["mean_out_of_5"]
    categories = {"result":result, "simplicity":simplicity, "framework":weighted(parts, config["framework_weights"])}
    normalization = {"requests_completed": completed, "divisor": per_request} if maze else None
    if not artifact:
        result_trace = calculation("missing_submission.result", config["missing_submission"], result, source="missing_submission_policy")
    elif metrics["benchmark"] == "chess960":
        result_trace = calculation("100 / (1 + 10 ** (-elo_change / elo_scale))",
                                   {"elo_change": delta, "elo_scale": refs["elo_scale"]}, result,
                                   source="deterministic_chess_tournament", tournament_metrics=report["metrics"])
    else:
        criteria = {key: value for key, value in report.get("scores", {}).items()
                    if isinstance(value, dict) and isinstance(value.get("score"), (int, float))}
        ratings = {key: value["score"] for key, value in criteria.items()}
        quality_trace = calculation("mean_out_of_5 * 20", {"mean_out_of_5": report["mean_out_of_5"]}, report["mean_out_of_5"] * 20,
                                    source="independent_judge", criteria=criteria,
                                    mean=aggregation(ratings, dict.fromkeys(ratings, 1)) if ratings else None)
        if maze:
            result_trace = aggregation({"completion": completion * 100, "quality": quality * 100},
                                       {"completion": weights["completion_weight"], "quality": weights["quality_weight"]})
            result_trace["components"] = {"quality": quality_trace,
                "completion": calculation("100 * fraction_sum / requests_total", report["progress"], completion * 100,
                                          requests=report.get("scores", {}).get("requests", []))}
        else:
            result_trace = quality_trace
    efficiency_numerator = tokens["recorded_total"] if token_mode == "measured_tokens" else tokens["proxy_agent_seconds"]
    efficiency_denominator = refs["tokens"] if token_mode == "measured_tokens" else tokens["development_seconds"]
    relation_values = {"connectivity": graph["connectivity"],
                       "type_diversity": relation_types / (relation_types + refs["relation_types"]),
                       "edge_density": richness / (richness + refs["edges_per_task"])}
    trace = {
        "result": result_trace,
        "source_line_count": calculation("sum(nonblank_source_lines_by_file)", metrics.get("source_lines_by_file", {}), metrics["source_lines"]),
        "simplicity": calculation("100 / (1 + source_lines / request_divisor / reference_lines)" if artifact else "missing_submission.simplicity",
            {"source_lines": metrics["source_lines"], "request_divisor": per_request, "reference_lines": refs["source_lines"],
             "normalized_lines": metrics["source_lines"] / per_request if artifact else None,
             "missing_submission_value": config["missing_submission"]["simplicity"]}, simplicity),
        "framework": aggregation(parts, config["framework_weights"]),
        "framework_components": {
            "task_count": calculation("100 * task_count / (task_count + reference_tasks)",
                {"task_count": n, "reference_tasks": refs["task_count"], "containers": graph["containers"], "runnable_tasks": graph["runnable_tasks"]}, parts["task_count"]),
            "relations": calculation("100 * mean(connectivity, type_diversity, edge_density)",
                {"connectivity": graph["connectivity"], "distinct_relation_types": relation_types, "edge_count": graph["edge_count"],
                 "task_count": n, "edges_per_task": richness, "reference_types": refs["relation_types"], "reference_density": refs["edges_per_task"]}, relations,
                components={"connectivity": calculation("mean(min(unique_neighbors / 2, 1))", {"tasks": graph["connectivity_by_task"], "task_count": n}, graph["connectivity"]),
                    "type_diversity": calculation("distinct_types / (distinct_types + reference_types)", {"distinct_types": relation_types, "reference_types": refs["relation_types"]}, relation_values["type_diversity"]),
                    "edge_density": calculation("edges_per_task / (edges_per_task + reference_density)", {"edges_per_task": richness, "reference_density": refs["edges_per_task"]}, relation_values["edge_density"])},
                aggregation=aggregation(relation_values, dict.fromkeys(relation_values, 1))),
            "parallelism": calculation("50 * (parallel_fraction + (1 - 1 / effective_parallelism if effective_parallelism else 0))",
                parallel, parallelism, overlap_term=parallel["parallel_fraction"], concurrency_term=1 - 1 / concurrency if concurrency else 0,
                components={"parallel_fraction":calculation("parallel_seconds / busy_seconds if busy_seconds else 0", {"parallel_seconds":parallel["parallel_seconds"],"busy_seconds":parallel["busy_seconds"]}, parallel["parallel_fraction"]),
                    "effective_parallelism":calculation("distinct_task_seconds / busy_seconds if busy_seconds else 0", {"distinct_task_seconds":parallel["distinct_task_seconds"],"busy_seconds":parallel["busy_seconds"]}, concurrency)}),
            "token_efficiency": calculation("100 / (1 + cost) if task_count else 0",
                {"mode": token_mode, "numerator": efficiency_numerator, "denominator": efficiency_denominator,
                 "request_divisor": per_request, "task_count": n}, parts["token_efficiency"],
                cost=calculation("numerator / denominator / request_divisor if task_count else 0",
                    {"numerator": efficiency_numerator, "denominator": efficiency_denominator, "request_divisor": per_request, "task_count": n}, token_cost))},
        "benchmark": aggregation(categories, config["category_weights"])}
    trace["framework_components"]["classification"] = calculation("100 * attribute_fill_share * classified_dependency_share",
        {**fill, "classified_dependency_share":classification["classified_dependency_share"],
         "dependency_count":classification["dependency_count"], "unclassified_edges":classification["unclassified_edges"],
         "definition":"attribute_fill_share = filled_slots / (task_count * len(attributes)). An attribute is unfilled when it is null or empty. Containers count. No dependency edges means no dependency penalty; no tasks earns zero."},
        parts["classification"], unfilled_penalty_share=1 - fill["attribute_fill_share"],
        unclassified_penalty_share=classification["unclassified_dependency_share"])
    return {**metrics, "quality_report":report, "token_scoring_mode":token_mode, "framework_components":parts, "per_request_normalization":normalization,
            "categories":categories, "score":weighted(categories, config["category_weights"]), "calculation": trace,
            "status":"scored" if artifact else "no_submission"}


def aggregate(cases, config):
    grouped = defaultdict(dict)
    for case in cases:
        if case["benchmark"] in grouped[case["workspace"]]:
            raise ValueError("duplicate workspace/benchmark case")
        grouped[case["workspace"]][case["benchmark"]] = case
    rows = []
    for workspace, benchmarks in grouped.items():
        if set(benchmarks) != set(config["benchmark_weights"]):
            raise ValueError("every weighted benchmark result is required for each workspace")
        values = {key:value["score"] for key,value in benchmarks.items()}
        rows.append({"workspace":workspace, "score":weighted(values, config["benchmark_weights"]),
                     "benchmarks":values, "calculation":aggregation(values, config["benchmark_weights"])})
    return sorted(rows, key=lambda row: (-row["score"], row["workspace"]))
