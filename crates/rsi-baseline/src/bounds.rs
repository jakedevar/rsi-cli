//! Immutable, dimension-bound admission limits for the dormant contract.

use crate::error::WitnessBuildError;

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundProfileV1 {
    pub witness_bytes: u64,
    pub preflight_json_bytes: u64,
    pub enumeration_bytes: u64,
    pub capture_bytes: u64,
    pub json_bytes: u64,
    pub raw_stream_bytes: u64,
    pub aggregate_evidence_bytes: u64,
    pub terminal_reserve_bytes: u64,
    pub summary_reserve_bytes: u64,
    pub ordinary_evidence_bytes: u64,
    pub generated_baseline_bytes: u64,
    pub json_depth: u64,
    pub json_nodes: u64,
    pub json_container_items: u64,
    pub suites: u64,
    pub tests: u64,
    pub identities: u64,
    pub samples: u64,
    pub string_bytes: u64,
    pub raw_line_bytes: u64,
    pub lines_per_stream: u64,
    pub hash_chunk_bytes: u64,
    pub journal_events: u64,
    pub journal_line_bytes: u64,
    pub watch_raw_bytes: u64,
    pub watch_parsed_events: u64,
    pub watched_directories: u64,
    pub retained_source_files: u64,
    pub producer_fds: u64,
    pub reserved_cleanup_fds: u64,
    pub commands: u64,
    pub live_groups: u64,
    pub descendant_tasks: u64,
    pub child_fds: u64,
    pub core_bytes: u64,
    pub per_command_deadline_seconds: u64,
    pub whole_run_deadline_seconds: u64,
    pub run_root_mode: u32,
    pub regular_file_mode: u32,
}

pub const BOUNDS_V1: BoundProfileV1 = BoundProfileV1 {
    witness_bytes: 64 * 1024,
    preflight_json_bytes: 4 * MIB,
    enumeration_bytes: 64 * MIB,
    capture_bytes: 64 * MIB,
    json_bytes: 64 * MIB,
    raw_stream_bytes: 64 * MIB,
    aggregate_evidence_bytes: GIB,
    terminal_reserve_bytes: 2 * MIB,
    summary_reserve_bytes: 64 * 1024,
    ordinary_evidence_bytes: GIB - (2 * MIB) - (64 * 1024),
    generated_baseline_bytes: 128 * MIB,
    json_depth: 64,
    json_nodes: 3_000_000,
    json_container_items: 200_000,
    suites: 512,
    tests: 100_000,
    identities: 100_000,
    samples: 5,
    string_bytes: MIB,
    raw_line_bytes: MIB,
    lines_per_stream: 404_096,
    hash_chunk_bytes: 64 * 1024,
    journal_events: 65_536,
    journal_line_bytes: MIB,
    watch_raw_bytes: MIB,
    watch_parsed_events: 65_536,
    watched_directories: 4_096,
    retained_source_files: 16_384,
    producer_fds: 24_576,
    reserved_cleanup_fds: 256,
    commands: 256,
    live_groups: 1,
    descendant_tasks: 1_024,
    child_fds: 4_096,
    core_bytes: 0,
    per_command_deadline_seconds: 45 * 60,
    whole_run_deadline_seconds: 8 * 60 * 60,
    run_root_mode: 0o700,
    regular_file_mode: 0o600,
};

/// The sole admitted fixed WJR2 envelope profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WjrCapacitiesV1 {
    header_offset: u32,
    header_bytes: u32,
    total_file_bytes: u32,
    dictionary_offset: u32,
    dictionary_bytes: u32,
    dictionary_records: u32,
    dictionary_record_bytes: u32,
    detail_offset: u32,
    detail_bytes: u32,
    detail_records: u32,
    detail_record_bytes: u32,
    footer_offset: u32,
    footer_bytes: u32,
    zero_fill_offset: u32,
    zero_fill_bytes: u32,
    maximum_parsed_events: u32,
    maximum_raw_watch_bytes: u32,
    per_mask_slots: u32,
    maximum_watch_path_bytes: u32,
    maximum_detail_raw_leaf_bytes: u32,
    capacity_schema_version: u32,
}

pub const WJR_CAPACITIES_V1: WjrCapacitiesV1 = WjrCapacitiesV1 {
    header_offset: 0,
    header_bytes: 4_096,
    total_file_bytes: 2_097_152,
    dictionary_offset: 4_096,
    dictionary_bytes: 524_288,
    dictionary_records: 4_096,
    dictionary_record_bytes: 128,
    detail_offset: 528_384,
    detail_bytes: 1_310_720,
    detail_records: 4_096,
    detail_record_bytes: 320,
    footer_offset: 1_839_104,
    footer_bytes: 131_072,
    zero_fill_offset: 1_970_176,
    zero_fill_bytes: 126_976,
    maximum_parsed_events: 65_536,
    maximum_raw_watch_bytes: 1_048_576,
    per_mask_slots: 32,
    maximum_watch_path_bytes: 96,
    maximum_detail_raw_leaf_bytes: 284,
    capacity_schema_version: 1,
};

pub const WJR2_BYTES: usize = WJR_CAPACITIES_V1.total_file_bytes as usize;

