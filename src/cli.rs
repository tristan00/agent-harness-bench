use crate::Result;
use serde_json::{Value, json};
use std::{fs, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}};

pub const USAGE: &str = "harness-bench serve [--bind 127.0.0.1:8741] [--data DIR]
harness-bench service setup|start|stop [--data DIR] [--bind ADDR] [--binary EXE] [--skip-build]
harness-bench batch start [--count 12] [--max-parallel 4] [--coordinator URL]
harness-bench batch create REQUEST.json | status [ID] [--json] | stop [ID] | score [ID] [--max-judges N] [--url URL] [--token-file FILE]
  (ID defaults to the batch in DIR/batch-receipt.json)
harness-bench baseline [random|capture]

service setup builds the release binary into runtime/build/benchmark and starts it; run it through `cargo run --release --` so the
running executable is not the one being rebuilt. DIR defaults to runtime/benchmark-service next to this repository.";

fn source() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn workspace() -> PathBuf {
    source().parent().unwrap_or(source()).to_path_buf()
}

struct Args {
    positional: Vec<String>,
    flags: std::collections::HashMap<String, String>,
    switches: Vec<String>,
}

fn parse(args: &[String], valued: &[&str]) -> Result<Args> {
    let mut parsed = Args { positional: vec![], flags: Default::default(), switches: vec![] };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if valued.contains(&arg.as_str()) {
            i += 1;
            parsed.flags.insert(arg.clone(), args.get(i).ok_or_else(|| format!("missing {arg} value"))?.clone());
        } else if arg.starts_with("--") {
            parsed.switches.push(arg.clone());
        } else {
            parsed.positional.push(arg.clone());
        }
        i += 1;
    }
    Ok(parsed)
}

fn data_dir(args: &Args) -> PathBuf {
    args.flags.get("--data").map(PathBuf::from).unwrap_or_else(|| workspace().join("runtime").join("benchmark-service"))
}

