use crate::{Result, agents, metrics::{self, read}, ml_eval, process::OwnedChild};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeSet, HashMap}, fs, path::{Path, PathBuf}, process::{Command, Stdio}, sync::Mutex, time::{Duration, Instant}};

pub type Api<'a> = &'a (dyn Fn(&str, &str, &str, Value) -> Result<Value> + Sync);

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    max_judges: std::num::NonZeroUsize,
    timeout_seconds: std::num::NonZeroU64,
}

impl Default for Options {
    fn default() -> Self {
        Self { max_judges: std::num::NonZeroUsize::new(2).unwrap(), timeout_seconds: std::num::NonZeroU64::new(1800).unwrap() }
    }
}

pub fn config() -> Value {
    serde_json::from_str(include_str!("../scoring.json")).expect("valid bundled scoring config")
}

const IMPLEMENTATION: [(&str, &str); 4] = [("scoring.rs", include_str!("scoring.rs")), ("metrics.rs", include_str!("metrics.rs")),
    ("ml_eval.rs", include_str!("ml_eval.rs")), ("agents.rs", include_str!("agents.rs"))];

pub fn validate_config(config: &Value, family: Option<&str>) -> Result<()> {
    let groups: [(&str, &[&str]); 6] = [
        ("category_weights", &["result", "simplicity", "framework"]),
        ("benchmark_weights", &["chess960", "game_build", "dataset_dashboard", "maze_game", "ml_predictor"]),
        ("framework_weights", &["task_count", "relations", "parallelism", "token_efficiency", "classification"]),
        ("references", &["elo_scale", "source_lines", "task_count", "relation_types", "edges_per_task", "tokens"]),
        ("maze", &["completion_weight", "quality_weight"]),
        ("ml", &["eda_weight", "model_weight"]),
    ];
    for (name, keys) in groups {
        let values = config[name].as_object().ok_or_else(|| format!("Invalid scoring configuration: {name}"))?;
        let positive = values.values().all(|v| v.as_f64().is_some_and(|v| v.is_finite() && v > 0.0));
        if values.keys().map(String::as_str).collect::<BTreeSet<_>>() != keys.iter().copied().collect() || !positive {
            return Err(format!("Invalid scoring configuration: {name}").into());
        }
    }
    let attributes: Vec<&str> = config["classification_attributes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let unique: BTreeSet<_> = attributes.iter().collect();
    if attributes.is_empty() || unique.len() != attributes.len() || !attributes.iter().all(|a| metrics::TRACKED_ATTRIBUTES.contains(a)) {
        return Err("Invalid scoring configuration: classification_attributes".into());
    }
    if config["incomplete_token_fallback"] != "agent_seconds_per_development_second" {
        return Err("Unsupported token fallback".into());
    }
    agents::resolve(judge_selection(config, family)?, family, true)?;
    Ok(())
}

fn judge_selection<'a>(config: &'a Value, family: Option<&str>) -> Result<&'a str> {
    config["judge_selections"][family.unwrap_or("")].as_str()
        .ok_or_else(|| format!("Scoring configuration has no judge for the {} launcher family", family.unwrap_or("undetected")).into())
}

pub fn judge_command(selection: &str, family: Option<&str>, prompt: &Path, output: &Path, browser_version: &str) -> Result<Vec<String>> {
    let browser = vec!["-y".to_owned(), format!("@playwright/mcp@{browser_version}"), "--isolated".into(), "--headless".into(), "--browser".into(), "chrome".into(),
        "--output-dir".into(), prompt.parent().unwrap_or(Path::new("")).join("browser").display().to_string()];
    let agent = agents::resolve(selection, family, true)?;
    let text = fs::read_to_string(prompt)?;
    if agent["harness"] == "claude_code" {
        let servers = json!({"mcpServers":{"playwright":{"command":"npx","args":browser}}});
        let mut argv = agents::claude_arguments(&agent);
        argv.extend(["--strict-mcp-config".into(), "--mcp-config".into(), servers.to_string(), "--".into(), text]);
        return Ok(argv);
    }
    if agent["harness"] != "codex" {
        return Err(format!("Judge {selection} needs a browser-capable harness").into());
    }
    let mut argv: Vec<String> = [std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".into()).as_str(), "exec", "--ephemeral", "--skip-git-repo-check", "--json",
        "--sandbox", "danger-full-access", "-c", "approval_policy=\"never\"", "--disable", "memories"].map(str::to_owned).to_vec();
    argv.extend(agents::codex_arguments(&agent));
    argv.extend(["-c".into(), "mcp_servers.chrome-devtools.enabled=false".into(), "-c".into(), "mcp_servers.playwright.command=\"npx\"".into(),
        "-c".into(), "mcp_servers.playwright.enabled=true".into(), "-c".into(), format!("mcp_servers.playwright.args={}", json!(browser)),
        "--output-last-message".into(), output.display().to_string(), text]);
    Ok(argv)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

struct Context<'a> {
    api: Api<'a>,
    admin: &'a str,
    url: &'a str,
    batch: &'a str,
    judge: &'a str,
    family: Option<&'a str>,
    output: &'a Path,
    config: &'a Value,
    timeout: Duration,
    log: Mutex<fs::File>,
}

