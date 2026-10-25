//! Daemon-evaluated wait predicates for `AgentScheduleWake` mode `when` (#1006).
//!
//! A routine wait ("tell me when these jobs are done", "tell me when this
//! commit is on rolling") is a fact the daemon can check itself, so the caller
//! is woken once when the predicate is true (or its timeout passes) instead of
//! taking a model turn per event. The predicate and its bookkeeping live under
//! the `$.wake_when` key of the wake row's `schedule_json`; no table is added.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Most job ids one predicate may name.
pub const WAKE_WHEN_MAX_JOBS: usize = 32;
/// Most commit ids one predicate may name.
pub const WAKE_WHEN_MAX_SHAS: usize = 32;
/// Longest `timeout_seconds` (seven days).
pub const WAKE_WHEN_MAX_TIMEOUT_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Most enabled predicate wakes one session may hold, not counting
/// `topology_execution_settled` wakes (they have their own cap below). One
/// more is refused [`WAKE_WHEN_CAP_REACHED`]; batch commits into one
/// `shas_on_rolling` wake (with `fire: "each"` for one firing per landing,
/// #217) instead of one wake per commit.
pub const WAKE_WHEN_MAX_PER_SESSION: usize = 16;
/// Most enabled `topology_execution_settled` wakes one session may hold (#3019).
/// A manager runs every ordinary Issue as a pipeline with one daemon-armed settle
/// wake each, so 14 or more concurrent executions are normal; counting them against
/// [`WAKE_WHEN_MAX_PER_SESSION`] silently dropped the newest executions' wakes.
/// They are counted apart, so they neither starve nor are starved by the other
/// predicates.
pub const WAKE_WHEN_MAX_SETTLE_PER_SESSION: usize = 128;
/// The daemon re-evaluates a pending `jobs_terminal` predicate this often.
pub const WAKE_WHEN_JOBS_POLL_SECS: i64 = 5;
/// The daemon re-checks pending single-commit and batch rolling predicates this often.
pub const WAKE_WHEN_SHA_POLL_SECS: i64 = 15;
/// Per-job bound on the refusal text carried by the wake message.
pub const WAKE_WHEN_REFUSAL_BYTES: usize = 200;
/// Bound on the whole wake message body the daemon appends.
pub const WAKE_WHEN_MESSAGE_BYTES: usize = 6_000;
/// The `delivery` field of an accepted `AgentScheduleWake` mode `when` reply
/// (#170). The arming call is always made mid-turn, so nothing has fired into
/// it: the predicate is pending and its report arrives later as a new turn.
/// `next_fire_at` is the next evaluation, which is often the arming instant.
pub const WAKE_WHEN_ARMED_DELIVERY: &str = "pending: nothing has been delivered. \
next_fire_at is the next evaluation, not a delivery. The daemon resumes this session \
once, as a new turn (<rsid-daemon-message source=\"scheduled-wake\">) carrying a \
[wake_when] report with each job's stored state and exit_code, when the predicate holds; \
while this turn runs that wake is held (continuation_target_busy). End your turn now.";

/// Stable refusal codes; each is the whole `InvalidParam` text.
pub const WAKE_WHEN_PREDICATE_INVALID: &str = "wake_when_predicate_invalid";
pub const WAKE_WHEN_JOB_NOT_FOUND: &str = "wake_when_job_not_found";

/// The refusal text for a `jobs_terminal` id the caller does not own (#2867):
/// the stable code first (the andon signature and `contains` checks key on it),
/// then the fix. Agents repeatedly armed a wake on the newest file under the
/// shared jobs log directory, which can belong to another session; the id must
/// be the one the caller's own `AgentSubmitJob` returned.
#[must_use]
pub fn wake_when_job_not_found_message(id: impl std::fmt::Display) -> String {
    format!(
        "{WAKE_WHEN_JOB_NOT_FOUND}: job {id} is unknown or not owned by this session; \
         wait on the job id that your own AgentSubmitJob call returned (result.id), \
         never one read from the shared jobs log directory"
    )
}
pub const WAKE_WHEN_ISSUE_NOT_FOUND: &str = "wake_when_issue_not_found";
/// An unknown operator to-do id, or one outside the caller's own scope.
pub const WAKE_WHEN_TODO_NOT_FOUND: &str = "wake_when_todo_not_found";
/// An unknown topology execution id, or one outside the caller's scope.
pub const WAKE_WHEN_EXECUTION_NOT_FOUND: &str = "wake_when_execution_not_found";
/// An unknown source-worktree settlement run id, or one outside the caller's
/// cohort authority (#2789).
pub const WAKE_WHEN_SETTLEMENT_RUN_NOT_FOUND: &str = "wake_when_settlement_run_not_found";
/// `health_anomaly` is a manager's predicate: a PM or a portfolio seat.
pub const WAKE_WHEN_HEALTH_MANAGER_ONLY: &str = "wake_when_health_anomaly_manager_only";
pub const WAKE_WHEN_TIMEOUT_INVALID: &str = "wake_when_timeout_invalid";
pub const WAKE_WHEN_CAP_REACHED: &str = "wake_when_cap_reached";
pub const WAKE_WHEN_FIELD_MISPLACED: &str = "wake_when_field_requires_mode_when";
pub const WAKE_WHEN_TIMING_UNSUPPORTED: &str = "wake_when_uses_daemon_timing";

