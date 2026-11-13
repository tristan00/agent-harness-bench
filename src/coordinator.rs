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

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("curl");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.args(["-q", "--silent", "--show-error", "--fail", "--max-time", "5", "--noproxy", "*"]).args(args).output()?;
    if !output.status.success() {
        return Err(format!("coordinator request failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(output.stdout)
}

fn plain(path: &str) -> String {
    path.trim_start_matches(r"\\?\").replace('/', r"\").trim_end_matches('\\').to_lowercase()
}

pub fn stop_workspace_tasks(root: &Path, id: &str) -> Result<()> {
    let path = root.join("coordinator").join(id).join("launch.json");
    if !path.exists() {
        return Ok(());
    }
    let launch: Value = serde_json::from_slice(&fs::read(path)?)?;
    let (Some(url), Some(workspace)) = (launch["profile"]["coordinator_url"].as_str(), launch["profile"]["workspace_path"].as_str()) else {
        return Ok(());
    };
    let workspace = plain(workspace);
    let state: Value = serde_json::from_slice(&curl(&[&format!("{url}/api/state")])?)?;
    for task in state["tasks"].as_array().into_iter().flatten() {
        let cwd = plain(task["cwd"].as_str().unwrap_or_default());
        let inside = cwd == workspace || cwd.starts_with(&format!("{workspace}\\"));
        if inside && task["stage"] != "done" && task["stopped"] != true {
            curl(&["-H", "Content-Type: application/json", "-d", "{}", &format!("{url}/api/tasks/{}/stop", task["id"].as_str().unwrap_or_default())])?;
        }
    }
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
        .env("AGENT_FAMILY", crate::agents::family().unwrap_or(""))
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

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_paths_compare_without_prefix_case_or_separator() {
        assert_eq!(super::plain(r"\\?\D:\Work\WS1\"), r"d:\work\ws1");
        assert_eq!(super::plain("D:/Work/ws1/chess960"), r"d:\work\ws1\chess960");
    }
}