macro_rules! capacity_getters {
    ($($field:ident),+ $(,)?) => { $(
        #[must_use] pub const fn $field(self) -> u32 { self.$field }
    )+ };
}
impl WjrCapacitiesV1 {
    capacity_getters!(
        header_offset,
        header_bytes,
        total_file_bytes,
        dictionary_offset,
        dictionary_bytes,
        dictionary_records,
        dictionary_record_bytes,
        detail_offset,
        detail_bytes,
        detail_records,
        detail_record_bytes,
        footer_offset,
        footer_bytes,
        zero_fill_offset,
        zero_fill_bytes,
        maximum_parsed_events,
        maximum_raw_watch_bytes,
        per_mask_slots,
        maximum_watch_path_bytes,
        maximum_detail_raw_leaf_bytes,
        capacity_schema_version
    );
}

macro_rules! wjr_dimension {
    ($name:ident, $maximum:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(u64);
        impl $name {
            pub const MAXIMUM: u64 = $maximum;
            pub const fn try_new(value: u64) -> Result<Self, crate::error::WjrBuildError> {
                if value <= Self::MAXIMUM {
                    Ok(Self(value))
                } else {
                    Err(crate::error::WjrBuildError::Capacity)
                }
            }
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

wjr_dimension!(
    WatchParsedCount,
    WJR_CAPACITIES_V1.maximum_parsed_events as u64
);
wjr_dimension!(
    WatchRawBytes,
    WJR_CAPACITIES_V1.maximum_raw_watch_bytes as u64
);
wjr_dimension!(
    WatchDictionaryCount,
    WJR_CAPACITIES_V1.dictionary_records as u64
);
wjr_dimension!(WatchDetailCount, WJR_CAPACITIES_V1.detail_records as u64);
wjr_dimension!(
    WatchPathBytes,
    WJR_CAPACITIES_V1.maximum_watch_path_bytes as u64
);
wjr_dimension!(
    WatchRawLeafBytes,
    WJR_CAPACITIES_V1.maximum_detail_raw_leaf_bytes as u64
);

const _: () = assert!(BOUNDS_V1.witness_bytes <= BOUNDS_V1.preflight_json_bytes);
const _: () = assert!(BOUNDS_V1.producer_fds > BOUNDS_V1.reserved_cleanup_fds);
const _: () = assert!(BOUNDS_V1.live_groups == 1);
const _: () =
    assert!(BOUNDS_V1.per_command_deadline_seconds < BOUNDS_V1.whole_run_deadline_seconds);
const _: () = assert!(
    BOUNDS_V1.ordinary_evidence_bytes
        + BOUNDS_V1.terminal_reserve_bytes
        + BOUNDS_V1.summary_reserve_bytes
        == BOUNDS_V1.aggregate_evidence_bytes
);

macro_rules! dimension {
    ($name:ident, $maximum:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(u64);
        impl $name {
            pub const MAXIMUM: u64 = $maximum;
            pub const fn try_new(value: u64) -> Result<Self, WitnessBuildError> {
                if value > Self::MAXIMUM {
                    Err(WitnessBuildError::Invalid {
                        field: stringify!($name),
                    })
                } else {
                    Ok(Self(value))
                }
            }
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
            pub const fn checked_add(self, rhs: u64) -> Result<Self, WitnessBuildError> {
                match self.0.checked_add(rhs) {
                    Some(value) => Self::try_new(value),
                    None => Err(WitnessBuildError::Arithmetic),
                }
            }
            pub const fn checked_sub(self, rhs: u64) -> Result<Self, WitnessBuildError> {
                match self.0.checked_sub(rhs) {
                    Some(value) => Ok(Self(value)),
                    None => Err(WitnessBuildError::Arithmetic),
                }
            }
        }
    };
}

dimension!(WitnessBytes, BOUNDS_V1.witness_bytes);
dimension!(StringBytes, BOUNDS_V1.string_bytes);
dimension!(PolicyItems, BOUNDS_V1.json_container_items);
dimension!(JsonDepth, BOUNDS_V1.json_depth);
dimension!(JsonNodes, BOUNDS_V1.json_nodes);
dimension!(JsonContainerItems, BOUNDS_V1.json_container_items);
dimension!(PreflightJsonBytes, BOUNDS_V1.preflight_json_bytes);
dimension!(AggregateEvidenceBytes, BOUNDS_V1.aggregate_evidence_bytes);
dimension!(TerminalReserveBytes, BOUNDS_V1.terminal_reserve_bytes);
dimension!(SummaryReserveBytes, BOUNDS_V1.summary_reserve_bytes);
dimension!(OrdinaryEvidenceBytes, BOUNDS_V1.ordinary_evidence_bytes);
dimension!(ProducerFds, BOUNDS_V1.producer_fds);
dimension!(CleanupReserveFds, BOUNDS_V1.reserved_cleanup_fds);
dimension!(LiveGroups, BOUNDS_V1.live_groups);
dimension!(PerCommandDeadline, BOUNDS_V1.per_command_deadline_seconds);
dimension!(WholeRunDeadline, BOUNDS_V1.whole_run_deadline_seconds);
