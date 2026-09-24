//! Immutable contracts for the future test-suite baseline producer.
//!
//! This crate deliberately contains no filesystem, process, repository, or
//! publication implementation. Those authority-bearing capabilities belong to
//! later reviewed slices; ZR-0 only makes their inputs and bounds explicit.

#![allow(clippy::missing_errors_doc)]

pub mod bounds;
mod canonical_json;
pub mod error;
pub mod policy;
pub mod state;
pub mod witness;
pub mod wjr;

pub use error::{BaselineError, Result};
pub use policy::{
    ArgvAtom, CanonicalAbsolutePath, CanonicalUtcTime, CanonicalUuid, EnvironmentName,
    EnvironmentValue, GitCommit, PolicyDigestV1, RollingRef, Sha256Digest, ValidatedLaunchPolicyV1,
    ValidatedToolPolicyV1,
};
pub use state::{
    AuthorityConstructed, CandidateReady, Dormant, Executing, Producer, Published, Rejected,
    WitnessValidated,
};
pub use witness::{ValidatedLaunchWitnessV1, WitnessDigestV1, WitnessDraftV1};
pub use wjr::{
    CleanupOutcomeV2, KernelDropKnowledgeV2, ReproofOutcomeV2, TerminalBoundaryV2, TerminalCodeV2,
    TerminalFactV2, TerminalPhaseV2, ValidatedWjrV2, WatchAccountingV2, WatchDictionaryEntryV2,
    WatchEventV2,
};

/// The installed binary is intentionally non-operational until the final
/// cutover slice grants it an assembled producer.
pub const PRODUCER_ASSEMBLED: bool = false;

/// Return the only result available from the dormant command surface.
pub const fn dormant_result() -> Result<()> {
    Err(BaselineError::ProducerNotAssembled)
}
