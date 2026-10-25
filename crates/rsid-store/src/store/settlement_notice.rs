//! #2452: an automatic source-worktree settlement pass has no waiting RPC
//! caller, so a refusal, a recovery fence or a failed audit/apply used to be a
//! log line only. These helpers turn a *persistent* outcome into one Issue in
//! the project that owns the retained work, carrying the run id, repository,
//! sessions, refusal codes and observations.
//!
//! Decisions:
//! - #2630: only a persistent outcome has an owner. A root retained by the
//!   same refusal (or recovery fence) in [`SETTLEMENT_NOTICE_PERSISTENT_PASSES`]
//!   consecutive automatic passes, or the same audit/apply error class that
//!   many passes in a row, files; a one-pass custody drift, a stale-digest
//!   race or a fence the next pass cleared is run history only
//!   ([`SettlementNoticeAction::Deferred`]). The streak lives in a
//!   [`SettlementPersistence`] the daemon keeps for the process: a restart
//!   delays a filing by at most one streak, it never files early.
//! - The Issue id is a deterministic UUIDv5 of the project, repository and the
//!   outcome *signature* (#2650: the phase class and the refusal code of a
//!   persistent root, or the failed stage and error class), never of the run id
//!   or the root set. A later pass with other roots or counts of the same
//!   signature rewrites the open Issue in place (counts, latest run) instead
//!   of filing another; a different refusal code is a different problem with
//!   its own Issue, even in the same run.
//! - #2650: a root whose sandbox another path already purged (the normal
//!   archive) is no longer retained work: it is left out before anything is
//!   observed or filed, and a run with nothing else files nothing. An open
//!   Issue that listed it is never closed here; closing stays the manager's
//!   acknowledgement.
//! - A closed Issue is the manager's acknowledgement: it is never rewritten
//!   or reopened.
//! - Custody checks are untouched: this module only reads the run and the
//!   session/project rows, and writes one Issue.

use super::Store;
use super::issues::{IssueWriteActor, issue_write_before_commit};
use crate::error::Result;
use rsi_common::cohort_settlement::{
    SourceWorktreeSettlementItemV1, SourceWorktreeSettlementPhaseV1,
    SourceWorktreeSettlementRunStateV1, SourceWorktreeSettlementRunV1,
};
use rsi_common::types::{IssueStatus, IssueUpdate};
use rusqlite::{Transaction, TransactionBehavior, params};
use std::collections::HashMap;
use uuid::Uuid;

/// Labels of the filed Issue.
pub const SETTLEMENT_NOTICE_ISSUE_LABELS: [&str; 3] =
    ["storage", "source-worktree-settlement", "auto-filed"];
/// Items listed in one notice body; the rest are counted.
const NOTICE_ITEM_LIMIT: usize = 20;
/// Bytes kept from one observation or error text.
const NOTICE_TEXT_LIMIT: usize = 600;

/// Consecutive automatic passes an outcome must survive before it has an owner.
pub const SETTLEMENT_NOTICE_PERSISTENT_PASSES: u32 = 2;

#[derive(Debug, Clone, Copy)]
struct Streak {
    count: u32,
    last_pass: u64,
}

/// Consecutive-pass memory of the outcomes an automatic settlement pass saw.
///
/// The daemon calls [`Self::begin_pass`] once per automatic pass; every
/// outcome observed in a pass counts once for that pass, and an outcome not
/// seen in the immediately preceding pass starts again at one.
#[derive(Debug, Default)]
pub struct SettlementPersistence {
    pass: u64,
    streaks: HashMap<String, Streak>,
}

impl SettlementPersistence {
    /// Start the next automatic pass and forget outcomes the last pass missed.
    pub fn begin_pass(&mut self) {
        self.pass += 1;
        let pass = self.pass;
        self.streaks
            .retain(|_, streak| streak.last_pass.saturating_add(1) >= pass);
    }

    /// Record `key` for the current pass and return its consecutive count.
    fn observe(&mut self, key: String) -> u32 {
        let pass = self.pass;
        let streak = self.streaks.entry(key).or_insert(Streak {
            count: 0,
            last_pass: pass,
        });
        if streak.count == 0 {
            streak.count = 1;
        } else if streak.last_pass == pass {
            // Another look at the same pass (a re-read after a failed attempt).
        } else if streak.last_pass.saturating_add(1) == pass {
            streak.count += 1;
        } else {
            streak.count = 1;
        }
        streak.last_pass = pass;
        streak.count
    }
}

/// What [`Store::upsert_source_worktree_settlement_notice_for_run`] and
/// [`Store::upsert_source_worktree_settlement_failure_notice`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementNoticeAction {
    Created,
    /// The open Issue of this outcome class was rewritten with the latest run.
    Updated,
    /// The outcome class already has an Issue whose text is current, or a
    /// closed (acknowledged) one; nothing written.
    Unchanged,
    /// The outcome has not persisted yet (#2630): run history only.
    Deferred,
    /// The run needs no owner action (every fence item is settled).
    NothingToReport,
    /// No Session of the retained work belongs to an existing project.
    NoProject,
}

impl SettlementNoticeAction {
    /// The outcome has an owner: an Issue exists for it or it is being tracked
    /// toward one, so a failure it causes files no second notice.
    #[must_use]
    pub const fn is_covered(self) -> bool {
        matches!(
            self,
            Self::Created | Self::Updated | Self::Unchanged | Self::Deferred
        )
    }
}

