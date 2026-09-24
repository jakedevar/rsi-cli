//! Pure validation for live recursive DAG output contracts.
//!
//! This module intentionally depends only on shared read-model and contract
//! types. It does not touch `SQLite`, RPC, sessions, provider processes, or
//! scheduler execution.
#![allow(clippy::must_use_candidate, clippy::too_many_lines)]

use crate::recursive_dag::{
    RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION, RecursiveAttemptId, RecursiveAttemptPhase,
    RecursiveCancellationRequestId, RecursiveExecutionArtifact, RecursiveExecutionArtifactKind,
    RecursiveLiveAcceptanceCheck, RecursiveLiveAcceptanceStatus, RecursiveLiveAttemptId,
    RecursiveLiveBlockedPayload, RecursiveLiveCancelledPayload, RecursiveLiveChildTaskSpec,
    RecursiveLiveDecompositionPayload, RecursiveLiveDependencyOutputReference,
    RecursiveLiveDiffSummary, RecursiveLiveOutputArtifactReference, RecursiveLiveOutputEnvelope,
    RecursiveLiveOutputKind, RecursiveLiveOutputMappingDecision, RecursiveLiveOutputOutcome,
    RecursiveLiveOutputParserSource, RecursiveLiveOutputRetryDecision,
    RecursiveLiveOutputRetryDecisionKind, RecursiveLiveOutputValidationArtifactLinks,
    RecursiveLiveOutputValidationIssue, RecursiveLiveOutputValidationResult,
    RecursiveLiveOutputValidationStatus, RecursiveLiveOutputValidationSummary,
    RecursiveLivePermanentFailurePayload, RecursiveLiveRetryableFailurePayload,
    RecursiveLiveSuccessPayload, RecursiveLiveTaskDependencyRef, RecursiveLiveTestResultSummary,
    RecursiveLiveTestStatus, RecursiveLiveValidationIssueClass, RecursiveLiveValidationIssueCode,
    RecursiveLiveValidationIssueLocation, RecursiveLiveValidationIssueSeverity,
    RecursiveSchedulerRunId, RecursiveTaskAttempt, RecursiveTaskEdgeKind, RecursiveTaskGraphDetail,
    RecursiveTaskGraphId, RecursiveTaskId, RecursiveTaskLifecycleState, RecursiveTaskNode,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_LIVE_OUTPUT_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecursiveLiveUnknownFieldPolicy {
    Allow,
    Warn,
    #[default]
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveLiveRawOutputValidationPolicy {
    pub unknown_fields: RecursiveLiveUnknownFieldPolicy,
    pub require_schema_version: bool,
}

impl Default for RecursiveLiveRawOutputValidationPolicy {
    fn default() -> Self {
        Self {
            unknown_fields: RecursiveLiveUnknownFieldPolicy::Reject,
            require_schema_version: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecursiveLiveOutputRepairRetryPolicy {
    pub remaining_output_repair_attempts: u32,
    pub remaining_task_retries: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecursiveLiveCancellationValidationContext {
    pub open_or_observed_request_ids: Vec<RecursiveCancellationRequestId>,
    pub interrupted_session: bool,
    pub live_interrupt_recorded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecursiveLiveSandboxValidationPolicy {
    pub allowed_write_roots: Vec<PathBuf>,
    pub trusted_diff_sources: Vec<String>,
    pub require_trusted_diff_source: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecursiveLiveToolValidationPolicy {
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub network_allowed: bool,
    pub credential_access_allowed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecursiveLiveDecompositionValidationLimits {
    pub max_fanout: Option<u32>,
    pub max_depth: Option<u32>,
    pub max_descendants: Option<u32>,
    pub max_child_retries: Option<u32>,
    pub existing_descendant_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveLiveAllowedArtifactReference {
    pub artifact_id: i64,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub attempt_id: Option<RecursiveAttemptId>,
    pub kind: RecursiveExecutionArtifactKind,
    pub uri: Option<String>,
}

impl From<&RecursiveExecutionArtifact> for RecursiveLiveAllowedArtifactReference {
    fn from(artifact: &RecursiveExecutionArtifact) -> Self {
        Self {
            artifact_id: artifact.id,
            graph_id: artifact.graph_id,
            task_id: artifact.task_id,
            attempt_id: artifact.attempt_id,
            kind: artifact.kind,
            uri: artifact.uri.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveLiveAllowedDependencyOutput {
    pub task_id: RecursiveTaskId,
    pub status: RecursiveTaskLifecycleState,
    pub artifact_ids: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct RecursiveLiveOutputValidationContext<'a> {
    pub parent_task: &'a RecursiveTaskNode,
    pub attempt: &'a RecursiveTaskAttempt,
    pub graph: Option<&'a RecursiveTaskGraphDetail>,
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub scheduler_run_id: RecursiveSchedulerRunId,
    pub session_id: Option<Uuid>,
    pub parser_source: Option<RecursiveLiveOutputParserSource>,
    pub allowed_artifacts: Vec<RecursiveLiveAllowedArtifactReference>,
    pub allowed_dependency_outputs: Vec<RecursiveLiveAllowedDependencyOutput>,
    pub decomposition_limits: RecursiveLiveDecompositionValidationLimits,
    pub raw_output_policy: RecursiveLiveRawOutputValidationPolicy,
    pub retry_policy: RecursiveLiveOutputRepairRetryPolicy,
    pub cancellation_context: RecursiveLiveCancellationValidationContext,
    pub sandbox_policy: RecursiveLiveSandboxValidationPolicy,
    pub tool_policy: RecursiveLiveToolValidationPolicy,
}

impl<'a> RecursiveLiveOutputValidationContext<'a> {
    #[must_use]
    pub fn new(
        parent_task: &'a RecursiveTaskNode,
        attempt: &'a RecursiveTaskAttempt,
        live_attempt_id: RecursiveLiveAttemptId,
        scheduler_run_id: RecursiveSchedulerRunId,
        session_id: Option<Uuid>,
    ) -> Self {
        Self {
            parent_task,
            attempt,
            graph: None,
            live_attempt_id,
            scheduler_run_id,
            session_id,
            parser_source: None,
            allowed_artifacts: Vec::new(),
            allowed_dependency_outputs: Vec::new(),
            decomposition_limits: RecursiveLiveDecompositionValidationLimits {
                max_child_retries: Some(parent_task.max_retries),
                ..RecursiveLiveDecompositionValidationLimits::default()
            },
            raw_output_policy: RecursiveLiveRawOutputValidationPolicy::default(),
            retry_policy: RecursiveLiveOutputRepairRetryPolicy::default(),
            cancellation_context: RecursiveLiveCancellationValidationContext::default(),
            sandbox_policy: RecursiveLiveSandboxValidationPolicy::default(),
            tool_policy: RecursiveLiveToolValidationPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_graph(mut self, graph: &'a RecursiveTaskGraphDetail) -> Self {
        self.graph = Some(graph);
        self.decomposition_limits.max_fanout = self
            .decomposition_limits
            .max_fanout
            .or(Some(graph.graph.max_fanout));
        self.decomposition_limits.max_depth = self
            .decomposition_limits
            .max_depth
            .or(Some(graph.graph.max_depth));
        self.decomposition_limits.max_descendants = self
            .decomposition_limits
            .max_descendants
            .or(Some(graph.graph.max_descendants));
        self.decomposition_limits.existing_descendant_count = self
            .decomposition_limits
            .existing_descendant_count
            .or_else(|| Some(count_descendants(graph, self.parent_task.id)));
        self
    }
}

pub fn validate_recursive_live_output(
    output: &RecursiveLiveOutputEnvelope,
    context: &RecursiveLiveOutputValidationContext<'_>,
) -> RecursiveLiveOutputValidationResult {
    validate_parsed_recursive_live_output(output, context, Vec::new())
}

pub fn validate_recursive_live_output_value(
    raw_output: &Value,
    context: &RecursiveLiveOutputValidationContext<'_>,
) -> RecursiveLiveOutputValidationResult {
    let mut issues = validate_raw_output_shape(raw_output, context);
    let raw_kind = raw_output_kind(raw_output);

    match serde_json::from_value::<RecursiveLiveOutputEnvelope>(raw_output.clone()) {
        Ok(output) => validate_parsed_recursive_live_output(&output, context, issues),
        Err(error) => {
            if !issues.iter().any(is_error_issue) {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::MalformedOutput,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Malformed,
                    output_path("/"),
                    format!("output JSON does not match the live output contract: {error}"),
                ));
            }
            finish_validation(context, raw_kind, None, issues)
        }
    }
}

pub fn validate_recursive_live_output_json(
    raw_output: &str,
    context: &RecursiveLiveOutputValidationContext<'_>,
) -> RecursiveLiveOutputValidationResult {
    match serde_json::from_str::<Value>(raw_output) {
        Ok(value) => validate_recursive_live_output_value(&value, context),
        Err(error) => finish_validation(
            context,
            None,
            None,
            vec![issue(
                RecursiveLiveValidationIssueCode::MalformedOutput,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Malformed,
                output_path("/"),
                format!("raw live output is not valid JSON: {error}"),
            )],
        ),
    }
}

fn validate_parsed_recursive_live_output(
    output: &RecursiveLiveOutputEnvelope,
    context: &RecursiveLiveOutputValidationContext<'_>,
    mut issues: Vec<RecursiveLiveOutputValidationIssue>,
) -> RecursiveLiveOutputValidationResult {
    validate_common_output(output, context, &mut issues);
    validate_artifact_references(output, context, &mut issues);
    validate_dependency_outputs(&output.dependency_outputs, context, &mut issues);
    validate_tests(&output.tests, &mut issues);
    validate_diffs(&output.diffs, context, &mut issues);
    validate_tool_claims(&output.metadata, context, &mut issues);

    match &output.outcome {
        RecursiveLiveOutputOutcome::Success(payload) => {
            validate_success(payload, output, context, &mut issues);
        }
        RecursiveLiveOutputOutcome::Decomposition(payload) => {
            validate_decomposition(payload, context, &mut issues);
        }
        RecursiveLiveOutputOutcome::RetryableFailure(payload) => {
            validate_retryable_failure(payload, context, &mut issues);
        }
        RecursiveLiveOutputOutcome::PermanentFailure(payload) => {
            validate_permanent_failure(payload, context, &mut issues);
        }
        RecursiveLiveOutputOutcome::Blocked(payload) => {
            validate_blocked(payload, context, &mut issues);
        }
        RecursiveLiveOutputOutcome::Cancelled(payload) => {
            validate_cancelled(payload, context, &mut issues);
        }
    }

    finish_validation(context, Some(output.kind()), Some(output.clone()), issues)
}

fn validate_common_output(
    output: &RecursiveLiveOutputEnvelope,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    if output.schema_version != RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::UnknownSchemaVersion,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/schema_version"),
            format!(
                "schema_version {} is not supported; expected {}",
                output.schema_version, RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION
            ),
        ));
    }

    validate_required_text(
        &output.summary,
        "/summary",
        "summary is required",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );

    let correlation = &output.correlation;
    validate_correlation_id(
        correlation.graph_id == context.parent_task.graph_id,
        "/correlation/graph_id",
        "graph_id does not match the selected task",
        issues,
    );
    validate_correlation_id(
        correlation.task_id == context.parent_task.id,
        "/correlation/task_id",
        "task_id does not match the selected task",
        issues,
    );
    validate_correlation_id(
        correlation.attempt_id == context.attempt.id,
        "/correlation/attempt_id",
        "attempt_id does not match the selected recursive attempt",
        issues,
    );
    validate_correlation_id(
        correlation.live_attempt_id == context.live_attempt_id,
        "/correlation/live_attempt_id",
        "live_attempt_id does not match the selected live attempt",
        issues,
    );
    validate_correlation_id(
        correlation.scheduler_run_id == context.scheduler_run_id,
        "/correlation/scheduler_run_id",
        "scheduler_run_id does not match the selected scheduler run",
        issues,
    );

    if let Some(expected_session_id) = context.session_id {
        match correlation.session_id {
            Some(actual_session_id) if actual_session_id == expected_session_id => {}
            Some(_) => validate_correlation_id(
                false,
                "/correlation/session_id",
                "session_id does not match the attached live session",
                issues,
            ),
            None => issues.push(issue(
                RecursiveLiveValidationIssueCode::MissingRequiredField,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Missing,
                output_path("/correlation/session_id"),
                "session_id is required after a live attempt is attached to a session",
            )),
        }
    }
}

fn validate_correlation_id(
    matches_context: bool,
    path: &'static str,
    message: &'static str,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    if !matches_context {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::CorrelationMismatch,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Mismatch,
            output_path(path),
            message,
        ));
    }
}

fn validate_success(
    payload: &RecursiveLiveSuccessPayload,
    output: &RecursiveLiveOutputEnvelope,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    validate_required_text(
        &payload.result_summary,
        "/result_summary",
        "result_summary is required for success output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );

    if payload.acceptance.is_empty() {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Missing,
            output_path("/acceptance"),
            "success output must include acceptance evidence",
        ));
    }

    validate_success_acceptance(&payload.acceptance, context, issues);

    if output.tests.is_empty() {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Missing,
            output_path("/tests"),
            "success output must include test evidence or an explicit not_run test record",
        ));
    }

    for (index, test) in output.tests.iter().enumerate() {
        if test.status == RecursiveLiveTestStatus::Failed {
            if test.required {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::SuccessWithFailedRequiredTest,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Ambiguous,
                    output_path(format!("/tests/{index}/status")),
                    "success output includes a failed required test",
                ));
            } else if test
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
            {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::AmbiguousTerminalKind,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Ambiguous,
                    output_path(format!("/tests/{index}/reason")),
                    "success output with a non-required failed test must explain why it does not affect acceptance",
                ));
            }
        }
    }
}

