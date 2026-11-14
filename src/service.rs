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
                let coordinator = batch["profile_config"]["coordinator_url"].as_str().ok_or_else(|| ApiError(500, "Batch coordinator URL is missing".into()))?;
                let api = |method: &str, path: &str, token: &str, body: Value| -> crate::Result<Value> {
                    self.route(method, path, token, body).map_err(|ApiError(code, message)| format!("HTTP {code}: {message}").into())
                };
                Ok(crate::scoring::run(&api, &self.admin, &self.root, &url, id, coordinator, options)?)
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
