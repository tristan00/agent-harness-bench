use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn catalog() -> Value {
    serde_json::from_str(include_str!("../agents.json")).expect("valid bundled agent catalog")
}

pub fn detect(variable: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    if let Some(explicit) = variable("AGENT_FAMILY").filter(|v| !v.trim().is_empty()) {
        return match explicit.trim().to_ascii_lowercase().as_str() {
            "claude" => Some("claude"),
            "codex" => Some("codex"),
            _ => None,
        };
    }
    let claude = variable("CLAUDECODE").is_some_and(|v| v == "1") || variable("CLAUDE_CODE_ENTRYPOINT").is_some();
    let codex = ["CODEX_WINDOWS_REGISTERED_CORE", "CODEX_THREAD_ID", "CODEX_SANDBOX", "CODEX_SANDBOX_NETWORK_DISABLED", "CODEX_MANAGED_BY_NPM", "CODEX_MANAGED_BY_BUN"]
        .iter()
        .any(|name| variable(name).is_some());
    match (claude, codex) {
        (true, false) => Some("claude"),
        (false, true) => Some("codex"),
        _ => None,
    }
}

static FAMILY: OnceLock<Option<&'static str>> = OnceLock::new();
pub fn family() -> Option<&'static str> {
    *FAMILY.get_or_init(|| detect(|name| std::env::var(name).ok().filter(|v| !v.is_empty())))
}

pub fn allowed(id: &str, family: Option<&str>) -> bool {
    let catalog = catalog();
    let agent = &catalog[id];
    if !agent.is_object() || agent["retired"] == true {
        return false;
    }
    match agent["provider"].as_str() {
        Some("anthropic") => family == Some("claude"),
        Some("openai") => family == Some("codex"),
        _ => false,
    }
}

pub fn resolve(selection: &str, family: Option<&str>, retired: bool) -> crate::Result<Value> {
    let agent = catalog()[selection].clone();
    if !agent.is_object() {
        return Err(format!("Unknown agent {selection}").into());
    }
    let pair = (agent["provider"].as_str().unwrap_or(""), agent["harness"].as_str().unwrap_or(""));
    if !matches!(pair, ("openai", "codex") | ("anthropic", "claude_code")) || agent["model"].as_str().is_none_or(|m| m.trim().is_empty()) {
        return Err(format!("Agent {selection} must run in its own vendor's harness").into());
    }
    if agent["retired"] == true && !retired {
        return Err(format!("Agent {selection} is retired").into());
    }
    let vendor = if pair.0 == "anthropic" { "claude" } else { "codex" };
    if family != Some(vendor) {
        return Err(format!("Agent {selection} is not allowed when started by {}", family.unwrap_or("an undetected launcher")).into());
    }
    Ok(agent)
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into()).split(';').filter(|e| !e.is_empty()).map(str::to_owned).collect()
    } else {
        vec![]
    };
    std::env::split_paths(&path).find_map(|dir| {
        let plain = dir.join(name);
        if plain.is_file() && (!cfg!(windows) || plain.extension().is_some()) {
            return Some(plain);
        }
        extensions.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|file| file.is_file())
    })
}

pub fn claude_executable(found: Option<PathBuf>) -> String {
    let found = found.or_else(|| which("claude"));
    if let Some(found) = &found
        && found.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd"))
    {
        let native = found.parent().unwrap_or(Path::new("")).join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
        if native.is_file() {
            return native.display().to_string();
        }
    }
    found.map_or_else(|| "claude".into(), |f| f.display().to_string())
}

pub fn claude_arguments(agent: &Value) -> Vec<String> {
    let binary = std::env::var("CLAUDE_BIN").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| claude_executable(None));
    let mut argv = vec![binary, "-p".into(), "--model".into(), agent["model"].as_str().unwrap_or("").into(), "--output-format".into(), "stream-json".into(),
        "--verbose".into(), "--no-session-persistence".into(), "--dangerously-skip-permissions".into()];
    if let Some(effort) = agent["effort"].as_str().filter(|e| !e.is_empty()) {
        argv.extend(["--effort".into(), effort.into()]);
    }
    argv
}

pub fn codex_arguments(agent: &Value) -> Vec<String> {
    vec!["--model".into(), agent["model"].as_str().unwrap_or("").into(), "-c".into(), format!("model_reasoning_effort=\"{}\"", agent["effort"].as_str().unwrap_or(""))]
}

pub fn hosted_claude_variable(name: &str) -> bool {
    matches!(name, "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT") || ["CLAUDE_CODE_", "CLAUDE_AGENT_SDK", "CLAUDE_PREVIEW"].iter().any(|p| name.starts_with(p))
}