impl Context<'_> {
    fn note(&self, value: Value) {
        use std::io::Write;
        let _ = writeln!(self.log.lock().unwrap(), "{value}");
    }
    fn report(&self, run: &str) -> Result<Value> {
        Ok((self.api)("GET", &format!("/v1/runs/{run}"), self.admin, Value::Null)?["report"].clone())
    }
}

fn judge_prompt(cx: &Context, case: &Value, job: &Value, directory: &Path, score: &Path) -> String {
    let id = job["id"].as_str().unwrap_or("");
    if case["benchmark"] == "ml_predictor" {
        return format!("\nReview only this submission's EDA report. Its files are in {}; extract any figures you need into your current directory. \
            Do not run, rebuild or repair the submission, and do not inspect other workspaces or solutions. \
            GET /v1/judgments/{id} on {} (token in HARNESS_JUDGMENT_TOKEN) reports when scoring opens. \
            Write your score object to {}; the runner submits it when scoring opens. You may finish after writing score.json. \
            Give concrete evidence for every criterion. The runner records your session and will not invent missing ratings.\n",
            directory.join("submission.json").display(), cx.url, score.display());
    }
    format!("\nReview only this submission. Benchmark: {}. UI: {}. Launch error: {}. Use the Playwright MCP tools for an isolated browser. \
        Capture actual screenshots and exercise the UI before rating it. Save screenshots in your current directory. \
        Use browser evaluation to record navigation/paint timings and requestAnimationFrame intervals during interaction. \
        Use the frozen submission.json here for source/data verification. Do not inspect other workspaces or solutions. \
        Do not change implementation or repair launch failures. Agent framework and code simplicity are scored by code elsewhere. \
        Your scoped benchmark API URL is {}; token is in HARNESS_JUDGMENT_TOKEN. \
        GET /v1/judgments/{id} reports exploration/scoring status. \
        POST /v1/judgments/{id}/action takes {{\"action\":\"describe\",\"params\":{{}}}} or observe and other actions. \
        Write your score object to {} after collecting evidence; the runner submits it when scoring opens. \
        Do not spend your whole session waiting for scoring to open. You may finish after writing score.json. \
        Give concrete evidence for every criterion and disclose unavailable measurements. \
        The runner records your session and will not invent missing ratings.\n",
        case["benchmark"].as_str().unwrap_or(""), job["ui_url"].as_str().unwrap_or("None"), job["launch_error"].as_str().unwrap_or("None"), cx.url, score.display())
}

