//! Wire types for the daemon-owned rolling merge queue (#1007).
//!
//! `AgentEnqueueLandingSource` admits one accepted source commit. A manager
//! also reads its own entries (`AgentListRollingQueue`) and withdraws a
//! still-`queued` one (`AgentCancelRollingQueueEntry`, #1964); the lander stays
//! the only publisher, so an admitted or gating entry is never touched. The
//! queue settings and the unfiltered queue view are operator-only
//! (`daemon_config` fields and `GetRollingQueue`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ROLLING_QUEUE_MAX_TEST_FILTERS: usize = 32;
pub const ROLLING_QUEUE_MAX_FILTER_BYTES: usize = 256;
pub const ROLLING_QUEUE_MAX_IDEMPOTENCY_BYTES: usize = 128;
pub const ROLLING_QUEUE_MIN_BATCH_SIZE: u64 = 1;
pub const ROLLING_QUEUE_MAX_BATCH_SIZE: u64 = 8;
pub const ROLLING_QUEUE_DEFAULT_BATCH_SIZE: u32 = 4;
pub const ROLLING_QUEUE_LIST_MAX: usize = 200;
/// Wall-clock budget, in minutes, of one batch's gating (every lander run of
/// the batch, its bisect included). Operator setting
/// `rolling_queue_gate_timeout_mins` (#1208). The default is about two and a
/// half times the slowest single gate observed under load (25 minutes), so a
/// stuck gate holds the queue for an hour at most (#1940); a batch past it is
/// refused `queue_gate_timeout` and the queue moves on.
pub const ROLLING_QUEUE_MIN_GATE_TIMEOUT_MINS: u64 = 30;
pub const ROLLING_QUEUE_MAX_GATE_TIMEOUT_MINS: u64 = 1440;
pub const ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS: u32 = 60;
/// Minimum minutes between two starts of the async full-library QA run
/// (`rolling-library-qa`, #2092) for one project's landings. Operator setting
/// `rolling_queue_full_qa_interval_mins` (#2112); landings published in
/// between coalesce into the next run, and 0 starts it as soon as the previous
/// run ends. The cap keeps one interval plus one 180-minute run inside the
/// eight-hour overdue deadline.
pub const ROLLING_QUEUE_MAX_FULL_QA_INTERVAL_MINS: u64 = 240;
pub const ROLLING_QUEUE_DEFAULT_FULL_QA_INTERVAL_MINS: u32 = 60;

/// Stable refusal codes; each is the whole `InvalidParam`/`PolicyDenied` text.
pub const QUEUE_DISABLED: &str = "queue_disabled";
/// An operator landing (#2055) names a source that `origin/rolling` already
/// contains; there is nothing to gate.
pub const QUEUE_SOURCE_ALREADY_ON_ROLLING: &str = "queue_source_already_on_rolling";
pub const QUEUE_SOURCE_INVALID: &str = "queue_source_invalid";
/// One accepted source carries multiple provisional declarations, which the
/// lander's per-source proof currently cannot audit as one unit.
pub const QUEUE_MULTIPLE_PROVISIONAL_MIGRATIONS: &str = "queue_multiple_provisional_migrations";
pub const QUEUE_FILTER_INVALID: &str = "queue_filter_invalid";
pub const QUEUE_KEY_INVALID: &str = "queue_idempotency_key_invalid";
pub const QUEUE_DUPLICATE_SOURCE: &str = "queue_duplicate_source";
/// A test filter selects no test (refused at enqueue, or reported by the gate
/// when "no tests to run" slips past the static check). Names the filter.
pub const QUEUE_FILTER_MATCHES_NO_TESTS: &str = "filter_matches_no_tests";
/// The lander's non-test `cargo check` of a changed crate (no
/// dev-dependencies, so no `test-seam`) failed (#1970). The refusal is
/// `release_build_failed:<crate>`; it is a red gate, attributable by bisecting.
pub const QUEUE_RELEASE_BUILD_FAILED: &str = "release_build_failed";
pub const QUEUE_NOT_AUTHORIZED: &str = "queue_not_authorized";
/// A manager or lead named a `source_session_id` outside its scope. Names the
/// field and the next step; the caller keeps its enqueue authority.
pub const QUEUE_SOURCE_SESSION_OUT_OF_SCOPE: &str = "queue_source_session_out_of_scope";
pub const QUEUE_REGATE_EXHAUSTED: &str = "queue_out_of_band_regate_exhausted";
/// A source's merge onto the tip (or onto its batch predecessors) conflicted.
pub const QUEUE_BATCH_MERGE_CONFLICT: &str = "queue_batch_merge_conflict";
/// #2118: a typed enqueue warning, never a refusal: the new source would
/// conflict with the entry named (queued or in flight ahead of it).
pub const QUEUE_CONFLICTS_WITH_ENTRY: &str = "queue_conflicts_with_entry";
/// #2118: a typed enqueue warning: the new source conflicts with the rolling
/// tip itself, so the lander would refuse it `queue_batch_merge_conflict`.
pub const QUEUE_CONFLICTS_WITH_ROLLING: &str = "queue_conflicts_with_rolling";
/// Most conflicting paths one enqueue warning names.
pub const QUEUE_CONFLICT_WARNING_MAX_FILES: usize = 8;
/// #2907: a typed enqueue warning: the conflict scan hit its wall budget
/// before it probed every entry ahead, so an absent warning proves nothing for
/// the entries named as skipped in `detail`.
pub const QUEUE_CONFLICT_SCAN_INCOMPLETE: &str = "queue_conflict_scan_incomplete";
/// #2993: a retryable refusal of `AgentEnqueueLandingSource {derive_filters:true}`:
/// the rolling-tip read behind the filter derivation did not finish within the
/// call's wall bound (a slow or stalled git) and was killed. No entry was
/// recorded; call again with the same `idempotency_key`. Genuine git trouble,
/// so it stays andon friction (#2992); a derivation that is merely still
/// running is not refused at all, the entry is recorded with its filters
/// pending ([`QueueFiltersState::Deriving`]).
pub const QUEUE_TIP_READ_TIMEOUT: &str = "queue_tip_read_timeout";
/// #2993: the outcome refusal of an entry recorded with its filters pending
/// whose derivation then failed (the checker did not start, timed out or
/// printed no usable receipt). The entry settles `refused` and its enqueuer is
/// woken once with this code.
pub const QUEUE_FILTER_DERIVATION_FAILED: &str = "queue_filter_derivation_failed";
/// #2912: a retryable refusal of `AgentEnqueueLandingSource`: a git read of the
/// source probe did not finish within the call's wall bound (a slow or stalled
/// git) and was killed. No entry was recorded; call again with the same
/// `idempotency_key`.
pub const QUEUE_PROBE_TIMEOUT: &str = "queue_probe_timeout";
/// The lander's policy refused the source or its batch before any gate ran.
pub const QUEUE_BATCH_POLICY_REFUSED: &str = "queue_batch_policy_refused";
/// A migration-carrying source cannot take the next migration number in order.
pub const QUEUE_MIGRATION_OUT_OF_ORDER: &str = "queue_migration_out_of_order";
/// The published tip does not contain the source the batch reported landing.
pub const QUEUE_BATCH_ANCESTRY_UNVERIFIED: &str = "queue_batch_ancestry_unverified";
/// A red-batch bisect step failed to shrink the suspect window (a guard: the
/// remaining sources settle with this code rather than looping).
pub const QUEUE_BISECT_NO_PROGRESS: &str = "queue_bisect_no_progress";
/// The batch spent its whole gate wall-time budget
/// (`rolling_queue_gate_timeout_mins`); its unsettled members are refused
/// with this code instead of holding the queue (#1208).
pub const QUEUE_GATE_TIMEOUT: &str = "queue_gate_timeout";
/// The lander reported a source already integrated but the queue could not
/// confirm it on `origin/rolling` (#1208). A confirmed one settles published.
pub const QUEUE_SOURCE_ALREADY_INTEGRATED: &str = "queue_source_already_integrated";
/// `AgentListRollingQueue` / `AgentCancelRollingQueueEntry` (#1964): the
/// request does not match the closed schema.
pub const QUEUE_INVALID_REQUEST: &str = "queue_invalid_request";
/// The named entry does not exist or is outside the caller's scope (one code
/// for both, so the verb never confirms an entry the caller cannot see).
pub const QUEUE_ENTRY_NOT_FOUND: &str = "queue_entry_not_found";
/// The entry is admitted or gating: the lander owns it until it settles.
pub const QUEUE_ENTRY_IN_FLIGHT: &str = "queue_entry_in_flight";
/// The entry already settled (published, refused, failed or superseded by the
/// lander); there is nothing left to cancel.
pub const QUEUE_ENTRY_SETTLED: &str = "queue_entry_settled";
/// The cancel `reason` is empty, over [`ROLLING_QUEUE_MAX_CANCEL_REASON_BYTES`]
/// or holds a control character.
pub const QUEUE_REASON_INVALID: &str = "queue_reason_invalid";
/// The outcome refusal a cancelled entry settles with (state `superseded`).
pub const QUEUE_ENTRY_CANCELLED: &str = "queue_entry_cancelled";
/// A replacement must descend from the superseded entry's source commit.
pub const QUEUE_SUPERSEDES_NOT_DESCENDANT: &str = "queue_supersedes_not_descendant";
/// Terminal outcome for a queued entry replaced in place, without a wake.
pub const QUEUE_ENTRY_SUPERSEDED: &str = "queue_entry_superseded";
/// #2167: a landing that claims to fix known-red tests (`fixes_tests`,
/// `fixes_issue`) must turn every named test green in its gate. A claimed test
/// that still fails on the candidate (even where the base fails it too), or
/// that the gate's selection never ran, refuses the landing with this code.
pub const QUEUE_CLAIMED_FIX_STILL_RED: &str = "claimed_fix_still_red";
/// `fixes_issue` is not a positive display number, or `fixes_tests` holds a
/// name that is not a test name.
pub const QUEUE_FIXES_INVALID: &str = "queue_fixes_invalid";
/// The claimed tests, explicit `fixes_tests` plus those a `fixes_issue` body
/// lists, number more than [`ROLLING_QUEUE_MAX_FIXES_TESTS`] (#2175). The
/// refusal names the count and the limit; a claim set is never truncated,
/// because a partial claim would let a partial fix land.
pub const QUEUE_FIXES_TOO_MANY: &str = "queue_fixes_too_many";
/// `fixes_issue` names no Issue of the source's project.
pub const QUEUE_FIXES_ISSUE_NOT_FOUND: &str = "queue_fixes_issue_not_found";
/// `fixes_issue` names an Issue whose body has no `Failing tests:` line with a
/// test name on it: pass the names as `fixes_tests` instead.
pub const QUEUE_FIXES_ISSUE_LISTS_NO_TESTS: &str = "queue_fixes_issue_lists_no_failing_tests";
/// Most explicit `fixes_tests` names one enqueue may carry, and the longest.
pub const ROLLING_QUEUE_MAX_FIXES_TESTS: usize = 64;
pub const ROLLING_QUEUE_MAX_TEST_NAME_BYTES: usize = 512;
/// The reserved item a fix claim is stored as inside the entry's
/// `test_filters_json` (#2167, no schema change). It names no workspace
/// package, so a lander that predates claims refuses it as an invalid filter:
/// a claim never degrades to an unchecked landing.
pub const FIX_CLAIM_FILTER_PREFIX: &str = "fixes_test=";
/// #2993: the reserved item an entry whose filter derivation is still running
/// is stored with inside `test_filters_json` (no schema change, like the fix
/// claims). The lander admits no entry carrying it; the derivation's end
/// replaces it with the filters, or settles the entry `refused`.
pub const FILTERS_DERIVING_MARKER: &str = "filters_state=deriving";

