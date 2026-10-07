//! Strict shared wire contract for sandbox target-cache maintenance.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Historical report shape emitted before durable sweep state existed.
pub const SANDBOX_BUILD_CACHE_REPORT_VERSION: u8 = 1;
pub const SANDBOX_BUILD_CACHE_REPORT_VERSION_V2: u8 = 2;
pub const SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MIN: u64 = 1;
pub const SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MAX: u64 = 30 * 24 * 60 * 60;
pub const SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MIN: u64 = 60;
pub const SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MAX: u64 = 24 * 60 * 60;
pub const SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MIN: u8 = 2;
pub const SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MAX: u8 = 99;
pub const SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MIN: u8 = 1;
pub const SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MAX: u8 = 98;
pub const SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MIN: u32 = 1;
pub const SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MAX: u32 = 1024;
/// Refusal samples one pass reports, and how many of them one skip reason may
/// hold, so a single noisy reason cannot hide the others (#1575).
pub const SANDBOX_BUILD_CACHE_REFUSAL_SAMPLES_MAX: usize = 24;
pub const SANDBOX_BUILD_CACHE_REFUSAL_SAMPLES_PER_REASON: usize = 4;
/// Longest subject (a top-level `target/` entry name, a process id) a refusal
/// check carries; longer names are cut, never rejected.
pub const SANDBOX_RECLAIM_CHECK_SUBJECT_MAX: usize = 40;
const SANDBOX_RECLAIM_CHECK_TOKEN_MAX: usize = 96;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxBuildCacheReclaimConfig {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub interval_secs: u64,
    pub high_watermark_pct: u8,
    pub low_watermark_pct: u8,
    pub max_candidates: u32,
}

