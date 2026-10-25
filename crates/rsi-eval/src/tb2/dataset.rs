//! Pinned task definitions, checkout hashes, and dataset regeneration.
use super::{
    Result,
    split::{HEAVY_BYTES, SHARD_CAP_BYTES, Shard},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const COMMIT: &str = "2fd12b88aafdd04a52c298e3940bcb189f9766d6";
pub const REPO: &str = "https://github.com/harbor-framework/terminal-bench-2";
pub const HARBOR: &str = "0.24.0";
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Task {
    pub name: String,
    pub difficulty: String,
    pub category: String,
    pub cpus: u64,
    pub memory_mb: u64,
    pub agent_timeout_sec: f64,
    pub verifier_timeout_sec: f64,
    pub image: String,
    pub image_digest: String,
    pub image_bytes: u64,
    pub task_toml_sha256: String,
}
#[derive(Debug, Deserialize)]
pub struct Dataset {
    pub dataset: Header,
    #[serde(default)]
    pub task: Vec<Task>,
    #[serde(default)]
    pub shard: Vec<Shard>,
}
#[derive(Debug, Deserialize)]
pub struct Header {
    pub commit: String,
    pub repo: String,
    pub harbor_version: String,
    pub task_count: usize,
    pub shard_cap_bytes: u64,
}
pub fn load(path: &Path) -> Result<Dataset> {
    let mut doc: Dataset = toml::from_str(&fs::read_to_string(path)?)?;
    doc.task.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(doc)
}
pub fn task_hash(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"\r\n") {
            i += 1;
        }
        normalized.push(bytes[i]);
        i += 1;
    }
    Ok(format!("{:x}", Sha256::digest(normalized)))
}
pub fn task_dirs(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.path().join("task.toml").is_file() {
            names.push(
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "non-UTF-8 task name")?,
            );
        }
    }
    names.sort();
    Ok(names)
}
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub digest: String,
    pub bytes: u64,
}
pub fn manifest_info(doc: &serde_json::Value) -> Result<Manifest> {
    let entries = doc
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(doc));
    for e in entries {
        let p = &e["Descriptor"]["platform"];
        if p["os"] == "linux" && p["architecture"] == "amd64" {
            let m = &e["SchemaV2Manifest"];
            let mut bytes = m["config"]["size"]
                .as_u64()
                .ok_or("manifest config size missing")?;
            for layer in m["layers"].as_array().ok_or("manifest layers missing")? {
                bytes = bytes
                    .checked_add(
                        layer["size"]
                            .as_u64()
                            .ok_or("manifest layer size missing")?,
                    )
                    .ok_or("manifest size overflow")?;
            }
            return Ok(Manifest {
                digest: e["Descriptor"]["digest"]
                    .as_str()
                    .ok_or("manifest digest missing")?
                    .into(),
                bytes,
            });
        }
    }
    Err("no linux/amd64 manifest".into())
}

fn inspect_image(image: &str) -> Result<Manifest> {
    let mut child = Command::new("docker")
        .args(["manifest", "inspect", "-v", image])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("missing docker stdout")?;
    let stderr = child.stderr.take().ok_or("missing docker stderr")?;
    let read = |mut pipe: Box<dyn Read + Send>| -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    // Drain both pipes while waiting so a large registry response cannot block Docker.
    let stdout = std::thread::spawn(move || read(Box::new(stdout)));
    let stderr = std::thread::spawn(move || read(Box::new(stderr)));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < Duration::from_secs(180) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(match result {
                    Err(error) => error.to_string(),
                    _ => format!("docker manifest inspect {image} timed out after 180 seconds"),
                });
            }
        }
    };
    let stdout = stdout
        .join()
        .map_err(|_| "docker stdout reader panicked")??;
    let stderr = stderr
        .join()
        .map_err(|_| "docker stderr reader panicked")??;
    if !status?.success() {
        return Err(format!(
            "docker manifest inspect {image} failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        )
        .into());
    }
    manifest_info(&serde_json::from_slice(&stdout)?)
}
pub fn read_tasks(
    root: &Path,
    manifests: Option<&std::collections::BTreeMap<String, Manifest>>,
) -> Result<Vec<Task>> {
    let mut rows = Vec::new();
    for name in task_dirs(root)? {
        let path = root.join(&name).join("task.toml");
        let t: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
        validate_task(&name, &t)?;
        let string = |table: &str, key: &str| -> Result<String> {
            Ok(t.get(table)
                .and_then(|v| v.get(key))
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("{name}: missing {table}.{key}"))?
                .into())
        };
        let number = |table: &str, key: &str| -> Result<f64> {
            let v = t
                .get(table)
                .and_then(|v| v.get(key))
                .ok_or_else(|| format!("{name}: missing {table}.{key}"))?;
            v.as_float()
                .or_else(|| v.as_integer().map(|v| v as f64))
                .ok_or_else(|| format!("{name}: invalid {table}.{key}").into())
        };
        let image = string("environment", "docker_image")?;
        let info = if let Some(manifests) = manifests {
            let m = manifests
                .get(&image)
                .ok_or_else(|| format!("missing manifest: {image}"))?;
            Manifest {
                digest: m.digest.clone(),
                bytes: m.bytes,
            }
        } else {
            inspect_image(&image)?
        };
        rows.push(Task {
            name: name.clone(),
            difficulty: string("metadata", "difficulty")?,
            category: string("metadata", "category")?,
            cpus: resource(&name, &t, "cpus")?,
            memory_mb: resource(&name, &t, "memory_mb")?,
            agent_timeout_sec: number("agent", "timeout_sec")?,
            verifier_timeout_sec: number("verifier", "timeout_sec")?,
            image,
            image_digest: info.digest,
            image_bytes: info.bytes,
            task_toml_sha256: task_hash(&path)?,
        });
    }
    Ok(rows)
}

