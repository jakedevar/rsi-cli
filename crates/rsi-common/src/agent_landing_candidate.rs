//! `AgentCheckLandingCandidate` (#2310): one read that tells a manager whether
//! a worker's verification job covers the commit it is about to enqueue.
//!
//! The manager used to read `agent_jobs` with raw SQL to answer four
//! questions before `AgentEnqueueLandingSource`: did the job finish, did its
//! receipt head equal the source commit, did it report new failures, and
//! which reds were already in the baseline. This verb answers them from the
//! job's typed `scoped_test` receipt and the daemon's own scope rules.
//!
//! Two kinds of "no" are kept apart on purpose:
//!
//! - a **call refusal** (`candidate_*` as the whole error text) means the
//!   caller may not ask, or the target is unknown/out of scope (the two look
//!   the same, so the verb is no oracle for foreign sessions or jobs);
//! - a **verdict refusal** (`ok: false` with `refusal` in the result) means
//!   the call worked and the candidate is not yet verified. The result still
//!   carries every fact a manager needs to decide.

use crate::agent_jobs::{AgentJobV1, JOB_ADMISSION_TIMED_OUT, JOB_START_TIMED_OUT, JobState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Call refusals (the whole `InvalidParam`/`PolicyDenied` text).
pub const CANDIDATE_INVALID_REQUEST: &str = "candidate_invalid_request";
/// The caller is not the appointed manager, a portfolio seat or an Epic lead.
pub const CANDIDATE_NOT_AUTHORIZED: &str = "candidate_not_authorized";
/// The job or session is unknown or outside the caller's scope (identical).
pub const CANDIDATE_NOT_FOUND: &str = "candidate_not_found";
/// The whole `candidate_not_found` refusal text (kaizen #254): the code, then
/// the fix. Static and identical for an unknown and an out-of-scope job or
/// session, so it is no oracle for foreign jobs. The fix names the mistake a
/// manager made twice: padding an 8-hex prefix with zeros into a UUID-shaped
/// id, which is then looked up as that (nonexistent) id.
pub const CANDIDATE_NOT_FOUND_REFUSAL: &str = "candidate_not_found: no job or session in your reach matches; send the full job id as AgentWorkerRoster latest_job.job_id shows it, or its first 8+ hex characters as they are (never padded with zeros into a UUID), or omit job_id and name the worker's source_session_id";
/// A job-id prefix names more than one job inside the caller's scope.
pub const CANDIDATE_JOB_ID_AMBIGUOUS: &str = "candidate_job_id_ambiguous";

/// Verdict refusals (`result.refusal` when `ok` is false), most blocking first.
/// No `scoped_test` job exists for the session yet.
pub const CANDIDATE_NO_VERIFICATION_JOB: &str = "candidate_no_verification_job";
/// The job is still queued or running.
pub const CANDIDATE_JOB_NOT_TERMINAL: &str = "candidate_job_not_terminal";
/// The job never got a build slot or start in time (`job_admission_timed_out`
/// or `job_start_timed_out`): it verified nothing, resubmit it.
pub const CANDIDATE_ADMISSION_TIMEOUT: &str = "candidate_admission_timeout";
/// The job is not a `scoped_test` run, or it left no receipt.
pub const CANDIDATE_RECEIPT_MISSING: &str = "candidate_receipt_missing";
/// Candidate/sandbox tools (and legacy jobs without daemon provenance) are
/// tool-under-test evidence, not trusted verification.
pub const CANDIDATE_TOOL_UNTRUSTED: &str = "candidate_tool_untrusted";
/// The run failed (or was lost) before the baseline comparison could name its
/// reds, so a failure cannot be called old.
pub const CANDIDATE_JOB_NOT_GREEN: &str = "candidate_job_not_green";
/// The receipt lists failures that are not baseline reds.
pub const CANDIDATE_NEW_FAILURES: &str = "candidate_new_failures";
/// The receipt head is not the source commit. `docs_only_delta` says whether
/// the commits between them touch documentation only.
pub const CANDIDATE_HEAD_MISMATCH: &str = "candidate_head_mismatch";
pub const CANDIDATE_PROFILE_STALE: &str = "candidate_profile_stale";
pub const CANDIDATE_NO_COMMANDS: &str = "candidate_no_verification_commands";
/// The candidate no longer merges onto `origin/rolling` (#348): the receipt's
/// `merges_cleanly_onto_rolling` is false at the verified head, or a fresh
/// trial merge of `source_commit` onto the current rolling tip conflicts.
/// `conflict_paths` names the files. The worker merges `origin/rolling` (no
/// rebase), resolves, pushes and reruns one `scoped_test`.
pub const CANDIDATE_CONFLICTS_WITH_ROLLING: &str = "candidate_conflicts_with_rolling";

/// A job id may be abbreviated to this many hex characters.
pub const CANDIDATE_JOB_PREFIX_MIN: usize = 8;
/// At most this many non-documentation paths of the head delta are returned.
pub const CANDIDATE_DELTA_PATHS_MAX: usize = 10;
/// At most this many failures per list are returned (the totals stay exact).
pub const CANDIDATE_FAILURES_MAX: usize = 50;
/// At most this many conflicted paths are returned (the receipt records 32).
pub const CANDIDATE_CONFLICT_PATHS_MAX: usize = 32;

/// What a fresh trial merge of the candidate onto the current rolling tip found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollingMergeProbe {
    /// The merge is clean.
    Clean,
    /// The merge conflicts on these paths (possibly empty when git names none).
    Conflict(Vec<String>),
    /// The probe could not run (no rolling ref, a missing object, a timeout).
    Unknown,
}

/// Strict request. Name the job, the worker session, or both (then the job
/// must belong to that session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCheckLandingCandidateRequestV1 {
    /// A job id, or a prefix of at least [`CANDIDATE_JOB_PREFIX_MIN`] hex
    /// characters that is unique inside your scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// The worker session that ran the verification. Without `job_id`, the
    /// newest `scoped_test` job whose receipt head is `source_commit` (else
    /// the newest `scoped_test` job) owned by this session is checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<Uuid>,
    /// The full lowercase 40-hex commit you would enqueue.
    pub source_commit: String,
}

