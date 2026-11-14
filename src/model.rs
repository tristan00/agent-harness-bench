use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Benchmark {
    Chess960,
    GameBuild,
    DatasetDashboard,
    MazeGame,
    MlPredictor,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub benchmark: Benchmark,
    pub development_seconds: u64,
    pub judge_seconds: u64,
    pub score_seconds: u64,
    pub build_seconds: u64,
    pub seed: u64,
    pub positions: u16,
    pub max_plies: u16,
    pub initial_elo: f64,
    pub elo_k: f64,
    pub parallel_games: u16,
    pub baselines: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub follow_ups: Vec<String>,
    pub predict_seconds: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ml_python: String,
}
pub const TIME_LIMITS: [&str; 5] = ["development_seconds", "judge_seconds", "score_seconds", "build_seconds", "predict_seconds"];

pub fn development_seconds(benchmark: Benchmark) -> u64 {
    match benchmark {
        Benchmark::Chess960 => 1200,
        Benchmark::DatasetDashboard | Benchmark::MlPredictor => 900,
        Benchmark::GameBuild | Benchmark::MazeGame => 1800,
    }
}

pub fn judge_seconds(benchmark: Benchmark) -> u64 {
    match benchmark {
        Benchmark::MazeGame => 600,
        Benchmark::MlPredictor => 180,
        Benchmark::Chess960 | Benchmark::GameBuild | Benchmark::DatasetDashboard => 300,
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            benchmark: Benchmark::Chess960,
            development_seconds: development_seconds(Benchmark::Chess960),
            judge_seconds: judge_seconds(Benchmark::Chess960),
            score_seconds: 120,
            build_seconds: 120,
            seed: crate::random_seed(),
            positions: 4,
            max_plies: 400,
            initial_elo: 1500.0,
            elo_k: 32.0,
            parallel_games: 8,
            baselines: true,
            follow_ups: vec![],
            predict_seconds: 60,
            ml_python: String::new(),
        }
    }
}
impl Config {
    pub fn new(benchmark: Benchmark, mut value: serde_json::Value) -> crate::Result<Self> {
        if !value.is_object() {
            return Err("task configuration must be an object".into());
        }
        if let Some(file) = value.as_object_mut().unwrap().remove("follow_ups_file") {
            value["follow_ups"] = serde_json::from_str(&std::fs::read_to_string(file.as_str().ok_or("follow_ups_file must be a path")?)?)?;
        }
        if let Some(key) = TIME_LIMITS.iter().find(|key| value.get(**key).is_some()) {
            return Err(format!("time limits are fixed per benchmark; remove {key} from the task configuration").into());
        }
        value["benchmark"] = serde_json::json!(benchmark);
        value["development_seconds"] = development_seconds(benchmark).into();
        value["judge_seconds"] = judge_seconds(benchmark).into();
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> crate::Result<()> {
        if (self.benchmark == Benchmark::MazeGame) == self.follow_ups.is_empty()
            || self.follow_ups.len() > 32
            || self.follow_ups.iter().any(|text| text.trim().is_empty() || text.len() > 4000)
        {
            return Err("maze_game campaigns require 1..32 nonblank follow_ups; other benchmarks take none".into());
        }
        for (name, n, max) in [
            ("development_seconds", self.development_seconds, 86400),
            ("judge_seconds", self.judge_seconds, 3600),
            ("score_seconds", self.score_seconds, 3600),
            ("build_seconds", self.build_seconds, 1800),
            ("predict_seconds", self.predict_seconds, 600),
        ] {
            if n == 0 || n > max {
                return Err(format!("{name} must be in 1..={max}").into());
            }
        }
        if (self.benchmark == Benchmark::MlPredictor) == self.ml_python.trim().is_empty() {
            return Err("ml_predictor campaigns require ml_python; other benchmarks take none".into());
        }
        if !(1..=64).contains(&self.parallel_games) {
            return Err("parallel_games must be in 1..=64".into());
        }
        if !(1..=960).contains(&self.positions)
            || !(2..=2000).contains(&self.max_plies)
            || !self.initial_elo.is_finite()
            || !self.elo_k.is_finite()
            || self.elo_k <= 0.0
            || self.elo_k > 100.0
        {
            return Err("invalid chess configuration".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Campaign {
    pub id: String,
    pub config: Config,
    pub created_at_ms: i64,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspiration: Option<crate::prompts::Inspiration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<crate::datasets::Assignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ml: Option<crate::ml::Assignment>,
    pub id: String,
    pub campaign_id: String,
    pub agent: String,
    pub status: String,
    pub started_at_ms: i64,
    pub deadline_ms: i64,
    pub submission_id: Option<String>,
    pub prepared_dir: Option<String>,
    pub error: Option<String>,
}

impl Run {
    pub fn new(campaign: &Campaign, agent: &str, inspiration: Option<crate::prompts::Inspiration>, dataset: Option<crate::datasets::Assignment>) -> Self {
        let now = crate::now_ms();
        Self {
            inspiration,
            dataset,
            ml: None,
            id: crate::id(),
            campaign_id: campaign.id.clone(),
            agent: agent.into(),
            status: "developing".into(),
            started_at_ms: now,
            deadline_ms: now + campaign.config.development_seconds as i64 * 1000,
            submission_id: None,
            prepared_dir: None,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub run: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predict: Option<Vec<String>>,
    #[serde(default)]
    pub build: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub source_files: Vec<String>,
    /// "web" exposes the assigned HARNESS_PORT, "native" is a desktop UI.
    #[serde(default)]
    pub ui: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileData {
    pub path: String,
    pub text: Option<String>,
    pub base64: Option<String>,
    #[serde(default)]
    pub executable: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub manifest: Manifest,
    pub files: Vec<FileData>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub sha256: String,
    pub lines_of_code: usize,
    pub source_files: Vec<String>,
    pub bytes: usize,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Judgment {
    pub id: String,
    pub run_id: String,
    pub status: String,
    pub judge: Option<String>,
    pub started_at_ms: Option<i64>,
    pub explore_deadline_ms: Option<i64>,
    pub score_deadline_ms: Option<i64>,
    pub ui_url: Option<String>,
    pub launch_error: Option<String>,
    pub actions: usize,
    pub successful_actions: usize,
    pub action_elapsed_ms: f64,
    pub peak_memory_bytes: u64,
    pub cpu_time_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    pub score: u8,
    pub evidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameScore {
    pub interface: Criterion,
    pub efficiency: Criterion,
    pub gameplay: Criterion,
    pub visuals: Criterion,
    pub testability: Criterion,
    pub compared_games: Vec<String>,
    pub notes: String,
}
impl GameScore {
    pub fn validate(&self) -> crate::Result<()> {
        for c in self.criteria() {
            if c.score > 5 || c.evidence.trim().is_empty() || c.evidence.len() > 10000 {
                return Err(
                    "each criterion requires a score in 0..5 and 1..10000 bytes of evidence".into(),
                );
            }
        }
        if self.compared_games.is_empty()
            || self.compared_games.len() > 10
            || self
                .compared_games
                .iter()
                .any(|s| s.trim().is_empty() || s.len() > 500)
            || self.notes.len() > 20000
        {
            return Err("provide 1..10 comparable published games and bounded notes".into());
        }
        Ok(())
    }
    pub fn criteria(&self) -> [&Criterion; 5] {
        [
            &self.interface,
            &self.efficiency,
            &self.gameplay,
            &self.visuals,
            &self.testability,
        ]
    }
    pub fn total(&self) -> u16 {
        self.criteria().iter().map(|c| u16::from(c.score)).sum()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestScore {
    pub request: usize,
    pub fraction: f64,
    pub evidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MazeScore {
    pub requests: Vec<RequestScore>,
    pub interface: Criterion,
    pub efficiency: Criterion,
    pub visuals: Criterion,
    pub testability: Criterion,
    pub notes: String,
}
impl MazeScore {
    pub fn criteria(&self) -> [&Criterion; 4] {
        [&self.interface, &self.efficiency, &self.visuals, &self.testability]
    }
    pub fn validate(&self, received: usize) -> crate::Result<()> {
        if self.criteria().iter().any(|c| c.score > 5 || c.evidence.trim().is_empty() || c.evidence.len() > 10000) {
            return Err("each quality criterion requires 0..5 and evidence".into());
        }
        if self.requests.len() != received
            || self.requests.iter().enumerate().any(|(i, r)| r.request != i + 1 || !(0.0..=1.0).contains(&r.fraction) || r.evidence.trim().is_empty() || r.evidence.len() > 10000)
        {
            return Err(format!("requests must list requests 1..{received} in order, each with a fraction in 0..1 and evidence").into());
        }
        if self.notes.len() > 20000 {
            return Err("notes are limited to 20000 bytes".into());
        }
        Ok(())
    }
    pub fn total(&self) -> u16 { self.criteria().iter().map(|c| u16::from(c.score)).sum() }
    pub fn progress(&self) -> f64 { self.requests.iter().map(|r| r.fraction).sum() }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardScore {
    pub completeness: Criterion,
    pub information_architecture: Criterion,
    pub prose_economy: Criterion,
    pub animation: Criterion,
    pub originality: Criterion,
    pub performance: Criterion,
    pub measurements: serde_json::Value,
    pub notes: String,
}
impl DashboardScore {
    pub fn criteria(&self) -> [&Criterion; 6] {
        [&self.completeness, &self.information_architecture, &self.prose_economy,
            &self.animation, &self.originality, &self.performance]
    }
    pub fn validate(&self) -> crate::Result<()> {
        if self.criteria().iter().any(|c| c.score > 5 || c.evidence.trim().is_empty() || c.evidence.len() > 10000) {
            return Err("each dashboard criterion requires 0..5 and evidence".into());
        }
        if !self.measurements.is_object() || self.notes.len() > 20000 {
            return Err("dashboard measurements must be an object with bounded notes".into());
        }
        Ok(())
    }
    pub fn total(&self) -> u16 { self.criteria().iter().map(|c| u16::from(c.score)).sum() }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdaScore {
    pub thoroughness: Criterion,
    pub prose_economy: Criterion,
    pub presentation: Criterion,
    pub information_architecture: Criterion,
    pub notes: String,
}
impl EdaScore {
    pub fn criteria(&self) -> [&Criterion; 4] {
        [&self.thoroughness, &self.prose_economy, &self.presentation, &self.information_architecture]
    }
    pub fn validate(&self) -> crate::Result<()> {
        if self.criteria().iter().any(|c| c.score > 5 || c.evidence.trim().is_empty() || c.evidence.len() > 10000) || self.notes.len() > 20000 {
            return Err("each EDA criterion requires 0..5 and evidence, with bounded notes".into());
        }
        Ok(())
    }
    pub fn total(&self) -> u16 { self.criteria().iter().map(|c| u16::from(c.score)).sum() }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn time_limits_come_only_from_the_benchmark() {
        let follow = serde_json::json!({"follow_ups":["Next."]});
        let ml = serde_json::json!({"ml_python":std::env::current_exe().unwrap()});
        for (benchmark, value, development, judge) in [
            (Benchmark::Chess960, serde_json::json!({}), 1200, 300),
            (Benchmark::GameBuild, serde_json::json!({}), 1800, 300),
            (Benchmark::DatasetDashboard, serde_json::json!({}), 900, 300),
            (Benchmark::MazeGame, follow.clone(), 1800, 600),
            (Benchmark::MlPredictor, ml.clone(), 900, 180),
        ] {
            let config = Config::new(benchmark, value).unwrap();
            assert_eq!((config.development_seconds, config.judge_seconds), (development, judge));
        }
        for key in TIME_LIMITS {
            let error = Config::new(Benchmark::Chess960, serde_json::json!({key: 1200})).unwrap_err().to_string();
            assert!(error.contains(key), "{error}");
        }
    }
}
