use serde::{Deserialize, Serialize};

pub const VERSION: &str = "2026-11-12.v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    pub urls: Vec<String>,
    pub instructions: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Assignment {
    pub registry_version: String,
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
        registry_version: VERSION.into(),
        seed,
        dataset: datasets[index].clone(),
    }
}

pub fn random_assignment() -> Assignment {
    select(crate::random_seed())
}

#[cfg(test)]
mod tests {
    #[test]
    fn selections_are_reproducible_and_cover_the_rotation() {
        let mut ids = std::collections::HashSet::new();
        for seed in 0..100 {
            let assignment = super::select(seed);
            assert_eq!(assignment, super::select(seed));
            ids.insert(assignment.dataset.id);
            assert!(
                assignment
                    .dataset
                    .urls
                    .iter()
                    .all(|url| url.starts_with("https://") && !url.contains(['<', '{']))
            );
        }
        assert_eq!(ids.len(), super::registry().len());
    }
}
