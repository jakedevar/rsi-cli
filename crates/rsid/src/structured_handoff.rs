//! Schema-validated topology handoffs, daemon side (#2759, gap G08 of #2748).
//!
//! The wire pieces (the schema, the payload-to-text bridge) are in
//! `rsi_common::structured_handoff`. This module owns the three daemon facts:
//!
//! * which session's next provider turn is the handoff turn, so the Claude
//!   (`--json-schema`) and Codex (`--output-schema` + `-o`) launch builders add
//!   the flags on that turn and on no other (`arm` / `take_for_launch` /
//!   `finish_turn`);
//! * the Codex schema and last-message files, which live in the session's
//!   execution scratch directory and are removed when the turn is observed;
//! * the `handoff_observed_total` / `handoff_invalid_total` counters, by
//!   provider and whether the turn was structured, read by `AgentGetDaemonInfo`.
//!
//! A turn is the handoff turn when the topology executor opens it to obtain a
//! repeated handoff (the #2584 format continuation): the daemon cannot tell
//! ahead of time which ordinary turn will be a node's last, and a schema on a
//! turn that parks for a wake or a job would force a handoff before the work is
//! done. State is in memory: a restart between arming and launch simply sends
//! the continuation as free text, the behaviour before this feature.

use rsi_common::agent_daemon_info::{
    HANDOFF_INVALID_TOTAL, HANDOFF_OBSERVED_TOTAL, HandoffCounterV1,
};
use rsi_common::types::SessionProvider;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The next provider launch of the session is the handoff turn.
    Armed,
    /// A provider turn launched with the schema; its message is not read yet.
    Launched,
}

struct Turn {
    phase: Phase,
    /// Directory holding the Codex schema and last-message files.
    files: Option<PathBuf>,
}

#[derive(Default)]
struct State {
    turns: HashMap<Uuid, Turn>,
    counters: BTreeMap<(&'static str, String, bool), u64>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Mark `session_id`'s next provider launch as its handoff turn.
pub(crate) fn arm(session_id: Uuid) {
    let mut state = state();
    let previous = state.turns.insert(
        session_id,
        Turn {
            phase: Phase::Armed,
            files: None,
        },
    );
    if let Some(files) = previous.and_then(|turn| turn.files) {
        remove_dir(&files);
    }
}

/// The continuation was not delivered: the turn stays free text.
pub(crate) fn disarm(session_id: Uuid) {
    if let Some(turn) = state().turns.remove(&session_id)
        && let Some(files) = turn.files
    {
        remove_dir(&files);
    }
}

/// A provider launch is being built for `session_id`: `true` exactly once per
/// [`arm`], and the launch must carry the schema. A turn that is already
/// `Launched` is stale (its message was never observed); it is forgotten and
/// this launch is an ordinary one.
pub(crate) fn take_for_launch(session_id: Uuid) -> bool {
    let mut state = state();
    match state.turns.get_mut(&session_id) {
        Some(turn) if turn.phase == Phase::Armed => {
            turn.phase = Phase::Launched;
            true
        }
        Some(_) => {
            if let Some(files) = state.turns.remove(&session_id).and_then(|turn| turn.files) {
                remove_dir(&files);
            }
            false
        }
        None => false,
    }
}

/// The handoff of `session_id` is being read: `true` when the turn that wrote
/// it carried the schema. Removes the turn's files.
pub(crate) fn finish_turn(session_id: Uuid) -> bool {
    match state().turns.remove(&session_id) {
        Some(turn) => {
            if let Some(files) = turn.files {
                remove_dir(&files);
            }
            turn.phase == Phase::Launched
        }
        None => false,
    }
}

/// The files a Codex handoff turn is given.
pub(crate) struct CodexHandoffFiles {
    pub(crate) schema: PathBuf,
    pub(crate) last_message: PathBuf,
}

/// Write the schema for `session_id`'s Codex handoff turn into `scratch_temp`
/// (the session's execution scratch `TMPDIR`) and name the last-message file
/// beside it. The directory is removed by [`finish_turn`], [`disarm`] or the
/// next [`arm`]; the scratch itself is reclaimed with the sandbox.
pub(crate) fn prepare_codex_files(
    scratch_temp: &Path,
    session_id: Uuid,
) -> std::io::Result<CodexHandoffFiles> {
    let dir = scratch_temp.join(format!("structured-handoff-{session_id}"));
    if dir.exists() {
        remove_dir(&dir);
    }
    std::fs::create_dir_all(&dir)?;
    let schema = dir.join("schema.json");
    std::fs::write(
        &schema,
        rsi_common::structured_handoff::structured_handoff_schema_text(),
    )?;
    if let Some(turn) = state().turns.get_mut(&session_id) {
        turn.files = Some(dir.clone());
    }
    Ok(CodexHandoffFiles {
        schema,
        last_message: dir.join("last-message.json"),
    })
}

fn remove_dir(dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), %error, "structured handoff scratch files not removed");
    }
}

