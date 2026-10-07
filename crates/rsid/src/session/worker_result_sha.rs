//! #1494: verify worker commit reports against local Git objects before an
//! owner observes terminal truth. Raw provider events remain immutable; the
//! diagnostic carries verified replacements for the final-message projection.

use std::{path::Path, sync::LazyLock, time::Duration};

use regex::Regex;
use rsi_common::types::{
    ConversationEvent, EventType, NewSessionDiagnosticV1, Role, SessionDiagnosticLevelV1,
    SessionStatus,
};
use rusqlite::OptionalExtension;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::super::types::{CompletedSession, PersistenceHandle};
use crate::{
    process_control::{CaptureLimits, capture_bounded},
    store::Store,
};

pub(crate) const RESULT_SHA_UNKNOWN: &str = "result_sha_unknown";
const DIAGNOSTIC: &str = "worker_result_sha_validation";
const MAX_REPORT_SHAS: usize = 12;

static REPORT_SHA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?m)^[\t ]*(?:RESULT[\t ]+(?:commit=|sha=)?|(?:\#{1,6}[\t ]*)?PIPELINE HANDOFF — (?:[A-Z]+[\t ]+)?|(?i:commit|sha)[\t ]*[:=][\t ]*)([0-9a-fA-F]{7,40})\b"
).expect("worker report SHA regex")
});

fn tokens(report: &str) -> Vec<&str> {
    REPORT_SHA
        .captures_iter(report)
        .filter_map(|caps| caps.get(1).map(|sha| sha.as_str()))
        .collect()
}

/// Local reads only, with bounded output, wall time and process cleanup.
async fn git(root: &Path, args: &[&str], deadline: tokio::time::Instant) -> Option<String> {
    let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
    let mut command = Command::new("git");
    command
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
            "-c",
            "credential.helper=",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ] {
        command.env_remove(key);
    }
    let mut limits = CaptureLimits::local_tool();
    limits.execution_timeout = remaining.min(Duration::from_secs(3));
    limits.max_stdout_bytes = 4096;
    limits.max_stderr_bytes = 4096;
    let out = capture_bounded(command, limits, &CancellationToken::new())
        .await
        .ok()?;
    if !out.status.success() || out.stdout_truncated || out.stderr_truncated {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|text| text.trim().to_owned())
}

async fn objects(root: &Path, sha: &str, deadline: tokio::time::Instant) -> Option<Vec<String>> {
    let text = git(
        root,
        &["rev-parse", &format!("--disambiguate={sha}")],
        deadline,
    )
    .await?;
    Some(text.lines().map(str::to_owned).collect())
}

/// A missing long hash may be repaired from its first seven characters only
/// when exactly one local object matches and that object is a commit. Do not
/// peel tags or choose among ambiguous objects (even commit/blob ambiguity).
async fn resolve_within(
    root: &Path,
    reported: &str,
    deadline: tokio::time::Instant,
) -> (Option<String>, bool) {
    let sha = reported.to_ascii_lowercase();
    let Some(mut candidates) = objects(root, &sha, deadline).await else {
        return (None, true);
    };
    let missing = candidates.is_empty();
    if missing && sha.len() > 7 {
        let Some(prefix_candidates) = objects(root, &sha[..7], deadline).await else {
            return (None, true);
        };
        candidates = prefix_candidates;
    }
    if candidates.len() != 1 {
        return (None, true);
    }
    let resolved = candidates.pop().unwrap();
    if git(root, &["cat-file", "-t", &resolved], deadline)
        .await
        .as_deref()
        != Some("commit")
    {
        return (None, true);
    }
    (Some(resolved), missing)
}

pub(crate) async fn validate_terminal_report(
    completed: &mut CompletedSession,
    persistence: &PersistenceHandle,
) {
    let session = &mut completed.session;
    if session.parent_id.is_none()
        || !rsi_common::is_leaf_kind(session.session_kind)
        || !session.status.is_terminal()
    {
        return;
    }
    let Some(event) = completed
        .events
        .iter()
        .rev()
        .take_while(|event| {
            !(event.event_type == EventType::Message && event.role == Some(Role::User))
        })
        .find(|event| {
            event.event_type == EventType::Message
                && event.role == Some(Role::Assistant)
                && !rsi_common::agent_session_events::is_provider_diagnostic_metadata(
                    event.metadata.as_deref(),
                )
        })
    else {
        return;
    };
    if !super::has_result_line(&event.content) {
        return;
    }
    let shas = tokens(&event.content);
    if shas.is_empty() {
        return;
    }
    let mut replacements = Vec::new();
    let mut unknown = shas.len() > MAX_REPORT_SHAS;
    if let Some(root) = session.sandbox_root.as_deref() {
        // One total probe budget per report, regardless of how many hashes it names.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        for reported in shas.iter().take(MAX_REPORT_SHAS) {
            let (resolved, invalid) = resolve_within(Path::new(root), reported, deadline).await;
            unknown |= invalid;
            if let Some(resolved) = resolved
                && resolved != *reported
            {
                replacements.push(serde_json::json!({"reported":reported,"resolved":resolved}));
            }
        }
    } else {
        unknown = true;
    }
    if unknown && session.status == SessionStatus::Completed {
        session.stop_reason = Some(RESULT_SHA_UNKNOWN.into());
    }
    if unknown || !replacements.is_empty() {
        let diagnostic = NewSessionDiagnosticV1 {
            session_id: session.id,
            timestamp: chrono::Utc::now(),
            level: SessionDiagnosticLevelV1::Warn,
            message: DIAGNOSTIC.into(),
            fields: Some(
                serde_json::json!({"code":if unknown { RESULT_SHA_UNKNOWN } else { "result_sha_verified" }, "event_sequence":event.sequence, "replacements":replacements}),
            ),
        };
        if let Err(error) = persistence.insert_session_diagnostic(diagnostic).await {
            tracing::warn!(session_id=%session.id, %error, "Could not persist worker SHA verification");
        }
    }
}