fn validate_success_acceptance(
    acceptance: &[RecursiveLiveAcceptanceCheck],
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let covered_criteria = acceptance
        .iter()
        .map(|check| check.criterion.trim())
        .filter(|criterion| !criterion.is_empty())
        .collect::<BTreeSet<_>>();

    for expected in &context.parent_task.acceptance_criteria {
        if !expected.trim().is_empty() && !covered_criteria.contains(expected.trim()) {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::SuccessWithUnmetAcceptance,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Ambiguous,
                output_path("/acceptance"),
                format!("success output does not report acceptance criterion `{expected}`"),
            ));
        }
    }

    for (index, check) in acceptance.iter().enumerate() {
        validate_required_text(
            &check.criterion,
            format!("/acceptance/{index}/criterion"),
            "acceptance criterion is required",
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            issues,
        );
        match check.status {
            RecursiveLiveAcceptanceStatus::Met => {
                if check.evidence.is_empty()
                    && check.artifact_ids.is_empty()
                    && check.notes.is_none()
                {
                    issues.push(issue(
                        RecursiveLiveValidationIssueCode::MissingRequiredField,
                        RecursiveLiveValidationIssueSeverity::Error,
                        RecursiveLiveValidationIssueClass::Missing,
                        output_path(format!("/acceptance/{index}/evidence")),
                        "met acceptance criteria require evidence, artifact ids, or notes",
                    ));
                }
            }
            RecursiveLiveAcceptanceStatus::NotApplicable => {
                if check.evidence.is_empty() && check.notes.is_none() {
                    issues.push(issue(
                        RecursiveLiveValidationIssueCode::MissingRequiredField,
                        RecursiveLiveValidationIssueSeverity::Error,
                        RecursiveLiveValidationIssueClass::Missing,
                        output_path(format!("/acceptance/{index}/notes")),
                        "not_applicable acceptance criteria require an explanation",
                    ));
                }
            }
            RecursiveLiveAcceptanceStatus::NotVerified | RecursiveLiveAcceptanceStatus::Unmet => {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::SuccessWithUnmetAcceptance,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Ambiguous,
                    output_path(format!("/acceptance/{index}/status")),
                    "success output cannot include unmet or unverified acceptance criteria",
                ));
            }
        }
    }
}

fn validate_decomposition(
    payload: &RecursiveLiveDecompositionPayload,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    if context.attempt.phase != RecursiveAttemptPhase::Execute {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/kind"),
            "decomposition output is valid only for execute attempts",
        ));
    }

    if context.parent_task.decomposed_once {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/children"),
            "selected parent task has already decomposed",
        ));
    }

    validate_required_text(
        &payload.reason,
        "/reason",
        "reason is required for decomposition output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_required_text(
        &payload.integration_strategy,
        "/integration_strategy",
        "integration_strategy is required for decomposition output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_required_text(
        &payload.verification_strategy,
        "/verification_strategy",
        "verification_strategy is required for decomposition output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );

    if payload.children.is_empty() {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/children"),
            "decomposition output must include at least one child task",
        ));
    }

    let child_count = usize_to_u32_saturating(payload.children.len());
    if let Some(max_fanout) = context.decomposition_limits.max_fanout
        && child_count > max_fanout
    {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/children"),
            format!(
                "decomposition fanout {} exceeds max_fanout {max_fanout}",
                payload.children.len()
            ),
        ));
    }

    if let Some(max_depth) = context.decomposition_limits.max_depth
        && context.parent_task.depth.saturating_add(1) > max_depth
    {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/children"),
            format!(
                "child depth {} exceeds max_depth {max_depth}",
                context.parent_task.depth.saturating_add(1)
            ),
        ));
    }

    if let Some(max_descendants) = context.decomposition_limits.max_descendants {
        for (task_id, existing_descendants) in
            descendant_limit_subjects(context, context.parent_task.id)
        {
            let candidate_descendants = existing_descendants.saturating_add(child_count);
            if candidate_descendants > max_descendants {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::InvalidDecomposition,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Invalid,
                    output_path("/children"),
                    format!(
                        "decomposition would make task {task_id} descendant count {candidate_descendants} exceed max_descendants {max_descendants}"
                    ),
                ));
            }
        }
    }

    validate_child_specs(&payload.children, context, issues);
    validate_decomposition_dependencies(payload, context, issues);
}

fn validate_child_specs(
    children: &[RecursiveLiveChildTaskSpec],
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let mut local_ids = BTreeMap::new();
    let mut task_ids = BTreeSet::new();
    let known_task_ids = known_task_ids(context);
    let mut total_scope_units = 0u32;

    for (index, child) in children.iter().enumerate() {
        validate_required_text(
            &child.local_id,
            format!("/children/{index}/local_id"),
            "child local_id is required",
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            issues,
        );
        if !child.local_id.trim().is_empty()
            && local_ids.insert(child.local_id.clone(), index).is_some()
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::InvalidDecomposition,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Invalid,
                output_path(format!("/children/{index}/local_id")),
                format!("duplicate child local_id `{}`", child.local_id),
            ));
        }

        if let Some(task_id) = child.task_id
            && (!task_ids.insert(task_id) || known_task_ids.contains(&task_id))
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::InvalidDecomposition,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Invalid,
                output_path(format!("/children/{index}/task_id")),
                format!("child task_id {task_id} is duplicated or already exists"),
            ));
        }

        validate_required_text(
            &child.title,
            format!("/children/{index}/title"),
            "child title is required",
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            issues,
        );
        validate_required_text(
            &child.objective,
            format!("/children/{index}/objective"),
            "child objective is required",
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            issues,
        );
        validate_required_text(
            &child.scope,
            format!("/children/{index}/scope"),
            "child scope is required",
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            issues,
        );
        if child.acceptance_criteria.is_empty()
            || child
                .acceptance_criteria
                .iter()
                .any(|criterion| criterion.trim().is_empty())
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::MissingRequiredField,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Missing,
                output_path(format!("/children/{index}/acceptance_criteria")),
                "child acceptance_criteria must include non-empty criteria",
            ));
        }

        if child.scope_units == 0 || child.scope_units >= context.parent_task.scope_units {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ChildNotSmallerThanParent,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Invalid,
                output_path(format!("/children/{index}/scope_units")),
                format!(
                    "child scope_units {} must be positive and smaller than parent scope_units {}",
                    child.scope_units, context.parent_task.scope_units
                ),
            ));
        }
        total_scope_units = total_scope_units.saturating_add(child.scope_units);

        let max_child_retries = context
            .decomposition_limits
            .max_child_retries
            .unwrap_or(context.parent_task.max_retries);
        if child.max_retries > max_child_retries {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::InvalidDecomposition,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Policy,
                output_path(format!("/children/{index}/max_retries")),
                format!(
                    "child max_retries {} exceeds allowed retry budget {max_child_retries}",
                    child.max_retries
                ),
            ));
        }
    }

    if total_scope_units > context.parent_task.scope_units {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::ChildNotSmallerThanParent,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/children"),
            format!(
                "sum of child scope_units {total_scope_units} exceeds parent scope_units {}",
                context.parent_task.scope_units
            ),
        ));
    }
}

fn validate_decomposition_dependencies(
    payload: &RecursiveLiveDecompositionPayload,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let child_keys = child_node_keys(&payload.children);
    let mut graph_edges = existing_candidate_edges(context);
    let mut proposed_dependency_edges = BTreeSet::<(String, String)>::new();
    let parent_key = task_key(context.parent_task.id);

    for child in &payload.children {
        let Some(child_key) = child_keys.get(&child.local_id) else {
            continue;
        };
        graph_edges
            .entry(parent_key.clone())
            .or_default()
            .insert(child_key.clone());
    }

    for (child_index, child) in payload.children.iter().enumerate() {
        let Some(child_key) = child_keys.get(&child.local_id).cloned() else {
            continue;
        };
        for (dependency_index, dependency) in child.dependencies.iter().enumerate() {
            let path = format!("/children/{child_index}/dependencies/{dependency_index}");
            match dependency_key(dependency, &child_keys, context) {
                Ok(dependency_key) => {
                    if dependency_key == child_key {
                        issues.push(issue(
                            RecursiveLiveValidationIssueCode::DependencyCycle,
                            RecursiveLiveValidationIssueSeverity::Error,
                            RecursiveLiveValidationIssueClass::Invalid,
                            output_path(path),
                            "child task cannot depend on itself",
                        ));
                    } else if dependency_key == parent_key {
                        issues.push(issue(
                            RecursiveLiveValidationIssueCode::UnknownDependency,
                            RecursiveLiveValidationIssueSeverity::Error,
                            RecursiveLiveValidationIssueClass::Invalid,
                            output_path(path),
                            "child task cannot depend on its parent task",
                        ));
                    } else {
                        if !proposed_dependency_edges
                            .insert((dependency_key.clone(), child_key.clone()))
                        {
                            issues.push(issue(
                                RecursiveLiveValidationIssueCode::InvalidDecomposition,
                                RecursiveLiveValidationIssueSeverity::Error,
                                RecursiveLiveValidationIssueClass::Invalid,
                                output_path(path),
                                format!(
                                    "duplicate dependency edge from {dependency_key} to {child_key}"
                                ),
                            ));
                        }
                        graph_edges
                            .entry(dependency_key)
                            .or_default()
                            .insert(child_key.clone());
                    }
                }
                Err(message) => issues.push(issue(
                    RecursiveLiveValidationIssueCode::UnknownDependency,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Invalid,
                    output_path(path),
                    message,
                )),
            }
        }
    }

    for (edge_index, edge) in payload.dependency_edges.iter().enumerate() {
        let path = format!("/dependency_edges/{edge_index}");
        let from_key = dependency_key(&edge.from, &child_keys, context);
        let to_key = dependency_key(&edge.to, &child_keys, context);
        let to_is_child = matches!(edge.to, RecursiveLiveTaskDependencyRef::ChildLocal { .. });
        match (from_key, to_key) {
            (Ok(from_key), Ok(to_key)) => {
                if from_key == to_key {
                    issues.push(issue(
                        RecursiveLiveValidationIssueCode::DependencyCycle,
                        RecursiveLiveValidationIssueSeverity::Error,
                        RecursiveLiveValidationIssueClass::Invalid,
                        output_path(path),
                        "dependency edge cannot point from a task to itself",
                    ));
                } else if from_key == parent_key || to_key == parent_key {
                    issues.push(issue(
                        RecursiveLiveValidationIssueCode::UnknownDependency,
                        RecursiveLiveValidationIssueSeverity::Error,
                        RecursiveLiveValidationIssueClass::Invalid,
                        output_path(path),
                        "dependency edge cannot use the selected parent task as a dependency endpoint",
                    ));
                } else if !to_is_child {
                    issues.push(issue(
                        RecursiveLiveValidationIssueCode::UnknownDependency,
                        RecursiveLiveValidationIssueSeverity::Error,
                        RecursiveLiveValidationIssueClass::Invalid,
                        output_path(format!("{path}/to")),
                        "dependency edge target must be a proposed child task",
                    ));
                } else {
                    if !proposed_dependency_edges.insert((from_key.clone(), to_key.clone())) {
                        issues.push(issue(
                            RecursiveLiveValidationIssueCode::InvalidDecomposition,
                            RecursiveLiveValidationIssueSeverity::Error,
                            RecursiveLiveValidationIssueClass::Invalid,
                            output_path(path),
                            format!("duplicate dependency edge from {from_key} to {to_key}"),
                        ));
                    }
                    graph_edges.entry(from_key).or_default().insert(to_key);
                }
            }
            (Err(message), _) | (_, Err(message)) => issues.push(issue(
                RecursiveLiveValidationIssueCode::UnknownDependency,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Invalid,
                output_path(path),
                message,
            )),
        }
    }

    if has_cycle(&graph_edges) {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::DependencyCycle,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path("/dependency_edges"),
            "candidate parent-child and dependency edges would create a cycle",
        ));
    }
}

fn validate_retryable_failure(
    payload: &RecursiveLiveRetryableFailurePayload,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    validate_required_text(
        &payload.reason,
        "/reason",
        "reason is required for retryable_failure output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_required_text(
        &payload.retry_hint,
        "/retry_hint",
        "retry_hint is required for retryable_failure output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);
    validate_artifact_ref_slice(
        &payload.partial_artifacts,
        "/partial_artifacts",
        context,
        issues,
    );
}

fn validate_permanent_failure(
    payload: &RecursiveLivePermanentFailurePayload,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    validate_required_text(
        &payload.reason,
        "/reason",
        "reason is required for permanent_failure output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);
}

fn validate_blocked(
    payload: &RecursiveLiveBlockedPayload,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    validate_required_text(
        &payload.reason,
        "/reason",
        "reason is required for blocked output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_required_text(
        &payload.requested_operator_input,
        "/requested_operator_input",
        "requested_operator_input is required for blocked output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);
}

fn validate_cancelled(
    payload: &RecursiveLiveCancelledPayload,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    validate_required_text(
        &payload.reason,
        "/reason",
        "reason is required for cancelled output",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );
    validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);

    let has_matching_request = payload.cancellation_request_id.is_some_and(|request_id| {
        context
            .cancellation_context
            .open_or_observed_request_ids
            .contains(&request_id)
    });
    let has_cancellation_evidence = has_matching_request
        || context.cancellation_context.interrupted_session
        || context.cancellation_context.live_interrupt_recorded;

    if !has_cancellation_evidence {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::CancelWithoutRequest,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Cancellation,
            output_path("/cancellation_request_id"),
            "cancelled output requires durable cancellation, session interruption, or live interrupt evidence",
        ));
    }
}

