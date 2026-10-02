//! K1 Closure terminal ingress and bounded restart reconciliation.
//!
//! This module deliberately exposes no destination mutation or cleanup
//! executor. Its terminal state is an internal `eligible_k2` queue row.

pub(crate) mod ingress;
pub(crate) mod operator;

pub use crate::store_support::closure_selector::ClosureLaunchSelectorV1;

pub(crate) fn terminal_contract(
    selector: &ClosureLaunchSelectorV1,
    session_id: uuid::Uuid,
    rotation_depth: u32,
) -> crate::error::Result<String> {
    let custody_id = selector.custody_id.ok_or_else(|| {
        crate::error::DaemonError::Store("Closure launch selector omitted custody id".into())
    })?;
    let custody_generation = selector.custody_generation.ok_or_else(|| {
        crate::error::DaemonError::Store(
            "Closure launch selector omitted custody generation".into(),
        )
    })?;
    let invocation_id = selector.model_invocation_id.ok_or_else(|| {
        crate::error::DaemonError::Store("Closure launch selector omitted invocation id".into())
    })?;
    let lineage_root_session_id = selector.lineage_root_session_id.ok_or_else(|| {
        crate::error::DaemonError::Store(
            "Closure launch selector omitted lineage root session id".into(),
        )
    })?;
    Ok(format!(
        "Closure terminal contract (binding): emit exactly one final `PIPELINE HANDOFF — CLOSURE:` response with one compact `closure_outcome_v1: {{json}}` line and the canonical Stage contract Inputs/Process/Outputs/Verify block. The JSON correlation must use program_id={}, source_id={}, custody_id={}, custody_generation={}, lineage_root_session_id={}, tip_session_id={}, rotation_depth={}, model_invocation_id={}, source_base_sha={}. Immutable integration identity: destination_ref={}, destination_pre_head={}, staging_ref={}. Do not infer or omit these values.",
        selector.program_id,
        selector.source_id,
        custody_id,
        custody_generation,
        lineage_root_session_id,
        session_id,
        rotation_depth,
        invocation_id,
        selector.base_sha,
        selector.destination_ref,
        selector.destination_pre_head,
        selector.staging_ref,
    ))
}
