use agent_harness_bench::{
    chess::{self, Entry, Pairing},
    model::Manifest,
    process::{self, MOVE_TIMEOUT, Protocol},
};
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn manifest(script: &str) -> Manifest {
    Manifest {
        run: vec![
            std::env::var("PYTHON").unwrap_or_else(|_| "python".into()),
            "-u".into(),
            "-c".into(),
            script.into(),
        ],
        build: None,
        env: Default::default(),
        source_files: vec![],
        ui: None,
    }
}
fn entry(id: &str, script: &str) -> Entry {
    Entry {
        id: id.into(),
        agent: id.into(),
        manifest: Some(manifest(script)),
        directory: std::env::current_dir().unwrap(),
        error: None,
        lines_of_code: Some(1),
        artifact_sha256: None,
    }
}
const VALID: &str = "import sys,json\nprint('{\"ready\":true}',flush=True)\nfor line in sys.stdin:\n r=json.loads(line);print(json.dumps({'id':r['id'],'move':r['allowed_moves'][0]}),flush=True)";

#[cfg(windows)]
#[test]
fn sanitized_build_can_find_windows_rust_linker() {
    let dir = std::env::temp_dir().join(format!("harness-compiler-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn main() { println!(\"ready\"); }").unwrap();
    let mut candidate = manifest(VALID);
    candidate.build = Some(vec!["rustc".into(), "main.rs".into(), "-o".into(), "compiled.exe".into()]);
    let result = process::build(&candidate, &dir, Duration::from_secs(30));
    let exists = dir.join("compiled.exe").is_file();
    std::fs::remove_dir_all(&dir).unwrap();
    result.unwrap();
    assert!(exists);
}

#[test]
fn persistent_protocol_and_timeout() {
    let dir = std::env::current_dir().unwrap();
    let mut p = Protocol::launch(&manifest(VALID), &dir, None).unwrap();
    for _ in 0..3 {
        let (r, _) = p
            .exchange(
                json!({"fen":"unused","allowed_moves":["e2e4"]}),
                MOVE_TIMEOUT,
            )
            .unwrap();
        assert_eq!(r["move"], "e2e4");
    }
    let mut slow=Protocol::launch(&manifest("import sys,time\nprint('{\"ready\":true}',flush=True)\nsys.stdin.readline()\ntime.sleep(10)"),&dir,None).unwrap();
    let start = Instant::now();
    let e = slow
        .exchange(
            json!({"fen":"unused","allowed_moves":["e2e4"]}),
            MOVE_TIMEOUT,
        )
        .unwrap_err();
    assert!(e.to_string().contains("timeout"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn illegal_crashed_malformed_and_wrong_id_players_forfeit() {
    for script in [
        "import sys,json\nprint('{\"ready\":true}',flush=True)\nr=json.loads(sys.stdin.readline());print(json.dumps({'id':r['id'],'move':'a1a1'}),flush=True)",
        "import sys\nprint('{\"ready\":true}',flush=True)\nsys.stdin.readline();sys.exit(1)",
        "import sys\nprint('{\"ready\":true}',flush=True)\nsys.stdin.readline();print('broken',flush=True)",
        "import sys\nprint('{\"ready\":true}',flush=True)\nsys.stdin.readline();print('{\"id\":999,\"move\":\"e2e4\"}',flush=True)",
    ] {
        let result = chess::play(
            &[entry("bad", script), entry("good", VALID)],
            Pairing {
                white: 0,
                black: 1,
                position: 518,
            },
            4,
        )
        .unwrap();
        assert_eq!(result.white_score, 0.0);
        assert_eq!(result.black_score, 1.0);
        assert_eq!(result.reason, "white_move_forfeit");
        assert!(result.error.is_some());
    }
}

#[test]
fn missing_programs_are_double_forfeits() {
    let bad = Entry {
        id: "bad".into(),
        agent: "bad".into(),
        manifest: None,
        directory: PathBuf::new(),
        error: Some("build failed".into()),
        lines_of_code: Some(1),
        artifact_sha256: None,
    };
    let result = chess::play(
        &[bad.clone(), bad],
        Pairing {
            white: 0,
            black: 1,
            position: 518,
        },
        4,
    )
    .unwrap();
    assert_eq!((result.white_score, result.black_score), (0.0, 0.0));
    assert_eq!(result.reason, "double_forfeit");
}

#[test]
fn oversized_output_and_blocking_build_are_bounded() {
    let dir = std::env::current_dir().unwrap();
    let mut player=Protocol::launch(&manifest("import sys\nprint('{\"ready\":true}',flush=True)\nsys.stdin.readline();print('x'*100000,flush=True)"),&dir,None).unwrap();
    assert!(
        player
            .exchange(json!({"action":"observe"}), MOVE_TIMEOUT)
            .is_err()
    );
    let temp = std::env::temp_dir().join(format!(
        "harness-process-test-{}",
        agent_harness_bench::id()
    ));
    std::fs::create_dir(&temp).unwrap();
    let mut m = manifest(VALID);
    m.build = Some(manifest("import time;time.sleep(10)").run);
    let start = Instant::now();
    assert!(process::build(&m, &temp, Duration::from_millis(100)).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(temp.parent(), Some(std::env::temp_dir().as_path()));
    std::fs::remove_dir_all(temp).unwrap();
}

#[test]
fn timeout_kills_descendants() {
    let dir = std::env::temp_dir().join(format!(
        "harness-process-test-{}",
        agent_harness_bench::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let script = "import sys,subprocess,time\nprint('{\"ready\":true}',flush=True)\nsys.stdin.readline()\nsubprocess.Popen([sys.executable,'-c',\"import time,pathlib;time.sleep(0.5);pathlib.Path('escaped').write_text('bad')\"])\ntime.sleep(10)";
    let mut p = Protocol::launch(&manifest(script), &dir, None).unwrap();
    assert!(
        p.exchange(json!({"action":"observe"}), MOVE_TIMEOUT)
            .is_err()
    );
    drop(p);
    std::thread::sleep(Duration::from_millis(650));
    assert!(!dir.join("escaped").exists());
    assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
    std::fs::remove_dir_all(dir).unwrap();
}
