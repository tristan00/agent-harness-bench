use serde::{Deserialize, Serialize};


#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    pub urls: Vec<String>,
    pub instructions: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Assignment {
    pub seed: u64,
    pub dataset: Dataset,
}

pub fn registry() -> Vec<Dataset> {
    serde_json::from_str(include_str!("../datasets.json")).expect("valid bundled dataset registry")
}

pub fn select(seed: u64) -> Assignment {
    let datasets = registry();
    let mut rng = seed;
    let index = crate::random(&mut rng) as usize % datasets.len();
    Assignment {
        seed,
        dataset: datasets[index].clone(),
    }
}

pub fn random_assignment() -> Assignment {
    select(crate::random_seed())
}