/// The Issue a notice produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementNotice {
    pub project_id: Option<Uuid>,
    pub issue_id: Option<Uuid>,
    pub display_number: Option<i64>,
    pub action: SettlementNoticeAction,
}

impl SettlementNotice {
    const fn without_issue(action: SettlementNoticeAction) -> Self {
        Self {
            project_id: None,
            issue_id: None,
            display_number: None,
            action,
        }
    }
}

/// The deterministic Issue id of one settlement outcome in `project_id`.
#[must_use]
pub fn settlement_notice_issue_id(
    project_id: Uuid,
    repository_identity: &str,
    signature: &str,
) -> Uuid {
    Uuid::new_v5(
        &project_id,
        format!("source-worktree-settlement-notice:{repository_identity}:{signature}").as_bytes(),
    )
}

fn parse_uuid(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw)
        .map_err(|error| crate::error::DaemonError::Store(format!("invalid uuid {raw}: {error}")))
}

fn bounded(text: &str) -> String {
    let flat = text.replace(['\n', '\r', '`'], " ");
    if flat.len() <= NOTICE_TEXT_LIMIT {
        return flat;
    }
    let mut end = NOTICE_TEXT_LIMIT;
    while !flat.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &flat[..end])
}

/// A fence item the owner has to decide on.
fn is_unsettled_terminal(item: &SourceWorktreeSettlementItemV1) -> bool {
    matches!(
        item.phase,
        SourceWorktreeSettlementPhaseV1::Refused
            | SourceWorktreeSettlementPhaseV1::RecoveryRequired
            | SourceWorktreeSettlementPhaseV1::Unattempted
    )
}

/// The unsettled terminal items of `run` when the run needs the owner, in
/// item order. A run whose only leftovers are `Unattempted` items (automatic
/// pressure relief stopped it) needs no action unless the run itself ended
/// partial, refused or recovery-required.
fn actionable_items(run: &SourceWorktreeSettlementRunV1) -> Vec<&SourceWorktreeSettlementItemV1> {
    let items: Vec<_> = run
        .items
        .iter()
        .filter(|item| is_unsettled_terminal(item))
        .collect();
    let hard = items.iter().any(|item| {
        matches!(
            item.phase,
            SourceWorktreeSettlementPhaseV1::Refused
                | SourceWorktreeSettlementPhaseV1::RecoveryRequired
        )
    });
    let run_needs_owner = matches!(
        run.state,
        SourceWorktreeSettlementRunStateV1::Partial
            | SourceWorktreeSettlementRunStateV1::Refused
            | SourceWorktreeSettlementRunStateV1::RecoveryRequired
    );
    if hard || (run_needs_owner && !items.is_empty()) {
        items
    } else {
        Vec::new()
    }
}

/// The tracker key of one retained root: the same root with the same phase
/// and refusal in the next pass is the same outcome.
fn item_key(repository_identity: &str, item: &SourceWorktreeSettlementItemV1) -> String {
    format!(
        "{repository_identity}:item:{}:{}:{}",
        item.phase.as_str(),
        item_code(item),
        item.session_id
    )
}

/// The phase class of one retained root.
fn item_class(item: &SourceWorktreeSettlementItemV1) -> &'static str {
    match item.phase {
        SourceWorktreeSettlementPhaseV1::RecoveryRequired => "recovery_required",
        SourceWorktreeSettlementPhaseV1::Refused => "refused",
        _ => "unattempted",
    }
}

/// The refusal code of one retained root (`none` when it has none).
fn item_code(item: &SourceWorktreeSettlementItemV1) -> &str {
    item.refusal_code
        .as_ref()
        .map_or("none", |refusal| refusal.as_str())
}

/// The persistent roots of one outcome signature, `class:code` (#2650).
struct NoticeGroup<'a> {
    class: &'static str,
    code: &'a str,
    items: Vec<&'a SourceWorktreeSettlementItemV1>,
}

impl NoticeGroup<'_> {
    /// The Issue signature of the group: stable across runs, root sets and counts.
    fn signature(&self) -> String {
        format!("run:{}:{}", self.class, self.code)
    }
}

/// Split the persistent roots into one group per (phase class, refusal code),
/// most severe class first, then by code. One repository has at most one open
/// Issue per group (#2630, #2650); the roots and counts are the body, rewritten
/// as they change. Unattempted leftovers are pressure-relief state, not an
/// owner decision, so they only form a group when no root is refused or fenced.
fn notice_groups<'a>(items: &[&'a SourceWorktreeSettlementItemV1]) -> Vec<NoticeGroup<'a>> {
    let hard = items.iter().any(|item| item_class(item) != "unattempted");
    let mut groups: Vec<NoticeGroup<'a>> = Vec::new();
    for item in items.iter().copied() {
        let class = item_class(item);
        if hard && class == "unattempted" {
            continue;
        }
        let code = item_code(item);
        match groups
            .iter_mut()
            .find(|group| group.class == class && group.code == code)
        {
            Some(group) => group.items.push(item),
            None => groups.push(NoticeGroup {
                class,
                code,
                items: vec![item],
            }),
        }
    }
    let rank = |class: &str| match class {
        "recovery_required" => 0,
        "refused" => 1,
        _ => 2,
    };
    groups.sort_by(|a, b| (rank(a.class), a.code).cmp(&(rank(b.class), b.code)));
    groups
}

