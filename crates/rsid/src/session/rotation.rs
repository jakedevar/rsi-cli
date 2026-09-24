//! Context rotation operations for SessionManager.
//!
//! Handles the lifecycle of context rotation: detecting when a session's context
//! window is filling up, interrupting to write a handoff document, archiving the
//! parent, and spawning a fresh child session with a clean context.

use super::SessionManager;

mod recovery;

#[cfg(test)]
mod fence_tests;
use super::types::{
    CompletedSession, ROTATION_HANDOFF_PROMPT, TrackedSession, install_context_budget,
};
use crate::bus::DaemonEvent;
use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::{
    AdmissionDecision, InvocationCompletion, admit_invocation, complete_invocation,
    hash_request_fingerprint,
};
use crate::monitor;
use crate::sandbox::custody::{
    RotationBindFailure, RotationCustodyDisposition, RotationPredecessorSource,
};
use crate::store::Store;
use rsi_common::model_control::{InvocationOwner, ModelUsageConfidence};
use rsi_common::types::{
    ContextUsageConfidence, ControllerReleaseReasonV1, ConversationEvent, EventType,
    IdeaControllerLaunchConfirmationV1, IdeaControllerReservationV1,
    ReleaseAssignedIdeaControllerRequestV1, ReleaseIdeaControllerReservationRequestV1,
    ReserveIdeaControllerRequestV1, Role, Session, SessionProvider, SessionStatus,
};
use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

const ROTATION_RATE_WINDOW_MINUTES: i64 = 10;
const ROTATION_RATE_LIMIT: i64 = 4;
const ROTATION_DEPTH_SAFETY_CEILING: u32 = 64;

use super::types::PersistenceHandle;

/// Keyed, explicit opt-in seams for real handoff-resume custody tests. Every
/// registration is consumed at the named seam or removed by its test's final
/// observation, so parallel tests cannot accumulate global state.
#[cfg(test)]
fn handoff_custody_root_mutations() -> &'static std::sync::Mutex<std::collections::HashSet<String>>
{
    static MUTATIONS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    MUTATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
fn handoff_custody_config_observations() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, (std::path::PathBuf, Option<std::path::PathBuf>)>,
> {
    static OBSERVATIONS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<String, (std::path::PathBuf, Option<std::path::PathBuf>)>,
        >,
    > = std::sync::OnceLock::new();
    OBSERVATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn handoff_custody_config_observation_keys()
-> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static KEYS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    KEYS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
fn handoff_custody_test_key(session_id: Uuid) -> String {
    format!("handoff_resume:{session_id}")
}

#[cfg(test)]
fn install_handoff_custody_root_mutation_for_test(session_id: Uuid) {
    handoff_custody_root_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(handoff_custody_test_key(session_id));
}

#[cfg(test)]
fn install_handoff_custody_config_observation_for_test(session_id: Uuid) {
    handoff_custody_config_observation_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(handoff_custody_test_key(session_id));
}

#[cfg(test)]
fn apply_handoff_custody_root_mutation_for_test(session: &Session) -> Result<()> {
    let key = handoff_custody_test_key(session.id);
    if !handoff_custody_root_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key)
    {
        return Ok(());
    }
    let root = session.sandbox_root.as_ref().ok_or_else(|| {
        DaemonError::Store("handoff custody mutation test needs a sandbox root".to_string())
    })?;
    std::fs::rename(
        root,
        root.with_file_name(format!("{}-handoff-raced", session.id)),
    )
    .map_err(DaemonError::Io)
}

#[cfg(test)]
fn observe_handoff_custody_config_for_test(session_id: Uuid, config: &LaunchConfig) {
    let key = handoff_custody_test_key(session_id);
    if !handoff_custody_config_observation_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key)
    {
        return;
    }
    let working_dir = config
        .working_dir
        .clone()
        .expect("handoff config must carry permit-derived cwd");
    handoff_custody_config_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (working_dir, config.cargo_target_dir.clone()));
}

#[cfg(test)]
fn take_handoff_custody_config_for_test(
    session_id: Uuid,
) -> Option<(std::path::PathBuf, Option<std::path::PathBuf>)> {
    let key = handoff_custody_test_key(session_id);
    handoff_custody_config_observation_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key);
    handoff_custody_config_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key)
}

#[cfg(test)]
fn handoff_custody_test_seams_are_clean(session_id: Uuid) -> bool {
    let key = handoff_custody_test_key(session_id);
    !handoff_custody_root_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&key)
        && !handoff_custody_config_observation_keys()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&key)
        && !handoff_custody_config_observations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PostFinalizeRotationOutcome {
    Handled,
    ContinueNormalCompletion,
}

pub(super) fn resolve_rotation_task_query(
    store: &Store,
    predecessor: &Session,
) -> Result<Option<(Uuid, String)>> {
    let mut current = predecessor.clone();
    for _ in 0..8 {
        if !current
            .query
            .trim_start()
            .starts_with(ROTATION_HANDOFF_PROMPT)
            && !current.query.trim().is_empty()
        {
            return Ok(Some((current.id, current.query)));
        }
        let Some(ancestor_id) = current.continued_from else {
            return Ok(None);
        };
        let Some(ancestor) = store.get_session(ancestor_id)? else {
            return Ok(None);
        };
        current = ancestor;
    }
    Ok(None)
}

#[derive(Clone, Copy)]
pub(super) enum PointerKind {
    Rotation,
    FreshRelaunch,
}

pub(super) async fn rotation_task_pointer(predecessor: &Session, kind: PointerKind) -> String {
    let root = predecessor
        .sandbox_root
        .as_deref()
        .unwrap_or(&predecessor.working_dir);
    let head = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::process::Command::new("git")
            .kill_on_drop(true)
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output(),
    )
    .await
    .ok()
    .and_then(std::result::Result::ok)
    .filter(|output| output.status.success())
    .and_then(|output| String::from_utf8(output.stdout).ok())
    .map_or_else(|| "unknown".into(), |head| head.trim().to_owned());
    let branch = predecessor
        .sandbox_branch
        .as_deref()
        .or(predecessor.git_branch.as_deref())
        .unwrap_or("unknown");
    match kind {
        PointerKind::Rotation => format!(
            "Rotation continuation of {} (branch {}@{}); no handoff was bound — review predecessor commits before continuing.",
            predecessor.id, branch, head
        ),
        PointerKind::FreshRelaunch => format!(
            "Fresh relaunch of {} (branch {}@{}); review predecessor commits before continuing.",
            predecessor.id, branch, head
        ),
    }
}

async fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::process::Command::new("git")
            .kill_on_drop(true)
            .arg("-C")
            .arg(root)
            .args(args)
            .output(),
    )
    .await
    .ok()?
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| String::from_utf8(output.stdout).ok())
    .map(|output| output.trim().to_owned())
}

fn rotation_rate_limit_reached(store: &Store, session_id: Uuid) -> Result<bool> {
    let prior_rotations: i64 = store.conn.query_row(
        "WITH RECURSIVE lineage(id, continued_from) AS (
             SELECT id, continued_from FROM sessions WHERE id=?1
             UNION ALL
             SELECT parent.id, parent.continued_from FROM sessions parent
             JOIN lineage child ON child.continued_from=parent.id
         )
         SELECT count(*) FROM rotation_events event
         JOIN lineage ON lineage.id=event.session_id
         WHERE event.event_type='completed'
           AND julianday(event.created_at) >= julianday('now', ?2)",
        rusqlite::params![
            session_id.to_string(),
            format!("-{ROTATION_RATE_WINDOW_MINUTES} minutes")
        ],
        |row| row.get(0),
    )?;
    Ok(prior_rotations >= ROTATION_RATE_LIMIT - 1)
}

fn rotation_root(session: &Session) -> &Path {
    session
        .sandbox_root
        .as_deref()
        .unwrap_or(&session.working_dir)
}

async fn bound_handoff_commit(session: &Session, path: &str, start_head: &str) -> Option<String> {
    if start_head.len() != 40 || !start_head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let relative = Path::new(path);
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        || !rotation_root(session).join(relative).is_file()
    {
        return None;
    }
    let range = format!("{start_head}..HEAD");
    let committed = git_output(
        rotation_root(session),
        &["log", "-1", "--format=%H", &range, "--", path],
    )
    .await?;
    if !committed.is_empty() {
        return Some(committed);
    }
    let dirty = git_output(
        rotation_root(session),
        &["status", "--porcelain", "--", path],
    )
    .await?;
    (!dirty.is_empty()).then(|| "uncommitted".to_string())
}

fn rotation_start_head(store: &Store, session_id: Uuid, rotation_id: &str) -> Option<String> {
    let metadata: Option<String> = store.conn.query_row(
        "SELECT metadata FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND phase='writing_handoff' AND event_type='entered' ORDER BY id DESC LIMIT 1",
        rusqlite::params![session_id.to_string(), rotation_id],
        |row| row.get(0),
    ).ok()?;
    serde_json::from_str::<serde_json::Value>(&metadata?)
        .ok()?
        .get("start_head")?
        .as_str()
        .map(str::to_owned)
}

#[allow(clippy::significant_drop_tightening)]
async fn should_suppress_final_handoff(
    session_id: Uuid,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<Store>>,
) -> Result<bool> {
    let (parent_id, final_handoff) = {
        let completed_guard = completed.read().await;
        let Some(turn) = completed_guard.get(&session_id) else {
            return Ok(false);
        };
        let last_user_sequence = turn
            .events
            .iter()
            .rev()
            .find(|event| event.event_type == EventType::Message && event.role == Some(Role::User))
            .map_or(0, |event| event.sequence);
        let final_message = turn.events.iter().rev().find(|event| {
            event.event_type == EventType::Message
                && event.role == Some(Role::Assistant)
                && event.sequence > last_user_sequence
        });
        (
            turn.session.parent_id,
            final_message.is_some_and(|event| event.content.starts_with("PIPELINE HANDOFF — ")),
        )
    };
    if !final_handoff {
        return Ok(false);
    }
    let guard = store.lock().await;
    if let Some(epic_id) = parent_id
        && guard
            .get_session(epic_id)?
            .is_some_and(|epic| epic.lead_session_id == Some(session_id))
    {
        return Ok(false);
    }
    let mut lineage = std::collections::HashSet::new();
    let mut current = Some(session_id);
    for _ in 0..super::WATCH_LINEAGE_DEPTH_CAP {
        let Some(id) = current else { break };
        if !lineage.insert(id) {
            break;
        }
        current = guard.get_session(id)?.and_then(|row| row.continued_from);
    }
    for job in guard.list_scheduled_jobs()? {
        let rsi_common::types::WakeMode::OnTerminal(watched) = job.wake_mode else {
            continue;
        };
        if !job.enabled || !lineage.contains(&watched) || guard.is_harness_manager_watch(job.id)? {
            continue;
        }
        let Some(mut target) = job.wake_session_id else {
            continue;
        };
        let mut target_in_lineage = false;
        for _ in 0..super::WATCH_LINEAGE_DEPTH_CAP {
            if lineage.contains(&target) {
                target_in_lineage = true;
                break;
            }
            let Some(row) = guard.get_session(target)? else {
                break;
            };
            let Some(parent) = row.continued_from else {
                break;
            };
            target = parent;
        }
        if !target_in_lineage {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A clean-context child created only because the preceding handoff writer did
/// not report a filepath. One such retry is useful; a second identical bridge
/// has no new handoff-path progress signal and nests another monitor/rotation
/// await on the current task.
fn is_handoff_retry_bridge(completed: &CompletedSession) -> bool {
    completed.session.rotation_depth > 0
        && completed.session.continued_from.is_some()
        && completed.session.query.trim() == ROTATION_HANDOFF_PROMPT
        && completed
            .events
            .iter()
            .filter(|event| {
                event.event_type == EventType::Message
                    && event.role == Some(Role::User)
                    && event.content.trim() == ROTATION_HANDOFF_PROMPT
            })
            .take(2)
            .count()
            == 1
}

#[cfg(test)]
fn rotation_provider_unavailable() -> &'static std::sync::Mutex<std::collections::HashSet<Uuid>> {
    static UNAVAILABLE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Uuid>>> =
        std::sync::OnceLock::new();
    UNAVAILABLE.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
fn install_rotation_provider_unavailable_for_test(session_id: Uuid) {
    rotation_provider_unavailable()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id);
}

#[cfg(test)]
fn take_rotation_provider_unavailable_for_test(session_id: Uuid) -> bool {
    rotation_provider_unavailable()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id)
}

#[cfg(test)]
fn rotation_child_ids_for_test() -> &'static std::sync::Mutex<std::collections::HashMap<Uuid, Uuid>>
{
    static IDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Uuid, Uuid>>> =
        std::sync::OnceLock::new();
    IDS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Test barrier immediately before C1 acquires the publication guards:
/// `reached` fires when the rotation arrives, and publication proceeds only
/// after `resume` is sent. Lets a race test order capture -> publish ->
/// dispatch deterministically with the real spawn guards (K2 finding c).
#[cfg(test)]
type RotationPublicationPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
fn rotation_publication_pauses()
-> &'static std::sync::Mutex<HashMap<Uuid, RotationPublicationPause>> {
    static PAUSES: std::sync::OnceLock<std::sync::Mutex<HashMap<Uuid, RotationPublicationPause>>> =
        std::sync::OnceLock::new();
    PAUSES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub fn install_rotation_publication_pause_for_test(
    parent_id: Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    rotation_publication_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(parent_id, (reached_tx, resume_rx));
    (reached_rx, resume_tx)
}

#[cfg(test)]
async fn pause_rotation_publication_for_test(parent_id: Uuid) {
    let pause = rotation_publication_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&parent_id);
    if let Some((reached, resume)) = pause {
        let _ = reached.send(());
        let _ = resume.await;
    }
}

#[cfg(test)]
fn install_rotation_child_id_for_test(parent_id: Uuid, child_id: Uuid) {
    rotation_child_ids_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(parent_id, child_id);
}

#[cfg(test)]
fn take_rotation_child_id_for_test(parent_id: Uuid) -> Option<Uuid> {
    rotation_child_ids_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&parent_id)
}

#[cfg(test)]
#[derive(Clone)]
struct RotationConfigObservation {
    cwd: std::path::PathBuf,
    cargo_target_dir: Option<std::path::PathBuf>,
    system_prompt: Option<String>,
}

#[cfg(test)]
fn rotation_config_observation_keys() -> &'static std::sync::Mutex<std::collections::HashSet<Uuid>>
{
    static KEYS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Uuid>>> =
        std::sync::OnceLock::new();
    KEYS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
fn rotation_config_observations()
-> &'static std::sync::Mutex<std::collections::HashMap<Uuid, RotationConfigObservation>> {
    static OBSERVATIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, RotationConfigObservation>>,
    > = std::sync::OnceLock::new();
    OBSERVATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn install_rotation_config_observation_for_test(session_id: Uuid) {
    rotation_config_observation_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id);
}

#[cfg(test)]
fn observe_rotation_config_for_test(session_id: Uuid, config: &LaunchConfig) {
    if !rotation_config_observation_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id)
    {
        return;
    }
    rotation_config_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            session_id,
            RotationConfigObservation {
                cwd: config
                    .working_dir
                    .clone()
                    .expect("rotation config must carry permit cwd"),
                cargo_target_dir: config.cargo_target_dir.clone(),
                system_prompt: config.system_prompt.clone(),
            },
        );
}

#[cfg(test)]
fn take_rotation_config_observation_for_test(
    session_id: Uuid,
) -> Option<RotationConfigObservation> {
    rotation_config_observation_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id);
    rotation_config_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id)
}

#[cfg(test)]
fn rotation_context_read_observations()
-> &'static std::sync::Mutex<std::collections::HashMap<Uuid, bool>> {
    static OBSERVATIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, bool>>,
    > = std::sync::OnceLock::new();
    OBSERVATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn install_rotation_context_read_observation_for_test(session_id: Uuid) {
    rotation_context_read_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id, false);
}

#[cfg(test)]
fn observe_rotation_context_read_for_test(session_id: Uuid) {
    if let Some(observed) = rotation_context_read_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&session_id)
    {
        *observed = true;
    }
}

#[cfg(test)]
fn take_rotation_context_read_observation_for_test(session_id: Uuid) -> Option<bool> {
    rotation_context_read_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id)
}

#[cfg(test)]
fn rotation_context_root_mutations() -> &'static std::sync::Mutex<std::collections::HashSet<Uuid>> {
    static MUTATIONS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Uuid>>> =
        std::sync::OnceLock::new();
    MUTATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
fn rotation_execution_scratch_mutations()
-> &'static std::sync::Mutex<std::collections::HashSet<Uuid>> {
    static MUTATIONS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Uuid>>> =
        std::sync::OnceLock::new();
    MUTATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
fn install_rotation_context_root_mutation_for_test(session_id: Uuid) {
    rotation_context_root_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id);
}

#[cfg(test)]
fn install_rotation_execution_scratch_failure_for_test(session_id: Uuid) {
    rotation_execution_scratch_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id);
}

#[cfg(test)]
fn apply_rotation_context_root_mutation_for_test(session: &Session) -> Result<()> {
    if !rotation_context_root_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session.id)
    {
        return Ok(());
    }
    let root = session.sandbox_root.as_ref().ok_or_else(|| {
        DaemonError::Store("rotation context mutation requires a sandbox root".into())
    })?;
    std::fs::rename(root, root.with_extension("rotation-context-raced")).map_err(DaemonError::Io)
}

#[cfg(test)]
fn apply_rotation_execution_scratch_failure_for_test(session: &Session) -> Result<()> {
    if !rotation_execution_scratch_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session.id)
    {
        return Ok(());
    }
    let root = session.sandbox_root.as_ref().ok_or_else(|| {
        DaemonError::Store("rotation scratch mutation requires a sandbox root".into())
    })?;
    std::os::unix::fs::symlink(root, root.join("target")).map_err(DaemonError::Io)
}

#[cfg(test)]
fn rotation_competing_winners()
-> &'static std::sync::Mutex<std::collections::HashMap<Uuid, Session>> {
    static WINNERS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, Session>>,
    > = std::sync::OnceLock::new();
    WINNERS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn install_rotation_competing_winner_for_test(attempted_id: Uuid, winner: Session) {
    rotation_competing_winners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(attempted_id, winner);
}

#[cfg(test)]
fn take_rotation_competing_winner_for_test(attempted_id: Uuid) -> Option<Session> {
    rotation_competing_winners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&attempted_id)
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum RotationPreBindPredecessorMutationForTest {
    LineageRouting,
    ExecutionPrompt,
    ModelInvocation,
}

#[cfg(test)]
fn rotation_pre_bind_predecessor_mutations() -> &'static std::sync::Mutex<
    std::collections::HashMap<Uuid, RotationPreBindPredecessorMutationForTest>,
> {
    static MUTATIONS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<Uuid, RotationPreBindPredecessorMutationForTest>,
        >,
    > = std::sync::OnceLock::new();
    MUTATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn install_rotation_pre_bind_predecessor_mutation_for_test(
    child_id: Uuid,
    mutation: RotationPreBindPredecessorMutationForTest,
) {
    rotation_pre_bind_predecessor_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(child_id, mutation);
}

#[cfg(test)]
fn take_rotation_pre_bind_predecessor_mutation_for_test(
    child_id: Uuid,
) -> Option<RotationPreBindPredecessorMutationForTest> {
    rotation_pre_bind_predecessor_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&child_id)
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum RotationBindMutationForTest {
    CandidatePredecessorMismatch,
    SuccessorLineageMismatch,
    SuccessorTupleMismatch,
    SandboxHandleSessionMismatch,
}

#[cfg(test)]
fn rotation_bind_mutations_for_test()
-> &'static std::sync::Mutex<std::collections::HashMap<Uuid, RotationBindMutationForTest>> {
    static MUTATIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, RotationBindMutationForTest>>,
    > = std::sync::OnceLock::new();
    MUTATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn install_rotation_bind_mutation_for_test(
    session_id: Uuid,
    mutation: RotationBindMutationForTest,
) {
    rotation_bind_mutations_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id, mutation);
}

#[cfg(test)]
fn apply_rotation_bind_mutation_for_test(
    session_id: Uuid,
    candidate: &mut crate::sandbox::custody::RotationCustodyCandidate,
    successor: &mut Session,
) {
    let mutation = rotation_bind_mutations_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id);
    match mutation {
        Some(RotationBindMutationForTest::CandidatePredecessorMismatch) => {
            crate::sandbox::custody::CustodyExecutionRuntime::replace_rotation_candidate_predecessor_for_test(
                candidate,
                Uuid::new_v4(),
            );
        }
        Some(RotationBindMutationForTest::SuccessorLineageMismatch) => {
            successor.continued_from = Some(Uuid::new_v4());
        }
        Some(RotationBindMutationForTest::SuccessorTupleMismatch) => {
            successor.sandbox_branch = Some("refs/heads/mismatched-rotation-successor".into());
        }
        Some(RotationBindMutationForTest::SandboxHandleSessionMismatch) => {
            crate::sandbox::custody::CustodyExecutionRuntime::replace_rotation_candidate_handle_session_for_test(
                candidate,
                Uuid::new_v4(),
            );
        }
        None => {}
    }
}

#[cfg(test)]
fn rotation_monitor_panics_for_test() -> &'static std::sync::Mutex<std::collections::HashSet<Uuid>>
{
    static PANICS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Uuid>>> =
        std::sync::OnceLock::new();
    PANICS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
fn install_rotation_monitor_panic_for_test(session_id: Uuid) {
    rotation_monitor_panics_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id);
}

#[cfg(test)]
fn panic_rotation_monitor_for_test(session_id: Uuid) {
    if rotation_monitor_panics_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id)
    {
        panic!("injected rotation monitor panic for {session_id}");
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum RotationPreRestoreMutationForTest {
    Status,
    Authority,
}

#[cfg(test)]
fn rotation_pre_restore_mutations()
-> &'static std::sync::Mutex<std::collections::HashMap<Uuid, RotationPreRestoreMutationForTest>> {
    static MUTATIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, RotationPreRestoreMutationForTest>>,
    > = std::sync::OnceLock::new();
    MUTATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn install_rotation_pre_restore_mutation_for_test(
    child_id: Uuid,
    mutation: RotationPreRestoreMutationForTest,
) {
    rotation_pre_restore_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(child_id, mutation);
}

#[cfg(test)]
async fn apply_rotation_pre_restore_mutation_for_test(
    child_id: Uuid,
    parent_id: Uuid,
    store: &Arc<tokio::sync::Mutex<Store>>,
) {
    let mutation = rotation_pre_restore_mutations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&child_id);
    let Some(mutation) = mutation else {
        return;
    };
    let sql = match mutation {
        RotationPreRestoreMutationForTest::Status => {
            "UPDATE sessions SET status='Interrupted', updated_at='2099-01-02T00:00:00.000000000Z' WHERE id=?1"
        }
        RotationPreRestoreMutationForTest::Authority => {
            "UPDATE sessions SET active_task='substituted archived prompt authority', updated_at='2099-01-02T00:00:00.000000000Z' WHERE id=?1"
        }
    };
    store
        .lock()
        .await
        .conn
        .execute(sql, [parent_id.to_string()])
        .expect("mutate archived predecessor before restoration");
}

fn rotation_owner_from_session(session: &Session) -> InvocationOwner {
    InvocationOwner {
        session_id: Some(session.id),
        project_id: session.project_id,
        workflow_id: session.workflow_id,
        scheduled_job_id: session.scheduled_job_id,
        issue_tracker_id: session.issue_tracker_id.clone(),
        issue_identifier: session.issue_identifier.clone(),
        topology_node_id: session.topology_node_id.clone(),
        recursive_graph_id: None,
        recursive_task_id: None,
        recursive_attempt_id: None,
        operator: None,
    }
}

async fn release_rotation_controller_reservation(
    transfer: Option<&(
        crate::idea_control::IdeaControllerTransferHandle,
        IdeaControllerReservationV1,
    )>,
    reason: ControllerReleaseReasonV1,
) {
    if let Some((handle, reservation)) = transfer
        && let Err(error) = handle
            .release_reservation(&ReleaseIdeaControllerReservationRequestV1 {
                transfer_intent_key: reservation.transfer_intent_key.clone(),
                reason,
            })
            .await
    {
        tracing::error!(
            reservation_id = %reservation.reservation_id,
            error = %error,
            "Failed to release rotation controller reservation"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn schedule_prepared_rotation_retry(
    parent_id: Uuid,
    handoff: Option<String>,
    rotation_id: Option<String>,
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    event_bus: Arc<crate::bus::EventBus>,
    store: Arc<tokio::sync::Mutex<Store>>,
    model_call_settlements: crate::model_control::call_control::ModelCallSettlementHandle,
    persistence: PersistenceHandle,
    context_rotation_enabled: bool,
    socket_path: std::path::PathBuf,
    counter: Arc<monitor::TokenCounter>,
    memory_handle: Option<crate::memory::worker::MemoryHandle>,
    retry_tx: mpsc::Sender<Uuid>,
    runtime_config: Arc<crate::config::RuntimeConfig>,
    spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
    agent_tokens: Arc<RwLock<super::AgentTokenRegistry>>,
    spawn_epoch: Arc<std::sync::atomic::AtomicU64>,
    agent_message_arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
    codegraph_handle: Option<crate::codegraph::IndexHandle>,
    custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
) {
    tokio::spawn(async move {
        // Keep this one named retry owner alive while the Prepared gate holds.
        // Re-entering rotation every second would repeat model admission and
        // create an invocation row for each refused reservation. The eventual
        // reservation still checks the gate atomically after this read hint.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let tuple = {
                let completed = completed.read().await;
                let Some(parent) = completed.get(&parent_id) else {
                    return;
                };
                (
                    parent.session.sandbox_root.clone(),
                    parent.session.sandbox_branch.clone(),
                )
            };
            match store.lock().await.prepared_reclaim_for_owner_tuple(
                parent_id,
                tuple.0.as_deref(),
                tuple.1.as_deref(),
            ) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(error) => {
                    tracing::warn!(%parent_id, %error, "Prepared rotation gate read failed; retrying");
                }
            }
        }
        SessionManager::rotate_completed_session(
            parent_id,
            handoff,
            rotation_id,
            active,
            completed,
            event_bus,
            store,
            model_call_settlements,
            persistence,
            context_rotation_enabled,
            socket_path,
            counter,
            memory_handle,
            retry_tx,
            runtime_config,
            spawn_coordinator,
            agent_tokens,
            spawn_epoch,
            agent_message_arbiter,
            codegraph_handle,
            custody_runtime,
        )
        .await;
    });
}

async fn preflight_rotation_lead_transfer(
    store: &Arc<tokio::sync::Mutex<Store>>,
    parent_id: Uuid,
) -> Result<Vec<Uuid>> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || {
        store
            .blocking_lock()
            .preflight_rotation_lead_transfer(parent_id)
    })
    .await
    .map_err(|error| DaemonError::Store(format!("rotation lead preflight join failed: {error}")))?
}

async fn revoke_transferred_rotation_predecessor_after_failure(
    disposition: RotationCustodyDisposition,
    parent_id: Uuid,
    store: &Arc<tokio::sync::Mutex<Store>>,
    agent_tokens: &Arc<RwLock<super::AgentTokenRegistry>>,
) {
    if disposition != RotationCustodyDisposition::Transferred {
        return;
    }
    // Preserve the established live-custody rule (a transferred historical
    // predecessor loses A6) without reopening HIGH-2. Store -> A6 is the
    // global authority lock order, and any nonterminal baton/error retains the
    // predecessor token rather than guessing that rotation may supersede it.
    let store = store.lock().await;
    match store.preflight_rotation_lead_transfer(parent_id) {
        Ok(_) => agent_tokens.write().await.revoke_session(parent_id),
        Err(error) => tracing::warn!(
            %parent_id,
            error = %error,
            "Retaining transferred rotation predecessor token behind Epic lead fence"
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchivedParentRestoration {
    AuthorityPublished,
    DurableRestoreFailed,
}

impl SessionManager {
    async fn restore_archived_parent_after_child_failure(
        parent_id: Uuid,
        parent_completed: Option<CompletedSession>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: &Arc<crate::bus::EventBus>,
        bound_rotation: &crate::sandbox::custody::BoundRotationCustody,
        custody_runtime: &crate::sandbox::custody::CustodyExecutionRuntime,
        agent_tokens: &Arc<RwLock<super::AgentTokenRegistry>>,
    ) -> ArchivedParentRestoration {
        if let Some(retained) = parent_completed.as_ref()
            && retained.session.id != parent_id
        {
            tracing::error!(
                parent_id = %parent_id,
                retained_parent_id = %retained.session.id,
                "Retained parent history identity refused before durable restoration"
            );
            return ArchivedParentRestoration::DurableRestoreFailed;
        }
        let durable_parent = match custody_runtime
            .restore_archived_rotation_predecessor(parent_id, bound_rotation)
            .await
        {
            Ok(Some(session)) => session,
            Ok(None) => {
                tracing::error!(
                    parent_id = %parent_id,
                    "Archived parent authority fence refused restoration after child monitor panic"
                );
                return ArchivedParentRestoration::DurableRestoreFailed;
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    parent_id = %parent_id,
                    "Durable parent restoration failed after child monitor panic"
                );
                return ArchivedParentRestoration::DurableRestoreFailed;
            }
        };
        if durable_parent.id != parent_id {
            tracing::error!(
                parent_id = %parent_id,
                durable_parent_id = %durable_parent.id,
                "Durable parent restoration identity mismatch"
            );
            return ArchivedParentRestoration::DurableRestoreFailed;
        }
        let restored_status = durable_parent.status;
        // No in-memory snapshot was available (`parent_completed` was None) --
        // this is a fresh reconstruction from a durable read, not a real
        // transcript load. Mark it unhydrated so a later access (continue,
        // rotate, or a TUI poll) loads the real events from SQLite instead of
        // treating this placeholder as an authoritative empty transcript.
        let mut parent_completed = parent_completed.unwrap_or_else(|| CompletedSession {
            session: durable_parent.clone(),
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: false,
        });
        parent_completed.session = durable_parent;

        // The archive saga revokes the old token. Re-mint before restoring
        // completed visibility, but only after the exact Archived-phase Store
        // transaction has returned its durable row.
        let _restored_token = super::remint_agent_token(agent_tokens, parent_id).await;
        completed.write().await.insert(parent_id, parent_completed);
        event_bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: parent_id,
            old_status: SessionStatus::Archived,
            new_status: restored_status,
        });
        tracing::info!(
            parent_id = %parent_id,
            restored_status = ?restored_status,
            "Parent restored after child monitor panic"
        );
        ArchivedParentRestoration::AuthorityPublished
    }

    /// Inject a visible system event into a completed session.
    ///
    /// Shared by rotation refusal events so they persist and display consistently.
    async fn inject_session_system_event(
        session_id: Uuid,
        message: String,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: &Arc<crate::bus::EventBus>,
        persistence: &PersistenceHandle,
    ) {
        let next_seq = {
            let completed_guard = completed.read().await;
            completed_guard
                .get(&session_id)
                .and_then(|cs| cs.events.last())
                .map(|e| e.sequence + 1)
                .unwrap_or(0)
        };
        let system_event = ConversationEvent {
            id: 0,
            session_id,
            sequence: next_seq,
            event_type: EventType::System,
            role: None,
            content: message,
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        match persistence.insert_event(system_event.clone()).await {
            Ok(db_id) => {
                let mut ev = system_event.clone();
                ev.id = db_id;
                let mut completed_guard = completed.write().await;
                if let Some(cs) = completed_guard.get_mut(&session_id) {
                    cs.events.push(ev.clone());
                }
                drop(completed_guard);
                event_bus.publish(DaemonEvent::ConversationEvent {
                    session_id,
                    event: ev,
                });
            }
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to persist rotation system event"
                );
            }
        }
    }

    async fn record_rotation_refusal(
        session_id: Uuid,
        rotation_id: Option<&str>,
        code: &str,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: &Arc<crate::bus::EventBus>,
        persistence: &PersistenceHandle,
        store: &Arc<tokio::sync::Mutex<Store>>,
    ) {
        let generated_rotation_id;
        let rotation_id = if let Some(rotation_id) = rotation_id {
            rotation_id
        } else {
            generated_rotation_id = format!(
                "untracked:{session_id}:{}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            );
            &generated_rotation_id
        };
        if let Err(error) = persistence
            .log_rotation_event(
                session_id,
                rotation_id,
                "completed",
                &format!("refused:{code}"),
                None,
            )
            .await
        {
            tracing::error!(%session_id, %error, "Failed to persist rotation refusal");
        }
        Self::inject_session_system_event(
            session_id,
            format!("Context rotation refused: {code}. The predecessor remains available."),
            completed,
            event_bus,
            persistence,
        )
        .await;
        let lead = completed
            .read()
            .await
            .get(&session_id)
            .map(|turn| turn.session.clone());
        if let Some(lead) = lead {
            let notice_result = store
                .lock()
                .await
                .record_manager_terminal_notice_before_archive(&lead);
            match notice_result {
                Ok(Some(job_id)) => event_bus.publish(DaemonEvent::ManagerNoticeQueued { job_id }),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    %session_id,
                    %error,
                    "Failed to persist manager notice for rotation refusal"
                ),
            }
        }
    }

    /// Execute rotation-specific follow-up after the session has been finalized.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_post_finalization_rotation_action(
        session_id: Uuid,
        rotation_action: super::rotation_coordinator::RotationAction,
        rotation_id_for_log: Option<String>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: Arc<crate::bus::EventBus>,
        store: Arc<tokio::sync::Mutex<Store>>,
        model_call_settlements: crate::model_control::call_control::ModelCallSettlementHandle,
        persistence: PersistenceHandle,
        context_rotation_enabled: bool,
        socket_path: std::path::PathBuf,
        counter: Arc<monitor::TokenCounter>,
        memory_handle: Option<crate::memory::worker::MemoryHandle>,
        retry_tx: mpsc::Sender<Uuid>,
        runtime_config: Arc<crate::config::RuntimeConfig>,
        spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
        agent_tokens: Arc<RwLock<super::AgentTokenRegistry>>,
        spawn_epoch: Arc<std::sync::atomic::AtomicU64>,
        agent_message_arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
        codegraph_handle: Option<crate::codegraph::IndexHandle>,
        custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
    ) -> PostFinalizeRotationOutcome {
        match rotation_action {
            super::rotation_coordinator::RotationAction::SendCreateHandoff => {
                if Self::suppress_final_handoff_rotation(
                    session_id,
                    None,
                    rotation_id_for_log.as_deref(),
                    &completed,
                    &store,
                    &persistence,
                )
                .await
                {
                    return PostFinalizeRotationOutcome::ContinueNormalCompletion;
                }
                // ── Circuit breaker #1: empty session ──────────────────────────────────
                // If the session produced no meaningful output (status == Failed), the
                // provider likely crashed or hit a rate/session limit. Sending
                // /create_handoff to an empty session just creates another failing
                // session, which in turn fires SendCreateHandoff again — an infinite
                // cascade (observed: 64 empty sessions from a single Antigravity limit hit).
                //
                // Fall through to ContinueNormalCompletion so the normal retry path can
                // decide what to do (respects max_retries, backoff, etc).
                {
                    let completed_guard = completed.read().await;
                    if let Some(cs) = completed_guard.get(&session_id) {
                        if cs.session.status == SessionStatus::Failed {
                            tracing::warn!(
                                session_id = %session_id,
                                "Rotation circuit breaker: session produced no meaningful \
                                 output — aborting handoff write to prevent cascade"
                            );
                            drop(completed_guard);
                            Self::record_rotation_refusal(
                                session_id,
                                rotation_id_for_log.as_deref(),
                                "no_progress",
                                &completed,
                                &event_bus,
                                &persistence,
                                &store,
                            )
                            .await;
                            return PostFinalizeRotationOutcome::ContinueNormalCompletion;
                        }
                    }
                }
                // ───────────────────────────────────────────────────────────────────────
                if let Some(ref rid) = rotation_id_for_log {
                    let start_head = {
                        let guard = completed.read().await;
                        if let Some(turn) = guard.get(&session_id) {
                            git_output(rotation_root(&turn.session), &["rev-parse", "HEAD"]).await
                        } else {
                            None
                        }
                    };
                    let _ = persistence
                        .log_rotation_event(
                            session_id,
                            rid,
                            "writing_handoff",
                            "entered",
                            start_head
                                .map(|head| serde_json::json!({"start_head":head}).to_string()),
                        )
                        .await;
                }
                let provider_replacement =
                    completed
                        .read()
                        .await
                        .get(&session_id)
                        .is_some_and(|completed_session| {
                            completed_session.session.provider == SessionProvider::CodexAppServer
                        });
                if provider_replacement {
                    Box::pin(Self::rotate_completed_session(
                        session_id,
                        None,
                        rotation_id_for_log.clone(),
                        active,
                        completed,
                        event_bus,
                        store,
                        model_call_settlements,
                        persistence,
                        context_rotation_enabled,
                        socket_path,
                        counter,
                        memory_handle,
                        retry_tx,
                        runtime_config.clone(),
                        spawn_coordinator.clone(),
                        agent_tokens,
                        spawn_epoch,
                        agent_message_arbiter,
                        codegraph_handle,
                        custody_runtime,
                    ))
                    .await;
                } else {
                    Self::resume_for_handoff_write(
                        session_id,
                        rotation_id_for_log.clone(),
                        active,
                        completed,
                        event_bus,
                        store,
                        model_call_settlements,
                        persistence,
                        context_rotation_enabled,
                        socket_path,
                        counter,
                        memory_handle,
                        retry_tx,
                        runtime_config.clone(),
                        spawn_coordinator.clone(),
                        agent_tokens,
                        spawn_epoch,
                        agent_message_arbiter,
                        codegraph_handle,
                        custody_runtime,
                    )
                    .await;
                }
                PostFinalizeRotationOutcome::Handled
            }
            super::rotation_coordinator::RotationAction::SpawnChild {
                session_id: sid,
                handoff_filepath,
            } => {
                if Self::suppress_final_handoff_rotation(
                    sid,
                    handoff_filepath.as_deref(),
                    rotation_id_for_log.as_deref(),
                    &completed,
                    &store,
                    &persistence,
                )
                .await
                {
                    return PostFinalizeRotationOutcome::ContinueNormalCompletion;
                }
                // ── Circuit breaker #2: failed handoff writer ──────────────────────────
                // If the handoff-writing session itself failed (no meaningful output),
                // don't spawn a child — the child would also fail immediately, trigger
                // SpawnChild again, and so on. This is the second leg of the cascade
                // guard; circuit breaker #1 stops the PendingInterrupt→SendCreateHandoff
                // path, this one stops the WritingHandoff→SpawnChild path.
                //
                // Record a durable refusal and notify the user, then return Handled
                // (not ContinueNormalCompletion) because the parent
                // session is already finalized and there is nothing to retry.
                {
                    let completed_guard = completed.read().await;
                    if let Some(cs) = completed_guard.get(&sid) {
                        if cs.session.status == SessionStatus::Failed {
                            tracing::warn!(
                                session_id = %sid,
                                "Rotation circuit breaker: handoff-writing session produced \
                                 no output — stopping rotation chain to prevent cascade"
                            );
                            drop(completed_guard);
                            Self::record_rotation_refusal(
                                sid,
                                rotation_id_for_log.as_deref(),
                                "no_progress",
                                &completed,
                                &event_bus,
                                &persistence,
                                &store,
                            )
                            .await;
                            return PostFinalizeRotationOutcome::Handled;
                        }
                    }
                }
                // ───────────────────────────────────────────────────────────────────────
                if handoff_filepath.is_none() {
                    let exhausted_clean_context_retry = completed
                        .read()
                        .await
                        .get(&sid)
                        .is_some_and(is_handoff_retry_bridge);
                    if exhausted_clean_context_retry {
                        tracing::warn!(
                            session_id = %sid,
                            "Clean-context handoff retry completed without a filepath; \
                             stopping rotation instead of recursively spawning another retry"
                        );
                        if let Some(ref rid) = rotation_id_for_log {
                            let _ = persistence
                                .log_rotation_event(
                                    sid,
                                    rid,
                                    "completed",
                                    "stopped_missing_handoff",
                                    None,
                                )
                                .await;
                        }
                        Self::inject_session_system_event(
                            sid,
                            "Context rotation stopped: the clean-context handoff retry \
                             completed without producing a handoff filepath. The completed \
                             retry session was retained; start a new session to continue."
                                .to_string(),
                            &completed,
                            &event_bus,
                            &persistence,
                        )
                        .await;
                        return PostFinalizeRotationOutcome::Handled;
                    }
                    tracing::warn!(session_id = %sid, "Handoff write completed without a detected path; resuming durable task query");
                }
                tracing::info!(
                    session_id = %sid,
                    handoff_filepath = ?handoff_filepath,
                    "Handoff write complete, rotating to child session"
                );
                Self::rotate_completed_session(
                    sid,
                    handoff_filepath,
                    rotation_id_for_log.clone(),
                    active,
                    completed,
                    event_bus,
                    store,
                    model_call_settlements,
                    persistence,
                    context_rotation_enabled,
                    socket_path,
                    counter,
                    memory_handle,
                    retry_tx,
                    runtime_config.clone(),
                    spawn_coordinator.clone(),
                    agent_tokens,
                    spawn_epoch,
                    agent_message_arbiter,
                    codegraph_handle,
                    custody_runtime,
                )
                .await;
                PostFinalizeRotationOutcome::Handled
            }
            super::rotation_coordinator::RotationAction::NoOp => {
                PostFinalizeRotationOutcome::ContinueNormalCompletion
            }
            unexpected => {
                tracing::warn!(
                    session_id = %session_id,
                    action = ?unexpected,
                    "Unexpected post-finalization rotation action"
                );
                PostFinalizeRotationOutcome::Handled
            }
        }
    }

    async fn suppress_final_handoff_rotation(
        session_id: Uuid,
        handoff_filepath: Option<&str>,
        rotation_id: Option<&str>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
        persistence: &PersistenceHandle,
    ) -> bool {
        match should_suppress_final_handoff(session_id, completed, store).await {
            Ok(true) => {
                if let Some(rotation_id) = rotation_id {
                    let _decision_guard =
                        super::spawn_single_flight::acquire_spawn_guard(session_id).await;
                    let already_settled = {
                        let guard = store.lock().await;
                        guard.conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND (event_type='completed' OR event_type='suppressed_final_handoff' OR event_type LIKE 'refused:%'))",
                            rusqlite::params![session_id.to_string(), rotation_id],
                            |row| row.get::<_, bool>(0),
                        )
                    };
                    if matches!(already_settled, Ok(true)) {
                        return true;
                    }
                    let metadata = handoff_filepath
                        .map(|path| serde_json::json!({"handoff_filepath": path}).to_string());
                    if let Err(error) = persistence
                        .log_rotation_event(
                            session_id,
                            rotation_id,
                            "completed",
                            "suppressed_final_handoff",
                            metadata,
                        )
                        .await
                    {
                        tracing::error!(%session_id, %error, "Failed to persist final-handoff suppression");
                    }
                    if let Err(error) = persistence.barrier().await {
                        tracing::error!(%session_id, %error, "Failed to settle final-handoff suppression");
                    }
                }
                true
            }
            Ok(false) => false,
            Err(error) => {
                tracing::error!(%session_id, %error, "Final-handoff suppression check failed");
                false
            }
        }
    }

    /// Resume a session with `/create_handoff` directly, without going through RPC.
    /// Replaces `send_continue_rpc()` + ContinueSession dispatch + `continue_session()`.
    ///
    /// Called from the monitor post-loop after the coordinator returns `SendCreateHandoff`.
    /// Unlike `continue_session()`, this function:
    /// - Has no `&self` (no access to provider client fields)
    /// - Constructs a fresh provider client inline (same pattern as `spawn_rotation_child`)
    /// - Hardcodes query = "/create_handoff" and rotation state = WritingHandoff
    /// - Reuses the same session UUID (resume in-context, not a new session)
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resume_for_handoff_write(
        session_id: Uuid,
        rotation_id: Option<String>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: Arc<crate::bus::EventBus>,
        store: Arc<tokio::sync::Mutex<Store>>,
        model_call_settlements: crate::model_control::call_control::ModelCallSettlementHandle,
        persistence: PersistenceHandle,
        context_rotation_enabled: bool,
        socket_path: std::path::PathBuf,
        counter: Arc<monitor::TokenCounter>,
        memory_handle: Option<crate::memory::worker::MemoryHandle>,
        retry_tx: mpsc::Sender<Uuid>,
        runtime_config: Arc<crate::config::RuntimeConfig>,
        spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
        agent_tokens: Arc<RwLock<super::AgentTokenRegistry>>,
        spawn_epoch: Arc<std::sync::atomic::AtomicU64>,
        agent_message_arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
        codegraph_handle: Option<crate::codegraph::IndexHandle>,
        custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
    ) {
        // Single-flight spawn guard: covers ONLY this fn's check -> launch ->
        // active.insert span, then is explicitly dropped BEFORE the inline
        // `monitor_session` await below. Releasing it before the monitor is
        // mandatory: `monitor_session` drives this session's post-finalization
        // rotation inline on THIS task, which can re-enter `acquire_spawn_guard`
        // for the SAME id (monitor -> SendCreateHandoff -> resume_for_handoff_write).
        // Holding across that await would self-deadlock the non-reentrant
        // `tokio::Mutex`. A racing handoff-resume/continue that arrives during the
        // covered window blocks here and, on acquiring, adopts the now-live child
        // instead of spawning a twin. The non-contended path is unchanged.
        let spawn_guard = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        let cwd_admission_guard =
            super::spawn_single_flight::acquire_provider_cwd_admission().await;
        if spawn_guard.contended()
            && super::spawn_single_flight::adopt_if_live(&active, session_id).await
        {
            event_bus.publish(DaemonEvent::SessionSpawnDeduped {
                session_id,
                source: "resume_for_handoff_write".to_string(),
            });
            return;
        }
        match store
            .lock()
            .await
            .session_or_custody_has_settlement_fence(session_id)
        {
            Ok(false) => {}
            Ok(true) => {
                tracing::warn!(%session_id, "handoff resume refused by settlement fence");
                return;
            }
            Err(error) => {
                tracing::warn!(%session_id, %error, "handoff resume settlement-fence check failed closed");
                return;
            }
        }

        let query = "/create_handoff".to_string();

        // Extract session from completed map (finalize_session already ran)
        let mut completed_session = {
            let mut completed_guard = completed.write().await;
            match completed_guard.remove(&session_id) {
                Some(cs) => cs,
                None => {
                    tracing::error!(
                        session_id = %session_id,
                        "resume_for_handoff_write: session not found in completed map"
                    );
                    return;
                }
            }
        };

        // C7 Phase 1: this session is normally freshly finalized (already
        // fully hydrated), but a rare recovery path (an archived parent
        // reconstructed after a child monitor panic) can hand this function a
        // placeholder with `events_hydrated == false`. `.events` below is
        // replayed to API providers and carried forward verbatim into the new
        // `TrackedSession`, so hydrate defensively before any of that.
        if !completed_session.events_hydrated {
            match super::queries::load_completed_events_from_store(&store, session_id).await {
                Ok(events) => {
                    completed_session.events = events;
                    completed_session.events_hydrated = true;
                }
                Err(error) => {
                    tracing::error!(session_id = %session_id, error = %error, "resume_for_handoff_write: failed to hydrate transcript");
                    completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return;
                }
            }
        }

        // A completed row's raw paths are not authority. Authenticate the
        // exact all-null Ordinary tuple or current V83 owner/generation before
        // grants, tokens, admission, raw config, provider dispatch, or active
        // publication. Every refusal restores this exact removed value.
        let prepared_launch = match custody_runtime
            .prepare_handoff_resume(&completed_session.session)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(session_id = %session_id, error = %error, "resume_for_handoff_write custody refused");
                completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return;
            }
        };

        let provider = completed_session.session.provider;
        let depth = completed_session.session.rotation_depth;
        let old_status = completed_session.session.status;

        if let Err(error) = crate::provider_capabilities::refresh_installed_catalog_for_provider(
            provider,
            Arc::clone(&runtime_config),
        )
        .await
        {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "Installed provider catalog refresh failed on handoff resume; using degraded capability evidence"
            );
        }
        let prior_model = completed_session.session.model.clone();
        let prior_context_window = completed_session.session.context_window;
        let prior_budget = completed_session.session.resolved_context_budget.clone();
        let next_budget = crate::provider_capabilities::resolve_new_incarnation_context_budget(
            &completed_session.session,
        );
        match persistence
            .compare_and_update_session_model(
                Arc::clone(&store),
                session_id,
                prior_model.clone(),
                prior_context_window,
                prior_budget,
                prior_model,
                Some(next_budget.active_tokens),
                Some(next_budget.clone()),
            )
            .await
        {
            Ok(true) => install_context_budget(&mut completed_session.session, next_budget),
            Ok(false) => {
                tracing::warn!(
                    %session_id,
                    "Handoff resume context-budget ownership changed before persistence"
                );
                completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return;
            }
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %error,
                    "Handoff resume context-budget persistence failed"
                );
                completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return;
            }
        }

        // Build resume config. CLI providers need claude_session_id for --resume.
        let resume_session_id = completed_session.session.claude_session_id.clone();

        let conversation_history = match provider {
            SessionProvider::Local => {
                Some(Self::events_to_openai_messages(&completed_session.events))
            }
            _ => None,
        };

        // Handoff write uses raw /create_handoff query — no ambient context injection
        // (memory/git context would confuse the model writing a handoff doc). A
        // sandboxed session still receives its custody instruction.
        let system_prompt = super::preamble::prepend_sandbox_custody_instruction(
            None,
            super::preamble::sandbox_custody_instruction_for_session(&completed_session.session),
        );

        #[cfg(test)]
        if let Err(error) = apply_handoff_custody_root_mutation_for_test(&completed_session.session)
        {
            tracing::warn!(session_id = %session_id, error = %error, "resume_for_handoff_write custody mutation seam failed");
            completed
                .write()
                .await
                .insert(session_id, completed_session);
            return;
        }

        // This transitional ContextRead permit is the sole authority for the
        // raw config's cwd/target. Dropping it queues settlement; the later
        // ProviderLaunch/process-lifetime custody slice is intentionally not
        // claimed here.
        let context_permit = match custody_runtime
            .begin_handoff_context_read(&prepared_launch, &completed_session.session)
            .await
        {
            Ok(permit) => permit,
            Err(error) => {
                tracing::warn!(session_id = %session_id, error = %error, "resume_for_handoff_write ContextRead refused");
                completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return;
            }
        };
        let config_working_dir = context_permit.effective_cwd().to_path_buf();
        let config_target_dir = context_permit.cargo_target_dir().map(ToOwned::to_owned);
        let config_execution_scratch = if config_target_dir.is_some() {
            match crate::sandbox::execution_scratch::SandboxExecutionScratch::from_context_permit(
                &context_permit,
            ) {
                Ok(scratch) => scratch,
                Err(error) => {
                    tracing::warn!(session_id = %session_id, %error, "handoff execution scratch refused");
                    drop(context_permit);
                    completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return;
                }
            }
        } else {
            None
        };

        // A6 (G2): the handoff writer is a NEW OS process re-using this
        // session id — re-mint (revoke-then-register) its authority token
        // after successful custody revalidation, so a refusal leaves the
        // preexisting grant and token registry byte-for-byte untouched.
        store.lock().await.remove_controller_grant_v1(session_id);
        let session_token = super::remint_agent_token(&agent_tokens, session_id).await;

        let config = LaunchConfig {
            query: query.clone(),
            title: None,
            agent_role: completed_session.session.agent_role.clone(),
            epic_spawn_ordinal: completed_session.session.epic_spawn_ordinal,
            working_dir: Some(config_working_dir),
            provider: Some(provider),
            model: completed_session.session.model.clone(),
            configured_context_window: completed_session
                .session
                .resolved_context_budget
                .as_ref()
                .and_then(|budget| budget.capacity.configured_tokens),
            max_turns: None,
            system_prompt,
            resume_session_id,
            session_kind: Some(completed_session.session.session_kind),
            project_id: completed_session.session.project_id,
            rsi_session_id: Some(session_id),
            rsi_socket: Some(socket_path.clone()),
            rsi_session_token: Some(session_token.clone()),
            continued_from: completed_session.session.continued_from,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history,
            workflow_id: completed_session.session.workflow_id,
            workflow_id_override: completed_session.session.workflow_id_override,
            max_retries: None,
            group_id: completed_session.session.group_id,
            skip_project_model_default: false,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionContinueResume,
            parent_id: completed_session.session.parent_id,
            effort: completed_session.session.effort.clone(),
            issue_identifier: completed_session.session.issue_identifier.clone(),
            issue_url: completed_session.session.issue_url.clone(),
            issue_tracker_id: completed_session.session.issue_tracker_id.clone(),
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            // This same-ID Reuse path never allocates a replacement root.
            sandbox: None,
            cargo_target_dir: config_target_dir,
            execution_scratch: config_execution_scratch,
            // RSI-006: handoff-write inherits eval status from the parent so
            // hashes stay deterministic across the rotation chain.
            is_eval: completed_session.session.is_eval,
            skip_context_pipeline: completed_session.session.is_eval,
            // Inherit declared capability class from the parent session —
            // a rotation child continues the same declared routing intent.
            capability_class: completed_session.session.capability_class,
            // Inherit tags from parent session (rotation chain continuity).
            tags: completed_session.session.tags.clone(),
            // P1.7: rotation is a context-window event, not a new topology
            // node spawn — children do NOT inherit the parent's binding.
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        };
        #[cfg(test)]
        observe_handoff_custody_config_for_test(session_id, &config);
        drop(context_permit);
        let purpose = config.model_invocation_purpose;
        let parent_invocation_id = {
            let store_ref = store.clone();
            tokio::task::spawn_blocking(move || {
                let store = store_ref.blocking_lock();
                store.session_model_invocation_id(session_id)
            })
            .await
            .map_err(|e| {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "resume_for_handoff_write: failed to join model invocation lookup"
                );
            })
            .ok()
            .and_then(|result| result.ok())
            .flatten()
        };
        let provider_label = format!("{provider:?}");
        let admission_request = crate::model_control::ModelAdmissionRequest {
            purpose,
            provider: Some(provider_label.clone()),
            model: config.model.clone(),
            backend: Some(provider_label.clone()),
            effort: config.effort.clone(),
            trigger: "resume_for_handoff_write".to_string(),
            owner: rotation_owner_from_session(&completed_session.session),
            dedup_key: Some(format!("{purpose}:{session_id}:handoff:{rotation_id:?}")),
            request_fingerprint: Some(hash_request_fingerprint(&[
                purpose.as_str(),
                &format!("{provider:?}"),
                config.model.as_deref().unwrap_or(""),
                &query,
            ])),
            parent_invocation_id,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some(provider_label.as_str()),
                Some(provider_label.as_str()),
                config.model.as_deref(),
            )),
            baseline_input_tokens: completed_session.session.total_input_tokens.unwrap_or(0),
            baseline_output_tokens: completed_session.session.total_output_tokens.unwrap_or(0),
            baseline_cache_creation_tokens: completed_session
                .session
                .total_cache_creation_tokens
                .unwrap_or(0),
            baseline_cache_read_tokens: completed_session
                .session
                .total_cache_read_tokens
                .unwrap_or(0),
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: completed_session.session.work_time_ms.unwrap_or(0),
        };
        let admission_permit = match admit_invocation(&store, admission_request, &event_bus).await {
            Ok(AdmissionDecision::Admitted(permit)) => permit,
            Ok(AdmissionDecision::Duplicate { invocation_id }) => {
                tracing::error!(
                    session_id = %session_id,
                    invocation_id = %invocation_id,
                    "resume_for_handoff_write: duplicate admission blocked backend execution"
                );
                super::revoke_agent_tokens_for_session(&agent_tokens, session_id).await;
                completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return;
            }
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "resume_for_handoff_write: model admission denied"
                );
                super::revoke_agent_tokens_for_session(&agent_tokens, session_id).await;
                completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return;
            }
        };

        // Launch the provider through the single guarded spawn primitive
        // (`&spawn_guard` is the mandatory single-flight witness). The rotation
        // Harness arm rebuilds its `AgentControlHandle` from these daemon-global
        // collaborator `Arc`s (cheap clones; only the Harness leaf reads them),
        // so a rotated Harness session keeps an IDENTICAL tool set — including the
        // native `rsi_control` + `schedule_wake` tools — via the single
        // `default_tools` source of truth. The bound caller id is this session's
        // own id.
        let launcher = super::provider_spawn::FreshLauncher {
            runtime_config: Arc::clone(&runtime_config),
            harness: super::provider_spawn::FreshHarnessCtx {
                codegraph_handle: codegraph_handle.clone(),
                custody_runtime: custody_runtime.clone(),
                active: Arc::clone(&active),
                completed: Arc::clone(&completed),
                store: Arc::clone(&store),
                event_bus: Arc::clone(&event_bus),
                model_call_settlements: model_call_settlements.clone(),
                spawn_coordinator: Arc::clone(&spawn_coordinator),
                memory_handle: memory_handle.clone(),
                initial_admission_permit: admission_permit.clone(),
                resolved_context_budget: completed_session
                    .session
                    .resolved_context_budget
                    .clone()
                    .expect("handoff resumes carry a resolved context budget"),
                bound_session_id: session_id,
            },
        };

        #[cfg(test)]
        let launch_result = super::launch::take_controller_candidate_test_process(session_id)
            .map_or_else(
                || {
                    super::provider_spawn::spawn_provider_process(
                        provider,
                        &config,
                        &launcher,
                        &admission_permit,
                        &spawn_guard,
                    )
                },
                Ok,
            );
        #[cfg(not(test))]
        let launch_result = super::provider_spawn::spawn_provider_process(
            provider,
            &config,
            &launcher,
            &admission_permit,
            &spawn_guard,
        );
        let (process, event_rx) = match launch_result {
            Ok(pair) => pair,
            Err(e) => {
                if let Err(settle_error) = complete_invocation(
                    &store,
                    &admission_permit,
                    InvocationCompletion {
                        error_class: Some("spawn_failed".to_string()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    &event_bus,
                )
                .await
                {
                    tracing::warn!(
                        error = %settle_error,
                        session_id = %session_id,
                        "Failed to settle handoff resume admission after spawn error"
                    );
                }
                tracing::error!(session_id = %session_id, error = %e, "resume_for_handoff_write: provider launch failed");
                super::revoke_agent_tokens_for_session(&agent_tokens, session_id).await;
                // Re-insert into completed so user sees the session
                completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return;
            }
        };
        let (stop_tx, stop_rx) = mpsc::channel(1);

        // Reconstruct TrackedSession reusing session UUID, carrying forward events/metrics
        let initial_sequence = completed_session
            .events
            .last()
            .map(|e| e.sequence + 1)
            .unwrap_or(0);
        let mut session = completed_session.session;
        session.status = SessionStatus::Starting;
        session.updated_at = chrono::Utc::now();
        // Rotation-child reset (Q1): child is a new provider run with its own
        // approval-wait lifetime. Parent's accumulated wait stays persisted on
        // the parent row; inheritance would conflate two runs.
        session.approval_wait_ms = Some(0);

        let spawn_generation = Self::next_spawn_generation_from(&spawn_epoch);
        let tracked = TrackedSession {
            rotation:
                super::rotation_coordinator::RotationCoordinator::new_writing_handoff_with_rotation_id(
                    session_id,
                    depth,
                    context_rotation_enabled && session.rotation_disabled_at.is_none(),
                    rotation_id,
            ),
            session: session.clone(),
            spawn_generation,
            events: completed_session.events,
            turn_metrics: completed_session.turn_metrics,
            process: Some(process),
            deferred_successor_start_gate: None,
            stop_tx,
            interrupt_requested: false,
            pending_archive: false,
            live_input_tokens: 0,
            live_output_tokens: 0,
            live_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: 0,
            daemon_output_tokens: 0,
            daemon_tokens_at_last_api_update: 0,
            codex_context_tokens: 0,
            pipeline_artifact: None,
            memory_flush_compaction_count: None,
            pending_question: None,
            approval_wait_start: None,
            approval_wait_total_ms: 0,
            // TD1: handoff-write resume reuses the SAME session id/row (not a new
            // child) — work_time_ms must inherit like continue, not reset. No
            // explicit `session.work_time_ms` assignment above (unlike
            // approval_wait_ms) is the mechanism: the reused floor is picked up at
            // the next Running-entry (monitor.rs Site 1).
            work_run_start: None,
            work_time_base_ms: 0,
            received_meaningful_output: false,
            exit_code: None,
            retry_attempt: 0,
            max_retries: 0,
            last_event_at: chrono::Utc::now(),
            stall_interrupted: false,
            last_usage_update: None,
            last_mismatch_warn: None,
            last_classified_at: None,
            classification_count: 0,
            last_verdict: None,
        };

        active.write().await.insert(session_id, tracked);

        // B1: the guard covers check -> launch -> insert ONLY. Drop it now,
        // before the inline `monitor_session` await below, so a same-task
        // 2nd-generation rotation can re-acquire this id's guard without
        // self-deadlocking the non-reentrant `tokio::Mutex`. The live child is
        // already in `active`, so any contended racer now adopts it.
        drop(spawn_guard);

        // Persist status, create user event, publish events
        if let Err(e) = persistence
            .update_status(session_id, SessionStatus::Starting)
            .await
        {
            tracing::warn!(error = %e, session_id = %session_id, "Failed to persist handoff-write status");
        }
        drop(cwd_admission_guard);
        #[cfg(test)]
        super::launch::pause_controller_candidate_test(
            session_id,
            super::launch::ControllerCandidateTestPhase::SameIdBeforeReconstruction,
        )
        .await;
        let controller_grant = Self::reconstruct_live_same_id_controller_grant(
            &active,
            &store,
            &agent_tokens,
            session_id,
            session.project_id,
            provider,
            &session_token,
        )
        .await;
        if controller_grant == super::SameIdControllerGrantOutcome::EstablishmentInvalid {
            super::revoke_agent_tokens_for_session(&agent_tokens, session_id).await;
        }
        {
            let store_ref = store.clone();
            let invocation_id = admission_permit.invocation_id();
            if let Err(e) = tokio::task::spawn_blocking(move || {
                let store = store_ref.blocking_lock();
                store.set_session_model_invocation(session_id, Some(invocation_id))
            })
            .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to bind handoff-write session row to active model invocation"
                );
            }
        }

        let user_event = Self::create_user_event(session_id, initial_sequence, &query);
        {
            let mut active_guard = active.write().await;
            if let Some(t) = active_guard.get_mut(&session_id) {
                t.events.push(user_event.clone());
            }
        }
        match persistence.insert_event(user_event.clone()).await {
            Ok(db_id) => {
                let mut active_guard = active.write().await;
                if let Some(t) = active_guard.get_mut(&session_id)
                    && let Some(last) = t.events.last_mut()
                {
                    last.id = db_id;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, session_id = %session_id, "Failed to persist handoff-write user event");
            }
        }

        event_bus.publish(DaemonEvent::SessionStatusChanged {
            session_id,
            old_status,
            new_status: SessionStatus::Starting,
        });
        event_bus.publish(DaemonEvent::ConversationEvent {
            session_id,
            event: user_event,
        });

        {
            let query_tokens = counter.count(&query);
            let mut active_guard = active.write().await;
            if let Some(t) = active_guard.get_mut(&session_id) {
                t.daemon_input_tokens += query_tokens;
                t.session.daemon_input_tokens = Some(t.daemon_input_tokens);
            }
        }

        tracing::info!(session_id = %session_id, "Resumed session for handoff write (direct, no RPC)");

        // Re-enter monitor loop (boxed to break async recursion until Phase 6 replaces with tokio::spawn)
        let provider_session = Box::new(crate::provider::CliProviderSession::new(event_rx));
        // Rotation sessions use CLI providers — Single turn policy, no dynamic tool registry.
        let turn_controller = crate::turn_controller::TurnController::new(
            crate::turn_controller::ContinuationPolicy::Single,
        );
        let tool_registry = std::sync::Arc::new(crate::tool_registry::ToolRegistry::new());
        Box::pin(Self::monitor_session(
            session_id,
            spawn_generation,
            provider_session,
            active,
            completed,
            event_bus,
            stop_rx,
            store.clone(),
            model_call_settlements,
            persistence,
            initial_sequence,
            context_rotation_enabled,
            socket_path,
            counter,
            memory_handle,
            retry_tx,
            tool_registry,
            turn_controller,
            runtime_config,
            spawn_coordinator.clone(),
            agent_tokens,
            spawn_epoch,
            agent_message_arbiter,
            codegraph_handle,
            custody_runtime,
        ))
        .await;
    }

    /// Rotate a completed/stopped session: archive parent, spawn fresh child.
    ///
    /// Unlike rotate_session(), this pulls from the completed map and doesn't
    /// need to interrupt a process. The child starts with a CLEAN context:
    /// - If a handoff filepath was detected, the child uses `/resume_handoff <path>`
    /// - Otherwise resumes the predecessor's durable task query
    ///
    /// The child does NOT resume the parent's Claude session (no resume_session_id)
    /// so it starts with a fresh context window.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn rotate_completed_session(
        session_id: Uuid,
        handoff_filepath: Option<String>,
        rotation_id_for_log: Option<String>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: Arc<crate::bus::EventBus>,
        store: Arc<tokio::sync::Mutex<Store>>,
        model_call_settlements: crate::model_control::call_control::ModelCallSettlementHandle,
        persistence: PersistenceHandle,
        context_rotation_enabled: bool,
        socket_path: std::path::PathBuf,
        counter: Arc<monitor::TokenCounter>,
        mem_handle: Option<crate::memory::worker::MemoryHandle>,
        retry_tx: mpsc::Sender<Uuid>,
        runtime_config: Arc<crate::config::RuntimeConfig>,
        spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
        agent_tokens: Arc<RwLock<super::AgentTokenRegistry>>,
        spawn_epoch: Arc<std::sync::atomic::AtomicU64>,
        agent_message_arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
        codegraph_handle: Option<crate::codegraph::IndexHandle>,
        custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
    ) {
        Box::pin(Self::decide_rotation_successor(
            session_id,
            handoff_filepath,
            rotation_id_for_log,
            active,
            completed,
            event_bus,
            store,
            model_call_settlements,
            persistence,
            context_rotation_enabled,
            socket_path,
            counter,
            mem_handle,
            retry_tx,
            runtime_config,
            spawn_coordinator,
            agent_tokens,
            spawn_epoch,
            agent_message_arbiter,
            codegraph_handle,
            custody_runtime,
            RotationPredecessorSource::Completed,
        ))
        .await;
    }

    /// The rotation successor decider. `rotate_completed_session` is the live
    /// entry (`Completed` predecessor); restart recovery (`recovery.rs`) is
    /// the only caller passing `RecoveredOpenIntent`.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) async fn decide_rotation_successor(
        session_id: Uuid,
        handoff_filepath: Option<String>,
        rotation_id_for_log: Option<String>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: Arc<crate::bus::EventBus>,
        store: Arc<tokio::sync::Mutex<Store>>,
        model_call_settlements: crate::model_control::call_control::ModelCallSettlementHandle,
        persistence: PersistenceHandle,
        context_rotation_enabled: bool,
        socket_path: std::path::PathBuf,
        counter: Arc<monitor::TokenCounter>,
        mem_handle: Option<crate::memory::worker::MemoryHandle>,
        retry_tx: mpsc::Sender<Uuid>,
        runtime_config: Arc<crate::config::RuntimeConfig>,
        spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
        agent_tokens: Arc<RwLock<super::AgentTokenRegistry>>,
        spawn_epoch: Arc<std::sync::atomic::AtomicU64>,
        agent_message_arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
        codegraph_handle: Option<crate::codegraph::IndexHandle>,
        custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
        predecessor_source: RotationPredecessorSource,
    ) {
        let memory_handle = mem_handle;
        let rotation_id_for_log = Some(rotation_id_for_log.unwrap_or_else(|| {
            format!(
                "untracked:{session_id}:{}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            )
        }));
        macro_rules! refuse {
            ($code:expr) => {{
                Self::record_rotation_refusal(
                    session_id,
                    rotation_id_for_log.as_deref(),
                    $code,
                    &completed,
                    &event_bus,
                    &persistence,
                    &store,
                )
                .await;
                return;
            }};
        }
        let predecessor_spawn_guard =
            super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        let cwd_admission_guard =
            super::spawn_single_flight::acquire_provider_cwd_admission().await;
        let already_settled = {
            let store_guard = store.lock().await;
            store_guard.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND (event_type='completed' OR event_type='suppressed_final_handoff' OR event_type LIKE 'refused:%'))",
                rusqlite::params![session_id.to_string(), rotation_id_for_log.as_deref()],
                |row| row.get::<_, bool>(0),
            )
        };
        match already_settled {
            Ok(true) => return, // A replay of the same decision already has its one terminal event.
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%session_id, %error, "Rotation terminal-event lookup failed closed");
                refuse!("event_lookup_failed");
            }
        }
        let settlement_fence = {
            store
                .lock()
                .await
                .session_or_custody_has_settlement_fence(session_id)
        };
        match settlement_fence {
            Ok(false) => {}
            Ok(true) => {
                tracing::warn!(%session_id, "rotation refused by source-worktree settlement fence");
                refuse!("settlement_fence");
            }
            Err(error) => {
                tracing::warn!(%session_id, %error, "rotation settlement-fence check failed closed");
                refuse!("settlement_fence_error");
            }
        }
        // RPC-1 C5 / K2: a predecessor covered by a live manager retirement
        // witness is not rotated; its continuation authority was retired.
        let retired = store
            .lock()
            .await
            .manager_lead_program_outcome_superseded(session_id);
        match retired {
            Ok(false) => {}
            Ok(true) => refuse!("retired"),
            Err(error) => {
                tracing::warn!(%session_id, %error, "rotation retirement check failed closed");
                refuse!("retired_check_error");
            }
        }

        // Bound short failure bursts by recent successful rotations in this lineage.
        // The attempted rotation is the Kth event; the previous K-1 successors
        // within the window prove the cascade is rapid.
        {
            let rate_limited = {
                let store_guard = store.lock().await;
                rotation_rate_limit_reached(&store_guard, session_id)
            };
            match rate_limited {
                Ok(true) => refuse!("rate_limited"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%session_id, %error, "Rotation rate history lookup failed closed");
                    refuse!("rate_limited");
                }
            }
        }
        // Retain a high corruption/loop safety ceiling, independent of normal
        // lineage lifetime. Ordinary depth is no longer a rotation policy.
        {
            let completed_guard = completed.read().await;
            if let Some(parent) = completed_guard.get(&session_id)
                && parent.session.rotation_depth >= ROTATION_DEPTH_SAFETY_CEILING
            {
                drop(completed_guard);
                refuse!("depth_ceiling");
            }
        }
        // ─────────────────────────────────────────────────────────────────────────

        // Remove the exact completed snapshot, without rewriting it or
        // publishing an event.  Rotation authorization must precede child
        // construction and every reservation/effect.
        let mut parent_completed = {
            let mut completed_guard = completed.write().await;
            let Some(parent) = completed_guard.remove(&session_id) else {
                tracing::error!(session_id = %session_id, "Rotation failed: parent not in completed map");
                drop(completed_guard);
                refuse!("predecessor_missing");
            };
            parent
        };
        let start_head = {
            let guard = store.lock().await;
            rotation_id_for_log
                .as_deref()
                .and_then(|rid| rotation_start_head(&guard, session_id, rid))
        };
        let bound_commit = if let (Some(path), Some(start_head)) =
            (handoff_filepath.as_deref(), start_head.as_deref())
        {
            bound_handoff_commit(&parent_completed.session, path, start_head).await
        } else {
            None
        };
        let handoff_filepath = if bound_commit.is_some() {
            handoff_filepath
        } else {
            if let (Some(path), Some(rid)) = (handoff_filepath, rotation_id_for_log.as_deref()) {
                let metadata = serde_json::json!({"handoff_filepath": path}).to_string();
                let _ = persistence
                    .log_rotation_event(
                        session_id,
                        rid,
                        "writing_handoff",
                        "handoff_rejected_unbound",
                        Some(metadata),
                    )
                    .await;
            }
            None
        };
        if parent_completed.session.handoff_filepath != handoff_filepath {
            parent_completed.session.handoff_filepath = handoff_filepath.clone();
            let persisted = persistence
                .update_session_metadata(parent_completed.session.clone())
                .await;
            let persisted = match persisted {
                Ok(()) => persistence.barrier().await,
                Err(error) => Err(error),
            };
            if let Err(error) = persisted {
                tracing::error!(%session_id, %error, "Failed to persist bound handoff on predecessor");
                completed.write().await.insert(session_id, parent_completed);
                refuse!("handoff_persistence");
            }
        }
        let rotation_candidate = match custody_runtime
            .prepare_rotation_successor_from(&parent_completed.session, predecessor_source)
            .await
        {
            Ok(candidate) => candidate,
            Err(error) => {
                tracing::warn!(session_id = %session_id, error = %error, "Rotation predecessor custody refused");
                completed.write().await.insert(session_id, parent_completed);
                let code = match &error {
                    DaemonError::StructuredRpc { data, .. } => {
                        data["error"]["code"].as_str().unwrap_or("custody_changed")
                    }
                    _ => "custody_changed",
                };
                refuse!(code);
            }
        };
        // Nothing belonging to the successor exists until the exact
        // predecessor has authenticated: in particular no child UUID, query,
        // controller candidate, token, admission, or durable reservation.
        #[cfg(test)]
        let child_id = take_rotation_child_id_for_test(session_id).unwrap_or_else(Uuid::new_v4);
        #[cfg(not(test))]
        let child_id = Uuid::new_v4();
        // Slice 2 binds a detected path to the handoff turn before this choice.
        let child_query = if let Some(ref path) = handoff_filepath {
            format!("/resume_handoff {}", path)
        } else {
            let task_result = {
                let store_guard = store.lock().await;
                resolve_rotation_task_query(&store_guard, &parent_completed.session)
            };
            let task = match task_result {
                Ok(Some((_, task))) => task,
                Ok(None) => {
                    completed.write().await.insert(session_id, parent_completed);
                    refuse!("no_task");
                }
                Err(error) => {
                    tracing::warn!(%session_id, %error, "Rotation task resolution failed");
                    completed.write().await.insert(session_id, parent_completed);
                    refuse!("task_lookup_failed");
                }
            };
            format!(
                "{task}\n\n{}",
                rotation_task_pointer(&parent_completed.session, PointerKind::Rotation).await
            )
        };
        let parent_rotation_depth = parent_completed.session.rotation_depth;
        let child_budget = crate::provider_capabilities::resolve_new_incarnation_context_budget(
            &parent_completed.session,
        );
        let mut child_session = Session {
            context_fill_pct: None,
            id: child_id,
            provider: parent_completed.session.provider,
            claude_session_id: None,
            query: child_query.clone(),
            title: None,
            agent_role: parent_completed.session.agent_role.clone(),
            epic_spawn_ordinal: parent_completed.session.epic_spawn_ordinal,
            description: None,
            short_summary: None,
            working_dir: parent_completed.session.working_dir.clone(),
            git_branch: parent_completed.session.git_branch.clone(),
            status: SessionStatus::Starting,
            project_id: parent_completed.session.project_id,
            pinned_at: None,
            testing_needed_at: None,
            // Rotation-disabled is a safety setting, so a manual rotation of
            // a disabled predecessor must not silently re-enable automatic
            // rotation on its successor.
            rotation_disabled_at: parent_completed.session.rotation_disabled_at,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: parent_completed.session.model.clone(),
            input_tokens: None,
            output_tokens: None,
            context_window: Some(child_budget.active_tokens),
            resolved_context_budget: Some(child_budget),
            total_input_tokens: None,
            total_output_tokens: None,
            session_kind: parent_completed.session.session_kind,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: Some(session_id),
            context_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: parent_completed.session.active_task.clone(),
            group_id: parent_completed.session.group_id,
            pipeline_artifact: None,
            workflow_id: parent_completed.session.workflow_id,
            workflow_id_override: parent_completed.session.workflow_id_override,
            pending_question: None,
            pending_archive: false,
            rotation_depth: parent_rotation_depth + 1,
            retry_attempt: None,
            max_retries: None,
            effort: parent_completed.session.effort.clone(),
            issue_identifier: parent_completed.session.issue_identifier.clone(),
            issue_url: parent_completed.session.issue_url.clone(),
            issue_tracker_id: parent_completed.session.issue_tracker_id.clone(),
            scheduled_job_id: None,
            rating: None,
            // Populated below inside `spawn_rotation_child` once the child's
            // system_prompt is resolved (the context pipeline assembles a fresh
            // prompt for the rotated session). See ~line 937 — the canonical
            // hash helper is invoked on (system_prompt + \x00 + query) before
            // persistence. Tech Wizard veto (plan §1.3): production rotation
            // children must never reach persistence with `None`.
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            // Rotation-child reset (Q1): fresh approval-wait lifetime.
            approval_wait_ms: Some(0),
            // Children always start outside an approval interval — None.
            approval_started_at: None,
            // TD1 (D-continue semantics): a rotation child is a NEW row
            // (`continued_from` lineage) starting fresh, mirroring approval_wait_ms.
            work_time_ms: Some(0),
            // The authenticated predecessor candidate supplies either the
            // exact all-null Ordinary tuple or the exact live Transfer tuple
            // below.  There is no inherited fallback or reallocation path.
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            // Rotation child inherits the hierarchical parent slot —
            // rotation is a context-window event, not a reparent.
            tag: String::new(),
            tags: Vec::new(),
            parent_id: parent_completed.session.parent_id,
            // Lead pointer is intentionally not inherited at construction
            // time. Phase 2's rotation lead-inherit hook re-points the
            // parent Epic's lead_session_id to this child after persist.
            lead_session_id: None,
            // RSI-006: rotation child inherits the parent's eval-replay flag.
            is_eval: parent_completed.session.is_eval,
            // Declared capability class — rotation carries the parent's
            // declared intent forward to the fresh context window.
            capability_class: parent_completed.session.capability_class,
            // P1.7: rotation is a context-window event, not a new topology
            // node spawn — children do NOT inherit the parent's binding.
            // Per plan: "RotateSession/ContinueSession — do not copy
            // topology_node_id; child of a rotation is a continuation,
            // not a new topology node spawn."
            topology_node_id: None,
            topology_iteration: 0,
            // V99: populated from the provider handshake / result event, not at
            // construction. A session that never reaches those has none of these facts.
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        };

        if let Err(error) =
            crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
                &rotation_candidate,
                &parent_completed.session,
                &mut child_session,
            )
        {
            tracing::warn!(session_id = %session_id, error = %error, "Rotation successor tuple refused");
            completed.write().await.insert(session_id, parent_completed);
            refuse!("successor_tuple");
        }

        // Always start fresh — never resume parent session after rotation.
        // The whole point of rotation is a clean context window.
        // If no handoff filepath was detected, the child starts without one,
        // which is better than resuming a bloated 65%+ context.
        //
        // Parent archival is deferred into spawn_rotation_child (Phase 4 saga):
        // the parent is archived ONLY after the child is confirmed launched.
        // If child launch fails, the parent is rolled back into the completed map.
        Self::spawn_rotation_child(
            session_id,
            None,
            child_query,
            child_session,
            rotation_candidate,
            Some(parent_completed),
            rotation_id_for_log.clone(),
            bound_commit,
            active,
            Arc::clone(&completed),
            Arc::clone(&event_bus),
            Arc::clone(&store),
            model_call_settlements,
            persistence.clone(),
            context_rotation_enabled,
            socket_path,
            counter,
            memory_handle,
            retry_tx,
            runtime_config,
            spawn_coordinator,
            agent_tokens,
            spawn_epoch,
            agent_message_arbiter,
            codegraph_handle,
            custody_runtime,
            Some(cwd_admission_guard),
            Some(predecessor_spawn_guard),
        )
        .await;
        // The launch path records `completed` at the publication point, or
        // its own `refused:lead_transfer` when publication is refused (C4).
        // Any other return before that point is a refusal, including
        // admission and durable-reservation failures. Binding verification
        // is Slice 2.
        let established = if let Some(ref rid) = rotation_id_for_log {
            store.lock().await.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND (event_type='completed' OR event_type LIKE 'refused:%'))",
                rusqlite::params![session_id.to_string(), rid],
                |row| row.get::<_, bool>(0),
            ).unwrap_or(false)
        } else {
            false
        };
        if !established {
            Self::record_rotation_refusal(
                session_id,
                rotation_id_for_log.as_deref(),
                "launch_failed",
                &completed,
                &event_bus,
                &persistence,
                &store,
            )
            .await;
        }
    }

    /// Shared tail of context rotation: launch provider child, persist it, and monitor it.
    ///
    /// Called by both `rotate_session` (active → child) and `rotate_completed_session`
    /// (completed → child). All provider-specific launch logic lives here exactly once
    /// so the two paths cannot diverge.
    ///
    /// `resume_session_id` controls whether the child resumes the parent's session
    /// (bloated context) or starts fresh (clean context). When a handoff doc exists,
    /// callers should pass `None` so the child starts clean.
    ///
    /// `parent_for_archival` — if `Some`, the parent session is archived ONLY after
    /// the child is confirmed launched (saga-style). If child launch fails, the parent
    /// is rolled back into the completed map. If `None`, archival was already handled
    /// by the caller.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn spawn_rotation_child(
        parent_id: Uuid,
        resume_session_id: Option<String>,
        query: String,
        mut child_session: Session,
        rotation_candidate: crate::sandbox::custody::RotationCustodyCandidate,
        mut parent_for_archival: Option<CompletedSession>,
        rotation_id_for_log: Option<String>,
        handoff_commit: Option<String>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: Arc<crate::bus::EventBus>,
        store: Arc<tokio::sync::Mutex<Store>>,
        model_call_settlements: crate::model_control::call_control::ModelCallSettlementHandle,
        persistence: PersistenceHandle,
        context_rotation_enabled: bool,
        socket_path: std::path::PathBuf,
        counter: Arc<monitor::TokenCounter>,
        memory_handle: Option<crate::memory::worker::MemoryHandle>,
        retry_tx: mpsc::Sender<Uuid>,
        runtime_config: Arc<crate::config::RuntimeConfig>,
        spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
        agent_tokens: Arc<RwLock<super::AgentTokenRegistry>>,
        spawn_epoch: Arc<std::sync::atomic::AtomicU64>,
        agent_message_arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
        codegraph_handle: Option<crate::codegraph::IndexHandle>,
        custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
        preheld_cwd_admission: Option<super::spawn_single_flight::ProviderCwdAdmissionGuard>,
        predecessor_spawn_guard: Option<super::spawn_single_flight::SpawnGuard>,
    ) {
        let cwd_admission_guard = match preheld_cwd_admission {
            Some(guard) => guard,
            None => super::spawn_single_flight::acquire_provider_cwd_admission().await,
        };
        #[cfg(test)]
        let mut rotation_candidate = rotation_candidate;
        child_session.provider =
            super::provider_spawn::effective_sync_provider(child_session.provider);
        if let Err(error) = crate::provider_capabilities::refresh_installed_catalog_for_provider(
            child_session.provider,
            Arc::clone(&runtime_config),
        )
        .await
        {
            tracing::warn!(
                parent_id = %parent_id,
                %error,
                "Installed provider catalog refresh failed for rotation child; using degraded capability evidence"
            );
        }
        let child_budget =
            crate::provider_capabilities::resolve_new_incarnation_context_budget(&child_session);
        install_context_budget(&mut child_session, child_budget);
        let initial_child_id = child_session.id;
        if let Err(error) = preflight_rotation_lead_transfer(&store, parent_id).await {
            tracing::warn!(
                %parent_id,
                error = %error,
                "Context rotation deferred by durable Epic lead fence"
            );
            if let Some(parent) = parent_for_archival.take() {
                completed.write().await.insert(parent_id, parent);
            }
            return;
        }
        let assigned_idea = {
            let store = store.lock().await;
            store.assigned_idea_for_session_v1(parent_id)
        };
        let assigned_idea = match assigned_idea {
            Ok(idea) => idea,
            Err(error) => {
                tracing::error!(
                    %parent_id,
                    error = %error,
                    "Rotation controller lookup failed closed"
                );
                if let Some(parent) = parent_for_archival.take() {
                    completed.write().await.insert(parent_id, parent);
                }
                return;
            }
        };
        let controller_transfer: Option<(
            crate::idea_control::IdeaControllerTransferHandle,
            IdeaControllerReservationV1,
        )> = if let Some(idea) = assigned_idea {
            let transfer = match crate::idea_control::IdeaControllerTransferHandle::for_system(
                Arc::clone(&store),
                idea.project_id,
                idea.id,
                Arc::new(crate::idea_control::SystemIdeaControllerClock),
            )
            .await
            {
                Ok(transfer) => transfer,
                Err(error) => {
                    tracing::error!(
                        %parent_id,
                        error = %error,
                        "Rotation controller transfer binding failed"
                    );
                    if let Some(parent) = parent_for_archival.take() {
                        completed.write().await.insert(parent_id, parent);
                    }
                    return;
                }
            };
            let rotation_key = rotation_id_for_log
                .clone()
                .unwrap_or_else(|| initial_child_id.to_string());
            let transfer_intent_key = format!("rotation:{parent_id}:{rotation_key}");
            let reserved = match transfer
                .reserve(&ReserveIdeaControllerRequestV1 {
                    expected_row_version: idea.row_version,
                    transfer_intent_key,
                })
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::error!(
                        %parent_id,
                        error = %error,
                        "Rotation controller reservation failed"
                    );
                    if let Some(parent) = parent_for_archival.take() {
                        completed.write().await.insert(parent_id, parent);
                    }
                    return;
                }
            };
            let Some(reservation) = reserved.reservation else {
                tracing::error!(%parent_id, "Rotation reserve returned no reservation");
                if let Some(parent) = parent_for_archival.take() {
                    completed.write().await.insert(parent_id, parent);
                }
                return;
            };
            child_session.id = reservation.candidate_session_id;
            child_session.project_id = Some(idea.project_id);
            Some((transfer, reservation))
        } else {
            None
        };
        let child_id = child_session.id;

        #[cfg(test)]
        apply_rotation_bind_mutation_for_test(
            child_id,
            &mut rotation_candidate,
            &mut child_session,
        );

        // Single-flight spawn guard (structural). `child_id` is a brand-new
        // UUID, so no concurrent same-id spawn can exist and the adopt branch is
        // skipped; the guard keeps the check -> launch -> insert invariant
        // uniform across every provider-spawn entry point. It covers ONLY that
        // span and is dropped explicitly BEFORE the inline `monitor_session`
        // await below: the monitor runs this child's post-finalization rotation
        // inline on THIS task, and when the child itself rotates it re-enters
        // `acquire_spawn_guard(child_id)` (via `resume_for_handoff_write`).
        // Holding the guard across that await would self-deadlock the
        // non-reentrant `tokio::Mutex` (B1).
        let spawn_guard = super::spawn_single_flight::acquire_spawn_guard(child_id).await;

        let provider = child_session.provider;

        let query_for_event = query.clone();

        // A6 (G3): the rotation child is a fresh session id — mint and
        // register its authority token BEFORE the config is built, so
        // registration precedes the spawn (launch_session's
        // fast-first-callback ordering). Through the shared re-mint helper
        // the revoke half is a no-op for this fresh UUID.
        let session_token = super::remint_agent_token(&agent_tokens, child_id).await;
        let prospective_controller_token = session_token.clone();

        let purpose = rsi_common::model_control::ModelInvocationPurpose::SessionRotateChild;
        let parent_invocation_id = {
            let store_ref = store.clone();
            tokio::task::spawn_blocking(move || {
                let store = store_ref.blocking_lock();
                store.session_model_invocation_id(parent_id)
            })
            .await
            .map_err(|e| {
                tracing::error!(
                    parent_id = %parent_id,
                    child_id = %child_id,
                    error = %e,
                    "rotation child: failed to join parent invocation lookup"
                );
            })
            .ok()
            .and_then(|result| result.ok())
            .flatten()
        };
        let provider_label = format!("{provider:?}");
        let admission_request = crate::model_control::ModelAdmissionRequest {
            purpose,
            provider: Some(provider_label.clone()),
            model: child_session.model.clone(),
            backend: Some(provider_label.clone()),
            effort: child_session.effort.clone(),
            trigger: "rotate_completed_session".to_string(),
            owner: rotation_owner_from_session(&child_session),
            dedup_key: Some(format!("{purpose}:{child_id}")),
            request_fingerprint: Some(hash_request_fingerprint(&[
                purpose.as_str(),
                &format!("{provider:?}"),
                child_session.model.as_deref().unwrap_or(""),
                &child_session.query,
            ])),
            parent_invocation_id,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some(provider_label.as_str()),
                Some(provider_label.as_str()),
                child_session.model.as_deref(),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let admission_permit = match admit_invocation(&store, admission_request, &event_bus).await {
            Ok(AdmissionDecision::Admitted(permit)) => permit,
            Ok(AdmissionDecision::Duplicate { invocation_id }) => {
                tracing::error!(
                    parent_id = %parent_id,
                    child_id = %child_id,
                    invocation_id = %invocation_id,
                    "Rotation child duplicate admission blocked backend execution"
                );
                if let Some(parent) = parent_for_archival {
                    completed.write().await.insert(parent_id, parent);
                }
                super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                release_rotation_controller_reservation(
                    controller_transfer.as_ref(),
                    ControllerReleaseReasonV1::LaunchAdmissionFailed,
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::error!(
                    parent_id = %parent_id,
                    child_id = %child_id,
                    error = %e,
                    "Rotation child admission denied"
                );
                if let Some(parent) = parent_for_archival {
                    completed.write().await.insert(parent_id, parent);
                }
                super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                release_rotation_controller_reservation(
                    controller_transfer.as_ref(),
                    ControllerReleaseReasonV1::LaunchAdmissionFailed,
                )
                .await;
                return;
            }
        };

        // The final controller candidate id is now durable before any custody
        // bind, context read, provider dispatch, or active publication.
        let store_ref = store.clone();
        let invocation_id = admission_permit.invocation_id();
        let durable_child = child_session.clone();
        // One rotation id names the reservation marker, the publication and
        // any refusal, so C2 can tell this current row from a legacy one.
        let publication_rotation_id = rotation_id_for_log.clone().unwrap_or_else(|| {
            format!(
                "untracked:{parent_id}:{}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            )
        });
        let reservation_rotation_id = publication_rotation_id.clone();
        let child_persisted = tokio::task::spawn_blocking(move || {
            let mut store = store_ref.blocking_lock();
            store.insert_reserved_rotation_session_with_invocation(
                &durable_child,
                invocation_id,
                &reservation_rotation_id,
            )
        })
        .await;
        let reclaim_prepared = matches!(
            &child_persisted,
            Ok(Err(error)) if crate::error::is_reclaim_prepared_error(error)
        );
        if !matches!(child_persisted, Ok(Ok(_))) {
            let _ = complete_invocation(
                &store,
                &admission_permit,
                InvocationCompletion {
                    error_class: Some(if reclaim_prepared {
                        "reclaim_prepared".to_string()
                    } else {
                        "durable_row_failed".to_string()
                    }),
                    confidence: Some(ModelUsageConfidence::Unavailable),
                    ..InvocationCompletion::default()
                },
                &event_bus,
            )
            .await;
            super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
            release_rotation_controller_reservation(
                controller_transfer.as_ref(),
                ControllerReleaseReasonV1::DurableRowFailed,
            )
            .await;
            if let Some(parent) = parent_for_archival.take() {
                completed.write().await.insert(parent_id, parent);
            }
            if reclaim_prepared && completed.read().await.contains_key(&parent_id) {
                let handoff = query.strip_prefix("/resume_handoff ").map(str::to_string);
                schedule_prepared_rotation_retry(
                    parent_id,
                    handoff,
                    rotation_id_for_log,
                    active,
                    completed,
                    event_bus,
                    store,
                    model_call_settlements,
                    persistence,
                    context_rotation_enabled,
                    socket_path,
                    counter,
                    memory_handle,
                    retry_tx,
                    runtime_config,
                    spawn_coordinator,
                    agent_tokens,
                    spawn_epoch,
                    agent_message_arbiter,
                    codegraph_handle,
                    custody_runtime,
                );
            }
            return;
        }
        #[cfg(test)]
        if let Some(mutation) = take_rotation_pre_bind_predecessor_mutation_for_test(child_id) {
            let sql = match mutation {
                RotationPreBindPredecessorMutationForTest::LineageRouting => {
                    "UPDATE sessions SET parent_id='00000000-0000-4000-8000-000000000001', \
                     updated_at='2099-01-01T00:00:00.000000000Z' WHERE id=?1"
                }
                RotationPreBindPredecessorMutationForTest::ExecutionPrompt => {
                    "UPDATE sessions SET query='durable predecessor prompt changed before bind', \
                     updated_at='2099-01-01T00:00:00.000000000Z' WHERE id=?1"
                }
                RotationPreBindPredecessorMutationForTest::ModelInvocation => {
                    "UPDATE sessions \
                     SET model_invocation_id='00000000-0000-4000-8000-000000000003', \
                         updated_at='2099-01-01T00:00:00.000000000Z' WHERE id=?1"
                }
            };
            store
                .lock()
                .await
                .conn
                .execute(sql, [parent_id.to_string()])
                .expect("mutate durable predecessor before rotation bind");
        }
        #[cfg(test)]
        if let Some(winner) = take_rotation_competing_winner_for_test(child_id) {
            let mut store_guard = store.lock().await;
            let current = store_guard
                .live_custody_for_session(parent_id)
                .expect("competing rotation predecessor remains current");
            store_guard
                .insert_session(&winner)
                .expect("persist competing rotation winner");
            store_guard
                .bind_reserved_session_custody(
                    winner.id,
                    crate::store::sandbox_custody::SessionCustodyBinding::Transfer {
                        custody_id: current.custody_id,
                        from_session_id: parent_id,
                        generation: current.generation,
                        cause: crate::store::sandbox_custody::CustodyCause::Rotation,
                        origin_session_id: Some(parent_id),
                        scheduled_job_id: None,
                    },
                )
                .expect("commit competing rotation winner");
        }
        let Some(parent) = parent_for_archival.as_ref() else {
            // The shared helper's non-completed callers are not an authorized
            // production rotation establishment path.
            let mut store_guard = store.lock().await;
            if let Err(error) = store_guard.settle_reserved_rotation_custody_failure(
                child_id,
                rsi_common::types::SandboxCustodyErrorCodeV1::CustodyChanged,
            ) {
                tracing::error!(%child_id, %error, "rotation missing-parent settlement fence failed");
            }
            drop(store_guard);
            let _ = complete_invocation(
                &store,
                &admission_permit,
                InvocationCompletion {
                    error_class: Some("rotation_parent_missing".to_string()),
                    confidence: Some(ModelUsageConfidence::Unavailable),
                    ..InvocationCompletion::default()
                },
                &event_bus,
            )
            .await;
            super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
            release_rotation_controller_reservation(
                controller_transfer.as_ref(),
                ControllerReleaseReasonV1::DurableRowFailed,
            )
            .await;
            return;
        };
        let bound_rotation = match custody_runtime
            .bind_rotation_successor(rotation_candidate, &parent.session, &child_session)
            .await
        {
            Ok(bound) => bound,
            Err(recovery) => {
                let restorable = match recovery {
                    RotationBindFailure::Restorable => true,
                    RotationBindFailure::Superseded => false,
                    RotationBindFailure::SettlementFailed(error) => {
                        tracing::error!(%child_id, %error, "rotation bind-refusal settlement fence failed");
                        false
                    }
                };
                let _ = complete_invocation(
                    &store,
                    &admission_permit,
                    InvocationCompletion {
                        error_class: Some("custody_bind_failed".to_string()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    &event_bus,
                )
                .await;
                super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                release_rotation_controller_reservation(
                    controller_transfer.as_ref(),
                    ControllerReleaseReasonV1::DurableRowFailed,
                )
                .await;
                if restorable {
                    if let Some(parent) = parent_for_archival.take() {
                        completed.write().await.insert(parent_id, parent);
                    }
                }
                return;
            }
        };
        #[cfg(test)]
        if let Err(error) = apply_rotation_context_root_mutation_for_test(&child_session) {
            tracing::error!(%child_id, %error, "rotation context mutation test seam failed");
        }
        #[cfg(test)]
        observe_rotation_context_read_for_test(child_id);
        let context_permit = match custody_runtime
            .begin_rotation_context_read(&bound_rotation, &child_session)
            .await
        {
            Ok(permit) => permit,
            Err(error) => {
                tracing::warn!(%child_id, %error, "Rotation successor context custody refused");
                if let Err(settlement_error) = custody_runtime
                    .settle_rotation_context_failure(child_id, &bound_rotation, &error)
                    .await
                {
                    tracing::error!(%child_id, %settlement_error, "rotation context-refusal settlement fence failed");
                }
                let _ = complete_invocation(
                    &store,
                    &admission_permit,
                    InvocationCompletion {
                        error_class: Some("context_custody_failed".to_string()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    &event_bus,
                )
                .await;
                super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                release_rotation_controller_reservation(
                    controller_transfer.as_ref(),
                    ControllerReleaseReasonV1::DurableRowFailed,
                )
                .await;
                revoke_transferred_rotation_predecessor_after_failure(
                    bound_rotation.disposition(),
                    parent_id,
                    &store,
                    &agent_tokens,
                )
                .await;
                if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary {
                    if let Some(parent) = parent_for_archival.take() {
                        completed.write().await.insert(parent_id, parent);
                    }
                }
                return;
            }
        };
        let permit_cwd = context_permit.effective_cwd().to_path_buf();
        #[cfg(test)]
        if let Err(error) = apply_rotation_execution_scratch_failure_for_test(&child_session) {
            tracing::error!(%child_id, %error, "rotation scratch mutation test seam failed");
        }
        let execution_scratch = if context_permit.cargo_target_dir().is_some() {
            match crate::sandbox::execution_scratch::SandboxExecutionScratch::from_context_permit(
                &context_permit,
            ) {
                Ok(scratch) => scratch,
                Err(error) => {
                    tracing::warn!(session_id = %child_id, %error, "rotation execution scratch refused");
                    drop(context_permit);
                    if let Err(settlement_error) = custody_runtime
                        .settle_rotation_context_failure(child_id, &bound_rotation, &error)
                        .await
                    {
                        tracing::error!(%child_id, %settlement_error, "rotation execution-scratch settlement fence failed");
                    }
                    let _ = complete_invocation(
                        &store,
                        &admission_permit,
                        InvocationCompletion {
                            error_class: Some("execution_scratch_unavailable".to_string()),
                            confidence: Some(ModelUsageConfidence::Unavailable),
                            ..InvocationCompletion::default()
                        },
                        &event_bus,
                    )
                    .await;
                    super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                    release_rotation_controller_reservation(
                        controller_transfer.as_ref(),
                        ControllerReleaseReasonV1::DurableRowFailed,
                    )
                    .await;
                    revoke_transferred_rotation_predecessor_after_failure(
                        bound_rotation.disposition(),
                        parent_id,
                        &store,
                        &agent_tokens,
                    )
                    .await;
                    if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary {
                        if let Some(parent) = parent_for_archival.take() {
                            completed.write().await.insert(parent_id, parent);
                        }
                    }
                    return;
                }
            }
        } else {
            None
        };
        // Raw LaunchConfig is an effect input, so it is constructed only from
        // the successor's ContextRead permit after the durable bind and exact
        // successor authentication have completed.
        let mut config = LaunchConfig {
            query,
            title: None,
            agent_role: child_session.agent_role.clone(),
            epic_spawn_ordinal: child_session.epic_spawn_ordinal,
            working_dir: Some(permit_cwd.clone()),
            provider: Some(provider),
            model: child_session.model.clone(),
            configured_context_window: child_session
                .resolved_context_budget
                .as_ref()
                .and_then(|budget| budget.capacity.configured_tokens),
            max_turns: None,
            system_prompt: None,
            resume_session_id,
            session_kind: Some(child_session.session_kind),
            project_id: child_session.project_id,
            rsi_session_id: Some(child_id),
            rsi_socket: Some(socket_path.clone()),
            rsi_session_token: Some(session_token),
            continued_from: Some(parent_id),
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: child_session.workflow_id,
            workflow_id_override: child_session.workflow_id_override,
            max_retries: None,
            group_id: child_session.group_id,
            skip_project_model_default: false,
            model_invocation_purpose: purpose,
            parent_id: child_session.parent_id,
            effort: child_session.effort.clone(),
            issue_identifier: child_session.issue_identifier.clone(),
            issue_url: child_session.issue_url.clone(),
            issue_tracker_id: child_session.issue_tracker_id.clone(),
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            sandbox: None,
            cargo_target_dir: context_permit.cargo_target_dir().map(ToOwned::to_owned),
            execution_scratch,
            is_eval: child_session.is_eval,
            skip_context_pipeline: child_session.is_eval,
            capability_class: child_session.capability_class,
            tags: child_session.tags.clone(),
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        };
        let system_prompt = if matches!(
            provider,
            SessionProvider::Claude | SessionProvider::Local | SessionProvider::Antigravity
        ) {
            let mut project = if let Some(pid) = child_session.project_id {
                let store_ref = store.clone();
                tokio::task::spawn_blocking(move || {
                    let store = store_ref.blocking_lock();
                    store.get_project(pid).ok().flatten()
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };
            if let Some(project) = project.as_mut() {
                project.path = Some(permit_cwd.clone());
            }
            let pipeline = super::context_pipeline::ContextPipeline::new(
                Arc::clone(&counter),
                memory_handle.clone(),
                store.clone(),
            );
            let context = pipeline
                .assemble(
                    &config.query,
                    &permit_cwd,
                    project.as_ref(),
                    child_session.active_task.as_deref(),
                    None,
                    Some(parent_id),
                    super::context_pipeline::context_injection_allowance(
                        child_session
                            .resolved_context_budget
                            .as_ref()
                            .expect("rotation children carry a resolved context budget")
                            .active_tokens,
                    ),
                )
                .await;
            match (super::preamble::load(child_session.session_kind), context) {
                (Some(preamble), Some(context)) => Some(format!("{preamble}\n\n{context}")),
                (Some(preamble), None) => Some(preamble),
                (None, context) => context,
            }
        } else {
            None
        };
        let system_prompt = super::preamble::prepend_sandbox_custody_instruction(
            system_prompt,
            super::preamble::sandbox_custody_instruction_for_session(&child_session),
        );
        config.system_prompt = system_prompt.clone();
        child_session.harness_version_hash =
            Some(super::harness_hash::compute_harness_version_hash(
                system_prompt.as_deref(),
                &config.query,
                Some(child_session.session_kind),
            ));
        let metadata_result = {
            let store_ref = store.clone();
            let hash = child_session.harness_version_hash.clone();
            let branch = child_session.git_branch.clone();
            tokio::task::spawn_blocking(move || {
                let mut store = store_ref.blocking_lock();
                store.finalize_direct_launch_metadata(
                    child_id,
                    invocation_id,
                    branch.as_deref(),
                    hash.as_deref(),
                )
            })
            .await
        };
        drop(context_permit); // ContextRead is transitional; provider sealing lands later.
        if !metadata_result.is_ok_and(|result| result.is_ok()) {
            if let Err(settlement_error) = custody_runtime
                .settle_bound_rotation_failure(
                    child_id,
                    &bound_rotation,
                    rsi_common::types::SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                )
                .await
            {
                tracing::error!(%child_id, %settlement_error, "rotation metadata-failure settlement fence failed");
            }
            let _ = complete_invocation(
                &store,
                &admission_permit,
                InvocationCompletion {
                    error_class: Some("metadata_persist_failed".to_string()),
                    confidence: Some(ModelUsageConfidence::Unavailable),
                    ..InvocationCompletion::default()
                },
                &event_bus,
            )
            .await;
            super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
            release_rotation_controller_reservation(
                controller_transfer.as_ref(),
                ControllerReleaseReasonV1::DurableRowFailed,
            )
            .await;
            revoke_transferred_rotation_predecessor_after_failure(
                bound_rotation.disposition(),
                parent_id,
                &store,
                &agent_tokens,
            )
            .await;
            if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary {
                if let Some(parent) = parent_for_archival.take() {
                    completed.write().await.insert(parent_id, parent);
                }
            }
            return;
        }

        let closure_source_id = {
            let store = store.lock().await;
            store.closure_source_for_session(parent_id)
        };
        let closure_binding = match &closure_source_id {
            Ok(Some(source_id)) => {
                let store = store.lock().await;
                store
                    .live_custody_for_session(child_id)
                    .and_then(|custody| {
                        store.append_closure_source_session(
                            *source_id,
                            child_id,
                            parent_id,
                            child_session.rotation_depth,
                            custody.custody_id,
                            custody.generation,
                        )
                    })
                    .map(|()| Some(*source_id))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(DaemonError::Store(error.to_string())),
        };
        match closure_binding {
            Err(error) => {
                tracing::error!(%child_id, %error, "Closure rotation lineage binding failed; terminal capture will remain fail-closed");
                if let Ok(Some(source_id)) = &closure_source_id
                    && let Err(capture_error) =
                        crate::closure_kernel::ingress::capture_or_replay_terminal_output(
                            Arc::clone(&store),
                            *source_id,
                        )
                        .await
                {
                    tracing::error!(%child_id, %capture_error, "Closure failed rotation binding could not be settled immediately; bounded recovery will retry");
                }
            }
            Ok(Some(_)) => {
                let selector = store
                    .lock()
                    .await
                    .closure_launch_selector_for_session(child_id);
                match selector {
                    Ok(Some(mut selector)) => {
                        selector.model_invocation_id = Some(invocation_id);
                        let contract = match crate::closure_kernel::terminal_contract(
                            &selector,
                            child_id,
                            child_session.rotation_depth,
                        ) {
                            Ok(contract) => contract,
                            Err(error) => {
                                tracing::error!(%child_id, %error, "Closure rotation contract construction failed");
                                String::new()
                            }
                        };
                        if !contract.is_empty() {
                            config.system_prompt = Some(match config.system_prompt.take() {
                                Some(existing) => format!("{existing}\n\n{contract}"),
                                None => contract,
                            });
                            config.closure_selector = Some(selector);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(%child_id, %error, "Closure rotation selector lookup failed");
                    }
                }
            }
            Ok(None) => {}
        }

        // Launch the child through the single guarded spawn primitive
        // (`&spawn_guard` is the mandatory single-flight witness). The rotation
        // Harness arm rebuilds its `AgentControlHandle` from these daemon-global
        // collaborator `Arc`s (cheap clones; only the Harness leaf reads them),
        // so a rotated Harness child carries an IDENTICAL tool set — native
        // `rsi_control` + `schedule_wake` + memory/list_files — via the single
        // `default_tools` source of truth. The bound caller id is the child's
        // own id.
        let launcher = super::provider_spawn::FreshLauncher {
            runtime_config: Arc::clone(&runtime_config),
            harness: super::provider_spawn::FreshHarnessCtx {
                codegraph_handle: codegraph_handle.clone(),
                custody_runtime: custody_runtime.clone(),
                active: Arc::clone(&active),
                completed: Arc::clone(&completed),
                store: Arc::clone(&store),
                event_bus: Arc::clone(&event_bus),
                model_call_settlements: model_call_settlements.clone(),
                spawn_coordinator: Arc::clone(&spawn_coordinator),
                memory_handle: memory_handle.clone(),
                initial_admission_permit: admission_permit.clone(),
                resolved_context_budget: child_session
                    .resolved_context_budget
                    .clone()
                    .expect("rotation children carry a resolved context budget"),
                bound_session_id: child_id,
            },
        };

        #[cfg(test)]
        observe_rotation_config_for_test(child_id, &config);

        #[cfg(test)]
        let launch_result = if take_rotation_provider_unavailable_for_test(child_id) {
            Err(DaemonError::Store(
                "rotation provider unavailable test seam".to_string(),
            ))
        } else {
            super::launch::take_controller_candidate_test_process(child_id).map_or_else(
                || {
                    super::provider_spawn::spawn_provider_process(
                        provider,
                        &config,
                        &launcher,
                        &admission_permit,
                        &spawn_guard,
                    )
                },
                Ok,
            )
        };
        #[cfg(not(test))]
        let launch_result = super::provider_spawn::spawn_provider_process(
            provider,
            &config,
            &launcher,
            &admission_permit,
            &spawn_guard,
        );
        let (mut process, event_rx) = match launch_result {
            Ok(pair) => pair,
            Err(e) => {
                if let Err(settle_error) = complete_invocation(
                    &store,
                    &admission_permit,
                    InvocationCompletion {
                        error_class: Some("spawn_failed".to_string()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    &event_bus,
                )
                .await
                {
                    tracing::warn!(
                        error = %settle_error,
                        child_id = %child_id,
                        "Failed to settle rotation-child admission after spawn error"
                    );
                }
                persistence
                    .record_session_diagnostic(
                        child_id,
                        rsi_common::types::SessionDiagnosticLevelV1::Error,
                        "Rotation child provider launch failed",
                    )
                    .await;
                tracing::error!(
                    parent_id = %parent_id,
                    child_id = %child_id,
                    error = %e,
                    "Rotation child launch failed — rolling back parent"
                );
                if let Err(settlement_error) = custody_runtime
                    .settle_bound_rotation_failure(
                        child_id,
                        &bound_rotation,
                        rsi_common::types::SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                    )
                    .await
                {
                    tracing::error!(%child_id, %settlement_error, "rotation bound-failure settlement fence failed");
                }
                // An ordinary rotation has no exclusive owner to retain. A
                // committed Transfer is irreversible: its Failed child stays
                // the sole root owner and the historical predecessor is never
                // revived.
                if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary
                    && let Some(parent) = parent_for_archival
                {
                    completed.write().await.insert(parent_id, parent);
                    if let Some(ref rid) = rotation_id_for_log {
                        let metadata = serde_json::json!({
                            "reason": "spawn_failed",
                            "error": e.to_string(),
                        })
                        .to_string();
                        let _ = persistence
                            .log_rotation_event(
                                parent_id,
                                rid,
                                "completed",
                                "rollback",
                                Some(metadata),
                            )
                            .await;
                    }
                    tracing::info!(
                        parent_id = %parent_id,
                        "Parent session rolled back into completed map after child launch failure"
                    );
                }
                super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                release_rotation_controller_reservation(
                    controller_transfer.as_ref(),
                    ControllerReleaseReasonV1::ProviderSpawnFailed,
                )
                .await;
                revoke_transferred_rotation_predecessor_after_failure(
                    bound_rotation.disposition(),
                    parent_id,
                    &store,
                    &agent_tokens,
                )
                .await;
                return;
            }
        };
        let controller_confirmation_kind = controller_transfer.as_ref().and_then(|_| {
            super::provider_spawn::installed_provider_confirmation(provider, &mut process)
        });

        let (stop_tx, stop_rx) = mpsc::channel(1);

        let spawn_generation = Self::next_spawn_generation_from(&spawn_epoch);
        let tracked = TrackedSession {
            session: child_session.clone(),
            spawn_generation,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            process: Some(process),
            deferred_successor_start_gate: None,
            stop_tx,
            interrupt_requested: false,
            pending_archive: false,
            rotation: if child_session.query.trim() == "/create_handoff" {
                debug_assert!(
                    parent_for_archival.is_none(),
                    "rotation successor query must never be /create_handoff"
                );
                super::rotation_coordinator::RotationCoordinator::new_writing_handoff(
                    child_id,
                    child_session.rotation_depth,
                    context_rotation_enabled && child_session.rotation_disabled_at.is_none(),
                )
            } else {
                super::rotation_coordinator::RotationCoordinator::new(
                    child_id,
                    child_session.rotation_depth,
                    context_rotation_enabled && child_session.rotation_disabled_at.is_none(),
                )
            },
            live_input_tokens: 0,
            live_output_tokens: 0,
            live_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
            daemon_input_tokens: 0,
            daemon_output_tokens: 0,
            daemon_tokens_at_last_api_update: 0,
            codex_context_tokens: 0,
            pipeline_artifact: None,
            memory_flush_compaction_count: None,
            pending_question: None,
            approval_wait_start: None,
            approval_wait_total_ms: 0,
            work_run_start: None,
            work_time_base_ms: 0,
            received_meaningful_output: false,
            exit_code: None,
            retry_attempt: 0,
            max_retries: 0,
            last_event_at: chrono::Utc::now(),
            stall_interrupted: false,
            last_usage_update: None,
            last_mismatch_warn: None,
            last_classified_at: None,
            classification_count: 0,
            last_verdict: None,
        };

        active.write().await.insert(child_id, tracked);
        drop(cwd_admission_guard);
        drop(predecessor_spawn_guard);

        // B1: the guard covers check -> launch -> insert ONLY. Drop it now,
        // before the inline `monitor_session` await below, so this child's own
        // 2nd-generation rotation can re-acquire `guard(child_id)` without
        // self-deadlocking the non-reentrant `tokio::Mutex`.
        drop(spawn_guard);

        #[cfg(test)]
        if controller_transfer.is_some() {
            super::launch::pause_controller_candidate_test(
                child_id,
                super::launch::ControllerCandidateTestPhase::AfterDurablePersistence,
            )
            .await;
        }

        if let Err(error) = preflight_rotation_lead_transfer(&store, parent_id).await {
            tracing::warn!(
                %parent_id,
                %child_id,
                error = %error,
                "Context rotation deferred before controller assignment by Epic lead fence"
            );
            if let Some(mut tracked) = active.write().await.remove(&child_id)
                && let Some(process) = tracked.process.as_mut()
            {
                let _ = process.kill().await;
            }
            let _ = complete_invocation(
                &store,
                &admission_permit,
                InvocationCompletion {
                    error_class: Some("rotation_lead_transfer_failed".to_string()),
                    confidence: Some(ModelUsageConfidence::Unavailable),
                    ..InvocationCompletion::default()
                },
                &event_bus,
            )
            .await;
            if let Err(settlement_error) = custody_runtime
                .settle_bound_rotation_failure(
                    child_id,
                    &bound_rotation,
                    rsi_common::types::SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                )
                .await
            {
                tracing::error!(%child_id, %settlement_error, "rotation pre-assignment lead-refusal settlement fence failed");
            }
            super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
            release_rotation_controller_reservation(
                controller_transfer.as_ref(),
                ControllerReleaseReasonV1::DurableRowFailed,
            )
            .await;
            if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary
                && let Some(parent) = parent_for_archival.take()
            {
                completed.write().await.insert(parent_id, parent);
            }
            return;
        }

        let mut controller_assignment = None;
        if let Some((transfer, reservation)) = &controller_transfer {
            let Some(confirmation_kind) = controller_confirmation_kind else {
                if let Some(mut tracked) = active.write().await.remove(&child_id)
                    && let Some(process) = tracked.process.as_mut()
                {
                    let _ = process.kill().await;
                }
                let _ = complete_invocation(
                    &store,
                    &admission_permit,
                    InvocationCompletion {
                        error_class: Some("controller_confirmation_failed".to_string()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    &event_bus,
                )
                .await;
                if let Err(settlement_error) = custody_runtime
                    .settle_bound_rotation_failure(
                        child_id,
                        &bound_rotation,
                        rsi_common::types::SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                    )
                    .await
                {
                    tracing::error!(%child_id, %settlement_error, "rotation confirmation-failure settlement fence failed");
                }
                super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                release_rotation_controller_reservation(
                    controller_transfer.as_ref(),
                    ControllerReleaseReasonV1::ConfirmationFailed,
                )
                .await;
                revoke_transferred_rotation_predecessor_after_failure(
                    bound_rotation.disposition(),
                    parent_id,
                    &store,
                    &agent_tokens,
                )
                .await;
                if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary
                    && let Some(parent) = parent_for_archival.take()
                {
                    completed.write().await.insert(parent_id, parent);
                }
                return;
            };
            let confirmation = IdeaControllerLaunchConfirmationV1 {
                candidate_session_id: child_id,
                project_id: transfer.project_id(),
                provider,
                admission_invocation_id: admission_permit.invocation_id(),
                durable_session_id: child_id,
                durable_project_id: transfer.project_id(),
                durable_provider: provider,
                a6_bound_session_id: child_id,
                confirmation_kind,
                confirmed_at: chrono::Utc::now(),
            };
            #[cfg(test)]
            super::launch::pause_controller_candidate_test(
                child_id,
                super::launch::ControllerCandidateTestPhase::AfterConfirmation,
            )
            .await;
            let assigned = Self::assign_live_controller_candidate(
                &active,
                transfer,
                reservation,
                &confirmation,
                &prospective_controller_token,
                &agent_tokens,
            )
            .await;
            let assigned = match assigned {
                Ok(assigned) => assigned,
                Err(error) => {
                    tracing::error!(
                        %parent_id,
                        %child_id,
                        error = %error,
                        "Rotation controller assignment failed; preserving parent"
                    );
                    if let Some(mut tracked) = active.write().await.remove(&child_id)
                        && let Some(process) = tracked.process.as_mut()
                    {
                        let _ = process.kill().await;
                    }
                    let _ = complete_invocation(
                        &store,
                        &admission_permit,
                        InvocationCompletion {
                            error_class: Some("controller_assignment_failed".to_string()),
                            confidence: Some(ModelUsageConfidence::Unavailable),
                            ..InvocationCompletion::default()
                        },
                        &event_bus,
                    )
                    .await;
                    if let Err(settlement_error) = custody_runtime
                        .settle_bound_rotation_failure(
                            child_id,
                            &bound_rotation,
                            rsi_common::types::SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                        )
                        .await
                    {
                        tracing::error!(%child_id, %settlement_error, "rotation assignment-failure settlement fence failed");
                    }
                    super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                    let reason =
                        if matches!(error, super::ControllerCandidateCommitError::Cancelled) {
                            ControllerReleaseReasonV1::Cancelled
                        } else {
                            ControllerReleaseReasonV1::StaleAssignmentBase
                        };
                    release_rotation_controller_reservation(controller_transfer.as_ref(), reason)
                        .await;
                    revoke_transferred_rotation_predecessor_after_failure(
                        bound_rotation.disposition(),
                        parent_id,
                        &store,
                        &agent_tokens,
                    )
                    .await;
                    if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary
                        && let Some(parent) = parent_for_archival.take()
                    {
                        completed.write().await.insert(parent_id, parent);
                    }
                    return;
                }
            };
            controller_assignment = Some(assigned);
        }

        #[cfg(test)]
        if controller_assignment.is_some() {
            super::launch::pause_controller_candidate_test(
                child_id,
                super::launch::ControllerCandidateTestPhase::AfterAssignmentBeforeRotationLeadTransfer,
            )
            .await;
        }

        // RPC-1 C1 publication: any Epic whose lead_session_id pointed at the
        // parent transfers to the child, and the terminal
        // `completed{successor_id}` event is written, in ONE IMMEDIATE
        // transaction. That commit is the publication point for the tip and
        // the lead generation alike. A nonterminal master-successor
        // reservation (including one that raced the preflight) rejects the
        // whole transaction; the rotation then records `refused:lead_transfer`
        // (C4). No success cache/event, predecessor-token, or archival
        // follow-up may cross that failure boundary. If an already-assigned
        // controller cannot be durably released, its live process and
        // authority remain monitored so the exact idempotent release stays
        // recoverable.
        let mut publish_controller_success = true;
        let publication_metadata = {
            let handoff_path = query_for_event.strip_prefix("/resume_handoff ");
            serde_json::json!({
                "handoff_filepath": handoff_path,
                "handoff_commit": handoff_commit.as_deref(),
                "successor_id": child_id,
                "query_kind": if handoff_path.is_some() { "resume_handoff" } else { "task" },
            })
            .to_string()
        };
        // K2 finding a: C1 runs holding guard(P) and guard(S) in the global
        // lock order. The spawn span above released both, so a continuation
        // of either that is mid-flight completes before, or observes, this
        // commit. No store lock is held across the guard await.
        #[cfg(test)]
        pause_rotation_publication_for_test(parent_id).await;
        let publication_guards =
            super::RotationPublicationGuards::acquire(parent_id, child_id).await;
        let store_ref = Arc::clone(&store);
        let publish_rotation_id = publication_rotation_id.clone();
        let lead_transfer = tokio::task::spawn_blocking(move || {
            let published = store_ref.blocking_lock().publish_rotation_successor(
                &publication_guards,
                &publish_rotation_id,
                &publication_metadata,
            );
            drop(publication_guards);
            published
        })
        .await
        .map_err(|error| DaemonError::Store(format!("rotation publication join failed: {error}")))
        .and_then(|result| result);
        let affected_epics = match lead_transfer {
            Ok(affected_epics) => affected_epics,
            Err(error) => {
                tracing::warn!(
                    %parent_id,
                    %child_id,
                    error = %error,
                    "Context rotation refused after durable Epic lead transfer refusal"
                );
                let controller_release = if let (Some((transfer, reservation)), Some(assigned)) =
                    (controller_transfer.as_ref(), controller_assignment.as_ref())
                {
                    let rebound = crate::idea_control::IdeaControllerTransferHandle::for_system(
                        Arc::clone(&store),
                        transfer.project_id(),
                        transfer.idea_id(),
                        Arc::new(crate::idea_control::SystemIdeaControllerClock),
                    )
                    .await;
                    Some(match rebound {
                        Ok(rebound) => {
                            rebound
                                .release_assigned(&ReleaseAssignedIdeaControllerRequestV1 {
                                    expected_row_version: assigned.idea.row_version,
                                    release_intent_key: format!(
                                        "rotation-lead-refused:{}",
                                        reservation.reservation_id
                                    ),
                                    reason: ControllerReleaseReasonV1::OperatorRelease,
                                })
                                .await
                        }
                        Err(error) => Err(error),
                    })
                } else {
                    None
                };
                if let Some(Err(release_error)) = controller_release {
                    tracing::error!(
                        %parent_id,
                        %child_id,
                        error = %release_error,
                        "Controller release failed after rotation lead refusal; retaining live authority for recovery"
                    );
                    if let Some(assigned) = controller_assignment.as_ref() {
                        // Reuse the startup reconciliation signal and its
                        // committed event ID. Consumers can deduplicate this
                        // visibility edge with restart reconciliation.
                        event_bus.publish(DaemonEvent::SystemMessage {
                            level: "error".to_string(),
                            message: format!("idea_controller_reconciled:{}", assigned.event.id),
                        });
                    }
                    publish_controller_success = false;
                    if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary
                        && let Some(parent) = parent_for_archival.take()
                    {
                        completed.write().await.insert(parent_id, parent);
                    }
                    Self::record_rotation_refusal(
                        parent_id,
                        Some(&publication_rotation_id),
                        "lead_transfer",
                        &completed,
                        &event_bus,
                        &persistence,
                        &store,
                    )
                    .await;
                    Vec::new()
                } else {
                    if let Some(mut tracked) = active.write().await.remove(&child_id)
                        && let Some(process) = tracked.process.as_mut()
                    {
                        let _ = process.kill().await;
                    }
                    let _ = complete_invocation(
                        &store,
                        &admission_permit,
                        InvocationCompletion {
                            error_class: Some("rotation_lead_transfer_failed".to_string()),
                            confidence: Some(ModelUsageConfidence::Unavailable),
                            ..InvocationCompletion::default()
                        },
                        &event_bus,
                    )
                    .await;
                    if let Err(settlement_error) = custody_runtime
                        .settle_bound_rotation_failure(
                            child_id,
                            &bound_rotation,
                            rsi_common::types::SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                        )
                        .await
                    {
                        tracing::error!(%child_id, %settlement_error, "rotation lead-refusal settlement fence failed");
                    }
                    super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                    if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary
                        && let Some(parent) = parent_for_archival.take()
                    {
                        completed.write().await.insert(parent_id, parent);
                    }
                    Self::record_rotation_refusal(
                        parent_id,
                        Some(&publication_rotation_id),
                        "lead_transfer",
                        &completed,
                        &event_bus,
                        &persistence,
                        &store,
                    )
                    .await;
                    return;
                }
            }
        };
        // The rotated-to child now holds every lead pointer the parent held, so
        // it earns the same marks an ordinary promotion applies. The
        // predecessor is deliberately left pinned: the pinned set is the
        // succession line, and pruning it is the operator's call.
        let inherited_lead_authority = !affected_epics.is_empty();
        for epic_id in affected_epics {
            if let Some(tracked) = active.write().await.get_mut(&epic_id) {
                tracked.session.lead_session_id = Some(child_id);
            } else if let Some(completed_epic) = completed.write().await.get_mut(&epic_id) {
                completed_epic.session.lead_session_id = Some(child_id);
            }
            event_bus.publish(DaemonEvent::SessionMetadataChanged {
                session_id: epic_id,
                model: None,
                pinned_at: None,
                project_id: None,
                parent_id: None,
                lead_session_id: Some(Some(child_id)),
                testing_needed_at: None,
                rotation_disabled_at: None,
                resolved_context_budget: None,
            });
            tracing::info!(
                epic_id = %epic_id,
                old_lead = %parent_id,
                new_lead = %child_id,
                "Transferred Epic lead pointer after rotation"
            );
        }
        if inherited_lead_authority {
            super::hierarchy_ops::mark_lead_session_runtime(
                &active, &completed, &store, &event_bus, child_id,
            )
            .await;
        }
        if publish_controller_success && let Some(assigned) = controller_assignment {
            event_bus.publish(DaemonEvent::SystemMessage {
                level: "info".to_string(),
                message: format!("idea_controller_assigned:{}", assigned.event.id),
            });
        }

        // Saga-style archival: archive parent ONLY after child is confirmed launched
        // and inserted into the active map. This prevents orphaning the parent if
        // the child launch fails.
        if parent_for_archival.is_some() {
            // A6 (D2): the parent is being archived because its successor is
            // confirmed launched — revoke the parent's outstanding tokens at
            // this exact saga point, so a rotated-away session's credential
            // dies with its supersession (plan §4.3). The failure/rollback
            // path above must NOT revoke: a rolled-back parent stays
            // restorable and its token stays valid.
            super::revoke_agent_tokens_for_session(&agent_tokens, parent_id).await;
            let archived = match custody_runtime
                .finalize_rotation_predecessor(child_id, &bound_rotation)
                .await
            {
                Ok(archived) => archived,
                Err(error) => {
                    tracing::error!(%parent_id, %child_id, %error, "rotation archival and receipt transaction failed");
                    event_bus.publish(DaemonEvent::SystemMessage {
                        level: "error".into(),
                        message: "Rotation predecessor archival and authority receipt could not be committed.".into(),
                    });
                    false
                }
            };
            if archived {
                event_bus.publish(DaemonEvent::SessionArchived {
                    session_id: parent_id,
                    projection_id: None,
                });
            }
            #[cfg(test)]
            tests::observe_rotation_finalization_for_test(
                child_id,
                archived,
                &custody_runtime,
                &bound_rotation,
            )
            .await;
        }

        // Create and persist user event for child
        let user_event = Self::create_user_event(child_id, 1, &query_for_event);
        {
            let mut active_guard = active.write().await;
            if let Some(tracked) = active_guard.get_mut(&child_id) {
                tracked.events.push(user_event.clone());
            }
        }
        match persistence.insert_event(user_event.clone()).await {
            Ok(db_id) => {
                let mut active_guard = active.write().await;
                if let Some(tracked) = active_guard.get_mut(&child_id)
                    && let Some(last_event) = tracked.events.last_mut()
                {
                    last_event.id = db_id;
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    session_id = %child_id,
                    "Failed to persist rotation child bootstrap event"
                );
            }
        }

        // Publish events for child
        event_bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: child_id,
            old_status: SessionStatus::Starting,
            new_status: SessionStatus::Starting,
        });
        event_bus.publish(DaemonEvent::ConversationEvent {
            session_id: child_id,
            event: user_event,
        });

        // Count initial query tokens for daemon-side tracking
        {
            let query_tokens = counter.count(&query_for_event);
            let mut active_guard = active.write().await;
            if let Some(tracked) = active_guard.get_mut(&child_id) {
                tracked.daemon_input_tokens += query_tokens;
                tracked.session.daemon_input_tokens = Some(tracked.daemon_input_tokens);
            }
        }

        tracing::info!(
            parent_id = %parent_id,
            child_id = %child_id,
            "Context rotation complete, child launched"
        );

        // Monitor child session with panic catching.
        // We use AssertUnwindSafe + catch_unwind around Box::pin to detect panics
        // without requiring Send (which tokio::spawn would need for recursive futures).
        let child_provider_session = Box::new(crate::provider::CliProviderSession::new(event_rx));
        // Rotation child sessions use CLI providers — Single turn policy, no dynamic tool registry.
        let child_turn_controller = crate::turn_controller::TurnController::new(
            crate::turn_controller::ContinuationPolicy::Single,
        );
        let child_tool_registry = std::sync::Arc::new(crate::tool_registry::ToolRegistry::new());
        let monitor_active = Arc::clone(&active);
        let monitor_completed = Arc::clone(&completed);
        let monitor_event_bus = Arc::clone(&event_bus);
        let monitor_store = Arc::clone(&store);
        let monitor_persistence = persistence.clone();
        let monitor_spawn_coordinator = Arc::clone(&spawn_coordinator);
        let monitor_agent_tokens = Arc::clone(&agent_tokens);
        let monitor_custody_runtime = custody_runtime.clone();
        let monitor_codegraph_handle = codegraph_handle.clone();
        let monitor_result = std::panic::AssertUnwindSafe(Box::pin(async move {
            #[cfg(test)]
            panic_rotation_monitor_for_test(child_id);
            Self::monitor_session(
                child_id,
                spawn_generation,
                child_provider_session,
                monitor_active,
                monitor_completed,
                monitor_event_bus,
                stop_rx,
                monitor_store,
                model_call_settlements,
                monitor_persistence,
                1,
                context_rotation_enabled,
                socket_path,
                counter,
                memory_handle,
                retry_tx,
                child_tool_registry,
                child_turn_controller,
                runtime_config,
                monitor_spawn_coordinator,
                monitor_agent_tokens,
                spawn_epoch,
                agent_message_arbiter,
                monitor_codegraph_handle,
                monitor_custody_runtime,
            )
            .await;
        }));
        match futures::FutureExt::catch_unwind(monitor_result).await {
            Ok(()) => {
                tracing::debug!(
                    child_id = %child_id,
                    parent_id = %parent_id,
                    "Child session monitor completed normally"
                );
            }
            Err(_panic) => {
                tracing::error!(
                    parent_id = %parent_id,
                    child_id = %child_id,
                    "Child monitor panicked — settling failed rotation successor"
                );
                // Kill before settlement; a monitor panic must not leave a
                // provider or its admitted invocation running.
                if let Some(mut tracked) = active.write().await.remove(&child_id)
                    && let Some(process) = tracked.process.as_mut()
                {
                    let _ = process.kill().await;
                }
                // Provider publication already moved this row beyond the
                // Starting-only custody settlement phase. Retain its bound
                // projection and use the existing C5 failure/autofile path.
                if let Err(e) = persistence
                    .update_failed_and_stage_autofile(
                        child_id,
                        crate::store::daemon_settings::AutofileCause::RotationMonitorPanic,
                    )
                    .await
                {
                    tracing::error!(error = %e, child_id = %child_id, "Failed to mark panicked child as Failed");
                }
                event_bus.publish(DaemonEvent::SessionStatusChanged {
                    session_id: child_id,
                    old_status: SessionStatus::Running,
                    new_status: SessionStatus::Failed,
                });
                let _ = complete_invocation(
                    &store,
                    &admission_permit,
                    InvocationCompletion {
                        error_class: Some("rotation_monitor_panic".to_string()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    &event_bus,
                )
                .await;
                super::revoke_agent_tokens_for_session(&agent_tokens, child_id).await;
                release_rotation_controller_reservation(
                    controller_transfer.as_ref(),
                    ControllerReleaseReasonV1::ProviderSpawnFailed,
                )
                .await;
                crate::session::agent_verbs::AgentControlHandle::new(
                    Arc::clone(&active),
                    Arc::clone(&completed),
                    Arc::clone(&store),
                    Arc::clone(&event_bus),
                    Arc::clone(&spawn_coordinator),
                )
                .maybe_autofile_terminal_failure(
                    child_id,
                    crate::store::daemon_settings::RecoveryDisposition::NoRecoverySource,
                )
                .await;
                if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary
                    && let Some(ref rid) = rotation_id_for_log
                {
                    let metadata = serde_json::json!({
                        "reason": "child_monitor_panic",
                        "child_id": child_id.to_string(),
                    })
                    .to_string();
                    let _ = persistence
                        .log_rotation_event(parent_id, rid, "completed", "rollback", Some(metadata))
                        .await;
                }
                if bound_rotation.disposition() == RotationCustodyDisposition::Ordinary {
                    #[cfg(test)]
                    apply_rotation_pre_restore_mutation_for_test(child_id, parent_id, &store).await;
                    if let Some(parent_completed) = parent_for_archival {
                        let _restoration = Self::restore_archived_parent_after_child_failure(
                            parent_id,
                            Some(parent_completed),
                            &completed,
                            &event_bus,
                            &bound_rotation,
                            &custody_runtime,
                            &agent_tokens,
                        )
                        .await;
                    } else {
                        // Fallback for callers that already consumed the in-memory parent snapshot.
                        let _restoration = Self::restore_archived_parent_after_child_failure(
                            parent_id,
                            None,
                            &completed,
                            &event_bus,
                            &bound_rotation,
                            &custody_runtime,
                            &agent_tokens,
                        )
                        .await;
                    }
                }
            }
        }
    }

    /// Trigger context rotation on a session.
    /// For running sessions: sets rotation flag and sends stop signal so the
    /// monitor loop breaks with MonitorBreakReason::Rotation.
    /// For stopped sessions: directly spawns rotation as a background task.
    pub async fn trigger_rotation(&self, session_id: Uuid) -> Result<()> {
        if !self.context_rotation_enabled {
            return Err(DaemonError::Rpc(
                "Context rotation is disabled (set RSI_CONTEXT_ROTATION_ENABLED=1 to enable)"
                    .to_string(),
            ));
        }

        // Try active sessions first (running)
        let active_manual_rotation = {
            let mut active = self.active.write().await;
            if let Some(tracked) = active.get_mut(&session_id) {
                if tracked.session.status != SessionStatus::Running {
                    return Err(DaemonError::Rpc(format!(
                        "Cannot rotate active session {}: not running (status: {:?})",
                        session_id, tracked.session.status
                    )));
                }
                if tracked.rotation.is_rotating() {
                    return Err(DaemonError::Rpc(
                        "Session is already writing a handoff, cannot rotate again".to_string(),
                    ));
                }
                match tracked
                    .rotation
                    .advance(super::rotation_coordinator::RotationEvent::ManualTrigger)
                {
                    super::rotation_coordinator::RotationAction::InterruptForRotation => {}
                    _ => {
                        return Err(DaemonError::Rpc(
                            "Session could not enter rotation state".to_string(),
                        ));
                    }
                }
                let Some(rotation_id) = tracked.rotation.rotation_id().map(str::to_string) else {
                    return Err(DaemonError::Rpc(
                        "Session entered rotation state without a rotation_id".to_string(),
                    ));
                };
                let _ = tracked.stop_tx.try_send(());
                Some(rotation_id)
            } else {
                None
            }
        };
        if let Some(rotation_id) = active_manual_rotation {
            let metadata = serde_json::json!({ "manual": true }).to_string();
            let _ = self
                .persistence
                .log_rotation_event(
                    session_id,
                    &rotation_id,
                    "pending_interrupt",
                    "manual_triggered",
                    Some(metadata),
                )
                .await;
            return Ok(());
        }

        // Try completed sessions (stopped)
        {
            let completed = self.completed.read().await;
            if let Some(cs) = completed.get(&session_id) {
                if !matches!(
                    cs.session.status,
                    SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
                ) {
                    return Err(DaemonError::Rpc(format!(
                        "Cannot rotate session {}: status {:?} is not rotatable",
                        session_id, cs.session.status
                    )));
                }
                // Verify we have a provider session ID to resume from
                if cs.session.claude_session_id.is_none() {
                    return Err(DaemonError::Rpc(
                        "Cannot rotate: no provider session ID captured".to_string(),
                    ));
                }
            } else {
                return Err(DaemonError::SessionNotFound(session_id));
            }
        }

        // Spawn rotation for completed session as background task
        let active = self.active.clone();
        let completed = self.completed.clone();
        let event_bus = self.event_bus.clone();
        let store = self.store.clone();
        let persistence = self.persistence.clone();
        let context_rotation_enabled = self.context_rotation_enabled;
        let socket_path = self.socket_path.clone();
        let counter = Arc::clone(&self.token_counter);
        let memory_handle = self.memory_handle.clone();
        let retry_tx = self.retry_tx.clone();
        let runtime_config = Arc::clone(&self.runtime_config);
        let spawn_coordinator = Arc::clone(&self.spawn_coordinator);
        let agent_tokens = Arc::clone(&self.agent_tokens);
        let spawn_epoch = Arc::clone(&self.spawn_epoch);
        let agent_message_arbiter = Arc::clone(&self.agent_message_arbiter);
        let codegraph_handle = self.codegraph_handle.clone();
        let custody_runtime = self.custody_execution_runtime();
        let model_call_settlements = self.model_call_settlements.handle()?;
        let rotation_id = Uuid::new_v4().to_string();
        let metadata = serde_json::json!({ "manual": true }).to_string();
        let _ = self
            .persistence
            .log_rotation_event(
                session_id,
                &rotation_id,
                "completed",
                "manual_triggered",
                Some(metadata),
            )
            .await;
        tokio::spawn(async move {
            Self::rotate_completed_session(
                session_id,
                None, // No handoff filepath — resume the durable task query
                Some(rotation_id),
                active,
                completed,
                event_bus,
                store,
                model_call_settlements,
                persistence,
                context_rotation_enabled,
                socket_path,
                counter,
                memory_handle,
                retry_tx,
                runtime_config,
                spawn_coordinator,
                agent_tokens,
                spawn_epoch,
                agent_message_arbiter,
                codegraph_handle,
                custody_runtime,
            )
            .await;
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{DaemonEvent, EventBus};
    use crate::store::successor_reservations::{
        AgentSuccessorLaunchIds, AgentSuccessorReservationIds, ClaimAgentSuccessorOutcome,
        ReserveAgentSuccessorOutcome,
    };
    use crate::store::{IdeaControllerWriteFault, inject_d03_idea_controller_write_fault};
    use rsi_common::agent_coordination::{AgentReserveSuccessorRequestV1, AgentSuccessorStateV1};
    use rsi_common::types::{
        AutonomyPolicy, Capture, CaptureSourceKind, ContentAddressedRef, ContextUsageConfidence,
        ConversationEvent, CreateIdeaRequestV1, EventType, IdeaActorKind,
        IdeaControllerControlOperationV1, IdeaEventPageRequestV1, Project, Role,
        SandboxCleanupState, SandboxKind, Session, SessionKind, SessionProvider, SessionStatus,
        Sha256Digest, idea_controller_candidate_session_id, idea_controller_reservation_id,
    };

    /// Rotation-parity ratchet (source pin): both Harness rotation launch sites
    /// now funnel through the shared `provider_spawn` primitive (A4), whose
    /// Family-B `FreshLauncher` routes Harness through the FRESH
    /// `session::harness::HarnessClient` (which builds its registry via the
    /// single shared `default_tools`), NOT the legacy
    /// `crate::harness::client::HarnessClient` whose registry omitted
    /// `schedule_wake` + the native `rsi_control` tools. Pin the fresh-client
    /// routing at its new home and prove the legacy split-brain client appears
    /// in neither the funnel nor rotation — guarding against a regression back
    /// to the split-brain registry.
    #[test]
    fn harness_rotation_uses_fresh_client_not_legacy_split_brain() {
        let funnel_source = include_str!("provider_spawn.rs");
        let rotation_source = include_str!("rotation.rs");
        // Needles are assembled from fragments so these very assertions (also
        // captured by `include_str!`) don't self-satisfy the `contains` checks.
        let fresh_needle = [
            "crate::session::harness::HarnessClient",
            "::new().",
            "launch(",
        ]
        .concat();
        let legacy_needle = ["crate::harness", "::client::HarnessClient::", "launch("].concat();
        assert!(
            funnel_source.contains(&fresh_needle),
            "rotation Harness launches must go through the fresh HarnessClient"
        );
        assert!(
            !funnel_source.contains(&legacy_needle) && !rotation_source.contains(&legacy_needle),
            "rotation must not call the legacy split-brain client registry"
        );
    }

    /// A6 source-pin ratchet (pattern of the fresh-client pin above): both
    /// rotation `LaunchConfig` sites (G2 `resume_for_handoff_write`, G3
    /// `spawn_rotation_child`) must stamp a minted token — a config site
    /// regressing its token field back to `None` would silently re-open the
    /// rotation token gap the A6 slice closed.
    #[test]
    fn rotation_config_sites_carry_session_token() {
        let rotation_source = include_str!("rotation.rs");
        // Needles are assembled from fragments so this test's own source
        // (also captured by `include_str!`) can't satisfy the checks.
        let none_needle = ["rsi_session_token", ": None"].concat();
        assert!(
            !rotation_source.contains(&none_needle),
            "rotation.rs must not construct a LaunchConfig without a session token (A6 G2/G3)"
        );
        let some_needle = ["rsi_session_token", ": Some("].concat();
        assert_eq!(
            rotation_source.matches(&some_needle).count(),
            2,
            "expected exactly the two rotation config sites (G2 + G3) to stamp a token"
        );
    }

    #[test]
    fn h1_v83_handoff_resume_source_ratchet_orders_custody_before_config_and_dispatch() {
        let source = include_str!("rotation.rs");
        let start = source
            .find("pub(super) async fn resume_for_handoff_write")
            .expect("handoff implementation");
        let end = source[start..]
            .find("pub(super) async fn rotate_completed_session")
            .map(|offset| start + offset)
            .expect("handoff implementation end");
        let body = &source[start..end];
        let preflight = body
            .find("prepare_handoff_resume")
            .expect("custody preflight");
        let mutation = body
            .find("apply_handoff_custody_root_mutation_for_test")
            .expect("keyed mutation seam");
        let context = body
            .find("begin_handoff_context_read")
            .expect("ContextRead revalidation");
        let permit_paths = body
            .find("let config_working_dir = context_permit")
            .expect("permit-derived config paths");
        let grant = body
            .find("remove_controller_grant_v1")
            .expect("grant removal");
        let remint = body.find("remint_agent_token").expect("token remint");
        let config = body.find("let config = LaunchConfig").expect("raw config");
        let admission = body.find("admit_invocation").expect("admission");
        let dispatch = body
            .find("super::provider_spawn::spawn_provider_process(")
            .expect("single provider funnel");
        assert!(
            preflight < mutation
                && mutation < context
                && context < permit_paths
                && permit_paths < grant
                && grant < remint
                && remint < config
                && config < admission
                && admission < dispatch
        );
        assert!(!body.contains("sandbox_root.unwrap_or"));
        assert!(!body.contains("sandbox_allocator.allocate("));
    }

    #[test]
    fn h1_v83_rotation_successor_source_ratchet_orders_authenticated_phases() {
        let source = include_str!("rotation.rs");
        let completed_start = source
            .find("pub(super) async fn rotate_completed_session")
            .expect("completed rotation implementation");
        let spawn_start = source
            .find("pub(super) async fn spawn_rotation_child")
            .expect("rotation spawn implementation");
        let completed = &source[completed_start..spawn_start];
        let spawn_end = source[spawn_start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| spawn_start + offset)
            .expect("rotation spawn implementation end");
        let spawn = &source[spawn_start..spawn_end];

        let remove = completed
            .find("completed_guard.remove")
            .expect("exact predecessor removal");
        let authenticate = completed
            .find("prepare_rotation_successor")
            .expect("predecessor authentication");
        let child_id = completed
            .find("Uuid::new_v4")
            .expect("successor id after authentication");
        assert!(remove < authenticate && authenticate < child_id);

        let admission = spawn.find("admit_invocation").expect("admission");
        let durable = spawn
            .find("insert_reserved_rotation_session_with_invocation")
            .expect("atomic successor reservation");
        let bind = spawn
            .find("bind_rotation_successor")
            .expect("successor custody bind");
        let context = spawn
            .find("begin_rotation_context_read")
            .expect("successor context permit");
        let config = spawn
            .find("let mut config = LaunchConfig")
            .expect("permit config");
        let provider = spawn
            .find("spawn_provider_process")
            .expect("provider dispatch");
        assert!(admission < durable && durable < bind && bind < context && context < config);
        assert!(
            config < provider,
            "provider must not precede reservation/bind/context"
        );
        assert!(
            !spawn.contains("rotation_candidate: Option")
                && !spawn.contains("insert_session_with_model_invocation")
                && !spawn.contains("parent_sandbox_inheritable"),
            "rotation may not use late optional authorization, non-atomic row insertion, or fallback inheritance"
        );
        assert!(
            !completed.contains("Uuid::new_v4();\n\n        // ── Circuit"),
            "successor construction must not precede predecessor authentication"
        );
        let monitor = spawn
            .find("let monitor_result")
            .expect("rotation monitor publication boundary");
        assert!(
            !spawn[..monitor].contains("AutofileCause::RotationMonitorPanic"),
            "pre-monitor settlement must not use the generic panic fallback"
        );
        assert!(
            !spawn[monitor..].contains("settle_bound_rotation_failure"),
            "post-publication monitor panic must not call the Starting-only helper"
        );
        assert!(spawn[monitor..].contains("AutofileCause::RotationMonitorPanic"));
    }

    #[test]
    fn h2_rotation_lead_acknowledgement_precedes_controller_success_and_parent_revocation() {
        let source = include_str!("rotation.rs");
        let spawn_start = source
            .find("pub(super) async fn spawn_rotation_child")
            .expect("rotation spawn implementation");
        let spawn_end = source[spawn_start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| spawn_start + offset)
            .expect("rotation spawn implementation end");
        let spawn = &source[spawn_start..spawn_end];
        let preflight = spawn
            .find("preflight_rotation_lead_transfer")
            .expect("durable lead preflight");
        let controller_lookup = spawn
            .find("assigned_idea_for_session_v1")
            .expect("controller lookup");
        let transfer = spawn
            .find("publish_rotation_successor")
            .expect("acknowledged durable lead transfer");
        let controller_success = spawn
            .find("idea_controller_assigned:")
            .expect("controller success publication");
        let parent_revocation = spawn
            .find("revoke_agent_tokens_for_session(&agent_tokens, parent_id)")
            .expect("predecessor token revocation");
        let archival = spawn
            .find("finalize_rotation_predecessor")
            .expect("predecessor archival");
        assert!(preflight < controller_lookup);
        assert!(transfer < controller_success);
        assert!(transfer < parent_revocation && transfer < archival);
        assert_eq!(
            spawn
                .matches("revoke_agent_tokens_for_session(&agent_tokens, parent_id)")
                .count(),
            1,
            "rotation must have one predecessor-revocation point behind lead acknowledgement"
        );
    }

    #[test]
    fn h2_rotation_lead_marks_follow_the_durable_lead_transfer() {
        let source = include_str!("rotation.rs");
        let spawn_start = source
            .find("pub(super) async fn spawn_rotation_child")
            .expect("rotation spawn implementation");
        let spawn_end = source[spawn_start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| spawn_start + offset)
            .expect("rotation spawn implementation end");
        let spawn = &source[spawn_start..spawn_end];
        let transfer = spawn
            .find("publish_rotation_successor")
            .expect("acknowledged durable lead transfer");
        let marks = spawn
            .find("mark_lead_session_runtime")
            .expect("successor lead marks");
        assert!(
            transfer < marks,
            "title re-badge and pin must follow the durable lead transfer, so a \
             refused transfer cannot mark a session for authority it lacks",
        );
        assert_eq!(
            spawn.matches("mark_lead_session_runtime").count(),
            1,
            "rotation must have exactly one successor-marking point",
        );
    }

    /// Manager fixture for driving the rotation statics directly (mirrors
    /// `session::tests::manager`, which is private to that module).
    #[allow(clippy::expect_used)]
    fn rotation_manager_with_context_rotation(
        context_rotation_enabled: bool,
    ) -> (SessionManager, tempfile::TempDir) {
        // Live sandbox fixtures need a disk-backed root: execution scratch
        // correctly rejects tmpfs, which may be the host's TMPDIR.
        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::current_dir().expect("test worktree"))
            .expect("disk-backed rotation fixture");
        let manager = rotation_manager_on(dir.path(), context_rotation_enabled);
        (manager, dir)
    }

    /// A daemon instance over an existing fixture directory: a second
    /// manager on the same database models a restarted daemon.
    #[allow(clippy::expect_used)]
    pub(super) fn rotation_manager_on(
        dir: &std::path::Path,
        context_rotation_enabled: bool,
    ) -> SessionManager {
        let store = crate::store::Store::open(&dir.join("rsi.db")).expect("open store");
        let config = crate::config::Config::from_env();
        let runtime_config = crate::config::RuntimeConfig::from_config(&config);
        SessionManager::new(
            std::sync::Arc::new(EventBus::new(16)),
            store,
            context_rotation_enabled,
            dir.join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.join("sandboxes"),
        )
        .expect("manager")
    }

    pub(super) fn rotation_manager() -> (SessionManager, tempfile::TempDir) {
        rotation_manager_with_context_rotation(false)
    }

    async fn bind_and_archive_ordinary_rotation_for_test(
        manager: &SessionManager,
        parent: &Session,
    ) -> anyhow::Result<crate::sandbox::custody::BoundRotationCustody> {
        manager.store.lock().await.insert_session(parent)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent.id)?;
        let runtime = manager.custody_execution_runtime();
        let candidate = runtime.prepare_rotation_successor(parent).await?;
        let mut child = test_session(Uuid::new_v4(), SessionStatus::Starting);
        child.working_dir = parent.working_dir.clone();
        child.continued_from = Some(parent.id);
        manager.store.lock().await.insert_session(&child)?;
        let bound = runtime
            .bind_rotation_successor(candidate, parent, &child)
            .await
            .map_err(|_| anyhow::anyhow!("bind ordinary rotation fixture"))?;
        anyhow::ensure!(
            runtime
                .finalize_rotation_predecessor(child.id, &bound)
                .await?,
            "archive ordinary rotation fixture"
        );
        Ok(bound)
    }

    struct SqliteRestorationFailureGuard {
        db_path: std::path::PathBuf,
        trigger_name: String,
        installed: bool,
    }

    impl SqliteRestorationFailureGuard {
        fn install(db_path: &std::path::Path, parent_id: Uuid) -> anyhow::Result<Self> {
            let trigger_name = format!("h1_rotation_restore_fail_{}", parent_id.simple());
            let connection = rusqlite::Connection::open(db_path)?;
            connection.execute_batch(&format!(
                "CREATE TRIGGER {trigger_name}
                 BEFORE UPDATE OF status ON sessions
                 WHEN OLD.id='{parent_id}'
                   AND OLD.status='Archived'
                   AND NEW.status='Completed'
                 BEGIN
                   SELECT RAISE(ABORT, 'injected UUID-scoped rotation restoration failure');
                 END;"
            ))?;
            Ok(Self {
                db_path: db_path.to_path_buf(),
                trigger_name,
                installed: true,
            })
        }

        fn remove(&mut self) -> anyhow::Result<()> {
            if self.installed {
                rusqlite::Connection::open(&self.db_path)?
                    .execute_batch(&format!("DROP TRIGGER {}", self.trigger_name))?;
                self.installed = false;
            }
            Ok(())
        }
    }

    impl Drop for SqliteRestorationFailureGuard {
        fn drop(&mut self) {
            let _ = self.remove();
        }
    }

    struct ControllerCandidateTestStreamGuard(Uuid);

    impl Drop for ControllerCandidateTestStreamGuard {
        fn drop(&mut self) {
            super::super::launch::drop_controller_candidate_test_stream(self.0);
        }
    }

    /// Only live-custody fixtures need a Git repository.  Keep it below the
    /// manager fixture root so the SQLite database remains outside the test
    /// project and cannot become an untracked project input.
    fn live_rotation_repository(fixture_root: &std::path::Path) -> std::path::PathBuf {
        let repo = fixture_root.join("live-rotation-repo");
        std::fs::create_dir(&repo).expect("create live handoff repository");
        for args in [
            vec!["init"],
            vec!["config", "user.email", "rotation@example.test"],
            vec!["config", "user.name", "rotation-test"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("run git setup");
            assert!(status.success(), "git setup succeeds");
        }
        std::fs::write(repo.join(".seed"), "rotation custody fixture\n").expect("write git seed");
        let status = std::process::Command::new("git")
            .args(["add", ".seed"])
            .current_dir(&repo)
            .status()
            .expect("stage git seed");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["commit", "-m", "rotation fixture"])
            .current_dir(&repo)
            .status()
            .expect("commit git seed");
        assert!(status.success());
        repo
    }

    struct LiveRotationFixture {
        parent: Session,
        repo: std::path::PathBuf,
        root: std::path::PathBuf,
        branch: String,
        custody_id: Uuid,
    }

    async fn persist_live_rotation_parent(
        manager: &SessionManager,
        fixture_root: &std::path::Path,
        project_id: Option<Uuid>,
    ) -> LiveRotationFixture {
        persist_live_rotation_parent_with_id(manager, fixture_root, project_id, Uuid::new_v4())
            .await
    }

    async fn persist_live_rotation_parent_with_id(
        manager: &SessionManager,
        fixture_root: &std::path::Path,
        project_id: Option<Uuid>,
        parent_id: Uuid,
    ) -> LiveRotationFixture {
        let repo = live_rotation_repository(fixture_root);
        let allocation = crate::sandbox::SandboxAllocator::new(fixture_root.join("sandboxes"))
            .allocate(parent_id, &repo, SandboxKind::GitWorktree, "HEAD", None)
            .expect("allocate real rotation worktree");
        let branch = allocation.branch.clone().expect("rotation worktree branch");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&allocation.root)
                .output()
                .expect("read rotation worktree metadata");
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .expect("rotation git output utf8")
                .trim()
                .to_owned()
        };
        let repository_identity = std::fs::canonicalize(git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))
        .expect("canonical rotation git common dir");
        let source_commit = git(&["rev-parse", "HEAD"]);
        let custody_id = Uuid::new_v4();
        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.working_dir = repo.clone();
        parent.project_id = project_id;
        parent.sandbox_kind = Some(SandboxKind::GitWorktree);
        parent.sandbox_root = Some(allocation.root.clone());
        parent.sandbox_branch = Some(branch.clone());
        parent.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        parent.git_branch = Some(branch.clone());
        manager
            .store
            .lock()
            .await
            .insert_session_with_custody(
                &parent,
                crate::store::sandbox_custody::SessionCustodyBinding::New(
                    crate::store::sandbox_custody::NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: repo.display().to_string(),
                        sandbox_root: allocation.root.display().to_string(),
                        sandbox_branch: branch.clone(),
                        repository_identity: repository_identity.display().to_string(),
                        source_commit,
                        cause: crate::store::sandbox_custody::CustodyCause::FreshLaunch,
                    },
                ),
            )
            .expect("persist live rotation parent");
        LiveRotationFixture {
            parent,
            repo,
            root: allocation.root,
            branch,
            custody_id,
        }
    }

    fn insert_rotation_invocation_fixture(
        store: &Store,
        session_id: Uuid,
        invocation_id: Uuid,
        status: &str,
    ) {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations (
                     id, purpose, invocation_kind, foreground, paid_risk,
                     admission_status, status, trigger_source, session_id,
                     policy_snapshot_json, created_at, started_at
                 ) VALUES (
                     ?1, 'session.rotate.child', 'model', 'foreground', 'paid_capable',
                     'admitted', ?3, 'h1_v83_rotation_custody_test', ?2,
                     '{}', ?4, ?4
                 )",
                rusqlite::params![
                    invocation_id.to_string(),
                    session_id.to_string(),
                    status,
                    now,
                ],
            )
            .expect("insert rotation invocation fixture");
    }

    async fn spawn_rotation_child_for_test(
        manager: &SessionManager,
        parent: CompletedSession,
        child: Session,
        candidate: crate::sandbox::custody::RotationCustodyCandidate,
        rotation_id: Option<String>,
    ) {
        try_spawn_rotation_child_for_test(manager, parent, child, candidate, rotation_id)
            .await
            .expect("rotation child settles within bounded timeout");
    }

    /// A rotation that is *expected* to be fenced settles immediately. Callers
    /// that assert on the resulting lineage use this variant so a rotation
    /// which wrongly proceeds fails on the lineage assertion, with a message
    /// naming the defect, rather than on a bounded-timeout panic.
    async fn try_spawn_rotation_child_for_test(
        manager: &SessionManager,
        parent: CompletedSession,
        child: Session,
        candidate: crate::sandbox::custody::RotationCustodyCandidate,
        rotation_id: Option<String>,
    ) -> std::result::Result<(), tokio::time::error::Elapsed> {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            SessionManager::spawn_rotation_child(
                parent.session.id,
                None,
                child.query.clone(),
                child,
                candidate,
                Some(parent),
                rotation_id,
                None,
                Arc::clone(&manager.active),
                Arc::clone(&manager.completed),
                Arc::clone(&manager.event_bus),
                Arc::clone(&manager.store),
                manager
                    .model_call_settlements
                    .handle()
                    .expect("settlement producer"),
                manager.persistence.clone(),
                false,
                manager.socket_path.clone(),
                Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                Arc::clone(&manager.runtime_config),
                Arc::clone(&manager.spawn_coordinator),
                Arc::clone(&manager.agent_tokens),
                Arc::clone(&manager.spawn_epoch),
                Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
                None,
                None,
            ),
        )
        .await
    }

    async fn rotate_completed_session_for_test(manager: &SessionManager, session_id: Uuid) {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            SessionManager::rotate_completed_session(
                session_id,
                None,
                Some(format!("h1-v83-predecessor-refusal:{session_id}")),
                Arc::clone(&manager.active),
                Arc::clone(&manager.completed),
                Arc::clone(&manager.event_bus),
                Arc::clone(&manager.store),
                manager
                    .model_call_settlements
                    .handle()
                    .expect("settlement producer"),
                manager.persistence.clone(),
                false,
                manager.socket_path.clone(),
                Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                Arc::clone(&manager.runtime_config),
                Arc::clone(&manager.spawn_coordinator),
                Arc::clone(&manager.agent_tokens),
                Arc::clone(&manager.spawn_epoch),
                Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
            ),
        )
        .await
        .expect("completed rotation refusal returns within bounded timeout");
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    pub(super) async fn rotate_task_successor_for_test(
        manager: &SessionManager,
        parent_id: Uuid,
    ) -> anyhow::Result<Session> {
        let child_id = Uuid::new_v4();
        install_rotation_child_id_for_test(parent_id, child_id);
        super::super::launch::install_controller_candidate_test_process(child_id);
        let release = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !manager.active.read().await.contains_key(&child_id) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("rotation child launches");
            super::super::launch::drop_controller_candidate_test_stream(child_id);
        };
        tokio::join!(
            rotate_completed_session_for_test(manager, parent_id),
            release
        );
        manager
            .store
            .lock()
            .await
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("rotation child row missing"))
    }

    pub(super) fn terminal_rotation_events(
        store: &Store,
        parent_id: Uuid,
    ) -> anyhow::Result<Vec<(String, Option<String>)>> {
        let mut statement = store.conn.prepare(
            "SELECT event_type,metadata FROM rotation_events WHERE session_id=?1 AND (event_type='completed' OR event_type='suppressed_final_handoff' OR event_type LIKE 'refused:%') ORDER BY id",
        )?;
        Ok(statement
            .query_map([parent_id.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn final_handoff_event(session_id: Uuid) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id,
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: chrono::Utc::now(),
            content: "PIPELINE HANDOFF — IMPLEMENT: status complete".into(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn final_handoff_fixture(
        manager: &SessionManager,
        root: &Path,
        lead: bool,
    ) -> anyhow::Result<(Uuid, Uuid)> {
        let epic_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let mut epic = test_session(epic_id, SessionStatus::Completed);
        epic.session_kind = SessionKind::Epic;
        let mut worker = test_session(worker_id, SessionStatus::Completed);
        worker.parent_id = Some(epic_id);
        worker.working_dir = root.to_path_buf();
        worker.query = "Objective final handoff".into();
        let owner = test_session(owner_id, SessionStatus::Completed);
        {
            let mut guard = manager.store.lock().await;
            guard.insert_session(&epic)?;
            guard.insert_session(&owner)?;
            guard.insert_session(&worker)?;
            guard.publish_startup_ordinary(worker_id)?;
            if lead {
                guard.set_lead_session(epic_id, Some(worker_id))?;
            }
            let now = chrono::Utc::now();
            guard.insert_scheduled_job(&rsi_common::types::ScheduledJob {
                id: Uuid::new_v4(),
                name: "final handoff watch".into(),
                message: String::new(),
                schedule: rsi_common::types::ScheduleSpec {
                    recurrence: rsi_common::types::Recurrence::EverySeconds(60),
                    anchor: now,
                },
                last_fired_at: None,
                next_fire_at: now,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: rsi_common::types::WakeMode::OnTerminal(worker_id),
                wake_session_id: Some(owner_id),
            })?;
        }
        let mut done = CompletedSession::for_test(worker);
        done.events.push(final_handoff_event(worker_id));
        manager.completed.write().await.insert(worker_id, done);
        Ok((worker_id, owner_id))
    }

    #[allow(clippy::expect_used, clippy::large_futures)]
    async fn post_finalize_action_for_test(
        manager: &SessionManager,
        session_id: Uuid,
        action: super::super::rotation_coordinator::RotationAction,
        rotation_id: &str,
    ) -> PostFinalizeRotationOutcome {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            SessionManager::execute_post_finalization_rotation_action(
                session_id,
                action,
                Some(rotation_id.into()),
                Arc::clone(&manager.active),
                Arc::clone(&manager.completed),
                Arc::clone(&manager.event_bus),
                Arc::clone(&manager.store),
                manager
                    .model_call_settlements
                    .handle()
                    .expect("settlement producer"),
                manager.persistence.clone(),
                false,
                manager.socket_path.clone(),
                Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                Arc::clone(&manager.runtime_config),
                Arc::clone(&manager.spawn_coordinator),
                Arc::clone(&manager.agent_tokens),
                Arc::clone(&manager.spawn_epoch),
                Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
            ),
        )
        .await
        .expect("post-finalization decision completes")
    }

    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::large_futures,
        clippy::significant_drop_tightening
    )]
    async fn final_pipeline_handoff_to_waiting_parent_suppresses_successor() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (worker, _) = final_handoff_fixture(&manager, dir.path(), false).await?;
        let outcome = post_finalize_action_for_test(
            &manager,
            worker,
            super::super::rotation_coordinator::RotationAction::SpawnChild {
                session_id: worker,
                handoff_filepath: Some("thoughts/shared/handoffs/own.md".into()),
            },
            "final-suppress",
        )
        .await;
        manager.persistence.barrier().await?;
        assert_eq!(
            outcome,
            PostFinalizeRotationOutcome::ContinueNormalCompletion
        );
        let guard = manager.store.lock().await;
        assert_eq!(
            guard.get_session(worker)?.unwrap().status,
            SessionStatus::Completed
        );
        assert_eq!(guard.find_rotation_successor(worker)?, None);
        let events = terminal_rotation_events(&guard, worker)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "suppressed_final_handoff");
        let metadata: serde_json::Value = serde_json::from_str(events[0].1.as_deref().unwrap())?;
        assert_eq!(
            metadata["handoff_filepath"],
            "thoughts/shared/handoffs/own.md"
        );
        assert!(
            guard.list_scheduled_jobs()?.iter().any(|job| job.enabled
                && job.wake_mode == rsi_common::types::WakeMode::OnTerminal(worker))
        );
        Ok(())
    }

    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::large_futures,
        clippy::significant_drop_tightening
    )]
    async fn pending_interrupt_turn_ending_in_final_handoff_skips_create_handoff()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (worker, _) = final_handoff_fixture(&manager, dir.path(), false).await?;
        assert!(should_suppress_final_handoff(worker, &manager.completed, &manager.store).await?);
        let outcome = post_finalize_action_for_test(
            &manager,
            worker,
            super::super::rotation_coordinator::RotationAction::SendCreateHandoff,
            "pending-final",
        )
        .await;
        manager.persistence.barrier().await?;
        assert_eq!(
            outcome,
            PostFinalizeRotationOutcome::ContinueNormalCompletion
        );
        assert_eq!(
            terminal_rotation_events(&*manager.store.lock().await, worker)?.len(),
            1
        );
        assert_eq!(
            manager.store.lock().await.find_rotation_successor(worker)?,
            None
        );
        Ok(())
    }

    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::large_futures,
        clippy::significant_drop_tightening
    )]
    async fn final_pipeline_handoff_from_epic_lead_still_rotates() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (worker, _) = final_handoff_fixture(&manager, dir.path(), false).await?;
        let worker_outcome = post_finalize_action_for_test(
            &manager,
            worker,
            super::super::rotation_coordinator::RotationAction::SpawnChild {
                session_id: worker,
                handoff_filepath: None,
            },
            "worker-before-lead",
        )
        .await;
        assert_eq!(
            worker_outcome,
            PostFinalizeRotationOutcome::ContinueNormalCompletion
        );
        let (worker, _) = final_handoff_fixture(&manager, dir.path(), true).await?;
        let child = Uuid::new_v4();
        install_rotation_child_id_for_test(worker, child);
        super::super::launch::install_controller_candidate_test_process(child);
        let release = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !manager.active.read().await.contains_key(&child) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("lead successor launches");
            super::super::launch::drop_controller_candidate_test_stream(child);
        };
        let (outcome, ()) = tokio::join!(
            post_finalize_action_for_test(
                &manager,
                worker,
                super::super::rotation_coordinator::RotationAction::SpawnChild {
                    session_id: worker,
                    handoff_filepath: None,
                },
                "lead-final"
            ),
            release
        );
        assert_eq!(outcome, PostFinalizeRotationOutcome::Handled);
        let guard = manager.store.lock().await;
        assert_eq!(guard.find_rotation_successor(worker)?, Some(child));
        let events = terminal_rotation_events(&guard, worker)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        Ok(())
    }

    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::large_futures,
        clippy::significant_drop_tightening,
        clippy::too_many_lines,
        clippy::uninlined_format_args
    )]
    async fn foreign_newer_handoff_in_tree_is_not_bound() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let repo = live_rotation_repository(dir.path());
        let foreign = "thoughts/shared/handoffs/foreign/newer.md";
        let own = "thoughts/shared/handoffs/own/own.md";
        for path in [foreign, own] {
            std::fs::create_dir_all(repo.join(path).parent().unwrap())?;
        }
        std::fs::write(repo.join(foreign), "foreign handoff")?;
        let git = |args: &[&str]| -> anyhow::Result<String> {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()?;
            anyhow::ensure!(output.status.success(), "git {:?} failed", args);
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        git(&["add", foreign])?;
        git(&["commit", "-m", "foreign handoff"])?;
        let start_head = git(&["rev-parse", "HEAD"])?;
        std::fs::write(repo.join(own), "own handoff")?;
        git(&["add", own])?;
        git(&["commit", "-m", "own handoff"])?;
        let own_commit = git(&["rev-parse", "HEAD"])?;
        let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        parent.working_dir = repo.clone();
        parent.query = "Objective own task".into();
        parent.handoff_filepath = Some(foreign.into());
        let rotation_id = format!("foreign-{}", parent.id);
        {
            let mut guard = manager.store.lock().await;
            guard.insert_session(&parent)?;
            guard.publish_startup_ordinary(parent.id)?;
            guard.insert_rotation_event(
                parent.id,
                &rotation_id,
                "writing_handoff",
                "entered",
                Some(&serde_json::json!({"start_head": start_head}).to_string()),
            )?;
        }
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(parent.clone()));
        let child = Uuid::new_v4();
        install_rotation_child_id_for_test(parent.id, child);
        super::super::launch::install_controller_candidate_test_process(child);
        let release = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !manager.active.read().await.contains_key(&child) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("task successor launches");
            super::super::launch::drop_controller_candidate_test_stream(child);
        };
        tokio::join!(
            SessionManager::rotate_completed_session(
                parent.id,
                Some(foreign.into()),
                Some(rotation_id.clone()),
                Arc::clone(&manager.active),
                Arc::clone(&manager.completed),
                Arc::clone(&manager.event_bus),
                Arc::clone(&manager.store),
                manager
                    .model_call_settlements
                    .handle()
                    .expect("settlement producer"),
                manager.persistence.clone(),
                false,
                manager.socket_path.clone(),
                Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                Arc::clone(&manager.runtime_config),
                Arc::clone(&manager.spawn_coordinator),
                Arc::clone(&manager.agent_tokens),
                Arc::clone(&manager.spawn_epoch),
                Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
            ),
            release
        );
        let guard = manager.store.lock().await;
        let child_row = guard.get_session(child)?.expect("successor row");
        assert!(child_row.query.starts_with("Objective own task"));
        assert_eq!(
            guard.get_session(parent.id)?.unwrap().handoff_filepath,
            None
        );
        let rejected: i64 = guard.conn.query_row(
            "SELECT count(*) FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND event_type='handoff_rejected_unbound'",
            rusqlite::params![parent.id.to_string(), rotation_id], |row| row.get(0))?;
        assert_eq!(rejected, 1);
        let events = terminal_rotation_events(&guard, parent.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        drop(guard);

        let mut own_parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        own_parent.working_dir = repo;
        own_parent.query = "Objective bound task".into();
        let own_rotation_id = format!("bound-{}", own_parent.id);
        {
            let mut guard = manager.store.lock().await;
            guard.insert_session(&own_parent)?;
            guard.publish_startup_ordinary(own_parent.id)?;
            guard.insert_rotation_event(
                own_parent.id,
                &own_rotation_id,
                "writing_handoff",
                "entered",
                Some(&serde_json::json!({"start_head": start_head}).to_string()),
            )?;
        }
        manager.completed.write().await.insert(
            own_parent.id,
            CompletedSession::for_test(own_parent.clone()),
        );
        let own_child = Uuid::new_v4();
        install_rotation_child_id_for_test(own_parent.id, own_child);
        super::super::launch::install_controller_candidate_test_process(own_child);
        let release_own = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !manager.active.read().await.contains_key(&own_child) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("bound successor launches");
            super::super::launch::drop_controller_candidate_test_stream(own_child);
        };
        tokio::join!(
            SessionManager::rotate_completed_session(
                own_parent.id,
                Some(own.into()),
                Some(own_rotation_id),
                Arc::clone(&manager.active),
                Arc::clone(&manager.completed),
                Arc::clone(&manager.event_bus),
                Arc::clone(&manager.store),
                manager
                    .model_call_settlements
                    .handle()
                    .expect("settlement producer"),
                manager.persistence.clone(),
                false,
                manager.socket_path.clone(),
                Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                Arc::clone(&manager.runtime_config),
                Arc::clone(&manager.spawn_coordinator),
                Arc::clone(&manager.agent_tokens),
                Arc::clone(&manager.spawn_epoch),
                Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
            ),
            release_own
        );
        manager.persistence.barrier().await?;
        let guard = manager.store.lock().await;
        assert_eq!(
            guard
                .get_session(own_parent.id)?
                .unwrap()
                .handoff_filepath
                .as_deref(),
            Some(own)
        );
        assert_eq!(
            guard.get_session(own_child)?.unwrap().query,
            format!("/resume_handoff {own}")
        );
        let own_events = terminal_rotation_events(&guard, own_parent.id)?;
        assert_eq!(own_events.len(), 1);
        let own_metadata: serde_json::Value =
            serde_json::from_str(own_events[0].1.as_deref().unwrap())?;
        assert_eq!(own_metadata["handoff_commit"], own_commit);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn rotation_without_bound_handoff_resumes_original_task() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        parent.query = "Objective X".into();
        manager.store.lock().await.insert_session(&parent)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent.id)?;
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(parent.clone()));
        let child = Box::pin(rotate_task_successor_for_test(&manager, parent.id)).await?;
        assert!(child.query.starts_with("Objective X"));
        assert!(child.query.contains("Rotation continuation of"));
        assert_ne!(child.query.trim(), ROTATION_HANDOFF_PROMPT);
        let events = terminal_rotation_events(&*manager.store.lock().await, parent.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(events[0].1.as_deref().unwrap())?["query_kind"],
            "task"
        );
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn rotation_chain_task_resolution_skips_create_handoff_rows() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut root = test_session(Uuid::new_v4(), SessionStatus::Archived);
        root.working_dir = dir.path().to_path_buf();
        root.query = "Objective X".into();
        let mut mid = test_session(Uuid::new_v4(), SessionStatus::Completed);
        mid.working_dir = dir.path().to_path_buf();
        mid.query = ROTATION_HANDOFF_PROMPT.into();
        mid.continued_from = Some(root.id);
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&root)?;
            store.insert_session(&mid)?;
            store.publish_startup_ordinary(mid.id)?;
        }
        manager
            .completed
            .write()
            .await
            .insert(mid.id, CompletedSession::for_test(mid.clone()));
        let child = Box::pin(rotate_task_successor_for_test(&manager, mid.id)).await?;
        assert!(child.query.starts_with("Objective X"));
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn lead_at_depth_three_with_progress_rotates_to_depth_four() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        fixture.parent.rotation_depth = 3;
        let epic_id = Uuid::new_v4();
        let mut group = test_session(Uuid::new_v4(), SessionStatus::Completed);
        group.session_kind = SessionKind::Group;
        let mut epic = test_session(epic_id, SessionStatus::Completed);
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        fixture.parent.parent_id = Some(epic_id);
        {
            let store = manager.store.lock().await;
            store.insert_session(&group)?;
            store.insert_session(&epic)?;
            store.conn.execute(
                "UPDATE sessions SET rotation_depth=3,parent_id=?2 WHERE id=?1",
                rusqlite::params![fixture.parent.id.to_string(), epic_id.to_string()],
            )?;
            store.set_lead_session(epic_id, Some(fixture.parent.id))?;
        }
        manager.completed.write().await.insert(
            fixture.parent.id,
            CompletedSession::for_test(fixture.parent.clone()),
        );

        let child = Box::pin(rotate_task_successor_for_test(&manager, fixture.parent.id)).await?;

        assert_eq!(child.rotation_depth, 4);
        assert_eq!(child.continued_from, Some(fixture.parent.id));
        assert_eq!(
            terminal_rotation_events(&*manager.store.lock().await, fixture.parent.id)?[0].0,
            "completed"
        );
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::large_futures)]
    async fn failing_child_cascade_is_still_stopped() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (session_id, _) = final_handoff_fixture(&manager, dir.path(), true).await?;
        if let Some(turn) = manager.completed.write().await.get_mut(&session_id) {
            turn.session.status = SessionStatus::Failed;
        }

        let outcome = post_finalize_action_for_test(
            &manager,
            session_id,
            super::super::rotation_coordinator::RotationAction::SpawnChild {
                session_id,
                handoff_filepath: None,
            },
            "failing-cascade",
        )
        .await;
        manager.persistence.barrier().await?;

        assert_eq!(outcome, PostFinalizeRotationOutcome::Handled);
        let store = manager.store.lock().await;
        let events = terminal_rotation_events(&store, session_id)?;
        assert_eq!(events, vec![("refused:no_progress".into(), None)]);
        drop(store);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn rate_window_refuses_burst_rotations() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut ancestors = Vec::new();
        for depth in 0..3 {
            let mut ancestor = test_session(Uuid::new_v4(), SessionStatus::Archived);
            ancestor.working_dir = dir.path().to_path_buf();
            ancestor.query = "Objective rate window".into();
            ancestor.rotation_depth = depth;
            ancestor.continued_from = ancestors.last().copied();
            manager.store.lock().await.insert_session(&ancestor)?;
            manager.store.lock().await.insert_rotation_event(
                ancestor.id,
                &format!("recent-success-{depth}"),
                "completed",
                "completed",
                None,
            )?;
            ancestors.push(ancestor.id);
        }
        let mut current = test_session(Uuid::new_v4(), SessionStatus::Completed);
        current.working_dir = dir.path().to_path_buf();
        current.query = "Objective rate window".into();
        current.rotation_depth = 3;
        current.continued_from = ancestors.last().copied();
        manager.store.lock().await.insert_session(&current)?;
        manager
            .completed
            .write()
            .await
            .insert(current.id, CompletedSession::for_test(current.clone()));

        Box::pin(rotate_completed_session_for_test(&manager, current.id)).await;

        let store = manager.store.lock().await;
        let events = terminal_rotation_events(&store, current.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "refused:rate_limited");
        drop(store);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn epic_lead_rotation_refusal_emits_durable_notice() -> anyhow::Result<()> {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;

        let (manager, _dir) = rotation_manager();
        let project = Project {
            id: Uuid::new_v4(),
            name: "Rotation refusal notice".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let mut manager_session = test_session(Uuid::new_v4(), SessionStatus::Completed);
        manager_session.project_id = Some(project.id);
        let mut group = test_session(Uuid::new_v4(), SessionStatus::Completed);
        group.project_id = Some(project.id);
        group.session_kind = SessionKind::Group;
        let epic_id = Uuid::new_v4();
        let mut epic = test_session(epic_id, SessionStatus::Completed);
        epic.project_id = Some(project.id);
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        let mut lead = test_session(Uuid::new_v4(), SessionStatus::Completed);
        lead.project_id = Some(project.id);
        lead.parent_id = Some(epic_id);
        lead.session_kind = SessionKind::Feature;
        {
            let store = manager.store.lock().await;
            store.insert_project(&project)?;
            store.insert_session(&manager_session)?;
            store.insert_session(&group)?;
            store.insert_session(&epic)?;
            store.insert_session(&lead)?;
            store.set_lead_session(epic_id, Some(lead.id))?;
            store.configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project.id,
                session_id: manager_session.id,
                epic_ids: Some(vec![epic_id]),
                expected_row_version: 0,
            })?;
        }
        manager
            .completed
            .write()
            .await
            .insert(lead.id, CompletedSession::for_test(lead.clone()));

        {
            let store = manager.store.lock().await;
            let config = store
                .get_harness_manager_notice_config(project.id)?
                .expect("configured manager notice scope");
            assert!(config.current_session_id.is_some());
            assert!(store.manager_config_covers_epic(&config, epic_id)?);
            assert_eq!(store.manager_lead(project.id, epic_id)?.id, lead.id);
            drop(store);
        }

        SessionManager::record_rotation_refusal(
            lead.id,
            Some("lead-refusal-notice"),
            "rate_limited",
            &manager.completed,
            &manager.event_bus,
            &manager.persistence,
            &manager.store,
        )
        .await;
        manager.persistence.barrier().await?;

        let store = manager.store.lock().await;
        let (notice_count, notice_state): (i64, Option<String>) = store.conn.query_row(
            "SELECT count(*), max(state_json) FROM harness_manager_notices
             WHERE kind='session_state' AND subject_id=?1",
            [lead.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(notice_count, 1);
        assert!(
            notice_state
                .as_deref()
                .is_some_and(|state| state.contains(&lead.id.to_string()))
        );
        let events = terminal_rotation_events(&store, lead.id)?;
        assert_eq!(events[0].0, "refused:rate_limited");
        assert!(
            manager.completed.read().await[&lead.id]
                .events
                .iter()
                .any(|event| event.event_type == EventType::System
                    && event.content.contains("rate_limited"))
        );
        drop(store);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn pipeline_artifact_detected_after_insert_does_not_refuse_rotation_custody()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        fixture.parent.pipeline_artifact = Some("thoughts/shared/plans/task.md".into());
        manager
            .store
            .lock()
            .await
            .update_session_metadata(&fixture.parent)?;
        manager.completed.write().await.insert(
            fixture.parent.id,
            CompletedSession::for_test(fixture.parent.clone()),
        );
        let child = Box::pin(rotate_task_successor_for_test(&manager, fixture.parent.id)).await?;
        assert_eq!(child.continued_from, Some(fixture.parent.id));
        assert_eq!(
            terminal_rotation_events(&*manager.store.lock().await, fixture.parent.id)?[0].0,
            "completed"
        );
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn custody_refusal_records_refused_event_and_keeps_predecessor_completed()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        parent.query = "Objective X".into();
        manager.store.lock().await.insert_session(&parent)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent.id)?;
        parent.query = "changed only in memory".into();
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(parent.clone()));
        Box::pin(rotate_completed_session_for_test(&manager, parent.id)).await;
        let events = terminal_rotation_events(&*manager.store.lock().await, parent.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "refused:custody_changed");
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_session(parent.id)?
                .unwrap()
                .status,
            SessionStatus::Completed
        );
        assert!(
            manager
                .completed
                .read()
                .await
                .get(&parent.id)
                .unwrap()
                .events
                .iter()
                .any(|event| event.event_type == EventType::System
                    && event.content.contains("Context rotation refused"))
        );
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn rotation_without_task_records_no_task_refusal() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        parent.query = ROTATION_HANDOFF_PROMPT.into();
        manager.store.lock().await.insert_session(&parent)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent.id)?;
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(parent.clone()));
        Box::pin(rotate_completed_session_for_test(&manager, parent.id)).await;
        let events = terminal_rotation_events(&*manager.store.lock().await, parent.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "refused:no_task");
        assert!(
            manager
                .completed
                .read()
                .await
                .get(&parent.id)
                .unwrap()
                .events
                .iter()
                .any(|event| event.event_type == EventType::System
                    && event.content.contains("no_task"))
        );
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn rotation_replayed_decision_keeps_one_terminal_event() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        parent.query = "Objective X".into();
        manager.store.lock().await.insert_session(&parent)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent.id)?;
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(parent.clone()));
        let child = Box::pin(rotate_task_successor_for_test(&manager, parent.id)).await?;
        assert_eq!(child.continued_from, Some(parent.id));
        Box::pin(rotate_completed_session_for_test(&manager, parent.id)).await;
        let events = terminal_rotation_events(&*manager.store.lock().await, parent.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        Ok(())
    }

    fn rotation_custody_error(error: &DaemonError) -> rsi_common::types::SandboxCustodyErrorV1 {
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected typed rotation custody refusal, got {error}");
        };
        serde_json::from_value(data["error"].clone()).expect("decode typed custody refusal")
    }

    async fn rotation_creation_counts(manager: &SessionManager) -> anyhow::Result<(i64, i64, i64)> {
        let store = manager.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM sessions),
                   (SELECT COUNT(*) FROM session_execution_projections),
                   (SELECT COUNT(*) FROM model_invocations)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(Into::into)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RotationPreflightCounts {
        successor_rows: i64,
        projections: i64,
        provider_invocations: i64,
        controller_reservations: i64,
        context_effects: i64,
        tokens: usize,
        active: usize,
        completed: usize,
        target_directories: usize,
    }

    fn count_target_directories(root: &std::path::Path) -> std::io::Result<usize> {
        if !root.exists() {
            return Ok(0);
        }
        let mut count = 0;
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    if entry.file_name() == "target" {
                        count += 1;
                    }
                    pending.push(entry.path());
                }
            }
        }
        Ok(count)
    }

    async fn rotation_preflight_counts(
        manager: &SessionManager,
        fixture_root: &std::path::Path,
    ) -> anyhow::Result<RotationPreflightCounts> {
        let (
            successor_rows,
            projections,
            provider_invocations,
            controller_reservations,
            context_effects,
        ) = {
            let store = manager.store.lock().await;
            store.conn.query_row(
                "SELECT
                   (SELECT COUNT(*) FROM sessions),
                   (SELECT COUNT(*) FROM session_execution_projections),
                   (SELECT COUNT(*) FROM model_invocations),
                   (SELECT COUNT(*) FROM idea_events WHERE event_type='controller_reserved'),
                   (SELECT COALESCE(SUM(reserved_effects + active_effects), 0)
                      FROM sandbox_custody_roots)",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?
        };
        Ok(RotationPreflightCounts {
            successor_rows,
            projections,
            provider_invocations,
            controller_reservations,
            context_effects,
            tokens: manager.agent_tokens.read().await.len(),
            active: manager.active.read().await.len(),
            completed: manager.completed.read().await.len(),
            target_directories: count_target_directories(fixture_root)?,
        })
    }

    fn bind_d03_rotation_invocation(
        store: &Store,
        session_id: Uuid,
        project_id: Uuid,
        invocation_id: Uuid,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store.conn.execute(
            "INSERT INTO model_invocations (
                     id, purpose, invocation_kind, foreground, paid_risk,
                     admission_status, status, trigger_source, session_id,
                     project_id, policy_snapshot_json, created_at, started_at
                 ) VALUES (
                     ?1, 'session.launch.fresh', 'model', 'foreground', 'paid_capable',
                     'admitted', 'running', 'd03_rotation_test', ?2, ?3, '{}', ?4, ?4
                 )",
            rusqlite::params![
                invocation_id.to_string(),
                session_id.to_string(),
                project_id.to_string(),
                now,
            ],
        )?;
        store
            .set_session_model_invocation(session_id, Some(invocation_id))
            .map_err(anyhow::Error::from)
    }

    async fn d03_rotation_idea_fixture(
        manager: &SessionManager,
        working_dir: &std::path::Path,
        intent: &str,
    ) -> anyhow::Result<(Project, rsi_common::types::Idea)> {
        let now = chrono::Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: format!("D03 rotation {intent}"),
            path: Some(working_dir.to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        let digest = Sha256Digest::parse(format!("sha256:{}", "b".repeat(64)))
            .map_err(anyhow::Error::msg)?;
        let capture = Capture {
            id: Uuid::new_v4(),
            project_id: project.id,
            creator_kind: IdeaActorKind::Operator,
            creator_id: "d03-rotation-test".to_string(),
            captured_at: now,
            source_kind: CaptureSourceKind::OperatorInput,
            raw_content_digest: digest.clone(),
            storage_policy_id: "cas-v1".to_string(),
            content_ref: ContentAddressedRef::for_digest(&digest),
        };
        {
            let store = manager.store.lock().await;
            store.insert_project(&project)?;
            store.insert_d02_capture_fixture(&capture)?;
        }
        let operator = crate::idea_control::IdeaControlHandle::for_operator(
            Arc::clone(&manager.store),
            project.id,
            "d03-rotation-test",
        )?;
        let idea = operator
            .create_idea(&CreateIdeaRequestV1 {
                idempotency_key: format!("d03-rotation-idea:{intent}"),
                genesis_capture_id: capture.id,
                genesis_span: None,
                slug: format!("d03-rotation-{intent}"),
                sigil: Some("D03".to_string()),
                title: "Controller rotation cancellation".to_string(),
                description: "real rotation entry point".to_string(),
                portfolio_summary: "preserve parent controller".to_string(),
                priority: 1,
                autonomy_policy: AutonomyPolicy::CaptureOnly,
                integration_target_ref: "refs/heads/main".to_string(),
                program_template_policy_id: None,
                derived_from_idea_id: None,
                artifact_digests: Vec::new(),
                evidence_digests: Vec::new(),
            })
            .await?
            .idea;
        Ok((project, idea))
    }

    async fn d03_rotation_fixture(
        manager: &SessionManager,
        working_dir: &std::path::Path,
        intent: &str,
    ) -> anyhow::Result<(Project, rsi_common::types::Idea, Session, String)> {
        let (project, idea) = d03_rotation_idea_fixture(manager, working_dir, intent).await?;
        let transfer = crate::idea_control::IdeaControllerTransferHandle::for_system(
            Arc::clone(&manager.store),
            project.id,
            idea.id,
            Arc::new(crate::idea_control::SystemIdeaControllerClock),
        )
        .await?;
        let reserved = transfer
            .reserve(&ReserveIdeaControllerRequestV1 {
                expected_row_version: idea.row_version,
                transfer_intent_key: format!("d03-rotation-parent:{intent}"),
            })
            .await?;
        let reservation = reserved
            .reservation
            .ok_or_else(|| anyhow::anyhow!("initial rotation reservation missing"))?;
        let mut parent = test_session(reservation.candidate_session_id, SessionStatus::Running);
        parent.working_dir = working_dir.to_path_buf();
        parent.project_id = Some(project.id);
        parent.provider = SessionProvider::Claude;
        let invocation_id = Uuid::new_v4();
        {
            let store = manager.store.lock().await;
            store.insert_session(&parent)?;
            bind_d03_rotation_invocation(&store, parent.id, project.id, invocation_id)?;
            drop(store);
        }
        let parent_token = format!("d03-rotation-parent-token:{intent}");
        manager
            .agent_tokens
            .write()
            .await
            .insert(parent_token.clone(), parent.id);
        let assigned = transfer
            .assign_confirmed_guarded(
                &reservation,
                &IdeaControllerLaunchConfirmationV1 {
                    candidate_session_id: parent.id,
                    project_id: project.id,
                    provider: SessionProvider::Claude,
                    admission_invocation_id: invocation_id,
                    durable_session_id: parent.id,
                    durable_project_id: project.id,
                    durable_provider: SessionProvider::Claude,
                    a6_bound_session_id: parent.id,
                    confirmation_kind:
                        rsi_common::types::ControllerConfirmationKindV1::InstalledProvider,
                    confirmed_at: chrono::Utc::now(),
                },
                &parent_token,
                &manager.agent_tokens,
            )
            .await?;
        parent = {
            let store = manager.store.lock().await;
            store.update_session_status(parent.id, SessionStatus::Completed)?;
            store
                .get_session(parent.id)?
                .ok_or_else(|| anyhow::anyhow!("durable ordinary controller parent missing"))?
        };
        Ok((project, assigned.idea, parent, parent_token))
    }

    async fn d03_live_rotation_fixture(
        manager: &SessionManager,
        fixture_root: &std::path::Path,
        intent: &str,
    ) -> anyhow::Result<(
        Project,
        rsi_common::types::Idea,
        LiveRotationFixture,
        String,
    )> {
        let (project, idea) = d03_rotation_idea_fixture(manager, fixture_root, intent).await?;
        let transfer = crate::idea_control::IdeaControllerTransferHandle::for_system(
            Arc::clone(&manager.store),
            project.id,
            idea.id,
            Arc::new(crate::idea_control::SystemIdeaControllerClock),
        )
        .await?;
        let reserved = transfer
            .reserve(&ReserveIdeaControllerRequestV1 {
                expected_row_version: idea.row_version,
                transfer_intent_key: format!("d03-live-rotation-parent:{intent}"),
            })
            .await?;
        let reservation = reserved
            .reservation
            .ok_or_else(|| anyhow::anyhow!("initial live rotation reservation missing"))?;
        let mut fixture = persist_live_rotation_parent_with_id(
            manager,
            fixture_root,
            Some(project.id),
            reservation.candidate_session_id,
        )
        .await;
        fixture.parent.status = SessionStatus::Running;
        fixture.parent.provider = SessionProvider::Claude;
        let invocation_id = Uuid::new_v4();
        {
            let store = manager.store.lock().await;
            store.update_session_status(fixture.parent.id, SessionStatus::Running)?;
            bind_d03_rotation_invocation(&store, fixture.parent.id, project.id, invocation_id)?;
        }
        let parent_token = format!("d03-live-rotation-parent-token:{intent}");
        manager
            .register_agent_token(parent_token.clone(), fixture.parent.id)
            .await;
        let assigned = transfer
            .assign_confirmed_guarded(
                &reservation,
                &IdeaControllerLaunchConfirmationV1 {
                    candidate_session_id: fixture.parent.id,
                    project_id: project.id,
                    provider: SessionProvider::Claude,
                    admission_invocation_id: invocation_id,
                    durable_session_id: fixture.parent.id,
                    durable_project_id: project.id,
                    durable_provider: SessionProvider::Claude,
                    a6_bound_session_id: fixture.parent.id,
                    confirmation_kind:
                        rsi_common::types::ControllerConfirmationKindV1::InstalledProvider,
                    confirmed_at: chrono::Utc::now(),
                },
                &parent_token,
                &manager.agent_tokens,
            )
            .await?;
        fixture.parent = {
            let store = manager.store.lock().await;
            store.update_session_status(fixture.parent.id, SessionStatus::Completed)?;
            store
                .get_session(fixture.parent.id)?
                .ok_or_else(|| anyhow::anyhow!("durable live controller parent missing"))?
        };
        Ok((project, assigned.idea, fixture, parent_token))
    }

    #[tokio::test]
    async fn resume_for_handoff_write_remints_agent_token() {
        // G2 (A6) + D03: the handoff writer re-mints before spawn, revoking
        // the prior process. A failed replacement then revokes its prospective
        // token too; durable semantic ownership remains unchanged but cannot
        // act until another same-ID establishment succeeds.
        let (manager, _dir) = rotation_manager();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, SessionStatus::Completed);
        session.provider = SessionProvider::Codex;
        session.working_dir = std::path::PathBuf::from("/definitely/missing/rsi-a6-g2-dir");
        session.context_window = Some(333_000);
        session.resolved_context_budget = Some(
            rsi_common::ResolvedContextBudget::new(
                333_000,
                rsi_common::ContextCapacity {
                    runtime_effective_tokens: Some(333_000),
                    ..rsi_common::ContextCapacity::default()
                },
                rsi_common::CapabilityEvidence {
                    source: rsi_common::CapabilitySource::RuntimeTelemetry,
                    source_version: None,
                    source_digest: None,
                    observed_at: Some(chrono::Utc::now()),
                    confidence: rsi_common::CapabilityConfidence::Authoritative,
                },
            )
            .expect("positive prior process budget"),
        );

        manager
            .store
            .lock()
            .await
            .insert_session(&session)
            .expect("persist handoff-resume source");

        manager.completed.write().await.insert(
            session_id,
            CompletedSession {
                session,
                events: Vec::new(),
                turn_metrics: Vec::new(),
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );
        manager
            .register_agent_token("stale-writer-token".to_string(), session_id)
            .await;

        SessionManager::resume_for_handoff_write(
            session_id,
            None,
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager
                .model_call_settlements
                .handle()
                .expect("settlement producer"),
            manager.persistence.clone(),
            false,
            manager.socket_path.clone(),
            Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            Arc::clone(&manager.runtime_config),
            Arc::clone(&manager.spawn_coordinator),
            Arc::clone(&manager.agent_tokens),
            Arc::clone(&manager.spawn_epoch),
            Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
        )
        .await;

        assert!(
            manager
                .resolve_agent_token("stale-writer-token")
                .await
                .is_none(),
            "G2 re-mint must revoke the session's prior token"
        );
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|bound| *bound == session_id),
            "failed same-ID establishment must revoke the prospective token"
        );
        assert!(
            manager.completed.read().await.contains_key(&session_id),
            "failed handoff-write spawn must roll the session back into completed"
        );
        let completed = manager.completed.read().await;
        let resumed = &completed
            .get(&session_id)
            .expect("failed spawn restores resumed projection")
            .session;
        let resumed_budget = resumed
            .resolved_context_budget
            .as_ref()
            .expect("handoff resume resolves a typed budget");
        assert_eq!(resumed.context_window, Some(resumed_budget.active_tokens));
        assert_ne!(resumed_budget.active_tokens, 333_000);
        assert_ne!(
            resumed_budget.evidence.source,
            rsi_common::CapabilitySource::RuntimeTelemetry,
            "a new process incarnation must invalidate prior runtime authority"
        );
        let resumed_window = resumed.context_window;
        let resumed_active_tokens = resumed_budget.active_tokens;
        let resumed_evidence = resumed_budget.evidence.clone();
        drop(completed);
        let durable = manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .expect("read durable resumed budget")
            .expect("handoff source remains durable");
        assert_eq!(durable.context_window, resumed_window);
        let durable_budget = durable
            .resolved_context_budget
            .expect("durable handoff resume budget");
        assert_eq!(durable_budget.active_tokens, resumed_active_tokens);
        assert_eq!(durable_budget.evidence, resumed_evidence);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn d03_controller_handoff_real_interrupt_blocks_same_id_reconstruction()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let intent = Uuid::new_v4().simple().to_string();
        let (project, assigned_idea, parent, _parent_token) =
            d03_rotation_fixture(&manager, dir.path(), &intent).await?;
        let session_id = parent.id;
        manager.completed.write().await.insert(
            session_id,
            CompletedSession {
                session: parent,
                events: Vec::new(),
                turn_metrics: Vec::new(),
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );
        let scripted = super::super::launch::install_controller_candidate_test_process(session_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            session_id,
            super::super::launch::ControllerCandidateTestPhase::SameIdBeforeReconstruction,
        );

        let handoff = SessionManager::resume_for_handoff_write(
            session_id,
            Some(format!("d03-handoff:{intent}")),
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager.model_call_settlements.handle()?,
            manager.persistence.clone(),
            false,
            manager.socket_path.clone(),
            Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            Arc::clone(&manager.runtime_config),
            Arc::clone(&manager.spawn_coordinator),
            Arc::clone(&manager.agent_tokens),
            Arc::clone(&manager.spawn_epoch),
            Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
        );
        tokio::pin!(handoff);
        tokio::select! {
            () = &mut handoff => panic!("handoff completed before reconstruction pause"),
            result = reached => {
                result?;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("handoff did not reach reconstruction pause");
            }
        }
        manager.interrupt_session(session_id).await?;
        resume
            .send(())
            .map_err(|()| anyhow::anyhow!("resume handoff reconstruction receiver dropped"))?;
        tokio::time::timeout(std::time::Duration::from_secs(5), handoff).await?;

        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        let store = manager.store.lock().await;
        let projection = store.load_idea_controller_projection_v1(project.id, assigned_idea.id)?;
        assert_eq!(projection.current_controller_session_id, Some(session_id));
        assert_eq!(projection.controller_epoch, assigned_idea.controller_epoch);
        assert_eq!(projection.row_version, assigned_idea.row_version);
        assert!(store.controller_grant_v1(session_id).is_none());
        drop(store);
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|bound| *bound == session_id),
            "failed handoff establishment revokes prospective A6"
        );
        super::super::launch::drop_controller_candidate_test_stream(session_id);
        Ok(())
    }

    #[test]
    fn h1_v83_rotation_custody_atomic_reservation_is_all_or_nothing() {
        let mut store = Store::open_in_memory().expect("open reservation store");
        let parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        store
            .insert_session(&parent)
            .expect("persist rotation parent");

        let mut child = test_session(Uuid::new_v4(), SessionStatus::Starting);
        child.continued_from = Some(parent.id);
        child.query = "atomic rotation reservation".to_string();
        let invocation_id = Uuid::new_v4();
        insert_rotation_invocation_fixture(&store, child.id, invocation_id, "running");
        store
            .insert_reserved_rotation_session_with_invocation(
                &child,
                invocation_id,
                "test-rotation",
            )
            .expect("atomically reserve child and invocation");
        let persisted = store
            .get_session(child.id)
            .expect("load reserved child")
            .expect("reserved child exists");
        assert_eq!(persisted.status, SessionStatus::Starting);
        assert_eq!(persisted.continued_from, Some(parent.id));
        assert_eq!(persisted.query, child.query);
        assert_eq!(
            store
                .session_model_invocation_id(child.id)
                .expect("load child invocation pointer"),
            Some(invocation_id)
        );
        let projection: (String, Option<String>, Option<String>) = store
            .conn
            .query_row(
                "SELECT freshness,effective_cwd,custody_id
                 FROM session_execution_projections WHERE session_id=?1",
                [child.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("load trigger-created reservation projection");
        assert_eq!(projection, ("unverified".into(), None, None));

        for wrong_status in [false, true] {
            let mut rejected = test_session(Uuid::new_v4(), SessionStatus::Starting);
            rejected.continued_from = Some(parent.id);
            let invocation_owner = if wrong_status {
                rejected.id
            } else {
                Uuid::new_v4()
            };
            if invocation_owner != rejected.id {
                let owner = test_session(invocation_owner, SessionStatus::Starting);
                store.insert_session(&owner).expect("persist wrong owner");
            }
            let rejected_invocation = Uuid::new_v4();
            insert_rotation_invocation_fixture(
                &store,
                invocation_owner,
                rejected_invocation,
                if wrong_status { "failed" } else { "running" },
            );
            assert!(
                store
                    .insert_reserved_rotation_session_with_invocation(
                        &rejected,
                        rejected_invocation,
                        "test-rotation",
                    )
                    .is_err(),
                "wrong invocation owner/status must refuse"
            );
            let residue: (i64, i64) = store
                .conn
                .query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM sessions WHERE id=?1),
                       (SELECT COUNT(*) FROM session_execution_projections WHERE session_id=?1)",
                    [rejected.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("count rejected reservation residue");
            assert_eq!(
                residue,
                (0, 0),
                "transaction leaves no half-row or orphan projection"
            );
        }

        let malformed_tuples = [
            (Some(SandboxKind::GitWorktree), None, None, None),
            (
                None,
                Some(std::path::PathBuf::from("/tmp/rotation-partial-root")),
                None,
                None,
            ),
            (
                None,
                None,
                Some("rotation-partial-branch".to_string()),
                None,
            ),
            (None, None, None, Some(SandboxCleanupState::Live)),
            (
                Some(SandboxKind::GitWorktree),
                Some(std::path::PathBuf::from("/tmp/rotation-partial-root")),
                Some("rotation-partial-branch".to_string()),
                None,
            ),
            (Some(SandboxKind::None), None, None, None),
        ];
        for (kind, root, branch, cleanup) in malformed_tuples {
            let mut rejected = test_session(Uuid::new_v4(), SessionStatus::Starting);
            rejected.continued_from = Some(parent.id);
            rejected.sandbox_kind = kind;
            rejected.sandbox_root = root;
            rejected.sandbox_branch = branch;
            rejected.sandbox_cleanup_state = cleanup;
            let invocation_id = Uuid::new_v4();
            insert_rotation_invocation_fixture(&store, rejected.id, invocation_id, "running");
            assert!(
                store
                    .insert_reserved_rotation_session_with_invocation(
                        &rejected,
                        invocation_id,
                        "test-rotation"
                    )
                    .is_err(),
                "partial rotation tuple must refuse atomically"
            );
            let residue: (i64, i64) = store
                .conn
                .query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM sessions WHERE id=?1),
                       (SELECT COUNT(*) FROM session_execution_projections WHERE session_id=?1)",
                    [rejected.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("count malformed reservation residue");
            assert_eq!(residue, (0, 0));
        }

        let mut live = test_session(Uuid::new_v4(), SessionStatus::Starting);
        live.continued_from = Some(parent.id);
        live.sandbox_kind = Some(SandboxKind::GitWorktree);
        live.sandbox_root = Some(std::path::PathBuf::from("/tmp/rotation-complete-root"));
        live.sandbox_branch = Some("rotation-complete-branch".into());
        live.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        let live_invocation = Uuid::new_v4();
        insert_rotation_invocation_fixture(&store, live.id, live_invocation, "running");
        store
            .insert_reserved_rotation_session_with_invocation(
                &live,
                live_invocation,
                "test-rotation",
            )
            .expect("complete live pre-bind tuple is structurally reservable");
        let live_projection: (String, Option<String>, Option<String>) = store
            .conn
            .query_row(
                "SELECT freshness,effective_cwd,custody_id
                 FROM session_execution_projections WHERE session_id=?1",
                [live.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("load complete live pre-bind projection");
        assert_eq!(live_projection, ("unverified".into(), None, None));
    }

    async fn assert_h1_v83_rotation_bind_refusal(
        manager: &SessionManager,
        child_id: Uuid,
    ) -> anyhow::Result<()> {
        assert_eq!(
            take_rotation_context_read_observation_for_test(child_id),
            Some(false),
            "bind refusal must precede ContextRead"
        );
        assert!(take_rotation_config_observation_for_test(child_id).is_none());
        assert!(
            take_rotation_provider_unavailable_for_test(child_id),
            "provider seam must remain unconsumed"
        );
        assert!(!manager.active.read().await.contains_key(&child_id));
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|session_id| *session_id == child_id)
        );
        let store = manager.store.lock().await;
        let child = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("settled bind-refused child missing"))?;
        assert_eq!(child.status, SessionStatus::Failed);
        assert_eq!(
            child.stop_reason.as_deref(),
            Some("sandbox_custody:custody_changed")
        );
        let projection: (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = store.conn.query_row(
            "SELECT execution_state,freshness,effective_cwd,custody_id,
                    custody_generation,error_code
             FROM session_execution_projections WHERE session_id=?1",
            [child_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        assert_eq!(
            projection,
            (
                "invalid".into(),
                "invalid".into(),
                None,
                None,
                None,
                Some("custody_changed".into()),
            )
        );
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("bind-refused child invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            ("failed".into(), Some("custody_bind_failed".into()))
        );
        assert!(store.controller_grant_v1(child_id).is_none());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_predecessor_refusal_matrix_has_no_successor_effects()
    -> anyhow::Result<()> {
        use rsi_common::types::{SandboxCustodyErrorCodeV1, SandboxCustodyTransitionV1};

        for (mut session, expected) in [
            (
                {
                    let mut session = test_session(Uuid::new_v4(), SessionStatus::Completed);
                    session.sandbox_kind = Some(SandboxKind::GitWorktree);
                    session.sandbox_root = Some(std::path::PathBuf::from("/tmp/partial-rotation"));
                    session
                },
                SandboxCustodyErrorCodeV1::TupleIncomplete,
            ),
            (
                {
                    let mut session = test_session(Uuid::new_v4(), SessionStatus::Completed);
                    session.sandbox_kind = Some(SandboxKind::GitWorktree);
                    session.sandbox_cleanup_state = Some(SandboxCleanupState::Purged);
                    session
                },
                SandboxCustodyErrorCodeV1::HistoricalPurged,
            ),
            (
                {
                    let mut session = test_session(Uuid::new_v4(), SessionStatus::Completed);
                    session.sandbox_kind = Some(SandboxKind::GitWorktree);
                    session.sandbox_cleanup_state = Some(SandboxCleanupState::Failed);
                    session
                },
                SandboxCustodyErrorCodeV1::CleanupFailed,
            ),
            (
                {
                    let mut session = test_session(Uuid::new_v4(), SessionStatus::Completed);
                    session.sandbox_kind = Some(SandboxKind::GitWorktree);
                    session.sandbox_root =
                        Some(std::path::PathBuf::from("/tmp/missing-rotation-owner"));
                    session.sandbox_branch = Some("missing-rotation-owner".into());
                    session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
                    session
                },
                SandboxCustodyErrorCodeV1::OwnershipMissing,
            ),
        ] {
            let (manager, dir) = rotation_manager();
            session.working_dir = dir.path().to_path_buf();
            manager.store.lock().await.insert_session(&session)?;
            let snapshot = serde_json::to_value(&session)?;
            manager
                .completed
                .write()
                .await
                .insert(session.id, CompletedSession::for_test(session.clone()));
            manager
                .register_agent_token(format!("predecessor-refusal:{}", session.id), session.id)
                .await;
            let error = match manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&session)
                .await
            {
                Ok(_) => panic!("predecessor must refuse before successor creation"),
                Err(error) => error,
            };
            let typed = rotation_custody_error(&error);
            assert_eq!(typed.code, expected);
            assert_eq!(typed.transition, SandboxCustodyTransitionV1::Rotation);
            let before = rotation_creation_counts(&manager).await?;
            rotate_completed_session_for_test(&manager, session.id).await;
            assert_eq!(rotation_creation_counts(&manager).await?, before);
            assert!(!manager.active.read().await.contains_key(&session.id));
            let restored = manager.completed.read().await;
            assert_eq!(
                serde_json::to_value(
                    &restored
                        .get(&session.id)
                        .ok_or_else(|| anyhow::anyhow!("exact completed snapshot not restored"))?
                        .session
                )?,
                snapshot
            );
            drop(restored);
            assert_eq!(
                manager
                    .agent_tokens
                    .read()
                    .await
                    .values()
                    .filter(|bound| **bound == session.id)
                    .count(),
                1
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_7f3e_ordinary_durable_preflight_refuses_missing_substituted_and_stale_rows()
    -> anyhow::Result<()> {
        use rsi_common::types::{SandboxCustodyErrorCodeV1, SandboxCustodyTransitionV1};

        for scenario in ["missing", "substituted", "stale"] {
            let (manager, dir) = rotation_manager();
            let parent_id = Uuid::new_v4();
            let mut snapshot = test_session(parent_id, SessionStatus::Completed);
            snapshot.working_dir = dir.path().to_path_buf();
            match scenario {
                "missing" => {}
                "substituted" => {
                    let mut durable = snapshot.clone();
                    durable.query = "substituted durable ordinary predecessor".into();
                    manager.store.lock().await.insert_session(&durable)?;
                }
                "stale" => {
                    manager.store.lock().await.insert_session(&snapshot)?;
                    manager.store.lock().await.conn.execute(
                        "UPDATE sessions SET parent_id='00000000-0000-4000-8000-000000000002' \
                         WHERE id=?1",
                        [parent_id.to_string()],
                    )?;
                }
                _ => unreachable!(),
            }
            let snapshot_json = serde_json::to_value(&snapshot)?;
            manager
                .completed
                .write()
                .await
                .insert(parent_id, CompletedSession::for_test(snapshot.clone()));
            let parent_token = format!("ordinary-preflight:{scenario}:{parent_id}");
            manager
                .register_agent_token(parent_token.clone(), parent_id)
                .await;

            let error = match manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&snapshot)
                .await
            {
                Ok(_) => panic!("durable predecessor fence must refuse"),
                Err(error) => error,
            };
            let typed = rotation_custody_error(&error);
            assert_eq!(
                typed.code,
                if scenario == "missing" {
                    SandboxCustodyErrorCodeV1::OwnershipMissing
                } else {
                    SandboxCustodyErrorCodeV1::CustodyChanged
                }
            );
            assert_eq!(typed.transition, SandboxCustodyTransitionV1::Rotation);

            let before = rotation_preflight_counts(&manager, dir.path()).await?;
            rotate_completed_session_for_test(&manager, parent_id).await;
            assert_eq!(
                rotation_preflight_counts(&manager, dir.path()).await?,
                before,
                "{scenario} durable predecessor refusal must precede every successor, invocation, controller, token, context, provider, and target effect"
            );
            let completed = manager.completed.read().await;
            assert_eq!(
                serde_json::to_value(
                    &completed
                        .get(&parent_id)
                        .ok_or_else(|| anyhow::anyhow!("local predecessor visibility missing"))?
                        .session
                )?,
                snapshot_json
            );
            drop(completed);
            assert_eq!(
                manager.resolve_agent_token(&parent_token).await,
                Some(parent_id)
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_missing_and_substituted_live_evidence_quarantines_before_child()
    -> anyhow::Result<()> {
        use rsi_common::types::{SandboxCustodyErrorCodeV1, SandboxCustodyTransitionV1};

        for substituted in [false, true] {
            let (manager, dir) = rotation_manager();
            let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
            let parent_id = fixture.parent.id;
            let snapshot = serde_json::to_value(&fixture.parent)?;
            manager.completed.write().await.insert(
                parent_id,
                CompletedSession::for_test(fixture.parent.clone()),
            );
            let displaced = fixture.root.with_extension("displaced");
            std::fs::rename(&fixture.root, &displaced)?;
            let expected = if substituted {
                let substitute = dir.path().join("substituted-root");
                std::fs::create_dir(&substitute)?;
                std::os::unix::fs::symlink(&substitute, &fixture.root)?;
                SandboxCustodyErrorCodeV1::RootOutsideBase
            } else {
                SandboxCustodyErrorCodeV1::RootMissing
            };
            let error = match manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&fixture.parent)
                .await
            {
                Ok(_) => panic!("invalid filesystem evidence must refuse"),
                Err(error) => error,
            };
            let typed = rotation_custody_error(&error);
            assert_eq!(typed.code, expected);
            assert_eq!(typed.transition, SandboxCustodyTransitionV1::Rotation);
            let before = rotation_creation_counts(&manager).await?;
            rotate_completed_session_for_test(&manager, parent_id).await;
            assert_eq!(rotation_creation_counts(&manager).await?, before);
            let restored = manager.completed.read().await;
            assert_eq!(
                serde_json::to_value(
                    &restored
                        .get(&parent_id)
                        .ok_or_else(|| anyhow::anyhow!("quarantined parent snapshot missing"))?
                        .session
                )?,
                snapshot
            );
            drop(restored);
            let store = manager.store.lock().await;
            let root: (Option<String>, i64, i64, String, String, Option<String>) =
                store.conn.query_row(
                    "SELECT owner_session_id,generation,event_sequence,state,validation_state,
                            validation_error_code
                     FROM sandbox_custody_roots WHERE custody_id=?1",
                    [fixture.custody_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )?;
            assert_eq!(root.0, None);
            assert_eq!(root.1, 2);
            assert_eq!(root.2, 3);
            assert_eq!(root.3, "quarantined");
            assert_eq!(root.4, "invalid");
            assert_eq!(root.5.as_deref(), Some(expected.as_str()));
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_transferred_predecessor_refuses_without_backward_effects()
    -> anyhow::Result<()> {
        use rsi_common::types::{SandboxCustodyErrorCodeV1, SandboxCustodyTransitionV1};

        let (manager, dir) = rotation_manager();
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        let parent_id = fixture.parent.id;
        let snapshot = serde_json::to_value(&fixture.parent)?;
        manager.completed.write().await.insert(
            parent_id,
            CompletedSession::for_test(fixture.parent.clone()),
        );
        let live = manager
            .store
            .lock()
            .await
            .live_custody_for_session(parent_id)?;
        let successor_id = Uuid::new_v4();
        let mut successor = test_session(successor_id, SessionStatus::Starting);
        successor.working_dir = fixture.repo.clone();
        successor.continued_from = Some(parent_id);
        successor.sandbox_kind = fixture.parent.sandbox_kind;
        successor.sandbox_root = fixture.parent.sandbox_root.clone();
        successor.sandbox_branch = fixture.parent.sandbox_branch.clone();
        successor.sandbox_cleanup_state = fixture.parent.sandbox_cleanup_state;
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&successor)?;
            store.bind_reserved_session_custody(
                successor_id,
                crate::store::sandbox_custody::SessionCustodyBinding::Transfer {
                    custody_id: live.custody_id,
                    from_session_id: parent_id,
                    generation: live.generation,
                    cause: crate::store::sandbox_custody::CustodyCause::Rotation,
                    origin_session_id: Some(parent_id),
                    scheduled_job_id: None,
                },
            )?;
        }
        let error = match manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await
        {
            Ok(_) => panic!("transferred predecessor must not rotate again"),
            Err(error) => error,
        };
        let typed = rotation_custody_error(&error);
        assert_eq!(typed.code, SandboxCustodyErrorCodeV1::OwnershipMissing);
        assert_eq!(typed.transition, SandboxCustodyTransitionV1::Rotation);
        let before = rotation_creation_counts(&manager).await?;
        rotate_completed_session_for_test(&manager, parent_id).await;
        assert_eq!(rotation_creation_counts(&manager).await?, before);
        let restored = manager.completed.read().await;
        assert_eq!(
            serde_json::to_value(
                &restored
                    .get(&parent_id)
                    .ok_or_else(|| anyhow::anyhow!("transferred snapshot not restored"))?
                    .session
            )?,
            snapshot
        );
        drop(restored);
        let store = manager.store.lock().await;
        let root: (String, i64, i64) = store.conn.query_row(
            "SELECT owner_session_id,generation,event_sequence
             FROM sandbox_custody_roots WHERE custody_id=?1",
            [fixture.custody_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(root, (successor_id.to_string(), 2, 2));
        let backward: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sandbox_custody_events
             WHERE custody_id=?1 AND event_kind='transferred'
               AND from_owner_session_id=?2 AND to_owner_session_id=?3",
            rusqlite::params![
                fixture.custody_id.to_string(),
                successor_id.to_string(),
                parent_id.to_string(),
            ],
            |row| row.get(0),
        )?;
        assert_eq!(backward, 0);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_ordinary_bind_mismatch_matrix_settles_reserved_child()
    -> anyhow::Result<()> {
        for (mutation, should_restore) in [
            (
                RotationBindMutationForTest::CandidatePredecessorMismatch,
                false,
            ),
            (RotationBindMutationForTest::SuccessorLineageMismatch, true),
        ] {
            let (manager, dir) = rotation_manager();
            let parent_id = Uuid::new_v4();
            let child_id = Uuid::new_v4();
            let mut parent = test_session(parent_id, SessionStatus::Completed);
            parent.working_dir = dir.path().to_path_buf();
            manager.store.lock().await.insert_session(&parent)?;
            let candidate = manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&parent)
                .await?;
            let mut child = test_session(child_id, SessionStatus::Starting);
            child.working_dir = parent.working_dir.clone();
            child.continued_from = Some(parent_id);
            install_rotation_bind_mutation_for_test(child_id, mutation);
            install_rotation_context_read_observation_for_test(child_id);
            install_rotation_config_observation_for_test(child_id);
            install_rotation_provider_unavailable_for_test(child_id);
            spawn_rotation_child_for_test(
                &manager,
                CompletedSession::for_test(parent),
                child,
                candidate,
                None,
            )
            .await;
            assert_h1_v83_rotation_bind_refusal(&manager, child_id).await?;
            assert_eq!(
                manager.completed.read().await.contains_key(&parent_id),
                should_restore,
                "only an exactly re-authenticated ordinary predecessor is restorable"
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_7f3e_stale_ordinary_fence_settles_child_as_superseded_without_parent_revival()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        manager.store.lock().await.insert_session(&parent)?;
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent)
            .await?;
        let parent_token = format!("ordinary-stale-bind:{parent_id}");
        manager
            .register_agent_token(parent_token.clone(), parent_id)
            .await;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = parent.working_dir.clone();
        child.continued_from = Some(parent_id);
        install_rotation_pre_bind_predecessor_mutation_for_test(
            child_id,
            RotationPreBindPredecessorMutationForTest::LineageRouting,
        );
        install_rotation_context_read_observation_for_test(child_id);
        install_rotation_config_observation_for_test(child_id);
        install_rotation_provider_unavailable_for_test(child_id);

        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent),
            child,
            candidate,
            None,
        )
        .await;

        assert_h1_v83_rotation_bind_refusal(&manager, child_id).await?;
        assert!(
            !manager.completed.read().await.contains_key(&parent_id),
            "a changed durable predecessor must be Superseded, not reinserted from stale memory"
        );
        let tokens = manager.agent_tokens.read().await;
        assert_eq!(
            tokens.token_for_session(parent_id),
            Some(parent_token.as_str())
        );
        assert_eq!(
            tokens
                .values()
                .filter(|session_id| **session_id == parent_id)
                .count(),
            1,
            "bind refusal must neither revoke nor revive the still-local predecessor token"
        );
        drop(tokens);
        assert!(
            !rotation_pre_bind_predecessor_mutations()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&child_id)
        );
        let durable = manager
            .store
            .lock()
            .await
            .get_session(parent_id)?
            .ok_or_else(|| anyhow::anyhow!("changed durable predecessor missing"))?;
        assert!(durable.parent_id.is_some());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_7f3g_finalizer_barrier_reaches_post_finalize_provider_seam()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut running = test_session(parent_id, SessionStatus::Running);
        running.working_dir = dir.path().to_path_buf();
        running.session_kind = SessionKind::TaskRabbit;
        manager.store.lock().await.insert_session(&running)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent_id)?;
        let mut tracked = TrackedSession::new_for_test(running);
        tracked.events.push(ConversationEvent {
            id: 0,
            session_id: parent_id,
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: chrono::Utc::now(),
            content: "escalate this rotation [TASKRABBIT_ESCALATE]".into(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        });
        manager.active.write().await.insert(parent_id, tracked);

        let finalized = SessionManager::finalize_session(
            parent_id,
            0,
            super::super::types::TerminalFinalizeDecision::completed(),
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager.persistence.clone(),
            None,
            Arc::clone(&manager.runtime_config),
        )
        .await
        .ok_or_else(|| anyhow::anyhow!("finalizer did not return a rotation-safe decision"))?;
        assert_eq!(
            finalized,
            super::super::types::TerminalFinalizeDecision::completed()
        );

        install_rotation_child_id_for_test(parent_id, child_id);
        install_rotation_context_read_observation_for_test(child_id);
        install_rotation_config_observation_for_test(child_id);
        install_rotation_provider_unavailable_for_test(child_id);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            SessionManager::execute_post_finalization_rotation_action(
                parent_id,
                super::super::rotation_coordinator::RotationAction::SpawnChild {
                    session_id: parent_id,
                    handoff_filepath: None,
                },
                None,
                Arc::clone(&manager.active),
                Arc::clone(&manager.completed),
                Arc::clone(&manager.event_bus),
                Arc::clone(&manager.store),
                manager
                    .model_call_settlements
                    .handle()
                    .expect("settlement producer"),
                manager.persistence.clone(),
                false,
                manager.socket_path.clone(),
                Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                Arc::clone(&manager.runtime_config),
                Arc::clone(&manager.spawn_coordinator),
                Arc::clone(&manager.agent_tokens),
                Arc::clone(&manager.spawn_epoch),
                Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
            ),
        )
        .await?;
        assert_eq!(outcome, PostFinalizeRotationOutcome::Handled);

        assert_eq!(
            take_rotation_context_read_observation_for_test(child_id),
            Some(true)
        );
        let config = take_rotation_config_observation_for_test(child_id)
            .ok_or_else(|| anyhow::anyhow!("rotation did not reach deterministic provider seam"))?;
        assert_eq!(config.cwd, dir.path());
        assert!(config.cargo_target_dir.is_none());
        let durable = manager
            .store
            .lock()
            .await
            .get_session(parent_id)?
            .ok_or_else(|| anyhow::anyhow!("finalized parent missing from Store"))?;
        assert_eq!(durable.status, SessionStatus::Completed);
        assert_eq!(
            durable.session_kind,
            SessionKind::Standard,
            "the stable finalizer-owned escalation must cross the FIFO barrier"
        );
        assert!(manager.completed.read().await.contains_key(&parent_id));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_7f3f_stable_pre_bind_authority_mutations_refuse_before_effect()
    -> anyhow::Result<()> {
        for mutation in [
            RotationPreBindPredecessorMutationForTest::LineageRouting,
            RotationPreBindPredecessorMutationForTest::ExecutionPrompt,
            RotationPreBindPredecessorMutationForTest::ModelInvocation,
        ] {
            let (manager, dir) = rotation_manager();
            let parent_id = Uuid::new_v4();
            let child_id = Uuid::new_v4();
            let mut parent = test_session(parent_id, SessionStatus::Completed);
            parent.working_dir = dir.path().to_path_buf();
            manager.store.lock().await.insert_session(&parent)?;
            let candidate = manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&parent)
                .await?;
            let mut child = test_session(child_id, SessionStatus::Starting);
            child.working_dir = parent.working_dir.clone();
            child.continued_from = Some(parent_id);
            install_rotation_pre_bind_predecessor_mutation_for_test(child_id, mutation);
            install_rotation_context_read_observation_for_test(child_id);
            install_rotation_config_observation_for_test(child_id);
            install_rotation_provider_unavailable_for_test(child_id);
            spawn_rotation_child_for_test(
                &manager,
                CompletedSession::for_test(parent),
                child,
                candidate,
                None,
            )
            .await;
            assert_h1_v83_rotation_bind_refusal(&manager, child_id).await?;
            assert!(!manager.completed.read().await.contains_key(&parent_id));
        }
        Ok(())
    }

    #[tokio::test]
    async fn h1_v83_rotation_custody_7f3f_all_null_session_with_durable_custody_link_refuses()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        manager.store.lock().await.conn.execute(
            "UPDATE sessions SET sandbox_kind=NULL, sandbox_root=NULL, sandbox_branch=NULL, \
             sandbox_cleanup_state=NULL WHERE id=?1",
            [fixture.parent.id.to_string()],
        )?;
        let apparent_ordinary = manager
            .store
            .lock()
            .await
            .get_session(fixture.parent.id)?
            .ok_or_else(|| anyhow::anyhow!("malformed linked predecessor missing"))?;
        assert!(apparent_ordinary.sandbox_kind.is_none());
        let error = match manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&apparent_ordinary)
            .await
        {
            Ok(_) => panic!("SQL-only custody link must refuse apparent ordinary predecessor"),
            Err(error) => error,
        };
        assert_eq!(
            rotation_custody_error(&error).code,
            rsi_common::types::SandboxCustodyErrorCodeV1::CustodyChanged
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_live_bind_refusal_classifies_current_and_mismatched_owner()
    -> anyhow::Result<()> {
        for (mutation, should_restore) in [
            (RotationBindMutationForTest::SuccessorTupleMismatch, true),
            (
                RotationBindMutationForTest::SandboxHandleSessionMismatch,
                false,
            ),
        ] {
            let (manager, dir) = rotation_manager();
            let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
            let parent_id = fixture.parent.id;
            let child_id = Uuid::new_v4();
            let candidate = manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&fixture.parent)
                .await?;
            let mut child = test_session(child_id, SessionStatus::Starting);
            child.working_dir = fixture.repo.clone();
            child.continued_from = Some(parent_id);
            crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
                &candidate,
                &fixture.parent,
                &mut child,
            )?;
            install_rotation_bind_mutation_for_test(child_id, mutation);
            install_rotation_context_read_observation_for_test(child_id);
            install_rotation_config_observation_for_test(child_id);
            install_rotation_provider_unavailable_for_test(child_id);
            spawn_rotation_child_for_test(
                &manager,
                CompletedSession::for_test(fixture.parent),
                child,
                candidate,
                None,
            )
            .await;
            assert_h1_v83_rotation_bind_refusal(&manager, child_id).await?;
            assert_eq!(
                manager.completed.read().await.contains_key(&parent_id),
                should_restore,
                "only the exact still-current authenticated live owner is restorable"
            );
            let store = manager.store.lock().await;
            let root: (String, i64, i64) = store.conn.query_row(
                "SELECT owner_session_id,generation,event_sequence
                 FROM sandbox_custody_roots WHERE custody_id=?1",
                [fixture.custody_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            assert_eq!(root, (parent_id.to_string(), 1, 1));
            let backward: i64 = store.conn.query_row(
                "SELECT COUNT(*) FROM sandbox_custody_events
                 WHERE custody_id=?1 AND event_kind='transferred'",
                [fixture.custody_id.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(backward, 0);
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_ordinary_provider_unavailable_settles_exactly()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let intent = Uuid::new_v4().simple().to_string();
        let (project, assigned_idea, parent_session, parent_token) =
            d03_rotation_fixture(&manager, dir.path(), &intent).await?;
        let parent_id = parent_session.id;
        let parent_snapshot = serde_json::to_value(&parent_session)?;
        let rotation_id = format!("h1-v83-ordinary:{intent}");
        let transfer_intent_key = format!("rotation:{parent_id}:{rotation_id}");
        let reservation_id = idea_controller_reservation_id(assigned_idea.id, &transfer_intent_key);
        let child_id = idea_controller_candidate_session_id(reservation_id);
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent_session)
            .await?;
        let mut child = test_session(Uuid::new_v4(), SessionStatus::Starting);
        child.working_dir = parent_session.working_dir.clone();
        child.project_id = Some(project.id);
        child.provider = SessionProvider::Codex;
        child.continued_from = Some(parent_id);
        child.query = "ordinary provider unavailable".to_string();
        install_rotation_provider_unavailable_for_test(child_id);
        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent_session),
            child,
            candidate,
            Some(rotation_id),
        )
        .await;

        let restored_snapshot = {
            let completed = manager.completed.read().await;
            let restored = completed
                .get(&parent_id)
                .expect("ordinary predecessor restored exactly");
            serde_json::to_value(&restored.session)?
        };
        assert_eq!(restored_snapshot, parent_snapshot);
        assert_eq!(
            manager.resolve_agent_token(&parent_token).await,
            Some(parent_id)
        );
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|session_id| *session_id == child_id)
        );
        let store = manager.store.lock().await;
        let child = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("ordinary failed child missing"))?;
        assert_eq!(child.status, SessionStatus::Failed);
        assert_eq!(
            child.stop_reason.as_deref(),
            Some("sandbox_custody:persistence_transition_failed")
        );
        assert!(
            child.sandbox_kind.is_none()
                && child.sandbox_root.is_none()
                && child.sandbox_branch.is_none()
                && child.sandbox_cleanup_state.is_none()
        );
        let projection: (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = store.conn.query_row(
            "SELECT execution_state,freshness,canonical_repo_dir,effective_cwd,
                        custody_id,custody_generation,validated_at,error_code
                 FROM session_execution_projections WHERE session_id=?1",
            [child_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        assert_eq!(projection.0, "ordinary_unsandboxed");
        assert_eq!(projection.1, "verified");
        assert_eq!(projection.2, child.working_dir.display().to_string());
        assert_eq!(projection.3.as_deref(), Some(projection.2.as_str()));
        assert!(projection.4.is_none());
        assert!(projection.5.is_none());
        assert!(projection.6.is_some());
        assert!(projection.7.is_none());
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("ordinary child invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(invocation, ("failed".into(), Some("spawn_failed".into())));
        assert!(store.controller_grant_v1(parent_id).is_some());
        assert!(store.controller_grant_v1(child_id).is_none());
        let events = store.list_idea_events_v1(
            project.id,
            assigned_idea.id,
            IdeaEventPageRequestV1 {
                after_sequence: 0,
                limit: Some(16),
            },
        )?;
        let tail = events
            .events
            .last()
            .ok_or_else(|| anyhow::anyhow!("controller release event missing"))?
            .controller_control_payload_v1()
            .map_err(anyhow::Error::msg)?;
        assert!(matches!(
            tail.request.operation,
            IdeaControllerControlOperationV1::ReleaseReservation {
                reservation,
                reason: ControllerReleaseReasonV1::ProviderSpawnFailed,
                ..
            } if reservation.reservation_id == reservation_id
                && reservation.candidate_session_id == child_id
        ));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_live_provider_unavailable_preserves_worktree()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let project_id = Uuid::new_v4();
        let absolute_context = dir.path().join("absolute-context.txt");
        std::fs::write(&absolute_context, "absolute-context-sentinel")?;
        let project = Project {
            id: project_id,
            name: "live rotation custody".into(),
            path: Some(dir.path().join("live-rotation-repo")),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: Some(vec![
                std::path::PathBuf::from("relative-context.txt"),
                absolute_context.clone(),
            ]),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        manager.store.lock().await.insert_project(&project)?;
        let fixture = persist_live_rotation_parent(&manager, dir.path(), Some(project_id)).await;
        std::fs::write(
            fixture.repo.join("canonical-only.txt"),
            "canonical-sentinel",
        )?;
        std::fs::write(
            fixture.repo.join("relative-context.txt"),
            "canonical-relative-context",
        )?;
        std::fs::write(fixture.root.join(".seed"), "dirty-tracked-sentinel")?;
        std::fs::write(fixture.root.join("untracked.txt"), "untracked-sentinel")?;
        std::fs::write(
            fixture.root.join("relative-context.txt"),
            "live-relative-context",
        )?;
        let watched = [
            fixture.repo.join("canonical-only.txt"),
            fixture.repo.join("relative-context.txt"),
            fixture.root.join(".seed"),
            fixture.root.join("untracked.txt"),
            fixture.root.join("relative-context.txt"),
            absolute_context.clone(),
        ];
        let before = watched
            .iter()
            .map(std::fs::read)
            .collect::<std::io::Result<Vec<_>>>()?;
        let parent_id = fixture.parent.id;
        let child_id = Uuid::new_v4();
        manager
            .register_agent_token("live-rotation-parent-token".into(), parent_id)
            .await;
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.provider = SessionProvider::Claude;
        child.working_dir = fixture.repo.clone();
        child.project_id = Some(project_id);
        child.continued_from = Some(parent_id);
        child.query = "live provider unavailable".into();
        crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
            &candidate,
            &fixture.parent,
            &mut child,
        )?;
        install_rotation_config_observation_for_test(child_id);
        install_rotation_provider_unavailable_for_test(child_id);
        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(fixture.parent.clone()),
            child,
            candidate,
            None,
        )
        .await;

        let observation = take_rotation_config_observation_for_test(child_id)
            .ok_or_else(|| anyhow::anyhow!("live rotation config was not observed"))?;
        assert_eq!(observation.cwd, fixture.root);
        assert_eq!(
            observation.cargo_target_dir,
            Some(fixture.root.join("target"))
        );
        let prompt = observation
            .system_prompt
            .ok_or_else(|| anyhow::anyhow!("live rotation context missing"))?;
        assert!(prompt.contains("live-relative-context"));
        assert!(prompt.contains("absolute-context-sentinel"));
        assert!(!prompt.contains("canonical-relative-context"));
        let after = watched
            .iter()
            .map(std::fs::read)
            .collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(
            after, before,
            "provider failure must not touch any sentinel"
        );
        assert!(
            fixture.root.join("target/.rsi-tmp").is_dir(),
            "the authenticated scratch layout remains available after provider failure"
        );

        assert!(!manager.completed.read().await.contains_key(&parent_id));
        let tokens = manager.agent_tokens.read().await;
        assert!(!tokens.values().any(|session_id| *session_id == parent_id));
        assert!(!tokens.values().any(|session_id| *session_id == child_id));
        drop(tokens);
        let store = manager.store.lock().await;
        let failed = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("live failed child missing"))?;
        assert_eq!(failed.status, SessionStatus::Failed);
        assert_eq!(
            failed.stop_reason.as_deref(),
            Some("sandbox_custody:persistence_transition_failed")
        );
        let root: (String, i64, i64, String, String) = store.conn.query_row(
            "SELECT owner_session_id,generation,event_sequence,state,validation_state
             FROM sandbox_custody_roots WHERE custody_id=?1",
            [fixture.custody_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        assert_eq!(
            root,
            (child_id.to_string(), 2, 2, "live".into(), "verified".into())
        );
        let child_projection: (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = store.conn.query_row(
            "SELECT execution_state,freshness,effective_cwd,custody_id,
                    custody_generation,error_code
             FROM session_execution_projections WHERE session_id=?1",
            [child_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        assert_eq!(child_projection.0, "live_sandboxed");
        assert_eq!(child_projection.1, "verified");
        assert_eq!(
            child_projection.2.as_deref(),
            Some(fixture.root.to_string_lossy().as_ref())
        );
        assert_eq!(
            child_projection.3.as_deref(),
            Some(fixture.custody_id.to_string().as_str())
        );
        assert_eq!(child_projection.4, Some(2));
        assert!(child_projection.5.is_none());
        let predecessor_projection: (String, String, Option<String>) = store.conn.query_row(
            "SELECT execution_state,freshness,effective_cwd
             FROM session_execution_projections WHERE session_id=?1",
            [parent_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            predecessor_projection,
            ("historical_transferred".into(), "verified".into(), None)
        );
        let backward: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sandbox_custody_events
             WHERE custody_id=?1 AND event_kind='transferred'
               AND from_owner_session_id=?2 AND to_owner_session_id=?3",
            rusqlite::params![
                fixture.custody_id.to_string(),
                child_id.to_string(),
                parent_id.to_string(),
            ],
            |row| row.get(0),
        )?;
        assert_eq!(backward, 0);
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("live child invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(invocation, ("failed".into(), Some("spawn_failed".into())));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_competing_transfer_loser_is_superseded() -> anyhow::Result<()>
    {
        let (manager, dir) = rotation_manager();
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        let parent_id = fixture.parent.id;
        let attempted_id = Uuid::new_v4();
        let winner_id = Uuid::new_v4();
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await?;
        let mut attempted = test_session(attempted_id, SessionStatus::Starting);
        attempted.working_dir = fixture.repo.clone();
        attempted.continued_from = Some(parent_id);
        attempted.query = "competing transfer loser".into();
        crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
            &candidate,
            &fixture.parent,
            &mut attempted,
        )?;
        let mut winner = test_session(winner_id, SessionStatus::Starting);
        winner.working_dir = fixture.repo.clone();
        winner.continued_from = Some(parent_id);
        winner.query = "legitimate competing transfer winner".into();
        winner.sandbox_kind = Some(SandboxKind::GitWorktree);
        winner.sandbox_root = Some(fixture.root.clone());
        winner.sandbox_branch = Some(fixture.branch.clone());
        winner.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        install_rotation_competing_winner_for_test(attempted_id, winner);
        install_rotation_context_read_observation_for_test(attempted_id);
        install_rotation_config_observation_for_test(attempted_id);
        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(fixture.parent.clone()),
            attempted,
            candidate,
            None,
        )
        .await;

        assert_eq!(
            take_rotation_context_read_observation_for_test(attempted_id),
            Some(false),
            "loser must not reach ContextRead"
        );
        assert!(take_rotation_config_observation_for_test(attempted_id).is_none());
        assert!(
            !rotation_competing_winners()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&attempted_id)
        );
        assert!(!manager.completed.read().await.contains_key(&parent_id));
        let store = manager.store.lock().await;
        let loser = store
            .get_session(attempted_id)?
            .ok_or_else(|| anyhow::anyhow!("competing loser missing"))?;
        assert_eq!(loser.status, SessionStatus::Failed);
        assert_eq!(
            loser.stop_reason.as_deref(),
            Some("sandbox_custody:custody_changed")
        );
        let loser_projection: (String, String, Option<String>, Option<String>, Option<i64>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,effective_cwd,custody_id,custody_generation
             FROM session_execution_projections WHERE session_id=?1",
                [attempted_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        assert_eq!(
            loser_projection,
            ("invalid".into(), "invalid".into(), None, None, None)
        );
        let root: (String, i64, i64) = store.conn.query_row(
            "SELECT owner_session_id,generation,event_sequence
             FROM sandbox_custody_roots WHERE custody_id=?1",
            [fixture.custody_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(root, (winner_id.to_string(), 2, 2));
        let winner_projection: (String, String, Option<String>, Option<i64>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,effective_cwd,custody_generation
             FROM session_execution_projections WHERE session_id=?1",
                [winner_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(winner_projection.0, "live_sandboxed");
        assert_eq!(winner_projection.1, "verified");
        assert_eq!(
            winner_projection.2.as_deref(),
            Some(fixture.root.to_string_lossy().as_ref())
        );
        assert_eq!(winner_projection.3, Some(2));
        let invocation_id = store
            .session_model_invocation_id(attempted_id)?
            .ok_or_else(|| anyhow::anyhow!("loser invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            ("failed".into(), Some("custody_bind_failed".into()))
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_post_bind_refusal_preserves_invalid_projection()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        let parent_id = fixture.parent.id;
        let child_id = Uuid::new_v4();
        manager
            .register_agent_token("invalidated-parent-token".into(), parent_id)
            .await;
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = fixture.repo.clone();
        child.continued_from = Some(parent_id);
        child.query = "post-bind context refusal".into();
        crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
            &candidate,
            &fixture.parent,
            &mut child,
        )?;
        install_rotation_context_root_mutation_for_test(child_id);
        install_rotation_config_observation_for_test(child_id);
        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(fixture.parent.clone()),
            child,
            candidate,
            None,
        )
        .await;

        assert!(take_rotation_config_observation_for_test(child_id).is_none());
        assert!(!manager.completed.read().await.contains_key(&parent_id));
        let tokens = manager.agent_tokens.read().await;
        assert!(!tokens.values().any(|session_id| *session_id == parent_id));
        assert!(!tokens.values().any(|session_id| *session_id == child_id));
        drop(tokens);
        let store = manager.store.lock().await;
        let child = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("invalidated child missing"))?;
        assert_eq!(child.status, SessionStatus::Failed);
        assert_eq!(
            child.sandbox_cleanup_state,
            Some(SandboxCleanupState::Failed)
        );
        assert_eq!(
            child.stop_reason.as_deref(),
            Some("sandbox_custody:root_missing")
        );
        let projection: (String, String, Option<String>, Option<i64>, Option<String>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,effective_cwd,custody_generation,error_code
                 FROM session_execution_projections WHERE session_id=?1",
                [child_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        assert_eq!(
            projection,
            (
                "live_sandboxed".into(),
                "invalid".into(),
                None,
                Some(2),
                Some("root_missing".into())
            )
        );
        let root: (Option<String>, i64, i64, String, String, Option<String>) =
            store.conn.query_row(
                "SELECT owner_session_id,generation,event_sequence,state,validation_state,
                        validation_error_code
                 FROM sandbox_custody_roots WHERE custody_id=?1",
                [fixture.custody_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?;
        assert_eq!(
            root,
            (
                None,
                3,
                4,
                "quarantined".into(),
                "invalid".into(),
                Some("root_missing".into())
            )
        );
        assert!(
            fixture
                .root
                .with_extension("rotation-context-raced")
                .exists()
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rotation_execution_scratch_failure_preserves_forward_transfer_and_settles_invocation()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        let parent_id = fixture.parent.id;
        let child_id = Uuid::new_v4();
        manager
            .register_agent_token("descriptor-parent-token".into(), parent_id)
            .await;
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = fixture.repo.clone();
        child.continued_from = Some(parent_id);
        child.query = "rotation descriptor failure".into();
        crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
            &candidate,
            &fixture.parent,
            &mut child,
        )?;
        install_rotation_execution_scratch_failure_for_test(child_id);
        install_rotation_config_observation_for_test(child_id);
        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(fixture.parent.clone()),
            child,
            candidate,
            None,
        )
        .await;

        assert!(take_rotation_config_observation_for_test(child_id).is_none());
        assert!(
            !manager.completed.read().await.contains_key(&parent_id),
            "a transferred predecessor is never revived after descriptor failure"
        );
        let tokens = manager.agent_tokens.read().await;
        assert!(!tokens.values().any(|session_id| *session_id == parent_id));
        assert!(!tokens.values().any(|session_id| *session_id == child_id));
        drop(tokens);

        let store = manager.store.lock().await;
        let child = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("descriptor-failed child missing"))?;
        assert_eq!(child.status, SessionStatus::Failed);
        assert_eq!(child.sandbox_cleanup_state, Some(SandboxCleanupState::Live));
        assert_eq!(
            child.stop_reason.as_deref(),
            Some("sandbox_custody:persistence_transition_failed")
        );
        let projection: (String, String, Option<String>, Option<i64>, Option<String>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,effective_cwd,custody_generation,error_code
                 FROM session_execution_projections WHERE session_id=?1",
                [child_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        assert_eq!(projection.0, "live_sandboxed");
        assert_eq!(projection.1, "verified");
        assert_eq!(
            projection.2.as_deref(),
            Some(fixture.root.to_string_lossy().as_ref())
        );
        assert_eq!(projection.3, Some(2));
        assert_eq!(projection.4, None);
        let root: (String, String, i64, String) = store.conn.query_row(
            "SELECT state,owner_session_id,generation,validation_state
             FROM sandbox_custody_roots WHERE custody_id=?1",
            [fixture.custody_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(
            root,
            ("live".into(), child_id.to_string(), 2, "verified".into())
        );
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("descriptor-failed invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            (
                "failed".into(),
                Some("execution_scratch_unavailable".into())
            )
        );
        assert!(
            std::fs::symlink_metadata(fixture.root.join("target"))?
                .file_type()
                .is_symlink()
        );
        assert!(!fixture.root.join(".rsi-tmp").exists());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn manual_completed_rotation_missing_handoff_resumes_task_once() -> anyhow::Result<()> {
        let (manager, directory) = rotation_manager_with_context_rotation(true);
        let parent_id = Uuid::new_v4();
        let bridge_id = Uuid::new_v4();
        let forbidden_grandchild_id = Uuid::new_v4();
        let _catalog_observation =
            crate::provider_capabilities::pin_codex_catalog_observation_for_test(
                bridge_id,
                &manager.runtime_config,
                crate::provider_capabilities::VALIDATED_CODEX_CLI_VERSION,
                include_bytes!("../../tests/fixtures/codex-models-0.155.1.json"),
            )?;
        let configured_budget = rsi_common::ResolvedContextBudget::new(
            380_000,
            rsi_common::ContextCapacity {
                effective_percent: Some(95),
                configured_tokens: Some(400_000),
                runtime_effective_tokens: Some(380_000),
                ..rsi_common::ContextCapacity::default()
            },
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::RuntimeTelemetry,
                source_version: None,
                source_digest: None,
                observed_at: Some(chrono::Utc::now()),
                confidence: rsi_common::CapabilityConfidence::Authoritative,
            },
        )
        .map_err(anyhow::Error::msg)?;

        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.provider = SessionProvider::Codex;
        parent.model = Some("gpt-6-astra".to_string());
        parent.working_dir = directory.path().to_path_buf();
        parent.context_window = Some(configured_budget.active_tokens);
        parent.resolved_context_budget = Some(configured_budget);
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&parent)?;
            store.publish_startup_ordinary(parent_id)?;
        }
        manager
            .completed
            .write()
            .await
            .insert(parent_id, CompletedSession::for_test(parent));

        install_rotation_child_id_for_test(parent_id, bridge_id);
        install_rotation_child_id_for_test(bridge_id, forbidden_grandchild_id);
        install_rotation_provider_unavailable_for_test(forbidden_grandchild_id);
        let scripted = super::super::launch::install_controller_candidate_test_process(bridge_id);

        manager.trigger_rotation(parent_id).await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if manager.active.read().await.contains_key(&bridge_id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            super::super::launch::drop_controller_candidate_test_stream(bridge_id);
            loop {
                let bridge_is_retained = manager.completed.read().await.contains_key(&bridge_id);
                if bridge_is_retained && !manager.active.read().await.contains_key(&bridge_id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("manual completed rotation did not settle"))?;

        assert_eq!(
            take_rotation_child_id_for_test(bridge_id),
            Some(forbidden_grandchild_id),
            "the completed bridge must not request another rotation child"
        );
        assert!(
            take_rotation_provider_unavailable_for_test(forbidden_grandchild_id),
            "the completed bridge must stop before a second provider launch"
        );
        assert_eq!(
            scripted
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(rotation_creation_counts(&manager).await?, (2, 2, 1));

        let (durable_parent, durable_bridge) = {
            let store = manager.store.lock().await;
            (
                store
                    .get_session(parent_id)?
                    .ok_or_else(|| anyhow::anyhow!("rotation predecessor missing"))?,
                store
                    .get_session(bridge_id)?
                    .ok_or_else(|| anyhow::anyhow!("handoff retry bridge missing"))?,
            )
        };
        assert_eq!(durable_parent.status, SessionStatus::Archived);
        assert_eq!(durable_bridge.status, SessionStatus::Failed);
        assert_eq!(durable_bridge.continued_from, Some(parent_id));
        assert_eq!(durable_bridge.rotation_depth, 1);
        assert!(
            durable_bridge
                .query
                .starts_with("test\n\nRotation continuation of")
        );
        let bridge_budget = durable_bridge
            .resolved_context_budget
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("handoff retry bridge budget missing"))?;
        assert_eq!(
            durable_bridge.context_window,
            Some(bridge_budget.active_tokens)
        );
        assert_eq!(bridge_budget.capacity.configured_tokens, Some(400_000));
        assert_eq!(
            bridge_budget.evidence.source,
            rsi_common::CapabilitySource::Configured
        );
        assert_eq!(
            bridge_budget.evidence.confidence,
            rsi_common::CapabilityConfidence::Authoritative
        );
        assert!(bridge_budget.active_tokens <= 400_000);
        assert_eq!(bridge_budget.capacity.runtime_effective_tokens, None);

        let completed = manager.completed.read().await;
        let retained_bridge = completed
            .get(&bridge_id)
            .ok_or_else(|| anyhow::anyhow!("completed handoff retry bridge missing"))?;
        assert_eq!(retained_bridge.session.status, SessionStatus::Failed);
        let retained_budget = retained_bridge
            .session
            .resolved_context_budget
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("completed bridge budget missing"))?;
        assert_eq!(retained_budget.capacity.effective_percent, Some(95));
        assert_eq!(retained_budget.active_tokens, 380_000);
        assert_eq!(
            retained_budget.evidence.source_version.as_deref(),
            Some(crate::provider_capabilities::VALIDATED_CODEX_CLI_VERSION)
        );
        assert_eq!(
            retained_budget.evidence.source_digest.as_deref(),
            Some(crate::provider_capabilities::VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST)
        );
        assert!(
            retained_bridge
                .session
                .query
                .starts_with("test\n\nRotation continuation of")
        );
        drop(completed);
        Ok(())
    }

    #[tokio::test]
    async fn rotation_child_inherits_parent_rotation_disabled_at() {
        let (manager, directory) = rotation_manager();
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let disabled_at = chrono::Utc::now();
        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.working_dir = directory.path().to_path_buf();
        parent.rotation_disabled_at = Some(disabled_at);
        {
            let mut store = manager.store.lock().await;
            store
                .insert_session(&parent)
                .expect("persist rotation predecessor");
            store
                .publish_startup_ordinary(parent_id)
                .expect("publish ordinary predecessor projection");
        }
        manager
            .completed
            .write()
            .await
            .insert(parent_id, CompletedSession::for_test(parent));
        install_rotation_child_id_for_test(parent_id, child_id);
        install_rotation_provider_unavailable_for_test(child_id);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            SessionManager::rotate_completed_session(
                parent_id,
                Some("thoughts/rotation-disabled-inheritance.md".to_string()),
                None,
                Arc::clone(&manager.active),
                Arc::clone(&manager.completed),
                Arc::clone(&manager.event_bus),
                Arc::clone(&manager.store),
                manager
                    .model_call_settlements
                    .handle()
                    .expect("settlement producer"),
                manager.persistence.clone(),
                true,
                manager.socket_path.clone(),
                Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                Arc::clone(&manager.runtime_config),
                Arc::clone(&manager.spawn_coordinator),
                Arc::clone(&manager.agent_tokens),
                Arc::clone(&manager.spawn_epoch),
                Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
            ),
        )
        .await
        .expect("rotation attempt returns");

        let child = manager
            .store
            .lock()
            .await
            .get_session(child_id)
            .expect("load rotation successor")
            .expect("rotation successor row exists");
        assert_eq!(child.rotation_disabled_at, Some(disabled_at));
        let budget = child
            .resolved_context_budget
            .as_ref()
            .expect("rotation successor persists a typed context budget");
        assert_eq!(child.context_window, Some(budget.active_tokens));
        assert_eq!(budget.active_tokens, 128_000);
        assert_eq!(
            budget.evidence.source,
            rsi_common::CapabilitySource::LegacyUnverified
        );
    }

    #[tokio::test]
    async fn harness_manager_rotation_records_only_the_successfully_established_successor()
    -> anyhow::Result<()> {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        let (manager, directory) = rotation_manager_with_context_rotation(true);
        let project = Project {
            id: Uuid::new_v4(),
            name: "Manager rotation".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
        parent.project_id = Some(project.id);
        parent.working_dir = directory.path().to_path_buf();
        parent.provider = SessionProvider::Codex;
        parent.model = Some("gpt-6-astra".into());
        {
            let mut store = manager.store.lock().await;
            store.insert_project(&project)?;
            store.insert_session(&parent)?;
            store.publish_startup_ordinary(parent.id)?;
            store.configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project.id,
                session_id: parent.id,
                epic_ids: Some(Vec::new()),
                expected_row_version: 0,
            })?;
        }
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(parent.clone()));
        let failed = Uuid::new_v4();
        install_rotation_child_id_for_test(parent.id, failed);
        install_rotation_provider_unavailable_for_test(failed);
        manager.trigger_rotation(parent.id).await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let settled = manager
                    .store
                    .lock()
                    .await
                    .get_session(failed)
                    .unwrap()
                    .is_some_and(|row| row.status == SessionStatus::Failed);
                if settled && manager.completed.read().await.contains_key(&parent.id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_harness_manager(project.id)?
                .unwrap()
                .current_session_id,
            Some(parent.id)
        );

        let successful = Uuid::new_v4();
        install_rotation_child_id_for_test(parent.id, successful);
        super::super::launch::install_controller_candidate_test_process(successful);
        manager.trigger_rotation(parent.id).await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let current = manager
                    .store
                    .lock()
                    .await
                    .get_harness_manager(project.id)
                    .unwrap()
                    .unwrap()
                    .current_session_id;
                if current == Some(successful) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        {
            let store = manager.store.lock().await;
            assert_eq!(store.manager_lineage_tip(parent.id)?, successful);
            assert!(store.manager_progress(failed).is_err());
            assert!(store.manager_progress(successful).is_ok());
            let receipts: i64 = store.conn.query_row(
                "SELECT count(*) FROM harness_manager_rotation_edges
                WHERE predecessor_session_id=?1 AND retired_at IS NULL",
                [parent.id.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(receipts, 1);
        }
        super::super::launch::drop_controller_candidate_test_stream(successful);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager.completed.read().await.contains_key(&successful) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let reopened = crate::store::Store::open(&directory.path().join("rsi.db"))?;
        assert_eq!(
            reopened
                .get_harness_manager(project.id)?
                .unwrap()
                .current_session_id,
            Some(successful)
        );
        // The retained failed row does not get rewritten or selected as owner.
        assert_eq!(
            reopened.get_session(failed)?.unwrap().status,
            SessionStatus::Failed
        );
        Ok(())
    }

    struct RotationFinalizationProbe {
        reached: tokio::sync::oneshot::Sender<bool>,
        replays: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<bool>>>,
    }

    fn rotation_finalization_probes()
    -> &'static std::sync::Mutex<HashMap<Uuid, RotationFinalizationProbe>> {
        static PROBES: std::sync::OnceLock<
            std::sync::Mutex<HashMap<Uuid, RotationFinalizationProbe>>,
        > = std::sync::OnceLock::new();
        PROBES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
    }

    fn install_rotation_finalization_probe(
        child_id: Uuid,
    ) -> (
        tokio::sync::oneshot::Receiver<bool>,
        mpsc::Sender<tokio::sync::oneshot::Sender<Result<bool>>>,
    ) {
        let (reached, observed) = tokio::sync::oneshot::channel();
        let (requests, replays) = mpsc::channel(1);
        rotation_finalization_probes()
            .lock()
            .unwrap()
            .insert(child_id, RotationFinalizationProbe { reached, replays });
        (observed, requests)
    }

    // Retain the real callback's opaque bound custody while the test restores
    // or rearchives its predecessor. Every replay uses the production finalizer.
    pub(super) async fn observe_rotation_finalization_for_test(
        child_id: Uuid,
        archived: bool,
        runtime: &crate::sandbox::custody::CustodyExecutionRuntime,
        bound: &crate::sandbox::custody::BoundRotationCustody,
    ) {
        let probe = rotation_finalization_probes()
            .lock()
            .unwrap()
            .remove(&child_id);
        if let Some(mut probe) = probe {
            let _ = probe.reached.send(archived);
            while let Some(reply) = probe.replays.recv().await {
                let result = runtime.finalize_rotation_predecessor(child_id, bound).await;
                let _ = reply.send(result);
            }
        }
    }

    async fn replay_rotation_finalization(
        requests: &mpsc::Sender<tokio::sync::oneshot::Sender<Result<bool>>>,
    ) -> anyhow::Result<Result<bool>> {
        let (reply, result) = tokio::sync::oneshot::channel();
        requests.send(reply).await?;
        Ok(tokio::time::timeout(std::time::Duration::from_secs(10), result).await??)
    }

    async fn manager_rotation_finalization_fixture(
        transferred: bool,
    ) -> anyhow::Result<(SessionManager, tempfile::TempDir, Session)> {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        let (manager, directory) = rotation_manager_with_context_rotation(true);
        let project = Project {
            id: Uuid::new_v4(),
            name: "Atomic manager rotation".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        manager.store.lock().await.insert_project(&project)?;
        let parent = if transferred {
            persist_live_rotation_parent(&manager, directory.path(), Some(project.id))
                .await
                .parent
        } else {
            let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
            parent.working_dir = directory.path().to_path_buf();
            parent.project_id = Some(project.id);
            let mut store = manager.store.lock().await;
            store.insert_session(&parent)?;
            store.publish_startup_ordinary(parent.id)?;
            parent
        };
        {
            // A real completed provider session already has an invocation.
            // Seed it before capture so Store::open's legacy repair cannot
            // change this fixture's SQL-only authority fence on reopen.
            let store = manager.store.lock().await;
            let invocation_id = Uuid::new_v4();
            insert_rotation_invocation_fixture(&store, parent.id, invocation_id, "completed");
            store.set_session_model_invocation(parent.id, Some(invocation_id))?;
        }
        manager.store.lock().await.configure_harness_manager(
            &ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project.id,
                session_id: parent.id,
                epic_ids: Some(Vec::new()),
                expected_row_version: 0,
            },
        )?;
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(parent.clone()));
        Ok((manager, directory, parent))
    }

    async fn finish_manager_rotation_test(
        manager: &SessionManager,
        child_id: Uuid,
    ) -> anyhow::Result<()> {
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager.completed.read().await.contains_key(&child_id) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }

    fn assert_manager_rotation_custody(
        store: &Store,
        parent: &Session,
        child_id: Uuid,
        transferred: bool,
    ) -> anyhow::Result<()> {
        let states: (String, String) = store.conn.query_row(
            "SELECT previous.execution_state, successor.execution_state
             FROM session_execution_projections previous, session_execution_projections successor
             WHERE previous.session_id=?1 AND successor.session_id=?2",
            rusqlite::params![parent.id.to_string(), child_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if transferred {
            assert_eq!(
                states,
                ("historical_transferred".into(), "live_sandboxed".into())
            );
            let root = store.live_custody_for_session(child_id)?;
            assert_eq!(root.owner_session_id, child_id);
            assert_eq!(root.generation, 2);
            assert_eq!(
                std::path::Path::new(&root.sandbox_root),
                parent.sandbox_root.as_ref().unwrap()
            );
            assert!(std::path::Path::new(&root.sandbox_root).is_dir());
        } else {
            assert_eq!(
                states,
                ("ordinary_unsandboxed".into(), "ordinary_unsandboxed".into())
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn harness_manager_rotation_receipt_failure_rolls_back_archival_for_both_custodies()
    -> anyhow::Result<()> {
        for transferred in [false, true] {
            let (manager, directory, parent) =
                manager_rotation_finalization_fixture(transferred).await?;
            let child_id = Uuid::new_v4();
            install_rotation_child_id_for_test(parent.id, child_id);
            super::super::launch::install_controller_candidate_test_process(child_id);
            let _stream = ControllerCandidateTestStreamGuard(child_id);
            let (observed, replays) = install_rotation_finalization_probe(child_id);
            manager.store.lock().await.conn.execute_batch(
                "CREATE TEMP TRIGGER abort_manager_rotation_receipt
                 BEFORE INSERT ON harness_manager_rotation_edges
                 BEGIN SELECT RAISE(ABORT,'injected manager rotation receipt failure'); END;",
            )?;
            let mut events = manager.event_bus.subscribe();
            manager.trigger_rotation(parent.id).await?;
            assert!(!tokio::time::timeout(std::time::Duration::from_secs(10), observed).await??);
            {
                let store = manager.store.lock().await;
                assert_eq!(
                    store.get_session(parent.id)?.unwrap().status,
                    SessionStatus::Completed
                );
                assert_eq!(store.manager_lineage_tip(parent.id)?, parent.id);
                assert_eq!(
                    store
                        .get_harness_manager(parent.project_id.unwrap())?
                        .unwrap()
                        .current_session_id,
                    Some(parent.id)
                );
                let count: i64 = store.conn.query_row(
                    "SELECT count(*) FROM harness_manager_rotation_edges",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(count, 0);
                assert_manager_rotation_custody(&store, &parent, child_id, transferred)?;
            }
            let reopened = Store::open(&directory.path().join("rsi.db"))?;
            assert_eq!(
                reopened.get_session(parent.id)?.unwrap().status,
                SessionStatus::Completed
            );
            assert_eq!(reopened.manager_lineage_tip(parent.id)?, parent.id);
            drop(reopened);
            let mut visible_failure = false;
            while let Ok(event) = events.try_recv() {
                match event.as_ref() {
                    DaemonEvent::SessionArchived { session_id, .. } => {
                        assert_ne!(*session_id, parent.id)
                    }
                    DaemonEvent::SystemMessage { level, message } if level == "error" => {
                        visible_failure |= message
                            .contains("archival and authority receipt could not be committed");
                    }
                    _ => {}
                }
            }
            assert!(
                visible_failure,
                "the aborted finalization must report its persistence failure"
            );
            manager
                .store
                .lock()
                .await
                .conn
                .execute_batch("DROP TRIGGER abort_manager_rotation_receipt")?;
            assert!(replay_rotation_finalization(&replays).await??);
            drop(replays);
            finish_manager_rotation_test(&manager, child_id).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn harness_manager_rotation_atomic_receipt_survives_reopen_and_rejects_retired_replays()
    -> anyhow::Result<()> {
        for transferred in [false, true] {
            let (manager, directory, parent) =
                manager_rotation_finalization_fixture(transferred).await?;
            let project_id = parent.project_id.unwrap();
            let child_id = Uuid::new_v4();
            install_rotation_child_id_for_test(parent.id, child_id);
            super::super::launch::install_controller_candidate_test_process(child_id);
            let _stream = ControllerCandidateTestStreamGuard(child_id);
            let (observed, replays) = install_rotation_finalization_probe(child_id);
            manager.trigger_rotation(parent.id).await?;
            assert!(tokio::time::timeout(std::time::Duration::from_secs(10), observed).await??);
            let receipt = |store: &Store| -> anyhow::Result<(String, Option<String>)> {
                Ok(store.conn.query_row(
                    "SELECT committed_at,retired_at FROM harness_manager_rotation_edges
                     WHERE predecessor_session_id=?1 AND successor_session_id=?2",
                    rusqlite::params![parent.id.to_string(), child_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            };
            let committed = {
                let reopened = Store::open(&directory.path().join("rsi.db"))?;
                assert_eq!(
                    reopened.get_session(parent.id)?.unwrap().status,
                    SessionStatus::Archived
                );
                assert_eq!(
                    reopened
                        .get_harness_manager(project_id)?
                        .unwrap()
                        .current_session_id,
                    Some(child_id)
                );
                assert_manager_rotation_custody(&reopened, &parent, child_id, transferred)?;
                let committed = receipt(&reopened)?;
                assert_eq!(committed.1, None);
                committed
            };
            assert!(replay_rotation_finalization(&replays).await??);
            {
                let store = manager.store.lock().await;
                assert_eq!(
                    receipt(&store)?,
                    committed,
                    "active replay preserves the committed receipt"
                );
                store.update_session_status(parent.id, SessionStatus::Completed)?;
                assert_eq!(
                    store
                        .get_harness_manager(project_id)?
                        .unwrap()
                        .current_session_id,
                    Some(parent.id)
                );
                assert!(receipt(&store)?.1.is_some());
            }
            // Even a replay while restored cannot archive and reactivate the
            // retired receipt: insertion refusal rolls that archival back.
            assert!(replay_rotation_finalization(&replays).await?.is_err());
            let retired = {
                let store = manager.store.lock().await;
                assert_eq!(
                    store.get_session(parent.id)?.unwrap().status,
                    SessionStatus::Completed
                );
                store.update_session_status(parent.id, SessionStatus::Archived)?;
                receipt(&store)?
            };
            assert!(!replay_rotation_finalization(&replays).await??);
            {
                let reopened = Store::open(&directory.path().join("rsi.db"))?;
                assert_eq!(receipt(&reopened)?, retired);
                assert_eq!(
                    reopened.get_session(parent.id)?.unwrap().status,
                    SessionStatus::Archived
                );
                assert_eq!(reopened.manager_lineage_tip(parent.id)?, parent.id);
                assert_eq!(
                    reopened
                        .get_harness_manager(project_id)?
                        .unwrap()
                        .current_session_id,
                    None
                );
                assert!(reopened.manager_progress(child_id).is_err());
                assert_manager_rotation_custody(&reopened, &parent, child_id, transferred)?;
            }
            drop(replays);
            finish_manager_rotation_test(&manager, child_id).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn harness_manager_rotation_provider_failure_has_no_receipt_for_both_custodies()
    -> anyhow::Result<()> {
        for transferred in [false, true] {
            let (manager, _directory, parent) =
                manager_rotation_finalization_fixture(transferred).await?;
            let child_id = Uuid::new_v4();
            install_rotation_child_id_for_test(parent.id, child_id);
            install_rotation_provider_unavailable_for_test(child_id);
            Box::pin(rotate_completed_session_for_test(&manager, parent.id)).await;
            let store = manager.store.lock().await;
            assert_eq!(
                store.get_session(child_id)?.unwrap().status,
                SessionStatus::Failed
            );
            assert_eq!(
                store.get_session(parent.id)?.unwrap().status,
                SessionStatus::Completed
            );
            assert_eq!(store.manager_lineage_tip(parent.id)?, parent.id);
            let count: i64 = store.conn.query_row(
                "SELECT count(*) FROM harness_manager_rotation_edges",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(count, 0);
            assert_manager_rotation_custody(&store, &parent, child_id, transferred)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn rotation_child_gets_registered_agent_token() {
        let (manager, _dir) = rotation_manager();

        // ── Failure leg (saga rollback): CLI provider + missing working_dir
        // → spawn fails. The prospective child token is revoked while the
        // parent's token remains live, so rollback preserves only the former
        // controller's authority.
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let provider_cwd = tempfile::tempdir().expect("provider cwd fixture");
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.provider = SessionProvider::Codex;
        child.working_dir = provider_cwd.path().to_path_buf();
        child.continued_from = Some(parent_id);
        let mut parent_session = test_session(parent_id, SessionStatus::Completed);
        parent_session.working_dir = provider_cwd.path().to_path_buf();
        let parent = CompletedSession {
            session: parent_session,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        };
        manager
            .store
            .lock()
            .await
            .insert_session(&parent.session)
            .expect("persist ordinary rotation predecessor");
        manager
            .register_agent_token("parent-token".to_string(), parent_id)
            .await;
        let rotation_candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent.session)
            .await
            .expect("authenticate ordinary rotation parent");
        install_rotation_provider_unavailable_for_test(child_id);

        SessionManager::spawn_rotation_child(
            parent_id,
            None,
            "/resume_handoff thoughts/a6-g3-fail.md".to_string(),
            child,
            rotation_candidate,
            Some(parent),
            None,
            None,
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager
                .model_call_settlements
                .handle()
                .expect("settlement producer"),
            manager.persistence.clone(),
            false,
            manager.socket_path.clone(),
            Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            Arc::clone(&manager.runtime_config),
            Arc::clone(&manager.spawn_coordinator),
            Arc::clone(&manager.agent_tokens),
            Arc::clone(&manager.spawn_epoch),
            Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
            None,
            None,
        )
        .await;

        assert_eq!(
            manager.resolve_agent_token("parent-token").await,
            Some(parent_id),
            "failure/rollback path must NOT revoke the parent's token"
        );
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|s| *s == child_id),
            "failed rotation must revoke the prospective child token"
        );
        assert!(
            manager.completed.read().await.contains_key(&parent_id),
            "parent must be rolled back into the completed map"
        );
        let store = manager.store.lock().await;
        let failed_child = store
            .get_session(child_id)
            .expect("load failed ordinary rotation child")
            .expect("durable failed ordinary rotation child");
        assert_eq!(failed_child.status, SessionStatus::Failed);
        assert!(
            failed_child.sandbox_kind.is_none()
                && failed_child.sandbox_root.is_none()
                && failed_child.sandbox_branch.is_none()
                && failed_child.sandbox_cleanup_state.is_none(),
            "ordinary rotation failure retains an all-null durable child tuple"
        );
    }

    /// Every durable `continued_from` child of `predecessor_id`, ordered. The
    /// one-continuation invariant is a statement about this list's length, so
    /// the tests assert the exact surviving list rather than that some other
    /// row is absent.
    async fn continued_from_successors(
        manager: &SessionManager,
        predecessor_id: Uuid,
    ) -> anyhow::Result<Vec<Uuid>> {
        let store = manager.store.lock().await;
        let mut statement = store
            .conn
            .prepare("SELECT id FROM sessions WHERE continued_from=?1 ORDER BY created_at,id")?;
        let ids = statement
            .query_map([predecessor_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids
            .into_iter()
            .map(|id| Uuid::parse_str(&id))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn install_rotation_baton_state(
        store: &mut Store,
        fixture_root: &std::path::Path,
        epic_id: Uuid,
        predecessor_id: Uuid,
        state: AgentSuccessorStateV1,
        working_dir: &std::path::Path,
    ) -> anyhow::Result<Uuid> {
        let reservation_ids = AgentSuccessorReservationIds {
            reservation_id: Uuid::new_v4(),
            candidate_session_id: Uuid::new_v4(),
            transition_id: Uuid::new_v4(),
        };
        let request = AgentReserveSuccessorRequestV1 {
            kind: SessionKind::Task,
            model: Some("rotation-baton-model".into()),
            effort: Some("high".into()),
            query: "continue the reserved master baton".into(),
            topology_node: Some("rotation-baton".into()),
            iteration: Some(1),
            tags: Some(vec!["rotation-baton".into()]),
            idempotency_key: format!("rotation-baton-{}", state.as_str()),
        };
        let reserved =
            match store.reserve_agent_successor(predecessor_id, &request, reservation_ids)? {
                ReserveAgentSuccessorOutcome::Reserved(reservation) => reservation,
                ReserveAgentSuccessorOutcome::Replayed(_) => {
                    anyhow::bail!("fresh rotation baton reservation replayed")
                }
            };
        if state == AgentSuccessorStateV1::Reserved {
            return Ok(reserved.candidate_session_id);
        }

        let launch_ids = AgentSuccessorLaunchIds {
            launch_attempt_id: Uuid::new_v4(),
            model_invocation_id: Uuid::new_v4(),
            transition_id: Uuid::new_v4(),
        };
        let launching = match store.claim_agent_successor_launch(
            reserved.reservation_id,
            reserved.state_version,
            launch_ids,
        )? {
            ClaimAgentSuccessorOutcome::Claimed(reservation) => reservation,
            other => anyhow::bail!("rotation baton claim did not launch: {other:?}"),
        };
        if state == AgentSuccessorStateV1::Launching {
            return Ok(launching.candidate_session_id);
        }
        if state == AgentSuccessorStateV1::Uncertain {
            let uncertain = store.settle_agent_successor_uncertain(
                launching.reservation_id,
                launching.state_version,
                launch_ids.launch_attempt_id,
                Uuid::new_v4(),
                "rotation interleaving provider effect is uncertain",
                "rotation_interleaving_uncertain",
            )?;
            return Ok(uncertain.candidate_session_id);
        }
        if state == AgentSuccessorStateV1::Failed {
            let failed = store.settle_agent_successor_failed(
                launching.reservation_id,
                launching.state_version,
                launch_ids.launch_attempt_id,
                Uuid::new_v4(),
                "rotation interleaving provider failed before establishment",
                "rotation_interleaving_failed",
            )?;
            return Ok(failed.candidate_session_id);
        }
        anyhow::ensure!(state == AgentSuccessorStateV1::Committed);

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let repo = live_rotation_repository(fixture_root);
        let allocation = crate::sandbox::SandboxAllocator::new(fixture_root.join("sandboxes"))
            .allocate(
                launching.candidate_session_id,
                &repo,
                SandboxKind::GitWorktree,
                "HEAD",
                None,
            )
            .expect("allocate committed rotation baton worktree");
        let branch = allocation
            .branch
            .clone()
            .expect("committed rotation baton worktree branch");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&allocation.root)
                .output()
                .expect("read committed rotation baton metadata");
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .expect("committed rotation baton git output utf8")
                .trim()
                .to_owned()
        };
        let repository_identity = std::fs::canonicalize(git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))
        .expect("canonical committed rotation baton git common dir");
        let source_commit = git(&["rev-parse", "HEAD"]);
        let mut candidate = test_session(launching.candidate_session_id, SessionStatus::Running);
        candidate.session_kind = launching.candidate_kind;
        candidate.parent_id = Some(epic_id);
        candidate.continued_from = Some(predecessor_id);
        candidate.working_dir = repo.clone();
        candidate.git_branch = Some(branch.clone());
        candidate.sandbox_kind = Some(SandboxKind::GitWorktree);
        candidate.sandbox_root = Some(allocation.root.clone());
        candidate.sandbox_branch = Some(branch.clone());
        candidate.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store.insert_session_with_custody(
            &candidate,
            crate::store::sandbox_custody::SessionCustodyBinding::New(
                crate::store::sandbox_custody::NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: repo.display().to_string(),
                    sandbox_root: allocation.root.display().to_string(),
                    sandbox_branch: branch,
                    repository_identity: repository_identity.display().to_string(),
                    source_commit,
                    cause: crate::store::sandbox_custody::CustodyCause::FreshLaunch,
                },
            ),
        )?;
        store.conn.execute(
            "INSERT INTO model_invocations
             (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
             VALUES(?1,'agent.reserve_successor','orchestration','foreground','paid','admitted','running','agent_reserve_successor',?2,?3)",
            rusqlite::params![
                launch_ids.model_invocation_id.to_string(),
                candidate.id.to_string(),
                now,
            ],
        )?;
        store.set_session_model_invocation(candidate.id, Some(launch_ids.model_invocation_id))?;
        let committed = store.commit_agent_successor_authority(
            launching.reservation_id,
            launching.state_version,
            launch_ids.launch_attempt_id,
            Uuid::new_v4(),
            &serde_json::json!({"provider":"Claude","live":true}),
        )?;
        Ok(committed.candidate_session_id)
    }

    async fn drive_successful_rotation_to_authority_boundary(
        manager: &SessionManager,
        parent: Session,
        child: Session,
        rotation_candidate: crate::sandbox::custody::RotationCustodyCandidate,
        epic_id: Uuid,
        expected_lead: Uuid,
        predecessor_token: &str,
    ) -> anyhow::Result<()> {
        let parent_id = parent.id;
        let child_id = child.id;
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let mut events = manager.event_bus().subscribe();
        let rotation = spawn_rotation_child_for_test(
            manager,
            CompletedSession::for_test(parent),
            child,
            rotation_candidate,
            None,
        );
        let observe = async {
            loop {
                let event = events.recv().await?;
                if matches!(
                    event.as_ref(),
                    DaemonEvent::SessionArchived { session_id, .. } if *session_id == parent_id
                ) {
                    break;
                }
            }
            let durable_lead = manager
                .store
                .lock()
                .await
                .get_session(epic_id)?
                .ok_or_else(|| anyhow::anyhow!("rotation matrix Epic missing"))?
                .lead_session_id;
            assert_eq!(durable_lead, Some(expected_lead));
            let cached_lead = manager
                .completed
                .read()
                .await
                .get(&epic_id)
                .ok_or_else(|| anyhow::anyhow!("rotation matrix Epic cache missing"))?
                .session
                .lead_session_id;
            assert_eq!(cached_lead, Some(expected_lead));
            let tokens = manager.agent_tokens.read().await;
            assert!(
                tokens.token_for_session(expected_lead).is_some(),
                "durable/cache lead must own one live authority token"
            );
            assert!(tokens.get(predecessor_token).is_none());
            drop(tokens);
            manager.interrupt_session(child_id).await?;
            Ok::<(), anyhow::Error>(())
        };
        let ((), observation) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(rotation, observe)
        })
        .await?;
        observation?;
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        manager.event_bus().unsubscribe();
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    struct H2PostAssignmentRotationFixture {
        project: Project,
        assigned_idea: rsi_common::types::Idea,
        parent: Session,
        parent_token: String,
        epic_id: Uuid,
        rotation_id: String,
        controller_reservation_id: Uuid,
        child_id: Uuid,
        child: Session,
        rotation_candidate: crate::sandbox::custody::RotationCustodyCandidate,
    }

    async fn h2_post_assignment_rotation_fixture(
        manager: &SessionManager,
        working_dir: &std::path::Path,
        label: &str,
    ) -> anyhow::Result<H2PostAssignmentRotationFixture> {
        let intent = Uuid::new_v4().simple().to_string();
        let (project, assigned_idea, mut parent, parent_token) =
            d03_rotation_fixture(manager, working_dir, &intent).await?;
        let parent_id = parent.id;
        let epic_id = Uuid::new_v4();
        let rotation_id = format!("{label}:{intent}");
        let controller_reservation_id = idea_controller_reservation_id(
            assigned_idea.id,
            &format!("rotation:{parent_id}:{rotation_id}"),
        );
        let child_id = idea_controller_candidate_session_id(controller_reservation_id);
        let mut epic = test_session(epic_id, SessionStatus::Completed);
        epic.session_kind = SessionKind::Epic;
        epic.working_dir = working_dir.to_path_buf();
        {
            let store = manager.store.lock().await;
            store.insert_session(&epic)?;
            store.conn.execute(
                "UPDATE sessions SET session_kind='Task' WHERE id=?1",
                [parent_id.to_string()],
            )?;
            store.update_session_parent(parent_id, Some(epic_id))?;
            store.set_lead_session(epic_id, Some(parent_id))?;
        }
        parent.session_kind = SessionKind::Task;
        parent.parent_id = Some(epic_id);
        epic.lead_session_id = Some(parent_id);
        manager
            .completed
            .write()
            .await
            .insert(epic_id, CompletedSession::for_test(epic));
        let rotation_candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.session_kind = SessionKind::Task;
        child.parent_id = Some(epic_id);
        child.continued_from = Some(parent_id);
        child.project_id = Some(project.id);
        child.working_dir = working_dir.to_path_buf();
        Ok(H2PostAssignmentRotationFixture {
            project,
            assigned_idea,
            parent,
            parent_token,
            epic_id,
            rotation_id,
            controller_reservation_id,
            child_id,
            child,
            rotation_candidate,
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h2_rotation_successor_interleaving_matrix_preserves_one_authority_projection()
    -> anyhow::Result<()> {
        for state in [
            AgentSuccessorStateV1::Reserved,
            AgentSuccessorStateV1::Launching,
            AgentSuccessorStateV1::Uncertain,
            AgentSuccessorStateV1::Committed,
            AgentSuccessorStateV1::Failed,
        ] {
            let (manager, directory) = rotation_manager();
            let epic_id = Uuid::new_v4();
            let parent_id = Uuid::new_v4();
            let child_id = Uuid::new_v4();
            let predecessor_token = format!("rotation-predecessor-{}", state.as_str());
            let baton_token = format!("rotation-baton-{}", state.as_str());

            let mut epic = test_session(epic_id, SessionStatus::Completed);
            epic.session_kind = SessionKind::Epic;
            epic.lead_session_id = None;
            epic.working_dir = directory.path().to_path_buf();
            let mut parent = test_session(parent_id, SessionStatus::Completed);
            parent.session_kind = SessionKind::Task;
            parent.parent_id = Some(epic_id);
            parent.working_dir = directory.path().to_path_buf();
            {
                let store = manager.store.lock().await;
                store.insert_session(&epic)?;
                store.insert_session(&parent)?;
                store.set_lead_session(epic_id, Some(parent_id))?;
            }
            epic.lead_session_id = Some(parent_id);
            manager
                .completed
                .write()
                .await
                .insert(epic_id, CompletedSession::for_test(epic));
            manager
                .register_agent_token(predecessor_token.clone(), parent_id)
                .await;

            let baton_candidate = {
                let mut store = manager.store.lock().await;
                install_rotation_baton_state(
                    &mut store,
                    directory.path(),
                    epic_id,
                    parent_id,
                    state,
                    directory.path(),
                )?
            };
            if matches!(
                state,
                AgentSuccessorStateV1::Launching
                    | AgentSuccessorStateV1::Uncertain
                    | AgentSuccessorStateV1::Committed
            ) {
                manager
                    .register_agent_token(baton_token.clone(), baton_candidate)
                    .await;
            }
            if state == AgentSuccessorStateV1::Committed {
                manager
                    .completed
                    .write()
                    .await
                    .get_mut(&epic_id)
                    .expect("committed matrix Epic cache")
                    .session
                    .lead_session_id = Some(baton_candidate);
                manager.agent_tokens.write().await.revoke_session(parent_id);
            }

            let rotation_candidate = manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&parent)
                .await?;
            let mut child = test_session(child_id, SessionStatus::Starting);
            child.session_kind = SessionKind::Task;
            child.parent_id = Some(epic_id);
            child.continued_from = Some(parent_id);
            child.working_dir = directory.path().to_path_buf();

            // Every reservation state except `failed` owns, or is about to
            // own, the predecessor's single continuation, so context rotation
            // defers to the successor-reservation kernel and creates no second
            // `continued_from` successor. `committed` differs from the
            // nonterminal states only in where the authority already sits: the
            // commit has already moved the durable Epic lead to the baton
            // candidate and retired the predecessor's token.
            if state != AgentSuccessorStateV1::Failed {
                let expected_lead = if state == AgentSuccessorStateV1::Committed {
                    baton_candidate
                } else {
                    parent_id
                };
                let mut events = manager.event_bus().subscribe();
                spawn_rotation_child_for_test(
                    &manager,
                    CompletedSession::for_test(parent.clone()),
                    child,
                    rotation_candidate,
                    None,
                )
                .await;
                assert_eq!(
                    manager
                        .store
                        .lock()
                        .await
                        .get_session(epic_id)?
                        .expect("locked matrix Epic")
                        .lead_session_id,
                    Some(expected_lead),
                    "{state:?} durable lead must stay with the successor kernel"
                );
                assert_eq!(
                    manager
                        .completed
                        .read()
                        .await
                        .get(&epic_id)
                        .expect("locked matrix Epic cache")
                        .session
                        .lead_session_id,
                    Some(expected_lead),
                    "{state:?} cached lead must stay with the successor kernel"
                );
                if state == AgentSuccessorStateV1::Committed {
                    assert_eq!(
                        manager.resolve_agent_token(&predecessor_token).await,
                        None,
                        "a committed handoff retires the predecessor token, and the \
                         fenced rotation must not mint it back"
                    );
                } else {
                    assert_eq!(
                        manager.resolve_agent_token(&predecessor_token).await,
                        Some(parent_id),
                        "{state:?} predecessor token must remain dispatch-capable"
                    );
                }
                assert!(manager.completed.read().await.contains_key(&parent_id));
                assert_eq!(
                    manager
                        .store
                        .lock()
                        .await
                        .get_session(parent_id)?
                        .expect("fenced matrix predecessor")
                        .status,
                    SessionStatus::Completed,
                    "{state:?} fenced rotation leaves the predecessor Completed and restorable"
                );
                assert!(manager.store.lock().await.get_session(child_id)?.is_none());
                assert!(
                    !manager
                        .agent_tokens
                        .read()
                        .await
                        .values()
                        .any(|session_id| *session_id == child_id)
                );
                if state != AgentSuccessorStateV1::Reserved {
                    assert_eq!(
                        manager.resolve_agent_token(&baton_token).await,
                        Some(baton_candidate)
                    );
                }
                if state == AgentSuccessorStateV1::Committed {
                    assert_eq!(
                        continued_from_successors(&manager, parent_id).await?,
                        vec![baton_candidate],
                        "the committed baton candidate is the predecessor's one continuation"
                    );
                }
                while let Ok(event) = events.try_recv() {
                    assert!(
                        !matches!(
                            event.as_ref(),
                            DaemonEvent::SessionMetadataChanged { session_id, .. }
                                if *session_id == epic_id
                        ),
                        "{state:?} rotation must not publish a lead delta: {event:?}"
                    );
                    assert!(
                        !matches!(
                            event.as_ref(),
                            DaemonEvent::SessionArchived { session_id, .. }
                                if *session_id == parent_id
                        ),
                        "{state:?} rotation must not archive the predecessor: {event:?}"
                    );
                }
                manager.event_bus().unsubscribe();
                continue;
            }

            // A settled `failed` reservation transferred no authority, so
            // rotation proceeds and its successor takes the lead.
            drive_successful_rotation_to_authority_boundary(
                &manager,
                parent,
                child,
                rotation_candidate,
                epic_id,
                child_id,
                &predecessor_token,
            )
            .await?;
            assert_eq!(
                continued_from_successors(&manager, parent_id).await?,
                vec![child_id],
                "a failed baton leaves the rotation successor as the one continuation"
            );
        }
        Ok(())
    }

    /// Both turnover mechanisms driven at one predecessor: a committed
    /// successor reservation, then a context rotation of the same session.
    /// Exactly one continuation survives, and `manager_lineage_tip` resolves to
    /// it — the state that four times in one working session became
    /// `manager_lineage_ambiguous`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_predecessor_keeps_one_continuation_across_both_turnover_mechanisms()
    -> anyhow::Result<()> {
        let (manager, directory) = rotation_manager();
        let epic_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let rotation_child_id = Uuid::new_v4();

        let mut epic = test_session(epic_id, SessionStatus::Completed);
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = None;
        epic.working_dir = directory.path().to_path_buf();
        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.session_kind = SessionKind::Task;
        parent.parent_id = Some(epic_id);
        parent.working_dir = directory.path().to_path_buf();
        {
            let store = manager.store.lock().await;
            store.insert_session(&epic)?;
            store.insert_session(&parent)?;
            store.set_lead_session(epic_id, Some(parent_id))?;
        }
        epic.lead_session_id = Some(parent_id);
        manager
            .completed
            .write()
            .await
            .insert(epic_id, CompletedSession::for_test(epic));

        // Mechanism 1 — the successor-reservation kernel commits a candidate.
        let baton_candidate = {
            let mut store = manager.store.lock().await;
            install_rotation_baton_state(
                &mut store,
                directory.path(),
                epic_id,
                parent_id,
                AgentSuccessorStateV1::Committed,
                directory.path(),
            )?
        };
        manager
            .completed
            .write()
            .await
            .get_mut(&epic_id)
            .expect("turnover Epic cache")
            .session
            .lead_session_id = Some(baton_candidate);
        assert_eq!(
            continued_from_successors(&manager, parent_id).await?,
            vec![baton_candidate]
        );

        // Mechanism 2 — a context rotation of the same predecessor.
        let rotation_candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent)
            .await?;
        let mut rotation_child = test_session(rotation_child_id, SessionStatus::Starting);
        rotation_child.session_kind = SessionKind::Task;
        rotation_child.parent_id = Some(epic_id);
        rotation_child.continued_from = Some(parent_id);
        rotation_child.rotation_depth = parent.rotation_depth + 1;
        rotation_child.working_dir = directory.path().to_path_buf();
        let _ = try_spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent.clone()),
            rotation_child.clone(),
            rotation_candidate,
            None,
        )
        .await;

        // One predecessor, one continuation.
        assert_eq!(
            continued_from_successors(&manager, parent_id).await?,
            vec![baton_candidate],
            "the committed reservation candidate is the surviving continuation"
        );
        {
            let store = manager.store.lock().await;
            assert_eq!(
                store.manager_lineage_tip(parent_id)?,
                baton_candidate,
                "manager lineage resolves to the one continuation"
            );
            assert_eq!(
                store
                    .get_session(parent_id)?
                    .expect("fenced turnover predecessor")
                    .status,
                SessionStatus::Completed,
                "the fenced rotation leaves the predecessor Completed and restorable"
            );
            assert_eq!(
                store
                    .get_session(epic_id)?
                    .expect("turnover Epic")
                    .lead_session_id,
                Some(baton_candidate)
            );
        }
        assert!(manager.completed.read().await.contains_key(&parent_id));

        // The atomic layer: even with the preflight bypassed entirely, the
        // transaction that would create the branching row refuses.
        {
            let mut store = manager.store.lock().await;
            let refusal = store
                .insert_reserved_rotation_session_with_invocation(
                    &rotation_child,
                    Uuid::new_v4(),
                    "test-rotation",
                )
                .expect_err("branching rotation successor must not reach the sessions table");
            assert!(
                format!("{refusal}").contains("agent_successor_predecessor_already_continued"),
                "unexpected refusal: {refusal}"
            );
        }

        // A second reservation under a different idempotency key is the
        // intra-kernel shape of the same branch, and is refused the same way.
        {
            let store = manager.store.lock().await;
            let refusal = store
                .reserve_agent_successor(
                    parent_id,
                    &AgentReserveSuccessorRequestV1 {
                        kind: SessionKind::Task,
                        model: Some("second-baton-model".into()),
                        effort: Some("high".into()),
                        query: "continue the master program again".into(),
                        topology_node: Some("second-baton".into()),
                        iteration: Some(2),
                        tags: Some(vec!["second-baton".into()]),
                        idempotency_key: "second-baton".into(),
                    },
                    AgentSuccessorReservationIds {
                        reservation_id: Uuid::new_v4(),
                        candidate_session_id: Uuid::new_v4(),
                        transition_id: Uuid::new_v4(),
                    },
                )
                .expect_err("a predecessor may reserve only one successor");
            assert!(
                format!("{refusal}").contains("agent_successor_predecessor_already_continued"),
                "unexpected refusal: {refusal}"
            );
        }

        assert_eq!(
            continued_from_successors(&manager, parent_id).await?,
            vec![baton_candidate],
            "both refusals leave the single continuation intact"
        );
        assert_eq!(
            manager.store.lock().await.manager_lineage_tip(parent_id)?,
            baton_candidate
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h2_rotation_baton_fence_precedes_controller_assignment_and_revocation()
    -> anyhow::Result<()> {
        let (manager, directory) = rotation_manager();
        let intent = Uuid::new_v4().simple().to_string();
        let (project, assigned_idea, mut parent, parent_token) =
            d03_rotation_fixture(&manager, directory.path(), &intent).await?;
        let parent_id = parent.id;
        let epic_id = Uuid::new_v4();
        let rotation_id = format!("h2-controller-baton:{intent}");
        let controller_reservation_id = idea_controller_reservation_id(
            assigned_idea.id,
            &format!("rotation:{parent_id}:{rotation_id}"),
        );
        let child_id = idea_controller_candidate_session_id(controller_reservation_id);
        let mut epic = test_session(epic_id, SessionStatus::Completed);
        epic.session_kind = SessionKind::Epic;
        epic.working_dir = directory.path().to_path_buf();
        {
            let store = manager.store.lock().await;
            store.insert_session(&epic)?;
            store.conn.execute(
                "UPDATE sessions SET session_kind='Task' WHERE id=?1",
                [parent_id.to_string()],
            )?;
            store.update_session_parent(parent_id, Some(epic_id))?;
            store.set_lead_session(epic_id, Some(parent_id))?;
        }
        parent.session_kind = SessionKind::Task;
        parent.parent_id = Some(epic_id);
        epic.lead_session_id = Some(parent_id);
        manager
            .completed
            .write()
            .await
            .insert(epic_id, CompletedSession::for_test(epic));
        let rotation_candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.parent_id = Some(epic_id);
        child.continued_from = Some(parent_id);
        child.project_id = Some(project.id);
        child.working_dir = directory.path().to_path_buf();
        let mut events = manager.event_bus().subscribe();
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            child_id,
            super::super::launch::ControllerCandidateTestPhase::AfterDurablePersistence,
        );
        let rotation = spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent),
            child,
            rotation_candidate,
            Some(rotation_id),
        );
        let reserve_during_rotation = async {
            reached.await?;
            {
                let mut store = manager.store.lock().await;
                let _ = install_rotation_baton_state(
                    &mut store,
                    directory.path(),
                    epic_id,
                    parent_id,
                    AgentSuccessorStateV1::Reserved,
                    directory.path(),
                )?;
            }
            resume
                .send(())
                .map_err(|()| anyhow::anyhow!("rotation baton pause receiver dropped"))?;
            Ok::<(), anyhow::Error>(())
        };
        let ((), reservation) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(rotation, reserve_during_rotation)
        })
        .await?;
        reservation?;

        let projection = manager
            .store
            .lock()
            .await
            .load_idea_controller_projection_v1(project.id, assigned_idea.id)?;
        assert_eq!(projection.current_controller_session_id, Some(parent_id));
        assert_eq!(projection.controller_epoch, assigned_idea.controller_epoch);
        assert!(
            projection.row_version > assigned_idea.row_version,
            "controller reservation/release may advance row version without transferring assignment"
        );
        assert_eq!(
            manager.resolve_agent_token(&parent_token).await,
            Some(parent_id)
        );
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_session(epic_id)?
                .ok_or_else(|| anyhow::anyhow!("controller-interleaving Epic missing"))?
                .lead_session_id,
            Some(parent_id)
        );
        assert_eq!(
            manager
                .completed
                .read()
                .await
                .get(&epic_id)
                .ok_or_else(|| anyhow::anyhow!("controller-interleaving Epic cache missing"))?
                .session
                .lead_session_id,
            Some(parent_id)
        );
        assert!(manager.completed.read().await.contains_key(&parent_id));
        assert!(!manager.active.read().await.contains_key(&child_id));
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_session(child_id)?
                .ok_or_else(|| anyhow::anyhow!("fenced rotation child missing"))?
                .status,
            SessionStatus::Failed
        );
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|session_id| *session_id == child_id)
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SystemMessage { message, .. }
                        if message.starts_with("idea_controller_assigned:")
                ),
                "baton-fenced rotation must not publish controller assignment: {event:?}"
            );
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SessionArchived { session_id, .. } if *session_id == parent_id
                ),
                "baton-fenced rotation must not archive its controller predecessor: {event:?}"
            );
        }
        manager.event_bus().unsubscribe();
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    /// RPC-1 C4: a publication refused by a nonterminal master-successor
    /// reservation leaves the predecessor as tip and lead, settles the
    /// reserved child Failed, and closes the rotation with exactly one
    /// `refused:lead_transfer` terminal event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lead_transfer_refusal_leaves_predecessor_tip_and_lead() -> anyhow::Result<()> {
        let (manager, directory) = rotation_manager();
        let fixture = h2_post_assignment_rotation_fixture(
            &manager,
            directory.path(),
            "f3-lead-transfer-refusal",
        )
        .await?;
        let H2PostAssignmentRotationFixture {
            parent,
            epic_id,
            rotation_id,
            child_id,
            child,
            rotation_candidate,
            ..
        } = fixture;
        let parent_id = parent.id;
        let _scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            child_id,
            super::super::launch::ControllerCandidateTestPhase::AfterAssignmentBeforeRotationLeadTransfer,
        );
        let rotation = Box::pin(spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent),
            child,
            rotation_candidate,
            Some(rotation_id.clone()),
        ));
        let reserve_before_publication = async {
            reached.await?;
            {
                let mut store = manager.store.lock().await;
                let _ = install_rotation_baton_state(
                    &mut store,
                    directory.path(),
                    epic_id,
                    parent_id,
                    AgentSuccessorStateV1::Reserved,
                    directory.path(),
                )?;
            }
            resume
                .send(())
                .map_err(|()| anyhow::anyhow!("publication pause receiver dropped"))?;
            Ok::<(), anyhow::Error>(())
        };
        let ((), reservation) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(rotation, reserve_before_publication)
        })
        .await?;
        reservation?;

        let store = manager.store.lock().await;
        let events = terminal_rotation_events(&store, parent_id)?;
        assert_eq!(
            events
                .iter()
                .map(|(kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            vec!["refused:lead_transfer"],
            "exactly one terminal refusal closes the rotation"
        );
        assert_eq!(
            store
                .get_session(epic_id)?
                .ok_or_else(|| anyhow::anyhow!("Epic missing"))?
                .lead_session_id,
            Some(parent_id),
            "the predecessor keeps the Epic lead"
        );
        assert_eq!(
            store.find_published_rotation_successor(parent_id)?,
            None,
            "the refused child is never the published tip; the predecessor stays tip"
        );
        assert_eq!(
            store
                .get_session(child_id)?
                .ok_or_else(|| anyhow::anyhow!("refused child missing"))?
                .status,
            SessionStatus::Failed
        );
        assert_eq!(
            store
                .get_session(parent_id)?
                .ok_or_else(|| anyhow::anyhow!("predecessor missing"))?
                .status,
            SessionStatus::Completed
        );
        drop(store);
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h2_rotation_final_fence_after_controller_assignment_releases_and_settles_child()
    -> anyhow::Result<()> {
        let (manager, directory) = rotation_manager();
        let fixture = h2_post_assignment_rotation_fixture(
            &manager,
            directory.path(),
            "h2-post-assignment-release",
        )
        .await?;
        let H2PostAssignmentRotationFixture {
            project,
            assigned_idea,
            parent,
            parent_token,
            epic_id,
            rotation_id,
            controller_reservation_id,
            child_id,
            child,
            rotation_candidate,
        } = fixture;
        let parent_id = parent.id;
        let mut events = manager.event_bus().subscribe();
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            child_id,
            super::super::launch::ControllerCandidateTestPhase::AfterAssignmentBeforeRotationLeadTransfer,
        );
        let rotation = spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent),
            child,
            rotation_candidate,
            Some(rotation_id),
        );
        let reserve_after_assignment = async {
            reached.await?;
            {
                let mut store = manager.store.lock().await;
                let projection =
                    store.load_idea_controller_projection_v1(project.id, assigned_idea.id)?;
                assert_eq!(projection.current_controller_session_id, Some(child_id));
                assert!(store.controller_grant_v1(child_id).is_some());
                let _ = install_rotation_baton_state(
                    &mut store,
                    directory.path(),
                    epic_id,
                    parent_id,
                    AgentSuccessorStateV1::Reserved,
                    directory.path(),
                )?;
            }
            resume
                .send(())
                .map_err(|()| anyhow::anyhow!("post-assignment rotation pause receiver dropped"))?;
            Ok::<(), anyhow::Error>(())
        };
        let ((), reservation) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(rotation, reserve_after_assignment)
        })
        .await?;
        reservation?;

        assert!(!manager.active.read().await.contains_key(&child_id));
        assert!(manager.completed.read().await.contains_key(&parent_id));
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            manager.resolve_agent_token(&parent_token).await,
            Some(parent_id)
        );
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|session_id| *session_id == child_id)
        );
        {
            let store = manager.store.lock().await;
            let projection =
                store.load_idea_controller_projection_v1(project.id, assigned_idea.id)?;
            assert_eq!(projection.current_controller_session_id, None);
            assert!(store.controller_grant_v1(child_id).is_none());
            assert_eq!(
                store
                    .get_session(epic_id)?
                    .ok_or_else(|| anyhow::anyhow!("post-assignment Epic missing"))?
                    .lead_session_id,
                Some(parent_id)
            );
            let child = store
                .get_session(child_id)?
                .ok_or_else(|| anyhow::anyhow!("post-assignment refused child missing"))?;
            assert_eq!(child.status, SessionStatus::Failed);
            assert_eq!(
                child.stop_reason.as_deref(),
                Some("sandbox_custody:persistence_transition_failed")
            );
            let invocation_id = store
                .session_model_invocation_id(child_id)?
                .ok_or_else(|| anyhow::anyhow!("post-assignment child invocation missing"))?;
            let invocation: (String, Option<String>) = store.conn.query_row(
                "SELECT status,error_class FROM model_invocations WHERE id=?1",
                [invocation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(
                invocation,
                (
                    "failed".to_string(),
                    Some("rotation_lead_transfer_failed".to_string())
                )
            );
            let page = store.list_idea_events_v1(
                project.id,
                assigned_idea.id,
                IdeaEventPageRequestV1 {
                    after_sequence: 0,
                    limit: Some(16),
                },
            )?;
            let tail = page
                .events
                .last()
                .ok_or_else(|| anyhow::anyhow!("assigned controller release event missing"))?
                .controller_control_payload_v1()
                .map_err(anyhow::Error::msg)?;
            assert!(matches!(
                tail.request.operation,
                IdeaControllerControlOperationV1::ReleaseAssigned {
                    controller_session_id,
                    reason: ControllerReleaseReasonV1::OperatorRelease,
                    release_intent_key,
                    ..
                } if controller_session_id == child_id
                    && release_intent_key
                        == format!("rotation-lead-refused:{controller_reservation_id}")
            ));
        }
        assert_eq!(
            manager
                .completed
                .read()
                .await
                .get(&epic_id)
                .ok_or_else(|| anyhow::anyhow!("post-assignment Epic cache missing"))?
                .session
                .lead_session_id,
            Some(parent_id)
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SessionMetadataChanged { session_id, .. }
                        if *session_id == epic_id
                ),
                "final-fence refusal must not publish an Epic cache delta: {event:?}"
            );
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SystemMessage { message, .. }
                        if message.starts_with("idea_controller_assigned:")
                ),
                "final-fence refusal must not publish controller success: {event:?}"
            );
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SessionArchived { session_id, .. } if *session_id == parent_id
                ),
                "final-fence refusal must not archive its predecessor: {event:?}"
            );
        }
        manager.event_bus().unsubscribe();
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn h2_rotation_release_failure_retains_visible_controller_until_exact_retry()
    -> anyhow::Result<()> {
        let (manager, directory) = rotation_manager();
        let fixture = h2_post_assignment_rotation_fixture(
            &manager,
            directory.path(),
            "h2-post-assignment-release-failure",
        )
        .await?;
        let H2PostAssignmentRotationFixture {
            project,
            assigned_idea,
            parent,
            parent_token,
            epic_id,
            rotation_id,
            controller_reservation_id,
            child_id,
            child,
            rotation_candidate,
        } = fixture;
        let parent_id = parent.id;
        let mut events = manager.event_bus().subscribe();
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            child_id,
            super::super::launch::ControllerCandidateTestPhase::AfterAssignmentBeforeRotationLeadTransfer,
        );
        let rotation = spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent),
            child,
            rotation_candidate,
            Some(rotation_id),
        );
        let recover = async {
            reached.await?;
            let assignment_event_id = {
                let mut store = manager.store.lock().await;
                let projection =
                    store.load_idea_controller_projection_v1(project.id, assigned_idea.id)?;
                assert_eq!(projection.current_controller_session_id, Some(child_id));
                assert!(store.controller_grant_v1(child_id).is_some());
                let event_id: String = store.conn.query_row(
                    "SELECT id FROM idea_events
                     WHERE idea_id=?1 AND event_type='controller_assigned'
                     ORDER BY sequence DESC LIMIT 1",
                    [assigned_idea.id.to_string()],
                    |row| row.get(0),
                )?;
                let _ = install_rotation_baton_state(
                    &mut store,
                    directory.path(),
                    epic_id,
                    parent_id,
                    AgentSuccessorStateV1::Reserved,
                    directory.path(),
                )?;
                Uuid::parse_str(&event_id)?
            };
            inject_d03_idea_controller_write_fault(
                IdeaControllerWriteFault::AssignedReleaseBeforeCommit,
            );
            resume
                .send(())
                .map_err(|()| anyhow::anyhow!("release-failure rotation pause receiver dropped"))?;

            let mut observed = Vec::new();
            loop {
                let event = events.recv().await?;
                let recovery_visible = matches!(
                    event.as_ref(),
                    DaemonEvent::SystemMessage { message, .. }
                        if message == &format!("idea_controller_reconciled:{assignment_event_id}")
                );
                observed.push(event);
                if recovery_visible {
                    break;
                }
            }
            assert!(manager.active.read().await.contains_key(&child_id));
            assert!(manager.completed.read().await.contains_key(&parent_id));
            assert!(scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(
                manager.resolve_agent_token(&parent_token).await,
                Some(parent_id)
            );
            assert!(
                manager
                    .agent_tokens
                    .read()
                    .await
                    .token_for_session(child_id)
                    .is_some()
            );
            {
                let store = manager.store.lock().await;
                let projection =
                    store.load_idea_controller_projection_v1(project.id, assigned_idea.id)?;
                assert_eq!(projection.current_controller_session_id, Some(child_id));
                assert!(store.controller_grant_v1(child_id).is_some());
                assert_eq!(
                    store
                        .get_session(epic_id)?
                        .ok_or_else(|| anyhow::anyhow!("release-failure Epic missing"))?
                        .lead_session_id,
                    Some(parent_id)
                );
            }
            assert_eq!(
                manager
                    .completed
                    .read()
                    .await
                    .get(&epic_id)
                    .ok_or_else(|| anyhow::anyhow!("release-failure Epic cache missing"))?
                    .session
                    .lead_session_id,
                Some(parent_id)
            );

            let current = manager
                .store
                .lock()
                .await
                .load_idea_controller_projection_v1(project.id, assigned_idea.id)?;
            let recovery = crate::idea_control::IdeaControllerTransferHandle::for_system(
                Arc::clone(&manager.store),
                project.id,
                assigned_idea.id,
                Arc::new(crate::idea_control::SystemIdeaControllerClock),
            )
            .await?;
            let released = recovery
                .release_assigned(&ReleaseAssignedIdeaControllerRequestV1 {
                    expected_row_version: current.row_version,
                    release_intent_key: format!(
                        "rotation-lead-refused:{controller_reservation_id}"
                    ),
                    reason: ControllerReleaseReasonV1::OperatorRelease,
                })
                .await?;
            assert_eq!(released.idea.current_controller_session_id, None);
            assert!(
                manager
                    .store
                    .lock()
                    .await
                    .controller_grant_v1(child_id)
                    .is_none()
            );
            manager.interrupt_session(child_id).await?;
            Ok::<Vec<Arc<DaemonEvent>>, anyhow::Error>(observed)
        };
        let ((), observed) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(rotation, recover)
        })
        .await?;
        let mut observed = observed?;
        while let Ok(event) = events.try_recv() {
            observed.push(event);
        }
        for event in observed {
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SessionMetadataChanged { session_id, .. }
                        if *session_id == epic_id
                ),
                "release-failure recovery must not publish an Epic cache delta: {event:?}"
            );
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SystemMessage { message, .. }
                        if message.starts_with("idea_controller_assigned:")
                ),
                "release-failure recovery must not publish controller success: {event:?}"
            );
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SessionArchived { session_id, .. } if *session_id == parent_id
                ),
                "release-failure recovery must not archive its predecessor: {event:?}"
            );
        }
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            manager.resolve_agent_token(&parent_token).await,
            Some(parent_id)
        );
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .load_idea_controller_projection_v1(project.id, assigned_idea.id)?
                .current_controller_session_id,
            None
        );
        manager.event_bus().unsubscribe();
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    pub(super) fn test_session(id: Uuid, status: SessionStatus) -> Session {
        Session {
            context_fill_pct: None,
            id,
            status,
            session_kind: SessionKind::Standard,
            provider: SessionProvider::Claude,
            context_usage_confidence: ContextUsageConfidence::Missing,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            model: None,
            claude_session_id: Some("provider-session".to_string()),
            project_id: None,
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            stop_reason: None,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            pending_question: None,
            pending_archive: false,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            approval_started_at: None,
            work_time_ms: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            // V99: populated from the provider handshake / result event, not at
            // construction. A session that never reaches those has none of these facts.
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    async fn insert_live_handoff_fixture(
        manager: &SessionManager,
        fixture_root: &std::path::Path,
    ) -> (Uuid, std::path::PathBuf) {
        let repo = live_rotation_repository(fixture_root);
        let session_id = Uuid::new_v4();
        let allocation = crate::sandbox::SandboxAllocator::new(fixture_root.join("sandboxes"))
            .allocate(session_id, &repo, SandboxKind::GitWorktree, "HEAD", None)
            .expect("allocate real handoff worktree");
        let branch = allocation.branch.clone().expect("worktree branch");
        let mut session = test_session(session_id, SessionStatus::Completed);
        session.working_dir = repo.clone();
        session.claude_session_id = Some(format!("handoff-{session_id}"));
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(allocation.root.clone());
        session.sandbox_branch = Some(branch.clone());
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        session.git_branch = Some(branch.clone());
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&allocation.root)
                .output()
                .expect("read real worktree metadata");
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .expect("git output utf8")
                .trim()
                .to_owned()
        };
        let repository_identity = std::fs::canonicalize(git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))
        .expect("canonical git common dir");
        let source_commit = git(&["rev-parse", "HEAD"]);
        manager
            .store
            .lock()
            .await
            .insert_session_with_custody(
                &session,
                crate::store::sandbox_custody::SessionCustodyBinding::New(
                    crate::store::sandbox_custody::NewCustodyRoot {
                        custody_id: Uuid::new_v4(),
                        canonical_repo_dir: repo.display().to_string(),
                        sandbox_root: allocation.root.display().to_string(),
                        sandbox_branch: branch,
                        repository_identity: repository_identity.display().to_string(),
                        source_commit,
                        cause: crate::store::sandbox_custody::CustodyCause::FreshLaunch,
                    },
                ),
            )
            .expect("persist V83 live custody fixture");
        manager
            .completed
            .write()
            .await
            .insert(session_id, CompletedSession::for_test(session));
        (session_id, allocation.root)
    }

    fn handoff_resume_for_test(
        manager: &SessionManager,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = ()> + '_ {
        SessionManager::resume_for_handoff_write(
            session_id,
            None,
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager
                .model_call_settlements
                .handle()
                .expect("settlement producer"),
            manager.persistence.clone(),
            false,
            manager.socket_path.clone(),
            Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            Arc::clone(&manager.runtime_config),
            Arc::clone(&manager.spawn_coordinator),
            Arc::clone(&manager.agent_tokens),
            Arc::clone(&manager.spawn_epoch),
            Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
        )
    }

    async fn resume_handoff_for_test(manager: &SessionManager, session_id: Uuid) {
        handoff_resume_for_test(manager, session_id).await;
    }

    async fn resume_handoff_through_reconstruction_for_test(
        manager: &SessionManager,
        session_id: Uuid,
    ) {
        let scripted = super::super::launch::install_controller_candidate_test_process(session_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            session_id,
            super::super::launch::ControllerCandidateTestPhase::SameIdBeforeReconstruction,
        );
        let handoff = handoff_resume_for_test(manager, session_id);
        tokio::pin!(handoff);
        tokio::select! {
            () = &mut handoff => panic!("handoff completed before reconstruction pause"),
            result = reached => result.expect("handoff reconstruction pause sender remained live"),
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("handoff reached no reconstruction pause within deadline");
            }
        }
        manager
            .interrupt_session(session_id)
            .await
            .expect("interrupt paused handoff provider");
        resume
            .send(())
            .expect("resume paused handoff reconstruction");
        tokio::time::timeout(std::time::Duration::from_secs(5), handoff)
            .await
            .expect("interrupted handoff completes within deadline");
        assert!(
            !scripted.alive.load(std::sync::atomic::Ordering::SeqCst),
            "interrupted scripted handoff provider is cleaned up"
        );
        super::super::launch::drop_controller_candidate_test_stream(session_id);
    }

    fn assert_handoff_transition(error: &DaemonError) {
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected typed sandbox-custody refusal, got {error}");
        };
        assert_eq!(data["error"]["transition"], "handoff_resume");
    }

    async fn handoff_invocation_state(
        manager: &SessionManager,
        session_id: Uuid,
    ) -> (i64, Option<Uuid>) {
        let store = manager.store.lock().await;
        let count = store
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("count model invocations");
        let pointer = store
            .session_model_invocation_id(session_id)
            .expect("load session model invocation pointer");
        (count, pointer)
    }

    #[tokio::test]
    async fn h1_v83_handoff_config_uses_exact_ordinary_and_live_reuse_paths() {
        let (ordinary, ordinary_dir) = rotation_manager();
        let ordinary_id = Uuid::new_v4();
        let mut ordinary_session = test_session(ordinary_id, SessionStatus::Completed);
        ordinary_session.working_dir = ordinary_dir.path().to_path_buf();
        ordinary_session.claude_session_id = Some(format!("ordinary-{ordinary_id}"));
        ordinary
            .store
            .lock()
            .await
            .insert_session(&ordinary_session)
            .expect("persist ordinary fixture");
        ordinary
            .completed
            .write()
            .await
            .insert(ordinary_id, CompletedSession::for_test(ordinary_session));
        install_handoff_custody_config_observation_for_test(ordinary_id);
        resume_handoff_through_reconstruction_for_test(&ordinary, ordinary_id).await;
        assert_eq!(
            take_handoff_custody_config_for_test(ordinary_id),
            Some((ordinary_dir.path().to_path_buf(), None)),
            "ordinary handoff must use only its exact canonical cwd"
        );
        assert!(ordinary.completed.read().await.contains_key(&ordinary_id));
        assert!(!ordinary.active.read().await.contains_key(&ordinary_id));
        assert!(handoff_custody_test_seams_are_clean(ordinary_id));

        let (live, live_dir) = rotation_manager();
        let (live_id, root) = insert_live_handoff_fixture(&live, live_dir.path()).await;
        install_handoff_custody_config_observation_for_test(live_id);
        resume_handoff_through_reconstruction_for_test(&live, live_id).await;
        assert_eq!(
            take_handoff_custody_config_for_test(live_id),
            Some((root.clone(), Some(root.join("target")))),
            "live Reuse handoff must use only its authenticated root and target"
        );
        assert!(live.completed.read().await.contains_key(&live_id));
        assert!(!live.active.read().await.contains_key(&live_id));
        assert!(handoff_custody_test_seams_are_clean(live_id));
    }

    #[tokio::test]
    async fn h1_v83_handoff_revalidation_refusal_restores_completed_before_mutation() {
        let (manager, dir) = rotation_manager();
        let (session_id, root) = insert_live_handoff_fixture(&manager, dir.path()).await;
        manager
            .register_agent_token("handoff-preexisting-token".to_string(), session_id)
            .await;
        install_handoff_custody_config_observation_for_test(session_id);
        install_handoff_custody_root_mutation_for_test(session_id);
        let invocation_before = handoff_invocation_state(&manager, session_id).await;
        resume_handoff_for_test(&manager, session_id).await;

        assert!(manager.completed.read().await.contains_key(&session_id));
        assert!(!manager.active.read().await.contains_key(&session_id));
        assert_eq!(
            manager
                .resolve_agent_token("handoff-preexisting-token")
                .await,
            Some(session_id),
            "ContextRead refusal must not mutate existing tokens"
        );
        assert!(take_handoff_custody_config_for_test(session_id).is_none());
        assert_eq!(
            handoff_invocation_state(&manager, session_id).await,
            invocation_before,
            "root-mutation refusal creates no invocation or prospective pointer"
        );
        assert!(handoff_custody_test_seams_are_clean(session_id));
        assert!(!root.exists());
        assert!(
            root.with_file_name(format!("{session_id}-handoff-raced"))
                .exists()
        );
    }

    #[tokio::test]
    async fn h1_v83_handoff_historical_and_partial_refuse_with_handoff_transition() {
        use rsi_common::types::SandboxCustodyTransitionV1;

        let (manager, dir) = rotation_manager();
        for (index, (cleanup, root)) in [
            (Some(SandboxCleanupState::Purged), None),
            (Some(SandboxCleanupState::Failed), None),
            (None, Some(dir.path().join("partial-handoff-root"))),
        ]
        .into_iter()
        .enumerate()
        {
            let session_id = Uuid::new_v4();
            let mut session = test_session(session_id, SessionStatus::Completed);
            session.working_dir = dir.path().to_path_buf();
            session.claude_session_id = Some(format!("historical-handoff-{index}"));
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = root;
            session.sandbox_cleanup_state = cleanup;
            manager
                .store
                .lock()
                .await
                .insert_session(&session)
                .expect("persist non-executable handoff fixture");
            manager
                .completed
                .write()
                .await
                .insert(session_id, CompletedSession::for_test(session.clone()));
            install_handoff_custody_config_observation_for_test(session_id);
            let error = manager
                .custody_execution_runtime()
                .prepare_handoff_resume(&session)
                .await
                .expect_err("historical handoff custody must refuse");
            assert_handoff_transition(&error);
            resume_handoff_for_test(&manager, session_id).await;
            assert!(manager.completed.read().await.contains_key(&session_id));
            assert!(take_handoff_custody_config_for_test(session_id).is_none());
            assert!(handoff_custody_test_seams_are_clean(session_id));
        }

        let intent = Uuid::new_v4().simple().to_string();
        let (project, assigned_idea, mut d03_parent, d03_token) =
            d03_rotation_fixture(&manager, dir.path(), &intent)
                .await
                .expect("build real D03 controller fixture");
        let d03_session_id = d03_parent.id;
        let d03_invocation_before = handoff_invocation_state(&manager, d03_session_id).await;
        d03_parent.sandbox_kind = Some(SandboxKind::GitWorktree);
        d03_parent.sandbox_root = Some(dir.path().join("partial-d03-handoff-root"));
        manager
            .completed
            .write()
            .await
            .insert(d03_session_id, CompletedSession::for_test(d03_parent));
        resume_handoff_for_test(&manager, d03_session_id).await;
        let store = manager.store.lock().await;
        let projection = store
            .load_idea_controller_projection_v1(project.id, assigned_idea.id)
            .expect("load D03 projection after refusal");
        assert_eq!(
            projection.current_controller_session_id,
            Some(d03_session_id)
        );
        assert!(
            store.controller_grant_v1(d03_session_id).is_some(),
            "pre-ContextRead refusal preserves the real D03 controller grant"
        );
        drop(store);
        assert_eq!(
            manager.resolve_agent_token(&d03_token).await,
            Some(d03_session_id)
        );
        assert_eq!(
            handoff_invocation_state(&manager, d03_session_id).await,
            d03_invocation_before,
            "D03 custody refusal preserves invocation count and current pointer"
        );
        let _ = SandboxCustodyTransitionV1::HandoffResume;
    }

    #[tokio::test]
    async fn h1_v83_handoff_transferred_predecessor_refuses_and_preserves_successor_owner() {
        let (manager, dir) = rotation_manager();
        let (predecessor_id, root) = insert_live_handoff_fixture(&manager, dir.path()).await;
        let predecessor = manager
            .completed
            .read()
            .await
            .get(&predecessor_id)
            .expect("completed predecessor")
            .session
            .clone();
        let live = manager
            .store
            .lock()
            .await
            .live_custody_for_session(predecessor_id)
            .expect("predecessor initially owns custody");
        let successor_id = Uuid::new_v4();
        let mut successor = test_session(successor_id, SessionStatus::Starting);
        successor.working_dir = predecessor.working_dir.clone();
        successor.continued_from = Some(predecessor_id);
        successor.sandbox_kind = predecessor.sandbox_kind;
        successor.sandbox_root = predecessor.sandbox_root.clone();
        successor.sandbox_branch = predecessor.sandbox_branch.clone();
        successor.sandbox_cleanup_state = predecessor.sandbox_cleanup_state;
        successor.git_branch = predecessor.git_branch.clone();
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&successor).expect("reserve successor");
            store
                .bind_reserved_session_custody(
                    successor_id,
                    crate::store::sandbox_custody::SessionCustodyBinding::Transfer {
                        custody_id: live.custody_id,
                        from_session_id: predecessor_id,
                        generation: live.generation,
                        cause: crate::store::sandbox_custody::CustodyCause::Rotation,
                        origin_session_id: Some(predecessor_id),
                        scheduled_job_id: None,
                    },
                )
                .expect("real ownership transfer");
        }
        let error = manager
            .custody_execution_runtime()
            .prepare_handoff_resume(&predecessor)
            .await
            .expect_err("transferred predecessor must refuse");
        assert!(error.to_string().contains("ownership_missing"));
        assert_handoff_transition(&error);
        install_handoff_custody_config_observation_for_test(predecessor_id);
        let invocation_before = handoff_invocation_state(&manager, predecessor_id).await;
        resume_handoff_for_test(&manager, predecessor_id).await;
        assert!(manager.completed.read().await.contains_key(&predecessor_id));
        assert!(!manager.active.read().await.contains_key(&predecessor_id));
        assert!(take_handoff_custody_config_for_test(predecessor_id).is_none());
        assert_eq!(
            handoff_invocation_state(&manager, predecessor_id).await,
            invocation_before,
            "transferred-predecessor refusal creates no invocation or prospective pointer"
        );
        assert!(handoff_custody_test_seams_are_clean(predecessor_id));
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|id| *id == predecessor_id)
        );
        let successor_live = manager
            .store
            .lock()
            .await
            .live_custody_for_session(successor_id)
            .expect("successor remains sole owner");
        assert_eq!(successor_live.owner_session_id, successor_id);
        assert!(
            root.exists(),
            "handoff refusal must preserve the retained root"
        );
    }

    async fn assert_rotation_monitor_panic_settlement(
        manager: &SessionManager,
        child_id: Uuid,
    ) -> anyhow::Result<()> {
        assert!(!manager.active.read().await.contains_key(&child_id));
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|session_id| *session_id == child_id)
        );
        let store = manager.store.lock().await;
        let child = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("panicked rotation child missing"))?;
        assert_eq!(child.status, SessionStatus::Failed);
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("panicked child invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            ("failed".into(), Some("rotation_monitor_panic".into()))
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_ordinary_monitor_panic_restores_exact_parent_and_authority()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        manager.store.lock().await.insert_session(&parent)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent_id)?;
        let event = ConversationEvent {
            id: 0,
            session_id: parent_id,
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: chrono::Utc::now(),
            content: "exact ordinary parent history".into(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let parent_completed = CompletedSession {
            session: parent.clone(),
            events: vec![event],
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        };
        let events_snapshot = serde_json::to_value(&parent_completed.events)?;
        let old_token = format!("ordinary-monitor-parent:{parent_id}");
        manager
            .register_agent_token(old_token.clone(), parent_id)
            .await;
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = parent.working_dir.clone();
        child.continued_from = Some(parent_id);
        child.model = Some("scripted-monitor-panic".into());
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let _stream_guard = ControllerCandidateTestStreamGuard(child_id);
        install_rotation_monitor_panic_for_test(child_id);
        let mut restoration_events = manager.event_bus.subscribe();
        spawn_rotation_child_for_test(&manager, parent_completed, child, candidate, None).await;

        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert_rotation_monitor_panic_settlement(&manager, child_id).await?;
        let completed = manager.completed.read().await;
        let restored = completed
            .get(&parent_id)
            .ok_or_else(|| anyhow::anyhow!("ordinary parent not restored"))?;
        let restored_session = serde_json::to_value(&restored.session)?;
        assert_eq!(serde_json::to_value(&restored.events)?, events_snapshot);
        drop(completed);
        assert_eq!(manager.resolve_agent_token(&old_token).await, None);
        let new_token = manager
            .agent_tokens
            .read()
            .await
            .token_for_session(parent_id)
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("restored parent has no reminted authority"))?;
        assert_ne!(new_token, old_token);
        assert_eq!(
            manager.resolve_agent_token(&new_token).await,
            Some(parent_id)
        );
        assert_eq!(
            manager
                .agent_tokens
                .read()
                .await
                .values()
                .filter(|session_id| **session_id == parent_id)
                .count(),
            1,
            "ordinary restoration must mint exactly one parent authority token"
        );
        let store = manager.store.lock().await;
        let durable_parent = store
            .get_session(parent_id)?
            .ok_or_else(|| anyhow::anyhow!("durably restored parent missing"))?;
        assert_eq!(serde_json::to_value(durable_parent)?, restored_session);
        let projection: (String, String, Option<String>, Option<i64>, Option<String>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,custody_id,custody_generation,error_code
                 FROM session_execution_projections WHERE session_id=?1",
                [child_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        assert_eq!(
            projection,
            (
                "ordinary_unsandboxed".into(),
                "verified".into(),
                None,
                None,
                None
            )
        );
        drop(store);
        let mut restored_publications = 0;
        loop {
            match restoration_events.try_recv() {
                Ok(event) => {
                    if matches!(
                        event.as_ref(),
                        DaemonEvent::SessionStatusChanged {
                            session_id,
                            old_status: SessionStatus::Archived,
                            new_status: SessionStatus::Completed,
                        } if *session_id == parent_id
                    ) {
                        restored_publications += 1;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(error) => return Err(anyhow::anyhow!("event audit failed: {error}")),
            }
        }
        assert_eq!(restored_publications, 1);
        manager.event_bus.unsubscribe();
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_7f3e_monitor_panic_restoration_failure_keeps_parent_non_authoritative()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        manager.store.lock().await.insert_session(&parent)?;
        manager
            .store
            .lock()
            .await
            .publish_startup_ordinary(parent_id)?;
        let old_token = format!("ordinary-restore-failure:{parent_id}");
        manager
            .register_agent_token(old_token.clone(), parent_id)
            .await;
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = parent.working_dir.clone();
        child.continued_from = Some(parent_id);
        child.model = Some("scripted-monitor-panic".into());
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let _stream_guard = ControllerCandidateTestStreamGuard(child_id);
        install_rotation_monitor_panic_for_test(child_id);
        let mut events = manager.event_bus.subscribe();
        let db_path = dir.path().join("rsi.db");
        let mut restoration_failure = SqliteRestorationFailureGuard::install(&db_path, parent_id)?;

        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(parent),
            child,
            candidate,
            None,
        )
        .await;

        restoration_failure.remove()?;
        let trigger_count: i64 = rusqlite::Connection::open(&db_path)?.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name=?1",
            [&restoration_failure.trigger_name],
            |row| row.get(0),
        )?;
        assert_eq!(trigger_count, 0, "UUID-scoped restoration trigger leaked");
        assert!(
            !rotation_monitor_panics_for_test()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&child_id),
            "UUID-scoped monitor panic control leaked"
        );
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert_rotation_monitor_panic_settlement(&manager, child_id).await?;
        assert!(!manager.completed.read().await.contains_key(&parent_id));
        assert_eq!(manager.resolve_agent_token(&old_token).await, None);
        let tokens = manager.agent_tokens.read().await;
        assert!(!tokens.values().any(|session_id| *session_id == parent_id));
        assert!(!tokens.values().any(|session_id| *session_id == child_id));
        drop(tokens);

        let store = manager.store.lock().await;
        let durable_parent = store
            .get_session(parent_id)?
            .ok_or_else(|| anyhow::anyhow!("archived parent missing after restoration failure"))?;
        assert_eq!(durable_parent.status, SessionStatus::Archived);
        assert!(store.controller_grant_v1(child_id).is_none());
        let controller_reservations: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM idea_events WHERE event_type='controller_reserved'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(controller_reservations, 0);
        let custody_roots: i64 =
            store
                .conn
                .query_row("SELECT COUNT(*) FROM sandbox_custody_roots", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(
            custody_roots, 0,
            "ordinary rollback must not create root audit state"
        );
        drop(store);

        loop {
            match events.try_recv() {
                Ok(event) => assert!(
                    !matches!(
                        event.as_ref(),
                        DaemonEvent::SessionStatusChanged {
                            session_id,
                            old_status: SessionStatus::Archived,
                            new_status: SessionStatus::Completed,
                        } if *session_id == parent_id
                    ),
                    "durable restoration failure must not publish restored status"
                ),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(error) => return Err(anyhow::anyhow!("event audit failed: {error}")),
            }
        }
        manager.event_bus.unsubscribe();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_7f3f_monitor_panic_restoration_refuses_changed_archived_parent()
    -> anyhow::Result<()> {
        for mutation in [
            RotationPreRestoreMutationForTest::Status,
            RotationPreRestoreMutationForTest::Authority,
        ] {
            let (manager, dir) = rotation_manager();
            let parent_id = Uuid::new_v4();
            let child_id = Uuid::new_v4();
            let mut parent = test_session(parent_id, SessionStatus::Completed);
            parent.working_dir = dir.path().to_path_buf();
            manager.store.lock().await.insert_session(&parent)?;
            manager
                .store
                .lock()
                .await
                .publish_startup_ordinary(parent_id)?;
            manager
                .register_agent_token(format!("pre-restore-race:{parent_id}"), parent_id)
                .await;
            let candidate = manager
                .custody_execution_runtime()
                .prepare_rotation_successor(&parent)
                .await?;
            let mut child = test_session(child_id, SessionStatus::Starting);
            child.working_dir = parent.working_dir.clone();
            child.continued_from = Some(parent_id);
            child.model = Some("scripted-monitor-panic".into());
            let scripted =
                super::super::launch::install_controller_candidate_test_process(child_id);
            let _stream_guard = ControllerCandidateTestStreamGuard(child_id);
            install_rotation_monitor_panic_for_test(child_id);
            install_rotation_pre_restore_mutation_for_test(child_id, mutation);
            let mut events = manager.event_bus.subscribe();

            spawn_rotation_child_for_test(
                &manager,
                CompletedSession::for_test(parent),
                child,
                candidate,
                None,
            )
            .await;

            assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
            assert_rotation_monitor_panic_settlement(&manager, child_id).await?;
            assert!(!manager.completed.read().await.contains_key(&parent_id));
            assert!(
                !manager
                    .agent_tokens
                    .read()
                    .await
                    .values()
                    .any(|session_id| *session_id == parent_id)
            );
            let durable = manager
                .store
                .lock()
                .await
                .get_session(parent_id)?
                .ok_or_else(|| anyhow::anyhow!("mutated archived predecessor missing"))?;
            match mutation {
                RotationPreRestoreMutationForTest::Status => {
                    assert_eq!(durable.status, SessionStatus::Interrupted);
                }
                RotationPreRestoreMutationForTest::Authority => {
                    assert_eq!(durable.status, SessionStatus::Archived);
                    assert_eq!(
                        durable.active_task.as_deref(),
                        Some("substituted archived prompt authority")
                    );
                }
            }
            assert!(
                !rotation_pre_restore_mutations()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&child_id)
            );
            loop {
                match events.try_recv() {
                    Ok(event) => assert!(
                        !matches!(
                            event.as_ref(),
                            DaemonEvent::SessionStatusChanged {
                                session_id,
                                old_status: SessionStatus::Archived,
                                new_status: SessionStatus::Completed,
                            } if *session_id == parent_id
                        ),
                        "stale archived predecessor must publish no restored authority"
                    ),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(error) => return Err(anyhow::anyhow!("event audit failed: {error}")),
                }
            }
            manager.event_bus.unsubscribe();
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_live_monitor_panic_is_forward_only_and_retains_worktree()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        let parent_id = fixture.parent.id;
        let child_id = Uuid::new_v4();
        std::fs::write(fixture.root.join("monitor-sentinel"), "retained")?;
        manager
            .register_agent_token(format!("live-monitor-parent:{parent_id}"), parent_id)
            .await;
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = fixture.repo.clone();
        child.continued_from = Some(parent_id);
        child.model = Some("scripted-monitor-panic".into());
        crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
            &candidate,
            &fixture.parent,
            &mut child,
        )?;
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        install_rotation_monitor_panic_for_test(child_id);
        spawn_rotation_child_for_test(
            &manager,
            CompletedSession::for_test(fixture.parent.clone()),
            child,
            candidate,
            None,
        )
        .await;

        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert_rotation_monitor_panic_settlement(&manager, child_id).await?;
        assert!(!manager.completed.read().await.contains_key(&parent_id));
        let tokens = manager.agent_tokens.read().await;
        assert!(!tokens.values().any(|session_id| *session_id == parent_id));
        assert!(!tokens.values().any(|session_id| *session_id == child_id));
        drop(tokens);
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("monitor-sentinel"))?,
            "retained"
        );
        let store = manager.store.lock().await;
        let root: (String, i64, i64, String, String) = store.conn.query_row(
            "SELECT owner_session_id,generation,event_sequence,state,validation_state
             FROM sandbox_custody_roots WHERE custody_id=?1",
            [fixture.custody_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        assert_eq!(
            root,
            (child_id.to_string(), 2, 2, "live".into(), "verified".into())
        );
        let child_projection: (String, String, Option<String>, Option<i64>, Option<String>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,custody_id,custody_generation,error_code
                 FROM session_execution_projections WHERE session_id=?1",
                [child_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        assert_eq!(
            child_projection,
            (
                "live_sandboxed".into(),
                "verified".into(),
                Some(fixture.custody_id.to_string()),
                Some(2),
                None,
            )
        );
        let predecessor_projection: (String, String, Option<String>) = store.conn.query_row(
            "SELECT execution_state,freshness,effective_cwd
             FROM session_execution_projections WHERE session_id=?1",
            [parent_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            predecessor_projection,
            ("historical_transferred".into(), "verified".into(), None)
        );
        let backward: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sandbox_custody_events
             WHERE custody_id=?1 AND event_kind='transferred'
               AND from_owner_session_id=?2 AND to_owner_session_id=?3",
            rusqlite::params![
                fixture.custody_id.to_string(),
                child_id.to_string(),
                parent_id.to_string(),
            ],
            |row| row.get(0),
        )?;
        assert_eq!(backward, 0);
        drop(store);
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    #[tokio::test]
    async fn h1_v83_rotation_custody_7f3g_exact_identity_restoration_preserves_history() {
        let (manager, dir) = rotation_manager();
        let session_id = Uuid::new_v4();
        let mut parent = test_session(session_id, SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        let bound = bind_and_archive_ordinary_rotation_for_test(&manager, &parent)
            .await
            .unwrap();

        let mut rx = manager.event_bus.subscribe();

        let event = ConversationEvent {
            id: 1,
            session_id,
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: chrono::Utc::now(),
            content: "handoff history".to_string(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let parent_completed = CompletedSession {
            session: parent,
            events: vec![event.clone()],
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        };

        let restoration = SessionManager::restore_archived_parent_after_child_failure(
            session_id,
            Some(parent_completed),
            &manager.completed,
            &manager.event_bus,
            &bound,
            &manager.custody_execution_runtime(),
            &manager.agent_tokens,
        )
        .await;
        assert_eq!(restoration, ArchivedParentRestoration::AuthorityPublished);

        let restored = manager.completed.read().await;
        let restored_parent = restored.get(&session_id).unwrap();
        assert_eq!(restored_parent.session.status, SessionStatus::Completed);
        assert!(!restored_parent.session.pending_archive);
        assert_eq!(restored_parent.events.len(), 1);
        assert_eq!(restored_parent.events[0].content, event.content);
        drop(restored);

        let durable = manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .unwrap()
            .expect("durable restored parent");
        assert_eq!(durable.id, session_id);
        assert_eq!(durable.status, SessionStatus::Completed);
        assert!(
            manager
                .agent_tokens
                .read()
                .await
                .token_for_session(session_id)
                .is_some()
        );

        let bus_event = rx.try_recv().unwrap();
        match bus_event.as_ref() {
            DaemonEvent::SessionStatusChanged {
                session_id: sid,
                old_status,
                new_status,
            } => {
                assert_eq!(*sid, session_id);
                assert_eq!(*old_status, SessionStatus::Archived);
                assert_eq!(*new_status, SessionStatus::Completed);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "exact restoration publishes one event"
        );

        manager.event_bus.unsubscribe();
    }

    #[tokio::test]
    async fn h1_v83_rotation_custody_7f3g_caller_identity_mismatch_refuses_before_store_mutation()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let caller_a = Uuid::new_v4();
        let parent_b = Uuid::new_v4();
        let mut parent = test_session(parent_b, SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        let bound = bind_and_archive_ordinary_rotation_for_test(&manager, &parent).await?;
        let mut rx = manager.event_bus.subscribe();

        let restoration = SessionManager::restore_archived_parent_after_child_failure(
            caller_a,
            None,
            &manager.completed,
            &manager.event_bus,
            &bound,
            &manager.custody_execution_runtime(),
            &manager.agent_tokens,
        )
        .await;
        assert_eq!(restoration, ArchivedParentRestoration::DurableRestoreFailed);
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_session(parent_b)?
                .ok_or_else(|| anyhow::anyhow!("archived B missing"))?
                .status,
            SessionStatus::Archived
        );
        let completed = manager.completed.read().await;
        assert!(!completed.contains_key(&caller_a));
        assert!(!completed.contains_key(&parent_b));
        drop(completed);
        let tokens = manager.agent_tokens.read().await;
        assert!(tokens.token_for_session(caller_a).is_none());
        assert!(tokens.token_for_session(parent_b).is_none());
        drop(tokens);
        assert!(
            rx.try_recv().is_err(),
            "identity refusal publishes no event"
        );
        manager.event_bus.unsubscribe();
        Ok(())
    }

    #[tokio::test]
    async fn h1_v83_rotation_custody_7f3g_retained_history_identity_mismatch_refuses_before_store_mutation()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let retained_a = Uuid::new_v4();
        let parent_b = Uuid::new_v4();
        let mut parent = test_session(parent_b, SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        let bound = bind_and_archive_ordinary_rotation_for_test(&manager, &parent).await?;
        let mut retained = test_session(retained_a, SessionStatus::Completed);
        retained.working_dir = dir.path().to_path_buf();
        let mut rx = manager.event_bus.subscribe();

        let restoration = SessionManager::restore_archived_parent_after_child_failure(
            parent_b,
            Some(CompletedSession::for_test(retained)),
            &manager.completed,
            &manager.event_bus,
            &bound,
            &manager.custody_execution_runtime(),
            &manager.agent_tokens,
        )
        .await;
        assert_eq!(restoration, ArchivedParentRestoration::DurableRestoreFailed);
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_session(parent_b)?
                .ok_or_else(|| anyhow::anyhow!("archived B missing"))?
                .status,
            SessionStatus::Archived
        );
        let completed = manager.completed.read().await;
        assert!(!completed.contains_key(&retained_a));
        assert!(!completed.contains_key(&parent_b));
        drop(completed);
        let tokens = manager.agent_tokens.read().await;
        assert!(tokens.token_for_session(retained_a).is_none());
        assert!(tokens.token_for_session(parent_b).is_none());
        drop(tokens);
        assert!(rx.try_recv().is_err(), "history refusal publishes no event");
        manager.event_bus.unsubscribe();
        Ok(())
    }

    #[tokio::test]
    async fn h1_v83_rotation_custody_7f3e_fallback_restoration_uses_acknowledged_durable_row()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let parent_id = Uuid::new_v4();
        let mut parent = test_session(parent_id, SessionStatus::Completed);
        parent.working_dir = dir.path().to_path_buf();
        parent.title = Some("durable fallback parent".into());
        let bound = bind_and_archive_ordinary_rotation_for_test(&manager, &parent).await?;

        let restoration = SessionManager::restore_archived_parent_after_child_failure(
            parent_id,
            None,
            &manager.completed,
            &manager.event_bus,
            &bound,
            &manager.custody_execution_runtime(),
            &manager.agent_tokens,
        )
        .await;
        assert_eq!(restoration, ArchivedParentRestoration::AuthorityPublished);
        let durable = manager
            .store
            .lock()
            .await
            .get_session(parent_id)?
            .ok_or_else(|| anyhow::anyhow!("fallback durable parent missing"))?;
        let completed = manager.completed.read().await;
        let restored = completed
            .get(&parent_id)
            .ok_or_else(|| anyhow::anyhow!("fallback completed parent missing"))?;
        assert_eq!(
            serde_json::to_value(&restored.session)?,
            serde_json::to_value(&durable)?
        );
        assert_eq!(durable.status, SessionStatus::Completed);
        assert!(!durable.pending_archive);
        drop(completed);
        assert!(
            manager
                .agent_tokens
                .read()
                .await
                .token_for_session(parent_id)
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn d03_controller_rotation_uses_effective_provider_identity_for_new_child() {
        assert_eq!(
            super::super::provider_spawn::effective_sync_provider(SessionProvider::CodexAppServer),
            SessionProvider::Codex
        );
        for provider in [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::Pioneer,
            SessionProvider::Local,
            SessionProvider::Antigravity,
            SessionProvider::Harness,
        ] {
            assert_eq!(
                super::super::provider_spawn::effective_sync_provider(provider),
                provider
            );
        }
    }

    async fn assert_d03_cancelled_rotation_state(
        manager: &SessionManager,
        project_id: Uuid,
        assigned_idea: &rsi_common::types::Idea,
        parent_id: Uuid,
        parent_token: &str,
        reservation_id: Uuid,
        child_id: Uuid,
    ) -> anyhow::Result<()> {
        let store = manager.store.lock().await;
        let projection = store.load_idea_controller_projection_v1(project_id, assigned_idea.id)?;
        assert_eq!(projection.current_controller_session_id, Some(parent_id));
        assert_eq!(projection.controller_epoch, assigned_idea.controller_epoch);
        assert_eq!(projection.row_version, assigned_idea.row_version + 2);
        let page = store.list_idea_events_v1(
            project_id,
            assigned_idea.id,
            IdeaEventPageRequestV1 {
                after_sequence: 0,
                limit: Some(16),
            },
        )?;
        let tail = page
            .events
            .last()
            .ok_or_else(|| anyhow::anyhow!("rotation cancellation tail missing"))?
            .controller_control_payload_v1()
            .map_err(anyhow::Error::msg)?;
        assert!(matches!(
            tail.request.operation,
            IdeaControllerControlOperationV1::ReleaseReservation {
                reservation,
                reason: ControllerReleaseReasonV1::Cancelled,
                ..
            } if reservation.reservation_id == reservation_id
                && reservation.candidate_session_id == child_id
        ));
        assert!(store.controller_grant_v1(parent_id).is_some());
        assert!(store.controller_grant_v1(child_id).is_none());
        drop(store);
        assert_eq!(
            manager.resolve_agent_token(parent_token).await,
            Some(parent_id)
        );
        assert!(
            !manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|session_id| *session_id == child_id)
        );
        assert!(manager.completed.read().await.contains_key(&parent_id));
        Ok(())
    }

    fn assert_rotation_controller_release(
        store: &Store,
        project_id: Uuid,
        idea_id: Uuid,
        reservation_id: Uuid,
        child_id: Uuid,
        expected_reason: ControllerReleaseReasonV1,
    ) -> anyhow::Result<()> {
        let events = store.list_idea_events_v1(
            project_id,
            idea_id,
            IdeaEventPageRequestV1 {
                after_sequence: 0,
                limit: Some(32),
            },
        )?;
        let tail = events
            .events
            .last()
            .ok_or_else(|| anyhow::anyhow!("controller release event missing"))?
            .controller_control_payload_v1()
            .map_err(anyhow::Error::msg)?;
        assert!(matches!(
            tail.request.operation,
            IdeaControllerControlOperationV1::ReleaseReservation {
                reservation,
                reason,
                ..
            } if reservation.reservation_id == reservation_id
                && reservation.candidate_session_id == child_id
                && reason == expected_reason
        ));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_controller_confirmation_failure_matrix_is_forward_only()
    -> anyhow::Result<()> {
        let (ordinary, ordinary_dir) = rotation_manager();
        let intent = Uuid::new_v4().simple().to_string();
        let (project, idea, parent, parent_token) =
            d03_rotation_fixture(&ordinary, ordinary_dir.path(), &intent).await?;
        let parent_id = parent.id;
        let parent_snapshot = serde_json::to_value(&parent)?;
        let rotation_id = format!("h1-v83-confirmation-ordinary:{intent}");
        let reservation_id =
            idea_controller_reservation_id(idea.id, &format!("rotation:{parent_id}:{rotation_id}"));
        let child_id = idea_controller_candidate_session_id(reservation_id);
        let candidate = ordinary
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = parent.working_dir.clone();
        child.project_id = Some(project.id);
        child.continued_from = Some(parent_id);
        child.model = Some("dead-installed-provider".into());
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        scripted
            .alive
            .store(false, std::sync::atomic::Ordering::SeqCst);
        spawn_rotation_child_for_test(
            &ordinary,
            CompletedSession::for_test(parent),
            child,
            candidate,
            Some(rotation_id),
        )
        .await;
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        let restored = ordinary.completed.read().await;
        assert_eq!(
            serde_json::to_value(
                &restored
                    .get(&parent_id)
                    .ok_or_else(|| anyhow::anyhow!("ordinary parent not restored"))?
                    .session
            )?,
            parent_snapshot
        );
        drop(restored);
        assert_eq!(
            ordinary.resolve_agent_token(&parent_token).await,
            Some(parent_id)
        );
        let store = ordinary.store.lock().await;
        let failed = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("confirmation-failed child missing"))?;
        assert_eq!(failed.status, SessionStatus::Failed);
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("confirmation-failed invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            (
                "failed".into(),
                Some("controller_confirmation_failed".into())
            )
        );
        assert_rotation_controller_release(
            &store,
            project.id,
            idea.id,
            reservation_id,
            child_id,
            ControllerReleaseReasonV1::ConfirmationFailed,
        )?;
        drop(store);
        assert!(!ordinary.active.read().await.contains_key(&child_id));
        assert!(
            !ordinary
                .agent_tokens
                .read()
                .await
                .values()
                .any(|session_id| *session_id == child_id)
        );
        super::super::launch::drop_controller_candidate_test_stream(child_id);

        let (live, live_dir) = rotation_manager();
        let live_intent = Uuid::new_v4().simple().to_string();
        let (project, idea, fixture, _parent_token) =
            d03_live_rotation_fixture(&live, live_dir.path(), &live_intent).await?;
        let parent_id = fixture.parent.id;
        let rotation_id = format!("h1-v83-confirmation-live:{live_intent}");
        let reservation_id =
            idea_controller_reservation_id(idea.id, &format!("rotation:{parent_id}:{rotation_id}"));
        let child_id = idea_controller_candidate_session_id(reservation_id);
        let candidate = live
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = fixture.repo.clone();
        child.project_id = Some(project.id);
        child.continued_from = Some(parent_id);
        child.model = Some("dead-installed-provider".into());
        crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
            &candidate,
            &fixture.parent,
            &mut child,
        )?;
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        scripted
            .alive
            .store(false, std::sync::atomic::Ordering::SeqCst);
        spawn_rotation_child_for_test(
            &live,
            CompletedSession::for_test(fixture.parent),
            child,
            candidate,
            Some(rotation_id),
        )
        .await;
        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!live.completed.read().await.contains_key(&parent_id));
        let tokens = live.agent_tokens.read().await;
        assert!(!tokens.values().any(|session_id| *session_id == parent_id));
        assert!(!tokens.values().any(|session_id| *session_id == child_id));
        drop(tokens);
        let store = live.store.lock().await;
        let root: (String, i64, i64) = store.conn.query_row(
            "SELECT owner_session_id,generation,event_sequence
             FROM sandbox_custody_roots WHERE custody_id=?1",
            [fixture.custody_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(root, (child_id.to_string(), 2, 2));
        let failed = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("live confirmation-failed child missing"))?;
        assert_eq!(failed.status, SessionStatus::Failed);
        let predecessor_projection: (String, String, Option<String>) = store.conn.query_row(
            "SELECT execution_state,freshness,effective_cwd
             FROM session_execution_projections WHERE session_id=?1",
            [parent_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            predecessor_projection,
            ("historical_transferred".into(), "verified".into(), None)
        );
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("live confirmation invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            (
                "failed".into(),
                Some("controller_confirmation_failed".into())
            )
        );
        assert_rotation_controller_release(
            &store,
            project.id,
            idea.id,
            reservation_id,
            child_id,
            ControllerReleaseReasonV1::ConfirmationFailed,
        )?;
        let backward: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sandbox_custody_events
             WHERE custody_id=?1 AND event_kind='transferred'
               AND from_owner_session_id=?2 AND to_owner_session_id=?3",
            rusqlite::params![
                fixture.custody_id.to_string(),
                child_id.to_string(),
                parent_id.to_string(),
            ],
            |row| row.get(0),
        )?;
        assert_eq!(backward, 0);
        drop(store);
        assert!(!live.active.read().await.contains_key(&child_id));
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_ordinary_controller_cancellation_after_confirmation_preserves_parent()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let intent = Uuid::new_v4().simple().to_string();
        let (project, assigned_idea, parent, parent_token) =
            d03_rotation_fixture(&manager, dir.path(), &intent).await?;
        let parent_id = parent.id;
        let parent_snapshot = serde_json::to_value(&parent)?;
        let rotation_id = format!("d03-rotation-cancel:{intent}");
        let transfer_intent_key = format!("rotation:{parent_id}:{rotation_id}");
        let reservation_id = idea_controller_reservation_id(assigned_idea.id, &transfer_intent_key);
        let child_id = idea_controller_candidate_session_id(reservation_id);
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            child_id,
            super::super::launch::ControllerCandidateTestPhase::AfterConfirmation,
        );
        let mut child = test_session(Uuid::new_v4(), SessionStatus::Starting);
        child.working_dir = dir.path().to_path_buf();
        child.project_id = Some(project.id);
        child.provider = SessionProvider::Claude;
        child.continued_from = Some(parent_id);
        child.model = Some("d03-scripted-rotation".to_string());
        let parent_for_archival = CompletedSession {
            session: parent,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        };
        let rotation_candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&parent_for_archival.session)
            .await
            .expect("authenticate ordinary rotation parent");
        let mut events = manager.event_bus().subscribe();

        let rotation = SessionManager::spawn_rotation_child(
            parent_id,
            None,
            "D03 rotation cancellation".to_string(),
            child,
            rotation_candidate,
            Some(parent_for_archival),
            Some(rotation_id),
            None,
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager.model_call_settlements.handle()?,
            manager.persistence.clone(),
            false,
            manager.socket_path.clone(),
            Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            Arc::clone(&manager.runtime_config),
            Arc::clone(&manager.spawn_coordinator),
            Arc::clone(&manager.agent_tokens),
            Arc::clone(&manager.spawn_epoch),
            Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
            None,
            None,
        );
        let interrupt = async {
            reached.await?;
            manager.interrupt_session(child_id).await?;
            resume
                .send(())
                .map_err(|()| anyhow::anyhow!("resume rotation assignment receiver dropped"))?;
            Ok::<(), anyhow::Error>(())
        };
        let ((), interrupt_result) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(rotation, interrupt)
            })
            .await?;
        interrupt_result?;

        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            scripted
                .interrupt_count
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 1
        );
        assert_d03_cancelled_rotation_state(
            &manager,
            project.id,
            &assigned_idea,
            parent_id,
            &parent_token,
            reservation_id,
            child_id,
        )
        .await?;
        let restored = manager.completed.read().await;
        assert_eq!(
            serde_json::to_value(
                &restored
                    .get(&parent_id)
                    .ok_or_else(|| anyhow::anyhow!("cancelled ordinary parent missing"))?
                    .session
            )?,
            parent_snapshot
        );
        assert!(
            restored
                .get(&parent_id)
                .expect("cancelled ordinary parent")
                .events
                .is_empty()
        );
        drop(restored);
        let store = manager.store.lock().await;
        let failed = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("cancelled ordinary child missing"))?;
        assert_eq!(failed.status, SessionStatus::Failed);
        let projection: (String, String, Option<String>, Option<i64>, Option<String>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,custody_id,custody_generation,error_code
                 FROM session_execution_projections WHERE session_id=?1",
                [child_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        assert_eq!(
            projection,
            (
                "ordinary_unsandboxed".into(),
                "verified".into(),
                None,
                None,
                None,
            )
        );
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("cancelled ordinary invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            ("failed".into(), Some("controller_assignment_failed".into()))
        );
        drop(store);
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(
                    event.as_ref(),
                    DaemonEvent::SystemMessage { message, .. }
                        if message.starts_with("idea_controller_assigned:")
                ),
                "cancelled rotation must not publish assignment: {event:?}"
            );
        }
        manager.event_bus().unsubscribe();
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h1_v83_rotation_custody_live_controller_cancellation_after_confirmation_is_forward_only()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let intent = Uuid::new_v4().simple().to_string();
        let (project, assigned_idea, fixture, _parent_token) =
            d03_live_rotation_fixture(&manager, dir.path(), &intent).await?;
        let parent_id = fixture.parent.id;
        std::fs::write(fixture.root.join("controller-cancel-sentinel"), "retained")?;
        let rotation_id = format!("h1-v83-live-rotation-cancel:{intent}");
        let reservation_id = idea_controller_reservation_id(
            assigned_idea.id,
            &format!("rotation:{parent_id}:{rotation_id}"),
        );
        let child_id = idea_controller_candidate_session_id(reservation_id);
        let scripted = super::super::launch::install_controller_candidate_test_process(child_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            child_id,
            super::super::launch::ControllerCandidateTestPhase::AfterConfirmation,
        );
        let candidate = manager
            .custody_execution_runtime()
            .prepare_rotation_successor(&fixture.parent)
            .await?;
        let mut child = test_session(child_id, SessionStatus::Starting);
        child.working_dir = fixture.repo.clone();
        child.project_id = Some(project.id);
        child.provider = SessionProvider::Claude;
        child.continued_from = Some(parent_id);
        child.model = Some("d03-scripted-live-rotation".into());
        crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
            &candidate,
            &fixture.parent,
            &mut child,
        )?;
        let parent_for_archival = CompletedSession::for_test(fixture.parent);
        let rotation = SessionManager::spawn_rotation_child(
            parent_id,
            None,
            "live D03 rotation cancellation".into(),
            child,
            candidate,
            Some(parent_for_archival),
            Some(rotation_id),
            None,
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager.model_call_settlements.handle()?,
            manager.persistence.clone(),
            false,
            manager.socket_path.clone(),
            Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            Arc::clone(&manager.runtime_config),
            Arc::clone(&manager.spawn_coordinator),
            Arc::clone(&manager.agent_tokens),
            Arc::clone(&manager.spawn_epoch),
            Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
            None,
            None,
        );
        let interrupt = async {
            reached.await?;
            manager.interrupt_session(child_id).await?;
            resume.send(()).map_err(|()| {
                anyhow::anyhow!("resume live rotation assignment receiver dropped")
            })?;
            Ok::<(), anyhow::Error>(())
        };
        let ((), interrupt_result) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(rotation, interrupt)
            })
            .await?;
        interrupt_result?;

        assert!(!scripted.alive.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            scripted
                .interrupt_count
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 1
        );
        assert!(!manager.active.read().await.contains_key(&child_id));
        assert!(!manager.completed.read().await.contains_key(&parent_id));
        let tokens = manager.agent_tokens.read().await;
        assert!(!tokens.values().any(|session_id| *session_id == parent_id));
        assert!(!tokens.values().any(|session_id| *session_id == child_id));
        drop(tokens);
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("controller-cancel-sentinel"))?,
            "retained"
        );
        let store = manager.store.lock().await;
        let failed = store
            .get_session(child_id)?
            .ok_or_else(|| anyhow::anyhow!("cancelled live child missing"))?;
        assert_eq!(failed.status, SessionStatus::Failed);
        let root: (String, i64, i64) = store.conn.query_row(
            "SELECT owner_session_id,generation,event_sequence
             FROM sandbox_custody_roots WHERE custody_id=?1",
            [fixture.custody_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(root, (child_id.to_string(), 2, 2));
        let projection: (String, String, Option<String>, Option<i64>, Option<String>) =
            store.conn.query_row(
                "SELECT execution_state,freshness,custody_id,custody_generation,error_code
                 FROM session_execution_projections WHERE session_id=?1",
                [child_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        assert_eq!(
            projection,
            (
                "live_sandboxed".into(),
                "verified".into(),
                Some(fixture.custody_id.to_string()),
                Some(2),
                None,
            )
        );
        let invocation_id = store
            .session_model_invocation_id(child_id)?
            .ok_or_else(|| anyhow::anyhow!("cancelled live invocation missing"))?;
        let invocation: (String, Option<String>) = store.conn.query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            invocation,
            ("failed".into(), Some("controller_assignment_failed".into()))
        );
        assert_rotation_controller_release(
            &store,
            project.id,
            assigned_idea.id,
            reservation_id,
            child_id,
            ControllerReleaseReasonV1::Cancelled,
        )?;
        let backward: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sandbox_custody_events
             WHERE custody_id=?1 AND event_kind='transferred'
               AND from_owner_session_id=?2 AND to_owner_session_id=?3",
            rusqlite::params![
                fixture.custody_id.to_string(),
                child_id.to_string(),
                parent_id.to_string(),
            ],
            |row| row.get(0),
        )?;
        assert_eq!(backward, 0);
        drop(store);
        super::super::launch::drop_controller_candidate_test_stream(child_id);
        Ok(())
    }
}