/// #2993: where an entry's landing filters stand. Absent on an entry whose
/// filters are final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueFiltersState {
    /// `AgentEnqueueLandingSource {derive_filters:true}` recorded the entry
    /// before the derivation finished. `test_filters` is empty until it does;
    /// the lander does not admit the entry meanwhile, and a failed derivation
    /// settles it `refused` (`queue_filter_derivation_failed`).
    Deriving,
}

/// Default and maximum page of `AgentListRollingQueue`.
pub const ROLLING_QUEUE_AGENT_LIST_DEFAULT: u32 = 50;
pub const ROLLING_QUEUE_AGENT_LIST_MAX: u32 = 100;
/// Longest cancel `reason` and longest outcome detail in a list row.
pub const ROLLING_QUEUE_MAX_CANCEL_REASON_BYTES: usize = 256;
pub const ROLLING_QUEUE_OUTCOME_DETAIL_BYTES: usize = 240;

/// Lifecycle of one queue entry. Strings match the SQLite CHECK exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingQueueEntryState {
    Queued,
    Admitted,
    Gating,
    Published,
    Refused,
    Failed,
    Superseded,
}

impl RollingQueueEntryState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Admitted => "admitted",
            Self::Gating => "gating",
            Self::Published => "published",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "admitted" => Self::Admitted,
            "gating" => Self::Gating,
            "published" => Self::Published,
            "refused" => Self::Refused,
            "failed" => Self::Failed,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Published | Self::Refused | Self::Failed | Self::Superseded
        )
    }
}

/// Whether the source is bound to a ledger Work item. Reported, never required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingQueueBinding {
    Bound,
    Unbound,
}

impl RollingQueueBinding {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Unbound => "unbound",
        }
    }
}

/// Typed per-source result delivered once to the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RollingQueueOutcome {
    pub landed_sha: Option<String>,
    /// Stable refusal code (`queue_*`, or a lander `policy_fence`).
    pub refusal: Option<String>,
    /// Failing test names the gate reported, if any.
    #[serde(default)]
    pub failing_tests: Vec<String>,
    /// Bounded tail of the lander's own message.
    pub detail: Option<String>,
}

/// One queue entry as reported to operators and enqueue receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollingQueueEntryV1 {
    pub id: Uuid,
    pub sequence: i64,
    pub source_commit: String,
    pub source_session_id: Uuid,
    pub owner_epic_id: Option<Uuid>,
    pub binding: RollingQueueBinding,
    pub work_key: Option<String>,
    pub migration_version: Option<u32>,
    pub hot_files: Vec<String>,
    pub test_filters: Vec<String>,
    /// #2167: the tests this landing claims to fix. The gate refuses it
    /// `claimed_fix_still_red` unless each one runs green on the candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes_tests: Vec<String>,
    /// #2993: `deriving` while the derived filters are still pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters_state: Option<QueueFiltersState>,
    pub state: RollingQueueEntryState,
    pub outcome: Option<RollingQueueOutcome>,
    pub enqueued_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEnqueueLandingSourceRequestV1 {
    /// Replace this same-enqueuer queued entry with a descendant source at its
    /// queue position. Admitted/gating entries must be enqueued normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Uuid>,
    pub source_commit: String,
    /// Explicit `PACKAGE=FILTER` landing filters: at most
    /// [`ROLLING_QUEUE_MAX_TEST_FILTERS`] of at most
    /// [`ROLLING_QUEUE_MAX_FILTER_BYTES`] bytes each (`queue_filter_invalid`
    /// otherwise). Prefer `derive_filters`, which has neither limit.
    #[serde(default)]
    pub test_filters: Vec<String>,
    /// #1807/#1781/#1786: let the queue derive the filters itself. The daemon
    /// runs `scripts/check-touched-shards` on the source's diff against the
    /// rolling tip, dedupes focused rsid/rsid-store filters that name the same
    /// expression across shards, and splits any filter over
    /// [`ROLLING_QUEUE_MAX_FILTER_BYTES`] into equivalent filters. A derived
    /// set over [`ROLLING_QUEUE_MAX_TEST_FILTERS`] is not refused. Mutually
    /// exclusive with `test_filters` (`queue_filter_invalid`). Never copy,
    /// prune or split filters by hand. One call is enough (#2993): a derivation
    /// still running at the call's budget does not refuse the call; the entry is
    /// recorded `queued` with `filters_state: deriving`, the lander admits it
    /// once its filters land, and a derivation that fails settles it `refused`
    /// (`queue_filter_derivation_failed`) with the enqueuer's single outcome
    /// wake. Never poll or retry for the filters.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub derive_filters: bool,
    /// #2167: the display number of an Issue of the source's project that
    /// this landing claims to fix. The queue adds the test names its body
    /// lists on `Failing tests:` lines (the qa Issue format) to `fixes_tests`;
    /// an Issue with no such name is refused
    /// `queue_fixes_issue_lists_no_failing_tests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixes_issue: Option<i64>,
    /// #2167: test names (as the gate and the QA run report them, for example
    /// `module::tests::name`) this landing claims to turn green. Its gate must
    /// run each one on the candidate and see it pass, or the landing is
    /// refused `claimed_fix_still_red`, also when the base fails it too.
    /// Combines with `derive_filters` or `test_filters`; at most
    /// [`ROLLING_QUEUE_MAX_FIXES_TESTS`], counting the names `fixes_issue`
    /// adds (`queue_fixes_too_many` otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes_tests: Vec<String>,
    pub idempotency_key: String,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// #1235: land from this in-reach session's sandbox instead of the
    /// caller's own. The caller's manager scope must reach the session; an
    /// explicit caller ID selects the caller's own sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<Uuid>,
}

