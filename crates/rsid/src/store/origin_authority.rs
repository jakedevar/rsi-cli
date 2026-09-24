//! Private V85 execution-origin catalog and migration support.
//!
//! This module deliberately exposes no mutation API.  Runtime claim APIs land
//! in H1-AF-01B after their transaction boundary is reviewed; V85 establishes
//! only the durable, read-only catalog and deterministic upgrade seed.

use crate::error::DaemonError;
use crate::store::daemon_settings::{C5AutofilePending, c5_autofile_pending_key};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use uuid::Uuid;

pub(crate) const CLAIM_NAMESPACE: Uuid = Uuid::from_u128(0x672e0ca2_9b3e_5a70_84a2_7c4c8f5ec7a1);
pub(crate) const RECEIPT_NAMESPACE: Uuid = Uuid::from_u128(0x8d4ca179_a598_596e_9e5b_5e22265c4a49);
pub(crate) const MODEL_INVOCATION_NAMESPACE: Uuid =
    Uuid::from_u128(0x695ef6d4_3f91_5410_9ee0_6c65b0ecb6f1);
pub(crate) const MALFORMED_LINEAGE_NAMESPACE: Uuid =
    Uuid::from_u128(0x50c3df80_188c_52a1_93de_5f63e2034516);

const V84_REQUIRED_RELATIONS: &[&str] = &[
    "sandbox_custody_roots",
    "sandbox_custody_events",
    "session_execution_projections",
    "closure_programs",
    "closure_source_launch_reservations",
    "closure_sources",
    "closure_source_sessions",
    "closure_events",
];

/// Tables whose complete SQLite catalog entries are part of the accepted V84
/// source. `scheduled_jobs` pins the older job facts the backfill reads, while
/// `tbl_name` selection also captures every owned index, trigger, and SQLite
/// auto-index. `sessions` is handled as a dependency projection below because
/// equivalent legacy migration paths preserve different CREATE statement text.
const V84_CATALOG_TABLES: &[&str] = &[
    "scheduled_jobs",
    "sandbox_custody_roots",
    "sandbox_custody_events",
    "session_execution_projections",
    "closure_programs",
    "closure_source_launch_reservations",
    "closure_sources",
    "closure_source_sessions",
    "conversation_event_provenance",
    "closure_output_validations",
    "closure_evidence",
    "closure_integration_queue",
    "closure_target_leases",
    "closure_integration_attempts",
    "integration_receipts",
    "closure_receipt_consumptions",
    "closure_final_gate_attempt_receipts",
    "closure_gate_failure_settlements",
    "closure_cleanup_proofs",
    "closure_cleanup_actions",
    "closure_operator_requests",
    "closure_events",
];

const V84_SESSION_CATALOG_OBJECTS: &[&str] = &[
    "idx_sessions_sandbox_custody_id_id",
    "idx_sessions_sandbox_root_id",
    "idx_sessions_startup_unlinked_root",
    "idx_sessions_startup_unlinked_rootless",
    "sessions_execution_projection_after_insert",
];

const V84_SESSION_DEPENDENCY_COLUMNS: &[&str] = &[
    "created_at",
    "continued_from",
    "id",
    "sandbox_branch",
    "sandbox_cleanup_state",
    "sandbox_custody_id",
    "sandbox_kind",
    "sandbox_root",
    "scheduled_job_id",
    "session_kind",
];

/// Fingerprint of the exact accepted V83 custody plus V84 Closure catalog.
/// The normalized inventory includes tables, indexes, and triggers: additions,
/// omissions, and changed definitions all refuse before V85 DDL.
pub(super) const V84_ACCEPTED_CATALOG_FINGERPRINT: &str =
    "sha256:fff864f0b836c13ddf4c5f285f2be2308e15d898fcb91a00ac0aa1cbe9d6d26e";

/// The one parser for the durable idempotency namespace.  It is deliberately
/// kept out of the DDL: a SQLite pattern is much too easy to make permissive
/// around delimiters, numeric spellings, and fixed-width components.
pub(crate) fn is_canonical_execution_origin_request_key(value: &str) -> bool {
    parse_execution_origin_request_key(value).is_some()
}

pub(crate) fn execution_origin_request_key_family(value: &str) -> Option<&'static str> {
    Some(parse_execution_origin_request_key(value)?.family())
}

/// The parsed form is the single source of truth for both the grammar check
/// and the claim/receipt provenance checks.  Do not add SQL substring parsing:
/// the request key is a security boundary and its components must be parsed
/// exactly once.
#[derive(Debug, PartialEq, Eq)]
enum ExecutionOriginRequestKey<'a> {
    Continue {
        source: &'a str,
        initial_sequence: i32,
        request_digest: &'a str,
    },
    Handoff {
        source: &'a str,
        rotation_id: Option<&'a str>,
        initial_sequence: i32,
    },
    AgentFresh {
        job_id: &'a str,
        fire_at: &'a str,
    },
    Rotation {
        source: &'a str,
        source_invocation_id: &'a str,
        rotation_id: Option<&'a str>,
        action_digest: &'a str,
    },
    AutomaticRetry {
        source: &'a str,
        retry_attempt: u8,
        c5_marker_digest: &'a str,
    },
}

impl ExecutionOriginRequestKey<'_> {
    const fn family(&self) -> &'static str {
        match self {
            Self::Continue { .. } => "continue",
            Self::Handoff { .. } => "handoff",
            Self::AgentFresh { .. } => "agent_fresh",
            Self::Rotation { .. } => "rotation",
            Self::AutomaticRetry { .. } => "automatic_retry",
        }
    }
}

fn parse_execution_origin_request_key(value: &str) -> Option<ExecutionOriginRequestKey<'_>> {
    if let Some(agent_fresh) = value.strip_prefix("agent-fresh:") {
        let (job_id, fire_at) = agent_fresh.split_once(':')?;
        return (canonical_uuid_text(job_id) && canonical_nanos_utc_timestamp(fire_at))
            .then_some(ExecutionOriginRequestKey::AgentFresh { job_id, fire_at });
    }
    let parts: Vec<_> = value.split(':').collect();
    match parts.as_slice() {
        ["continue", source, sequence, digest]
            if canonical_uuid_text(source) && canonical_digest(digest) =>
        {
            Some(ExecutionOriginRequestKey::Continue {
                source,
                initial_sequence: canonical_decimal_i32(sequence)?,
                request_digest: digest,
            })
        }
        ["handoff", source, rotation_id, sequence]
            if canonical_uuid_text(source) && canonical_optional_uuid(rotation_id) =>
        {
            Some(ExecutionOriginRequestKey::Handoff {
                source,
                rotation_id: (*rotation_id != "none").then_some(*rotation_id),
                initial_sequence: canonical_decimal_i32(sequence)?,
            })
        }
        ["rotation", source, invocation, rotation_id, digest]
            if canonical_uuid_text(source)
                && canonical_uuid_text(invocation)
                && canonical_optional_uuid(rotation_id)
                && canonical_digest(digest) =>
        {
            Some(ExecutionOriginRequestKey::Rotation {
                source,
                source_invocation_id: invocation,
                rotation_id: (*rotation_id != "none").then_some(*rotation_id),
                action_digest: digest,
            })
        }
        ["retry", source, attempt, digest]
            if canonical_uuid_text(source) && canonical_digest(digest) =>
        {
            Some(ExecutionOriginRequestKey::AutomaticRetry {
                source,
                retry_attempt: canonical_decimal_u8(attempt)?,
                c5_marker_digest: digest,
            })
        }
        _ => None,
    }
}

/// Bind every request-key component that is duplicated by a claim's durable
/// provenance.  The continue key's trailing SHA is intentionally excluded: it
/// is the request SHA, while `model_no_execution_request_digest` authenticates
/// the distinct canonical admission descriptor (accepted plan §§280-320).
pub(crate) fn execution_origin_request_key_matches_claim(
    request_key: &str,
    claimant_kind: &str,
    source_session_id: &str,
    scheduled_job_id: Option<&str>,
    scheduled_fire_at: Option<&str>,
    rotation_id: Option<&str>,
    source_model_invocation_id: Option<&str>,
    rotation_action_digest: Option<&str>,
    retry_attempt: Option<i64>,
    c5_marker_digest_column: Option<&str>,
) -> bool {
    match parse_execution_origin_request_key(request_key) {
        Some(ExecutionOriginRequestKey::Continue { source, .. }) => {
            claimant_kind == "continue" && source == source_session_id
        }
        Some(ExecutionOriginRequestKey::Handoff {
            source,
            rotation_id: key_rotation_id,
            ..
        }) => {
            claimant_kind == "handoff"
                && source == source_session_id
                && key_rotation_id == rotation_id
        }
        Some(ExecutionOriginRequestKey::AgentFresh { job_id, fire_at }) => {
            claimant_kind == "agent_fresh"
                && scheduled_job_id == Some(job_id)
                && scheduled_fire_at == Some(fire_at)
        }
        Some(ExecutionOriginRequestKey::Rotation {
            source,
            source_invocation_id,
            rotation_id: key_rotation_id,
            action_digest,
        }) => {
            claimant_kind == "rotation"
                && source == source_session_id
                && source_model_invocation_id == Some(source_invocation_id)
                && key_rotation_id == rotation_id
                && rotation_action_digest.and_then(|digest| digest.strip_prefix("sha256:"))
                    == Some(action_digest)
        }
        Some(ExecutionOriginRequestKey::AutomaticRetry {
            source,
            retry_attempt: key_retry_attempt,
            c5_marker_digest,
        }) => {
            claimant_kind == "automatic_retry"
                && source == source_session_id
                && retry_attempt == Some(i64::from(key_retry_attempt))
                && c5_marker_digest_column.and_then(|digest| digest.strip_prefix("sha256:"))
                    == Some(c5_marker_digest)
        }
        None => false,
    }
}

/// Receipt occurrences have fewer carried columns than claims.  They still
/// bind the source whenever it is carried, require the full AgentFresh job
/// occurrence, and forbid job occurrence columns for every other family.
pub(crate) fn execution_origin_request_key_matches_receipt(
    request_key: &str,
    source_session_id: Option<&str>,
    scheduled_job_id: Option<&str>,
    scheduled_fire_at: Option<&str>,
) -> bool {
    match parse_execution_origin_request_key(request_key) {
        Some(ExecutionOriginRequestKey::Continue { source, .. })
        | Some(ExecutionOriginRequestKey::Handoff { source, .. })
        | Some(ExecutionOriginRequestKey::Rotation { source, .. })
        | Some(ExecutionOriginRequestKey::AutomaticRetry { source, .. }) => {
            source_session_id.is_none_or(|carried| carried == source)
                && scheduled_job_id.is_none()
                && scheduled_fire_at.is_none()
        }
        Some(ExecutionOriginRequestKey::AgentFresh { job_id, fire_at }) => {
            scheduled_job_id == Some(job_id) && scheduled_fire_at == Some(fire_at)
        }
        None => false,
    }
}

fn canonical_uuid_text(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes.iter().enumerate().all(|(index, byte)| {
            [8, 13, 18, 23].contains(&index) || matches!(byte, b'0'..=b'9' | b'a'..=b'f')
        })
}

fn canonical_optional_uuid(value: &str) -> bool {
    value == "none" || canonical_uuid_text(value)
}

fn canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn canonical_decimal_i32(value: &str) -> Option<i32> {
    canonical_decimal(value)?.parse().ok()
}

fn canonical_decimal_u8(value: &str) -> Option<u8> {
    canonical_decimal(value)?.parse().ok()
}

fn canonical_decimal(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    (!bytes.is_empty()
        && bytes.iter().all(u8::is_ascii_digit)
        && (value == "0" || !value.starts_with('0'))
        && value.parse::<u64>().is_ok())
    .then_some(value)
}

fn canonical_nanos_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 30
        && matches!(
            bytes,
            [
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'-',
                b'0'..=b'9',
                b'0'..=b'9',
                b'-',
                b'0'..=b'9',
                b'0'..=b'9',
                b'T',
                b'0'..=b'9',
                b'0'..=b'9',
                b':',
                b'0'..=b'9',
                b'0'..=b'9',
                b':',
                b'0'..=b'9',
                b'0'..=b'9',
                b'.',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'0'..=b'9',
                b'Z'
            ]
        )
        && DateTime::parse_from_rfc3339(value).is_ok()
}

fn canonical_uuid_value(value: &str) -> Option<Uuid> {
    canonical_uuid_text(value)
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
        .filter(|parsed| parsed.to_string() == value)
}

/// Validate the complete static controller proposal carried by an origin
/// claim. Live Idea/reservation equality is deliberately left to the 01B
/// transaction; V88 owns only canonical bytes and deterministic identities.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execution_origin_controller_is_canonical(
    claimant_kind: &str,
    source_session_id: &str,
    claimant_session_id: &str,
    source_model_invocation_id: Option<&str>,
    rotation_action_digest: Option<&str>,
    retry_attempt: Option<i64>,
    c5_marker_digest: Option<&str>,
    controller_project_id: Option<&str>,
    controller_idea_id: Option<&str>,
    controller_transfer_key: Option<&str>,
    controller_reservation_id: Option<&str>,
    controller_candidate_session_id: Option<&str>,
    controller_base_row_id: Option<&str>,
    controller_base_event_id: Option<&str>,
    controller_expires_at: Option<&str>,
) -> bool {
    let group = [
        controller_project_id,
        controller_idea_id,
        controller_transfer_key,
        controller_reservation_id,
        controller_candidate_session_id,
        controller_base_row_id,
        controller_base_event_id,
        controller_expires_at,
    ];
    if group.iter().all(|value| value.is_none()) {
        return true;
    }
    if group.iter().any(|value| value.is_none())
        || !matches!(claimant_kind, "rotation" | "automatic_retry")
    {
        return false;
    }

    let Some(source_session_id) = canonical_uuid_value(source_session_id) else {
        return false;
    };
    let Some(claimant_session_id) = canonical_uuid_value(claimant_session_id) else {
        return false;
    };
    let Some(_project_id) = controller_project_id.and_then(canonical_uuid_value) else {
        return false;
    };
    let Some(idea_id) = controller_idea_id.and_then(canonical_uuid_value) else {
        return false;
    };
    let transfer_key = controller_transfer_key.expect("complete controller group");
    if transfer_key.trim().is_empty() || transfer_key.len() > 256 || transfer_key.contains('\0') {
        return false;
    }
    let Some(reservation_id) = controller_reservation_id.and_then(canonical_uuid_value) else {
        return false;
    };
    let Some(candidate_session_id) = controller_candidate_session_id.and_then(canonical_uuid_value)
    else {
        return false;
    };
    let Some(base_row_id) = controller_base_row_id.and_then(canonical_decimal) else {
        return false;
    };
    if base_row_id == "0" || base_row_id.parse::<i64>().is_err() {
        return false;
    }
    if controller_base_event_id
        .and_then(canonical_uuid_value)
        .is_none()
        || !controller_expires_at.is_some_and(canonical_nanos_utc_timestamp)
    {
        return false;
    }

    let expected_key = match claimant_kind {
        "rotation" => {
            let Some(invocation_id) = source_model_invocation_id.and_then(canonical_uuid_value)
            else {
                return false;
            };
            let Some(action_digest) = rotation_action_digest
                .filter(|digest| digest.strip_prefix("sha256:").is_some_and(canonical_digest))
            else {
                return false;
            };
            format!("controller:rotation:{source_session_id}:{invocation_id}:{action_digest}")
        }
        "automatic_retry" => {
            let Some(attempt) = retry_attempt.filter(|attempt| *attempt >= 0) else {
                return false;
            };
            let Some(marker_digest) = c5_marker_digest
                .filter(|digest| digest.strip_prefix("sha256:").is_some_and(canonical_digest))
            else {
                return false;
            };
            format!("controller:retry:{source_session_id}:{attempt}:{marker_digest}")
        }
        _ => return false,
    };
    transfer_key == expected_key
        && reservation_id
            == rsi_common::types::idea_controller_reservation_id(idea_id, transfer_key)
        && candidate_session_id
            == rsi_common::types::idea_controller_candidate_session_id(reservation_id)
        && candidate_session_id == claimant_session_id
}

