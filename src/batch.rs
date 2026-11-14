use crate::{Result, coordinator, model::*, now_ms, prompts, service::Service};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Workspace {
    path: PathBuf,
    #[serde(default = "empty_object")]
    settings: Value,
}

fn empty_object() -> Value {
    json!({})
}

pub(crate) fn allowed_agents(settings: &Value) -> bool {
    settings["planner"].as_str().is_some_and(|id| crate::agents::allowed(id, crate::agents::family()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    profile: String,
    template_path: PathBuf,
    workspace_root: PathBuf,
    workspaces: Vec<Workspace>,
    #[serde(default = "default_parallel")]
    max_parallel: usize,
    #[serde(default)]
    order_seed: Option<u64>,
    #[serde(default)]
    task_configs: BTreeMap<String, Value>,
    #[serde(default)]
    benchmarks: Option<Vec<String>>,
}

fn default_parallel() -> usize { 8 }

fn shuffle<T>(items: &mut [T], mut seed: u64) {
    for end in (1..items.len()).rev() {
        let selected = crate::random(&mut seed) as usize % (end + 1);
        items.swap(end, selected);
    }
}

pub fn public(batch: &Value) -> Value {
    let mut value = batch.clone();
    for field in ["profile_config", "template"] {
        value.as_object_mut().unwrap().remove(field);
    }
    value
}

const BENCHMARKS: [&str; 5] = ["chess960", "game_build", "dataset_dashboard", "maze_game", "ml_predictor"];

fn ml_python(root: &Path) -> PathBuf {
    let venv = root.parent().unwrap_or(root).join("ml-python");
    if cfg!(windows) { venv.join("Scripts").join("python.exe") } else { venv.join("bin").join("python") }
}

fn assigned<T: serde::de::DeserializeOwned>(case: &Value, field: &str) -> Result<Option<T>> {
    Ok(case.get(field).map(|value| serde_json::from_value(value.clone())).transpose()?)
}

fn plain(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_owned()
}

const LIMITS_FILE: &str = "LIMITS.json";
pub(crate) const WORKSPACE_FILES: [&str; 3] = ["PROJECT_PLANNER.md", "WORKER_PROTOCOL.md", LIMITS_FILE];

fn workspace_documents(path: &Path, settings: &Value) -> Result<Vec<(&'static str, String)>> {
    let mut documents = Vec::new();
    for (role, filename) in [("project_planner", "PROJECT_PLANNER.md"), ("worker_protocol", "WORKER_PROTOCOL.md")] {
        let selection = &settings["prompts"][role];
        let candidate = selection["candidate"].as_str().filter(|name| !name.is_empty()).ok_or("Workspace prompt candidate required")?;
        if candidate.contains(['/', '\\', ':']) || candidate == "." || candidate == ".." {
            return Err("Workspace prompt candidate must be a filename".into());
        }
        let file = path.join(filename);
        if !fs::symlink_metadata(&file)?.is_file() { return Err("Workspace prompt must be a regular file".into()); }
        let text = fs::read_to_string(file)?;
        if !(1..=200).contains(&text.split_whitespace().count()) { return Err("Workspace prompts must contain 1..200 words".into()); }
        if selection["sha256"] != format!("{:x}", Sha256::digest(text.as_bytes())) {
            return Err("Workspace prompt differs from its selected candidate snapshot".into());
        }
        documents.push((filename, text));
    }
    let limits: Value = serde_json::from_slice(&fs::read(path.join(LIMITS_FILE)).map_err(|_| "Workspace LIMITS.json is required")?)?;
    if limits != settings["limits"] {
        return Err("Workspace LIMITS.json differs from its settings".into());
    }
    documents.push((LIMITS_FILE, serde_json::to_string_pretty(&limits)?));
    Ok(documents)
}

impl Service {
    pub(crate) fn create_batch(&self, body: Value) -> Result<Value> {
        let _guard = self.batch_lock.lock().unwrap();
        let request: Request = serde_json::from_value(body)?;
        if request.max_parallel == 0 || request.workspaces.is_empty() {
            return Err("max_parallel and workspace count must be positive".into());
        }
        let mut profile = coordinator::profile(&self.root, &request.profile)?;
        if !profile["coordinator_url"]
            .as_str()
            .is_some_and(|url| url.starts_with("http://127.0.0.1:"))
        {
            return Err("profile requires a shared loopback coordinator_url".into());
        }
        for (field, target, subcommand) in [("session_command", "command", "session"), ("session_planner_command", "planner_command", "planner")] {
            let command = if profile[field].is_null() { json!(coordinator::BUILTIN) } else { profile[field].clone() };
            coordinator::command(&command, subcommand)?;
            profile[target] = command;
        }
        let template = fs::read_to_string(&request.template_path)?;
        if !template.contains("{workspace_name}") || !template.contains("{benchmark_task}") {
            return Err("template requires {workspace_name} and {benchmark_task}".into());
        }
        let workspace_root = fs::canonicalize(&request.workspace_root)?;
        let mut roots = Vec::new();
        let mut cases = Vec::new();
        for workspace in &request.workspaces {
            if !allowed_agents(&workspace.settings) {
                return Err(
                    format!("workspace planner and worker must each select an agent from agents.json allowed for the {} launcher family", crate::agents::family().unwrap_or("undetected")).into(),
                );
            }
            let path = fs::canonicalize(&workspace.path)?;
            if !path.is_dir()
                || !path.starts_with(&workspace_root)
                || path == workspace_root
                || !workspace.settings.is_object()
            {
                return Err("each workspace must be a directory below workspace_root with object settings".into());
            }
            workspace_documents(&path, &workspace.settings)?;
            if fs::read_dir(&path)?.any(|entry| entry.is_err() || !WORKSPACE_FILES.iter().any(|name| entry.as_ref().unwrap().file_name() == *name)) {
                return Err("fresh workspaces may contain only their planner and worker documents and LIMITS.json".into());
            }
            if roots
                .iter()
                .any(|p: &PathBuf| path.starts_with(p) || p.starts_with(&path))
            {
                return Err("workspace paths must be distinct and nonoverlapping".into());
            }
            roots.push(path);
        }
        let db = self.db();
        for batch in db.batches()? {
            for case in batch["cases"].as_array().ok_or("invalid saved batch")? {
                let other = PathBuf::from(case["workspace"].as_str().ok_or("invalid workspace")?);
                if roots.iter().any(|p| plain(p) == plain(&other)) {
                    return Err(
                        "workspace was already benchmarked; create a fresh workspace".into(),
                    );
                }
            }
        }
        let id = crate::id();
        if request.benchmarks.as_ref().is_some_and(|names| names.is_empty() || names.iter().any(|n| !BENCHMARKS.contains(&n.as_str()))) {
            return Err("benchmarks must list supported tasks".into());
        }
        let mut campaigns = BTreeMap::new();
        let mut campaign_records = Vec::new();
        if request
            .task_configs
            .keys()
            .any(|k| !BENCHMARKS.contains(&k.as_str()))
        {
            return Err(format!("supported tasks are {}", BENCHMARKS.join(", ")).into());
        }
        for (name, benchmark) in [
            ("chess960", Benchmark::Chess960),
            ("game_build", Benchmark::GameBuild),
            ("dataset_dashboard", Benchmark::DatasetDashboard),
            ("maze_game", Benchmark::MazeGame),
            ("ml_predictor", Benchmark::MlPredictor),
        ] {
            if request.benchmarks.as_ref().is_some_and(|names| !names.iter().any(|n| n == name)) {
                continue;
            }
            let mut value = request.task_configs.get(name).cloned().unwrap_or(json!({}));
            if benchmark == Benchmark::MlPredictor && value.get("ml_python").is_none() && value.is_object() {
                value["ml_python"] = json!(plain(&ml_python(&self.root)));
            }
            let config = Config::new(benchmark, value)?;
            if benchmark == Benchmark::MlPredictor && !Path::new(&config.ml_python).is_file() {
                return Err(format!("ml_python {} does not exist", config.ml_python).into());
            }
            let campaign = Campaign { id: crate::id(), config, created_at_ms: now_ms(), status: "enrolling".into() };
            campaigns.insert(name, campaign.id.clone());
            for (index, path) in roots.iter().enumerate() {
                let directory = path.join(name);
                let relative = directory.strip_prefix(&workspace_root)?.to_string_lossy();
                let prompt = template
                    .replace("{benchmark_task}", name)
                    .replace("{workspace_name}", &relative);
                if !prompt.contains(&plain(&directory)) {
                    return Err(
                        "filled template must identify the actual benchmark workspace path".into(),
                    );
                }
                let mut case = json!({"workspace":plain(path),"directory":plain(&directory),"workspace_name":relative,
                    "benchmark":name,"settings":request.workspaces[index].settings,"campaign_id":campaign.id,"status":"queued"});
                if benchmark == Benchmark::DatasetDashboard {
                    case["dataset"] = json!(crate::datasets::random_assignment());
                }
                if benchmark == Benchmark::MlPredictor {
                    case["ml"] = json!(crate::ml::random_assignment());
                }
                if matches!(benchmark, Benchmark::GameBuild | Benchmark::MazeGame) {
                    case["inspiration"] = json!(prompts::random_inspiration(benchmark == Benchmark::GameBuild));
                }
                cases.push(case);
            }
            campaign_records.push(campaign);
        }
        let order_seed = request.order_seed.unwrap_or_else(crate::random_seed);
        shuffle(&mut cases, order_seed);
        let batch = json!({"id":id,"status":"running","created_at_ms":now_ms(),"max_parallel":request.max_parallel,
            "order_seed":order_seed,"order_algorithm":"splitmix64-fisher-yates.v1",
            "chess_seed_explicit":request.task_configs.get("chess960").and_then(|config| config.get("seed")).is_some(),
            "profile":request.profile,"profile_config":profile,"template":template,"template_path":request.template_path,
            "campaigns":campaigns,"cases":cases});
        let tx = db.db.unchecked_transaction()?;
        for campaign in campaign_records {
            db.save_campaign(&campaign)?;
        }
        db.save_batch(&batch)?;
        tx.commit()?;
        Ok(public(&batch))
    }

    pub(crate) fn schedule_batches(&self) -> Result<()> {
        let _guard = self.batch_lock.lock().unwrap();
        let Some(url) = self.address.lock().unwrap().clone() else {
            return Ok(());
        };
        let batches = self.db().batches()?;
        for mut batch in batches {
            if batch["status"] != "running" {
                continue;
            }
            let mut changed = false;
            for case in batch["cases"].as_array_mut().ok_or("invalid cases")? {
                if case["status"] == "running" {
                    let run_id = case["run_id"].as_str().ok_or("missing run id")?;
                    let db = self.db();
                    let workflow = db.workflow(run_id)?.ok_or("missing workflow")?;
                    if workflow["status"] != "running" {
                        let mut run = db.run(run_id)?;
                        run.status = "frozen".into();
                        db.save_run(&run)?;
                        case["status"] = workflow["status"].clone();
                        case["error"] = workflow["error"].clone();
                        case["ended_at_ms"] = json!(now_ms());
                        changed = true;
                    }
                }
            }
            let cases = batch["cases"].as_array().unwrap();
            let mut active: HashSet<String> = cases
                .iter()
                .filter(|c| c["status"] == "running")
                .map(|c| c["workspace"].as_str().unwrap().to_owned())
                .collect();
            let cap = batch["max_parallel"].as_u64().ok_or("invalid cap")? as usize;
            for index in 0..cases.len() {
                let case = batch["cases"][index].clone();
                let workspace = case["workspace"].as_str().unwrap();
                if active.len() >= cap {
                    break;
                }
                if case["status"] != "queued" || active.contains(workspace) {
                    continue;
                }
                if !allowed_agents(&case["settings"]) {
                    batch["cases"][index]["status"] = json!("failed");
                    batch["cases"][index]["error"] =
                        json!("missing or unsupported workspace planner");
                    changed = true;
                    continue;
                }
                let mut profile = batch["profile_config"].clone();
                profile["workspace"] = case["settings"].clone();
                profile["workspace_path"] = case["directory"].clone();
                profile["workspace_group"] = case["workspace"].clone();
                profile["template_prompt"] = json!(
                    batch["template"]
                        .as_str()
                        .unwrap()
                        .replace("{benchmark_task}", case["benchmark"].as_str().unwrap())
                        .replace("{workspace_name}", case["workspace_name"].as_str().unwrap())
                );
                let result = self.start_batch_case(&url, &case, &profile, &mut batch, index);
                match result {
                    Ok(run_id) => {
                        batch["cases"][index]["run_id"] = json!(run_id);
                        batch["cases"][index]["status"] = json!("running");
                        batch["cases"][index]["started_at_ms"] = json!(now_ms());
                        active.insert(workspace.into());
                    }
                    Err(error) => {
                        batch["cases"][index]["status"] = json!("failed");
                        batch["cases"][index]["error"] = json!(error.to_string());
                    }
                }
                changed = true;
                self.db().save_batch(&batch)?;
            }
            if !batch["cases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["status"] == "queued" || c["status"] == "running")
            {
                batch["status"] = json!("completed");
                batch["ended_at_ms"] = json!(now_ms());
                changed = true;
            }
            if changed {
                self.db().save_batch(&batch)?;
            }
            self.close_dispatched_campaigns(&batch)?;
        }
        Ok(())
    }

    fn start_batch_case(
        &self,
        url: &str,
        case: &Value,
        profile: &Value,
        batch: &mut Value,
        index: usize,
    ) -> Result<String> {
        let directory = Path::new(case["directory"].as_str().ok_or("missing directory")?);
        let documents = workspace_documents(Path::new(case["workspace"].as_str().ok_or("missing workspace")?), &case["settings"])?;
        fs::create_dir(directory)?;
        for (filename, text) in documents { fs::write(directory.join(filename), text)?; }
        let mut workflows = self.workflows.lock().unwrap();
        let db = self.db();
        let campaign = db.campaign(case["campaign_id"].as_str().ok_or("missing campaign")?)?;
        let mut run = Run::new(&campaign, case["workspace"].as_str().unwrap(), assigned(case, "inspiration")?, assigned(case, "dataset")?);
        run.ml = assigned(case, "ml")?;
        let now = run.started_at_ms;
        let token = crate::id();
        let mut record =
            json!({"run_id":run.id,"status":"running","mode":"template","started_at_ms":now});
        let tx = db.db.unchecked_transaction()?;
        db.insert_run(&run, &token)?;
        db.save_workflow(&run.id, &record)?;
        db.event(Some(&run.id), "started", &run)?;
        batch["cases"][index]["run_id"] = json!(run.id);
        batch["cases"][index]["status"] = json!("running");
        batch["cases"][index]["started_at_ms"] = json!(now);
        db.save_batch(batch)?;
        tx.commit()?;
        match coordinator::launch(&self.root, url, &crate::service::started(&self.admin, &campaign, &run, &token), profile) {
            Ok(child) => {
                workflows.insert(run.id.clone(), child);
            }
            Err(error) => {
                record["status"] = json!("failed");
                record["error"] = json!(error.to_string());
                db.save_workflow(&run.id, &record)?;
            }
        }
        Ok(run.id)
    }

    fn close_dispatched_campaigns(&self, batch: &Value) -> Result<()> {
        let db = self.db();
        for (name, id) in batch["campaigns"].as_object().ok_or("invalid campaigns")? {
            if batch["cases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["benchmark"] == *name && c["status"] == "queued")
            {
                continue;
            }
            let mut campaign = db.campaign(id.as_str().unwrap())?;
            if campaign.status == "enrolling" {
                campaign.status = "awaiting_score".into();
                db.save_campaign(&campaign)?;
            }
        }
        Ok(())
    }

    pub(crate) fn stop_batch(&self, id: &str) -> Result<Value> {
        let _guard = self.batch_lock.lock().unwrap();
        let mut workflows = self.workflows.lock().unwrap();
        let db = self.db();
        let mut batch = db.batch(id)?;
        for case in batch["cases"].as_array_mut().ok_or("invalid cases")? {
            if case["status"] == "running" {
                let run_id = case["run_id"].as_str().unwrap();
                if let Some(workflow) = workflows.remove(run_id) {
                    workflow.child.tree.stop();
                }
                coordinator::stop_workspace_tasks(&self.root, run_id)?;
                let mut record = db.workflow(run_id)?.ok_or("missing workflow")?;
                record["status"] = json!("stopped");
                record["ended_at_ms"] = json!(now_ms());
                db.save_workflow(run_id, &record)?;
                let mut run = db.run(run_id)?;
                run.status = "frozen".into();
                db.save_run(&run)?;
            }
            if case["status"] == "queued" || case["status"] == "running" {
                case["status"] = json!("stopped");
            }
        }
        batch["status"] = json!("stopped");
        batch["ended_at_ms"] = json!(now_ms());
        db.save_batch(&batch)?;
        drop(db);
        self.close_dispatched_campaigns(&batch)?;
        Ok(public(&batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_check_only_the_planner_agent_they_launch() {
        for planner in ["sol_6_1_light", "sol_6_1_medium"] {
            assert!(allowed_agents(&json!({"planner":planner,"worker":"anything the coordinator accepts"})));
            assert!(allowed_agents(&json!({"planner":planner})));
        }
        for invalid in [Value::Null, json!("sol_light"), json!("luna_6_high"), json!("claude_haiku_5_5_low")] {
            assert!(!allowed_agents(&json!({"planner":invalid,"worker":"sol_6_1_light"})));
        }
    }

    #[test]
    fn shuffle_is_reproducible_and_preserves_all_workspace_benchmark_pairs() {
        let original: Vec<_> = (0..3).flat_map(|benchmark| (0..16).map(move |workspace| (benchmark, workspace))).collect();
        let mut first = original.clone();
        let mut second = original.clone();
        shuffle(&mut first, 960);
        shuffle(&mut second, 960);
        assert_eq!(first, second);
        assert_ne!(first, original);
        assert!(first[..8].iter().map(|pair| pair.0).collect::<HashSet<_>>().len() > 1);
        shuffle(&mut second, 961);
        assert_ne!(first, second);
        first.sort();
        assert_eq!(first, original);
    }

    #[test]
    fn default_cap_and_shuffled_queue_are_persisted_before_launch() {
        let root = crate::test_support::dir();
        let service = Service::open(root.path()).unwrap();
        fs::write(root.path().join("coordinator-profiles.json"), json!({"fixture":{
            
            "session_command":["unused"],"session_planner_command":["unused"],"coordinator_url":"http://127.0.0.1:1"}}).to_string()).unwrap();
        let workspaces = root.path().join("workspaces");
        fs::create_dir(&workspaces).unwrap();
        let mut selected = Vec::new();
        for index in 0..16 {
            let path = workspaces.join(index.to_string());
            fs::create_dir(&path).unwrap();
            let mut settings = json!({"planner":"sol_6_1_light","worker":"sol_6_1_light","prompts":{},"limits":{"criteria":3}});
            fs::write(path.join(LIMITS_FILE), r#"{"criteria":3}"#).unwrap();
            for (role, filename) in [("project_planner","PROJECT_PLANNER.md"),("worker_protocol","WORKER_PROTOCOL.md")] {
                fs::write(path.join(filename), "Selected fixture instructions.").unwrap();
                settings["prompts"][role] = json!({"candidate":"fixture.md","sha256":format!("{:x}",Sha256::digest(b"Selected fixture instructions."))});
            }
            selected.push(json!({"path":path,"settings":settings}));
        }
        let template = root.path().join("template.txt");
        fs::write(&template, format!("Task {{benchmark_task}} Workspace: {}",workspaces.join("{workspace_name}").display())).unwrap();
        let batch = service.create_batch(json!({"profile":"fixture","template_path":template,"workspace_root":workspaces,
            "workspaces":selected,"order_seed":960,"task_configs":{"maze_game":{"follow_ups":["Second request."]},"ml_predictor":{"ml_python":std::env::current_exe().unwrap()}}})).unwrap();
        assert_eq!(batch["max_parallel"],8);
        assert_eq!(batch["order_seed"],960);
        assert_eq!(batch["cases"].as_array().unwrap().len(),80);
        let ml: Vec<_> = batch["cases"].as_array().unwrap().iter().filter(|case| case["benchmark"] == "ml_predictor").collect();
        assert_eq!(ml.len(), 16);
        assert!(ml.iter().all(|case| case["ml"]["task"]["train"].as_array().is_some_and(|urls| !urls.is_empty())));
        assert!(batch["cases"].as_array().unwrap().iter().all(|case| case["status"] == "queued"));
        for (benchmark, field) in [("game_build", "inspiration"), ("dataset_dashboard", "dataset")] {
            let assigned: Vec<_> = batch["cases"].as_array().unwrap().iter().filter(|case| case["benchmark"] == benchmark).collect();
            assert_eq!(assigned.len(), 16);
            assert_eq!(assigned.iter().map(|case| case[field]["seed"].as_u64().unwrap()).collect::<HashSet<_>>().len(), 16);
        }
        let id = batch["id"].as_str().unwrap().to_owned();
        drop(service);
        let reopened = Service::open(root.path()).unwrap();
        assert_eq!(public(&reopened.db().batch(&id).unwrap()), batch);
        let follow_ups = root.path().join("maze-follow-ups.json");
        fs::write(&follow_ups, json!(["Second request.", "Third request."]).to_string()).unwrap();
        let only = workspaces.join("maze-only");
        fs::create_dir(&only).unwrap();
        let mut single = selected[0].clone();
        for filename in ["PROJECT_PLANNER.md", "WORKER_PROTOCOL.md"] { fs::write(only.join(filename), "Selected fixture instructions.").unwrap(); }
        fs::write(only.join(LIMITS_FILE), r#"{"criteria":3}"#).unwrap();
        single["path"] = json!(only);
        let maze = reopened.create_batch(json!({"profile":"fixture","template_path":template,"workspace_root":workspaces,
            "workspaces":[single],"benchmarks":["maze_game"],"task_configs":{"maze_game":{"follow_ups_file":follow_ups}}})).unwrap();
        assert_eq!(maze["cases"].as_array().unwrap().len(),1);
        assert_eq!(maze["cases"][0]["benchmark"],"maze_game");
        let campaign = reopened.db().campaign(maze["campaigns"]["maze_game"].as_str().unwrap()).unwrap();
        assert_eq!((campaign.config.development_seconds, campaign.config.judge_seconds, campaign.config.follow_ups.len()), (1800, 600, 2));
        let missing = workspaces.join("maze-missing");
        fs::create_dir(&missing).unwrap();
        for filename in ["PROJECT_PLANNER.md", "WORKER_PROTOCOL.md"] { fs::write(missing.join(filename), "Selected fixture instructions.").unwrap(); }
        fs::write(missing.join(LIMITS_FILE), r#"{"criteria":3}"#).unwrap();
        let mut unconfigured = selected[0].clone();
        unconfigured["path"] = json!(missing);
        assert!(reopened.create_batch(json!({"profile":"fixture","template_path":template,"workspace_root":workspaces,
            "workspaces":[unconfigured],"benchmarks":["maze_game"]})).unwrap_err().to_string().contains("follow_ups"));
    }
}
