use crate::Result;
use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

pub const TRACKED_ATTRIBUTES: [&str; 3] = ["languages", "functions", "priority"];

pub fn read(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    Ok(serde_json::from_slice(bytes)?)
}

fn f(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn path_key(path: &str) -> String {
    path.strip_prefix("\\\\?\\").unwrap_or(path).replace('\\', "/").trim_end_matches('/').to_lowercase()
}

pub fn inside(path: &str, root: &str) -> bool {
    let (path, root) = (path_key(path), path_key(root));
    path == root || path.starts_with(&format!("{root}/"))
}

fn int(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().map(|v| v as i64))
}

fn event_usage(path: &Path) -> (HashMap<String, i64>, usize) {
    let mut usage = HashMap::new();
    let mut turns = 0;
    if let Ok(text) = std::fs::read(path) {
        for line in String::from_utf8_lossy(&text).lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
            if event["type"] == "turn.completed" && event["usage"].is_object() {
                turns += 1;
                for (key, value) in event["usage"].as_object().unwrap() {
                    if let Some(v) = value.as_i64() {
                        *usage.entry(key.clone()).or_insert(0) += v;
                    }
                }
            }
        }
    }
    (usage, turns)
}

fn measured_tokens(attempt: &Value, board: &Path) -> Option<i64> {
    let usage = &attempt["evidence"]["usage"];
    if let Some(total) = int(&usage["totalTokens"]).or_else(|| int(&usage["total_tokens"])) {
        return Some(total);
    }
    let directory = board.join("attempts").join(attempt["id"].as_str().unwrap_or(""));
    let (fields, turns) = event_usage(&directory.join("events.jsonl"));
    if turns > 0 {
        return Some(fields.get("input_tokens").copied().unwrap_or(0) + fields.get("output_tokens").copied().unwrap_or(0));
    }
    let file = directory.join("usage.json");
    if file.exists() {
        let data = read(&file).ok()?;
        return int(&data["total_tokens"]).or_else(|| int(&data["totalTokens"]));
    }
    None
}

pub fn parallel_metrics(intervals: &[(String, i64, i64)]) -> Value {
    let mut events: BTreeMap<i64, Vec<(&str, i64)>> = BTreeMap::new();
    let mut total = 0.0;
    for (task, start, end) in intervals {
        if end <= start {
            continue;
        }
        events.entry(*start).or_default().push((task, 1));
        events.entry(*end).or_default().push((task, -1));
        total += (end - start) as f64 / 1000.0;
    }
    let mut active: HashMap<&str, i64> = HashMap::new();
    let mut previous: Option<i64> = None;
    let (mut busy, mut overlap, mut task_seconds) = (0.0, 0.0, 0.0);
    let mut peak = 0;
    for (at, changes) in &events {
        if let Some(previous) = previous {
            let seconds = (at - previous) as f64 / 1000.0;
            let count = active.values().filter(|v| **v > 0).count();
            if count > 0 {
                busy += seconds;
            }
            if count >= 2 {
                overlap += seconds;
            }
            task_seconds += seconds * count as f64;
        }
        for (task, delta) in changes {
            *active.entry(task).or_insert(0) += delta;
        }
        peak = peak.max(active.values().filter(|v| **v > 0).count());
        previous = Some(*at);
    }
    json!({"worker_agent_seconds":total, "distinct_task_seconds":task_seconds, "busy_seconds":busy, "parallel_seconds":overlap,
           "peak_parallel_tasks":peak, "effective_parallelism":if busy > 0.0 { task_seconds / busy } else { 0.0 },
           "parallel_fraction":if busy > 0.0 { overlap / busy } else { 0.0 }})
}

fn supersedes(task: &Value) -> Vec<String> {
    let mut targets: BTreeSet<String> = list(&task["supersedes"]).iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    for event in list(&task["history"]) {
        if event["kind"] == "created" {
            targets.extend(list(&event["evidence"]["supersedes"]).iter().filter_map(|v| v.as_str().map(str::to_owned)));
        }
    }
    targets.into_iter().collect()
}

fn attribute_missing(task: &Value, name: &str) -> bool {
    let value = &task["attributes"][name];
    value.is_null() || value.as_array().is_some_and(Vec::is_empty) || value.as_str() == Some("") || value.as_object().is_some_and(Map::is_empty)
}

type Edge = (String, String, String);

