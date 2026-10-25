//! Checkout and Docker image drift checks.
use super::{
    Result,
    dataset::{self, Task},
};
use std::{collections::BTreeSet, path::Path, process::Command};
pub fn checkout(root: &Path, commit: &str, tasks: &[Task]) -> Result<Vec<String>> {
    let have: BTreeSet<_> = dataset::task_dirs(root)?.into_iter().collect();
    let want: BTreeSet<_> = tasks.iter().map(|t| t.name.clone()).collect();
    let mut drift = Vec::new();
    for name in want.difference(&have) {
        drift.push(format!("task missing from checkout: {name}"));
    }
    for name in have.difference(&want) {
        drift.push(format!("task not in the pinned set: {name}"));
    }
    for t in tasks.iter().filter(|t| have.contains(&t.name)) {
        let got = dataset::task_hash(&root.join(&t.name).join("task.toml"))?;
        if got != t.task_toml_sha256 {
            drift.push(format!(
                "task.toml changed: {} (pinned {}, found {got})",
                t.name, t.task_toml_sha256
            ));
        }
    }
    if !root.join(".git").exists() {
        return Err("checkout has no .git; cannot verify the pinned commit".into());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "cannot verify checkout commit: git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    check_commit(&String::from_utf8_lossy(&out.stdout), commit, &mut drift);
    Ok(drift)
}
fn check_commit(head: &str, commit: &str, drift: &mut Vec<String>) {
    if head.trim() != commit {
        drift.push(format!(
            "checkout commit {} is not the pinned {commit}",
            head.trim()
        ));
    }
}
pub fn image_repo_digests(tag: &str) -> Result<Option<Vec<String>>> {
    let out = Command::new("docker")
        .args(["image", "inspect", "--format", "{{json .RepoDigests}}", tag])
        .output()?;
    parse_digests(
        tag,
        out.status.success(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    )
}
pub(crate) fn parse_digests(
    tag: &str,
    success: bool,
    stdout: &str,
    stderr: &str,
) -> Result<Option<Vec<String>>> {
    if !success {
        if stderr.contains("No such image") || stderr.contains("No such object") {
            return Ok(None);
        }
        return Err(format!("docker image inspect {tag}: {}", stderr.trim()).into());
    }
    Ok(Some(
        serde_json::from_str::<Option<Vec<String>>>(if stdout.trim().is_empty() {
            "[]"
        } else {
            stdout.trim()
        })?
        .unwrap_or_default(),
    ))
}
pub fn images(
    tasks: &[Task],
    names: &[String],
    require_pulled: bool,
    mut inspect: impl FnMut(&str) -> Result<Option<Vec<String>>>,
) -> Result<Vec<String>> {
    let mut drift = Vec::new();
    for t in tasks {
        if !names.is_empty() && !names.contains(&t.name) {
            continue;
        }
        let Some(digests) = inspect(&t.image)? else {
            if require_pulled {
                drift.push(format!("image not pulled: {} ({})", t.image, t.name));
            }
            continue;
        };
        if !digests
            .iter()
            .any(|d| d.ends_with(&format!("@{}", t.image_digest)))
        {
            // Docker RepoDigests use the registry reference alphabet (no quotes).
            // Preserve the legacy list spelling in the public drift line.
            let local = digests
                .iter()
                .map(|d| format!("'{d}'"))
                .collect::<Vec<_>>()
                .join(", ");
            drift.push(format!(
                "image digest drift: {} ({}) pinned {}, local RepoDigests [{local}]",
                t.image, t.name, t.image_digest
            ));
        }
    }
    Ok(drift)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commit_and_docker_responses() {
        let mut drift = Vec::new();
        check_commit("wrong\n", "pinned", &mut drift);
        assert_eq!(
            drift,
            vec!["checkout commit wrong is not the pinned pinned"]
        );
        assert_eq!(
            parse_digests("x", false, "", "No such image").unwrap(),
            None
        );
        assert_eq!(
            parse_digests("x", false, "", "No such object").unwrap(),
            None
        );
        assert_eq!(parse_digests("x", true, "null", "").unwrap(), Some(vec![]));
        assert_eq!(parse_digests("x", true, "", "").unwrap(), Some(vec![]));
        assert!(parse_digests("x", false, "", "daemon unavailable").is_err());
        assert!(parse_digests("x", true, "bad json", "").is_err());
    }
}
