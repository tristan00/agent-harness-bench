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

struct Session {
    protocol: Arc<Mutex<Protocol>>,
    tree: Arc<Tree>,
    deadline: Instant,
}
pub struct Service {
    pub root: PathBuf,
    pub store: Mutex<Store>,
    admin: String,
    judge_key: String,
    sessions: Mutex<HashMap<String, Session>>,
    workflows: Mutex<HashMap<String, coordinator::Workflow>>,
    address: Mutex<Option<String>>,
    pub stopping: AtomicBool,
    _lock: File,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (crate::test_support::TestDir, Arc<Service>, String, String) {
        let root = crate::test_support::dir();
        let service = Service::open(root.path()).unwrap();
        let token = service.admin.clone();
        let c = service
            .route(
                "POST",
                "/v1/campaigns",
                &token,
                json!({"benchmark":"chess960","development_seconds":10}),
            )
            .unwrap();
        let cid = c["campaign"]["id"].as_str().unwrap().to_owned();
        (root, service, token, cid)
    }
    #[test]
    fn game_evaluation_requires_manual_request() {
        let (_root, s, token, cid) = setup();
        {
            let db = s.db();
            let mut c = db.campaign(&cid).unwrap();
            c.config.benchmark = Benchmark::GameBuild;
            c.config.development_seconds = 1800;
            db.save_campaign(&c).unwrap();
        }
        let started = s.route("POST", "/v1/runs", &token, json!({"campaign_id":cid,"agent":"game"})).unwrap();
        let id = started["run"]["id"].as_str().unwrap();
        {
            let db = s.db();
            let mut run = db.run(id).unwrap();
            assert_eq!(run.deadline_ms - run.started_at_ms, 1_800_000);
            run.deadline_ms = now_ms() - 1;
            db.save_run(&run).unwrap();
        }
        s.route("POST", &format!("/v1/campaigns/{cid}/close"), &token, json!({})).unwrap();
        s.tick().unwrap();
        s.evaluate_next().unwrap();
        assert_eq!(s.db().run(id).unwrap().status, "frozen");
        assert!(s.db().judgments().unwrap().is_empty());
        s.route("POST", &format!("/v1/campaigns/{cid}/score"), &token, json!({})).unwrap();
        s.evaluate_next().unwrap();
        assert_eq!(s.db().judgments().unwrap().len(), 1);
        assert_eq!(s.db().judgments().unwrap()[0].status, "pending");
    }
    #[test]
    fn manual_enrollment_defers_scoring_but_preserves_run_deadlines() {
        let (_root, s, token, cid) = setup();
        {
            let db = s.db();
            let mut c = db.campaign(&cid).unwrap();
            c.config.baselines = false;
            c.config.development_seconds = 1200;
            c.enrollment_deadline_ms = now_ms() - 1;
            db.save_campaign(&c).unwrap();
        }
        s.tick().unwrap();
        let started = s.route("POST", "/v1/runs", &token, json!({"campaign_id":cid,"agent":"a"})).unwrap();
        let id = started["run"]["id"].as_str().unwrap();
        {
            let db = s.db();
            let mut run = db.run(id).unwrap();
            assert_eq!(run.deadline_ms - run.started_at_ms, 1_200_000);
            run.deadline_ms = now_ms() - 1;
            db.save_run(&run).unwrap();
        }
        s.tick().unwrap();
        s.evaluate_next().unwrap();
        assert_eq!(s.db().run(id).unwrap().status, "frozen");
        assert_eq!(s.db().campaign(&cid).unwrap().status, "enrolling");
        assert!(s.db().games(&cid).unwrap().is_empty());
        s.route("POST", "/v1/runs", &token, json!({"campaign_id":cid,"agent":"b"})).unwrap();
        s.route("POST", &format!("/v1/campaigns/{cid}/close"), &token, json!({})).unwrap();
        assert_eq!(s.db().campaign(&cid).unwrap().status, "awaiting_score");
        s.evaluate_next().unwrap();
        assert_eq!(s.db().campaign(&cid).unwrap().status, "awaiting_score");
        assert!(s.db().games(&cid).unwrap().is_empty());
        assert_eq!(s.route("POST", &format!("/v1/campaigns/{cid}/score"), "invalid", json!({})).unwrap_err().0, 401);
        s.route("POST", &format!("/v1/campaigns/{cid}/score"), &token, json!({})).unwrap();
        assert_eq!(s.db().campaign(&cid).unwrap().status, "scoring_requested");
        assert_eq!(s.route("POST", "/v1/runs", &token, json!({"campaign_id":cid,"agent":"c"})).unwrap_err().0, 409);
    }
    #[test]
    fn deadline_freezes_last_snapshot_and_rejects_late_uploads() {
        let (_root, s, token, cid) = setup();
        let started = s
            .route(
                "POST",
                "/v1/runs",
                &token,
                json!({"campaign_id":cid,"agent":"a"}),
            )
            .unwrap();
        let id = started["run"]["id"].as_str().unwrap();
        let scoped = started["run_token"].as_str().unwrap();
        let submission = json!({"manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":"print('first')"}]});
        s.route(
            "POST",
            &format!("/v1/runs/{id}/submissions"),
            scoped,
            submission.clone(),
        )
        .unwrap();
        let mut latest = submission.clone();
        latest["files"][0]["text"] = json!("print('second')");
        let accepted = s
            .route(
                "POST",
                &format!("/v1/runs/{id}/submissions"),
                scoped,
                latest,
            )
            .unwrap();
        {
            let db = s.db();
            let mut r = db.run(id).unwrap();
            r.deadline_ms = now_ms() - 1;
            db.save_run(&r).unwrap();
        }
        assert_eq!(
            s.route(
                "POST",
                &format!("/v1/runs/{id}/submissions"),
                scoped,
                submission
            )
            .unwrap_err()
            .0,
            409
        );
        s.tick().unwrap();
        let r = s.db().run(id).unwrap();
        assert_eq!(r.status, "frozen");
        assert_eq!(
            r.submission_id.as_deref(),
            accepted["submission"]["id"].as_str()
        );
    }
    #[test]
    fn iterations_are_sequential_idempotent_and_deadline_bound() {
        let (_root, service, admin, campaign) = setup();
        let start = service
            .route(
                "POST",
                "/v1/runs",
                &admin,
                json!({"campaign_id":campaign,"agent":"iterations"}),
            )
            .unwrap();
        let id = start["run"]["id"].as_str().unwrap();
        let token = start["run_token"].as_str().unwrap();
        let path = format!("/v1/runs/{id}/iterations");
        let mut completion = json!({"iteration":1,"tasks":["reviewed-task"],"submission":{
            "manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":"print('first')"}]
        }});
        assert_eq!(
            service
                .route("POST", &path, "invalid", completion.clone())
                .unwrap_err()
                .0,
            401
        );
        completion["iteration"] = json!(2);
        assert_eq!(
            service
                .route("POST", &path, token, completion.clone())
                .unwrap_err()
                .0,
            409
        );
        completion["iteration"] = json!(1);
        let first = service
            .route("POST", &path, token, completion.clone())
            .unwrap();
        assert_eq!(
            first,
            service
                .route("POST", &path, token, completion.clone())
                .unwrap()
        );
        completion["submission"]["files"][0]["text"] = json!("print('second')");
        assert_eq!(
            service
                .route("POST", &path, token, completion.clone())
                .unwrap_err()
                .0,
            409
        );
        completion["iteration"] = json!(2);
        let second = service
            .route("POST", &path, token, completion.clone())
            .unwrap();
        assert_eq!(second["next_iteration"], 3);
        assert_eq!(service.db().iterations(id).unwrap().len(), 2);
        {
            let db = service.db();
            let mut run = db.run(id).unwrap();
            run.deadline_ms = now_ms() - 1;
            db.save_run(&run).unwrap();
        }
        completion["iteration"] = json!(3);
        assert_eq!(
            service
                .route("POST", &path, token, completion)
                .unwrap_err()
                .0,
            409
        );
        service.tick().unwrap();
        assert_eq!(
            service.db().run(id).unwrap().submission_id.as_deref(),
            second["submission"]["id"].as_str()
        );
        assert_eq!(service.db().run(id).unwrap().status, "frozen");
    }
    #[test]
    fn restart_marks_active_workflow_interrupted_without_losing_completions() {
        let (root, service, admin, campaign) = setup();
        let start = service
            .route(
                "POST",
                "/v1/runs",
                &admin,
                json!({"campaign_id":campaign,"agent":"restart-workflow"}),
            )
            .unwrap();
        let id = start["run"]["id"].as_str().unwrap();
        let accepted = service.route("POST", &format!("/v1/runs/{id}/iterations"), &admin,
            json!({"iteration":1,"submission":{"manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":"print('accepted')"}]}})).unwrap();
        service
            .db()
            .save_workflow(id, &json!({"run_id":id,"status":"running"}))
            .unwrap();
        drop(service);
        let reopened = Service::open(root.path()).unwrap();
        assert_eq!(
            reopened.db().workflow(id).unwrap().unwrap()["status"],
            "interrupted"
        );
        assert_eq!(
            reopened.db().iterations(id).unwrap(),
            vec![accepted.clone()]
        );
        let run = reopened.db().run(id).unwrap();
        assert_eq!(
            run.deadline_ms,
            start["run"]["deadline_ms"].as_i64().unwrap()
        );
        assert_eq!(
            run.submission_id.as_deref(),
            accepted["submission"]["id"].as_str()
        );
    }
    #[test]
    fn scoped_tokens_cannot_administer_or_read_other_runs() {
        let (_root, s, admin, cid) = setup();
        let a = s
            .route(
                "POST",
                "/v1/runs",
                &admin,
                json!({"campaign_id":cid,"agent":"a"}),
            )
            .unwrap();
        let b = s
            .route(
                "POST",
                "/v1/runs",
                &admin,
                json!({"campaign_id":cid,"agent":"b"}),
            )
            .unwrap();
        let token = a["run_token"].as_str().unwrap();
        assert_eq!(
            s.route("GET", "/v1/campaigns", token, json!({}))
                .unwrap_err()
                .0,
            401
        );
        assert_eq!(
            s.route(
                "GET",
                &format!("/v1/runs/{}", b["run"]["id"].as_str().unwrap()),
                token,
                json!({})
            )
            .unwrap_err()
            .0,
            401
        );
        assert_eq!(
            s.route("POST", "/v1/judges/claim", token, json!({"judge":"bad"}))
                .unwrap_err()
                .0,
            401
        );
        assert!(Service::open(s.root.as_path()).is_err());
    }
    #[test]
    fn score_schema_rejects_missing_evidence_and_out_of_range_values() {
        let mut c = Criterion {
            score: 5,
            evidence: "observed".into(),
        };
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
    #[test]
    fn restart_retains_deadlines_and_last_accepted_version() {
        let (root, s, admin, cid) = setup();
        let a = s
            .route(
                "POST",
                "/v1/runs",
                &admin,
                json!({"campaign_id":cid,"agent":"a"}),
            )
            .unwrap();
        let id = a["run"]["id"].as_str().unwrap();
        let deadline = a["run"]["deadline_ms"].as_i64().unwrap();
        drop(s);
        let reopened = Service::open(root.path()).unwrap();
        assert_eq!(reopened.db().run(id).unwrap().deadline_ms, deadline);
    }
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
            address: Mutex::new(None),
            stopping: AtomicBool::new(false),
            _lock: lock,
        });
        service.recover()?;
        Ok(service)
    }
    fn db(&self) -> MutexGuard<'_, Store> {
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
                workflow["status"] = json!("interrupted");
                workflow["error"] =
                    json!("Service restarted; last accepted completion is retained for scoring");
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
        let evaluator = self.clone();
        vec![
            thread::spawn(move || {
                while !timers.stopping.load(Ordering::Relaxed) {
                    if let Err(e) = timers.tick() {
                        eprintln!("scheduler: {e}");
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }),
            thread::spawn(move || {
                while !evaluator.stopping.load(Ordering::Relaxed) {
                    if let Err(e) = evaluator.evaluate_next() {
                        eprintln!("evaluation worker: {e}");
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }),
        ]
    }
    pub fn tick(&self) -> Result<()> {
        let now = now_ms();
        let mut finished = Vec::new();
        {
            let mut workflows = self.workflows.lock().unwrap();
            for (id, workflow) in workflows.iter_mut() {
                if now >= workflow.deadline_ms || Instant::now() >= workflow.deadline {
                    workflow.child.tree.stop();
                    finished.push((id.clone(), "deadline", None));
                } else if let Some(status) = workflow.child.try_wait()? {
                    workflow.child.tree.stop();
                    finished.push((
                        id.clone(),
                        if status.success() {
                            "completed"
                        } else {
                            "failed"
                        },
                        status.code(),
                    ));
                }
            }
            for (id, _, _) in &finished {
                workflows.remove(id);
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
            if c.config.benchmark != Benchmark::Chess960 && c.status == "enrolling" && now >= c.enrollment_deadline_ms {
                c.status = "waiting".into();
                db.save_campaign(&c)?;
            }
            if c.config.benchmark == Benchmark::GameBuild && c.status == "scoring_requested" {
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
    fn evaluate_next(&self) -> Result<()> {
        let campaigns = self.db().campaigns()?;
        for c in campaigns {
            let runs = self.db().runs(&c.id)?;
            if c.config.benchmark == Benchmark::GameBuild {
                if c.status != "scoring_requested" {
                    continue;
                }
                if let Some(run) = runs.into_iter().find(|r| r.status == "frozen") {
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
        // Imported opponents are evaluated from their immutable source, with fresh Elo.
        // They do not mutate or replace reports from the original campaign.
        for id in &c.config.pool_run_ids {
            let run = self.db().run(id)?;
            let artifact = self.db().artifact(
                run.submission_id
                    .as_deref()
                    .ok_or("pool entry has no artifact")?,
            )?;
            let preparation = (|| -> Result<PathBuf> {
                let dir = artifacts::materialize(&self.root, &artifact, "pool")?;
                process::build(
                    &artifact.manifest,
                    &dir,
                    Duration::from_secs(c.config.build_seconds),
                )?;
                Ok(dir)
            })();
            let (directory, error) = match preparation {
                Ok(dir) => (dir, None),
                Err(e) => (self.root.clone(), Some(e.to_string())),
            };
            entries.push(chess::Entry {
                id: format!("pool:{id}"),
                agent: run.agent,
                manifest: Some(artifact.manifest),
                directory,
                error,
                lines_of_code: Some(artifact.lines_of_code),
                artifact_sha256: Some(artifact.sha256),
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
        let schedule = chess::schedule(&c.config, entries.len());
        let mut table = chess::standings(&entries, &c.config);
        let completed = self.db().games(&c.id)?;
        for saved in &completed {
            chess::rate(
                &mut table,
                &serde_json::from_value(saved.clone())?,
                c.config.elo_k,
            );
        }
        for (number, pairing) in schedule.into_iter().enumerate().skip(completed.len()) {
            if self.stopping.load(Ordering::Relaxed) {
                return Ok(());
            }
            let record = chess::play(&entries, pairing, c.config.max_plies)?;
            self.db().game(&c.id, number, &record)?;
            chess::rate(&mut table, &record, c.config.elo_k);
        }
        let db = self.db();
        let tx = db.db.unchecked_transaction()?;
        for standing in &table {
            if standing.id.starts_with("baseline:") || standing.id.starts_with("pool:") {
                continue;
            }
            let report = json!({"benchmark":"chess960","prompt_version":prompts::VERSION,"campaign_id":c.id,"seed":c.config.seed,"config":c.config,
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
            let Some(mut j) = db.judgments()?.into_iter().find(|j| j.status == "pending") else {
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
        if j.launch_error.is_none() {
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
            json!({"job":j,"judgment_token":claim_token,"prompt":prompts::judge(&c),"run":run,"themes":c.themes}),
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
            ("GET", ["health"]) => Ok(
                json!({"status":"ok","service":"agent-harness-bench","protocol_version":1,"server_time_ms":now_ms()}),
            ),
            ("GET", ["v1", "benchmarks"]) => {
                self.admin(token)?;
                Ok(
                    json!({"benchmarks":["chess960","game_build"],"prompt_version":prompts::VERSION,"chess_defaults":Config::default(),"game_defaults":Config{benchmark:Benchmark::GameBuild,development_seconds:1800,..Config::default()}}),
                )
            }
            ("POST", ["v1", "campaigns"]) => {
                self.admin(token)?;
                let mut config: Config = serde_json::from_value(body.clone())?;
                if config.benchmark == Benchmark::GameBuild
                    && body.get("development_seconds").is_none()
                {
                    config.development_seconds = 1800;
                }
                if body.get("seed").is_none() {
                    config.seed = u64::from_le_bytes(
                        uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap(),
                    );
                }
                for id in &config.pool_run_ids {
                    let db = self.db();
                    let run = db.run(id)?;
                    let source = db.campaign(&run.campaign_id)?;
                    if run.status != "scored"
                        || run.submission_id.is_none()
                        || source.config.benchmark != Benchmark::Chess960
                    {
                        return Err(ApiError(
                            400,
                            "pool entries must be scored chess runs with submitted source".into(),
                        ));
                    }
                }
                config.validate()?;
                let now = now_ms();
                let c = Campaign {
                    id: crate::id(),
                    themes: if config.benchmark == Benchmark::GameBuild {
                        prompts::themes(config.seed)
                    } else {
                        vec![]
                    },
                    enrollment_deadline_ms: now + config.enrollment_seconds as i64 * 1000,
                    config,
                    created_at_ms: now,
                    status: "enrolling".into(),
                };
                self.db().save_campaign(&c)?;
                Ok(json!({"campaign":c}))
            }
            ("GET", ["v1", "campaigns"]) => {
                self.admin(token)?;
                Ok(json!({"campaigns":self.db().campaigns()?}))
            }
            ("GET", ["v1", "campaigns", id]) => {
                self.admin(token)?;
                let db = self.db();
                Ok(
                    json!({"campaign":db.campaign(id)?,"runs":db.runs(id)?,"games_completed":db.games(id)?.len()}),
                )
            }
            ("POST", ["v1", "campaigns", id, "close"]) => {
                self.admin(token)?;
                let db = self.db();
                let mut c = db.campaign(id)?;
                if c.status != "enrolling" {
                    return Err(ApiError(409, "enrollment already closed".into()));
                }
                c.enrollment_deadline_ms = now_ms();
                c.status = if c.config.benchmark == Benchmark::Chess960 { "awaiting_score" } else { "waiting" }.into();
                db.save_campaign(&c)?;
                Ok(json!({"campaign":c}))
            }
            ("POST", ["v1", "campaigns", id, "score"]) => {
                self.admin(token)?;
                let db = self.db();
                let mut c = db.campaign(id)?;
                if !["enrolling", "awaiting_score", "waiting"].contains(&c.status.as_str()) {
                    return Err(ApiError(409, "campaign is not awaiting a manual scoring request".into()));
                }
                let runs = db.runs(id)?;
                let entries = runs.len() + c.config.pool_run_ids.len() + if c.config.baselines { 2 } else { 0 };
                if c.config.benchmark == Benchmark::Chess960 && entries < 2 {
                    return Err(ApiError(409, "scoring requires at least two entries".into()));
                }
                if c.config.benchmark == Benchmark::GameBuild && runs.is_empty() {
                    return Err(ApiError(409, "game evaluation requires at least one run".into()));
                }
                c.enrollment_deadline_ms = now_ms();
                c.status = "scoring_requested".into();
                db.save_campaign(&c)?;
                Ok(json!({"campaign":c}))
            }
            ("GET", ["v1", "campaigns", id, "games"]) => {
                self.admin(token)?;
                Ok(json!({"games":self.db().games(id)?}))
            }
            ("GET", ["v1", "coordinator-profiles"]) => {
                self.admin(token)?;
                let profiles = coordinator::profiles(&self.root)?;
                Ok(json!({"profiles":profiles.as_object().unwrap().keys().collect::<Vec<_>>()}))
            }
            ("POST", ["v1", "coordinator-runs"]) => {
                self.admin(token)?;
                let name = body["profile"]
                    .as_str()
                    .ok_or_else(|| ApiError(400, "profile required".into()))?;
                let profile = coordinator::profile(&self.root, name)?;
                let url = self
                    .address
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| ApiError(503, "HTTP service is not running".into()))?;
                let started = self.route("POST", "/v1/runs", token, body.clone())?;
                let id = started["run"]["id"].as_str().unwrap();
                let mut workflows = self.workflows.lock().unwrap();
                let mut record =
                    json!({"run_id":id,"profile":name,"status":"running","started_at_ms":now_ms()});
                self.db().save_workflow(id, &record)?;
                match coordinator::launch(&self.root, &url, &started, &profile) {
                    Ok(workflow) => {
                        workflows.insert(id.into(), workflow);
                    }
                    Err(error) => {
                        record["status"] = json!("failed");
                        record["error"] = json!(error.to_string());
                        self.db().save_workflow(id, &record)?;
                    }
                }
                self.db().event(Some(id), "coordinator_started", &record)?;
                Ok(json!({"run":started["run"],"workflow":record,"server_time_ms":now_ms()}))
            }
            ("GET", ["v1", "runs", id, "coordinator"]) => {
                self.run_auth(id, token)?;
                let workflow = self.db().workflow(id)?;
                let progress =
                    fs::read(self.root.join("coordinator").join(id).join("progress.json"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                Ok(json!({"workflow":workflow,"progress":progress,"server_time_ms":now_ms()}))
            }
            ("GET", ["v1", "runs", id, "iterations"]) => {
                self.run_auth(id, token)?;
                Ok(json!({"iterations":self.db().iterations(id)?}))
            }
            ("POST", ["v1", "runs"]) => {
                self.admin(token)?;
                let campaign = body
                    .get("campaign_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError(400, "campaign_id required".into()))?;
                let agent = body
                    .get("agent")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty() && s.len() <= 200)
                    .ok_or_else(|| ApiError(400, "agent name required".into()))?;
                let db = self.db();
                let c = db.campaign(campaign)?;
                let now = now_ms();
                if c.status != "enrolling" || (c.config.benchmark != Benchmark::Chess960 && now >= c.enrollment_deadline_ms) {
                    return Err(ApiError(409, "enrollment closed".into()));
                }
                let enrolled = db.runs(campaign)?;
                if enrolled.len() >= 32 || enrolled.iter().any(|r| r.agent == agent) {
                    return Err(ApiError(
                        409,
                        "campaign full or agent already enrolled".into(),
                    ));
                }
                let run = Run {
                    id: crate::id(),
                    campaign_id: campaign.into(),
                    agent: agent.into(),
                    status: "developing".into(),
                    started_at_ms: now,
                    deadline_ms: now + c.config.development_seconds as i64 * 1000,
                    submission_id: None,
                    prepared_dir: None,
                    error: None,
                };
                let run_token = crate::id();
                db.insert_run(&run, &run_token)?;
                db.event(Some(&run.id), "started", &run)?;
                Ok(
                    json!({"run":run,"run_token":run_token,"prompt":prompts::builder(&c),"config":c.config,"themes":c.themes,"server_time_ms":now}),
                )
            }
            ("GET", ["v1", "runs", id]) => {
                self.run_auth(id, token)?;
                let db = self.db();
                Ok(json!({"run":db.run(id)?,"report":db.report(id)?,"server_time_ms":now_ms()}))
            }
            ("GET", ["v1", "runs", id, "events"]) => {
                self.run_auth(id, token)?;
                Ok(json!({"events":self.db().events(id)?}))
            }
            ("POST", ["v1", "runs", id, kind @ ("submissions" | "iterations")]) => {
                self.run_auth(id, token)?;
                use sha2::{Digest, Sha256};
                let iteration = if *kind == "iterations" {
                    Some(
                        body["iteration"]
                            .as_u64()
                            .filter(|n| *n > 0 && *n < i64::MAX as u64)
                            .ok_or_else(|| {
                                ApiError(400, "positive iteration number required".into())
                            })?,
                    )
                } else {
                    None
                };
                let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&body)?));
                let replay = |db: &Store| -> ApiResult<Option<Value>> {
                    if let Some(number) = iteration {
                        if let Some(saved) = db
                            .iteration(id, number, &digest)
                            .map_err(|e| ApiError(409, e.to_string()))?
                        {
                            return Ok(Some(saved));
                        }
                        if number != db.iterations(id)?.len() as u64 + 1 {
                            return Err(ApiError(
                                409,
                                "iterations must be sequential, starting at 1".into(),
                            ));
                        }
                    }
                    Ok(None)
                };
                {
                    let db = self.db();
                    let r = db.run(id)?;
                    if r.status != "developing" || now_ms() >= r.deadline_ms {
                        return Err(ApiError(409, "development deadline passed".into()));
                    }
                    if let Some(saved) = replay(&db)? {
                        return Ok(saved);
                    }
                }
                let submission = if iteration.is_some() {
                    body["submission"].clone()
                } else {
                    body.clone()
                };
                let artifact = artifacts::save(&self.root, serde_json::from_value(submission)?)?;
                let db = self.db();
                let mut run = db.run(id)?;
                // Check again after decoding/writing: arrival before expiry alone is insufficient.
                if run.status != "developing" || now_ms() >= run.deadline_ms {
                    return Err(ApiError(
                        409,
                        "deadline passed before snapshot was accepted".into(),
                    ));
                }
                if let Some(saved) = replay(&db)? {
                    return Ok(saved);
                }
                let tx = db
                    .db
                    .unchecked_transaction()
                    .map_err(|e| ApiError(500, e.to_string()))?;
                db.save_artifact(id, &artifact)?;
                run.submission_id = Some(artifact.id.clone());
                db.save_run(&run)?;
                db.event(Some(id), "submitted", &artifact)?;
                let mut accepted = json!({"submission":artifact,"accepted_at_ms":now_ms(),"deadline_ms":run.deadline_ms});
                if let Some(number) = iteration {
                    accepted["iteration"] = json!(number);
                    accepted["next_iteration"] = json!(number + 1);
                    accepted["tasks"] = body.get("tasks").cloned().unwrap_or(json!([]));
                    db.save_iteration(id, number, &digest, &accepted)?;
                    db.event(Some(id), "iteration_completed", &accepted)?;
                }
                tx.commit().map_err(|e| ApiError(500, e.to_string()))?;
                Ok(accepted)
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
            ("GET", ["v1", "judgments", id, "events"]) => {
                self.judge_auth(id, token)?;
                let run = self.db().judgment(id)?.run_id;
                Ok(json!({"events":self.db().events(&run)?}))
            }
            ("POST", ["v1", "judgments", id, "action"]) => self.action(id, token, body),
            ("POST", ["v1", "judgments", id, "score"]) => {
                self.judge_auth(id, token)?;
                let score: GameScore = serde_json::from_value(body)?;
                score.validate()?;
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
                let artifact = run
                    .submission_id
                    .as_deref()
                    .map(|id| db.artifact(id))
                    .transpose()?;
                let report = json!({"benchmark":"game_build","prompt_version":prompts::VERSION,"campaign_id":run.campaign_id,"themes":c.themes,"config":c.config,"judge":j.judge,"scores":score,"total_out_of_25":score.total(),"mean_out_of_5":f64::from(score.total())/5.0,"telemetry":j,"artifact":artifact});
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
