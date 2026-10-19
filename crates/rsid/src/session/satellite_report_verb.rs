//! `AgentReportToHub` (#1103): queue one short typed report for the hub
//! manager. Satellite side only.
//!
//! Authority is the current, unrevoked appointed manager that is also the operator-declared
//! seat (or its rotation tip), resolved from the token-bound caller. Every
//! refusal is the same `satellite_report_not_authorized`. The report is
//! informational: it carries no authority on the hub.
//!
//! #276: a 40-hex SHA in the report text that no origin ref contains is a
//! warning in the receipt (`sha_not_on_origin: <sha>`), never a refusal: the
//! hub could not fetch that commit.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::satellite_dispatch::{
    AgentReportToHubReceiptV1, AgentReportToHubRequestV1, SATELLITE_REPORT_NOT_AUTHORIZED,
    SATELLITE_REPORT_SHA_NOT_ON_ORIGIN, report_text_full_shas,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

/// Wall budget for the whole origin check (one fetch plus the per-SHA
/// lookups). An inconclusive check warns about nothing.
const ORIGIN_CHECK_BUDGET: Duration = Duration::from_secs(20);
/// Share of the budget the fetch may take; the local lookups need little.
const ORIGIN_FETCH_BUDGET: Duration = Duration::from_secs(12);

impl AgentControlHandle {
    /// # Errors
    /// A stable `satellite_report_*` refusal, or a persistence error.
    pub async fn agent_report_to_hub(
        &self,
        caller: Uuid,
        request: AgentReportToHubRequestV1,
    ) -> Result<AgentReportToHubReceiptV1> {
        let (mut receipt, repo) = {
            let store = self.store.lock().await;
            let is_manager = store
                .agent_authority_projection(caller)
                .map(|projection| projection.is_manager)
                .unwrap_or(false);
            // A revoked scope or policy keeps `is_manager` true but ends the
            // appointment: require it to be current and unrevoked.
            if !is_manager || !store.manager_appointment_active(caller).unwrap_or(false) {
                return Err(DaemonError::PolicyDenied(
                    SATELLITE_REPORT_NOT_AUTHORIZED.into(),
                ));
            }
            let receipt = store.queue_hub_report(caller, &request)?;
            let repo = store
                .get_session(caller)
                .ok()
                .flatten()
                .map(|session| session.working_dir);
            (receipt, repo)
        };
        // The store lock is released: git runs without blocking the daemon.
        let shas = report_text_full_shas(&request.text);
        let Some(repo) = repo.filter(|_| !shas.is_empty()) else {
            return Ok(receipt);
        };
        receipt.warnings = shas_not_on_origin(&PathBuf::from(repo), &shas)
            .await
            .into_iter()
            .map(|sha| format!("{SATELLITE_REPORT_SHA_NOT_ON_ORIGIN}: {sha}"))
            .collect();
        Ok(receipt)
    }
}

/// The SHAs of `shas` that no `refs/remotes/origin/*` ref of the repository at
/// `repo` contains, after one bounded `git fetch origin`. A repository that is
/// not one, a fetch that fails (the remote-tracking refs are then as of the
/// last fetch) and a blown budget are not evidence: the first yields nothing,
/// the second checks the refs as they are, the third yields nothing.
async fn shas_not_on_origin(repo: &Path, shas: &[String]) -> Vec<String> {
    let check = async {
        if !git_succeeds(repo, &["rev-parse", "--git-dir"]).await {
            return Vec::new();
        }
        let _ = tokio::time::timeout(
            ORIGIN_FETCH_BUDGET,
            git_succeeds(repo, &["fetch", "--quiet", "--no-tags", "origin"]),
        )
        .await;
        let mut missing = Vec::new();
        for sha in shas {
            if !sha_on_origin(repo, sha).await {
                missing.push(sha.clone());
            }
        }
        missing
    };
    tokio::time::timeout(ORIGIN_CHECK_BUDGET, check)
        .await
        .unwrap_or_default()
}

/// Whether some origin remote-tracking ref contains `sha`. An object this
/// repository does not have is on no ref it can see.
async fn sha_on_origin(repo: &Path, sha: &str) -> bool {
    let output = hardened_git(repo)
        .args(["for-each-ref", "--count=1", "--format=%(refname)"])
        .arg(format!("--contains={sha}"))
        .arg("refs/remotes/origin")
        .output()
        .await;
    output.is_ok_and(|out| out.status.success() && !out.stdout.trim_ascii().is_empty())
}

/// Git run in `repo` with the repository's own hooks and fsmonitor off, no
/// credential prompt and the standard input closed: the daemon runs this in
/// the reporting seat's working tree, so a repository setting must not run
/// code in the daemon's name.
fn hardened_git(repo: &Path) -> Command {
    let mut git = Command::new("git");
    git.arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    git
}

async fn git_succeeds(repo: &Path, args: &[&str]) -> bool {
    hardened_git(repo)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    use super::shas_not_on_origin;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    use std::path::Path;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    use std::process::Command;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn only_shas_on_no_origin_ref_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin.git");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(
            root.path(),
            &["init", "-q", "--bare", origin.to_str().unwrap()],
        );
        git(&work, &["init", "-q", "-b", "rolling"]);
        git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&work, &["commit", "-q", "--allow-empty", "-m", "published"]);
        let published = git(&work, &["rev-parse", "HEAD"]);
        git(
            &work,
            &["push", "-q", "origin", "HEAD:refs/heads/rsi/worker"],
        );
        git(
            &work,
            &["commit", "-q", "--allow-empty", "-m", "local only"],
        );
        let local_only = git(&work, &["rev-parse", "HEAD"]);
        let unknown = "f".repeat(40);

        let missing = shas_not_on_origin(
            &work,
            &[published.clone(), local_only.clone(), unknown.clone()],
        )
        .await;
        assert_eq!(missing, vec![local_only.clone(), unknown]);

        // Once the worker branch is pushed the same SHA is no longer reported.
        git(
            &work,
            &["push", "-q", "origin", "HEAD:refs/heads/rsi/worker"],
        );
        assert!(
            shas_not_on_origin(&work, &[published, local_only])
                .await
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_directory_that_is_not_a_repository_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            shas_not_on_origin(dir.path(), &["a".repeat(40)])
                .await
                .is_empty()
        );
    }
}
