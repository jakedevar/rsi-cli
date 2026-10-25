//! `AgentEnqueueLandingSource` (#1007 S1): admit one accepted source onto the
//! daemon-owned rolling merge queue.
//!
//! Authority is the current appointed manager or the current lead of the
//! caller's Epic, resolved from the token-bound caller by the same projection
//! the other Agent verbs advertise from. Workers are refused
//! `queue_not_authorized`; a manager or lead naming a `source_session_id`
//! outside its scope is refused `queue_source_session_out_of_scope`.
//!
//! `AgentListRollingQueue` and `AgentCancelRollingQueueEntry` (#1964) let the
//! same seats see and withdraw their own entries. The verb gate is the
//! advertised projection; which entries a caller reaches is decided in the
//! store, in the transaction that acts on them, so it follows the manager seat
//! across a rotation. Cancel only moves `queued -> superseded`: the lander
//! stays the single publisher of an admitted or gating entry.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use crate::rolling_queue::DeltaRead;
use crate::store::Store;
use crate::store::rolling_queue::{ExpediteAdmission, NewQueueEntry, expedite_cap_detail};
use crate::topology::land::{LandAcceptance, LandEnqueue, LandRequest, LandStatus};
use crate::topology::review_policy::{self, Candidate, ReviewPolicy};
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::rolling_queue::{
    AgentCancelRollingQueueEntryReceiptV1, AgentCancelRollingQueueEntryRequestV1,
    AgentEnqueueLandingSourceReceiptV1, AgentEnqueueLandingSourceRequestV1,
    AgentListRollingQueueReceiptV1, AgentListRollingQueueRequestV1,
    EnqueueRollingQueueSourceParams, EnqueueRollingQueueSourceResponse, LandSourceStatusV1,
    PROVISIONAL_PROBE_INCONCLUSIVE, PROVISIONAL_UNPROVABLE, QUEUE_DISABLED, QUEUE_ENTRY_IN_FLIGHT,
    QUEUE_ENTRY_SETTLED, QUEUE_EXPEDITE_NOT_AUTHORIZED, QUEUE_EXPEDITE_QA_LABEL,
    QUEUE_FILTER_MATCHES_NO_TESTS, QUEUE_FIXES_ISSUE_LISTS_NO_TESTS, QUEUE_FIXES_ISSUE_NOT_FOUND,
    QUEUE_KEY_INVALID, QUEUE_MULTIPLE_PROVISIONAL_MIGRATIONS, QUEUE_NOT_AUTHORIZED,
    QUEUE_PROBE_TIMEOUT, QUEUE_SOURCE_ALREADY_ON_ROLLING, QUEUE_SOURCE_INVALID,
    QUEUE_SOURCE_SESSION_OUT_OF_SCOPE, QUEUE_SUPERSEDES_NOT_DESCENDANT, QUEUE_TIP_READ_TIMEOUT,
    QueueFiltersState, RollingQueueBinding, RollingQueueEntryState, check_fix_claims_cap,
};
use rsi_common::types::Session;
use std::path::PathBuf;
use uuid::Uuid;

/// The admission shared by the first resolution and the effect-time recheck:
/// the session whose sandbox is queued, whether the caller is its Epic lead,
/// and the authority fence.
fn admit(
    store: &Store,
    caller: Uuid,
    request: &AgentEnqueueLandingSourceRequestV1,
) -> Result<(Session, bool, String)> {
    let source_session_id = request.source_session_id.filter(|source| *source != caller);
    let (session, is_lead) = match source_session_id {
        // #1235: land from an in-reach session's sandbox. Only a caller that
        // holds enqueue authority learns the source is out of its scope; any
        // other caller keeps the opaque refusal.
        Some(source) => {
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
            if !(projection.is_lead || projection.is_manager || projection.is_global_manager) {
                return Err(DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()));
            }
            let out_of_scope = DaemonError::PolicyDenied(format!(
                "{QUEUE_SOURCE_SESSION_OUT_OF_SCOPE}: source_session_id is outside your scope; omit it to land your own sandbox or name a session in your scope"
            ));
            let session = super::agent_jobs_verb::manager_reached_sandbox(
                store,
                caller,
                source,
                request.project_id,
                QUEUE_NOT_AUTHORIZED,
                QUEUE_SOURCE_SESSION_OUT_OF_SCOPE,
            )
            .map_err(|error| match error {
                DaemonError::PolicyDenied(code) if code == QUEUE_SOURCE_SESSION_OUT_OF_SCOPE => {
                    out_of_scope
                }
                other => other,
            })?;
            (session, false)
        }
        None => {
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
            if !(projection.is_lead || projection.is_manager) {
                return Err(DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()));
            }
            let session = store
                .get_session(caller)?
                .ok_or_else(|| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
            if request
                .project_id
                .is_some_and(|project| session.project_id != Some(project))
            {
                return Err(DaemonError::InvalidParam(
                    rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE.into(),
                ));
            }
            (session, projection.is_lead)
        }
    };
    let fence = store
        .agent_authority_fence(caller, source_session_id)
        .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
    Ok((session, is_lead, fence))
}

/// One source to admit onto the queue, resolved by the caller's admission.
pub(crate) struct QueueSource {
    pub(crate) issue: Option<i64>,
    pub(crate) supersedes: Option<(Uuid, String)>,
    pub(crate) project_id: Option<Uuid>,
    /// The checkout the lander fetches the source from.
    pub(crate) repo: PathBuf,
    /// The entry's identity with `idempotency_key` (the replay key).
    pub(crate) source_session_id: Uuid,
    pub(crate) owner_epic_id: Option<Uuid>,
    pub(crate) commit: String,
    pub(crate) test_filters: Vec<String>,
    /// #2112: the caller named `test_filters` itself (operator or agent) rather
    /// than letting the queue derive them. Only such filters are exempt from
    /// the async full-library QA obligation a derived-filter publish owes.
    pub(crate) explicit_filters: bool,
    pub(crate) handwritten_filters: Vec<String>,
    /// #2167: the tests the landing claims to fix; its gate must run each one
    /// green on the candidate.
    pub(crate) fixes_tests: Vec<String>,
    /// #2993: the filter derivation was still running when the entry was
    /// admitted; `test_filters` is empty and the entry is recorded with its
    /// filters pending.
    pub(crate) filters_pending: bool,
    pub(crate) idempotency_key: String,
    /// Where the settlement wake goes when it is not the source session.
    pub(crate) wake_session_id: Option<Uuid>,
    /// #3487: admit the entry ahead of ordinary queued entries.
    pub(crate) expedite: Option<ExpediteAdmission>,
}

/// The git probe shared by every enqueue path: the source must exist in
/// `repo`, and no filter may provably select no test. Runs without the store
/// lock.
///
/// #2912: `deadline` bounds every git read of the probe: each is killed when it
/// passes, and a probe it cut short is refused `queue_probe_timeout` (never
/// `queue_source_invalid`: a read that did not finish proves nothing about the
/// source). `None` waits for git, the operator path.
///
/// # Errors
/// `queue_source_invalid`, `queue_multiple_provisional_migrations`,
/// `provisional_unprovable`, `provisional_probe_inconclusive` (retryable),
/// `queue_probe_timeout`, or `filter_matches_no_tests: <filter>`.
pub(crate) async fn probe_source(
    repo: &std::path::Path,
    commit: &str,
    filters: &[String],
    deadline: Option<std::time::Instant>,
) -> Result<crate::rolling_queue::SourceFacts> {
    let (probe_repo, source, filters) = (repo.to_path_buf(), commit.to_owned(), filters.to_vec());
    let (outcome, timed_out) = tokio::task::spawn_blocking(move || {
        let read = || {
            if !crate::rolling_queue::source_commit_exists(&probe_repo, &source) {
                return Err(DaemonError::InvalidParam(QUEUE_SOURCE_INVALID.into()));
            }
            let declarations =
                crate::rolling_queue::source_provisional_declaration_count(&probe_repo, &source);
            if declarations.is_some_and(|count| count > 1) {
                return Err(DaemonError::InvalidParam(format!(
                    "{QUEUE_MULTIPLE_PROVISIONAL_MIGRATIONS}: drop the extra provisional declarations or split the migrations across separate landings"
                )));
            }
            // #3658: every cheap git read comes BEFORE the renumber proof.
            // The proof clones and runs the guard (seconds under load); it
            // used to run first, spend the call's whole wall bound, and
            // leave the reads below to be killed at an expired deadline
            // (`queue_probe_timeout` for every source with a provisional
            // declaration, on every retry).
            let facts = crate::rolling_queue::derive_source_facts(&probe_repo, &source);
            if let Some(filter) =
                crate::rolling_queue::filter_selecting_no_tests(&probe_repo, &source, &filters)
            {
                return Err(DaemonError::InvalidParam(format!(
                    "{QUEUE_FILTER_MATCHES_NO_TESTS}: {filter}"
                )));
            }
            // A failed count is not proof that the source has no declaration.
            if declarations == Some(0) {
                return Ok(facts);
            }
            // #3545: a proof that could not run (no guard, bad exit,
            // timeout, malformed output) is not a pass: it refuses retryably and
            // no entry is recorded. No trusted guard is Inconclusive too (#465).
            let refusal = crate::rolling_queue::provisional_source_refusal(
                &probe_repo,
                &source,
                deadline.unwrap_or_else(|| {
                    std::time::Instant::now() + std::time::Duration::from_secs(60)
                }),
            );
            use crate::rolling_queue::ProvisionalSourceRefusal;
            match refusal {
                None => Ok(facts),
                Some(ProvisionalSourceRefusal::Unprovable(reason)) => {
                    Err(DaemonError::InvalidParam(format!(
                        "{PROVISIONAL_UNPROVABLE}: {reason}; correct the provisional declaration or its undeclared sites and enqueue the corrected commit"
                    )))
                }
                Some(ProvisionalSourceRefusal::Inconclusive(cause)) => {
                    Err(DaemonError::InvalidParam(format!(
                        "{PROVISIONAL_PROBE_INCONCLUSIVE}: {cause}; infrastructure prevented validation; no entry was recorded; restore the named tool/ref or retry after the timeout with the same idempotency_key"
                    )))
                }
            }
        };
        match deadline {
            Some(deadline) => crate::rolling_queue::with_git_deadline(deadline, read),
            None => (read(), false),
        }
    })
    .await
    .map_err(|error| DaemonError::Process(format!("enqueue source probe: {error}")))?;
    if timed_out {
        return Err(DaemonError::InvalidParam(format!(
            "{QUEUE_PROBE_TIMEOUT}: a git read of the source probe did not finish within the call's wall bound and was killed; no entry was recorded. Call again with the same idempotency_key; if it recurs, check host load and git health (a stalled repository read), since a slow provisional-migration proof alone no longer causes this"
        )));
    }
    outcome
}

