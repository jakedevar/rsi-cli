//! Exact, fixed-layout WJR2 terminal-rejection codec.
#![allow(
    clippy::cast_possible_truncation,
    clippy::expect_used,
    clippy::ignored_unit_patterns,
    clippy::missing_const_for_fn,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::match_same_arms,
    clippy::similar_names,
    clippy::too_many_lines
)]

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    bounds::{WJR_CAPACITIES_V1, WJR2_BYTES, WjrCapacitiesV1},
    error::{TerminalBuildError, WjrBuildError, WjrDecodeError},
    policy::CanonicalUuid,
    state::{Producer, Rejected},
    witness::WitnessDigestV1,
};

const HEADER_END: usize = 4_096;
const DICTIONARY: usize = 4_096;
const DETAIL: usize = 528_384;
const FOOTER: usize = 1_839_104;
const ZERO_FILL: usize = 1_970_176;
const OVERFLOW_BIT: u32 = 1 << 14;

macro_rules! fixed_enum {
    ($name:ident : $ty:ty { $($variant:ident = $value:expr),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr($ty)]
        pub enum $name { $($variant = $value),+ }
        impl $name {
            const fn from_raw(value: $ty) -> Option<Self> { match value { $($value => Some(Self::$variant),)+ _ => None } }
            #[must_use] pub const fn code(self) -> $ty { self as $ty }
        }
    };
}

fixed_enum!(TerminalBoundaryV2: u8 {
    PreAuthority = 1, AuthorityConstruction = 2, Watch = 3, Command = 4,
    Normalize = 5, HeldOut = 6, Publication = 7, FinalReproof = 8, Finalization = 9
});
fixed_enum!(TerminalPhaseV2: u8 { Construct = 1, Execute = 2, Drain = 3, Cleanup = 4, Reproof = 5, Seal = 6 });
fixed_enum!(CleanupOutcomeV2: u8 { Passed = 1, Failed = 2, Unknown = 3 });
fixed_enum!(ReproofOutcomeV2: u8 { Passed = 1, Failed = 2, Unknown = 3 });
fixed_enum!(KernelDropKnowledgeV2: u8 { NotObserved = 0, Unknown = 1 });

fixed_enum!(TerminalCodeV2: u16 {
    UnsupportedPlatform = 0x0101, UnsupportedKernel = 0x0102,
    WitnessMalformed = 0x0201, WitnessNonCanonical = 0x0202, WitnessPolicy = 0x0203, WitnessDigest = 0x0204,
    AuthorityConstruction = 0x0301, AuthorityDiscovery = 0x0302, AuthorityReconciliation = 0x0303,
    WatchOverflow = 0x0401, WatchIgnored = 0x0402, WatchUnmounted = 0x0403, WatchDecode = 0x0404, WatchCeiling = 0x0405,
    StorageReserve = 0x0501, StorageOpen = 0x0502, StorageWrite = 0x0503, StorageFsync = 0x0504, StorageCapacity = 0x0505, StorageSeal = 0x0506,
    ChildBootstrap = 0x0601, ChildPipe = 0x0602, ChildTimeout = 0x0603, ChildReap = 0x0604, ChildGroupLive = 0x0605,
    ParserMalformed = 0x0701, ParserLimit = 0x0702, ParserSchema = 0x0703,
    NormalizationRejected = 0x0801, HeldOutRejected = 0x0901,
    CleanupTerminate = 0x0a01, CleanupKill = 0x0a02, CleanupReap = 0x0a03, CleanupResidual = 0x0a04,
    ReproofAuthority = 0x0b01, ReproofPathIdentity = 0x0b02, ReproofContentHash = 0x0b03, ReproofModeLink = 0x0b04,
    PublicationCandidate = 0x0c01, PublicationNoOverwrite = 0x0c02, PublicationFsync = 0x0c03, PublicationRollback = 0x0c04,
    Panic = 0x0d01
});

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveCommand {
    sequence: u64,
    argv_digest: [u8; 32],
    pid: i32,
    pgid: i32,
}

/// Closed terminal fact used to latch the first terminal rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFactV2 {
    code: TerminalCodeV2,
    boundary: TerminalBoundaryV2,
    phase: TerminalPhaseV2,
    cleanup: CleanupOutcomeV2,
    authority_reproof: ReproofOutcomeV2,
    path_reproof: ReproofOutcomeV2,
    content_reproof: ReproofOutcomeV2,
    active: Option<ActiveCommand>,
}

