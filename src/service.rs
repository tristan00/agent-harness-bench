use crate::{
    Result, artifacts, chess, coordinator,
    model::*,
    now_ms,
    process::{self, Protocol, Tree},
    prompts,
    storage::Store,
};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Read,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Header, Request, Response, Server, StatusCode};

fn completed_requests(db: &Store, run: &str) -> Result<Vec<Value>> {
    let mut completed: Vec<Value> = db.events(run)?.into_iter().filter(|e| e["kind"] == "request_completed").map(|e| e["data"].clone()).collect();
    completed.reverse();
    Ok(completed)
}

pub(crate) fn callback_secret(admin: &str, run: &str) -> String {
    crate::storage::hash(&format!("{admin}:coordinator-callback:{run}"))
}

struct Session {
    protocol: Arc<Mutex<Protocol>>,
    tree: Arc<Tree>,
    deadline: Instant,
}
pub struct Service {
    pub root: PathBuf,
    pub store: Mutex<Store>,
    pub(crate) admin: String,
    judge_key: String,
    sessions: Mutex<HashMap<String, Session>>,
    pub(crate) workflows: Mutex<HashMap<String, coordinator::Workflow>>,
    pub(crate) address: Mutex<Option<String>>,
    pub(crate) batch_lock: Mutex<()>,
    scoring_lock: Mutex<()>,
    pub stopping: AtomicBool,
    _lock: File,
}

#[derive(Debug)]
struct ApiError(u16, String);
type ApiResult<T> = std::result::Result<T, ApiError>;
impl From<crate::Error> for ApiError {
    fn from(e: crate::Error) -> Self {
        Self(400, e.to_string())
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self(500, e.to_string())
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self(400, e.to_string())
    }
}