impl AgentCheckLandingCandidateRequestV1 {
    /// # Errors
    /// `candidate_invalid_request` for a malformed commit or job id, a nil
    /// session id, or a request naming neither a job nor a session.
    pub fn validate(&self) -> Result<(), &'static str> {
        let commit_ok = self.source_commit.len() == 40
            && self
                .source_commit
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        let job_ok = self.job_id.as_deref().is_none_or(valid_job_id_text);
        let session_ok = self.source_session_id.is_none_or(|id| !id.is_nil());
        if !commit_ok
            || !job_ok
            || !session_ok
            || (self.job_id.is_none() && self.source_session_id.is_none())
        {
            return Err(CANDIDATE_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// A full job id or a lowercase hex prefix of at least
/// [`CANDIDATE_JOB_PREFIX_MIN`] hex characters; hyphens (as a UUID shows them)
/// are allowed and ignored.
fn valid_job_id_text(text: &str) -> bool {
    text.len() <= 36
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'-'))
        && (CANDIDATE_JOB_PREFIX_MIN..=32).contains(&job_id_hex(text).len())
}

/// The hex digits of a job-id prefix: what the store compares with the
/// stored id text without its hyphens.
#[must_use]
pub fn job_id_hex(text: &str) -> String {
    text.chars().filter(|c| *c != '-').collect()
}

/// One failing test, as the `scoped_test` receipt names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateFailureV1 {
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub test: String,
}

/// One package's outcome in the receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePackageV1 {
    pub package: String,
    pub status: String,
    pub completed: u64,
}

/// A candidate-copy run retained alongside the trusted rolling verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateToolEvidenceV1 {
    pub job_id: Uuid,
    pub scoped_tool: Option<crate::agent_jobs::ScopedToolProvenanceV1>,
}

/// What a manager needs to decide whether to enqueue `source_commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCheckLandingCandidateResultV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_verification: Option<crate::agent_jobs::ProfileVerificationV1>,
    /// True only when a finished `scoped_test` job's receipt head is
    /// `source_commit` and it reports no new failure.
    pub ok: bool,
    /// The verdict refusal when `ok` is false (`candidate_*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// One line saying what to do next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub source_commit: String,
    /// The session that owns the checked job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_owner_session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The job's own refusal (for example `job_admission_timed_out`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// The commit the job verified, as its receipt records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_head: Option<String>,
    /// Daemon-written identity of the selected tool; never receipt.tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_tool: Option<crate::agent_jobs::ScopedToolProvenanceV1>,
    /// When another job supplied trusted verification, retain the requested
    /// candidate-tool run as tool-under-test evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_under_test: Option<CandidateToolEvidenceV1>,
    /// The scoped receipt's failure classification, independent of the red verdict (#516).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    pub head_matches: bool,
    /// When the receipt head is not `source_commit`: whether every path that
    /// differs between them is documentation (see [`is_docs_path`]). Absent
    /// when the heads match or the delta could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_only_delta: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_files: Option<u32>,
    /// Up to [`CANDIDATE_DELTA_PATHS_MAX`] delta paths that are not documentation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delta_code_paths: Vec<String>,
    /// Failures that are not baseline reds; `null` when the baseline
    /// comparison did not complete or the run never reached it.
    pub new_failures: Option<Vec<CandidateFailureV1>>,
    #[serde(default)]
    pub new_failures_total: u32,
    /// Failures that also fail on the base (the landing gate excuses them).
    pub baseline_reds: Vec<CandidateFailureV1>,
    #[serde(default)]
    pub baseline_reds_total: u32,
    /// Completed tests, summed over the receipt's packages.
    pub tests_completed: u64,
    pub packages: Vec<CandidatePackageV1>,
    /// `precise`, or `targeted_subset` when the plan was bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// True when the receipt says the merge-queue gate covers what the run left out.
    pub gate_covers_rest: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_tests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_tests: Option<u64>,
    /// The receipt's uncertainty marker (for example `broad_observers`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertain: Option<String>,
    /// The receipt's stale-base note: reds already present at the branch point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_base: Option<String>,
    /// The receipt's trial merge of its head onto `origin/rolling` at job end
    /// (#348): `Some(false)` means the verified commit no longer merges. Absent
    /// for a receipt that predates the field, and `null` when git could not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merges_cleanly_onto_rolling: Option<bool>,
    /// The receipt's conflicted paths (at most [`CANDIDATE_CONFLICT_PATHS_MAX`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_paths: Vec<String>,
    /// A fresh trial merge of `source_commit` onto the current `origin/rolling`:
    /// `Some(true)` clean, `Some(false)` conflicts, absent when it did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merges_cleanly_onto_rolling_now: Option<bool>,
    /// The paths the fresh trial merge conflicts on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_paths_now: Vec<String>,
}

impl AgentCheckLandingCandidateResultV1 {
    /// The result for a session with no `scoped_test` job.
    #[must_use]
    pub fn no_job(source_commit: &str, owner: Option<Uuid>) -> Self {
        let mut result = Self::empty(source_commit);
        result.job_owner_session_id = owner;
        result.refuse(
            CANDIDATE_NO_VERIFICATION_JOB,
            "no scoped_test job exists for this session: have the worker submit one",
        );
        result
    }

    fn empty(source_commit: &str) -> Self {
        Self {
            profile_verification: None,
            ok: false,
            refusal: None,
            detail: None,
            source_commit: source_commit.to_owned(),
            job_owner_session_id: None,
            job_id: None,
            job_kind: None,
            job_state: None,
            exit_code: None,
            job_refusal: None,
            finished_at: None,
            receipt_head: None,
            scoped_tool: None,
            tool_under_test: None,
            failure_class: None,
            head_matches: false,
            docs_only_delta: None,
            delta_files: None,
            delta_code_paths: Vec::new(),
            new_failures: None,
            new_failures_total: 0,
            baseline_reds: Vec::new(),
            baseline_reds_total: 0,
            tests_completed: 0,
            packages: Vec::new(),
            plan: None,
            gate_covers_rest: false,
            selected_tests: None,
            run_tests: None,
            uncertain: None,
            stale_base: None,
            merges_cleanly_onto_rolling: None,
            conflict_paths: Vec::new(),
            merges_cleanly_onto_rolling_now: None,
            conflict_paths_now: Vec::new(),
        }
    }

