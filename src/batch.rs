use crate::{Result, coordinator, model::*, now_ms, prompts, service::Service};
use serde::Deserialize;
use serde_json::{Value, json};

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    profile: String,
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
    for field in ["profile_config"] {
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

impl Service {
    pub(crate) fn create_batch(&self, body: Value) -> Result<Value> {
        let _guard = self.batch_lock.lock().unwrap();
        let request: Request = serde_json::from_value(body)?;
        if request.max_parallel == 0 || request.workspaces.is_empty() {
            return Err("max_parallel and workspace count must be positive".into());
        }
        let profile = coordinator::profile(&self.root, &request.profile)?;
        let workspace_root = fs::canonicalize(&request.workspace_root)?;
        let mut roots = Vec::new();
        let mut cases = Vec::new();
        for workspace in &request.workspaces {
            let path = fs::canonicalize(&workspace.path)?;
            if !path.is_dir()
                || !path.starts_with(&workspace_root)
                || path == workspace_root
                || !workspace.settings.is_object()
            {
                return Err("each workspace must be a directory below workspace_root with object settings".into());
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
            "profile":request.profile,"profile_config":profile,
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
                let coordinator_url = batch["profile_config"]["coordinator_url"].as_str().ok_or("profile has no coordinator_url")?.to_owned();
                let result = self.start_batch_case(&url, &coordinator_url, &case, &mut batch, index);
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
        coordinator_url: &str,
        case: &Value,
        batch: &mut Value,
        index: usize,
    ) -> Result<String> {
        let directory = case["directory"].as_str().ok_or("missing directory")?;
        let root = case["workspace"].as_str().ok_or("missing workspace")?;
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
        drop(db);
        let output = self.root.join("deliveries").join(&run.id);
        let callback = format!("{url}/v1/runs/{}/coordinator/{}", run.id, crate::service::callback_secret(&self.admin, &run.id));
        match coordinator::start(coordinator_url, root, directory, &prompts::builder(&campaign, &run), &output, &callback) {
            Ok(session) => {
                record["session"] = json!(session);
                record["coordinator_url"] = json!(coordinator_url);
                record["deadline_ms"] = json!(run.deadline_ms);
                self.db().save_workflow(&run.id, &record)?;
                batch["cases"][index]["session"] = json!(session);
                self.workflows.lock().unwrap().insert(run.id.clone(), coordinator::Workflow { url: coordinator_url.to_owned(), session, deadline_ms: run.deadline_ms });
            }
            Err(error) => {
                record["status"] = json!("failed");
                record["error"] = json!(error.to_string());
                self.db().save_workflow(&run.id, &record)?;
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
        let mut sessions = Vec::new();
        for case in batch["cases"].as_array_mut().ok_or("invalid cases")? {
            if case["status"] == "running" {
                let run_id = case["run_id"].as_str().unwrap();
                sessions.extend(workflows.remove(run_id));
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
        drop(workflows);
        for workflow in sessions {
            coordinator::stop(&workflow.url, &workflow.session)?;
        }
        self.close_dispatched_campaigns(&batch)?;
        Ok(public(&batch))
    }
}