/// Typed reasons a predicate settles without becoming true.
pub const REASON_JOB_MISSING: &str = "job_missing";
pub const REASON_REPO_UNAVAILABLE: &str = "repo_unavailable";
pub const REASON_ISSUE_MISSING: &str = "issue_missing";
pub const REASON_ISSUE_CANCELLED: &str = "issue_cancelled";
pub const REASON_TODO_MISSING: &str = "todo_missing";
pub const REASON_EXECUTION_MISSING: &str = "execution_missing";
pub const REASON_SETTLEMENT_RUN_MISSING: &str = "settlement_run_missing";

/// When a `shas_on_rolling` wake fires (#217).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WakeFire {
    /// Once, when every listed commit is on rolling (the default).
    #[default]
    All,
    /// Once per landing: each evaluation that finds commits newly on rolling
    /// resumes the owner and names them; the wake stays armed until every
    /// listed commit has landed (or its timeout passes).
    Each,
}

/// The typed predicate: exactly one predicate field is set (`fire` is a
/// modifier of `shas_on_rolling`, not a predicate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WakePredicate {
    /// Every listed job (owned by the caller) is terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jobs_terminal: Option<Vec<Uuid>>,
    /// This 40-hex commit is an ancestor of `origin/rolling` in the caller's
    /// repository (as last fetched or pushed there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha_on_rolling: Option<String>,
    /// Every listed 40-hex commit is an ancestor of `origin/rolling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shas_on_rolling: Option<Vec<String>>,
    /// Firing mode of `shas_on_rolling` (#217); absent means
    /// [`WakeFire::All`]. Any other predicate with `fire` is invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire: Option<WakeFire>,
    /// The Issue with this display number (in the caller's project) is
    /// `Closed`: the manager closes an Issue once its work is on `rolling`, so
    /// a worker can wait on a dependency's landing without knowing its SHA.
    /// A `Cancelled` or vanished Issue settles the wait as unsatisfiable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_closed: Option<u32>,
    /// The operator to-do item with this id (in the caller's own scope) is
    /// closed, `done` or `dismissed`; the wake carries the status and the
    /// closing evidence. A vanished item settles the wait as unsatisfiable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_closed: Option<Uuid>,
    /// The topology execution with this id settles: its status is
    /// `succeeded`, `failed`, `cancelled` or `blocked` (#2261). The caller is
    /// the execution's requester, or a manager or Epic lead that can read its
    /// Epic. The wake carries the status, error, blocked-reason summary and
    /// finish time. A blocked execution that is later resolved needs a new
    /// wake. A vanished execution settles the wait as unsatisfiable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology_execution_settled: Option<Uuid>,
    /// Any daemon health anomaly holds (#2747): a PM seat stuck behind a
    /// refused succession or over its rotation threshold, blocked or overrun
    /// topology work, hub disk or CPU temperature, a provider fault, a red
    /// rolling QA no Issue tracks. Manager seats only. The wake carries one
    /// typed line per condition; with nothing abnormal the owner takes no
    /// model turn at all. `{}` watches everything the seat covers; `ignore`
    /// lists `kind:subject` keys already seen (the `ignore_key=` value the wake line printed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_anomaly: Option<crate::health_anomaly::HealthAnomalySpec>,
    /// The source-worktree settlement run with this id is finished (#2789):
    /// every root is `settled`, `refused`, `recovery_required` or
    /// `unattempted`, so the run row carries `finished_at`. Pair it with
    /// `AgentApplySourceWorktreeCohort`, which returns the run id at once
    /// while the run is still applying. The caller needs the cohort's
    /// authority. The wake carries the run state, the per-phase counts and any
    /// terminal error. A vanished run settles the wait as unsatisfiable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement_run_settled: Option<Uuid>,
}

