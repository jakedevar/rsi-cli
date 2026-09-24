use super::*;
use rsi_common::types::SessionStatus;
use std::collections::{HashMap, HashSet};

impl Store {
    pub(crate) fn manager_v2_prepare_update(
        &self,
        caller: Uuid,
        request: &AgentManagerUpdateRequestV2,
    ) -> Result<LedgerObservationContext> {
        text(&request.idempotency_key, 128).map_err(refused)?;
        let authority = self.manager_v2_authorize(caller, &request.fence, None)?;
        let capability = if matches!(request.change, ManagerUpdateV2::Integration { .. }) {
            ManagerCapabilityV2::Integration
        } else {
            ManagerCapabilityV2::WorkPlan
        };
        if !authority.grant.policy.capabilities.contains(&capability) {
            return Err(refused("manager_v2_capability_denied"));
        }
        if matches!(request.change, ManagerUpdateV2::RequestReview { .. })
            && !authority
                .grant
                .policy
                .capabilities
                .contains(&ManagerCapabilityV2::SessionCreate)
        {
            return Err(refused("manager_v2_capability_denied"));
        }
        let key = match &request.change {
            ManagerUpdateV2::Work { key, epic_id, .. } => {
                if !authority.is_manager {
                    return Err(refused("manager_v2_manager_required"));
                }
                self.manager_v2_require_epic(&authority, *epic_id)?;
                if let Some(prior) = self.manager_v2_record(&authority.config, "work", key)? {
                    if prior.epic_id != Some(*epic_id) {
                        return Err(refused("manager_v2_work_identity_changed"));
                    }
                }
                None
            }
            ManagerUpdateV2::Stage { key, .. }
            | ManagerUpdateV2::Accept { key, .. }
            | ManagerUpdateV2::Integration { key, .. } => Some(key),
            ManagerUpdateV2::RequestReview { key, .. } => Some(key),
            ManagerUpdateV2::Dependency {
                key, prerequisite, ..
            } => {
                if !authority.is_manager {
                    return Err(refused("manager_v2_manager_required"));
                }
                let (_, p) = self.manager_v2_work(&authority.config, prerequisite)?;
                self.manager_v2_require_epic(&authority, p.epic_id)?;
                Some(key)
            }
            ManagerUpdateV2::Ownership { key, .. }
            | ManagerUpdateV2::Migration { key, .. }
            | ManagerUpdateV2::MigrationTransfer { key, .. }
            | ManagerUpdateV2::MigrationRelease { key, .. } => {
                if !authority.is_manager {
                    return Err(refused("manager_v2_manager_required"));
                }
                Some(key)
            }
            ManagerUpdateV2::Request {
                request_id,
                work_key,
                ..
            } => {
                if authority.is_manager {
                    return Err(refused("manager_v2_current_lead_required"));
                }
                let epic = self.manager_v2_request_target(&authority, *request_id)?;
                if let Some(key) = work_key {
                    if self.manager_v2_work(&authority.config, key)?.1.epic_id != epic {
                        return Err(refused("manager_v2_work_out_of_scope"));
                    }
                }
                None
            }
            ManagerUpdateV2::Decision {
                key,
                epic_id,
                request_id,
                work_key,
                ..
            } => {
                self.manager_v2_require_epic(&authority, *epic_id)?;
                if key.starts_with("accept:")
                    || key.starts_with("question:")
                    || key.starts_with("approval:")
                    || self
                        .manager_v2_record(&authority.config, "decision_target", key)?
                        .is_some()
                {
                    return Err(refused("manager_v2_reserved_decision_key"));
                }
                if self
                    .manager_v2_record(&authority.config, "decision", key)?
                    .is_some_and(|r| r.epic_id != Some(*epic_id))
                {
                    return Err(refused("manager_v2_decision_identity_changed"));
                }
                if let Some(id) = request_id {
                    if self.manager_v2_request_target(&authority, *id)? != *epic_id {
                        return Err(refused("manager_v2_request_out_of_scope"));
                    }
                }
                if let Some(key) = work_key {
                    if self.manager_v2_work(&authority.config, key)?.1.epic_id != *epic_id {
                        return Err(refused("manager_v2_work_out_of_scope"));
                    }
                }
                None
            }
            ManagerUpdateV2::Handoff { .. } => {
                if !authority.is_manager {
                    return Err(refused("manager_v2_manager_required"));
                }
                None
            }
        };
        let (work_version, work) = if let Some(key) = key {
            let (row, w) = self.manager_v2_work(&authority.config, key)?;
            self.manager_v2_require_epic(&authority, w.epic_id)?;
            (row.row_version, Some(w))
        } else {
            (0, None)
        };
        let source_id = match &request.change {
            ManagerUpdateV2::Stage {
                evidence: Some(e), ..
            }
            | ManagerUpdateV2::Integration {
                verification: Some(e),
                ..
            } => Some(e.source_session_id),
            ManagerUpdateV2::Integration {
                verification: None, ..
            } => Some(
                work.as_ref()
                    .and_then(|work| work.source_session_id)
                    .ok_or_else(|| refused("manager_v2_source_custody_required"))?,
            ),
            ManagerUpdateV2::Migration { .. } => {
                let w = work
                    .as_ref()
                    .ok_or_else(|| refused("manager_v2_work_missing"))?;
                self.manager_lead(authority.config.project_id, w.epic_id)
                    .ok()
                    .map(|s| s.id)
            }
            ManagerUpdateV2::RequestReview { .. } => Some(
                work.as_ref()
                    .and_then(|work| work.source_session_id)
                    .ok_or_else(|| refused("manager_review_author_missing"))?,
            ),
            _ => None,
        };
        let source_session = if let Some(id) = source_id {
            let w = work
                .as_ref()
                .ok_or_else(|| refused("manager_v2_work_missing"))?;
            if self.manager_v2_descendant_epic(&authority.config, id)? != w.epic_id {
                return Err(refused("manager_v2_evidence_out_of_scope"));
            }
            let source = self
                .get_session(id)?
                .ok_or_else(|| refused("manager_v2_session_unavailable"))?;
            if !rsi_common::is_leaf_kind(source.session_kind) {
                return Err(refused("manager_v2_evidence_source_unavailable"));
            }
            if matches!(request.change, ManagerUpdateV2::RequestReview { .. }) {
                // #599 S1: review observes the custody holder, which may be
                // the author's rotation tip. Other evidence keeps its rules.
                Some(self.manager_review_source_holder(&authority.config, w.epic_id, &source)?)
            } else {
                if source.status == SessionStatus::Deleted {
                    return Err(refused("manager_v2_evidence_source_unavailable"));
                }
                if source.status == SessionStatus::Archived {
                    // Only the immutable, exact-source DB review receipt permits
                    // integration after the original author rotates away. The
                    // observer uses the historical custody's repository identity,
                    // without depending on any provider session still being live.
                    let eligible = match (&request.change, work.as_ref()) {
                        (
                            ManagerUpdateV2::Integration {
                                source_commit,
                                verification: None,
                                ..
                            },
                            Some(work),
                        ) => {
                            work.source_session_id == Some(id)
                                && self.manager_review_enrolled(
                                    &authority.config,
                                    work,
                                    source_commit,
                                )?
                                && self
                                    .manager_v2_accepted_source(
                                        &authority.config,
                                        work,
                                        source_commit,
                                    )?
                                    .is_some()
                        }
                        _ => false,
                    };
                    if !eligible {
                        return Err(refused("manager_v2_evidence_source_unavailable"));
                    }
                }
                Some(source)
            }
        } else {
            None
        };
        Ok(LedgerObservationContext {
            authority,
            work,
            work_version,
            source_session,
        })
    }
    pub(crate) fn manager_v2_request_target(
        &self,
        a: &ManagerAuthorityV2,
        id: Uuid,
    ) -> Result<Uuid> {
        let raw:Option<String>=self.conn.query_row(&format!("SELECT epic_id FROM harness_manager_messages WHERE id=?1 AND {} AND project_id=?2 AND manager_session_id=?3 AND scope_version=?4", super::super::harness_manager::MANAGER_REQUEST_ROW),
            params![id.to_string(),a.config.project_id.to_string(),a.config.manager_session_id.to_string(),a.config.row_version],|r|r.get(0)).optional()?;
        let epic = raw.ok_or_else(|| refused("manager_v2_request_out_of_scope"))?;
        let epic =
            Uuid::parse_str(&epic).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
        self.manager_v2_require_epic(a, epic)?;
        if a.is_manager {
            // A manager may correlate an escalation with its undelivered request.
            // State transitions still require the current recipient lead below.
            return Ok(epic);
        }
        let lead = self.manager_lead(a.config.project_id, epic)?;
        // The immutable recipient is an audit identity. Inheritance requires
        // exactly the live, receipt-backed sender AND recipient checks used by
        // the v1 inbox/reply consumers, never merely continued_from or a title.
        if lead.id != a.caller || !self.manager_v2_operator_request_live(&a.config, epic, id)? {
            return Err(refused("manager_v2_request_lead_changed"));
        }
        Ok(epic)
    }
    pub(crate) fn manager_v2_commit_update(
        &self,
        caller: Uuid,
        request: &AgentManagerUpdateRequestV2,
        observed: &LedgerObservation,
    ) -> Result<ManagerMutationReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let context = self.manager_v2_prepare_update(caller, request)?;
        let a = &context.authority;
        let config = &a.config;
        let payload = json!({"actor":caller,"request":request});
        if let Some(receipt) = self.manager_v2_replay(config, &request.idempotency_key, &payload)? {
            tx.commit()?;
            return Ok(serde_json::from_value(receipt)?);
        }
        for (session, id, generation) in &observed.custody {
            let current = self.live_custody_for_session(*session)?;
            if current.custody_id != *id || current.generation != *generation {
                return Err(refused("manager_v2_evidence_custody_changed"));
            }
        }
        if let Some(proof) = &observed.evidence {
            if let Some(reviewer) = proof.reviewer_session_id {
                if self.manager_v2_descendant_epic(config, reviewer)?
                    != context
                        .work
                        .as_ref()
                        .ok_or_else(|| refused("manager_v2_work_missing"))?
                        .epic_id
                {
                    return Err(refused("manager_v2_evidence_out_of_scope"));
                }
            }
            if let (Some(reviewer), Some(invocation), Some(event)) = (
                proof.reviewer_session_id,
                proof.reviewer_invocation_id,
                proof.review_handoff_event_id,
            ) {
                let valid:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sessions s JOIN model_invocations mi ON mi.id=s.model_invocation_id AND mi.session_id=s.id WHERE s.id=?1 AND s.status='Completed' AND mi.id=?2 AND mi.status='completed' AND ?3=(SELECT e.id FROM conversation_events e JOIN conversation_event_provenance p ON p.conversation_event_id=e.id WHERE e.session_id=s.id AND p.model_invocation_id=mi.id AND p.producer_kind='provider_assistant_output' AND e.role='Assistant' AND e.event_type='Message' AND trim(e.content)<>'' ORDER BY e.sequence DESC,e.id DESC LIMIT 1))",params![reviewer.to_string(),invocation.to_string(),event],|r|r.get(0))?;
                if !valid {
                    return Err(refused("manager_v2_evidence_producer_changed"));
                }
            }
        }
        if matches!(request.change, ManagerUpdateV2::RequestReview { .. }) {
            let work = context
                .work
                .as_ref()
                .ok_or_else(|| refused("manager_v2_work_missing"))?;
            let receipt = self.manager_review_reserve_on(
                caller,
                config,
                request,
                work,
                context.work_version,
                observed,
            )?;
            self.manager_v2_save_receipt(
                config,
                Some(caller),
                a.grant.row_version,
                "ledger",
                &request.idempotency_key,
                &payload,
                &serde_json::to_value(&receipt)?,
            )?;
            tx.commit()?;
            return Ok(receipt);
        }
        let (kind, key, epic, expected, value) = match &request.change {
            ManagerUpdateV2::Work {
                key,
                expected_row_version,
                epic_id,
                title,
                kind,
                priority,
                weight,
                required_gates,
            } => {
                text(key, 128).map_err(refused)?;
                text(title, 512).map_err(refused)?;
                if *weight == 0
                    || *priority > 9
                    || required_gates.is_empty()
                    || required_gates.len() > 5
                    || !required_gates
                        .iter()
                        .any(|s| *s != ManagerWorkStageV2::Integration)
                    || required_gates
                        .iter()
                        .enumerate()
                        .any(|(i, s)| required_gates[..i].contains(s))
                {
                    return Err(refused("manager_v2_invalid_work"));
                }
                if *kind == ManagerWorkKindV2::Product
                    && [
                        ManagerWorkStageV2::Implementation,
                        ManagerWorkStageV2::Review,
                        ManagerWorkStageV2::Verification,
                    ]
                    .iter()
                    .any(|s| !required_gates.contains(s))
                {
                    return Err(refused("manager_v2_product_gates_required"));
                }
                let old = self.manager_v2_record(config, "work", key)?;
                if old.is_none()
                    && self.manager_v2_records(config, "work")?.len() >= MANAGER_V2_MAX_WORK
                {
                    return Err(refused("manager_v2_work_limit"));
                }
                let old_work = old.as_ref().map(decode::<WorkRecord>).transpose()?;
                let revision = old_work.as_ref().map_or(Ok(1), |w| {
                    w.spec_revision
                        .checked_add(1)
                        .ok_or_else(|| refused("manager_v2_version_exhausted"))
                })?;
                // A revised plan invalidates approval, but keeps committed partial work visible.
                let mut stages = old_work
                    .as_ref()
                    .map(|w| w.stages.clone())
                    .unwrap_or_else(|| {
                        STAGES
                            .into_iter()
                            .map(|stage| StageRecord {
                                stage,
                                state: ManagerStageStateV2::Unknown,
                                note: String::new(),
                                evidence: None,
                                admission: None,
                                updated_at: now(),
                            })
                            .collect()
                    });
                for stage in &mut stages {
                    stage.admission = None;
                    if stage.state == ManagerStageStateV2::Passed {
                        stage.state = ManagerStageStateV2::Partial;
                    }
                }
                let work = WorkRecord {
                    key: key.clone(),
                    epic_id: *epic_id,
                    title: title.clone(),
                    kind: *kind,
                    priority: *priority,
                    weight: *weight,
                    required_gates: required_gates.clone(),
                    spec_revision: revision,
                    source_session_id: old_work.as_ref().and_then(|w| w.source_session_id),
                    source_commit: old_work.and_then(|w| w.source_commit),
                    stages,
                    acceptance: None,
                    integration: None,
                    pending_acceptance: None,
                };
                (
                    "work",
                    key.clone(),
                    Some(*epic_id),
                    *expected_row_version,
                    serde_json::to_value(work)?,
                )
            }
            ManagerUpdateV2::Stage {
                key,
                expected_row_version,
                stage,
                state,
                note,
                evidence,
            } => {
                if note.len() > 2048 || note.contains('\0') {
                    return Err(refused("manager_v2_invalid_note"));
                }
                if *stage == ManagerWorkStageV2::Integration
                    && *state == ManagerStageStateV2::Passed
                {
                    return Err(refused("manager_v2_integration_receipt_required"));
                }
                let mut work = context
                    .work
                    .clone()
                    .ok_or_else(|| refused("manager_v2_work_missing"))?;
                if evidence.is_some() && observed.source_commit.is_none() {
                    return Err(refused("manager_v2_source_observation_required"));
                }
                if let Some(head) = &observed.source_commit {
                    if work.source_commit.as_ref() != Some(head)
                        || work.source_session_id != evidence.as_ref().map(|e| e.source_session_id)
                    {
                        work.acceptance = None;
                        work.integration = None;
                        for s in &mut work.stages {
                            s.admission = None;
                        }
                    }
                    work.source_commit = Some(head.clone());
                    work.source_session_id = evidence.as_ref().map(|e| e.source_session_id);
                }
                let s = work
                    .stages
                    .iter_mut()
                    .find(|s| s.stage == *stage)
                    .ok_or_else(|| refused("manager_v2_stage_missing"))?;
                *s = StageRecord {
                    stage: *stage,
                    state: *state,
                    note: note.clone(),
                    evidence: evidence.clone(),
                    admission: observed.evidence.clone(),
                    updated_at: now(),
                };
                work.acceptance = None;
                work.integration = None;
                work.pending_acceptance = None;
                (
                    "work",
                    key.clone(),
                    Some(work.epic_id),
                    *expected_row_version,
                    serde_json::to_value(work)?,
                )
            }
            ManagerUpdateV2::Dependency {
                key,
                expected_row_version,
                prerequisite,
                require_integrated,
                enabled,
            } => {
                let edge = Dependency {
                    work_key: key.clone(),
                    prerequisite: prerequisite.clone(),
                    require_integrated: *require_integrated,
                    enabled: *enabled,
                };
                if *enabled {
                    self.manager_v2_check_cycle(config, &edge)?;
                }
                (
                    "dependency",
                    record_key(&[key, prerequisite])?,
                    context.work.as_ref().map(|w| w.epic_id),
                    *expected_row_version,
                    serde_json::to_value(edge)?,
                )
            }
            ManagerUpdateV2::Ownership {
                key,
                expected_row_version,
                domain,
                mode,
                files,
                active,
            } => {
                text(domain, 256).map_err(refused)?;
                if files.len() > 64 || files.iter().any(|f| f.len() > 512 || f.contains('\0')) {
                    return Err(refused("manager_v2_invalid_files"));
                }
                // Domain exclusivity is project-wide: a narrowed scope must not hide a live claim.
                for r in self.manager_v2_facts(config, "ownership", FactReach::Project)? {
                    let c: Ownership = decode(&r)?;
                    if *active
                        && c.active
                        && c.domain == *domain
                        && c.work_key != *key
                        && (c.mode == ManagerOwnershipModeV2::Exclusive
                            || *mode == ManagerOwnershipModeV2::Exclusive)
                    {
                        return Err(refused("manager_v2_domain_conflict"));
                    }
                }
                let claim = Ownership {
                    work_key: key.clone(),
                    domain: domain.clone(),
                    mode: *mode,
                    files: files.clone(),
                    active: *active,
                };
                (
                    "ownership",
                    record_key(&[key, domain])?,
                    context.work.as_ref().map(|w| w.epic_id),
                    *expected_row_version,
                    serde_json::to_value(claim)?,
                )
            }
            ManagerUpdateV2::Migration {
                key,
                expected_row_version,
                version,
                baseline_commit,
                inventory_digest,
            } => {
                if *version <= super::super::LATEST_SCHEMA_VERSION as u32
                    || observed.migration.as_ref()
                        != Some(&(baseline_commit.clone(), inventory_digest.clone()))
                {
                    return Err(refused("manager_v2_migration_baseline_or_released"));
                }
                let k = version.to_string();
                if let Some(prior) = self.manager_v2_record(config, "migration", &k)? {
                    let reservation: Migration = decode(&prior)?;
                    if reservation.active && reservation.work_key != *key {
                        return Err(refused("manager_v2_migration_conflict"));
                    }
                }
                let m = Migration {
                    work_key: key.clone(),
                    version: *version,
                    baseline_commit: baseline_commit.clone(),
                    inventory_digest: inventory_digest.clone(),
                    active: true,
                };
                (
                    "migration",
                    k,
                    context.work.as_ref().map(|w| w.epic_id),
                    *expected_row_version,
                    serde_json::to_value(m)?,
                )
            }
            ManagerUpdateV2::MigrationTransfer {
                key,
                expected_row_version,
                version,
            }
            | ManagerUpdateV2::MigrationRelease {
                key,
                expected_row_version,
                version,
            } => {
                if *version <= super::super::LATEST_SCHEMA_VERSION as u32 {
                    return Err(refused("manager_v2_migration_baseline_or_released"));
                }
                let record_key = version.to_string();
                let prior = self
                    .manager_v2_record(config, "migration", &record_key)?
                    .ok_or_else(|| refused("manager_v2_migration_missing"))?;
                if prior.row_version != *expected_row_version {
                    return Err(refused("manager_v2_record_changed"));
                }
                let mut reservation: Migration = decode(&prior)?;
                if !reservation.active {
                    return Err(refused("manager_v2_migration_released"));
                }
                let transfer = matches!(request.change, ManagerUpdateV2::MigrationTransfer { .. });
                if transfer {
                    reservation.work_key = key.clone();
                } else if reservation.work_key == *key {
                    reservation.active = false;
                } else {
                    return Err(refused("manager_v2_migration_conflict"));
                }
                (
                    "migration",
                    record_key,
                    if transfer {
                        context.work.as_ref().map(|w| w.epic_id)
                    } else {
                        prior.epic_id
                    },
                    *expected_row_version,
                    serde_json::to_value(reservation)?,
                )
            }
            ManagerUpdateV2::RequestReview { .. } => {
                unreachable!("review reservations return before generic records")
            }
            ManagerUpdateV2::Accept {
                key,
                expected_row_version,
            } => {
                let mut w = context
                    .work
                    .clone()
                    .ok_or_else(|| refused("manager_v2_work_missing"))?;
                if !self.manager_v2_dependency_blockers(config, key)?.is_empty() {
                    return Err(refused("manager_v2_prerequisite_unaccepted"));
                }
                let source = w.source_commit.clone();
                let db_enrolled = source
                    .as_deref()
                    .map(|source| self.manager_review_enrolled(config, &w, source))
                    .transpose()?
                    .unwrap_or(false);
                if db_enrolled {
                    let acceptance = self
                        .manager_v2_accepted_source(
                            config,
                            &w,
                            source.as_deref().unwrap_or_default(),
                        )?
                        .ok_or_else(|| refused("manager_review_source_not_accepted"))?;
                    w.acceptance = Some(acceptance);
                    w.pending_acceptance = None;
                } else {
                    let admitted = w
                        .required_gates
                        .iter()
                        .filter(|s| **s != ManagerWorkStageV2::Integration)
                        .all(|stage| {
                            w.stages.iter().any(|s| {
                                s.stage == *stage
                                    && s.state == ManagerStageStateV2::Passed
                                    && s.admission.as_ref().is_some_and(|p| {
                                        Some(&p.source_commit) == source.as_ref()
                                            && policy_digest(&w, &p.source_commit)
                                                .is_ok_and(|d| d == p.policy_digest)
                                    })
                            })
                        });
                    if admitted && source.is_some() {
                        w.acceptance = Some(Acceptance {
                            source_commit: source.unwrap(),
                            spec_revision: w.spec_revision,
                            evidence_digest: fingerprint(&json!(w.stages))?,
                            method: "independent_evidence".into(),
                            accepted_at: now(),
                        });
                        w.pending_acceptance = None;
                    } else {
                        return Err(refused("manager_v2_independent_evidence_required"));
                    }
                }
                (
                    "work",
                    key.clone(),
                    Some(w.epic_id),
                    *expected_row_version,
                    serde_json::to_value(w)?,
                )
            }
            ManagerUpdateV2::Integration {
                key,
                expected_row_version,
                source_commit,
                target_commit,
                ..
            } => {
                if !self.manager_v2_dependency_blockers(config, key)?.is_empty() {
                    return Err(refused("manager_v2_prerequisite_unaccepted"));
                }
                let mut w = context
                    .work
                    .clone()
                    .ok_or_else(|| refused("manager_v2_work_missing"))?;
                let acceptance = self
                    .manager_v2_accepted_source(config, &w, source_commit)?
                    .ok_or_else(|| refused("manager_v2_integrated_evidence_required"))?;
                if observed.target_commit.as_ref() != Some(target_commit) {
                    return Err(refused("manager_v2_integrated_evidence_required"));
                }
                w.acceptance = Some(acceptance);
                let db_review = self.manager_review_enrolled(config, &w, source_commit)?;
                let proof = observed.evidence.clone();
                if db_review {
                    if proof.is_some() {
                        return Err(refused("manager_review_legacy_evidence_forbidden"));
                    }
                } else {
                    let proof = proof
                        .as_ref()
                        .ok_or_else(|| refused("manager_v2_integrated_evidence_required"))?;
                    if proof.source_commit != *target_commit
                        || proof.policy_digest != integration_policy_digest(&w, target_commit)?
                    {
                        return Err(refused("manager_v2_evidence_changed"));
                    }
                }
                w.integration = Some(Integration {
                    source_commit: source_commit.clone(),
                    target_commit: target_commit.clone(),
                    verification: proof.clone(),
                    integrated_at: now(),
                });
                let s = w
                    .stages
                    .iter_mut()
                    .find(|s| s.stage == ManagerWorkStageV2::Integration)
                    .unwrap();
                s.state = ManagerStageStateV2::Passed;
                s.admission = proof;
                s.updated_at = now();
                (
                    "work",
                    key.clone(),
                    Some(w.epic_id),
                    *expected_row_version,
                    serde_json::to_value(w)?,
                )
            }
            ManagerUpdateV2::Request {
                request_id,
                expected_row_version,
                state,
                message,
                work_key,
            } => {
                text(message, 2048).map_err(refused)?;
                let epic = self.manager_v2_request_target(a, *request_id)?;
                // #664: a settled request accepts no further lead lifecycle;
                // an exact replay was already answered above.
                if self
                    .manager_request_settlement(config, *request_id)?
                    .is_some()
                {
                    return Err(refused("manager_v2_request_settled"));
                }
                let key = request_id.to_string();
                let previous = self
                    .manager_v2_record(config, "request", &key)?
                    .map(|r| decode::<RequestRecord>(&r))
                    .transpose()?;
                let retrieved = self
                    .manager_v2_record(config, "retrieval", &format!("{request_id}:{caller}"))?
                    .is_some();
                if !retrieved {
                    return Err(refused("manager_v2_request_retrieval_required"));
                }
                let old = previous.map_or(
                    if retrieved {
                        ManagerRequestStateV2::Retrieved
                    } else {
                        ManagerRequestStateV2::Queued
                    },
                    |r| r.state,
                );
                if !request_transition(old, *state) {
                    return Err(refused("manager_v2_request_transition"));
                }
                // #664 permanent release: a lead-terminal state frees its
                // pending slot forever. A `failed -> accepted` reopen is a
                // lifecycle-only change: the request stays released, so it is
                // neither counted against the cap nor reported unanswered
                // (review 7c6df190). A pre-upgrade `failed` row carries no
                // marker yet, so it is released first; the open (= unanswered)
                // count then grows only by sends and never exceeds the cap.
                if is_terminal_request_state(*state) {
                    self.manager_v2_release_request(config, caller, epic, *request_id, *state)?;
                } else if old == ManagerRequestStateV2::Failed {
                    self.manager_v2_release_request(config, caller, epic, *request_id, old)?;
                }
                let execution_evidence = if *state == ManagerRequestStateV2::Completed {
                    if let Some(key) = work_key {
                        let (record, work) = self.manager_v2_work(config, key)?;
                        let source = work.source_commit.ok_or_else(|| {
                            refused("manager_v2_request_execution_evidence_required")
                        })?;
                        Some(
                            json!({"kind":"committed_source","work_key":key,"work_row_version":record.row_version,"source_commit":source}),
                        )
                    } else {
                        let reply:Option<String>=self.conn.query_row("SELECT id FROM harness_manager_messages WHERE request_id=?1 AND sender_session_id=?2 AND project_id=?3 AND manager_session_id=?4 AND scope_version=?5 ORDER BY sequence DESC LIMIT 1",params![request_id.to_string(),caller.to_string(),config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version],|r|r.get(0)).optional()?;
                        Some(
                            json!({"kind":"attributed_reply","message_id":reply.ok_or_else(||refused("manager_v2_request_execution_evidence_required"))?}),
                        )
                    }
                } else {
                    None
                };
                let r = RequestRecord {
                    request_id: *request_id,
                    state: *state,
                    message: message.clone(),
                    work_key: work_key.clone(),
                    lead_session_id: caller,
                    execution_evidence,
                    updated_at: now(),
                };
                (
                    "request",
                    key,
                    Some(epic),
                    *expected_row_version,
                    serde_json::to_value(r)?,
                )
            }
            ManagerUpdateV2::Decision {
                key,
                expected_row_version,
                epic_id,
                question,
                request_id,
                work_key,
            } => {
                text(key, 128).map_err(refused)?;
                text(question, 4096).map_err(refused)?;
                let target_version = expected_row_version
                    .checked_add(1)
                    .ok_or_else(|| refused("manager_v2_version_exhausted"))?;
                let work_version = work_key
                    .as_ref()
                    .map(|key| {
                        self.manager_v2_work(config, key)
                            .map(|(r, _)| r.row_version)
                    })
                    .transpose()?;
                let request_version = request_id
                    .map(|id| {
                        self.manager_v2_record(config, "request", &id.to_string())
                            .map(|r| r.map_or(0, |r| r.row_version))
                    })
                    .transpose()?;
                let d = DecisionRecord {
                    key: key.clone(),
                    epic_id: *epic_id,
                    question: question.clone(),
                    request_id: *request_id,
                    work_key: work_key.clone(),
                    target_digest: fingerprint(
                        &json!({"epic_id":epic_id,"question":question,"request_id":request_id,"work_key":work_key,"revision":target_version,"work_row_version":work_version,"request_row_version":request_version}),
                    )?,
                    target_row_version: work_version,
                    request_row_version: request_version,
                    status: "pending".into(),
                    answer: None,
                    delivery: None,
                };
                (
                    "decision",
                    key.clone(),
                    Some(*epic_id),
                    *expected_row_version,
                    serde_json::to_value(d)?,
                )
            }
            ManagerUpdateV2::Handoff {
                summary,
                next_actions,
            } => {
                text(summary, 4096).map_err(refused)?;
                if next_actions.len() > 32 || next_actions.iter().any(|s| text(s, 1024).is_err()) {
                    return Err(refused("manager_v2_handoff_limit"));
                }
                let expected = self
                    .manager_v2_record(config, "handoff", "current")?
                    .map_or(0, |r| r.row_version);
                (
                    "handoff",
                    "current".into(),
                    None,
                    expected,
                    json!({"summary":summary,"next_actions":next_actions}),
                )
            }
        };
        let row = self.manager_v2_put_record(config, kind, &key, epic, expected, &value)?;
        let sequence =
            self.manager_v2_event(config, Some(caller), kind, &key, row.row_version, &value)?;
        let receipt = ManagerMutationReceiptV2 {
            event_sequence: sequence,
            key,
            row_version: row.row_version,
            deduplicated: false,
        };
        self.manager_v2_save_receipt(
            config,
            Some(caller),
            a.grant.row_version,
            "ledger",
            &request.idempotency_key,
            &payload,
            &serde_json::to_value(&receipt)?,
        )?;
        tx.commit()?;
        Ok(receipt)
    }
    fn manager_v2_check_cycle(
        &self,
        config: &HarnessManagerConfigV1,
        edge: &Dependency,
    ) -> Result<()> {
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        for r in self.manager_v2_facts(config, "dependency", FactReach::Project)? {
            let d: Dependency = decode(&r)?;
            if d.enabled {
                edges.entry(d.work_key).or_default().push(d.prerequisite);
            }
        }
        let mut todo = vec![edge.prerequisite.clone()];
        let mut seen = HashSet::new();
        while let Some(at) = todo.pop() {
            if at == edge.work_key {
                return Err(refused("manager_v2_dependency_cycle"));
            }
            if seen.insert(at.clone()) {
                if seen.len() > MANAGER_V2_MAX_WORK {
                    return Err(refused("manager_v2_dependency_budget"));
                }
                todo.extend(edges.get(&at).into_iter().flatten().cloned());
            }
        }
        Ok(())
    }
    pub(crate) fn manager_v2_dependency_blockers(
        &self,
        config: &HarnessManagerConfigV1,
        key: &str,
    ) -> Result<Vec<String>> {
        // Prerequisites are judged project-wide: scope narrowing never satisfies
        // one. Edges come from live work only (a delivered work's own edges were
        // satisfied when it integrated); each prerequisite is read by key, so a
        // delivered prerequisite still resolves however much history exists.
        let mut works: HashMap<String, Option<WorkRecord>> = HashMap::new();
        let edges = self
            .manager_v2_facts(config, "dependency", FactReach::Project)?
            .iter()
            .map(decode::<Dependency>)
            .collect::<Result<Vec<_>>>()?;
        let mut blockers = Vec::new();
        let mut todo = vec![key.to_string()];
        let mut seen = HashSet::new();
        while let Some(at) = todo.pop() {
            if !seen.insert(at.clone()) {
                continue;
            }
            if seen.len() > MANAGER_V2_MAX_WORK {
                blockers.push("dependency_budget_exceeded".into());
                break;
            }
            for dependency in edges
                .iter()
                .filter(|edge| edge.enabled && edge.work_key == at)
            {
                if !works.contains_key(&dependency.prerequisite) {
                    let prerequisite = self
                        .manager_v2_fact(config.project_id, "work", &dependency.prerequisite)?
                        .map(|record| decode::<WorkRecord>(&record))
                        .transpose()?;
                    works.insert(dependency.prerequisite.clone(), prerequisite);
                }
                let prerequisite = works.get(&dependency.prerequisite).and_then(Option::as_ref);
                let accepted = if let Some(work) = prerequisite {
                    if let Some(source) = work.source_commit.as_deref() {
                        self.manager_v2_accepted_source(config, work, source)?
                            .is_some()
                    } else {
                        false
                    }
                } else {
                    false
                };
                let satisfied = accepted
                    && prerequisite.is_some_and(|work| {
                        !dependency.require_integrated
                            || work.integration.as_ref().is_some_and(|integration| {
                                Some(&integration.source_commit) == work.source_commit.as_ref()
                            })
                    });
                if !satisfied {
                    blockers.push(dependency.prerequisite.clone());
                }
                todo.push(dependency.prerequisite.clone());
            }
        }
        blockers.sort();
        blockers.dedup();
        Ok(blockers)
    }
}
#[cfg(test)]
pub(super) fn dependency_blockers(
    works: &std::collections::BTreeMap<String, WorkRecord>,
    edges: &[Dependency],
    key: &str,
) -> Vec<String> {
    let mut blockers = Vec::new();
    let mut todo = vec![key.to_string()];
    let mut seen = HashSet::new();
    while let Some(at) = todo.pop() {
        if !seen.insert(at.clone()) {
            continue;
        }
        if seen.len() > MANAGER_V2_MAX_WORK {
            blockers.push("dependency_budget_exceeded".into());
            break;
        }
        for d in edges.iter().filter(|d| d.enabled && d.work_key == at) {
            let satisfied = works.get(&d.prerequisite).is_some_and(|w| {
                w.acceptance.as_ref().is_some_and(|a| {
                    a.spec_revision == w.spec_revision
                        && Some(&a.source_commit) == w.source_commit.as_ref()
                }) && (!d.require_integrated
                    || w.integration
                        .as_ref()
                        .is_some_and(|i| Some(&i.source_commit) == w.source_commit.as_ref()))
            });
            if !satisfied {
                blockers.push(d.prerequisite.clone());
            }
            todo.push(d.prerequisite.clone());
        }
    }
    blockers.sort();
    blockers.dedup();
    blockers
}