fn review_case(cx: &Context, case: &Value) -> Result<Value> {
    let Some(run_id) = case["run_id"].as_str() else { return Ok(Value::Null) };
    let state = (cx.api)("GET", &format!("/v1/runs/{run_id}"), cx.admin, Value::Null)?;
    if !state["report"].is_null() {
        return Ok(state["report"].clone());
    }
    if state["run"]["submission_id"].is_null() {
        return Ok(Value::Null);
    }
    let directory = cx.output.join("judges").join(run_id);
    fs::create_dir_all(&directory)?;
    let until = Instant::now() + cx.timeout;
    let mut claimed = Value::Null;
    while Instant::now() < until {
        claimed = (cx.api)("POST", "/v1/judges/claim", cx.admin, json!({"judge":format!("batch:{}:{}", cx.batch, cx.judge), "run_id":run_id}))?;
        if claimed["job"].is_object() {
            break;
        }
        let jobs = (cx.api)("GET", "/v1/judgments", cx.admin, Value::Null)?;
        if let Some(previous) = jobs["judgments"].as_array().into_iter().flatten().find(|j| j["run_id"] == run_id) {
            match previous["status"].as_str() {
                Some("completed") => return cx.report(run_id),
                Some(status @ ("timed_out" | "interrupted")) => return Err(format!("Judge session already ended for {run_id}: {status}").into()),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let job = claimed["job"].clone();
    if !job.is_object() {
        return Err(format!("No judge job became available for {run_id}").into());
    }
    let job_id = job["id"].as_str().unwrap_or("").to_owned();
    let scoped = claimed["judgment_token"].as_str().unwrap_or("").to_owned();
    let score_path = directory.join("score.json");
    write_json(&directory.join("submission.json"), &(cx.api)("GET", &format!("/v1/judgments/{job_id}/artifact"), &scoped, Value::Null)?)?;
    let prompt_path = directory.join("prompt.txt");
    fs::write(&prompt_path, format!("{}{}", claimed["prompt"].as_str().unwrap_or(""), judge_prompt(cx, case, &job, &directory, &score_path)))?;
    let command = judge_command(cx.judge, cx.family, &prompt_path, &directory.join("last-message.txt"), cx.config["playwright_mcp_version"].as_str().unwrap_or(""))?;
    let agent = agents::resolve(cx.judge, cx.family, true)?;
    write_json(&directory.join("review-metadata.json"), &json!({"run_id":run_id, "judgment_id":job_id, "model":agent["model"], "effort":agent["effort"],
        "prompt_sha256":format!("{:x}", Sha256::digest(fs::read(&prompt_path)?)), "artifact_id":state["run"]["submission_id"], "fresh_session":true, "browser_isolated":true}))?;
    let mut process = Command::new(&command[0]);
    process.env_clear();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy();
        let hidden = matches!(name.as_ref(), "HARNESS_API_TOKEN" | "HARNESS_JUDGE_TOKEN" | "HARNESS_RUN_TOKEN" | "HARNESS_JUDGMENT_TOKEN" | "CODEX_WINDOWS_REGISTERED_CORE")
            || (agent["harness"] == "claude_code" && agents::hosted_claude_variable(&name));
        if !hidden {
            process.env(key, value);
        }
    }
    let log = fs::File::create(directory.join("agent.jsonl"))?;
    process.env("HARNESS_JUDGMENT_TOKEN", &scoped).env("HARNESS_URL", cx.url).args(&command[1..]).current_dir(&directory)
        .stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    let mut child = OwnedChild::launch(process)?;
    while Instant::now() < until {
        let status = (cx.api)("GET", &format!("/v1/judgments/{job_id}"), &scoped, Value::Null)?["job"]["status"].clone();
        match status.as_str() {
            Some("completed") => return cx.report(run_id),
            Some(ended @ ("timed_out" | "interrupted")) => return Err(format!("Judge {run_id} ended with {ended}; inspect {}", directory.display()).into()),
            Some("scoring") if score_path.exists() => {
                match read(&score_path).and_then(|score| (cx.api)("POST", &format!("/v1/judgments/{job_id}/score"), &scoped, score)) {
                    Ok(result) => {
                        write_json(&directory.join("report.json"), &result["report"])?;
                        return Ok(result["report"].clone());
                    }
                    Err(error) => fs::write(directory.join("score-error.txt"), error.to_string())?,
                }
            }
            _ => {}
        }
        if let Some(exit) = child.try_wait()?
            && !score_path.exists()
        {
            return Err(format!("Judge exited {} without scores; inspect {}", exit.code().unwrap_or(-1), directory.display()).into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("Judge timeout for {run_id}").into())
}

pub fn run(api: Api, admin: &str, root: &Path, url: &str, batch_id: &str, coordinator: &str, options: Options) -> Result<Value> {
    let config = config();
    let family = agents::family();
    validate_config(&config, family)?;
    let judge = judge_selection(&config, family)?;
    let output = root.join("batch-scores").join(batch_id).join(config["version"].as_str().ok_or("Scoring version is missing")?);
    fs::create_dir_all(&output)?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(output.join(".scoring.lock"))?;
    lock.try_lock_exclusive().map_err(|_| "Another scoring command already owns this output directory")?;
    let cx = Context { api, admin, url, batch: batch_id, judge, family, output: &output, config: &config,
        timeout: Duration::from_secs(options.timeout_seconds.get()), log: Mutex::new(fs::OpenOptions::new().create(true).append(true).open(output.join("scoring.log"))?) };
    let result = score_batch(&cx, root, coordinator, options.max_judges.get());
    if let Err(error) = &result {
        cx.note(json!({"error":error.to_string()}));
    }
    result
}

fn score_batch(cx: &Context, root: &Path, coordinator: &str, max_judges: usize) -> Result<Value> {
    let (api, admin, output, config) = (cx.api, cx.admin, cx.output, cx.config);
    let mut batch = api("GET", &format!("/v1/benchmark-batches/{}", cx.batch), admin, Value::Null)?["batch"].clone();
    let weights = config["benchmark_weights"].as_object().unwrap();
    let all: Vec<Value> = batch["cases"].as_array().cloned().unwrap_or_default();
    let (cases, deferred): (Vec<Value>, Vec<Value>) = all.into_iter().partition(|c| weights.contains_key(c["benchmark"].as_str().unwrap_or("")));
    batch["cases"] = json!(cases);
    if let Some(campaigns) = batch["campaigns"].as_object_mut() {
        campaigns.retain(|name, _| weights.contains_key(name));
    }
    if cases.iter().any(|c| c["status"] == "queued" || c["status"] == "running") {
        return Err("Development must finish before scoring".into());
    }
    if output.join("config.json").exists() && read(&output.join("config.json"))? != *config {
        return Err("Scoring config changed; use a new version or output directory".into());
    }
    if output.join("batch.json").exists() && read(&output.join("batch.json"))?["cases"] != batch["cases"] {
        return Err("Selected batch attempts changed; use a new output directory".into());
    }
    write_json(&output.join("batch.json"), &batch)?;
    write_json(&output.join("config.json"), config)?;
    let board: PathBuf = if cases.iter().chain(&deferred).any(|c| c["run_id"].is_string()) {
        let state = crate::coordinator::state(coordinator)?;
        PathBuf::from(state["dataDirectory"].as_str().ok_or("coordinator reported no data directory")?.trim_start_matches("\\\\?\\"))
    } else {
        PathBuf::new()
    };
    let session = |case: &Value| -> Result<Value> {
        match case["session"].as_str() {
            Some(id) => crate::coordinator::session(coordinator, id),
            None => Ok(Value::Null),
        }
    };
    let collect = |case: &Value| -> Result<(Value, Value)> {
        let run = match case["run_id"].as_str() {
            Some(id) => api("GET", &format!("/v1/runs/{id}"), admin, Value::Null)?,
            None => Value::Null,
        };
        let run_value = (!run["run"].is_null()).then_some(&run["run"]);
        Ok((metrics::collect(case, run_value, root, Some(&board), &session(case)?)?, run))
    };
    let identity = |item: &Value| (item["workspace"].to_string(), item["benchmark"].to_string(), item["run_id"].as_str().unwrap_or("").to_owned());
    let metric_rows: Vec<Value> = if output.join("metrics.json").exists() {
        let saved: Vec<Value> = serde_json::from_value(read(&output.join("metrics.json"))?)?;
        let mut a: Vec<_> = saved.iter().map(identity).collect();
        let mut b: Vec<_> = cases.iter().map(identity).collect();
        a.sort();
        b.sort();
        if a != b {
            return Err("Saved metrics do not match the selected batch attempts".into());
        }
        saved
    } else {
        let rows = cases.iter().map(|c| collect(c).map(|(m, _)| m)).collect::<Result<Vec<_>>>()?;
        write_json(&output.join("metrics.json"), &json!(rows))?;
        rows
    };
    for campaign in batch["campaigns"].as_object().into_iter().flatten().filter_map(|(_, id)| id.as_str()) {
        if !cases.iter().any(|c| c["campaign_id"] == campaign && c["run_id"].is_string()) {
            continue;
        }
        let status = api("GET", &format!("/v1/campaigns/{campaign}"), admin, Value::Null)?["campaign"]["status"].clone();
        if ["enrolling", "awaiting_score", "waiting"].iter().any(|s| status == *s) {
            api("POST", &format!("/v1/campaigns/{campaign}/score"), admin, json!({}))?;
        }
    }
    let queue = Mutex::new(cases.iter().filter(|c| c["benchmark"] != "chess960").collect::<Vec<_>>());
    let reviewed: Mutex<HashMap<String, Value>> = Mutex::new(HashMap::new());
    let errors: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..max_judges {
            scope.spawn(|| loop {
                let Some(case) = queue.lock().unwrap().pop() else { break };
                let run = case["run_id"].as_str().unwrap_or("").to_owned();
                match review_case(cx, case) {
                    Ok(report) => {
                        cx.note(json!({"run_id":run, "status":if report.is_null() { "no_submission" } else { "reviewed" }, "benchmark":case["benchmark"], "workspace":case["workspace"]}));
                        reviewed.lock().unwrap().insert(run, report);
                    }
                    Err(error) => {
                        let entry = json!({"run_id":case["run_id"], "error":error.to_string()});
                        cx.note(entry.clone());
                        errors.lock().unwrap().push(entry);
                    }
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    if !errors.is_empty() {
        write_json(&output.join("errors.json"), &json!(errors))?;
        return Err(format!("Incomplete judge results: {}", output.join("errors.json").display()).into());
    }
    let mut reviewed = reviewed.into_inner().unwrap();
    let evaluations_path = output.join("ml-evaluations.json");
    let mut evaluations = if evaluations_path.exists() { read(&evaluations_path)? } else { json!({}) };
    for case in cases.iter().filter(|c| c["benchmark"] == "ml_predictor") {
        let run = case["run_id"].as_str().unwrap_or("");
        if reviewed.get(run).is_none_or(Value::is_null) {
            continue;
        }
        if evaluations.get(run).is_none() {
            let state = api("GET", &format!("/v1/runs/{run}"), admin, Value::Null)?;
            let campaign = api("GET", &format!("/v1/campaigns/{}", case["campaign_id"].as_str().unwrap_or("")), admin, Value::Null)?;
            evaluations[run] = ml_eval::evaluate(&state["run"], &campaign["campaign"]["config"], root, output);
            write_json(&evaluations_path, &evaluations)?;
        }
        reviewed.get_mut(run).unwrap()["model_evaluation"] = evaluations[run].clone();
    }
    let until = Instant::now() + cx.timeout;
    let chess: Vec<&str> = cases.iter().filter(|c| c["benchmark"] == "chess960").filter_map(|c| c["run_id"].as_str()).collect();
    while Instant::now() < until {
        for run in &chess {
            let report = cx.report(run)?;
            if !report.is_null() {
                reviewed.insert(run.to_string(), report);
            }
        }
        if chess.iter().all(|run| reviewed.contains_key(*run)) {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let complete = metric_rows.iter().all(|m| m["tokens"]["complete"] == true);
    let mode = if complete { "measured_tokens" } else { config["incomplete_token_fallback"].as_str().unwrap_or("") };
    let scored = metric_rows.iter().map(|m| metrics::score(m, reviewed.get(m["run_id"].as_str().unwrap_or("")).unwrap_or(&Value::Null), config, mode)).collect::<Result<Vec<_>>>()?;
    let unscored = deferred.iter().map(|case| {
        let (metrics, run) = collect(case)?;
        Ok(json!({"metrics":metrics, "completed_requests":run.get("completed_requests").cloned().unwrap_or(json!([]))}))
    }).collect::<Result<Vec<_>>>()?;
    let implementation: serde_json::Map<String, Value> = IMPLEMENTATION.iter().map(|(name, text)| (name.to_string(), json!(format!("{:x}", Sha256::digest(text))))).collect();
    let report = json!({"schema_version":"benchmark-score-report.v2", "producer":"agent-harness-bench scoring utility",
        "score_generation":{"arithmetic":"deterministic code", "quality_ratings":"independent judge sessions", "aggregation":"deterministic weighted means", "human_or_supervising_agent_edits":false},
        "batch_id":cx.batch, "config":config, "created_at_ms":crate::now_ms(), "cases":scored, "unscored":unscored,
        "scoring_execution":{"judge":cx.judge, "max_judges":max_judges, "timeout_seconds":cx.timeout.as_secs(), "implementation_sha256":implementation},
        "token_scoring_mode":mode, "ranking":metrics::aggregate(&scored, config)?,
        "token_mode_decision":{"rule":"Use measured tokens only when every launch in every case has complete usage; otherwise use the same runtime proxy for all cases.",
            "selected":mode, "incomplete_cases":metric_rows.iter().filter(|m| m["tokens"]["complete"] != true).map(|m| json!({"run_id":m["run_id"], "workspace":m["workspace"],
                "benchmark":m["benchmark"], "measured_launches":m["tokens"]["measured_launches"], "launches":m["tokens"]["launches"]})).collect::<Vec<_>>()},
        "notes":["Only selected final attempts are scored; previous aborted attempts are excluded.",
                 "Historical workspace settings are preserved without reinterpreting model or prompt selections.",
                 "Dataset dashboard assignments differ across workspaces; this is one observation per assignment, not a causal estimate.",
                 "Source lines include comments and vendored source, matching the existing artifact counter."]});
    write_json(&output.join("scores.json"), &report)?;
    Ok(report)
}