/// Admit only runtime settings implemented by the pinned single-service runner. Descriptive
/// task/metadata fields do not affect execution; future runtime settings must fail explicitly.
fn validate_task(name: &str, task: &toml::Value) -> Result<()> {
    let unsupported = |field: &str| format!("{name}: unsupported task.toml field {field}");
    let keys = |table: &toml::Value, prefix: &str, allowed: &[&str]| -> Result<()> {
        for key in table.as_table().ok_or_else(|| unsupported(prefix))?.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(unsupported(&format!("{prefix}{key}")).into());
            }
        }
        Ok(())
    };
    keys(
        task,
        "",
        &[
            "schema_version",
            "artifacts",
            "task",
            "metadata",
            "agent",
            "verifier",
            "environment",
            "solution",
        ],
    )?;
    for (section, allowed) in [
        ("agent", &["timeout_sec"][..]),
        ("verifier", &["timeout_sec", "env"][..]),
        (
            "environment",
            &[
                "docker_image",
                "cpus",
                "memory_mb",
                "storage_mb",
                "build_timeout_sec",
                "gpus",
                "allow_internet",
                "mcp_servers",
                "env",
            ][..],
        ),
        ("solution", &["env"][..]),
    ] {
        if let Some(table) = task.get(section) {
            keys(table, &format!("{section}."), allowed)?;
            if let Some(env) = table.get("env") {
                keys(env, &format!("{section}.env."), &[])?;
            }
        }
    }
    for (section, key, expected) in [
        // Harbor's pinned default is descriptive; other disk quotas are unsupported.
        ("environment", "storage_mb", toml::Value::Integer(10240)),
        ("environment", "gpus", toml::Value::Integer(0)),
        ("environment", "allow_internet", toml::Value::Boolean(true)),
        ("environment", "mcp_servers", toml::Value::Array(vec![])),
        ("", "artifacts", toml::Value::Array(vec![])),
        ("", "schema_version", toml::Value::String("1.1".into())),
    ] {
        let table = if section.is_empty() {
            Some(task)
        } else {
            task.get(section)
        };
        if table
            .and_then(|t| t.get(key))
            .is_some_and(|value| *value != expected)
        {
            return Err(unsupported(&format!("{section}.{key}")).into());
        }
    }
    if let Some(value) = task
        .get("environment")
        .and_then(|e| e.get("build_timeout_sec"))
    {
        let seconds = value
            .as_float()
            .or_else(|| value.as_integer().map(|n| n as f64));
        if seconds != Some(600.0) {
            return Err(unsupported("environment.build_timeout_sec").into());
        }
    }
    resource(name, task, "cpus")?;
    resource(name, task, "memory_mb")?;
    Ok(())
}

fn resource(name: &str, task: &toml::Value, key: &str) -> Result<u64> {
    let value = task.get("environment").and_then(|e| e.get(key));
    let valid = match value {
        Some(toml::Value::Integer(n)) => u64::try_from(*n).ok().filter(|n| *n > 0),
        Some(toml::Value::Float(n))
            if n.is_finite() && *n > 0.0 && n.fract() == 0.0 && *n < u64::MAX as f64 =>
        {
            Some(*n as u64)
        }
        _ => None,
    };
    valid.ok_or_else(|| {
        format!("{name}: environment.{key} must be a positive integral resource limit").into()
    })
}
// Keep the explicit field order and float spelling of the pinned definition.
pub fn render(rows: &[Task], shards: &[Shard]) -> String {
    let mut text = format!(
        "# Pinned Terminal-Bench 2.0 definition (#3175). Do not edit by hand:\n# regenerate with rsi-eval bench tb2 refresh-dataset; rsi-eval bench tb2 pin-check fails a run on any drift.\n\n[dataset]\nname = \"terminal-bench-2\"\nrepo = \"{REPO}\"\ncommit = \"{COMMIT}\"\nharbor_version = \"{HARBOR}\"\nplatform = \"linux/amd64\"\ntask_count = {}\nshard_cap_bytes = {SHARD_CAP_BYTES}\nheavy_image_bytes = {HEAVY_BYTES}\n",
        rows.len()
    );
    for r in rows {
        text += &format!(
            "\n[[task]]\nname = {:?}\ndifficulty = {:?}\ncategory = {:?}\ncpus = {}\nmemory_mb = {}\nagent_timeout_sec = {:?}\nverifier_timeout_sec = {:?}\nimage = {:?}\nimage_digest = {:?}\nimage_bytes = {}\ntask_toml_sha256 = {:?}\n",
            r.name,
            r.difficulty,
            r.category,
            r.cpus,
            r.memory_mb,
            r.agent_timeout_sec,
            r.verifier_timeout_sec,
            r.image,
            r.image_digest,
            r.image_bytes,
            r.task_toml_sha256
        );
    }
    for s in shards {
        text += &format!(
            "\n[[shard]]\nname = {:?}\nsplit = {:?}\nheavy = {}\nbytes = {}\ntasks = {:?}\n",
            s.name, s.split, s.heavy, s.bytes, s.tasks
        );
    }
    text
}
