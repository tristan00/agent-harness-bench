use crate::{Result, model::*};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub const MAX_UPLOAD: usize = 48 * 1024 * 1024;
const SKIPPED: [&str; 8] = [".git", "target", "node_modules", ".venv", "venv", "__pycache__", ".harness", ".env"];

pub fn snapshot(directory: &Path) -> Result<Submission> {
    let manifest = fs::read(directory.join("harness.json")).map_err(|_| "the delivery has no harness.json manifest at its root")?;
    let mut manifest: serde_json::Value = serde_json::from_slice(&manifest).map_err(|error| format!("harness.json is not valid JSON: {error}"))?;
    if let Some(fields) = manifest.as_object_mut() {
        fields.retain(|key, _| ["run", "predict", "build", "env", "source_files", "ui"].contains(&key.as_str()));
    }
    let manifest: Manifest = serde_json::from_value(manifest).map_err(|error| format!("harness.json is not a valid manifest: {error}"))?;
    let mut files = Vec::new();
    collect(directory, directory, &mut files)?;
    Ok(Submission { manifest, files })
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<FileData>) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry.file_type()?;
        let relative = entry.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/");
        if kind.is_symlink() {
            return Err(format!("symlinks cannot be delivered: {relative}").into());
        }
        if SKIPPED.contains(&name.as_str()) || name.ends_with(".pyc") || name.ends_with(".log") || name.starts_with(".env.") {
            continue;
        }
        if kind.is_dir() {
            collect(root, &entry.path(), files)?;
            continue;
        }
        let bytes = fs::read(entry.path())?;
        let (text, base64) = match String::from_utf8(bytes) {
            Ok(text) => (Some(text), None),
            Err(error) => (None, Some(STANDARD.encode(error.into_bytes()))),
        };
        files.push(FileData { path: relative, text, base64, executable: false });
    }
    Ok(())
}
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

pub fn safe_path(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 240 || value.contains(['\\', ':', '\0']) {
        return Err("file paths must be portable relative paths with '/' separators".into());
    }
    for part in value.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|c| c.is_control() || "<>\"|?*".contains(c))
            || [
                "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
                "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
                "LPT9",
            ]
            .contains(&stem.as_str())
        {
            return Err(format!("unsafe file path: {value}").into());
        }
    }
    Ok(())
}

fn is_source(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "rs" | "py"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "jsx"
            | "html"
            | "css"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cc"
            | "go"
            | "java"
            | "cs"
            | "lua"
            | "rb"
            | "gd"
            | "swift"
            | "kt"
            | "sh"
            | "ps1"
            | "sql"
            | "wgsl"
            | "glsl"
    )
}