fn classification_metrics(tasks: &[Value], edges: &BTreeSet<Edge>) -> Value {
    let mut missing = Map::new();
    let mut filled = Map::new();
    for name in TRACKED_ATTRIBUTES {
        let mut ids: Vec<String> = tasks.iter().filter(|t| attribute_missing(t, name)).map(|t| t["id"].as_str().unwrap_or("").to_owned()).collect();
        ids.sort();
        filled.insert(name.into(), json!(tasks.len() - ids.len()));
        missing.insert(name.into(), json!(ids));
    }
    let dependencies: Vec<&Edge> = edges.iter().filter(|e| e.2.starts_with("dependency:")).collect();
    let unclassified: Vec<&Edge> = dependencies.iter().copied().filter(|e| e.2 == "dependency:unclassified").collect();
    let d = dependencies.len();
    let share = |count: usize, empty: f64| if d > 0 { count as f64 / d as f64 } else { empty };
    json!({"task_count":tasks.len(), "missing_by_attribute":missing, "filled_by_attribute":filled, "dependency_count":d,
           "unclassified_edges":unclassified.iter().map(|e| json!([e.0, e.1, e.2])).collect::<Vec<_>>(),
           "unclassified_dependency_share":share(unclassified.len(), 0.0), "classified_dependency_share":share(d - unclassified.len(), 1.0)})
}

pub fn graph_metrics(tasks: &[Value]) -> Value {
    let ids: BTreeSet<&str> = tasks.iter().filter_map(|t| t["id"].as_str()).collect();
    let mut edges: BTreeSet<Edge> = BTreeSet::new();
    let mut external = 0;
    for task in tasks {
        let source = task["id"].as_str().unwrap_or("");
        let mut candidates: Vec<(String, Value)> = vec![("parent".into(), task["parent"].clone())];
        candidates.extend(supersedes(task).into_iter().map(|t| ("supersedes".into(), json!(t))));
        for relation in ["after", "conflicts", "related"] {
            candidates.extend(list(&task[relation]).iter().map(|t| (relation.to_owned(), t.clone())));
        }
        for link in list(&task["dependencies"]).iter().filter(|l| l.is_object()) {
            let reason = if truthy(&link["reason"]) { link["reason"].as_str().map_or_else(|| link["reason"].to_string(), str::to_owned) } else { "unclassified".into() };
            candidates.push((format!("dependency:{reason}"), link["target"].clone()));
        }
        for (relation, target) in candidates {
            let Some(target) = target.as_str().filter(|t| !t.is_empty() && *t != source) else { continue };
            if !ids.contains(target) {
                external += 1;
                continue;
            }
            let (a, b) = if (relation == "related" || relation == "conflicts") && target < source { (target, source) } else { (source, target) };
            edges.insert((a.into(), b.into(), relation));
        }
    }
    let mut neighbors: BTreeMap<&str, BTreeSet<&str>> = ids.iter().map(|id| (*id, BTreeSet::new())).collect();
    for (source, target, _) in &edges {
        neighbors.get_mut(source.as_str()).unwrap().insert(target);
        neighbors.get_mut(target.as_str()).unwrap().insert(source);
    }
    let degrees: Vec<usize> = neighbors.values().map(BTreeSet::len).collect();
    let mut types: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, _, relation) in &edges {
        *types.entry(relation).or_insert(0) += 1;
    }
    let n = tasks.len();
    let contribution = |degree: usize| (degree as f64 / 2.0).min(1.0);
    json!({"task_count":n, "runnable_tasks":tasks.iter().filter(|t| truthy(&t["acceptance"])).count(),
           "containers":tasks.iter().filter(|t| !truthy(&t["acceptance"])).count(), "edge_count":edges.len(),
           "relation_types":types, "distinct_relation_types":types.len(),
           "dependency_edges":types.iter().filter(|(r, _)| r.starts_with("dependency:")).map(|(_, c)| c).sum::<usize>(),
           "orphans":degrees.iter().filter(|d| **d == 0).count(), "near_orphans":degrees.iter().filter(|d| **d == 1).count(),
           "connectivity":if n > 0 { degrees.iter().map(|d| contribution(*d)).sum::<f64>() / n as f64 } else { 0.0 },
           "external_edges_excluded":external, "edges":edges.iter().map(|e| json!([e.0, e.1, e.2])).collect::<Vec<_>>(),
           "connectivity_by_task":neighbors.iter().map(|(k, v)| (k.to_string(), json!({"neighbors":v, "degree":v.len(), "contribution":contribution(v.len())}))).collect::<Map<_, _>>(),
           "classification":classification_metrics(tasks, &edges)})
}