/// What [`derive_and_probe`] resolved (#2993).
enum Derived {
    /// The filters (derived or explicit) and the source probe made with them.
    Ready(Vec<String>, crate::rolling_queue::SourceFacts),
    /// The derivation was still running at `derive_wait`. The source was
    /// probed with the extra filters alone (#3264); the entry is recorded with
    /// them and its derived filters pending, and the derivation finishes it in
    /// the background.
    Deferred(Vec<String>, crate::rolling_queue::SourceFacts),
    /// #3372: the derivation exceeded its budget (it timed out, or the diff is
    /// too wide to trace in time). The source was probed with the extra filters
    /// alone and the entry is recorded with them and `filters_state:
    /// lander_selects`: the lander selects the tests itself (its bounded
    /// closure selection plus the automatic full-library QA). Not a refusal.
    LanderSelects(Vec<String>, crate::rolling_queue::SourceFacts),
}

/// The filters and probe shared by the agent and operator enqueue paths:
/// derive the landing filters from the source's diff (or take the explicit
/// ones), then probe the source with them. Runs without the store lock.
///
/// #3264: with `derive_filters`, `explicit` are extra filters: the landing gates
/// the derived set plus them. A derivation that FAILED falls back to the
/// unfiltered gate plus them.
///
/// #2907: `derive_wait` bounds the derivation (`None` waits for it, the
/// operator path). #2993: a derivation still running at the bound is not a
/// refusal; the result is [`Derived::Deferred`] and the derivation keeps
/// running for the entry the caller then records. A derivation that FAILED
/// inline falls back to the unfiltered gate, as it always did. A rolling-tip
/// read that did not finish is refused `queue_tip_read_timeout` (a git stall,
/// not a derivation in progress). `probe_deadline` (#2912) is the call's wall
/// deadline for the source probe that follows: what the derivation left of it,
/// `queue_probe_timeout` past it.
async fn derive_and_probe(
    repo: &std::path::Path,
    commit: &str,
    derive_filters: bool,
    explicit: Vec<String>,
    derive_wait: Option<std::time::Duration>,
    probe_deadline: Option<std::time::Instant>,
) -> Result<Derived> {
    use crate::rolling_queue::SharedDerivation;
    let started = std::time::Instant::now();
    let mut deferred = false;
    let mut over_budget = false;
    let test_filters = if derive_filters {
        match crate::rolling_queue::derive_test_filters_shared(repo, commit, derive_wait).await {
            SharedDerivation::Ready(filters) => {
                rsi_common::rolling_queue::merge_filters(filters, &explicit)
            }
            SharedDerivation::Failed(reason) => {
                tracing::warn!(%reason, "enqueue: filter derivation failed; gating unfiltered");
                explicit
            }
            SharedDerivation::OverBudget(reason) => {
                tracing::warn!(%reason, "enqueue: filter derivation over budget; the lander selects the tests");
                over_budget = true;
                explicit
            }
            SharedDerivation::Running => {
                deferred = true;
                explicit
            }
            SharedDerivation::TipUnreadable => {
                return Err(DaemonError::InvalidParam(format!(
                    "{QUEUE_TIP_READ_TIMEOUT}: the rolling-tip read behind the filter derivation did not finish within {} s and was killed (a slow or stalled git); no entry was recorded, so call again with the same idempotency_key",
                    derive_wait.map_or(0, |wait| wait.as_secs())
                )));
            }
        }
    } else {
        explicit
    };
    let derived = started.elapsed();
    let facts = probe_source(repo, commit, &test_filters, probe_deadline).await?;
    tracing::info!(
        derive_ms = u64::try_from(derived.as_millis()).unwrap_or(u64::MAX),
        probe_ms = u64::try_from((started.elapsed() - derived).as_millis()).unwrap_or(u64::MAX),
        deferred,
        "enqueue phase: derive_and_probe"
    );
    Ok(if deferred {
        Derived::Deferred(test_filters, facts)
    } else if over_budget {
        Derived::LanderSelects(test_filters, facts)
    } else {
        Derived::Ready(test_filters, facts)
    })
}

/// [`derive_and_probe`] with no wait bound, the operator path: it always
/// ends with filters.
async fn derive_and_probe_to_end(
    repo: &std::path::Path,
    commit: &str,
    derive_filters: bool,
    explicit: Vec<String>,
) -> Result<(Vec<String>, crate::rolling_queue::SourceFacts)> {
    match derive_and_probe(repo, commit, derive_filters, explicit, None, None).await? {
        Derived::Ready(filters, facts) => Ok((filters, facts)),
        // The operator path has no pending state: an over-budget derivation is
        // the unfiltered gate it always fell back to.
        Derived::LanderSelects(filters, facts) => Ok((filters, facts)),
        Derived::Deferred(..) => Err(DaemonError::Process(
            "filter derivation deferred without a wait bound".into(),
        )),
    }
}

/// The insert shared by every enqueue path, under the caller's re-resolved
/// admission (the store lock stays held from the re-check through here).
pub(crate) fn insert_source(
    store: &Store,
    source: &QueueSource,
    facts: &crate::rolling_queue::SourceFacts,
) -> Result<(rsi_common::rolling_queue::RollingQueueEntryV1, bool)> {
    let new = NewQueueEntry {
        project_id: source.project_id,
        repo_path: source.repo.display().to_string(),
        source_commit: source.commit.clone(),
        source_session_id: source.source_session_id,
        owner_epic_id: source.owner_epic_id,
        binding: RollingQueueBinding::Unbound,
        work_key: None,
        migration_version: facts.migration_version,
        hot_files: facts.hot_files.clone(),
        test_filters: source.test_filters.clone(),
        fixes_tests: source.fixes_tests.clone(),
        idempotency_key: source.idempotency_key.clone(),
    };
    let supersedes = source
        .supersedes
        .as_ref()
        .map(|(id, commit)| (*id, commit.as_str()));
    let derived: Vec<String> = source
        .test_filters
        .iter()
        .filter(|filter| !source.explicit_filters && !source.handwritten_filters.contains(filter))
        .cloned()
        .collect();
    let (entry, replayed) = store.enqueue_rolling_queue_source_expediting(
        &new,
        source.wake_session_id,
        supersedes,
        source.filters_pending,
        source.issue,
        &derived,
        source.expedite.as_ref(),
        chrono::Utc::now(),
    )?;
    // A replay re-marks a live entry (after a restart too); a settled one
    // has already been judged.
    if source.explicit_filters && !entry.state.is_terminal() {
        crate::rolling_queue::note_explicit_filters(entry.id);
    }
    Ok((entry, replayed))
}

/// What [`resolve_fix_claims`] resolved (#2167, #3264).
struct FixClaims {
    /// The tests the landing claims to fix.
    tests: Vec<String>,
    /// The filters that select the `fixes_issue` Issue's tests (from its
    /// `Shard command:` lines): added to the landing's filters so the gate runs
    /// the claim instead of refusing it `claimed_fix_still_red`.
    selecting: Vec<String>,
    /// #3487: the `fixes_issue` Issue is open and labelled `qa`: the landing is
    /// expedited automatically. The reason names the Issue.
    qa_expedite_reason: Option<String>,
}