pub fn save(root: &Path, mut input: Submission) -> Result<Artifact> {
    if input.files.is_empty() || input.files.len() > 1024 {
        return Err("submit 1..1024 files".into());
    }
    if input.manifest.run.is_empty() && input.manifest.predict.is_none() {
        return Err("manifest requires run or predict".into());
    }
    if input.manifest.predict.as_ref().is_some_and(|argv| !argv.iter().any(|a| a.contains("{input}")) || !argv.iter().any(|a| a.contains("{output}"))) {
        return Err("predict must contain {input} and {output}".into());
    }
    for cmd in [&input.manifest.run].into_iter().filter(|cmd| !cmd.is_empty()).chain(input.manifest.build.iter()).chain(input.manifest.predict.iter()) {
        if cmd.is_empty()
            || cmd.len() > 128
            || cmd
                .iter()
                .any(|s| s.is_empty() || s.len() > 8192 || s.contains('\0'))
        {
            return Err("commands must be nonempty argv arrays with bounded arguments".into());
        }
    }
    if input.manifest.env.len() > 64
        || input.manifest.env.iter().any(|(k, v)| {
            k.is_empty()
                || k.contains(['=', '\0'])
                || k.starts_with("HARNESS_")
                || v.len() > 8192
                || v.contains('\0')
        })
    {
        return Err("invalid environment (HARNESS_* is reserved)".into());
    }
    if input
        .manifest
        .ui
        .as_ref()
        .is_some_and(|s| s != "web" && s != "native")
    {
        return Err("ui must be web or native".into());
    }
    input.files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut names = BTreeSet::new();
    let mut sources: BTreeSet<String> = input.manifest.source_files.iter().cloned().collect();
    for s in &sources {
        safe_path(s)?;
    }
    let mut decoded = Vec::new();
    let mut bytes = 0;
    for file in &input.files {
        safe_path(&file.path)?;
        if !names.insert(file.path.to_ascii_lowercase()) {
            return Err("duplicate file paths, ignoring case".into());
        }
        let data = match (&file.text, &file.base64) {
            (Some(text), None) => text.as_bytes().to_vec(),
            (None, Some(b64)) => STANDARD.decode(b64)?,
            _ => return Err("each file needs exactly one of text or base64".into()),
        };
        bytes += data.len();
        if bytes > MAX_BYTES {
            return Err("decoded submission exceeds 32 MiB".into());
        }
        if is_source(&file.path) {
            sources.insert(file.path.clone());
        }
        decoded.push(data);
    }
    for source in &sources {
        if !input.files.iter().any(|f| &f.path == source) {
            return Err(format!("source file missing: {source}").into());
        }
    }
    if sources.is_empty() {
        return Err("source code is required to measure lines of code; list unusual languages in source_files".into());
    }
    let mut lines = 0;
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&input.manifest)?);
    for (file, data) in input.files.iter().zip(&decoded) {
        digest.update((file.path.len() as u64).to_le_bytes());
        digest.update(file.path.as_bytes());
        digest.update([u8::from(file.executable)]);
        digest.update((data.len() as u64).to_le_bytes());
        digest.update(data);
        if sources.contains(&file.path) {
            lines += std::str::from_utf8(data)?
                .lines()
                .filter(|s| !s.trim().is_empty())
                .count();
        }
    }
    let artifact = Artifact {
        id: crate::id(),
        sha256: format!("{:x}", digest.finalize()),
        lines_of_code: lines,
        source_files: sources.into_iter().collect(),
        bytes,
        manifest: input.manifest.clone(),
    };
    let dir = root.join("submissions").join(&artifact.id);
    fs::create_dir_all(&dir)?;
    let result = (|| -> Result<()> {
        fs::write(dir.join("submission.json"), serde_json::to_vec(&input)?)?;
        fs::write(
            dir.join("artifact.json"),
            serde_json::to_vec_pretty(&artifact)?,
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result?;
    Ok(artifact)
}

pub fn read(root: &Path, id: &str) -> Result<Submission> {
    // Only persisted server-generated UUIDs are accepted by callers.
    uuid::Uuid::parse_str(id)?;
    Ok(serde_json::from_slice(&fs::read(
        root.join("submissions").join(id).join("submission.json"),
    )?)?)
}

/// Materialize only the accepted files, never a developer's mutable workspace.
pub fn materialize(root: &Path, artifact: &Artifact, label: &str) -> Result<PathBuf> {
    let input = read(root, &artifact.id)?;
    let dir = root.join("work").join(format!("{label}-{}", crate::id()));
    fs::create_dir_all(&dir)?;
    for file in input.files {
        safe_path(&file.path)?;
        let path = dir.join(&file.path);
        fs::create_dir_all(path.parent().unwrap())?;
        let data = if let Some(text) = file.text {
            text.into_bytes()
        } else {
            STANDARD.decode(file.base64.unwrap())?
        };
        fs::write(&path, data)?;
        #[cfg(unix)]
        if file.executable {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_traversal_and_windows_special_paths() {
        for path in [
            "../x", "/tmp/x", "C:/x", "a\\b", "a/../b", "a//b", "nul.py", "a/COM1", "a.",
            "a:stream",
        ] {
            assert!(safe_path(path).is_err(), "{path}");
        }
        assert!(safe_path("src/picker.rs").is_ok());
    }
    #[test]
    fn count_and_snapshot_are_independent_of_workdir() {
        let root = crate::test_support::dir();
        let submission = Submission {
            manifest: Manifest {
                run: vec!["python".into(), "x.py".into()],
                build: None,
                predict: None,
                env: Default::default(),
                source_files: vec![],
                ui: None,
            },
            files: vec![FileData {
                path: "x.py".into(),
                text: Some("# comment\n\nprint('x')\n".into()),
                base64: None,
                executable: false,
            }],
        };
        let a = save(root.path(), submission.clone()).unwrap();
        let b = save(root.path(), submission).unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.lines_of_code, 2);
        let work = materialize(root.path(), &a, "test").unwrap();
        fs::write(work.join("x.py"), "changed").unwrap();
        assert_eq!(
            read(root.path(), &a.id).unwrap().files[0].text.as_deref(),
            Some("# comment\n\nprint('x')\n")
        );
    }
}

#[cfg(test)]
mod delivery_tests {
    #[test]
    fn deliveries_ignore_unused_manifest_fields_and_skip_generated_trees() {
        let root = crate::test_support::dir();
        let folder = root.path().join("delivery");
        std::fs::create_dir_all(folder.join("node_modules")).unwrap();
        std::fs::write(folder.join("harness.json"), r#"{"name":"Game","version":"1.0","run":["python","main.py"],"ui":"web"}"#).unwrap();
        std::fs::write(folder.join("main.py"), "print(1)").unwrap();
        std::fs::write(folder.join("node_modules").join("x.js"), "x").unwrap();
        let submission = super::snapshot(&folder).unwrap();
        assert_eq!((submission.manifest.run.clone(), submission.manifest.ui.clone()), (vec!["python".to_owned(), "main.py".to_owned()], Some("web".to_owned())));
        assert_eq!(submission.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["harness.json", "main.py"]);
        std::fs::write(folder.join("harness.json"), "not json").unwrap();
        assert!(super::snapshot(&folder).unwrap_err().to_string().contains("not valid JSON"));
    }
}
