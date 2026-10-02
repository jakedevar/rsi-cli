//! Per-session single-flight spawn guard. The lock map, `SpawnGuard` and
//! `RotationPublicationGuards` live in `store_support::spawn_single_flight`
//! (below `store`, which takes the rotation witness); they are re-exported here
//! so session call sites keep their paths. See that module for the design notes.

pub(super) use crate::store_support::spawn_single_flight::*;
use uuid::Uuid;

/// True iff a *live* tracked child already exists for `session_id`. Gates on
/// `is_alive()` (NOT mere map membership) so a finalized zombie whose handle was
/// nulled on finalize (`process: None`) is NOT adopted — a contended caller then
/// falls through and spawns fresh.
pub(super) async fn adopt_if_live(
    active: &tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>,
    session_id: Uuid,
) -> bool {
    let mut guard = active.write().await;
    match guard.get_mut(&session_id) {
        Some(t) => t
            .process
            .as_mut()
            .is_some_and(super::types::ProviderProcess::is_alive),
        None => false,
    }
}