impl AgentEnqueueLandingSourceRequestV1 {
    /// Validate the source OID, `PACKAGE=FILTER` pairs and replay key.
    ///
    /// # Errors
    /// Returns the stable `queue_*` code for the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !is_full_lower_hex_oid(&self.source_commit) {
            return Err(QUEUE_SOURCE_INVALID);
        }
        if self.derive_filters && !self.test_filters.is_empty() {
            return Err(QUEUE_FILTER_INVALID);
        }
        if self.test_filters.len() > ROLLING_QUEUE_MAX_TEST_FILTERS
            || self.test_filters.iter().any(|f| !valid_test_filter(f))
        {
            return Err(QUEUE_FILTER_INVALID);
        }
        if self.fixes_issue.is_some_and(|number| number < 1)
            || self.fixes_tests.iter().any(|name| !valid_test_name(name))
        {
            return Err(QUEUE_FIXES_INVALID);
        }
        if self.fixes_tests.len() > ROLLING_QUEUE_MAX_FIXES_TESTS {
            return Err(QUEUE_FIXES_TOO_MANY);
        }
        let key = &self.idempotency_key;
        if key.is_empty() || key.len() > ROLLING_QUEUE_MAX_IDEMPOTENCY_BYTES || key.contains('\0') {
            return Err(QUEUE_KEY_INVALID);
        }
        Ok(())
    }
}

impl AgentEnqueueLandingSourceRequestV1 {
    /// The refusal text for a validation `code`: the stable code, then (for an
    /// over-size filter) the field path, the offending filter and the limit.
    #[must_use]
    pub fn refusal_detail(&self, code: &str) -> String {
        if code == QUEUE_FILTER_INVALID {
            if let Some(detail) = oversize_filter_detail("test_filters", &self.test_filters) {
                return format!("{code}: {detail}");
            }
        }
        if code == QUEUE_FIXES_TOO_MANY {
            return fix_claims_over_cap_detail(self.fixes_tests.len());
        }
        code.to_string()
    }
}

/// The `queue_fixes_too_many` refusal text for `count` claimed tests: the code,
/// the count and the limit (#2175).
#[must_use]
pub fn fix_claims_over_cap_detail(count: usize) -> String {
    format!(
        "{QUEUE_FIXES_TOO_MANY}: {count} claimed tests, over the {ROLLING_QUEUE_MAX_FIXES_TESTS}-test limit; claim fewer tests (claims are never truncated)"
    )
}

/// #2175: refuse a claim set over [`ROLLING_QUEUE_MAX_FIXES_TESTS`] with
/// [`fix_claims_over_cap_detail`]. Applies to the combined explicit and
/// `fixes_issue`-listed claims; never truncates.
///
/// # Errors
/// The refusal text when `claims` holds more than the limit.
pub fn check_fix_claims_cap(claims: &[String]) -> Result<(), String> {
    if claims.len() > ROLLING_QUEUE_MAX_FIXES_TESTS {
        return Err(fix_claims_over_cap_detail(claims.len()));
    }
    Ok(())
}

/// Describe the first filter in `filters` over [`ROLLING_QUEUE_MAX_FILTER_BYTES`]
/// bytes: `FIELD[i] is N bytes, over the 256-byte limit: "<truncated>"`.
#[must_use]
pub fn oversize_filter_detail(field: &str, filters: &[String]) -> Option<String> {
    oversize_filter_detail_at(field, filters, ROLLING_QUEUE_MAX_FILTER_BYTES)
}

/// [`oversize_filter_detail`] with an explicit byte `limit`.
#[must_use]
pub fn oversize_filter_detail_at(field: &str, filters: &[String], limit: usize) -> Option<String> {
    let (index, filter) = filters.iter().enumerate().find(|(_, f)| f.len() > limit)?;
    let mut shown: String = filter.chars().take(80).collect();
    if shown.len() < filter.len() {
        shown.push('…');
    }
    Some(format!(
        "{field}[{index}] is {} bytes, over the {limit}-byte limit: \"{shown}\"",
        filter.len()
    ))
}

fn is_full_lower_hex_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// #2167: a test name as the gate and the QA run report it (`a::tests::b`):
/// ASCII letters, digits and `_:.-`, not starting with `-`, at most
/// [`ROLLING_QUEUE_MAX_TEST_NAME_BYTES`] bytes. The lander runs an isolated
/// retry only for names of this shape.
#[must_use]
pub fn valid_test_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= ROLLING_QUEUE_MAX_TEST_NAME_BYTES
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
}

/// #2172: the `Failing tests:` entries of a QA Issue, from the names a run
/// reported. A run reports a failure as `<binary> <test path>` (nextest) or as
/// a bare test path (libtest), and an older satellite also left nextest's
/// progress counter in front (`_5774/8220_ rsid::bin/rsid tests::a`, the
/// hub's display-safe form of `( 5774/8220) rsid::bin/rsid tests::a`). The
/// gate and [`failing_tests_listed`] speak test paths, so each entry is
/// reduced to its last word: one clean, deduplicated, sorted path per test.
#[must_use]
pub fn failing_test_paths(reported: &[String]) -> Vec<String> {
    let paths: std::collections::BTreeSet<&str> = reported
        .iter()
        .filter_map(|name| name.split_whitespace().next_back())
        .collect();
    paths.into_iter().map(str::to_owned).collect()
}

/// #185: the QA Issue's separate `Failing outcomes:` entries, from the names a
/// run reported: `<OUTCOME> <binary> <test path>`, one per exact binary+test
/// (`FAIL`, or `TIMEOUT` for a name the run labelled so), sorted and
/// deduplicated, with an older satellite's progress counter dropped. The
/// canonical `Failing tests:` line stays path-only ([`failing_test_paths`]);
/// this list keeps what that reduction loses, which binary and which outcome.
#[must_use]
pub fn failing_test_outcomes(reported: &[String]) -> Vec<String> {
    let outcomes: std::collections::BTreeSet<String> = reported
        .iter()
        .filter_map(|name| {
            let name = name.trim();
            let (label, rest) = match name.strip_prefix("TIMEOUT ") {
                Some(rest) => ("TIMEOUT", rest),
                None => ("FAIL", name),
            };
            let identity = rest
                .split_whitespace()
                .filter(|word| !is_hub_progress_word(word))
                .collect::<Vec<_>>()
                .join(" ");
            (!identity.is_empty()).then(|| format!("{label} {identity}"))
        })
        .collect();
    outcomes.into_iter().collect()
}

/// The hub's display-safe progress counter an older satellite left in a name:
/// `_5774/8220_`.
fn is_hub_progress_word(word: &str) -> bool {
    let Some((done, total)) = word
        .strip_prefix('_')
        .and_then(|inner| inner.strip_suffix('_'))
        .and_then(|inner| inner.split_once('/'))
    else {
        return false;
    };
    let is_count = |digits: &str| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    is_count(done) && is_count(total)
}

/// #2167: the entry's stored filter list: its filters, then one reserved
/// [`FIX_CLAIM_FILTER_PREFIX`] item per claimed test. The claims live in the
/// entry's existing `test_filters_json`, so no schema change is needed.
#[must_use]
pub fn encode_fix_claims(filters: &[String], fixes: &[String]) -> Vec<String> {
    filters
        .iter()
        .cloned()
        .chain(
            fixes
                .iter()
                .map(|name| format!("{FIX_CLAIM_FILTER_PREFIX}{name}")),
        )
        .collect()
}

/// #2993: whether the stored list carries [`FILTERS_DERIVING_MARKER`], and the
/// list without it.
#[must_use]
pub fn split_filters_state(stored: Vec<String>) -> (Vec<String>, Option<QueueFiltersState>) {
    let deriving = stored.iter().any(|item| item == FILTERS_DERIVING_MARKER);
    let rest = stored
        .into_iter()
        .filter(|item| item != FILTERS_DERIVING_MARKER)
        .collect();
    (rest, deriving.then_some(QueueFiltersState::Deriving))
}

/// The inverse of [`encode_fix_claims`]: `(filters, claimed tests)`.
#[must_use]
pub fn split_fix_claims(stored: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut filters = Vec::new();
    let mut fixes = Vec::new();
    for item in stored {
        match item.strip_prefix(FIX_CLAIM_FILTER_PREFIX) {
            Some(name) => fixes.push(name.to_owned()),
            None => filters.push(item),
        }
    }
    (filters, fixes)
}

