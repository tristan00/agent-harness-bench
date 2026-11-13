use serde::{Deserialize, Serialize};

pub const VERSION: &str = "2026-11-13.v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    pub problem: String,
    pub target: String,
    pub train: Vec<String>,
    pub input_format: String,
    pub output_format: String,
    pub metric: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Assignment {
    pub registry_version: String,
    pub seed: u64,
    pub task: Task,
}

pub fn registry() -> Vec<Task> {
    serde_json::from_str(include_str!("../ml_datasets.json")).expect("valid bundled ML registry")
}

pub fn select(seed: u64) -> Assignment {
    let tasks = registry();
    let mut rng = seed;
    let index = crate::random(&mut rng) as usize % tasks.len();
    Assignment { registry_version: VERSION.into(), seed, task: tasks[index].clone() }
}

pub fn random_assignment() -> Assignment {
    select(crate::random_seed())
}

pub fn metric_name(metric: &str) -> &str {
    match metric {
        "roc_auc" => "ROC AUC",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn assignments_are_reproducible_cover_every_task_and_hide_test_data() {
        let mut ids = std::collections::HashSet::new();
        for seed in 0..60 {
            let assignment = super::select(seed);
            assert_eq!(assignment, super::select(seed));
            let shown = serde_json::to_string(&assignment).unwrap();
            assert!(!shown.contains("test") && !shown.contains("t10k") && !shown.contains("reference"), "{shown}");
            ids.insert(assignment.task.id);
        }
        assert_eq!(ids.len(), super::registry().len());
    }
}