pub fn batch(args: &[String]) -> Result<()> {
    let args = parse(args, &["--url", "--token-file", "--data", "--max-judges", "--count", "--max-parallel", "--coordinator"])?;
    let url = args.flags.get("--url").cloned().or_else(|| std::env::var("HARNESS_URL").ok()).unwrap_or_else(|| "http://127.0.0.1:8741".into());
    let token = match args.flags.get("--token-file") {
        Some(file) => fs::read_to_string(file)?.trim().to_owned(),
        None => match std::env::var("HARNESS_API_TOKEN") {
            Ok(token) if !token.is_empty() => token,
            _ => fs::read_to_string(data_dir(&args).join("admin-token"))?.trim().to_owned(),
        },
    };
    let call = |method: &str, path: &str, body: Option<Value>, timeout: Option<Duration>| -> Result<Value> {
        let mut request = ureq::request(method, &format!("{}{path}", url.trim_end_matches('/'))).set("Authorization", &format!("Bearer {token}"));
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        match response {
            Ok(response) => Ok(response.into_json()?),
            Err(ureq::Error::Status(code, response)) => Err(format!("HTTP {code}: {}", response.into_string().unwrap_or_default()).into()),
            Err(error) => Err(error.into()),
        }
    };
    let data = data_dir(&args);
    let id = || -> Result<String> {
        if let Some(id) = args.positional.get(1) {
            return Ok(id.clone());
        }
        let receipt = crate::metrics::read(&data.join("batch-receipt.json")).map_err(|_| "batch ID required (no batch-receipt.json)")?;
        Ok(receipt["batch"]["id"].as_str().ok_or("batch-receipt.json has no batch id")?.to_owned())
    };
    let short = Some(Duration::from_secs(120));
    let result = match args.positional.first().map(String::as_str) {
        Some("start") => {
            let count: usize = args.flags.get("--count").map_or(Ok(12), |n| n.parse())?;
            let parallel: usize = args.flags.get("--max-parallel").map_or(Ok(4), |n| n.parse())?;
            let coordinator = match args.flags.get("--coordinator") {
                Some(url) => url.clone(),
                None => crate::metrics::read(&data.join("coordinator-profiles.json"))?["default"]["coordinator_url"].as_str().ok_or("no default coordinator profile")?.to_owned(),
            };
            let root = workspace().join("workspaces");
            let paths: Vec<String> = (1..=count).map(|i| root.join(format!("benchmark_{i}")).display().to_string().replace('\\', "/")).collect();
            let generated: Value = ureq::post(&format!("{}/api/workspaces/generate", coordinator.trim_end_matches('/'))).timeout(Duration::from_secs(60))
                .send_json(json!({"paths":paths}))?.into_json()?;
            fs::write(data.join("generated-workspaces.json"), serde_json::to_string_pretty(&generated)?)?;
            let workspaces: Vec<Value> = generated["workspaces"].as_array().ok_or("coordinator returned no workspaces")?.iter()
                .map(|w| json!({"path":w["workspace"], "settings":w["settings"]})).collect();
            for w in &workspaces {
                let s = &w["settings"];
                println!("{} planner={} worker={} planner_prompt={}", w["path"].as_str().unwrap_or(""), s["planner"], s["worker"], s["prompts"]["project_planner"]["candidate"]);
            }
            let mut task_configs = json!({});
            let follow_ups = workspace().join("benchmark-private").join("maze_game_follow_ups.json");
            if follow_ups.is_file() {
                task_configs["maze_game"] = json!({"follow_ups_file":follow_ups});
            }
            let request = json!({"profile":"default", "workspace_root":root, "max_parallel":parallel, "task_configs":task_configs, "workspaces":workspaces});
            fs::write(data.join("batch-request.json"), serde_json::to_string_pretty(&request)?)?;
            let receipt = call("POST", "/v1/benchmark-batches", Some(request), short)?;
            fs::write(data.join("batch-receipt.json"), serde_json::to_string_pretty(&receipt)?)?;
            let batch = &receipt["batch"];
            println!("batch {} {} {} cases", batch["id"].as_str().unwrap_or(""), batch["status"].as_str().unwrap_or(""), batch["cases"].as_array().map_or(0, Vec::len));
            return Ok(());
        }
        Some("create") => {
            let file = args.positional.get(1).ok_or("request file required")?;
            call("POST", "/v1/benchmark-batches", Some(crate::metrics::read(Path::new(file))?), short)?
        }
        Some("status") => {
            let result = call("GET", &format!("/v1/benchmark-batches/{}", id()?), None, short)?;
            if !args.switches.iter().any(|s| s == "--json") {
                print_status(&result["batch"]);
                return Ok(());
            }
            result
        }
        Some("stop") => call("POST", &format!("/v1/benchmark-batches/{}/stop", id()?), Some(json!({})), short)?,
        Some("score") => {
            let judges: usize = args.flags.get("--max-judges").map_or(Ok(2), |n| n.parse())?;
            call("POST", &format!("/v1/benchmark-batches/{}/score", id()?), Some(json!({"max_judges":judges})), None)?
        }
        _ => return Err(USAGE.into()),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn print_status(batch: &Value) {
    let cases = batch["cases"].as_array().cloned().unwrap_or_default();
    let mut counts = std::collections::BTreeMap::new();
    for case in &cases {
        *counts.entry(case["status"].as_str().unwrap_or("?").to_owned()).or_insert(0) += 1;
    }
    println!("batch {} {} {:?}", batch["id"].as_str().unwrap_or(""), batch["status"].as_str().unwrap_or(""), counts);
    let time = |ms: &Value| ms.as_i64().map_or("-".to_owned(), |ms| {
        let secs = ms / 1000;
        format!("{:02}:{:02}Z", secs / 3600 % 24, secs / 60 % 60)
    });
    for case in cases.iter().filter(|c| c["status"] != "queued") {
        let workspace = case["workspace_name"].as_str().or(case["workspace"].as_str()).unwrap_or("");
        println!("  {:<24} {:<18} {:<10} {:>6} {:>6} {}", workspace, case["benchmark"].as_str().unwrap_or(""), case["status"].as_str().unwrap_or(""),
            time(&case["started_at_ms"]), time(&case["ended_at_ms"]), case["error"].as_str().unwrap_or(""));
    }
}

#[cfg(windows)]
fn executable(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize])))
    }
}

#[cfg(unix)]
fn executable(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(windows)]
fn terminate(pid: u32) -> Result<()> {
    use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() || TerminateProcess(handle, 1) == 0 {
            let error = std::io::Error::last_os_error();
            if !handle.is_null() {
                CloseHandle(handle);
            }
            return Err(error.into());
        }
        CloseHandle(handle);
    }
    Ok(())
}

#[cfg(unix)]
fn terminate(pid: u32) -> Result<()> {
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn same(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => if cfg!(windows) { a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()) } else { a == b },
        _ => false,
    }
}