fn validate_artifact_references(
    output: &RecursiveLiveOutputEnvelope,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    validate_artifact_ref_slice(&output.artifacts, "/artifacts", context, issues);
    for (test_index, test) in output.tests.iter().enumerate() {
        if let Some(reference) = &test.output_artifact {
            validate_artifact_reference(
                reference,
                &format!("/tests/{test_index}/output_artifact"),
                context,
                issues,
            );
        }
    }
    for (diff_index, diff) in output.diffs.iter().enumerate() {
        if let Some(reference) = &diff.diff_artifact {
            validate_artifact_reference(
                reference,
                &format!("/diffs/{diff_index}/diff_artifact"),
                context,
                issues,
            );
        }
    }
    match &output.outcome {
        RecursiveLiveOutputOutcome::RetryableFailure(payload) => {
            validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);
            validate_artifact_ref_slice(
                &payload.partial_artifacts,
                "/partial_artifacts",
                context,
                issues,
            );
        }
        RecursiveLiveOutputOutcome::PermanentFailure(payload) => {
            validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);
        }
        RecursiveLiveOutputOutcome::Blocked(payload) => {
            validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);
        }
        RecursiveLiveOutputOutcome::Cancelled(payload) => {
            validate_artifact_ref_slice(&payload.evidence, "/evidence", context, issues);
        }
        RecursiveLiveOutputOutcome::Success(_) | RecursiveLiveOutputOutcome::Decomposition(_) => {}
    }
}

fn validate_artifact_ref_slice(
    references: &[RecursiveLiveOutputArtifactReference],
    base_path: &str,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    for (index, reference) in references.iter().enumerate() {
        validate_artifact_reference(reference, &format!("{base_path}/{index}"), context, issues);
    }
}

fn validate_artifact_reference(
    reference: &RecursiveLiveOutputArtifactReference,
    path: &str,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    validate_required_text(
        &reference.label,
        format!("{path}/label"),
        "artifact label is required",
        RecursiveLiveValidationIssueCode::MissingRequiredField,
        issues,
    );

    if let Some(existing_artifact_id) = reference.existing_artifact_id {
        let allowed = context
            .allowed_artifacts
            .iter()
            .find(|allowed| allowed.artifact_id == existing_artifact_id);
        let Some(allowed) = allowed else {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/existing_artifact_id")),
                format!("artifact id {existing_artifact_id} is not in the allowed artifact set"),
            ));
            return;
        };

        if allowed.graph_id != context.parent_task.graph_id {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/existing_artifact_id")),
                "artifact graph_id does not match the selected recursive graph",
            ));
        }
        if reference
            .task_id
            .is_some_and(|task_id| task_id != allowed.task_id)
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/task_id")),
                "artifact task_id does not match the allowed persisted artifact owner",
            ));
        }
        if reference
            .attempt_id
            .is_some_and(|attempt_id| Some(attempt_id) != allowed.attempt_id)
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/attempt_id")),
                "artifact attempt_id does not match the allowed persisted artifact owner",
            ));
        }
        if reference.kind != allowed.kind {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/kind")),
                "artifact kind does not match the allowed persisted artifact",
            ));
        }
        if let (Some(actual), Some(expected)) = (&reference.uri, &allowed.uri)
            && actual != expected
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/uri")),
                "artifact uri does not match the allowed persisted artifact",
            ));
        }
    } else {
        if is_external_artifact(reference) {
            return;
        }

        if matches!(
            reference.kind,
            RecursiveExecutionArtifactKind::File
                | RecursiveExecutionArtifactKind::WorkflowExecution
        ) && reference
            .uri
            .as_deref()
            .is_none_or(|uri| uri.trim().is_empty())
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::MissingRequiredField,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Missing,
                output_path(format!("{path}/uri")),
                "file and workflow artifacts require uri",
            ));
        }
        if reference
            .task_id
            .is_some_and(|task_id| task_id != context.parent_task.id)
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/task_id")),
                "new live output artifacts may only target the selected task",
            ));
        }
        if reference
            .attempt_id
            .is_some_and(|attempt_id| attempt_id != context.attempt.id)
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::ArtifactMismatch,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Mismatch,
                output_path(format!("{path}/attempt_id")),
                "new live output artifacts may only target the selected recursive attempt",
            ));
        }
    }

    validate_artifact_uri(reference, path, context, issues);
}

fn validate_artifact_uri(
    reference: &RecursiveLiveOutputArtifactReference,
    path: &str,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(uri) = &reference.uri else {
        return;
    };
    let Some(raw_path) = uri.strip_prefix("file://") else {
        return;
    };
    if context.sandbox_policy.allowed_write_roots.is_empty() {
        return;
    }
    let artifact_path = PathBuf::from(raw_path);
    let inside_allowed_root = context
        .sandbox_policy
        .allowed_write_roots
        .iter()
        .any(|root| path_starts_with(&artifact_path, root));
    if !inside_allowed_root {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::UnsafeSandboxClaim,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Unsafe,
            output_path(format!("{path}/uri")),
            "file artifact uri is outside the allowed live output roots",
        ));
    }
}

fn validate_dependency_outputs(
    dependency_outputs: &[RecursiveLiveDependencyOutputReference],
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    for (index, dependency_output) in dependency_outputs.iter().enumerate() {
        let path = format!("/dependency_outputs/{index}");
        let allowed = context
            .allowed_dependency_outputs
            .iter()
            .find(|allowed| allowed.task_id == dependency_output.task_id);
        let Some(allowed) = allowed else {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::UnknownDependency,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Invalid,
                output_path(format!("{path}/task_id")),
                format!(
                    "dependency output task {} is not in the allowed dependency context",
                    dependency_output.task_id
                ),
            ));
            continue;
        };
        if dependency_output.status != RecursiveTaskLifecycleState::Succeeded
            || allowed.status != RecursiveTaskLifecycleState::Succeeded
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::UnknownDependency,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Invalid,
                output_path(format!("{path}/status")),
                "dependency outputs must reference succeeded dependencies",
            ));
        }
        validate_required_text(
            &dependency_output.summary,
            format!("{path}/summary"),
            "dependency output summary is required",
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            issues,
        );
        for (artifact_index, artifact_id) in dependency_output.artifact_ids.iter().enumerate() {
            if !allowed.artifact_ids.contains(artifact_id) {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::ArtifactMismatch,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Mismatch,
                    output_path(format!("{path}/artifact_ids/{artifact_index}")),
                    format!(
                        "dependency output artifact id {artifact_id} is not allowed for dependency {}",
                        dependency_output.task_id
                    ),
                ));
            }
        }
    }
}

fn validate_tests(
    tests: &[RecursiveLiveTestResultSummary],
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    for (index, test) in tests.iter().enumerate() {
        let path = format!("/tests/{index}");
        if test.status != RecursiveLiveTestStatus::NotRun
            && test
                .command
                .as_deref()
                .is_none_or(|command| command.trim().is_empty())
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::MissingRequiredField,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Missing,
                output_path(format!("{path}/command")),
                "test command is required unless status is not_run",
            ));
        }
        if matches!(
            test.status,
            RecursiveLiveTestStatus::Passed | RecursiveLiveTestStatus::Failed
        ) && test.exit_code.is_none()
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::MissingRequiredField,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Missing,
                output_path(format!("{path}/exit_code")),
                "test exit_code is required when a command ran",
            ));
        }
        if matches!(
            test.status,
            RecursiveLiveTestStatus::Skipped | RecursiveLiveTestStatus::NotRun
        ) && test
            .reason
            .as_deref()
            .is_none_or(|reason| reason.trim().is_empty())
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::MissingRequiredField,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Missing,
                output_path(format!("{path}/reason")),
                "skipped and not_run tests require a reason",
            ));
        }
        if test.status == RecursiveLiveTestStatus::Passed
            && test.exit_code.is_some_and(|code| code != 0)
        {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::IncoherentTestCounts,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Ambiguous,
                output_path(format!("{path}/exit_code")),
                "passed test cannot report a non-zero exit_code",
            ));
        }
        if test.status == RecursiveLiveTestStatus::Failed && test.exit_code == Some(0) {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::IncoherentTestCounts,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Ambiguous,
                output_path(format!("{path}/exit_code")),
                "failed test cannot report a zero exit_code",
            ));
        }
        validate_test_count_metadata(test, &path, issues);
    }
}

fn validate_test_count_metadata(
    test: &RecursiveLiveTestResultSummary,
    path: &str,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(metadata) = test.metadata.as_object() else {
        return;
    };
    let pass_count = metadata.get("pass_count").and_then(Value::as_u64);
    let fail_count = metadata.get("fail_count").and_then(Value::as_u64);
    let skipped_count = metadata.get("skipped_count").and_then(Value::as_u64);
    let not_run_count = metadata.get("not_run_count").and_then(Value::as_u64);
    let total_count = metadata.get("total_count").and_then(Value::as_u64);

    if let Some(total_count) = total_count {
        let sum = pass_count.unwrap_or(0)
            + fail_count.unwrap_or(0)
            + skipped_count.unwrap_or(0)
            + not_run_count.unwrap_or(0);
        if sum != total_count {
            issues.push(issue(
                RecursiveLiveValidationIssueCode::IncoherentTestCounts,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Ambiguous,
                output_path(format!("{path}/metadata/total_count")),
                format!("test count metadata sums to {sum}, not total_count {total_count}"),
            ));
        }
    }

    if test.status == RecursiveLiveTestStatus::Passed && fail_count.is_some_and(|count| count > 0) {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::IncoherentTestCounts,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Ambiguous,
            output_path(format!("{path}/metadata/fail_count")),
            "passed test summary cannot include failing test counts",
        ));
    }
}

fn validate_diffs(
    diffs: &[RecursiveLiveDiffSummary],
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    for (index, diff) in diffs.iter().enumerate() {
        let path = format!("/diffs/{index}");
        if context.sandbox_policy.require_trusted_diff_source {
            let source = diff.metadata.get("source").and_then(Value::as_str);
            let trusted = source.is_some_and(|source| {
                context
                    .sandbox_policy
                    .trusted_diff_sources
                    .iter()
                    .any(|trusted| trusted == source)
            });
            if !trusted {
                issues.push(issue_with_metadata(
                    RecursiveLiveValidationIssueCode::UnsafeSandboxClaim,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Unsafe,
                    output_path(format!("{path}/metadata/source")),
                    "diff summary source is not trusted by the live output validation policy",
                    serde_json::json!({ "source": source }),
                ));
            }
        }

        for (file_index, file) in diff.files.iter().enumerate() {
            if file.inside_allowed_root == Some(false) {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::UnsafeSandboxClaim,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Unsafe,
                    Some(RecursiveLiveValidationIssueLocation::DiffPath {
                        path: file.path.clone(),
                    }),
                    format!(
                        "diff file {} is marked outside the allowed workspace or sandbox root",
                        file.path.display()
                    ),
                ));
            }
            if file.path.as_os_str().is_empty() {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::MissingRequiredField,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Missing,
                    output_path(format!("{path}/files/{file_index}/path")),
                    "diff file path is required",
                ));
            }
        }
    }
}

fn validate_tool_claims(
    metadata: &Value,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    if let Some(tools) = metadata.get("tools_used").and_then(Value::as_array) {
        for (index, tool) in tools.iter().enumerate() {
            let Some(tool) = tool.as_str() else {
                continue;
            };
            if context
                .tool_policy
                .denied_tools
                .iter()
                .any(|denied| denied == tool)
            {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::UnsafeToolClaim,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Unsafe,
                    output_path(format!("/metadata/tools_used/{index}")),
                    format!("output claims use of denied tool `{tool}`"),
                ));
            }
            if !context.tool_policy.allowed_tools.is_empty()
                && !context
                    .tool_policy
                    .allowed_tools
                    .iter()
                    .any(|allowed| allowed == tool)
            {
                issues.push(issue(
                    RecursiveLiveValidationIssueCode::UnsafeToolClaim,
                    RecursiveLiveValidationIssueSeverity::Error,
                    RecursiveLiveValidationIssueClass::Unsafe,
                    output_path(format!("/metadata/tools_used/{index}")),
                    format!("output claims use of tool `{tool}` outside the allowed tool policy"),
                ));
            }
        }
    }

    if metadata
        .get("network_access")
        .and_then(Value::as_bool)
        .is_some_and(|claimed| claimed)
        && !context.tool_policy.network_allowed
    {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::UnsafeToolClaim,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Unsafe,
            output_path("/metadata/network_access"),
            "output claims network access that was not granted",
        ));
    }
    if metadata
        .get("credential_access")
        .and_then(Value::as_bool)
        .is_some_and(|claimed| claimed)
        && !context.tool_policy.credential_access_allowed
    {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::UnsafeToolClaim,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Unsafe,
            output_path("/metadata/credential_access"),
            "output claims credential access that was not granted",
        ));
    }
}

