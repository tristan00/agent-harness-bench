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
    let family = crate::agents::family();
    let listed = settings.get("enabled_agents").map_or(Some(true), |list| list.as_array().map(|items| items.iter().all(|id| id.as_str().is_some_and(|id| crate::agents::allowed(id, family)))));
    listed == Some(true) && ["planner", "worker"].iter().all(|role| settings[role].as_str().is_some_and(|id| crate::agents::allowed(id, family)))
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

fn plain(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_owned()
}

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
    Ok(documents)
}

impl Service {
    pub(crate) fn retry_benchmark(&self, id: &str, name: &str) -> Result<Value> {
        let _guard = self.batch_lock.lock().unwrap();
        let db = self.db();
        let mut batch = db.batch(id)?;
        let campaign_id = batch["campaigns"][name]
            .as_str()
            .ok_or("benchmark not in batch")?;
        let mut campaign = db.campaign(campaign_id)?;
        let cases = batch["cases"].as_array().ok_or("invalid cases")?;
        let selected: Vec<_> = cases
            .iter()
            .filter(|case| case["benchmark"] == name)
            .cloned()
            .collect();
        if selected.is_empty()
            || selected
                .iter()
                .any(|case| case["status"] == "running" || case["status"] == "queued")
        {
            return Err("retry requires finished or stopped cases".into());
        }
        let profile_name = batch["profile"].as_str().ok_or("missing profile")?;
        let profile = coordinator::profile(&self.root, profile_name)?;
        let coordinator_url = profile["coordinator_url"]
            .as_str()
            .ok_or("shared coordinator_url required")?;
        let previous_campaign = campaign.id.clone();
        campaign.id = crate::id();
        campaign.status = "enrolling".into();
        campaign.created_at_ms = now_ms();
        campaign.enrollment_deadline_ms = i64::MAX;
        for case in batch["cases"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|case| case["benchmark"] == name)
        {
            let folder = format!("{name}_{}", &campaign.id[..8]);
            let directory = Path::new(case["workspace"].as_str().unwrap()).join(&folder);
            let relative = Path::new(case["workspace_name"].as_str().unwrap())
                .parent()
                .ok_or("invalid workspace name")?
                .join(folder);
            if directory.exists() {
                return Err("retry directory already exists".into());
            }
            workspace_documents(Path::new(case["workspace"].as_str().unwrap()), &case["settings"])?;
            if !allowed_agents(&case["settings"]) { return Err("Retry requires eligible explicit workspace agent selections".into()); }
            case["directory"] = json!(plain(&directory));
            case["workspace_name"] = json!(relative);
            case["campaign_id"] = json!(campaign.id);
            case["status"] = json!("queued");
            for field in ["run_id", "error", "started_at_ms", "ended_at_ms"] {
                case.as_object_mut().unwrap().remove(field);
            }
        }
        if batch.get("previous_cases").is_none() {
            batch["previous_cases"] = json!([]);
        }
        batch["previous_cases"]
            .as_array_mut()
            .unwrap()
            .extend(selected);
        if batch.get("previous_campaigns").is_none() {
            batch["previous_campaigns"] = json!([]);
        }
        batch["previous_campaigns"]
            .as_array_mut()
            .unwrap()
            .push(json!(previous_campaign));
        batch["campaigns"][name] = json!(campaign.id);
        batch["profile_config"]["coordinator_url"] = json!(coordinator_url);
        batch["status"] = json!("running");
        batch.as_object_mut().unwrap().remove("ended_at_ms");
        let tx = db.db.unchecked_transaction()?;
        db.save_campaign(&campaign)?;
        db.save_batch(&batch)?;
        tx.commit()?;
        Ok(public(&batch))
    }

