//! Master-side contract validator (RSI-021).
//!
//! Parses the leading `PIPELINE HANDOFF — STAGE:` block from a worker
//! reply and surfaces structured errors when the worker drifts. The master
//! shells out to `rsi-contract-validate` (the thin CLI in
//! `bin/rsi-contract-validate.rs`) at each parse site in
//! `.claude/commands/master_implement.md` (and `team_implement.md` for the
//! sibling `WORKER REPORT:` shape).
//!
//! ## Why a separate module from `handoff_schema`?
//!
//! `handoff_schema` validates the on-disk handoff document — a markdown
//! file with frontmatter and required body sections. `agent_contract`
//! validates the **inline reply text** the worker returns to the master,
//! which is shorter, has a different shape (`PIPELINE HANDOFF — STAGE:`
//! preamble + key/value lines), and is checked in-memory rather than from
//! disk. The two surfaces share the lenient/strict philosophy but their
//! grammars are independent.
//!
//! ## Scope
//!
//! Two grammars are supported:
//!
//! - **`PIPELINE HANDOFF — <STAGE>:`** — the master-spawn → master-receive
//!   contract, used between `master_implement` and the three pipeline
//!   stages. Required fields per `<handoff_contract>` in
//!   `master_implement.md`: `doc_path`, `status`. IMPLEMENTATION and VERIFY
//!   additionally require `manifest_path`. Optional: `blocker`,
//!   `next_action_hint`.
//! - **`WORKER REPORT:`** — the `team_implement` worker → master contract.
//!   Required fields: `status`, `Phase`, `files_modified`. Optional:
//!   `blocker`.
//!
//! Both grammars share the first-line check: the marker MUST appear in the
//! first 200 bytes of the reply. Anything before it (prose, narration,
//! "Here's the handoff:") is a contract violation.

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::closure_kernel::{
    ClosureChildOutputEnvelopeV1, ClosureEvidencePathV1, ClosureGitShaV1,
    parse_closure_child_output_envelope_v1,
};
use crate::handoff_schema::{ContractBlock, scan_contract_block};

/// One parsed `PIPELINE HANDOFF — <STAGE>:` block.
///
/// `raw_first_line` is preserved for audit so a downstream master log can
/// quote what the worker actually emitted, not a normalized reconstruction.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PipelineHandoff {
    /// Closed pipeline stage vocabulary accepted by the handoff parser.
    pub stage: Stage,
    /// Absolute path to the artifact this stage produced. Required.
    pub doc_path: String,
    /// Absolute path to the centralized verification manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
    /// `complete`, `blocked`, or `partial`. Required.
    pub status: String,
    /// VERIFY-only daemon check pass count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_checks_passed: Option<u32>,
    /// VERIFY-only daemon check total.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// VERIFY-only failed daemon check titles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// One-sentence blocker description. Optional, present iff status != complete.
    pub blocker: Option<String>,
    /// Cross-stage linkage keys this handoff *declares* must be satisfied
    /// (S6/D1). Populated from a `satisfies:` / `covers:` / `linkage:` /
    /// `requires:` line on a VERIFY handoff; each key is a research
    /// `Finding.id` or plan-item ID that the plan said the implementation
    /// must cover. Empty for every pre-S6 handoff (strict superset). The
    /// cross-stage pass [`cross_stage_verify_coverage`] checks these against
    /// the manifest's covered keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linkage: Vec<String>,
    /// The exact first line of the reply, including any trailing whitespace.
    pub raw_first_line: String,
}

/// Closed status vocabulary for the opt-in strict master-orchestrate handoff.
///
/// The legacy parser intentionally keeps its historical string/default
/// behavior. New master-orchestrate workers use [`parse_pipeline_handoff_v2`]
/// so a missing or invented status can no longer impersonate completion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatusV2 {
    Complete,
    Partial,
    Blocked,
    HumanGate,
}

impl PipelineStatusV2 {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "complete" => Some(Self::Complete),
            "partial" => Some(Self::Partial),
            "blocked" => Some(Self::Blocked),
            "human_gate" => Some(Self::HumanGate),
            _ => None,
        }
    }
}

/// The only conditions that can legitimately require an operator turn.
/// Budget exhaustion, review findings, elapsed time, and reclassification are
/// deliberately absent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineBlockerClassV1 {
    Authority,
    ForbiddenScope,
    Destructive,
    Production,
    Resource,
    TechnicalImpasse,
}

impl PipelineBlockerClassV1 {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "authority" => Some(Self::Authority),
            "forbidden_scope" => Some(Self::ForbiddenScope),
            "destructive" => Some(Self::Destructive),
            "production" => Some(Self::Production),
            "resource" => Some(Self::Resource),
            "technical_impasse" => Some(Self::TechnicalImpasse),
            _ => None,
        }
    }
}

/// Strict V2 handoff envelope returned by the opt-in validator.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StrictPipelineHandoffV2 {
    #[serde(flatten)]
    pub handoff: PipelineHandoff,
    pub strict_status: PipelineStatusV2,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker_class: Option<PipelineBlockerClassV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker_evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<PipelineHandoffWarningV2>,
}

/// A known legacy field alias accepted by strict V2 and mapped to its
/// canonical key. Included in the validator result so the producer can repair
/// its carrier without losing the successfully parsed handoff.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PipelineHandoffWarningV2 {
    pub alias: String,
    pub canonical_key: String,
}

/// Durable continuation state asserted by a master-orchestrate final report.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationContinuationStateV1 {
    ChildWatch,
    ResumeWake,
    QueueExhausted,
    HumanGate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationModeV1 {
    Slice,
    Program,
}

/// Strict final liveness carrier for master-orchestrate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationOutcomeV1 {
    pub schema_version: u8,
    pub mode: OrchestrationModeV1,
    pub next_slice_ready: bool,
    pub continuation_state: OrchestrationContinuationStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_job_id: Option<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker_class: Option<PipelineBlockerClassV1>,
    pub evidence: String,
}

/// Daemon-facing interpretation of one terminal assistant response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramContinuationIntentV1 {
    NotProgram,
    TerminalAllowed,
    RequireChildWatch {
        job_id: uuid::Uuid,
    },
    RequireResumeWake {
        job_id: uuid::Uuid,
    },
    /// Compatibility path for the prior `ORCHESTRATION COMPLETE` key/value
    /// report. Either durable mechanism satisfies it.
    RequireAnyGuard,
    InvalidProgram(String),
}

/// Dedicated VERIFY-stage payload embedded in the generic pipeline handoff.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VerifyHandoff {
    pub manifest_path: String,
    pub daemon_checks_passed: u32,
    pub total: u32,
    pub failed: Vec<String>,
    pub status: String,
    pub blocker: Option<String>,
}

/// Strict Closure terminal handoff. Unlike legacy pipeline handoffs, its stage
/// contract is mandatory and its one outcome field is parsed as strict JSON.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ClosurePipelineHandoffV1 {
    pub outcome: ClosureChildOutputEnvelopeV1,
    pub raw_outcome_json: String,
    pub raw_first_line: String,
}

/// Strict evidence-owner handoff emitted only by a Closure review session.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ClosureReviewHandoffV1 {
    pub reviewer_session_id: uuid::Uuid,
    pub reviewer_model_invocation_id: uuid::Uuid,
    pub review_json_path: ClosureEvidencePathV1,
    pub manifest_v2_path: ClosureEvidencePathV1,
    pub sealed_source_sha: ClosureGitShaV1,
    pub evidence_commit_sha: ClosureGitShaV1,
    pub raw_first_line: String,
}

/// One parsed `WORKER REPORT:` block from a `team_implement` worker.
///
/// `team_implement.md` declares the schema:
/// - `status`: complete | blocked | partial (required)
/// - `Phase`: integer phase number (required, but stored as string for
///   forward compatibility with mixed-case phase identifiers)
/// - `files_modified`: comma- or bullet-separated list (required, may be
///   empty for status=blocked)
/// - `blocker`: one-sentence blocker description (optional, present iff
///   status != complete)
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkerReport {
    pub status: String,
    pub phase: String,
    pub files_modified: Vec<String>,
    pub blocker: Option<String>,
    pub raw_first_line: String,
}

/// Pipeline stage extracted from the first-line capture.
///
/// Stored as an enum so consumers don't have to string-compare.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
#[non_exhaustive]
pub enum Stage {
    Research,
    Plan,
    Implementation,
    Review,
    Fix,
    Verify,
    Smoke,
    Documentation,
}

impl Stage {
    fn from_marker(s: &str) -> Option<Self> {
        match s {
            "RESEARCH" => Some(Self::Research),
            "PLAN" => Some(Self::Plan),
            "IMPLEMENTATION" => Some(Self::Implementation),
            "REVIEW" => Some(Self::Review),
            "FIX" => Some(Self::Fix),
            "VERIFY" => Some(Self::Verify),
            "SMOKE" => Some(Self::Smoke),
            "DOCUMENTATION" | "DOCUMENT" => Some(Self::Documentation),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Research => "RESEARCH",
            Self::Plan => "PLAN",
            Self::Implementation => "IMPLEMENTATION",
            Self::Review => "REVIEW",
            Self::Fix => "FIX",
            Self::Verify => "VERIFY",
            Self::Smoke => "SMOKE",
            Self::Documentation => "DOCUMENTATION",
        }
    }
}

