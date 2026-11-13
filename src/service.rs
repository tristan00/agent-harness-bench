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

pub(crate) fn handoff_token(admin: &str, run: &str) -> String {
    crate::storage::hash(&format!("{admin}:handoff:{run}"))
}

pub(crate) fn started_config(config: &Config) -> Value {
    let mut value = serde_json::to_value(config).expect("serializable config");
    value.as_object_mut().unwrap().remove("follow_ups");
    value
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
    pub stopping: AtomicBool,
    _lock: File,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_inspiration_is_per_run_persisted_and_used_by_builders_and_judges() {
        let (root, service, admin, _) = setup();
        let campaign = service.route("POST", "/v1/campaigns", &admin, json!({"benchmark":"game_build","seed":960})).unwrap()["campaign"].clone();
        assert_eq!(campaign["themes"], json!([]));
        let mut assigned = Vec::new();
        for name in ["first", "second"] {
            let started = service.route("POST", "/v1/runs", &admin, json!({"campaign_id":campaign["id"],"agent":name})).unwrap();
            let run: Run = serde_json::from_value(started["run"].clone()).unwrap();
            let c = service.db().campaign(&run.campaign_id).unwrap();
            let inspiration = run.inspiration.as_ref().unwrap();
            assert_eq!(inspiration.themes.len(), 3);
            assert_eq!(inspiration.themes, prompts::themes(inspiration.seed));
            assert_eq!(started["themes"], json!(inspiration.themes));
            let task = service.route("GET", &format!("/v1/runs/{}/task",run.id), &admin, Value::Null).unwrap();
            assert_eq!(task["inspiration"], json!(inspiration));
            assert_eq!(task["prompt"], started["prompt"]);
            for theme in &inspiration.themes {
                assert!(started["prompt"].as_str().unwrap().contains(theme));
                assert!(prompts::judge_for_run(&c, &run, 0).contains(theme));
            }
            assigned.push(run);
        }
        assert_ne!(assigned[0].inspiration.as_ref().unwrap().seed, assigned[1].inspiration.as_ref().unwrap().seed);
        drop(service);
        let reopened = Service::open(root.path()).unwrap();
        for run in assigned { assert_eq!(reopened.db().run(&run.id).unwrap().inspiration, run.inspiration); }
    }

    #[test]
    fn legacy_run_prompts_keep_the_original_campaign_themes() {
        let (_root, service, admin, cid) = setup();
        let mut campaign = service.db().campaign(&cid).unwrap();
        campaign.config.benchmark = Benchmark::GameBuild;
        campaign.config.seed = 960;
        campaign.themes = prompts::themes(960);
        service.db().save_campaign(&campaign).unwrap();
        let started = service.route("POST", "/v1/runs", &admin, json!({"campaign_id":cid,"agent":"legacy"})).unwrap();
        let mut saved = started["run"].clone();
        saved.as_object_mut().unwrap().remove("inspiration");
        let legacy: Run = serde_json::from_value(saved).unwrap();
        assert!(legacy.inspiration.is_none());
        assert_eq!(prompts::builder_for_run(&campaign, &legacy), prompts::builder(&campaign));
        assert_eq!(prompts::judge_for_run(&campaign, &legacy, 0), prompts::judge(&campaign));
        assert_eq!(prompts::themes_for_run(&campaign, &legacy), campaign.themes);
    }

    #[test]
    fn dashboard_defaults_assignments_and_legacy_batch_extension() {
        let (root, service, admin, _) = setup();
        let campaign = service.route("POST", "/v1/campaigns", &admin, json!({"benchmark":"dataset_dashboard"})).unwrap();
        assert_eq!(campaign["campaign"]["config"]["development_seconds"], 900);
        let started = service.route("POST", "/v1/runs", &admin, json!({"campaign_id":campaign["campaign"]["id"],"agent":"dashboard"})).unwrap();
        let task_path = format!("/v1/runs/{}/task", started["run"]["id"].as_str().unwrap());
        let task = service.route("GET", &task_path, started["run_token"].as_str().unwrap(), Value::Null).unwrap();
        assert_eq!(task["dataset"], started["run"]["dataset"]);
        assert!(task["prompt"].as_str().unwrap().contains(task["dataset"]["dataset"]["id"].as_str().unwrap()));
        assert_eq!(service.route("GET", &task_path, "wrong", Value::Null).unwrap_err().0, 401);
        let workspaces = root.path().join("workspaces");
        fs::create_dir(&workspaces).unwrap();
        let mut cases = Vec::new();
        for index in 0..4 {
            let path = workspaces.join(format!("workspace-{index}"));
            fs::create_dir(&path).unwrap();
            let mut settings = json!({"planner":"sol_6_1_light","worker":"sol_6_1_light","prompts":{}});
            for (role, filename) in [("project_planner", "PROJECT_PLANNER.md"), ("worker_protocol", "WORKER_PROTOCOL.md")] {
                use sha2::{Digest, Sha256};
                fs::write(path.join(filename), "Selected fixture instructions.").unwrap();
                settings["prompts"][role] = json!({"candidate":"fixture.md","sha256":format!("{:x}", Sha256::digest(b"Selected fixture instructions."))});
            }
            for benchmark in ["chess960", "game_build"] {
                cases.push(json!({"workspace":path,"workspace_name":format!("workspace-{index}/{benchmark}"),
                    "benchmark":benchmark,"status":"deadline","settings":settings}));
            }
        }
        let before = cases.clone();
        let template = format!("Task {{benchmark_task}}\nWorkspace: {}", workspaces.join("{workspace_name}").display());
        service.db().save_batch(&json!({"id":"legacy","status":"completed","cases":cases,"campaigns":{},"template":template})).unwrap();
        let extended = service.append_dashboard("legacy", json!({})).unwrap();
        assert_eq!(&extended["cases"].as_array().unwrap()[..8], before.as_slice());
        assert_eq!(extended["cases"].as_array().unwrap().len(), 12);
        assert_eq!(extended, service.append_dashboard("legacy", json!({})).unwrap());
        let seeds: std::collections::HashSet<_> = extended["cases"].as_array().unwrap()[8..].iter().map(|c| c["dataset"]["seed"].as_u64().unwrap()).collect();
        assert_eq!(seeds.len(), 4);
    }
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
        assert!(s.db().judgments().unwrap().is_empty());
        assert_eq!(s.db().run(id).unwrap().status, "failed");
    }
    #[test]
    fn tournaments_and_judged_runs_are_evaluated_by_separate_workers() {
        let (_root, s, token, chess) = setup();
        let game = s.route("POST", "/v1/campaigns", &token, json!({"benchmark":"game_build"})).unwrap()["campaign"]["id"].as_str().unwrap().to_owned();
        let started = s.route("POST", "/v1/runs", &token, json!({"campaign_id":game,"agent":"game"})).unwrap();
        let id = started["run"]["id"].as_str().unwrap().to_owned();
        s.route("POST", &format!("/v1/runs/{id}/submissions"), started["run_token"].as_str().unwrap(),
            json!({"manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":"print(1)"}]})).unwrap();
        {
            let db = s.db();
            let mut run = db.run(&id).unwrap();
            run.deadline_ms = now_ms() - 1;
            db.save_run(&run).unwrap();
            let mut c = db.campaign(&chess).unwrap();
            c.status = "scoring_requested".into();
            db.save_campaign(&c).unwrap();
        }
        s.route("POST", &format!("/v1/campaigns/{game}/close"), &token, json!({})).unwrap();
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
        let (_root, s, token, cid) = setup();
        {
            let db = s.db();
            let mut c = db.campaign(&cid).unwrap();
            c.config.benchmark = Benchmark::DatasetDashboard;
            db.save_campaign(&c).unwrap();
        }
        let started = s.route("POST", "/v1/runs", &token, json!({"campaign_id":cid,"agent":"dashboard-score"})).unwrap();
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
        let mut invalid = body.clone(); invalid["animation"]["score"] = json!(6);
        assert_eq!(s.route("POST", &path, &token, invalid).unwrap_err().0, 400);
        let response = s.route("POST", &path, &token, body.clone()).unwrap();
        assert_eq!(response["report"]["total_out_of_30"],18);
        assert_eq!(response["report"]["mean_out_of_5"],3.0);
        assert_eq!(response["report"]["benchmark"],"dataset_dashboard");
        assert_eq!(s.route("POST", &path, &token, body).unwrap_err().0,409);
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
    fn maze_campaign(service: &Service, admin: &str) -> (String, String, String) {
        let campaign = service.route("POST", "/v1/campaigns", admin, json!({"benchmark":"maze_game","follow_ups":["Second request.","Third request."]})).unwrap();
        assert_eq!(campaign["campaign"]["config"]["development_seconds"], 1800);
        assert_eq!(campaign["campaign"]["config"]["judge_seconds"], 600);
        let cid = campaign["campaign"]["id"].as_str().unwrap();
        let start = service.route("POST", "/v1/runs", admin, json!({"campaign_id":cid,"agent":"maze"})).unwrap();
        assert!(start["config"].get("follow_ups").is_none());
        (start["run"]["id"].as_str().unwrap().to_owned(), start["run_token"].as_str().unwrap().to_owned(), start["handoff_token"].as_str().unwrap().to_owned())
    }
    #[test]
    fn maze_requests_advance_only_through_the_supervisor_handoff() {
        let (_root, service, admin, chess) = setup();
        let (id, token, handoff) = maze_campaign(&service, &admin);
        let task = service.route("GET", &format!("/v1/runs/{id}/task"), &token, Value::Null).unwrap();
        let prompt = task["prompt"].as_str().unwrap();
        assert!(prompt.contains(prompts::MAZE_REQUEST) && !prompt.contains("Second request."));
        for word in ["stage", "sequen", "follow", "next request", "complete it"] {
            assert!(!prompt.to_lowercase().contains(word), "{word}");
        }
        let path = format!("/v1/runs/{id}/handoff");
        assert_eq!(service.route("POST", &path, &token, json!({"action":"advance"})).unwrap_err().0, 401);
        let current = service.route("POST", &path, &handoff, json!({"action":"current"})).unwrap();
        assert_eq!((current["request"].as_u64(), current["submitted"].as_bool()), (Some(1), Some(false)));
        assert_eq!(service.route("POST", &path, &handoff, json!({"action":"advance"})).unwrap_err().0, 409);
        let submit = |text: &str| service.route("POST", &format!("/v1/runs/{id}/submissions"), &token,
            json!({"manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":text}]})).unwrap();
        submit("print(1)");
        submit("print(1.5)");
        let next = service.route("POST", &path, &handoff, json!({"action":"advance"})).unwrap();
        assert_eq!(next["next"], prompts::maze_follow_up("Second request."));
        assert_eq!(service.route("POST", &path, &handoff, json!({"action":"advance"})).unwrap_err().0, 409);
        let current = service.route("POST", &path, &handoff, json!({"action":"current"})).unwrap();
        assert_eq!(current["continue"], prompts::maze_continue("Second request."));
        submit("print(2)");
        assert!(service.route("POST", &path, &handoff, json!({"action":"advance"})).unwrap()["next"].as_str().unwrap().starts_with("Third request."));
        submit("print(3)");
        assert!(service.route("POST", &path, &handoff, json!({"action":"advance"})).unwrap()["next"].is_null());
        service.tick().unwrap();
        let run = service.route("GET", &format!("/v1/runs/{id}"), &token, Value::Null).unwrap();
        assert_eq!(run["run"]["status"], "frozen");
        assert_eq!(run["completed_requests"].as_array().unwrap().len(), 3);
        assert!(run["completed_requests"].as_array().unwrap().iter().all(|r| r["submission_id"].is_string()));
        let other = service.route("POST", "/v1/runs", &admin, json!({"campaign_id":chess,"agent":"single"})).unwrap();
        assert!(other.get("handoff_token").is_none());
        let other_path = format!("/v1/runs/{}/handoff", other["run"]["id"].as_str().unwrap());
        assert_eq!(service.route("POST", &other_path, &admin, json!({"action":"current"})).unwrap_err().0, 409);
    }
    #[test]
    fn wipe_deletes_every_run_and_submission() {
        let (root, service, admin, _chess) = setup();
        let (id, token, _) = maze_campaign(&service, &admin);
        service.route("POST", &format!("/v1/runs/{id}/submissions"), &token, json!({"manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":"print(1)"}]})).unwrap();
        assert!(root.path().join("submissions").exists());
        assert_eq!(service.route("POST", "/v1/admin/wipe", &token, Value::Null).unwrap_err().0, 401);
        let report = service.route("POST", "/v1/admin/wipe", &admin, Value::Null).unwrap();
        assert_eq!((report["deleted"]["submissions"].as_u64(), report["deleted"]["runs"].as_u64(), report["deleted"]["campaigns"].as_u64()), (Some(1), Some(1), Some(2)));
        assert_eq!(report["errors"], json!([]));
        assert!(!root.path().join("submissions").exists());
        assert!(service.db().campaigns().unwrap().is_empty() && service.db().all_runs().unwrap().is_empty());
        assert!(service.route("POST", "/v1/campaigns", &admin, json!({"benchmark":"chess960"})).is_ok());
    }
    #[test]
    fn maze_handoff_stop_freezes_and_judging_scores_received_requests() {
        let (_root, service, admin, _chess) = setup();
        let (id, token, handoff) = maze_campaign(&service, &admin);
        let path = format!("/v1/runs/{id}/handoff");
        service.route("POST", &format!("/v1/runs/{id}/submissions"), &token, json!({"manifest":{"run":["python","x.py"]},"files":[{"path":"x.py","text":"print(1)"}]})).unwrap();
        service.route("POST", &path, &handoff, json!({"action":"advance"})).unwrap();
        service.route("POST", &path, &handoff, json!({"action":"stop"})).unwrap();
        service.tick().unwrap();
        assert_eq!(service.db().run(&id).unwrap().status, "frozen");
        let db = service.db();
        let campaign = db.campaign(&db.run(&id).unwrap().campaign_id).unwrap();
        drop(db);
        let judge = prompts::judge_for_run(&campaign, &service.db().run(&id).unwrap(), 1);
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
            batch_lock: Mutex::new(()),
            address: Mutex::new(None),
            stopping: AtomicBool::new(false),
            _lock: lock,
        });
        service.recover()?;
        Ok(service)
    }
    pub(crate) fn wipe(&self) -> Result<Value> {
        let _batches = self.batch_lock.lock().unwrap();
        let mut workflows = self.workflows.lock().unwrap();
        let mut errors = Vec::new();
        let stopped = workflows.len();
        for (id, workflow) in workflows.drain() {
            workflow.child.tree.stop();
            if let Err(error) = coordinator::stop_workspace_tasks(&self.root, &id) {
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
            for table in ["iterations", "workflows", "events", "games", "reports", "judgments", "submissions", "runs", "campaigns", "batches"] {
                deleted.insert(table.into(), json!(tx.execute(&format!("DELETE FROM {table}"), [])?));
            }
            tx.commit()?;
        }
        drop(workflows);
        let mut removed = Vec::new();
        for name in ["submissions", "work", "coordinator", "batch-scores"] {
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
                workflow["status"] = json!("interrupted");
                if let Err(error) = coordinator::stop_workspace_tasks(&self.root, &id) { eprintln!("{error}"); }
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
                if let Err(error) = coordinator::stop_workspace_tasks(&self.root, id) { eprintln!("{error}"); }
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
            if standing.id.starts_with("baseline:") || standing.id.starts_with("pool:") {
                continue;
            }
            let report = json!({"benchmark":"chess960","prompt_version":prompts::VERSION,"campaign_id":c.id,"seed":c.config.seed,"config":c.config,"rating":"per_round_pre_round_elo",
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
            json!({"job":j,"judgment_token":claim_token,"prompt":prompts::judge_for_run(&c, &run, completed_requests(&db, &run.id)?.len()),"run":run,"themes":prompts::themes_for_run(&c, &run),"benchmark":c.config.benchmark}),
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
            ("POST", ["v1", "admin", "wipe"]) => {
                self.admin(token)?;
                Ok(self.wipe()?)
            }
            ("POST", ["v1", "benchmark-batches", id, "stop"]) => {
                self.admin(token)?;
                Ok(json!({"batch":self.stop_batch(id)?}))
            }
            ("POST", ["v1", "benchmark-batches", id, "dashboard"]) => {
                self.admin(token)?;
                Ok(json!({"batch":self.append_dashboard(id, body)?}))
            }
            ("GET", ["v1", "runs", id, "task"]) => {
                self.run_auth(id, token)?;
                let db = self.db();
                let run = db.run(id)?;
                let campaign = db.campaign(&run.campaign_id)?;
                let mut task = json!({"run_id":id,"benchmark":campaign.config.benchmark,"prompt":prompts::builder_for_run(&campaign, &run)});
                if campaign.config.benchmark == Benchmark::GameBuild {
                    task["themes"] = json!(prompts::themes_for_run(&campaign, &run));
                    if let Some(inspiration) = &run.inspiration { task["inspiration"] = json!(inspiration); }
                }
                if let Some(dataset) = run.dataset { task["dataset"] = json!(dataset); }
                Ok(task)
            }
            ("GET", ["health"]) => Ok(
                json!({"status":"ok","service":"agent-harness-bench","protocol_version":1,"server_time_ms":now_ms()}),
            ),
            ("POST", ["v1", "campaigns"]) => {
                self.admin(token)?;
                let mut config: Config = serde_json::from_value(body.clone())?;
                if config.benchmark == Benchmark::GameBuild
                    && body.get("development_seconds").is_none()
                {
                    config.development_seconds = 1800;
                }
                if config.benchmark == Benchmark::DatasetDashboard && body.get("development_seconds").is_none() {
                    config.development_seconds = 900;
                }
                if config.benchmark == Benchmark::MazeGame && body.get("development_seconds").is_none() {
                    config.development_seconds = 1800;
                }
                if config.benchmark == Benchmark::MazeGame && body.get("judge_seconds").is_none() {
                    config.judge_seconds = 600;
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
                    themes: vec![],
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
                if db.batches()?.iter().any(|batch| batch["cases"].as_array().is_some_and(|cases|
                    cases.iter().any(|case| case["campaign_id"] == *id && ["queued", "running"].contains(&case["status"].as_str().unwrap_or(""))))) {
                    return Err(ApiError(409, "batch cases must finish before scoring".into()));
                }
                let mut c = db.campaign(id)?;
                if !["enrolling", "awaiting_score", "waiting"].contains(&c.status.as_str()) {
                    return Err(ApiError(409, "campaign is not awaiting a manual scoring request".into()));
                }
                let runs = db.runs(id)?;
                let entries = runs.len() + c.config.pool_run_ids.len() + if c.config.baselines { 2 } else { 0 };
                if c.config.benchmark == Benchmark::Chess960 && entries < 2 {
                    return Err(ApiError(409, "scoring requires at least two entries".into()));
                }
                if c.config.benchmark != Benchmark::Chess960 && runs.is_empty() {
                    return Err(ApiError(409, "game evaluation requires at least one run".into()));
                }
                c.enrollment_deadline_ms = now_ms();
                c.status = "scoring_requested".into();
                db.save_campaign(&c)?;
                Ok(json!({"campaign":c}))
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
                    inspiration: (c.config.benchmark == Benchmark::GameBuild).then(prompts::random_inspiration),
                    dataset: (c.config.benchmark == Benchmark::DatasetDashboard).then(crate::datasets::random_assignment),
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
                    {
                        let mut started = json!({"run":run,"run_token":run_token,"prompt":prompts::builder_for_run(&c, &run),"config":started_config(&c.config),"themes":prompts::themes_for_run(&c, &run),"server_time_ms":now});
                        if c.config.benchmark == Benchmark::MazeGame { started["handoff_token"] = json!(handoff_token(&self.admin, &run.id)); }
                        started
                    },
                )
            }
            ("GET", ["v1", "runs", id]) => {
                self.run_auth(id, token)?;
                let db = self.db();
                Ok(json!({"run":db.run(id)?,"report":db.report(id)?,"completed_requests":completed_requests(&db, id)?,"server_time_ms":now_ms()}))
            }
            ("POST", ["v1", "runs", id, "handoff"]) => {
                if token != self.admin && token != handoff_token(&self.admin, id) {
                    return Err(ApiError(401, "supervisor handoff token required".into()));
                }
                let db = self.db();
                let mut run = db.run(id)?;
                let campaign = db.campaign(&run.campaign_id)?;
                if campaign.config.benchmark != Benchmark::MazeGame {
                    return Err(ApiError(409, "this benchmark has no follow-up requests".into()));
                }
                let requests = prompts::maze_requests(&campaign);
                let completed = completed_requests(&db, id)?;
                let current = completed.len() + 1;
                let submitted = db.events(id)?.iter().take_while(|e| e["kind"] != "request_completed").any(|e| e["kind"] == "submitted");
                let now = now_ms();
                let open = run.status == "developing" && now < run.deadline_ms;
                match body["action"].as_str() {
                    Some("current") => {
                        let text = requests.get(current - 1).cloned();
                        Ok(json!({"request":current,"requests":requests.len(),"submitted":submitted,"open":open,
                            "continue":text.as_deref().map(prompts::maze_continue),"completed":completed}))
                    }
                    Some(action @ ("advance" | "stop")) => {
                        if !open {
                            return Err(ApiError(409, "development deadline passed".into()));
                        }
                        let mut next = None;
                        if action == "advance" {
                            if !submitted || current > requests.len() {
                                return Err(ApiError(409, "a new accepted submission is required before handing off".into()));
                            }
                            db.event(Some(id), "request_completed", &json!({"request":current,"submission_id":run.submission_id,"completed_at_ms":now,"elapsed_ms":now - run.started_at_ms}))?;
                            next = requests.get(current).map(|text| prompts::maze_follow_up(text));
                        } else {
                            db.event(Some(id), "handoff_stopped", &json!({"request":current,"at_ms":now}))?;
                        }
                        if next.is_none() {
                            run.deadline_ms = now;
                            db.save_run(&run)?;
                            drop(db);
                            if let Some(workflow) = self.workflows.lock().unwrap().get_mut(*id) {
                                workflow.deadline_ms = now;
                                workflow.deadline = Instant::now();
                            }
                        }
                        Ok(json!({"next":next}))
                    }
                    _ => Err(ApiError(400, "action must be current, advance or stop".into())),
                }
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
                let mut report = json!({"benchmark":c.config.benchmark,"prompt_version":prompts::VERSION,"campaign_id":run.campaign_id,"themes":prompts::themes_for_run(&c, &run),"inspiration":run.inspiration,"dataset":run.dataset,"config":c.config,"judge":j.judge,"scores":score,"total":total,"maximum":count*5,"mean_out_of_5":f64::from(total)/f64::from(count),"telemetry":j,"artifact":artifact});
                report[format!("total_out_of_{}", count * 5)] = json!(total);
                if let Some(progress) = progress {
                    report["prompt_version"] = json!(prompts::MAZE_VERSION);
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
