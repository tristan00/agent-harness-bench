use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Benchmark {
    Chess960,
    GameBuild,
    DatasetDashboard,
    MazeSequence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub benchmark: Benchmark,
    pub name: String,
    pub development_seconds: u64,
    pub enrollment_seconds: u64,
    pub judge_seconds: u64,
    pub score_seconds: u64,
    pub build_seconds: u64,
    pub seed: u64,
    pub positions: u16,
    pub max_plies: u16,
    pub initial_elo: f64,
    pub elo_k: f64,
    pub baselines: bool,
    pub pool_run_ids: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            benchmark: Benchmark::Chess960,
            name: "benchmark".into(),
            development_seconds: 600,
            enrollment_seconds: 60,
            judge_seconds: 300,
            score_seconds: 120,
            build_seconds: 120,
            seed: 960,
            positions: 4,
            max_plies: 400,
            initial_elo: 1500.0,
            elo_k: 32.0,
            baselines: true,
            pool_run_ids: vec![],
        }
    }
}
impl Config {
    pub fn validate(&self) -> crate::Result<()> {
        if self.name.is_empty() || self.name.len() > 200 {
            return Err("name must have 1..200 bytes".into());
        }
        if self.pool_run_ids.len() > 16
            || self
                .pool_run_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.pool_run_ids.len()
            || self
                .pool_run_ids
                .iter()
                .any(|id| uuid::Uuid::parse_str(id).is_err())
        {
            return Err("pool_run_ids must contain at most 16 distinct run UUIDs".into());
        }
        if self.benchmark != Benchmark::Chess960 && !self.pool_run_ids.is_empty() {
            return Err("game campaigns do not use chess pools".into());
        }
        for (name, n, max) in [
            ("development_seconds", self.development_seconds, 86400),
            ("enrollment_seconds", self.enrollment_seconds, 86400),
            ("judge_seconds", self.judge_seconds, 3600),
            ("score_seconds", self.score_seconds, 3600),
            ("build_seconds", self.build_seconds, 1800),
        ] {
            if n == 0 || n > max {
                return Err(format!("{name} must be in 1..={max}").into());
            }
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
    pub themes: Vec<String>,
    pub created_at_ms: i64,
    pub enrollment_deadline_ms: i64,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<crate::datasets::Assignment>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub run: Vec<String>,
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
