use crate::Result;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

#[derive(Clone)]
pub struct Workflow {
    pub url: String,
    pub session: String,
    pub deadline_ms: i64,
}

impl Workflow {
    pub fn from_record(record: &Value) -> Option<Self> {
        Some(Self { url: record["coordinator_url"].as_str()?.to_owned(), session: record["session"].as_str()?.to_owned(), deadline_ms: record["deadline_ms"].as_i64()? })
    }
}

fn curl(args: &[&str]) -> Result<Value> {
    let mut command = Command::new("curl");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.args(["-q", "--silent", "--show-error", "--max-time", "30", "--noproxy", "*"]).args(args).output()?;
    if !output.status.success() {
        return Err(format!("coordinator request failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| format!("coordinator returned non-JSON: {}", String::from_utf8_lossy(&output.stdout)))?;
    if let Some(error) = value["error"].as_str() {
        return Err(format!("coordinator rejected the request: {error}").into());
    }
    Ok(value)
}

fn post(url: &str, path: &str, body: &Value) -> Result<Value> {
    curl(&["-H", "Content-Type: application/json", "-d", &body.to_string(), &format!("{}{path}", url.trim_end_matches('/'))])
}

pub fn start(url: &str, workspace: &str, directory: &str, request: &str, output: &Path, callback: &str) -> Result<String> {
    let body = json!({"workspace":workspace,"directory":directory,"request":request,"output":output,"deliverable":crate::prompts::DELIVERABLE,"callback":callback});
    let session = post(url, "/api/sessions", &body)?;
    Ok(session["id"].as_str().ok_or("coordinator returned no session id")?.to_owned())
}

pub fn message(url: &str, session: &str, request: &str) -> Result<()> {
    post(url, &format!("/api/sessions/{session}/messages"), &json!({"request":request})).map(|_| ())
}

pub fn stop(url: &str, session: &str) -> Result<()> {
    post(url, &format!("/api/sessions/{session}/stop"), &json!({})).map(|_| ())
}

pub fn session(url: &str, session: &str) -> Result<Value> {
    curl(&[&format!("{}/api/sessions/{session}", url.trim_end_matches('/'))])
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

pub fn profile(root: &Path, name: &str) -> Result<Value> {
    let value = profiles(root)?.get(name).cloned().ok_or("unknown coordinator profile")?;
    if !value["coordinator_url"].as_str().is_some_and(|url| url.starts_with("http://127.0.0.1:")) {
        return Err("profile requires a loopback coordinator_url".into());
    }
    Ok(value)
}