fn bump(name: &'static str, provider: Option<SessionProvider>, structured: bool) {
    let label = provider.map_or_else(|| "Unknown".to_string(), |provider| format!("{provider:?}"));
    *state()
        .counters
        .entry((name, label, structured))
        .or_insert(0) += 1;
}

/// A finished node's last message was read as a handoff (valid or not).
pub(crate) fn record_observed(provider: Option<SessionProvider>, structured: bool) {
    bump(HANDOFF_OBSERVED_TOTAL, provider, structured);
}

/// The handoff parser refused a finished node's last message.
pub(crate) fn record_invalid(provider: Option<SessionProvider>, structured: bool) {
    bump(HANDOFF_INVALID_TOTAL, provider, structured);
}

/// Every counter that has been bumped, in a stable order.
pub(crate) fn counters() -> Vec<HandoffCounterV1> {
    state()
        .counters
        .iter()
        .map(|((name, provider, structured), value)| HandoffCounterV1 {
            name: (*name).to_string(),
            provider: provider.clone(),
            structured: *structured,
            value: *value,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_turn_is_structured_only_once_per_arm() {
        let session = Uuid::new_v4();
        assert!(!take_for_launch(session), "unarmed: ordinary launch");
        arm(session);
        assert!(take_for_launch(session));
        assert!(
            !take_for_launch(session),
            "a stale launched turn is forgotten, not repeated"
        );
        assert!(!finish_turn(session));

        arm(session);
        assert!(take_for_launch(session));
        assert!(finish_turn(session), "the observed turn was structured");
        assert!(!finish_turn(session));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn disarm_keeps_the_next_launch_free_text() {
        let session = Uuid::new_v4();
        arm(session);
        disarm(session);
        assert!(!take_for_launch(session));
        assert!(!finish_turn(session));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_never_launched_turn_is_not_reported_structured() {
        let session = Uuid::new_v4();
        arm(session);
        assert!(!finish_turn(session));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn codex_files_are_written_under_the_scratch_and_removed_with_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let session = Uuid::new_v4();
        arm(session);
        let files = prepare_codex_files(temp.path(), session).unwrap();
        assert!(files.schema.starts_with(temp.path()));
        assert!(files.last_message.starts_with(temp.path()));
        let schema: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&files.schema).unwrap()).unwrap();
        assert_eq!(
            schema,
            rsi_common::structured_handoff::structured_handoff_schema()
        );
        let dir = files.schema.parent().unwrap().to_path_buf();
        assert!(take_for_launch(session));
        assert!(finish_turn(session));
        assert!(!dir.exists(), "the turn's files are removed");

        // A disarmed turn removes them too.
        arm(session);
        let files = prepare_codex_files(temp.path(), session).unwrap();
        let dir = files.schema.parent().unwrap().to_path_buf();
        disarm(session);
        assert!(!dir.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn counters_are_labelled_by_provider_and_structured() {
        // Counters are process-wide: assert deltas on labels no other test uses.
        let value = |name: &str, provider: &str, structured: bool| {
            counters()
                .into_iter()
                .find(|c| c.name == name && c.provider == provider && c.structured == structured)
                .map_or(0, |c| c.value)
        };
        let before_plain = value(HANDOFF_INVALID_TOTAL, "Pioneer", false);
        let before_structured = value(HANDOFF_INVALID_TOTAL, "Pioneer", true);
        let before_observed = value(HANDOFF_OBSERVED_TOTAL, "Pioneer", true);
        record_invalid(Some(SessionProvider::Pioneer), false);
        record_invalid(Some(SessionProvider::Pioneer), true);
        record_invalid(Some(SessionProvider::Pioneer), true);
        record_observed(Some(SessionProvider::Pioneer), true);
        assert_eq!(
            value(HANDOFF_INVALID_TOTAL, "Pioneer", false),
            before_plain + 1
        );
        assert_eq!(
            value(HANDOFF_INVALID_TOTAL, "Pioneer", true),
            before_structured + 2
        );
        assert_eq!(
            value(HANDOFF_OBSERVED_TOTAL, "Pioneer", true),
            before_observed + 1
        );
    }
}