impl SandboxBuildCacheReclaimConfig {
    pub fn validate(self) -> Result<Self, &'static str> {
        if !(SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MIN..=SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MAX)
            .contains(&self.ttl_secs)
        {
            return Err("target-cache TTL is outside 1..=2592000 seconds");
        }
        if !(SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MIN
            ..=SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MAX)
            .contains(&self.interval_secs)
        {
            return Err("target-cache interval is outside 60..=86400 seconds");
        }
        if !(SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MIN
            ..=SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MAX)
            .contains(&self.high_watermark_pct)
        {
            return Err("target-cache high watermark is outside 2..=99");
        }
        if !(SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MIN
            ..=SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MAX)
            .contains(&self.low_watermark_pct)
        {
            return Err("target-cache low watermark is outside 1..=98");
        }
        if self.low_watermark_pct >= self.high_watermark_pct {
            return Err("low watermark must be less than high watermark");
        }
        if !(SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MIN
            ..=SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MAX)
            .contains(&self.max_candidates)
        {
            return Err("target-cache candidate budget is outside 1..=1024");
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxFilesystemStats {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub used_percent: u8,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBuildCacheReclaimSkipReason {
    FreshOrInvalidTimestamp,
    ActiveOwner,
    ActiveMapBusy,
    StoreBusy,
    RootBusy,
    CustodyOrGenerationDrift,
    GitOrRootIdentityRefusal,
    TargetAbsent,
    TargetNotDirectory,
    TargetSymlink,
    MountOrDeviceCrossing,
    TargetIdentityChanged,
    StageConflict,
    InvalidRecoveryEntry,
    RejectedRecoveryEntry,
    Openat2Unavailable,
    UnreadableEntry,
    StagedDeletionIncomplete,
    RecoveryEntryBudget,
    FilesystemEntryBudget,
    ByteBudget,
    DurationBudget,
    DepthBudget,
    /// An enabled wake, pending restart intent, manager seat or running job
    /// still needs the owner's `target/` (#1564).
    LiveConsumer,
    /// `target/` holds a top-level entry that is neither Cargo output nor RSI
    /// scratch; it may be someone's state, so the whole tree is retained.
    TargetUnrecognizedContent,
}

impl SandboxBuildCacheReclaimSkipReason {
    /// Every variant, in declaration order.
    pub const ALL: [Self; 25] = [
        Self::FreshOrInvalidTimestamp,
        Self::ActiveOwner,
        Self::ActiveMapBusy,
        Self::StoreBusy,
        Self::RootBusy,
        Self::CustodyOrGenerationDrift,
        Self::GitOrRootIdentityRefusal,
        Self::TargetAbsent,
        Self::TargetNotDirectory,
        Self::TargetSymlink,
        Self::MountOrDeviceCrossing,
        Self::TargetIdentityChanged,
        Self::StageConflict,
        Self::InvalidRecoveryEntry,
        Self::RejectedRecoveryEntry,
        Self::Openat2Unavailable,
        Self::UnreadableEntry,
        Self::StagedDeletionIncomplete,
        Self::RecoveryEntryBudget,
        Self::FilesystemEntryBudget,
        Self::ByteBudget,
        Self::DurationBudget,
        Self::DepthBudget,
        Self::LiveConsumer,
        Self::TargetUnrecognizedContent,
    ];

    /// The serde spelling, for reports and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FreshOrInvalidTimestamp => "fresh_or_invalid_timestamp",
            Self::ActiveOwner => "active_owner",
            Self::ActiveMapBusy => "active_map_busy",
            Self::StoreBusy => "store_busy",
            Self::RootBusy => "root_busy",
            Self::CustodyOrGenerationDrift => "custody_or_generation_drift",
            Self::GitOrRootIdentityRefusal => "git_or_root_identity_refusal",
            Self::TargetAbsent => "target_absent",
            Self::TargetNotDirectory => "target_not_directory",
            Self::TargetSymlink => "target_symlink",
            Self::MountOrDeviceCrossing => "mount_or_device_crossing",
            Self::TargetIdentityChanged => "target_identity_changed",
            Self::StageConflict => "stage_conflict",
            Self::InvalidRecoveryEntry => "invalid_recovery_entry",
            Self::RejectedRecoveryEntry => "rejected_recovery_entry",
            Self::Openat2Unavailable => "openat2_unavailable",
            Self::UnreadableEntry => "unreadable_entry",
            Self::StagedDeletionIncomplete => "staged_deletion_incomplete",
            Self::RecoveryEntryBudget => "recovery_entry_budget",
            Self::FilesystemEntryBudget => "filesystem_entry_budget",
            Self::ByteBudget => "byte_budget",
            Self::DurationBudget => "duration_budget",
            Self::DepthBudget => "depth_budget",
            Self::LiveConsumer => "live_consumer",
            Self::TargetUnrecognizedContent => "target_unrecognized_content",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBuildCacheReclaimStopReason {
    Disabled,
    NoCandidates,
    Completed,
    LowWatermark,
    CandidateBudget,
    AllRefused,
    RecoveryOnly,
    RecoveryEntryBudget,
    FilesystemEntryBudget,
    ByteBudget,
    DurationBudget,
    DepthBudget,
}

/// The exact check that refused one reclaim candidate: a stable snake_case
/// name, optionally with a short subject (the offending top-level `target/`
/// entry, a process id). `Copy` so it rides in the store's reclaim outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SandboxReclaimCheck {
    name: &'static str,
    subject: [u8; SANDBOX_RECLAIM_CHECK_SUBJECT_MAX],
    subject_len: u8,
}

impl SandboxReclaimCheck {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            subject: [0; SANDBOX_RECLAIM_CHECK_SUBJECT_MAX],
            subject_len: 0,
        }
    }

    /// Attach a subject. Bytes outside printable ASCII (and the `:` separator)
    /// become `?`, so an arbitrary file name is safe to log and report.
    pub fn with_subject(name: &'static str, subject: impl AsRef<[u8]>) -> Self {
        let mut check = Self::new(name);
        for byte in subject
            .as_ref()
            .iter()
            .take(SANDBOX_RECLAIM_CHECK_SUBJECT_MAX)
        {
            check.subject[usize::from(check.subject_len)] =
                if byte.is_ascii_graphic() && *byte != b':' {
                    *byte
                } else {
                    b'?'
                };
            check.subject_len += 1;
        }
        check
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn subject(&self) -> Option<&str> {
        (self.subject_len > 0)
            .then(|| std::str::from_utf8(&self.subject[..usize::from(self.subject_len)]).ok())
            .flatten()
    }

    /// `name` or `name:subject`: the wire spelling.
    pub fn token(&self) -> String {
        match self.subject() {
            Some(subject) => format!("{}:{subject}", self.name),
            None => self.name.to_string(),
        }
    }
}

/// One refused candidate, named. Reports keep a bounded sample (see
/// [`SANDBOX_BUILD_CACHE_REFUSAL_SAMPLES_MAX`]) so a manager can see which
/// sandbox failed which check without reading daemon logs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxBuildCacheReclaimRefusalV2 {
    pub session_id: Uuid,
    pub custody_id: Uuid,
    pub reason: SandboxBuildCacheReclaimSkipReason,
    pub check: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxBuildCacheReclaimReport {
    pub version: u8,
    pub dry_run: bool,
    pub enabled: bool,
    pub config: SandboxBuildCacheReclaimConfig,
    pub pressure_active_before: bool,
    pub pressure_active_after: bool,
    pub filesystem_before: SandboxFilesystemStats,
    pub filesystem_after: SandboxFilesystemStats,
    pub candidates_considered: u32,
    pub eligible_candidates: u32,
    pub skip_counts: BTreeMap<SandboxBuildCacheReclaimSkipReason, u32>,
    /// Definite future capacity claim. The daemon conservatively emits zero:
    /// external hard links cannot be frozen between preview and deletion.
    pub would_reclaim_count: u32,
    /// Definite future capacity bytes, not a point-in-time footprint estimate.
    /// Exact capacity is available only in `reclaimed_bytes` after deletion.
    pub would_reclaim_bytes: u64,
    pub staged_count: u32,
    pub staged_bytes: u64,
    pub newly_staged_count: u32,
    pub newly_staged_bytes: u64,
    pub recovered_count: u32,
    pub recovered_bytes: u64,
    pub pending_count: u32,
    pub pending_bytes: u64,
    pub fully_removed_count: u32,
    pub reclaimed_bytes: u64,
    pub stopped_at_low_watermark: bool,
    pub candidate_budget_exhausted: bool,
    pub stop_reason: SandboxBuildCacheReclaimStopReason,
}

impl SandboxBuildCacheReclaimReport {
    pub fn validate_wire(self) -> Result<Self, &'static str> {
        if self.version != SANDBOX_BUILD_CACHE_REPORT_VERSION {
            return Err("unsupported sandbox target-cache report version");
        }
        self.config.validate()?;
        if self.enabled != self.config.enabled {
            return Err("sandbox target-cache report/config enabled mismatch");
        }
        Ok(self)
    }
}

/// One ordered terminal-candidate key. Store preserves historical RFC3339
/// spellings verbatim because this value is a key into the existing relation;
/// the UUID breaks equal-string ties.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxTargetReclaimKeyV2 {
    pub updated_at: String,
    pub session_id: Uuid,
}