/// All ways the contract can fail.
///
/// Accepted input keys for a required field, for error messages.
///
/// The parser resolves several human-written aliases onto one internal field
/// name, so an error naming only the internal name (`doc_path`) tells an author
/// nothing about what to write. Workers have been instructed to emit
/// `artifact:` — a reasonable guess this parser does not accept — and the
/// resulting failures read as worker error when the handoffs were correct.
fn accepted_keys_for(field: &str) -> &'static [&'static str] {
    match field {
        "doc_path" => &[
            "research document",
            "plan document",
            "implementation document",
            "review document",
            "smoke document",
            "documentation document",
            "impl_doc",
            "doc_path",
        ],
        "manifest_path" => &["manifest_path", "manifest path", "manifest"],
        _ => &[],
    }
}

fn missing_field_message(field: &str) -> String {
    let accepted = accepted_keys_for(field);
    if accepted.is_empty() {
        return format!("missing required field `{field}`");
    }
    let list = accepted
        .iter()
        .map(|key| format!("`{key}:`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("missing required field `{field}` (write any one of: {list})")
}

/// Serialized to JSON when the binary exits non-zero so the master can
/// route the corrective `SendMessage` based on the `kind`.
#[derive(Debug, Clone, Error, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContractError {
    #[error("missing PIPELINE HANDOFF marker in first 200 bytes of reply")]
    MissingMarker,
    #[error("malformed first line: expected `PIPELINE HANDOFF — <STAGE>:`, got `{got}`")]
    MalformedFirstLine { got: String },
    #[error(
        "unknown stage `{stage}` (expected RESEARCH|PLAN|IMPLEMENTATION|REVIEW|FIX|VERIFY|SMOKE|DOCUMENTATION)"
    )]
    UnknownStage { stage: String },
    #[error("{}", missing_field_message(field))]
    MissingField { field: String },
    #[error("ticket mismatch: expected `{expected}`, got `{got}`")]
    TicketMismatch { expected: String, got: String },
    /// Cross-stage drift (S6/D1): the VERIFY handoff declared plan/research
    /// linkage keys that no verification-manifest item `satisfies`/`covers`.
    /// This is the "research→plan→impl drift is a hard gate" failure — an
    /// implementation that silently dropped a planned finding lands here.
    #[error("uncovered plan/research linkage key(s) not satisfied by any manifest item: {keys:?}")]
    UncoveredLinkage { keys: Vec<String> },
    /// The cross-stage pass was invoked on a non-VERIFY handoff. The coverage
    /// gate is defined only for the VERIFY stage.
    #[error("cross-stage linkage check requires a VERIFY handoff, got `{stage}`")]
    NotVerifyStage { stage: String },
    #[error("Closure handoff marker must be the first nonblank line, got `{got}`")]
    ClosureMarkerNotFirst { got: String },
    #[error("duplicate Closure handoff field `{field}`")]
    DuplicateField { field: String },
    #[error("invalid Closure handoff field `{field}`: {message}")]
    InvalidField { field: String, message: String },
    #[error("Closure handoff requires a valid canonical Stage contract block")]
    ClosureStageContractRequired,
    #[error("Closure handoff may not contain Markdown code fences")]
    ClosureCodeFenceForbidden,
}

/// Anchored regex for the master `PIPELINE HANDOFF —` marker.
///
/// The em-dash (U+2014) is required — RSI-021 picked this delimiter
/// deliberately to avoid collisions with `--` in code blocks. The regex is
/// multiline-anchored so the marker can begin on any line in the first
/// 200-byte window. The trailing `:\s*$` permits an optional CR/LF before
/// the line ends but rejects extra prose on the same line.
static PIPELINE_FIRST_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^PIPELINE HANDOFF — ([A-Z]+):\s*$")
        .unwrap_or_else(|err| panic!("valid regex: {err}"))
});

static INVALID_PROGRAM_MODE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"mode"\s*:\s*"program""#).unwrap_or_else(|err| panic!("valid regex: {err}"))
});

/// Anchored regex for the worker `WORKER REPORT:` marker.
///
/// The worker grammar uses a colon directly after the literal — there is
/// no stage in the marker (the phase number lives in the body).
static WORKER_REPORT_FIRST_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^WORKER REPORT:\s*$").unwrap_or_else(|err| panic!("valid regex: {err}"))
});

/// First-200-byte window we scan for the marker. Anything past this is
/// treated as missing — the marker MUST be the leading content of the
/// reply, modulo any ASCII whitespace/empty lines.
const MARKER_SCAN_WINDOW: usize = 200;

/// Validate that the master-grammar `PIPELINE HANDOFF — <STAGE>:` marker
/// appears in the first 200 bytes of `reply`. Returns the captured stage
/// string (e.g. `"RESEARCH"`) on success.
fn extract_pipeline_marker(reply: &str) -> Result<String, ContractError> {
    let head = head_window(reply);
    if let Some(caps) = PIPELINE_FIRST_LINE_RE.captures(head)
        && let Some(stage) = caps.get(1)
    {
        return Ok(stage.as_str().to_string());
    }
    let first_line = head.lines().next().unwrap_or("").to_string();
    if first_line.is_empty() {
        Err(ContractError::MissingMarker)
    } else {
        Err(ContractError::MalformedFirstLine { got: first_line })
    }
}

/// Public alias for the master-grammar first-line check.
///
/// Discards the captured stage. Useful for the binary's `--first-line-only`
/// path where the master only cares whether the marker is well-formed.
///
/// # Errors
///
/// Returns [`ContractError`] when the marker is missing or malformed.
pub fn validate_first_line(reply: &str) -> Result<(), ContractError> {
    extract_pipeline_marker(reply).map(|_| ())
}

/// Sibling first-line check for the `team_implement` worker grammar.
///
/// Kept as a separate function rather than a unified `validate_marker(kind)`
/// so the master can call the right one explicitly at each parse site —
/// the two grammars are structurally similar but distinct, and conflating
/// them in a single dispatch would mask drift between the two.
///
/// # Errors
///
/// Returns [`ContractError`] when the marker is missing or malformed.
pub fn validate_worker_report_first_line(reply: &str) -> Result<(), ContractError> {
    let head = head_window(reply);
    if WORKER_REPORT_FIRST_LINE_RE.is_match(head) {
        return Ok(());
    }
    let first_line = head.lines().next().unwrap_or("").to_string();
    if first_line.is_empty() {
        Err(ContractError::MissingMarker)
    } else {
        Err(ContractError::MalformedFirstLine { got: first_line })
    }
}

/// Return the leading slice of `reply` capped at `MARKER_SCAN_WINDOW` bytes,
/// careful to land on a UTF-8 character boundary.
fn head_window(reply: &str) -> &str {
    if reply.len() <= MARKER_SCAN_WINDOW {
        return reply;
    }
    // Walk back from MARKER_SCAN_WINDOW until we find a char boundary.
    let mut end = MARKER_SCAN_WINDOW;
    while end > 0 && !reply.is_char_boundary(end) {
        end -= 1;
    }
    &reply[..end]
}

/// Parse a `PIPELINE HANDOFF — <STAGE>:` block from a worker reply.
///
/// On success, returns a fully populated `PipelineHandoff`. The `ticket`
/// argument is currently informational (the v1 master grammar has no
/// `ticket:` line — we accept the parameter so the master can pass it
/// through for a future v2 that does, without breaking the binary's CLI).
/// If the reply contains a `ticket:` field and it disagrees with the
/// master's `ticket` parameter, `TicketMismatch` is returned.
///
/// # Errors
///
/// Returns [`ContractError`] when the marker, required fields, ticket, or
/// stage-specific schema are invalid.
pub fn parse_pipeline_handoff(reply: &str, ticket: &str) -> Result<PipelineHandoff, ContractError> {
    let stage_str = extract_pipeline_marker(reply)?;
    let stage = Stage::from_marker(&stage_str).ok_or_else(|| ContractError::UnknownStage {
        stage: stage_str.clone(),
    })?;

    let raw_first_line = format!("PIPELINE HANDOFF — {stage_str}:");

    // Scan all key/value lines after the marker. Keys are case-preserving
    // but matched case-insensitively so the spawn template's "Research
    // document:" lines and a future "doc_path:" both resolve.
    let kv = collect_key_values(reply);

    let manifest_path = pick_first(&kv, &["manifest_path", "manifest path", "manifest"]);

    let doc_path = if stage == Stage::Verify {
        manifest_path
            .clone()
            .ok_or_else(|| ContractError::MissingField {
                field: "manifest_path".to_string(),
            })?
    } else {
        pick_first(
            &kv,
            &[
                "doc_path",
                "research document",
                "plan document",
                "implementation document",
                "impl_doc",
                "review document",
                "smoke document",
                "documentation document",
            ],
        )
        .ok_or_else(|| ContractError::MissingField {
            field: "doc_path".to_string(),
        })?
    };

    if stage == Stage::Implementation && manifest_path.is_none() {
        return Err(ContractError::MissingField {
            field: "manifest_path".to_string(),
        });
    }

    // Status is optional in the v1 spawn template (research/plan/implement
    // emit free-form blocks), but the `<handoff_contract>` declares it
    // required. We accept missing status for backwards compat with the
    // research/plan templates that don't emit it; mark missing as
    // "complete" — the absence of a blocker is the proxy.
    let status = pick_first(&kv, &["status"]).unwrap_or_else(|| "complete".to_string());
    let blocker = pick_first(&kv, &["blocker"]);

    // Cross-stage linkage (S6/D1): the plan/research keys this handoff
    // declares must be satisfied. Only VERIFY handoffs carry these today;
    // absent everywhere else, so parsing them is a strict superset.
    let linkage = parse_linkage_keys(pick_first(
        &kv,
        &["satisfies", "covers", "linkage", "requires"],
    ));

    let (daemon_checks_passed, total, failed) = if stage == Stage::Verify {
        let raw = pick_first(&kv, &["daemon_checks", "daemon checks"]).ok_or_else(|| {
            ContractError::MissingField {
                field: "daemon_checks".to_string(),
            }
        })?;
        let (passed, total) =
            parse_daemon_checks(&raw).ok_or_else(|| ContractError::MissingField {
                field: "daemon_checks".to_string(),
            })?;
        let failed = parse_failed_checks(pick_first(&kv, &["failed_checks", "failed checks"]));
        if status != "complete" && failed.is_empty() {
            return Err(ContractError::MissingField {
                field: "failed_checks".to_string(),
            });
        }
        (Some(passed), Some(total), failed)
    } else {
        (None, None, Vec::new())
    };

    if let Some(emitted) = pick_first(&kv, &["ticket"])
        && !ticket.is_empty()
        && emitted != ticket
    {
        return Err(ContractError::TicketMismatch {
            expected: ticket.to_string(),
            got: emitted,
        });
    }

    Ok(PipelineHandoff {
        stage,
        doc_path,
        manifest_path,
        status,
        daemon_checks_passed,
        total,
        failed,
        blocker,
        linkage,
        raw_first_line,
    })
}