const fn is_terminal_request_state(state: ManagerRequestStateV2) -> bool {
    use ManagerRequestStateV2::*;
    matches!(state, Completed | Failed | Declined)
}

impl Store {
    /// Append the permanent `request_released` capacity marker once (CAS 0).
    /// Idempotent: an existing marker is never rewritten.
    fn manager_v2_release_request(
        &self,
        config: &HarnessManagerConfigV1,
        actor: Uuid,
        epic: Uuid,
        request_id: Uuid,
        state: ManagerRequestStateV2,
    ) -> Result<()> {
        let kind = super::super::harness_manager::REQUEST_RELEASED_KIND;
        let key = request_id.to_string();
        if self.manager_v2_record(config, kind, &key)?.is_none() {
            self.manager_v2_put_record(
                config,
                kind,
                &key,
                Some(epic),
                0,
                &json!({"state": state, "actor": actor}),
            )?;
        }
        Ok(())
    }
}

fn request_transition(old: ManagerRequestStateV2, next: ManagerRequestStateV2) -> bool {
    use ManagerRequestStateV2::*;
    matches!(
        (old, next),
        (Retrieved, Accepted | Declined | Blocked)
            | (Accepted, Running | Blocked | Failed)
            | (Running, Completed | Failed | Blocked)
            | (Blocked, Accepted | Running | Declined)
            | (Failed, Accepted)
    )
}