/// Durable terminal-candidate sweep evidence attached to a V2 report.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxTargetReclaimSweepV2 {
    pub cycle_before: u64,
    pub cycle_after: u64,
    pub cursor_before: Option<SandboxTargetReclaimKeyV2>,
    pub cursor_after: Option<SandboxTargetReclaimKeyV2>,
    pub upper_bound: Option<SandboxTargetReclaimKeyV2>,
    pub page_key_digest: String,
    pub reserved: bool,
    pub wrapped: bool,
}

/// Recovery-queue continuation evidence. Slice 3 assigns the opaque cursor's
/// bucket representation; callers can observe it but cannot choose entries.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxTargetRecoverySweepV2 {
    pub cycle_before: u64,
    pub cycle_after: u64,
    pub cursor_before: Option<String>,
    pub cursor_after: Option<String>,
    /// Actual passes persist cursor progress. Dry-runs observe the same
    /// bucket without creating or advancing scheduler state.
    pub reserved: bool,
    pub wrapped: bool,
    pub entries_deleted: u64,
    /// Exact count for the entire active recovery queue. Producers must emit
    /// `None` unless every bucket and the legacy flat namespace were observed
    /// completely in the same pass.
    pub residual_entries: Option<u64>,
    pub legacy_migrated: u32,
    pub nonprogress_count: u32,
    /// Independently scheduled registered v3 intents. Historical V2 payloads
    /// predate this field and decode it as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_sweep: Option<SandboxTargetReclaimIntentSweepV2>,
    /// Active and terminal durable intent inventory at the end of the pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_counts: Option<SandboxTargetReclaimIntentCountsV2>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxTargetReclaimIntentSweepV2 {
    pub cycle_before: u64,
    pub cycle_after: u64,
    pub cursor_before: Option<u64>,
    pub cursor_after: Option<u64>,
    pub upper_bound: Option<u64>,
    pub page_key_digest: String,
    pub reserved: bool,
    pub wrapped: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxTargetReclaimIntentCountsV2 {
    pub prepared: u32,
    pub staged: u32,
    pub deleting: u32,
    pub completed: u32,
    pub abandoned: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBuildCacheReclaimPassContentionV2 {
    StoreBusy,
}

/// V2 envelope keeps the complete V1 payload intact while adding durable
/// continuation evidence. This lets stored V1 JSON remain decodable without
/// making new fields optional on newly emitted V2 reports.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxBuildCacheReclaimReportV2 {
    pub version: u8,
    pub report: SandboxBuildCacheReclaimReport,
    pub candidate_sweep: SandboxTargetReclaimSweepV2,
    pub recovery_sweep: SandboxTargetRecoverySweepV2,
    pub pass_contention: Option<SandboxBuildCacheReclaimPassContentionV2>,
    /// A bounded sample of refused candidates and the exact check each failed.
    /// Payloads from before #1575 decode it as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<SandboxBuildCacheReclaimRefusalV2>,
    /// The hub's shared cargo `debug/` cache, which no sandbox owns (#1607).
    /// Payloads from before #1607 decode it as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_target: Option<SandboxSharedTargetReclaimV1>,
}

/// What one pass decided about the shared cargo `debug/` directory.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxSharedTargetReclaimOutcome {
    /// No shared target directory resolved, or it has no `debug/`.
    Absent,
    /// `debug/` is a symlink or not a directory; it is never followed.
    Unsafe,
    /// The platform cannot prove the cache has no live consumer.
    Unsupported,
    /// Newest write is younger than the idle floor.
    Fresh,
    /// A cargo build holds `debug/.cargo-lock`.
    LockHeld,
    /// A process still uses the cache (the check names its pid).
    LiveConsumer,
    /// The cache could not be inspected completely; nothing was touched.
    Unreadable,
    /// Idle and unused, but disk pressure is not active (or the pass is
    /// disabled), so the cache is kept.
    PressureInactive,
    /// Dry-run: a pressure pass would reclaim it.
    WouldReclaim,
    /// Removed in full.
    Reclaimed,
    /// Staged out of place; deletion continues on the next pass.
    Partial,
}

/// Shared cargo `debug/` reclaim evidence. `release/` is never considered.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxSharedTargetReclaimV1 {
    pub outcome: SandboxSharedTargetReclaimOutcome,
    /// The exact refusing check (`name` or `name:subject`), when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Seconds since the newest write seen, when the cache was inspected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u64>,
    /// Idle floor applied: 30 minutes under pressure (which bypasses the
    /// target-cache TTL, as for sandbox targets), else the TTL.
    pub min_idle_secs: u64,
    /// Allocated bytes seen by a bounded dry-run walk (a lower bound when
    /// `measured_truncated`), or by the removal itself.
    pub measured_bytes: u64,
    pub measured_truncated: bool,
    pub reclaimed_bytes: u64,
    /// An earlier pass's staged deletion is still unfinished.
    pub staged_pending: bool,
}