/// Parse and enforce the strict master-orchestrate V2 handoff contract.
///
/// This is opt-in so historical `master_implement`/`team_implement` replies
/// retain their existing parser behavior. V2 requires an explicit closed
/// status. Partial requires concrete remaining-work evidence but no blocker
/// class. Blocked/human-gate additionally require a typed blocker class;
/// budget/review/reclassification cannot be named as a class because they are
/// continuation conditions, not authority gates.
///
/// # Errors
///
/// Returns [`ContractError`] for every legacy parse failure, missing strict
/// field, invented status/class, or inconsistent blocker group.
pub fn parse_pipeline_handoff_v2(
    reply: &str,
    ticket: &str,
) -> Result<StrictPipelineHandoffV2, ContractError> {
    extract_pipeline_marker(reply)?;
    let (normalized_reply, warnings) = canonicalize_v2_handoff_aliases(reply)?;
    let handoff = parse_pipeline_handoff(&normalized_reply, ticket)?;
    let kv = collect_key_values(&normalized_reply);
    let status_text = strict_optional_value(&kv, &["status"], "status")?.ok_or_else(|| {
        ContractError::MissingField {
            field: "status".into(),
        }
    })?;
    let strict_status =
        PipelineStatusV2::parse(&status_text).ok_or_else(|| ContractError::InvalidField {
            field: "status".into(),
            message: format!("expected complete|partial|blocked|human_gate, got {status_text}"),
        })?;
    let blocker_class_text = meaningful_optional(strict_optional_value(
        &kv,
        &["blocker_class", "blocker class"],
        "blocker_class",
    )?);
    let blocker_class = blocker_class_text
        .as_deref()
        .map(|value| {
            PipelineBlockerClassV1::parse(value).ok_or_else(|| ContractError::InvalidField {
                field: "blocker_class".into(),
                message: format!(
                    "expected authority|forbidden_scope|destructive|production|resource|technical_impasse, got {value}"
                ),
            })
        })
        .transpose()?;
    let blocker = meaningful_optional(strict_optional_value(&kv, &["blocker"], "blocker")?);
    let blocker_evidence = meaningful_optional(strict_optional_value(
        &kv,
        &["blocker_evidence", "blocker evidence"],
        "blocker_evidence",
    )?);

    match strict_status {
        PipelineStatusV2::Complete => {
            if blocker.is_some() || blocker_class.is_some() || blocker_evidence.is_some() {
                return Err(ContractError::InvalidField {
                    field: "status".into(),
                    message: "complete handoff must not carry blocker fields".into(),
                });
            }
        }
        PipelineStatusV2::Partial => {
            if blocker.is_none() {
                return Err(ContractError::MissingField {
                    field: "blocker".into(),
                });
            }
            if blocker_class.is_some() {
                return Err(ContractError::InvalidField {
                    field: "blocker_class".into(),
                    message: "partial is a continuation state and must not claim a blocker class"
                        .into(),
                });
            }
            if blocker_evidence.is_none() {
                return Err(ContractError::MissingField {
                    field: "blocker_evidence".into(),
                });
            }
        }
        PipelineStatusV2::Blocked | PipelineStatusV2::HumanGate => {
            if blocker.is_none() {
                return Err(ContractError::MissingField {
                    field: "blocker".into(),
                });
            }
            if blocker_class.is_none() {
                return Err(ContractError::MissingField {
                    field: "blocker_class".into(),
                });
            }
            if blocker_evidence.is_none() {
                return Err(ContractError::MissingField {
                    field: "blocker_evidence".into(),
                });
            }
        }
    }

    Ok(StrictPipelineHandoffV2 {
        handoff,
        strict_status,
        blocker_class,
        blocker_evidence,
        warnings,
    })
}

fn canonicalize_v2_handoff_aliases(
    reply: &str,
) -> Result<(String, Vec<PipelineHandoffWarningV2>), ContractError> {
    let kv = collect_key_values(reply);
    let mut normalized = reply.to_string();
    let mut warnings = Vec::new();

    for (canonical_key, aliases) in [
        ("doc_path", &["plan_path", "review_path"][..]),
        ("status", &["worker_status"][..]),
    ] {
        let fields = kv
            .iter()
            .filter(|(key, _)| key == canonical_key || aliases.contains(&key.as_str()))
            .collect::<Vec<_>>();
        if fields.len() > 1 {
            return Err(ContractError::DuplicateField {
                field: canonical_key.into(),
            });
        }
        let Some((alias, value)) = fields.first() else {
            continue;
        };
        if alias.as_str() == canonical_key {
            continue;
        }

        if !normalized.ends_with('\n') {
            normalized.push('\n');
        }
        normalized.push_str(canonical_key);
        normalized.push_str(": ");
        normalized.push_str(value);
        normalized.push('\n');
        warnings.push(PipelineHandoffWarningV2 {
            alias: alias.clone(),
            canonical_key: canonical_key.into(),
        });
    }

    Ok((normalized, warnings))
}

/// Parse the one strict `orchestration_outcome_v1: <json>` carrier.
///
/// # Errors
///
/// Returns [`ContractError`] if the field is missing/duplicated, JSON is not
/// the closed V1 shape, or its program liveness relationships are invalid.
pub fn parse_orchestration_outcome_v1(
    reply: &str,
) -> Result<OrchestrationOutcomeV1, ContractError> {
    let raw = one_required_value(
        exact_field_values(reply, "orchestration_outcome_v1"),
        "orchestration_outcome_v1",
    )?;
    let outcome: OrchestrationOutcomeV1 =
        serde_json::from_str(&raw).map_err(|error| ContractError::InvalidField {
            field: "orchestration_outcome_v1".into(),
            message: error.to_string(),
        })?;
    validate_orchestration_outcome_v1(&outcome)?;
    Ok(outcome)
}

fn validate_orchestration_outcome_v1(
    outcome: &OrchestrationOutcomeV1,
) -> Result<(), ContractError> {
    if outcome.schema_version != 1 {
        return Err(ContractError::InvalidField {
            field: "orchestration_outcome_v1.schema_version".into(),
            message: format!("expected 1, got {}", outcome.schema_version),
        });
    }
    let evidence_len = outcome.evidence.trim().len();
    if !(1..=2_048).contains(&evidence_len) {
        return Err(ContractError::InvalidField {
            field: "orchestration_outcome_v1.evidence".into(),
            message: "must contain 1..=2048 non-whitespace bytes".into(),
        });
    }
    if outcome.continuation_state == OrchestrationContinuationStateV1::HumanGate {
        if outcome.blocker_class.is_none() {
            return Err(ContractError::MissingField {
                field: "orchestration_outcome_v1.blocker_class".into(),
            });
        }
    } else if outcome.blocker_class.is_some() {
        return Err(ContractError::InvalidField {
            field: "orchestration_outcome_v1.blocker_class".into(),
            message: "is permitted only with continuation_state=human_gate".into(),
        });
    }

    if matches!(
        outcome.continuation_state,
        OrchestrationContinuationStateV1::ChildWatch | OrchestrationContinuationStateV1::ResumeWake
    ) {
        if outcome.continuation_job_id.is_none() {
            return Err(ContractError::MissingField {
                field: "orchestration_outcome_v1.continuation_job_id".into(),
            });
        }
    } else if outcome.continuation_job_id.is_some() {
        return Err(ContractError::InvalidField {
            field: "orchestration_outcome_v1.continuation_job_id".into(),
            message: "is permitted only with child_watch or resume_wake".into(),
        });
    }

    if outcome.mode == OrchestrationModeV1::Program {
        if outcome.next_slice_ready
            && !matches!(
                outcome.continuation_state,
                OrchestrationContinuationStateV1::ChildWatch
                    | OrchestrationContinuationStateV1::ResumeWake
            )
        {
            return Err(ContractError::InvalidField {
                field: "orchestration_outcome_v1.continuation_state".into(),
                message:
                    "open program queue requires child_watch or resume_wake before the turn ends"
                        .into(),
            });
        }
        if !outcome.next_slice_ready
            && outcome.continuation_state == OrchestrationContinuationStateV1::QueueExhausted
        {
            return Ok(());
        }
        if !outcome.next_slice_ready
            && outcome.continuation_state == OrchestrationContinuationStateV1::HumanGate
        {
            return Ok(());
        }
    }
    Ok(())
}