fn validate_required_text(
    value: &str,
    path: impl Into<String>,
    empty_message: impl Into<String>,
    code: RecursiveLiveValidationIssueCode,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let path = path.into();
    if value.trim().is_empty() {
        issues.push(issue(
            code,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Missing,
            output_path(path),
            empty_message,
        ));
    } else if value.len() > MAX_LIVE_OUTPUT_TEXT_BYTES {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::Other,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Invalid,
            output_path(path),
            format!(
                "text field exceeds the live output limit of {MAX_LIVE_OUTPUT_TEXT_BYTES} bytes"
            ),
        ));
    }
}

fn validate_raw_output_shape(
    raw_output: &Value,
    context: &RecursiveLiveOutputValidationContext<'_>,
) -> Vec<RecursiveLiveOutputValidationIssue> {
    let mut issues = Vec::new();
    let Some(object) = raw_output.as_object() else {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::MalformedOutput,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Malformed,
            output_path("/"),
            "top-level live output must be a JSON object",
        ));
        return issues;
    };

    let output_kind = raw_output_kind(raw_output);
    let known_top_level = known_top_level_fields(output_kind);
    check_unknown_fields(
        object,
        "/",
        &known_top_level,
        context.raw_output_policy.unknown_fields,
        &mut issues,
    );

    if context.raw_output_policy.require_schema_version {
        match object.get("schema_version") {
            Some(Value::Number(number)) if number.as_u64() == Some(1) => {}
            Some(Value::Number(number)) => issues.push(issue(
                RecursiveLiveValidationIssueCode::UnknownSchemaVersion,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Invalid,
                output_path("/schema_version"),
                format!(
                    "schema_version {number} is not supported; expected {RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION}"
                ),
            )),
            Some(_) => issues.push(issue(
                RecursiveLiveValidationIssueCode::MalformedOutput,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Malformed,
                output_path("/schema_version"),
                "schema_version must be an integer",
            )),
            None => issues.push(issue(
                RecursiveLiveValidationIssueCode::MissingRequiredField,
                RecursiveLiveValidationIssueSeverity::Error,
                RecursiveLiveValidationIssueClass::Missing,
                output_path("/schema_version"),
                "schema_version is required in raw live output",
            )),
        }
    }

    require_raw_field(object, "/kind", "kind", &mut issues);
    require_raw_field(object, "/correlation", "correlation", &mut issues);
    require_raw_text(object, "/summary", "summary", &mut issues);

    if object.get("kind").is_some() && output_kind.is_none() {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::MalformedOutput,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Malformed,
            output_path("/kind"),
            "kind is not a recognized recursive live output kind",
        ));
    }

    check_nested_unknown_fields(raw_output, output_kind, context, &mut issues);
    validate_raw_outcome_required_fields(object, output_kind, &mut issues);
    validate_raw_terminal_field_consistency(object, output_kind, &mut issues);
    issues
}

fn validate_raw_outcome_required_fields(
    object: &serde_json::Map<String, Value>,
    output_kind: Option<RecursiveLiveOutputKind>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    match output_kind {
        Some(RecursiveLiveOutputKind::Success) => {
            require_raw_text(object, "/result_summary", "result_summary", issues);
            require_raw_non_empty_array(object, "/acceptance", "acceptance", issues);
        }
        Some(RecursiveLiveOutputKind::Decomposition) => {
            require_raw_text(object, "/reason", "reason", issues);
            require_raw_non_empty_array(object, "/children", "children", issues);
            require_raw_text(
                object,
                "/integration_strategy",
                "integration_strategy",
                issues,
            );
            require_raw_text(
                object,
                "/verification_strategy",
                "verification_strategy",
                issues,
            );
        }
        Some(RecursiveLiveOutputKind::RetryableFailure) => {
            require_raw_text(object, "/reason", "reason", issues);
            require_raw_text(object, "/retry_hint", "retry_hint", issues);
        }
        Some(RecursiveLiveOutputKind::PermanentFailure) => {
            require_raw_text(object, "/reason", "reason", issues);
            require_raw_field(object, "/failure_class", "failure_class", issues);
        }
        Some(RecursiveLiveOutputKind::Blocked) => {
            require_raw_text(object, "/reason", "reason", issues);
            require_raw_text(
                object,
                "/requested_operator_input",
                "requested_operator_input",
                issues,
            );
            require_raw_field(object, "/blocked_on", "blocked_on", issues);
            require_raw_field(
                object,
                "/safe_to_retry_without_input",
                "safe_to_retry_without_input",
                issues,
            );
        }
        Some(RecursiveLiveOutputKind::Cancelled) => {
            require_raw_text(object, "/reason", "reason", issues);
            require_raw_field(object, "/observed_scope", "observed_scope", issues);
        }
        None => {}
    }
}

fn check_nested_unknown_fields(
    raw_output: &Value,
    output_kind: Option<RecursiveLiveOutputKind>,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(object) = raw_output.as_object() else {
        return;
    };
    check_object_value(
        object.get("correlation"),
        "/correlation",
        &[
            "graph_id",
            "task_id",
            "attempt_id",
            "live_attempt_id",
            "scheduler_run_id",
            "session_id",
        ],
        context,
        issues,
    );
    check_object_value(
        object.get("confidence"),
        "/confidence",
        &["self_assessment", "evidence", "known_risks", "metadata"],
        context,
        issues,
    );
    check_array_object_values(
        object.get("artifacts"),
        "/artifacts",
        &artifact_fields(),
        context,
        issues,
    );
    check_array_object_values(
        object.get("tests"),
        "/tests",
        &[
            "command",
            "status",
            "exit_code",
            "duration_ms",
            "output_artifact",
            "required",
            "reason",
            "metadata",
        ],
        context,
        issues,
    );
    if let Some(tests) = object.get("tests").and_then(Value::as_array) {
        for (test_index, test) in tests.iter().enumerate() {
            check_object_value(
                test.get("output_artifact"),
                &format!("/tests/{test_index}/output_artifact"),
                &artifact_fields(),
                context,
                issues,
            );
        }
    }
    check_array_object_values(
        object.get("dependency_outputs"),
        "/dependency_outputs",
        &["task_id", "status", "artifact_ids", "summary", "metadata"],
        context,
        issues,
    );
    check_diff_unknown_fields(object.get("diffs"), context, issues);

    match output_kind {
        Some(RecursiveLiveOutputKind::Success) => {
            check_array_object_values(
                object.get("acceptance"),
                "/acceptance",
                &["criterion", "status", "evidence", "artifact_ids", "notes"],
                context,
                issues,
            );
        }
        Some(RecursiveLiveOutputKind::Decomposition) => {
            check_array_object_values(
                object.get("children"),
                "/children",
                &[
                    "local_id",
                    "task_id",
                    "title",
                    "objective",
                    "scope",
                    "acceptance_criteria",
                    "scope_units",
                    "max_retries",
                    "dependencies",
                    "metadata",
                ],
                context,
                issues,
            );
            check_dependency_ref_arrays(object.get("children"), context, issues);
            check_array_object_values(
                object.get("dependency_edges"),
                "/dependency_edges",
                &["from", "to", "rationale", "metadata"],
                context,
                issues,
            );
            check_dependency_edge_refs(object.get("dependency_edges"), context, issues);
            check_object_value(
                object.get("budget_hints"),
                "/budget_hints",
                &[
                    "expected_child_count",
                    "total_scope_units",
                    "max_child_scope_units",
                    "retry_budget",
                    "scope_notes",
                    "metadata",
                ],
                context,
                issues,
            );
        }
        Some(RecursiveLiveOutputKind::RetryableFailure) => {
            check_array_object_values(
                object.get("evidence"),
                "/evidence",
                &artifact_fields(),
                context,
                issues,
            );
            check_array_object_values(
                object.get("partial_artifacts"),
                "/partial_artifacts",
                &artifact_fields(),
                context,
                issues,
            );
        }
        Some(
            RecursiveLiveOutputKind::PermanentFailure
            | RecursiveLiveOutputKind::Blocked
            | RecursiveLiveOutputKind::Cancelled,
        ) => {
            check_array_object_values(
                object.get("evidence"),
                "/evidence",
                &artifact_fields(),
                context,
                issues,
            );
        }
        None => {}
    }
}

fn validate_raw_terminal_field_consistency(
    object: &serde_json::Map<String, Value>,
    output_kind: Option<RecursiveLiveOutputKind>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(output_kind) = output_kind else {
        return;
    };

    for field in object.keys() {
        if is_outcome_field_for_kind(field, output_kind) || !is_known_outcome_field(field) {
            continue;
        }
        issues.push(issue_with_metadata(
            RecursiveLiveValidationIssueCode::AmbiguousTerminalKind,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Ambiguous,
            output_path(format!("/{field}")),
            format!("field `{field}` belongs to a different recursive live output kind"),
            serde_json::json!({ "field": field, "kind": output_kind }),
        ));
    }
}

fn is_known_outcome_field(field: &str) -> bool {
    matches!(
        field,
        "result_summary"
            | "acceptance"
            | "reason"
            | "children"
            | "dependency_edges"
            | "integration_strategy"
            | "verification_strategy"
            | "budget_hints"
            | "scope_hints"
            | "retry_hint"
            | "partial_artifacts"
            | "failure_class"
            | "requested_operator_input"
            | "blocked_on"
            | "safe_to_retry_without_input"
            | "cancellation_request_id"
            | "observed_scope"
            | "operator_message"
            | "evidence"
            | "suggested_next_action"
    )
}

fn is_outcome_field_for_kind(field: &str, kind: RecursiveLiveOutputKind) -> bool {
    match kind {
        RecursiveLiveOutputKind::Success => matches!(field, "result_summary" | "acceptance"),
        RecursiveLiveOutputKind::Decomposition => matches!(
            field,
            "reason"
                | "children"
                | "dependency_edges"
                | "integration_strategy"
                | "verification_strategy"
                | "budget_hints"
                | "scope_hints"
        ),
        RecursiveLiveOutputKind::RetryableFailure => matches!(
            field,
            "reason"
                | "retry_hint"
                | "operator_message"
                | "evidence"
                | "partial_artifacts"
                | "suggested_next_action"
        ),
        RecursiveLiveOutputKind::PermanentFailure => matches!(
            field,
            "reason" | "failure_class" | "operator_message" | "evidence" | "suggested_next_action"
        ),
        RecursiveLiveOutputKind::Blocked => matches!(
            field,
            "reason"
                | "requested_operator_input"
                | "blocked_on"
                | "safe_to_retry_without_input"
                | "operator_message"
                | "evidence"
                | "suggested_next_action"
        ),
        RecursiveLiveOutputKind::Cancelled => matches!(
            field,
            "reason"
                | "cancellation_request_id"
                | "observed_scope"
                | "operator_message"
                | "evidence"
                | "suggested_next_action"
        ),
    }
}

fn check_diff_unknown_fields(
    diffs: Option<&Value>,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    check_array_object_values(
        diffs,
        "/diffs",
        &[
            "worktree_root",
            "sandbox_root",
            "files",
            "diff_artifact",
            "clean_worktree",
            "metadata",
        ],
        context,
        issues,
    );
    let Some(diff_array) = diffs.and_then(Value::as_array) else {
        return;
    };
    for (diff_index, diff) in diff_array.iter().enumerate() {
        check_object_value(
            diff.get("diff_artifact"),
            &format!("/diffs/{diff_index}/diff_artifact"),
            &artifact_fields(),
            context,
            issues,
        );
        check_array_object_values(
            diff.get("files"),
            &format!("/diffs/{diff_index}/files"),
            &[
                "path",
                "status",
                "previous_path",
                "insertions",
                "deletions",
                "inside_allowed_root",
                "metadata",
            ],
            context,
            issues,
        );
    }
}

fn check_dependency_ref_arrays(
    children: Option<&Value>,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(children) = children.and_then(Value::as_array) else {
        return;
    };
    for (child_index, child) in children.iter().enumerate() {
        let Some(dependencies) = child.get("dependencies").and_then(Value::as_array) else {
            continue;
        };
        for (dependency_index, dependency) in dependencies.iter().enumerate() {
            check_dependency_ref_unknown_fields(
                dependency,
                &format!("/children/{child_index}/dependencies/{dependency_index}"),
                context,
                issues,
            );
        }
    }
}

fn check_dependency_edge_refs(
    dependency_edges: Option<&Value>,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(edges) = dependency_edges.and_then(Value::as_array) else {
        return;
    };
    for (edge_index, edge) in edges.iter().enumerate() {
        check_dependency_ref_unknown_fields(
            edge.get("from").unwrap_or(&Value::Null),
            &format!("/dependency_edges/{edge_index}/from"),
            context,
            issues,
        );
        check_dependency_ref_unknown_fields(
            edge.get("to").unwrap_or(&Value::Null),
            &format!("/dependency_edges/{edge_index}/to"),
            context,
            issues,
        );
    }
}

fn check_dependency_ref_unknown_fields(
    dependency_ref: &Value,
    path: &str,
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(object) = dependency_ref.as_object() else {
        return;
    };
    let allowed = match object.get("kind").and_then(Value::as_str) {
        Some("child_local") => vec!["kind", "local_id"],
        Some("existing_task") => vec!["kind", "task_id"],
        _ => vec!["kind", "local_id", "task_id"],
    };
    check_unknown_fields(
        object,
        path,
        &allowed,
        context.raw_output_policy.unknown_fields,
        issues,
    );
}

