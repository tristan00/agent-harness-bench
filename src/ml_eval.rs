use crate::{Result, process::OwnedChild};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fs, io::Read, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}};

pub fn registry() -> HashMap<String, Value> {
    let entries: Vec<Value> = serde_json::from_str(include_str!("../ml_datasets.json")).expect("valid bundled ML registry");
    entries.into_iter().map(|e| (e["id"].as_str().unwrap_or("").to_owned(), e)).collect()
}

fn file_name(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

fn expected_hash<'a>(entry: &'a Value, name: &str) -> Option<&'a str> {
    let hashes = &entry["reference"]["test_sha256"];
    match hashes.as_str() {
        Some(single) => (entry["test"][0].as_str().map(file_name) == Some(name)).then_some(single),
        None => hashes[name].as_str(),
    }
}

pub fn fetch_test(entry: &Value, cache: &Path) -> Result<HashMap<String, PathBuf>> {
    fs::create_dir_all(cache)?;
    let mut files = HashMap::new();
    for url in entry["test"].as_array().ok_or("ML entry has no test files")?.iter().filter_map(Value::as_str) {
        let name = file_name(url);
        let path = cache.join(name);
        if !path.exists() {
            let response = ureq::get(url).set("User-Agent", "agent-harness-bench").timeout(Duration::from_secs(120)).call()?;
            let mut bytes = Vec::new();
            response.into_reader().read_to_end(&mut bytes)?;
            fs::write(&path, bytes)?;
        }
        let digest = format!("{:x}", Sha256::digest(fs::read(&path)?));
        if Some(digest.as_str()) != expected_hash(entry, name) {
            return Err(format!("Test file {name} does not match its recorded checksum").into());
        }
        files.insert(name.to_owned(), path);
    }
    Ok(files)
}

fn idx(data: &[u8]) -> &[u8] {
    let dims = data[3] as usize;
    &data[4 + 4 * dims..]
}

fn split_rows(text: &str, skip: usize) -> Vec<(String, String)> {
    text.lines().skip(skip).map(str::trim).filter(|l| !l.is_empty())
        .map(|l| l.rsplit_once(',').map_or((l.to_owned(), String::new()), |(h, t)| (h.to_owned(), t.to_owned()))).collect()
}

pub fn test_split(task: &str, files: &HashMap<String, PathBuf>, directory: &Path) -> Result<(PathBuf, Vec<i64>)> {
    fs::create_dir_all(directory)?;
    let get = |name: &str| files.get(name).ok_or_else(|| format!("missing test file {name}"));
    let features = directory.join("input");
    let write = |rows: &[(String, String)]| fs::write(&features, rows.iter().map(|(h, _)| format!("{h}\n")).collect::<String>());
    match task {
        "adult_income" => {
            let rows = split_rows(&fs::read_to_string(get("adult.test")?)?, 1);
            write(&rows)?;
            Ok((features.clone(), rows.iter().map(|(_, l)| i64::from(l.trim().trim_end_matches('.') == ">50K")).collect()))
        }
        "optical_digits" => {
            let rows = split_rows(&fs::read_to_string(get("optdigits.tes")?)?, 0);
            write(&rows)?;
            Ok((features.clone(), rows.iter().map(|(_, l)| l.trim().parse::<i64>()).collect::<std::result::Result<_, _>>()?))
        }
        "fashion_mnist" => {
            let features = directory.join("t10k-images-idx3-ubyte.gz");
            fs::copy(get("t10k-images-idx3-ubyte.gz")?, &features)?;
            let mut labels = Vec::new();
            flate2::read::GzDecoder::new(fs::File::open(get("t10k-labels-idx1-ubyte.gz")?)?).read_to_end(&mut labels)?;
            Ok((features, idx(&labels).iter().map(|b| i64::from(*b)).collect()))
        }
        _ => Err(format!("Unknown ML task {task}").into()),
    }
}

pub fn roc_auc(labels: &[i64], scores: &[f64]) -> Result<f64> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|a, b| scores[*a].total_cmp(&scores[*b]));
    let mut ranks = vec![0.0; scores.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && scores[order[j + 1]] == scores[order[i]] {
            j += 1;
        }
        for k in i..=j {
            ranks[order[k]] = (i + j) as f64 / 2.0 + 1.0;
        }
        i = j + 1;
    }
    let positives = labels.iter().filter(|l| **l != 0).count() as f64;
    let negatives = labels.len() as f64 - positives;
    if positives == 0.0 || negatives == 0.0 {
        return Err("ROC AUC needs both classes".into());
    }
    let positive_ranks: f64 = ranks.iter().zip(labels).filter(|(_, l)| **l != 0).map(|(r, _)| r).sum();
    Ok((positive_ranks - positives * (positives + 1.0) / 2.0) / (positives * negatives))
}

pub fn metric(name: &str, labels: &[i64], lines: &[String]) -> Result<f64> {
    if lines.len() != labels.len() {
        return Err(format!("Expected {} predictions, got {}", labels.len(), lines.len()).into());
    }
    let values: Vec<f64> = lines.iter().map(|l| l.parse::<f64>()).collect::<std::result::Result<_, _>>()?;
    match name {
        "roc_auc" => roc_auc(labels, &values),
        "accuracy" => Ok(values.iter().zip(labels).filter(|(v, l)| v.trunc() as i64 == **l).count() as f64 / labels.len() as f64),
        _ => Err(format!("Unknown metric {name}").into()),
    }
}