/// #2167: the test names an Issue body lists on its `Failing tests:` lines
/// (the qa Issue format: `Failing tests: a, b (and N more)`) or on the base-red
/// filer's ``Test: `<id>` `` line (#2787), first occurrence first. Anything that
/// is not a test name is dropped.
#[must_use]
pub fn failing_tests_listed(body: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in body.lines() {
        let line = line.trim().trim_start_matches(['-', '*']).trim_start();
        let Some(list) = line.strip_prefix("Failing tests:") else {
            // #2787: the gate base-red filer's Issue names its one test as
            // ``Test: `<id>` `` (backticks required, so a prose `Test: ...`
            // line is not read as a test name).
            if let Some(name) = line
                .strip_prefix("Test:")
                .map(str::trim)
                .and_then(|value| value.strip_prefix('`')?.strip_suffix('`'))
                .filter(|name| valid_test_name(name) && !names.iter().any(|seen| seen == name))
            {
                names.push(name.to_owned());
            }
            continue;
        };
        for item in list.split(',') {
            let name = item.split(" (").next().unwrap_or(item).trim();
            if valid_test_name(name) && !names.iter().any(|seen| seen == name) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// #2167: `explicit` then each name of `listed` not already in it.
#[must_use]
pub fn union_fix_claims(explicit: &[String], listed: &[String]) -> Vec<String> {
    let mut claims = explicit.to_vec();
    for name in listed {
        if !claims.contains(name) {
            claims.push(name.clone());
        }
    }
    claims
}

/// `PACKAGE=FILTER`, both non-empty, neither starting with `-` (the lander
/// refuses the same shapes; refusing here gives the owner an immediate answer).
#[must_use]
pub fn valid_test_filter(value: &str) -> bool {
    let Some((package, filter)) = value.split_once('=') else {
        return false;
    };
    value.len() <= ROLLING_QUEUE_MAX_FILTER_BYTES
        && !package.is_empty()
        && !filter.is_empty()
        && !package.starts_with('-')
        && !filter.starts_with('-')
        && !value.chars().any(char::is_control)
}

/// Make a derived filter set queue-valid: drop duplicates, drop focused
/// `rsid`/`rsid-store` filters that name an expression an earlier one already
/// names (a focused filter runs on one unsharded lib build of both packages,
/// so the shard and the package do not change what it selects), and split each
/// filter over [`ROLLING_QUEUE_MAX_FILTER_BYTES`] into equivalent filters.
/// The count is never capped; the first occurrence keeps its position.
#[must_use]
pub fn normalize_test_filters(filters: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for filter in filters {
        let key = focused_filter_key(&filter).unwrap_or_else(|| filter.clone());
        if !seen.insert(key) {
            continue;
        }
        for part in split_test_filter(&filter) {
            if !out.contains(&part) {
                out.push(part);
            }
        }
    }
    out
}

/// The expression a focused `rsid`/`rsid-store` filter selects, shard and
/// package stripped; `None` for a whole-shard filter or another package.
fn focused_filter_key(filter: &str) -> Option<String> {
    let (package, value) = filter.split_once('=')?;
    if !matches!(package, "rsid" | "rsid-store") {
        return None;
    }
    // Only a focused lib filter (`shard:S:EXPR` or a plain name) merges across
    // the two packages; target selectors such as `test:NAME` and `bin:NAME`
    // are per package and never merge.
    let expr = match value.strip_prefix("shard:") {
        Some(rest) => rest.split_once(':')?.1,
        None if value.contains(['(', ':']) => return None,
        None => value,
    };
    if expr.is_empty() {
        return None;
    }
    // A plain name is the substring filter `test(name)`.
    Some(if expr.contains(['(', ':']) {
        format!("focused:{expr}")
    } else {
        format!("focused:test({expr})")
    })
}

/// Split `filter` into equivalent filters of at most
/// [`ROLLING_QUEUE_MAX_FILTER_BYTES`] bytes. A filter that fits, or whose
/// shape cannot be split, is returned whole. Splits the alternatives of a
/// named-set regex `test(/^prefix(?:a|b|c)$/)` and top-level `|` unions of
/// `test(..)` atoms; the parts select exactly the union the whole selects.
#[must_use]
pub fn split_test_filter(filter: &str) -> Vec<String> {
    let whole = || vec![filter.to_owned()];
    if filter.len() <= ROLLING_QUEUE_MAX_FILTER_BYTES {
        return whole();
    }
    let Some((package, value)) = filter.split_once('=') else {
        return whole();
    };
    let (head, expr) = match value.strip_prefix("shard:").and_then(|r| r.split_once(':')) {
        Some((shard, expr)) => (format!("{package}=shard:{shard}:"), expr),
        None => (format!("{package}="), value),
    };
    let Some(shape) = SplitShape::parse(expr) else {
        return whole();
    };
    let pack = |parts: &[String]| {
        format!(
            "{head}{}{}{}",
            shape.open,
            parts.join(shape.sep),
            shape.close
        )
    };
    let mut out = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for part in &shape.parts {
        let mut trial = current.clone();
        trial.push(part.clone());
        if !current.is_empty() && pack(&trial).len() > ROLLING_QUEUE_MAX_FILTER_BYTES {
            out.push(pack(&current));
            current = vec![part.clone()];
        } else {
            current = trial;
        }
    }
    if !current.is_empty() {
        out.push(pack(&current));
    }
    if out.len() < 2 {
        return whole();
    }
    out
}

/// A filter expression as `open + parts.join(sep) + close`.
struct SplitShape {
    open: String,
    sep: &'static str,
    close: &'static str,
    parts: Vec<String>,
}

impl SplitShape {
    fn parse(expr: &str) -> Option<Self> {
        let union = split_top_level(expr, " | ");
        if union.len() > 1 && union.iter().all(|p| p.starts_with("test(")) {
            return Some(Self {
                open: String::new(),
                sep: " | ",
                close: "",
                parts: union,
            });
        }
        let body = expr
            .strip_prefix("test(/")?
            .strip_suffix("$/)")?
            .strip_suffix(')')?;
        let open = group_start(body)?;
        let parts = split_top_level(&body[open + 3..], "|");
        (parts.len() > 1).then(|| Self {
            open: format!("test(/{}(?:", &body[..open]),
            sep: "|",
            close: ")$/)",
            parts,
        })
    }
}

/// Byte index of the `(?:` that opens the group closed by the final `)` of
/// `body_without_close` (the text before the closing paren).
fn group_start(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut depth = 0usize;
    let mut stack = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'(' => {
                stack.push(i);
                depth += 1;
            }
            b')' => {
                stack.pop()?;
                depth = depth.checked_sub(1)?;
            }
            _ => {}
        }
        i += 1;
    }
    let open = stack.pop()?;
    (stack.is_empty() && depth == 1 && body[open..].starts_with("(?:")).then_some(open)
}

/// Split `text` at `sep` occurrences outside any `(...)` and not escaped.
fn split_top_level(text: &str, sep: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let (mut depth, mut start, mut i) = (0i32, 0usize, 0usize);
    let mut parts = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && text[i..].starts_with(sep) => {
                parts.push(text[start..i].to_owned());
                start = i + sep.len();
                i = start;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(text[start..].to_owned());
    parts
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEnqueueLandingSourceReceiptV1 {
    pub entry: RollingQueueEntryV1,
    /// True when an identical earlier request was replayed.
    pub replayed: bool,
    /// #2118: merges the queue would refuse `queue_batch_merge_conflict`,
    /// found by an in-memory `git merge-tree` of the source against the
    /// rolling tip and each live entry ahead of it. The entry is queued
    /// regardless (an entry ahead may still be withdrawn or refused); the
    /// enqueuer can have the worker merge first, or withdraw the entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_warnings: Vec<QueueConflictWarningV1>,
}

/// One predicted integration conflict of a newly enqueued source (#2118).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueConflictWarningV1 {
    /// [`QUEUE_CONFLICTS_WITH_ENTRY`] or [`QUEUE_CONFLICTS_WITH_ROLLING`].
    pub code: String,
    /// The entry ahead whose source the new one conflicts with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_sequence: Option<i64>,
    /// The conflicting paths, at most [`QUEUE_CONFLICT_WARNING_MAX_FILES`].
    pub files: Vec<String>,
    pub detail: String,
}

/// Bounded outcome of a settled entry as `AgentListRollingQueue` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollingQueueOutcomeSummaryV1 {
    pub landed_sha: Option<String>,
    /// Stable refusal code (`queue_*`, or a lander `policy_fence`).
    pub refusal: Option<String>,
    pub failing_test_count: u32,
    /// The lander's message tail, cut to [`ROLLING_QUEUE_OUTCOME_DETAIL_BYTES`].
    pub detail: Option<String>,
}

impl RollingQueueOutcomeSummaryV1 {
    #[must_use]
    pub fn of(outcome: &RollingQueueOutcome) -> Self {
        Self {
            landed_sha: outcome.landed_sha.clone(),
            refusal: outcome.refusal.clone(),
            failing_test_count: u32::try_from(outcome.failing_tests.len()).unwrap_or(u32::MAX),
            detail: outcome
                .detail
                .as_deref()
                .map(|detail| truncate_utf8(detail, ROLLING_QUEUE_OUTCOME_DETAIL_BYTES)),
        }
    }
}

fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// One queue entry as `AgentListRollingQueue` and `AgentCancelRollingQueueEntry`
/// report it: bounded (no hot-file or filter lists), with the project, batch
/// and delivery identity a manager needs to track an entry across a seat
/// rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRollingQueueEntryV1 {
    pub id: Uuid,
    pub sequence: i64,
    pub state: RollingQueueEntryState,
    pub source_commit: String,
    pub project_id: Option<Uuid>,
    /// The session that enqueued it (immutable identity).
    pub enqueuer_session_id: Uuid,
    /// Where the outcome wake goes when it is not the enqueuer: the owning
    /// manager of a topology landing, or the successor after a seat rotation.
    pub wake_session_id: Option<Uuid>,
    pub owner_epic_id: Option<Uuid>,
    /// The batch that claimed it; `None` while still queued.
    pub batch_id: Option<Uuid>,
    pub migration_version: Option<u32>,
    pub test_filter_count: u32,
    pub outcome: Option<RollingQueueOutcomeSummaryV1>,
    pub enqueued_at: String,
    pub finished_at: Option<String>,
}