fn check_object_value(
    value: Option<&Value>,
    path: &str,
    allowed_fields: &[&str],
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    if let Some(object) = value.and_then(Value::as_object) {
        check_unknown_fields(
            object,
            path,
            allowed_fields,
            context.raw_output_policy.unknown_fields,
            issues,
        );
    }
}

fn check_array_object_values(
    value: Option<&Value>,
    path: &str,
    allowed_fields: &[&str],
    context: &RecursiveLiveOutputValidationContext<'_>,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    let Some(array) = value.and_then(Value::as_array) else {
        return;
    };
    for (index, value) in array.iter().enumerate() {
        if let Some(object) = value.as_object() {
            check_unknown_fields(
                object,
                &format!("{path}/{index}"),
                allowed_fields,
                context.raw_output_policy.unknown_fields,
                issues,
            );
        }
    }
}

fn check_unknown_fields(
    object: &serde_json::Map<String, Value>,
    path: &str,
    allowed_fields: &[&str],
    policy: RecursiveLiveUnknownFieldPolicy,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    if policy == RecursiveLiveUnknownFieldPolicy::Allow {
        return;
    }
    let allowed = allowed_fields.iter().copied().collect::<BTreeSet<_>>();
    for field in object.keys() {
        if !allowed.contains(field.as_str()) {
            let severity = match policy {
                RecursiveLiveUnknownFieldPolicy::Allow => continue,
                RecursiveLiveUnknownFieldPolicy::Warn => {
                    RecursiveLiveValidationIssueSeverity::Warning
                }
                RecursiveLiveUnknownFieldPolicy::Reject => {
                    RecursiveLiveValidationIssueSeverity::Error
                }
            };
            issues.push(issue_with_metadata(
                RecursiveLiveValidationIssueCode::UnknownField,
                severity,
                RecursiveLiveValidationIssueClass::Policy,
                output_path(format!("{}/{}", path.trim_end_matches('/'), field)),
                format!(
                    "unknown field `{field}` is not part of the recursive live output contract"
                ),
                serde_json::json!({ "field": field }),
            ));
        }
    }
}

fn require_raw_field(
    object: &serde_json::Map<String, Value>,
    path: &'static str,
    field: &'static str,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    if !object.contains_key(field) {
        issues.push(issue(
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Missing,
            output_path(path),
            format!("{field} is required in raw live output"),
        ));
    }
}

fn require_raw_text(
    object: &serde_json::Map<String, Value>,
    path: &'static str,
    field: &'static str,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    match object.get(field) {
        Some(Value::String(value)) if !value.trim().is_empty() => {}
        Some(Value::String(_)) | None => issues.push(issue(
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Missing,
            output_path(path),
            format!("{field} must be a non-empty string in raw live output"),
        )),
        Some(_) => issues.push(issue(
            RecursiveLiveValidationIssueCode::MalformedOutput,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Malformed,
            output_path(path),
            format!("{field} must be a string in raw live output"),
        )),
    }
}

fn require_raw_non_empty_array(
    object: &serde_json::Map<String, Value>,
    path: &'static str,
    field: &'static str,
    issues: &mut Vec<RecursiveLiveOutputValidationIssue>,
) {
    match object.get(field) {
        Some(Value::Array(values)) if !values.is_empty() => {}
        Some(Value::Array(_)) | None => issues.push(issue(
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Missing,
            output_path(path),
            format!("{field} must be a non-empty array in raw live output"),
        )),
        Some(_) => issues.push(issue(
            RecursiveLiveValidationIssueCode::MalformedOutput,
            RecursiveLiveValidationIssueSeverity::Error,
            RecursiveLiveValidationIssueClass::Malformed,
            output_path(path),
            format!("{field} must be an array in raw live output"),
        )),
    }
}

fn finish_validation(
    context: &RecursiveLiveOutputValidationContext<'_>,
    output_kind: Option<RecursiveLiveOutputKind>,
    normalized_output: Option<RecursiveLiveOutputEnvelope>,
    mut issues: Vec<RecursiveLiveOutputValidationIssue>,
) -> RecursiveLiveOutputValidationResult {
    sort_and_deduplicate_issues(&mut issues);
    let error_count =
        usize_to_u32_saturating(issues.iter().filter(|issue| is_error_issue(issue)).count());
    let warning_count = usize_to_u32_saturating(
        issues
            .iter()
            .filter(|issue| issue.severity == RecursiveLiveValidationIssueSeverity::Warning)
            .count(),
    );
    let info_count = usize_to_u32_saturating(
        issues
            .iter()
            .filter(|issue| issue.severity == RecursiveLiveValidationIssueSeverity::Info)
            .count(),
    );
    let (status, mapping_decision, retry_decision) =
        decide_validation_result(output_kind, &issues, context);

    let accepted_output = if status == RecursiveLiveOutputValidationStatus::Valid {
        normalized_output
    } else {
        None
    };
    let normalized_digest = accepted_output
        .as_ref()
        .and_then(deterministic_normalized_digest);

    RecursiveLiveOutputValidationResult {
        summary: RecursiveLiveOutputValidationSummary {
            validation_id: None,
            live_attempt_id: context.live_attempt_id,
            graph_id: context.parent_task.graph_id,
            task_id: context.parent_task.id,
            scheduler_run_id: context.scheduler_run_id,
            attempt_id: context.attempt.id,
            session_id: context.session_id,
            status,
            output_kind,
            mapping_decision: Some(mapping_decision),
            retry_decision: Some(retry_decision),
            raw_output_artifact_id: None,
            normalized_output_artifact_id: None,
            validation_artifact_id: None,
            normalized_digest,
            issue_count: usize_to_u32_saturating(issues.len()),
            error_count,
            warning_count,
            info_count,
            created_at: None,
        },
        artifact_links: RecursiveLiveOutputValidationArtifactLinks::default(),
        parser_source: context.parser_source.clone(),
        issues,
        normalized_output: accepted_output,
        validation_report: None,
        metadata: Value::Null,
    }
}

fn decide_validation_result(
    output_kind: Option<RecursiveLiveOutputKind>,
    issues: &[RecursiveLiveOutputValidationIssue],
    context: &RecursiveLiveOutputValidationContext<'_>,
) -> (
    RecursiveLiveOutputValidationStatus,
    RecursiveLiveOutputMappingDecision,
    RecursiveLiveOutputRetryDecision,
) {
    let has_errors = issues.iter().any(is_error_issue);
    if !has_errors {
        let mapping = match output_kind {
            Some(RecursiveLiveOutputKind::Success) => RecursiveLiveOutputMappingDecision::Success,
            Some(RecursiveLiveOutputKind::Decomposition) => {
                RecursiveLiveOutputMappingDecision::Decomposition
            }
            Some(RecursiveLiveOutputKind::RetryableFailure) => {
                RecursiveLiveOutputMappingDecision::RetryFailure
            }
            Some(RecursiveLiveOutputKind::PermanentFailure) => {
                RecursiveLiveOutputMappingDecision::PermanentFailure
            }
            Some(RecursiveLiveOutputKind::Blocked) => RecursiveLiveOutputMappingDecision::Blocked,
            Some(RecursiveLiveOutputKind::Cancelled) => {
                RecursiveLiveOutputMappingDecision::Cancelled
            }
            None => RecursiveLiveOutputMappingDecision::NoOp,
        };
        let retry = match output_kind {
            Some(RecursiveLiveOutputKind::RetryableFailure) => RecursiveLiveOutputRetryDecision {
                decision: RecursiveLiveOutputRetryDecisionKind::RetryTaskAttempt,
                reason: Some("retryable_failure output was accepted".to_string()),
                remaining_repair_attempts: Some(
                    context.retry_policy.remaining_output_repair_attempts,
                ),
                remaining_task_retries: context.retry_policy.remaining_task_retries,
                suggested_next_action: Some(
                    "retry the recursive task attempt if policy allows".to_string(),
                ),
            },
            Some(RecursiveLiveOutputKind::PermanentFailure) => RecursiveLiveOutputRetryDecision {
                decision: RecursiveLiveOutputRetryDecisionKind::NoRetry,
                reason: Some("permanent_failure output was accepted".to_string()),
                remaining_repair_attempts: Some(
                    context.retry_policy.remaining_output_repair_attempts,
                ),
                remaining_task_retries: context.retry_policy.remaining_task_retries,
                suggested_next_action: None,
            },
            Some(RecursiveLiveOutputKind::Blocked) => RecursiveLiveOutputRetryDecision {
                decision: RecursiveLiveOutputRetryDecisionKind::OperatorReview,
                reason: Some("blocked output requires operator-visible resolution".to_string()),
                remaining_repair_attempts: Some(
                    context.retry_policy.remaining_output_repair_attempts,
                ),
                remaining_task_retries: context.retry_policy.remaining_task_retries,
                suggested_next_action: Some("surface requested operator input".to_string()),
            },
            _ => RecursiveLiveOutputRetryDecision {
                decision: RecursiveLiveOutputRetryDecisionKind::NotApplicable,
                reason: Some("accepted terminal output does not require output repair".to_string()),
                remaining_repair_attempts: Some(
                    context.retry_policy.remaining_output_repair_attempts,
                ),
                remaining_task_retries: context.retry_policy.remaining_task_retries,
                suggested_next_action: None,
            },
        };
        return (RecursiveLiveOutputValidationStatus::Valid, mapping, retry);
    }

    if issues.iter().any(is_operator_review_issue) {
        return (
            RecursiveLiveOutputValidationStatus::OperatorReviewRequired,
            RecursiveLiveOutputMappingDecision::OperatorReview,
            RecursiveLiveOutputRetryDecision {
                decision: RecursiveLiveOutputRetryDecisionKind::OperatorReview,
                reason: Some(
                    "validation found output claims that require operator review".to_string(),
                ),
                remaining_repair_attempts: Some(
                    context.retry_policy.remaining_output_repair_attempts,
                ),
                remaining_task_retries: context.retry_policy.remaining_task_retries,
                suggested_next_action: Some(
                    "preserve the live attempt context for inspection".to_string(),
                ),
            },
        );
    }

    if context.retry_policy.remaining_output_repair_attempts > 0
        && !issues.iter().any(is_non_repairable_issue)
    {
        return (
            RecursiveLiveOutputValidationStatus::Repairable,
            RecursiveLiveOutputMappingDecision::RepairSameLiveAttempt,
            RecursiveLiveOutputRetryDecision {
                decision: RecursiveLiveOutputRetryDecisionKind::RetrySameLiveAttempt,
                reason: Some("output repair budget remains".to_string()),
                remaining_repair_attempts: Some(
                    context.retry_policy.remaining_output_repair_attempts,
                ),
                remaining_task_retries: context.retry_policy.remaining_task_retries,
                suggested_next_action: Some(
                    "ask the linked session to emit corrected JSON".to_string(),
                ),
            },
        );
    }

    let ambiguous = issues.iter().any(is_ambiguous_issue);
    let retry_decision = if context
        .retry_policy
        .remaining_task_retries
        .is_some_and(|remaining| remaining > 0)
    {
        RecursiveLiveOutputRetryDecision {
            decision: RecursiveLiveOutputRetryDecisionKind::RetryTaskAttempt,
            reason: Some("invalid unrepaired output can be retried by task policy".to_string()),
            remaining_repair_attempts: Some(context.retry_policy.remaining_output_repair_attempts),
            remaining_task_retries: context.retry_policy.remaining_task_retries,
            suggested_next_action: Some("fail this attempt and retry the task".to_string()),
        }
    } else {
        RecursiveLiveOutputRetryDecision {
            decision: RecursiveLiveOutputRetryDecisionKind::NoRetry,
            reason: Some("validation failed with no output repair budget remaining".to_string()),
            remaining_repair_attempts: Some(context.retry_policy.remaining_output_repair_attempts),
            remaining_task_retries: context.retry_policy.remaining_task_retries,
            suggested_next_action: Some("fail the live output attempt".to_string()),
        }
    };

    (
        if ambiguous {
            RecursiveLiveOutputValidationStatus::Ambiguous
        } else {
            RecursiveLiveOutputValidationStatus::Invalid
        },
        RecursiveLiveOutputMappingDecision::FailAttempt,
        retry_decision,
    )
}

fn is_error_issue(issue: &RecursiveLiveOutputValidationIssue) -> bool {
    issue.severity == RecursiveLiveValidationIssueSeverity::Error
}

const fn is_operator_review_issue(issue: &RecursiveLiveOutputValidationIssue) -> bool {
    matches!(
        issue.code,
        RecursiveLiveValidationIssueCode::UnsafeToolClaim
            | RecursiveLiveValidationIssueCode::UnsafeSandboxClaim
    )
}

const fn is_ambiguous_issue(issue: &RecursiveLiveOutputValidationIssue) -> bool {
    matches!(
        issue.code,
        RecursiveLiveValidationIssueCode::AmbiguousTerminalKind
            | RecursiveLiveValidationIssueCode::SuccessWithFailedRequiredTest
            | RecursiveLiveValidationIssueCode::SuccessWithUnmetAcceptance
            | RecursiveLiveValidationIssueCode::IncoherentTestCounts
            | RecursiveLiveValidationIssueCode::CancelWithoutRequest
    )
}

const fn is_non_repairable_issue(issue: &RecursiveLiveOutputValidationIssue) -> bool {
    matches!(
        issue.code,
        RecursiveLiveValidationIssueCode::CancelWithoutRequest
    )
}

fn sort_and_deduplicate_issues(issues: &mut Vec<RecursiveLiveOutputValidationIssue>) {
    issues.sort_by_key(issue_sort_key);
    issues.dedup_by(|left, right| issue_sort_key(left) == issue_sort_key(right));
}