impl Store {
    /// Parent operator answer path calls this inside its exact-decision transaction.
    /// This is not an agent surface; the decision must be the pending acceptance gate.
    pub(crate) fn manager_v2_apply_operator_acceptance(
        &self,
        config: &HarnessManagerConfigV1,
        decision_key: &str,
        expected_decision_version: i64,
        target_digest: &str,
    ) -> Result<ManagerMutationReceiptV2> {
        if self.conn.is_autocommit() {
            return Err(refused("manager_v2_operator_transaction_required"));
        }
        let decision = self
            .manager_v2_record(config, "decision", decision_key)?
            .ok_or_else(|| refused("manager_v2_decision_missing"))?;
        let d: DecisionRecord = decode(&decision)?;
        if decision.row_version != expected_decision_version
            || d.status != "pending"
            || d.target_digest != target_digest
        {
            return Err(refused("manager_v2_decision_changed"));
        }
        let key = d
            .work_key
            .as_ref()
            .ok_or_else(|| refused("manager_v2_acceptance_target_required"))?;
        let (record, mut w) = self.manager_v2_work(config, key)?;
        if d.target_row_version != Some(record.row_version)
            || w.pending_acceptance.as_deref() != Some(decision_key)
        {
            return Err(refused("manager_v2_acceptance_target_changed"));
        }
        let mut target = w.clone();
        target.pending_acceptance = None;
        if fingerprint(&json!({"work":target,"version":record.row_version}))? != target_digest {
            return Err(refused("manager_v2_acceptance_target_changed"));
        }
        if !self.manager_v2_dependency_blockers(config, key)?.is_empty() {
            return Err(refused("manager_v2_prerequisite_unaccepted"));
        }
        let source = w
            .source_commit
            .clone()
            .ok_or_else(|| refused("manager_v2_source_observation_required"))?;
        if self.manager_review_enrolled(config, &w, &source)? {
            return Err(refused("manager_review_authoritative"));
        }
        w.acceptance = Some(Acceptance {
            source_commit: source,
            spec_revision: w.spec_revision,
            evidence_digest: target_digest.into(),
            method: "operator_exact_gate".into(),
            accepted_at: now(),
        });
        w.pending_acceptance = None;
        let value = serde_json::to_value(&w)?;
        let row = self.manager_v2_put_record(
            config,
            "work",
            key,
            Some(w.epic_id),
            record.row_version,
            &value,
        )?;
        let event = self.manager_v2_event(config, None, "work", key, row.row_version, &value)?;
        Ok(ManagerMutationReceiptV2 {
            event_sequence: event,
            key: key.clone(),
            row_version: row.row_version,
            deduplicated: false,
        })
    }
}
