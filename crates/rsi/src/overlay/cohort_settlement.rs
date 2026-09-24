//! Operator source-worktree settlement overlay input and RPC flow.

use crate::app::App;
use crate::types::{OverlayState, SourceWorktreeSettlementOverlayState};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::cohort_settlement::{
    ApplySourceWorktreeCohortParams, SourceWorktreeCohortAuditV1, SourceWorktreeSettlementRunV1,
};

pub async fn open_source_worktree_settlement(app: &mut App) {
    match app.client.list_source_worktree_cohorts().await {
        Ok(cohorts) => {
            app.overlay =
                OverlayState::SourceWorktreeSettlement(SourceWorktreeSettlementOverlayState {
                    cohorts,
                    selected_index: 0,
                    scroll_offset: 0,
                    audit: None,
                    receipt: None,
                    authorization_input: String::new(),
                    authorization_active: false,
                    idempotency_key: None,
                    last_error: None,
                });
        }
        Err(error) => app.notify_error(format!("Settlement cohort load failed: {error}")),
    }
}

pub(super) async fn handle_source_worktree_settlement_key(app: &mut App, key: KeyEvent) {
    let authorization_active = matches!(
        &app.overlay,
        OverlayState::SourceWorktreeSettlement(state) if state.authorization_active
    );
    if authorization_active {
        handle_authorization_key(app, key).await;
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
                let max = state.cohorts.len().saturating_sub(1);
                if state.selected_index < max {
                    state.selected_index += 1;
                    invalidate_audit(state);
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay
                && state.selected_index > 0
            {
                state.selected_index -= 1;
                invalidate_audit(state);
            }
        }
        KeyCode::PageDown | KeyCode::Char('J') => {
            if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
                state.scroll_offset = state.scroll_offset.saturating_add(8);
            }
        }
        KeyCode::PageUp | KeyCode::Char('K') => {
            if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
                state.scroll_offset = state.scroll_offset.saturating_sub(8);
            }
        }
        KeyCode::Enter => audit_selected(app).await,
        KeyCode::Char('A') => begin_authorization(app),
        KeyCode::Char('r') => refresh_receipt(app).await,
        _ => {}
    }
}

fn invalidate_audit(state: &mut SourceWorktreeSettlementOverlayState) {
    state.audit = None;
    state.receipt = None;
    state.authorization_input.clear();
    state.authorization_active = false;
    state.idempotency_key = None;
    state.last_error = None;
    state.scroll_offset = 0;
}

async fn audit_selected(app: &mut App) {
    let selected = match &app.overlay {
        OverlayState::SourceWorktreeSettlement(state) => {
            state.cohorts.get(state.selected_index).map(|cohort| {
                (
                    cohort.repository_identity.clone(),
                    cohort.canonical_repo_dir.clone(),
                )
            })
        }
        _ => None,
    };
    let Some((identity, canonical_repo_dir)) = selected else {
        if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
            state.last_error = Some("No daemon-discovered repository cohort".into());
        }
        return;
    };
    let result = app
        .client
        .audit_source_worktree_cohort(identity.clone())
        .await;
    let result = match result {
        Ok(audit) => match validate_selected_audit(&identity, &canonical_repo_dir, &audit) {
            Ok(()) => {
                let receipt = match audit.run_id {
                    Some(run_id) => app
                        .client
                        .get_source_worktree_settlement_run(run_id)
                        .await
                        .map_err(|error| error.to_string()),
                    None => Ok(None),
                };
                Ok((audit, receipt))
            }
            Err(error) => Err(error.to_string()),
        },
        Err(error) => Err(error.to_string()),
    };
    if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
        match result {
            Ok((audit, receipt)) => install_audit(state, audit, receipt),
            Err(error) => state.last_error = Some(format!("Audit failed: {error}")),
        }
    }
}