/// Validate the exact Failed-only C5 marker frozen into a prepared claim.
/// Parsing and reserialization reject alternate JSON spellings, field order,
/// duplicate/unknown fields, and any mismatch between carried columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execution_origin_c5_is_canonical(
    source_session_id: &str,
    prepared_terminal_status: Option<&str>,
    prepared_c5_cause: Option<&str>,
    prepared_c5_key: Option<&str>,
    prepared_c5_value: Option<&str>,
    prepared_c5_at: Option<&str>,
    prepared_c5_digest: Option<&str>,
    prepared_c5_expected_retry_count: Option<i64>,
    prepared_c5_max_retries: Option<i64>,
) -> bool {
    #[derive(Serialize)]
    struct C5AutofilePendingV1<'a> {
        version: u8,
        source_session_id: Uuid,
        cause: &'a str,
        staged_at: &'a str,
    }

    #[derive(Serialize)]
    struct C5AutofilePendingV2<'a> {
        version: u8,
        source_session_id: Uuid,
        cause: &'a str,
        staged_at: &'a str,
        terminal_model_invocation_id: Option<Uuid>,
    }

    let group = [
        prepared_c5_cause,
        prepared_c5_key,
        prepared_c5_value,
        prepared_c5_at,
        prepared_c5_digest,
    ];
    let counts_absent =
        prepared_c5_expected_retry_count.is_none() && prepared_c5_max_retries.is_none();
    if group.iter().all(|value| value.is_none()) && counts_absent {
        return prepared_terminal_status != Some("Failed");
    }
    if group.iter().any(|value| value.is_none())
        || prepared_c5_expected_retry_count.is_none()
        || prepared_c5_max_retries.is_none()
        || prepared_terminal_status != Some("Failed")
    {
        return false;
    }

    let Some(source_session_id) = canonical_uuid_value(source_session_id) else {
        return false;
    };
    let cause = prepared_c5_cause.expect("complete C5 group");
    let key = prepared_c5_key.expect("complete C5 group");
    let value = prepared_c5_value.expect("complete C5 group");
    let staged_at = prepared_c5_at.expect("complete C5 group");
    let digest = prepared_c5_digest.expect("complete C5 group");
    let expected_retry_count = prepared_c5_expected_retry_count.expect("complete C5 group");
    let max_retries = prepared_c5_max_retries.expect("complete C5 group");
    if expected_retry_count < 0
        || max_retries < 0
        || expected_retry_count > max_retries
        || key != c5_autofile_pending_key(source_session_id)
        || !canonical_nanos_utc_timestamp(staged_at)
    {
        return false;
    }
    let Ok(pending) = C5AutofilePending::parse(value) else {
        return false;
    };
    let canonical = match pending.version {
        1 => serde_json::to_string(&C5AutofilePendingV1 {
            version: 1,
            source_session_id,
            cause,
            staged_at,
        }),
        2 => serde_json::to_string(&C5AutofilePendingV2 {
            version: 2,
            source_session_id,
            cause,
            staged_at,
            terminal_model_invocation_id: pending.terminal_model_invocation_id,
        }),
        _ => return false,
    };
    if pending.source_session_id != source_session_id
        || pending.cause.as_str() != cause
        || !canonical.is_ok_and(|canonical| canonical == value)
        || serde_json::from_str::<serde_json::Value>(value)
            .ok()
            .and_then(|json| {
                json.get("staged_at")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .as_deref()
            != Some(staged_at)
    {
        return false;
    }
    digest == format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

/// DDL is kept adjacent to the canonical byte builders so both migration and
/// later hydration have one private schema owner.
const V85_SESSION_COLUMN_SQL: &str = r#"
ALTER TABLE sessions ADD COLUMN execution_origin_claim_id TEXT;
ALTER TABLE sessions ADD COLUMN execution_origin_write_seq INTEGER NOT NULL DEFAULT 0 CHECK(execution_origin_write_seq >= 0);
"#;

const V85_CATALOG_BEFORE_CLAIM_MATCH_SQL: &str = r#"
CREATE TABLE execution_origin_authorities (
 authority_kind TEXT NOT NULL CHECK(authority_kind IN ('ordinary','sandbox')),
 authority_uuid TEXT NOT NULL CHECK(length(authority_uuid)=36 AND authority_uuid=lower(authority_uuid) AND substr(authority_uuid,9,1)='-' AND substr(authority_uuid,14,1)='-' AND substr(authority_uuid,19,1)='-' AND substr(authority_uuid,24,1)='-' AND length(replace(authority_uuid,'-',''))=32 AND replace(authority_uuid,'-','') NOT GLOB '*[^0-9a-f]*'),
 owner_session_id TEXT UNIQUE,
 owner_generation INTEGER NOT NULL CHECK(owner_generation>0),
 claim_generation INTEGER NOT NULL DEFAULT 0 CHECK(claim_generation>=0),
 event_sequence INTEGER NOT NULL CHECK(event_sequence>0),
 active_claim_id TEXT UNIQUE REFERENCES execution_origin_claims(claim_id) ON DELETE RESTRICT,
 phase TEXT NOT NULL CHECK(phase IN ('unverified','idle','claimed','launch_ready','launching','provider_live','settling','quarantined')),
 boot_id TEXT,
 quarantine_code TEXT,
 created_at TEXT NOT NULL CHECK(created_at GLOB '????-??-??T??:??:??.?????????Z'),
 updated_at TEXT NOT NULL CHECK(updated_at GLOB '????-??-??T??:??:??.?????????Z'),
 PRIMARY KEY(authority_kind,authority_uuid),
 CHECK((phase IN ('unverified','quarantined') AND owner_session_id IS NULL) OR owner_session_id IS NOT NULL),
 CHECK((active_claim_id IS NULL AND phase IN ('unverified','idle','quarantined')) OR (active_claim_id IS NOT NULL AND phase IN ('claimed','launch_ready','launching','provider_live','settling','quarantined')))
);
CREATE TABLE execution_origin_claims (
 claim_id TEXT PRIMARY KEY CHECK(length(claim_id)=36 AND claim_id=lower(claim_id) AND substr(claim_id,9,1)='-' AND substr(claim_id,14,1)='-' AND substr(claim_id,19,1)='-' AND substr(claim_id,24,1)='-' AND length(replace(claim_id,'-',''))=32 AND replace(claim_id,'-','') NOT GLOB '*[^0-9a-f]*'),
 request_key TEXT NOT NULL UNIQUE CHECK(rsi_execution_origin_request_key_is_canonical(request_key)=1),
 authority_kind TEXT NOT NULL,
 authority_uuid TEXT NOT NULL,
 requested_origin_session_id TEXT NOT NULL,
 source_session_id TEXT NOT NULL,
 claimant_session_id TEXT NOT NULL,
 claimant_kind TEXT NOT NULL CHECK(claimant_kind IN ('continue','handoff','agent_fresh','rotation','automatic_retry')),
 trigger_kind TEXT NOT NULL CHECK(trigger_kind IN ('direct','answer_question','stall_classifier','scheduled_resume','terminal_watch','handoff_write','rpc_agent_fresh','native_agent_fresh','context_rotation','automatic_retry')),
 expected_owner_generation INTEGER NOT NULL CHECK(expected_owner_generation>0),
 expected_claim_generation INTEGER NOT NULL CHECK(expected_claim_generation>=0),
 prior_session_status TEXT NOT NULL CHECK(prior_session_status IN ('Starting','Running','WaitingApproval','Completed','Failed','Interrupted','Archived','Deleted')),
 boot_id TEXT NOT NULL,
 model_invocation_id TEXT NOT NULL CHECK(length(model_invocation_id)=36 AND model_invocation_id=lower(model_invocation_id) AND substr(model_invocation_id,9,1)='-' AND substr(model_invocation_id,14,1)='-' AND substr(model_invocation_id,19,1)='-' AND substr(model_invocation_id,24,1)='-' AND length(replace(model_invocation_id,'-',''))=32 AND replace(model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*'),
 model_no_execution_request_json TEXT NOT NULL CHECK(json_valid(model_no_execution_request_json)),
 model_no_execution_request_digest TEXT NOT NULL CHECK(length(model_no_execution_request_digest)=71 AND substr(model_no_execution_request_digest,1,7)='sha256:' AND substr(model_no_execution_request_digest,8)=lower(substr(model_no_execution_request_digest,8)) AND substr(model_no_execution_request_digest,8) NOT GLOB '*[^0-9a-f]*'),
 scheduled_job_id TEXT,
 scheduled_fire_at TEXT CHECK(scheduled_fire_at IS NULL OR scheduled_fire_at GLOB '????-??-??T??:??:??.?????????Z'),
 rotation_id TEXT,
 source_model_invocation_id TEXT CHECK(source_model_invocation_id IS NULL OR (length(source_model_invocation_id)=36 AND source_model_invocation_id=lower(source_model_invocation_id) AND substr(source_model_invocation_id,9,1)='-' AND substr(source_model_invocation_id,14,1)='-' AND substr(source_model_invocation_id,19,1)='-' AND substr(source_model_invocation_id,24,1)='-' AND length(replace(source_model_invocation_id,'-',''))=32 AND replace(source_model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*')),
 rotation_action_digest TEXT CHECK(rotation_action_digest IS NULL OR (length(rotation_action_digest)=71 AND substr(rotation_action_digest,1,7)='sha256:' AND substr(rotation_action_digest,8)=lower(substr(rotation_action_digest,8)) AND substr(rotation_action_digest,8) NOT GLOB '*[^0-9a-f]*')),
 retry_attempt INTEGER CHECK(retry_attempt IS NULL OR retry_attempt>=0),
 max_retries INTEGER CHECK(max_retries IS NULL OR max_retries>=0),
 c5_marker_digest TEXT CHECK(c5_marker_digest IS NULL OR (length(c5_marker_digest)=71 AND substr(c5_marker_digest,1,7)='sha256:' AND substr(c5_marker_digest,8)=lower(substr(c5_marker_digest,8)) AND substr(c5_marker_digest,8) NOT GLOB '*[^0-9a-f]*')),
 provider_create_state TEXT NOT NULL DEFAULT 'not_attempted' CHECK(provider_create_state IN ('not_attempted','no_create','created','create_unknown')),
 observed_cell_phase TEXT CHECK(observed_cell_phase IS NULL OR observed_cell_phase IN ('reserved','created_unconfigured','configured_unpublished','published','cleanup_requested','terminal','quarantined')),
 absence_evidence TEXT CHECK(absence_evidence IS NULL OR absence_evidence IN ('no_create','immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent')),
 authorized_session_status TEXT CHECK(authorized_session_status IS NULL OR authorized_session_status IN ('Starting','Running','WaitingApproval','Completed','Failed','Interrupted')),
 authorized_session_write_seq INTEGER CHECK(authorized_session_write_seq IS NULL OR authorized_session_write_seq>=0),
 prepared_terminal_status TEXT CHECK(prepared_terminal_status IS NULL OR prepared_terminal_status IN ('Completed','Failed','Interrupted')),
 prepared_stop_reason TEXT CHECK(prepared_stop_reason IS NULL OR length(prepared_stop_reason)>0),
 prepared_session_write_seq INTEGER CHECK(prepared_session_write_seq IS NULL OR prepared_session_write_seq>=0),
 prepared_provider_evidence TEXT CHECK(prepared_provider_evidence IS NULL OR prepared_provider_evidence IN ('no_create','immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent')),
 terminal_model_invocation_id TEXT CHECK(terminal_model_invocation_id IS NULL OR (length(terminal_model_invocation_id)=36 AND terminal_model_invocation_id=lower(terminal_model_invocation_id) AND substr(terminal_model_invocation_id,9,1)='-' AND substr(terminal_model_invocation_id,14,1)='-' AND substr(terminal_model_invocation_id,19,1)='-' AND substr(terminal_model_invocation_id,24,1)='-' AND length(replace(terminal_model_invocation_id,'-',''))=32 AND replace(terminal_model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*')),
 prepared_c5_cause TEXT, prepared_c5_key TEXT, prepared_c5_value TEXT, prepared_c5_at TEXT, prepared_c5_digest TEXT, prepared_c5_expected_retry_count INTEGER, prepared_c5_max_retries INTEGER,
 controller_project_id TEXT, controller_idea_id TEXT, controller_transfer_key TEXT, controller_reservation_id TEXT, controller_candidate_session_id TEXT, controller_base_row_id TEXT, controller_base_event_id TEXT, controller_expires_at TEXT,
 phase TEXT NOT NULL CHECK(phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')),
 claimed_at TEXT NOT NULL CHECK(claimed_at GLOB '????-??-??T??:??:??.?????????Z'), updated_at TEXT NOT NULL CHECK(updated_at GLOB '????-??-??T??:??:??.?????????Z'),
 FOREIGN KEY(authority_kind,authority_uuid) REFERENCES execution_origin_authorities(authority_kind,authority_uuid) ON DELETE RESTRICT,
"#;

const V85_CATALOG_AFTER_CLAIM_MATCH_SQL: &str = r#"
 CHECK(
   rsi_execution_origin_request_key_family(request_key)=claimant_kind AND (
     (claimant_kind='continue' AND trigger_kind IN ('direct','answer_question','stall_classifier','scheduled_resume','terminal_watch') AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND rotation_id IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='handoff' AND trigger_kind='handoff_write' AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='agent_fresh' AND trigger_kind IN ('rpc_agent_fresh','native_agent_fresh') AND scheduled_job_id IS NOT NULL AND scheduled_fire_at IS NOT NULL AND rotation_id IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='rotation' AND trigger_kind='context_rotation' AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND source_model_invocation_id IS NOT NULL AND rotation_action_digest IS NOT NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='automatic_retry' AND trigger_kind='automatic_retry' AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND rotation_id IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NOT NULL AND max_retries IS NOT NULL AND c5_marker_digest IS NOT NULL)
   )
 ),
 CHECK((authorized_session_status IS NULL AND authorized_session_write_seq IS NULL) OR (authorized_session_status IS NOT NULL AND authorized_session_write_seq IS NOT NULL)),
 CHECK((prepared_terminal_status IS NULL AND prepared_stop_reason IS NULL AND prepared_session_write_seq IS NULL AND prepared_provider_evidence IS NULL AND terminal_model_invocation_id IS NULL) OR (prepared_terminal_status IS NOT NULL AND prepared_stop_reason IS NOT NULL AND prepared_session_write_seq IS NOT NULL AND prepared_provider_evidence IS NOT NULL AND terminal_model_invocation_id IS NOT NULL AND phase IN ('settling','settled','failed','abandoned','quarantined'))),
 CHECK((prepared_c5_cause IS NULL AND prepared_c5_key IS NULL AND prepared_c5_value IS NULL AND prepared_c5_at IS NULL AND prepared_c5_digest IS NULL AND prepared_c5_expected_retry_count IS NULL AND prepared_c5_max_retries IS NULL) OR (prepared_c5_cause IS NOT NULL AND prepared_c5_key IS NOT NULL AND prepared_c5_value IS NOT NULL AND prepared_c5_at GLOB '????-??-??T??:??:??.?????????Z' AND length(prepared_c5_digest)=71 AND substr(prepared_c5_digest,1,7)='sha256:' AND substr(prepared_c5_digest,8)=lower(substr(prepared_c5_digest,8)) AND substr(prepared_c5_digest,8) NOT GLOB '*[^0-9a-f]*' AND prepared_c5_expected_retry_count>=0 AND prepared_c5_max_retries>=0)),
 CHECK((controller_project_id IS NULL AND controller_idea_id IS NULL AND controller_transfer_key IS NULL AND controller_reservation_id IS NULL AND controller_candidate_session_id IS NULL AND controller_base_row_id IS NULL AND controller_base_event_id IS NULL AND controller_expires_at IS NULL) OR (controller_project_id IS NOT NULL AND controller_idea_id IS NOT NULL AND controller_transfer_key IS NOT NULL AND controller_reservation_id IS NOT NULL AND controller_candidate_session_id IS NOT NULL AND controller_base_row_id IS NOT NULL AND controller_base_event_id IS NOT NULL AND controller_expires_at IS NOT NULL))
);
CREATE TABLE execution_origin_members (
 session_id TEXT PRIMARY KEY,
 authority_kind TEXT NOT NULL, authority_uuid TEXT NOT NULL,
 joined_owner_generation INTEGER NOT NULL CHECK(joined_owner_generation>0),
 source_session_id TEXT NOT NULL, join_cause TEXT NOT NULL CHECK(join_cause IN ('v85_seed','lazy_seed','agent_fresh','rotation','automatic_retry')),
 claim_id TEXT, joined_at TEXT NOT NULL CHECK(joined_at GLOB '????-??-??T??:??:??.?????????Z'),
 FOREIGN KEY(authority_kind,authority_uuid) REFERENCES execution_origin_authorities(authority_kind,authority_uuid) ON DELETE RESTRICT,
 FOREIGN KEY(claim_id) REFERENCES execution_origin_claims(claim_id) ON DELETE RESTRICT,
 CHECK((claim_id IS NULL AND join_cause IN ('v85_seed','lazy_seed')) OR claim_id IS NOT NULL)
);
CREATE TABLE execution_origin_events (
 event_id TEXT PRIMARY KEY CHECK(length(event_id)=36 AND event_id=lower(event_id) AND substr(event_id,9,1)='-' AND substr(event_id,14,1)='-' AND substr(event_id,19,1)='-' AND substr(event_id,24,1)='-' AND length(replace(event_id,'-',''))=32 AND replace(event_id,'-','') NOT GLOB '*[^0-9a-f]*'),
 authority_kind TEXT NOT NULL, authority_uuid TEXT NOT NULL, sequence INTEGER NOT NULL CHECK(sequence>0), claim_id TEXT,
 from_phase TEXT CHECK(from_phase IS NULL OR from_phase IN ('unverified','idle','claimed','launch_ready','launching','provider_live','settling','quarantined')), to_phase TEXT NOT NULL CHECK(to_phase IN ('unverified','idle','claimed','launch_ready','launching','provider_live','settling','quarantined')), from_owner_generation INTEGER CHECK(from_owner_generation IS NULL OR from_owner_generation>0), to_owner_generation INTEGER NOT NULL CHECK(to_owner_generation>0),
 provider_absence_evidence TEXT CHECK(provider_absence_evidence IS NULL OR provider_absence_evidence IN ('no_create','immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent')), event_kind TEXT NOT NULL CHECK(event_kind IN ('seeded','reconciled','claimed','launch_ready','launching','provider_live','settlement_prepared','settled','quarantined')), detail_json TEXT NOT NULL CHECK(json_valid(detail_json)), occurred_at TEXT NOT NULL CHECK(occurred_at GLOB '????-??-??T??:??:??.?????????Z'),
 FOREIGN KEY(authority_kind,authority_uuid) REFERENCES execution_origin_authorities(authority_kind,authority_uuid) ON DELETE RESTRICT,
 FOREIGN KEY(claim_id) REFERENCES execution_origin_claims(claim_id) ON DELETE RESTRICT,
 UNIQUE(authority_kind,authority_uuid,sequence),
 CHECK((from_phase IS NULL AND from_owner_generation IS NULL) OR (from_phase IS NOT NULL AND from_owner_generation IS NOT NULL))
);
CREATE TABLE execution_origin_receipts (
 receipt_id TEXT PRIMARY KEY CHECK(length(receipt_id)=36 AND receipt_id=lower(receipt_id) AND substr(receipt_id,9,1)='-' AND substr(receipt_id,14,1)='-' AND substr(receipt_id,19,1)='-' AND substr(receipt_id,24,1)='-' AND length(replace(receipt_id,'-',''))=32 AND replace(receipt_id,'-','') NOT GLOB '*[^0-9a-f]*'),
 request_key TEXT NOT NULL UNIQUE CHECK(rsi_execution_origin_request_key_is_canonical(request_key)=1), claim_id TEXT, authority_kind TEXT, authority_uuid TEXT,
 requested_origin_session_id TEXT NOT NULL, source_session_id TEXT, claimant_session_id TEXT, scheduled_job_id TEXT, scheduled_fire_at TEXT CHECK(scheduled_fire_at IS NULL OR scheduled_fire_at GLOB '????-??-??T??:??:??.?????????Z'),
 outcome TEXT NOT NULL CHECK(outcome IN ('accepted','busy','stale','refused','superseded','settled','failed','abandoned','quarantined')),
 code TEXT NOT NULL CHECK(length(code)>0), occurred_at TEXT NOT NULL CHECK(occurred_at GLOB '????-??-??T??:??:??.?????????Z'),
 FOREIGN KEY(claim_id) REFERENCES execution_origin_claims(claim_id) ON DELETE RESTRICT,
 FOREIGN KEY(authority_kind,authority_uuid) REFERENCES execution_origin_authorities(authority_kind,authority_uuid) ON DELETE RESTRICT,
"#;

const V85_CATALOG_RECEIPT_TAIL_SQL: &str = r#"
 CHECK((authority_kind IS NULL AND authority_uuid IS NULL) OR (authority_kind IS NOT NULL AND authority_uuid IS NOT NULL))
);
"#;

/// V86 rebuilds the two request-key-bearing relations while retaining the
/// exact V85 DDL above as the data-bearing upgrade source.  Fresh V84 histories
/// deliberately run V85 then this rebuild, so they land on the same catalog as
/// a pre-existing V85 database.
pub(crate) fn v86_catalog_table_sql() -> String {
    [
        V85_CATALOG_BEFORE_CLAIM_MATCH_SQL,
        " CHECK(rsi_execution_origin_request_key_matches_claim(request_key,claimant_kind,source_session_id,scheduled_job_id,scheduled_fire_at,rotation_id,source_model_invocation_id,rotation_action_digest,retry_attempt,c5_marker_digest)=1),\n",
        V85_CATALOG_AFTER_CLAIM_MATCH_SQL,
        " CHECK((authority_kind IS NULL AND authority_uuid IS NULL) OR (authority_kind IS NOT NULL AND authority_uuid IS NOT NULL)),\n CHECK(rsi_execution_origin_request_key_matches_receipt(request_key,source_session_id,scheduled_job_id,scheduled_fire_at)=1)\n);\n",
    ]
    .concat()
}

/// V88 adds only new claim CHECKs around the immutable V86 request-key
/// catalog. Historical V85/V86 chunks above remain byte-for-byte inputs for
/// deployed upgrades and rewind.
const V88_CLAIM_STATE_CHECK_SQL: &str = r#"
 CHECK(
   (phase IN ('claimed','launch_ready','launching') AND provider_create_state='not_attempted' AND observed_cell_phase IS NULL AND absence_evidence IS NULL) OR
   (phase='provider_live' AND provider_create_state='created' AND observed_cell_phase IN ('configured_unpublished','published') AND absence_evidence IS NULL) OR
   (phase='recovery_quarantined' AND provider_create_state IN ('created','create_unknown') AND observed_cell_phase IN ('cleanup_requested','quarantined','terminal') AND (absence_evidence IS NULL OR absence_evidence IN ('immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent'))) OR
   (phase IN ('settling','settled','failed','abandoned','quarantined') AND (
     (provider_create_state='no_create' AND observed_cell_phase IS NULL AND absence_evidence='no_create') OR
     (provider_create_state IN ('created','create_unknown') AND observed_cell_phase='terminal' AND absence_evidence IN ('immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent'))
   ))
 ),
 CHECK(authorized_session_status IS NOT NULL AND authorized_session_write_seq IS NOT NULL AND authorized_session_status IN ('Starting','Running','WaitingApproval') AND (phase NOT IN ('claimed','launch_ready','launching') OR authorized_session_status='Starting')),
 CHECK(rsi_execution_origin_controller_is_canonical(claimant_kind,source_session_id,claimant_session_id,source_model_invocation_id,rotation_action_digest,retry_attempt,c5_marker_digest,controller_project_id,controller_idea_id,controller_transfer_key,controller_reservation_id,controller_candidate_session_id,controller_base_row_id,controller_base_event_id,controller_expires_at)=1),
 CHECK(
   (phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined') AND prepared_terminal_status IS NULL AND prepared_stop_reason IS NULL AND prepared_session_write_seq IS NULL AND prepared_provider_evidence IS NULL AND terminal_model_invocation_id IS NULL) OR
   (phase IN ('settling','settled','failed','abandoned','quarantined') AND prepared_terminal_status IS NOT NULL AND prepared_stop_reason IS NOT NULL AND prepared_session_write_seq=authorized_session_write_seq+1 AND prepared_provider_evidence=absence_evidence AND terminal_model_invocation_id IS NOT NULL AND (
     phase='settling' OR
     (phase='settled' AND prepared_terminal_status='Completed') OR
     (phase='failed' AND prepared_terminal_status='Failed') OR
     (phase IN ('abandoned','quarantined') AND prepared_terminal_status='Interrupted')
   ))
 ),
 CHECK(rsi_execution_origin_c5_is_canonical(source_session_id,prepared_terminal_status,prepared_c5_cause,prepared_c5_key,prepared_c5_value,prepared_c5_at,prepared_c5_digest,prepared_c5_expected_retry_count,prepared_c5_max_retries)=1),
"#;

pub(crate) fn v88_catalog_table_sql() -> String {
    [
        V85_CATALOG_BEFORE_CLAIM_MATCH_SQL,
        " CHECK(rsi_execution_origin_request_key_matches_claim(request_key,claimant_kind,source_session_id,scheduled_job_id,scheduled_fire_at,rotation_id,source_model_invocation_id,rotation_action_digest,retry_attempt,c5_marker_digest)=1),\n",
        V88_CLAIM_STATE_CHECK_SQL,
        V85_CATALOG_AFTER_CLAIM_MATCH_SQL,
        " CHECK((authority_kind IS NULL AND authority_uuid IS NULL) OR (authority_kind IS NOT NULL AND authority_uuid IS NOT NULL)),\n CHECK(rsi_execution_origin_request_key_matches_receipt(request_key,source_session_id,scheduled_job_id,scheduled_fire_at)=1)\n);\n",
        V88_TRANSITION_COMMAND_TABLE_SQL,
    ]
    .concat()
}

const V88_TRANSITION_COMMAND_TABLE_SQL: &str = r#"
ALTER TABLE execution_origin_claims ADD COLUMN committed_binding TEXT NOT NULL DEFAULT 'historical' CHECK(committed_binding IN ('historical','active'));
CREATE UNIQUE INDEX idx_execution_origin_authorities_committed_state ON execution_origin_authorities(authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,phase,active_claim_id,boot_id);
CREATE UNIQUE INDEX idx_execution_origin_claims_committed_state ON execution_origin_claims(claim_id,authority_kind,authority_uuid,phase,claimant_session_id,claimant_kind,expected_owner_generation,expected_claim_generation,boot_id,committed_binding);
CREATE UNIQUE INDEX idx_execution_origin_claims_committed_active ON execution_origin_claims(authority_kind,authority_uuid) WHERE committed_binding='active';
CREATE TABLE execution_origin_active_claim_couplings (
 claim_id TEXT PRIMARY KEY,
 authority_kind TEXT NOT NULL CHECK(authority_kind IN ('ordinary','sandbox')),
 authority_uuid TEXT NOT NULL,
 claim_phase TEXT NOT NULL CHECK(claim_phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')),
 authority_phase TEXT NOT NULL CHECK(authority_phase IN ('claimed','launch_ready','launching','provider_live','quarantined','settling')),
 owner_session_id TEXT NOT NULL,
 claimant_kind TEXT NOT NULL CHECK(claimant_kind IN ('continue','handoff','agent_fresh','rotation','automatic_retry')),
 expected_owner_generation INTEGER NOT NULL CHECK(expected_owner_generation>0),
 owner_generation INTEGER NOT NULL CHECK(owner_generation=expected_owner_generation+CASE WHEN claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END),
 expected_claim_generation INTEGER NOT NULL CHECK(expected_claim_generation>=0),
 claim_generation INTEGER NOT NULL CHECK(claim_generation=expected_claim_generation+1),
 claim_boot_id TEXT NOT NULL,
 authority_active_claim_id TEXT NOT NULL CHECK(authority_active_claim_id=claim_id),
 authority_boot_id TEXT NOT NULL CHECK(authority_boot_id=claim_boot_id),
 claim_binding TEXT NOT NULL DEFAULT 'active' CHECK(claim_binding='active'),
 CHECK(authority_phase=CASE WHEN claim_phase='recovery_quarantined' THEN 'quarantined' WHEN claim_phase IN ('settled','failed','abandoned','quarantined') THEN 'settling' ELSE claim_phase END),
 FOREIGN KEY(claim_id,authority_kind,authority_uuid,claim_phase,owner_session_id,claimant_kind,expected_owner_generation,expected_claim_generation,claim_boot_id,claim_binding) REFERENCES execution_origin_claims(claim_id,authority_kind,authority_uuid,phase,claimant_session_id,claimant_kind,expected_owner_generation,expected_claim_generation,boot_id,committed_binding) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
 FOREIGN KEY(authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,authority_phase,authority_active_claim_id,authority_boot_id) REFERENCES execution_origin_authorities(authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,phase,active_claim_id,boot_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE execution_origin_transition_commands (
 transition_id TEXT PRIMARY KEY CHECK(length(transition_id)=36 AND transition_id=lower(transition_id) AND substr(transition_id,9,1)='-' AND substr(transition_id,14,1)='-' AND substr(transition_id,19,1)='-' AND substr(transition_id,24,1)='-' AND length(replace(transition_id,'-',''))=32 AND replace(transition_id,'-','') NOT GLOB '*[^0-9a-f]*'),
 event_id TEXT NOT NULL UNIQUE CHECK(length(event_id)=36 AND event_id=lower(event_id) AND substr(event_id,9,1)='-' AND substr(event_id,14,1)='-' AND substr(event_id,19,1)='-' AND substr(event_id,24,1)='-' AND length(replace(event_id,'-',''))=32 AND replace(event_id,'-','') NOT GLOB '*[^0-9a-f]*'),
 authority_kind TEXT NOT NULL CHECK(authority_kind IN ('ordinary','sandbox')),
 authority_uuid TEXT NOT NULL,
 claim_id TEXT NOT NULL REFERENCES execution_origin_claims(claim_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
 from_phase TEXT NOT NULL CHECK(from_phase IN ('idle','claimed','launch_ready','launching','provider_live','settling','quarantined')),
 to_phase TEXT NOT NULL CHECK(to_phase IN ('idle','claimed','launch_ready','launching','provider_live','settling','quarantined')),
 from_owner_generation INTEGER NOT NULL CHECK(from_owner_generation>0),
 to_owner_generation INTEGER NOT NULL CHECK(to_owner_generation>0),
 from_event_sequence INTEGER NOT NULL CHECK(from_event_sequence>0),
 to_owner_session_id TEXT NOT NULL,
 to_claim_generation INTEGER NOT NULL CHECK(to_claim_generation>0),
 to_active_claim_id TEXT,
 to_boot_id TEXT,
 to_quarantine_code TEXT,
 provider_absence_evidence TEXT CHECK(provider_absence_evidence IS NULL OR provider_absence_evidence IN ('no_create','immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent')),
 event_kind TEXT NOT NULL CHECK(event_kind IN ('claimed','launch_ready','launching','provider_live','settlement_prepared','settled','quarantined')),
 detail_json TEXT NOT NULL CHECK(json_valid(detail_json)),
 occurred_at TEXT NOT NULL CHECK(occurred_at GLOB '????-??-??T??:??:??.?????????Z'),
 FOREIGN KEY(authority_kind,authority_uuid) REFERENCES execution_origin_authorities(authority_kind,authority_uuid) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
 UNIQUE(authority_kind,authority_uuid,from_event_sequence)
);
"#;

pub(crate) fn v85_table_sql() -> String {
    [
        V85_SESSION_COLUMN_SQL,
        V85_CATALOG_BEFORE_CLAIM_MATCH_SQL,
        V85_CATALOG_AFTER_CLAIM_MATCH_SQL,
        V85_CATALOG_RECEIPT_TAIL_SQL,
    ]
    .concat()
}

pub(crate) fn v85_catalog_table_sql() -> String {
    [
        V85_CATALOG_BEFORE_CLAIM_MATCH_SQL,
        V85_CATALOG_AFTER_CLAIM_MATCH_SQL,
        V85_CATALOG_RECEIPT_TAIL_SQL,
    ]
    .concat()
}

/// Reconstruct the exact request-key portion of the sealed pre-RK V85
/// catalog.  This is test-only migration input: V86 must upgrade databases
/// created before the parser functions and family/provenance CHECK landed,
/// rather than proving only an upgrade from the already-amended V85 text.
#[cfg(test)]
pub(crate) fn v85_pre_rk_catalog_table_sql() -> String {
    const AMENDED_KEY_CHECK: &str = "request_key TEXT NOT NULL UNIQUE CHECK(rsi_execution_origin_request_key_is_canonical(request_key)=1)";
    const SEALED_KEY_CHECK: &str = r#"request_key TEXT NOT NULL UNIQUE CHECK(
   (request_key GLOB 'continue:????????-????-????-????-????????????:*:*' AND length(request_key)>=112 AND replace(replace(substr(request_key,10),':',''),'-','') NOT GLOB '*[^0-9a-f]*') OR
   (request_key GLOB 'agent-fresh:????????-????-????-????-????????????:????-??-??T??:??:??.?????????Z' AND length(request_key)=79) OR
   (request_key GLOB 'handoff:????????-????-????-????-????????????:none:*' AND length(request_key)>50 AND substr(request_key,51) NOT GLOB '*[^0-9]*') OR
   (request_key GLOB 'handoff:????????-????-????-????-????????????:????????-????-????-????-????????????:*' AND length(request_key)>82 AND replace(replace(substr(request_key,9),':',''),'-','') NOT GLOB '*[^0-9a-f]*') OR
   (request_key GLOB 'rotation:????????-????-????-????-????????????:????????-????-????-????-????????????:none:*' AND length(request_key)=152 AND substr(request_key,89) NOT GLOB '*[^0-9a-f]*') OR
   (request_key GLOB 'rotation:????????-????-????-????-????????????:????????-????-????-????-????????????:????????-????-????-????-????????????:*' AND length(request_key)=184 AND replace(replace(substr(request_key,10),':',''),'-','') NOT GLOB '*[^0-9a-f]*') OR
   (request_key GLOB 'retry:????????-????-????-????-????????????:*:*' AND length(request_key)>=109 AND replace(replace(substr(request_key,7),':',''),'-','') NOT GLOB '*[^0-9a-f]*')
 )"#;
    const AMENDED_CLAIMANT_CHECK: &str = r#" CHECK(
   rsi_execution_origin_request_key_family(request_key)=claimant_kind AND (
     (claimant_kind='continue' AND trigger_kind IN ('direct','answer_question','stall_classifier','scheduled_resume','terminal_watch') AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND rotation_id IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='handoff' AND trigger_kind='handoff_write' AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='agent_fresh' AND trigger_kind IN ('rpc_agent_fresh','native_agent_fresh') AND scheduled_job_id IS NOT NULL AND scheduled_fire_at IS NOT NULL AND rotation_id IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='rotation' AND trigger_kind='context_rotation' AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND source_model_invocation_id IS NOT NULL AND rotation_action_digest IS NOT NULL AND retry_attempt IS NULL AND max_retries IS NULL AND c5_marker_digest IS NULL) OR
     (claimant_kind='automatic_retry' AND trigger_kind='automatic_retry' AND scheduled_job_id IS NULL AND scheduled_fire_at IS NULL AND rotation_id IS NULL AND source_model_invocation_id IS NULL AND rotation_action_digest IS NULL AND retry_attempt IS NOT NULL AND max_retries IS NOT NULL AND c5_marker_digest IS NOT NULL)
   )
 ),"#;
    const SEALED_CLAIMANT_CHECK: &str = r#" CHECK((claimant_kind='agent_fresh') = (scheduled_job_id IS NOT NULL AND scheduled_fire_at IS NOT NULL)),
 CHECK((claimant_kind='rotation') = (source_model_invocation_id IS NOT NULL AND rotation_action_digest IS NOT NULL)),
 CHECK((claimant_kind='automatic_retry') = (retry_attempt IS NOT NULL AND max_retries IS NOT NULL AND c5_marker_digest IS NOT NULL)),"#;

    let amended = v85_table_sql();
    assert_eq!(
        amended.matches(AMENDED_KEY_CHECK).count(),
        2,
        "the sealed V85 reconstruction must replace both request-key CHECKs"
    );
    assert_eq!(
        amended.matches(AMENDED_CLAIMANT_CHECK).count(),
        1,
        "the sealed V85 reconstruction must replace the sole claimant CHECK"
    );
    amended
        .replace(AMENDED_KEY_CHECK, SEALED_KEY_CHECK)
        .replacen(AMENDED_CLAIMANT_CHECK, SEALED_CLAIMANT_CHECK, 1)
}

pub(crate) const V86_REBUILD_RENAME_SQL: &str = r#"
DROP TRIGGER execution_origin_authorities_no_delete;
DROP TRIGGER execution_origin_members_no_delete;
DROP TRIGGER execution_origin_claims_no_delete;
DROP TRIGGER execution_origin_events_no_delete;
DROP TRIGGER execution_origin_receipts_no_delete;
DROP TRIGGER execution_origin_members_immutable;
DROP TRIGGER execution_origin_events_immutable;
DROP TRIGGER execution_origin_receipts_immutable;
DROP TRIGGER execution_origin_authority_transition;
DROP TRIGGER execution_origin_claim_rank;
DROP TRIGGER execution_origin_claim_prepared_immutable;
DROP TRIGGER sessions_execution_origin_delete_guard;
DROP TRIGGER sessions_execution_origin_write_guard;
DROP INDEX idx_execution_origin_authorities_recovery;
DROP INDEX idx_execution_origin_members_history;
DROP INDEX idx_execution_origin_claims_active;
DROP INDEX idx_execution_origin_claim_agent_fresh;
DROP INDEX idx_execution_origin_claim_rotation;
DROP INDEX idx_execution_origin_claim_retry;
DROP INDEX idx_execution_origin_claim_controller_recovery;
DROP INDEX idx_execution_origin_claim_controller_key;
DROP INDEX idx_execution_origin_claim_controller_transfer_key;
DROP INDEX idx_execution_origin_claim_agent_fresh_latest;
DROP INDEX idx_execution_origin_events_order;
DROP INDEX idx_execution_origin_receipts_source_time;
ALTER TABLE execution_origin_authorities RENAME TO execution_origin_authorities_v85;
ALTER TABLE execution_origin_claims RENAME TO execution_origin_claims_v85;
ALTER TABLE execution_origin_members RENAME TO execution_origin_members_v85;
ALTER TABLE execution_origin_events RENAME TO execution_origin_events_v85;
ALTER TABLE execution_origin_receipts RENAME TO execution_origin_receipts_v85;
"#;

pub(crate) const V86_REBUILD_COPY_AND_DROP_SQL: &str = r#"
INSERT INTO execution_origin_authorities SELECT * FROM execution_origin_authorities_v85;
INSERT INTO execution_origin_claims SELECT * FROM execution_origin_claims_v85;
INSERT INTO execution_origin_members SELECT * FROM execution_origin_members_v85;
INSERT INTO execution_origin_events SELECT * FROM execution_origin_events_v85;
INSERT INTO execution_origin_receipts SELECT * FROM execution_origin_receipts_v85;
DROP TABLE execution_origin_receipts_v85;
DROP TABLE execution_origin_events_v85;
DROP TABLE execution_origin_members_v85;
DROP TABLE execution_origin_claims_v85;
DROP TABLE execution_origin_authorities_v85;
"#;

pub(crate) const V88_REBUILD_RENAME_SQL: &str = r#"
DROP TRIGGER IF EXISTS execution_origin_transition_commands_no_delete;
DROP TRIGGER IF EXISTS execution_origin_transition_commands_immutable;
DROP TRIGGER IF EXISTS execution_origin_transition_commands_apply;
DROP TRIGGER IF EXISTS execution_origin_events_require_transition_command;
DROP TRIGGER IF EXISTS execution_origin_authority_requires_transition_command;
DROP TRIGGER IF EXISTS execution_origin_authority_active_requires_coupling;
DROP TRIGGER IF EXISTS execution_origin_authority_insert_requires_seed;
DROP TRIGGER IF EXISTS execution_origin_transition_commands_require_active_coupling;
DROP TRIGGER IF EXISTS execution_origin_claim_active_requires_coupling;
DROP TRIGGER IF EXISTS execution_origin_active_claim_couplings_delete_guard;
DROP TABLE IF EXISTS execution_origin_transition_commands;
DROP TABLE IF EXISTS execution_origin_active_claim_couplings;
DROP INDEX IF EXISTS idx_execution_origin_claims_committed_active;
DROP INDEX IF EXISTS idx_execution_origin_claims_committed_state;
DROP INDEX IF EXISTS idx_execution_origin_authorities_committed_state;
DROP TRIGGER execution_origin_authorities_no_delete;
DROP TRIGGER execution_origin_members_no_delete;
DROP TRIGGER execution_origin_claims_no_delete;
DROP TRIGGER execution_origin_events_no_delete;
DROP TRIGGER execution_origin_receipts_no_delete;
DROP TRIGGER execution_origin_members_immutable;
DROP TRIGGER execution_origin_events_immutable;
DROP TRIGGER execution_origin_receipts_immutable;
DROP TRIGGER execution_origin_authority_transition;
DROP TRIGGER execution_origin_claim_rank;
DROP TRIGGER execution_origin_claim_prepared_immutable;
DROP TRIGGER sessions_execution_origin_delete_guard;
DROP TRIGGER sessions_execution_origin_write_guard;
DROP TRIGGER execution_origin_receipts_claim_match;
DROP INDEX idx_execution_origin_authorities_recovery;
DROP INDEX idx_execution_origin_members_history;
DROP INDEX idx_execution_origin_claims_active;
DROP INDEX idx_execution_origin_claim_agent_fresh;
DROP INDEX idx_execution_origin_claim_rotation;
DROP INDEX idx_execution_origin_claim_retry;
DROP INDEX idx_execution_origin_claim_controller_recovery;
DROP INDEX idx_execution_origin_claim_controller_key;
DROP INDEX idx_execution_origin_claim_controller_transfer_key;
DROP INDEX idx_execution_origin_claim_agent_fresh_latest;
DROP INDEX idx_execution_origin_events_order;
DROP INDEX idx_execution_origin_receipts_source_time;
ALTER TABLE execution_origin_authorities RENAME TO execution_origin_authorities_v87;
ALTER TABLE execution_origin_claims RENAME TO execution_origin_claims_v87;
ALTER TABLE execution_origin_members RENAME TO execution_origin_members_v87;
ALTER TABLE execution_origin_events RENAME TO execution_origin_events_v87;
ALTER TABLE execution_origin_receipts RENAME TO execution_origin_receipts_v87;
"#;

/// Every column is named on both sides so V88 cannot silently normalize,
/// default, reorder, or omit a deployed byte during the five-table rebuild.
pub(crate) const V88_REBUILD_COPY_AND_DROP_SQL: &str = r#"
INSERT INTO execution_origin_authorities(authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,event_sequence,active_claim_id,phase,boot_id,quarantine_code,created_at,updated_at)
SELECT authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,event_sequence,active_claim_id,phase,boot_id,quarantine_code,created_at,updated_at FROM execution_origin_authorities_v87;
INSERT INTO execution_origin_claims(claim_id,request_key,authority_kind,authority_uuid,requested_origin_session_id,source_session_id,claimant_session_id,claimant_kind,trigger_kind,expected_owner_generation,expected_claim_generation,prior_session_status,boot_id,model_invocation_id,model_no_execution_request_json,model_no_execution_request_digest,scheduled_job_id,scheduled_fire_at,rotation_id,source_model_invocation_id,rotation_action_digest,retry_attempt,max_retries,c5_marker_digest,provider_create_state,observed_cell_phase,absence_evidence,authorized_session_status,authorized_session_write_seq,prepared_terminal_status,prepared_stop_reason,prepared_session_write_seq,prepared_provider_evidence,terminal_model_invocation_id,prepared_c5_cause,prepared_c5_key,prepared_c5_value,prepared_c5_at,prepared_c5_digest,prepared_c5_expected_retry_count,prepared_c5_max_retries,controller_project_id,controller_idea_id,controller_transfer_key,controller_reservation_id,controller_candidate_session_id,controller_base_row_id,controller_base_event_id,controller_expires_at,phase,claimed_at,updated_at)
SELECT claim_id,request_key,authority_kind,authority_uuid,requested_origin_session_id,source_session_id,claimant_session_id,claimant_kind,trigger_kind,expected_owner_generation,expected_claim_generation,prior_session_status,boot_id,model_invocation_id,model_no_execution_request_json,model_no_execution_request_digest,scheduled_job_id,scheduled_fire_at,rotation_id,source_model_invocation_id,rotation_action_digest,retry_attempt,max_retries,c5_marker_digest,provider_create_state,observed_cell_phase,absence_evidence,authorized_session_status,authorized_session_write_seq,prepared_terminal_status,prepared_stop_reason,prepared_session_write_seq,prepared_provider_evidence,terminal_model_invocation_id,prepared_c5_cause,prepared_c5_key,prepared_c5_value,prepared_c5_at,prepared_c5_digest,prepared_c5_expected_retry_count,prepared_c5_max_retries,controller_project_id,controller_idea_id,controller_transfer_key,controller_reservation_id,controller_candidate_session_id,controller_base_row_id,controller_base_event_id,controller_expires_at,phase,claimed_at,updated_at FROM execution_origin_claims_v87;
INSERT INTO execution_origin_members(session_id,authority_kind,authority_uuid,joined_owner_generation,source_session_id,join_cause,claim_id,joined_at)
SELECT session_id,authority_kind,authority_uuid,joined_owner_generation,source_session_id,join_cause,claim_id,joined_at FROM execution_origin_members_v87;
INSERT INTO execution_origin_events(event_id,authority_kind,authority_uuid,sequence,claim_id,from_phase,to_phase,from_owner_generation,to_owner_generation,provider_absence_evidence,event_kind,detail_json,occurred_at)
SELECT event_id,authority_kind,authority_uuid,sequence,claim_id,from_phase,to_phase,from_owner_generation,to_owner_generation,provider_absence_evidence,event_kind,detail_json,occurred_at FROM execution_origin_events_v87;
INSERT INTO execution_origin_receipts(receipt_id,request_key,claim_id,authority_kind,authority_uuid,requested_origin_session_id,source_session_id,claimant_session_id,scheduled_job_id,scheduled_fire_at,outcome,code,occurred_at)
SELECT receipt_id,request_key,claim_id,authority_kind,authority_uuid,requested_origin_session_id,source_session_id,claimant_session_id,scheduled_job_id,scheduled_fire_at,outcome,code,occurred_at FROM execution_origin_receipts_v87;
DROP TABLE execution_origin_receipts_v87;
DROP TABLE execution_origin_events_v87;
DROP TABLE execution_origin_members_v87;
DROP TABLE execution_origin_claims_v87;
DROP TABLE execution_origin_authorities_v87;
"#;

/// V87-to-V88 copy adds only the derived committed-state discriminator. Every
/// predecessor column remains an explicit byte-for-byte copy.
pub(crate) const V88_REBUILD_UPGRADE_COPY_SQL: &str = r#"
INSERT INTO execution_origin_authorities(authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,event_sequence,active_claim_id,phase,boot_id,quarantine_code,created_at,updated_at)
SELECT authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,event_sequence,active_claim_id,phase,boot_id,quarantine_code,created_at,updated_at FROM execution_origin_authorities_v87;
INSERT INTO execution_origin_claims(claim_id,request_key,authority_kind,authority_uuid,requested_origin_session_id,source_session_id,claimant_session_id,claimant_kind,trigger_kind,expected_owner_generation,expected_claim_generation,prior_session_status,boot_id,model_invocation_id,model_no_execution_request_json,model_no_execution_request_digest,scheduled_job_id,scheduled_fire_at,rotation_id,source_model_invocation_id,rotation_action_digest,retry_attempt,max_retries,c5_marker_digest,provider_create_state,observed_cell_phase,absence_evidence,authorized_session_status,authorized_session_write_seq,prepared_terminal_status,prepared_stop_reason,prepared_session_write_seq,prepared_provider_evidence,terminal_model_invocation_id,prepared_c5_cause,prepared_c5_key,prepared_c5_value,prepared_c5_at,prepared_c5_digest,prepared_c5_expected_retry_count,prepared_c5_max_retries,controller_project_id,controller_idea_id,controller_transfer_key,controller_reservation_id,controller_candidate_session_id,controller_base_row_id,controller_base_event_id,controller_expires_at,phase,claimed_at,updated_at,committed_binding)
SELECT c.claim_id,c.request_key,c.authority_kind,c.authority_uuid,c.requested_origin_session_id,c.source_session_id,c.claimant_session_id,c.claimant_kind,c.trigger_kind,c.expected_owner_generation,c.expected_claim_generation,c.prior_session_status,c.boot_id,c.model_invocation_id,c.model_no_execution_request_json,c.model_no_execution_request_digest,c.scheduled_job_id,c.scheduled_fire_at,c.rotation_id,c.source_model_invocation_id,c.rotation_action_digest,c.retry_attempt,c.max_retries,c.c5_marker_digest,c.provider_create_state,c.observed_cell_phase,c.absence_evidence,c.authorized_session_status,c.authorized_session_write_seq,c.prepared_terminal_status,c.prepared_stop_reason,c.prepared_session_write_seq,c.prepared_provider_evidence,c.terminal_model_invocation_id,c.prepared_c5_cause,c.prepared_c5_key,c.prepared_c5_value,c.prepared_c5_at,c.prepared_c5_digest,c.prepared_c5_expected_retry_count,c.prepared_c5_max_retries,c.controller_project_id,c.controller_idea_id,c.controller_transfer_key,c.controller_reservation_id,c.controller_candidate_session_id,c.controller_base_row_id,c.controller_base_event_id,c.controller_expires_at,c.phase,c.claimed_at,c.updated_at,CASE WHEN EXISTS (SELECT 1 FROM execution_origin_authorities_v87 a WHERE a.authority_kind=c.authority_kind AND a.authority_uuid=c.authority_uuid AND a.active_claim_id=c.claim_id) THEN 'active' ELSE 'historical' END FROM execution_origin_claims_v87 c;
INSERT INTO execution_origin_active_claim_couplings(claim_id,authority_kind,authority_uuid,claim_phase,authority_phase,owner_session_id,claimant_kind,expected_owner_generation,owner_generation,expected_claim_generation,claim_generation,claim_boot_id,authority_active_claim_id,authority_boot_id)
SELECT c.claim_id,c.authority_kind,c.authority_uuid,c.phase,a.phase,c.claimant_session_id,c.claimant_kind,c.expected_owner_generation,a.owner_generation,c.expected_claim_generation,a.claim_generation,c.boot_id,a.active_claim_id,a.boot_id FROM execution_origin_claims c JOIN execution_origin_authorities a ON a.authority_kind=c.authority_kind AND a.authority_uuid=c.authority_uuid WHERE c.committed_binding='active';
INSERT INTO execution_origin_members(session_id,authority_kind,authority_uuid,joined_owner_generation,source_session_id,join_cause,claim_id,joined_at)
SELECT session_id,authority_kind,authority_uuid,joined_owner_generation,source_session_id,join_cause,claim_id,joined_at FROM execution_origin_members_v87;
INSERT INTO execution_origin_events(event_id,authority_kind,authority_uuid,sequence,claim_id,from_phase,to_phase,from_owner_generation,to_owner_generation,provider_absence_evidence,event_kind,detail_json,occurred_at)
SELECT event_id,authority_kind,authority_uuid,sequence,claim_id,from_phase,to_phase,from_owner_generation,to_owner_generation,provider_absence_evidence,event_kind,detail_json,occurred_at FROM execution_origin_events_v87;
INSERT INTO execution_origin_receipts(receipt_id,request_key,claim_id,authority_kind,authority_uuid,requested_origin_session_id,source_session_id,claimant_session_id,scheduled_job_id,scheduled_fire_at,outcome,code,occurred_at)
SELECT receipt_id,request_key,claim_id,authority_kind,authority_uuid,requested_origin_session_id,source_session_id,claimant_session_id,scheduled_job_id,scheduled_fire_at,outcome,code,occurred_at FROM execution_origin_receipts_v87;
"#;

/// Dropping the exact V87 source is deliberately separate from the upgrade
/// copy so production can authenticate every source/destination row count
/// before any predecessor bytes become unreachable.
pub(crate) const V88_REBUILD_DROP_V87_SQL: &str = r#"
DROP TABLE execution_origin_receipts_v87;
DROP TABLE execution_origin_events_v87;
DROP TABLE execution_origin_members_v87;
DROP TABLE execution_origin_claims_v87;
DROP TABLE execution_origin_authorities_v87;
"#;

pub(crate) const V85_INDEX_SQL: &str = r#"
CREATE INDEX idx_execution_origin_authorities_recovery ON execution_origin_authorities(phase,updated_at,authority_kind,authority_uuid);
CREATE INDEX idx_execution_origin_members_history ON execution_origin_members(authority_kind,authority_uuid,joined_owner_generation,session_id);
CREATE INDEX idx_execution_origin_claims_active ON execution_origin_claims(authority_kind,authority_uuid,phase,claimed_at);
CREATE UNIQUE INDEX idx_execution_origin_claim_agent_fresh ON execution_origin_claims(scheduled_job_id,scheduled_fire_at) WHERE claimant_kind='agent_fresh';
CREATE UNIQUE INDEX idx_execution_origin_claim_rotation ON execution_origin_claims(source_session_id,source_model_invocation_id,ifnull(rotation_id,''),rotation_action_digest) WHERE claimant_kind='rotation';
CREATE UNIQUE INDEX idx_execution_origin_claim_retry ON execution_origin_claims(source_session_id,retry_attempt,c5_marker_digest) WHERE claimant_kind='automatic_retry';
CREATE INDEX idx_execution_origin_claim_controller_recovery ON execution_origin_claims(controller_reservation_id,controller_candidate_session_id,phase);
CREATE INDEX idx_execution_origin_claim_controller_key ON execution_origin_claims(controller_transfer_key);
CREATE UNIQUE INDEX idx_execution_origin_claim_controller_transfer_key ON execution_origin_claims(controller_transfer_key) WHERE controller_transfer_key IS NOT NULL;
CREATE INDEX idx_execution_origin_claim_agent_fresh_latest ON execution_origin_claims(scheduled_job_id,claimed_at DESC) WHERE claimant_kind='agent_fresh';
CREATE INDEX idx_execution_origin_events_order ON execution_origin_events(authority_kind,authority_uuid,sequence);
CREATE INDEX idx_execution_origin_receipts_source_time ON execution_origin_receipts(source_session_id,occurred_at);
"#;

pub(crate) const V85_TRIGGER_SQL: &str = r#"
CREATE TRIGGER execution_origin_authorities_no_delete BEFORE DELETE ON execution_origin_authorities BEGIN SELECT RAISE(ABORT,'execution origin authority history is immutable'); END;
CREATE TRIGGER execution_origin_members_no_delete BEFORE DELETE ON execution_origin_members BEGIN SELECT RAISE(ABORT,'execution origin membership history is immutable'); END;
CREATE TRIGGER execution_origin_claims_no_delete BEFORE DELETE ON execution_origin_claims BEGIN SELECT RAISE(ABORT,'execution origin claim history is immutable'); END;
CREATE TRIGGER execution_origin_events_no_delete BEFORE DELETE ON execution_origin_events BEGIN SELECT RAISE(ABORT,'execution origin event history is immutable'); END;
CREATE TRIGGER execution_origin_receipts_no_delete BEFORE DELETE ON execution_origin_receipts BEGIN SELECT RAISE(ABORT,'execution origin receipt history is immutable'); END;
CREATE TRIGGER execution_origin_members_immutable BEFORE UPDATE ON execution_origin_members BEGIN SELECT RAISE(ABORT,'execution origin memberships are immutable'); END;
CREATE TRIGGER execution_origin_events_immutable BEFORE UPDATE ON execution_origin_events BEGIN SELECT RAISE(ABORT,'execution origin events are immutable'); END;
CREATE TRIGGER execution_origin_receipts_immutable BEFORE UPDATE ON execution_origin_receipts BEGIN SELECT RAISE(ABORT,'execution origin receipts are immutable'); END;
CREATE TRIGGER execution_origin_authority_transition BEFORE UPDATE ON execution_origin_authorities
WHEN NEW.authority_kind!=OLD.authority_kind OR NEW.authority_uuid!=OLD.authority_uuid OR NEW.created_at!=OLD.created_at OR NEW.updated_at NOT GLOB '????-??-??T??:??:??.?????????Z' OR NEW.owner_generation<OLD.owner_generation OR NEW.claim_generation<OLD.claim_generation OR NEW.event_sequence<OLD.event_sequence OR (NEW.owner_generation!=OLD.owner_generation AND NEW.owner_generation!=OLD.owner_generation+1) OR (NEW.claim_generation!=OLD.claim_generation AND NOT (OLD.phase='idle' AND NEW.phase='claimed' AND NEW.claim_generation=OLD.claim_generation+1)) OR ((NEW.phase!=OLD.phase OR NEW.owner_session_id IS NOT OLD.owner_session_id OR NEW.owner_generation!=OLD.owner_generation) AND (NEW.event_sequence!=OLD.event_sequence+1 OR NOT EXISTS (SELECT 1 FROM execution_origin_events e WHERE e.authority_kind=NEW.authority_kind AND e.authority_uuid=NEW.authority_uuid AND e.sequence=NEW.event_sequence AND e.claim_id IS CASE WHEN NEW.active_claim_id IS NULL THEN OLD.active_claim_id ELSE NEW.active_claim_id END AND e.from_phase IS OLD.phase AND e.to_phase=NEW.phase AND e.from_owner_generation IS OLD.owner_generation AND e.to_owner_generation=NEW.owner_generation AND e.event_kind=CASE WHEN NEW.phase='claimed' THEN 'claimed' WHEN NEW.phase='launch_ready' THEN 'launch_ready' WHEN NEW.phase='launching' THEN 'launching' WHEN NEW.phase='provider_live' THEN 'provider_live' WHEN NEW.phase='settling' THEN 'settlement_prepared' WHEN NEW.phase='idle' AND OLD.phase='unverified' THEN 'reconciled' WHEN NEW.phase='idle' THEN 'settled' WHEN NEW.phase='quarantined' THEN 'quarantined' ELSE 'reconciled' END))) OR (NEW.phase!=OLD.phase AND NOT ((OLD.phase='unverified' AND NEW.phase IN ('quarantined','idle')) OR (OLD.phase='idle' AND NEW.phase='claimed') OR (OLD.phase='claimed' AND NEW.phase IN ('launch_ready','settling','quarantined')) OR (OLD.phase='launch_ready' AND NEW.phase IN ('launching','settling','quarantined')) OR (OLD.phase='launching' AND NEW.phase IN ('provider_live','settling','quarantined')) OR (OLD.phase='provider_live' AND NEW.phase IN ('settling','quarantined')) OR (OLD.phase='quarantined' AND NEW.phase='settling') OR (OLD.phase='settling' AND NEW.phase='idle')))
BEGIN SELECT RAISE(ABORT,'invalid execution origin authority transition'); END;
CREATE TRIGGER execution_origin_claim_rank BEFORE UPDATE ON execution_origin_claims
WHEN NEW.claim_id!=OLD.claim_id OR NEW.request_key!=OLD.request_key OR NEW.authority_kind!=OLD.authority_kind OR NEW.authority_uuid!=OLD.authority_uuid OR NEW.requested_origin_session_id!=OLD.requested_origin_session_id OR NEW.source_session_id!=OLD.source_session_id OR NEW.claimant_session_id!=OLD.claimant_session_id OR NEW.claimant_kind!=OLD.claimant_kind OR NEW.trigger_kind!=OLD.trigger_kind OR NEW.expected_owner_generation!=OLD.expected_owner_generation OR NEW.expected_claim_generation!=OLD.expected_claim_generation OR NEW.prior_session_status!=OLD.prior_session_status OR NEW.boot_id!=OLD.boot_id OR NEW.model_invocation_id!=OLD.model_invocation_id OR NEW.model_no_execution_request_json!=OLD.model_no_execution_request_json OR NEW.model_no_execution_request_digest!=OLD.model_no_execution_request_digest OR NEW.scheduled_job_id IS NOT OLD.scheduled_job_id OR NEW.scheduled_fire_at IS NOT OLD.scheduled_fire_at OR NEW.rotation_id IS NOT OLD.rotation_id OR NEW.source_model_invocation_id IS NOT OLD.source_model_invocation_id OR NEW.rotation_action_digest IS NOT OLD.rotation_action_digest OR NEW.retry_attempt IS NOT OLD.retry_attempt OR NEW.max_retries IS NOT OLD.max_retries OR NEW.c5_marker_digest IS NOT OLD.c5_marker_digest OR NEW.controller_project_id IS NOT OLD.controller_project_id OR NEW.controller_idea_id IS NOT OLD.controller_idea_id OR NEW.controller_transfer_key IS NOT OLD.controller_transfer_key OR NEW.controller_reservation_id IS NOT OLD.controller_reservation_id OR NEW.controller_candidate_session_id IS NOT OLD.controller_candidate_session_id OR NEW.controller_base_row_id IS NOT OLD.controller_base_row_id OR NEW.controller_base_event_id IS NOT OLD.controller_base_event_id OR NEW.controller_expires_at IS NOT OLD.controller_expires_at OR NEW.claimed_at!=OLD.claimed_at OR NEW.updated_at NOT GLOB '????-??-??T??:??:??.?????????Z' OR (NEW.phase!=OLD.phase AND NOT ((OLD.phase='claimed' AND NEW.phase IN ('launch_ready','settling','recovery_quarantined')) OR (OLD.phase='launch_ready' AND NEW.phase IN ('launching','settling','recovery_quarantined')) OR (OLD.phase='launching' AND NEW.phase IN ('provider_live','settling','recovery_quarantined')) OR (OLD.phase='provider_live' AND NEW.phase IN ('settling','recovery_quarantined')) OR (OLD.phase='recovery_quarantined' AND NEW.phase='settling') OR (OLD.phase='settling' AND NEW.phase IN ('settled','failed','abandoned','quarantined'))))
BEGIN SELECT RAISE(ABORT,'invalid execution origin claim transition or immutable provenance'); END;
CREATE TRIGGER execution_origin_claim_prepared_immutable BEFORE UPDATE ON execution_origin_claims
WHEN (OLD.prepared_terminal_status IS NOT NULL AND (NEW.prepared_terminal_status IS NOT OLD.prepared_terminal_status OR NEW.prepared_stop_reason IS NOT OLD.prepared_stop_reason OR NEW.prepared_session_write_seq IS NOT OLD.prepared_session_write_seq OR NEW.prepared_provider_evidence IS NOT OLD.prepared_provider_evidence OR NEW.terminal_model_invocation_id IS NOT OLD.terminal_model_invocation_id OR NEW.prepared_c5_cause IS NOT OLD.prepared_c5_cause OR NEW.prepared_c5_key IS NOT OLD.prepared_c5_key OR NEW.prepared_c5_value IS NOT OLD.prepared_c5_value OR NEW.prepared_c5_at IS NOT OLD.prepared_c5_at OR NEW.prepared_c5_digest IS NOT OLD.prepared_c5_digest OR NEW.prepared_c5_expected_retry_count IS NOT OLD.prepared_c5_expected_retry_count OR NEW.prepared_c5_max_retries IS NOT OLD.prepared_c5_max_retries)) OR (OLD.prepared_terminal_status IS NULL AND NEW.prepared_terminal_status IS NOT NULL AND NEW.phase!='settling') OR (OLD.prepared_c5_cause IS NULL AND NEW.prepared_c5_cause IS NOT NULL AND NEW.phase!='settling')
BEGIN SELECT RAISE(ABORT,'execution origin prepared bytes are immutable'); END;
CREATE TRIGGER sessions_execution_origin_delete_guard BEFORE DELETE ON sessions WHEN OLD.execution_origin_claim_id IS NOT NULL BEGIN SELECT RAISE(ABORT,'active execution origin claim binds session'); END;
CREATE TRIGGER sessions_execution_origin_write_guard BEFORE UPDATE ON sessions
WHEN OLD.execution_origin_claim_id IS NOT NULL OR NEW.execution_origin_claim_id IS NOT NULL
BEGIN SELECT CASE WHEN
  (OLD.execution_origin_claim_id IS NULL AND NEW.execution_origin_claim_id IS NOT NULL AND NEW.execution_origin_write_seq>OLD.execution_origin_write_seq AND NEW.pending_archive IS OLD.pending_archive AND EXISTS (SELECT 1 FROM execution_origin_claims c WHERE c.claim_id=NEW.execution_origin_claim_id AND c.claimant_session_id=NEW.id AND c.phase='claimed' AND c.authorized_session_status='Starting' AND c.authorized_session_status=NEW.status AND c.authorized_session_write_seq=NEW.execution_origin_write_seq)) OR
  (OLD.execution_origin_claim_id IS NOT NULL AND NEW.execution_origin_claim_id=OLD.execution_origin_claim_id AND NEW.execution_origin_write_seq>OLD.execution_origin_write_seq AND NEW.pending_archive IS OLD.pending_archive AND EXISTS (SELECT 1 FROM execution_origin_claims c WHERE c.claim_id=OLD.execution_origin_claim_id AND c.claimant_session_id=NEW.id AND c.phase IN ('claimed','launch_ready','launching','provider_live') AND c.authorized_session_status=NEW.status AND c.authorized_session_write_seq=NEW.execution_origin_write_seq)) OR
  (OLD.execution_origin_claim_id IS NOT NULL AND NEW.execution_origin_claim_id IS NULL AND NEW.execution_origin_write_seq>OLD.execution_origin_write_seq AND NEW.pending_archive IS OLD.pending_archive AND EXISTS (SELECT 1 FROM execution_origin_claims c WHERE c.claim_id=OLD.execution_origin_claim_id AND c.claimant_session_id=NEW.id AND c.phase='settling' AND c.prepared_terminal_status=NEW.status AND c.prepared_session_write_seq=NEW.execution_origin_write_seq))
THEN NULL ELSE RAISE(ABORT,'invalid execution origin session binding') END; END;
"#;

pub(crate) const V86_TRIGGER_SQL: &str = r#"
CREATE TRIGGER execution_origin_receipts_claim_match BEFORE INSERT ON execution_origin_receipts
WHEN NEW.claim_id IS NOT NULL AND NOT EXISTS (
 SELECT 1 FROM execution_origin_claims c
 WHERE c.claim_id=NEW.claim_id
   AND c.request_key=NEW.request_key
   AND c.authority_kind IS NEW.authority_kind
   AND c.authority_uuid IS NEW.authority_uuid
   AND c.requested_origin_session_id IS NEW.requested_origin_session_id
   AND c.source_session_id IS NEW.source_session_id
   AND c.claimant_session_id IS NEW.claimant_session_id
   AND c.scheduled_job_id IS NEW.scheduled_job_id
   AND c.scheduled_fire_at IS NEW.scheduled_fire_at
)
BEGIN SELECT RAISE(ABORT,'execution origin receipt must exactly match its claim'); END;
"#;

/// V87 replaces only the Session write fence. The historical V85/V86 trigger
/// text above remains the accepted reconstruction input for rewind and copied
/// deployed databases.
pub(crate) const V87_SESSION_TRIGGER_SQL: &str = r#"
CREATE TRIGGER sessions_execution_origin_write_guard BEFORE UPDATE ON sessions
WHEN OLD.execution_origin_claim_id IS NOT NULL OR NEW.execution_origin_claim_id IS NOT NULL
BEGIN SELECT CASE WHEN
  (
    OLD.execution_origin_claim_id IS NULL
    AND NEW.execution_origin_claim_id IS NOT NULL
    AND NEW.execution_origin_write_seq=OLD.execution_origin_write_seq+1
    AND NEW.pending_archive IS OLD.pending_archive
    AND NEW.stop_reason IS OLD.stop_reason
    AND NEW.status='Starting'
    AND EXISTS (
      SELECT 1
      FROM execution_origin_claims c
      JOIN execution_origin_authorities a
        ON a.authority_kind=c.authority_kind AND a.authority_uuid=c.authority_uuid
      JOIN execution_origin_events e
        ON e.authority_kind=a.authority_kind AND e.authority_uuid=a.authority_uuid
       AND e.sequence=a.event_sequence
      WHERE c.claim_id=NEW.execution_origin_claim_id
        AND c.claimant_session_id=NEW.id
        AND c.phase='claimed'
        AND c.provider_create_state='not_attempted'
        AND c.observed_cell_phase IS NULL
        AND c.absence_evidence IS NULL
        AND c.authorized_session_status='Starting'
        AND c.authorized_session_status=NEW.status
        AND c.authorized_session_write_seq=NEW.execution_origin_write_seq
        AND a.owner_session_id=NEW.id
        AND a.active_claim_id=c.claim_id
        AND a.phase='claimed'
        AND a.claim_generation=c.expected_claim_generation+1
        AND a.owner_generation=c.expected_owner_generation+
          CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
        AND a.boot_id=c.boot_id
        AND e.claim_id=c.claim_id
        AND e.from_phase='idle'
        AND e.to_phase='claimed'
        AND e.from_owner_generation=c.expected_owner_generation
        AND e.to_owner_generation=a.owner_generation
        AND e.provider_absence_evidence IS NULL
        AND e.event_kind='claimed'
        AND (
          (a.authority_kind='ordinary' AND NEW.sandbox_custody_id IS NULL)
          OR
          (a.authority_kind='sandbox'
           AND NEW.sandbox_custody_id=a.authority_uuid
           AND EXISTS (
             SELECT 1 FROM sandbox_custody_roots r
             WHERE r.custody_id=a.authority_uuid
               AND r.owner_session_id=NEW.id
               AND r.generation=a.owner_generation
               AND r.state='live'
               AND r.validation_state='verified'
               AND r.validated_generation=r.generation
               AND r.effect_boot_id=c.boot_id
               AND r.reserved_effects=0
               AND r.active_effects=1
           ))
        )
    )
  ) OR
  (
    OLD.execution_origin_claim_id IS NOT NULL
    AND NEW.execution_origin_claim_id=OLD.execution_origin_claim_id
    AND NEW.execution_origin_write_seq=OLD.execution_origin_write_seq+1
    AND NEW.pending_archive IS OLD.pending_archive
    AND NEW.stop_reason IS OLD.stop_reason
    AND NEW.status IN ('Running','WaitingApproval')
    AND EXISTS (
      SELECT 1
      FROM execution_origin_claims c
      JOIN execution_origin_authorities a
        ON a.authority_kind=c.authority_kind AND a.authority_uuid=c.authority_uuid
      JOIN execution_origin_events e
        ON e.authority_kind=a.authority_kind AND e.authority_uuid=a.authority_uuid
       AND e.sequence=a.event_sequence
      WHERE c.claim_id=OLD.execution_origin_claim_id
        AND c.claimant_session_id=NEW.id
        AND c.phase='provider_live'
        AND c.provider_create_state='created'
        AND c.observed_cell_phase IN ('configured_unpublished','published')
        AND c.absence_evidence IS NULL
        AND c.authorized_session_status=NEW.status
        AND c.authorized_session_write_seq=NEW.execution_origin_write_seq
        AND a.owner_session_id=NEW.id
        AND a.active_claim_id=c.claim_id
        AND a.phase='provider_live'
        AND a.claim_generation=c.expected_claim_generation+1
        AND a.owner_generation=c.expected_owner_generation+
          CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
        AND a.boot_id=c.boot_id
        AND e.claim_id=c.claim_id
        AND e.from_phase='launching'
        AND e.to_phase='provider_live'
        AND e.from_owner_generation=a.owner_generation
        AND e.to_owner_generation=a.owner_generation
        AND e.provider_absence_evidence IS NULL
        AND e.event_kind='provider_live'
        AND (
          (a.authority_kind='ordinary' AND NEW.sandbox_custody_id IS NULL)
          OR
          (a.authority_kind='sandbox'
           AND NEW.sandbox_custody_id=a.authority_uuid
           AND EXISTS (
             SELECT 1 FROM sandbox_custody_roots r
             WHERE r.custody_id=a.authority_uuid
               AND r.owner_session_id=NEW.id
               AND r.generation=a.owner_generation
               AND r.state='live'
               AND r.validation_state='verified'
               AND r.validated_generation=r.generation
               AND r.effect_boot_id=c.boot_id
               AND r.reserved_effects=0
               AND r.active_effects=1
           ))
        )
    )
  ) OR
  (
    OLD.execution_origin_claim_id IS NOT NULL
    AND NEW.execution_origin_claim_id IS NULL
    AND NEW.execution_origin_write_seq=OLD.execution_origin_write_seq+1
    AND NEW.pending_archive IS OLD.pending_archive
    AND NEW.status IN ('Completed','Failed','Interrupted')
    AND EXISTS (
      SELECT 1
      FROM execution_origin_claims c
      JOIN execution_origin_authorities a
        ON a.authority_kind=c.authority_kind AND a.authority_uuid=c.authority_uuid
      JOIN execution_origin_receipts rcp
        ON rcp.claim_id=c.claim_id
       AND rcp.request_key=c.request_key
       AND rcp.authority_kind=c.authority_kind
       AND rcp.authority_uuid=c.authority_uuid
       AND rcp.requested_origin_session_id=c.requested_origin_session_id
       AND rcp.source_session_id IS c.source_session_id
       AND rcp.claimant_session_id IS c.claimant_session_id
       AND rcp.scheduled_job_id IS c.scheduled_job_id
       AND rcp.scheduled_fire_at IS c.scheduled_fire_at
       AND rcp.outcome=c.phase
      JOIN execution_origin_events settled
        ON settled.authority_kind=a.authority_kind
       AND settled.authority_uuid=a.authority_uuid
       AND settled.sequence=a.event_sequence
      JOIN execution_origin_events prepared
        ON prepared.authority_kind=a.authority_kind
       AND prepared.authority_uuid=a.authority_uuid
       AND prepared.sequence=a.event_sequence-1
      JOIN model_invocations terminal
        ON terminal.id=c.terminal_model_invocation_id
       AND terminal.session_id=NEW.id
       AND terminal.status IN ('completed','failed','cancelled','denied')
      WHERE c.claim_id=OLD.execution_origin_claim_id
        AND c.claimant_session_id=NEW.id
        AND c.phase IN ('settled','failed','abandoned','quarantined')
        AND ((c.phase='settled' AND NEW.status='Completed')
          OR (c.phase='failed' AND NEW.status='Failed')
          OR (c.phase IN ('abandoned','quarantined') AND NEW.status='Interrupted'))
        AND c.prepared_terminal_status=NEW.status
        AND c.prepared_stop_reason=NEW.stop_reason
        AND c.prepared_session_write_seq=NEW.execution_origin_write_seq
        AND c.prepared_provider_evidence IS NOT NULL
        AND c.authorized_session_status=OLD.status
        AND c.authorized_session_write_seq=OLD.execution_origin_write_seq
        AND a.owner_session_id=NEW.id
        AND a.active_claim_id IS NULL
        AND a.phase='idle'
        AND a.claim_generation=c.expected_claim_generation+1
        AND a.owner_generation=c.expected_owner_generation+
          CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
        AND a.boot_id IS NULL
        AND settled.claim_id=c.claim_id
        AND settled.from_phase='settling'
        AND settled.to_phase='idle'
        AND settled.from_owner_generation=a.owner_generation
        AND settled.to_owner_generation=a.owner_generation
        AND settled.provider_absence_evidence=c.prepared_provider_evidence
        AND settled.event_kind='settled'
        AND prepared.claim_id=c.claim_id
        AND prepared.to_phase='settling'
        AND prepared.to_owner_generation=a.owner_generation
        AND prepared.provider_absence_evidence=c.prepared_provider_evidence
        AND prepared.event_kind='settlement_prepared'
        AND (c.prepared_c5_key IS NULL OR EXISTS (
          SELECT 1 FROM daemon_settings ds
          WHERE ds.key=c.prepared_c5_key
            AND ds.value=c.prepared_c5_value
            AND ds.updated_at=c.prepared_c5_at
        ))
        AND (
          (a.authority_kind='ordinary' AND NEW.sandbox_custody_id IS NULL)
          OR
          (a.authority_kind='sandbox'
           AND NEW.sandbox_custody_id=a.authority_uuid
           AND EXISTS (
             SELECT 1 FROM sandbox_custody_roots root
             WHERE root.custody_id=a.authority_uuid
               AND root.owner_session_id=NEW.id
               AND root.generation=a.owner_generation
               AND root.state='live'
               AND root.validation_state='verified'
               AND root.validated_generation=root.generation
               AND root.effect_boot_id IS NULL
               AND root.reserved_effects=0
               AND root.active_effects=0
           ))
        )
    )
  )
THEN NULL ELSE RAISE(ABORT,'invalid execution origin session binding') END; END;
"#;

/// V88 replacements for the three same-name state-machine guards. The other
/// immutable-history guards, indexes, receipt matcher, and V87 Session fence
/// are restored from their sealed historical literals during the rebuild.
pub(crate) const V88_STATE_TRIGGER_SQL: &str = r#"
CREATE TRIGGER execution_origin_claim_rank BEFORE UPDATE ON execution_origin_claims
WHEN
  OLD.committed_binding='historical' OR
  NEW.claim_id!=OLD.claim_id OR NEW.request_key!=OLD.request_key OR NEW.authority_kind!=OLD.authority_kind OR NEW.authority_uuid!=OLD.authority_uuid OR NEW.requested_origin_session_id!=OLD.requested_origin_session_id OR NEW.source_session_id!=OLD.source_session_id OR NEW.claimant_session_id!=OLD.claimant_session_id OR NEW.claimant_kind!=OLD.claimant_kind OR NEW.trigger_kind!=OLD.trigger_kind OR NEW.expected_owner_generation!=OLD.expected_owner_generation OR NEW.expected_claim_generation!=OLD.expected_claim_generation OR NEW.prior_session_status!=OLD.prior_session_status OR NEW.boot_id!=OLD.boot_id OR NEW.model_invocation_id!=OLD.model_invocation_id OR NEW.model_no_execution_request_json!=OLD.model_no_execution_request_json OR NEW.model_no_execution_request_digest!=OLD.model_no_execution_request_digest OR NEW.scheduled_job_id IS NOT OLD.scheduled_job_id OR NEW.scheduled_fire_at IS NOT OLD.scheduled_fire_at OR NEW.rotation_id IS NOT OLD.rotation_id OR NEW.source_model_invocation_id IS NOT OLD.source_model_invocation_id OR NEW.rotation_action_digest IS NOT OLD.rotation_action_digest OR NEW.retry_attempt IS NOT OLD.retry_attempt OR NEW.max_retries IS NOT OLD.max_retries OR NEW.c5_marker_digest IS NOT OLD.c5_marker_digest OR NEW.controller_project_id IS NOT OLD.controller_project_id OR NEW.controller_idea_id IS NOT OLD.controller_idea_id OR NEW.controller_transfer_key IS NOT OLD.controller_transfer_key OR NEW.controller_reservation_id IS NOT OLD.controller_reservation_id OR NEW.controller_candidate_session_id IS NOT OLD.controller_candidate_session_id OR NEW.controller_base_row_id IS NOT OLD.controller_base_row_id OR NEW.controller_base_event_id IS NOT OLD.controller_base_event_id OR NEW.controller_expires_at IS NOT OLD.controller_expires_at OR NEW.claimed_at!=OLD.claimed_at OR NEW.updated_at NOT GLOB '????-??-??T??:??:??.?????????Z' OR
  (NEW.phase!=OLD.phase AND NOT ((OLD.phase='claimed' AND NEW.phase IN ('launch_ready','settling','recovery_quarantined')) OR (OLD.phase='launch_ready' AND NEW.phase IN ('launching','settling','recovery_quarantined')) OR (OLD.phase='launching' AND NEW.phase IN ('provider_live','settling','recovery_quarantined')) OR (OLD.phase='provider_live' AND NEW.phase IN ('settling','recovery_quarantined')) OR (OLD.phase='recovery_quarantined' AND NEW.phase='settling') OR (OLD.phase='settling' AND NEW.phase IN ('settled','failed','abandoned','quarantined')))) OR
  ((NEW.provider_create_state!=OLD.provider_create_state OR NEW.observed_cell_phase IS NOT OLD.observed_cell_phase OR NEW.absence_evidence IS NOT OLD.absence_evidence) AND NOT (
    (OLD.phase='launching' AND NEW.phase='provider_live' AND OLD.provider_create_state='not_attempted' AND OLD.observed_cell_phase IS NULL AND OLD.absence_evidence IS NULL AND NEW.provider_create_state='created' AND NEW.observed_cell_phase IN ('configured_unpublished','published') AND NEW.absence_evidence IS NULL) OR
    (OLD.phase='provider_live' AND NEW.phase='provider_live' AND OLD.provider_create_state='created' AND NEW.provider_create_state='created' AND OLD.observed_cell_phase='configured_unpublished' AND NEW.observed_cell_phase='published' AND OLD.absence_evidence IS NULL AND NEW.absence_evidence IS NULL) OR
    (NEW.phase='recovery_quarantined' AND OLD.phase IN ('claimed','launch_ready','launching','provider_live') AND NEW.provider_create_state IN ('created','create_unknown') AND NEW.observed_cell_phase IN ('cleanup_requested','quarantined','terminal') AND (NEW.absence_evidence IS NULL OR NEW.absence_evidence IN ('immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent'))) OR
    (NEW.phase='settling' AND (
      (OLD.phase IN ('claimed','launch_ready','launching') AND OLD.provider_create_state='not_attempted' AND NEW.provider_create_state='no_create' AND NEW.observed_cell_phase IS NULL AND NEW.absence_evidence='no_create') OR
      (OLD.phase='provider_live' AND OLD.provider_create_state='created' AND NEW.provider_create_state='created' AND NEW.observed_cell_phase='terminal' AND NEW.absence_evidence IN ('immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent')) OR
      (OLD.phase='recovery_quarantined' AND NEW.provider_create_state=OLD.provider_create_state AND NEW.provider_create_state IN ('created','create_unknown') AND NEW.observed_cell_phase='terminal' AND NEW.absence_evidence IN ('immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent'))
    ))
  )) OR
  ((NEW.authorized_session_status IS NOT OLD.authorized_session_status OR NEW.authorized_session_write_seq IS NOT OLD.authorized_session_write_seq) AND NOT (OLD.phase='provider_live' AND NEW.phase='provider_live' AND NEW.authorized_session_status IN ('Running','WaitingApproval') AND NEW.authorized_session_write_seq=OLD.authorized_session_write_seq+1)) OR
  (NEW.committed_binding!=OLD.committed_binding AND NOT (
    OLD.committed_binding='active' AND NEW.committed_binding='historical' AND NEW.phase=OLD.phase AND OLD.phase IN ('settled','failed','abandoned','quarantined') AND EXISTS (SELECT 1 FROM execution_origin_transition_commands t JOIN execution_origin_authorities a ON a.authority_kind=t.authority_kind AND a.authority_uuid=t.authority_uuid WHERE t.claim_id=OLD.claim_id AND t.authority_kind=OLD.authority_kind AND t.authority_uuid=OLD.authority_uuid AND t.to_phase='idle' AND a.phase='idle' AND a.event_sequence=t.from_event_sequence+1 AND a.owner_session_id=OLD.claimant_session_id AND a.owner_generation=OLD.expected_owner_generation+CASE WHEN OLD.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END AND a.claim_generation=OLD.expected_claim_generation+1 AND a.active_claim_id IS NULL AND a.boot_id IS NULL)
  ))
BEGIN SELECT RAISE(ABORT,'invalid execution origin claim transition or immutable provenance'); END;

CREATE TRIGGER execution_origin_claim_prepared_immutable BEFORE UPDATE ON execution_origin_claims
WHEN
  (OLD.prepared_terminal_status IS NOT NULL AND (NEW.prepared_terminal_status IS NOT OLD.prepared_terminal_status OR NEW.prepared_stop_reason IS NOT OLD.prepared_stop_reason OR NEW.prepared_session_write_seq IS NOT OLD.prepared_session_write_seq OR NEW.prepared_provider_evidence IS NOT OLD.prepared_provider_evidence OR NEW.terminal_model_invocation_id IS NOT OLD.terminal_model_invocation_id OR NEW.prepared_c5_cause IS NOT OLD.prepared_c5_cause OR NEW.prepared_c5_key IS NOT OLD.prepared_c5_key OR NEW.prepared_c5_value IS NOT OLD.prepared_c5_value OR NEW.prepared_c5_at IS NOT OLD.prepared_c5_at OR NEW.prepared_c5_digest IS NOT OLD.prepared_c5_digest OR NEW.prepared_c5_expected_retry_count IS NOT OLD.prepared_c5_expected_retry_count OR NEW.prepared_c5_max_retries IS NOT OLD.prepared_c5_max_retries)) OR
  (OLD.phase!='settling' AND NEW.phase='settling' AND (NEW.prepared_terminal_status IS NULL OR NEW.prepared_stop_reason IS NULL OR NEW.prepared_session_write_seq IS NULL OR NEW.prepared_provider_evidence IS NULL OR NEW.terminal_model_invocation_id IS NULL)) OR
  (OLD.phase!='settling' AND NEW.phase!='settling' AND (NEW.prepared_terminal_status IS NOT OLD.prepared_terminal_status OR NEW.prepared_stop_reason IS NOT OLD.prepared_stop_reason OR NEW.prepared_session_write_seq IS NOT OLD.prepared_session_write_seq OR NEW.prepared_provider_evidence IS NOT OLD.prepared_provider_evidence OR NEW.terminal_model_invocation_id IS NOT OLD.terminal_model_invocation_id OR NEW.prepared_c5_cause IS NOT OLD.prepared_c5_cause OR NEW.prepared_c5_key IS NOT OLD.prepared_c5_key OR NEW.prepared_c5_value IS NOT OLD.prepared_c5_value OR NEW.prepared_c5_at IS NOT OLD.prepared_c5_at OR NEW.prepared_c5_digest IS NOT OLD.prepared_c5_digest OR NEW.prepared_c5_expected_retry_count IS NOT OLD.prepared_c5_expected_retry_count OR NEW.prepared_c5_max_retries IS NOT OLD.prepared_c5_max_retries))
BEGIN SELECT RAISE(ABORT,'execution origin prepared bytes are immutable'); END;

CREATE TRIGGER execution_origin_claim_active_requires_coupling AFTER INSERT ON execution_origin_claims
WHEN NEW.committed_binding!='active' OR NOT EXISTS (
  SELECT 1 FROM execution_origin_active_claim_couplings c
  WHERE c.claim_id=NEW.claim_id AND c.authority_kind=NEW.authority_kind AND c.authority_uuid=NEW.authority_uuid AND c.claim_phase=NEW.phase AND c.owner_session_id=NEW.claimant_session_id AND c.claimant_kind=NEW.claimant_kind AND c.expected_owner_generation=NEW.expected_owner_generation AND c.expected_claim_generation=NEW.expected_claim_generation AND c.claim_boot_id=NEW.boot_id
)
BEGIN SELECT RAISE(ABORT,'active execution origin claim requires committed coupling'); END;

CREATE TRIGGER execution_origin_authority_active_requires_coupling BEFORE UPDATE ON execution_origin_authorities
WHEN NEW.active_claim_id IS NOT NULL AND NOT EXISTS (
  SELECT 1 FROM execution_origin_active_claim_couplings c JOIN execution_origin_claims x ON x.claim_id=c.claim_id
  WHERE c.claim_id=NEW.active_claim_id AND c.authority_kind=NEW.authority_kind AND c.authority_uuid=NEW.authority_uuid AND c.authority_phase=NEW.phase AND c.owner_session_id=NEW.owner_session_id AND c.owner_generation=NEW.owner_generation AND c.claim_generation=NEW.claim_generation AND c.authority_active_claim_id=NEW.active_claim_id AND c.authority_boot_id=NEW.boot_id AND x.committed_binding='active'
)
BEGIN SELECT RAISE(ABORT,'active execution origin authority requires committed coupling'); END;

CREATE TRIGGER execution_origin_authority_insert_requires_seed BEFORE INSERT ON execution_origin_authorities
WHEN NEW.phase!='unverified' OR NEW.active_claim_id IS NOT NULL OR NEW.boot_id IS NOT NULL OR NEW.claim_generation!=0 OR NEW.event_sequence!=1 OR NEW.quarantine_code IS NOT NULL
BEGIN SELECT RAISE(ABORT,'execution origin authority inserts only as an unverified seed'); END;

CREATE TRIGGER execution_origin_active_claim_couplings_delete_guard BEFORE DELETE ON execution_origin_active_claim_couplings
WHEN NOT EXISTS (
  SELECT 1 FROM execution_origin_transition_commands t JOIN execution_origin_authorities a ON a.authority_kind=t.authority_kind AND a.authority_uuid=t.authority_uuid
  WHERE t.claim_id=OLD.claim_id AND t.authority_kind=OLD.authority_kind AND t.authority_uuid=OLD.authority_uuid AND t.to_phase='idle' AND a.phase='idle' AND a.event_sequence=t.from_event_sequence+1 AND a.active_claim_id IS NULL AND a.boot_id IS NULL
)
BEGIN SELECT RAISE(ABORT,'active execution origin coupling clears only with final transition command'); END;

CREATE TRIGGER execution_origin_events_require_transition_command BEFORE INSERT ON execution_origin_events
WHEN NOT (
   NEW.claim_id IS NULL AND NEW.provider_absence_evidence IS NULL AND (
     (NEW.sequence=1 AND NEW.from_phase IS NULL AND NEW.from_owner_generation IS NULL AND NEW.to_phase='unverified' AND NEW.event_kind='seeded') OR
     (NEW.from_phase='unverified' AND NEW.to_phase IN ('idle','quarantined') AND NEW.from_owner_generation=NEW.to_owner_generation AND NEW.event_kind=CASE NEW.to_phase WHEN 'idle' THEN 'reconciled' ELSE 'quarantined' END AND EXISTS (
       SELECT 1 FROM execution_origin_authorities a WHERE a.authority_kind=NEW.authority_kind AND a.authority_uuid=NEW.authority_uuid AND a.phase='unverified' AND a.event_sequence+1=NEW.sequence AND a.owner_generation=NEW.from_owner_generation AND a.active_claim_id IS NULL AND a.boot_id IS NULL
     ))
   )
 ) AND NOT EXISTS (
   SELECT 1 FROM execution_origin_transition_commands t
   WHERE t.event_id=NEW.event_id AND t.authority_kind=NEW.authority_kind AND t.authority_uuid=NEW.authority_uuid AND t.claim_id=NEW.claim_id AND t.from_event_sequence+1=NEW.sequence AND t.from_phase=NEW.from_phase AND t.to_phase=NEW.to_phase AND t.from_owner_generation=NEW.from_owner_generation AND t.to_owner_generation=NEW.to_owner_generation AND t.provider_absence_evidence IS NEW.provider_absence_evidence AND t.event_kind=NEW.event_kind AND t.detail_json=NEW.detail_json AND t.occurred_at=NEW.occurred_at
 )
BEGIN SELECT RAISE(ABORT,'execution origin event requires atomic transition command'); END;

CREATE TRIGGER execution_origin_authority_requires_transition_command BEFORE UPDATE ON execution_origin_authorities
WHEN (NEW.phase!=OLD.phase OR NEW.owner_session_id IS NOT OLD.owner_session_id OR NEW.owner_generation!=OLD.owner_generation OR NEW.claim_generation!=OLD.claim_generation OR NEW.event_sequence!=OLD.event_sequence OR NEW.active_claim_id IS NOT OLD.active_claim_id OR NEW.boot_id IS NOT OLD.boot_id OR NEW.quarantine_code IS NOT OLD.quarantine_code)
 AND (OLD.active_claim_id IS NOT NULL OR NEW.active_claim_id IS NOT NULL)
 AND NOT EXISTS (
   SELECT 1 FROM execution_origin_transition_commands t
   WHERE t.authority_kind=OLD.authority_kind AND t.authority_uuid=OLD.authority_uuid AND t.from_phase=OLD.phase AND t.to_phase=NEW.phase AND t.from_owner_generation=OLD.owner_generation AND t.to_owner_generation=NEW.owner_generation AND t.from_event_sequence=OLD.event_sequence AND t.to_owner_session_id=NEW.owner_session_id AND t.to_claim_generation=NEW.claim_generation AND t.to_active_claim_id IS NEW.active_claim_id AND t.to_boot_id IS NEW.boot_id AND t.to_quarantine_code IS NEW.quarantine_code
 )
BEGIN SELECT RAISE(ABORT,'execution origin authority edge requires atomic transition command'); END;

CREATE TRIGGER execution_origin_transition_commands_require_active_coupling BEFORE INSERT ON execution_origin_transition_commands
WHEN NOT EXISTS (
  SELECT 1
  FROM execution_origin_claims x
  JOIN execution_origin_active_claim_couplings c ON c.claim_id=x.claim_id
  JOIN execution_origin_authorities a ON a.authority_kind=c.authority_kind AND a.authority_uuid=c.authority_uuid
  WHERE x.claim_id=NEW.claim_id AND x.authority_kind=NEW.authority_kind AND x.authority_uuid=NEW.authority_uuid AND x.committed_binding='active'
    AND c.authority_kind=NEW.authority_kind AND c.authority_uuid=NEW.authority_uuid
    AND c.claim_phase=x.phase AND c.owner_session_id=x.claimant_session_id AND c.claimant_kind=x.claimant_kind AND c.expected_owner_generation=x.expected_owner_generation AND c.expected_claim_generation=x.expected_claim_generation AND c.claim_boot_id=x.boot_id
    AND a.phase=NEW.from_phase AND a.owner_generation=NEW.from_owner_generation AND a.event_sequence=NEW.from_event_sequence
    AND (
      (NEW.to_phase!='idle' AND c.authority_phase=NEW.to_phase AND c.owner_generation=NEW.to_owner_generation AND c.claim_generation=NEW.to_claim_generation AND c.authority_active_claim_id=NEW.to_active_claim_id AND c.authority_boot_id=NEW.to_boot_id) OR
      (NEW.to_phase='idle' AND x.phase IN ('settled','failed','abandoned','quarantined') AND c.authority_phase=NEW.from_phase AND c.owner_generation=NEW.from_owner_generation AND c.claim_generation=NEW.to_claim_generation AND c.authority_active_claim_id=NEW.claim_id AND c.authority_boot_id=x.boot_id AND NEW.to_active_claim_id IS NULL AND NEW.to_boot_id IS NULL)
    )
)
BEGIN SELECT RAISE(ABORT,'execution origin transition command requires exact active coupling'); END;

CREATE TRIGGER execution_origin_transition_commands_apply AFTER INSERT ON execution_origin_transition_commands
BEGIN
  SELECT CASE WHEN NOT EXISTS (
    SELECT 1 FROM execution_origin_authorities a
    WHERE a.authority_kind=NEW.authority_kind AND a.authority_uuid=NEW.authority_uuid AND a.phase=NEW.from_phase AND a.owner_generation=NEW.from_owner_generation AND a.event_sequence=NEW.from_event_sequence
  ) THEN RAISE(ABORT,'stale execution origin transition command') END;
  INSERT INTO execution_origin_events(event_id,authority_kind,authority_uuid,sequence,claim_id,from_phase,to_phase,from_owner_generation,to_owner_generation,provider_absence_evidence,event_kind,detail_json,occurred_at)
  VALUES(NEW.event_id,NEW.authority_kind,NEW.authority_uuid,NEW.from_event_sequence+1,NEW.claim_id,NEW.from_phase,NEW.to_phase,NEW.from_owner_generation,NEW.to_owner_generation,NEW.provider_absence_evidence,NEW.event_kind,NEW.detail_json,NEW.occurred_at);
  UPDATE execution_origin_authorities
  SET owner_session_id=NEW.to_owner_session_id,owner_generation=NEW.to_owner_generation,claim_generation=NEW.to_claim_generation,event_sequence=NEW.from_event_sequence+1,active_claim_id=NEW.to_active_claim_id,phase=NEW.to_phase,boot_id=NEW.to_boot_id,quarantine_code=NEW.to_quarantine_code,updated_at=NEW.occurred_at
  WHERE authority_kind=NEW.authority_kind AND authority_uuid=NEW.authority_uuid AND phase=NEW.from_phase AND owner_generation=NEW.from_owner_generation AND event_sequence=NEW.from_event_sequence;
  SELECT CASE WHEN changes()!=1 THEN RAISE(ABORT,'stale execution origin transition command') END;
  DELETE FROM execution_origin_active_claim_couplings
  WHERE claim_id=NEW.claim_id AND NEW.to_phase='idle';
  UPDATE execution_origin_claims SET committed_binding='historical'
  WHERE claim_id=NEW.claim_id AND NEW.to_phase='idle' AND committed_binding='active' AND phase IN ('settled','failed','abandoned','quarantined');
END;

CREATE TRIGGER execution_origin_transition_commands_immutable BEFORE UPDATE ON execution_origin_transition_commands
BEGIN SELECT RAISE(ABORT,'execution origin transition command is immutable'); END;
CREATE TRIGGER execution_origin_transition_commands_no_delete BEFORE DELETE ON execution_origin_transition_commands
BEGIN SELECT RAISE(ABORT,'execution origin transition command is immutable'); END;

CREATE TRIGGER execution_origin_authority_transition BEFORE UPDATE ON execution_origin_authorities
WHEN
  NEW.authority_kind!=OLD.authority_kind OR NEW.authority_uuid!=OLD.authority_uuid OR NEW.created_at!=OLD.created_at OR NEW.updated_at NOT GLOB '????-??-??T??:??:??.?????????Z' OR
  ((NEW.phase!=OLD.phase OR NEW.owner_session_id IS NOT OLD.owner_session_id OR NEW.owner_generation!=OLD.owner_generation OR NEW.claim_generation!=OLD.claim_generation OR NEW.event_sequence!=OLD.event_sequence OR NEW.active_claim_id IS NOT OLD.active_claim_id OR NEW.boot_id IS NOT OLD.boot_id OR NEW.quarantine_code IS NOT OLD.quarantine_code) AND NOT (
    (OLD.phase='unverified' AND NEW.phase='idle' AND NEW.owner_session_id IS OLD.owner_session_id AND NEW.owner_generation=OLD.owner_generation AND NEW.claim_generation=OLD.claim_generation AND OLD.active_claim_id IS NULL AND NEW.active_claim_id IS NULL AND OLD.boot_id IS NULL AND NEW.boot_id IS NULL AND NEW.quarantine_code IS OLD.quarantine_code AND NEW.event_sequence=OLD.event_sequence+1 AND EXISTS (
      SELECT 1 FROM execution_origin_events e WHERE e.authority_kind=NEW.authority_kind AND e.authority_uuid=NEW.authority_uuid AND e.sequence=NEW.event_sequence AND e.claim_id IS NULL AND e.from_phase='unverified' AND e.to_phase='idle' AND e.from_owner_generation=OLD.owner_generation AND e.to_owner_generation=NEW.owner_generation AND e.provider_absence_evidence IS NULL AND e.event_kind='reconciled'
    )) OR
    (OLD.phase='unverified' AND NEW.phase='quarantined' AND NEW.owner_session_id IS OLD.owner_session_id AND NEW.owner_generation=OLD.owner_generation AND NEW.claim_generation=OLD.claim_generation AND OLD.active_claim_id IS NULL AND NEW.active_claim_id IS NULL AND OLD.boot_id IS NULL AND NEW.boot_id IS NULL AND NEW.quarantine_code IS NOT NULL AND NEW.event_sequence=OLD.event_sequence+1 AND EXISTS (
      SELECT 1 FROM execution_origin_events e WHERE e.authority_kind=NEW.authority_kind AND e.authority_uuid=NEW.authority_uuid AND e.sequence=NEW.event_sequence AND e.claim_id IS NULL AND e.from_phase='unverified' AND e.to_phase='quarantined' AND e.from_owner_generation=OLD.owner_generation AND e.to_owner_generation=NEW.owner_generation AND e.provider_absence_evidence IS NULL AND e.event_kind='quarantined'
    )) OR
    (OLD.phase='idle' AND NEW.phase='claimed' AND OLD.active_claim_id IS NULL AND OLD.boot_id IS NULL AND NEW.active_claim_id IS NOT NULL AND NEW.boot_id IS NOT NULL AND NEW.claim_generation=OLD.claim_generation+1 AND NEW.quarantine_code IS NULL AND NEW.event_sequence=OLD.event_sequence+1 AND EXISTS (
      SELECT 1 FROM execution_origin_claims c JOIN execution_origin_events e ON e.authority_kind=NEW.authority_kind AND e.authority_uuid=NEW.authority_uuid AND e.sequence=NEW.event_sequence
      WHERE c.claim_id=NEW.active_claim_id AND c.authority_kind=NEW.authority_kind AND c.authority_uuid=NEW.authority_uuid AND c.phase='claimed' AND c.claimant_session_id=NEW.owner_session_id AND c.expected_owner_generation=OLD.owner_generation AND c.expected_claim_generation=OLD.claim_generation AND c.boot_id=NEW.boot_id AND NEW.owner_generation=c.expected_owner_generation+CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END AND ((c.claimant_kind IN ('continue','handoff') AND NEW.owner_session_id IS OLD.owner_session_id) OR c.claimant_kind IN ('agent_fresh','rotation','automatic_retry')) AND e.claim_id=c.claim_id AND e.from_phase='idle' AND e.to_phase='claimed' AND e.from_owner_generation=OLD.owner_generation AND e.to_owner_generation=NEW.owner_generation AND e.provider_absence_evidence IS NULL AND e.event_kind='claimed'
    )) OR
    (NEW.owner_session_id IS OLD.owner_session_id AND NEW.owner_generation=OLD.owner_generation AND NEW.claim_generation=OLD.claim_generation AND NEW.active_claim_id IS OLD.active_claim_id AND NEW.boot_id IS OLD.boot_id AND NEW.active_claim_id IS NOT NULL AND NEW.event_sequence=OLD.event_sequence+1 AND ((NEW.phase='quarantined' AND NEW.quarantine_code IS NOT NULL) OR NEW.quarantine_code IS OLD.quarantine_code) AND ((OLD.phase='claimed' AND NEW.phase IN ('launch_ready','settling','quarantined')) OR (OLD.phase='launch_ready' AND NEW.phase IN ('launching','settling','quarantined')) OR (OLD.phase='launching' AND NEW.phase IN ('provider_live','settling','quarantined')) OR (OLD.phase='provider_live' AND NEW.phase IN ('settling','quarantined')) OR (OLD.phase='quarantined' AND NEW.phase='settling')) AND EXISTS (
      SELECT 1 FROM execution_origin_claims c JOIN execution_origin_events e ON e.authority_kind=NEW.authority_kind AND e.authority_uuid=NEW.authority_uuid AND e.sequence=NEW.event_sequence
      WHERE c.claim_id=NEW.active_claim_id AND c.authority_kind=NEW.authority_kind AND c.authority_uuid=NEW.authority_uuid AND c.claimant_session_id=NEW.owner_session_id AND c.expected_claim_generation+1=NEW.claim_generation AND c.expected_owner_generation+CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END=NEW.owner_generation AND c.boot_id=NEW.boot_id AND c.phase=CASE WHEN NEW.phase='quarantined' THEN 'recovery_quarantined' ELSE NEW.phase END AND e.claim_id=c.claim_id AND e.from_phase=OLD.phase AND e.to_phase=NEW.phase AND e.from_owner_generation=OLD.owner_generation AND e.to_owner_generation=NEW.owner_generation AND e.provider_absence_evidence IS c.absence_evidence AND e.event_kind=CASE NEW.phase WHEN 'launch_ready' THEN 'launch_ready' WHEN 'launching' THEN 'launching' WHEN 'provider_live' THEN 'provider_live' WHEN 'settling' THEN 'settlement_prepared' WHEN 'quarantined' THEN 'quarantined' END
    )) OR
    (OLD.phase='settling' AND NEW.phase='idle' AND NEW.owner_session_id IS OLD.owner_session_id AND NEW.owner_generation=OLD.owner_generation AND NEW.claim_generation=OLD.claim_generation AND OLD.active_claim_id IS NOT NULL AND NEW.active_claim_id IS NULL AND OLD.boot_id IS NOT NULL AND NEW.boot_id IS NULL AND NEW.quarantine_code IS NULL AND NEW.event_sequence=OLD.event_sequence+1 AND EXISTS (
      SELECT 1 FROM execution_origin_claims c JOIN execution_origin_events e ON e.authority_kind=NEW.authority_kind AND e.authority_uuid=NEW.authority_uuid AND e.sequence=NEW.event_sequence
      WHERE c.claim_id=OLD.active_claim_id AND c.authority_kind=NEW.authority_kind AND c.authority_uuid=NEW.authority_uuid AND c.claimant_session_id=NEW.owner_session_id AND c.expected_claim_generation+1=NEW.claim_generation AND c.expected_owner_generation+CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END=NEW.owner_generation AND c.boot_id=OLD.boot_id AND ((c.phase='settled' AND c.prepared_terminal_status='Completed') OR (c.phase='failed' AND c.prepared_terminal_status='Failed') OR (c.phase IN ('abandoned','quarantined') AND c.prepared_terminal_status='Interrupted')) AND e.claim_id=c.claim_id AND e.from_phase='settling' AND e.to_phase='idle' AND e.from_owner_generation=OLD.owner_generation AND e.to_owner_generation=NEW.owner_generation AND e.provider_absence_evidence=c.prepared_provider_evidence AND e.event_kind='settled'
    ))
  ))
BEGIN SELECT RAISE(ABORT,'invalid execution origin authority transition'); END;
"#;

/// Exact raw SQLite catalog digest of the accepted V87 five-relation source,
/// including auto-indexes, all owned triggers/indexes, and both Session guards.
pub(super) const V87_ACCEPTED_ORIGIN_CATALOG_FINGERPRINT: &str =
    "sha256:7481ed88b94838c80654b630cb37eacb299508af895ac8085968eea4348b186e";

pub(crate) fn execution_origin_catalog_fingerprint(
    tx: &Transaction<'_>,
) -> Result<String, DaemonError> {
    let mut statement = tx.prepare(
        "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master
         WHERE type IN ('table','index','trigger')
           AND (tbl_name IN ('execution_origin_authorities','execution_origin_claims','execution_origin_members','execution_origin_events','execution_origin_receipts')
                OR name IN ('sessions_execution_origin_delete_guard','sessions_execution_origin_write_guard'))
         ORDER BY type,name",
    )?;
    let catalog = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut digest = Sha256::new();
    for (kind, name, table, sql) in catalog {
        digest.update(kind);
        digest.update("\0");
        digest.update(name);
        digest.update("\0");
        digest.update(table);
        digest.update("\0");
        digest.update(sql);
        digest.update("\n");
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub(crate) fn validate_v87_catalog(tx: &Transaction<'_>) -> Result<(), DaemonError> {
    let actual = execution_origin_catalog_fingerprint(tx)?;
    if actual != V87_ACCEPTED_ORIGIN_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V88 requires exact accepted V87 origin catalog, found {actual}"
        )));
    }
    Ok(())
}

pub(crate) fn v86_session_trigger_sql() -> &'static str {
    let marker = "CREATE TRIGGER sessions_execution_origin_write_guard";
    let start = V85_TRIGGER_SQL
        .find(marker)
        .expect("historical V85 trigger batch owns the Session fence");
    V85_TRIGGER_SQL[start..].trim().trim_end_matches(';')
}

pub(crate) fn validate_v86_session_trigger(tx: &Transaction<'_>) -> Result<(), DaemonError> {
    let actual: Option<String> = tx
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='sessions_execution_origin_write_guard'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if actual.as_deref() != Some(v86_session_trigger_sql()) {
        return Err(DaemonError::Store(
            "V87 requires the exact accepted V86 Session write trigger".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_v84_catalog(tx: &Transaction<'_>) -> Result<(), DaemonError> {
    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(DaemonError::Store(format!(
            "V85 requires integrity_check=ok, got {integrity}"
        )));
    }
    let fk_errors: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if fk_errors != 0 {
        return Err(DaemonError::Store(
            "V85 requires clean V84 foreign keys".into(),
        ));
    }
    for relation in V84_REQUIRED_RELATIONS {
        let sql: Option<String> = tx
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name=?1",
                [relation],
                |row| row.get(0),
            )
            .optional()?;
        if sql.is_none() {
            return Err(DaemonError::Store(format!(
                "V85 requires accepted V84 relation `{relation}`"
            )));
        }
    }
    let fingerprint = v84_catalog_fingerprint(tx)?;
    if fingerprint != V84_ACCEPTED_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V85 requires exact accepted V83/V84 catalog, found {fingerprint}"
        )));
    }
    Ok(())
}

pub(crate) fn v84_catalog_fingerprint(tx: &Transaction<'_>) -> Result<String, DaemonError> {
    let mut stmt = tx.prepare(
        "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master \
         WHERE type IN ('table','index','trigger') ORDER BY type,name",
    )?;
    let catalog = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .filter_map(|row| match row {
            Ok((kind, name, table, sql))
                if V84_CATALOG_TABLES.contains(&table.as_str())
                    || V84_SESSION_CATALOG_OBJECTS.contains(&name.as_str()) =>
            {
                Some(Ok((kind, name, table, sql)))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut digest = Sha256::new();
    for (kind, name, table, sql) in catalog {
        digest.update(kind);
        digest.update("\0");
        digest.update(name);
        digest.update("\0");
        digest.update(table);
        digest.update("\0");
        digest.update(
            sql.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase(),
        );
        digest.update("\n");
    }
    let mut columns = tx.prepare(
        "SELECT name,type,\"notnull\",coalesce(dflt_value,''),pk,hidden \
         FROM pragma_table_xinfo('sessions') ORDER BY name",
    )?;
    let columns = columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .filter_map(|row| match row {
            Ok(column) if V84_SESSION_DEPENDENCY_COLUMNS.contains(&column.0.as_str()) => {
                Some(Ok(column))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if columns.len() != V84_SESSION_DEPENDENCY_COLUMNS.len() {
        return Err(DaemonError::Store(
            "V85 requires every accepted V84 Session dependency column".into(),
        ));
    }
    for (name, kind, not_null, default, primary_key, hidden) in columns {
        digest.update("session-column\0");
        digest.update(name);
        digest.update("\0");
        digest.update(kind.to_ascii_lowercase());
        digest.update("\0");
        digest.update(not_null.to_string());
        digest.update("\0");
        digest.update(default.to_ascii_lowercase());
        digest.update("\0");
        digest.update(primary_key.to_string());
        digest.update("\0");
        digest.update(hidden.to_string());
        digest.update("\n");
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

#[derive(Clone, Debug)]
struct OrdinaryRow {
    id: String,
    created_at: String,
    continued_from: Option<String>,
    scheduled_job_id: Option<String>,
    projection_ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeedMember {
    pub(crate) session_id: String,
    pub(crate) generation: i64,
    pub(crate) source_session_id: String,
    pub(crate) joined_at: String,
    pub(crate) edge: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HydratedOriginAuthority {
    pub(crate) kind: String,
    pub(crate) authority: String,
    pub(crate) owner: Option<String>,
    pub(crate) owner_generation: i64,
    pub(crate) claim_generation: i64,
    pub(crate) event_sequence: i64,
    pub(crate) phase: String,
    pub(crate) members: Vec<SeedMember>,
}

/// Read-only hydration for V85 consumers.  It deliberately validates every
/// persisted token and aggregate; corrupt rows are Store errors, never an
/// implicit idle authority.
pub(crate) fn hydrate_authority(
    tx: &Transaction<'_>,
    kind: &str,
    authority: &str,
) -> Result<HydratedOriginAuthority, DaemonError> {
    let authority = canonical_uuid(authority)?;
    let (owner, owner_generation, claim_generation, event_sequence, active_claim, phase, created_at, updated_at):
        (Option<String>, i64, i64, i64, Option<String>, String, String, String) = tx
        .query_row(
            "SELECT owner_session_id,owner_generation,claim_generation,event_sequence,active_claim_id,phase,created_at,updated_at FROM execution_origin_authorities WHERE authority_kind=?1 AND authority_uuid=?2",
            params![kind, authority],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
        )
        .optional()?
        .ok_or_else(|| DaemonError::Store(format!("execution origin {kind}:{authority} is missing")))?;
    if !matches!(kind, "ordinary" | "sandbox")
        || !matches!(
            phase.as_str(),
            "unverified"
                | "idle"
                | "claimed"
                | "launch_ready"
                | "launching"
                | "provider_live"
                | "settling"
                | "quarantined"
        )
        || owner_generation <= 0
        || claim_generation < 0
        || event_sequence <= 0
    {
        return Err(DaemonError::Store(format!(
            "invalid execution origin aggregate {kind}:{authority}"
        )));
    }
    if canonical_time(&created_at)? != created_at || canonical_time(&updated_at)? != updated_at {
        return Err(DaemonError::Store(format!(
            "invalid execution origin timestamp {kind}:{authority}"
        )));
    }
    if let Some(owner) = &owner {
        if canonical_uuid(owner)? != *owner {
            return Err(DaemonError::Store(format!(
                "noncanonical execution origin owner `{owner}`"
            )));
        }
    } else if !matches!(phase.as_str(), "unverified" | "quarantined") {
        return Err(DaemonError::Store(format!(
            "ownerless execution origin {kind}:{authority} is not quarantined"
        )));
    }
    match (&active_claim, phase.as_str()) {
        (
            Some(claim),
            "claimed" | "launch_ready" | "launching" | "provider_live" | "settling" | "quarantined",
        ) if canonical_uuid(claim)? == *claim => {
            let matches_authority: i64 = tx.query_row(
                "SELECT count(*) FROM execution_origin_claims WHERE claim_id=?1 AND authority_kind=?2 AND authority_uuid=?3 AND phase IN ('claimed','launch_ready','launching','provider_live','settling','recovery_quarantined','quarantined')",
                params![claim, kind, authority],
                |row| row.get(0),
            )?;
            if matches_authority != 1 {
                return Err(DaemonError::Store(format!(
                    "execution origin {kind}:{authority} has inconsistent active claim"
                )));
            }
        }
        (None, "unverified" | "idle" | "quarantined") => {}
        _ => {
            return Err(DaemonError::Store(format!(
                "execution origin {kind}:{authority} has invalid active claim binding"
            )));
        }
    }
    let mut statement = tx.prepare(
        "SELECT session_id,joined_owner_generation,source_session_id,join_cause,claim_id,joined_at FROM execution_origin_members WHERE authority_kind=?1 AND authority_uuid=?2 ORDER BY joined_owner_generation,session_id",
    )?;
    let members = statement
        .query_map(params![kind, authority], |row| {
            let session_id: String = row.get(0)?;
            let generation: i64 = row.get(1)?;
            let source_session_id: String = row.get(2)?;
            let join_cause: String = row.get(3)?;
            let claim_id: Option<String> = row.get(4)?;
            let joined_at: String = row.get(5)?;
            Ok((
                session_id,
                generation,
                source_session_id,
                join_cause,
                claim_id,
                canonical_time(&joined_at)?,
            ))
        })?
        .map(|row| {
            let (session_id, generation, source_session_id, join_cause, claim_id, joined_at) = row?;
            if generation <= 0
                || canonical_uuid(&session_id)? != session_id
                || canonical_uuid(&source_session_id)? != source_session_id
                || !matches!(
                    join_cause.as_str(),
                    "v85_seed" | "lazy_seed" | "agent_fresh" | "rotation" | "automatic_retry"
                )
                || !matches!(
                    (&claim_id, join_cause.as_str()),
                    (None, "v85_seed" | "lazy_seed")
                        | (Some(_), "agent_fresh" | "rotation" | "automatic_retry")
                )
                || claim_id.as_ref().is_some_and(|claim| {
                    canonical_uuid(claim).map_or(true, |canonical| canonical != *claim)
                })
            {
                return Err(DaemonError::Store(format!(
                    "invalid execution origin member for {kind}:{authority}"
                )));
            }
            Ok(SeedMember {
                session_id,
                generation,
                source_session_id,
                joined_at,
                edge: "hydrated".into(),
            })
        })
        .collect::<Result<Vec<_>, DaemonError>>()?;
    if members.is_empty()
        || owner.as_ref().is_some_and(|selected| {
            !members.iter().any(|member| {
                member.session_id == *selected && member.generation == owner_generation
            })
        })
    {
        return Err(DaemonError::Store(format!(
            "execution origin {kind}:{authority} has inconsistent members"
        )));
    }
    let events: i64 = tx.query_row(
        "SELECT count(*) FROM execution_origin_events WHERE authority_kind=?1 AND authority_uuid=?2 AND sequence=?3 AND to_phase=?4 AND to_owner_generation=?5",
        params![kind, authority, event_sequence, phase, owner_generation],
        |row| row.get(0),
    )?;
    if events != 1 {
        return Err(DaemonError::Store(format!(
            "execution origin {kind}:{authority} has inconsistent event sequence"
        )));
    }
    let mut event_statement = tx.prepare(
        "SELECT event_id,sequence,claim_id,from_phase,to_phase,from_owner_generation,to_owner_generation,detail_json,occurred_at FROM execution_origin_events WHERE authority_kind=?1 AND authority_uuid=?2 ORDER BY sequence",
    )?;
    let events = event_statement
        .query_map(params![kind, authority], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if events.len() != usize::try_from(event_sequence).unwrap_or(usize::MAX)
        || events.iter().enumerate().any(
            |(
                index,
                (
                    event_id,
                    sequence,
                    claim_id,
                    from_phase,
                    to_phase,
                    from_generation,
                    generation,
                    detail,
                    occurred_at,
                ),
            )| {
                canonical_uuid(event_id).map_or(true, |canonical| canonical != *event_id)
                    || *sequence != i64::try_from(index + 1).unwrap_or(-1)
                    || claim_id.as_ref().is_some_and(|claim| {
                        canonical_uuid(claim).map_or(true, |canonical| canonical != *claim)
                    })
                    || from_phase.as_ref().is_some_and(|phase| {
                        !matches!(
                            phase.as_str(),
                            "unverified"
                                | "idle"
                                | "claimed"
                                | "launch_ready"
                                | "launching"
                                | "provider_live"
                                | "settling"
                                | "quarantined"
                        )
                    })
                    || !matches!(
                        to_phase.as_str(),
                        "unverified"
                            | "idle"
                            | "claimed"
                            | "launch_ready"
                            | "launching"
                            | "provider_live"
                            | "settling"
                            | "quarantined"
                    )
                    || from_generation.is_some_and(|generation| generation <= 0)
                    || *generation <= 0
                    || serde_json::from_str::<serde_json::Value>(detail).is_err()
                    || canonical_time(occurred_at).as_deref() != Ok(occurred_at.as_str())
            },
        )
        || events.windows(2).any(|pair| {
            let (_, _, _, _, previous_phase, _, previous_generation, _, _) = &pair[0];
            let (_, _, _, from_phase, _, from_generation, _, _, _) = &pair[1];
            from_phase.as_deref() != Some(previous_phase.as_str())
                || *from_generation != Some(*previous_generation)
        })
    {
        return Err(DaemonError::Store(format!(
            "invalid execution origin event for {kind}:{authority}"
        )));
    }
    Ok(HydratedOriginAuthority {
        kind: kind.into(),
        authority,
        owner,
        owner_generation,
        claim_generation,
        event_sequence,
        phase,
        members,
    })
}

/// Backfill is intentionally a pure read/classify/insert pass.  It never uses
/// presentation order (timestamps, paths, or UUIDs) to make lineage choices.
pub(crate) fn seed_v84_authorities(tx: &Transaction<'_>) -> Result<(), DaemonError> {
    let ordinary = load_ordinary_rows(tx)?;
    let ordinary_ids = ordinary.keys().cloned().collect::<HashSet<_>>();
    let jobs = load_scheduled_job_facts(tx)?;
    // Preserve every durable edge fact.  A conflicting direct/AgentFresh
    // child is malformed, but both parents still belong to its weak component;
    // dropping either would incorrectly mint a second authority.
    let mut parents = HashMap::<String, BTreeSet<String>>::new();
    let mut edge_facts = HashMap::<String, Vec<String>>::new();
    let mut malformed = HashSet::<String>::new();

    for row in ordinary.values() {
        let direct = row.continued_from.as_ref();
        let scheduled = row.scheduled_job_id.as_ref();
        if let Some(parent) = direct {
            edge_facts
                .entry(row.id.clone())
                .or_default()
                .push(format!("continued_from:{parent}"));
            if ordinary_ids.contains(parent) {
                parents
                    .entry(row.id.clone())
                    .or_default()
                    .insert(parent.clone());
            } else {
                malformed.insert(row.id.clone());
            }
        }
        if let Some(job_id) = scheduled {
            match jobs.get(job_id) {
                Some((kind, wake)) if kind == "agent_fresh" => match wake {
                    Some(wake) if ordinary_ids.contains(wake) => {
                        edge_facts
                            .entry(row.id.clone())
                            .or_default()
                            .push(format!("agent_fresh:{job_id}:{wake}"));
                        parents
                            .entry(row.id.clone())
                            .or_default()
                            .insert(wake.clone());
                    }
                    _ => {
                        malformed.insert(row.id.clone());
                        edge_facts
                            .entry(row.id.clone())
                            .or_default()
                            .push(format!("agent_fresh:{job_id}:unresolved"));
                    }
                },
                // Fresh, including the legacy bound-Fresh spelling with a
                // wake session, is launch metadata and never an origin edge.
                Some((kind, _)) if kind == "fresh" => {}
                Some((kind, wake)) => {
                    malformed.insert(row.id.clone());
                    edge_facts.entry(row.id.clone()).or_default().push(format!(
                        "{kind}:{job_id}:{}",
                        wake.as_deref().unwrap_or("none")
                    ));
                }
                None => {
                    malformed.insert(row.id.clone());
                    edge_facts
                        .entry(row.id.clone())
                        .or_default()
                        .push(format!("unresolved:{job_id}"));
                }
            }
        }
        if !row.projection_ok {
            malformed.insert(row.id.clone());
        }
        if parents.get(&row.id).is_some_and(|facts| facts.len() != 1)
            || (direct.is_some()
                && edge_facts
                    .get(&row.id)
                    .is_some_and(|facts| facts.iter().any(|fact| fact.starts_with("agent_fresh:"))))
        {
            malformed.insert(row.id.clone());
        }
    }

    let parent_of = parents
        .iter()
        .filter_map(|(child, facts)| {
            (facts.len() == 1).then(|| (child.clone(), facts.first().expect("one parent").clone()))
        })
        .collect::<HashMap<_, _>>();

    let mut children = HashMap::<String, Vec<String>>::new();
    for (child, parent) in &parent_of {
        children
            .entry(parent.clone())
            .or_default()
            .push(child.clone());
    }
    for siblings in children.values() {
        if siblings.len() > 1 {
            malformed.extend(siblings.iter().cloned());
            if let Some(parent) = parent_of.get(&siblings[0]) {
                malformed.insert(parent.clone());
            }
        }
    }

    let mut adjacent = HashMap::<String, Vec<String>>::new();
    for id in ordinary.keys() {
        adjacent.entry(id.clone()).or_default();
    }
    for (child, facts) in &parents {
        for parent in facts {
            adjacent
                .entry(child.clone())
                .or_default()
                .push(parent.clone());
            adjacent
                .entry(parent.clone())
                .or_default()
                .push(child.clone());
        }
    }
    let mut seen = HashSet::new();
    let mut ordinary_members = HashSet::new();
    for id in ordinary.keys() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut component = BTreeSet::new();
        let mut stack = vec![id.clone()];
        while let Some(current) = stack.pop() {
            if !component.insert(current.clone()) {
                continue;
            }
            for next in adjacent.get(&current).into_iter().flatten() {
                if seen.insert(next.clone()) {
                    stack.push(next.clone());
                }
            }
        }
        let (authority, owner, phase, members) = classify_ordinary_component(
            &component,
            &ordinary,
            &parent_of,
            &edge_facts,
            &malformed,
        )?;
        let owner_generation = owner
            .as_ref()
            .and_then(|selected| members.iter().find(|member| member.session_id == *selected))
            .map(|member| member.generation)
            .unwrap_or(1);
        ordinary_members.extend(component.iter().cloned());
        insert_seed(
            tx,
            "ordinary",
            &authority,
            owner.as_deref(),
            owner_generation,
            &phase,
            members
                .first()
                .map(|member| member.joined_at.as_str())
                .ok_or_else(|| DaemonError::Store("ordinary seed has no members".into()))?,
            &members,
        )?;
    }
    seed_sandbox_authorities(tx, &ordinary_members)?;
    Ok(())
}

fn load_ordinary_rows(tx: &Transaction<'_>) -> Result<BTreeMap<String, OrdinaryRow>, DaemonError> {
    let mut statement = tx.prepare(
        "SELECT s.id,s.created_at,s.continued_from,s.scheduled_job_id,
                p.execution_state,p.custody_id
         FROM sessions s LEFT JOIN session_execution_projections p ON p.session_id=s.id
         WHERE s.session_kind NOT IN ('Group','Epic')
           AND s.sandbox_custody_id IS NULL
           AND s.sandbox_kind IS NULL AND s.sandbox_root IS NULL
           AND s.sandbox_branch IS NULL AND s.sandbox_cleanup_state IS NULL
         ORDER BY s.id",
    )?;
    statement
        .query_map([], |row| {
            let state: Option<String> = row.get(4)?;
            let custody: Option<String> = row.get(5)?;
            Ok(OrdinaryRow {
                id: row.get(0)?,
                created_at: canonical_time(&row.get::<_, String>(1)?)?,
                continued_from: row.get(2)?,
                scheduled_job_id: row.get(3)?,
                projection_ok: state.as_deref() == Some("ordinary_unsandboxed")
                    && custody.is_none(),
            })
        })?
        .map(|row| row.map(|row| (row.id.clone(), row)))
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .map_err(Into::into)
}

fn load_scheduled_job_facts(
    tx: &Transaction<'_>,
) -> Result<HashMap<String, (String, Option<String>)>, DaemonError> {
    tx.prepare("SELECT id,wake_mode,wake_session_id FROM scheduled_jobs")?
        .query_map([], |row| {
            let mode: String = row.get(1)?;
            Ok((row.get(0)?, (mode, row.get(2)?)))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(Into::into)
}

fn classify_ordinary_component(
    component: &BTreeSet<String>,
    rows: &BTreeMap<String, OrdinaryRow>,
    parent_of: &HashMap<String, String>,
    edge_facts: &HashMap<String, Vec<String>>,
    malformed: &HashSet<String>,
) -> Result<(String, Option<String>, String, Vec<SeedMember>), DaemonError> {
    let mut bad = component.iter().any(|id| malformed.contains(id));
    let roots = component
        .iter()
        .filter(|id| {
            !parent_of.contains_key(*id)
                && rows[*id].continued_from.is_none()
                && !edge_facts
                    .get(*id)
                    .is_some_and(|facts| facts.iter().any(|fact| fact.starts_with("agent_fresh:")))
        })
        .cloned()
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        bad = true;
    }
    let root = roots.first().cloned();
    let mut ordered = Vec::new();
    if let Some(root) = &root {
        let mut current = root.clone();
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current.clone()) {
                bad = true;
                break;
            }
            ordered.push(current.clone());
            let next = component
                .iter()
                .filter(|candidate| parent_of.get(*candidate) == Some(&current))
                .cloned()
                .collect::<Vec<_>>();
            if next.len() > 1 {
                bad = true;
                break;
            }
            match next.first() {
                Some(next) => current = next.clone(),
                None => break,
            }
        }
        if ordered.len() != component.len() {
            bad = true;
        }
    }
    let authority = if let Some(root) = &root {
        canonical_uuid(root)?
    } else {
        malformed_authority(component, rows, edge_facts)?
    };
    let seed_time = if bad {
        rows[component
            .first()
            .ok_or_else(|| DaemonError::Store("empty ordinary component".into()))?]
        .created_at
        .clone()
    } else {
        rows[ordered.first().expect("nonempty component")]
            .created_at
            .clone()
    };
    let members = if bad {
        component
            .iter()
            .map(|id| SeedMember {
                session_id: id.clone(),
                generation: 1,
                source_session_id: id.clone(),
                joined_at: seed_time.clone(),
                edge: edge_facts
                    .get(id)
                    .map(|facts| facts.join("|"))
                    .unwrap_or_else(|| "none".into()),
            })
            .collect()
    } else {
        ordered
            .iter()
            .enumerate()
            .map(|(index, id)| SeedMember {
                session_id: id.clone(),
                generation: i64::try_from(index + 1).expect("generation fits i64"),
                source_session_id: parent_of.get(id).cloned().unwrap_or_else(|| id.clone()),
                joined_at: seed_time.clone(),
                edge: edge_facts
                    .get(id)
                    .map(|facts| facts.join("|"))
                    .unwrap_or_else(|| "none".into()),
            })
            .collect()
    };
    let owner = (!bad).then(|| ordered.last().expect("nonempty component").clone());
    Ok((
        authority,
        owner,
        if bad { "quarantined" } else { "unverified" }.into(),
        members,
    ))
}

fn malformed_authority(
    component: &BTreeSet<String>,
    rows: &BTreeMap<String, OrdinaryRow>,
    edge_facts: &HashMap<String, Vec<String>>,
) -> Result<String, DaemonError> {
    let mut bytes = Vec::from("execution-origin-malformed-lineage/v1\0".as_bytes());
    for id in component {
        let row = &rows[id];
        bytes.extend_from_slice(id.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(row.continued_from.as_deref().unwrap_or("none").as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(row.scheduled_job_id.as_deref().unwrap_or("none").as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(
            edge_facts
                .get(id)
                .map(|facts| facts.join("|"))
                .unwrap_or_else(|| "none".into())
                .as_bytes(),
        );
        bytes.push(b'\n');
    }
    Ok(Uuid::new_v5(&MALFORMED_LINEAGE_NAMESPACE, &bytes).to_string())
}

fn seed_sandbox_authorities(
    tx: &Transaction<'_>,
    ordinary_members: &HashSet<String>,
) -> Result<(), DaemonError> {
    let mut roots = tx.prepare("SELECT custody_id,state,owner_session_id,generation,created_at FROM sandbox_custody_roots ORDER BY custody_id")?;
    let roots = roots
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                canonical_time(&r.get::<_, String>(4)?)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (custody, state, owner, generation, created_at) in roots {
        let mut statement = tx.prepare("SELECT p.session_id,p.custody_generation,s.sandbox_custody_id FROM session_execution_projections p LEFT JOIN sessions s ON s.id=p.session_id WHERE p.custody_id=?1 ORDER BY p.session_id")?;
        let projections = statement
            .query_map([&custody], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut seen = HashSet::new();
        let mut malformed = state != "live" || owner.is_none();
        let mut members = Vec::new();
        for (session, projection_generation, session_custody) in projections {
            if !seen.insert(session.clone())
                || ordinary_members.contains(&session)
                || session_custody.as_deref() != Some(&custody)
            {
                return Err(DaemonError::Store(format!(
                    "V85 sandbox custody `{custody}` has a cross-domain or inconsistent projection for `{session}`"
                )));
            }
            let projection_generation = projection_generation.unwrap_or_else(|| {
                malformed = true;
                1
            });
            if projection_generation <= 0 {
                malformed = true;
            }
            let mut event_statement = tx.prepare(
                "SELECT event_kind,cause,from_owner_session_id,to_generation,occurred_at \
                 FROM sandbox_custody_events \
                 WHERE custody_id=?1 AND to_owner_session_id=?2 \
                   AND event_kind IN ('allocated','transferred') \
                 ORDER BY sequence",
            )?;
            let events = event_statement
                .query_map(params![custody, session], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                        canonical_time(&row.get::<_, String>(4)?)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let (event_kind, cause, source_session_id, joined_generation, joined_at) =
                match events.as_slice() {
                    [(event_kind, cause, source, joined_generation, joined_at)] => (
                        event_kind.clone(),
                        cause.clone(),
                        source.clone().unwrap_or_else(|| session.clone()),
                        *joined_generation,
                        joined_at.clone(),
                    ),
                    _ => {
                        malformed = true;
                        (
                            "missing_or_ambiguous".into(),
                            "missing_or_ambiguous".into(),
                            session.clone(),
                            1,
                            created_at.clone(),
                        )
                    }
                };
            if joined_generation <= 0
                || (state == "live" && projection_generation != joined_generation)
                || (state != "live" && projection_generation != generation)
            {
                malformed = true;
            }
            members.push(SeedMember {
                session_id: session.clone(),
                generation: joined_generation,
                source_session_id,
                joined_at,
                edge: format!("sandbox:{custody}:{event_kind}:{cause}"),
            });
        }
        if owner.as_ref().is_some_and(|id| !seen.contains(id))
            || owner.as_ref().is_some_and(|id| {
                !members
                    .iter()
                    .any(|member| member.session_id == *id && member.generation == generation)
            })
        {
            malformed = true;
        }
        // Quarantine retains durable projection generations rather than
        // rewriting history.  The authority has no selected owner, so these
        // values cannot be mistaken for a claimable owner generation.
        insert_seed(
            tx,
            "sandbox",
            &canonical_uuid(&custody)?,
            if malformed { None } else { owner.as_deref() },
            generation,
            if malformed {
                "quarantined"
            } else {
                "unverified"
            },
            &created_at,
            &members,
        )?;
    }
    Ok(())
}

fn insert_seed(
    tx: &Transaction<'_>,
    kind: &str,
    authority: &str,
    owner: Option<&str>,
    owner_generation: i64,
    phase: &str,
    created_at: &str,
    members: &[SeedMember],
) -> Result<(), DaemonError> {
    let detail = seed_detail(kind, authority, owner, members)?;
    let digest = detail
        .split("\"sha256\":\"")
        .nth(1)
        .and_then(|suffix| suffix.split('"').next())
        .ok_or_else(|| DaemonError::Store("seed detail is missing digest".into()))?;
    let authority_uuid = Uuid::parse_str(authority).map_err(|error| {
        DaemonError::Store(format!("invalid seed authority `{authority}`: {error}"))
    })?;
    let event_id = Uuid::new_v5(
        &authority_uuid,
        format!("rsi.execution-origin.seed-event/v1:{digest}").as_bytes(),
    )
    .to_string();
    if owner_generation <= 0 {
        return Err(DaemonError::Store(
            "seed owner generation must be positive".into(),
        ));
    }
    tx.execute("INSERT INTO execution_origin_authorities(authority_kind,authority_uuid,owner_session_id,owner_generation,claim_generation,event_sequence,active_claim_id,phase,boot_id,quarantine_code,created_at,updated_at) VALUES(?1,?2,?3,?4,0,1,NULL,?5,NULL,?6,?7,?7)", params![kind, authority, owner, owner_generation, phase, if phase == "quarantined" { Some("v84_seed_malformed") } else { None::<&str> }, created_at])?;
    for member in members {
        tx.execute("INSERT INTO execution_origin_members(session_id,authority_kind,authority_uuid,joined_owner_generation,source_session_id,join_cause,claim_id,joined_at) VALUES(?1,?2,?3,?4,?5,'v85_seed',NULL,?6)", params![member.session_id, kind, authority, member.generation, member.source_session_id, member.joined_at])?;
    }
    tx.execute("INSERT INTO execution_origin_events(event_id,authority_kind,authority_uuid,sequence,claim_id,from_phase,to_phase,from_owner_generation,to_owner_generation,provider_absence_evidence,event_kind,detail_json,occurred_at) VALUES(?1,?2,?3,1,NULL,NULL,?4,NULL,?5,NULL,'seeded',?6,?7)", params![event_id, kind, authority, phase, owner_generation, detail, created_at])?;
    Ok(())
}

fn canonical_uuid(value: &str) -> Result<String, DaemonError> {
    let id = Uuid::parse_str(value).map_err(|error| {
        DaemonError::Store(format!("invalid V84 session UUID `{value}`: {error}"))
    })?;
    let canonical = id.to_string();
    if canonical != value {
        return Err(DaemonError::Store(format!(
            "noncanonical V85 UUID `{value}`"
        )));
    }
    Ok(canonical)
}

#[derive(Serialize)]
struct SeedDetailMember<'a> {
    session_id: &'a str,
    edge: &'a str,
    generation: i64,
    source_session_id: &'a str,
    joined_at: &'a str,
}

#[derive(Serialize)]
struct SeedDetail<'a> {
    schema: &'static str,
    authority_kind: &'a str,
    authority_uuid: &'a str,
    owner_session_id: Option<&'a str>,
    members: Vec<SeedDetailMember<'a>>,
    sha256: String,
}

fn seed_detail(
    kind: &str,
    authority: &str,
    owner: Option<&str>,
    members: &[SeedMember],
) -> Result<String, DaemonError> {
    let mut bytes = format!(
        "execution-origin-seed/v1\0{kind}\0{authority}\0{}\0",
        owner.unwrap_or("null")
    )
    .into_bytes();
    let mut tuples = members.iter().collect::<Vec<_>>();
    tuples.sort_by_key(|member| &member.session_id);
    for member in tuples {
        bytes.extend_from_slice(
            format!(
                "{}\0{}\0{}\0{}\0{}\n",
                member.session_id,
                member.edge,
                member.generation,
                member.source_session_id,
                member.joined_at
            )
            .as_bytes(),
        );
    }
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let mut members = members.iter().collect::<Vec<_>>();
    members.sort_by_key(|member| &member.session_id);
    let members = members
        .into_iter()
        .map(|member| SeedDetailMember {
            session_id: &member.session_id,
            edge: &member.edge,
            generation: member.generation,
            source_session_id: &member.source_session_id,
            joined_at: &member.joined_at,
        })
        .collect();
    serde_json::to_string(&SeedDetail {
        schema: "execution-origin-seed/v1",
        authority_kind: kind,
        authority_uuid: authority,
        owner_session_id: owner,
        members,
        sha256: digest,
    })
    .map_err(|error| DaemonError::Store(format!("serialize V85 seed detail: {error}")))
}

fn canonical_time(value: &str) -> Result<String, rusqlite::Error> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Nanos, true)
        })
        .map_err(|_| rusqlite::Error::InvalidQuery)
}