fn attribute_fill(classification: &Value, attributes: &[String]) -> Value {
    let filled: i64 = attributes.iter().map(|a| classification["filled_by_attribute"][a].as_i64().unwrap_or(0)).sum();
    let slots = classification["task_count"].as_i64().unwrap_or(0) * attributes.len() as i64;
    json!({"attributes":attributes, "filled_slots":filled, "total_slots":slots,
           "filled_by_attribute":attributes.iter().map(|a| (a.clone(), classification["filled_by_attribute"][a].clone())).collect::<Map<_, _>>(),
           "missing_by_attribute":attributes.iter().map(|a| (a.clone(), classification["missing_by_attribute"][a].clone())).collect::<Map<_, _>>(),
           "attribute_fill_share":if slots > 0 { filled as f64 / slots as f64 } else { 0.0 }})
}

pub fn source_line_details(root: &Path, artifact: &Value) -> Result<Value> {
    if !artifact.is_object() {
        return Ok(json!({}));
    }
    let submission = read(&root.join("submissions").join(artifact["id"].as_str().unwrap_or("")).join("submission.json"))?;
    let sources: BTreeSet<&str> = list(&artifact["source_files"]).iter().filter_map(Value::as_str).collect();
    let mut lines = BTreeMap::new();
    for file in list(&submission["files"]) {
        let path = file["path"].as_str().unwrap_or("");
        if !sources.contains(path) {
            continue;
        }
        let text = match file["text"].as_str() {
            Some(text) => text.to_owned(),
            None => String::from_utf8(base64::engine::general_purpose::STANDARD.decode(file["base64"].as_str().unwrap_or(""))?)?,
        };
        lines.insert(path.to_owned(), text.split('\n').filter(|l| !l.trim().is_empty()).count());
    }
    if lines.values().sum::<usize>() as i64 != artifact["lines_of_code"].as_i64().unwrap_or(-1) {
        return Err("Frozen source line breakdown disagrees with the accepted artifact".into());
    }
    Ok(json!(lines))
}

fn session_planner(session: &Value, run: &Value) -> (Value, Option<i64>, f64) {
    let requests = list(&session["requests"]);
    let deadline = run["deadline_ms"].as_i64().unwrap_or(0);
    let started = run["started_at_ms"].as_i64().unwrap_or(0);
    let seconds = requests.iter().map(|r| {
        let ended = r["plannerEnded"].as_i64().filter(|v| *v != 0).or_else(|| r["planner_ended"].as_i64().filter(|v| *v != 0)).unwrap_or(deadline);
        (ended.min(deadline) - r["started"].as_i64().unwrap_or(0).max(started)).max(0)
    }).sum::<i64>() as f64 / 1000.0;
    let mut fields: BTreeMap<String, i64> = BTreeMap::new();
    for usage in requests.iter().map(|r| &r["usage"]) {
        for (key, value) in usage.as_object().into_iter().flatten() {
            if key != "totalTokens" && let Some(v) = value.as_i64() {
                *fields.entry(key.clone()).or_insert(0) += v;
            }
        }
    }
    let tokens = (!requests.is_empty() && requests.iter().all(|r| r["usage"]["totalTokens"].is_number()))
        .then(|| requests.iter().map(|r| int(&r["usage"]["totalTokens"]).unwrap_or(0)).sum());
    (json!(fields), tokens, seconds)
}