impl TerminalFactV2 {
    pub fn try_new(
        code: TerminalCodeV2,
        boundary: TerminalBoundaryV2,
        phase: TerminalPhaseV2,
        cleanup: CleanupOutcomeV2,
        authority_reproof: ReproofOutcomeV2,
        path_reproof: ReproofOutcomeV2,
        content_reproof: ReproofOutcomeV2,
    ) -> Result<Self, TerminalBuildError> {
        if !compatible(code, boundary, phase) {
            return Err(TerminalBuildError::IncompatibleCode);
        }
        Ok(Self {
            code,
            boundary,
            phase,
            cleanup,
            authority_reproof,
            path_reproof,
            content_reproof,
            active: None,
        })
    }
    pub fn with_active_command(
        mut self,
        sequence: u64,
        argv_digest: [u8; 32],
        pid: i32,
        pgid: i32,
    ) -> Result<Self, TerminalBuildError> {
        if sequence == 0 || argv_digest.iter().all(|b| *b == 0) || pid <= 0 || pgid <= 0 {
            return Err(TerminalBuildError::InvalidActiveCommand);
        }
        self.active = Some(ActiveCommand {
            sequence,
            argv_digest,
            pid,
            pgid,
        });
        Ok(self)
    }
    #[must_use]
    pub const fn code(&self) -> TerminalCodeV2 {
        self.code
    }
    #[must_use]
    pub const fn boundary(&self) -> TerminalBoundaryV2 {
        self.boundary
    }
    #[must_use]
    pub const fn phase(&self) -> TerminalPhaseV2 {
        self.phase
    }
    #[must_use]
    pub const fn cleanup_outcome(&self) -> CleanupOutcomeV2 {
        self.cleanup
    }
    #[must_use]
    pub const fn authority_reproof_outcome(&self) -> ReproofOutcomeV2 {
        self.authority_reproof
    }
}

fn compatible(code: TerminalCodeV2, boundary: TerminalBoundaryV2, phase: TerminalPhaseV2) -> bool {
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

/// One checked dictionary record; its fields never expose mutable wire memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchDictionaryEntryV2 {
    watch_id: u32,
    path: Vec<u8>,
    flags: u16,
    dev: u64,
    ino: u64,
}
impl WatchDictionaryEntryV2 {
    pub fn try_new(
        path: impl AsRef<[u8]>,
        watch_id: u32,
        flags: u16,
        dev: u64,
        ino: u64,
    ) -> Result<Self, WjrBuildError> {
        let path = path.as_ref();
        if watch_id == 0 || dev == 0 || ino == 0 || path.len() > 96 || !canonical_path(path) {
            return Err(WjrBuildError::Dictionary);
        }
        Ok(Self {
            watch_id,
            path: path.to_vec(),
            flags,
            dev,
            ino,
        })
    }
    #[must_use]
    pub const fn watch_id(&self) -> u32 {
        self.watch_id
    }
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }
}

/// One checked detail record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEventV2 {
    sequence: u64,
    monotonic_ns: u64,
    watch_id: u32,
    raw_mask: u32,
    cookie: u32,
    flags: u32,
    raw_leaf: Vec<u8>,
}
impl WatchEventV2 {
    pub fn try_new(
        sequence: u64,
        monotonic_ns: u64,
        watch_id: u32,
        raw_mask: u32,
        cookie: u32,
        flags: u32,
        raw_leaf: impl AsRef<[u8]>,
    ) -> Result<Self, WjrBuildError> {
        let raw_leaf = raw_leaf.as_ref();
        if raw_leaf.len() > 284 || (watch_id == 0 && raw_mask & OVERFLOW_BIT == 0) {
            return Err(WjrBuildError::Detail);
        }
        Ok(Self {
            sequence,
            monotonic_ns,
            watch_id,
            raw_mask,
            cookie,
            flags,
            raw_leaf: raw_leaf.to_vec(),
        })
    }
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    #[must_use]
    pub const fn watch_id(&self) -> u32 {
        self.watch_id
    }
    #[must_use]
    pub fn raw_leaf(&self) -> &[u8] {
        &self.raw_leaf
    }
}