impl SandboxSharedTargetReclaimV1 {
    pub fn validate_wire(&self) -> Result<(), &'static str> {
        if let Some(check) = &self.check {
            if check.is_empty()
                || check.len() > SANDBOX_RECLAIM_CHECK_TOKEN_MAX
                || !check.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err("shared-target reclaim check is not a bounded token");
            }
        }
        Ok(())
    }
}

impl SandboxBuildCacheReclaimReportV2 {
    pub fn validate_wire(self) -> Result<Self, &'static str> {
        if self.version != SANDBOX_BUILD_CACHE_REPORT_VERSION_V2 {
            return Err("unsupported sandbox target-cache V2 report version");
        }
        self.report.clone().validate_wire()?;
        validate_candidate_sweep(&self.candidate_sweep)?;
        validate_recovery_sweep(&self.recovery_sweep)?;
        validate_refusals(&self.refusals)?;
        if let Some(shared) = &self.shared_target {
            shared.validate_wire()?;
        }
        Ok(self)
    }
}

fn validate_refusals(refusals: &[SandboxBuildCacheReclaimRefusalV2]) -> Result<(), &'static str> {
    if refusals.len() > SANDBOX_BUILD_CACHE_REFUSAL_SAMPLES_MAX {
        return Err("sandbox target-cache refusal sample is unbounded");
    }
    let mut per_reason: BTreeMap<SandboxBuildCacheReclaimSkipReason, usize> = BTreeMap::new();
    for refusal in refusals {
        if refusal.check.is_empty()
            || refusal.check.len() > SANDBOX_RECLAIM_CHECK_TOKEN_MAX
            || !refusal.check.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("sandbox target-cache refusal check is not a bounded token");
        }
        let count = per_reason.entry(refusal.reason).or_insert(0);
        *count += 1;
        if *count > SANDBOX_BUILD_CACHE_REFUSAL_SAMPLES_PER_REASON {
            return Err("sandbox target-cache refusal sample exceeds its per-reason bound");
        }
    }
    Ok(())
}

/// Backward-compatible decoder for persisted/report-stream JSON.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SandboxBuildCacheReclaimReportWire {
    V2(SandboxBuildCacheReclaimReportV2),
    V1(SandboxBuildCacheReclaimReport),
}

impl SandboxBuildCacheReclaimReportWire {
    pub fn validate_wire(self) -> Result<Self, &'static str> {
        match self {
            Self::V1(report) => report.validate_wire().map(Self::V1),
            Self::V2(report) => report.validate_wire().map(Self::V2),
        }
    }

    pub fn report(&self) -> &SandboxBuildCacheReclaimReport {
        match self {
            Self::V1(report) => report,
            Self::V2(report) => &report.report,
        }
    }

    pub fn v2(&self) -> Option<&SandboxBuildCacheReclaimReportV2> {
        match self {
            Self::V1(_) => None,
            Self::V2(report) => Some(report),
        }
    }
}

fn validate_candidate_sweep(value: &SandboxTargetReclaimSweepV2) -> Result<(), &'static str> {
    if value.cycle_after < value.cycle_before
        || value.cycle_after > value.cycle_before.saturating_add(1)
    {
        return Err("sandbox target-cache candidate cycle is not forward-only");
    }
    if !is_canonical_sha256(&value.page_key_digest) {
        return Err("sandbox target-cache page-key digest is not canonical");
    }
    if !value.reserved
        && (value.cycle_after != value.cycle_before
            || value.cursor_after != value.cursor_before
            || value.wrapped)
    {
        return Err("unreserved sandbox target-cache evidence claims durable progress");
    }
    if value.wrapped && value.cursor_after.is_some() {
        return Err("wrapped sandbox target-cache evidence retains a cursor");
    }
    if value.cycle_after > value.cycle_before && value.cursor_before.is_some() {
        return Err("new sandbox target-cache cycle starts after a prior cursor");
    }
    if (value.cursor_before.is_some() || value.cursor_after.is_some())
        && value.upper_bound.is_none()
    {
        return Err("sandbox target-cache cursor is missing its upper bound");
    }
    for key in [
        value.cursor_before.as_ref(),
        value.cursor_after.as_ref(),
        value.upper_bound.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if !is_rfc3339_timestamp(&key.updated_at) {
            return Err("sandbox target-cache cursor timestamp is invalid");
        }
    }
    if let (Some(after), Some(upper)) = (&value.cursor_after, &value.upper_bound)
        && (after.updated_at.as_str(), after.session_id)
            > (upper.updated_at.as_str(), upper.session_id)
    {
        return Err("sandbox target-cache cursor exceeds its upper bound");
    }
    if let (Some(before), Some(upper)) = (&value.cursor_before, &value.upper_bound)
        && (before.updated_at.as_str(), before.session_id)
            > (upper.updated_at.as_str(), upper.session_id)
    {
        return Err("sandbox target-cache prior cursor exceeds its upper bound");
    }
    if value.cycle_after == value.cycle_before
        && !value.wrapped
        && let Some(before) = &value.cursor_before
        && value.cursor_after.as_ref().is_none_or(|after| {
            (after.updated_at.as_str(), after.session_id)
                < (before.updated_at.as_str(), before.session_id)
        })
    {
        return Err("sandbox target-cache cursor moves backward");
    }
    Ok(())
}