fn secret(root: &Path, name: &str, env: &str) -> Result<String> {
    if let Ok(value) = std::env::var(env) {
        if value.len() < 24 {
            return Err(format!("{env} must be at least 24 characters").into());
        }
        return Ok(value);
    }
    let path = root.join(name);
    if path.is_file() {
        return Ok(fs::read_to_string(path)?.trim().into());
    }
    let value = format!("{}{}", crate::id(), crate::id());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(path)?.write_all(value.as_bytes())?;
    Ok(value)
}
impl Service {
    pub fn open(root: &Path) -> Result<Arc<Self>> {
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("service.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| "another evaluator already owns this data directory")?;
        let store = Store::open(&root.join("benchmark.sqlite3"))?;
        let service = Arc::new(Self {
            admin: secret(&root, "admin-token", "HARNESS_API_TOKEN")?,
            judge_key: secret(&root, "judge-token", "HARNESS_JUDGE_TOKEN")?,
            store: Mutex::new(store),
            root,
            sessions: Mutex::new(HashMap::new()),
            workflows: Mutex::new(HashMap::new()),
            batch_lock: Mutex::new(()),
            scoring_lock: Mutex::new(()),
            address: Mutex::new(None),
            stopping: AtomicBool::new(false),
            _lock: lock,
        });
        service.recover()?;
        Ok(service)
    }
    pub(crate) fn wipe(&self) -> Result<Value> {
        let _batches = self.batch_lock.lock().unwrap();
        let open: Vec<coordinator::Workflow> = self.workflows.lock().unwrap().drain().map(|(_, workflow)| workflow).collect();
        let mut errors = Vec::new();
        let stopped = open.len();
        for workflow in open {
            if let Err(error) = coordinator::stop(&workflow.url, &workflow.session) {
                errors.push(error.to_string());
            }
        }
        for (_, session) in self.sessions.lock().unwrap().drain() {
            session.tree.stop();
        }
        let mut deleted = serde_json::Map::new();
        {
            let db = self.db();
            let tx = db.db.unchecked_transaction()?;
            for table in ["workflows", "events", "games", "reports", "judgments", "submissions", "runs", "campaigns", "batches"] {
                deleted.insert(table.into(), json!(tx.execute(&format!("DELETE FROM {table}"), [])?));
            }
            tx.commit()?;
        }
        let mut removed = Vec::new();
        for name in ["submissions", "work", "deliveries", "batch-scores"] {
            let path = self.root.join(name);
            if path.exists() {
                match fs::remove_dir_all(&path) {
                    Ok(()) => removed.push(name),
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }
        }
        Ok(json!({"deleted":deleted,"stopped_planners":stopped,"removed":removed,"errors":errors}))
    }
    pub(crate) fn accept(&self, id: &str, submission: Submission) -> Result<Artifact> {
        let open = |run: &Run| run.status == "developing" && now_ms() < run.deadline_ms;
        if !open(&self.db().run(id)?) {
            return Err("development deadline passed".into());
        }
        let artifact = artifacts::save(&self.root, submission)?;
        let db = self.db();
        let mut run = db.run(id)?;
        if !open(&run) {
            return Err("deadline passed before the delivery was accepted".into());
        }
        let tx = db.db.unchecked_transaction()?;
        db.save_artifact(id, &artifact)?;
        run.submission_id = Some(artifact.id.clone());
        db.save_run(&run)?;
        db.event(Some(id), "submitted", &artifact)?;
        tx.commit()?;
        Ok(artifact)
    }
    fn coordinator_event(&self, id: &str, body: &Value) -> Result<Value> {
        let Some(workflow) = self.workflows.lock().unwrap().get(id).cloned() else {
            return Ok(json!({"ignored":"case is no longer running"}));
        };
        if body["session"].as_str() != Some(workflow.session.as_str()) {
            return Ok(json!({"ignored":"unknown session"}));
        }
        let kind = body["event"].as_str().unwrap_or_default();
        let key = format!("{kind}:{}:{}", body["request"], body["delivery"]);
        let mut record = self.db().workflow(id)?.ok_or("missing workflow")?;
        if record["handled"].as_array().is_some_and(|seen| seen.iter().any(|k| k == &json!(key))) {
            return Ok(json!({"duplicate":true}));
        }
        let mut accepted = None;
        if let Some(path) = body["path"].as_str() {
            match artifacts::snapshot(Path::new(path)).and_then(|submission| self.accept(id, submission)) {
                Ok(artifact) => accepted = Some(artifact.id),
                Err(error) => {
                    self.db().event(Some(id), "delivery_rejected", &json!({"delivery":body["delivery"],"error":error.to_string()}))?;
                }
            }
        }
        if !record["handled"].is_array() {
            record["handled"] = json!([]);
        }
        record["handled"].as_array_mut().unwrap().push(json!(key));
        if kind == "done" {
            let db = self.db();
            let run = db.run(id)?;
            let campaign = db.campaign(&run.campaign_id)?;
            let open = run.status == "developing" && now_ms() < run.deadline_ms;
            if campaign.config.benchmark == Benchmark::MazeGame && open {
                let requests = prompts::maze_requests(&campaign);
                let current = completed_requests(&db, id)?.len() + 1;
                let stalls = record["stalls"].as_u64().unwrap_or(0);
                let next = if accepted.is_some() {
                    db.event(Some(id), "request_completed", &json!({"request":current,"submission_id":run.submission_id,"completed_at_ms":now_ms(),"elapsed_ms":now_ms() - run.started_at_ms}))?;
                    record["stalls"] = json!(0);
                    requests.get(current).map(|text| prompts::maze_follow_up(text))
                } else if stalls + 1 < 3 {
                    record["stalls"] = json!(stalls + 1);
                    requests.get(current - 1).map(|text| prompts::maze_continue(text))
                } else {
                    None
                };
                if let Some(text) = next {
                    record["pending_message"] = json!(text);
                    db.save_workflow(id, &record)?;
                    return Ok(json!({"next":true}));
                }
            }
            record["status"] = json!(if body["status"] == "completed" { "completed" } else { "failed" });
            record["error"] = body["error"].clone();
            record["ended_at_ms"] = json!(now_ms());
            db.save_workflow(id, &record)?;
            db.event(Some(id), "coordinator_stopped", &record)?;
            drop(db);
            self.workflows.lock().unwrap().remove(id);
            return Ok(json!({"finished":true}));
        }
        self.db().save_workflow(id, &record)?;
        Ok(json!({"accepted":accepted}))
    }
    pub(crate) fn db(&self) -> MutexGuard<'_, Store> {
        self.store.lock().expect("database mutex poisoned")
    }
    fn recover(&self) -> Result<()> {
        let db = self.db();
        for mut workflow in db.workflows()? {
            if workflow["status"] == "running" {
                let id = workflow["run_id"]
                    .as_str()
                    .ok_or("invalid workflow")?
                    .to_owned();
                if let Some(resumed) = coordinator::Workflow::from_record(&workflow) {
                    self.workflows.lock().unwrap().insert(id, resumed);
                    continue;
                }
                workflow["status"] = json!("interrupted");
                workflow["error"] = json!("Service restarted without a coordinator session record");
                db.save_workflow(&id, &workflow)?;
                db.event(Some(&id), "coordinator_interrupted", &workflow)?;
            }
        }
        for mut run in db.all_runs()? {
            if run.status == "building" {
                run.status = "frozen".into();
                run.prepared_dir = None;
                db.save_run(&run)?;
            }
            if run.status == "queued" && !db.judgments()?.iter().any(|j| j.run_id == run.id) {
                run.status = "frozen".into();
                db.save_run(&run)?;
            }
        }
        for mut j in db.judgments()? {
            if ["starting", "exploring", "scoring"].contains(&j.status.as_str()) {
                j.status = "interrupted".into();
                db.save_judgment(&j)?;
                let mut run = db.run(&j.run_id)?;
                run.status = "failed".into();
                run.error =
                    Some("evaluator restarted during judge session; no score fabricated".into());
                db.save_run(&run)?;
                db.event(Some(&run.id), "judge_interrupted", &j)?;
            }
        }
        Ok(())
    }
    pub fn background(self: &Arc<Self>) -> Vec<thread::JoinHandle<()>> {
        let timers = self.clone();
        let judged = self.clone();
        let tournaments = self.clone();
        let evaluator = |service: Arc<Self>, tournaments: bool| {
            thread::spawn(move || {
                while !service.stopping.load(Ordering::Relaxed) {
                    if let Err(e) = service.evaluate(tournaments) {
                        eprintln!("evaluation worker: {e}");
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            })
        };
        vec![
            thread::spawn(move || {
                while !timers.stopping.load(Ordering::Relaxed) {
                    if let Err(e) = timers.tick() {
                        eprintln!("scheduler: {e}");
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }),
            evaluator(judged, false),
            evaluator(tournaments, true),
        ]
    }
    pub fn tick(&self) -> Result<()> {
        let now = now_ms();
        let mut finished = Vec::new();
        let (expired, open): (Vec<(String, coordinator::Workflow)>, Vec<(String, coordinator::Workflow)>) = {
            let mut workflows = self.workflows.lock().unwrap();
            let due: Vec<String> = workflows.iter().filter(|(_, workflow)| now >= workflow.deadline_ms).map(|(id, _)| id.clone()).collect();
            let expired = due.into_iter().filter_map(|id| workflows.remove(&id).map(|workflow| (id, workflow))).collect();
            (expired, workflows.iter().map(|(id, workflow)| (id.clone(), workflow.clone())).collect())
        };
        for (id, workflow) in expired {
            if let Err(error) = coordinator::stop(&workflow.url, &workflow.session) {
                eprintln!("{error}");
            }
            finished.push((id, "deadline", None::<i32>));
        }
        for (id, workflow) in open {
            let pending = self.db().workflow(&id)?.and_then(|record| record["pending_message"].as_str().map(str::to_owned));
            if let Some(text) = pending {
                match coordinator::message(&workflow.url, &workflow.session, &text) {
                    Ok(()) => {
                        let db = self.db();
                        if let Some(mut record) = db.workflow(&id)? {
                            record["pending_message"] = Value::Null;
                            db.save_workflow(&id, &record)?;
                        }
                    }
                    Err(error) => eprintln!("{error}"),
                }
            }
        }
        // Kill without acquiring the protocol mutex: a hung action cannot postpone termination.
        let stopped = {
            let mut sessions = self.sessions.lock().unwrap();
            let due: Vec<_> = sessions
                .iter()
                .filter(|(_, s)| Instant::now() >= s.deadline)
                .map(|(id, _)| id.clone())
                .collect();
            due.into_iter()
                .filter_map(|id| {
                    sessions.remove(&id).map(|s| {
                        let usage = s.tree.usage();
                        s.tree.stop();
                        (id, usage)
                    })
                })
                .collect::<Vec<_>>()
        };
        let db = self.db();
        for (id, status, code) in finished {
            if let Some(mut workflow) = db.workflow(&id)? {
                workflow["status"] = json!(status);
                workflow["exit_code"] = json!(code);
                workflow["ended_at_ms"] = json!(now);
                db.save_workflow(&id, &workflow)?;
                db.event(Some(&id), "coordinator_stopped", &workflow)?;
            }
        }
        for mut run in db.all_runs()? {
            if run.status == "developing" && now >= run.deadline_ms {
                run.status = "frozen".into();
                db.save_run(&run)?;
                db.event(
                    Some(&run.id),
                    "frozen",
                    &json!({"deadline_ms":run.deadline_ms,"submission_id":run.submission_id}),
                )?;
            }
        }
        for mut c in db.campaigns()? {
            if c.config.benchmark != Benchmark::Chess960 && c.status == "scoring_requested" {
                let runs = db.runs(&c.id)?;
                if runs
                    .iter()
                    .all(|r| ["scored", "failed"].contains(&r.status.as_str()))
                {
                    c.status = "completed".into();
                    db.save_campaign(&c)?;
                }
            }
        }
        for mut j in db.judgments()? {
            if j.status == "exploring"
                && (now >= j.explore_deadline_ms.unwrap()
                    || stopped.iter().any(|(id, _)| id == &j.id))
            {
                // The monotonic deadline normally fired above. Also enforce wall-clock expiry.
                if let Some(session) = self.sessions.lock().unwrap().remove(&j.id) {
                    let (memory, cpu) = session.tree.usage();
                    j.peak_memory_bytes = j.peak_memory_bytes.max(memory);
                    j.cpu_time_ms = cpu;
                    session.tree.stop();
                }
                if let Some((_, (memory, cpu))) = stopped.iter().find(|(id, _)| id == &j.id) {
                    j.peak_memory_bytes = j.peak_memory_bytes.max(*memory);
                    j.cpu_time_ms = *cpu;
                }
                j.status = "scoring".into();
                db.save_judgment(&j)?;
                db.event(Some(&j.run_id), "exploration_stopped", &j)?;
            } else if j.status == "scoring" && now >= j.score_deadline_ms.unwrap() {
                j.status = "timed_out".into();
                db.save_judgment(&j)?;
                let mut run = db.run(&j.run_id)?;
                run.status = "failed".into();
                run.error = Some("judge did not submit scores before score deadline".into());
                db.save_run(&run)?;
                db.event(Some(&run.id), "judge_timeout", &j)?;
            }
        }
        drop(db);
        self.schedule_batches()?;
        Ok(())
    }
    fn prepare(&self, mut run: Run, config: &Config) -> Result<Run> {
        if run.prepared_dir.is_some() || run.error.is_some() {
            return Ok(run);
        }
        run.status = "building".into();
        self.db().save_run(&run)?;
        let preparation = (|| -> Result<PathBuf> {
            let artifact = self.db().artifact(
                run.submission_id
                    .as_deref()
                    .ok_or("no submission before deadline")?,
            )?;
            let dir = artifacts::materialize(&self.root, &artifact, &run.id)?;
            process::build(
                &artifact.manifest,
                &dir,
                Duration::from_secs(config.build_seconds),
            )?;
            Ok(dir)
        })();
        match preparation {
            Ok(dir) => run.prepared_dir = Some(dir.to_string_lossy().into()),
            Err(e) => run.error = Some(e.to_string()),
        }
        run.status = "queued".into();
        let db = self.db();
        db.save_run(&run)?;
        db.event(Some(&run.id), "prepared", &json!({"error":run.error}))?;
        Ok(run)
    }
    #[cfg(test)]
    fn evaluate_next(&self) -> Result<()> {
        self.evaluate(false)?;
        self.evaluate(true)
    }
    fn evaluate(&self, tournaments: bool) -> Result<()> {
        let campaigns = self.db().campaigns()?;
        for c in campaigns {
            if (c.config.benchmark == Benchmark::Chess960) != tournaments {
                continue;
            }
            let runs = self.db().runs(&c.id)?;
            if c.config.benchmark != Benchmark::Chess960 {
                if c.status != "scoring_requested" {
                    continue;
                }
                if let Some(run) = runs.into_iter().find(|r| r.status == "frozen") {
                    if run.submission_id.is_none() {
                        let mut run = run;
                        run.status = "failed".into();
                        run.error = Some("No accepted submission before cutoff".into());
                        self.db().save_run(&run)?;
                        return Ok(());
                    }
                    let run = self.prepare(run, &c.config)?;
                    let j = Judgment {
                        id: crate::id(),
                        run_id: run.id.clone(),
                        status: "pending".into(),
                        judge: None,
                        started_at_ms: None,
                        explore_deadline_ms: None,
                        score_deadline_ms: None,
                        ui_url: None,
                        launch_error: run.error.clone(),
                        actions: 0,
                        successful_actions: 0,
                        action_elapsed_ms: 0.0,
                        peak_memory_bytes: 0,
                        cpu_time_ms: 0,
                    };
                    let db = self.db();
                    db.save_judgment(&j)?;
                    db.event(Some(&run.id), "judge_queued", &j)?;
                    return Ok(());
                }
            } else if ["scoring_requested", "evaluating"].contains(&c.status.as_str())
                && runs.iter().all(|r| r.status != "developing")
            {
                self.tournament(c, runs)?;
                return Ok(());
            }
        }
        Ok(())
    }
    fn tournament(&self, mut c: Campaign, runs: Vec<Run>) -> Result<()> {
        if runs.is_empty() {
            c.status = "completed".into();
            self.db().save_campaign(&c)?;
            return Ok(());
        }
        c.status = "evaluating".into();
        self.db().save_campaign(&c)?;
        let mut entries = Vec::new();
        for run in runs {
            let mut run = self.prepare(run, &c.config)?;
            run.status = "evaluating".into();
            self.db().save_run(&run)?;
            let artifact = run
                .submission_id
                .as_ref()
                .map(|id| self.db().artifact(id))
                .transpose()?;
            entries.push(chess::Entry {
                id: run.id,
                agent: run.agent,
                manifest: artifact.as_ref().map(|a| a.manifest.clone()),
                directory: run
                    .prepared_dir
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.root.clone()),
                error: run.error,
                lines_of_code: artifact.as_ref().map(|a| a.lines_of_code),
                artifact_sha256: artifact.map(|a| a.sha256),
            });
        }
        if c.config.baselines {
            for name in ["random", "capture"] {
                entries.push(chess::Entry {
                    id: format!("baseline:{name}"),
                    agent: format!("baseline:{name}"),
                    manifest: Some(Manifest {
                        run: vec![
                            std::env::current_exe()?.to_string_lossy().into(),
                            "baseline".into(),
                            name.into(),
                        ],
                        build: None,
                        predict: None,
                        env: Default::default(),
                        source_files: vec![],
                        ui: None,
                    }),
                    directory: self.root.clone(),
                    error: None,
                    lines_of_code: None,
                    artifact_sha256: None,
                });
            }
        }
        if entries.len() < 2 {
            let db = self.db();
            let mut run = db.run(&entries[0].id)?;
            run.status = "failed".into();
            run.error=Some("a tournament requires at least two entries; enable baselines or enroll more solutions".into());
            db.save_run(&run)?;
            c.status = "failed".into();
            db.save_campaign(&c)?;
            return Ok(());
        }
        let mut table = chess::standings(&entries, &c.config);
        let completed: Vec<chess::GameRecord> = self.db().games(&c.id)?.into_iter().map(serde_json::from_value).collect::<std::result::Result<_, _>>()?;
        let mut number = 0;
        for round in chess::rounds(&c.config, entries.len()) {
            let records = if number + round.len() <= completed.len() {
                completed[number..number + round.len()].to_vec()
            } else {
                if self.stopping.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let records = chess::play_round(&entries, &round, c.config.max_plies, c.config.parallel_games.into())?;
                let db = self.db();
                let tx = db.db.unchecked_transaction()?;
                for (offset, record) in records.iter().enumerate() {
                    db.game(&c.id, number + offset, record)?;
                }
                tx.commit()?;
                records
            };
            chess::rate_round(&mut table, &records, c.config.elo_k);
            number += round.len();
        }
        let db = self.db();
        let tx = db.db.unchecked_transaction()?;
        for standing in &table {
            if standing.id.starts_with("baseline:") {
                continue;
            }
            let report = json!({"benchmark":"chess960","prompt_version":prompts::version(Benchmark::Chess960),"campaign_id":c.id,"seed":c.config.seed,"config":c.config,"rating":"per_round_pre_round_elo",
                "metrics":standing,"pool":table,"loc_definition":"nonblank physical source lines including comments; detected source extensions plus source_files"});
            db.save_report(&standing.id, &report)?;
            let mut run = db.run(&standing.id)?;
            run.status = "scored".into();
            db.save_run(&run)?;
            db.event(Some(&run.id), "scored", standing)?;
        }
        c.status = "completed".into();
        db.save_campaign(&c)?;
        tx.commit()?;
        Ok(())
    }
    fn admin(&self, token: &str) -> ApiResult<()> {
        if token == self.admin {
            Ok(())
        } else {
            Err(ApiError(401, "admin bearer token required".into()))
        }
    }
    fn run_auth(&self, id: &str, token: &str) -> ApiResult<()> {
        if token == self.admin || self.db().run_auth(id, token)? {
            Ok(())
        } else {
            Err(ApiError(401, "run bearer token required".into()))
        }
    }
    fn judge_auth(&self, id: &str, token: &str) -> ApiResult<()> {
        if token == self.admin || self.db().judge_auth(id, token)? {
            Ok(())
        } else {
            Err(ApiError(401, "judgment bearer token required".into()))
        }
    }

    fn claim(&self, token: &str, body: Value) -> ApiResult<Value> {
        if token != self.admin && token != self.judge_key {
            return Err(ApiError(401, "judge bearer token required".into()));
        }
        let judge = body
            .get("judge")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && s.len() <= 200)
            .ok_or_else(|| ApiError(400, "judge name required".into()))?;
        let claim_token = crate::id();
        let (mut j, run, c) = {
            let db = self.db();
            let Some(mut j) = db.judgments()?.into_iter().find(|j| j.status == "pending" && body["run_id"].as_str().is_none_or(|id| j.run_id == id)) else {
                return Ok(json!({"job":null}));
            };
            j.status = "starting".into();
            j.judge = Some(judge.into());
            let tx = db
                .db
                .unchecked_transaction()
                .map_err(|e| ApiError(500, e.to_string()))?;
            db.save_judgment(&j)?;
            db.set_judge_token(&j.id, &claim_token)?;
            let run = db.run(&j.run_id)?;
            let c = db.campaign(&run.campaign_id)?;
            tx.commit().map_err(|e| ApiError(500, e.to_string()))?;
            (j, run, c)
        };
        let mut session = None;
        if j.launch_error.is_none() && c.config.benchmark != Benchmark::MlPredictor {
            let launch = (|| -> Result<(Protocol, Option<String>)> {
                let artifact = self
                    .db()
                    .artifact(run.submission_id.as_ref().ok_or("missing artifact")?)?;
                let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
                let protocol = Protocol::launch(
                    &artifact.manifest,
                    Path::new(
                        run.prepared_dir
                            .as_deref()
                            .ok_or("missing prepared directory")?,
                    ),
                    Some(port),
                )?;
                let url = if artifact.manifest.ui.as_deref() == Some("web") {
                    Some(format!("http://127.0.0.1:{port}/"))
                } else {
                    None
                };
                Ok((protocol, url))
            })();
            match launch {
                Ok((protocol, url)) => {
                    j.ui_url = url;
                    session = Some(protocol);
                }
                Err(e) => j.launch_error = Some(e.to_string()),
            }
        }
        let started = now_ms();
        j.started_at_ms = Some(started);
        j.explore_deadline_ms = Some(started + c.config.judge_seconds as i64 * 1000);
        j.score_deadline_ms =
            Some(j.explore_deadline_ms.unwrap() + c.config.score_seconds as i64 * 1000);
        j.status = "exploring".into();
        if let Some(protocol) = session {
            let tree = protocol.process.tree.clone();
            self.sessions.lock().unwrap().insert(
                j.id.clone(),
                Session {
                    protocol: Arc::new(Mutex::new(protocol)),
                    tree,
                    deadline: Instant::now() + Duration::from_secs(c.config.judge_seconds),
                },
            );
        }
        let db = self.db();
        db.save_judgment(&j)?;
        let mut run = db.run(&j.run_id)?;
        run.status = "judging".into();
        db.save_run(&run)?;
        db.event(Some(&run.id), "judge_started", &j)?;
        Ok(
            json!({"job":j,"judgment_token":claim_token,"prompt":prompts::judge(&c, &run, completed_requests(&db, &run.id)?.len()),"run":run,"themes":prompts::themes(&run),"benchmark":c.config.benchmark}),
        )
    }
    fn action(&self, id: &str, token: &str, body: Value) -> ApiResult<Value> {
        self.judge_auth(id, token)?;
        let before = self.db().judgment(id)?;
        if before.status != "exploring" || now_ms() >= before.explore_deadline_ms.unwrap_or(0) {
            return Err(ApiError(409, "exploration is closed".into()));
        }
        if body.get("action").and_then(Value::as_str).is_none() {
            return Err(ApiError(400, "action string required".into()));
        }
        let (protocol, tree, deadline) = {
            let sessions = self.sessions.lock().unwrap();
            let s = sessions
                .get(id)
                .ok_or_else(|| ApiError(409, "game is unavailable; inspect launch_error".into()))?;
            (s.protocol.clone(), s.tree.clone(), s.deadline)
        };
        let start = Instant::now();
        let mut process = protocol
            .try_lock()
            .map_err(|_| ApiError(409, "another action is in flight".into()))?;
        let budget = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(5));
        if budget.is_zero() {
            return Err(ApiError(409, "exploration is closed".into()));
        }
        let response = process.exchange(body.clone(), budget);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        let (memory, cpu) = tree.usage();
        let value = match response {
            Ok((v, _)) => json!({"ok":true,"response":v,"elapsed_ms":elapsed}),
            Err(e) => json!({"ok":false,"error":e.to_string(),"elapsed_ms":elapsed}),
        };
        let db = self.db();
        let mut j = db.judgment(id)?;
        j.actions += 1;
        if value["ok"] == true {
            j.successful_actions += 1;
        }
        j.action_elapsed_ms += elapsed;
        j.peak_memory_bytes = j.peak_memory_bytes.max(memory);
        j.cpu_time_ms = j.cpu_time_ms.max(cpu);
        db.save_judgment(&j)?;
        db.event(
            Some(&j.run_id),
            "game_action",
            &json!({"request":body,"result":value}),
        )?;
        Ok(value)
    }

    fn route(&self, method: &str, path: &str, token: &str, body: Value) -> ApiResult<Value> {
        let segments: Vec<_> = path.trim_matches('/').split('/').collect();
        match (method, segments.as_slice()) {
            ("POST", ["v1", "benchmark-batches"]) => {
                self.admin(token)?;
                Ok(json!({"batch":self.create_batch(body)?}))
            }
            ("GET", ["v1", "benchmark-batches"]) => {
                self.admin(token)?;
                Ok(json!({"batches":self.db().batches()?.iter().map(crate::batch::public).collect::<Vec<_>>()}))
            }
            ("GET", ["v1", "benchmark-batches", id]) => {
                self.admin(token)?;
                Ok(json!({"batch":crate::batch::public(&self.db().batch(id)?)}))
            }
            ("POST", ["v1", "benchmark-batches", id, "score"]) => {
                self.admin(token)?;
                let _scoring = self.scoring_lock.try_lock().map_err(|_| ApiError(409, "A scoring request is already running".into()))?;
                let batch = self.db().batch(id)?;
                if batch["cases"].as_array().ok_or_else(|| ApiError(500, "Invalid batch cases".into()))?.iter()
                    .any(|case| case["status"] == "queued" || case["status"] == "running") {
                    return Err(ApiError(409, "All batch cases must finish before scoring".into()));
                }
                let options = serde_json::from_value(body).map_err(|error| ApiError(400, format!("Invalid scoring options: {error}")))?;
                let url = self.address.lock().unwrap().clone().ok_or_else(|| ApiError(503, "Service is not listening".into()))?;
                Ok(crate::scoring::run(&self.root, &url, &batch, options)?)
            }
            ("POST", ["v1", "admin", "wipe"]) => {
                self.admin(token)?;
                Ok(self.wipe()?)
            }
            ("POST", ["v1", "benchmark-batches", id, "stop"]) => {
                self.admin(token)?;
                Ok(json!({"batch":self.stop_batch(id)?}))
            }
            ("GET", ["health"]) => Ok(
                json!({"status":"ok","service":"agent-harness-bench","protocol_version":1,"server_time_ms":now_ms()}),
            ),
            ("GET", ["v1", "campaigns", id]) => {
                self.admin(token)?;
                let db = self.db();
                Ok(
                    json!({"campaign":db.campaign(id)?,"runs":db.runs(id)?,"games_completed":db.games(id)?.len()}),
                )
            }
            ("POST", ["v1", "campaigns", id, "score"]) => {
                self.admin(token)?;
                let db = self.db();
                if db.batches()?.iter().any(|batch| batch["cases"].as_array().is_some_and(|cases|
                    cases.iter().any(|case| case["campaign_id"] == *id && ["queued", "running"].contains(&case["status"].as_str().unwrap_or(""))))) {
                    return Err(ApiError(409, "batch cases must finish before scoring".into()));
                }
                let mut c = db.campaign(id)?;
                if !["enrolling", "awaiting_score"].contains(&c.status.as_str()) {
                    return Err(ApiError(409, "campaign is not awaiting a scoring request".into()));
                }
                let runs = db.runs(id)?.len();
                if runs + if c.config.benchmark == Benchmark::Chess960 && c.config.baselines { 2 } else { 0 } < if c.config.benchmark == Benchmark::Chess960 { 2 } else { 1 } {
                    return Err(ApiError(409, "scoring requires at least two chess entries or one run".into()));
                }
                c.status = "scoring_requested".into();
                db.save_campaign(&c)?;
                Ok(json!({"campaign":c}))
            }
            ("GET", ["v1", "runs", id]) => {
                self.run_auth(id, token)?;
                let db = self.db();
                Ok(json!({"run":db.run(id)?,"report":db.report(id)?,"completed_requests":completed_requests(&db, id)?,"server_time_ms":now_ms()}))
            }
            ("POST", ["v1", "runs", id, "coordinator", secret]) => {
                if *secret != callback_secret(&self.admin, id) {
                    return Err(ApiError(401, "unknown coordinator callback".into()));
                }
                Ok(self.coordinator_event(id, &body)?)
            }
            ("POST", ["v1", "runs", id, "submissions"]) => {
                self.admin(token)?;
                let run = self.db().run(id)?;
                if run.status != "developing" || now_ms() >= run.deadline_ms {
                    return Err(ApiError(409, "development deadline passed".into()));
                }
                let artifact = self.accept(id, serde_json::from_value(body)?)?;
                let deadline = self.db().run(id)?.deadline_ms;
                Ok(json!({"submission":artifact,"accepted_at_ms":now_ms(),"deadline_ms":deadline}))
            }
            ("POST", ["v1", "judges", "claim"]) => self.claim(token, body),
            ("GET", ["v1", "judgments"]) => {
                self.admin(token)?;
                Ok(json!({"judgments":self.db().judgments()?}))
            }
            ("GET", ["v1", "judgments", id]) => {
                self.judge_auth(id, token)?;
                Ok(json!({"job":self.db().judgment(id)?,"server_time_ms":now_ms()}))
            }
            ("GET", ["v1", "judgments", id, "artifact"]) => {
                self.judge_auth(id, token)?;
                let db = self.db();
                let j = db.judgment(id)?;
                let run = db.run(&j.run_id)?;
                Ok(
                    json!({"submission":run.submission_id.as_deref().map(|id|artifacts::read(&self.root,id)).transpose()?}),
                )
            }
            ("POST", ["v1", "judgments", id, "action"]) => self.action(id, token, body),
            ("POST", ["v1", "judgments", id, "score"]) => {
                self.judge_auth(id, token)?;
                self.tick()?;
                let db = self.db();
                let mut j = db.judgment(id)?;
                if j.status != "scoring" || now_ms() >= j.score_deadline_ms.unwrap_or(0) {
                    return Err(ApiError(
                        409,
                        "scores accepted only after exploration stops and before scoring deadline"
                            .into(),
                    ));
                }
                let mut run = db.run(&j.run_id)?;
                let c = db.campaign(&run.campaign_id)?;
                let received = prompts::maze_received(&c, completed_requests(&db, &run.id)?.len());
                let mut progress = None;
                let (score, total, count) = if c.config.benchmark == Benchmark::MazeGame {
                    let score: MazeScore = serde_json::from_value(body)?;
                    score.validate(received)?;
                    progress = Some(score.progress());
                    let total = score.total();
                    (serde_json::to_value(score)?, total, 4)
                } else if c.config.benchmark == Benchmark::MlPredictor {
                    let score: EdaScore = serde_json::from_value(body)?;
                    score.validate()?;
                    let total = score.total();
                    (serde_json::to_value(score)?, total, 4)
                } else if c.config.benchmark == Benchmark::DatasetDashboard {
                    let score: DashboardScore = serde_json::from_value(body)?;
                    score.validate()?;
                    let total = score.total();
                    (serde_json::to_value(score)?, total, 6)
                } else {
                    let score: GameScore = serde_json::from_value(body)?;
                    score.validate()?;
                    let total = score.total();
                    (serde_json::to_value(score)?, total, 5)
                };
                let artifact = run
                    .submission_id
                    .as_deref()
                    .map(|id| db.artifact(id))
                    .transpose()?;
                let mut report = json!({"benchmark":c.config.benchmark,"prompt_version":prompts::version(c.config.benchmark),"campaign_id":run.campaign_id,"themes":prompts::themes(&run),"inspiration":run.inspiration,"dataset":run.dataset,"ml":run.ml,"config":c.config,"judge":j.judge,"scores":score,"total":total,"maximum":count*5,"mean_out_of_5":f64::from(total)/f64::from(count),"telemetry":j,"artifact":artifact});
                report[format!("total_out_of_{}", count * 5)] = json!(total);
                if let Some(progress) = progress {
                    report["progress"] = json!({"fraction_sum":progress,"requests_received":received,"requests_total":prompts::maze_requests(&c).len()});
                }
                let tx = db
                    .db
                    .unchecked_transaction()
                    .map_err(|e| ApiError(500, e.to_string()))?;
                db.save_report(&run.id, &report)?;
                run.status = "scored".into();
                j.status = "completed".into();
                db.save_run(&run)?;
                db.save_judgment(&j)?;
                db.event(Some(&run.id), "scored", &report)?;
                tx.commit().map_err(|e| ApiError(500, e.to_string()))?;
                Ok(json!({"report":report}))
            }
            _ => Err(ApiError(404, "unknown endpoint".into())),
        }
    }
    pub fn serve(self: &Arc<Self>, address: SocketAddr) -> Result<()> {
        if !address.ip().is_loopback() {
            return Err("this local execution service must bind a loopback address; use an authenticated tunnel for remote clients".into());
        }
        let server = Arc::new(Server::http(address)?);
        *self.address.lock().unwrap() = Some(format!("http://{}", server.server_addr()));
        eprintln!(
            "Evaluator listening on http://{}; data: {}",
            server.server_addr(),
            self.root.display()
        );
        let _background = self.background();
        let mut workers = Vec::new();
        for _ in 0..8 {
            let service = self.clone();
            let server = server.clone();
            workers.push(thread::spawn(move || {
                for request in server.incoming_requests() {
                    service.respond_owned(request);
                }
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
        Ok(())
    }
    pub fn respond_owned(&self, mut request: Request) {
        let outcome = (|| -> ApiResult<Value> {
            let token = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
                .unwrap_or("")
                .to_owned();
            let url = request.url().split('?').next().unwrap_or("").to_owned();
            let method = request.method().as_str().to_owned();
            if request.body_length().unwrap_or(0) > artifacts::MAX_UPLOAD {
                return Err(ApiError(413, "request exceeds 48 MiB".into()));
            }
            let mut bytes = Vec::new();
            request
                .as_reader()
                .take((artifacts::MAX_UPLOAD + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > artifacts::MAX_UPLOAD {
                return Err(ApiError(413, "request exceeds 48 MiB".into()));
            }
            let body = if bytes.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&bytes)?
            };
            self.route(&method, &url, &token, body)
        })();
        let (status, value) = match outcome {
            Ok(value) => (200, value),
            Err(ApiError(code, error)) => (code, json!({"error":error})),
        };
        let response = Response::from_string(value.to_string())
            .with_status_code(StatusCode(status))
            .with_header(
                Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap(),
            )
            .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
        let _ = request.respond(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn campaign(service: &Service, benchmark: Benchmark, overrides: Value) -> String {
        let c = Campaign { id: crate::id(), config: Config::new(benchmark, overrides).unwrap(), created_at_ms: now_ms(), status: "enrolling".into() };
        service.db().save_campaign(&c).unwrap();
        c.id
    }
    fn start(service: &Service, cid: &str, agent: &str) -> Value {
        let db = service.db();
        let c = db.campaign(cid).unwrap();
        let mut run = Run::new(&c, agent, (c.config.benchmark == Benchmark::GameBuild).then(|| prompts::random_inspiration(true)),
            (c.config.benchmark == Benchmark::DatasetDashboard).then(crate::datasets::random_assignment));
        run.ml = (c.config.benchmark == Benchmark::MlPredictor).then(crate::ml::random_assignment);
        let token = crate::id();
        db.insert_run(&run, &token).unwrap();
        json!({"run":run,"run_token":token,"prompt":prompts::builder(&c, &run)})
    }
    fn expire(service: &Service, id: &str) {
        let db = service.db();
        let mut run = db.run(id).unwrap();
        run.deadline_ms = now_ms() - 1;
        db.save_run(&run).unwrap();
    }
    fn submit(service: &Service, started: &Value, text: &str) -> ApiResult<Value> {
        service.route("POST", &format!("/v1/runs/{}/submissions", started["run"]["id"].as_str().unwrap()), &service.admin,
            json!({"manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":text}]}))
    }
    fn setup() -> (crate::test_support::TestDir, Arc<Service>, String, String) {
        let root = crate::test_support::dir();
        let service = Service::open(root.path()).unwrap();
        let token = service.admin.clone();
        let cid = campaign(&service, Benchmark::Chess960, json!({}));
        (root, service, token, cid)
    }

    #[test]
    fn game_inspiration_is_per_run_persisted_and_used_by_builders_and_judges() {
        let (root, service, _admin, _) = setup();
        let cid = campaign(&service, Benchmark::GameBuild, json!({}));
        let mut assigned = Vec::new();
        for name in ["first", "second"] {
            let started = start(&service, &cid, name);
            let run: Run = serde_json::from_value(started["run"].clone()).unwrap();
            let c = service.db().campaign(&cid).unwrap();
            let inspiration = run.inspiration.as_ref().unwrap();
            assert_eq!(inspiration.themes.len(), 3);
            assert!((1..=3).contains(&inspiration.genres.len()));
            for word in inspiration.themes.iter().chain(&inspiration.genres) {
                assert!(started["prompt"].as_str().unwrap().contains(word.as_str()));
                assert!(prompts::judge(&c, &run, 0).contains(word.as_str()));
            }
            assert!(!started["prompt"].as_str().unwrap().contains("economic simulation, 4X and/or grand strategy"));
            assigned.push(run);
        }
        assert_ne!(assigned[0].inspiration.as_ref().unwrap().seed, assigned[1].inspiration.as_ref().unwrap().seed);
        drop(service);
        let reopened = Service::open(root.path()).unwrap();
        for run in assigned {
            assert_eq!(reopened.db().run(&run.id).unwrap().inspiration, run.inspiration);
        }
    }

    #[test]
    fn dashboard_defaults_and_assignments() {
        let (_root, service, _, _) = setup();
        let cid = campaign(&service, Benchmark::DatasetDashboard, json!({}));
        assert_eq!(service.db().campaign(&cid).unwrap().config.development_seconds, 900);
        let started = start(&service, &cid, "dashboard");
        let prompt = started["prompt"].as_str().unwrap();
        assert!(prompt.contains(started["run"]["dataset"]["dataset"]["name"].as_str().unwrap()) && !prompt.contains(started["run"]["dataset"]["dataset"]["id"].as_str().unwrap()));
    }

    #[test]
    fn game_evaluation_requires_a_scoring_request() {
        let (_root, s, token, _) = setup();
        let cid = campaign(&s, Benchmark::GameBuild, json!({}));
        let started = start(&s, &cid, "game");
        let id = started["run"]["id"].as_str().unwrap();
        let run = s.db().run(id).unwrap();
        assert_eq!(run.deadline_ms - run.started_at_ms, 1_800_000);
        expire(&s, id);
        s.tick().unwrap();
        s.evaluate_next().unwrap();
        assert_eq!(s.db().run(id).unwrap().status, "frozen");
        assert!(s.db().judgments().unwrap().is_empty());
        assert_eq!(s.route("POST", &format!("/v1/campaigns/{cid}/score"), "invalid", json!({})).unwrap_err().0, 401);
        s.route("POST", &format!("/v1/campaigns/{cid}/score"), &token, json!({})).unwrap();
        s.evaluate_next().unwrap();
        assert!(s.db().judgments().unwrap().is_empty());
        assert_eq!(s.db().run(id).unwrap().status, "failed");
    }

    #[test]
    fn tournaments_and_judged_runs_are_evaluated_by_separate_workers() {
        let (_root, s, token, chess) = setup();
        let game = campaign(&s, Benchmark::GameBuild, json!({}));
        let started = start(&s, &game, "game");
        let id = started["run"]["id"].as_str().unwrap().to_owned();
        submit(&s, &started, "print(1)").unwrap();
        expire(&s, &id);
        {
            let db = s.db();
            let mut c = db.campaign(&chess).unwrap();
            c.status = "scoring_requested".into();
            db.save_campaign(&c).unwrap();
        }
        s.tick().unwrap();
        s.route("POST", &format!("/v1/campaigns/{game}/score"), &token, json!({})).unwrap();
        s.evaluate(true).unwrap();
        assert!(s.db().judgments().unwrap().is_empty(), "the tournament worker never prepares judged runs");
        s.evaluate(false).unwrap();
        assert_eq!(s.db().judgments().unwrap().len(), 1);
        assert_eq!(s.db().campaign(&chess).unwrap().status, "completed", "the tournament worker still plays chess");
    }

    #[test]
    fn dashboard_scores_use_six_independent_criteria() {
        let (_root, s, token, _) = setup();
        let cid = campaign(&s, Benchmark::DatasetDashboard, json!({}));
        let started = start(&s, &cid, "dashboard-score");
        let run_id = started["run"]["id"].as_str().unwrap();
        let judgment = Judgment { id:crate::id(), run_id:run_id.into(), status:"scoring".into(), judge:Some("fixture".into()),
            started_at_ms:Some(now_ms()-2000), explore_deadline_ms:Some(now_ms()-1000), score_deadline_ms:Some(now_ms()+10000),
            ui_url:None, launch_error:None, actions:0, successful_actions:0, action_elapsed_ms:0.0, peak_memory_bytes:0, cpu_time_ms:0 };
        s.db().save_judgment(&judgment).unwrap();
        let path = format!("/v1/judgments/{}/score", judgment.id);
        let mut body = json!({"measurements":{"load_ms":12.0},"notes":"fixture"});
        for key in ["completeness","information_architecture","prose_economy","animation","originality","performance"] {
            body[key] = json!({"score":3,"evidence":"Observed fixture behavior"});
        }
        let mut invalid = body.clone();
        invalid["animation"]["score"] = json!(6);
        assert_eq!(s.route("POST", &path, &token, invalid).unwrap_err().0, 400);
        let response = s.route("POST", &path, &token, body.clone()).unwrap();
        assert_eq!(response["report"]["total_out_of_30"], 18);
        assert_eq!(response["report"]["mean_out_of_5"], 3.0);
        assert_eq!(response["report"]["benchmark"], "dataset_dashboard");
        assert_eq!(s.route("POST", &path, &token, body).unwrap_err().0, 409);
    }

    #[test]
    fn chess_waits_for_a_scoring_request() {
        let (_root, s, token, cid) = setup();
        let started = start(&s, &cid, "a");
        let id = started["run"]["id"].as_str().unwrap();
        expire(&s, id);
        s.tick().unwrap();
        s.evaluate_next().unwrap();
        assert_eq!(s.db().run(id).unwrap().status, "frozen");
        assert!(s.db().games(&cid).unwrap().is_empty());
        s.route("POST", &format!("/v1/campaigns/{cid}/score"), &token, json!({})).unwrap();
        assert_eq!(s.db().campaign(&cid).unwrap().status, "scoring_requested");
        assert_eq!(s.route("POST", &format!("/v1/campaigns/{cid}/score"), &token, json!({})).unwrap_err().0, 409);
    }

    #[test]
    fn deadline_freezes_last_snapshot_and_rejects_late_uploads() {
        let (_root, s, _token, cid) = setup();
        let started = start(&s, &cid, "a");
        let id = started["run"]["id"].as_str().unwrap();
        submit(&s, &started, "print('first')").unwrap();
        let accepted = submit(&s, &started, "print('second')").unwrap();
        expire(&s, id);
        assert_eq!(submit(&s, &started, "print('late')").unwrap_err().0, 409);
        s.tick().unwrap();
        let r = s.db().run(id).unwrap();
        assert_eq!(r.status, "frozen");
        assert_eq!(r.submission_id.as_deref(), accepted["submission"]["id"].as_str());
    }

    fn maze_campaign(service: &Service) -> (String, String, String) {
        let cid = campaign(service, Benchmark::MazeGame, json!({"follow_ups":["Second request.","Third request."]}));
        let config = service.db().campaign(&cid).unwrap().config;
        assert_eq!((config.development_seconds, config.judge_seconds), (1800, 600));
        let start = start(service, &cid, "maze");
        (start["run"]["id"].as_str().unwrap().to_owned(), start["run_token"].as_str().unwrap().to_owned(), start["prompt"].as_str().unwrap().to_owned())
    }

    fn running_session(service: &Service, id: &str) -> String {
        let deadline = service.db().run(id).unwrap().deadline_ms;
        let record = json!({"run_id":id,"status":"running","session":"s1","coordinator_url":"http://127.0.0.1:9","deadline_ms":deadline});
        service.db().save_workflow(id, &record).unwrap();
        service.workflows.lock().unwrap().insert(id.to_owned(), coordinator::Workflow::from_record(&record).unwrap());
        format!("/v1/runs/{id}/coordinator/{}", callback_secret(&service.admin, id))
    }

    fn delivery(root: &Path, number: u64, text: &str) -> PathBuf {
        let folder = root.join("deliveries-fixture").join(number.to_string());
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("harness.json"), r#"{"run":["python","main.py"]}"#).unwrap();
        fs::write(folder.join("main.py"), text).unwrap();
        folder
    }

    #[test]
    fn maze_requests_advance_through_coordinator_callbacks() {
        let (root, service, admin, chess) = setup();
        let (id, _token, prompt) = maze_campaign(&service);
        assert!(prompt.contains(prompts::MAZE_REQUEST) && !prompt.contains("Second request."));
        for word in ["stage", "sequen", "follow", "next request", "complete it", "HARNESS_RUN_TOKEN"] {
            assert!(!prompt.to_lowercase().contains(&word.to_lowercase()), "{word}");
        }
        let path = running_session(&service, &id);
        assert_eq!(service.route("POST", &format!("/v1/runs/{id}/coordinator/wrong"), "", json!({})).unwrap_err().0, 401);
        let event = |kind: &str, request: u64, number: Option<u64>| {
            let folder = number.map(|n| delivery(root.path(), n, &format!("print({n})")));
            json!({"event":kind,"session":"s1","request":request,"delivery":number,"path":folder,"status":"completed"})
        };
        let pending = || service.db().workflow(&id).unwrap().unwrap()["pending_message"].clone();
        service.route("POST", &path, "", event("done", 1, None)).unwrap();
        assert_eq!(pending(), json!(prompts::maze_continue(prompts::MAZE_REQUEST)));
        service.route("POST", &path, "", event("done", 1, Some(1))).unwrap();
        assert_eq!(pending(), json!(prompts::maze_follow_up("Second request.")));
        assert_eq!(service.route("POST", &path, "", event("done", 1, Some(1))).unwrap()["duplicate"], true);
        assert!(service.route("POST", &path, "", event("delivered", 2, Some(2))).unwrap()["accepted"].is_string());
        service.route("POST", &path, "", event("done", 2, Some(3))).unwrap();
        assert!(pending().as_str().unwrap().starts_with("Third request."));
        assert_eq!(service.route("POST", &path, "", event("done", 3, Some(4))).unwrap()["finished"], true);
        assert!(!service.workflows.lock().unwrap().contains_key(&id));
        assert_eq!(service.db().workflow(&id).unwrap().unwrap()["status"], "completed");
        let run = service.route("GET", &format!("/v1/runs/{id}"), &admin, Value::Null).unwrap();
        assert_eq!(run["completed_requests"].as_array().unwrap().len(), 3);
        assert!(run["completed_requests"].as_array().unwrap().iter().all(|r| r["submission_id"].is_string()));
        assert_eq!(service.route("POST", &path, "", event("done", 3, Some(5))).unwrap()["ignored"], "case is no longer running");
        let other = start(&service, &chess, "single");
        let other_id = other["run"]["id"].as_str().unwrap().to_owned();
        let other_path = running_session(&service, &other_id);
        assert_eq!(service.route("POST", &other_path, "", json!({"event":"done","session":"other"})).unwrap()["ignored"], "unknown session");
        let done = service.route("POST", &other_path, "", json!({"event":"done","session":"s1","request":1,"delivery":1,"path":delivery(root.path(), 9, "print(9)"),"status":"completed"})).unwrap();
        assert_eq!(done["finished"], true);
        assert!(service.db().run(&other_id).unwrap().submission_id.is_some());
    }

    #[test]
    fn deliveries_without_a_manifest_are_rejected_and_recorded() {
        let (root, service, _admin, chess) = setup();
        let started = start(&service, &chess, "broken");
        let id = started["run"]["id"].as_str().unwrap().to_owned();
        let path = running_session(&service, &id);
        let folder = root.path().join("no-manifest");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("main.py"), "print(1)").unwrap();
        service.route("POST", &path, "", json!({"event":"delivered","session":"s1","request":1,"delivery":1,"path":folder})).unwrap();
        assert!(service.db().run(&id).unwrap().submission_id.is_none());
        assert!(service.db().events(&id).unwrap().iter().any(|e| e["kind"] == "delivery_rejected" && e["data"]["error"].as_str().unwrap().contains("harness.json")));
    }

    #[test]
    fn wipe_deletes_every_run_and_submission() {
        let (root, service, admin, _chess) = setup();
        let (id, token, _) = maze_campaign(&service);
        submit(&service, &json!({"run":{"id":id},"run_token":token}), "print(1)").unwrap();
        assert!(root.path().join("submissions").exists());
        assert_eq!(service.route("POST", "/v1/admin/wipe", &token, Value::Null).unwrap_err().0, 401);
        let report = service.route("POST", "/v1/admin/wipe", &admin, Value::Null).unwrap();
        assert_eq!((report["deleted"]["submissions"].as_u64(), report["deleted"]["runs"].as_u64(), report["deleted"]["campaigns"].as_u64()), (Some(1), Some(1), Some(2)));
        assert_eq!(report["errors"], json!([]));
        assert!(!root.path().join("submissions").exists());
        assert!(service.db().campaigns().unwrap().is_empty() && service.db().all_runs().unwrap().is_empty());
    }

    #[test]
    fn maze_handoff_stop_freezes_and_judging_scores_received_requests() {
        let (root, service, _admin, _chess) = setup();
        let (id, _token, _) = maze_campaign(&service);
        let path = running_session(&service, &id);
        service.route("POST", &path, "", json!({"event":"done","session":"s1","request":1,"delivery":1,"path":delivery(root.path(), 1, "print(1)"),"status":"completed"})).unwrap();
        expire(&service, &id);
        service.tick().unwrap();
        let run = service.db().run(&id).unwrap();
        assert_eq!(run.status, "frozen");
        let campaign = service.db().campaign(&run.campaign_id).unwrap();
        let judge = prompts::judge(&campaign, &run, 1);
        assert!(judge.contains("1. Build a simple maze game") && judge.contains("2. Second request.") && !judge.contains("Third request."));
        let criterion = json!({"score":3,"evidence":"seen"});
        let mut score = json!({"requests":[{"request":1,"fraction":1.0,"evidence":"works"},{"request":2,"fraction":0.5,"evidence":"half"}],
            "interface":criterion,"efficiency":criterion,"visuals":criterion,"testability":criterion,"notes":""});
        let parsed: MazeScore = serde_json::from_value(score.clone()).unwrap();
        parsed.validate(2).unwrap();
        assert_eq!((parsed.total(), parsed.progress()), (12, 1.5));
        assert!(parsed.validate(3).is_err());
        score["requests"][1]["fraction"] = json!(1.5);
        assert!(serde_json::from_value::<MazeScore>(score).unwrap().validate(2).is_err());
    }

    #[test]
    fn restart_interrupts_workflows_and_keeps_deadlines_and_submissions() {
        let (root, service, _admin, cid) = setup();
        let started = start(&service, &cid, "restart-workflow");
        let id = started["run"]["id"].as_str().unwrap();
        let accepted = submit(&service, &started, "print('accepted')").unwrap();
        service.db().save_workflow(id, &json!({"run_id":id,"status":"running"})).unwrap();
        drop(service);
        let reopened = Service::open(root.path()).unwrap();
        assert_eq!(reopened.db().workflow(id).unwrap().unwrap()["status"], "interrupted");
        let run = reopened.db().run(id).unwrap();
        assert_eq!(run.deadline_ms, started["run"]["deadline_ms"].as_i64().unwrap());
        assert_eq!(run.submission_id.as_deref(), accepted["submission"]["id"].as_str());
    }

    #[test]
    fn scoped_tokens_cannot_administer_or_read_other_runs() {
        let (_root, s, _admin, cid) = setup();
        let a = start(&s, &cid, "a");
        let b = start(&s, &cid, "b");
        let token = a["run_token"].as_str().unwrap();
        assert_eq!(s.route("GET", &format!("/v1/campaigns/{cid}"), token, json!({})).unwrap_err().0, 401);
        assert_eq!(s.route("GET", &format!("/v1/runs/{}", b["run"]["id"].as_str().unwrap()), token, json!({})).unwrap_err().0, 401);
        assert_eq!(s.route("POST", "/v1/judges/claim", token, json!({"judge":"bad"})).unwrap_err().0, 401);
        assert!(Service::open(s.root.as_path()).is_err());
    }

    #[test]
    fn ml_runs_get_training_data_only_and_an_eda_judgment_without_a_launch() {
        let (_root, s, token, _) = setup();
        let python = std::env::current_exe().unwrap();
        let cid = campaign(&s, Benchmark::MlPredictor, json!({"ml_python":python}));
        let started = start(&s, &cid, "ml");
        let id = started["run"]["id"].as_str().unwrap().to_owned();
        let task = json!({"prompt":started["prompt"],"ml":started["run"]["ml"]});
        let prompt = task["prompt"].as_str().unwrap();
        let assignment = &task["ml"]["task"];
        assert!(assignment["train"].as_array().unwrap().iter().all(|url| prompt.contains(url.as_str().unwrap())));
        assert!(prompt.contains(&python.display().to_string()) && prompt.contains("\"predict\"") && prompt.contains("{input}"));
        for hidden in ["adult.test", "optdigits.tes", "t10k", "reference"] {
            assert!(!prompt.contains(hidden) && !task.to_string().contains(hidden), "{hidden}");
        }
        let path = format!("/v1/runs/{id}/submissions");
        let scoped = token.as_str();
        let missing_output = json!({"manifest":{"predict":["python","p.py","{input}"]},"files":[{"path":"p.py","text":"print(1)"}]});
        assert_eq!(s.route("POST", &path, scoped, missing_output).unwrap_err().0, 400);
        s.route("POST", &path, scoped, json!({"manifest":{"predict":["python","p.py","{input}","{output}"]},"files":[{"path":"p.py","text":"print(1)"},{"path":"EDA.md","text":"# EDA"}]})).unwrap();
        expire(&s, &id);
        s.tick().unwrap();
        s.route("POST", &format!("/v1/campaigns/{cid}/score"), &token, json!({})).unwrap();
        s.evaluate(false).unwrap();
        let claimed = s.route("POST", "/v1/judges/claim", &token, json!({"judge":"fixture","run_id":id})).unwrap();
        assert!(claimed["job"]["launch_error"].is_null() && claimed["job"]["ui_url"].is_null());
        assert!(claimed["prompt"].as_str().unwrap().contains("EDA report"));
        let judgment = claimed["job"]["id"].as_str().unwrap().to_owned();
        assert_eq!(s.route("POST", &format!("/v1/judgments/{judgment}/action"), &token, json!({"action":"describe"})).unwrap_err().0, 409);
        {
            let db = s.db();
            let mut j = db.judgment(&judgment).unwrap();
            j.status = "scoring".into();
            j.score_deadline_ms = Some(now_ms() + 10_000);
            db.save_judgment(&j).unwrap();
        }
        let criterion = json!({"score":4,"evidence":"Observed tables"});
        let report = s.route("POST", &format!("/v1/judgments/{judgment}/score"), &token,
            json!({"thoroughness":criterion,"prose_economy":criterion,"presentation":criterion,"information_architecture":criterion,"notes":""})).unwrap()["report"].clone();
        assert_eq!((report["total_out_of_20"].as_u64(), report["mean_out_of_5"].as_f64()), (Some(16), Some(4.0)));
        assert_eq!(report["ml"], task["ml"]);
    }

    #[test]
    fn score_schema_rejects_missing_evidence_and_out_of_range_values() {
        let mut c = Criterion { score: 5, evidence: "observed".into() };
        let mut score = GameScore {
            interface: c.clone(),
            efficiency: c.clone(),
            gameplay: c.clone(),
            visuals: c.clone(),
            testability: c.clone(),
            compared_games: vec!["Civilization VI".into()],
            notes: "".into(),
        };
        assert_eq!(score.total(), 25);
        assert!(score.validate().is_ok());
        c.score = 6;
        score.interface = c;
        assert!(score.validate().is_err());
        score.interface.score = 0;
        score.interface.evidence = " ".into();
        assert!(score.validate().is_err());
    }
}
