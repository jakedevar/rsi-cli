//! Hub-side storage for the external harness-report intake (#3789).
//!
//! The mapping from a GitHub report to its hub Issue lives in
//! `daemon_settings` (no schema migration):
//! - `external_report:gh:<repo>#<number>` -> `{"issue_id", "linked", "pending_write_back"}`:
//!   imports nothing on replay, but retries the receipt until acknowledged;
//! - `external_report:sig:<signature>` -> the hub Issue id the first report of that
//!   failure signature became, so a duplicate links to it instead of filing again;
//! - `external_report:status:v1` -> the last-run [`SyncStatusV1`].
//!
//! One IMMEDIATE transaction covers the dedupe lookup, the Issue create and both
//! mapping rows, so a crash cannot leave an Issue without its mapping.

use super::Store;
use super::issues::{IssueWriteActor, issue_write_before_commit};
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rsi_common::external_report::{HUB_LABEL, HubIssueDraft, SyncStatusV1};
use rsi_common::types::{Issue, NewIssue};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const STATUS_KEY: &str = "external_report:status:v1";
const EXTERNAL_REPORT_NAMESPACE: Uuid = Uuid::from_u128(0x3789_e7e2_0001_4a5b_9c1d_5e1f_a0b0_c0d1);

#[derive(Serialize, Deserialize)]
struct ReportLink {
    issue_id: Uuid,
    linked: bool,
    // Old mappings lack a receipt. Retry once rather than lose a failed link.
    #[serde(default = "pending_by_default")]
    pending_write_back: bool,
}

fn pending_by_default() -> bool {
    true
}

/// A durable GitHub receipt still waiting to be posted.
pub struct ExternalReportWriteBack {
    pub number: u64,
    pub hub_number: i64,
    pub linked_duplicate: bool,
}

/// What importing one GitHub report did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalImportOutcome {
    /// A new hub Issue was filed.
    Created(Issue),
    /// A hub Issue already covers this failure signature or title; the report
    /// was linked to it and nothing was filed.
    LinkedDuplicate(Issue),
    /// This GitHub report was imported or linked by an earlier run.
    AlreadyHandled { issue_id: Uuid },
}

fn report_key(repo: &str, number: u64) -> String {
    format!("external_report:gh:{repo}#{number}")
}

fn signature_key(signature: &str) -> String {
    format!("external_report:sig:{signature}")
}

fn upsert(tx: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO daemon_settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![
            key,
            value,
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
        ],
    )?;
    Ok(())
}