/// #3487: the recorded reason of the automatic expedite for `issue`, when it is
/// open and labelled `qa`: the Issue number and a bounded copy of its title.
fn qa_expedite_reason(issue: &rsi_common::types::Issue) -> Option<String> {
    use rsi_common::types::IssueStatus;
    let open = matches!(issue.status, IssueStatus::Open | IssueStatus::InProgress);
    let qa = issue
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(QUEUE_EXPEDITE_QA_LABEL));
    if !(open && qa) {
        return None;
    }
    let title: String = issue
        .title
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    Some(format!(
        "automatic: fixes open qa Issue #{}: {}",
        issue.display_number,
        title.trim()
    ))
}

/// #2167: the tests a landing claims to fix: the explicit `fixes_tests`, then
/// each test the `fixes_issue` Issue (of the source's project) lists on its
/// `Failing tests:` lines (the qa Issue format). #3264: with the claims, the
/// filters its `Shard command:` lines name, which select those tests.
///
/// # Errors
/// `queue_fixes_issue_not_found`, `queue_fixes_issue_lists_no_failing_tests` or
/// `queue_fixes_too_many` (the combined claims exceed the 64-test cap).
fn resolve_fix_claims(
    store: &Store,
    project_id: Option<Uuid>,
    request: &AgentEnqueueLandingSourceRequestV1,
) -> Result<FixClaims> {
    let Some(number) = request.fixes_issue else {
        // Already capped by `validate`; checked here too so both paths share it.
        check_fix_claims_cap(&request.fixes_tests).map_err(DaemonError::InvalidParam)?;
        return Ok(FixClaims {
            tests: request.fixes_tests.clone(),
            selecting: Vec::new(),
            qa_expedite_reason: None,
        });
    };
    let issue = match project_id {
        Some(project) => store.get_issue_by_display_number(project, number)?,
        None => None,
    }
    .ok_or_else(|| {
        DaemonError::InvalidParam(format!(
            "{QUEUE_FIXES_ISSUE_NOT_FOUND}: #{number} is not an Issue of the source's project"
        ))
    })?;
    let listed = rsi_common::rolling_queue::failing_tests_listed(&issue.body);
    if listed.is_empty() {
        return Err(DaemonError::InvalidParam(format!(
            "{QUEUE_FIXES_ISSUE_LISTS_NO_TESTS}: #{number} has no `Failing tests:` line naming a test; pass the names as fixes_tests"
        )));
    }
    let claims = rsi_common::rolling_queue::union_fix_claims(&request.fixes_tests, &listed);
    // #2175: the Issue body is operator-sized text; cap the combined claims.
    check_fix_claims_cap(&claims).map_err(DaemonError::InvalidParam)?;
    Ok(FixClaims {
        tests: claims,
        selecting: rsi_common::rolling_queue::issue_selecting_filters(&issue.body),
        qa_expedite_reason: qa_expedite_reason(&issue),
    })
}

/// #2118: the conflict warnings for `source` against the rolling tip and the
/// live entries of `project_id` ahead of it. Best effort: a store or git
/// failure yields none. The store lock is released before git runs, and the
/// scan is bounded by `budget` (#2907).
async fn conflict_warnings_for(
    store: &rsid_core::store_lock::Mutex<Store>,
    repo: &std::path::Path,
    project_id: Option<Uuid>,
    source: &str,
    budget: std::time::Duration,
) -> Vec<rsi_common::rolling_queue::QueueConflictWarningV1> {
    let ahead = {
        let store = store.lock().await;
        store
            .live_rolling_queue_sources(project_id, 32)
            .unwrap_or_default()
    };
    let (repo, source) = (repo.to_path_buf(), source.to_owned());
    tokio::task::spawn_blocking(move || {
        crate::rolling_queue::queue_conflict_warnings_within(&repo, &source, &ahead, budget)
    })
    .await
    .unwrap_or_default()
}

/// Where an operator-enqueued landing's outcome goes: the session's project's
/// current manager seat, when there is one and it is not the source itself.
fn settlement_wake_target(store: &Store, session: &Session) -> Result<Option<Uuid>> {
    let manager = match session.project_id {
        Some(project) => store
            .get_harness_manager(project)?
            .and_then(|config| config.current_session_id),
        None => None,
    };
    Ok(manager.filter(|manager| *manager != session.id))
}

/// The refusal text of the queue list and cancel gate (#2461): the stable code
/// first, then how a worker learns what became of its landing.
pub(crate) fn queue_verb_refusal() -> DaemonError {
    DaemonError::PolicyDenied(format!(
        "{QUEUE_NOT_AUTHORIZED}: only the appointed manager or an Epic lead lists or cancels queue entries; a worker hands its commit to its manager (who enqueues it and is woken with the outcome) and checks `git merge-base --is-ancestor <SHA> origin/rolling`"
    ))
}

/// The verb gate of the queue list and cancel (#1964): exactly the roles whose
/// catalog advertises `verb`. A caller the projection cannot resolve (a
/// retired or terminal session) holds nothing.
fn queue_verb_gate(store: &Store, caller: Uuid, verb: AgentControlVerbV1) -> Result<()> {
    let projection = store
        .agent_authority_projection(caller)
        .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
    if projection.verbs.contains(&verb) {
        Ok(())
    } else {
        Err(queue_verb_refusal())
    }
}

