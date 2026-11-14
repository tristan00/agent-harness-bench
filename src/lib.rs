pub mod agents;
pub mod artifacts;
pub mod batch;
pub mod chess;
pub mod cli;
pub mod coordinator;
pub mod datasets;
pub mod engine {
    pub use bench_tooling::engine::*;
}
pub mod metrics;
pub mod ml;
pub mod ml_eval;
pub mod model;
pub mod process;
pub mod prompts;
pub mod service;
pub mod scoring;
pub mod storage;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_millis() as i64
}

pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn random_seed() -> u64 {
    let bytes = uuid::Uuid::new_v4();
    u64::from_le_bytes(bytes.as_bytes()[8..].try_into().unwrap())
}

/// Versioned, reproducible PRNG for scheduling and prompt theme selection.
pub fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}