fn validate_recovery_sweep(value: &SandboxTargetRecoverySweepV2) -> Result<(), &'static str> {
    if value.cycle_after < value.cycle_before
        || value.cycle_after > value.cycle_before.saturating_add(1)
    {
        return Err("sandbox target-cache recovery cycle is not forward-only");
    }
    let parse_bucket = |cursor: &str| {
        (cursor.len() == 2 && cursor.is_ascii())
            .then(|| u8::from_str_radix(cursor, 16).ok())
            .flatten()
    };
    if [
        value.cursor_before.as_deref(),
        value.cursor_after.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|cursor| parse_bucket(cursor).is_none())
    {
        return Err("sandbox target-cache recovery cursor is invalid");
    }
    if !value.reserved
        && (value.cycle_after != value.cycle_before || value.wrapped || value.entries_deleted != 0)
    {
        return Err("unreserved sandbox target-cache recovery evidence claims progress");
    }
    if !value.reserved
        && value.cursor_after != value.cursor_before
        && !(value.cursor_before.is_some() && value.cursor_after.is_none())
    {
        return Err("unreserved sandbox target-cache recovery cursor is inconsistent");
    }
    if value.wrapped && value.cycle_after != value.cycle_before.saturating_add(1) {
        return Err("wrapped sandbox target-cache recovery evidence does not advance cycle");
    }
    if !value.wrapped && value.cycle_after != value.cycle_before {
        return Err("sandbox target-cache recovery cycle advances without wrapping");
    }
    if value.reserved && (value.cursor_before.is_none() || value.cursor_after.is_none()) {
        return Err("reserved sandbox target-cache recovery evidence lacks a bucket cursor");
    }
    if value.reserved {
        let before = parse_bucket(value.cursor_before.as_deref().unwrap()).unwrap();
        let after = parse_bucket(value.cursor_after.as_deref().unwrap()).unwrap();
        if after != before.wrapping_add(1) {
            return Err("sandbox target-cache recovery cursor does not advance one bucket");
        }
    }
    if value.residual_entries == Some(0) && value.nonprogress_count != 0 {
        return Err("sandbox target-cache recovery reports nonprogress without residual work");
    }
    if let Some(intent) = &value.intent_sweep {
        validate_intent_sweep(intent)?;
    }
    Ok(())
}

