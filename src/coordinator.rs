use crate::{Result, now_ms, process::OwnedChild};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub struct Workflow {
    pub child: OwnedChild,
    pub deadline: Instant,
    pub deadline_ms: i64,
}

pub fn stop_shared(root: &Path, id: &str) -> Result<()> {
    let path = root.join("coordinator").join(id).join("launch.json");
    if !path.exists() { return Ok(()); }
    let launch: Value = serde_json::from_slice(&fs::read(path)?)?;
    let Some(url) = launch["profile"]["coordinator_url"].as_str() else { return Ok(()); };
    let mut command = Command::new("curl");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.args(["-q", "--silent", "--show-error", "--fail", "--max-time", "2", "--noproxy", "*",
        "-H", "Content-Type: application/json", "-d", "{}", &format!("{url}/api/benchmark-runs/{id}/stop")]).output()?;
    if !output.status.success() { return Err(format!("shared coordinator cleanup failed: {}", String::from_utf8_lossy(&output.stderr)).into()); }
    Ok(())
}

pub fn profiles(root: &Path) -> Result<Value> {
    let file = root.join("coordinator-profiles.json");
    if !file.exists() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_slice(&fs::read(file)?)?;
    if !value.is_object() {
        return Err("coordinator-profiles.json must be an object".into());
    }
    Ok(value)
}

pub fn argv(value: &Value) -> Result<Vec<String>> {
    let args: Vec<String> = serde_json::from_value(value.clone())?;
    if args.is_empty() || args.iter().any(|s| s.trim().is_empty()) {
        return Err("profile commands must be nonempty argv arrays".into());
    }
    Ok(args)
}

pub fn profile(root: &Path, name: &str) -> Result<Value> {
    let value = profiles(root)?
        .get(name)
        .cloned()
        .ok_or("unknown coordinator profile")?;
    for field in ["command", "coordinator_command", "planner_command"] {
        argv(&value[field])?;
    }
    for field in ["implementation", "review", "replan"] {
        argv(&value["workers"][field])?;
    }
    Ok(value)
}

pub fn launch(root: &Path, url: &str, started: &Value, profile: &Value) -> Result<Workflow> {
    let run = &started["run"];
    let id = run["id"].as_str().ok_or("missing run id")?;
    let deadline_ms = run["deadline_ms"].as_i64().ok_or("missing deadline")?;
    let remaining = deadline_ms - now_ms();
    if remaining <= 0 {
        return Err("development deadline passed".into());
    }
    let deadline = Instant::now() + Duration::from_millis(remaining as u64);
    let directory = root.join("coordinator").join(id);
    fs::create_dir_all(&directory)?;
    let launch_file = directory.join("launch.json");
    fs::write(
        &launch_file,
        serde_json::to_vec_pretty(&json!({"url":url,"started":started,"profile":profile}))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&launch_file, fs::Permissions::from_mode(0o600))?;
    }
    let args = argv(&profile["command"])?;
    let log = fs::File::create(directory.join("controller.log"))?;
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .current_dir(&directory)
        .env_remove("HARNESS_API_TOKEN")
        .env_remove("HARNESS_JUDGE_TOKEN")
        .env_remove("HARNESS_RUN_TOKEN")
        .env_remove("HARNESS_JUDGMENT_TOKEN")
        .env("HARNESS_WORKFLOW_REQUEST", &launch_file)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let child = OwnedChild::launch(command)?;
    Ok(Workflow {
        child,
        deadline,
        deadline_ms,
    })
}
