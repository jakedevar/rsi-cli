//! Launch-time Closure selector (moved down from `closure_kernel`, which
//! re-exports it).

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
