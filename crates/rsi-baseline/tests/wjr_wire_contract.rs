#![allow(clippy::unwrap_used)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
};

use rsi_baseline::error::{TerminalBuildError, WjrBuildError, WjrDecodeError};
use rsi_baseline::{
    CanonicalAbsolutePath, CanonicalUtcTime, CanonicalUuid, CleanupOutcomeV2, Dormant,
    EnvironmentName, EnvironmentValue, GitCommit, KernelDropKnowledgeV2, Producer, Rejected,
    ReproofOutcomeV2, RollingRef, Sha256Digest, TerminalBoundaryV2, TerminalCodeV2, TerminalFactV2,
    TerminalPhaseV2, ValidatedLaunchPolicyV1, ValidatedLaunchWitnessV1, ValidatedToolPolicyV1,
    ValidatedWjrV2, WatchAccountingV2, WatchDictionaryEntryV2, WatchEventV2, WitnessDigestV1,
    WitnessDraftV1,
};
use sha2::{Digest, Sha256};

struct TrackingAllocator;

static MAX_ALLOCATION: AtomicUsize = AtomicUsize::new(0);

// SAFETY: this wrapper delegates every allocation and deallocation unchanged
// to the system allocator and records only the requested allocation size.
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        MAX_ALLOCATION.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller supplies the layout under `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from the delegated system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn sha(c: char) -> Sha256Digest {
    Sha256Digest::parse(c.to_string().repeat(64)).unwrap()
}
fn commit(c: char) -> GitCommit {
    GitCommit::parse(c.to_string().repeat(40)).unwrap()
}
fn policy() -> ValidatedToolPolicyV1 {
    ValidatedToolPolicyV1::try_new(
        vec![(
            EnvironmentName::parse("A").unwrap(),
            EnvironmentValue::parse("v").unwrap(),
        )],
        vec![],
    )
    .unwrap()
}
fn witness() -> ValidatedLaunchWitnessV1 {
    let policy = ValidatedLaunchPolicyV1::new(policy(), policy(), policy(), policy(), policy());
    ValidatedLaunchWitnessV1::try_new(WitnessDraftV1 {
        accepted_implementation_commit: commit('a'),
        accepted_review_commit: commit('b'),
        accepted_verification_manifest_commit: commit('c'),
        active_ref_sha256: sha('d'),
        authorization_time: CanonicalUtcTime::parse("2026-09-02T12:34:56.000000000Z").unwrap(),
        cargo_config_sha256: sha('e'),
        failure_record_commit: GitCommit::parse("b3c15be2ebfa7bf7ed9f66288e16742e6721dc99")
            .unwrap(),
        index_sha256: sha('0'),
        make_sha256: sha('1'),
        nextest_config_sha256: sha('2'),
        output_leaf: "metrics/test-suite-baseline.json".into(),
        policy,
        producer_sha256: sha('3'),
        repository_dev: 1,
        repository_ino: 2,
        repository_path: CanonicalAbsolutePath::parse("/srv/rsi").unwrap(),
        run_nonce: CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        rust_toolchain_sha256: sha('4'),
        source_commit: commit('5'),
        source_inventory_sha256: sha('6'),
        symbolic_head: RollingRef::parse("refs/heads/rolling").unwrap(),
        tracked_stage_set_sha256: sha('7'),
    })
    .unwrap()
}
fn terminal(
    code: TerminalCodeV2,
    boundary: TerminalBoundaryV2,
    phase: TerminalPhaseV2,
) -> TerminalFactV2 {
    TerminalFactV2::try_new(
        code,
        boundary,
        phase,
        CleanupOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
        ReproofOutcomeV2::Unknown,
    )
    .unwrap()
}
fn fixture_with_rejected(rejected: Producer<Rejected>) -> ValidatedWjrV2 {
    let dictionary = vec![WatchDictionaryEntryV2::try_new("/watched", 1, 0, 1, 2).unwrap()];
    let details = vec![WatchEventV2::try_new(0, 1, 1, 1, 0, 0, "leaf").unwrap()];
    let mut masks = [0; 32];
    masks[0] = 1;
    let accounting = WatchAccountingV2::try_new(
        1,
        1,
        0,
        4,
        masks,
        0,
        1,
        KernelDropKnowledgeV2::NotObserved,
        [0x51; 32],
    )
    .unwrap();
    ValidatedWjrV2::try_new(
        WitnessDigestV1::of(&witness()),
        CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        1,
        2,
        rejected,
        dictionary,
        details,
        accounting,
    )
    .unwrap()
}
fn fixture_with(terminal: TerminalFactV2) -> ValidatedWjrV2 {
    fixture_with_rejected(Producer::<Dormant>::dormant().reject(terminal))
}
fn fixture() -> ValidatedWjrV2 {
    fixture_with(terminal(
        TerminalCodeV2::WitnessMalformed,
        TerminalBoundaryV2::PreAuthority,
        TerminalPhaseV2::Construct,
    ))
}

fn capacity_digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"rsi-baseline/wjr2-capacities/v1\0");
    hash.update(&bytes[8..12]);
    hash.update(&bytes[12..16]);
    hash.update(&bytes[80..152]);
    hash.finalize().into()
}

#[allow(clippy::match_same_arms)]
fn test_oracle_allows(
    code: TerminalCodeV2,
    boundary: TerminalBoundaryV2,
    phase: TerminalPhaseV2,
) -> bool {
    use TerminalBoundaryV2 as B;
    use TerminalCodeV2 as C;
    use TerminalPhaseV2 as P;

    match code {
        C::UnsupportedPlatform
        | C::UnsupportedKernel
        | C::WitnessMalformed
        | C::WitnessNonCanonical
        | C::WitnessPolicy
        | C::WitnessDigest => boundary == B::PreAuthority && phase == P::Construct,
        C::AuthorityConstruction | C::AuthorityDiscovery | C::AuthorityReconciliation => {
            boundary == B::AuthorityConstruction && matches!(phase, P::Construct | P::Reproof)
        }
        C::WatchOverflow
        | C::WatchIgnored
        | C::WatchUnmounted
        | C::WatchDecode
        | C::WatchCeiling => {
            boundary == B::Watch && matches!(phase, P::Construct | P::Drain | P::Reproof)
        }
        C::StorageReserve => boundary == B::PreAuthority && phase == P::Construct,
        C::StorageOpen => {
            matches!(
                boundary,
                B::PreAuthority
                    | B::Command
                    | B::Normalize
                    | B::HeldOut
                    | B::Publication
                    | B::Finalization
            ) && phase == P::Construct
        }
        C::StorageCapacity | C::StorageWrite => matches!(
            (boundary, phase),
            (B::PreAuthority, P::Construct)
                | (B::AuthorityConstruction, P::Construct | P::Reproof)
                | (B::Watch, P::Construct | P::Drain | P::Reproof)
                | (
                    B::Command | B::HeldOut,
                    P::Construct | P::Execute | P::Drain | P::Cleanup | P::Seal
                )
                | (B::Normalize, P::Construct | P::Execute | P::Seal)
                | (
                    B::Publication,
                    P::Construct | P::Execute | P::Cleanup | P::Seal
                )
                | (B::FinalReproof, P::Reproof)
                | (
                    B::Finalization,
                    P::Construct | P::Cleanup | P::Reproof | P::Seal
                )
        ),
        C::StorageFsync | C::StorageSeal => {
            matches!(
                boundary,
                B::PreAuthority
                    | B::Command
                    | B::Normalize
                    | B::HeldOut
                    | B::Publication
                    | B::Finalization
            ) && phase == P::Seal
        }
        C::ChildBootstrap | C::ChildPipe | C::ChildTimeout | C::ChildReap | C::ChildGroupLive => {
            boundary == B::Command && matches!(phase, P::Execute | P::Drain | P::Cleanup)
        }
        C::ParserMalformed | C::ParserLimit | C::ParserSchema => {
            matches!(
                boundary,
                B::AuthorityConstruction | B::Command | B::Normalize
            ) && matches!(phase, P::Construct | P::Execute | P::Drain)
        }
        C::NormalizationRejected => boundary == B::Normalize && phase == P::Execute,
        C::HeldOutRejected => boundary == B::HeldOut && phase == P::Execute,
        C::CleanupTerminate | C::CleanupKill | C::CleanupReap | C::CleanupResidual => {
            phase == P::Cleanup
        }
        C::ReproofAuthority
        | C::ReproofPathIdentity
        | C::ReproofContentHash
        | C::ReproofModeLink => {
            matches!(
                boundary,
                B::AuthorityConstruction | B::FinalReproof | B::Finalization
            ) && phase == P::Reproof
        }
        C::PublicationCandidate
        | C::PublicationNoOverwrite
        | C::PublicationFsync
        | C::PublicationRollback => {
            boundary == B::Publication && matches!(phase, P::Execute | P::Seal | P::Cleanup)
        }
        C::Panic => true,
    }
}

const fn storage_index(code: TerminalCodeV2) -> Option<usize> {
    match code {
        TerminalCodeV2::StorageReserve => Some(0),
        TerminalCodeV2::StorageOpen => Some(1),
        TerminalCodeV2::StorageCapacity => Some(2),
        TerminalCodeV2::StorageWrite => Some(3),
        TerminalCodeV2::StorageFsync => Some(4),
        TerminalCodeV2::StorageSeal => Some(5),
        _ => None,
    }
}

#[test]
fn exact_envelope_round_trips_and_refuses_lengths_and_capacity_mutation() {
    let wire = fixture().encode_exact();
    assert_eq!(wire.len(), 2_097_152);
    assert_eq!(&wire[0..4], b"WJR2");
    assert!(ValidatedWjrV2::decode_exact(wire.as_ref()).is_ok());
    assert!(ValidatedWjrV2::decode_exact(&wire[..wire.len() - 1]).is_err());
    let mut plus = wire.to_vec();
    plus.push(0);
    assert!(ValidatedWjrV2::decode_exact(&plus).is_err());
    let mut changed = wire.to_vec();
    changed[80] ^= 1;
    assert!(ValidatedWjrV2::decode_exact(&changed).is_err());
}

#[test]
fn every_region_header_footer_and_zero_rule_is_at_its_exact_offset() {
    let wire = fixture().encode_exact();
    assert_eq!(u16::from_be_bytes(wire[4..6].try_into().unwrap()), 2);
    assert_eq!(wire[6..8], [1, 1]);
    assert_eq!(u32::from_be_bytes(wire[8..12].try_into().unwrap()), 4_096);
    assert_eq!(
        u32::from_be_bytes(wire[12..16].try_into().unwrap()),
        2_097_152
    );
    assert_eq!(u32::from_be_bytes(wire[80..84].try_into().unwrap()), 4_096);
    assert_eq!(u32::from_be_bytes(wire[144..148].try_into().unwrap()), 284);
    assert_eq!(u32::from_be_bytes(wire[148..152].try_into().unwrap()), 1);
    assert_eq!(
        u16::from_be_bytes(wire[184..186].try_into().unwrap()),
        TerminalCodeV2::WitnessMalformed.code()
    );
    assert_eq!(wire[186..189], [1, 1, 1]);
    assert!(wire[240..4_096].iter().all(|byte| *byte == 0));
    assert_eq!(
        u32::from_be_bytes(wire[4_096..4_100].try_into().unwrap()),
        1
    );
    assert_eq!(
        u64::from_be_bytes(wire[528_384..528_392].try_into().unwrap()),
        0
    );
    assert_eq!(
        u64::from_be_bytes(wire[1_839_104..1_839_112].try_into().unwrap()),
        1
    );
    assert_eq!(
        u16::from_be_bytes(wire[1_839_404..1_839_406].try_into().unwrap()),
        TerminalCodeV2::WitnessMalformed.code()
    );
    assert!(wire[1_839_520..1_970_176].iter().all(|byte| *byte == 0));
    assert!(wire[1_970_176..].iter().all(|byte| *byte == 0));
}

#[test]
fn capacity_profile_digest_and_wire_identity_refuse_every_mutation() {
    let wire = fixture().encode_exact();
    for offset in (80..152).step_by(4) {
        let mut changed = wire.to_vec();
        changed[offset] ^= 1;
        assert!(
            ValidatedWjrV2::decode_exact(&changed).is_err(),
            "field {offset}"
        );
        let digest = capacity_digest(&changed);
        changed[152..184].copy_from_slice(&digest);
        changed[1_839_448..1_839_480].copy_from_slice(&digest);
        assert!(
            ValidatedWjrV2::decode_exact(&changed).is_err(),
            "self-consistent field {offset}"
        );
    }
    for offset in [152, 1_839_448] {
        let mut changed = wire.to_vec();
        changed[offset] ^= 1;
        assert!(ValidatedWjrV2::decode_exact(&changed).is_err());
    }
    for offset in [0, 4, 6, 7, 1_839_444] {
        let mut changed = wire.to_vec();
        changed[offset] ^= 1;
        assert!(ValidatedWjrV2::decode_exact(&changed).is_err());
    }
    let capacities = ValidatedWjrV2::capacities();
    assert_eq!(capacities.dictionary_offset(), 4_096);
    assert_eq!(capacities.detail_offset(), 528_384);
    assert_eq!(capacities.footer_offset(), 1_839_104);
    assert_eq!(capacities.zero_fill_bytes(), 126_976);
}

#[test]
#[allow(clippy::too_many_lines)]
fn checked_records_and_accounting_enforce_boundaries() {
    assert!(WatchDictionaryEntryV2::try_new(format!("/{}", "a".repeat(95)), 1, 0, 1, 1).is_ok());
    assert_eq!(
        WatchDictionaryEntryV2::try_new(format!("/{}", "a".repeat(96)), 1, 0, 1, 1),
        Err(WjrBuildError::Dictionary)
    );
    assert!(WatchEventV2::try_new(0, 0, 1, 1, 0, 0, "a".repeat(284)).is_ok());
    assert_eq!(
        WatchEventV2::try_new(0, 0, 1, 1, 0, 0, "a".repeat(285)),
        Err(WjrBuildError::Detail)
    );
    let digest = [0x52; 32];
    let mut masks = [0; 32];
    masks[0] = 1;
    assert!(
        WatchAccountingV2::try_new(
            1,
            1,
            0,
            0,
            masks,
            0,
            1,
            KernelDropKnowledgeV2::NotObserved,
            digest
        )
        .is_ok()
    );
    assert!(
        WatchAccountingV2::try_new(
            1,
            1,
            1,
            0,
            masks,
            0,
            1,
            KernelDropKnowledgeV2::NotObserved,
            digest
        )
        .is_err()
    );
    let mut too_many = masks;
    too_many[0] = 2;
    assert!(
        WatchAccountingV2::try_new(
            1,
            1,
            0,
            0,
            too_many,
            0,
            1,
            KernelDropKnowledgeV2::NotObserved,
            digest
        )
        .is_err()
    );
    let mut overflow = masks;
    overflow[14] = 1;
    assert!(
        WatchAccountingV2::try_new(
            1,
            1,
            0,
            0,
            overflow,
            0,
            1,
            KernelDropKnowledgeV2::NotObserved,
            digest
        )
        .is_err()
    );
    assert!(
        WatchAccountingV2::try_new(
            1,
            1,
            0,
            0,
            masks,
            0,
            1,
            KernelDropKnowledgeV2::Unknown,
            digest
        )
        .is_err()
    );
    assert!(
        WatchAccountingV2::try_new(
            1,
            1,
            0,
            0,
            masks,
            0,
            1,
            KernelDropKnowledgeV2::NotObserved,
            [0; 32]
        )
        .is_err()
    );
    assert!(
        WatchAccountingV2::try_new(
            65_536,
            4_096,
            61_440,
            1_048_576,
            [0; 32],
            0,
            0,
            KernelDropKnowledgeV2::NotObserved,
            [1; 32]
        )
        .is_ok()
    );
    assert_eq!(
        WatchAccountingV2::try_new(
            65_537,
            4_096,
            61_441,
            0,
            [0; 32],
            0,
            0,
            KernelDropKnowledgeV2::NotObserved,
            [1; 32]
        ),
        Err(WjrBuildError::Accounting)
    );
    assert!(
        WatchAccountingV2::try_new(
            0,
            0,
            0,
            1_048_576,
            [0; 32],
            0,
            0,
            KernelDropKnowledgeV2::NotObserved,
            [1; 32]
        )
        .is_ok()
    );
    assert_eq!(
        WatchAccountingV2::try_new(
            0,
            0,
            0,
            1_048_577,
            [0; 32],
            0,
            0,
            KernelDropKnowledgeV2::NotObserved,
            [1; 32]
        ),
        Err(WjrBuildError::Accounting)
    );
    assert!(
        WatchAccountingV2::try_new(
            4_096,
            4_096,
            0,
            0,
            [0; 32],
            0,
            0,
            KernelDropKnowledgeV2::NotObserved,
            [1; 32]
        )
        .is_ok()
    );
    assert_eq!(
        WatchAccountingV2::try_new(
            4_097,
            4_097,
            0,
            0,
            [0; 32],
            0,
            0,
            KernelDropKnowledgeV2::NotObserved,
            [1; 32]
        ),
        Err(WjrBuildError::Accounting)
    );
}

#[test]
fn oversized_borrowed_records_are_refused_before_clone_sized_allocation() {
    let mut oversized = vec![b'a'; 16_777_216];
    oversized[0] = b'/';

    MAX_ALLOCATION.store(0, Ordering::SeqCst);
    assert_eq!(
        WatchDictionaryEntryV2::try_new(&oversized, 1, 0, 1, 1),
        Err(WjrBuildError::Dictionary)
    );
    assert!(MAX_ALLOCATION.load(Ordering::SeqCst) < oversized.len());

    MAX_ALLOCATION.store(0, Ordering::SeqCst);
    assert_eq!(
        WatchEventV2::try_new(0, 0, 1, 1, 0, 0, &oversized),
        Err(WjrBuildError::Detail)
    );
    assert!(MAX_ALLOCATION.load(Ordering::SeqCst) < oversized.len());
}

#[test]
fn dictionary_paths_reject_nul_while_raw_leaves_preserve_it() {
    assert!(WatchDictionaryEntryV2::try_new("/", 1, 0, 1, 1).is_ok());
    assert_eq!(
        WatchDictionaryEntryV2::try_new(b"/a\0b", 1, 0, 1, 1),
        Err(WjrBuildError::Dictionary)
    );

    let wire = fixture().encode_exact();
    let mut nul_path = wire.to_vec();
    nul_path[4_100..4_102].copy_from_slice(&4_u16.to_be_bytes());
    nul_path[4_120..4_216].fill(0);
    nul_path[4_120..4_124].copy_from_slice(b"/a\0b");
    assert_eq!(
        ValidatedWjrV2::decode_exact(&nul_path),
        Err(WjrDecodeError::Shape)
    );

    let raw_leaf = [0x00, 0xff, 0x80];
    let event = WatchEventV2::try_new(0, 1, 1, 1, 0, 0, raw_leaf).unwrap();
    assert_eq!(event.raw_leaf(), raw_leaf);
}

#[test]
#[allow(clippy::too_many_lines)]
fn validated_constructor_enforces_dictionary_and_detail_vector_bounds() {
    let dictionary = (0..4_096)
        .map(|id| {
            WatchDictionaryEntryV2::try_new(format!("/d/{id:04}"), id + 1, 0, 1, u64::from(id) + 1)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let accounting = WatchAccountingV2::try_new(
        0,
        0,
        0,
        0,
        [0; 32],
        0,
        4_096,
        KernelDropKnowledgeV2::NotObserved,
        [0; 32],
    )
    .unwrap();
    let rejected = Producer::<Dormant>::dormant().reject(terminal(
        TerminalCodeV2::WitnessMalformed,
        TerminalBoundaryV2::PreAuthority,
        TerminalPhaseV2::Construct,
    ));
    assert!(
        ValidatedWjrV2::try_new(
            WitnessDigestV1::of(&witness()),
            CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            1,
            2,
            rejected,
            dictionary.clone(),
            vec![],
            accounting.clone()
        )
        .is_ok()
    );
    let mut too_many_dictionary = dictionary;
    too_many_dictionary
        .push(WatchDictionaryEntryV2::try_new("/overflow", 4_097, 0, 1, 4_097).unwrap());
    let rejected = Producer::<Dormant>::dormant().reject(terminal(
        TerminalCodeV2::WitnessMalformed,
        TerminalBoundaryV2::PreAuthority,
        TerminalPhaseV2::Construct,
    ));
    assert!(
        ValidatedWjrV2::try_new(
            WitnessDigestV1::of(&witness()),
            CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            1,
            2,
            rejected,
            too_many_dictionary,
            vec![],
            accounting
        )
        .is_err()
    );
    let details = (0..4_096)
        .map(|sequence| WatchEventV2::try_new(sequence, sequence, 1, 1, 0, 0, "x").unwrap())
        .collect::<Vec<_>>();
    let mut masks = [0; 32];
    masks[0] = 4_096;
    let detail_accounting = WatchAccountingV2::try_new(
        4_096,
        4_096,
        0,
        4_096,
        masks,
        0,
        1,
        KernelDropKnowledgeV2::NotObserved,
        [0x41; 32],
    )
    .unwrap();
    let rejected = Producer::<Dormant>::dormant().reject(terminal(
        TerminalCodeV2::WitnessMalformed,
        TerminalBoundaryV2::PreAuthority,
        TerminalPhaseV2::Construct,
    ));
    assert!(
        ValidatedWjrV2::try_new(
            WitnessDigestV1::of(&witness()),
            CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            1,
            2,
            rejected,
            vec![WatchDictionaryEntryV2::try_new("/watched", 1, 0, 1, 2).unwrap()],
            details.clone(),
            detail_accounting.clone()
        )
        .is_ok()
    );
    let mut too_many_details = details;
    too_many_details.push(WatchEventV2::try_new(4_096, 4_096, 1, 1, 0, 0, "x").unwrap());
    let rejected = Producer::<Dormant>::dormant().reject(terminal(
        TerminalCodeV2::WitnessMalformed,
        TerminalBoundaryV2::PreAuthority,
        TerminalPhaseV2::Construct,
    ));
    assert!(
        ValidatedWjrV2::try_new(
            WitnessDigestV1::of(&witness()),
            CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            1,
            2,
            rejected,
            vec![WatchDictionaryEntryV2::try_new("/watched", 1, 0, 1, 2).unwrap()],
            too_many_details,
            detail_accounting
        )
        .is_err()
    );
}

#[test]
fn raw_leaf_and_raw_record_digest_are_exact_opaque_bytes() {
    let leaf = [0, 0xff, 0x80];
    let event = WatchEventV2::try_new(0, 1, 1, 1, 0, 0, leaf).unwrap();
    assert_eq!(event.raw_leaf(), leaf);
    assert!(WatchEventV2::try_new(0, 1, 1, 1, 0, 0, vec![0xff; 284]).is_ok());
    assert!(WatchEventV2::try_new(0, 1, 1, 1, 0, 0, vec![0xff; 285]).is_err());
    let mut masks = [0; 32];
    masks[0] = 1;
    let accounting = WatchAccountingV2::try_new(
        1,
        1,
        0,
        4,
        masks,
        0,
        1,
        KernelDropKnowledgeV2::NotObserved,
        [0xa5; 32],
    )
    .unwrap();
    let rejected = Producer::<Dormant>::dormant().reject(terminal(
        TerminalCodeV2::WitnessMalformed,
        TerminalBoundaryV2::PreAuthority,
        TerminalPhaseV2::Construct,
    ));
    let wire = ValidatedWjrV2::try_new(
        WitnessDigestV1::of(&witness()),
        CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        1,
        2,
        rejected,
        vec![WatchDictionaryEntryV2::try_new("/watched", 1, 0, 1, 2).unwrap()],
        vec![event],
        accounting,
    )
    .unwrap()
    .encode_exact();
    assert_eq!(&wire[528_420..528_423], &leaf);
    assert_eq!(&wire[1_839_408..1_839_440], &[0xa5; 32]);
    assert_eq!(
        ValidatedWjrV2::decode_exact(wire.as_ref())
            .unwrap()
            .details()[0]
            .raw_leaf(),
        leaf
    );
}

#[test]
fn duplicate_order_and_used_record_gap_are_rejected() {
    for dictionary in [
        vec![
            WatchDictionaryEntryV2::try_new("/b", 2, 0, 1, 2).unwrap(),
            WatchDictionaryEntryV2::try_new("/a", 1, 0, 1, 1).unwrap(),
        ],
        vec![
            WatchDictionaryEntryV2::try_new("/a", 1, 0, 1, 1).unwrap(),
            WatchDictionaryEntryV2::try_new("/b", 1, 0, 1, 2).unwrap(),
        ],
    ] {
        let accounting = WatchAccountingV2::try_new(
            0,
            0,
            0,
            0,
            [0; 32],
            0,
            2,
            KernelDropKnowledgeV2::NotObserved,
            [0; 32],
        )
        .unwrap();
        let rejected = Producer::<Dormant>::dormant().reject(terminal(
            TerminalCodeV2::WitnessMalformed,
            TerminalBoundaryV2::PreAuthority,
            TerminalPhaseV2::Construct,
        ));
        assert!(
            ValidatedWjrV2::try_new(
                WitnessDigestV1::of(&witness()),
                CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
                1,
                2,
                rejected,
                dictionary,
                vec![],
                accounting
            )
            .is_err()
        );
    }
    let wire = fixture().encode_exact();
    let mut used_gap = wire.to_vec();
    used_gap[1_839_400..1_839_404].copy_from_slice(&2_u32.to_be_bytes());
    assert!(ValidatedWjrV2::decode_exact(&used_gap).is_err());
    let mut unused = wire.to_vec();
    unused[4_224] = 1;
    assert!(ValidatedWjrV2::decode_exact(&unused).is_err());
}

#[test]
fn hostile_dictionary_detail_and_unused_record_mutations_refuse() {
    let wire = fixture().encode_exact();
    for offset in [4_096, 4_100, 4_120, 528_384, 528_400, 528_404] {
        let mut changed = wire.to_vec();
        changed[offset] ^= 1;
        assert!(
            ValidatedWjrV2::decode_exact(&changed).is_err(),
            "hostile record {offset}"
        );
    }
    let mut sequence = wire.to_vec();
    sequence[528_391] = 1;
    assert!(ValidatedWjrV2::decode_exact(&sequence).is_err());
    let mut reference = wire.to_vec();
    reference[528_403] = 2;
    assert!(ValidatedWjrV2::decode_exact(&reference).is_err());
}

#[test]
fn retained_detail_masks_are_a_monotonic_subset_when_events_are_summarized() {
    let details = (0..4_096)
        .map(|sequence| {
            WatchEventV2::try_new(
                sequence,
                sequence,
                1,
                1 | (u32::from(sequence == 0) << 14),
                0,
                0,
                "leaf",
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let dictionary = vec![WatchDictionaryEntryV2::try_new("/watched", 1, 0, 1, 2).unwrap()];
    let mut masks = [0; 32];
    masks[0] = 4_097;
    masks[14] = 1;
    let accounting = WatchAccountingV2::try_new(
        4_097,
        4_096,
        1,
        4_097,
        masks,
        1,
        1,
        KernelDropKnowledgeV2::Unknown,
        [7; 32],
    )
    .unwrap();
    let rejected = Producer::<Dormant>::dormant().reject(terminal(
        TerminalCodeV2::WatchOverflow,
        TerminalBoundaryV2::Watch,
        TerminalPhaseV2::Drain,
    ));
    let wire = ValidatedWjrV2::try_new(
        WitnessDigestV1::of(&witness()),
        CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        1,
        2,
        rejected,
        dictionary,
        details,
        accounting,
    )
    .unwrap()
    .encode_exact();
    assert!(ValidatedWjrV2::decode_exact(wire.as_ref()).is_ok());
    let mut mask_loss = wire.to_vec();
    mask_loss[1_839_136..1_839_144].copy_from_slice(&4_095_u64.to_be_bytes());
    assert!(ValidatedWjrV2::decode_exact(&mask_loss).is_err());
    let mut overflow_loss = wire.to_vec();
    overflow_loss[1_839_248..1_839_256].copy_from_slice(&0_u64.to_be_bytes());
    overflow_loss[1_839_392..1_839_400].copy_from_slice(&0_u64.to_be_bytes());
    overflow_loss[1_839_407] = 0;
    assert!(ValidatedWjrV2::decode_exact(&overflow_loss).is_err());
}

#[test]
fn hostile_reserved_shape_finality_and_active_command_mutations_refuse() {
    let wire = fixture().encode_exact();
    for offset in [
        190, 240, 4_216, 4_248, 528_418, 528_424, 528_704, 1_839_446, 1_839_520, 1_970_176,
    ] {
        let mut changed = wire.to_vec();
        changed[offset] = 1;
        assert!(
            ValidatedWjrV2::decode_exact(&changed).is_err(),
            "reserved {offset}"
        );
    }
    for offset in [
        184, 186, 187, 188, 1_839_406, 1_839_440, 1_839_441, 1_839_442, 1_839_443,
    ] {
        let mut changed = wire.to_vec();
        changed[offset] = 0;
        assert!(
            ValidatedWjrV2::decode_exact(&changed).is_err(),
            "code {offset}"
        );
    }
    let mut mismatch = wire.to_vec();
    mismatch[1_839_405] ^= 1;
    assert_eq!(
        ValidatedWjrV2::decode_exact(&mismatch),
        Err(WjrDecodeError::TerminalFinality)
    );
    let mut footer_reserved = wire.to_vec();
    footer_reserved[1_839_104 + 416] = 1;
    assert_eq!(
        ValidatedWjrV2::decode_exact(&footer_reserved),
        Err(WjrDecodeError::NonzeroReserved)
    );
    let mut header_finality = wire.to_vec();
    header_finality[188] = 0;
    assert_eq!(
        ValidatedWjrV2::decode_exact(&header_finality),
        Err(WjrDecodeError::TerminalFinality)
    );
    let mut footer_finality = wire.to_vec();
    footer_finality[1_839_104 + 302] = 0;
    assert_eq!(
        ValidatedWjrV2::decode_exact(&footer_finality),
        Err(WjrDecodeError::TerminalFinality)
    );
    let mut raw_digest = wire.to_vec();
    raw_digest[1_839_104 + 304..1_839_104 + 336].fill(0);
    assert_eq!(
        ValidatedWjrV2::decode_exact(&raw_digest),
        Err(WjrDecodeError::Digest)
    );
    let mut inactive = wire.to_vec();
    inactive[192] = 1;
    assert!(ValidatedWjrV2::decode_exact(&inactive).is_err());
    let mut active = wire.to_vec();
    active[189] = 1;
    active[192..200].copy_from_slice(&1_u64.to_be_bytes());
    active[200] = 1;
    active[232..236].copy_from_slice(&1_i32.to_be_bytes());
    active[236..240].copy_from_slice(&1_i32.to_be_bytes());
    assert!(ValidatedWjrV2::decode_exact(&active).is_ok());
    active[232..236].copy_from_slice(&0_i32.to_be_bytes());
    assert!(ValidatedWjrV2::decode_exact(&active).is_err());
    for range in [192..200, 200..232, 232..236, 236..240] {
        let mut invalid = wire.to_vec();
        invalid[189] = 1;
        invalid[192..200].copy_from_slice(&1_u64.to_be_bytes());
        invalid[200] = 1;
        invalid[232..236].copy_from_slice(&1_i32.to_be_bytes());
        invalid[236..240].copy_from_slice(&1_i32.to_be_bytes());
        invalid[range].fill(0);
        assert!(ValidatedWjrV2::decode_exact(&invalid).is_err());
    }
    let mut presence = active;
    presence[189] = 2;
    assert!(ValidatedWjrV2::decode_exact(&presence).is_err());
}

#[test]
fn golden_footer_outcome_codes_are_encoded_for_every_fixed_value() {
    for cleanup in [
        CleanupOutcomeV2::Passed,
        CleanupOutcomeV2::Failed,
        CleanupOutcomeV2::Unknown,
    ] {
        for reproof in [
            ReproofOutcomeV2::Passed,
            ReproofOutcomeV2::Failed,
            ReproofOutcomeV2::Unknown,
        ] {
            let fact = TerminalFactV2::try_new(
                TerminalCodeV2::WitnessMalformed,
                TerminalBoundaryV2::PreAuthority,
                TerminalPhaseV2::Construct,
                cleanup,
                reproof,
                reproof,
                reproof,
            )
            .unwrap();
            let wire = fixture_with(fact).encode_exact();
            assert_eq!(wire[1_839_440], cleanup.code());
            assert_eq!(wire[1_839_441..1_839_444], [reproof.code(); 3]);
            assert!(ValidatedWjrV2::decode_exact(wire.as_ref()).is_ok());
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn terminal_codes_and_closed_compatibility_matrix_are_exact() {
    use TerminalBoundaryV2 as B;
    use TerminalCodeV2 as C;
    use TerminalPhaseV2 as P;
    let codes: [(C, u16); 43] = [
        (C::UnsupportedPlatform, 0x0101),
        (C::UnsupportedKernel, 0x0102),
        (C::WitnessMalformed, 0x0201),
        (C::WitnessNonCanonical, 0x0202),
        (C::WitnessPolicy, 0x0203),
        (C::WitnessDigest, 0x0204),
        (C::AuthorityConstruction, 0x0301),
        (C::AuthorityDiscovery, 0x0302),
        (C::AuthorityReconciliation, 0x0303),
        (C::WatchOverflow, 0x0401),
        (C::WatchIgnored, 0x0402),
        (C::WatchUnmounted, 0x0403),
        (C::WatchDecode, 0x0404),
        (C::WatchCeiling, 0x0405),
        (C::StorageReserve, 0x0501),
        (C::StorageOpen, 0x0502),
        (C::StorageWrite, 0x0503),
        (C::StorageFsync, 0x0504),
        (C::StorageCapacity, 0x0505),
        (C::StorageSeal, 0x0506),
        (C::ChildBootstrap, 0x0601),
        (C::ChildPipe, 0x0602),
        (C::ChildTimeout, 0x0603),
        (C::ChildReap, 0x0604),
        (C::ChildGroupLive, 0x0605),
        (C::ParserMalformed, 0x0701),
        (C::ParserLimit, 0x0702),
        (C::ParserSchema, 0x0703),
        (C::NormalizationRejected, 0x0801),
        (C::HeldOutRejected, 0x0901),
        (C::CleanupTerminate, 0x0a01),
        (C::CleanupKill, 0x0a02),
        (C::CleanupReap, 0x0a03),
        (C::CleanupResidual, 0x0a04),
        (C::ReproofAuthority, 0x0b01),
        (C::ReproofPathIdentity, 0x0b02),
        (C::ReproofContentHash, 0x0b03),
        (C::ReproofModeLink, 0x0b04),
        (C::PublicationCandidate, 0x0c01),
        (C::PublicationNoOverwrite, 0x0c02),
        (C::PublicationFsync, 0x0c03),
        (C::PublicationRollback, 0x0c04),
        (C::Panic, 0x0d01),
    ];
    let boundaries: [(B, u8); 9] = [
        (B::PreAuthority, 1),
        (B::AuthorityConstruction, 2),
        (B::Watch, 3),
        (B::Command, 4),
        (B::Normalize, 5),
        (B::HeldOut, 6),
        (B::Publication, 7),
        (B::FinalReproof, 8),
        (B::Finalization, 9),
    ];
    let phases: [(P, u8); 6] = [
        (P::Construct, 1),
        (P::Execute, 2),
        (P::Drain, 3),
        (P::Cleanup, 4),
        (P::Reproof, 5),
        (P::Seal, 6),
    ];

    let mut visited = 0;
    let mut allowed = 0;
    let mut refused = 0;
    let mut storage_visited = 0;
    let mut storage_allowed = [0; 6];
    let mut storage_refused = 0;
    for (code, raw_code) in codes {
        assert_eq!(code.code(), raw_code);
        for (boundary, raw_boundary) in boundaries {
            assert_eq!(boundary.code(), raw_boundary);
            for (phase, raw_phase) in phases {
                assert_eq!(phase.code(), raw_phase);
                visited += 1;
                let built = TerminalFactV2::try_new(
                    code,
                    boundary,
                    phase,
                    CleanupOutcomeV2::Unknown,
                    ReproofOutcomeV2::Unknown,
                    ReproofOutcomeV2::Unknown,
                    ReproofOutcomeV2::Unknown,
                );
                if test_oracle_allows(code, boundary, phase) {
                    allowed += 1;
                    if let Some(index) = storage_index(code) {
                        storage_allowed[index] += 1;
                    }
                    let wire = fixture_with(built.unwrap()).encode_exact();
                    assert_eq!(
                        u16::from_be_bytes(wire[184..186].try_into().unwrap()),
                        raw_code
                    );
                    assert_eq!(wire[186], raw_boundary);
                    assert_eq!(wire[187], raw_phase);
                    assert_eq!(wire[188], 1);
                    assert_eq!(wire[1_839_104 + 302], 1);
                    let decoded = ValidatedWjrV2::decode_exact(wire.as_ref()).unwrap();
                    assert_eq!(decoded.terminal().code(), code);
                    assert_eq!(decoded.terminal().boundary(), boundary);
                    assert_eq!(decoded.terminal().phase(), phase);
                } else {
                    refused += 1;
                    assert_eq!(built, Err(TerminalBuildError::IncompatibleCode));
                    if storage_index(code).is_some() {
                        storage_refused += 1;
                    }
                }
                if storage_index(code).is_some() {
                    storage_visited += 1;
                }
            }
        }
    }
    assert_eq!((visited, allowed, refused), (2_322, 260, 2_062));
    assert_eq!(storage_visited, 324);
    assert_eq!(storage_allowed, [1, 6, 28, 28, 6, 6]);
    assert_eq!(storage_allowed.iter().sum::<usize>(), 75);
    assert_eq!(storage_refused, 249);
    assert_eq!(allowed - storage_allowed.iter().sum::<usize>(), 185);

    let wire = fixture().encode_exact();
    assert_eq!(&wire[0..4], b"WJR2");
    assert_eq!(u16::from_be_bytes(wire[4..6].try_into().unwrap()), 2);
    assert_eq!(wire[6], 1);
    assert_eq!(wire[7], 1);
    assert_eq!(
        u16::from_be_bytes(wire[1_839_444..1_839_446].try_into().unwrap()),
        1
    );
    let expected_capacities = [
        4_096, 524_288, 4_096, 128, 528_384, 1_310_720, 4_096, 320, 1_839_104, 131_072, 1_970_176,
        126_976, 65_536, 1_048_576, 32, 96, 284, 1,
    ];
    let actual_capacities = (0..18)
        .map(|index| {
            let offset = 80 + index * 4;
            u32::from_be_bytes(wire[offset..offset + 4].try_into().unwrap())
        })
        .collect::<Vec<_>>();
    assert_eq!(actual_capacities, expected_capacities);
    assert_eq!(
        capacity_digest(wire.as_ref()),
        [
            0x46, 0x8f, 0xd9, 0x4e, 0xb0, 0xc7, 0x20, 0x0e, 0x7a, 0x45, 0x62, 0x0d, 0xac, 0xb1,
            0x29, 0xf2, 0xac, 0xf5, 0xe8, 0x77, 0x8b, 0x25, 0x88, 0x91, 0x15, 0x37, 0x0a, 0x0c,
            0x05, 0x18, 0xc7, 0xf0,
        ]
    );
    assert_eq!(CleanupOutcomeV2::Passed.code(), 1);
    assert_eq!(CleanupOutcomeV2::Failed.code(), 2);
    assert_eq!(CleanupOutcomeV2::Unknown.code(), 3);
    assert_eq!(ReproofOutcomeV2::Passed.code(), 1);
    assert_eq!(ReproofOutcomeV2::Failed.code(), 2);
    assert_eq!(ReproofOutcomeV2::Unknown.code(), 3);
    assert_eq!(KernelDropKnowledgeV2::NotObserved.code(), 0);
    assert_eq!(KernelDropKnowledgeV2::Unknown.code(), 1);
}

#[test]
fn command_bound_rejection_is_the_only_wjr_construction_path() {
    let mut executing = Producer::<Dormant>::dormant()
        .validate_witness()
        .construct_authority()
        .begin_execution();
    for expected in 1..=256 {
        executing = executing
            .next_command(terminal(
                TerminalCodeV2::StorageCapacity,
                TerminalBoundaryV2::Command,
                TerminalPhaseV2::Execute,
            ))
            .unwrap();
        assert_eq!(executing.command_count(), expected);
    }
    let first_red = terminal(
        TerminalCodeV2::StorageCapacity,
        TerminalBoundaryV2::Command,
        TerminalPhaseV2::Execute,
    );
    let rejected = executing.next_command(first_red.clone()).unwrap_err();
    let encoded = fixture_with_rejected(rejected).encode_exact();
    let decoded = ValidatedWjrV2::decode_exact(encoded.as_ref()).unwrap();
    assert_eq!(decoded.terminal(), &first_red);
}
