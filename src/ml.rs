use serde::{Deserialize, Serialize};


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
    Assignment { seed, task: tasks[index].clone() }
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