    /// Fold the rolling-merge facts into the verdict. A fresh probe of
    /// `source_commit` wins over the receipt; without one, the receipt counts
    /// only while its head is `source_commit`. A conflict turns an otherwise
    /// verified candidate into the `candidate_conflicts_with_rolling` refusal
    /// (lowest precedence: it never hides an earlier blocker), and a clean fresh
    /// probe lifts that refusal when only a stale receipt raised it.
    pub fn apply_rolling_merge(&mut self, fresh: &RollingMergeProbe) {
        match fresh {
            RollingMergeProbe::Clean => {
                self.merges_cleanly_onto_rolling_now = Some(true);
                self.conflict_paths_now.clear();
            }
            RollingMergeProbe::Conflict(paths) => {
                self.merges_cleanly_onto_rolling_now = Some(false);
                self.conflict_paths_now = bounded_paths(paths);
            }
            RollingMergeProbe::Unknown => {}
        }
        let conflict = match self.merges_cleanly_onto_rolling_now {
            Some(false) => Some((&self.conflict_paths_now, "now")),
            Some(true) => None,
            None if self.head_matches && self.merges_cleanly_onto_rolling == Some(false) => {
                Some((&self.conflict_paths, "at the end of the verification job"))
            }
            None => None,
        };
        let Some((paths, when)) = conflict else {
            if self.refusal.as_deref() == Some(CANDIDATE_CONFLICTS_WITH_ROLLING) {
                self.lift_rolling_conflict();
            }
            return;
        };
        let reason = conflict_reason(paths, when);
        let blocked_earlier = self
            .refusal
            .as_deref()
            .is_some_and(|code| code != CANDIDATE_CONFLICTS_WITH_ROLLING);
        if blocked_earlier {
            return;
        }
        self.refuse(CANDIDATE_CONFLICTS_WITH_ROLLING, &reason);
    }

    /// The conflict refusal no longer applies: the candidate is verified.
    fn lift_rolling_conflict(&mut self) {
        self.ok = true;
        self.refusal = None;
        self.detail = Some(self.verified_detail());
    }

    fn verified_detail(&self) -> String {
        if self.plan.as_deref() == Some("targeted_subset") && self.gate_covers_rest {
            "verified at source_commit (bounded subset; the merge-queue gate covers the rest)"
        } else {
            "verified at source_commit"
        }
        .to_owned()
    }

    pub fn refuse(&mut self, code: &str, detail: &str) {
        self.ok = false;
        self.refusal = Some(code.to_owned());
        self.detail = Some(detail.to_owned());
    }

    /// The delta between the receipt head and the source commit was too large
    /// to read (`why`: a cap or the deadline, #2344). Fails closed:
    /// `docs_only_delta` stays unset and the detail says why.
    pub fn set_delta_too_large(&mut self, why: &str) {
        if self.refusal.as_deref() == Some(CANDIDATE_HEAD_MISMATCH) {
            self.detail = Some(format!(
                "the receipt head is not source_commit and the delta was too large to classify ({why}): treat it as code and have the worker rerun scoped_test at source_commit"
            ));
        }
    }

    /// Record what differs between the receipt head and the source commit.
    /// `paths` is the full `git diff --name-only` list.
    pub fn set_delta(&mut self, paths: &[String]) {
        let code: Vec<&String> = paths.iter().filter(|path| !is_docs_path(path)).collect();
        self.delta_files = Some(u32::try_from(paths.len()).unwrap_or(u32::MAX));
        self.docs_only_delta = Some(code.is_empty());
        self.delta_code_paths = code
            .into_iter()
            .take(CANDIDATE_DELTA_PATHS_MAX)
            .cloned()
            .collect();
        if self.refusal.as_deref() == Some(CANDIDATE_HEAD_MISMATCH) {
            self.detail = Some(if paths.is_empty() {
                "the receipt head differs from source_commit but no path changed between them"
                    .to_owned()
            } else if self.docs_only_delta == Some(true) {
                format!(
                    "the receipt head is older than source_commit by a documentation-only delta ({} file(s)): your call whether to rerun",
                    paths.len()
                )
            } else {
                "the receipt head is not source_commit and the delta touches code: have the worker rerun scoped_test at source_commit"
                    .to_owned()
            });
        }
    }
}

/// Whether `path` is documentation for the docs-only delta test: outside
/// `crates/` and `scripts/` and either under `docs/`, `thoughts/` or a Markdown
/// file. A build file, tool config or workflow is never documentation.
#[must_use]
pub fn is_docs_path(path: &str) -> bool {
    if path.starts_with("crates/") || path.starts_with("scripts/") {
        return false;
    }
    path.starts_with("docs/")
        || path.starts_with("thoughts/")
        || std::path::Path::new(path)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

/// Whether `job` is a `scoped_test` run.
#[must_use]
pub fn is_scoped_test_job(job: &AgentJobV1) -> bool {
    matches!(&job.params, crate::agent_jobs::JobParams::Test(params) if params.is_scoped_test())
}

/// Whether the daemon stamped `job` as run with the rolling tool copy. A
/// missing (legacy) stamp, a candidate or a sandbox tool copy is not trusted:
/// the tool under test cannot vouch for its own receipt. The one predicate
/// every verification judge shares with [`assess_candidate`].
#[must_use]
pub fn has_rolling_provenance(job: &AgentJobV1) -> bool {
    job.result
        .as_ref()
        .and_then(|result| result.scoped_tool.as_ref())
        .is_some_and(|tool| tool.source == crate::agent_jobs::ScopedToolSource::Rolling)
}

/// The head commit a finished job's receipt records.
#[must_use]
pub fn receipt_head(job: &AgentJobV1) -> Option<&str> {
    if let Some(profile) = job.result.as_ref()?.profile_verification.as_ref() {
        return Some(&profile.binding.head);
    }
    job.result.as_ref()?.receipt.as_ref()?.get("head")?.as_str()
}

/// Includes profile recipe jobs without treating arbitrary recipe output as
/// evidence. Only the daemon can populate profile_verification in stored rows.
#[must_use]
pub fn is_verification_job(job: &AgentJobV1) -> bool {
    is_scoped_test_job(job)
        || job
            .result
            .as_ref()
            .is_some_and(|r| r.profile_verification.is_some())
}

fn failures(value: Option<&Value>) -> Option<Vec<CandidateFailureV1>> {
    let array = value?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect(),
    )
}

fn bounded(mut list: Vec<CandidateFailureV1>) -> (Vec<CandidateFailureV1>, u32) {
    let total = u32::try_from(list.len()).unwrap_or(u32::MAX);
    list.truncate(CANDIDATE_FAILURES_MAX);
    (list, total)
}