    pub(crate) fn append_dashboard(&self, id: &str, mut config: Value) -> Result<Value> {
        let _guard = self.batch_lock.lock().unwrap();
        let db = self.db();
        let mut batch = db.batch(id)?;
        if batch["status"] == "stopped" {
            return Err("cannot extend a stopped batch".into());
        }
        if batch["campaigns"].get("dataset_dashboard").is_some() {
            return Ok(public(&batch));
        }
        if !config.is_object() {
            return Err("dashboard configuration must be an object".into());
        }
        config["benchmark"] = json!(Benchmark::DatasetDashboard);
        if config.get("development_seconds").is_none() {
            config["development_seconds"] = json!(900);
        }
        let config: Config = serde_json::from_value(config)?;
        config.validate()?;
        let campaign = Campaign {
            id: crate::id(),
            config,
            themes: vec![],
            created_at_ms: now_ms(),
            enrollment_deadline_ms: i64::MAX,
            status: "enrolling".into(),
        };
        let mut workspaces = HashSet::new();
        let mut added = Vec::new();
        for case in batch["cases"].as_array().ok_or("invalid cases")? {
            let workspace = case["workspace"].as_str().ok_or("missing workspace")?;
            if !workspaces.insert(workspace.to_owned()) {
                continue;
            }
            if !allowed_agents(&case["settings"]) {
                return Err("missing or unsupported workspace planner".into());
            }
            workspace_documents(Path::new(workspace), &case["settings"])?;
            let directory = Path::new(workspace).join("dataset_dashboard");
            if directory.exists() {
                return Err("dashboard directory already exists".into());
            }
            let relative = Path::new(
                case["workspace_name"]
                    .as_str()
                    .ok_or("missing workspace name")?,
            )
            .parent()
            .ok_or("missing workspace parent")?
            .join("dataset_dashboard");
            let prompt = batch["template"]
                .as_str()
                .ok_or("missing template")?
                .replace("{benchmark_task}", "dataset_dashboard")
                .replace("{workspace_name}", &relative.to_string_lossy());
            if !prompt.contains(&plain(&directory)) {
                return Err("template workspace path mismatch".into());
            }
            added.push(json!({"benchmark":"dataset_dashboard","campaign_id":campaign.id,"workspace":workspace,
                "directory":plain(&directory),"workspace_name":relative,"settings":case["settings"],
                "status":"queued","dataset":crate::datasets::random_assignment()}));
        }
        batch["cases"].as_array_mut().unwrap().extend(added);
        batch["campaigns"]["dataset_dashboard"] = json!(campaign.id);
        batch["status"] = json!("running");
        batch.as_object_mut().unwrap().remove("ended_at_ms");
        let tx = db.db.unchecked_transaction()?;
        db.save_campaign(&campaign)?;
        db.save_batch(&batch)?;
        tx.commit()?;
        Ok(public(&batch))
    }

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
        coordinator::argv(&profile["session_command"])?;
        coordinator::argv(&profile["session_planner_command"])?;
        profile["command"] = profile["session_command"].clone();
        profile["planner_command"] = profile["session_planner_command"].clone();
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
            if fs::read_dir(&path)?.any(|entry| entry.is_err() || !["PROJECT_PLANNER.md", "WORKER_PROTOCOL.md"].iter().any(|name| entry.as_ref().unwrap().file_name() == *name)) {
                return Err("fresh workspaces may contain only their selected planner and worker documents".into());
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
        if request.benchmarks.as_ref().is_some_and(|names| names.is_empty() || names.iter().any(|n| !["chess960", "game_build", "dataset_dashboard", "maze_sequence"].contains(&n.as_str()))) {
            return Err("benchmarks must list supported tasks".into());
        }
        let mut campaigns = BTreeMap::new();
        let mut campaign_records = Vec::new();
        if request
            .task_configs
            .keys()
            .any(|k| !["chess960", "game_build", "dataset_dashboard", "maze_sequence"].contains(&k.as_str()))
        {
            return Err("supported tasks are chess960, game_build, dataset_dashboard and maze_sequence".into());
        }
        for (name, benchmark, seconds) in [
            ("chess960", Benchmark::Chess960, 600),
            ("game_build", Benchmark::GameBuild, 1800),
            ("dataset_dashboard", Benchmark::DatasetDashboard, 900),
            ("maze_sequence", Benchmark::MazeSequence, 5400),
        ] {
            if request.benchmarks.as_ref().is_some_and(|names| !names.iter().any(|n| n == name)) {
                continue;
            }
            let mut config = request.task_configs.get(name).cloned().unwrap_or(json!({}));
            if !config.is_object() {
                return Err("task configuration must be an object".into());
            }
            config["benchmark"] = json!(benchmark);
            if config.get("development_seconds").is_none() {
                config["development_seconds"] = json!(seconds);
            }
            let config: Config = serde_json::from_value(config)?;
            config.validate()?;
            if !config.pool_run_ids.is_empty() {
                return Err("batch scoring pools are configured later".into());
            }
            let campaign = Campaign {
                id: crate::id(),
                themes: if benchmark == Benchmark::GameBuild {
                    prompts::themes(config.seed)
                } else {
                    vec![]
                },
                config,
                created_at_ms: now_ms(),
                enrollment_deadline_ms: i64::MAX,
                status: "enrolling".into(),
            };
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
                cases.push(case);
            }
            campaign_records.push(campaign);
        }
        let order_seed = request.order_seed.unwrap_or_else(|| u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()));
        shuffle(&mut cases, order_seed);
        let batch = json!({"id":id,"status":"running","created_at_ms":now_ms(),"max_parallel":request.max_parallel,
            "order_seed":order_seed,"order_algorithm":"splitmix64-fisher-yates.v1",
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
        let now = now_ms();
        let run = Run {
            dataset: case
                .get("dataset")
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()?,
            id: crate::id(),
            campaign_id: campaign.id.clone(),
            agent: case["workspace"].as_str().unwrap().into(),
            status: "developing".into(),
            started_at_ms: now,
            deadline_ms: now + campaign.config.development_seconds as i64 * 1000,
            submission_id: None,
            prepared_dir: None,
            error: None,
        };
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
        let started = json!({"run":run,"run_token":token,"prompt":prompts::builder_for_run(&campaign, &run),"config":campaign.config});
        match coordinator::launch(&self.root, url, &started, profile) {
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
                campaign.enrollment_deadline_ms = now_ms();
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
                coordinator::stop_shared(&self.root, run_id)?;
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
    fn batch_agents_must_be_catalogued_and_allowed_for_the_launcher_family() {
        for planner in ["luna_6_high", "sol_6_1_light", "sol_6_1_medium"] {
            for worker in ["luna_6_high", "sol_6_1_light", "sol_6_1_medium"] {
                assert!(allowed_agents(&json!({"planner":planner,"worker":worker})));
            }
        }
        assert!(allowed_agents(&json!({"planner":"local_qwen3_4b","worker":"sol_6_1_light"})));
        assert!(allowed_agents(&json!({"planner":"sol_6_1_light","worker":"sol_6_1_light","enabled_agents":["local_granite_3b","sol_6_1_medium"]})));
        assert!(!allowed_agents(&json!({"planner":"sol_6_1_light","worker":"sol_6_1_light","enabled_agents":["claude_haiku_5_5_low"]})));
        assert!(!allowed_agents(&json!({"planner":"sol_6_1_light","worker":"sol_6_1_light","enabled_agents":"local_granite_3b"})));
        for invalid in [Value::Null, json!("sol_light"), json!("astra_xhigh"), json!("claude_haiku_5_5_low")] {
            assert!(!allowed_agents(&json!({"planner":invalid,"worker":"sol_6_1_light"})));
            assert!(!allowed_agents(&json!({"planner":"sol_6_1_light","worker":invalid})));
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
            "command":["unused"],"coordinator_command":["unused"],"planner_command":["unused"],
            "session_command":["unused"],"session_planner_command":["unused"],"coordinator_url":"http://127.0.0.1:1",
            "workers":{"implementation":["unused"],"review":["unused"],"replan":["unused"]}}}).to_string()).unwrap();
        let workspaces = root.path().join("workspaces");
        fs::create_dir(&workspaces).unwrap();
        let mut selected = Vec::new();
        for index in 0..16 {
            let path = workspaces.join(index.to_string());
            fs::create_dir(&path).unwrap();
            let mut settings = json!({"planner":"sol_6_1_light","worker":"sol_6_1_light","prompts":{}});
            for (role, filename) in [("project_planner","PROJECT_PLANNER.md"),("worker_protocol","WORKER_PROTOCOL.md")] {
                fs::write(path.join(filename), "Selected fixture instructions.").unwrap();
                settings["prompts"][role] = json!({"candidate":"fixture.md","sha256":format!("{:x}",Sha256::digest(b"Selected fixture instructions."))});
            }
            selected.push(json!({"path":path,"settings":settings}));
        }
        let template = root.path().join("template.txt");
        fs::write(&template, format!("Task {{benchmark_task}} Workspace: {}",workspaces.join("{workspace_name}").display())).unwrap();
        let batch = service.create_batch(json!({"profile":"fixture","template_path":template,"workspace_root":workspaces,
            "workspaces":selected,"order_seed":960})).unwrap();
        assert_eq!(batch["max_parallel"],8);
        assert_eq!(batch["order_seed"],960);
        assert_eq!(batch["cases"].as_array().unwrap().len(),64);
        assert!(batch["cases"].as_array().unwrap().iter().all(|case| case["status"] == "queued"));
        let id = batch["id"].as_str().unwrap().to_owned();
        drop(service);
        let reopened = Service::open(root.path()).unwrap();
        assert_eq!(public(&reopened.db().batch(&id).unwrap()), batch);
        let only = workspaces.join("maze-only");
        fs::create_dir(&only).unwrap();
        let mut single = selected[0].clone();
        for filename in ["PROJECT_PLANNER.md", "WORKER_PROTOCOL.md"] { fs::write(only.join(filename), "Selected fixture instructions.").unwrap(); }
        single["path"] = json!(only);
        let maze = reopened.create_batch(json!({"profile":"fixture","template_path":template,"workspace_root":workspaces,
            "workspaces":[single],"benchmarks":["maze_sequence"]})).unwrap();
        assert_eq!(maze["cases"].as_array().unwrap().len(),1);
        assert_eq!(maze["cases"][0]["benchmark"],"maze_sequence");
    }
}