/// Checked count equations and the supplied incremental raw-record digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchAccountingV2 {
    parsed_count: u64,
    detailed_count: u64,
    summarized_count: u64,
    raw_byte_count: u64,
    per_mask_counts: [u64; 32],
    overflow_marker_count: u64,
    dictionary_count: u32,
    kernel_drop: KernelDropKnowledgeV2,
    raw_digest: [u8; 32],
    mask_occurrence_count: u64,
}
impl WatchAccountingV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        parsed_count: u64,
        detailed_count: u64,
        summarized_count: u64,
        raw_byte_count: u64,
        per_mask_counts: [u64; 32],
        overflow_marker_count: u64,
        dictionary_count: u32,
        kernel_drop: KernelDropKnowledgeV2,
        raw_digest: [u8; 32],
    ) -> Result<Self, WjrBuildError> {
        if parsed_count > 65_536
            || raw_byte_count > 1_048_576
            || detailed_count != parsed_count.min(4_096)
            || summarized_count
                != parsed_count
                    .checked_sub(detailed_count)
                    .ok_or(WjrBuildError::Arithmetic)?
            || dictionary_count > 4_096
        {
            return Err(WjrBuildError::Accounting);
        }
        if per_mask_counts.iter().any(|count| *count > parsed_count)
            || overflow_marker_count != per_mask_counts[14]
            || overflow_marker_count > parsed_count
        {
            return Err(WjrBuildError::Accounting);
        }
        let mask_occurrence_count = per_mask_counts
            .iter()
            .try_fold(0_u64, |sum, count| sum.checked_add(*count))
            .ok_or(WjrBuildError::Arithmetic)?;
        if mask_occurrence_count
            > parsed_count
                .checked_mul(32)
                .ok_or(WjrBuildError::Arithmetic)?
            || (overflow_marker_count == 0) != (kernel_drop == KernelDropKnowledgeV2::NotObserved)
        {
            return Err(WjrBuildError::Accounting);
        }
        if parsed_count != 0 && raw_digest.iter().all(|byte| *byte == 0) {
            return Err(WjrBuildError::Digest);
        }
        Ok(Self {
            parsed_count,
            detailed_count,
            summarized_count,
            raw_byte_count,
            per_mask_counts,
            overflow_marker_count,
            dictionary_count,
            kernel_drop,
            raw_digest,
            mask_occurrence_count,
        })
    }
    #[must_use]
    pub const fn parsed_count(&self) -> u64 {
        self.parsed_count
    }
    #[must_use]
    pub const fn detailed_count(&self) -> u64 {
        self.detailed_count
    }
    #[must_use]
    pub const fn raw_byte_count(&self) -> u64 {
        self.raw_byte_count
    }
    #[must_use]
    pub const fn raw_record_digest(&self) -> &[u8; 32] {
        &self.raw_digest
    }
}