/// Interpret strict V1 output for the daemon, with one bounded compatibility
/// fallback for the prior key/value final report.
#[must_use]
pub fn program_continuation_intent_v1(reply: &str) -> ProgramContinuationIntentV1 {
    program_continuation_intent_v1_with_registration(reply, false)
}

/// Interpret a terminal response with daemon-authoritative program identity.
///
/// A registered program must emit exactly one valid strict program carrier.
/// Its possibly-failing prose can no longer opt out of the terminal interlock
/// by omitting the carrier, duplicating it, malforming it, or claiming slice
/// mode. Unregistered callers retain the bounded legacy `Mode: program`
/// fallback, including when a malformed strict carrier is also present. Only
/// for unregistered callers, fenced examples are excluded from interpretation
/// of both legacy fields and strict carriers.
#[must_use]
pub fn program_continuation_intent_v1_with_registration(
    reply: &str,
    program_registered: bool,
) -> ProgramContinuationIntentV1 {
    let unfenced = (!program_registered).then(|| without_fenced_program_examples(reply));
    let reply = unfenced.as_deref().unwrap_or(reply);
    let kv = collect_key_values(reply);
    let legacy_program =
        pick_first(&kv, &["mode"]).is_some_and(|mode| mode.eq_ignore_ascii_case("program"));
    let values = exact_field_values(reply, "orchestration_outcome_v1");
    if !values.is_empty() {
        return match parse_orchestration_outcome_v1(reply) {
            Ok(outcome) if outcome.mode != OrchestrationModeV1::Program && program_registered => {
                ProgramContinuationIntentV1::InvalidProgram(
                    "registered program emitted a non-program outcome".into(),
                )
            }
            Ok(outcome) if outcome.mode != OrchestrationModeV1::Program => {
                ProgramContinuationIntentV1::NotProgram
            }
            Ok(outcome) => match (outcome.continuation_state, outcome.continuation_job_id) {
                (OrchestrationContinuationStateV1::ChildWatch, Some(job_id)) => {
                    ProgramContinuationIntentV1::RequireChildWatch { job_id }
                }
                (OrchestrationContinuationStateV1::ResumeWake, Some(job_id)) => {
                    ProgramContinuationIntentV1::RequireResumeWake { job_id }
                }
                (
                    OrchestrationContinuationStateV1::QueueExhausted
                    | OrchestrationContinuationStateV1::HumanGate,
                    None,
                ) => ProgramContinuationIntentV1::TerminalAllowed,
                _ => ProgramContinuationIntentV1::InvalidProgram(
                    "validated outcome carried an inconsistent continuation job id".into(),
                ),
            },
            Err(error) => {
                let declares_program = values.iter().any(|value| {
                    serde_json::from_str::<serde_json::Value>(value)
                        .ok()
                        .and_then(|json| json.get("mode").cloned())
                        .and_then(|mode| mode.as_str().map(str::to_owned))
                        .is_some_and(|mode| mode == "program")
                        || INVALID_PROGRAM_MODE_RE.is_match(value)
                });
                if program_registered || declares_program || legacy_program {
                    ProgramContinuationIntentV1::InvalidProgram(error.to_string())
                } else {
                    ProgramContinuationIntentV1::NotProgram
                }
            }
        };
    }

    if program_registered {
        return ProgramContinuationIntentV1::InvalidProgram(
            "registered program omitted orchestration_outcome_v1".into(),
        );
    }
    if !legacy_program {
        return ProgramContinuationIntentV1::NotProgram;
    }
    match pick_first(&kv, &["next-slice-ready", "next slice ready"])
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("yes" | "true") => ProgramContinuationIntentV1::RequireAnyGuard,
        _ => ProgramContinuationIntentV1::TerminalAllowed,
    }
}

/// Keep report text outside backtick/tilde fences in one linear pass. Fence
/// delimiters allow up to three leading spaces; closing delimiters must use
/// the opener's marker, at least its length, and only trailing whitespace.
/// An unclosed example consumes the rest of the reply. This context filter is
/// deliberately separate from generic handoff and registered-program parsing.
fn without_fenced_program_examples(reply: &str) -> String {
    let mut report = String::with_capacity(reply.len());
    let mut fence = None;
    for line in reply.split_inclusive('\n') {
        let content = line.trim_start_matches(' ');
        if line.len() - content.len() <= 3
            && let Some(marker @ (b'`' | b'~')) = content.as_bytes().first().copied()
        {
            let length = content.bytes().take_while(|byte| *byte == marker).count();
            let rest = &content[length..];
            match fence {
                Some((opening_marker, opening_length))
                    if marker == opening_marker
                        && length >= opening_length
                        && rest.trim_matches([' ', '\t', '\r', '\n']).is_empty() =>
                {
                    fence = None;
                    continue;
                }
                None if length >= 3 && (marker == b'~' || !rest.contains('`')) => {
                    fence = Some((marker, length));
                    continue;
                }
                _ => {}
            }
        }
        if fence.is_none() {
            report.push_str(line);
        }
    }
    report
}

/// Parse the one strict Closure terminal output carrier.
pub fn parse_closure_pipeline_handoff_v1(
    reply: &str,
) -> Result<ClosurePipelineHandoffV1, ContractError> {
    require_first_nonblank_marker(reply, "PIPELINE HANDOFF — CLOSURE:")?;
    require_closure_contract_block(reply)?;
    if reply
        .lines()
        .any(|line| line.trim_start().starts_with("```"))
    {
        return Err(ContractError::ClosureCodeFenceForbidden);
    }

    let values = exact_field_values(reply, "closure_outcome_v1");
    let raw_outcome_json = one_required_value(values, "closure_outcome_v1")?;
    if raw_outcome_json.is_empty() {
        return Err(ContractError::MissingField {
            field: "closure_outcome_v1".into(),
        });
    }
    let outcome = parse_closure_child_output_envelope_v1(&raw_outcome_json).map_err(|error| {
        ContractError::InvalidField {
            field: "closure_outcome_v1".into(),
            message: error.to_string(),
        }
    })?;
    Ok(ClosurePipelineHandoffV1 {
        outcome,
        raw_outcome_json,
        raw_first_line: "PIPELINE HANDOFF — CLOSURE:".into(),
    })
}

/// Parse the strict Closure-specific review evidence handoff. Ordinary review
/// Markdown remains outside this opt-in parser and is therefore unchanged.
pub fn parse_closure_review_handoff_v1(
    reply: &str,
) -> Result<ClosureReviewHandoffV1, ContractError> {
    require_first_nonblank_marker(reply, "PIPELINE HANDOFF — REVIEW:")?;
    require_closure_contract_block(reply)?;
    if reply
        .lines()
        .any(|line| line.trim_start().starts_with("```"))
    {
        return Err(ContractError::ClosureCodeFenceForbidden);
    }

    let reviewer_session_id = parse_required_field(reply, "reviewer_session_id")?;
    let reviewer_model_invocation_id = parse_required_field(reply, "reviewer_model_invocation_id")?;
    let review_json_path = parse_required_field(reply, "review_json_path")?;
    let manifest_v2_path = parse_required_field(reply, "manifest_v2_path")?;
    let sealed_source_sha = parse_required_field(reply, "sealed_source_sha")?;
    let evidence_commit_sha = parse_required_field(reply, "evidence_commit_sha")?;

    Ok(ClosureReviewHandoffV1 {
        reviewer_session_id,
        reviewer_model_invocation_id,
        review_json_path,
        manifest_v2_path,
        sealed_source_sha,
        evidence_commit_sha,
        raw_first_line: "PIPELINE HANDOFF — REVIEW:".into(),
    })
}

fn parse_required_field<T>(reply: &str, field: &str) -> Result<T, ContractError>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    let value = one_required_value(exact_field_values(reply, field), field)?;
    value
        .parse::<T>()
        .map_err(|error| ContractError::InvalidField {
            field: field.into(),
            message: error.to_string(),
        })
}

fn exact_field_values(reply: &str, field: &str) -> Vec<String> {
    reply
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (key, value) = trimmed.split_once(':')?;
            (key == field).then(|| value.trim().to_string())
        })
        .collect()
}

fn one_required_value(values: Vec<String>, field: &str) -> Result<String, ContractError> {
    match values.as_slice() {
        [] => Err(ContractError::MissingField {
            field: field.into(),
        }),
        [value] => Ok(value.clone()),
        _ => Err(ContractError::DuplicateField {
            field: field.into(),
        }),
    }
}

fn require_first_nonblank_marker(reply: &str, expected: &str) -> Result<(), ContractError> {
    let got = reply
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or_default();
    if got == expected {
        Ok(())
    } else {
        Err(ContractError::ClosureMarkerNotFirst { got: got.into() })
    }
}

fn require_closure_contract_block(reply: &str) -> Result<(), ContractError> {
    if scan_contract_block(reply) == ContractBlock::Valid {
        Ok(())
    } else {
        Err(ContractError::ClosureStageContractRequired)
    }
}