impl AgentControlHandle {
    /// `AgentListRollingQueue` (#1964): the caller's scope of the queue,
    /// newest first.
    ///
    /// # Errors
    /// `queue_invalid_request`, `queue_not_authorized`, or
    /// `manager_project_not_in_scope`.
    pub async fn agent_list_rolling_queue(
        &self,
        caller: Uuid,
        request: AgentListRollingQueueRequestV1,
    ) -> Result<AgentListRollingQueueReceiptV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let store = self.store.lock().await;
        queue_verb_gate(&store, caller, AgentControlVerbV1::ListRollingQueue)?;
        let (entries, next_before_sequence) =
            store.list_rolling_queue_for_agent(caller, &request)?;
        let qa_green = store.rolling_qa_green_for_agent(caller, request.project_id)?;
        Ok(AgentListRollingQueueReceiptV1 {
            entries,
            next_before_sequence,
            qa_green,
            held_for_deploy: self
                .deploy_drain
                .as_ref()
                .and_then(|drain| drain.queue_deploy_hold()),
        })
    }

    /// `AgentCancelRollingQueueEntry` (#1964): withdraw one `queued` entry.
    /// Works while the queue is switched off: a withdrawal never publishes.
    ///
    /// # Errors
    /// `queue_invalid_request`, `queue_reason_invalid`, `queue_not_authorized`,
    /// `queue_entry_not_found`, `queue_entry_in_flight`, `queue_entry_settled`.
    pub async fn agent_cancel_rolling_queue_entry(
        &self,
        caller: Uuid,
        request: AgentCancelRollingQueueEntryRequestV1,
    ) -> Result<AgentCancelRollingQueueEntryReceiptV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let store = self.store.lock().await;
        queue_verb_gate(&store, caller, AgentControlVerbV1::CancelRollingQueueEntry)?;
        let (entry, replayed) = store.cancel_rolling_queue_entry_for_agent(
            caller,
            request.entry_id,
            request.reason.as_deref(),
            chrono::Utc::now(),
        )?;
        Ok(AgentCancelRollingQueueEntryReceiptV1 { entry, replayed })
    }

    /// # Errors
    /// A stable `queue_*` refusal, or a persistence error.
    ///
    /// #2993: one call is enough. A filter derivation still running at the
    /// call's budget is not a refusal: the entry is recorded `queued` with
    /// `filters_state: deriving` and returned; the lander admits it once its
    /// filters land, and a failed derivation settles it `refused` with the
    /// enqueuer's single outcome wake.
    pub async fn agent_enqueue_landing_source(
        &self,
        caller: Uuid,
        request: AgentEnqueueLandingSourceRequestV1,
        queue_enabled: bool,
    ) -> Result<AgentEnqueueLandingSourceReceiptV1> {
        self.agent_enqueue_landing_source_within(
            caller,
            request,
            queue_enabled,
            crate::rolling_queue::ENQUEUE_DERIVE_WAIT,
        )
        .await
    }

    /// [`Self::agent_enqueue_landing_source`] with the inline wait for the
    /// filter derivation as a parameter, so a test need not wait out the real
    /// budget. A derivation still running at `derive_wait` records the entry
    /// with its filters pending (#2993).
    pub(crate) async fn agent_enqueue_landing_source_within(
        &self,
        caller: Uuid,
        request: AgentEnqueueLandingSourceRequestV1,
        queue_enabled: bool,
        derive_wait: std::time::Duration,
    ) -> Result<AgentEnqueueLandingSourceReceiptV1> {
        let call_started = std::time::Instant::now();
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(request.refusal_detail(code)))?;
        let (session, is_lead, fence) = {
            let store = self.store.lock().await;
            let admitted = admit(&store, caller, &request)?;
            // #3487: only the current appointed manager expedites; an Epic
            // lead enqueues in order and its manager expedites if it must.
            if request.expedite.is_some() {
                let projection = store
                    .agent_authority_projection(caller)
                    .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
                if !(projection.is_manager || projection.is_global_manager) {
                    return Err(DaemonError::PolicyDenied(format!(
                        "{QUEUE_EXPEDITE_NOT_AUTHORIZED}: only the current appointed manager may expedite a landing; enqueue without expedite and ask your manager to expedite it if it cannot wait"
                    )));
                }
            }
            admitted
        };
        if !queue_enabled {
            return Err(DaemonError::InvalidParam(QUEUE_DISABLED.into()));
        }
        // #2167: a fix claim names its tests directly or through the red
        // Issue that lists them.
        let FixClaims {
            tests: fixes_tests,
            selecting: selecting_filters,
            qa_expedite_reason,
        } = {
            let store = self.store.lock().await;
            resolve_fix_claims(&store, session.project_id, &request)?
        };
        // #3264: the request's own filters, then the ones that select the
        // claimed tests; with `derive_filters` they join the derived set.
        let extra_filters = rsi_common::rolling_queue::merge_filters(
            request.test_filters.clone(),
            &selecting_filters,
        );
        let repo: PathBuf = session
            .sandbox_root
            .clone()
            .unwrap_or_else(|| session.working_dir.clone());
        // #2993/#3002: a replay of a call (same key, source commit and fix
        // claims) answers with its entry whatever the entry's state: filters
        // pending, settled (rolling may have advanced, so derivation would now
        // answer differently) or terminal. The derivation is not asked again;
        // the entry is the answer to the call. A different commit or claim
        // falls through to the insert, which refuses it as a conflict.
        if request.derive_filters {
            let existing = {
                let store = self.store.lock().await;
                store
                    .get_rolling_queue_entry_by_key(caller, &request.idempotency_key)?
                    .filter(|entry| {
                        entry.source_commit == request.source_commit
                            && entry.fixes_tests == fixes_tests
                            && entry.issue == request.issue
                    })
            };
            if let Some(entry) = existing {
                if entry.filters_state == Some(QueueFiltersState::Deriving) {
                    // Idempotent: a no-op while a task already finishes it.
                    crate::rolling_queue::finish_filter_derivation_in_background(
                        &self.store,
                        entry.id,
                        repo.clone(),
                        entry.source_commit.clone(),
                    );
                }
                return Ok(AgentEnqueueLandingSourceReceiptV1 {
                    entry,
                    replayed: true,
                    conflict_warnings: Vec::new(),
                    expedite_warning: None,
                });
            }
        }
        let supersedes = {
            let store = self.store.lock().await;
            // A completed replacement replays by key even though its old entry
            // is now terminal. The transactional insert checks the replay bytes.
            if store
                .get_rolling_queue_entry_by_key(caller, &request.idempotency_key)?
                .is_some()
            {
                None
            } else if let Some(id) = request.supersedes {
                let old = store
                    .get_rolling_queue_entry_for_supersession(caller, id)?
                    .ok_or_else(|| store.queue_entry_not_found_refusal(caller, id))?;
                match old.state {
                    RollingQueueEntryState::Queued => {}
                    RollingQueueEntryState::Admitted | RollingQueueEntryState::Gating => {
                        return Err(DaemonError::InvalidParam(format!(
                            "{QUEUE_ENTRY_IN_FLIGHT}: enqueue normally without supersedes; the lander owns this entry"
                        )));
                    }
                    // #648: the one settled state a descendant may supersede; the
                    // store re-checks the window and a prior takeover transactionally.
                    _ if rsi_common::rolling_queue::resumable_conflict_refusal(
                        &old,
                        chrono::Utc::now(),
                    ) => {}
                    _ => return Err(DaemonError::InvalidParam(QUEUE_ENTRY_SETTLED.into())),
                }
                Some((id, old.source_commit))
            } else {
                None
            }
        };
        if let Some((_, old_commit)) = &supersedes {
            let (probe_repo, old, new) = (
                repo.clone(),
                old_commit.clone(),
                request.source_commit.clone(),
            );
            let descendant = tokio::task::spawn_blocking(move || {
                crate::rolling_queue::source_descends_from(&probe_repo, &old, &new)
            })
            .await
            .map_err(|error| DaemonError::Process(format!("supersession source probe: {error}")))?;
            if !descendant {
                return Err(DaemonError::InvalidParam(
                    QUEUE_SUPERSEDES_NOT_DESCENDANT.into(),
                ));
            }
        }
        // #2907: the filter derivation and the conflict scan are independent,
        // so they run side by side, each under its own wall bound: the call
        // takes the longer of the two, never the sum, and neither can hold it
        // past its bound. The conflict scan (#2118) predicts the batch merge
        // conflicts the queue would otherwise find only after a gate; it is a
        // warning, never a refusal.
        let phases = std::time::Instant::now();
        let (derived, conflict_warnings) = tokio::join!(
            derive_and_probe(
                &repo,
                &request.source_commit,
                request.derive_filters,
                extra_filters.clone(),
                Some(derive_wait),
                Some(phases + crate::rolling_queue::ENQUEUE_WALL_BOUND),
            ),
            conflict_warnings_for(
                &self.store,
                &repo,
                session.project_id,
                &request.source_commit,
                crate::rolling_queue::ENQUEUE_CONFLICT_SCAN_BUDGET,
            )
        );
        let (test_filters, facts, filters_pending, lander_selects) = match derived? {
            Derived::Ready(filters, facts) => (filters, facts, false, false),
            Derived::Deferred(extra, facts) => (extra, facts, true, false),
            Derived::LanderSelects(extra, facts) => (extra, facts, false, true),
        };
        let probes_ms = u64::try_from(phases.elapsed().as_millis()).unwrap_or(u64::MAX);
        #[cfg(test)]
        super::effect_fence::seam::run(&self.store, caller, "enqueue").await;
        // #1277: the lock was released for the Git probe. Re-resolve the same
        // admission under the lock that stays held through the insert and
        // refuse if caller, grant, project, reach or custody changed.
        let store = self.store.lock().await;
        let (current, _, current_fence) = admit(&store, caller, &request)?;
        super::effect_fence::require_unchanged(&fence, &current_fence, QUEUE_NOT_AUTHORIZED)?;
        let current_repo = current
            .sandbox_root
            .clone()
            .unwrap_or_else(|| current.working_dir.clone());
        if current_repo != repo {
            return Err(DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()));
        }
        let commit = request.source_commit.clone();
        // #3487: a manager's `expedite` wins; otherwise a fix for an open `qa`
        // Issue expedites on its own, but is never refused for the cap.
        let automatic_wanted = request.expedite.is_none() && qa_expedite_reason.is_some();
        let expedite = match (&request.expedite, qa_expedite_reason) {
            (Some(manual), _) => Some(ExpediteAdmission {
                reason: manual.reason.trim().to_owned(),
                automatic: false,
            }),
            (None, Some(reason)) => Some(ExpediteAdmission {
                reason,
                automatic: true,
            }),
            (None, None) => None,
        };
        let (mut entry, replayed) = insert_source(
            &store,
            &QueueSource {
                issue: request.issue,
                supersedes,
                project_id: session.project_id,
                repo: repo.clone(),
                source_session_id: caller,
                owner_epic_id: is_lead.then_some(session.parent_id).flatten(),
                commit: request.source_commit,
                test_filters,
                explicit_filters: !request.derive_filters,
                handwritten_filters: extra_filters,
                fixes_tests,
                filters_pending,
                idempotency_key: request.idempotency_key,
                // #2405: the enqueuing seat owns the single outcome wake,
                // recorded as such so a seat change moves it like other
                // seat-keyed state (#2306). A worker named by
                // `source_session_id` only supplies the sandbox; it is never
                // the target.
                wake_session_id: Some(caller),
                expedite,
            },
            &facts,
        )?;
        // #3487: the automatic expedite met the cap and stayed ordinary.
        let expedite_warning = if automatic_wanted && !replayed && entry.expedite_reason.is_none() {
            let live = store.live_expedited_rolling_queue_entries(session.project_id)?;
            Some(format!(
                "{}; this qa fix was queued as an ordinary entry",
                expedite_cap_detail(&live)
            ))
        } else {
            None
        };
        // #3372: the derivation exceeded its budget. A fresh entry is kept with
        // no derived filters and marked, so the lander selects its tests and
        // the entry says why; a replay keeps the entry as it was recorded.
        if lander_selects
            && !replayed
            && entry.filters_state.is_none()
            && store.mark_rolling_queue_filters_lander_selects(entry.id, false)?
        {
            entry.filters_state = Some(QueueFiltersState::LanderSelects);
        }
        // #2993: the entry is recorded with its filters pending. The
        // derivation finishes it (or settles it refused) in the background, so
        // the caller ends its turn; nobody polls for the filters.
        if entry.filters_state == Some(QueueFiltersState::Deriving) {
            crate::rolling_queue::finish_filter_derivation_in_background(
                &self.store,
                entry.id,
                repo,
                commit,
            );
        }
        tracing::info!(
            entry = %entry.id,
            replayed,
            probes_ms,
            total_ms = u64::try_from(call_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "enqueue phase: total"
        );
        Ok(AgentEnqueueLandingSourceReceiptV1 {
            entry,
            replayed,
            conflict_warnings,
            expedite_warning,
        })
    }

    /// `EnqueueRollingQueueSource` (#2055, operator-only): land a session's
    /// branch through the queue. The operator holds the authority, so there is
    /// no caller admission; everything after the source is resolved is the
    /// agent path's own filter derivation, probe and insert. The settlement
    /// wake goes to the project's current manager seat when there is one, and
    /// to the owning session otherwise.
    ///
    /// # Errors
    /// The stable `queue_*` refusals of the agent path, plus
    /// `queue_source_already_on_rolling`.
    pub async fn operator_enqueue_landing_source(
        &self,
        request: EnqueueRollingQueueSourceParams,
        queue_enabled: bool,
    ) -> Result<EnqueueRollingQueueSourceResponse> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let (session, wake_session_id) = {
            let store = self.store.lock().await;
            let session = match (request.session_id, request.branch.as_deref()) {
                (Some(id), _) => store.get_session(id)?,
                (None, Some(branch)) => store.find_session_by_sandbox_branch(branch)?,
                (None, None) => None,
            }
            .ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "{QUEUE_SOURCE_INVALID}: no session owns that session id or branch"
                ))
            })?;
            let wake = settlement_wake_target(&store, &session)?;
            (session, wake)
        };
        let repo: PathBuf = session
            .sandbox_root
            .clone()
            .unwrap_or_else(|| session.working_dir.clone());
        let (named_commit, branch) = (request.commit.clone(), session.sandbox_branch.clone());
        let (probe_repo, probe_branch) = (repo.clone(), branch.clone());
        let (commit, ahead, on_rolling, dirty) = tokio::task::spawn_blocking(move || {
            let commit = named_commit
                .or_else(|| crate::rolling_queue::head_commit(&probe_repo))
                .or_else(|| {
                    probe_branch.and_then(|b| crate::rolling_queue::branch_commit(&probe_repo, &b))
                })?;
            let (ahead, on_rolling) = crate::rolling_queue::land_position(&probe_repo, &commit);
            let dirty = crate::rolling_queue::worktree_dirty(&probe_repo);
            Some((commit, ahead, on_rolling, dirty))
        })
        .await
        .map_err(|error| DaemonError::Process(format!("operator land probe: {error}")))?
        .ok_or_else(|| DaemonError::InvalidParam(QUEUE_SOURCE_INVALID.into()))?;
        let source = LandSourceStatusV1 {
            session_id: session.id,
            branch,
            commit: commit.clone(),
            ahead,
            on_rolling,
            dirty,
            title: session.title.clone(),
        };
        if request.dry_run {
            return Ok(EnqueueRollingQueueSourceResponse {
                source,
                entry: None,
                replayed: false,
            });
        }
        let Some(idempotency_key) = request.idempotency_key.clone() else {
            return Err(DaemonError::InvalidParam(QUEUE_KEY_INVALID.into()));
        };
        if !queue_enabled {
            return Err(DaemonError::InvalidParam(QUEUE_DISABLED.into()));
        }
        // A replay of an earlier key returns that entry even after the source
        // landed; a new key for a landed source is refused.
        if on_rolling {
            let store = self.store.lock().await;
            if let Some(entry) =
                store.get_rolling_queue_entry_by_key(session.id, &idempotency_key)?
            {
                // #2069: a key replays only the commit it first queued (and the
                // explicit filters, when named), exactly like the agent path.
                if entry.source_commit != source.commit
                    || (!request.derive_filters && entry.test_filters != request.test_filters)
                {
                    return Err(DaemonError::InvalidParam(
                        "queue_idempotency_conflict".into(),
                    ));
                }
                return Ok(EnqueueRollingQueueSourceResponse {
                    source,
                    entry: Some(entry),
                    replayed: true,
                });
            }
            return Err(DaemonError::InvalidParam(
                QUEUE_SOURCE_ALREADY_ON_ROLLING.into(),
            ));
        }
        let (test_filters, facts) = derive_and_probe_to_end(
            &repo,
            &commit,
            request.derive_filters,
            request.test_filters.clone(),
        )
        .await?;
        #[cfg(test)]
        super::effect_fence::seam::run(&self.store, session.id, "operator_enqueue").await;
        let store = self.store.lock().await;
        // The lock was released for the Git probe: refuse if the session's
        // custody moved meanwhile.
        let current = store
            .get_session(session.id)?
            .ok_or_else(|| DaemonError::InvalidParam(QUEUE_SOURCE_INVALID.into()))?;
        let current_repo = current
            .sandbox_root
            .clone()
            .unwrap_or_else(|| current.working_dir.clone());
        if current_repo != repo {
            return Err(DaemonError::InvalidParam(QUEUE_SOURCE_INVALID.into()));
        }
        // #2073: the project (and so the manager seat that hears the outcome)
        // may have moved during the probe; resolve both again under this lock.
        let wake_session_id = if current.project_id == session.project_id {
            wake_session_id
        } else {
            settlement_wake_target(&store, &current)?
        };
        let (entry, replayed) = insert_source(
            &store,
            &QueueSource {
                issue: None,
                supersedes: None,
                project_id: current.project_id,
                repo,
                source_session_id: session.id,
                owner_epic_id: None,
                commit,
                test_filters,
                explicit_filters: !request.derive_filters,
                handwritten_filters: request.test_filters.clone(),
                fixes_tests: Vec::new(),
                filters_pending: false,
                idempotency_key,
                wake_session_id,
                expedite: None,
            },
            &facts,
        )?;
        Ok(EnqueueRollingQueueSourceResponse {
            source,
            entry: Some(entry),
            replayed,
        })
    }

    /// #1641 S2: enqueue a topology land node's accepted commit on behalf of
    /// the execution's owning manager or Epic lead. The admission (the
    /// requester's live authority over the Epic and the acceptance of exactly
    /// this commit: the review's admitted verdict, or (#2249) the verified
    /// session's own terminal `scoped_test` job row) is re-derived from rows
    /// before the probe and again under the lock that stays held through the
    /// insert. A land admitted by verification alone never enqueues a source
    /// that carries a migration (`RequiresReview`). The entry's replay
    /// identity is the accepted author session plus the attempt's dedup key,
    /// so a duplicate enqueue returns the same entry; the settlement wake goes
    /// to the owner, never to a finished node session.
    ///
    /// # Errors
    /// A transient persistence or process error (asked again next tick);
    /// refusals are [`LandEnqueue`] values.
    pub(crate) async fn enqueue_for_topology(
        &self,
        request: &LandRequest,
        queue_enabled: bool,
    ) -> Result<LandEnqueue> {
        self.enqueue_topology_admitted(request, queue_enabled, review_admits)
            .await
    }

    /// [`Self::enqueue_for_topology`] with the review acceptance taken as given.
    #[cfg(test)]
    pub(crate) async fn enqueue_topology_admitted_for_test(
        &self,
        request: &LandRequest,
        queue_enabled: bool,
    ) -> Result<LandEnqueue> {
        self.enqueue_topology_admitted(request, queue_enabled, |_, _| Ok(true))
            .await
    }

    /// [`Self::enqueue_topology_admitted_for_test`], running `hook` at the
    /// probe boundary: after the first admission and the Git probe, before
    /// the insert's re-check.
    #[cfg(test)]
    pub(crate) async fn enqueue_topology_racing_for_test(
        &self,
        request: &LandRequest,
        hook: impl FnOnce(&Store) + Send + 'static,
    ) -> Result<LandEnqueue> {
        super::effect_fence::seam::install(request.execution_id, "topology_enqueue", hook);
        self.enqueue_topology_admitted_for_test(request, true).await
    }

    /// [`Self::enqueue_for_topology`] with the review-acceptance check
    /// injected, so a test can reach the probe boundary without a full review.
    async fn enqueue_topology_admitted(
        &self,
        request: &LandRequest,
        queue_enabled: bool,
        review_admitted: ReviewAdmitted,
    ) -> Result<LandEnqueue> {
        let owner = {
            let store = self.store.lock().await;
            match topology_admission(&store, request, review_admitted)? {
                Ok(owner) => owner,
                Err(reason) => return Ok(LandEnqueue::AdmissionLost(reason)),
            }
        };
        if !queue_enabled {
            return Ok(LandEnqueue::QueueDisabled);
        }
        if let LandAcceptance::RollingMerge {
            reviewed_commit, ..
        } = &request.acceptance
        {
            let (repo, base, head) = (
                request.repo_root.clone(),
                reviewed_commit.clone(),
                request.source_commit.clone(),
            );
            let proved = tokio::task::spawn_blocking(move || {
                crate::topology::custody::only_rolling_merges(&repo, &base, &head)
            })
            .await
            .unwrap_or(false);
            if !proved {
                return Ok(LandEnqueue::AdmissionLost(
                    "the reviewed source is not followed solely by clean rolling merges".into(),
                ));
            }
        }
        // #1807: a land node without explicit filters derives them from the
        // accepted diff, like a hand landing.
        let test_filters = if request.test_filters.is_empty() {
            crate::rolling_queue::derive_test_filters(&request.repo_root, &request.source_commit)
                .await
        } else {
            request.test_filters.clone()
        };
        let facts = match probe_source(
            &request.repo_root,
            &request.source_commit,
            &test_filters,
            None,
        )
        .await
        {
            Ok(facts) => facts,
            Err(DaemonError::InvalidParam(code)) => return Ok(LandEnqueue::Refused(code)),
            Err(error) => return Err(error),
        };
        // #2249: a verification receipt is not a review. AGENTS.md reserves the
        // pre-land review for new schema migrations, so a land admitted by the
        // receipt alone stops at one (a numbered or a provisional migration).
        match request.review_coverage {
            // #2304 slice 3: a policy-bearing execution checks the candidate
            // against what its graph reviews. A pre-merge graph already put a
            // reviewer on the exact head, so it needs no inspection.
            Some(coverage) if coverage >= ReviewPolicy::PreMerge => {}
            Some(coverage) => {
                let candidate = inspect_candidate(request, &facts).await;
                if let Some(reason) =
                    review_policy::block_reason(coverage, &request.source_commit, &candidate)
                {
                    return Ok(LandEnqueue::RequiresReview(reason));
                }
            }
            None => {
                if request.acceptance.is_verified()
                    && let Some(reason) = migration_needs_review(request, &facts).await
                {
                    return Ok(LandEnqueue::RequiresReview(reason.into()));
                }
            }
        }
        #[cfg(test)]
        super::effect_fence::seam::run(&self.store, request.execution_id, "topology_enqueue").await;
        let store = self.store.lock().await;
        // The lock was released for the Git probe: refuse if the owner, the
        // acceptance or the scope changed meanwhile.
        match topology_admission(&store, request, review_admitted)? {
            Ok(current) if current == owner => {}
            Ok(_) => {
                return Ok(LandEnqueue::AdmissionLost(
                    "the execution's owner changed while the landing was prepared".into(),
                ));
            }
            Err(reason) => return Ok(LandEnqueue::AdmissionLost(reason)),
        }
        // A cancellation committed during the probe (owner and acceptance are
        // not revoked by it) must not create a NEW entry; an entry created
        // before it is still adopted so the attempt mirrors what the queue
        // may publish. The check holds the same lock through the insert.
        if topology_land_entry(&store, request)?.is_none() && !topology_land_live(&store, request)?
        {
            return Ok(LandEnqueue::Cancelled);
        }
        let source = QueueSource {
            issue: None,
            supersedes: None,
            project_id: request.project_id,
            repo: request.repo_root.clone(),
            source_session_id: request.author_session_id,
            owner_epic_id: request.epic_id,
            commit: request.source_commit.clone(),
            test_filters,
            explicit_filters: !request.test_filters.is_empty(),
            handwritten_filters: request.test_filters.clone(),
            fixes_tests: Vec::new(),
            filters_pending: false,
            idempotency_key: request.dedup_key.clone(),
            wake_session_id: owner,
            expedite: None,
        };
        match insert_source(&store, &source, &facts) {
            Ok((entry, _)) => Ok(LandEnqueue::Queued(entry.id)),
            Err(DaemonError::InvalidParam(code)) if code.starts_with("queue_") => {
                Ok(LandEnqueue::Refused(code))
            }
            Err(error) => Err(error),
        }
    }
}

