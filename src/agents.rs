use serde_json::Value;
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
#[cfg(not(test))]
pub fn family() -> Option<&'static str> {
    *FAMILY.get_or_init(|| detect(|name| std::env::var(name).ok().filter(|v| !v.is_empty())))
}
#[cfg(test)]
pub fn family() -> Option<&'static str> {
    *FAMILY.get_or_init(|| Some("codex"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn launcher_detection_and_vendor_isolation() {
        assert_eq!(detect(env(&[("CLAUDECODE", "1")])), Some("claude"));
        assert_eq!(detect(env(&[("CODEX_THREAD_ID", "t")])), Some("codex"));
        assert_eq!(detect(env(&[("CLAUDECODE", "1"), ("CODEX_THREAD_ID", "t")])), None);
        assert_eq!(detect(env(&[("AGENT_FAMILY", "claude"), ("CODEX_THREAD_ID", "t")])), Some("claude"));
        assert_eq!(detect(env(&[])), None);
        assert!(allowed("sol_6_1_light", Some("codex")) && !allowed("sol_6_1_light", Some("claude")));
        assert!(allowed("claude_sonnet_5_5_low", Some("claude")) && !allowed("claude_sonnet_5_5_low", Some("codex")) && !allowed("claude_haiku_5_5_low", Some("claude")));
        assert!(!allowed("sol_6_1_light", None) && !allowed("luna_6_high", Some("codex")));
        assert!(!allowed("sol_light", Some("codex")) && !allowed("missing", Some("codex")));
    }
}
