//! #2956: a retained wake logs and records the real `sandbox_custody:*` cause,
//! never the generic `continuation_fence`.

use super::*;
use rsi_common::types::{
    SandboxCustodyErrorCodeV1, SandboxCustodyErrorV1, SandboxCustodyRecoveryV1,
    SandboxCustodyTransitionV1,
};

fn custody_error(code: SandboxCustodyErrorCodeV1) -> DaemonError {
    crate::error::sandbox_custody_error(SandboxCustodyErrorV1 {
        version: 1,
        code,
        session_id: None,
        transition: SandboxCustodyTransitionV1::ResumeWake,
        retryable: false,
        recovery: SandboxCustodyRecoveryV1::CreateNewSession,
    })
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_custody_refusal_is_recorded_under_its_real_code() {
    let error = custody_error(SandboxCustodyErrorCodeV1::OwnershipMissing);
    assert_eq!(
        resume_refusal_code(&error),
        "sandbox_custody:ownership_missing"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn every_custody_code_has_a_recorded_label() {
    for code in [
        SandboxCustodyErrorCodeV1::TupleIncomplete,
        SandboxCustodyErrorCodeV1::HistoricalPurged,
        SandboxCustodyErrorCodeV1::HistoricalTransferred,
        SandboxCustodyErrorCodeV1::CleanupFailed,
        SandboxCustodyErrorCodeV1::RootMissing,
        SandboxCustodyErrorCodeV1::RootOutsideBase,
        SandboxCustodyErrorCodeV1::RootIdentityMismatch,
        SandboxCustodyErrorCodeV1::WorktreeMismatch,
        SandboxCustodyErrorCodeV1::OwnershipMissing,
        SandboxCustodyErrorCodeV1::OwnershipConflict,
        SandboxCustodyErrorCodeV1::CustodyChanged,
        SandboxCustodyErrorCodeV1::ReclaimPrepared,
        SandboxCustodyErrorCodeV1::RootBusy,
        SandboxCustodyErrorCodeV1::SourceRevisionUnavailable,
        SandboxCustodyErrorCodeV1::SourceWorktreeDirty,
        SandboxCustodyErrorCodeV1::AllocationFailed,
        SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
    ] {
        // `root_busy` keeps its established `custody_root_busy` code.
        let expected = match code {
            SandboxCustodyErrorCodeV1::RootBusy => "custody_root_busy".to_string(),
            other => format!("sandbox_custody:{}", other.as_str()),
        };
        assert_eq!(resume_refusal_code(&custody_error(code)), expected);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn an_unrelated_refusal_still_reads_continuation_fence() {
    let error = DaemonError::InvalidParam("something_else".into());
    assert_eq!(resume_refusal_code(&error), "continuation_fence");
}