fn validate_selected_audit(
    expected_repository_identity: &str,
    expected_canonical_repo_dir: &str,
    audit: &SourceWorktreeCohortAuditV1,
) -> Result<(), &'static str> {
    if audit.repository_identity != expected_repository_identity {
        return Err("audit repository identity does not match the selected cohort");
    }
    if audit.canonical_repo_dir != expected_canonical_repo_dir {
        return Err("audit canonical repository path does not match the selected cohort");
    }
    Ok(())
}

fn install_audit(
    state: &mut SourceWorktreeSettlementOverlayState,
    audit: SourceWorktreeCohortAuditV1,
    receipt: Result<Option<SourceWorktreeSettlementRunV1>, String>,
) {
    let expected_receipt = audit.run_id.map(|run_id| {
        (
            run_id,
            audit.repository_identity.clone(),
            audit.canonical_repo_dir.clone(),
        )
    });
    state.audit = Some(audit);
    state.receipt = None;
    state.authorization_input.clear();
    state.authorization_active = false;
    state.idempotency_key = None;
    state.last_error = None;
    state.scroll_offset = 0;
    match receipt {
        Ok(Some(receipt)) => {
            let result = expected_receipt
                .as_ref()
                .ok_or("no durable run was selected")
                .and_then(|(run_id, repository_identity, canonical_repo_dir)| {
                    install_matching_receipt(
                        state,
                        *run_id,
                        repository_identity,
                        canonical_repo_dir,
                        receipt,
                    )
                });
            if let Err(error) = result {
                state.last_error = Some(format!("Receipt recovery rejected: {error}"));
            }
        }
        Ok(None) => {
            if let Some((run_id, _, _)) = expected_receipt {
                state.last_error = Some(format!("Durable receipt {run_id} is missing"));
            }
        }
        Err(error) => state.last_error = Some(format!("Receipt recovery failed: {error}")),
    }
}

fn install_matching_receipt(
    state: &mut SourceWorktreeSettlementOverlayState,
    expected_run_id: uuid::Uuid,
    expected_repository_identity: &str,
    expected_canonical_repo_dir: &str,
    receipt: SourceWorktreeSettlementRunV1,
) -> Result<(), &'static str> {
    validate_matching_receipt(
        expected_run_id,
        expected_repository_identity,
        expected_canonical_repo_dir,
        &receipt,
    )?;
    state.receipt = Some(receipt);
    Ok(())
}

fn begin_authorization(app: &mut App) {
    if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
        let selected_identity = state
            .cohorts
            .get(state.selected_index)
            .map(|cohort| cohort.repository_identity.as_str());
        let enabled = state.audit.as_ref().is_some_and(|audit| {
            audit.applyable
                && audit.authorization_phrase.is_some()
                && selected_identity == Some(audit.repository_identity.as_str())
        });
        if !enabled {
            state.last_error = Some("Run a fresh applyable audit before Apply".into());
            return;
        }
        // Never prefill or synthesize authorization text. The operator must
        // type the complete phrase displayed by the audited report.
        state.authorization_input.clear();
        state.authorization_active = true;
        state.idempotency_key = Some(format!("tui-{}", uuid::Uuid::new_v4()));
        state.last_error = None;
    }
}

async fn handle_authorization_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
                state.authorization_active = false;
                state.authorization_input.clear();
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
                state.authorization_input.pop();
            }
        }
        KeyCode::Enter => apply_exact_authorization(app).await,
        KeyCode::Char(character)
            if !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL) =>
        {
            if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay
                && state.authorization_input.len() < 8192
            {
                state.authorization_input.push(character);
            }
        }
        _ => {}
    }
}