/// A complete validated WJR2 envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedWjrV2 {
    witness: [u8; 32],
    run_nonce: CanonicalUuid,
    creation_dev: u64,
    creation_ino: u64,
    terminal: TerminalFactV2,
    dictionary: Vec<WatchDictionaryEntryV2>,
    details: Vec<WatchEventV2>,
    accounting: WatchAccountingV2,
}
impl ValidatedWjrV2 {
    /// ```compile_fail
    /// use rsi_baseline::{CanonicalUuid, TerminalFactV2, ValidatedWjrV2, WatchAccountingV2, WatchDictionaryEntryV2, WatchEventV2, WitnessDigestV1};
    /// let _: fn(WitnessDigestV1, CanonicalUuid, u64, u64, TerminalFactV2, Vec<WatchDictionaryEntryV2>, Vec<WatchEventV2>, WatchAccountingV2) -> _ = ValidatedWjrV2::try_new;
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        witness: WitnessDigestV1,
        run_nonce: CanonicalUuid,
        creation_dev: u64,
        creation_ino: u64,
        rejected: Producer<Rejected>,
        dictionary: Vec<WatchDictionaryEntryV2>,
        details: Vec<WatchEventV2>,
        accounting: WatchAccountingV2,
    ) -> Result<Self, WjrBuildError> {
        if creation_dev == 0
            || creation_ino == 0
            || dictionary.len() != accounting.dictionary_count as usize
            || details.len() != accounting.detailed_count as usize
        {
            return Err(WjrBuildError::Capacity);
        }
        validate_records(&dictionary, &details).map_err(|_| WjrBuildError::Reference)?;
        let retained_masks = detail_mask_counts(&details);
        if !mask_subset(&retained_masks, &accounting.per_mask_counts)
            || (accounting.parsed_count == accounting.detailed_count
                && retained_masks != accounting.per_mask_counts)
        {
            return Err(WjrBuildError::Accounting);
        }
        Ok(Self {
            witness: *witness.as_bytes(),
            run_nonce,
            creation_dev,
            creation_ino,
            terminal: rejected.into_terminal_fact(),
            dictionary,
            details,
            accounting,
        })
    }
    #[must_use]
    pub fn accounting(&self) -> &WatchAccountingV2 {
        &self.accounting
    }
    #[must_use]
    pub fn terminal(&self) -> &TerminalFactV2 {
        &self.terminal
    }
    #[must_use]
    pub fn dictionary(&self) -> &[WatchDictionaryEntryV2] {
        &self.dictionary
    }
    #[must_use]
    pub fn details(&self) -> &[WatchEventV2] {
        &self.details
    }
    #[must_use]
    pub const fn capacities() -> WjrCapacitiesV1 {
        WJR_CAPACITIES_V1
    }
    #[must_use]
    pub fn encode_exact(&self) -> Box<[u8; WJR2_BYTES]> {
        let bytes: Box<[u8]> = vec![0_u8; WJR2_BYTES].into_boxed_slice();
        let mut output: Box<[u8; WJR2_BYTES]> = bytes
            .try_into()
            .expect("fixed WJR2 envelope allocation length");
        output[0..4].copy_from_slice(b"WJR2");
        put_u16(output.as_mut(), 4, 2);
        output[6] = 1;
        output[7] = 1;
        put_u32(output.as_mut(), 8, 4_096);
        put_u32(output.as_mut(), 12, WJR2_BYTES as u32);
        output[16..48].copy_from_slice(&self.witness);
        let uuid = Uuid::parse_str(self.run_nonce.as_str()).expect("validated UUID");
        output[48..64].copy_from_slice(uuid.as_bytes());
        put_u64(output.as_mut(), 64, self.creation_dev);
        put_u64(output.as_mut(), 72, self.creation_ino);
        for (index, value) in capacity_fields().iter().enumerate() {
            put_u32(output.as_mut(), 80 + index * 4, *value);
        }
        output[152..184].copy_from_slice(&capacity_digest());
        put_u16(output.as_mut(), 184, self.terminal.code.code());
        output[186] = self.terminal.boundary.code();
        output[187] = self.terminal.phase.code();
        output[188] = 1;
        if let Some(active) = &self.terminal.active {
            output[189] = 1;
            put_u64(output.as_mut(), 192, active.sequence);
            output[200..232].copy_from_slice(&active.argv_digest);
            put_i32(output.as_mut(), 232, active.pid);
            put_i32(output.as_mut(), 236, active.pgid);
        }
        for (slot, entry) in self.dictionary.iter().enumerate() {
            encode_dictionary(
                &mut output[DICTIONARY + slot * 128..DICTIONARY + (slot + 1) * 128],
                entry,
            );
        }
        for (slot, event) in self.details.iter().enumerate() {
            encode_detail(
                &mut output[DETAIL + slot * 320..DETAIL + (slot + 1) * 320],
                event,
            );
        }
        let footer = &mut output[FOOTER..ZERO_FILL];
        put_u64(footer, 0, self.accounting.parsed_count);
        put_u64(footer, 8, self.accounting.detailed_count);
        put_u64(footer, 16, self.accounting.summarized_count);
        put_u64(footer, 24, self.accounting.raw_byte_count);
        for (index, count) in self.accounting.per_mask_counts.iter().enumerate() {
            put_u64(footer, 32 + index * 8, *count);
        }
        put_u64(footer, 288, self.accounting.overflow_marker_count);
        put_u32(footer, 296, self.accounting.dictionary_count);
        put_u16(footer, 300, self.terminal.code.code());
        footer[302] = 1;
        footer[303] = self.accounting.kernel_drop.code();
        footer[304..336].copy_from_slice(&self.accounting.raw_digest);
        footer[336] = self.terminal.cleanup.code();
        footer[337] = self.terminal.authority_reproof.code();
        footer[338] = self.terminal.path_reproof.code();
        footer[339] = self.terminal.content_reproof.code();
        put_u16(footer, 340, 1);
        footer[344..376].copy_from_slice(&capacity_digest());
        footer[376..408].copy_from_slice(&self.witness);
        put_u64(footer, 408, self.accounting.mask_occurrence_count);
        output
    }
    pub fn decode_exact(input: &[u8]) -> Result<Self, WjrDecodeError> {
        if input.len() != WJR2_BYTES {
            return Err(WjrDecodeError::WrongLength);
        }
        if &input[0..4] != b"WJR2" {
            return Err(WjrDecodeError::Magic);
        }
        if get_u16(input, 4) != 2 {
            return Err(WjrDecodeError::Version);
        }
        if input[6] != 1 {
            return Err(WjrDecodeError::Endian);
        }
        if input[7] != 1 || get_u16(input, FOOTER + 340) != 1 {
            return Err(WjrDecodeError::Schema);
        }
        if get_u32(input, 8) != 4_096 || get_u32(input, 12) != WJR2_BYTES as u32 {
            return Err(WjrDecodeError::UnsupportedCapacityProfile);
        }
        if capacity_fields()
            .iter()
            .enumerate()
            .any(|(index, value)| get_u32(input, 80 + index * 4) != *value)
        {
            return Err(WjrDecodeError::UnsupportedCapacityProfile);
        }
        let cap = capacity_digest();
        if input[152..184] != cap || input[FOOTER + 344..FOOTER + 376] != cap {
            return Err(WjrDecodeError::CapacityDigest);
        }
        if !zeros(&input[240..HEADER_END])
            || !zeros(&input[190..192])
            || !zeros(&input[FOOTER + 342..FOOTER + 344])
            || !zeros(&input[ZERO_FILL..])
        {
            return Err(WjrDecodeError::NonzeroReserved);
        }
        let terminal = decode_terminal(input)?;
        let mut witness = [0; 32];
        witness.copy_from_slice(&input[16..48]);
        if input[FOOTER + 376..FOOTER + 408] != witness {
            return Err(WjrDecodeError::Digest);
        }
        let run_nonce = CanonicalUuid::parse(
            Uuid::from_bytes(input[48..64].try_into().expect("fixed")).to_string(),
        )
        .map_err(|_| WjrDecodeError::Shape)?;
        let dictionary_count = get_u32(input, FOOTER + 296);
        let detailed_count = get_u64(input, FOOTER + 8);
        if dictionary_count > 4_096 || detailed_count > 4_096 {
            return Err(WjrDecodeError::Shape);
        }
        let mut dictionary = Vec::with_capacity(dictionary_count as usize);
        for slot in 0..4_096 {
            let record = &input[DICTIONARY + slot * 128..DICTIONARY + (slot + 1) * 128];
            if slot < dictionary_count as usize {
                dictionary.push(decode_dictionary(record)?);
            } else if !zeros(record) {
                return Err(WjrDecodeError::NonzeroReserved);
            }
        }
        let mut details = Vec::with_capacity(detailed_count as usize);
        for slot in 0..4_096 {
            let record = &input[DETAIL + slot * 320..DETAIL + (slot + 1) * 320];
            if slot < detailed_count as usize {
                details.push(decode_detail(record)?);
            } else if !zeros(record) {
                return Err(WjrDecodeError::NonzeroReserved);
            }
        }
        let footer = &input[FOOTER..ZERO_FILL];
        if !zeros(&footer[416..]) {
            return Err(WjrDecodeError::NonzeroReserved);
        }
        if get_u16(footer, 300) != terminal.code.code() || footer[302] != 1 {
            return Err(WjrDecodeError::TerminalFinality);
        }
        let kernel =
            KernelDropKnowledgeV2::from_raw(footer[303]).ok_or(WjrDecodeError::UnknownCode)?;
        let mut masks = [0; 32];
        for (index, count) in masks.iter_mut().enumerate() {
            *count = get_u64(footer, 32 + index * 8);
        }
        let mut digest = [0; 32];
        digest.copy_from_slice(&footer[304..336]);
        let accounting = WatchAccountingV2::try_new(
            get_u64(footer, 0),
            detailed_count,
            get_u64(footer, 16),
            get_u64(footer, 24),
            masks,
            get_u64(footer, 288),
            dictionary_count,
            kernel,
            digest,
        )
        .map_err(|error| match error {
            WjrBuildError::Accounting | WjrBuildError::Arithmetic => WjrDecodeError::Equation,
            WjrBuildError::Digest => WjrDecodeError::Digest,
            WjrBuildError::Reference => WjrDecodeError::Reference,
            WjrBuildError::Capacity => WjrDecodeError::UnsupportedCapacityProfile,
            WjrBuildError::Dictionary
            | WjrBuildError::Detail
            | WjrBuildError::TerminalCoherence => WjrDecodeError::Shape,
        })?;
        if accounting.mask_occurrence_count != get_u64(footer, 408) {
            return Err(WjrDecodeError::Equation);
        }
        validate_records(&dictionary, &details).map_err(|_| WjrDecodeError::Reference)?;
        let retained_masks = detail_mask_counts(&details);
        if !mask_subset(&retained_masks, &accounting.per_mask_counts)
            || (accounting.parsed_count == detailed_count
                && retained_masks != accounting.per_mask_counts)
        {
            return Err(WjrDecodeError::Equation);
        }
        if get_u64(input, 64) == 0 || get_u64(input, 72) == 0 {
            return Err(WjrDecodeError::Shape);
        }
        Ok(Self {
            witness,
            run_nonce,
            creation_dev: get_u64(input, 64),
            creation_ino: get_u64(input, 72),
            terminal,
            dictionary,
            details,
            accounting,
        })
    }
}