pub fn normalized(score: Option<f64>, reference: f64, trivial: f64) -> f64 {
    match score {
        Some(s) if s > trivial && s <= reference => 50.0 * (s - trivial) / (reference - trivial),
        Some(s) if s > reference => (50.0 + 50.0 * (s - reference) / (1.0 - reference)).min(100.0),
        _ => 0.0,
    }
}

fn materialize(submission: &Value, directory: &Path) -> Result<Value> {
    fs::create_dir_all(directory)?;
    for file in submission["files"].as_array().ok_or("submission has no files")? {
        let path = directory.join(file["path"].as_str().ok_or("file without path")?);
        fs::create_dir_all(path.parent().unwrap_or(directory))?;
        let bytes = match file["text"].as_str() {
            Some(text) => text.as_bytes().to_vec(),
            None => base64::engine::general_purpose::STANDARD.decode(file["base64"].as_str().unwrap_or(""))?,
        };
        fs::write(path, bytes)?;
    }
    Ok(submission["manifest"].clone())
}

fn predict(run: &Value, config: &Value, entry: &Value, data: &Path, directory: &Path, files: HashMap<String, PathBuf>, result: &mut Value) -> Result<f64> {
    let submission = crate::metrics::read(&data.join("submissions").join(run["submission_id"].as_str().ok_or("run has no submission")?).join("submission.json"))?;
    let (features, labels) = test_split(entry["id"].as_str().unwrap_or(""), &files, &directory.join("hidden"))?;
    let manifest = materialize(&submission, &directory.join("submission"))?;
    let template: Vec<&str> = manifest["predict"].as_array().filter(|a| !a.is_empty()).ok_or("Manifest has no predict command")?.iter().filter_map(Value::as_str).collect();
    let predictions = directory.join("predictions.txt");
    let argv: Vec<String> = template.iter().map(|a| a.replace("{input}", &features.display().to_string()).replace("{output}", &predictions.display().to_string())).collect();
    let python = PathBuf::from(config["ml_python"].as_str().ok_or("campaign has no ml_python")?);
    let path = std::env::join_paths(std::iter::once(python.parent().unwrap_or(Path::new("")).to_path_buf()).chain(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default()))?;
    let workdir = directory.join("submission");
    let found = std::env::split_paths(&path).find_map(|dir| {
        let plain = dir.join(&argv[0]);
        if cfg!(windows) { [plain.with_extension("exe"), plain.clone()].into_iter().find(|f| f.is_file() && f.extension().is_some()) } else { plain.is_file().then_some(plain) }
    });
    let program = found.unwrap_or_else(|| workdir.join(&argv[0]));
    let mut command = Command::new(program);
    command.env_clear();
    for (key, value) in std::env::vars_os() {
        if !key.to_string_lossy().starts_with("HARNESS_") {
            command.env(key, value);
        }
    }
    for (key, value) in manifest["env"].as_object().into_iter().flatten() {
        if !key.starts_with("HARNESS_") {
            command.env(key, value.as_str().unwrap_or(""));
        }
    }
    let log = fs::File::create(directory.join("predict.log"))?;
    command.env("PATH", &path).args(&argv[1..]).current_dir(&workdir).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    let limit = config["predict_seconds"].as_u64().unwrap_or(0);
    let started = Instant::now();
    let mut child = OwnedChild::launch(command)?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(limit) {
            return Err(format!("predict exceeded {limit} seconds").into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(child);
    result["predict_seconds"] = json!((started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0);
    if !status.success() {
        return Err(format!("predict exited with {}", status.code().unwrap_or(-1)).into());
    }
    let lines: Vec<String> = fs::read_to_string(&predictions)?.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    result["rows"] = json!(labels.len());
    metric(entry["metric"].as_str().unwrap_or(""), &labels, &lines)
}

pub fn evaluate_with(run: &Value, config: &Value, data: &Path, output: &Path, entry: &Value, files: impl FnOnce() -> Result<HashMap<String, PathBuf>>) -> Value {
    let (reference, trivial) = (entry["reference"]["score"].as_f64().unwrap_or(1.0), entry["reference"]["trivial"].as_f64().unwrap_or(0.0));
    let mut result = json!({"task":entry["id"], "metric":entry["metric"], "reference":reference, "trivial":trivial,
                            "reference_model":entry["reference"]["model"], "score":null, "error":null});
    let directory = output.join("ml").join(run["id"].as_str().unwrap_or(""));
    match files().and_then(|files| predict(run, config, entry, data, &directory, files, &mut result)) {
        Ok(score) => result["score"] = json!(score),
        Err(error) => result["error"] = json!(error.to_string()),
    }
    result["normalized"] = json!(normalized(result["score"].as_f64(), reference, trivial));
    result
}

pub fn evaluate(run: &Value, config: &Value, data: &Path, output: &Path) -> Value {
    let id = run["ml"]["task"]["id"].as_str().unwrap_or("");
    let Some(entry) = registry().remove(id) else {
        return json!({"task":id, "score":null, "error":format!("Unknown ML task {id}"), "normalized":0.0});
    };
    let cache = data.join("ml-data").join(id);
    evaluate_with(run, config, data, output, &entry.clone(), || fetch_test(&entry, &cache))
}