async fn apply_exact_authorization(app: &mut App) {
    let request = match &app.overlay {
        OverlayState::SourceWorktreeSettlement(state) => {
            let Some(audit) = state.audit.as_ref() else {
                return;
            };
            let Some(expected) = audit.authorization_phrase.as_ref() else {
                return;
            };
            if &state.authorization_input != expected {
                None
            } else {
                Some((
                    ApplySourceWorktreeCohortParams {
                        repository_identity: audit.repository_identity.clone(),
                        plan_digest: audit.plan_digest.clone(),
                        authorization: state.authorization_input.clone(),
                        idempotency_key: state
                            .idempotency_key
                            .clone()
                            .expect("authorization mode owns an idempotency key"),
                    },
                    audit.repository_identity.clone(),
                    audit.canonical_repo_dir.clone(),
                ))
            }
        }
        _ => return,
    };
    let Some((params, expected_repository_identity, expected_canonical_repo_dir)) = request else {
        if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
            state.last_error = Some("Authorization phrase does not match exactly".into());
        }
        return;
    };

    let apply_result = app.client.apply_source_worktree_cohort(params).await;
    let receipt_result = match apply_result {
        Ok(receipt) => {
            let run_id = receipt.run_id;
            match validate_matching_receipt(
                run_id,
                &expected_repository_identity,
                &expected_canonical_repo_dir,
                &receipt,
            ) {
                Err(error) => Err(format!("Apply response rejected: {error}")),
                Ok(()) => match app.client.get_source_worktree_settlement_run(run_id).await {
                    Ok(Some(readback)) => validate_matching_receipt(
                        run_id,
                        &expected_repository_identity,
                        &expected_canonical_repo_dir,
                        &readback,
                    )
                    .map(|()| (run_id, readback))
                    .map_err(|error| format!("Apply readback rejected: {error}")),
                    Ok(None) => Err(format!("Durable receipt {run_id} is missing")),
                    Err(error) => Err(error.to_string()),
                },
            }
        }
        Err(error) => Err(error.to_string()),
    };
    let cohorts_result = if receipt_result.is_ok() {
        Some(app.client.list_source_worktree_cohorts().await)
    } else {
        None
    };
    if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
        match receipt_result {
            Ok((run_id, receipt)) => {
                if let Err(error) = install_matching_receipt(
                    state,
                    run_id,
                    &expected_repository_identity,
                    &expected_canonical_repo_dir,
                    receipt,
                ) {
                    state.last_error = Some(format!("Apply readback rejected: {error}"));
                    return;
                }
                state.authorization_active = false;
                state.authorization_input.clear();
                state.audit = None;
                state.last_error = None;
                if let Some(cohorts_result) = cohorts_result {
                    match cohorts_result {
                        Ok(cohorts) => {
                            state.cohorts = cohorts;
                            state.selected_index = state
                                .selected_index
                                .min(state.cohorts.len().saturating_sub(1));
                        }
                        Err(error) => {
                            state.last_error = Some(format!(
                                "Receipt is durable; cohort refresh failed: {error}"
                            ));
                        }
                    }
                }
            }
            Err(error) => state.last_error = Some(format!("Apply/readback failed: {error}")),
        }
    }
}

async fn refresh_receipt(app: &mut App) {
    let expected = match &app.overlay {
        OverlayState::SourceWorktreeSettlement(state) => receipt_identity(state),
        _ => None,
    };
    let Some((run_id, repository_identity, canonical_repo_dir)) = expected else {
        return;
    };
    let result = app.client.get_source_worktree_settlement_run(run_id).await;
    if let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay {
        match result {
            Ok(Some(receipt)) => match install_matching_receipt(
                state,
                run_id,
                &repository_identity,
                &canonical_repo_dir,
                receipt,
            ) {
                Ok(()) => state.last_error = None,
                Err(error) => {
                    state.last_error = Some(format!("Receipt refresh rejected: {error}"));
                }
            },
            Ok(None) => state.last_error = Some("Durable receipt is missing".into()),
            Err(error) => state.last_error = Some(format!("Receipt refresh failed: {error}")),
        }
    }
}

fn receipt_identity(
    state: &SourceWorktreeSettlementOverlayState,
) -> Option<(uuid::Uuid, String, String)> {
    state
        .receipt
        .as_ref()
        .map(|receipt| {
            (
                receipt.run_id,
                receipt.repository_identity.clone(),
                receipt.canonical_repo_dir.clone(),
            )
        })
        .or_else(|| {
            state.audit.as_ref().and_then(|audit| {
                audit.run_id.map(|run_id| {
                    (
                        run_id,
                        audit.repository_identity.clone(),
                        audit.canonical_repo_dir.clone(),
                    )
                })
            })
        })
}