fn canonical_path(path: &[u8]) -> bool {
    !path.contains(&0)
        && std::str::from_utf8(path).is_ok_and(|value| {
            value == "/"
                || (value.starts_with('/')
                    && !value.ends_with('/')
                    && value
                        .split('/')
                        .skip(1)
                        .all(|part| !part.is_empty() && part != "." && part != ".."))
        })
}
fn validate_records(
    dictionary: &[WatchDictionaryEntryV2],
    details: &[WatchEventV2],
) -> Result<(), ()> {
    let mut prior = 0;
    for entry in dictionary {
        if entry.watch_id <= prior {
            return Err(());
        }
        prior = entry.watch_id;
    }
    for (slot, event) in details.iter().enumerate() {
        if event.sequence != slot as u64
            || (event.watch_id != 0
                && !dictionary
                    .iter()
                    .any(|entry| entry.watch_id == event.watch_id))
        {
            return Err(());
        }
    }
    Ok(())
}
fn encode_dictionary(out: &mut [u8], entry: &WatchDictionaryEntryV2) {
    put_u32(out, 0, entry.watch_id);
    put_u16(out, 4, entry.path.len() as u16);
    put_u16(out, 6, entry.flags);
    put_u64(out, 8, entry.dev);
    put_u64(out, 16, entry.ino);
    out[24..24 + entry.path.len()].copy_from_slice(&entry.path);
}
fn decode_dictionary(record: &[u8]) -> Result<WatchDictionaryEntryV2, WjrDecodeError> {
    if !zeros(&record[120..]) {
        return Err(WjrDecodeError::NonzeroReserved);
    }
    let length = get_u16(record, 4) as usize;
    if length > 96 || !zeros(&record[24 + length..120]) {
        return Err(WjrDecodeError::Shape);
    }
    WatchDictionaryEntryV2::try_new(
        &record[24..24 + length],
        get_u32(record, 0),
        get_u16(record, 6),
        get_u64(record, 8),
        get_u64(record, 16),
    )
    .map_err(|_| WjrDecodeError::Shape)
}
fn encode_detail(out: &mut [u8], event: &WatchEventV2) {
    put_u64(out, 0, event.sequence);
    put_u64(out, 8, event.monotonic_ns);
    put_u32(out, 16, event.watch_id);
    put_u32(out, 20, event.raw_mask);
    put_u32(out, 24, event.cookie);
    put_u32(out, 28, event.flags);
    put_u16(out, 32, event.raw_leaf.len() as u16);
    out[36..36 + event.raw_leaf.len()].copy_from_slice(&event.raw_leaf);
}
fn decode_detail(record: &[u8]) -> Result<WatchEventV2, WjrDecodeError> {
    if !zeros(&record[34..36]) {
        return Err(WjrDecodeError::NonzeroReserved);
    }
    let length = get_u16(record, 32) as usize;
    if length > 284 || !zeros(&record[36 + length..]) {
        return Err(WjrDecodeError::Shape);
    }
    WatchEventV2::try_new(
        get_u64(record, 0),
        get_u64(record, 8),
        get_u32(record, 16),
        get_u32(record, 20),
        get_u32(record, 24),
        get_u32(record, 28),
        &record[36..36 + length],
    )
    .map_err(|_| WjrDecodeError::Shape)
}
fn decode_terminal(input: &[u8]) -> Result<TerminalFactV2, WjrDecodeError> {
    if input[188] != 1 {
        return Err(WjrDecodeError::TerminalFinality);
    }
    let code = TerminalCodeV2::from_raw(get_u16(input, 184)).ok_or(WjrDecodeError::UnknownCode)?;
    let boundary = TerminalBoundaryV2::from_raw(input[186]).ok_or(WjrDecodeError::UnknownCode)?;
    let phase = TerminalPhaseV2::from_raw(input[187]).ok_or(WjrDecodeError::UnknownCode)?;
    let footer = &input[FOOTER..ZERO_FILL];
    let cleanup = CleanupOutcomeV2::from_raw(footer[336]).ok_or(WjrDecodeError::UnknownCode)?;
    let authority = ReproofOutcomeV2::from_raw(footer[337]).ok_or(WjrDecodeError::UnknownCode)?;
    let path = ReproofOutcomeV2::from_raw(footer[338]).ok_or(WjrDecodeError::UnknownCode)?;
    let content = ReproofOutcomeV2::from_raw(footer[339]).ok_or(WjrDecodeError::UnknownCode)?;
    let mut terminal =
        TerminalFactV2::try_new(code, boundary, phase, cleanup, authority, path, content)
            .map_err(|_| WjrDecodeError::TerminalFinality)?;
    match input[189] {
        0 => {
            if !zeros(&input[192..240]) {
                return Err(WjrDecodeError::Shape);
            }
        }
        1 => {
            let mut digest = [0; 32];
            digest.copy_from_slice(&input[200..232]);
            terminal = terminal
                .with_active_command(
                    get_u64(input, 192),
                    digest,
                    get_i32(input, 232),
                    get_i32(input, 236),
                )
                .map_err(|_| WjrDecodeError::Shape)?;
        }
        _ => return Err(WjrDecodeError::Shape),
    }
    Ok(terminal)
}
fn detail_mask_counts(details: &[WatchEventV2]) -> [u64; 32] {
    let mut counts = [0; 32];
    for detail in details {
        for (bit, count) in counts.iter_mut().enumerate() {
            if detail.raw_mask & (1_u32 << bit) != 0 {
                *count += 1;
            }
        }
    }
    counts
}
fn mask_subset(retained: &[u64; 32], totals: &[u64; 32]) -> bool {
    retained
        .iter()
        .zip(totals)
        .all(|(detail, total)| detail <= total)
}
fn capacity_fields() -> [u32; 18] {
    [
        4_096, 524_288, 4_096, 128, 528_384, 1_310_720, 4_096, 320, 1_839_104, 131_072, 1_970_176,
        126_976, 65_536, 1_048_576, 32, 96, 284, 1,
    ]
}
fn capacity_digest() -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"rsi-baseline/wjr2-capacities/v1\0");
    hash.update(4_096_u32.to_be_bytes());
    hash.update((WJR2_BYTES as u32).to_be_bytes());
    for field in capacity_fields() {
        hash.update(field.to_be_bytes());
    }
    hash.finalize().into()
}
fn zeros(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0)
}
fn put_u16(target: &mut [u8], offset: usize, value: u16) {
    target[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}
fn put_u32(target: &mut [u8], offset: usize, value: u32) {
    target[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
fn put_u64(target: &mut [u8], offset: usize, value: u64) {
    target[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}
fn put_i32(target: &mut [u8], offset: usize, value: i32) {
    target[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
fn get_u16(source: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(source[offset..offset + 2].try_into().expect("fixed"))
}
fn get_u32(source: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(source[offset..offset + 4].try_into().expect("fixed"))
}
fn get_u64(source: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(source[offset..offset + 8].try_into().expect("fixed"))
}
fn get_i32(source: &[u8], offset: usize) -> i32 {
    i32::from_be_bytes(source[offset..offset + 4].try_into().expect("fixed"))
}