/// Project only replacements verified for this exact event. Keep the raw
/// transcript intact, including the originally reported hash.
pub(crate) fn normalized_report(
    store: &Store,
    event: &ConversationEvent,
) -> crate::error::Result<Option<String>> {
    let fields: Option<String> = store.conn.query_row(
        "SELECT fields_json FROM session_diagnostics WHERE session_id=?1 AND message=?2 AND json_extract(fields_json,'$.event_sequence')=?3 ORDER BY id DESC LIMIT 1",
        rusqlite::params![event.session_id.to_string(), DIAGNOSTIC, event.sequence], |row| row.get(0),
    ).optional()?;
    let Some(fields) =
        fields.and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return Ok(None);
    };
    let Some(replacements) = fields["replacements"].as_array() else {
        return Ok(None);
    };
    let mut report = event.content.clone();
    let spans: Vec<_> = REPORT_SHA
        .captures_iter(&report)
        .filter_map(|caps| {
            caps.get(1)
                .map(|sha| (sha.start(), sha.end(), sha.as_str().to_owned()))
        })
        .collect();
    for (start, end, reported) in spans.into_iter().rev() {
        if let Some(resolved) = replacements
            .iter()
            .find(|pair| pair["reported"].as_str() == Some(&reported))
            .and_then(|pair| pair["resolved"].as_str())
        {
            report.replace_range(start..end, resolved);
        }
    }
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn resolve(root: &Path, reported: &str) -> (Option<String>, bool) {
        resolve_within(
            root,
            reported,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await
    }
    fn git_fixture(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn result_sha_resolves_commits_and_repairs_fabricated_suffix_only_when_unique() {
        let dir = crate::test_support::disk_backed_tempdir("result-sha-resolution");
        let root = dir.path();
        git_fixture(root, &["init", "-q"]);
        git_fixture(root, &["commit", "--allow-empty", "-qm", "fixture"]);
        let head = git_fixture(root, &["rev-parse", "HEAD"]);
        assert_eq!(resolve(root, &head).await, (Some(head.clone()), false));
        assert_eq!(resolve(root, &head[..7]).await, (Some(head.clone()), false));
        let invented = format!(
            "{}{}",
            &head[..7],
            if head[7..].chars().all(|c| c == '0') {
                "1".repeat(33)
            } else {
                "0".repeat(33)
            }
        );
        assert_eq!(resolve(root, &invented).await, (Some(head), true));
        assert_eq!(resolve(root, "0000000").await, (None, true));
        std::fs::write(root.join("blob"), "blob").unwrap();
        let blob = git_fixture(root, &["hash-object", "-w", "blob"]);
        assert_eq!(resolve(root, &blob).await, (None, true));
        // Two real blob objects with the same seven-digit SHA-1 prefix.
        for i in [8167, 18833] {
            std::fs::write(root.join("blob"), format!("1494 ambiguous {i}\n")).unwrap();
            git_fixture(root, &["hash-object", "-w", "blob"]);
        }
        assert_eq!(resolve(root, "918063c").await, (None, true));
        assert_eq!(
            resolve(root, "918063c0000000000000000000000000000000000").await,
            (None, true)
        );
        assert_eq!(
            resolve(&root.join("missing"), &invented).await,
            (None, true)
        );
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn result_sha_tokens_select_report_identity_and_baton() {
        assert_eq!(
            tokens(
                "PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT abcdefabcdefabcd status=green\nTests: deadbeef\ncommit: 1234567"
            ),
            vec!["abcdefabcdefabcd", "1234567"]
        );
        assert_eq!(
            tokens("PIPELINE HANDOFF — BATON 1234567\n"),
            vec!["1234567"]
        );
        assert_eq!(
            tokens("RESULT commit=1234567\nRESULT sha=abcdefa"),
            vec!["1234567", "abcdefa"]
        );
        assert_eq!(
            tokens("RESULT 12345678901234567890123456789012345678901"),
            Vec::<&str>::new()
        );
    }
}