pub fn collect(case: &Value, run: Option<&Value>, root: &Path, board: Option<&Path>, session: &Value) -> Result<Value> {
    let Some(run) = run else {
        return Ok(json!({"run_id":null, "workspace":case["workspace"], "benchmark":case["benchmark"], "settings_at_run":case["settings"],
            "dataset":null, "artifact":null, "source_lines":null, "source_lines_by_file":{}, "graph":graph_metrics(&[]), "parallel":parallel_metrics(&[]),
            "tokens":{"recorded_total":0, "measured_launches":0, "launches":0, "complete":true, "planner_usage":{}, "proxy_agent_seconds":0, "development_seconds":0},
            "evidence":{"error":case["error"], "tasks":[]}}));
    };
    let board = board.ok_or("Coordinator data directory is required for cases with runs")?;
    let state_path = board.join("state.json");
    let state_bytes = std::fs::read(&state_path)?;
    let state = read(&state_path)?;
    let directory = case["directory"].as_str().unwrap_or("");
    let mut order: Vec<String> = Vec::new();
    let mut tasks: HashMap<String, Value> = HashMap::new();
    for task in list(&state["archived"]).iter().chain(list(&state["tasks"])) {
        if inside(task["cwd"].as_str().unwrap_or(""), directory) {
            let id = task["id"].as_str().unwrap_or("").to_owned();
            if tasks.insert(id.clone(), task.clone()).is_none() {
                order.push(id);
            }
        }
    }
    let tasks: Vec<Value> = order.iter().map(|id| tasks.remove(id).unwrap()).collect();
    let started = run["started_at_ms"].as_i64().unwrap_or(0);
    let deadline = run["deadline_ms"].as_i64().unwrap_or(0);
    let mut intervals = Vec::new();
    let mut launches: Vec<Option<i64>> = Vec::new();
    let mut records = Vec::new();
    for task in &tasks {
        let mut attempts = Vec::new();
        for attempt in list(&task["attempts"]) {
            let start = attempt["started"].as_i64().unwrap_or(0).max(started);
            let end = attempt["ended"].as_i64().filter(|v| *v != 0).unwrap_or(deadline).min(deadline);
            let id = task["id"].as_str().unwrap_or("").to_owned();
            intervals.push((id.clone(), start, end));
            let total = measured_tokens(attempt, board);
            launches.push(total);
            attempts.push(json!({"id":attempt["id"], "task_id":id, "stage":attempt["stage"], "start_ms":start, "end_ms":end, "tokens":total,
                                 "observed":attempt["evidence"]["observed"]}));
        }
        let mut record: Map<String, Value> = ["id", "title", "cwd", "parent", "after", "dependencies", "conflicts", "related", "acceptance", "stage", "attributes"]
            .iter().map(|k| (k.to_string(), task[*k].clone())).collect();
        record.insert("supersedes".into(), json!(supersedes(task)));
        record.insert("attempts".into(), json!(attempts));
        records.push(Value::Object(record));
    }
    let (fields, planner_tokens, planner_seconds) = session_planner(session, run);
    launches.push(planner_tokens);
    let parallel = parallel_metrics(&intervals);
    let known: Vec<i64> = launches.iter().flatten().copied().collect();
    let artifact = match run["submission_id"].as_str() {
        Some(id) => read(&root.join("submissions").join(id).join("artifact.json"))?,
        None => Value::Null,
    };
    Ok(json!({"run_id":run["id"], "workspace":case["workspace"], "benchmark":case["benchmark"], "settings_at_run":case["settings"],
        "dataset":run["dataset"], "artifact":artifact, "source_lines":artifact["lines_of_code"], "source_lines_by_file":source_line_details(root, &artifact)?,
        "graph":graph_metrics(&tasks), "parallel":parallel,
        "tokens":{"recorded_total":known.iter().sum::<i64>(), "measured_launches":known.len(), "launches":launches.len(), "complete":known.len() == launches.len(),
                  "planner_usage":fields, "proxy_agent_seconds":f(&parallel["worker_agent_seconds"]) + planner_seconds, "planner_seconds":planner_seconds,
                  "development_seconds":(deadline - started) as f64 / 1000.0},
        "evidence":{"state_path":state_path.display().to_string(), "state_sha256":format!("{:x}", Sha256::digest(&state_bytes)),
                    "session":session["id"], "tasks":records}}))
}

fn weighted(values: &Value, weights: &Value) -> f64 {
    let weights = weights.as_object().unwrap();
    weights.iter().map(|(k, w)| f(&values[k]) * f(w)).sum::<f64>() / weights.values().map(f).sum::<f64>()
}

fn aggregation(values: &Value, weights: &Value) -> Value {
    let denominator: f64 = weights.as_object().unwrap().values().map(f).sum();
    let terms: Map<String, Value> = weights.as_object().unwrap().iter().map(|(k, w)| {
        let (v, w) = (f(&values[k]), f(w));
        (k.clone(), json!({"value":values[k], "weight":w, "weighted_value":v * w, "normalized_weight":w / denominator, "contribution":v * w / denominator}))
    }).collect();
    let weighted_sum: f64 = terms.values().map(|t| f(&t["weighted_value"])).sum();
    json!({"operation":"weighted_mean", "terms":terms, "weight_sum":denominator, "weighted_sum":weighted_sum, "value":weighted(values, weights)})
}