/// `AgentListRollingQueue` (#1964): the caller's own scope of the queue,
/// newest first. Authority is derived from the transport-bound caller, never
/// from this request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListRollingQueueRequestV1 {
    /// Only entries in this state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<RollingQueueEntryState>,
    /// Narrow to one project inside your scope (a portfolio seat: one covered
    /// project). Omitted means every entry in your scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// Page size, [`ROLLING_QUEUE_AGENT_LIST_MAX`] at most (default
    /// [`ROLLING_QUEUE_AGENT_LIST_DEFAULT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Page cursor: only entries with a smaller sequence (the previous
    /// page's `next_before_sequence`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sequence: Option<i64>,
}

impl AgentListRollingQueueRequestV1 {
    /// # Errors
    /// `queue_invalid_request` for an out-of-range limit or cursor.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .limit
            .is_some_and(|limit| limit == 0 || limit > ROLLING_QUEUE_AGENT_LIST_MAX)
            || self.before_sequence.is_some_and(|cursor| cursor < 1)
            || self.project_id.is_some_and(|project| project.is_nil())
        {
            return Err(QUEUE_INVALID_REQUEST);
        }
        Ok(())
    }

    /// The page size this request asks for.
    #[must_use]
    pub fn page_size(&self) -> usize {
        self.limit.unwrap_or(ROLLING_QUEUE_AGENT_LIST_DEFAULT) as usize
    }
}

/// The newest `rolling` commit a full-library QA run proved green with every
/// landing it owed covered (#2646). Read from the queue's own `bounded_qa_finished`
/// receipts: there is no pointer file to land, so it cannot go stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollingQaGreenV1 {
    /// The full commit the run tested (the pinned rolling snapshot).
    pub sha: String,
    /// When the passing receipt was recorded (RFC3339).
    pub passed_at: String,
    /// The QA job that ran it (the hub's, or the satellite's run id binding).
    pub job_id: Option<Uuid>,
}

/// Longest `open_qa_issues` list one health snapshot carries.
pub const ROLLING_HEALTH_MAX_QA_ISSUES: usize = 8;
/// Most failing test names a QA notice carries.
pub const QA_NOTICE_MAX_FAILING_TESTS: usize = 8;
/// The receipt warning code of a deploy whose SHA no full-library QA run
/// proved green (#2743).
pub const DEPLOY_WARNING_NOT_QA_GREEN: &str = "not_qa_green";

/// One open `qa` Issue: a full-library QA red, a red a gate saw on the base,
/// or incomplete QA coverage (#2743).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollingQaIssueV1 {
    pub number: i64,
    pub title: String,
    /// `Open` or `InProgress`.
    pub status: String,
    pub priority: Option<u8>,
    /// RFC3339.
    pub created_at: String,
}

/// Whether `rolling` is known good (#2743): the open `qa` Issues of a project
/// and the newest full-library-green SHA with its age. A manager reads it
/// before it chooses work or a deploy SHA; a red `rolling` is the first thing
/// the seat must act on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollingHealthV1 {
    /// The newest SHA a full-library QA run proved green; `None` before one.
    #[serde(default)]
    pub qa_green: Option<RollingQaGreenV1>,
    /// Whole seconds since `qa_green.passed_at`, read when this snapshot was
    /// taken; `None` without a green SHA.
    #[serde(default)]
    pub qa_green_age_secs: Option<i64>,
    /// Open `qa` Issues, lowest priority number first, then oldest.
    #[serde(default)]
    pub open_qa_issues: Vec<RollingQaIssueV1>,
    /// How many open `qa` Issues exist (`open_qa_issues` is capped at
    /// `ROLLING_HEALTH_MAX_QA_ISSUES`).
    #[serde(default)]
    pub open_qa_issues_total: u32,
}

impl RollingHealthV1 {
    /// A project with no open `qa` Issue and a green SHA is not red; anything
    /// else deserves the seat's attention.
    #[must_use]
    pub fn is_red(&self) -> bool {
        self.open_qa_issues_total > 0
    }

    /// The plain-text block a successor's launch prompt carries.
    #[must_use]
    pub fn render_block(&self) -> String {
        let mut text = String::from("## Rolling health\n\n");
        match (&self.qa_green, self.qa_green_age_secs) {
            (Some(green), Some(age)) => text.push_str(&format!(
                "qa_green: {} (passed {}, {} ago)\n",
                green.sha,
                green.passed_at,
                human_age(age)
            )),
            (Some(green), None) => {
                text.push_str(&format!(
                    "qa_green: {} (passed {})\n",
                    green.sha, green.passed_at
                ));
            }
            _ => text.push_str("qa_green: none recorded\n"),
        }
        if self.open_qa_issues_total == 0 {
            text.push_str("open qa Issues: none\n");
        } else {
            text.push_str(&format!(
                "open qa Issues: {} (rolling is red or QA coverage is incomplete; act on these before choosing other work or a deploy SHA)\n",
                self.open_qa_issues_total
            ));
            for issue in &self.open_qa_issues {
                text.push_str(&format!(
                    "- #{} [{}] {}\n",
                    issue.number, issue.status, issue.title
                ));
            }
            let hidden = usize::try_from(self.open_qa_issues_total)
                .unwrap_or(usize::MAX)
                .saturating_sub(self.open_qa_issues.len());
            if hidden > 0 {
                text.push_str(&format!("- ... and {hidden} more\n"));
            }
        }
        text
    }
}

fn human_age(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}m", secs / 60)
    }
}

/// A receipt warning on `AgentRequestDeploy` (#2743): the SHA is not covered
/// by the latest full-library-green SHA. The deploy still proceeds; the
/// caller sees why it may be shipping an unproven build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployQaWarningV1 {
    /// `not_qa_green`.
    pub code: String,
    /// The SHA being deployed.
    pub sha: String,
    /// The newest green SHA, `None` when no QA run ever passed.
    pub qa_green: Option<RollingQaGreenV1>,
    pub qa_green_age_secs: Option<i64>,
    /// The project's open `qa` Issues (red or incomplete coverage).
    pub open_qa_issues: Vec<RollingQaIssueV1>,
    pub open_qa_issues_total: u32,
    /// Plain-language explanation.
    pub detail: String,
}