/// True for a lowercase 40-hex object id.
#[must_use]
pub fn is_full_sha(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl WakePredicate {
    /// Exactly one predicate, well formed and bounded.
    ///
    /// # Errors
    /// [`WAKE_WHEN_PREDICATE_INVALID`].
    pub fn validate(&self) -> Result<(), &'static str> {
        let set = usize::from(self.jobs_terminal.is_some())
            + usize::from(self.sha_on_rolling.is_some())
            + usize::from(self.shas_on_rolling.is_some())
            + usize::from(self.issue_closed.is_some())
            + usize::from(self.todo_closed.is_some())
            + usize::from(self.topology_execution_settled.is_some())
            + usize::from(self.health_anomaly.is_some())
            + usize::from(self.settlement_run_settled.is_some());
        if set != 1 || (self.fire.is_some() && self.shas_on_rolling.is_none()) {
            return Err(WAKE_WHEN_PREDICATE_INVALID);
        }
        if let Some(ids) = &self.jobs_terminal {
            let mut seen = std::collections::BTreeSet::new();
            if ids.is_empty()
                || ids.len() > WAKE_WHEN_MAX_JOBS
                || ids.iter().any(|id| id.is_nil() || !seen.insert(*id))
            {
                return Err(WAKE_WHEN_PREDICATE_INVALID);
            }
        }
        if let Some(shas) = &self.shas_on_rolling {
            let mut seen = std::collections::BTreeSet::new();
            if shas.is_empty()
                || shas.len() > WAKE_WHEN_MAX_SHAS
                || shas
                    .iter()
                    .any(|sha| !is_full_sha(sha) || !seen.insert(sha))
            {
                return Err(WAKE_WHEN_PREDICATE_INVALID);
            }
        }
        let well_formed = self.sha_on_rolling.as_deref().is_none_or(is_full_sha)
            && self.issue_closed.is_none_or(|number| number > 0)
            && self.todo_closed.is_none_or(|id| !id.is_nil())
            && self
                .topology_execution_settled
                .is_none_or(|id| !id.is_nil())
            && self.settlement_run_settled.is_none_or(|id| !id.is_nil())
            && self
                .health_anomaly
                .as_ref()
                .is_none_or(crate::health_anomaly::HealthAnomalySpec::is_valid);
        if well_formed {
            Ok(())
        } else {
            Err(WAKE_WHEN_PREDICATE_INVALID)
        }
    }

    /// True for a `shas_on_rolling` wake that fires once per landing (#217).
    #[must_use]
    pub fn fires_each(&self) -> bool {
        self.shas_on_rolling.is_some() && self.fire == Some(WakeFire::Each)
    }

    /// Seconds between daemon evaluations while the predicate is pending.
    #[must_use]
    pub fn poll_seconds(&self) -> i64 {
        if self.sha_on_rolling.is_some() || self.shas_on_rolling.is_some() {
            WAKE_WHEN_SHA_POLL_SECS
        } else if self.health_anomaly.is_some() {
            crate::health_anomaly::HEALTH_POLL_SECS
        } else {
            WAKE_WHEN_JOBS_POLL_SECS
        }
    }
}

/// Validate `timeout_seconds` for a `when` wake.
///
/// # Errors
/// [`WAKE_WHEN_TIMEOUT_INVALID`].
pub fn validate_timeout(timeout_seconds: Option<i64>) -> Result<(), &'static str> {
    match timeout_seconds {
        Some(seconds) if !(1..=WAKE_WHEN_MAX_TIMEOUT_SECONDS).contains(&seconds) => {
            Err(WAKE_WHEN_TIMEOUT_INVALID)
        }
        _ => Ok(()),
    }
}

/// The persisted `$.wake_when` record of a predicate wake row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeWhenState {
    pub predicate: WakePredicate,
    pub armed_at: DateTime<Utc>,
    /// When the wake fires with `timed_out: true` if the predicate is still
    /// pending. `None` waits until the predicate settles.
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
    /// Directory of the caller's repository, for rolling commit predicates.
    #[serde(default)]
    pub repo_dir: Option<String>,
    /// #217: the commits a `fire: "each"` wake has already reported as on
    /// rolling, in delivery order. Written only after a delivery succeeded,
    /// so a held or refused firing reports the same commits again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivered_shas: Vec<String>,
}