fn ones(values: &Value) -> Value {
    Value::Object(values.as_object().unwrap().keys().map(|k| (k.clone(), json!(1))).collect())
}

fn calculation(formula: &str, inputs: Value, value: impl Into<Value>, details: Value) -> Value {
    let mut out = json!({"formula":formula, "inputs":inputs, "value":value.into()});
    if let Value::Object(details) = details {
        out.as_object_mut().unwrap().extend(details);
    }
    out
}

pub fn score(metrics: &Value, report: &Value, config: &Value, token_mode: &str) -> Result<Value> {
    let refs = &config["references"];
    let (graph, parallel, tokens) = (&metrics["graph"], &metrics["parallel"], &metrics["tokens"]);
    let n = graph["task_count"].as_i64().unwrap_or(0);
    let nf = n as f64;
    let relation_types = f(&graph["distinct_relation_types"]);
    let richness = if n > 0 { f(&graph["edge_count"]) / nf } else { 0.0 };
    let relation_values = json!({"connectivity":graph["connectivity"], "type_diversity":relation_types / (relation_types + f(&refs["relation_types"])),
                                 "edge_density":richness / (richness + f(&refs["edges_per_task"]))});
    let relations = 100.0 * (f(&graph["connectivity"]) + f(&relation_values["type_diversity"]) + f(&relation_values["edge_density"])) / 3.0;
    let concurrency = f(&parallel["effective_parallelism"]);
    let concurrency_term = if concurrency != 0.0 { 1.0 - 1.0 / concurrency } else { 0.0 };
    let parallelism = 50.0 * (f(&parallel["parallel_fraction"]) + concurrency_term);
    let maze = metrics["benchmark"] == "maze_game";
    let completed = if maze && truthy(report) { f(&report["progress"]["fraction_sum"]) } else { 0.0 };
    let per_request = if maze { completed.max(1.0) } else { 1.0 };
    let measured = token_mode == "measured_tokens";
    let numerator = if measured { f(&tokens["recorded_total"]) } else { f(&tokens["proxy_agent_seconds"]) };
    let denominator = if measured { f(&refs["tokens"]) } else { f(&tokens["development_seconds"]) };
    let token_cost = if n > 0 { numerator / denominator / per_request } else { 0.0 };
    let classification = &graph["classification"];
    let attributes: Vec<String> = list(&config["classification_attributes"]).iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let fill = attribute_fill(classification, &attributes);
    let parts = json!({"task_count":100.0 * nf / (nf + f(&refs["task_count"])), "relations":relations, "parallelism":parallelism,
                       "token_efficiency":if n > 0 { 100.0 / (1.0 + token_cost) } else { 0.0 },
                       "classification":100.0 * f(&fill["attribute_fill_share"]) * f(&classification["classified_dependency_share"])});
    let artifact = truthy(&metrics["artifact"]);
    let source_lines = f(&metrics["source_lines"]);
    let mean = f(&report["mean_out_of_5"]);
    let (result, simplicity, result_trace);
    if !artifact {
        result = f(&config["missing_submission"]["result"]);
        simplicity = f(&config["missing_submission"]["simplicity"]);
        result_trace = calculation("missing_submission.result", config["missing_submission"].clone(), result, json!({"source":"missing_submission_policy"}));
    } else {
        simplicity = 100.0 / (1.0 + source_lines / per_request / f(&refs["source_lines"]));
        if report.is_null() {
            return Err(format!("Missing quality report for {}", metrics["run_id"]).into());
        }
        if metrics["benchmark"] == "chess960" {
            let delta = f(&report["metrics"]["elo_change"]);
            result = 100.0 / (1.0 + 10f64.powf(-delta / f(&refs["elo_scale"])));
            result_trace = calculation("100 / (1 + 10 ** (-elo_change / elo_scale))", json!({"elo_change":report["metrics"]["elo_change"], "elo_scale":refs["elo_scale"]}),
                result, json!({"source":"deterministic_chess_tournament", "tournament_metrics":report["metrics"]}));
        } else {
            let criteria: Map<String, Value> = report["scores"].as_object().into_iter().flatten()
                .filter(|(_, v)| v["score"].is_number()).map(|(k, v)| (k.clone(), v.clone())).collect();
            let ratings: Value = Value::Object(criteria.iter().map(|(k, v)| (k.clone(), v["score"].clone())).collect());
            let quality_trace = calculation("mean_out_of_5 * 20", json!({"mean_out_of_5":report["mean_out_of_5"]}), mean * 20.0,
                json!({"source":"independent_judge", "criteria":criteria, "mean":if criteria.is_empty() { Value::Null } else { aggregation(&ratings, &ones(&ratings)) }}));
            if metrics["benchmark"] == "ml_predictor" {
                let weights = &config["ml"];
                let evaluation = if truthy(&report["model_evaluation"]) { report["model_evaluation"].clone() } else { json!({"normalized":0.0}) };
                let eda = 20.0 * mean;
                let (ew, mw) = (f(&weights["eda_weight"]), f(&weights["model_weight"]));
                result = (ew * eda + mw * f(&evaluation["normalized"])) / (ew + mw);
                let mut trace = aggregation(&json!({"eda":eda, "model":evaluation["normalized"]}), &json!({"eda":weights["eda_weight"], "model":weights["model_weight"]}));
                trace["components"] = json!({"eda":quality_trace,
                    "model":calculation("piecewise: trivial -> 0, reference -> 50, perfect -> 100", evaluation.clone(), evaluation["normalized"].clone(), Value::Null)});
                result_trace = trace;
            } else if maze {
                let weights = &config["maze"];
                let completion = completed / f(&report["progress"]["requests_total"]);
                let quality = mean / 5.0;
                let (cw, qw) = (f(&weights["completion_weight"]), f(&weights["quality_weight"]));
                result = 100.0 * (cw * completion + qw * quality) / (cw + qw);
                let mut trace = aggregation(&json!({"completion":completion * 100.0, "quality":quality * 100.0}),
                    &json!({"completion":weights["completion_weight"], "quality":weights["quality_weight"]}));
                trace["components"] = json!({"quality":quality_trace, "completion":calculation("100 * fraction_sum / requests_total", report["progress"].clone(), completion * 100.0,
                    json!({"requests":report["scores"].get("requests").cloned().unwrap_or(json!([]))}))});
                result_trace = trace;
            } else {
                result = 20.0 * mean;
                result_trace = quality_trace;
            }
        }
    }
    let categories = json!({"result":result, "simplicity":simplicity, "framework":weighted(&parts, &config["framework_weights"])});
    let normalization = if maze { json!({"requests_completed":completed, "divisor":per_request}) } else { Value::Null };
    let mut trace = json!({
        "result":result_trace,
        "source_line_count":calculation("sum(nonblank_source_lines_by_file)", metrics.get("source_lines_by_file").cloned().unwrap_or(json!({})), metrics["source_lines"].clone(), Value::Null),
        "simplicity":calculation(if artifact { "100 / (1 + source_lines / request_divisor / reference_lines)" } else { "missing_submission.simplicity" },
            json!({"source_lines":metrics["source_lines"], "request_divisor":per_request, "reference_lines":refs["source_lines"],
                   "normalized_lines":if artifact { json!(source_lines / per_request) } else { Value::Null },
                   "missing_submission_value":config["missing_submission"]["simplicity"]}), simplicity, Value::Null),
        "framework":aggregation(&parts, &config["framework_weights"]),
        "framework_components":{
            "task_count":calculation("100 * task_count / (task_count + reference_tasks)",
                json!({"task_count":n, "reference_tasks":refs["task_count"], "containers":graph["containers"], "runnable_tasks":graph["runnable_tasks"]}), parts["task_count"].clone(), Value::Null),
            "relations":calculation("100 * mean(connectivity, type_diversity, edge_density)",
                json!({"connectivity":graph["connectivity"], "distinct_relation_types":graph["distinct_relation_types"], "edge_count":graph["edge_count"], "task_count":n,
                       "edges_per_task":richness, "reference_types":refs["relation_types"], "reference_density":refs["edges_per_task"]}), relations,
                json!({"components":{
                    "connectivity":calculation("mean(min(unique_neighbors / 2, 1))", json!({"tasks":graph["connectivity_by_task"], "task_count":n}), graph["connectivity"].clone(), Value::Null),
                    "type_diversity":calculation("distinct_types / (distinct_types + reference_types)", json!({"distinct_types":graph["distinct_relation_types"], "reference_types":refs["relation_types"]}), relation_values["type_diversity"].clone(), Value::Null),
                    "edge_density":calculation("edges_per_task / (edges_per_task + reference_density)", json!({"edges_per_task":richness, "reference_density":refs["edges_per_task"]}), relation_values["edge_density"].clone(), Value::Null)},
                    "aggregation":aggregation(&relation_values, &ones(&relation_values))})),
            "parallelism":calculation("50 * (parallel_fraction + (1 - 1 / effective_parallelism if effective_parallelism else 0))", parallel.clone(), parallelism,
                json!({"overlap_term":parallel["parallel_fraction"], "concurrency_term":concurrency_term, "components":{
                    "parallel_fraction":calculation("parallel_seconds / busy_seconds if busy_seconds else 0", json!({"parallel_seconds":parallel["parallel_seconds"], "busy_seconds":parallel["busy_seconds"]}), parallel["parallel_fraction"].clone(), Value::Null),
                    "effective_parallelism":calculation("distinct_task_seconds / busy_seconds if busy_seconds else 0", json!({"distinct_task_seconds":parallel["distinct_task_seconds"], "busy_seconds":parallel["busy_seconds"]}), concurrency, Value::Null)}})),
            "token_efficiency":calculation("100 / (1 + cost) if task_count else 0",
                json!({"mode":token_mode, "numerator":numerator, "denominator":denominator, "request_divisor":per_request, "task_count":n}), parts["token_efficiency"].clone(),
                json!({"cost":calculation("numerator / denominator / request_divisor if task_count else 0",
                    json!({"numerator":numerator, "denominator":denominator, "request_divisor":per_request, "task_count":n}), token_cost, Value::Null)}))},
        "benchmark":aggregation(&categories, &config["category_weights"])});
    let mut inputs = fill.clone();
    inputs["classified_dependency_share"] = classification["classified_dependency_share"].clone();
    inputs["dependency_count"] = classification["dependency_count"].clone();
    inputs["unclassified_edges"] = classification["unclassified_edges"].clone();
    inputs["definition"] = json!("attribute_fill_share = filled_slots / (task_count * len(attributes)). An attribute is unfilled when it is null or empty. Containers count. No dependency edges means no dependency penalty; no tasks earns zero.");
    trace["framework_components"]["classification"] = calculation("100 * attribute_fill_share * classified_dependency_share", inputs, parts["classification"].clone(),
        json!({"unfilled_penalty_share":1.0 - f(&fill["attribute_fill_share"]), "unclassified_penalty_share":classification["unclassified_dependency_share"]}));
    let mut out = metrics.as_object().cloned().unwrap_or_default();
    out.extend([("quality_report".into(), report.clone()), ("token_scoring_mode".into(), json!(token_mode)), ("framework_components".into(), parts),
        ("per_request_normalization".into(), normalization), ("score".into(), json!(weighted(&categories, &config["category_weights"]))),
        ("categories".into(), categories), ("calculation".into(), trace), ("status".into(), json!(if artifact { "scored" } else { "no_submission" }))]);
    Ok(Value::Object(out))
}

pub fn aggregate(cases: &[Value], config: &Value) -> Result<Value> {
    let mut grouped: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for case in cases {
        let group = grouped.entry(case["workspace"].as_str().unwrap_or("").to_owned()).or_default();
        let benchmark = case["benchmark"].as_str().unwrap_or("").to_owned();
        if group.insert(benchmark, case["score"].clone()).is_some() {
            return Err("duplicate workspace/benchmark case".into());
        }
    }
    let expected: BTreeSet<&String> = config["benchmark_weights"].as_object().ok_or("benchmark_weights missing")?.keys().collect();
    let mut rows = Vec::new();
    for (workspace, values) in grouped {
        if values.keys().collect::<BTreeSet<_>>() != expected {
            return Err("every weighted benchmark result is required for each workspace".into());
        }
        let values = Value::Object(values);
        rows.push(json!({"workspace":workspace, "score":weighted(&values, &config["benchmark_weights"]), "benchmarks":values,
                         "calculation":aggregation(&values, &config["benchmark_weights"])}));
    }
    rows.sort_by(|a, b| f(&b["score"]).total_cmp(&f(&a["score"])).then_with(|| a["workspace"].as_str().cmp(&b["workspace"].as_str())));
    Ok(json!(rows))
}