impl DeployQaWarningV1 {
    /// The warning for deploying `sha`, or `None` when the latest
    /// full-library-green SHA covers it: `sha` is that SHA or one of its
    /// ancestors (`is_ancestor_of_green` is asked only for another SHA).
    #[must_use]
    pub fn for_deploy(
        sha: &str,
        health: &RollingHealthV1,
        is_ancestor_of_green: impl FnOnce(&str) -> bool,
    ) -> Option<Self> {
        if let Some(green) = &health.qa_green
            && (green.sha == sha || is_ancestor_of_green(&green.sha))
        {
            return None;
        }
        let detail = match (&health.qa_green, health.open_qa_issues_total) {
            (None, 0) => "no full-library QA run has passed, so no SHA is proven green".to_string(),
            (None, open) => format!(
                "no full-library QA run has passed and {open} qa Issue(s) are open: rolling is red or unproven"
            ),
            (Some(green), 0) => format!(
                "{sha} is not covered by qa_green {} (full-library QA has not yet proven it)",
                green.sha
            ),
            (Some(green), open) => format!(
                "{sha} is not covered by qa_green {} and {open} qa Issue(s) are open: rolling is red or unproven",
                green.sha
            ),
        };
        Some(Self {
            code: DEPLOY_WARNING_NOT_QA_GREEN.into(),
            sha: sha.into(),
            qa_green: health.qa_green.clone(),
            qa_green_age_secs: health.qa_green_age_secs,
            open_qa_issues: health.open_qa_issues.clone(),
            open_qa_issues_total: health.open_qa_issues_total,
            detail,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentListRollingQueueReceiptV1 {
    /// Newest first (descending sequence).
    pub entries: Vec<AgentRollingQueueEntryV1>,
    /// Set when older entries exist: pass it as `before_sequence` for the
    /// next page.
    pub next_before_sequence: Option<i64>,
    /// The latest full-library-green `rolling` SHA among the entries in your
    /// scope (and `project_id`, when given); `None` before any passing run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa_green: Option<RollingQaGreenV1>,
}

/// `AgentCancelRollingQueueEntry` (#1964): withdraw one still-`queued` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCancelRollingQueueEntryRequestV1 {
    pub entry_id: Uuid,
    /// Why, recorded in the entry's outcome detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AgentCancelRollingQueueEntryRequestV1 {
    /// # Errors
    /// `queue_invalid_request` for a nil id, `queue_reason_invalid` for an
    /// empty, oversized or control-character reason.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.entry_id.is_nil() {
            return Err(QUEUE_INVALID_REQUEST);
        }
        if let Some(reason) = &self.reason
            && (reason.trim().is_empty()
                || reason.len() > ROLLING_QUEUE_MAX_CANCEL_REASON_BYTES
                || reason.chars().any(char::is_control))
        {
            return Err(QUEUE_REASON_INVALID);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCancelRollingQueueEntryReceiptV1 {
    /// The entry after the cancel: `superseded`, outcome refusal
    /// `queue_entry_cancelled`.
    pub entry: AgentRollingQueueEntryV1,
    /// True when an earlier cancel of this entry was replayed.
    pub replayed: bool,
}

/// Operator-only read of the queue and its live settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetRollingQueueParams {
    #[serde(default)]
    pub state: Option<RollingQueueEntryState>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// #2068: read exactly these entries by id (at most
    /// [`ROLLING_QUEUE_WATCH_IDS_MAX`]) instead of the newest page, so a
    /// watched landing stays visible however many records follow it. When
    /// set, `state` and `limit` are ignored.
    #[serde(default)]
    pub entry_ids: Vec<Uuid>,
}

/// The most entry ids one [`GetRollingQueueParams::entry_ids`] read names.
pub const ROLLING_QUEUE_WATCH_IDS_MAX: usize = 64;

/// Operator-only `EnqueueRollingQueueSource` (#2055): land a session's branch
/// through the merge queue instead of a manual `git merge` and push. Exactly
/// one of `session_id` and `branch` names the source (a branch resolves to the
/// session that owns it); `commit` defaults to the tip of that sandbox. The
/// daemon runs the same probe, filter derivation and insert as
/// `AgentEnqueueLandingSource`. `dry_run` resolves and reports the source
/// without enqueueing, so a client can confirm with the operator first.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnqueueRollingQueueSourceParams {
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Full lowercase OID; defaults to the sandbox HEAD.
    #[serde(default)]
    pub commit: Option<String>,
    /// Default true. Mutually exclusive with `test_filters`.
    #[serde(default = "default_true")]
    pub derive_filters: bool,
    #[serde(default)]
    pub test_filters: Vec<String>,
    /// Required unless `dry_run`; the replay key of the entry.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

const fn default_true() -> bool {
    true
}

impl EnqueueRollingQueueSourceParams {
    /// # Errors
    /// The stable `queue_*` code for the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.session_id.is_some() == self.branch.is_some() {
            return Err(QUEUE_SOURCE_INVALID);
        }
        if self.branch.as_deref().is_some_and(|branch| {
            branch.is_empty() || branch.starts_with('-') || branch.chars().any(char::is_control)
        }) {
            return Err(QUEUE_SOURCE_INVALID);
        }
        if self
            .commit
            .as_deref()
            .is_some_and(|commit| !is_full_lower_hex_oid(commit))
        {
            return Err(QUEUE_SOURCE_INVALID);
        }
        if self.derive_filters && !self.test_filters.is_empty() {
            return Err(QUEUE_FILTER_INVALID);
        }
        if self.test_filters.len() > ROLLING_QUEUE_MAX_TEST_FILTERS
            || self.test_filters.iter().any(|f| !valid_test_filter(f))
        {
            return Err(QUEUE_FILTER_INVALID);
        }
        match self.idempotency_key.as_deref() {
            None if self.dry_run => {}
            Some(key)
                if !key.is_empty()
                    && key.len() <= ROLLING_QUEUE_MAX_IDEMPOTENCY_BYTES
                    && !key.contains('\0') => {}
            _ => return Err(QUEUE_KEY_INVALID),
        }
        Ok(())
    }
}

/// What the operator sees about a source before and after it is queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandSourceStatusV1 {
    pub session_id: Uuid,
    pub branch: Option<String>,
    pub commit: String,
    /// Commits the source has that the local `origin/rolling` does not.
    pub ahead: Option<u32>,
    /// The source is already an ancestor of `origin/rolling`.
    pub on_rolling: bool,
    /// The sandbox has uncommitted changes that the queued commit omits.
    pub dirty: bool,
    pub title: Option<String>,
}

impl LandSourceStatusV1 {
    /// One line for a confirmation: what would be queued and where it stands.
    #[must_use]
    pub fn summary(&self) -> String {
        let name = self
            .branch
            .clone()
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| self.session_id.to_string());
        let short = &self.commit[..self.commit.len().min(9)];
        let position = if self.on_rolling {
            "already on rolling".to_owned()
        } else {
            match self.ahead {
                Some(1) => "unmerged, 1 commit ahead of rolling".to_owned(),
                Some(count) => format!("unmerged, {count} commits ahead of rolling"),
                None => "unmerged".to_owned(),
            }
        };
        let dirty = if self.dirty {
            ", sandbox has uncommitted changes the landing omits"
        } else {
            ""
        };
        format!("{name} @ {short}: {position}{dirty}")
    }

    /// Why this source cannot be queued, if it cannot.
    #[must_use]
    pub const fn blocker(&self) -> Option<&'static str> {
        if self.on_rolling {
            Some("already on rolling; nothing to land")
        } else {
            None
        }
    }
}

