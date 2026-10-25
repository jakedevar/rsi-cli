//! #2698: a test the merge gate found red on the base (rolling tip) reaches the
//! manager as one `qa` Issue the first time any gate observes it, instead of
//! costing every later gate its base run silently.
//!
//! The lander reports every red the base shares (`base_red=`), on a gate that
//! publishes and on one that fails (#2705); the queue files (or dedupes into)
//! one Issue per such test unless the red is already named by an open `qa`
//! Issue or owned by a signature block of an open Issue, and the outcome
//! detail names the Issue. Reds that one shard command observed are one Issue
//! (#2865): a whole shard failing at setup is one cause, not N.

use super::*;
use crate::store::issues::{IssueWriteActor, issue_write_before_commit};
use crate::store::manager_notices::QaIssueNotice;
use rsi_common::failure_signature::parse_issue_blocks;
use rsi_common::types::NewIssue;

/// One base red the lander reported, with the shard command that observed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateBaseRed {
    pub test: String,
    pub command: Option<String>,
}

/// The Issue that owns one base red after a gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateBaseRedIssue {
    pub test: String,
    pub issue: i64,
    /// `true` when this call created the Issue, `false` when an open one
    /// already named the test.
    pub filed: bool,
}

impl Store {
    /// File (or dedupe into) the `qa` Issues for a gate's base reds: one per
    /// shard command (#2865; a red with no recorded command stands alone). `batch_id`/`entry_id` identify the gate (the entry fixes the
    /// project and the replay key, so a restart never files twice); `base` is
    /// the rolling tip the base run used; `log_path` is the gate log.
    ///
    /// An entry without a project files nothing. A red already owned by an
    /// open, non-archived Issue of the project is reported with that Issue's
    /// number and is not filed again: a `qa` Issue that names the exact test
    /// identity (never a longer test name that merely starts with it, #2704) or
    /// an Issue whose parsed signature block carries that `test_id`.
    ///
    /// # Errors
    /// A persistence error.
    pub fn report_gate_base_reds(
        &self,
        batch_id: Uuid,
        entry_id: Uuid,
        base: Option<&str>,
        log_path: Option<&str>,
        reds: &[GateBaseRed],
        now: DateTime<Utc>,
    ) -> Result<Vec<GateBaseRedIssue>> {
        if reds.is_empty() {
            return Ok(Vec::new());
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let project: Option<String> = tx
            .query_row(
                "SELECT project_id FROM rolling_queue_entries WHERE id=?1",
                [entry_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(project_id) = project
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|error| {
                DaemonError::Store(format!("invalid queue entry project id: {error}"))
            })?
        else {
            return Ok(Vec::new());
        };
        let mut seen = std::collections::BTreeSet::new();
        let reds: Vec<&GateBaseRed> = reds
            .iter()
            .filter(|red| seen.insert(red.test.clone()))
            .collect();
        // One slot per red, in the order the lander reported them.
        let mut outcome: Vec<Option<(i64, bool)>> = vec![None; reds.len()];
        let mut unowned: Vec<usize> = Vec::new();
        for (at, red) in reds.iter().enumerate() {
            match open_owner_of(&tx, project_id, &red.test)? {
                Some(number) => outcome[at] = Some((number, false)),
                None => unowned.push(at),
            }
        }
        // #2865: the unowned reds that one shard command observed are one
        // cause until shown otherwise, so they are filed as ONE Issue (a red
        // with no recorded command stands alone: nothing says its shard).
        let mut notices = Vec::new();
        for batch_members in shard_batches(&reds, &unowned) {
            let members: Vec<&GateBaseRed> = batch_members.iter().map(|&at| reds[at]).collect();
            let (new, key) = if let [only] = members[..] {
                (
                    base_red_issue(project_id, only, base, log_path),
                    format!("gate-base-red:{}", only.test),
                )
            } else {
                let mut tests: Vec<&str> = members.iter().map(|red| red.test.as_str()).collect();
                tests.sort_unstable();
                (
                    base_shard_red_issue(project_id, &members, base, log_path),
                    format!("gate-base-red-shard:{}", tests.join("\n")),
                )
            };
            let created = Self::idempotent_issue_in_tx(
                &tx,
                Uuid::new_v5(&entry_id, key.as_bytes()),
                &new,
                IssueWriteActor::System {
                    label: "rsi:merge-queue".into(),
                },
            )?;
            notices.push(QaIssueNotice {
                issue_number: created.issue.display_number,
                title: new.title.clone(),
                outcome: "test_red",
                rolling_sha: base.unwrap_or("unrecorded").to_string(),
                failing_tests: members.iter().map(|red| red.test.clone()).collect(),
            });
            for &at in &batch_members {
                outcome[at] = Some((created.issue.display_number, !created.deduplicated));
            }
        }
        let mut reported = Vec::new();
        for (red, slot) in reds.iter().zip(outcome) {
            let Some((issue, filed)) = slot else { continue };
            record_event(
                &tx,
                batch_id,
                "gate_base_red",
                Some(entry_id),
                Some(serde_json::json!({
                    "test": red.test,
                    "command": red.command,
                    "base": base,
                    "issue": issue,
                    "filed": filed,
                })),
                now,
            )?;
            reported.push(GateBaseRedIssue {
                test: red.test.clone(),
                issue,
                filed,
            });
        }
        // #2743/#2771: the current manager seat hears of each Issue this gate
        // filed, in the same transaction as the Issue (a replay finds its
        // notice recorded and adds nothing; a failure loses neither).
        for notice in notices {
            self.record_qa_issue_notice_in_tx(project_id, &notice)?;
        }
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(reported)
    }
}

/// The lowest-numbered open, non-archived Issue of the project that owns
/// `test`: a `qa`-labelled Issue whose body names the exact test identity, or
/// any Issue whose signature blocks parse and carry it as a `test_id`.
fn open_owner_of(tx: &Transaction<'_>, project_id: Uuid, test: &str) -> Result<Option<i64>> {
    let mut statement = tx.prepare(
        "SELECT display_number, labels, body FROM issues
         WHERE project_id = ?1
           AND status IN ('Open', 'InProgress')
           AND archived_at IS NULL
           AND instr(body, ?2) > 0
         ORDER BY display_number ASC, id ASC",
    )?;
    let rows = statement.query_map(params![project_id.to_string(), test], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (number, labels, body) = row?;
        if issue_owns_test(number, &labels, &body, test) {
            return Ok(Some(number));
        }
    }
    Ok(None)
}

fn issue_owns_test(number: i64, labels: &str, body: &str, test: &str) -> bool {
    let signed = u64::try_from(number)
        .ok()
        .and_then(|number| parse_issue_blocks(body, number).ok())
        .is_some_and(|records| records.iter().any(|record| record.test_id == test));
    signed
        || (serde_json::from_str::<Vec<String>>(labels)
            .is_ok_and(|labels| labels.iter().any(|label| label == "qa"))
            && body_names_test(body, test))
}

/// Whether `body` carries an explicit `Test: `<id>`` field equal to `test`
/// (the field `base_red_issue` writes). Only that structured identity counts:
/// a name that merely appears in prose or inside a command (`test(demo::red)` in
/// a filterset that selects `demo::red_extended`) is an incidental mention
/// (#2704, #2711).
fn body_names_test(body: &str, test: &str) -> bool {
    if test.is_empty() {
        return false;
    }
    body.lines().any(|line| {
        line.trim()
            .strip_prefix("Test:")
            .map(str::trim)
            .map(|value| value.trim_matches('`'))
            .is_some_and(|value| value == test)
    })
}

fn base_red_issue(
    project_id: Uuid,
    red: &GateBaseRed,
    base: Option<&str>,
    log_path: Option<&str>,
) -> NewIssue {
    let mut body = format!(
        "A merge-queue gate observed `{test}` red on the base (the rolling tip) while gating a \
         candidate. \"No new failures relative to rolling\" excuses a red the base shares \
         (#2698), so this red does not by itself decide whether that gate publishes. No open \
         `qa` Issue or known-failure signature named it, so nothing has told the manager: \
         every later gate that selects this test pays a base run for it.\n\nTest: `{test}`\nFailing tests: {test}\nShard command: `{command}`\nBase SHA: `{base}`\n",
        test = red.test,
        command = red.command.as_deref().unwrap_or("unrecorded"),
        base = base.unwrap_or("unrecorded"),
    );
    if let Some(log) = log_path {
        body.push_str(&format!("Gate log: `{log}`\n"));
    }
    body.push_str(
        "\nFix the red on rolling, or add a known-failure signature block if it is a known \
         baseline red. Closing this Issue stops it from deduplicating later gates.\n",
    );
    NewIssue {
        project_id,
        title: format!("Test red on rolling: {}", red.test),
        body,
        priority: Some(1),
        labels: vec!["qa".into(), "bug".into()],
        created_by_session_id: None,
        assignee: None,
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
    }
}

/// The `unowned` reds (indexes into `reds`) grouped into the Issues that file
/// them: reds with the same recorded shard command share one (at most
/// `ROLLING_QUEUE_MAX_FIXES_TESTS` per Issue, so `fixes_issue` can claim all of
/// them), a red with no recorded command is alone. Order follows first
/// appearance.
fn shard_batches(reds: &[&GateBaseRed], unowned: &[usize]) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut open: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for &at in unowned {
        let Some(command) = reds[at].command.as_deref() else {
            batches.push(vec![at]);
            continue;
        };
        match open.get(command) {
            Some(&slot)
                if batches[slot].len()
                    < rsi_common::rolling_queue::ROLLING_QUEUE_MAX_FIXES_TESTS =>
            {
                batches[slot].push(at);
            }
            _ => {
                open.insert(command, batches.len());
                batches.push(vec![at]);
            }
        }
    }
    batches
}

/// Tests red under one shard command from this count up read as one shared
/// cause (setup, prebuild, infrastructure) rather than that many regressions.
const SHARD_WIDE_RED_TESTS: usize = 3;

/// #2865: the one `qa` Issue for several unowned base reds that one shard
/// command observed. It carries the same fields as the single-test Issue: a
/// `Failing tests:` line naming every test (what `fixes_issue` reads, so a
/// landing that claims the Issue must make each of them green) and one
/// ``Test: `<id>` `` line per test (what a later gate's dedupe reads).
fn base_shard_red_issue(
    project_id: Uuid,
    reds: &[&GateBaseRed],
    base: Option<&str>,
    log_path: Option<&str>,
) -> NewIssue {
    let count = reds.len();
    let tests: Vec<&str> = reds.iter().map(|red| red.test.as_str()).collect();
    let command = reds[0].command.as_deref().unwrap_or("unrecorded");
    let mut body = format!(
        "A merge-queue gate observed {count} tests red on the base (the rolling tip) while gating \
         a candidate, all under one shard command, so they are filed as one Issue. \"No new \
         failures relative to rolling\" excuses a red the base shares (#2698), so these reds do \
         not by themselves decide whether that gate publishes. No open `qa` Issue or \
         known-failure signature named them, so nothing has told the manager: every later gate \
         that selects them pays a base run for each.\n\n"
    );
    if count >= SHARD_WIDE_RED_TESTS {
        body.push_str(&format!(
            "Shard-wide base failure: {count} tests failed under the same shard command, which \
             usually means one shared cause (a setup or prebuild step, the environment, a shared \
             fixture) rather than {count} independent regressions. Read the gate log for the \
             first error the tests have in common before fixing them one by one.\n\n"
        ));
    }
    body.push_str(&format!("Failing tests: {}\n", tests.join(", ")));
    for test in &tests {
        body.push_str(&format!("Test: `{test}`\n"));
    }
    body.push_str(&format!(
        "Shard command: `{command}`\nBase SHA: `{}`\n",
        base.unwrap_or("unrecorded")
    ));
    if let Some(log) = log_path {
        body.push_str(&format!("Gate log: `{log}`\n"));
    }
    body.push_str(
        "\nFix the shared cause on rolling, or add a known-failure signature block if these are \
         known baseline reds. A landing that names this Issue in `fixes_issue` claims every test \
         above, so each must be green. Closing this Issue stops it from deduplicating later \
         gates.\n",
    );
    NewIssue {
        project_id,
        title: format!(
            "Tests red on rolling: {} and {} more (one shard)",
            tests[0],
            count - 1
        ),
        body,
        priority: Some(1),
        labels: vec!["qa".into(), "bug".into()],
        created_by_session_id: None,
        assignee: None,
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    use rsi_common::agent_failure_signatures::FAILURE_SIGNATURE_BODY_MARKER;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    use rsi_common::rolling_queue::RollingQueueBinding;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    use rsi_common::types::Project;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn fixture() -> (Store, Uuid, Uuid, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "base-red-fixture".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let (batch, entry) = claim_entry(&store, project);
        (store, project, batch, entry)
    }

    /// One claimed queue entry of `project` (the gate that reports the reds).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn claim_entry(store: &Store, project: Uuid) -> (Uuid, Uuid) {
        let now = Utc::now();
        store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: "/tmp/base-red-repo".into(),
                    source_commit: "a".repeat(40),
                    source_session_id: Uuid::new_v4(),
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: vec![],
                    test_filters: vec![],
                    fixes_tests: Vec::new(),
                    idempotency_key: "source-0".into(),
                },
                now,
            )
            .unwrap();
        let batch = store.claim_rolling_queue_batch(now, 1).unwrap().unwrap();
        (batch.batch_id, batch.entries[0].id)
    }

    /// The later gate: settle the first claimed `entry` (a second batch cannot
    /// be claimed while the first is held), then enqueue and claim a second
    /// source of `project`. Its entry and batch ids differ from the first's.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn later_gate(store: &Store, project: Uuid, entry: Uuid) -> (Uuid, Uuid) {
        let now = Utc::now();
        store
            .settle_rolling_queue_entry(
                entry,
                rsi_common::rolling_queue::RollingQueueEntryState::Failed,
                &rsi_common::rolling_queue::RollingQueueOutcome {
                    landed_sha: None,
                    refusal: Some("queue_test_failed".into()),
                    failing_tests: Vec::new(),
                    detail: None,
                    ..Default::default()
                },
                now,
            )
            .unwrap()
            .unwrap();
        store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: "/tmp/base-red-repo".into(),
                    source_commit: "c".repeat(40),
                    source_session_id: Uuid::new_v4(),
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: vec![],
                    test_filters: vec![],
                    fixes_tests: Vec::new(),
                    idempotency_key: "source-later".into(),
                },
                now,
            )
            .unwrap();
        let batch = store.claim_rolling_queue_batch(now, 1).unwrap().unwrap();
        (batch.batch_id, batch.entries[0].id)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn filter(project: Uuid) -> rsi_common::types::IssueFilter {
        rsi_common::types::IssueFilter {
            project_id: Some(project),
            ..Default::default()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn red(test: &str) -> GateBaseRed {
        GateBaseRed {
            test: test.into(),
            command: Some("cargo test -p rsi-common --test research_corpus".into()),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn a_new_base_red_files_one_qa_issue_naming_test_command_and_base() {
        let (store, project, batch, entry) = fixture();
        let reported = store
            .report_gate_base_reds(
                batch,
                entry,
                Some(&"b".repeat(40)),
                Some("/tmp/gate.log"),
                &[red("research_corpus::every_sidecar_validates")],
                Utc::now(),
            )
            .unwrap();
        assert_eq!(reported.len(), 1);
        assert!(reported[0].filed);
        let issue = store
            .list_issues(&filter(project))
            .unwrap()
            .into_iter()
            .find(|issue| issue.display_number == reported[0].issue)
            .unwrap();
        assert!(issue.labels.contains(&"qa".to_string()));
        assert!(
            issue
                .title
                .contains("research_corpus::every_sidecar_validates")
        );
        assert!(issue.body.contains("--test research_corpus"));
        assert!(issue.body.contains(&"b".repeat(40)));
        assert!(issue.body.contains("/tmp/gate.log"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn a_replay_and_a_later_gate_dedupe_into_the_open_issue() {
        let (store, project, batch, entry) = fixture();
        let reds = [red("research_corpus::every_sidecar_validates")];
        let first = store
            .report_gate_base_reds(batch, entry, None, None, &reds, Utc::now())
            .unwrap();
        let again = store
            .report_gate_base_reds(batch, entry, None, None, &reds, Utc::now())
            .unwrap();
        assert_eq!(again[0].issue, first[0].issue);
        assert!(!again[0].filed);
        assert_eq!(store.list_issues(&filter(project)).unwrap().len(), 1);
    }

    /// #2743: the manager seat hears of a new base red at once, as one typed
    /// notice naming the Issue, the test and the rolling SHA; a replay and a
    /// later gate that dedupes into the open Issue add no second notice.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_new_base_red_notifies_the_current_manager_once_per_issue() {
        use crate::store::manager_coordinator::tests::fixture as manager_fixture;
        use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};
        let store = Store::open_in_memory().unwrap();
        let (config, _lead) = manager_fixture(
            &store,
            ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        );
        let (batch, entry) = claim_entry(&store, config.project_id);
        let base = "b".repeat(40);
        let reds = [red("research_corpus::every_sidecar_validates")];
        let first = store
            .report_gate_base_reds(batch, entry, Some(&base), None, &reds, Utc::now())
            .unwrap();
        assert!(first[0].filed);
        store
            .report_gate_base_reds(batch, entry, Some(&base), None, &reds, Utc::now())
            .unwrap();

        let manager = config.current_session_id.unwrap();
        let (notices, _) = store
            .retrieve_manager_notices(&config, manager, true, 32, None)
            .unwrap();
        let subject = format!("qa_issue:{}", first[0].issue);
        let found: Vec<_> = notices
            .iter()
            .filter(|notice| notice.subject_id == subject)
            .collect();
        assert_eq!(found.len(), 1, "one notice per Issue: {notices:?}");
        let state = &found[0].state;
        assert_eq!(state["record_kind"], "qa_issue");
        assert_eq!(state["issue_number"], first[0].issue);
        assert_eq!(state["rolling_sha"], base);
        assert_eq!(state["outcome"], "test_red");
        assert_eq!(
            state["failing_tests"][0],
            "research_corpus::every_sidecar_validates"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn issue_with(project: Uuid, title: &str, body: String, labels: &[&str]) -> NewIssue {
        NewIssue {
            project_id: project,
            title: title.into(),
            body,
            priority: None,
            labels: labels.iter().map(|label| (*label).to_string()).collect(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        }
    }

    /// A signature block as the known-failure tooling reads it: the record
    /// names the hosting Issue's own number.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn signature_body(test_id: &str, issue: i64) -> String {
        let record = serde_json::json!({
            "test_id": test_id,
            "matcher": {"contains": ["boom"]},
            "issue": issue,
            "class": "regression",
        });
        format!("```{FAILURE_SIGNATURE_BODY_MARKER}\n{record}\n```\n")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_open_signature_issue_carrying_the_test_owns_the_red() {
        let (store, project, batch, entry) = fixture();
        // The first Issue of the project is number 1.
        let owner = store
            .create_issue(&issue_with(
                project,
                "known red",
                signature_body("demo::red", 1),
                &[],
            ))
            .unwrap();
        assert_eq!(owner.display_number, 1);
        let reported = store
            .report_gate_base_reds(batch, entry, None, None, &[red("demo::red")], Utc::now())
            .unwrap();
        assert_eq!(reported[0].issue, owner.display_number);
        assert!(!reported[0].filed);
        assert_eq!(store.list_issues(&filter(project)).unwrap().len(), 1);
    }

    /// #2704: the open Issue names only `demo::red_extended`; `demo::red` is a
    /// different test and gets its own Issue, while the extended one dedupes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_prefix_test_name_does_not_dedupe_into_a_longer_name() {
        let (store, project, batch, entry) = fixture();
        let owner = store
            .report_gate_base_reds(
                batch,
                entry,
                None,
                None,
                &[red("demo::red_extended")],
                Utc::now(),
            )
            .unwrap();
        assert!(owner[0].filed);
        let prefix = store
            .report_gate_base_reds(batch, entry, None, None, &[red("demo::red")], Utc::now())
            .unwrap();
        assert!(prefix[0].filed, "{prefix:?}");
        assert_ne!(prefix[0].issue, owner[0].issue);
        let again = store
            .report_gate_base_reds(
                batch,
                entry,
                None,
                None,
                &[red("demo::red_extended")],
                Utc::now(),
            )
            .unwrap();
        assert_eq!(again[0].issue, owner[0].issue);
        assert!(!again[0].filed);
        assert_eq!(store.list_issues(&filter(project)).unwrap().len(), 2);
    }

    /// #2704: other whole-identity boundaries around the match.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_test_name_is_matched_as_a_whole_identity() {
        for (body, owns) in [
            ("Test: `demo::red`\nShard", true),
            ("Intro\nTest: demo::red\n", true),
            (
                "Test: `demo::red_extended`\nShard command: `cargo nextest run --filterset 'test(demo::red)'`\n",
                false,
            ),
            ("fails: demo::red.", false),
            ("demo::red, demo::other", false),
            ("Test: `demo::red, demo::other`", false),
            ("Test: `other::demo::red`", false),
            ("demo::red_extended", false),
            ("demo::red::inner", false),
            ("other::demo::red", false),
            ("my_demo::red", false),
            ("demo::red.inner", false),
            ("demo::redo", false),
            ("", false),
        ] {
            assert_eq!(body_names_test(body, "demo::red"), owns, "{body:?}");
        }
    }

    /// #2711: a command that mentions `demo::red` inside a filterset does not
    /// make an Issue whose `Test:` is `demo::red_extended` own `demo::red`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_command_mention_does_not_own_the_red() {
        let (store, project, batch, entry) = fixture();
        let longer = store
            .create_issue(&issue_with(
                project,
                "longer test red",
                "Test: `demo::red_extended`\nShard command: `cargo nextest run --filterset \
                 'test(demo::red)'`\n"
                    .into(),
                &["qa"],
            ))
            .unwrap();
        let exact = store
            .create_issue(&issue_with(
                project,
                "exact red",
                "Test: `demo::exact`\nShard command: `cargo test`\n".into(),
                &["qa"],
            ))
            .unwrap();
        let reported = store
            .report_gate_base_reds(
                batch,
                entry,
                None,
                None,
                &[red("demo::red"), red("demo::exact")],
                Utc::now(),
            )
            .unwrap();
        assert!(reported[0].filed, "{reported:?}");
        assert_ne!(reported[0].issue, longer.display_number);
        assert_eq!(reported[1].issue, exact.display_number);
        assert!(!reported[1].filed);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn red_on(test: &str, command: Option<&str>) -> GateBaseRed {
        GateBaseRed {
            test: test.into(),
            command: command.map(str::to_string),
        }
    }

    /// #2865: N unowned reds one shard command observed are ONE `qa` Issue that
    /// names all N on its `Failing tests:` line (so `fixes_issue` claims each),
    /// says it is a shard-wide failure, and sends one notice.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn reds_of_one_shard_command_file_exactly_one_issue_naming_all_of_them() {
        let (store, project, batch, entry) = fixture();
        let names: Vec<String> = (0..22).map(|i| format!("e2e_tui::case_{i:02}")).collect();
        let reds: Vec<GateBaseRed> = names.iter().map(|name| red(name)).collect();
        let reported = store
            .report_gate_base_reds(batch, entry, Some(&"b".repeat(40)), None, &reds, Utc::now())
            .unwrap();
        assert_eq!(reported.len(), 22);
        assert!(
            reported
                .iter()
                .all(|r| r.filed && r.issue == reported[0].issue)
        );
        assert_eq!(
            reported.iter().map(|r| r.test.clone()).collect::<Vec<_>>(),
            names
        );
        let issues = store.list_issues(&filter(project)).unwrap();
        assert_eq!(issues.len(), 1, "{issues:?}");
        let body = &issues[0].body;
        assert_eq!(rsi_common::rolling_queue::failing_tests_listed(body), names);
        assert!(body.contains(&format!("Failing tests: {}\n", names.join(", "))));
        assert!(body.contains("Shard-wide base failure: 22 tests"), "{body}");
        for name in &names {
            assert!(body_names_test(body, name), "{name}");
        }
    }

    /// #2865: a later gate that sees the same reds, or a subset of them, files
    /// nothing and names the open Issue.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_later_gate_with_a_subset_of_a_shard_issues_reds_files_nothing() {
        let (store, project, batch, entry) = fixture();
        let reds: Vec<GateBaseRed> = ["a::one", "a::two", "a::three", "a::four"]
            .iter()
            .map(|name| red(name))
            .collect();
        let first = store
            .report_gate_base_reds(batch, entry, None, None, &reds, Utc::now())
            .unwrap();
        // The later gate is a DIFFERENT queue entry (and batch id): the Issue
        // idempotency key is derived from the entry id, so only an owner
        // lookup across the project can dedupe this report.
        let (later_batch, later) = later_gate(&store, project, entry);
        assert_ne!(later, entry);
        assert_ne!(later_batch, batch);
        let subset = store
            .report_gate_base_reds(
                later_batch,
                later,
                None,
                None,
                &[red("a::two"), red("a::four")],
                Utc::now(),
            )
            .unwrap();
        assert_eq!(subset.len(), 2);
        assert!(subset.iter().all(|r| !r.filed && r.issue == first[0].issue));
        assert_eq!(store.list_issues(&filter(project)).unwrap().len(), 1);
    }

    /// #2865: only reds that share a recorded shard command are grouped; a
    /// different command or no recorded command files its own Issue.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn reds_of_different_or_unrecorded_shards_stay_separate_issues() {
        let (store, project, batch, entry) = fixture();
        let reds = [
            red_on("a::one", Some("cargo test -p a")),
            red_on("a::two", Some("cargo test -p a")),
            red_on("b::one", Some("cargo test -p b")),
            red_on("c::one", None),
            red_on("c::two", None),
        ];
        let reported = store
            .report_gate_base_reds(batch, entry, None, None, &reds, Utc::now())
            .unwrap();
        assert_eq!(reported[0].issue, reported[1].issue);
        let numbers: std::collections::BTreeSet<i64> = reported.iter().map(|r| r.issue).collect();
        assert_eq!(numbers.len(), 4, "{reported:?}");
        assert_eq!(store.list_issues(&filter(project)).unwrap().len(), 4);
        // A pair is not yet "shard-wide".
        let pair = store
            .list_issues(&filter(project))
            .unwrap()
            .into_iter()
            .find(|issue| issue.display_number == reported[0].issue)
            .unwrap();
        assert!(!pair.body.contains("Shard-wide"));
        assert!(pair.body.contains("Failing tests: a::one, a::two\n"));
    }

    /// #2865: one Issue never lists more tests than `fixes_issue` can claim.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_shard_issue_never_lists_more_tests_than_a_landing_can_claim() {
        let (store, project, batch, entry) = fixture();
        let cap = rsi_common::rolling_queue::ROLLING_QUEUE_MAX_FIXES_TESTS;
        let reds: Vec<GateBaseRed> = (0..cap + 3)
            .map(|i| red(&format!("big::case_{i:03}")))
            .collect();
        let reported = store
            .report_gate_base_reds(batch, entry, None, None, &reds, Utc::now())
            .unwrap();
        assert_eq!(reported.len(), cap + 3);
        let issues = store.list_issues(&filter(project)).unwrap();
        assert_eq!(issues.len(), 2);
        for issue in issues {
            assert!(rsi_common::rolling_queue::failing_tests_listed(&issue.body).len() <= cap);
        }
    }

    /// #2712: the Issue is outcome-neutral, so a failed gate never claims a
    /// publication.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_filed_issue_does_not_claim_a_publication() {
        let new = base_red_issue(Uuid::new_v4(), &red("demo::red"), Some("abc"), None);
        assert!(!new.body.contains("published its candidate anyway"));
        assert!(new.body.contains("observed `demo::red` red on the base"));
        assert!(body_names_test(&new.body, "demo::red"));
    }

    /// #2787: the filed Issue round-trips through `fixes_issue`: the parser the
    /// queue uses reads the one red test back, from the canonical
    /// `Failing tests:` line (and from the legacy `Test:` line alone).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_filed_issue_lists_its_test_for_fixes_issue() {
        let new = base_red_issue(Uuid::new_v4(), &red("demo::red"), Some("abc"), None);
        assert!(
            new.body
                .lines()
                .any(|line| line == "Failing tests: demo::red")
        );
        assert_eq!(
            rsi_common::rolling_queue::failing_tests_listed(&new.body),
            vec!["demo::red".to_string()]
        );
        let legacy: String = new
            .body
            .lines()
            .filter(|line| !line.starts_with("Failing tests:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rsi_common::rolling_queue::failing_tests_listed(&legacy),
            vec!["demo::red".to_string()]
        );
    }

    /// #3905: the auto-filer's own Issue bodies round-trip through the
    /// `fixes_issue` selection: a `cargo test -p PKG --lib <filter>` shard
    /// command selects the exact lib filter of each named test, so a fix whose
    /// diff lives in another package still gates the claimed tests. The filer's
    /// format and the parser cannot drift apart without this going red.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_filed_issue_round_trips_through_the_fixes_issue_selection() {
        use rsi_common::rolling_queue::issue_selecting_filters;
        let one = base_red_issue(
            Uuid::new_v4(),
            &red_on(
                "agent_deploy::tests::no_crate_but_the_guard_crate_depends_on_rsi_repo_guards",
                Some("cargo test -p rsi-common --lib agent_deploy -- --test-threads=4"),
            ),
            Some("abc"),
            Some("/tmp/gate.log"),
        );
        assert_eq!(
            issue_selecting_filters(&one.body),
            [
                "rsi-common=test(=agent_deploy::tests::no_crate_but_the_guard_crate_depends_on_rsi_repo_guards)"
            ]
        );
        let command = Some("cargo test -p rsi-common --lib agent_deploy");
        let (a, b) = (
            red_on("agent_deploy::tests::a", command),
            red_on("agent_deploy::tests::b", command),
        );
        let shard = base_shard_red_issue(Uuid::new_v4(), &[&a, &b], Some("abc"), None);
        assert_eq!(
            issue_selecting_filters(&shard.body),
            [
                "rsi-common=test(=agent_deploy::tests::a)",
                "rsi-common=test(=agent_deploy::tests::b)"
            ]
        );
        // The target form the filer also writes keeps selecting its target.
        let target = base_red_issue(
            Uuid::new_v4(),
            &red_on(
                "guard::a",
                Some("cargo test -p rsi-common --test manifest_dir_guard"),
            ),
            None,
            None,
        );
        assert_eq!(
            issue_selecting_filters(&target.body),
            ["rsi-common=test:manifest_dir_guard"]
        );
    }

    /// #2704: a signature marker plus an incidental mention is not ownership.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_signature_marker_plus_an_incidental_mention_does_not_own_the_red() {
        let (store, project, batch, entry) = fixture();
        let other = store
            .create_issue(&issue_with(
                project,
                "known red of another test",
                format!(
                    "Unlike demo::red this one is known.\n{}",
                    signature_body("demo::other", 1)
                ),
                &[],
            ))
            .unwrap();
        let malformed = store
            .create_issue(&issue_with(
                project,
                "malformed signature",
                format!("demo::red\n```{FAILURE_SIGNATURE_BODY_MARKER}\ndemo::red\n```\n"),
                &[],
            ))
            .unwrap();
        let reported = store
            .report_gate_base_reds(batch, entry, None, None, &[red("demo::red")], Utc::now())
            .unwrap();
        assert!(reported[0].filed, "{reported:?}");
        assert_ne!(reported[0].issue, other.display_number);
        assert_ne!(reported[0].issue, malformed.display_number);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn an_entry_without_a_known_project_files_nothing() {
        let (store, project, batch, _) = fixture();
        let reported = store
            .report_gate_base_reds(
                batch,
                Uuid::new_v4(),
                None,
                None,
                &[red("demo::red")],
                Utc::now(),
            )
            .unwrap();
        assert!(reported.is_empty());
        assert!(store.list_issues(&filter(project)).unwrap().is_empty());
    }
}