/// Judge one job against `source_commit` from its stored state and typed
/// receipt. Pure: the git delta is added afterwards with
/// [`AgentCheckLandingCandidateResultV1::set_delta`].
///
/// Precedence, most blocking first: not terminal, admission/start timeout,
/// missing receipt, run not green with no baseline comparison, new failures,
/// head mismatch. A run that exits non-zero only on baseline reds is `ok`:
/// the landing gate is "no new failures relative to rolling".
#[must_use]
pub fn assess_candidate(
    job: &AgentJobV1,
    source_commit: &str,
) -> AgentCheckLandingCandidateResultV1 {
    let mut result = AgentCheckLandingCandidateResultV1::empty(source_commit);
    result.job_owner_session_id = Some(job.owner_session_id);
    result.job_id = Some(job.id);
    result.job_kind = Some(job.kind.as_str().to_owned());
    result.job_state = Some(job.state.as_str().to_owned());
    result.exit_code = job.exit_code;
    result.finished_at = job.finished_at.clone();
    let job_result = job.result.as_ref();
    result.job_refusal = job_result.and_then(|r| r.refusal.clone());
    result.scoped_tool = job_result.and_then(|r| r.scoped_tool.clone());
    if let Some(profile) = job_result.and_then(|r| r.profile_verification.as_ref()) {
        result.profile_verification = Some(profile.clone());
        result.receipt_head = Some(profile.binding.head.clone());
        result.head_matches = profile.binding.head == source_commit;
        if matches!(job.state, JobState::Queued | JobState::Running) {
            result.refuse(
                CANDIDATE_JOB_NOT_TERMINAL,
                "wait for the profile verification job to finish",
            );
        } else if profile.binding.version != 1
            || !profile.binding.clean
            || profile.binding.cwd != job.cwd
            || profile.binding.warning_policy != "generic"
            || profile.binding.profile_digest.len() != 64
            || profile.binding.tools.is_empty()
            || profile.binding.tools.iter().any(|tool| {
                !std::path::Path::new(&tool.path).is_absolute()
                    || tool.blob.as_ref().is_none_or(|digest| digest.len() != 64)
            })
        {
            result.refuse(
                CANDIDATE_TOOL_UNTRUSTED,
                "missing or invalid daemon profile/tool binding; rerun verification",
            );
        } else if profile.binding.commands == 0 {
            result.refuse(
                CANDIDATE_NO_COMMANDS,
                "the profile executed no verification command",
            );
        } else if job.state != JobState::Succeeded
            || job.exit_code != Some(0)
            || job_result.is_none_or(|r| r.refusal.is_some() || r.exit_code != Some(0))
            || profile.completed_commands != profile.binding.commands
            || profile.warning_verdict != "generic_command_exit"
            || job.finished_at.is_none()
        {
            result.refuse(
                CANDIDATE_JOB_NOT_GREEN,
                "profile verification did not finish successfully; read the job refusal and rerun",
            );
        } else if !result.head_matches {
            result.refuse(
                CANDIDATE_HEAD_MISMATCH,
                "profile verification is for another source commit; rerun at source_commit",
            );
        } else {
            result.ok = true;
            result.detail = Some("verified declared project commands at source_commit; generic warning policy applies (no Rust baseline)".into());
        }
        return result;
    }

    let receipt = job_result.and_then(|r| r.receipt.as_ref());
    if let Some(receipt) = receipt {
        result.receipt_head = receipt
            .get("head")
            .and_then(Value::as_str)
            .map(str::to_owned);
        result.head_matches = result.receipt_head.as_deref() == Some(source_commit);
        let (reds, reds_total) =
            bounded(failures(receipt.get("baseline_reds")).unwrap_or_default());
        result.baseline_reds = reds;
        result.baseline_reds_total = reds_total;
        if let Some(list) = failures(receipt.get("new_failures")) {
            let (list, total) = bounded(list);
            result.new_failures = Some(list);
            result.new_failures_total = total;
        }
        result.packages = receipt
            .get("packages")
            .and_then(Value::as_array)
            .map(|packages| {
                packages
                    .iter()
                    .map(|package| CandidatePackageV1 {
                        package: package
                            .get("package")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        status: package
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        completed: package
                            .get("completed")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        result.tests_completed = result.packages.iter().map(|p| p.completed).sum();
        let text = |key: &str| receipt.get(key).and_then(Value::as_str).map(str::to_owned);
        result.failure_class = text("failure_class");
        result.plan = text("plan");
        result.uncertain = text("uncertain");
        result.stale_base = text("stale_base");
        result.gate_covers_rest = receipt
            .get("gate_covers_rest")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        result.selected_tests = receipt.get("selected_tests").and_then(Value::as_u64);
        result.run_tests = receipt.get("run_tests").and_then(Value::as_u64);
        result.merges_cleanly_onto_rolling = receipt
            .get("merges_cleanly_onto_rolling")
            .and_then(Value::as_bool);
        result.conflict_paths = receipt
            .get("conflict_paths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .take(CANDIDATE_CONFLICT_PATHS_MAX)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
    }

    if matches!(job.state, JobState::Queued | JobState::Running) {
        result.refuse(
            CANDIDATE_JOB_NOT_TERMINAL,
            "the job has not finished: wait for its wake, then check again",
        );
        return result;
    }
    if matches!(
        result.job_refusal.as_deref(),
        Some(JOB_ADMISSION_TIMED_OUT | JOB_START_TIMED_OUT)
    ) {
        result.refuse(
            CANDIDATE_ADMISSION_TIMEOUT,
            "the job never got a build slot or start in time, so it verified nothing: have the worker resubmit scoped_test",
        );
        return result;
    }
    if !is_scoped_test_job(job) || receipt.is_none() {
        result.refuse(
            CANDIDATE_RECEIPT_MISSING,
            "the job left no scoped_test receipt: have the worker submit scoped_test",
        );
        return result;
    }
    if !has_rolling_provenance(job) {
        result.refuse(
            CANDIDATE_TOOL_UNTRUSTED,
            "daemon provenance does not identify a rolling tool copy: rerun scoped_test with the rolling tool copy (params.scoped_test.rolling_tool:true); this run remains tool-under-test evidence",
        );
        return result;
    }
    let receipt_ok = receipt
        .and_then(|r| r.get("ok"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if result.failure_class.as_deref() == Some("infrastructure_interrupted") {
        result.refuse(
            CANDIDATE_JOB_NOT_GREEN,
            "compile/test processes died by signal: rerun once at the same committed head; this interruption does not count toward the worker's two-run limit",
        );
        return result;
    }
    match &result.new_failures {
        Some(new) if !new.is_empty() => {
            result.refuse(
                CANDIDATE_NEW_FAILURES,
                "the run reports failures that are not baseline reds: fix them or file them against the base",
            );
            return result;
        }
        // `null` new_failures is the green path only when the run itself was
        // green (no failure needed a baseline comparison).
        None if !receipt_ok => {
            result.refuse(
                CANDIDATE_JOB_NOT_GREEN,
                "the run failed before the baseline comparison could classify its failures: read the job log and rerun",
            );
            return result;
        }
        _ => {}
    }
    if !result.head_matches {
        result.refuse(
            CANDIDATE_HEAD_MISMATCH,
            "the receipt head is not source_commit: have the worker rerun scoped_test at source_commit",
        );
        return result;
    }
    result.ok = true;
    result.detail = Some(result.verified_detail());
    result.apply_rolling_merge(&RollingMergeProbe::Unknown);
    result
}

/// Supplement an untrusted tool run only with an independently acceptable
/// rolling-tool job at the same head, owned by the same session. All reported
/// verification fields then come from that trusted job, never the forged run.
#[must_use]
pub fn assess_candidate_with_alternatives<'a>(
    job: &AgentJobV1,
    alternatives: impl IntoIterator<Item = &'a AgentJobV1>,
    source_commit: &str,
) -> AgentCheckLandingCandidateResultV1 {
    let result = assess_candidate(job, source_commit);
    if result.refusal.as_deref() != Some(CANDIDATE_TOOL_UNTRUSTED) {
        return result;
    }
    for alternative in alternatives {
        if alternative.owner_session_id != job.owner_session_id || alternative.id == job.id {
            continue;
        }
        // This Rust-tool substitution has no live profile recheck. Profile
        // evidence must take the daemon's dedicated candidate path instead.
        if !is_scoped_test_job(alternative) || !has_rolling_provenance(alternative) {
            continue;
        }
        let mut trusted = assess_candidate(alternative, source_commit);
        if trusted.ok {
            trusted.tool_under_test = Some(CandidateToolEvidenceV1 {
                job_id: job.id,
                scoped_tool: result.scoped_tool,
            });
            return trusted;
        }
    }
    result
}

fn bounded_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .take(CANDIDATE_CONFLICT_PATHS_MAX)
        .cloned()
        .collect()
}

/// The refusal text: what conflicts, when it was seen and what the worker does.
fn conflict_reason(paths: &[String], when: &str) -> String {
    let named = if paths.is_empty() {
        "git named no path".to_owned()
    } else {
        format!("conflicting paths: {}", paths.join(", "))
    };
    format!(
        "source_commit does not merge onto origin/rolling ({when}; {named}): have the worker merge origin/rolling (no rebase), resolve, push and rerun one scoped_test"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn profile_verification_requires_completed_bound_commands_and_generic_warning_verdict() {
        use crate::agent_jobs::{
            ProfileVerificationBindingV1, ProfileVerificationV1, ScopedToolFileV1,
        };
        let mut job = job(
            JobState::Succeeded,
            Some(0),
            Some(AgentJobResultV1 {
                exit_code: Some(0),
                ..AgentJobResultV1::default()
            }),
        );
        job.result.as_mut().unwrap().profile_verification = Some(ProfileVerificationV1 {
            binding: ProfileVerificationBindingV1 {
                version: 1,
                head: COMMIT.into(),
                cwd: job.cwd.clone(),
                clean: true,
                profile_digest: "a".repeat(64),
                tools: vec![ScopedToolFileV1 {
                    path: "/usr/bin/make".into(),
                    blob: Some("b".repeat(64)),
                }],
                argv: vec!["/bin/sh".into()],
                commands: 1,
                warning_policy: "generic".into(),
            },
            completed_commands: 1,
            warning_verdict: "generic_command_exit".into(),
        });
        assert!(assess_candidate(&job, COMMIT).ok);
        assert_eq!(
            assess_candidate(&job, OTHER).refusal.as_deref(),
            Some(CANDIDATE_HEAD_MISMATCH)
        );
        for mutation in 0..7 {
            let mut invalid = job.clone();
            let evidence = invalid
                .result
                .as_mut()
                .unwrap()
                .profile_verification
                .as_mut()
                .unwrap();
            match mutation {
                0 => evidence.binding.commands = 0,
                1 => evidence.completed_commands = 0,
                2 => evidence.warning_verdict = "rust_waiver".into(),
                3 => evidence.binding.clean = false,
                4 => invalid.state = JobState::Failed,
                5 => invalid.exit_code = Some(1),
                _ => invalid.finished_at = None,
            }
            assert!(
                !assess_candidate(&invalid, COMMIT).ok,
                "mutation {mutation}"
            );
        }
        let mut forged = job.clone();
        forged.result.as_mut().unwrap().profile_verification = None;
        forged.result.as_mut().unwrap().receipt = Some(
            json!({"profile_verification": job.result.as_ref().unwrap().profile_verification.clone()}),
        );
        assert!(!assess_candidate(&forged, COMMIT).ok);
        let mut untrusted = forged.clone();
        untrusted.id = Uuid::new_v4();
        untrusted.result.as_mut().unwrap().receipt = Some(green_receipt(COMMIT));
        assert_eq!(
            assess_candidate_with_alternatives(&untrusted, [&job], COMMIT)
                .refusal
                .as_deref(),
            Some(CANDIDATE_TOOL_UNTRUSTED)
        );
    }
    #[test]
    fn not_found_refusal_leads_with_its_code_and_names_the_fix() {
        let (code, fix) = CANDIDATE_NOT_FOUND_REFUSAL
            .split_once(": ")
            .expect("code: detail");
        assert_eq!(code, CANDIDATE_NOT_FOUND);
        assert!(fix.contains("latest_job.job_id"), "{fix}");
        assert!(fix.contains("never padded with zeros"), "{fix}");
        assert!(fix.contains("source_session_id"), "{fix}");
    }

    use super::*;
    use crate::agent_jobs::{
        AgentJobResultV1, JobKind, JobParams, JobWake, ScopedTestParams, TestJobParams,
    };
    use serde_json::json;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn scoped_params() -> JobParams {
        JobParams::Test(TestJobParams {
            scoped_test: Some(ScopedTestParams {
                base: "origin/rolling".into(),
                head: None,
                rolling_tool: false,
            }),
            ..serde_json::from_value(json!({})).expect("default test params")
        })
    }

    fn job(
        state: JobState,
        exit_code: Option<i32>,
        result: Option<AgentJobResultV1>,
    ) -> AgentJobV1 {
        AgentJobV1 {
            id: Uuid::new_v4(),
            kind: JobKind::Test,
            name: None,
            state,
            owner_session_id: Uuid::new_v4(),
            unit_name: "unit".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/log".into(),
            params: scoped_params(),
            exit_code,
            result,
            created_at: "2026-10-13T00:00:00.000000000Z".into(),
            started_at: None,
            finished_at: Some("2026-10-13T00:10:00.000000000Z".into()),
            held: None,
            hold: None,
            wake: JobWake::Owner,
        }
    }

    fn with_receipt(receipt: Value) -> AgentJobResultV1 {
        AgentJobResultV1 {
            exit_code: Some(0),
            receipt: Some(receipt),
            scoped_tool: Some(crate::agent_jobs::ScopedToolProvenanceV1 {
                source: crate::agent_jobs::ScopedToolSource::Rolling,
                path: "/daemon/tool/scripts/scoped-test".into(),
                rev: Some(OTHER.into()),
                files: vec![crate::agent_jobs::ScopedToolFileV1 {
                    path: "/daemon/tool/scripts/scoped-test".into(),
                    blob: Some(COMMIT.into()),
                }],
            }),
            ..AgentJobResultV1::default()
        }
    }

    fn green_receipt(head: &str) -> Value {
        json!({
            "ok": true, "exit_code": 0, "head": head, "plan": "targeted_subset",
            "gate_covers_rest": true, "new_failures": null, "baseline_reds": [],
            "packages": [
                {"package": "rsid-store", "status": "not applicable", "completed": 0},
                {"package": "rsid", "status": "passed", "completed": 481}
            ],
            "selected_tests": 481, "run_tests": 481
        })
    }

    #[test]
    fn matching_head_and_no_failures_is_ok() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(COMMIT))),
        );
        let result = assess_candidate(&job, COMMIT);
        assert!(result.ok, "{result:?}");
        assert_eq!(result.refusal, None);
        assert!(result.head_matches);
        assert_eq!(result.tests_completed, 481);
        assert_eq!(result.plan.as_deref(), Some("targeted_subset"));
        assert!(result.gate_covers_rest);
        assert_eq!(result.job_id, Some(job.id));
        assert_eq!(result.receipt_head.as_deref(), Some(COMMIT));
    }

    #[test]
    fn only_daemon_rolling_provenance_can_verify_a_synthetic_green_receipt() {
        use crate::agent_jobs::ScopedToolSource;
        for source in [ScopedToolSource::Candidate, ScopedToolSource::Sandbox] {
            for claimed_tool in [Value::Null, json!({"source": "rolling", "rev": OTHER})] {
                let mut receipt = green_receipt(COMMIT);
                if !claimed_tool.is_null() {
                    receipt["tool"] = claimed_tool;
                }
                let mut result = with_receipt(receipt);
                result.scoped_tool.as_mut().unwrap().source = source.clone();
                let untrusted = job(JobState::Succeeded, Some(0), Some(result));
                let verdict = assess_candidate(&untrusted, COMMIT);
                assert_eq!(verdict.ok, false);
                assert_eq!(verdict.refusal.as_deref(), Some(CANDIDATE_TOOL_UNTRUSTED));
                assert_eq!(verdict.scoped_tool.as_ref().unwrap().source, source);
                assert!(
                    verdict
                        .detail
                        .unwrap()
                        .contains("rerun scoped_test with the rolling tool copy")
                );

                let mut rolling = job(
                    JobState::Succeeded,
                    Some(0),
                    Some(with_receipt(green_receipt(COMMIT))),
                );
                rolling.owner_session_id = untrusted.owner_session_id;
                let verified = assess_candidate_with_alternatives(&untrusted, [&rolling], COMMIT);
                assert!(verified.ok, "{verified:?}");
                assert_eq!(verified.job_id, Some(rolling.id));
                assert_eq!(
                    verified.scoped_tool.unwrap().source,
                    ScopedToolSource::Rolling
                );
                let evidence = verified.tool_under_test.unwrap();
                assert_eq!(evidence.job_id, untrusted.id);
                assert_eq!(evidence.scoped_tool.unwrap().source, source);

                rolling.owner_session_id = Uuid::new_v4();
                assert_eq!(
                    assess_candidate_with_alternatives(&untrusted, [&rolling], COMMIT)
                        .refusal
                        .as_deref(),
                    Some(CANDIDATE_TOOL_UNTRUSTED)
                );
                rolling.owner_session_id = untrusted.owner_session_id;
                rolling.result.as_mut().unwrap().receipt.as_mut().unwrap()["head"] = json!(OTHER);
                assert_eq!(
                    assess_candidate_with_alternatives(&untrusted, [&rolling], COMMIT)
                        .refusal
                        .as_deref(),
                    Some(CANDIDATE_TOOL_UNTRUSTED)
                );
            }
        }
        let mut legacy = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(COMMIT))),
        );
        legacy.result.as_mut().unwrap().scoped_tool = None;
        assert_eq!(
            assess_candidate(&legacy, COMMIT).refusal.as_deref(),
            Some(CANDIDATE_TOOL_UNTRUSTED)
        );
    }

    #[test]
    fn head_mismatch_is_refused_and_the_delta_decides_docs_only() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(OTHER))),
        );
        let mut result = assess_candidate(&job, COMMIT);
        assert!(!result.ok);
        assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_HEAD_MISMATCH));
        assert!(!result.head_matches);
        assert_eq!(result.docs_only_delta, None);

        result.set_delta(&[
            "docs/agent-control.md".into(),
            "thoughts/shared/x.md".into(),
        ]);
        assert_eq!(result.docs_only_delta, Some(true));
        assert_eq!(result.delta_files, Some(2));
        assert!(result.delta_code_paths.is_empty());
        assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_HEAD_MISMATCH));
        assert!(!result.ok);

        result.set_delta(&["docs/a.md".into(), "crates/rsid/src/lib.rs".into()]);
        assert_eq!(result.docs_only_delta, Some(false));
        assert_eq!(result.delta_code_paths, vec!["crates/rsid/src/lib.rs"]);
    }

    #[test]
    fn a_delta_too_large_to_read_leaves_docs_only_unset_and_says_why() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(OTHER))),
        );
        let mut result = assess_candidate(&job, COMMIT);
        result.set_delta_too_large("deadline");
        assert_eq!(result.docs_only_delta, None);
        assert_eq!(result.delta_files, None);
        assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_HEAD_MISMATCH));
        assert!(!result.ok);
        let detail = result.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("too large") && detail.contains("deadline"),
            "{detail}"
        );
    }

    #[test]
    fn docs_paths_exclude_code_build_files_and_crate_docs() {
        for docs in [
            "docs/a.png",
            "thoughts/shared/n.txt",
            "README.md",
            ".claude/skills/x/SKILL.md",
        ] {
            assert!(is_docs_path(docs), "{docs}");
        }
        for code in [
            "crates/rsid/src/lib.rs",
            "crates/rsid/AGENTS.md",
            "scripts/scoped-test",
            "scripts/notes.md",
            "Cargo.toml",
            "Cargo.lock",
            "tools/released-migrations.json",
            ".config/nextest.toml",
        ] {
            assert!(!is_docs_path(code), "{code}");
        }
    }

    #[test]
    fn a_new_failure_is_refused_before_a_head_mismatch() {
        let mut receipt = green_receipt(OTHER);
        receipt["ok"] = json!(false);
        receipt["exit_code"] = json!(1);
        receipt["new_failures"] = json!([
            {"package": "rsid", "kind": "lib", "target": "rsid", "test": "a::b"}
        ]);
        let job = job(JobState::Failed, Some(1), Some(with_receipt(receipt)));
        let result = assess_candidate(&job, COMMIT);
        assert!(!result.ok);
        assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_NEW_FAILURES));
        assert_eq!(result.new_failures_total, 1);
        assert_eq!(result.new_failures.as_ref().expect("list")[0].test, "a::b");
    }

    #[test]
    fn baseline_reds_alone_do_not_block() {
        let mut receipt = green_receipt(COMMIT);
        receipt["ok"] = json!(false);
        receipt["exit_code"] = json!(1);
        receipt["new_failures"] = json!([]);
        receipt["baseline_reds"] = json!([
            {"package": "rsid", "kind": "lib", "target": "rsid", "test": "old::red"}
        ]);
        let job = job(JobState::Failed, Some(1), Some(with_receipt(receipt)));
        let result = assess_candidate(&job, COMMIT);
        assert!(result.ok, "{result:?}");
        assert_eq!(result.baseline_reds_total, 1);
        assert_eq!(result.baseline_reds[0].test, "old::red");
        assert_eq!(result.new_failures, Some(Vec::new()));
    }

    #[test]
    fn a_red_run_without_a_baseline_comparison_is_not_green() {
        let mut receipt = green_receipt(COMMIT);
        receipt["ok"] = json!(false);
        receipt["exit_code"] = json!(1);
        let job = job(JobState::Failed, Some(1), Some(with_receipt(receipt)));
        let result = assess_candidate(&job, COMMIT);
        assert!(!result.ok);
        assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_JOB_NOT_GREEN));
        assert_eq!(result.new_failures, None);
    }

    #[test]
    fn an_admission_timeout_is_its_own_refusal() {
        for code in [JOB_ADMISSION_TIMED_OUT, JOB_START_TIMED_OUT] {
            let result = AgentJobResultV1 {
                refusal: Some(code.into()),
                ..AgentJobResultV1::default()
            };
            let job = job(JobState::Failed, None, Some(result));
            let verdict = assess_candidate(&job, COMMIT);
            assert!(!verdict.ok);
            assert_eq!(
                verdict.refusal.as_deref(),
                Some(CANDIDATE_ADMISSION_TIMEOUT)
            );
            assert_eq!(verdict.job_refusal.as_deref(), Some(code));
        }
    }

    #[test]
    fn running_jobs_and_receipt_less_jobs_are_refused() {
        let running = job(JobState::Running, None, None);
        assert_eq!(
            assess_candidate(&running, COMMIT).refusal.as_deref(),
            Some(CANDIDATE_JOB_NOT_TERMINAL)
        );
        let bare = job(
            JobState::Succeeded,
            Some(0),
            Some(AgentJobResultV1::default()),
        );
        assert_eq!(
            assess_candidate(&bare, COMMIT).refusal.as_deref(),
            Some(CANDIDATE_RECEIPT_MISSING)
        );
        let mut plain = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(COMMIT))),
        );
        plain.params =
            JobParams::Test(serde_json::from_value(json!({})).expect("default test params"));
        assert_eq!(
            assess_candidate(&plain, COMMIT).refusal.as_deref(),
            Some(CANDIDATE_RECEIPT_MISSING)
        );
    }

    #[test]
    fn no_job_is_a_verdict_with_the_owner_named() {
        let owner = Uuid::new_v4();
        let result = AgentCheckLandingCandidateResultV1::no_job(COMMIT, Some(owner));
        assert!(!result.ok);
        assert_eq!(
            result.refusal.as_deref(),
            Some(CANDIDATE_NO_VERIFICATION_JOB)
        );
        assert_eq!(result.job_owner_session_id, Some(owner));
    }

    #[test]
    fn request_validation_pins_the_shapes() {
        let ok = |job_id: Option<&str>, session: Option<Uuid>, commit: &str| {
            AgentCheckLandingCandidateRequestV1 {
                job_id: job_id.map(str::to_owned),
                source_session_id: session,
                source_commit: commit.into(),
            }
            .validate()
            .is_ok()
        };
        let session = Some(Uuid::new_v4());
        assert!(ok(None, session, COMMIT));
        assert!(ok(Some("a7f23944"), None, COMMIT));
        assert!(ok(Some("a7f23944-81a6"), None, COMMIT));
        assert!(ok(
            Some("a7f23944-81a6-4232-8ad0-e59a04269330"),
            session,
            COMMIT
        ));
        assert!(!ok(None, None, COMMIT));
        assert!(!ok(Some("a7f2394"), None, COMMIT));
        assert!(!ok(Some("A7F23944"), None, COMMIT));
        assert!(!ok(Some("a7f23944%"), None, COMMIT));
        assert!(ok(Some("a7f2394481a6"), None, COMMIT));
        assert!(!ok(Some("a7f2-394"), None, COMMIT));
        assert!(!ok(None, Some(Uuid::nil()), COMMIT));
        assert!(!ok(None, session, "0123"));
        assert!(!ok(None, session, &COMMIT.to_uppercase()));
        assert!(
            serde_json::from_value::<AgentCheckLandingCandidateRequestV1>(
                json!({"source_commit": COMMIT, "source_session_id": Uuid::new_v4(), "extra": 1})
            )
            .is_err()
        );
    }

    #[test]
    fn long_failure_lists_are_bounded_with_exact_totals() {
        let reds: Vec<Value> = (0..CANDIDATE_FAILURES_MAX + 7)
            .map(|index| json!({"package": "rsid", "kind": "lib", "target": "rsid", "test": format!("t{index}")}))
            .collect();
        let mut receipt = green_receipt(COMMIT);
        receipt["new_failures"] = json!([]);
        receipt["baseline_reds"] = json!(reds);
        let job = job(JobState::Failed, Some(1), Some(with_receipt(receipt)));
        let result = assess_candidate(&job, COMMIT);
        assert_eq!(result.baseline_reds.len(), CANDIDATE_FAILURES_MAX);
        assert_eq!(
            result.baseline_reds_total as usize,
            CANDIDATE_FAILURES_MAX + 7
        );
    }

    fn conflicting_receipt(head: &str) -> Value {
        let mut receipt = green_receipt(head);
        receipt["merges_cleanly_onto_rolling"] = json!(false);
        receipt["conflict_paths"] = json!(["crates/a.rs", "docs/b.md"]);
        receipt
    }

    #[test]
    fn a_signal_interruption_carries_its_class_and_stays_red_516() {
        for new_failures in [Value::Null, json!([])] {
            let mut receipt = green_receipt(COMMIT);
            receipt["ok"] = json!(false);
            receipt["new_failures"] = new_failures;
            receipt["failure_class"] = json!("infrastructure_interrupted");
            let job = job(JobState::Failed, Some(1), Some(with_receipt(receipt)));
            let result = assess_candidate(&job, COMMIT);
            assert!(!result.ok);
            assert_eq!(
                result.failure_class.as_deref(),
                Some("infrastructure_interrupted")
            );
            assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_JOB_NOT_GREEN));
            let wire = serde_json::to_value(&result).unwrap();
            assert_eq!(wire["failure_class"], "infrastructure_interrupted");
        }
    }

    #[test]
    fn a_receipt_without_the_merge_fields_still_parses_and_verifies() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(COMMIT))),
        );
        let result = assess_candidate(&job, COMMIT);
        assert!(result.ok, "{result:?}");
        assert_eq!(result.merges_cleanly_onto_rolling, None);
        assert!(result.conflict_paths.is_empty());
        let wire = serde_json::to_value(&result).unwrap();
        assert!(wire.get("merges_cleanly_onto_rolling").is_none());
        let old: AgentCheckLandingCandidateResultV1 = serde_json::from_value(wire).unwrap();
        assert_eq!(old, result);
    }

    #[test]
    fn a_receipt_that_merges_cleanly_stays_ok() {
        let mut receipt = green_receipt(COMMIT);
        receipt["merges_cleanly_onto_rolling"] = json!(true);
        receipt["conflict_paths"] = json!([]);
        let job = job(JobState::Succeeded, Some(0), Some(with_receipt(receipt)));
        let result = assess_candidate(&job, COMMIT);
        assert!(result.ok, "{result:?}");
        assert_eq!(result.merges_cleanly_onto_rolling, Some(true));
    }

    #[test]
    fn a_receipt_that_conflicts_is_refused_naming_the_paths() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(conflicting_receipt(COMMIT))),
        );
        let result = assess_candidate(&job, COMMIT);
        assert!(!result.ok);
        assert_eq!(
            result.refusal.as_deref(),
            Some(CANDIDATE_CONFLICTS_WITH_ROLLING)
        );
        assert_eq!(result.merges_cleanly_onto_rolling, Some(false));
        assert_eq!(result.conflict_paths, ["crates/a.rs", "docs/b.md"]);
        let detail = result.detail.unwrap();
        assert!(detail.contains("crates/a.rs, docs/b.md"), "{detail}");
        assert!(
            detail.contains("merge origin/rolling (no rebase)"),
            "{detail}"
        );
    }

    #[test]
    fn a_stale_receipt_conflict_does_not_block_a_different_head() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(conflicting_receipt(OTHER))),
        );
        let result = assess_candidate(&job, COMMIT);
        assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_HEAD_MISMATCH));
    }

    #[test]
    fn a_fresh_conflict_refuses_a_verified_candidate() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(COMMIT))),
        );
        let mut result = assess_candidate(&job, COMMIT);
        result.apply_rolling_merge(&RollingMergeProbe::Conflict(vec!["x.rs".into()]));
        assert!(!result.ok);
        assert_eq!(
            result.refusal.as_deref(),
            Some(CANDIDATE_CONFLICTS_WITH_ROLLING)
        );
        assert_eq!(result.merges_cleanly_onto_rolling_now, Some(false));
        assert_eq!(result.conflict_paths_now, ["x.rs"]);
        assert!(result.detail.unwrap().contains("x.rs"));
    }

    #[test]
    fn a_fresh_clean_probe_lifts_a_stale_receipt_conflict() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(conflicting_receipt(COMMIT))),
        );
        let mut result = assess_candidate(&job, COMMIT);
        assert!(!result.ok);
        result.apply_rolling_merge(&RollingMergeProbe::Clean);
        assert!(result.ok, "{result:?}");
        assert_eq!(result.refusal, None);
        assert_eq!(result.merges_cleanly_onto_rolling_now, Some(true));
        assert_eq!(result.merges_cleanly_onto_rolling, Some(false));
    }

    #[test]
    fn a_fresh_conflict_never_hides_an_earlier_blocker() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(OTHER))),
        );
        let mut result = assess_candidate(&job, COMMIT);
        result.apply_rolling_merge(&RollingMergeProbe::Conflict(vec!["x.rs".into()]));
        assert_eq!(result.refusal.as_deref(), Some(CANDIDATE_HEAD_MISMATCH));
        assert_eq!(result.conflict_paths_now, ["x.rs"]);
    }

    #[test]
    fn an_unreadable_probe_leaves_the_verdict_alone() {
        let job = job(
            JobState::Succeeded,
            Some(0),
            Some(with_receipt(green_receipt(COMMIT))),
        );
        let mut result = assess_candidate(&job, COMMIT);
        result.apply_rolling_merge(&RollingMergeProbe::Unknown);
        assert!(result.ok);
        assert_eq!(result.merges_cleanly_onto_rolling_now, None);
    }
}