fn run_notice_text(
    run: &SourceWorktreeSettlementRunV1,
    items: &[&SourceWorktreeSettlementItemV1],
    class: &str,
    code: &str,
) -> (String, String) {
    let title = format!(
        "storage: automatic source-worktree settlement keeps {} root(s) {class} ({code})",
        items.len(),
    );
    let mut listed = String::new();
    for item in items.iter().take(NOTICE_ITEM_LIMIT) {
        listed.push_str(&format!(
            "- session `{}` custody `{}` ref `{}`: phase `{}`, refusal `{}`\n  - before: `{}`\n  - after: `{}`\n",
            item.session_id,
            item.custody_id,
            bounded(&item.source_ref),
            item.phase.as_str(),
            item.refusal_code
                .as_ref()
                .map_or("none", |refusal| refusal.as_str()),
            item.before_observation
                .as_deref()
                .map_or_else(|| "none recorded".to_string(), bounded),
            item.after_observation
                .as_deref()
                .map_or_else(|| "none recorded".to_string(), bounded),
        ));
    }
    if items.len() > NOTICE_ITEM_LIMIT {
        listed.push_str(&format!(
            "- … and {} more\n",
            items.len() - NOTICE_ITEM_LIMIT
        ));
    }
    let body = format!(
        "## Automatic source-worktree settlement (auto-filed by the daemon, #2452)\n\
         - outcome: class `{class}`, refusal `{code}` (persisted over {passes} consecutive automatic passes)\n\
         - latest run: `{run_id}` (state `{state}`)\n\
         - repository: `{identity}` at `{dir}`\n\
         - counts: observed {observed}, eligible {eligible}, retained {retained}, settled {settled}, \
         refused {refused}, recovery_required {recovery}, unattempted {unattempted}\n\
         - terminal error: {error}\n\n\
         ## Retained roots\n{listed}\n\
         ## Intent\n\
         The automatic pass has no waiting caller, so these roots stay on disk until an owner \
         decides. Inspect the run with `AgentGetSourceWorktreeSettlementRun` / \
         `AgentListSourceWorktreeCohorts`, resolve the refusal or recovery fence (the custody \
         checks are never bypassed), then let the next pass settle the roots. The daemon keeps \
         one Issue per repository, outcome class and refusal code and rewrites it here as the roots and \
         counts change; a one-pass refusal files nothing. Close it to acknowledge.\n",
        passes = SETTLEMENT_NOTICE_PERSISTENT_PASSES,
        run_id = run.run_id,
        state = run.state.as_str(),
        identity = run.repository_identity,
        dir = bounded(&run.canonical_repo_dir),
        observed = run.counts.observed,
        eligible = run.counts.eligible,
        retained = run.counts.retained,
        settled = run.counts.settled,
        refused = run.counts.refused,
        recovery = run.counts.recovery_required,
        unattempted = run.counts.unattempted,
        error = run
            .terminal_error
            .as_deref()
            .map_or_else(|| "none".to_string(), bounded),
    );
    (title, body)
}

impl Store {
    /// The first existing project that owns one of `session_ids`, in order.
    fn settlement_notice_project_of_sessions(&self, session_ids: &[Uuid]) -> Result<Option<Uuid>> {
        for session_id in session_ids {
            let project: Option<String> = self
                .conn
                .query_row(
                    "SELECT p.id FROM sessions s JOIN projects p ON p.id=s.project_id
                     WHERE s.id=?1",
                    params![session_id.to_string()],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            if let Some(project) = project {
                return Ok(Some(parse_uuid(&project)?));
            }
        }
        Ok(None)
    }

    /// The project that owns the live roots of `repository_identity`.
    fn settlement_notice_project_of_repository(
        &self,
        repository_identity: &str,
    ) -> Result<Option<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id FROM sandbox_custody_roots r
             JOIN sessions s ON s.id=r.owner_session_id
             JOIN projects p ON p.id=s.project_id
             WHERE r.repository_identity=?1 AND r.state='live'
             ORDER BY r.custody_id",
        )?;
        let ids = statement
            .query_map(params![repository_identity], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let ids = ids
            .iter()
            .map(|id| parse_uuid(id))
            .collect::<Result<Vec<_>>>()?;
        self.settlement_notice_project_of_sessions(&ids)
    }

    /// Whether the session's sandbox was already purged by another path (the
    /// normal archive): its retained root is gone and needs no owner (#2650).
    /// An unknown session is not purged: it keeps its existing handling.
    fn settlement_notice_root_purged(&self, session_id: Uuid) -> Result<bool> {
        let state: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT sandbox_cleanup_state FROM sessions WHERE id=?1",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(matches!(state, Some(Some(state)) if state == "Purged"))
    }