fn running(data: &Path) -> Option<u32> {
    let pid: u32 = fs::read_to_string(data.join("service.pid")).ok()?.trim().parse().ok()?;
    let binary = fs::read_to_string(data.join("service-binary")).ok()?;
    same(&executable(pid)?, Path::new(binary.trim())).then_some(pid)
}

fn healthy(bind: &str) -> bool {
    ureq::get(&format!("http://{bind}/health")).timeout(Duration::from_secs(1)).call().is_ok()
}

fn start(data: &Path, bind: &str, binary: Option<PathBuf>) -> Result<()> {
    fs::create_dir_all(data)?;
    if running(data).is_some() {
        return Err("Benchmark service is already running. Leave it running until the active batch finishes.".into());
    }
    let marker = data.join("next-service-binary");
    let staged = fs::read_to_string(&marker).ok().map(|s| PathBuf::from(s.trim())).filter(|p| !p.as_os_str().is_empty());
    let binary = binary.or(staged.clone()).unwrap_or_else(|| source().join("target/release").join(format!("harness-bench{}", std::env::consts::EXE_SUFFIX)));
    let binary = fs::canonicalize(&binary).map_err(|_| format!("Missing executable: {}", binary.display()))?;
    let append = |name: &str| fs::OpenOptions::new().create(true).append(true).open(data.join(name));
    let mut command = Command::new(&binary);
    command.args(["serve", "--bind", bind, "--data"]).arg(data).current_dir(source()).stdin(Stdio::null()).stdout(append("stdout.log")?).stderr(append("stderr.log")?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    fs::write(data.join("service.pid"), child.id().to_string())?;
    fs::write(data.join("service-binary"), binary.to_string_lossy().trim_start_matches("\\\\?\\"))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !healthy(bind) {
        if child.try_wait()?.is_some() {
            let log = fs::read_to_string(data.join("stderr.log")).unwrap_or_default();
            return Err(log[log.len().saturating_sub(4000)..].to_owned().into());
        }
        if Instant::now() > deadline {
            return Err("Benchmark service did not become healthy".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if staged.is_some_and(|s| same(&s, &binary)) {
        let _ = fs::remove_file(marker);
    }
    println!("Evaluator running at http://{bind} (PID {})", child.id());
    println!("Admin token: {}", data.join("admin-token").display());
    println!("Judge token: {}", data.join("judge-token").display());
    Ok(())
}

fn stop(data: &Path) -> Result<()> {
    let pid_file = data.join("service.pid");
    let pid: u32 = fs::read_to_string(&pid_file).map_err(|_| format!("No service PID file at {}", pid_file.display()))?.trim().parse()?;
    if executable(pid).is_some() {
        if running(data).is_none() {
            return Err("PID belongs to another executable; refusing to stop it.".into());
        }
        terminate(pid)?;
        while executable(pid).is_some() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fs::remove_file(pid_file)?;
    println!("Evaluator stopped. Database and submissions are retained.");
    Ok(())
}

fn setup(data: &Path, bind: &str, skip_build: bool) -> Result<()> {
    let target = workspace().join("runtime").join("build").join("benchmark");
    if !skip_build {
        let cargo = std::env::var_os("CARGO").map(PathBuf::from).unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo).args(["build", "--release", "--locked"]).current_dir(source()).env("CARGO_TARGET_DIR", &target).status()?;
        if !status.success() {
            return Err("cargo build failed".into());
        }
    }
    fs::create_dir_all(data)?;
    let profiles = data.join("coordinator-profiles.json");
    if !profiles.exists() {
        fs::write(&profiles, serde_json::to_string_pretty(&json!({"default":{"coordinator_url":"http://127.0.0.1:4242"}}))?)?;
    }
    start(data, bind, Some(target.join("release").join(format!("harness-bench{}", std::env::consts::EXE_SUFFIX))))
}

pub fn service(args: &[String]) -> Result<()> {
    let args = parse(args, &["--data", "--bind", "--binary"])?;
    let data = data_dir(&args);
    let bind = args.flags.get("--bind").cloned().unwrap_or_else(|| "127.0.0.1:8741".into());
    match args.positional.first().map(String::as_str) {
        Some("setup") => setup(&data, &bind, args.switches.iter().any(|s| s == "--skip-build")),
        Some("start") => start(&data, &bind, args.flags.get("--binary").map(PathBuf::from)),
        Some("stop") => stop(&data),
        _ => Err(USAGE.into()),
    }
}