/// Split a comma-separated linkage value into trimmed, non-empty keys.
///
/// Mirrors the manifest-side parser: tolerates an optional `[...]` wrapper
/// and an `Option` input (a missing line yields no keys) so
/// `satisfies: [F-1, F-2]`, `satisfies: F-1, F-2`, and an absent line all
/// behave. Reused by the VERIFY-handoff linkage capture above.
fn parse_linkage_keys(raw: Option<String>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(',')
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty() && !k.eq_ignore_ascii_case("none"))
        .collect()
}

/// Cross-stage VERIFY coverage gate (S6/D1) — the "drift is a hard gate" pass.
///
/// Given a parsed VERIFY-stage [`PipelineHandoff`] and the ticket's
/// [`VerificationManifest`], assert that every plan/research linkage key the
/// handoff *declares* (via its `satisfies:` / `covers:` / `linkage:` /
/// `requires:` line, captured into [`PipelineHandoff::linkage`]) is covered
/// by some manifest item's `satisfies`/`covers`.
///
/// This is the machine gate that turns silent research→plan→impl drift into a
/// hard failure: if the plan declared that finding `F-007` must be satisfied
/// and the implementation's manifest never links a verification item back to
/// `F-007`, this returns [`ContractError::UncoveredLinkage`].
///
/// A handoff that declares no linkage keys passes vacuously (backward compat:
/// pre-S6 VERIFY handoffs carry none). A fully-covered handoff returns
/// `Ok(())`.
///
/// This is a **standalone** pass — it does not touch the existing first-line
/// or single-stage field validators. Call it after `parse_pipeline_handoff`
/// has produced a `Stage::Verify` handoff.
///
/// # Errors
///
/// - [`ContractError::NotVerifyStage`] if `handoff.stage != Stage::Verify`.
/// - [`ContractError::UncoveredLinkage`] listing every declared key that no
///   manifest item satisfies or covers.
pub fn cross_stage_verify_coverage(
    handoff: &PipelineHandoff,
    manifest: &crate::verification_manifest::VerificationManifest,
) -> Result<(), ContractError> {
    if handoff.stage != Stage::Verify {
        return Err(ContractError::NotVerifyStage {
            stage: handoff.stage.as_str().to_string(),
        });
    }

    // Match linkage keys case-insensitively so `F-001` == `f-001`. Both the
    // handoff-declared keys and the manifest-covered keys are hand-typed in
    // markdown; normalizing on both sides mirrors how `parse_linkage_keys`
    // already filters "none" case-insensitively. The error payload keeps the
    // handoff's original casing (what the author wrote), not the folded form.
    let covered: std::collections::BTreeSet<String> = manifest
        .covered_linkage_keys()
        .into_iter()
        .map(|key| key.to_ascii_lowercase())
        .collect();
    let mut uncovered: Vec<String> = handoff
        .linkage
        .iter()
        .filter(|key| !covered.contains(&key.to_ascii_lowercase()))
        .cloned()
        .collect();

    if uncovered.is_empty() {
        Ok(())
    } else {
        // Deterministic, de-duplicated ordering for a stable error payload.
        uncovered.sort();
        uncovered.dedup();
        Err(ContractError::UncoveredLinkage { keys: uncovered })
    }
}