    /// The unsettled roots of `run` an owner still has to decide on: the
    /// actionable items minus the roots another path already purged (#2650).
    fn retained_actionable_items<'a>(
        &self,
        run: &'a SourceWorktreeSettlementRunV1,
    ) -> Result<Vec<&'a SourceWorktreeSettlementItemV1>> {
        let mut retained = Vec::new();
        for item in actionable_items(run) {
            if !self.settlement_notice_root_purged(item.session_id)? {
                retained.push(item);
            }
        }
        Ok(retained)
    }

    fn file_settlement_notice(
        &self,
        project_id: Uuid,
        repository_identity: &str,
        signature: &str,
        title: String,
        body: String,
    ) -> Result<SettlementNotice> {
        let issue_id = settlement_notice_issue_id(project_id, repository_identity, signature);
        let done = |display_number, action| SettlementNotice {
            project_id: Some(project_id),
            issue_id: Some(issue_id),
            display_number: Some(display_number),
            action,
        };
        if let Some(existing) = self.get_issue(issue_id)? {
            let acknowledged = matches!(
                existing.status,
                IssueStatus::Closed | IssueStatus::Cancelled
            );
            if acknowledged || (existing.title == title && existing.body == body) {
                return Ok(done(
                    existing.display_number,
                    SettlementNoticeAction::Unchanged,
                ));
            }
            self.update_issue(
                issue_id,
                &IssueUpdate {
                    title: Some(title),
                    body: Some(body),
                    ..IssueUpdate::default()
                },
            )?;
            return Ok(done(
                existing.display_number,
                SettlementNoticeAction::Updated,
            ));
        }
        let new = rsi_common::types::NewIssue {
            project_id,
            title,
            body,
            priority: None,
            labels: SETTLEMENT_NOTICE_ISSUE_LABELS
                .iter()
                .map(ToString::to_string)
                .collect(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let created = Self::idempotent_issue_in_tx(
            &tx,
            issue_id,
            &new,
            IssueWriteActor::System {
                label: "rsi:source-worktree-settlement".to_string(),
            },
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        let action = if created.deduplicated {
            SettlementNoticeAction::Unchanged
        } else {
            SettlementNoticeAction::Created
        };
        Ok(done(created.issue.display_number, action))
    }

    /// Whether `run` leaves roots an owner has to decide on and a project owns
    /// them: the run's fence is (or is being) covered by a notice. It does not
    /// count the run as an observation of a pass.
    ///
    /// # Errors
    /// A persistence error.
    pub fn settlement_run_has_owner(&self, run: &SourceWorktreeSettlementRunV1) -> Result<bool> {
        let items = self.retained_actionable_items(run)?;
        if items.is_empty() {
            return Ok(false);
        }
        let session_ids: Vec<Uuid> = items.iter().map(|item| item.session_id).collect();
        let project = match self.settlement_notice_project_of_sessions(&session_ids)? {
            Some(project) => Some(project),
            None => self.settlement_notice_project_of_repository(&run.repository_identity)?,
        };
        Ok(project.is_some())
    }

    /// Record one automatic pass's `run` and, when a root has been retained by
    /// the same refusal (or recovery fence) for
    /// [`SETTLEMENT_NOTICE_PERSISTENT_PASSES`] consecutive passes, file or
    /// update the repository's Issue for that outcome class (#2630). A run
    /// whose roots are all first sightings, a replay of the same pass, or a
    /// closed Issue writes nothing.
    ///
    /// # Errors
    /// A persistence error.
    pub fn upsert_source_worktree_settlement_notice_for_run(
        &self,
        run: &SourceWorktreeSettlementRunV1,
        persistence: &mut SettlementPersistence,
    ) -> Result<SettlementNotice> {
        let items = self.retained_actionable_items(run)?;
        if items.is_empty() {
            return Ok(SettlementNotice::without_issue(
                SettlementNoticeAction::NothingToReport,
            ));
        }
        // Observe every root: a streak must not stall because another root of
        // the same run is already persistent.
        let persistent: Vec<&SourceWorktreeSettlementItemV1> = items
            .iter()
            .copied()
            .filter(|item| {
                persistence.observe(item_key(&run.repository_identity, item))
                    >= SETTLEMENT_NOTICE_PERSISTENT_PASSES
            })
            .collect();
        let session_ids: Vec<Uuid> = items.iter().map(|item| item.session_id).collect();
        let project = match self.settlement_notice_project_of_sessions(&session_ids)? {
            Some(project) => Some(project),
            None => self.settlement_notice_project_of_repository(&run.repository_identity)?,
        };
        let Some(project_id) = project else {
            return Ok(SettlementNotice::without_issue(
                SettlementNoticeAction::NoProject,
            ));
        };
        if persistent.is_empty() {
            return Ok(SettlementNotice {
                project_id: Some(project_id),
                issue_id: None,
                display_number: None,
                action: SettlementNoticeAction::Deferred,
            });
        }
        // One Issue per (repository, phase class, refusal code) (#2650). The
        // first group (most severe) is the notice returned; every group is filed.
        let mut first = None;
        for group in notice_groups(&persistent) {
            let (title, body) = run_notice_text(run, &group.items, group.class, group.code);
            let notice = self.file_settlement_notice(
                project_id,
                &run.repository_identity,
                &group.signature(),
                title,
                body,
            )?;
            if first.is_none() {
                first = Some(notice);
            }
        }
        Ok(first.unwrap_or(SettlementNotice {
            project_id: Some(project_id),
            issue_id: None,
            display_number: None,
            action: SettlementNoticeAction::NothingToReport,
        }))
    }

    /// File one Issue for an automatic audit/apply/resume that failed before it
    /// produced a receipt, once the same `error_class` failed the same `stage`
    /// in [`SETTLEMENT_NOTICE_PERSISTENT_PASSES`] consecutive passes (#2630: a
    /// one-off stale-digest race is run history only). `error_class` is the
    /// stable part of the failure (an RPC code, not a path) and keys the
    /// dedupe; `error_text` is evidence and refreshed in place.
    ///
    /// # Errors
    /// A persistence error.
    pub fn upsert_source_worktree_settlement_failure_notice(
        &self,
        repository_identity: &str,
        canonical_repo_dir: &str,
        stage: &str,
        error_class: &str,
        error_text: &str,
        persistence: &mut SettlementPersistence,
    ) -> Result<SettlementNotice> {
        let Some(project_id) = self.settlement_notice_project_of_repository(repository_identity)?
        else {
            return Ok(SettlementNotice::without_issue(
                SettlementNoticeAction::NoProject,
            ));
        };
        let streak = persistence.observe(format!(
            "{repository_identity}:failure:{stage}:{error_class}"
        ));
        if streak < SETTLEMENT_NOTICE_PERSISTENT_PASSES {
            return Ok(SettlementNotice {
                project_id: Some(project_id),
                issue_id: None,
                display_number: None,
                action: SettlementNoticeAction::Deferred,
            });
        }
        let title = format!("storage: automatic source-worktree settlement {stage} failed");
        let body = format!(
            "## Automatic source-worktree settlement (auto-filed by the daemon, #2452)\n\
             - stage: `{stage}`\n\
             - repository: `{repository_identity}` at `{dir}`\n\
             - error class: `{class}`\n\
             - observation: `{text}`\n\n\
             ## Intent\n\
             The automatic pass failed before it produced a receipt, so its terminal roots stay \
             on disk. Find why the {stage} fails for this repository and fix the cause; a \
             committed intent stays recoverable and the next pass resumes it. The daemon files \
             this only after {passes} consecutive failures, keeps one Issue per repository, stage \
             and error class and rewrites it as the failure repeats; close it to acknowledge.\n",
            passes = SETTLEMENT_NOTICE_PERSISTENT_PASSES,
            dir = bounded(canonical_repo_dir),
            class = bounded(error_class),
            text = bounded(error_text),
        );
        self.file_settlement_notice(
            project_id,
            repository_identity,
            &format!("failure:{stage}:{error_class}"),
            title,
            body,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::cohort_settlement::{
        SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION, SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        SourceWorktreeGitOidV1, SourceWorktreeSettlementCountsV1,
        SourceWorktreeSettlementRefusalV1,
    };
    use rsi_common::types::{IssueStatus, Project};

    fn project(store: &Store) -> Uuid {
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: format!("settlement-notice-{}", Uuid::new_v4()),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_project(&project).expect("insert project");
        project.id
    }

    fn session_in(store: &Store, project: Uuid) -> Uuid {
        let mut session =
            crate::test_support::test_session(Uuid::new_v4(), std::path::PathBuf::from("/tmp"));
        session.project_id = Some(project);
        store.insert_session(&session).expect("insert session");
        session.id
    }

    fn item(
        session_id: Uuid,
        phase: SourceWorktreeSettlementPhaseV1,
        refusal: Option<SourceWorktreeSettlementRefusalV1>,
    ) -> SourceWorktreeSettlementItemV1 {
        SourceWorktreeSettlementItemV1 {
            sequence: 0,
            session_id,
            custody_id: Uuid::new_v4(),
            source_ref: "refs/heads/rsi/example".into(),
            expected_source_oid: SourceWorktreeGitOidV1::parse(
                "0123456789abcdef0123456789abcdef01234567",
            )
            .expect("oid"),
            phase,
            refusal_code: refusal,
            before_observation: Some("root present".into()),
            after_observation: Some("residue at quarantine".into()),
        }
    }

    fn run(
        state: SourceWorktreeSettlementRunStateV1,
        items: Vec<SourceWorktreeSettlementItemV1>,
    ) -> SourceWorktreeSettlementRunV1 {
        SourceWorktreeSettlementRunV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            run_id: Uuid::new_v4(),
            repository_identity: "repo-identity".into(),
            canonical_repo_dir: "/repo".into(),
            target_ref: "refs/heads/rolling".into(),
            target_oid: SourceWorktreeGitOidV1::parse("fedcba9876543210fedcba9876543210fedcba98")
                .expect("oid"),
            plan_digest: rsi_common::types::Sha256Digest::parse(format!(
                "sha256:{}",
                "0".repeat(64)
            ))
            .expect("digest"),
            idempotency_key: "auto:fixture".into(),
            state,
            counts: SourceWorktreeSettlementCountsV1 {
                observed: 1,
                eligible: 1,
                retained: 0,
                settled: 0,
                refused: 0,
                recovery_required: 1,
                unattempted: 0,
            },
            attribution: None,
            items,
            retained_roots: Vec::new(),
            unproven_survivors: Vec::new(),
            created_at: "2026-10-14T00:00:00.000000000Z".into(),
            updated_at: "2026-10-14T00:00:00.000000000Z".into(),
            finished_at: None,
            terminal_error: None,
        }
    }

    /// One automatic pass: begin it, then observe `run`.
    fn pass(
        store: &Store,
        persistence: &mut SettlementPersistence,
        run: &SourceWorktreeSettlementRunV1,
    ) -> SettlementNotice {
        persistence.begin_pass();
        store
            .upsert_source_worktree_settlement_notice_for_run(run, persistence)
            .unwrap()
    }

    fn refused_run(item: SourceWorktreeSettlementItemV1) -> SourceWorktreeSettlementRunV1 {
        run(SourceWorktreeSettlementRunStateV1::Partial, vec![item])
    }

    fn notice_issue_count(store: &Store) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE body LIKE '%auto-filed by the daemon, #2452%'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_transient_refusal_files_nothing_and_a_gap_restarts_the_streak_2630() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let session = session_in(&store, project_id);
        let mut persistence = SettlementPersistence::default();
        let drift = || {
            refused_run(item(
                session,
                SourceWorktreeSettlementPhaseV1::Refused,
                Some(SourceWorktreeSettlementRefusalV1::CustodyDrift),
            ))
        };

        // Seen once: run history only.
        let first = pass(&store, &mut persistence, &drift());
        assert_eq!(first.action, SettlementNoticeAction::Deferred);
        assert_eq!(first.issue_id, None);
        // The next pass settles the root, so nothing is observed.
        persistence.begin_pass();
        // It drifts again two passes later: a new streak, still one sighting.
        let again = pass(&store, &mut persistence, &drift());
        assert_eq!(again.action, SettlementNoticeAction::Deferred);
        assert_eq!(notice_issue_count(&store), 0);

        // A pass that re-reads the same pass's run does not count twice.
        let reread = store
            .upsert_source_worktree_settlement_notice_for_run(&drift(), &mut persistence)
            .unwrap();
        assert_eq!(reread.action, SettlementNoticeAction::Deferred);
        assert_eq!(notice_issue_count(&store), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_persistent_refusal_files_one_issue_updated_in_place_2630() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let session = session_in(&store, project_id);
        let other = session_in(&store, project_id);
        let mut persistence = SettlementPersistence::default();
        let drift = |roots: &[Uuid]| {
            run(
                SourceWorktreeSettlementRunStateV1::Partial,
                roots
                    .iter()
                    .map(|root| {
                        item(
                            *root,
                            SourceWorktreeSettlementPhaseV1::Refused,
                            Some(SourceWorktreeSettlementRefusalV1::CustodyDrift),
                        )
                    })
                    .collect(),
            )
        };

        let first = drift(&[session]);
        assert_eq!(
            pass(&store, &mut persistence, &first).action,
            SettlementNoticeAction::Deferred
        );
        let second = drift(&[session]);
        let created = pass(&store, &mut persistence, &second);
        assert_eq!(created.action, SettlementNoticeAction::Created);
        assert_eq!(created.project_id, Some(project_id));
        let issue_id = created.issue_id.unwrap();
        let issue = store.get_issue(issue_id).unwrap().unwrap();
        assert_eq!(issue.status, IssueStatus::Open);
        for evidence in [
            second.run_id.to_string(),
            "repo-identity".to_string(),
            session.to_string(),
            "custody_drift".to_string(),
            "residue at quarantine".to_string(),
        ] {
            assert!(
                issue.body.contains(&evidence),
                "{evidence} in {}",
                issue.body
            );
        }

        // A later pass with another root of the same class and a new run
        // rewrites the same open Issue: no second Issue.
        let third = drift(&[session, other]);
        let updated = pass(&store, &mut persistence, &third);
        assert_eq!(updated.action, SettlementNoticeAction::Updated);
        assert_eq!(updated.issue_id, Some(issue_id));
        assert_eq!(updated.display_number, created.display_number);
        let issue = store.get_issue(issue_id).unwrap().unwrap();
        assert!(
            issue.body.contains(&third.run_id.to_string()),
            "{}",
            issue.body
        );
        assert!(issue.body.contains(&session.to_string()), "{}", issue.body);
        // `other` is a first sighting, so it is not listed until it persists.
        assert!(!issue.body.contains(&other.to_string()), "{}", issue.body);
        let fourth = drift(&[session, other]);
        assert_eq!(
            pass(&store, &mut persistence, &fourth).action,
            SettlementNoticeAction::Updated
        );
        assert!(
            store
                .get_issue(issue_id)
                .unwrap()
                .unwrap()
                .body
                .contains(&other.to_string())
        );
        assert_eq!(notice_issue_count(&store), 1);

        // The same body again writes nothing.
        let version = store.get_issue(issue_id).unwrap().unwrap().row_version;
        persistence.begin_pass();
        let replay = store
            .upsert_source_worktree_settlement_notice_for_run(&fourth, &mut persistence)
            .unwrap();
        assert_eq!(replay.action, SettlementNoticeAction::Unchanged);
        assert_eq!(
            store.get_issue(issue_id).unwrap().unwrap().row_version,
            version
        );

        // A closed Issue is the acknowledgement; it stays closed and unwritten.
        store
            .update_issue_status(issue_id, IssueStatus::Closed)
            .unwrap();
        let fifth = drift(&[session]);
        let closed = pass(&store, &mut persistence, &fifth);
        assert_eq!(closed.action, SettlementNoticeAction::Unchanged);
        let closed_issue = store.get_issue(issue_id).unwrap().unwrap();
        assert_eq!(closed_issue.status, IssueStatus::Closed);
        assert!(!closed_issue.body.contains(&fifth.run_id.to_string()));
        assert_eq!(notice_issue_count(&store), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_recovery_fence_that_survives_the_next_pass_files_its_own_class_2630() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let session = session_in(&store, project_id);
        let mut persistence = SettlementPersistence::default();
        let fenced = run(
            SourceWorktreeSettlementRunStateV1::RecoveryRequired,
            vec![item(
                session,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                Some(SourceWorktreeSettlementRefusalV1::RecoveryProofFailed),
            )],
        );
        assert_eq!(
            pass(&store, &mut persistence, &fenced).action,
            SettlementNoticeAction::Deferred
        );
        let created = pass(&store, &mut persistence, &fenced);
        assert_eq!(created.action, SettlementNoticeAction::Created);
        let issue = store.get_issue(created.issue_id.unwrap()).unwrap().unwrap();
        assert!(
            issue.body.contains("recovery_proof_failed"),
            "{}",
            issue.body
        );
        assert!(issue.title.contains("recovery_required"), "{}", issue.title);

        // A persistent plain refusal is another outcome class: its own Issue.
        let refusal = refused_run(item(
            session,
            SourceWorktreeSettlementPhaseV1::Refused,
            Some(SourceWorktreeSettlementRefusalV1::TargetDrift),
        ));
        pass(&store, &mut persistence, &refusal);
        let other = pass(&store, &mut persistence, &refusal);
        assert_eq!(other.action, SettlementNoticeAction::Created);
        assert_ne!(other.issue_id, created.issue_id);
        assert_eq!(notice_issue_count(&store), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn settled_unattempted_and_unowned_runs_file_nothing_2452() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let session = session_in(&store, project_id);
        let mut persistence = SettlementPersistence::default();

        let settled = run(
            SourceWorktreeSettlementRunStateV1::Settled,
            vec![item(
                session,
                SourceWorktreeSettlementPhaseV1::Settled,
                None,
            )],
        );
        assert_eq!(
            pass(&store, &mut persistence, &settled).action,
            SettlementNoticeAction::NothingToReport
        );
        // Pressure relief closing the rest of a settled run is not an owner decision.
        let relieved = run(
            SourceWorktreeSettlementRunStateV1::Settled,
            vec![item(
                session,
                SourceWorktreeSettlementPhaseV1::Unattempted,
                None,
            )],
        );
        assert_eq!(
            pass(&store, &mut persistence, &relieved).action,
            SettlementNoticeAction::NothingToReport
        );
        // A partial run with unattempted leftovers is, once it persists.
        let partial = run(
            SourceWorktreeSettlementRunStateV1::Partial,
            vec![item(
                session,
                SourceWorktreeSettlementPhaseV1::Unattempted,
                None,
            )],
        );
        assert_eq!(
            pass(&store, &mut persistence, &partial).action,
            SettlementNoticeAction::Deferred
        );
        assert_eq!(
            pass(&store, &mut persistence, &partial).action,
            SettlementNoticeAction::Created
        );
        // Work no project owns has nowhere to be filed.
        let orphan = run(
            SourceWorktreeSettlementRunStateV1::Refused,
            vec![item(
                Uuid::new_v4(),
                SourceWorktreeSettlementPhaseV1::Refused,
                Some(SourceWorktreeSettlementRefusalV1::TargetDrift),
            )],
        );
        assert_eq!(
            pass(&store, &mut persistence, &orphan).action,
            SettlementNoticeAction::NoProject
        );
        assert!(!store.settlement_run_has_owner(&orphan).unwrap());
        assert!(store.settlement_run_has_owner(&partial).unwrap());
    }

    fn open_notice_count(store: &Store) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE body LIKE '%auto-filed by the daemon, #2452%'
                   AND status NOT IN ('Closed','Cancelled')",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn refusal_run(
        repository: &str,
        roots: &[Uuid],
        refusal: SourceWorktreeSettlementRefusalV1,
    ) -> SourceWorktreeSettlementRunV1 {
        let mut run = run(
            SourceWorktreeSettlementRunStateV1::Partial,
            roots
                .iter()
                .map(|root| {
                    item(
                        *root,
                        SourceWorktreeSettlementPhaseV1::Refused,
                        Some(refusal.clone()),
                    )
                })
                .collect(),
        );
        run.repository_identity = repository.into();
        run
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn consecutive_runs_of_one_signature_refresh_the_one_open_issue_2650() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let roots = [
            session_in(&store, project_id),
            session_in(&store, project_id),
        ];
        let mut persistence = SettlementPersistence::default();
        let drift = |roots: &[Uuid]| {
            refusal_run(
                "repo-identity",
                roots,
                SourceWorktreeSettlementRefusalV1::CustodyDrift,
            )
        };

        pass(&store, &mut persistence, &drift(&roots[..1]));
        let created = pass(&store, &mut persistence, &drift(&roots[..1]));
        assert_eq!(created.action, SettlementNoticeAction::Created);
        // Six more runs with a growing then shrinking root set and a new run id
        // each time: the same signature, so the same one open Issue.
        let mut latest = None;
        for set in [&roots[..], &roots[..], &roots[..1], &roots[..], &roots[..1]] {
            let run = drift(set);
            let notice = pass(&store, &mut persistence, &run);
            assert_eq!(notice.issue_id, created.issue_id);
            assert_eq!(notice.display_number, created.display_number);
            latest = Some(run);
        }
        let latest = latest.unwrap();
        assert_eq!(open_notice_count(&store), 1);
        let issue = store.get_issue(created.issue_id.unwrap()).unwrap().unwrap();
        assert_eq!(issue.status, IssueStatus::Open);
        assert!(
            issue.body.contains(&latest.run_id.to_string()),
            "{}",
            issue.body
        );
        assert!(issue.title.contains("custody_drift"), "{}", issue.title);
    }

    /// One automatic pass that looks at several repositories' runs.
    fn pass_all(
        store: &Store,
        persistence: &mut SettlementPersistence,
        runs: &[&SourceWorktreeSettlementRunV1],
    ) -> Vec<SettlementNotice> {
        persistence.begin_pass();
        runs.iter()
            .map(|run| {
                store
                    .upsert_source_worktree_settlement_notice_for_run(run, persistence)
                    .unwrap()
            })
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn different_refusal_codes_and_repositories_get_their_own_issues_2650() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let session = session_in(&store, project_id);
        let other = session_in(&store, project_id);
        let mut persistence = SettlementPersistence::default();
        // One run retains a custody_drift root and a target_drift root; another
        // repository retains a custody_drift root of its own.
        let mixed = || {
            let mut run = refusal_run(
                "repo-identity",
                &[session],
                SourceWorktreeSettlementRefusalV1::CustodyDrift,
            );
            run.items.push(item(
                other,
                SourceWorktreeSettlementPhaseV1::Refused,
                Some(SourceWorktreeSettlementRefusalV1::TargetDrift),
            ));
            run
        };
        let elsewhere = || {
            refusal_run(
                "other-repo-identity",
                &[session],
                SourceWorktreeSettlementRefusalV1::CustodyDrift,
            )
        };
        let (first_mixed, first_elsewhere) = (mixed(), elsewhere());
        let deferred = pass_all(&store, &mut persistence, &[&first_mixed, &first_elsewhere]);
        assert!(
            deferred
                .iter()
                .all(|notice| notice.action == SettlementNoticeAction::Deferred)
        );
        let (second_mixed, second_elsewhere) = (mixed(), elsewhere());
        let created = pass_all(
            &store,
            &mut persistence,
            &[&second_mixed, &second_elsewhere],
        );
        assert!(
            created
                .iter()
                .all(|notice| notice.action == SettlementNoticeAction::Created)
        );
        // Two refusal codes in one repository plus the other repository.
        assert_eq!(open_notice_count(&store), 3);

        let drift_id =
            settlement_notice_issue_id(project_id, "repo-identity", "run:refused:custody_drift");
        let target_id =
            settlement_notice_issue_id(project_id, "repo-identity", "run:refused:target_drift");
        let elsewhere_id = settlement_notice_issue_id(
            project_id,
            "other-repo-identity",
            "run:refused:custody_drift",
        );
        assert_eq!(created[0].issue_id, Some(drift_id));
        assert_eq!(created[1].issue_id, Some(elsewhere_id));
        assert_ne!(drift_id, target_id);
        assert_ne!(drift_id, elsewhere_id);
        let drift = store.get_issue(drift_id).unwrap().unwrap();
        let target = store.get_issue(target_id).unwrap().unwrap();
        // Each Issue lists only the roots of its own refusal code.
        assert!(drift.body.contains(&session.to_string()));
        assert!(!drift.body.contains(&other.to_string()));
        assert!(target.body.contains(&other.to_string()));
        assert!(!target.body.contains(&session.to_string()));

        // Closing one code's Issue is that code's acknowledgement only: the
        // closed Issue stays closed and the other Issues are still refreshed.
        store
            .update_issue_status(drift_id, IssueStatus::Closed)
            .unwrap();
        let (third_mixed, third_elsewhere) = (mixed(), elsewhere());
        let again = pass_all(&store, &mut persistence, &[&third_mixed, &third_elsewhere]);
        assert_eq!(again[0].issue_id, Some(drift_id));
        assert_eq!(again[0].action, SettlementNoticeAction::Unchanged);
        assert_eq!(again[1].action, SettlementNoticeAction::Updated);
        let closed = store.get_issue(drift_id).unwrap().unwrap();
        assert_eq!(closed.status, IssueStatus::Closed);
        assert!(!closed.body.contains(&third_mixed.run_id.to_string()));
        let target = store.get_issue(target_id).unwrap().unwrap();
        assert!(target.body.contains(&third_mixed.run_id.to_string()));
        assert_eq!(open_notice_count(&store), 2);
    }

    fn purged_session_in(store: &Store, project: Uuid) -> Uuid {
        let mut session =
            crate::test_support::test_session(Uuid::new_v4(), std::path::PathBuf::from("/tmp"));
        session.project_id = Some(project);
        session.sandbox_kind = None;
        session.sandbox_root = None;
        session.sandbox_branch = None;
        session.sandbox_cleanup_state = Some(rsi_common::types::SandboxCleanupState::Purged);
        store
            .insert_session(&session)
            .expect("insert purged session");
        session.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_root_the_normal_archive_purged_is_not_filed_2650() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let retained = session_in(&store, project_id);
        let purged = purged_session_in(&store, project_id);
        let mut persistence = SettlementPersistence::default();
        let drift = |roots: &[Uuid]| {
            refusal_run(
                "repo-identity",
                roots,
                SourceWorktreeSettlementRefusalV1::CustodyDrift,
            )
        };

        // Only purged roots: nothing is actionable, nothing is observed or filed.
        let all_purged = drift(&[purged]);
        assert_eq!(
            pass(&store, &mut persistence, &all_purged).action,
            SettlementNoticeAction::NothingToReport
        );
        assert_eq!(
            pass(&store, &mut persistence, &all_purged).action,
            SettlementNoticeAction::NothingToReport
        );
        assert!(!store.settlement_run_has_owner(&all_purged).unwrap());
        assert_eq!(notice_issue_count(&store), 0);

        // A purged root beside a retained one is left out of the Issue.
        let mixed = drift(&[retained, purged]);
        pass(&store, &mut persistence, &mixed);
        let created = pass(&store, &mut persistence, &mixed);
        assert_eq!(created.action, SettlementNoticeAction::Created);
        let issue = store.get_issue(created.issue_id.unwrap()).unwrap().unwrap();
        assert!(issue.body.contains(&retained.to_string()), "{}", issue.body);
        assert!(!issue.body.contains(&purged.to_string()), "{}", issue.body);
        assert!(store.settlement_run_has_owner(&mixed).unwrap());
    }
}