/// The current admission of a topology landing: `Ok(owner)` carries the
/// session that owns the execution (the settlement wake target); `Err` is the
/// short reason the landing is no longer admitted. Pure over rows.
fn topology_admission(
    store: &Store,
    request: &LandRequest,
    review_admitted: ReviewAdmitted,
) -> Result<std::result::Result<Option<Uuid>, String>> {
    let Some(execution) = crate::topology::store::load_execution(store, request.execution_id)?
    else {
        return Ok(Err("the topology execution no longer exists".into()));
    };
    if let Some(code) = crate::topology::agent::landing_gate(store, &execution)? {
        return Ok(Err(format!("land_not_authorized: {code}")));
    }
    match &request.acceptance {
        LandAcceptance::Review { .. } | LandAcceptance::RollingMerge { .. } => {
            if !review_admitted(store, request)? {
                return Ok(Err(
                    "the review no longer admits this commit as accepted".into()
                ));
            }
        }
        LandAcceptance::Verified { .. } => {
            if !verification_admits(store, request)? {
                return Ok(Err(
                    "the verifying scoped_test job no longer admits this commit".into(),
                ));
            }
        }
    }
    Ok(Ok(execution.requested_by_session_id))
}

/// Why a land admitted by verification alone must not enqueue its source:
/// it carries a schema migration (a version above the rolling base's, or a
/// provisional declaration the lander renumbers), or git could not say it does
/// not. `None`: the source carries none. Fails closed, because this is the
/// backstop for AGENTS.md's pre-land review of migrations.
async fn migration_needs_review(
    request: &LandRequest,
    facts: &crate::rolling_queue::SourceFacts,
) -> Option<&'static str> {
    if facts.migration_version.is_some() {
        return Some(
            "the source carries a schema migration, which needs a pre-land review; relaunch it \
             through a topology with a review node",
        );
    }
    let (repo, commit) = (request.repo_root.clone(), request.source_commit.clone());
    let declared = tokio::task::spawn_blocking(move || {
        crate::rolling_queue::source_provisional_declaration_count(&repo, &commit)
    })
    .await;
    match declared {
        Ok(Some(0)) => None,
        Ok(Some(_)) => Some(
            "the source declares a provisional schema migration, which needs a pre-land review; \
             relaunch it through a topology with a review node",
        ),
        _ => Some(
            "git could not show that the source carries no schema migration, and a migration \
             needs a pre-land review; relaunch it through a topology with a review node",
        ),
    }
}