fn issue_sort_key(issue: &RecursiveLiveOutputValidationIssue) -> (u8, String, String, String) {
    (
        severity_rank(issue.severity),
        format!("{:?}", issue.code),
        location_sort_key(issue.location.as_ref()),
        issue.message.clone(),
    )
}

const fn severity_rank(severity: RecursiveLiveValidationIssueSeverity) -> u8 {
    match severity {
        RecursiveLiveValidationIssueSeverity::Error => 0,
        RecursiveLiveValidationIssueSeverity::Warning => 1,
        RecursiveLiveValidationIssueSeverity::Info => 2,
    }
}

fn location_sort_key(location: Option<&RecursiveLiveValidationIssueLocation>) -> String {
    match location {
        Some(RecursiveLiveValidationIssueLocation::OutputPath { path }) => {
            format!("output:{path}")
        }
        Some(RecursiveLiveValidationIssueLocation::Artifact { artifact_id, path }) => {
            format!("artifact:{artifact_id}:{}", path.as_deref().unwrap_or(""))
        }
        Some(RecursiveLiveValidationIssueLocation::ConversationEvent {
            event_id,
            sequence,
            path,
        }) => format!(
            "event:{event_id}:{}:{}",
            sequence.map_or_else(String::new, |sequence| sequence.to_string()),
            path.as_deref().unwrap_or("")
        ),
        Some(RecursiveLiveValidationIssueLocation::Task { task_id }) => {
            format!("task:{task_id}")
        }
        Some(RecursiveLiveValidationIssueLocation::Attempt { attempt_id }) => {
            format!("attempt:{attempt_id}")
        }
        Some(RecursiveLiveValidationIssueLocation::ChildLocal { local_id }) => {
            format!("child:{local_id}")
        }
        Some(RecursiveLiveValidationIssueLocation::Dependency { reference }) => {
            format!("dependency:{reference:?}")
        }
        Some(RecursiveLiveValidationIssueLocation::DiffPath { path }) => {
            format!("diff:{}", path.display())
        }
        Some(RecursiveLiveValidationIssueLocation::Other { description }) => {
            format!("other:{description}")
        }
        None => String::new(),
    }
}

fn issue(
    code: RecursiveLiveValidationIssueCode,
    severity: RecursiveLiveValidationIssueSeverity,
    class: RecursiveLiveValidationIssueClass,
    location: Option<RecursiveLiveValidationIssueLocation>,
    message: impl Into<String>,
) -> RecursiveLiveOutputValidationIssue {
    issue_with_metadata(code, severity, class, location, message, Value::Null)
}

fn issue_with_metadata(
    code: RecursiveLiveValidationIssueCode,
    severity: RecursiveLiveValidationIssueSeverity,
    class: RecursiveLiveValidationIssueClass,
    location: Option<RecursiveLiveValidationIssueLocation>,
    message: impl Into<String>,
    metadata: Value,
) -> RecursiveLiveOutputValidationIssue {
    RecursiveLiveOutputValidationIssue {
        code,
        severity,
        class,
        location,
        message: message.into(),
        evidence: Vec::new(),
        suggested_next_action: None,
        metadata,
    }
}

#[allow(clippy::unnecessary_wraps)]
fn output_path(path: impl Into<String>) -> Option<RecursiveLiveValidationIssueLocation> {
    Some(RecursiveLiveValidationIssueLocation::OutputPath { path: path.into() })
}

fn raw_output_kind(value: &Value) -> Option<RecursiveLiveOutputKind> {
    value
        .get("kind")
        .and_then(Value::as_str)
        .and_then(output_kind_from_str)
}

fn output_kind_from_str(value: &str) -> Option<RecursiveLiveOutputKind> {
    match value {
        "success" => Some(RecursiveLiveOutputKind::Success),
        "decomposition" => Some(RecursiveLiveOutputKind::Decomposition),
        "retryable_failure" => Some(RecursiveLiveOutputKind::RetryableFailure),
        "permanent_failure" => Some(RecursiveLiveOutputKind::PermanentFailure),
        "blocked" => Some(RecursiveLiveOutputKind::Blocked),
        "cancelled" => Some(RecursiveLiveOutputKind::Cancelled),
        _ => None,
    }
}

fn known_top_level_fields(output_kind: Option<RecursiveLiveOutputKind>) -> Vec<&'static str> {
    let mut fields = vec![
        "schema_version",
        "kind",
        "correlation",
        "summary",
        "artifacts",
        "tests",
        "diffs",
        "dependency_outputs",
        "notes",
        "metadata",
        "confidence",
    ];
    match output_kind {
        Some(RecursiveLiveOutputKind::Success) => fields.extend(["result_summary", "acceptance"]),
        Some(RecursiveLiveOutputKind::Decomposition) => fields.extend([
            "reason",
            "children",
            "dependency_edges",
            "integration_strategy",
            "verification_strategy",
            "budget_hints",
            "scope_hints",
        ]),
        Some(RecursiveLiveOutputKind::RetryableFailure) => fields.extend([
            "reason",
            "retry_hint",
            "operator_message",
            "evidence",
            "partial_artifacts",
            "suggested_next_action",
        ]),
        Some(RecursiveLiveOutputKind::PermanentFailure) => fields.extend([
            "reason",
            "failure_class",
            "operator_message",
            "evidence",
            "suggested_next_action",
        ]),
        Some(RecursiveLiveOutputKind::Blocked) => fields.extend([
            "reason",
            "requested_operator_input",
            "blocked_on",
            "safe_to_retry_without_input",
            "operator_message",
            "evidence",
            "suggested_next_action",
        ]),
        Some(RecursiveLiveOutputKind::Cancelled) => fields.extend([
            "reason",
            "cancellation_request_id",
            "observed_scope",
            "operator_message",
            "evidence",
            "suggested_next_action",
        ]),
        None => {}
    }
    fields
}

fn artifact_fields() -> Vec<&'static str> {
    vec![
        "label",
        "kind",
        "task_id",
        "attempt_id",
        "uri",
        "content_digest",
        "description",
        "existing_artifact_id",
        "metadata",
    ]
}

fn child_node_keys(children: &[RecursiveLiveChildTaskSpec]) -> BTreeMap<String, String> {
    children
        .iter()
        .map(|child| {
            let key = child
                .task_id
                .map_or_else(|| child_key(&child.local_id), task_key);
            (child.local_id.clone(), key)
        })
        .collect()
}

fn dependency_key(
    reference: &RecursiveLiveTaskDependencyRef,
    child_keys: &BTreeMap<String, String>,
    context: &RecursiveLiveOutputValidationContext<'_>,
) -> Result<String, String> {
    match reference {
        RecursiveLiveTaskDependencyRef::ChildLocal { local_id } => child_keys
            .get(local_id)
            .cloned()
            .ok_or_else(|| format!("child dependency local_id `{local_id}` is not defined")),
        RecursiveLiveTaskDependencyRef::ExistingTask { task_id } => {
            if !context
                .allowed_dependency_outputs
                .iter()
                .any(|allowed| allowed.task_id == *task_id)
            {
                Err(format!(
                    "existing dependency task {task_id} is not in the allowed dependency context"
                ))
            } else if is_descendant_of(context.graph, context.parent_task.id, *task_id) {
                Err(format!(
                    "existing dependency task {task_id} is a descendant of the selected parent task"
                ))
            } else {
                Ok(task_key(*task_id))
            }
        }
    }
}

fn existing_candidate_edges(
    context: &RecursiveLiveOutputValidationContext<'_>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut edges = BTreeMap::<String, BTreeSet<String>>::new();
    let Some(graph) = context.graph else {
        return edges;
    };
    for edge in &graph.edges {
        if matches!(
            edge.kind,
            RecursiveTaskEdgeKind::Dependency | RecursiveTaskEdgeKind::ParentChild
        ) {
            edges
                .entry(task_key(edge.from_task_id))
                .or_default()
                .insert(task_key(edge.to_task_id));
        }
    }
    edges
}

fn has_cycle(edges: &BTreeMap<String, BTreeSet<String>>) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit(
        node: &str,
        edges: &BTreeMap<String, BTreeSet<String>>,
        marks: &mut BTreeMap<String, Mark>,
    ) -> bool {
        if marks.get(node) == Some(&Mark::Visiting) {
            return true;
        }
        if marks.get(node) == Some(&Mark::Done) {
            return false;
        }
        marks.insert(node.to_string(), Mark::Visiting);
        if let Some(children) = edges.get(node) {
            for child in children {
                if visit(child, edges, marks) {
                    return true;
                }
            }
        }
        marks.insert(node.to_string(), Mark::Done);
        false
    }

    let mut marks = BTreeMap::new();
    for node in edges.keys() {
        if visit(node, edges, &mut marks) {
            return true;
        }
    }
    false
}

fn count_descendants(graph: &RecursiveTaskGraphDetail, parent_task_id: RecursiveTaskId) -> u32 {
    let mut children = BTreeMap::<RecursiveTaskId, Vec<RecursiveTaskId>>::new();
    for edge in &graph.edges {
        if edge.kind == RecursiveTaskEdgeKind::ParentChild {
            children
                .entry(edge.from_task_id)
                .or_default()
                .push(edge.to_task_id);
        }
    }

    let mut count = 0u32;
    let mut stack = children.get(&parent_task_id).cloned().unwrap_or_default();
    while let Some(task_id) = stack.pop() {
        count = count.saturating_add(1);
        if let Some(grandchildren) = children.get(&task_id) {
            stack.extend(grandchildren);
        }
    }
    count
}

fn descendant_limit_subjects(
    context: &RecursiveLiveOutputValidationContext<'_>,
    parent_task_id: RecursiveTaskId,
) -> Vec<(RecursiveTaskId, u32)> {
    let Some(graph) = context.graph else {
        return vec![(
            parent_task_id,
            context
                .decomposition_limits
                .existing_descendant_count
                .unwrap_or(0),
        )];
    };

    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let mut subjects = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = Some(parent_task_id);
    while let Some(task_id) = current {
        if !seen.insert(task_id) {
            break;
        }
        subjects.push((task_id, count_descendants(graph, task_id)));
        current = nodes.get(&task_id).and_then(|node| node.parent_task_id);
    }

    if subjects.is_empty() {
        vec![(
            parent_task_id,
            context
                .decomposition_limits
                .existing_descendant_count
                .unwrap_or(0),
        )]
    } else {
        subjects
    }
}

fn is_descendant_of(
    graph: Option<&RecursiveTaskGraphDetail>,
    ancestor_id: RecursiveTaskId,
    candidate_id: RecursiveTaskId,
) -> bool {
    if candidate_id == ancestor_id {
        return false;
    }
    let Some(graph) = graph else {
        return false;
    };
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut current = Some(candidate_id);
    while let Some(task_id) = current {
        if !seen.insert(task_id) {
            return false;
        }
        if task_id == ancestor_id {
            return true;
        }
        current = nodes.get(&task_id).and_then(|node| node.parent_task_id);
    }
    false
}

fn known_task_ids(context: &RecursiveLiveOutputValidationContext<'_>) -> BTreeSet<RecursiveTaskId> {
    let mut ids = BTreeSet::new();
    ids.insert(context.parent_task.id);
    ids.extend(
        context
            .allowed_dependency_outputs
            .iter()
            .map(|dependency| dependency.task_id),
    );
    if let Some(graph) = context.graph {
        ids.extend(graph.nodes.iter().map(|node| node.id));
    }
    ids
}

fn task_key(task_id: RecursiveTaskId) -> String {
    format!("task:{task_id}")
}

fn child_key(local_id: &str) -> String {
    format!("child:{local_id}")
}