fn validate_intent_sweep(value: &SandboxTargetReclaimIntentSweepV2) -> Result<(), &'static str> {
    if value.cycle_after < value.cycle_before
        || value.cycle_after > value.cycle_before.saturating_add(1)
    {
        return Err("sandbox target-cache intent cycle is not forward-only");
    }
    if !is_canonical_sha256(&value.page_key_digest) {
        return Err("sandbox target-cache intent page-key digest is not canonical");
    }
    if !value.reserved
        && (value.cycle_after != value.cycle_before
            || value.cursor_after != value.cursor_before
            || value.wrapped)
    {
        return Err("unreserved sandbox target-cache intent evidence claims durable progress");
    }
    if value.wrapped && value.cursor_after.is_some() {
        return Err("wrapped sandbox target-cache intent evidence retains a cursor");
    }
    if value.cycle_after > value.cycle_before && value.cursor_before.is_some() {
        return Err("new sandbox target-cache intent cycle starts after a prior cursor");
    }
    if (value.cursor_before.is_some() || value.cursor_after.is_some())
        && value.upper_bound.is_none()
    {
        return Err("sandbox target-cache intent cursor is missing its upper bound");
    }
    if let Some(upper) = value.upper_bound {
        if value.cursor_before.is_some_and(|cursor| cursor > upper)
            || value.cursor_after.is_some_and(|cursor| cursor > upper)
        {
            return Err("sandbox target-cache intent cursor exceeds its upper bound");
        }
    }
    if value.cycle_after == value.cycle_before
        && !value.wrapped
        && value
            .cursor_before
            .is_some_and(|before| value.cursor_after.is_none_or(|after| after < before))
    {
        return Err("sandbox target-cache intent cursor moves backward");
    }
    Ok(())
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 71
        && value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn is_rfc3339_timestamp(value: &str) -> bool {
    (20..=40).contains(&value.len()) && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> SandboxBuildCacheReclaimReport {
        let config = SandboxBuildCacheReclaimConfig {
            enabled: true,
            ttl_secs: 21_600,
            interval_secs: 3_600,
            high_watermark_pct: 85,
            low_watermark_pct: 75,
            max_candidates: 64,
        };
        let filesystem = SandboxFilesystemStats {
            total_bytes: 100,
            available_bytes: 40,
            used_bytes: 60,
            used_percent: 60,
        };
        SandboxBuildCacheReclaimReport {
            version: SANDBOX_BUILD_CACHE_REPORT_VERSION,
            dry_run: true,
            enabled: true,
            config,
            pressure_active_before: false,
            pressure_active_after: false,
            filesystem_before: filesystem,
            filesystem_after: filesystem,
            candidates_considered: 1,
            eligible_candidates: 1,
            skip_counts: BTreeMap::from([(SandboxBuildCacheReclaimSkipReason::TargetAbsent, 1)]),
            would_reclaim_count: 0,
            would_reclaim_bytes: 0,
            staged_count: 0,
            staged_bytes: 0,
            newly_staged_count: 0,
            newly_staged_bytes: 0,
            recovered_count: 0,
            recovered_bytes: 0,
            pending_count: 0,
            pending_bytes: 0,
            fully_removed_count: 0,
            reclaimed_bytes: 0,
            stopped_at_low_watermark: false,
            candidate_budget_exhausted: false,
            stop_reason: SandboxBuildCacheReclaimStopReason::AllRefused,
        }
    }

    #[test]
    fn sandbox_storage_report_round_trips_strictly() {
        let report = report();
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            serde_json::from_value::<SandboxBuildCacheReclaimReport>(value).unwrap(),
            report
        );
    }

    #[test]
    fn sandbox_storage_report_rejects_malformed_shapes_and_unknown_enums() {
        let mut value = serde_json::to_value(report()).unwrap();
        value.as_object_mut().unwrap().remove("version");
        assert!(serde_json::from_value::<SandboxBuildCacheReclaimReport>(value).is_err());

        let mut value = serde_json::to_value(report()).unwrap();
        value["dry_run"] = serde_json::json!("yes");
        assert!(serde_json::from_value::<SandboxBuildCacheReclaimReport>(value).is_err());

        let mut value = serde_json::to_value(report()).unwrap();
        value["future_field"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SandboxBuildCacheReclaimReport>(value).is_err());

        let mut value = serde_json::to_value(report()).unwrap();
        value["stop_reason"] = serde_json::json!("future_reason");
        assert!(serde_json::from_value::<SandboxBuildCacheReclaimReport>(value).is_err());

        let mut value = serde_json::to_value(report()).unwrap();
        value["skip_counts"] = serde_json::json!({ "future_skip": 1 });
        assert!(serde_json::from_value::<SandboxBuildCacheReclaimReport>(value).is_err());
    }

    #[test]
    fn sandbox_storage_config_enforces_complete_bounds() {
        let base = report().config;
        assert_eq!(base.validate(), Ok(base));
        assert!(
            SandboxBuildCacheReclaimConfig {
                ttl_secs: 0,
                ..base
            }
            .validate()
            .is_err()
        );
        assert!(
            SandboxBuildCacheReclaimConfig {
                interval_secs: 59,
                ..base
            }
            .validate()
            .is_err()
        );
        assert!(
            SandboxBuildCacheReclaimConfig {
                high_watermark_pct: 75,
                ..base
            }
            .validate()
            .is_err()
        );
        assert!(
            SandboxBuildCacheReclaimConfig {
                low_watermark_pct: 85,
                ..base
            }
            .validate()
            .is_err()
        );
        assert!(
            SandboxBuildCacheReclaimConfig {
                max_candidates: 1025,
                ..base
            }
            .validate()
            .is_err()
        );

        let mut invalid = report();
        invalid.version = SANDBOX_BUILD_CACHE_REPORT_VERSION + 1;
        assert!(invalid.validate_wire().is_err());
        let mut invalid = report();
        invalid.config.ttl_secs = 0;
        assert!(invalid.validate_wire().is_err());
        let mut invalid = report();
        invalid.enabled = false;
        assert!(invalid.validate_wire().is_err());
    }

    fn report_v2() -> SandboxBuildCacheReclaimReportV2 {
        SandboxBuildCacheReclaimReportV2 {
            version: SANDBOX_BUILD_CACHE_REPORT_VERSION_V2,
            report: report(),
            candidate_sweep: SandboxTargetReclaimSweepV2 {
                cycle_before: 3,
                cycle_after: 3,
                cursor_before: Some(SandboxTargetReclaimKeyV2 {
                    updated_at: "2026-09-13T12:00:00.000000000Z".into(),
                    session_id: Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap(),
                }),
                cursor_after: Some(SandboxTargetReclaimKeyV2 {
                    updated_at: "2026-09-13T12:00:01.000000000Z".into(),
                    session_id: Uuid::parse_str("00000000-0000-4000-8000-000000000002").unwrap(),
                }),
                upper_bound: Some(SandboxTargetReclaimKeyV2 {
                    updated_at: "2026-09-13T12:00:02.000000000Z".into(),
                    session_id: Uuid::parse_str("00000000-0000-4000-8000-000000000003").unwrap(),
                }),
                page_key_digest: format!("sha256:{}", "a".repeat(64)),
                reserved: true,
                wrapped: false,
            },
            recovery_sweep: SandboxTargetRecoverySweepV2 {
                cycle_before: 7,
                cycle_after: 8,
                cursor_before: Some("ff".into()),
                cursor_after: Some("00".into()),
                reserved: true,
                wrapped: true,
                entries_deleted: 12,
                residual_entries: Some(0),
                legacy_migrated: 1,
                nonprogress_count: 0,
                intent_sweep: Some(SandboxTargetReclaimIntentSweepV2 {
                    cycle_before: 7,
                    cycle_after: 7,
                    cursor_before: Some(41),
                    cursor_after: Some(42),
                    upper_bound: Some(48),
                    page_key_digest:
                        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                            .into(),
                    reserved: true,
                    wrapped: false,
                }),
                intent_counts: Some(SandboxTargetReclaimIntentCountsV2 {
                    prepared: 2,
                    staged: 1,
                    deleting: 1,
                    completed: 9,
                    abandoned: 3,
                }),
            },
            pass_contention: None,
            refusals: Vec::new(),
            shared_target: None,
        }
    }

    fn refusal(
        reason: SandboxBuildCacheReclaimSkipReason,
        check: &str,
    ) -> SandboxBuildCacheReclaimRefusalV2 {
        SandboxBuildCacheReclaimRefusalV2 {
            session_id: Uuid::parse_str("00000000-0000-4000-8000-0000000000a1").unwrap(),
            custody_id: Uuid::parse_str("00000000-0000-4000-8000-0000000000b1").unwrap(),
            reason,
            check: check.to_string(),
        }
    }

    #[test]
    fn skip_reason_as_str_is_the_serde_spelling_for_every_variant() {
        for reason in SandboxBuildCacheReclaimSkipReason::ALL {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::json!(reason.as_str())
            );
            assert_eq!(
                serde_json::from_value::<SandboxBuildCacheReclaimSkipReason>(serde_json::json!(
                    reason.as_str()
                ))
                .unwrap(),
                reason
            );
        }
        let names: std::collections::BTreeSet<_> = SandboxBuildCacheReclaimSkipReason::ALL
            .iter()
            .map(|reason| reason.as_str())
            .collect();
        assert_eq!(names.len(), SandboxBuildCacheReclaimSkipReason::ALL.len());
    }

    #[test]
    fn reclaim_check_token_names_the_check_and_sanitizes_its_subject() {
        let plain = SandboxReclaimCheck::new("git_branch_mismatch");
        assert_eq!(plain.token(), "git_branch_mismatch");
        assert_eq!(plain.subject(), None);

        let entry = SandboxReclaimCheck::with_subject("target_entry_unrecognized", "lander");
        assert_eq!(entry.token(), "target_entry_unrecognized:lander");
        assert_eq!(entry.name(), "target_entry_unrecognized");

        // Separators, whitespace, control and non-ASCII bytes never reach the wire.
        let hostile = SandboxReclaimCheck::with_subject("x", "a:b c\nd\u{e9}");
        assert!(hostile.token().bytes().all(|byte| byte.is_ascii_graphic()));
        assert_eq!(hostile.token().matches(':').count(), 1);

        let long = SandboxReclaimCheck::with_subject("x", "n".repeat(500));
        assert_eq!(
            long.subject().map(str::len),
            Some(SANDBOX_RECLAIM_CHECK_SUBJECT_MAX)
        );
        assert!(long.token().len() <= SANDBOX_RECLAIM_CHECK_TOKEN_MAX);
    }

    #[test]
    fn sandbox_storage_v2_refusal_samples_round_trip_stay_bounded_and_default_empty() {
        use SandboxBuildCacheReclaimSkipReason as Reason;
        let mut v2 = report_v2();
        v2.refusals = vec![
            refusal(
                Reason::TargetUnrecognizedContent,
                "target_entry_unrecognized:lander",
            ),
            refusal(Reason::GitOrRootIdentityRefusal, "git_branch_mismatch"),
        ];
        let value = serde_json::to_value(&v2).unwrap();
        let decoded = serde_json::from_value::<SandboxBuildCacheReclaimReportWire>(value)
            .unwrap()
            .validate_wire()
            .unwrap();
        assert_eq!(decoded.v2().unwrap().refusals.len(), 2);

        // A payload from before #1575 has no `refusals` key and decodes empty.
        let mut legacy = serde_json::to_value(report_v2()).unwrap();
        assert!(legacy.get("refusals").is_none());
        legacy.as_object_mut().unwrap().remove("refusals");
        let legacy = serde_json::from_value::<SandboxBuildCacheReclaimReportWire>(legacy).unwrap();
        assert!(legacy.v2().unwrap().refusals.is_empty());

        let mut too_many = report_v2();
        too_many.refusals = (0..=SANDBOX_BUILD_CACHE_REFUSAL_SAMPLES_MAX)
            .map(|_| refusal(Reason::TargetAbsent, "target_absent"))
            .collect();
        assert!(too_many.validate_wire().is_err());

        let mut one_noisy_reason = report_v2();
        one_noisy_reason.refusals = (0..=SANDBOX_BUILD_CACHE_REFUSAL_SAMPLES_PER_REASON)
            .map(|_| refusal(Reason::TargetAbsent, "target_absent"))
            .collect();
        assert!(one_noisy_reason.validate_wire().is_err());

        let mut bad_token = report_v2();
        bad_token.refusals = vec![refusal(Reason::LiveConsumer, "has space")];
        assert!(bad_token.validate_wire().is_err());
        let mut empty_token = report_v2();
        empty_token.refusals = vec![refusal(Reason::LiveConsumer, "")];
        assert!(empty_token.validate_wire().is_err());
    }

    #[test]
    fn sandbox_storage_v2_round_trips_and_v1_remains_decodable() {
        let v2 = report_v2();
        let value = serde_json::to_value(&v2).unwrap();
        assert_eq!(
            serde_json::from_value::<SandboxBuildCacheReclaimReportWire>(value)
                .unwrap()
                .validate_wire()
                .unwrap(),
            SandboxBuildCacheReclaimReportWire::V2(v2)
        );

        let v1 = report();
        let historical = serde_json::to_value(&v1).unwrap();
        assert_eq!(
            serde_json::from_value::<SandboxBuildCacheReclaimReportWire>(historical)
                .unwrap()
                .validate_wire()
                .unwrap(),
            SandboxBuildCacheReclaimReportWire::V1(v1)
        );

        let mut historical_v2 = serde_json::to_value(report_v2()).unwrap();
        historical_v2["recovery_sweep"]
            .as_object_mut()
            .unwrap()
            .remove("intent_sweep");
        historical_v2["recovery_sweep"]
            .as_object_mut()
            .unwrap()
            .remove("intent_counts");
        let SandboxBuildCacheReclaimReportWire::V2(historical_v2) =
            serde_json::from_value::<SandboxBuildCacheReclaimReportWire>(historical_v2)
                .unwrap()
                .validate_wire()
                .unwrap()
        else {
            panic!("historical V2 decoded as V1");
        };
        assert!(historical_v2.recovery_sweep.intent_sweep.is_none());
        assert!(historical_v2.recovery_sweep.intent_counts.is_none());
    }

    #[test]
    fn sandbox_storage_v2_rejects_cursor_and_version_drift() {
        let mut invalid = report_v2();
        invalid.version = 1;
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid.candidate_sweep.cycle_after = 5;
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid.candidate_sweep.page_key_digest = "sha256:ABC".into();
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid.candidate_sweep.cursor_after = invalid.candidate_sweep.cursor_before.clone();
        invalid
            .candidate_sweep
            .cursor_before
            .as_mut()
            .unwrap()
            .session_id = Uuid::parse_str("00000000-0000-4000-8000-000000000004").unwrap();
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid.candidate_sweep.reserved = false;
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid.candidate_sweep.wrapped = true;
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid.recovery_sweep.cursor_after = Some("80".into());
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid
            .recovery_sweep
            .intent_sweep
            .as_mut()
            .unwrap()
            .page_key_digest = "sha256:ABC".into();
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        let intent = invalid.recovery_sweep.intent_sweep.as_mut().unwrap();
        intent.cursor_after = intent.cursor_before.map(|cursor| cursor - 1);
        assert!(invalid.validate_wire().is_err());

        let mut invalid = report_v2();
        invalid
            .recovery_sweep
            .intent_sweep
            .as_mut()
            .unwrap()
            .upper_bound = None;
        assert!(invalid.validate_wire().is_err());

        let mut value = serde_json::to_value(report_v2()).unwrap();
        value["future_field"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SandboxBuildCacheReclaimReportWire>(value).is_err());
    }

    #[test]
    fn sandbox_storage_v2_allows_independent_legacy_progress_and_fsync_uncertainty() {
        let mut legacy = report_v2();
        legacy.recovery_sweep.cycle_after = legacy.recovery_sweep.cycle_before;
        legacy.recovery_sweep.cursor_after = legacy.recovery_sweep.cursor_before.clone();
        legacy.recovery_sweep.reserved = false;
        legacy.recovery_sweep.wrapped = false;
        legacy.recovery_sweep.entries_deleted = 0;
        legacy.recovery_sweep.residual_entries = None;
        legacy.recovery_sweep.legacy_migrated = 3;
        assert!(legacy.validate_wire().is_ok());

        let mut uncertain = report_v2();
        uncertain.recovery_sweep.cycle_after = uncertain.recovery_sweep.cycle_before;
        uncertain.recovery_sweep.cursor_after = None;
        uncertain.recovery_sweep.reserved = false;
        uncertain.recovery_sweep.wrapped = false;
        uncertain.recovery_sweep.entries_deleted = 0;
        uncertain.recovery_sweep.residual_entries = None;
        assert!(uncertain.validate_wire().is_ok());
    }

    #[test]
    fn sandbox_storage_v2_rejects_unreserved_deletion_and_impossible_residual() {
        let mut deletion = report_v2();
        deletion.recovery_sweep.cycle_after = deletion.recovery_sweep.cycle_before;
        deletion.recovery_sweep.cursor_after = deletion.recovery_sweep.cursor_before.clone();
        deletion.recovery_sweep.reserved = false;
        deletion.recovery_sweep.wrapped = false;
        deletion.recovery_sweep.entries_deleted = 1;
        deletion.recovery_sweep.residual_entries = None;
        assert!(deletion.validate_wire().is_err());

        let mut impossible = report_v2();
        impossible.recovery_sweep.residual_entries = Some(0);
        impossible.recovery_sweep.nonprogress_count = 1;
        assert!(impossible.validate_wire().is_err());
    }
}