/// Inspect a landing candidate for the review it needs: a migration (reusing
/// the migration detection above), then the repository paths it changes.
/// Anything git cannot show is [`Candidate::Unknown`], which fails closed.
async fn inspect_candidate(
    request: &LandRequest,
    facts: &crate::rolling_queue::SourceFacts,
) -> Candidate {
    if facts.migration_version.is_some() {
        return Candidate::Requires {
            policy: ReviewPolicy::PreMerge,
            why: "carries a schema migration",
        };
    }
    let (repo, commit) = (request.repo_root.clone(), request.source_commit.clone());
    let inspected = tokio::task::spawn_blocking(move || {
        (
            crate::rolling_queue::source_provisional_declaration_count(&repo, &commit),
            crate::rolling_queue::source_changed_paths(&repo, &commit),
        )
    })
    .await;
    match inspected {
        Ok((Some(0), DeltaRead::Paths(paths))) => review_policy::classify_paths(&paths),
        Ok((Some(count), _)) if count > 0 => Candidate::Requires {
            policy: ReviewPolicy::PreMerge,
            why: "declares a provisional schema migration",
        },
        _ => Candidate::Unknown(
            "git could not show what the source changes (migrations or changed paths)",
        ),
    }
}

/// Whether the accepted review still admits exactly the landing's commit.
type ReviewAdmitted = fn(&Store, &LandRequest) -> Result<bool>;

fn review_admits(store: &Store, request: &LandRequest) -> Result<bool> {
    match &request.acceptance {
        LandAcceptance::Review { assignment_id, .. } => {
            store.topology_land_admitted(*assignment_id, &request.source_commit)
        }
        // The immutable commits' merge proof runs outside the Store lock.
        // This live verdict is checked both before and after that Git work.
        LandAcceptance::RollingMerge {
            assignment_id,
            reviewed_commit,
            ..
        } => store.topology_land_admitted(*assignment_id, reviewed_commit),
        LandAcceptance::Verified { .. } => Ok(false),
    }
}

/// #2249: whether the verifying job row still admits exactly the landing's
/// commit: the same scoped-test job, still terminal, still owned by the
/// accepted session, and its receipt still names the commit. Pure over rows.
fn verification_admits(store: &Store, request: &LandRequest) -> Result<bool> {
    let LandAcceptance::Verified {
        session_id,
        job_id,
        head,
        ..
    } = &request.acceptance
    else {
        return Ok(false);
    };
    let Some(row) = store.get_agent_job(*job_id)? else {
        return Ok(false);
    };
    let job = row.job;
    let scoped = matches!(
        &job.params,
        rsi_common::agent_jobs::JobParams::Test(test) if test.is_scoped_test()
    );
    let receipt_head = job
        .result
        .as_ref()
        .and_then(|result| result.receipt.as_ref())
        .and_then(|receipt| receipt.get("head"))
        .and_then(serde_json::Value::as_str);
    Ok(head == &request.source_commit
        && scoped
        && job.state.is_terminal()
        && job.owner_session_id == *session_id
        && receipt_head == Some(request.source_commit.as_str()))
}

/// Whether the landing is still the live work of a running execution: the
/// execution is `running` (not cancelling or settled) and the request's land
/// attempt is still its reserved or launching attempt. Pure over rows.
fn topology_land_live(store: &Store, request: &LandRequest) -> Result<bool> {
    use crate::topology::store::{self as rows, AttemptStatus, ExecutionStatus};
    let running = rows::load_execution(store, request.execution_id)?
        .is_some_and(|execution| execution.status == ExecutionStatus::Running);
    let reserved =
        rows::load_attempt(store, request.attempt_id)?.is_some_and(|(execution, attempt)| {
            execution == request.execution_id
                && matches!(
                    attempt.status,
                    AttemptStatus::Reserved | AttemptStatus::Launching
                )
        });
    Ok(running && reserved)
}