fn is_external_artifact(reference: &RecursiveLiveOutputArtifactReference) -> bool {
    reference
        .metadata
        .get("external")
        .and_then(Value::as_bool)
        .is_some_and(|external| external)
        || reference
            .metadata
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(|source| source == "external")
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn deterministic_normalized_digest(output: &RecursiveLiveOutputEnvelope) -> Option<String> {
    let bytes = serde_json::to_vec(output).ok()?;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Some(format!("fnv1a64:{hash:016x}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::recursive_dag::{
        RecursiveLiveBlockedOn, RecursiveLiveCancelledPayload, RecursiveLiveDependencyEdgeSpec,
        RecursiveLivePermanentFailureClass, RecursiveLivePermanentFailurePayload,
        RecursiveLiveRetryableFailurePayload, RecursiveLiveTaskDependencyRef, RecursiveTaskGraphId,
    };
    use chrono::{DateTime, Utc};

    struct Fixture {
        parent: RecursiveTaskNode,
        attempt: RecursiveTaskAttempt,
        live_attempt_id: RecursiveLiveAttemptId,
        scheduler_run_id: RecursiveSchedulerRunId,
        session_id: Uuid,
        dependency_task_id: RecursiveTaskId,
        dependency_artifact_id: i64,
        allowed_artifact: RecursiveLiveAllowedArtifactReference,
        cancellation_request_id: RecursiveCancellationRequestId,
    }

    impl Fixture {
        fn new() -> Self {
            let graph_id = RecursiveTaskGraphId(uuid(1));
            let task_id = RecursiveTaskId(uuid(2));
            let attempt_id = RecursiveAttemptId(uuid(3));
            let dependency_task_id = RecursiveTaskId(uuid(4));
            let live_attempt_id = RecursiveLiveAttemptId(uuid(5));
            let scheduler_run_id = RecursiveSchedulerRunId(uuid(6));
            let cancellation_request_id = RecursiveCancellationRequestId(uuid(7));
            let session_id = uuid(8);
            let now = fixed_time();

            Self {
                parent: RecursiveTaskNode {
                    id: task_id,
                    graph_id,
                    parent_task_id: None,
                    title: "Parent task".to_string(),
                    objective: "Complete the selected parent task".to_string(),
                    scope: "Parent scope".to_string(),
                    acceptance_criteria: vec!["criterion a".to_string()],
                    depth: 0,
                    scope_units: 10,
                    max_retries: 2,
                    status: RecursiveTaskLifecycleState::Running,
                    decomposed_once: false,
                    integration_strategy: None,
                    verification_strategy: None,
                    blocked_reason: None,
                    created_at: now,
                    updated_at: now,
                },
                attempt: RecursiveTaskAttempt {
                    id: attempt_id,
                    graph_id,
                    task_id,
                    phase: RecursiveAttemptPhase::Execute,
                    attempt_no: 1,
                    retry_count: 0,
                    status: crate::recursive_dag::RecursiveAttemptStatus::Running,
                    started_at: now,
                    finished_at: None,
                    failure_reason: None,
                    block_reason: None,
                    dependency_snapshot: Vec::new(),
                    executor_kind: crate::recursive_dag::RecursiveExecutionMode::LiveSession,
                    session_id: Some(session_id),
                    workflow_execution_id: None,
                },
                live_attempt_id,
                scheduler_run_id,
                session_id,
                dependency_task_id,
                dependency_artifact_id: 41,
                allowed_artifact: RecursiveLiveAllowedArtifactReference {
                    artifact_id: 42,
                    graph_id,
                    task_id,
                    attempt_id: Some(attempt_id),
                    kind: RecursiveExecutionArtifactKind::File,
                    uri: Some("file:///work/root/result.txt".to_string()),
                },
                cancellation_request_id,
            }
        }

        fn context(&self) -> RecursiveLiveOutputValidationContext<'_> {
            let mut context = RecursiveLiveOutputValidationContext::new(
                &self.parent,
                &self.attempt,
                self.live_attempt_id,
                self.scheduler_run_id,
                Some(self.session_id),
            );
            context.allowed_artifacts = vec![self.allowed_artifact.clone()];
            context.allowed_dependency_outputs = vec![RecursiveLiveAllowedDependencyOutput {
                task_id: self.dependency_task_id,
                status: RecursiveTaskLifecycleState::Succeeded,
                artifact_ids: vec![self.dependency_artifact_id],
            }];
            context.decomposition_limits = RecursiveLiveDecompositionValidationLimits {
                max_fanout: Some(4),
                max_depth: Some(3),
                max_descendants: Some(8),
                max_child_retries: Some(2),
                existing_descendant_count: Some(0),
            };
            context
        }
    }

    fn uuid(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-26T00:00:00Z")
            .expect("fixed timestamp")
            .with_timezone(&Utc)
    }

    fn output_with(
        fixture: &Fixture,
        outcome: RecursiveLiveOutputOutcome,
    ) -> RecursiveLiveOutputEnvelope {
        RecursiveLiveOutputEnvelope {
            schema_version: RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION,
            correlation: crate::recursive_dag::RecursiveLiveOutputCorrelation {
                graph_id: fixture.parent.graph_id,
                task_id: fixture.parent.id,
                attempt_id: fixture.attempt.id,
                live_attempt_id: fixture.live_attempt_id,
                scheduler_run_id: fixture.scheduler_run_id,
                session_id: Some(fixture.session_id),
            },
            summary: "terminal output summary".to_string(),
            artifacts: Vec::new(),
            tests: Vec::new(),
            diffs: Vec::new(),
            dependency_outputs: Vec::new(),
            notes: Vec::new(),
            metadata: Value::Null,
            confidence: None,
            outcome,
        }
    }

    fn success_output(fixture: &Fixture) -> RecursiveLiveOutputEnvelope {
        let mut output = output_with(
            fixture,
            RecursiveLiveOutputOutcome::Success(RecursiveLiveSuccessPayload {
                result_summary: "implemented the parent task".to_string(),
                acceptance: vec![RecursiveLiveAcceptanceCheck {
                    criterion: "criterion a".to_string(),
                    status: RecursiveLiveAcceptanceStatus::Met,
                    evidence: vec!["verified by focused test".to_string()],
                    artifact_ids: Vec::new(),
                    notes: None,
                }],
            }),
        );
        output.tests = vec![passed_test()];
        output
    }

    fn passed_test() -> RecursiveLiveTestResultSummary {
        RecursiveLiveTestResultSummary {
            command: Some("cargo test -p rsi-common recursive_dag".to_string()),
            status: RecursiveLiveTestStatus::Passed,
            exit_code: Some(0),
            duration_ms: Some(100),
            output_artifact: None,
            required: true,
            reason: None,
            metadata: Value::Null,
        }
    }

    fn child(local_id: &str, scope_units: u32) -> RecursiveLiveChildTaskSpec {
        RecursiveLiveChildTaskSpec {
            local_id: local_id.to_string(),
            task_id: None,
            title: format!("Child {local_id}"),
            objective: format!("Objective for {local_id}"),
            scope: format!("Scope for {local_id}"),
            acceptance_criteria: vec![format!("{local_id} accepted")],
            scope_units,
            max_retries: 1,
            dependencies: Vec::new(),
            metadata: Value::Null,
        }
    }

    fn decomposition_output(fixture: &Fixture) -> RecursiveLiveOutputEnvelope {
        let mut child_b = child("child-b", 2);
        child_b
            .dependencies
            .push(RecursiveLiveTaskDependencyRef::ChildLocal {
                local_id: "child-a".to_string(),
            });
        output_with(
            fixture,
            RecursiveLiveOutputOutcome::Decomposition(RecursiveLiveDecompositionPayload {
                reason: "parent scope should be split".to_string(),
                children: vec![child("child-a", 2), child_b],
                dependency_edges: Vec::new(),
                integration_strategy: "integrate successful children".to_string(),
                verification_strategy: "run parent verification".to_string(),
                budget_hints: None,
                scope_hints: Vec::new(),
            }),
        )
    }

    fn assert_status(
        result: &RecursiveLiveOutputValidationResult,
        status: RecursiveLiveOutputValidationStatus,
        mapping: RecursiveLiveOutputMappingDecision,
    ) {
        assert_eq!(result.summary.status, status);
        assert_eq!(result.summary.mapping_decision, Some(mapping));
    }

    fn assert_issue(
        result: &RecursiveLiveOutputValidationResult,
        code: RecursiveLiveValidationIssueCode,
        path: &str,
    ) {
        assert!(
            result.issues.iter().any(|issue| {
                issue.code == code
                    && matches!(
                        &issue.location,
                        Some(RecursiveLiveValidationIssueLocation::OutputPath { path: issue_path })
                            if issue_path == path
                    )
            }),
            "expected issue {code:?} at {path}; got {:#?}",
            result.issues
        );
    }

    #[test]
    fn recursive_dag_live_validator_accepts_valid_success_output() {
        let fixture = Fixture::new();
        let output = success_output(&fixture);
        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::Success,
        );
        assert!(result.normalized_output.is_some());
        assert!(result.summary.normalized_digest.is_some());
        assert_eq!(result.summary.error_count, 0);
    }

    #[test]
    fn recursive_dag_live_validator_accepts_valid_decomposition_output() {
        let fixture = Fixture::new();
        let output = decomposition_output(&fixture);
        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::Decomposition,
        );
        assert!(result.normalized_output.is_some());
    }

    #[test]
    fn recursive_dag_live_validator_rejects_malformed_raw_json() {
        let fixture = Fixture::new();
        let result = validate_recursive_live_output_json("{not-json", &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Invalid,
            RecursiveLiveOutputMappingDecision::FailAttempt,
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::MalformedOutput,
            "/",
        );
    }

    #[test]
    fn recursive_dag_live_validator_enforces_raw_unknown_field_policy() {
        let fixture = Fixture::new();
        let mut raw = serde_json::to_value(success_output(&fixture)).expect("success value");
        raw.as_object_mut()
            .expect("object")
            .insert("surprise".to_string(), serde_json::json!(true));

        let raw_result = validate_recursive_live_output_value(&raw, &fixture.context());
        assert_status(
            &raw_result,
            RecursiveLiveOutputValidationStatus::Invalid,
            RecursiveLiveOutputMappingDecision::FailAttempt,
        );
        assert_issue(
            &raw_result,
            RecursiveLiveValidationIssueCode::UnknownField,
            "/surprise",
        );

        let typed: RecursiveLiveOutputEnvelope =
            serde_json::from_value(raw).expect("typed serde remains backward-compatible");
        let typed_result = validate_recursive_live_output(&typed, &fixture.context());
        assert_status(
            &typed_result,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::Success,
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_unknown_nested_raw_fields() {
        let fixture = Fixture::new();
        let mut raw = serde_json::to_value(success_output(&fixture)).expect("success value");
        raw["correlation"]["metadata"] = serde_json::json!({ "unexpected": true });

        let result = validate_recursive_live_output_value(&raw, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::UnknownField,
            "/correlation/metadata",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_conflicting_outcome_fields() {
        let fixture = Fixture::new();
        let mut context = fixture.context();
        context.raw_output_policy.unknown_fields = RecursiveLiveUnknownFieldPolicy::Allow;
        let mut raw = serde_json::to_value(success_output(&fixture)).expect("success value");
        raw.as_object_mut()
            .expect("object")
            .insert("reason".to_string(), serde_json::json!("also failed"));

        let result = validate_recursive_live_output_value(&raw, &context);

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Ambiguous,
            RecursiveLiveOutputMappingDecision::FailAttempt,
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::AmbiguousTerminalKind,
            "/reason",
        );
    }

    #[test]
    fn recursive_dag_live_validator_reports_missing_required_field() {
        let fixture = Fixture::new();
        let mut raw = serde_json::to_value(success_output(&fixture)).expect("success value");
        raw.as_object_mut()
            .expect("object")
            .remove("result_summary");

        let result = validate_recursive_live_output_value(&raw, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            "/result_summary",
        );
        assert_eq!(result.normalized_output, None);
    }

    #[test]
    fn recursive_dag_live_validator_rejects_empty_decomposition_children() {
        let fixture = Fixture::new();
        let output = output_with(
            &fixture,
            RecursiveLiveOutputOutcome::Decomposition(RecursiveLiveDecompositionPayload {
                reason: "split needed".to_string(),
                children: Vec::new(),
                dependency_edges: Vec::new(),
                integration_strategy: "integrate".to_string(),
                verification_strategy: "verify".to_string(),
                budget_hints: None,
                scope_hints: Vec::new(),
            }),
        );

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            "/children",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_child_scope_not_smaller_than_parent() {
        let fixture = Fixture::new();
        let mut output = decomposition_output(&fixture);
        let RecursiveLiveOutputOutcome::Decomposition(payload) = &mut output.outcome else {
            panic!("expected decomposition");
        };
        payload.children[0].scope_units = fixture.parent.scope_units;

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::ChildNotSmallerThanParent,
            "/children/0/scope_units",
        );
    }

    #[test]
    fn recursive_dag_live_validator_orders_multiple_decomposition_errors() {
        let fixture = Fixture::new();
        let mut output = decomposition_output(&fixture);
        let RecursiveLiveOutputOutcome::Decomposition(payload) = &mut output.outcome else {
            panic!("expected decomposition");
        };
        payload.children[0].scope_units = 0;
        payload.children[0].title.clear();
        payload.children.push(child("child-a", 2));

        let first = validate_recursive_live_output(&output, &fixture.context());
        let second = validate_recursive_live_output(&output, &fixture.context());
        let ordered = first
            .issues
            .iter()
            .map(|issue| (issue.code, location_sort_key(issue.location.as_ref())))
            .collect::<Vec<_>>();

        assert_eq!(first.issues, second.issues);
        assert_eq!(
            ordered,
            vec![
                (
                    RecursiveLiveValidationIssueCode::ChildNotSmallerThanParent,
                    "output:/children/0/scope_units".to_string(),
                ),
                (
                    RecursiveLiveValidationIssueCode::InvalidDecomposition,
                    "output:/children/2/local_id".to_string(),
                ),
                (
                    RecursiveLiveValidationIssueCode::MissingRequiredField,
                    "output:/children/0/title".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_duplicate_child_ids() {
        let fixture = Fixture::new();
        let mut output = decomposition_output(&fixture);
        let RecursiveLiveOutputOutcome::Decomposition(payload) = &mut output.outcome else {
            panic!("expected decomposition");
        };
        payload.children.push(child("child-a", 2));

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            "/children/2/local_id",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_dependency_cycle() {
        let fixture = Fixture::new();
        let mut output = decomposition_output(&fixture);
        let RecursiveLiveOutputOutcome::Decomposition(payload) = &mut output.outcome else {
            panic!("expected decomposition");
        };
        payload.children[0]
            .dependencies
            .push(RecursiveLiveTaskDependencyRef::ChildLocal {
                local_id: "child-b".to_string(),
            });

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::DependencyCycle,
            "/dependency_edges",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_three_child_dependency_cycle() {
        let fixture = Fixture::new();
        let mut output = decomposition_output(&fixture);
        let RecursiveLiveOutputOutcome::Decomposition(payload) = &mut output.outcome else {
            panic!("expected decomposition");
        };
        payload.children.push(child("child-c", 2));
        payload.children[2]
            .dependencies
            .push(RecursiveLiveTaskDependencyRef::ChildLocal {
                local_id: "child-b".to_string(),
            });
        payload.children[0]
            .dependencies
            .push(RecursiveLiveTaskDependencyRef::ChildLocal {
                local_id: "child-c".to_string(),
            });

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::DependencyCycle,
            "/dependency_edges",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_duplicate_dependency_edges() {
        let fixture = Fixture::new();
        let mut output = decomposition_output(&fixture);
        let RecursiveLiveOutputOutcome::Decomposition(payload) = &mut output.outcome else {
            panic!("expected decomposition");
        };
        payload
            .dependency_edges
            .push(RecursiveLiveDependencyEdgeSpec {
                from: RecursiveLiveTaskDependencyRef::ChildLocal {
                    local_id: "child-a".to_string(),
                },
                to: RecursiveLiveTaskDependencyRef::ChildLocal {
                    local_id: "child-b".to_string(),
                },
                rationale: None,
                metadata: Value::Null,
            });

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            "/dependency_edges/0",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_unknown_child_dependency_edge() {
        let fixture = Fixture::new();
        let mut output = decomposition_output(&fixture);
        let RecursiveLiveOutputOutcome::Decomposition(payload) = &mut output.outcome else {
            panic!("expected decomposition");
        };
        payload
            .dependency_edges
            .push(RecursiveLiveDependencyEdgeSpec {
                from: RecursiveLiveTaskDependencyRef::ChildLocal {
                    local_id: "missing-child".to_string(),
                },
                to: RecursiveLiveTaskDependencyRef::ChildLocal {
                    local_id: "child-a".to_string(),
                },
                rationale: None,
                metadata: Value::Null,
            });

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::UnknownDependency,
            "/dependency_edges/0",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_invalid_artifact_ref() {
        let fixture = Fixture::new();
        let mut output = success_output(&fixture);
        output.artifacts.push(RecursiveLiveOutputArtifactReference {
            label: "invented".to_string(),
            kind: RecursiveExecutionArtifactKind::File,
            task_id: Some(fixture.parent.id),
            attempt_id: Some(fixture.attempt.id),
            uri: Some("file:///work/root/result.txt".to_string()),
            content_digest: None,
            description: None,
            existing_artifact_id: Some(999),
            metadata: Value::Null,
        });

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::ArtifactMismatch,
            "/artifacts/0/existing_artifact_id",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_artifact_refs_in_wrong_context() {
        let fixture = Fixture::new();
        let mut context = fixture.context();
        context.allowed_artifacts[0].graph_id = RecursiveTaskGraphId(uuid(99));
        let mut output = success_output(&fixture);
        output.artifacts.push(RecursiveLiveOutputArtifactReference {
            label: "persisted from another graph".to_string(),
            kind: RecursiveExecutionArtifactKind::File,
            task_id: Some(fixture.parent.id),
            attempt_id: Some(fixture.attempt.id),
            uri: Some("file:///work/root/result.txt".to_string()),
            content_digest: None,
            description: None,
            existing_artifact_id: Some(fixture.allowed_artifact.artifact_id),
            metadata: Value::Null,
        });
        output.artifacts.push(RecursiveLiveOutputArtifactReference {
            label: "wrong task".to_string(),
            kind: RecursiveExecutionArtifactKind::Inline,
            task_id: Some(fixture.dependency_task_id),
            attempt_id: Some(fixture.attempt.id),
            uri: None,
            content_digest: None,
            description: None,
            existing_artifact_id: None,
            metadata: Value::Null,
        });

        let result = validate_recursive_live_output(&output, &context);

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::ArtifactMismatch,
            "/artifacts/0/existing_artifact_id",
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::ArtifactMismatch,
            "/artifacts/1/task_id",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_invalid_dependency_output_ref() {
        let fixture = Fixture::new();
        let mut output = success_output(&fixture);
        output
            .dependency_outputs
            .push(RecursiveLiveDependencyOutputReference {
                task_id: fixture.dependency_task_id,
                status: RecursiveTaskLifecycleState::Succeeded,
                artifact_ids: vec![999],
                summary: "used dependency".to_string(),
                metadata: Value::Null,
            });

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::ArtifactMismatch,
            "/dependency_outputs/0/artifact_ids/0",
        );
    }

    #[test]
    fn recursive_dag_live_validator_requires_trusted_diff_source() {
        let fixture = Fixture::new();
        let mut context = fixture.context();
        context.sandbox_policy.require_trusted_diff_source = true;
        context.sandbox_policy.trusted_diff_sources = vec!["daemon".to_string()];

        let mut output = success_output(&fixture);
        output.diffs.push(RecursiveLiveDiffSummary {
            worktree_root: Some(PathBuf::from("/work/root")),
            sandbox_root: None,
            files: Vec::new(),
            diff_artifact: None,
            clean_worktree: Some(false),
            metadata: serde_json::json!({ "source": "model" }),
        });

        let result = validate_recursive_live_output(&output, &context);

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::OperatorReviewRequired,
            RecursiveLiveOutputMappingDecision::OperatorReview,
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::UnsafeSandboxClaim,
            "/diffs/0/metadata/source",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_incoherent_test_counts() {
        let fixture = Fixture::new();
        let mut output = success_output(&fixture);
        output.tests[0].metadata =
            serde_json::json!({ "pass_count": 1, "fail_count": 1, "total_count": 1 });

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::IncoherentTestCounts,
            "/tests/0/metadata/fail_count",
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_success_with_failing_tests() {
        let fixture = Fixture::new();
        let mut output = success_output(&fixture);
        output.tests[0].status = RecursiveLiveTestStatus::Failed;
        output.tests[0].exit_code = Some(1);

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Ambiguous,
            RecursiveLiveOutputMappingDecision::FailAttempt,
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::SuccessWithFailedRequiredTest,
            "/tests/0/status",
        );
    }

    #[test]
    fn recursive_dag_live_validator_maps_retryable_failure() {
        let fixture = Fixture::new();
        let output = output_with(
            &fixture,
            RecursiveLiveOutputOutcome::RetryableFailure(RecursiveLiveRetryableFailurePayload {
                reason: "transient dependency outage".to_string(),
                retry_hint: "retry after dependency recovers".to_string(),
                operator_message: None,
                evidence: Vec::new(),
                partial_artifacts: Vec::new(),
                suggested_next_action: None,
            }),
        );

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::RetryFailure,
        );
        assert_eq!(
            result
                .summary
                .retry_decision
                .as_ref()
                .map(|retry| retry.decision),
            Some(RecursiveLiveOutputRetryDecisionKind::RetryTaskAttempt)
        );
    }

    #[test]
    fn recursive_dag_live_validator_maps_permanent_failure() {
        let fixture = Fixture::new();
        let output = output_with(
            &fixture,
            RecursiveLiveOutputOutcome::PermanentFailure(RecursiveLivePermanentFailurePayload {
                reason: "requirement is impossible".to_string(),
                failure_class: RecursiveLivePermanentFailureClass::ImpossibleRequirement,
                operator_message: None,
                evidence: Vec::new(),
                suggested_next_action: None,
            }),
        );

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::PermanentFailure,
        );
        assert_eq!(
            result
                .summary
                .retry_decision
                .as_ref()
                .map(|retry| retry.decision),
            Some(RecursiveLiveOutputRetryDecisionKind::NoRetry)
        );
    }

    #[test]
    fn recursive_dag_live_validator_maps_blocked() {
        let fixture = Fixture::new();
        let output = output_with(
            &fixture,
            RecursiveLiveOutputOutcome::Blocked(RecursiveLiveBlockedPayload {
                reason: "operator decision is required".to_string(),
                requested_operator_input: "Confirm the external service state".to_string(),
                blocked_on: RecursiveLiveBlockedOn::ExternalService,
                safe_to_retry_without_input: false,
                operator_message: None,
                evidence: Vec::new(),
                suggested_next_action: None,
            }),
        );

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::Blocked,
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_blocked_with_empty_reason() {
        let fixture = Fixture::new();
        let output = output_with(
            &fixture,
            RecursiveLiveOutputOutcome::Blocked(RecursiveLiveBlockedPayload {
                reason: " ".to_string(),
                requested_operator_input: "Confirm the external service state".to_string(),
                blocked_on: RecursiveLiveBlockedOn::ExternalService,
                safe_to_retry_without_input: false,
                operator_message: None,
                evidence: Vec::new(),
                suggested_next_action: None,
            }),
        );

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            "/reason",
        );
    }

    #[test]
    fn recursive_dag_live_validator_maps_cancelled_with_durable_evidence() {
        let fixture = Fixture::new();
        let mut context = fixture.context();
        context
            .cancellation_context
            .open_or_observed_request_ids
            .push(fixture.cancellation_request_id);
        let output = output_with(
            &fixture,
            RecursiveLiveOutputOutcome::Cancelled(RecursiveLiveCancelledPayload {
                reason: "graph cancellation observed".to_string(),
                cancellation_request_id: Some(fixture.cancellation_request_id),
                observed_scope: crate::recursive_dag::RecursiveCancellationScope::Graph,
                operator_message: None,
                evidence: Vec::new(),
                suggested_next_action: None,
            }),
        );

        let result = validate_recursive_live_output(&output, &context);

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::Cancelled,
        );
    }

    #[test]
    fn recursive_dag_live_validator_rejects_cancelled_without_evidence() {
        let fixture = Fixture::new();
        let output = output_with(
            &fixture,
            RecursiveLiveOutputOutcome::Cancelled(RecursiveLiveCancelledPayload {
                reason: "model stopped itself".to_string(),
                cancellation_request_id: None,
                observed_scope: crate::recursive_dag::RecursiveCancellationScope::Graph,
                operator_message: None,
                evidence: Vec::new(),
                suggested_next_action: None,
            }),
        );

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_status(
            &result,
            RecursiveLiveOutputValidationStatus::Ambiguous,
            RecursiveLiveOutputMappingDecision::FailAttempt,
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::CancelWithoutRequest,
            "/cancellation_request_id",
        );
    }

    #[test]
    fn recursive_dag_live_validator_reports_multiple_precise_locations() {
        let fixture = Fixture::new();
        let mut output = success_output(&fixture);
        let RecursiveLiveOutputOutcome::Success(payload) = &mut output.outcome else {
            panic!("expected success");
        };
        payload.result_summary.clear();
        payload.acceptance[0].status = RecursiveLiveAcceptanceStatus::Unmet;
        output.tests[0].status = RecursiveLiveTestStatus::Failed;
        output.tests[0].exit_code = Some(1);

        let result = validate_recursive_live_output(&output, &fixture.context());

        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            "/result_summary",
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::SuccessWithUnmetAcceptance,
            "/acceptance/0/status",
        );
        assert_issue(
            &result,
            RecursiveLiveValidationIssueCode::SuccessWithFailedRequiredTest,
            "/tests/0/status",
        );
    }

    #[test]
    fn recursive_dag_live_validator_orders_results_deterministically() {
        let fixture = Fixture::new();
        let mut raw = serde_json::to_value(success_output(&fixture)).expect("success value");
        raw.as_object_mut()
            .expect("object")
            .insert("z_unknown".to_string(), serde_json::json!(true));
        raw.as_object_mut()
            .expect("object")
            .insert("a_unknown".to_string(), serde_json::json!(true));

        let first = validate_recursive_live_output_value(&raw, &fixture.context());
        let second = validate_recursive_live_output_value(&raw, &fixture.context());
        let first_locations = first
            .issues
            .iter()
            .map(|issue| location_sort_key(issue.location.as_ref()))
            .collect::<Vec<_>>();
        let second_locations = second
            .issues
            .iter()
            .map(|issue| location_sort_key(issue.location.as_ref()))
            .collect::<Vec<_>>();

        assert_eq!(first.issues, second.issues);
        assert_eq!(first_locations, second_locations);
        assert_eq!(
            first_locations,
            vec!["output:/a_unknown", "output:/z_unknown"]
        );
    }

    #[test]
    fn recursive_dag_live_validator_digest_is_stable_for_equivalent_object_ordering() {
        let fixture = Fixture::new();
        let mut first_raw = serde_json::to_value(success_output(&fixture)).expect("success value");
        first_raw["metadata"] = serde_json::from_str(r#"{"b":2,"a":1}"#).expect("metadata");
        first_raw["tests"][0]["metadata"] =
            serde_json::from_str(r#"{"total_count":1,"pass_count":1,"b":2,"a":1}"#)
                .expect("test metadata");

        let mut second_raw = serde_json::to_value(success_output(&fixture)).expect("success value");
        second_raw["metadata"] = serde_json::from_str(r#"{"a":1,"b":2}"#).expect("metadata");
        second_raw["tests"][0]["metadata"] =
            serde_json::from_str(r#"{"a":1,"b":2,"pass_count":1,"total_count":1}"#)
                .expect("test metadata");

        let first = validate_recursive_live_output_value(&first_raw, &fixture.context());
        let second = validate_recursive_live_output_value(&second_raw, &fixture.context());

        assert_status(
            &first,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::Success,
        );
        assert_status(
            &second,
            RecursiveLiveOutputValidationStatus::Valid,
            RecursiveLiveOutputMappingDecision::Success,
        );
        assert_eq!(
            first.summary.normalized_digest,
            second.summary.normalized_digest
        );
    }
}