impl RollingQueueEntryV1 {
    /// One line for the entry's state and outcome.
    #[must_use]
    pub fn status_line(&self) -> String {
        let short = &self.source_commit[..self.source_commit.len().min(9)];
        let detail = self.outcome.as_ref().and_then(|outcome| {
            outcome
                .landed_sha
                .as_deref()
                .map(|sha| format!("landed {}", &sha[..sha.len().min(9)]))
                .or_else(|| outcome.refusal.clone())
                .or_else(|| outcome.detail.clone())
        });
        match detail {
            Some(detail) => format!(
                "queue #{} {short}: {} ({detail})",
                self.sequence,
                self.state.as_str()
            ),
            None => format!("queue #{} {short}: {}", self.sequence, self.state.as_str()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueRollingQueueSourceResponse {
    pub source: LandSourceStatusV1,
    /// `None` for a `dry_run`.
    pub entry: Option<RollingQueueEntryV1>,
    #[serde(default)]
    pub replayed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollingQueueResponse {
    pub enabled: bool,
    pub batch_size: u32,
    /// The batch gate wall-time budget in minutes (#1208).
    #[serde(default)]
    pub gate_timeout_mins: u32,
    pub entries: Vec<RollingQueueEntryV1>,
}

#[cfg(test)]
mod tests {
    fn health(green: Option<&str>, open: &[i64]) -> RollingHealthV1 {
        RollingHealthV1 {
            qa_green: green.map(|sha| RollingQaGreenV1 {
                sha: sha.into(),
                passed_at: "2026-10-14T18:33:00Z".into(),
                job_id: None,
            }),
            qa_green_age_secs: green.map(|_| 108_000),
            open_qa_issues: open
                .iter()
                .map(|number| RollingQaIssueV1 {
                    number: *number,
                    title: "Full library QA test red".into(),
                    status: "Open".into(),
                    priority: Some(1),
                    created_at: "2026-10-15T22:33:00Z".into(),
                })
                .collect(),
            open_qa_issues_total: u32::try_from(open.len()).unwrap(),
        }
    }

    /// #2743: a deploy of a SHA the green SHA covers carries no warning; any
    /// other SHA carries `not_qa_green` with the open qa Issues and the green
    /// SHA's age, and the ancestry probe runs only for a different SHA.
    #[test]
    fn a_deploy_not_covered_by_qa_green_warns_with_the_open_qa_issues() {
        let green = "d".repeat(40);
        let sha = "5".repeat(40);
        let covered = health(Some(&green), &[]);
        assert_eq!(
            DeployQaWarningV1::for_deploy(&green, &covered, |_| panic!("same sha needs no probe")),
            None
        );
        assert_eq!(
            DeployQaWarningV1::for_deploy(&sha, &covered, |g| g == green),
            None,
            "an ancestor of the green SHA is covered"
        );

        let red = health(Some(&green), &[2736]);
        let warning = DeployQaWarningV1::for_deploy(&sha, &red, |_| false).unwrap();
        assert_eq!(warning.code, DEPLOY_WARNING_NOT_QA_GREEN);
        assert_eq!(warning.sha, sha);
        assert_eq!(warning.qa_green.as_ref().unwrap().sha, green);
        assert_eq!(warning.qa_green_age_secs, Some(108_000));
        assert_eq!(warning.open_qa_issues[0].number, 2736);
        assert!(warning.detail.contains("1 qa Issue"), "{}", warning.detail);

        let never_green = DeployQaWarningV1::for_deploy(&sha, &health(None, &[]), |_| {
            panic!("no green SHA, nothing to probe")
        })
        .unwrap();
        assert!(never_green.qa_green.is_none());
        assert!(never_green.detail.contains("no full-library QA run"));
    }

    /// #2743: the successor's prompt block names the open qa Issues and the
    /// green SHA with its age.
    #[test]
    fn the_health_block_names_open_qa_issues_and_the_green_age() {
        let block = health(Some(&"d".repeat(40)), &[2736]).render_block();
        assert!(block.contains("#2736"), "{block}");
        assert!(block.contains(&"d".repeat(40)), "{block}");
        assert!(block.contains("30h00m"), "{block}");
        assert!(health(None, &[]).render_block().contains("none recorded"));
        assert!(!health(None, &[]).is_red());
    }

    use super::*;

    fn request() -> AgentEnqueueLandingSourceRequestV1 {
        AgentEnqueueLandingSourceRequestV1 {
            supersedes: None,
            project_id: None,
            source_session_id: None,
            source_commit: "a".repeat(40),
            test_filters: vec!["rsid=rolling_queue".into()],
            derive_filters: false,
            fixes_issue: None,
            fixes_tests: Vec::new(),
            idempotency_key: "k1".into(),
        }
    }

    #[test]
    fn oversize_filter_detail_names_field_filter_and_limit() {
        let long = format!("rsid={}", "x".repeat(300));
        let detail = oversize_filter_detail("test_filters", &["rsid=ok".into(), long.clone()])
            .expect("detail");
        assert!(
            detail.starts_with("test_filters[1] is 305 bytes"),
            "{detail}"
        );
        assert!(detail.contains("256-byte limit"), "{detail}");
        assert!(detail.contains("rsid=xxx"), "{detail}");
        assert!(detail.len() < 200, "{detail}");
        let request: AgentEnqueueLandingSourceRequestV1 =
            serde_json::from_value(serde_json::json!({
                "source_commit": "a".repeat(40), "idempotency_key": "k", "test_filters": [long]
            }))
            .unwrap();
        let code = request.validate().unwrap_err();
        let text = request.refusal_detail(code);
        assert!(
            text.starts_with("queue_filter_invalid: test_filters[0] is 305"),
            "{text}"
        );
        assert!(text.contains("256-byte limit"), "{text}");
    }

    #[test]
    fn valid_request_passes_and_unknown_fields_are_refused() {
        assert_eq!(request().validate(), Ok(()));
        let forged = serde_json::json!({
            "source_commit": "a".repeat(40), "idempotency_key": "k", "owner": "x"
        });
        assert!(serde_json::from_value::<AgentEnqueueLandingSourceRequestV1>(forged).is_err());
    }

    #[test]
    fn source_filter_and_key_are_validated() {
        let mut r = request();
        r.source_commit = "A".repeat(40);
        assert_eq!(r.validate(), Err(QUEUE_SOURCE_INVALID));
        r.source_commit = "abc".into();
        assert_eq!(r.validate(), Err(QUEUE_SOURCE_INVALID));
        for bad in ["nofilter", "=x", "p=", "-p=x", "p=-x"] {
            let mut r = request();
            r.test_filters = vec![bad.into()];
            assert_eq!(r.validate(), Err(QUEUE_FILTER_INVALID), "{bad}");
        }
        let mut r = request();
        r.idempotency_key = String::new();
        assert_eq!(r.validate(), Err(QUEUE_KEY_INVALID));
    }

    /// #2167: a fix claim names positive Issue numbers and test names only.
    #[test]
    fn fix_claims_are_validated() {
        let mut r = request();
        r.fixes_issue = Some(2143);
        r.fixes_tests = vec!["tests::scans_cli_spawn".into(), "a::b-c.d".into()];
        assert_eq!(r.validate(), Ok(()));
        r.derive_filters = true;
        r.test_filters.clear();
        assert_eq!(r.validate(), Ok(()), "claims combine with derive_filters");
        for bad in ["", "-x", "a b", "a;b", "a=b"] {
            let mut r = request();
            r.fixes_tests = vec![bad.into()];
            assert_eq!(r.validate(), Err(QUEUE_FIXES_INVALID), "{bad:?}");
        }
        let mut r = request();
        r.fixes_issue = Some(0);
        assert_eq!(r.validate(), Err(QUEUE_FIXES_INVALID));
        let mut r = request();
        r.fixes_tests = (0..ROLLING_QUEUE_MAX_FIXES_TESTS)
            .map(|i| format!("t{i}"))
            .collect();
        assert_eq!(r.validate(), Ok(()), "exactly the cap is accepted");
        r.fixes_tests.push("t_over".into());
        assert_eq!(r.validate(), Err(QUEUE_FIXES_TOO_MANY));
        let detail = r.refusal_detail(QUEUE_FIXES_TOO_MANY);
        assert!(detail.starts_with(QUEUE_FIXES_TOO_MANY), "{detail}");
        assert!(detail.contains("65 claimed tests"), "{detail}");
        assert!(detail.contains("64-test limit"), "{detail}");
    }

    /// #2175: a body listing more than the cap parses to more than the cap
    /// (the parser stays total) and the combined-claims check refuses it.
    #[test]
    fn combined_fix_claims_over_the_cap_are_refused_not_truncated() {
        let names: Vec<String> = (0..=ROLLING_QUEUE_MAX_FIXES_TESTS)
            .map(|i| format!("m::t{i}"))
            .collect();
        let body = format!("Failing tests: {}", names.join(", "));
        let listed = failing_tests_listed(&body);
        assert_eq!(listed.len(), ROLLING_QUEUE_MAX_FIXES_TESTS + 1);
        assert!(check_fix_claims_cap(&listed[..ROLLING_QUEUE_MAX_FIXES_TESTS]).is_ok());
        let err = check_fix_claims_cap(&union_fix_claims(&[], &listed)).unwrap_err();
        assert!(err.starts_with(QUEUE_FIXES_TOO_MANY), "{err}");
        assert!(err.contains("65 claimed tests"), "{err}");
        assert!(err.contains("64-test limit"), "{err}");
    }

    /// #2993: the pending-derivation marker rides in the stored filter list
    /// beside fix claims and comes back out as `filters_state`, leaving filters
    /// and claims exactly as they were.
    #[test]
    fn the_deriving_marker_round_trips_through_the_stored_filter_list() {
        let mut stored = encode_fix_claims(&[], &["tests::red".to_string()]);
        assert_eq!(split_filters_state(stored.clone()), (stored.clone(), None));
        stored.push(FILTERS_DERIVING_MARKER.to_owned());
        let (rest, state) = split_filters_state(stored);
        assert_eq!(state, Some(QueueFiltersState::Deriving));
        assert_eq!(
            split_fix_claims(rest),
            (Vec::new(), vec!["tests::red".to_string()])
        );
        assert_eq!(
            serde_json::to_string(&QueueFiltersState::Deriving).unwrap(),
            "\"deriving\""
        );
    }

    /// #2167: claims ride in the stored filter list and come back out of it
    /// unchanged, leaving the filters exactly as they were.
    #[test]
    fn fix_claims_round_trip_through_the_stored_filter_list() {
        let filters = vec!["rsid=rolling_queue".to_string()];
        let fixes = vec!["tests::red".to_string(), "x::y".to_string()];
        let stored = encode_fix_claims(&filters, &fixes);
        assert_eq!(
            stored,
            vec![
                "rsid=rolling_queue".to_string(),
                "fixes_test=tests::red".into(),
                "fixes_test=x::y".into()
            ]
        );
        assert_eq!(split_fix_claims(stored), (filters.clone(), fixes));
        assert_eq!(
            split_fix_claims(encode_fix_claims(&filters, &[])),
            (filters, Vec::new())
        );
    }

    /// #2172: the exact names the satellite run of #2172 reported (progress
    /// counter and binary id included, each test twice) reduce to one valid
    /// test path per test, and `failing_tests_listed` reads them back.
    #[test]
    fn failing_test_paths_are_one_clean_path_per_test() {
        let reported: Vec<String> = [
            "_5774/8220_ rsid::bin/rsid tests::startup_real_daemon_accepts_health",
            "_5938/8220_ rsid-store model_control::call_control::tests::unwritable_abandonment",
            "model_control::call_control::tests::unwritable_abandonment",
            "rsid migrations v9::replays",
            "tests::startup_real_daemon_accepts_health",
            "",
        ]
        .map(String::from)
        .into();
        let paths = failing_test_paths(&reported);
        assert_eq!(
            paths,
            vec![
                "model_control::call_control::tests::unwritable_abandonment".to_string(),
                "tests::startup_real_daemon_accepts_health".into(),
                "v9::replays".into(),
            ]
        );
        assert!(paths.iter().all(|path| valid_test_name(path)));
        let body = format!("Failing tests: {}\nRefusal: none", paths.join(", "));
        assert_eq!(failing_tests_listed(&body), paths);
        assert!(failing_test_paths(&[]).is_empty());
    }

    /// #185: the `Failing outcomes:` entries keep binary and outcome, one per
    /// exact binary+test, and drop an older satellite's progress counter.
    #[test]
    fn failing_outcomes_keep_the_binary_and_the_timeout_class() {
        let reported: Vec<String> = [
            "TIMEOUT rsid::bin/rsid tests::same",
            "rsid-store tests::same",
            "_5774/8220_ rsid-store tests::same",
            "libtest::only",
            "  ",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            failing_test_outcomes(&reported),
            [
                "FAIL libtest::only",
                "FAIL rsid-store tests::same",
                "TIMEOUT rsid::bin/rsid tests::same",
            ]
        );
        assert_eq!(
            failing_test_paths(&reported),
            ["libtest::only", "tests::same"]
        );
    }

    /// #2167: the qa Issue lists its reds as `Failing tests: a, b (and N more)`.
    #[test]
    fn failing_tests_are_read_from_an_issue_body() {
        let body = "Automatic run.\n\nRan on the hub.\nJob 1, state Failed, exit Some(1); log /x\nFailing tests: tests::a, b::c (and 3 more)\nRefusal: none\n\n- Failing tests: b::c, d::e\nFailing tests: \n";
        assert_eq!(
            failing_tests_listed(body),
            vec!["tests::a".to_string(), "b::c".into(), "d::e".into()]
        );
        assert!(failing_tests_listed("no list here").is_empty());
        // #2787: the gate base-red filer's ``Test: `<id>` `` line is read too;
        // an unquoted prose `Test:` line is not.
        let base_red = "Intro.\n\nTest: `demo::red`\nShard command: `x`\n";
        assert_eq!(
            failing_tests_listed(base_red),
            vec!["demo::red".to_string()]
        );
        assert!(failing_tests_listed("Test: demo::red\nTest: `two words`\nTest: ``\n").is_empty());
        assert_eq!(
            failing_tests_listed("Failing tests: a::b\nTest: `a::b`\nTest: `c::d`"),
            vec!["a::b".to_string(), "c::d".into()]
        );
        assert_eq!(
            union_fix_claims(&["d::e".into(), "z".into()], &failing_tests_listed(body)),
            vec![
                "d::e".to_string(),
                "z".into(),
                "tests::a".into(),
                "b::c".into()
            ]
        );
    }

    fn named_set(count: usize) -> String {
        let names: Vec<String> = (0..count).map(|i| format!("test_number_{i:03}")).collect();
        format!(
            "rsid=shard:session-01:test(/^session::tests::(?:\\w+::)*(?:{})$/)",
            names.join("|")
        )
    }

    #[test]
    fn long_named_set_splits_into_valid_equivalent_filters() {
        let long = named_set(40);
        assert!(long.len() > ROLLING_QUEUE_MAX_FILTER_BYTES);
        let parts = split_test_filter(&long);
        assert!(parts.len() > 1);
        let mut names = Vec::new();
        for part in &parts {
            assert!(valid_test_filter(part), "{part}");
            assert!(
                part.starts_with("rsid=shard:session-01:test(/^session::tests::(?:\\w+::)*(?:")
            );
            assert!(part.ends_with(")$/)"));
            let group = part
                .rsplit_once("*(?:")
                .unwrap()
                .1
                .strip_suffix(")$/)")
                .unwrap();
            names.extend(group.split('|').map(str::to_owned));
        }
        let expected: Vec<String> = (0..40).map(|i| format!("test_number_{i:03}")).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn long_union_splits_and_short_or_unsplittable_stay_whole() {
        let atoms: Vec<String> = (0..30)
            .map(|i| format!("test(name_number_{i:03})"))
            .collect();
        let long = format!("rsid=shard:store-01:{}", atoms.join(" | "));
        let parts = split_test_filter(&long);
        assert!(parts.len() > 1 && parts.iter().all(|p| valid_test_filter(p)));
        assert_eq!(parts.join(" ").matches("test(name_number_").count(), 30);
        assert_eq!(split_test_filter("rsid=short"), vec!["rsid=short"]);
        let opaque = format!("rsi={}", "x".repeat(300));
        assert_eq!(split_test_filter(&opaque), vec![opaque.clone()]);
        assert!(!valid_test_filter(&opaque));
    }

    #[test]
    fn normalize_dedupes_focused_filters_across_shards_and_packages_without_a_cap() {
        let mut input = vec![
            "rsid=shard:store-01:test(foo)".to_owned(),
            "rsid=shard:session-02:test(foo)".to_owned(),
            "rsid-store=shard:store-01:test(foo)".to_owned(),
            "rsid=foo".to_owned(),
            "rsid=shard:store-01".to_owned(),
            "rsid=shard:store-01".to_owned(),
            "rsi=foo".to_owned(),
            "rsi-common=foo".to_owned(),
        ];
        assert_eq!(
            normalize_test_filters(input.clone()),
            vec![
                "rsid=shard:store-01:test(foo)",
                "rsid=shard:store-01",
                "rsi=foo",
                "rsi-common=foo"
            ]
        );
        input = (0..49).map(|i| format!("rsi=module_{i}")).collect();
        input.push(named_set(40));
        let out = normalize_test_filters(input);
        assert!(out.len() > ROLLING_QUEUE_MAX_TEST_FILTERS);
        assert!(out.iter().all(|f| valid_test_filter(f)));
    }

    #[test]
    fn normalize_keeps_same_named_target_selectors_in_each_package() {
        for pair in [
            ["rsid=test:shared", "rsid-store=test:shared"],
            ["rsid=bin:shared", "rsid-store=bin:shared"],
        ] {
            let input: Vec<String> = pair.iter().map(|f| (*f).to_owned()).collect();
            assert_eq!(normalize_test_filters(input.clone()), input);
        }
        // A repeated target selector inside one package is still a duplicate.
        assert_eq!(
            normalize_test_filters(vec!["rsid=test:shared".into(), "rsid=test:shared".into()]),
            vec!["rsid=test:shared"]
        );
    }

    #[test]
    fn derive_filters_excludes_explicit_filters_and_defaults_off() {
        let mut r = request();
        r.derive_filters = true;
        assert_eq!(r.validate(), Err(QUEUE_FILTER_INVALID));
        r.test_filters.clear();
        assert_eq!(r.validate(), Ok(()));
        let parsed: AgentEnqueueLandingSourceRequestV1 = serde_json::from_value(
            serde_json::json!({"source_commit": "a".repeat(40), "idempotency_key": "k"}),
        )
        .unwrap();
        assert!(!parsed.derive_filters);
    }

    #[test]
    fn supersedes_is_optional_and_accepts_only_an_entry_uuid() {
        let mut value =
            serde_json::json!({"source_commit": "a".repeat(40), "idempotency_key": "k"});
        let default: AgentEnqueueLandingSourceRequestV1 =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(default.supersedes, None);
        let id = Uuid::new_v4();
        value["supersedes"] = serde_json::json!(id);
        let replacement: AgentEnqueueLandingSourceRequestV1 =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(replacement.supersedes, Some(id));
        assert_eq!(replacement.validate(), Ok(()));
        value["supersedes"] = serde_json::json!("a".repeat(40));
        assert!(serde_json::from_value::<AgentEnqueueLandingSourceRequestV1>(value).is_err());
    }

    #[test]
    fn state_strings_round_trip() {
        for state in [
            RollingQueueEntryState::Queued,
            RollingQueueEntryState::Admitted,
            RollingQueueEntryState::Gating,
            RollingQueueEntryState::Published,
            RollingQueueEntryState::Refused,
            RollingQueueEntryState::Failed,
            RollingQueueEntryState::Superseded,
        ] {
            assert_eq!(RollingQueueEntryState::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
    }
}
