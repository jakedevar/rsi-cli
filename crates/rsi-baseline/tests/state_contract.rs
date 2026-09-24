#![allow(clippy::unwrap_used)]

use rsi_baseline::{
    CleanupOutcomeV2, Dormant, Producer, ReproofOutcomeV2, TerminalBoundaryV2, TerminalCodeV2,
    TerminalFactV2, TerminalPhaseV2,
};

fn reason() -> TerminalFactV2 {
    TerminalFactV2::try_new(
        TerminalCodeV2::WitnessMalformed,
        TerminalBoundaryV2::PreAuthority,
        TerminalPhaseV2::Construct,
        CleanupOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
    )
    .unwrap()
}

fn command_capacity_reason() -> TerminalFactV2 {
    TerminalFactV2::try_new(
        TerminalCodeV2::StorageCapacity,
        TerminalBoundaryV2::Command,
        TerminalPhaseV2::Execute,
        CleanupOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
    )
    .unwrap()
}

#[test]
fn consuming_typestate_has_all_positive_edges_and_latches_reason() {
    let published = Producer::<Dormant>::dormant()
        .validate_witness()
        .construct_authority()
        .begin_execution()
        .candidate_ready()
        .publish();
    let _ = published;
    let rejected = Producer::<Dormant>::dormant().reject(reason());
    assert_eq!(
        rejected.into_terminal_fact().code(),
        TerminalCodeV2::WitnessMalformed
    );
}

#[test]
fn executing_command_bound_consumes_into_exact_first_red() {
    let mut executing = Producer::<Dormant>::dormant()
        .validate_witness()
        .construct_authority()
        .begin_execution();
    for expected in 1..=256 {
        executing = executing.next_command(command_capacity_reason()).unwrap();
        assert_eq!(executing.command_count(), expected);
    }
    assert_eq!(executing.command_count(), 256);
    let first_red = command_capacity_reason();
    let rejected = executing.next_command(first_red.clone()).unwrap_err();
    assert_eq!(rejected.into_terminal_fact(), first_red);
}