fn parse_daemon_checks(raw: &str) -> Option<(u32, u32)> {
    let raw = raw.trim();
    let (passed, total) = raw.split_once('/')?;
    Some((passed.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn parse_failed_checks(raw: Option<String>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let trimmed = raw
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    trimmed
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Parse a `WORKER REPORT:` block from a `team_implement` worker reply.
///
/// # Errors
///
/// Returns [`ContractError`] when the report marker or required fields are
/// missing.
pub fn parse_worker_report(reply: &str) -> Result<WorkerReport, ContractError> {
    validate_worker_report_first_line(reply)?;

    let kv = collect_key_values(reply);

    let status = pick_first(&kv, &["status"]).ok_or_else(|| ContractError::MissingField {
        field: "status".to_string(),
    })?;
    let phase = pick_first(&kv, &["phase"]).ok_or_else(|| ContractError::MissingField {
        field: "phase".to_string(),
    })?;
    let files_modified_raw = pick_first(&kv, &["files_modified"]).unwrap_or_default();
    let files_modified = files_modified_raw
        .split([',', '\n'])
        .map(|s| s.trim().trim_start_matches('-').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let blocker = pick_first(&kv, &["blocker"]);

    Ok(WorkerReport {
        status,
        phase,
        files_modified,
        blocker,
        raw_first_line: "WORKER REPORT:".to_string(),
    })
}

/// Collect every `key: value` line in `reply` into a Vec preserving order.
///
/// Lower-cases keys for case-insensitive lookup. Skips lines without a
/// colon and lines that begin with `#` or `=` (the marker underline).
fn collect_key_values(reply: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in reply.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('=') || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            out.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    out
}

/// Return the value of the first key in `keys` that appears in `kv`.
fn pick_first(kv: &[(String, String)], keys: &[&str]) -> Option<String> {
    for k in keys {
        let needle = k.to_lowercase();
        if let Some((_, v)) = kv.iter().find(|(key, _)| key == &needle) {
            return Some(v.clone());
        }
    }
    None
}

fn strict_optional_value(
    kv: &[(String, String)],
    keys: &[&str],
    field: &str,
) -> Result<Option<String>, ContractError> {
    let values = kv
        .iter()
        .filter(|(key, _)| keys.iter().any(|candidate| key == candidate))
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>();
    match values.as_slice() {
        [] => Ok(None),
        [value] => Ok(Some(value.clone())),
        _ => Err(ContractError::DuplicateField {
            field: field.into(),
        }),
    }
}

fn meaningful_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::uninlined_format_args
)]
mod tests {
    use super::*;

    fn closure_contract_suffix() -> &'static str {
        "\n## Stage contract\n### Inputs\nStatic input: `sealed-correlation`\n### Process\nimplemented K1\n### Outputs\ncommitted result\n### Verify\ntests passed\n"
    }

    #[test]
    fn closure_handoff_and_envelope_are_compatible() {
        let reply = format!(
            "PIPELINE HANDOFF — CLOSURE:\nclosure_outcome_v1: {{\"schema_version\":1,\"correlation\":{{\"program_id\":\"{}\",\"source_id\":\"{}\",\"custody_id\":\"{}\",\"custody_generation\":1,\"lineage_root_session_id\":\"{}\",\"tip_session_id\":\"{}\",\"rotation_depth\":0,\"model_invocation_id\":\"{}\",\"source_base_sha\":\"{}\"}},\"summary\":\"done\",\"outcome\":{{\"kind\":\"committed\",\"reported_source_head\":\"{}\"}}}}{}",
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            "a".repeat(40),
            "b".repeat(40),
            closure_contract_suffix(),
        );
        let parsed = parse_closure_pipeline_handoff_v1(&reply).expect("strict Closure handoff");
        assert_eq!(parsed.outcome.schema_version, 1);
    }

    #[test]
    fn closure_handoff_requires_one_field_and_canonical_contract() {
        let duplicate = format!(
            "PIPELINE HANDOFF — CLOSURE:\nclosure_outcome_v1: {{}}\nclosure_outcome_v1: {{}}{}",
            closure_contract_suffix()
        );
        assert!(matches!(
            parse_closure_pipeline_handoff_v1(&duplicate),
            Err(ContractError::DuplicateField { .. })
        ));
        assert!(matches!(
            parse_closure_pipeline_handoff_v1(
                "PIPELINE HANDOFF — CLOSURE:\nclosure_outcome_v1: {}\n"
            ),
            Err(ContractError::ClosureStageContractRequired)
        ));
    }

    #[test]
    fn closure_review_handoff_is_strict() {
        let reply = format!(
            "PIPELINE HANDOFF — REVIEW:\nreviewer_session_id: {}\nreviewer_model_invocation_id: {}\nreview_json_path: thoughts/shared/reviews/closure/p/s-review-v1.json\nmanifest_v2_path: thoughts/shared/verification/closure/p/s-manifest-v2.md\nsealed_source_sha: {}\nevidence_commit_sha: {}{}",
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            "a".repeat(40),
            "b".repeat(40),
            closure_contract_suffix(),
        );
        let parsed = parse_closure_review_handoff_v1(&reply).expect("strict review handoff");
        assert_eq!(parsed.sealed_source_sha.as_str(), "a".repeat(40));
    }

    fn well_formed_research() -> &'static str {
        "PIPELINE HANDOFF — RESEARCH:\n\
         =============================\n\
         Research document: /tmp/foo.md\n\
         Research question: How does X work?\n\
         Key findings:\n\
           - finding 1 with file:line\n\
         Codebase areas: TUI, Daemon\n\
         Open questions: none\n"
    }

    #[test]
    fn validate_first_line_accepts_research() {
        assert!(validate_first_line(well_formed_research()).is_ok());
    }

    #[test]
    fn validate_first_line_accepts_plan() {
        let r = "PIPELINE HANDOFF — PLAN:\nDoc: /tmp/p.md\n";
        assert!(validate_first_line(r).is_ok());
    }

    #[test]
    fn validate_first_line_accepts_implementation() {
        let r = "PIPELINE HANDOFF — IMPLEMENTATION:\nimpl_doc: /tmp/i.md\n";
        assert!(validate_first_line(r).is_ok());
    }

    #[test]
    fn validate_first_line_rejects_missing_marker() {
        let r = "I'd like to share some findings...\n\nResearch document: /tmp/foo.md\n";
        let err = validate_first_line(r).unwrap_err();
        assert!(matches!(err, ContractError::MalformedFirstLine { .. }));
    }

    #[test]
    fn validate_first_line_rejects_marker_after_window() {
        let prefix = "x".repeat(MARKER_SCAN_WINDOW + 50);
        let r = format!("{prefix}\nPIPELINE HANDOFF — RESEARCH:\n");
        let err = validate_first_line(&r).unwrap_err();
        // The marker is past the 200-byte window; we should report missing
        // or malformed first-line — both are acceptable here. The point is
        // the validator must not silently accept it.
        assert!(matches!(
            err,
            ContractError::MissingMarker | ContractError::MalformedFirstLine { .. }
        ));
    }

    #[test]
    fn validate_first_line_rejects_lowercase_stage() {
        let r = "PIPELINE HANDOFF — research:\nResearch document: /tmp/x.md\n";
        let err = validate_first_line(r).unwrap_err();
        assert!(matches!(err, ContractError::MalformedFirstLine { .. }));
    }

    #[test]
    fn validate_first_line_rejects_hyphen_minus_instead_of_em_dash() {
        // Hyphen-minus is U+002D; em-dash is U+2014. The regex requires
        // em-dash so the worker can't accidentally match a `--` in code.
        let r = "PIPELINE HANDOFF -- RESEARCH:\nResearch document: /tmp/x.md\n";
        let err = validate_first_line(r).unwrap_err();
        assert!(matches!(err, ContractError::MalformedFirstLine { .. }));
    }

    #[test]
    fn validate_first_line_rejects_empty_reply() {
        let err = validate_first_line("").unwrap_err();
        assert!(matches!(err, ContractError::MissingMarker));
    }

    #[test]
    fn parse_pipeline_handoff_well_formed() {
        let h = parse_pipeline_handoff(well_formed_research(), "RSI-021").unwrap();
        assert_eq!(h.stage, Stage::Research);
        assert_eq!(h.doc_path, "/tmp/foo.md");
        assert_eq!(h.status, "complete");
        assert!(h.blocker.is_none());
    }

    #[test]
    fn parse_pipeline_handoff_with_status_and_blocker() {
        let r = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
             ===================================\n\
             impl_doc: /tmp/handoff.md\n\
             Manifest path: /tmp/verification.md\n\
             status: blocked\n\
             blocker: cannot resolve sandbox path on rotation\n";
        let h = parse_pipeline_handoff(r, "RSI-021").unwrap();
        assert_eq!(h.stage, Stage::Implementation);
        assert_eq!(h.doc_path, "/tmp/handoff.md");
        assert_eq!(h.manifest_path.as_deref(), Some("/tmp/verification.md"));
        assert_eq!(h.status, "blocked");
        assert_eq!(
            h.blocker.as_deref(),
            Some("cannot resolve sandbox path on rotation")
        );
    }

    #[test]
    fn parse_pipeline_handoff_missing_doc_path() {
        let r = "PIPELINE HANDOFF — RESEARCH:\n\nfoo: bar\n";
        let err = parse_pipeline_handoff(r, "RSI-021").unwrap_err();
        match err {
            ContractError::MissingField { field } => assert_eq!(field, "doc_path"),
            other => panic!("expected MissingField{{doc_path}}, got {:?}", other),
        }
    }

    #[test]
    fn parse_pipeline_handoff_ticket_mismatch() {
        let r = "PIPELINE HANDOFF — RESEARCH:\n\
             Research document: /tmp/x.md\n\
             ticket: RSI-099\n";
        let err = parse_pipeline_handoff(r, "RSI-021").unwrap_err();
        match err {
            ContractError::TicketMismatch { expected, got } => {
                assert_eq!(expected, "RSI-021");
                assert_eq!(got, "RSI-099");
            }
            other => panic!("expected TicketMismatch, got {:?}", other),
        }
    }

    #[test]
    fn parse_pipeline_handoff_ignores_ticket_match() {
        let r = "PIPELINE HANDOFF — RESEARCH:\n\
             Research document: /tmp/x.md\n\
             ticket: RSI-021\n";
        let h = parse_pipeline_handoff(r, "RSI-021").unwrap();
        assert_eq!(h.stage, Stage::Research);
    }

    #[test]
    fn parse_pipeline_handoff_unknown_stage() {
        // The regex captures any [A-Z]+; unknown captures land in
        // UnknownStage rather than MalformedFirstLine.
        let r = "PIPELINE HANDOFF — DEPLOY:\nDoc: /tmp/x.md\n";
        let err = parse_pipeline_handoff(r, "RSI-021").unwrap_err();
        match err {
            ContractError::UnknownStage { stage } => assert_eq!(stage, "DEPLOY"),
            other => panic!("expected UnknownStage, got {:?}", other),
        }
    }

    #[test]
    fn parse_pipeline_handoff_requires_manifest_for_implementation() {
        let r = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
             Implementation document: /tmp/plan.md\n\
             status: complete\n";
        let err = parse_pipeline_handoff(r, "RSI-021").unwrap_err();
        match err {
            ContractError::MissingField { field } => assert_eq!(field, "manifest_path"),
            other => panic!("expected MissingField{{manifest_path}}, got {:?}", other),
        }
    }

    #[test]
    fn parse_verify_handoff_well_formed() {
        let r = "PIPELINE HANDOFF — VERIFY:\n\
             =============================\n\
             Manifest: /tmp/verification.md\n\
             Daemon checks: 3/4\n\
             Status: blocked\n\
             Failed checks: [rsi-rpc ListSessions]\n\
             Blocker: one daemon check failed\n";
        let h = parse_pipeline_handoff(r, "RSI-021").unwrap();
        assert_eq!(h.stage, Stage::Verify);
        assert_eq!(h.doc_path, "/tmp/verification.md");
        assert_eq!(h.manifest_path.as_deref(), Some("/tmp/verification.md"));
        assert_eq!(h.daemon_checks_passed, Some(3));
        assert_eq!(h.total, Some(4));
        assert_eq!(h.failed, vec!["rsi-rpc ListSessions"]);
        assert_eq!(h.status, "blocked");
    }

    #[test]
    fn strict_v2_requires_explicit_closed_status() {
        let err = parse_pipeline_handoff_v2(well_formed_research(), "RSI-021").unwrap_err();
        assert!(matches!(err, ContractError::MissingField { field } if field == "status"));

        let invented = "PIPELINE HANDOFF — RESEARCH:\n\
             Research document: /tmp/x.md\n\
             Status: waiting_for_approval\n";
        let err = parse_pipeline_handoff_v2(invented, "RSI-021").unwrap_err();
        assert!(matches!(err, ContractError::InvalidField { field, .. } if field == "status"));

        let duplicate = "PIPELINE HANDOFF — RESEARCH:\n\
             Research document: /tmp/x.md\n\
             Status: complete\n\
             Status: blocked\n";
        let err = parse_pipeline_handoff_v2(duplicate, "RSI-021").unwrap_err();
        assert!(matches!(err, ContractError::DuplicateField { field } if field == "status"));
    }

    #[test]
    fn strict_v2_canonicalizes_observed_plan_and_review_aliases_with_warnings() {
        let plan = "PIPELINE HANDOFF — PLAN:\n\
             plan_path: /tmp/plan.md\n\
             worker_status: complete\n";
        let parsed_plan = parse_pipeline_handoff_v2(plan, "RSI-021").unwrap();
        assert_eq!(parsed_plan.handoff.doc_path, "/tmp/plan.md");
        assert_eq!(parsed_plan.strict_status, PipelineStatusV2::Complete);
        assert_eq!(
            parsed_plan.warnings,
            vec![
                PipelineHandoffWarningV2 {
                    alias: "plan_path".into(),
                    canonical_key: "doc_path".into(),
                },
                PipelineHandoffWarningV2 {
                    alias: "worker_status".into(),
                    canonical_key: "status".into(),
                },
            ]
        );
        let plan_result = serde_json::to_value(&parsed_plan).unwrap();
        assert_eq!(plan_result["doc_path"], "/tmp/plan.md");
        assert_eq!(plan_result["strict_status"], "complete");
        assert_eq!(plan_result["warnings"][0]["alias"], "plan_path");

        let review = "PIPELINE HANDOFF — REVIEW:\n\
             review_path: /tmp/review.md\n\
             status: complete\n";
        let parsed_review = parse_pipeline_handoff_v2(review, "RSI-021").unwrap();
        assert_eq!(parsed_review.handoff.doc_path, "/tmp/review.md");
        assert_eq!(parsed_review.strict_status, PipelineStatusV2::Complete);
        assert_eq!(
            parsed_review.warnings,
            vec![PipelineHandoffWarningV2 {
                alias: "review_path".into(),
                canonical_key: "doc_path".into(),
            }]
        );
        let review_result = serde_json::to_value(&parsed_review).unwrap();
        assert_eq!(review_result["doc_path"], "/tmp/review.md");
        assert_eq!(review_result["warnings"][0]["canonical_key"], "doc_path");
    }

    #[test]
    fn strict_v2_rejects_canonical_and_alias_duplicates_without_picking_one() {
        let reply = "PIPELINE HANDOFF — RESEARCH:\n\
             doc_path: /tmp/canonical.md\n\
             plan_path: /tmp/alias.md\n\
             status: complete\n";
        assert!(matches!(
            parse_pipeline_handoff_v2(reply, "RSI-021"),
            Err(ContractError::DuplicateField { field }) if field == "doc_path"
        ));
    }

    #[test]
    fn strict_v2_noncomplete_requires_typed_class_and_evidence() {
        let missing = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
             Implementation document: /tmp/handoff.md\n\
             Manifest path: /tmp/verification.md\n\
             Status: blocked\n\
             Blocker: review budget ended\n";
        let err = parse_pipeline_handoff_v2(missing, "RSI-021").unwrap_err();
        assert!(matches!(err, ContractError::MissingField { field } if field == "blocker_class"));

        let forbidden_class =
            format!("{missing}Blocker class: review_findings\nBlocker evidence: HIGH=1\n");
        let err = parse_pipeline_handoff_v2(&forbidden_class, "RSI-021").unwrap_err();
        assert!(
            matches!(err, ContractError::InvalidField { field, .. } if field == "blocker_class")
        );

        let valid = format!(
            "{missing}Blocker class: technical_impasse\nBlocker evidence: three isolated attempts hit the same compiler defect\n"
        );
        let parsed = parse_pipeline_handoff_v2(&valid, "RSI-021").unwrap();
        assert_eq!(parsed.strict_status, PipelineStatusV2::Blocked);
        assert_eq!(
            parsed.blocker_class,
            Some(PipelineBlockerClassV1::TechnicalImpasse)
        );

        let partial = "PIPELINE HANDOFF — RESEARCH:\n\
             Research document: /tmp/research.md\n\
             Status: partial\n\
             Blocker: return budget ended before the last source\n\
             Blocker evidence: one named source remains unread\n";
        let parsed = parse_pipeline_handoff_v2(partial, "RSI-021").unwrap();
        assert_eq!(parsed.strict_status, PipelineStatusV2::Partial);
        assert_eq!(parsed.blocker_class, None);
    }

    fn outcome_json(body: &str) -> String {
        format!("orchestration_outcome_v1: {body}\n")
    }

    #[test]
    fn program_outcome_requires_a_guard_for_an_open_queue() {
        let invalid = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":true,"continuation_state":"queue_exhausted","evidence":"more work exists"}"#,
        );
        assert!(parse_orchestration_outcome_v1(&invalid).is_err());
        assert!(matches!(
            program_continuation_intent_v1(&invalid),
            ProgramContinuationIntentV1::InvalidProgram(_)
        ));

        let missing_job_id = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":true,"continuation_state":"child_watch","evidence":"unnamed row"}"#,
        );
        assert!(matches!(
            parse_orchestration_outcome_v1(&missing_job_id),
            Err(ContractError::MissingField { field })
                if field == "orchestration_outcome_v1.continuation_job_id"
        ));

        let job_id = uuid::Uuid::from_u128(42);
        for (state, expected) in [
            (
                "child_watch",
                ProgramContinuationIntentV1::RequireChildWatch { job_id },
            ),
            (
                "resume_wake",
                ProgramContinuationIntentV1::RequireResumeWake { job_id },
            ),
        ] {
            let valid = outcome_json(&format!(
                r#"{{"schema_version":1,"mode":"program","next_slice_ready":true,"continuation_state":"{state}","continuation_job_id":"{job_id}","evidence":"durable row verified"}}"#
            ));
            assert_eq!(program_continuation_intent_v1(&valid), expected);
        }
    }

    #[test]
    fn program_outcome_accepts_only_typed_idle_terminal_states() {
        let exhausted = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"ledger queue is empty"}"#,
        );
        assert!(matches!(
            program_continuation_intent_v1(&exhausted),
            ProgramContinuationIntentV1::TerminalAllowed
        ));

        let human = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"human_gate","blocker_class":"production","evidence":"requires production daemon restart"}"#,
        );
        assert!(parse_orchestration_outcome_v1(&human).is_ok());

        let untyped = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"human_gate","evidence":"review found HIGH=1"}"#,
        );
        assert!(parse_orchestration_outcome_v1(&untyped).is_err());

        let malformed =
            outcome_json(r#"{"schema_version":1, "mode" : "program", "next_slice_ready": true,"#);
        assert!(matches!(
            program_continuation_intent_v1(&malformed),
            ProgramContinuationIntentV1::InvalidProgram(_)
        ));
    }

    #[test]
    fn legacy_program_report_with_ready_queue_requires_any_guard() {
        let report = "ORCHESTRATION COMPLETE\nMode: program\nNext-slice-ready: yes\n";
        assert!(matches!(
            program_continuation_intent_v1(report),
            ProgramContinuationIntentV1::RequireAnyGuard
        ));
    }

    #[test]
    fn malformed_strict_carrier_cannot_suppress_legacy_program_identity() {
        let report = "orchestration_outcome_v1: not-json\n\
             ORCHESTRATION COMPLETE\n\
             Mode: program\n\
             Next-slice-ready: yes\n";
        assert!(matches!(
            program_continuation_intent_v1(report),
            ProgramContinuationIntentV1::InvalidProgram(_)
        ));
    }

    #[test]
    fn issue373_unregistered_fenced_legacy_examples_are_ordinary() {
        for (opening, interior, closing) in [
            ("```", "", "```"),
            ("```text", "", "```"),
            ("~~~prompt example", "", "~~~"),
            ("  ```text", "", "   ````\t"),
            ("~~~~", "~~~\n```\n", "~~~~~ "),
            ("````text", "```\n~~~~\n", "````"),
            ("```text", "```` trailing text\n", "```"),
            ("~~~text", "    ~~~\n", "~~~"),
            ("```text", "", ""),
            ("~~~text", "", ""),
        ] {
            let reply = format!(
                "Here is a prompt for later use:\n{opening}\n{interior}\
                 Mode: program\nNext slice ready: true\n{closing}\nExample ends here."
            );
            assert_eq!(
                program_continuation_intent_v1_with_registration(&reply, false),
                ProgramContinuationIntentV1::NotProgram,
                "{reply}"
            );
            let following_report = format!("{reply}\nMode: program\nNext slice ready: true\n");
            assert_eq!(
                program_continuation_intent_v1(&following_report),
                if closing.is_empty() {
                    ProgramContinuationIntentV1::NotProgram
                } else {
                    ProgramContinuationIntentV1::RequireAnyGuard
                },
                "{following_report}"
            );
        }
    }

    #[test]
    fn issue373_unregistered_fenced_strict_examples_are_ordinary() {
        let terminal = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"example queue"}"#,
        );
        for carrier in [
            terminal.clone(),
            format!("{terminal}{terminal}"),
            outcome_json(r#"{"mode":"program","#),
            outcome_json("not-json"),
        ] {
            for (opening, closing) in [("```text", "```"), ("~~~example", "~~~"), ("```", "")] {
                let reply = format!("Example:\n{opening}\n{carrier}{closing}\nFor reference.");
                assert_eq!(
                    program_continuation_intent_v1(&reply),
                    ProgramContinuationIntentV1::NotProgram,
                    "{reply}"
                );
            }
        }
    }

    #[test]
    fn issue373_actual_reports_alongside_examples_keep_their_intent() {
        let job_id = uuid::Uuid::from_u128(373);
        let mut reports = vec![
            (
                "Mode: program\nNext slice ready: true\n".to_string(),
                ProgramContinuationIntentV1::RequireAnyGuard,
            ),
            (
                "Mode: program\nNext-slice-ready: no\n".to_string(),
                ProgramContinuationIntentV1::TerminalAllowed,
            ),
            (
                outcome_json(
                    r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"actual empty queue"}"#,
                ),
                ProgramContinuationIntentV1::TerminalAllowed,
            ),
            (
                outcome_json(
                    r#"{"schema_version":1,"mode":"slice","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"actual slice report"}"#,
                ),
                ProgramContinuationIntentV1::NotProgram,
            ),
        ];
        for (state, expected) in [
            (
                "child_watch",
                ProgramContinuationIntentV1::RequireChildWatch { job_id },
            ),
            (
                "resume_wake",
                ProgramContinuationIntentV1::RequireResumeWake { job_id },
            ),
        ] {
            reports.push((outcome_json(&format!(
                r#"{{"schema_version":1,"mode":"program","next_slice_ready":true,"continuation_state":"{state}","continuation_job_id":"{job_id}","evidence":"actual continuation"}}"#
            )), expected));
        }
        let examples = "```text\nMode: slice\nNext slice ready: false\n```\n\
            ~~~text\norchestration_outcome_v1: {\"mode\":\"program\"}\n\
            orchestration_outcome_v1: not-json\n~~~\n";
        for (report, expected) in reports {
            for reply in [
                report.clone(),
                format!("{examples}{report}"),
                format!("{report}{examples}"),
            ] {
                assert_eq!(program_continuation_intent_v1(&reply), expected, "{reply}");
            }
        }
    }

    #[test]
    fn issue373_actual_invalid_carriers_remain_invalid_alongside_examples() {
        let terminal = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"actual empty queue"}"#,
        );
        for report in [
            format!("{terminal}{terminal}"),
            outcome_json(r#"{"mode":"program","#),
            "Mode: program\norchestration_outcome_v1: not-json\n".to_string(),
        ] {
            let example = format!("~~~text\n{terminal}~~~\n");
            for reply in [
                report.clone(),
                format!("{example}{report}"),
                format!("{report}{example}"),
            ] {
                assert!(
                    matches!(
                        program_continuation_intent_v1(&reply),
                        ProgramContinuationIntentV1::InvalidProgram(_)
                    ),
                    "{reply}"
                );
            }
        }
    }

    #[test]
    fn issue373_non_fence_markers_leave_actual_legacy_reports_visible() {
        for marker in [
            "``",
            "~~",
            "    ```text",
            "```info`invalid",
            "prose ```text",
        ] {
            let reply = format!("{marker}\nMode: program\nNext slice ready: true\n");
            assert_eq!(
                program_continuation_intent_v1(&reply),
                ProgramContinuationIntentV1::RequireAnyGuard
            );
        }
    }

    #[test]
    fn issue373_registered_and_generic_parsers_keep_existing_fenced_fields() {
        let carrier = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"strict carrier"}"#,
        );
        let fenced = format!("```text\n{carrier}```\n");
        assert_eq!(
            parse_orchestration_outcome_v1(&fenced),
            parse_orchestration_outcome_v1(&carrier)
        );
        assert_eq!(
            program_continuation_intent_v1_with_registration(&fenced, true),
            ProgramContinuationIntentV1::TerminalAllowed
        );
        assert!(matches!(
            program_continuation_intent_v1_with_registration(&format!("{carrier}{fenced}"), true),
            ProgramContinuationIntentV1::InvalidProgram(_)
        ));
        let handoff =
            parse_worker_report("WORKER REPORT:\n```text\nstatus: complete\nphase: 1\n```\n")
                .unwrap();
        assert_eq!(handoff.status, "complete");
        assert_eq!(handoff.phase, "1");
    }

    #[test]
    fn registered_program_fails_closed_on_missing_duplicate_malformed_or_slice_outcome() {
        let valid_slice = outcome_json(
            r#"{"schema_version":1,"mode":"slice","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"slice returned to parent"}"#,
        );
        let valid_program = outcome_json(
            r#"{"schema_version":1,"mode":"program","next_slice_ready":false,"continuation_state":"queue_exhausted","evidence":"ledger queue is empty"}"#,
        );
        let duplicate = format!("{valid_program}{valid_program}");
        let malformed = outcome_json("not-json");

        for reply in [
            "",
            "Here is a prompt for later use.",
            "```text\nMode: program\nNext slice ready: false\n```\n",
            duplicate.as_str(),
            malformed.as_str(),
            valid_slice.as_str(),
        ] {
            assert!(matches!(
                program_continuation_intent_v1_with_registration(reply, true),
                ProgramContinuationIntentV1::InvalidProgram(_)
            ));
        }
        assert_eq!(
            program_continuation_intent_v1_with_registration(&valid_program, true),
            ProgramContinuationIntentV1::TerminalAllowed
        );
    }

    #[test]
    fn validate_worker_report_first_line_well_formed() {
        let r = "WORKER REPORT:\n==============\nstatus: complete\n";
        assert!(validate_worker_report_first_line(r).is_ok());
    }

    #[test]
    fn validate_worker_report_first_line_rejects_missing_marker() {
        let err = validate_worker_report_first_line("status: complete\n").unwrap_err();
        assert!(matches!(err, ContractError::MalformedFirstLine { .. }));
    }

    #[test]
    fn parse_worker_report_well_formed() {
        let r = "WORKER REPORT:\n\
             ==============\n\
             status: complete\n\
             Phase: 3\n\
             files_modified: src/foo.rs, src/bar.rs\n";
        let w = parse_worker_report(r).unwrap();
        assert_eq!(w.status, "complete");
        assert_eq!(w.phase, "3");
        assert_eq!(w.files_modified, vec!["src/foo.rs", "src/bar.rs"]);
        assert!(w.blocker.is_none());
    }

    #[test]
    fn parse_worker_report_missing_phase() {
        let r = "WORKER REPORT:\nstatus: complete\nfiles_modified: a, b\n";
        let err = parse_worker_report(r).unwrap_err();
        match err {
            ContractError::MissingField { field } => assert_eq!(field, "phase"),
            other => panic!("expected MissingField{{phase}}, got {:?}", other),
        }
    }

    #[test]
    fn parse_worker_report_with_blocker() {
        let r = "WORKER REPORT:\n\
             status: blocked\n\
             Phase: 2\n\
             files_modified: \n\
             blocker: cargo test failed three times\n";
        let w = parse_worker_report(r).unwrap();
        assert_eq!(w.status, "blocked");
        assert_eq!(w.phase, "2");
        assert!(w.files_modified.is_empty());
        assert_eq!(w.blocker.as_deref(), Some("cargo test failed three times"));
    }

    // ----- Cross-stage VERIFY linkage coverage (S6/D1) -----

    fn manifest_covering(keys: &[&str]) -> crate::verification_manifest::VerificationManifest {
        let satisfies = keys.join(", ");
        let doc = format!(
            "---\n\
             ticket: S6\n\
             plan_doc: thoughts/shared/plans/example.md\n\
             generated: 2026-06-29T14:23:00Z\n\
             phases_sealed: [1]\n\
             status: pending_verification\n\
             ---\n\
             \n\
             # Verification Manifest - S6\n\
             \n\
             ## Phase 1 - linkage\n\
             \n\
             ### Automated\n\
             - cross-stage coverage\n\
             \x20\x20satisfies: {satisfies}\n\
             \n\
             ### Daemon-level\n\
             - [PENDING] rsi-rpc ListSessions\n\
             \x20\x20- check: rsi-rpc ListSessions\n\
             \x20\x20- expected: JSON object\n\
             \n\
             ### TUI manual\n\
             - (none)\n"
        );
        match crate::verification_manifest::parse(&doc) {
            Ok(m) => m,
            Err(e) => panic!("fixture manifest should parse: {:?}", e),
        }
    }

    fn verify_handoff_declaring(keys: &str) -> PipelineHandoff {
        let reply = format!(
            "PIPELINE HANDOFF — VERIFY:\n\
             =============================\n\
             Manifest: /tmp/verification.md\n\
             Daemon checks: 2/2\n\
             satisfies: {keys}\n\
             Status: complete\n"
        );
        match parse_pipeline_handoff(&reply, "S6") {
            Ok(h) => h,
            Err(e) => panic!("verify handoff should parse: {:?}", e),
        }
    }

    #[test]
    fn verify_handoff_captures_declared_linkage() {
        let h = verify_handoff_declaring("F-001, F-002");
        assert_eq!(h.stage, Stage::Verify);
        assert_eq!(h.linkage, vec!["F-001", "F-002"]);
    }

    #[test]
    fn cross_stage_pass_accepts_fully_covered() {
        let handoff = verify_handoff_declaring("F-001, F-002");
        let manifest = manifest_covering(&["F-001", "F-002", "F-003"]);
        assert!(cross_stage_verify_coverage(&handoff, &manifest).is_ok());
    }

    #[test]
    fn cross_stage_pass_rejects_uncovered_key() {
        // The plan/research declares F-002 must be satisfied, but the manifest
        // only links F-001 → the drift gate must fail.
        let handoff = verify_handoff_declaring("F-001, F-002");
        let manifest = manifest_covering(&["F-001"]);
        match cross_stage_verify_coverage(&handoff, &manifest).unwrap_err() {
            ContractError::UncoveredLinkage { keys } => assert_eq!(keys, vec!["F-002"]),
            other => panic!("expected UncoveredLinkage, got {:?}", other),
        }
    }

    #[test]
    fn cross_stage_pass_matches_keys_case_insensitively() {
        // The handoff declares lowercase `f-001, f-002`; the manifest covers
        // uppercase `F-001, F-002`. Coverage must match regardless of case.
        let handoff = verify_handoff_declaring("f-001, f-002");
        let manifest = manifest_covering(&["F-001", "F-002"]);
        assert!(
            cross_stage_verify_coverage(&handoff, &manifest).is_ok(),
            "linkage key matching must be case-insensitive"
        );

        // The reverse also holds, and an uncovered key still fails (with the
        // handoff's ORIGINAL casing preserved in the error payload).
        let handoff = verify_handoff_declaring("F-001, F-002");
        let manifest = manifest_covering(&["f-001"]);
        match cross_stage_verify_coverage(&handoff, &manifest).unwrap_err() {
            ContractError::UncoveredLinkage { keys } => assert_eq!(keys, vec!["F-002"]),
            other => panic!("expected UncoveredLinkage, got {:?}", other),
        }
    }

    #[test]
    fn cross_stage_pass_vacuous_when_no_linkage_declared() {
        // A pre-S6 VERIFY handoff declares no linkage; it passes vacuously
        // even against a manifest with no linkage annotations (backward compat).
        let reply = "PIPELINE HANDOFF — VERIFY:\n\
             Manifest: /tmp/verification.md\n\
             Daemon checks: 1/1\n\
             Status: complete\n";
        let handoff = parse_pipeline_handoff(reply, "S6").unwrap();
        assert!(handoff.linkage.is_empty());
        let manifest = manifest_covering(&[]);
        assert!(cross_stage_verify_coverage(&handoff, &manifest).is_ok());
    }

    #[test]
    fn cross_stage_pass_rejects_non_verify_stage() {
        let reply = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
             Implementation document: /tmp/plan.md\n\
             Manifest path: /tmp/verification.md\n\
             satisfies: F-001\n\
             status: complete\n";
        let handoff = parse_pipeline_handoff(reply, "S6").unwrap();
        match cross_stage_verify_coverage(&handoff, &manifest_covering(&["F-001"])).unwrap_err() {
            ContractError::NotVerifyStage { stage } => assert_eq!(stage, "IMPLEMENTATION"),
            other => panic!("expected NotVerifyStage, got {:?}", other),
        }
    }
}