fn read(tx: &Transaction<'_>, key: &str) -> Result<Option<String>> {
    Ok(tx
        .query_row(
            "SELECT value FROM daemon_settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

fn record_link(tx: &Transaction<'_>, key: &str, issue_id: Uuid, linked: bool) -> Result<()> {
    let value = ReportLink {
        issue_id,
        linked,
        pending_write_back: true,
    };
    upsert(tx, key, &serde_json::to_string(&value)?)
}

impl Store {
    /// Bounded independently of the open-report listing, so even a report
    /// closed on GitHub after import can receive its pending hub link.
    pub fn pending_external_report_write_backs(
        &self,
        repo: &str,
        limit: u32,
    ) -> Result<Vec<ExternalReportWriteBack>> {
        let prefix = format!("external_report:gh:{repo}#");
        let mut statement = self.conn.prepare(
            "SELECT key, value FROM daemon_settings WHERE substr(key, 1, length(?1)) = ?1 \
             AND COALESCE(json_extract(value, '$.pending_write_back'), 1) = 1 ORDER BY updated_at, key LIMIT ?2"
        )?;
        let rows = statement.query_map(params![prefix, limit.min(25)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut pending = Vec::new();
        for row in rows {
            let (key, raw) = row?;
            let link: ReportLink = serde_json::from_str(&raw)?;
            let number = key
                .strip_prefix(&prefix)
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| DaemonError::Store("invalid external report number".into()))?;
            let issue = self
                .get_issue(link.issue_id)?
                .ok_or_else(|| DaemonError::Store("external report Issue missing".into()))?;
            pending.push(ExternalReportWriteBack {
                number,
                hub_number: issue.display_number,
                linked_duplicate: link.linked,
            });
        }
        Ok(pending)
    }

    /// Clear only after GitHub acknowledged the comment; failures leave it pending.
    pub fn complete_external_report_write_back(&self, repo: &str, number: u64) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let key = report_key(repo, number);
        let raw = read(&tx, &key)?
            .ok_or_else(|| DaemonError::Store("external report link missing".into()))?;
        let mut link: ReportLink = serde_json::from_str(&raw)?;
        link.pending_write_back = false;
        upsert(&tx, &key, &serde_json::to_string(&link)?)?;
        tx.commit()?;
        Ok(())
    }

    /// Import one GitHub report as a hub Issue, or link it to the Issue that
    /// already covers it. `draft` is built by `hub_issue_from_report`, which
    /// fixes priority, labels and assignee; this function files exactly those.
    pub fn import_external_report(
        &self,
        project_id: Uuid,
        repo: &str,
        number: u64,
        draft: &HubIssueDraft,
    ) -> Result<ExternalImportOutcome> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let key = report_key(repo, number);
        if let Some(raw) = read(&tx, &key)? {
            let value: serde_json::Value = serde_json::from_str(&raw).map_err(|error| {
                DaemonError::Store(format!("bad external report link: {error}"))
            })?;
            let issue_id = value["issue_id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or_else(|| DaemonError::Store("external report link names no Issue".into()))?;
            return Ok(ExternalImportOutcome::AlreadyHandled { issue_id });
        }
        let issue_id = Uuid::new_v5(
            &EXTERNAL_REPORT_NAMESPACE,
            format!("{repo}#{number}").as_bytes(),
        );
        // 1. The first report of this failure signature.
        let by_signature = read(&tx, &signature_key(&draft.signature))?
            .and_then(|id| Uuid::parse_str(&id).ok())
            .map(|id| Self::get_issue_tx(&tx, id))
            .transpose()?
            .flatten();
        // 2. Any open Issue with an equivalent title (a hand-filed one).
        let raw_title = draft
            .title
            .strip_prefix(&format!("[{HUB_LABEL}] "))
            .unwrap_or(&draft.title);
        let existing = match by_signature {
            Some(issue) => Some(issue),
            None => {
                Self::find_open_issue_with_equivalent_title(&tx, project_id, raw_title, issue_id)?
            }
        };
        if let Some(existing) = existing {
            record_link(&tx, &key, existing.id, true)?;
            tx.commit()?;
            return Ok(ExternalImportOutcome::LinkedDuplicate(existing));
        }
        let new = NewIssue {
            project_id,
            title: draft.title.clone(),
            body: draft.body.clone(),
            priority: Some(draft.priority),
            labels: draft.labels.clone(),
            created_by_session_id: None,
            assignee: draft.assignee.clone(),
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let created = Self::idempotent_issue_in_tx(
            &tx,
            issue_id,
            &new,
            IssueWriteActor::System {
                label: "rsi:external-report".to_string(),
            },
        )?;
        record_link(&tx, &key, created.issue.id, false)?;
        upsert(
            &tx,
            &signature_key(&draft.signature),
            &created.issue.id.to_string(),
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(ExternalImportOutcome::Created(created.issue))
    }

    /// The last recorded sync status; `None` before the first run.
    pub fn external_report_sync_status(&self) -> Result<Option<SyncStatusV1>> {
        self.get_daemon_setting(STATUS_KEY)?
            .map(|raw| {
                serde_json::from_str(&raw)
                    .map_err(|error| DaemonError::Store(format!("bad sync status: {error}")))
            })
            .transpose()
    }

    pub fn set_external_report_sync_status(&self, status: &SyncStatusV1) -> Result<()> {
        let raw = serde_json::to_string(status)?;
        self.set_daemon_setting(STATUS_KEY, &raw)
    }
}

#[cfg(test)]
#[path = "external_reports_tests.rs"]
mod tests;