fn validate_matching_receipt(
    expected_run_id: uuid::Uuid,
    expected_repository_identity: &str,
    expected_canonical_repo_dir: &str,
    receipt: &SourceWorktreeSettlementRunV1,
) -> Result<(), &'static str> {
    if receipt.run_id != expected_run_id {
        return Err("run ID does not match the requested durable run");
    }
    if receipt.repository_identity != expected_repository_identity {
        return Err("repository identity does not match the selected cohort");
    }
    if receipt.canonical_repo_dir != expected_canonical_repo_dir {
        return Err("canonical repository path does not match the selected cohort");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::cohort_settlement::{
        SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST,
        SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST,
        SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION, SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        SourceWorktreeAuditItemV1, SourceWorktreeCohortAuditV1, SourceWorktreeCohortSummaryV1,
        SourceWorktreeDispositionV1, SourceWorktreeGitOidV1, SourceWorktreeProofV1,
        SourceWorktreeSettlementCountsV1, SourceWorktreeSettlementItemV1,
        SourceWorktreeSettlementPhaseV1, SourceWorktreeSettlementRunStateV1,
    };
    use rsi_common::types::Sha256Digest;

    fn settled_receipt(
        run_id: uuid::Uuid,
        identity: String,
        digest: Sha256Digest,
    ) -> SourceWorktreeSettlementRunV1 {
        SourceWorktreeSettlementRunV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            run_id,
            repository_identity: identity,
            canonical_repo_dir: "/repo".into(),
            target_ref: "refs/heads/main".into(),
            target_oid: SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap(),
            plan_digest: digest,
            idempotency_key: "tui-lost-ack".into(),
            state: SourceWorktreeSettlementRunStateV1::Settled,
            counts: SourceWorktreeSettlementCountsV1 {
                observed: 1,
                eligible: 1,
                settled: 1,
                ..Default::default()
            },
            items: vec![SourceWorktreeSettlementItemV1 {
                sequence: 0,
                session_id: uuid::Uuid::new_v4(),
                custody_id: uuid::Uuid::new_v4(),
                source_ref: "refs/heads/rsi/lost-ack".into(),
                expected_source_oid: SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap(),
                phase: SourceWorktreeSettlementPhaseV1::Settled,
                refusal_code: None,
                before_observation: None,
                after_observation: Some("settled".into()),
            }],
            created_at: "2026-08-24T00:00:00.000000000Z".into(),
            updated_at: "2026-08-24T00:00:01.000000000Z".into(),
            finished_at: Some("2026-08-24T00:00:01.000000000Z".into()),
            terminal_error: None,
        }
        .validate_wire()
        .expect("durable receipt")
    }

    fn live_audit_with_historical_run(
        run_id: uuid::Uuid,
        identity: String,
        digest: Sha256Digest,
    ) -> SourceWorktreeCohortAuditV1 {
        SourceWorktreeCohortAuditV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: identity.clone(),
            canonical_repo_dir: "/repo".into(),
            target_ref: Some("refs/heads/main".into()),
            target_oid: Some(SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap()),
            plan_digest: digest.clone(),
            authorization_phrase: Some(format!("APPLY {identity} {digest}")),
            writes: 0,
            applyable: true,
            counts: SourceWorktreeSettlementCountsV1 {
                observed: 1,
                eligible: 1,
                ..Default::default()
            },
            items: vec![SourceWorktreeAuditItemV1 {
                session_id: uuid::Uuid::new_v4(),
                status: "Completed".into(),
                updated_at: "2026-08-24T00:00:02.000000000Z".into(),
                custody_id: uuid::Uuid::new_v4(),
                custody_generation: 1,
                scheduled_dependency_count: 0,
                scheduled_dependency_digest: Sha256Digest::parse(
                    SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST,
                )
                .unwrap(),
                session_path_dependency_count: 0,
                session_path_dependency_digest: Sha256Digest::parse(
                    SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST,
                )
                .unwrap(),
                sandbox_root: "/sandboxes/live".into(),
                source_ref: "refs/heads/rsi/live".into(),
                source_oid: Some(SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap()),
                clean_state_digest: Some(digest.clone()),
                proof: SourceWorktreeProofV1::IntegratedAncestor,
                evidence_digest: digest,
                disposition: SourceWorktreeDispositionV1::EligibleIntegratedAncestor,
                diagnostic: None,
            }],
            run_id: Some(run_id),
            refusal: None,
        }
        .validate_wire()
        .expect("fresh live audit with historical receipt")
    }

    #[test]
    fn selection_change_invalidates_authority_and_phrase_is_never_prefilled() {
        let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        let mut state = SourceWorktreeSettlementOverlayState {
            cohorts: Vec::new(),
            selected_index: 0,
            scroll_offset: 9,
            audit: Some(SourceWorktreeCohortAuditV1 {
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: "/repo/.git".into(),
                canonical_repo_dir: "/repo".into(),
                target_ref: None,
                target_oid: None,
                plan_digest: digest,
                authorization_phrase: None,
                writes: 0,
                applyable: false,
                counts: SourceWorktreeSettlementCountsV1::default(),
                items: Vec::new(),
                run_id: None,
                refusal: Some("no action".into()),
            }),
            receipt: None,
            authorization_input: "must disappear".into(),
            authorization_active: true,
            idempotency_key: Some("key".into()),
            last_error: Some("error".into()),
        };
        invalidate_audit(&mut state);
        assert!(state.audit.is_none());
        assert!(state.authorization_input.is_empty());
        assert!(!state.authorization_active);
        assert!(state.idempotency_key.is_none());
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn selected_audit_must_match_the_captured_repository_identity_and_path() {
        let run_id = uuid::Uuid::new_v4();
        let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        let mut audit = live_audit_with_historical_run(run_id, "/repo/.git".into(), digest);
        assert_eq!(
            validate_selected_audit("/repo/.git", "/repo", &audit),
            Ok(())
        );

        audit.repository_identity = "/other/.git".into();
        assert_eq!(
            validate_selected_audit("/repo/.git", "/repo", &audit),
            Err("audit repository identity does not match the selected cohort")
        );
        audit.repository_identity = "/repo/.git".into();
        audit.canonical_repo_dir = "/other".into();
        assert_eq!(
            validate_selected_audit("/repo/.git", "/repo", &audit),
            Err("audit canonical repository path does not match the selected cohort")
        );
    }

    #[test]
    fn restarted_audit_recovers_latest_receipt_and_retains_refresh_identity() {
        let run_id = uuid::Uuid::new_v4();
        let identity = "/repo/.git".to_string();
        let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        let audit = SourceWorktreeCohortAuditV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: identity.clone(),
            canonical_repo_dir: "/repo".into(),
            target_ref: None,
            target_oid: None,
            plan_digest: digest.clone(),
            authorization_phrase: None,
            writes: 0,
            applyable: false,
            counts: SourceWorktreeSettlementCountsV1::default(),
            items: Vec::new(),
            run_id: Some(run_id),
            refusal: Some("no current eligible roots".into()),
        }
        .validate_wire()
        .expect("restart audit");
        let receipt = settled_receipt(run_id, identity, digest);
        let mut state = SourceWorktreeSettlementOverlayState {
            cohorts: Vec::new(),
            selected_index: 0,
            scroll_offset: 11,
            audit: None,
            receipt: None,
            authorization_input: "stale".into(),
            authorization_active: true,
            idempotency_key: Some("volatile".into()),
            last_error: Some("stale".into()),
        };

        install_audit(&mut state, audit, Ok(Some(receipt)));
        assert_eq!(
            state.receipt.as_ref().map(|value| value.run_id),
            Some(run_id)
        );
        assert_eq!(receipt_identity(&state).map(|value| value.0), Some(run_id));
        assert!(state.last_error.is_none());
        assert!(state.authorization_input.is_empty());
        assert!(state.idempotency_key.is_none());

        state.receipt = None;
        assert_eq!(receipt_identity(&state).map(|value| value.0), Some(run_id));
    }

    #[test]
    fn historical_receipt_does_not_block_a_fresh_live_apply() {
        crate::state::DevState::clear();
        let run_id = uuid::Uuid::new_v4();
        let identity = "/repo/.git".to_string();
        let digest = Sha256Digest::parse(format!("sha256:{}", "c".repeat(64))).unwrap();
        let receipt = settled_receipt(run_id, identity.clone(), digest.clone());
        let audit = live_audit_with_historical_run(run_id, identity.clone(), digest);
        let mut state = SourceWorktreeSettlementOverlayState {
            cohorts: vec![SourceWorktreeCohortSummaryV1 {
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: identity,
                canonical_repo_dir: "/repo".into(),
                live_roots: 1,
                terminal_roots: 1,
            }],
            selected_index: 0,
            scroll_offset: 0,
            audit: None,
            receipt: None,
            authorization_input: String::new(),
            authorization_active: false,
            idempotency_key: None,
            last_error: None,
        };
        install_audit(&mut state, audit, Ok(Some(receipt)));
        let mut app = App::new(crate::client::DaemonClient::new(std::path::PathBuf::from(
            "/tmp/test-settlement-historical-receipt.sock",
        )));
        app.overlay = OverlayState::SourceWorktreeSettlement(state);

        begin_authorization(&mut app);
        let OverlayState::SourceWorktreeSettlement(state) = &app.overlay else {
            panic!("settlement overlay");
        };
        assert!(state.audit.as_ref().is_some_and(|audit| audit.applyable));
        assert_eq!(
            state.receipt.as_ref().map(|receipt| receipt.run_id),
            Some(run_id)
        );
        assert!(state.authorization_active);
        assert!(state.authorization_input.is_empty());
        assert!(state.idempotency_key.is_some());
        assert!(state.last_error.is_none());
    }

    #[test]
    fn audit_and_refresh_share_exact_receipt_identity_validation() {
        let run_id = uuid::Uuid::new_v4();
        let identity = "/repo/.git".to_string();
        let digest = Sha256Digest::parse(format!("sha256:{}", "d".repeat(64))).unwrap();
        let audit = live_audit_with_historical_run(run_id, identity.clone(), digest.clone());
        let mut wrong_path = settled_receipt(run_id, identity.clone(), digest.clone());
        wrong_path.canonical_repo_dir = "/different-repo".into();
        let mut state = SourceWorktreeSettlementOverlayState {
            cohorts: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            audit: None,
            receipt: None,
            authorization_input: String::new(),
            authorization_active: false,
            idempotency_key: None,
            last_error: None,
        };

        install_audit(&mut state, audit, Ok(Some(wrong_path)));
        assert!(state.receipt.is_none());
        assert!(
            state
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("canonical repository path"))
        );

        let valid = settled_receipt(run_id, identity.clone(), digest);
        install_matching_receipt(&mut state, run_id, &identity, "/repo", valid.clone())
            .expect("matching audit receipt");
        let mut wrong_identity = valid;
        wrong_identity.repository_identity = "/other/.git".into();
        assert_eq!(
            install_matching_receipt(&mut state, run_id, &identity, "/repo", wrong_identity),
            Err("repository identity does not match the selected cohort")
        );
        assert_eq!(
            state
                .receipt
                .as_ref()
                .map(|receipt| receipt.repository_identity.as_str()),
            Some("/repo/.git")
        );
    }

    #[test]
    fn apply_readback_requires_the_new_run_and_audited_cohort_identity() {
        let historical_run_id = uuid::Uuid::new_v4();
        let new_run_id = uuid::Uuid::new_v4();
        let identity = "/repo/.git".to_string();
        let digest = Sha256Digest::parse(format!("sha256:{}", "e".repeat(64))).unwrap();
        let audit =
            live_audit_with_historical_run(historical_run_id, identity.clone(), digest.clone());
        let historical = settled_receipt(historical_run_id, identity.clone(), digest.clone());
        let mut state = SourceWorktreeSettlementOverlayState {
            cohorts: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            audit: None,
            receipt: None,
            authorization_input: String::new(),
            authorization_active: false,
            idempotency_key: None,
            last_error: None,
        };
        install_audit(&mut state, audit, Ok(Some(historical.clone())));

        assert_eq!(
            install_matching_receipt(&mut state, new_run_id, &identity, "/repo", historical,),
            Err("run ID does not match the requested durable run")
        );
        let mut wrong_path = settled_receipt(new_run_id, identity.clone(), digest.clone());
        wrong_path.canonical_repo_dir = "/other-repo".into();
        assert_eq!(
            install_matching_receipt(&mut state, new_run_id, &identity, "/repo", wrong_path,),
            Err("canonical repository path does not match the selected cohort")
        );
        assert_eq!(
            state.receipt.as_ref().map(|receipt| receipt.run_id),
            Some(historical_run_id)
        );
        assert!(
            state.audit.is_some(),
            "rejected readback retains fresh audit"
        );

        let valid = settled_receipt(new_run_id, identity.clone(), digest);
        install_matching_receipt(&mut state, new_run_id, &identity, "/repo", valid)
            .expect("matching new apply readback");
        assert_eq!(
            state.receipt.as_ref().map(|receipt| receipt.run_id),
            Some(new_run_id)
        );
    }

    #[tokio::test]
    async fn apply_requires_operator_typed_exact_phrase_before_any_rpc() {
        crate::state::DevState::clear();
        let identity = "/repo/.git".to_string();
        let digest = Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).unwrap();
        let phrase = format!("APPLY {identity} {digest}");
        let mut app = App::new(crate::client::DaemonClient::new(std::path::PathBuf::from(
            "/tmp/test-settlement-phrase.sock",
        )));
        app.overlay =
            OverlayState::SourceWorktreeSettlement(SourceWorktreeSettlementOverlayState {
                cohorts: vec![SourceWorktreeCohortSummaryV1 {
                    schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                    policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                    repository_identity: identity.clone(),
                    canonical_repo_dir: "/repo".into(),
                    live_roots: 1,
                    terminal_roots: 1,
                }],
                selected_index: 0,
                scroll_offset: 0,
                audit: Some(SourceWorktreeCohortAuditV1 {
                    schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                    policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                    repository_identity: identity,
                    canonical_repo_dir: "/repo".into(),
                    target_ref: Some("refs/heads/main".into()),
                    target_oid: Some(SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap()),
                    plan_digest: digest,
                    authorization_phrase: Some(phrase),
                    writes: 0,
                    applyable: true,
                    counts: SourceWorktreeSettlementCountsV1 {
                        observed: 1,
                        eligible: 1,
                        ..Default::default()
                    },
                    items: Vec::new(),
                    run_id: None,
                    refusal: None,
                }),
                receipt: None,
                authorization_input: "prefill is forbidden".into(),
                authorization_active: false,
                idempotency_key: None,
                last_error: None,
            });

        begin_authorization(&mut app);
        let OverlayState::SourceWorktreeSettlement(state) = &mut app.overlay else {
            panic!("settlement overlay");
        };
        assert!(state.authorization_active);
        assert!(state.authorization_input.is_empty());
        assert!(state.idempotency_key.is_some());
        state.authorization_input = "wrong phrase".into();

        apply_exact_authorization(&mut app).await;
        let OverlayState::SourceWorktreeSettlement(state) = &app.overlay else {
            panic!("settlement overlay");
        };
        assert_eq!(
            state.last_error.as_deref(),
            Some("Authorization phrase does not match exactly")
        );
        assert!(state.receipt.is_none());
    }
}
