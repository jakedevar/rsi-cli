//! K1 Closure terminal ingress and bounded restart reconciliation.
//!
//! This module deliberately exposes no destination mutation or cleanup
//! executor. Its terminal state is an internal `eligible_k2` queue row.

pub(crate) mod ingress;
pub(crate) mod operator;

/// Immutable launch-time Closure identity. Ordinary launches carry `None` and
/// retain their historical `HEAD` source selection.
#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct ClosureLaunchSelectorV1 {
    pub program_id: rsi_common::closure_kernel::ClosureProgramIdV1,
    pub source_id: rsi_common::closure_kernel::ClosureSourceIdV1,
    pub base_sha: rsi_common::closure_kernel::ClosureGitShaV1,
    pub destination_ref: rsi_common::closure_kernel::ClosureLocalBranchRefV1,
    pub destination_pre_head: rsi_common::closure_kernel::ClosureGitShaV1,
    pub staging_ref: rsi_common::closure_kernel::ClosureLocalBranchRefV1,
    pub lineage_root_session_id: Option<uuid::Uuid>,
    pub custody_id: Option<uuid::Uuid>,
    pub custody_generation: Option<u64>,
    pub model_invocation_id: Option<uuid::Uuid>,
}

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
