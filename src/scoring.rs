use crate::{Result, process::OwnedChild};
use serde::Deserialize;
use serde_json::Value;
use std::{fs, path::Path, process::{Command, Stdio}, time::Duration};

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    max_judges: std::num::NonZeroUsize,
    timeout_seconds: std::num::NonZeroU64,
}

impl Default for Options {
    fn default() -> Self {
        Self { max_judges: std::num::NonZeroUsize::new(2).unwrap(), timeout_seconds: std::num::NonZeroU64::new(1800).unwrap() }
    }
}

pub fn run(root: &Path, url: &str, batch: &Value, options: Options) -> Result<Value> {
    let id = batch["id"].as_str().ok_or("Batch ID is missing")?;
    let coordinator = batch["profile_config"]["coordinator_url"].as_str().ok_or("Batch coordinator URL is missing")?;
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = source.join("scoring.json");
    let version: Value = serde_json::from_slice(&fs::read(&config)?)?;
    let output = root.join("batch-scores").join(id).join(version["version"].as_str().ok_or("Scoring version is missing")?);
    fs::create_dir_all(&output)?;
    let log = fs::File::create(output.join("scoring.log"))?;
    let mut command = Command::new(std::env::var("HARNESS_PYTHON").unwrap_or_else(|_| "python".into()));
    command.arg(source.join("scripts/score_batch.py")).arg(id)
        .arg("--data").arg(root).arg("--url").arg(url)
        .arg("--coordinator-url").arg(coordinator)
        .arg("--config").arg(&config)
        .arg("--max-judges").arg(options.max_judges.to_string())
        .arg("--timeout").arg(options.timeout_seconds.to_string())
        .env("AGENT_FAMILY", crate::agents::family().unwrap_or(""))
        .stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    let mut child = OwnedChild::launch(command)?;
    let status = loop {
        if let Some(status) = child.try_wait()? { break status; }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        return Err(format!("Scoring failed; inspect {}", output.join("scoring.log").display()).into());
    }
    let report: Value = serde_json::from_slice(&fs::read(output.join("scores.json"))?)?;
    if report["batch_id"] != id || report["schema_version"] != "benchmark-score-report.v2" {
        return Err("Scoring output identity or schema mismatch".into());
    }
    Ok(report)
}

#[test]
fn score_json_preserves_scorer_float_precision() {
    let report: Value = serde_json::from_str(r#"{"value":13.878533449866323,"share":0.45454545454545453}"#).unwrap();
    assert_eq!(report["value"].as_f64().unwrap(), 13.878533449866323_f64);
    assert_eq!(report["share"].as_f64().unwrap(), 0.45454545454545453_f64);
}