/// #3387: the daemon-owned `$.quiet_settle` record of a topology settle wake
/// the daemon armed by default for an `AgentTopologyExecute`. Its presence
/// marks the wake as quiet-capable: a clean success (the execution
/// `succeeded`, its Issue Closed, no follow-up, no `changes_requested`, no
/// refused close) opens no owner turn of its own. The row is held with its
/// digest line and is delivered with the owner's next wake, or alone as one
/// batch once the oldest held line is [`QUIET_SETTLE_MAX_HOLD_SECS`] old.
/// Every other outcome, and any wake the owner armed itself (it has no such
/// record), wakes the owner at once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietSettle {
    /// Set once the wake observed a clean success and stopped short of a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<QuietHeld>,
}

/// One clean success held in the owner's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHeld {
    /// When the wake first held the success.
    pub at: DateTime<Utc>,
    /// The digest line: execution, Issue and landed SHA.
    pub line: String,
}

/// Longest a clean success stays in the digest before it is delivered alone
/// (one batched wake for everything held by then).
pub const QUIET_SETTLE_MAX_HOLD_SECS: i64 = 30 * 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_bounded_predicate_is_valid() {
        let id = Uuid::new_v4();
        let ok = WakePredicate {
            jobs_terminal: Some(vec![id]),
            sha_on_rolling: None,
            shas_on_rolling: None,
            issue_closed: None,
            todo_closed: None,
            topology_execution_settled: None,
            health_anomaly: None,
            settlement_run_settled: None,
            fire: None,
        };
        assert_eq!(ok.validate(), Ok(()));
        let sha = WakePredicate {
            jobs_terminal: None,
            sha_on_rolling: Some("a".repeat(40)),
            shas_on_rolling: None,
            issue_closed: None,
            todo_closed: None,
            topology_execution_settled: None,
            health_anomaly: None,
            settlement_run_settled: None,
            fire: None,
        };
        assert_eq!(sha.validate(), Ok(()));
        let issue = WakePredicate {
            issue_closed: Some(1881),
            ..WakePredicate::default()
        };
        assert_eq!(issue.validate(), Ok(()));
        let todo = WakePredicate {
            todo_closed: Some(id),
            ..WakePredicate::default()
        };
        assert_eq!(todo.validate(), Ok(()));
        let execution = WakePredicate {
            topology_execution_settled: Some(id),
            ..WakePredicate::default()
        };
        assert_eq!(execution.validate(), Ok(()));
        for bad in [
            WakePredicate {
                topology_execution_settled: Some(Uuid::nil()),
                ..WakePredicate::default()
            },
            WakePredicate {
                topology_execution_settled: Some(id),
                todo_closed: Some(id),
                ..WakePredicate::default()
            },
            WakePredicate {
                topology_execution_settled: Some(id),
                jobs_terminal: Some(vec![id]),
                ..WakePredicate::default()
            },
            WakePredicate {
                todo_closed: Some(Uuid::nil()),
                ..WakePredicate::default()
            },
            WakePredicate {
                todo_closed: Some(id),
                issue_closed: Some(7),
                ..WakePredicate::default()
            },
            WakePredicate {
                todo_closed: Some(id),
                jobs_terminal: Some(vec![id]),
                ..WakePredicate::default()
            },
            WakePredicate {
                issue_closed: Some(0),
                ..WakePredicate::default()
            },
            WakePredicate {
                issue_closed: Some(7),
                sha_on_rolling: Some("a".repeat(40)),
                ..WakePredicate::default()
            },
            WakePredicate {
                issue_closed: Some(7),
                jobs_terminal: Some(vec![id]),
                ..WakePredicate::default()
            },
            WakePredicate::default(),
            WakePredicate {
                jobs_terminal: Some(vec![]),
                sha_on_rolling: None,
                shas_on_rolling: None,
                issue_closed: None,
                todo_closed: None,
                topology_execution_settled: None,
                health_anomaly: None,
                settlement_run_settled: None,
                fire: None,
            },
            WakePredicate {
                jobs_terminal: Some(vec![id, id]),
                sha_on_rolling: None,
                shas_on_rolling: None,
                issue_closed: None,
                todo_closed: None,
                topology_execution_settled: None,
                health_anomaly: None,
                settlement_run_settled: None,
                fire: None,
            },
            WakePredicate {
                jobs_terminal: Some(vec![id]),
                sha_on_rolling: Some("a".repeat(40)),
                shas_on_rolling: None,
                issue_closed: None,
                todo_closed: None,
                topology_execution_settled: None,
                health_anomaly: None,
                settlement_run_settled: None,
                fire: None,
            },
            WakePredicate {
                jobs_terminal: None,
                sha_on_rolling: Some("A".repeat(40)),
                shas_on_rolling: None,
                issue_closed: None,
                todo_closed: None,
                topology_execution_settled: None,
                health_anomaly: None,
                settlement_run_settled: None,
                fire: None,
            },
            WakePredicate {
                jobs_terminal: None,
                sha_on_rolling: Some("abc".into()),
                shas_on_rolling: None,
                issue_closed: None,
                todo_closed: None,
                topology_execution_settled: None,
                health_anomaly: None,
                settlement_run_settled: None,
                fire: None,
            },
            WakePredicate {
                jobs_terminal: Some((0..=WAKE_WHEN_MAX_JOBS).map(|_| Uuid::new_v4()).collect()),
                sha_on_rolling: None,
                shas_on_rolling: None,
                issue_closed: None,
                todo_closed: None,
                topology_execution_settled: None,
                health_anomaly: None,
                settlement_run_settled: None,
                fire: None,
            },
        ] {
            assert_eq!(bad.validate(), Err(WAKE_WHEN_PREDICATE_INVALID), "{bad:?}");
        }
    }

    /// #217: `fire` modifies `shas_on_rolling` only; `each` is opt-in and
    /// the default (absent or `all`) keeps the all-at-once wire shape.
    #[test]
    fn fire_each_is_an_opt_in_modifier_of_shas_on_rolling_only() {
        let sha = "a".repeat(40);
        let each: WakePredicate = serde_json::from_value(serde_json::json!({
            "shas_on_rolling": [sha],
            "fire": "each"
        }))
        .unwrap();
        assert_eq!(each.validate(), Ok(()));
        assert_eq!(each.fire, Some(WakeFire::Each));
        assert!(each.fires_each());
        assert_eq!(each.poll_seconds(), WAKE_WHEN_SHA_POLL_SECS);
        let all: WakePredicate = serde_json::from_value(serde_json::json!({
            "shas_on_rolling": [sha],
            "fire": "all"
        }))
        .unwrap();
        assert_eq!(all.validate(), Ok(()));
        assert_eq!(all.fire, Some(WakeFire::All));
        assert!(!all.fires_each());
        let default = WakePredicate {
            shas_on_rolling: Some(vec![sha.clone()]),
            ..WakePredicate::default()
        };
        assert!(!default.fires_each());
        assert_eq!(
            serde_json::to_value(&default).unwrap(),
            serde_json::json!({"shas_on_rolling": [sha]})
        );
        for other in [
            serde_json::json!({"sha_on_rolling": sha, "fire": "each"}),
            serde_json::json!({"jobs_terminal": [Uuid::new_v4()], "fire": "each"}),
            serde_json::json!({"issue_closed": 7, "fire": "all"}),
            serde_json::json!({"fire": "each"}),
        ] {
            let predicate: WakePredicate = serde_json::from_value(other.clone()).unwrap();
            assert_eq!(
                predicate.validate(),
                Err(WAKE_WHEN_PREDICATE_INVALID),
                "{other}"
            );
        }
        assert!(
            serde_json::from_value::<WakePredicate>(serde_json::json!({
                "shas_on_rolling": [sha],
                "fire": "sometimes"
            }))
            .is_err()
        );
        // The delivered set rides on the persisted state and is omitted while
        // empty, so a pre-#217 row reads back unchanged.
        let state: WakeWhenState = serde_json::from_value(serde_json::json!({
            "predicate": {"shas_on_rolling": [sha]},
            "armed_at": "2026-10-20T00:00:00Z",
        }))
        .unwrap();
        assert_eq!(state.delivered_shas, Vec::<String>::new());
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json.get("delivered_shas"), None);
        let delivered = WakeWhenState {
            delivered_shas: vec![sha.clone()],
            ..state
        };
        let round: WakeWhenState =
            serde_json::from_value(serde_json::to_value(&delivered).unwrap()).unwrap();
        assert_eq!(round, delivered);
    }

    #[test]
    fn health_anomaly_is_one_predicate_polled_each_minute() {
        use crate::health_anomaly::{HEALTH_POLL_SECS, HealthAnomalySpec};
        let health = WakePredicate {
            health_anomaly: Some(HealthAnomalySpec::default()),
            ..WakePredicate::default()
        };
        assert_eq!(health.validate(), Ok(()));
        assert_eq!(health.poll_seconds(), HEALTH_POLL_SECS);
        let json = serde_json::to_value(&health).unwrap();
        assert_eq!(json, serde_json::json!({"health_anomaly": {}}));
        let both = WakePredicate {
            issue_closed: Some(7),
            ..health.clone()
        };
        assert_eq!(both.validate(), Err(WAKE_WHEN_PREDICATE_INVALID));
        let bad_key = WakePredicate {
            health_anomaly: Some(HealthAnomalySpec {
                ignore: vec!["nonsense:x".into()],
                project: None,
                projects: Vec::new(),
            }),
            ..WakePredicate::default()
        };
        assert_eq!(bad_key.validate(), Err(WAKE_WHEN_PREDICATE_INVALID));
    }

    #[test]
    fn settlement_run_settled_is_one_nonnil_predicate_2789() {
        let run = WakePredicate {
            settlement_run_settled: Some(Uuid::new_v4()),
            ..WakePredicate::default()
        };
        assert_eq!(run.validate(), Ok(()));
        assert_eq!(run.poll_seconds(), WAKE_WHEN_JOBS_POLL_SECS);
        let nil = WakePredicate {
            settlement_run_settled: Some(Uuid::nil()),
            ..WakePredicate::default()
        };
        assert_eq!(nil.validate(), Err(WAKE_WHEN_PREDICATE_INVALID));
        let two = WakePredicate {
            issue_closed: Some(1),
            ..run.clone()
        };
        assert_eq!(two.validate(), Err(WAKE_WHEN_PREDICATE_INVALID));
        let wire: WakePredicate = serde_json::from_value(
            serde_json::json!({"settlement_run_settled": run.settlement_run_settled}),
        )
        .unwrap();
        assert_eq!(wire, run);
    }

    #[test]
    fn timeout_is_bounded() {
        assert_eq!(validate_timeout(None), Ok(()));
        assert_eq!(validate_timeout(Some(1)), Ok(()));
        assert_eq!(validate_timeout(Some(0)), Err(WAKE_WHEN_TIMEOUT_INVALID));
        assert_eq!(
            validate_timeout(Some(WAKE_WHEN_MAX_TIMEOUT_SECONDS + 1)),
            Err(WAKE_WHEN_TIMEOUT_INVALID)
        );
    }

    #[test]
    fn commit_batch_is_bounded_distinct_and_backward_compatible() {
        let old_json = serde_json::json!({"sha_on_rolling": "a".repeat(40)});
        let single: WakePredicate = serde_json::from_value(old_json.clone()).unwrap();
        assert_eq!(single.validate(), Ok(()));
        assert_eq!(serde_json::to_value(single).unwrap(), old_json);

        for count in [1, 2, WAKE_WHEN_MAX_SHAS] {
            let json = serde_json::json!({
                "shas_on_rolling": (0..count).map(|n| format!("{n:040x}")).collect::<Vec<_>>()
            });
            let batch: WakePredicate = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(batch.validate(), Ok(()));
            assert_eq!(batch.poll_seconds(), WAKE_WHEN_SHA_POLL_SECS);
            assert_eq!(serde_json::to_value(batch).unwrap(), json);
        }
        for shas in [
            vec![],
            vec!["a".repeat(40); 2],
            vec!["A".repeat(40)],
            vec!["abc".into()],
            (0..=WAKE_WHEN_MAX_SHAS)
                .map(|n| format!("{n:040x}"))
                .collect(),
        ] {
            let batch = WakePredicate {
                shas_on_rolling: Some(shas),
                ..WakePredicate::default()
            };
            assert_eq!(batch.validate(), Err(WAKE_WHEN_PREDICATE_INVALID));
        }
        let both = WakePredicate {
            sha_on_rolling: Some("a".repeat(40)),
            shas_on_rolling: Some(vec!["b".repeat(40)]),
            ..WakePredicate::default()
        };
        assert_eq!(both.validate(), Err(WAKE_WHEN_PREDICATE_INVALID));
    }
}