/// The queue entry a topology landing already created, if any.
pub(crate) fn topology_land_entry(store: &Store, request: &LandRequest) -> Result<Option<Uuid>> {
    Ok(store
        .get_rolling_queue_entry_by_key(request.author_session_id, &request.dedup_key)?
        .map(|entry| entry.id))
}

/// The queue entry's state as a land node mirrors it.
pub(crate) fn topology_land_status(store: &Store, entry_id: Uuid) -> Result<LandStatus> {
    let Some(entry) = store.get_rolling_queue_entry(entry_id)? else {
        return Ok(LandStatus::Refused {
            state: "missing".into(),
            reason: "queue_entry_missing".into(),
        });
    };
    Ok(LandStatus::from_queue(entry.state, entry.outcome))
}

#[cfg(test)]
mod enqueue_bound_tests {
    use super::*;

    /// Exercise the shared enqueue admission with the real renumber guard (the
    /// `rsi-repo-guard` child process), while rolling is still V129 and the
    /// declared V130 is free (#3397). #452: the repository is built in Rust.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_free_provisional_version_is_validated_before_enqueue_3397() {
        use crate::provisional_fixture::{Provisional, git, real_guard};
        let guard = real_guard();
        let case = Provisional::new(&guard, &[]);
        let repo = case.repo.clone();
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/rolling", &case.base],
        );
        crate::repo_guard_call::register_test_tool(&repo, guard);
        let clean = case.source("clean", "");
        case.source("dirty", "");
        let dirty = case.commit_extra(
            "dirty",
            "crates/rsi-common/src/satellite_intake.rs",
            "const FIXTURE: &str = \"migrations/v130.rs\";\n",
        );
        let refusal = probe_source(&repo, &dirty, &[], None).await.unwrap_err();
        let DaemonError::InvalidParam(detail) = refusal else {
            panic!("unexpected refusal: {refusal}");
        };
        assert!(detail.starts_with(PROVISIONAL_UNPROVABLE), "{detail}");
        assert!(
            detail.contains(
                "undeclared version-bearing file or site: crates/rsi-common/src/satellite_intake.rs"
            ),
            "{detail}"
        );

        let facts = probe_source(&repo, &clean, &[], None).await.unwrap();
        let store = Store::open_in_memory().unwrap();
        let (entry, replayed) = insert_source(
            &store,
            &QueueSource {
                issue: None,
                supersedes: None,
                project_id: None,
                repo,
                source_session_id: Uuid::new_v4(),
                owner_epic_id: None,
                commit: clean.clone(),
                test_filters: Vec::new(),
                explicit_filters: true,
                handwritten_filters: Vec::new(),
                fixes_tests: Vec::new(),
                filters_pending: false,
                idempotency_key: "free-clean-provisional".into(),
                wake_session_id: None,
                expedite: None,
            },
            &facts,
        )
        .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Queued);
        assert_eq!(entry.source_commit, clean);
        assert!(!replayed);
    }

    /// The stub guard's still-free inspection when nothing is wrong.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const HEALTHY_INSPECTION: &str =
        r#"echo '{"old_version":130,"new_version":130,"renumber_noop":true}'"#;

    /// Register a stub trusted guard (`<exe> rolling-migration-renumber ...`)
    /// for the fixture repository: `--transform` answers a no-op unit and the
    /// still-free inspection (no `--transform`) runs `inspection`, a shell
    /// snippet that injects the infrastructure failure.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn register_stub_guard(case: &crate::provisional_fixture::Provisional, inspection: &str) {
        use std::os::unix::fs::PermissionsExt;
        let dir = case.root.path().join("stub-guard");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rsi-repo-guard");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\ncase \" $* \" in\n  *\" --transform \"*) echo '{{\"old_version\":130,\"new_version\":130,\"renumber_noop\":true}}'; exit 0;;\nesac\n{inspection}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::repo_guard_call::register_test_tool(&case.repo, exe);
    }

    /// The real fixture supplies declaration/git facts (built with the real
    /// guard, #452); a tiny stub guard injects an inspection failure after a
    /// successful still-free transform. `None` registers no guard at all.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn provisional_infrastructure_fixture(
        inspection: Option<&str>,
    ) -> (crate::provisional_fixture::Provisional, String) {
        use crate::provisional_fixture::{Provisional, git, real_guard};
        let case = Provisional::new(&real_guard(), &[]);
        git(
            &case.repo,
            &["update-ref", "refs/remotes/origin/rolling", &case.base],
        );
        let source = case.source("declared", "");
        if let Some(inspection) = inspection {
            register_stub_guard(&case, inspection);
        }
        (case, source)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    async fn attempt_provisional_enqueue(
        store: &Store,
        repo: &std::path::Path,
        commit: &str,
        session: Uuid,
    ) -> Result<rsi_common::rolling_queue::RollingQueueEntryV1> {
        let facts = probe_source(repo, commit, &[], None).await?;
        insert_source(
            store,
            &QueueSource {
                issue: None,
                supersedes: None,
                project_id: None,
                repo: repo.into(),
                source_session_id: session,
                owner_epic_id: None,
                commit: commit.into(),
                test_filters: Vec::new(),
                explicit_filters: true,
                handwritten_filters: Vec::new(),
                fixes_tests: Vec::new(),
                filters_pending: false,
                idempotency_key: "infrastructure-retry".into(),
                wake_session_id: None,
                expedite: None,
            },
            &facts,
        )
        .map(|(entry, _)| entry)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    async fn assert_infrastructure_refuses_enqueue(
        repo: &std::path::Path,
        source: &str,
        cause: &str,
    ) -> (Store, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let session = Uuid::new_v4();
        let error = attempt_provisional_enqueue(&store, repo, source, session)
            .await
            .unwrap_err();
        let DaemonError::InvalidParam(detail) = error else {
            panic!("{error}");
        };
        assert!(
            detail.starts_with(PROVISIONAL_PROBE_INCONCLUSIVE),
            "{detail}"
        );
        assert!(detail.contains(cause), "{detail}");
        assert!(detail.contains("no entry was recorded"), "{detail}");
        assert!(detail.contains("same idempotency_key"), "{detail}");
        assert!(
            store
                .get_rolling_queue_entry_by_key(session, "infrastructure-retry")
                .unwrap()
                .is_none()
        );
        (store, session)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_missing_guard_refuses_provisional_enqueue_retryably_3545() {
        // #465 decision: the guard is a trusted installed executable, never a
        // file of the source or of rolling. A declared source whose guard
        // cannot be resolved has no proof, so it fails closed (a retryable
        // refusal), exactly like #3545's missing-python3 case did.
        let (case, source) = provisional_infrastructure_fixture(None);
        let (store, session) = assert_infrastructure_refuses_enqueue(
            &case.repo,
            &source,
            "no trusted rolling-migration-renumber guard executable",
        )
        .await;
        let undeclared =
            attempt_provisional_enqueue(&store, &case.repo, &case.base, Uuid::new_v4())
                .await
                .unwrap();
        assert_eq!(undeclared.state, RollingQueueEntryState::Queued);
        // Restoring the prerequisite admits exactly the same commit/key.
        register_stub_guard(&case, HEALTHY_INSPECTION);
        let entry = attempt_provisional_enqueue(&store, &case.repo, &source, session)
            .await
            .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Queued);
        assert_eq!(entry.source_commit, source);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn inspection_exit_one_refuses_provisional_enqueue_retryably_3545() {
        let (case, source) = provisional_infrastructure_fixture(Some("exit 1"));
        assert_infrastructure_refuses_enqueue(
            &case.repo,
            &source,
            "still-free inspection exited Some(1)",
        )
        .await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn malformed_inspection_output_refuses_provisional_enqueue_retryably_3545() {
        for output in ["not json", "null", "{}"] {
            let (case, source) =
                provisional_infrastructure_fixture(Some(&format!("echo '{output}'")));
            assert_infrastructure_refuses_enqueue(
                &case.repo,
                &source,
                "still-free inspection: malformed output",
            )
            .await;
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn inspection_timeout_refuses_provisional_enqueue_retryably_3545() {
        let (case, source) = provisional_infrastructure_fixture(Some("sleep 60"));
        std::fs::write(case.repo.join(".rsi-provisional-step-timeout-ms"), "1000").unwrap();
        let started = std::time::Instant::now();
        assert_infrastructure_refuses_enqueue(
            &case.repo,
            &source,
            "still-free inspection: timeout",
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn missing_rolling_ref_refuses_only_declared_enqueue_retryably_3545() {
        let (case, source) = provisional_infrastructure_fixture(Some(HEALTHY_INSPECTION));
        for reference in ["refs/remotes/origin/rolling", "refs/heads/rolling"] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&case.repo)
                .args(["update-ref", "-d", reference])
                .status()
                .unwrap();
            assert!(status.success());
        }
        assert_infrastructure_refuses_enqueue(&case.repo, &source, "could not resolve rolling ref")
            .await;
        let store = Store::open_in_memory().unwrap();
        let entry = attempt_provisional_enqueue(&store, &case.repo, &case.base, Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Queued);
        assert_eq!(entry.source_commit, case.base);
    }

    /// #465: with no guard at all, a declared source that rolling already
    /// contains still needs no proof (the lander merges nothing), so it is
    /// admitted; only a source the guard would have to judge fails closed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_declared_source_already_in_the_tip_needs_no_guard_3545() {
        let (case, source) = provisional_infrastructure_fixture(None);
        crate::provisional_fixture::git(
            &case.repo,
            &["update-ref", "refs/remotes/origin/rolling", &source],
        );
        let entry = attempt_provisional_enqueue(
            &Store::open_in_memory().unwrap(),
            &case.repo,
            &source,
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Queued);
        assert_eq!(entry.source_commit, source);
    }

    /// #2907/#2993: a derivation that outruns the call's bound ends the wait at
    /// the bound, not when the derivation ends, and is deferred (the entry is
    /// recorded with its filters pending), never refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_slow_filter_derivation_defers_the_enqueue_at_the_bound_2907() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "rolling"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README"), "x\n").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-q", "-m", "base"]);
        let head = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(
            repo.join("scripts/check-touched-shards"),
            "import json, time\ntime.sleep(2)\nprint(json.dumps({'suggested_filters': []}))\n",
        )
        .unwrap();

        let started = std::time::Instant::now();
        let deferred = derive_and_probe(
            repo,
            &head,
            true,
            Vec::new(),
            Some(std::time::Duration::from_millis(200)),
            None,
        )
        .await
        .expect("a derivation still running at the bound is not a refusal");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(deferred, Derived::Deferred(..)),
            "the entry is recorded with its filters pending"
        );
        // Explicit filters never wait on a derivation.
        let Derived::Ready(filters, _) = derive_and_probe(
            repo,
            &head,
            false,
            Vec::new(),
            Some(std::time::Duration::ZERO),
            None,
        )
        .await
        .unwrap() else {
            panic!("explicit filters are ready at once");
        };
        assert!(filters.is_empty());
    }

    /// #3264: with `derive_filters`, the explicit filters are extra: the derived
    /// set comes first and the extras join it without a duplicate; a derivation
    /// that outruns the bound defers with the extras kept for the entry.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn derive_filters_gates_the_derived_set_plus_the_extra_filters_3264() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q", "-b", "rolling"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README"), "x\n").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-q", "-m", "base"]);
        let head = git(&["rev-parse", "HEAD"]);
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        let script = |sleep: u32| {
            std::fs::write(
                repo.join("scripts/check-touched-shards"),
                format!(
                    "import json, time\ntime.sleep({sleep})\nprint(json.dumps({{'suggested_filters': ['rsid=test:fast_shard']}}))\n"
                ),
            )
            .unwrap();
        };
        let extra = vec![
            "rsi-common=test:manifest_dir_guard".to_owned(),
            "rsid=test:fast_shard".to_owned(),
        ];

        script(0);
        let Derived::Ready(filters, _) = derive_and_probe(
            repo,
            &head,
            true,
            extra.clone(),
            Some(std::time::Duration::from_secs(20)),
            None,
        )
        .await
        .unwrap() else {
            panic!("a fast derivation is ready inline");
        };
        assert_eq!(
            filters,
            ["rsid=test:fast_shard", "rsi-common=test:manifest_dir_guard"],
            "the derived set, then the extra filter it did not already hold"
        );

        // A different commit is a different derivation (the shared one is cached
        // per commit), and this one outruns the bound.
        git(&["commit", "-q", "--allow-empty", "-m", "next"]);
        let next = git(&["rev-parse", "HEAD"]);
        script(3);
        let Derived::Deferred(kept, _) = derive_and_probe(
            repo,
            &next,
            true,
            extra.clone(),
            Some(std::time::Duration::from_millis(200)),
            None,
        )
        .await
        .unwrap() else {
            panic!("a slow derivation is deferred");
        };
        assert_eq!(kept, extra, "the deferred entry keeps the extra filters");
    }

    /// #3658: a source with a provisional-migration declaration whose renumber
    /// proof outlasts the call's wall bound is NOT refused `queue_probe_timeout`.
    /// The proof ran first and spent the whole bound, so the cheap reads after
    /// it were killed at an expired deadline. They now run first. #3545: a proof
    /// the deadline cuts short is not a pass either: it is refused
    /// `provisional_probe_inconclusive` (retryable, no entry recorded).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_slow_provisional_proof_does_not_time_out_the_probe_3658() {
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let marker = dir.path().join("proof-started");
        std::fs::write(repo.join("README"), "fixture\n").unwrap();
        // The repo guard (the renumber proof) outlasts the deadline: a shell stand-in that records
        // that it started and then sleeps.
        let guard = dir.path().join("rsi-repo-guard");
        std::fs::write(
            &guard,
            format!("#!/bin/sh\n: > '{}'\nexec sleep 120\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&guard, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let script = "git init -q -b rolling . && git config user.email t@t \
            && git config user.name t && git config commit.gpgsign false \
            && git add . && git commit -qm base \
            && git update-ref refs/remotes/origin/rolling HEAD \
            && git checkout -q -b feature && mkdir -p tools/provisional-migrations \
            && echo '{}' > tools/provisional-migrations/x.json \
            && git add tools && git commit -qm declare";
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
        let source = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        crate::repo_guard_call::register_test_tool(&repo, guard);
        assert_eq!(
            crate::rolling_queue::source_provisional_declaration_count(&repo, &source),
            Some(1)
        );
        let started = std::time::Instant::now();
        let refused = probe_source(&repo, &source, &[], Some(started + DEADLINE)).await;
        let Err(DaemonError::InvalidParam(detail)) = refused else {
            panic!("the slow proof must refuse retryably: {refused:?}");
        };
        assert!(
            detail.starts_with(PROVISIONAL_PROBE_INCONCLUSIVE),
            "{detail}"
        );
        assert!(
            !detail.starts_with(QUEUE_PROBE_TIMEOUT),
            "a slow proof is not a stalled git read: {detail}"
        );
        assert!(
            marker.is_file(),
            "the proof ran (and was cut short) rather than being skipped"
        );
        assert!(
            started.elapsed() < DEADLINE + std::time::Duration::from_secs(3),
            "the proof is killed at the deadline: {:?}",
            started.elapsed()
        );
    }

    /// #2912: a git read of the source probe that stalls is killed at the
    /// call's deadline and the call is refused `queue_probe_timeout`
    /// (retryable): never `queue_source_invalid`, never a hang.
    ///
    /// #187: the deadline also covers every git read the probe makes before the
    /// stalled one. With a 600 ms deadline a loaded host spent it on those reads,
    /// the stalled git never started (no pid file) and the kill check failed
    /// although the probe behaved correctly. The deadline is now far above the
    /// cost of those reads and the test asserts that the stalled git started.
    /// The three phases run at once, each in its own repository (the stall
    /// marker is per repository), so the test takes about one deadline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_stalled_source_probe_is_killed_at_the_deadline_and_refused_retryably_2912() {
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
        // The kill, reap and return after the deadline; the stall itself is 60 s.
        const KILL_SLACK: std::time::Duration = std::time::Duration::from_secs(3);
        fn source_repo() -> (tempfile::TempDir, String) {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            let script = "git init -q -b rolling . && git config user.email t@t \
                && git config user.name t && git config commit.gpgsign false \
                && echo a > a && git add a && git commit -qm base \
                && git update-ref refs/remotes/origin/rolling HEAD \
                && git checkout -q -b feature && echo b > b && git add b && git commit -qm feature";
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
            let source = String::from_utf8(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_owned();
            (dir, source)
        }
        let (dir, source) = source_repo();
        // The probe passes when git is fast.
        probe_source(
            dir.path(),
            &source,
            &[],
            Some(std::time::Instant::now() + std::time::Duration::from_secs(30)),
        )
        .await
        .unwrap();
        // Each phase of the probe in turn: the source existence read, the merge
        // base, and the diffs.
        let phases = ["cat-file", "merge-base", "diff"].map(|subcommand| {
            let (dir, source) = source_repo();
            crate::rolling_queue::stall_git(dir.path(), subcommand);
            async move {
                let started = std::time::Instant::now();
                let refusal = probe_source(dir.path(), &source, &[], Some(started + DEADLINE))
                    .await
                    .expect_err("the stalled read is cut short");
                (subcommand, dir, started.elapsed(), refusal)
            }
        });
        let [a, b, c] = phases;
        let (a, b, c) = tokio::join!(a, b, c);
        for (subcommand, dir, elapsed, refusal) in [a, b, c] {
            let repo = dir.path();
            assert!(
                repo.join(".rsi-stall-pid").is_file(),
                "{subcommand}: the deadline passed before the stalled git started ({elapsed:?})"
            );
            assert!(
                elapsed >= DEADLINE && elapsed < DEADLINE + KILL_SLACK,
                "{subcommand}: cut short at the deadline, not before and not after the stall: {elapsed:?}"
            );
            let text = refusal.to_string();
            assert!(text.contains(QUEUE_PROBE_TIMEOUT), "{subcommand}: {text}");
            assert!(!text.contains(QUEUE_SOURCE_INVALID), "{subcommand}: {text}");
            assert!(
                crate::rolling_queue::stalled_git_was_killed(repo),
                "{subcommand}: the stalled git must be killed"
            );
        }
    }
}
