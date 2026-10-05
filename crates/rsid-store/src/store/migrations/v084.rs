impl Store {
    fn migrate_v084(&self, version: i32) -> Result<()> {
        if version < 84 {
            // K1 owns one complete additive Closure catalog. K2/K3 consume
            // these predeclared journals but their effect routes remain
            // intentionally absent until their own slices pass review.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 83 {
                return Err(DaemonError::Store(format!(
                    "V84 requires exact V83 source, found V{active_version}"
                )));
            }
            tx.execute_batch(
                "
                CREATE TABLE closure_programs (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    version INTEGER NOT NULL CHECK(version>0),
                    repository_root TEXT NOT NULL,
                    repository_identity TEXT NOT NULL,
                    base_ref TEXT NOT NULL CHECK(substr(base_ref,1,11)='refs/heads/' AND length(base_ref)>11),
                    base_sha TEXT NOT NULL CHECK(length(base_sha) IN (40,64) AND base_sha=lower(base_sha) AND base_sha NOT GLOB '*[^0-9a-f]*'),
                    destination_ref TEXT NOT NULL CHECK(substr(destination_ref,1,11)='refs/heads/' AND length(destination_ref)>11),
                    destination_pre_head TEXT NOT NULL CHECK(length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*'),
                    state TEXT NOT NULL CHECK(state IN ('draft','configured','working','outcome_blocked','awaiting_evidence','eligible','awaiting_integration','integrating','conflict','gate_failed','integrated','quarantined_cleanup_pending','cleanup_pending','cleanup_in_progress','cleanup_failed','closed','quarantined_closed')),
                    destination_claim_state TEXT NOT NULL CHECK(destination_claim_state IN ('held','released')),
                    review_policy_json TEXT NOT NULL,
                    review_policy_digest TEXT NOT NULL CHECK(length(review_policy_digest)=71 AND substr(review_policy_digest,1,7)='sha256:' AND substr(review_policy_digest,8)=lower(substr(review_policy_digest,8)) AND substr(review_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verification_policy_json TEXT NOT NULL,
                    verification_policy_digest TEXT NOT NULL CHECK(length(verification_policy_digest)=71 AND substr(verification_policy_digest,1,7)='sha256:' AND substr(verification_policy_digest,8)=lower(substr(verification_policy_digest,8)) AND substr(verification_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_json TEXT NOT NULL,
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    checkout_remediation_policy TEXT NOT NULL CHECK(checkout_remediation_policy IN ('refuse','managed_detach_reattach')),
                    creation_idempotency_key TEXT NOT NULL UNIQUE CHECK(length(creation_idempotency_key)=36 AND creation_idempotency_key=lower(creation_idempotency_key) AND substr(creation_idempotency_key,9,1)='-' AND substr(creation_idempotency_key,14,1)='-' AND substr(creation_idempotency_key,19,1)='-' AND substr(creation_idempotency_key,24,1)='-' AND length(replace(creation_idempotency_key,'-',''))=32 AND replace(creation_idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    creation_request_fingerprint TEXT NOT NULL CHECK(length(creation_request_fingerprint)=71 AND substr(creation_request_fingerprint,1,7)='sha256:' AND substr(creation_request_fingerprint,8)=lower(substr(creation_request_fingerprint,8)) AND substr(creation_request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE UNIQUE INDEX idx_closure_programs_held_destination
                    ON closure_programs(repository_identity,destination_ref)
                    WHERE destination_claim_state='held';
                CREATE INDEX idx_closure_programs_state_updated
                    ON closure_programs(state,updated_at,id);

                CREATE TABLE closure_source_launch_reservations (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    program_id TEXT NOT NULL UNIQUE CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    idempotency_key TEXT NOT NULL UNIQUE CHECK(length(idempotency_key)=36 AND idempotency_key=lower(idempotency_key) AND substr(idempotency_key,9,1)='-' AND substr(idempotency_key,14,1)='-' AND substr(idempotency_key,19,1)='-' AND substr(idempotency_key,24,1)='-' AND length(replace(idempotency_key,'-',''))=32 AND replace(idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8)=lower(substr(request_fingerprint,8)) AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    request_json TEXT NOT NULL,
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    root_session_id TEXT NOT NULL UNIQUE CHECK(length(root_session_id)=36 AND root_session_id=lower(root_session_id) AND substr(root_session_id,9,1)='-' AND substr(root_session_id,14,1)='-' AND substr(root_session_id,19,1)='-' AND substr(root_session_id,24,1)='-' AND length(replace(root_session_id,'-',''))=32 AND replace(root_session_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    custody_id TEXT NOT NULL UNIQUE CHECK(length(custody_id)=36 AND custody_id=lower(custody_id) AND substr(custody_id,9,1)='-' AND substr(custody_id,14,1)='-' AND substr(custody_id,19,1)='-' AND substr(custody_id,24,1)='-' AND length(replace(custody_id,'-',''))=32 AND replace(custody_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    custody_generation INTEGER NOT NULL CHECK(custody_generation=1),
                    branch_name TEXT NOT NULL UNIQUE,
                    repository_identity TEXT NOT NULL,
                    source_ref TEXT NOT NULL UNIQUE CHECK(substr(source_ref,1,11)='refs/heads/' AND length(source_ref)>11),
                    source_base_sha TEXT NOT NULL CHECK(length(source_base_sha) IN (40,64) AND source_base_sha=lower(source_base_sha) AND source_base_sha NOT GLOB '*[^0-9a-f]*'),
                    destination_ref TEXT NOT NULL CHECK(substr(destination_ref,1,11)='refs/heads/' AND length(destination_ref)>11),
                    destination_pre_head TEXT NOT NULL CHECK(length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*'),
                    staging_ref TEXT NOT NULL UNIQUE CHECK(substr(staging_ref,1,29)='refs/heads/rsi/closure-stage/' AND length(staging_ref)>29),
                    created_at TEXT NOT NULL,
                    completed_at TEXT
                );

                CREATE TABLE closure_sources (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    program_id TEXT NOT NULL UNIQUE CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    custody_id TEXT NOT NULL CHECK(length(custody_id)=36 AND custody_id=lower(custody_id) AND substr(custody_id,9,1)='-' AND substr(custody_id,14,1)='-' AND substr(custody_id,19,1)='-' AND substr(custody_id,24,1)='-' AND length(replace(custody_id,'-',''))=32 AND replace(custody_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
                    root_session_id TEXT NOT NULL UNIQUE CHECK(length(root_session_id)=36 AND root_session_id=lower(root_session_id) AND substr(root_session_id,9,1)='-' AND substr(root_session_id,14,1)='-' AND substr(root_session_id,19,1)='-' AND substr(root_session_id,24,1)='-' AND length(replace(root_session_id,'-',''))=32 AND replace(root_session_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sessions(id) ON DELETE RESTRICT,
                    source_ref TEXT NOT NULL UNIQUE CHECK(substr(source_ref,1,11)='refs/heads/' AND length(source_ref)>11),
                    source_base_sha TEXT NOT NULL CHECK(length(source_base_sha) IN (40,64) AND source_base_sha=lower(source_base_sha) AND source_base_sha NOT GLOB '*[^0-9a-f]*'),
                    source_head TEXT CHECK(source_head IS NULL OR (length(source_head) IN (40,64) AND source_head=lower(source_head) AND source_head NOT GLOB '*[^0-9a-f]*')),
                    destination_ref TEXT NOT NULL CHECK(substr(destination_ref,1,11)='refs/heads/' AND length(destination_ref)>11),
                    destination_pre_head TEXT NOT NULL CHECK(length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*'),
                    staging_ref TEXT NOT NULL UNIQUE CHECK(substr(staging_ref,1,29)='refs/heads/rsi/closure-stage/' AND length(staging_ref)>29),
                    state TEXT NOT NULL CHECK(state IN ('working','outcome_committed','outcome_no_change','outcome_blocker','outcome_blocked','awaiting_evidence','eligible','integration_queued','integrated','retained','cleanup_complete')),
                    launch_idempotency_key TEXT NOT NULL UNIQUE CHECK(length(launch_idempotency_key)=36 AND launch_idempotency_key=lower(launch_idempotency_key) AND substr(launch_idempotency_key,9,1)='-' AND substr(launch_idempotency_key,14,1)='-' AND substr(launch_idempotency_key,19,1)='-' AND substr(launch_idempotency_key,24,1)='-' AND length(replace(launch_idempotency_key,'-',''))=32 AND replace(launch_idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    launch_request_fingerprint TEXT NOT NULL CHECK(length(launch_request_fingerprint)=71 AND substr(launch_request_fingerprint,1,7)='sha256:' AND substr(launch_request_fingerprint,8)=lower(substr(launch_request_fingerprint,8)) AND substr(launch_request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE(custody_id,custody_generation)
                );
                CREATE INDEX idx_closure_sources_recovery
                    ON closure_sources(created_at,id);
                CREATE INDEX idx_closure_sources_state
                    ON closure_sources(state,created_at,id);

                CREATE TABLE closure_source_sessions (
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    session_id TEXT NOT NULL UNIQUE CHECK(length(session_id)=36 AND session_id=lower(session_id) AND substr(session_id,9,1)='-' AND substr(session_id,14,1)='-' AND substr(session_id,19,1)='-' AND substr(session_id,24,1)='-' AND length(replace(session_id,'-',''))=32 AND replace(session_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sessions(id) ON DELETE RESTRICT,
                    rotation_depth INTEGER NOT NULL CHECK(rotation_depth>=0),
                    custody_id TEXT NOT NULL CHECK(length(custody_id)=36 AND custody_id=lower(custody_id) AND substr(custody_id,9,1)='-' AND substr(custody_id,14,1)='-' AND substr(custody_id,19,1)='-' AND substr(custody_id,24,1)='-' AND length(replace(custody_id,'-',''))=32 AND replace(custody_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
                    continued_from_session_id TEXT CHECK(continued_from_session_id IS NULL OR (length(continued_from_session_id)=36 AND continued_from_session_id=lower(continued_from_session_id) AND substr(continued_from_session_id,9,1)='-' AND substr(continued_from_session_id,14,1)='-' AND substr(continued_from_session_id,19,1)='-' AND substr(continued_from_session_id,24,1)='-' AND length(replace(continued_from_session_id,'-',''))=32 AND replace(continued_from_session_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES sessions(id) ON DELETE RESTRICT,
                    bound_at TEXT NOT NULL,
                    PRIMARY KEY(source_id,rotation_depth),
                    UNIQUE(source_id,session_id),
                    CHECK((rotation_depth=0 AND continued_from_session_id IS NULL) OR (rotation_depth>0 AND continued_from_session_id IS NOT NULL))
                );

                CREATE TABLE conversation_event_provenance (
                    conversation_event_id INTEGER PRIMARY KEY REFERENCES conversation_events(id) ON DELETE RESTRICT,
                    producer_kind TEXT NOT NULL CHECK(producer_kind IN ('provider_assistant_output','daemon_provider_diagnostic','provider_other')),
                    model_invocation_id TEXT NOT NULL CHECK(length(model_invocation_id)=36 AND model_invocation_id=lower(model_invocation_id) AND substr(model_invocation_id,9,1)='-' AND substr(model_invocation_id,14,1)='-' AND substr(model_invocation_id,19,1)='-' AND substr(model_invocation_id,24,1)='-' AND length(replace(model_invocation_id,'-',''))=32 AND replace(model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES model_invocations(id) ON DELETE RESTRICT,
                    provider_event_type TEXT NOT NULL CHECK(length(provider_event_type) BETWEEN 1 AND 128),
                    created_at TEXT NOT NULL
                );
                CREATE INDEX idx_conversation_event_provenance_invocation
                    ON conversation_event_provenance(model_invocation_id,producer_kind,conversation_event_id);
                CREATE TRIGGER conversation_event_provenance_identity_guard BEFORE INSERT ON conversation_event_provenance
                WHEN NOT EXISTS (
                    SELECT 1 FROM conversation_events e
                    JOIN model_invocations mi ON mi.id=NEW.model_invocation_id
                    WHERE e.id=NEW.conversation_event_id AND mi.session_id=e.session_id
                      AND (NEW.producer_kind='provider_other'
                           OR (e.event_type='Message' AND e.role='Assistant'))
                )
                BEGIN SELECT RAISE(ABORT,'conversation event provenance must match event/invocation identity'); END;

                CREATE TABLE closure_output_validations (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    tip_session_id TEXT NOT NULL CHECK(length(tip_session_id)=36 AND tip_session_id=lower(tip_session_id) AND substr(tip_session_id,9,1)='-' AND substr(tip_session_id,14,1)='-' AND substr(tip_session_id,19,1)='-' AND substr(tip_session_id,24,1)='-' AND length(replace(tip_session_id,'-',''))=32 AND replace(tip_session_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sessions(id) ON DELETE RESTRICT,
                    model_invocation_id TEXT NOT NULL CHECK(length(model_invocation_id)=36 AND model_invocation_id=lower(model_invocation_id) AND substr(model_invocation_id,9,1)='-' AND substr(model_invocation_id,14,1)='-' AND substr(model_invocation_id,19,1)='-' AND substr(model_invocation_id,24,1)='-' AND length(replace(model_invocation_id,'-',''))=32 AND replace(model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES model_invocations(id) ON DELETE RESTRICT,
                    conversation_event_id INTEGER REFERENCES conversation_events(id) ON DELETE RESTRICT,
                    conversation_sequence INTEGER,
                    producer_kind TEXT CHECK(producer_kind IS NULL OR producer_kind='provider_assistant_output'),
                    provider_event_type TEXT,
                    parser_source TEXT NOT NULL CHECK(parser_source IN ('closure_handoff_field_v1','missing_provider_output')),
                    raw_handoff TEXT,
                    raw_handoff_digest TEXT CHECK(raw_handoff_digest IS NULL OR (length(raw_handoff_digest)=71 AND substr(raw_handoff_digest,1,7)='sha256:' AND substr(raw_handoff_digest,8)=lower(substr(raw_handoff_digest,8)) AND substr(raw_handoff_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    normalized_envelope_json TEXT,
                    normalized_envelope_digest TEXT CHECK(normalized_envelope_digest IS NULL OR (length(normalized_envelope_digest)=71 AND substr(normalized_envelope_digest,1,7)='sha256:' AND substr(normalized_envelope_digest,8)=lower(substr(normalized_envelope_digest,8)) AND substr(normalized_envelope_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    disposition TEXT NOT NULL CHECK(disposition IN ('accepted_committed','accepted_no_change','accepted_blocker','missing_provider_output','malformed_output','correlation_mismatch','blocked_ambiguous_lineage','blocked_outcome_git_mismatch')),
                    validation_issues_json TEXT NOT NULL,
                    source_state TEXT NOT NULL CHECK(source_state IN ('outcome_committed','outcome_no_change','outcome_blocker','outcome_blocked')),
                    observed_source_ref_head TEXT CHECK(observed_source_ref_head IS NULL OR (length(observed_source_ref_head) IN (40,64) AND observed_source_ref_head=lower(observed_source_ref_head) AND observed_source_ref_head NOT GLOB '*[^0-9a-f]*')),
                    observed_worktree_head TEXT CHECK(observed_worktree_head IS NULL OR (length(observed_worktree_head) IN (40,64) AND observed_worktree_head=lower(observed_worktree_head) AND observed_worktree_head NOT GLOB '*[^0-9a-f]*')),
                    observed_worktree_clean INTEGER CHECK(observed_worktree_clean IN (0,1) OR observed_worktree_clean IS NULL),
                    created_at TEXT NOT NULL,
                    UNIQUE(source_id,tip_session_id,model_invocation_id),
                    CHECK((parser_source='missing_provider_output' AND conversation_event_id IS NULL AND conversation_sequence IS NULL AND producer_kind IS NULL AND raw_handoff IS NULL)
                       OR (parser_source='closure_handoff_field_v1' AND conversation_event_id IS NOT NULL AND conversation_sequence IS NOT NULL AND producer_kind='provider_assistant_output' AND raw_handoff IS NOT NULL)),
                    CHECK((disposition IN ('accepted_committed','accepted_no_change','accepted_blocker') AND normalized_envelope_json IS NOT NULL AND normalized_envelope_digest IS NOT NULL)
                       OR (disposition NOT IN ('accepted_committed','accepted_no_change','accepted_blocker')))
                );
                CREATE INDEX idx_closure_output_validations_source
                    ON closure_output_validations(source_id,created_at,id);

                CREATE TABLE closure_evidence (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    disposition TEXT NOT NULL CHECK(disposition IN ('required_independent','not_required_tier0_deterministic','not_required_proven_no_change')),
                    reviewed_source_head TEXT NOT NULL CHECK(length(reviewed_source_head) IN (40,64) AND reviewed_source_head=lower(reviewed_source_head) AND reviewed_source_head NOT GLOB '*[^0-9a-f]*'),
                    review_schema_version INTEGER,
                    review_raw_bytes BLOB,
                    review_digest TEXT CHECK(review_digest IS NULL OR (length(review_digest)=71 AND substr(review_digest,1,7)='sha256:' AND substr(review_digest,8)=lower(substr(review_digest,8)) AND substr(review_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    normalized_review_json TEXT,
                    finding_set_digest TEXT CHECK(finding_set_digest IS NULL OR (length(finding_set_digest)=71 AND substr(finding_set_digest,1,7)='sha256:' AND substr(finding_set_digest,8)=lower(substr(finding_set_digest,8)) AND substr(finding_set_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    reviewer_session_id TEXT CHECK(reviewer_session_id IS NULL OR (length(reviewer_session_id)=36 AND reviewer_session_id=lower(reviewer_session_id) AND substr(reviewer_session_id,9,1)='-' AND substr(reviewer_session_id,14,1)='-' AND substr(reviewer_session_id,19,1)='-' AND substr(reviewer_session_id,24,1)='-' AND length(replace(reviewer_session_id,'-',''))=32 AND replace(reviewer_session_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES sessions(id) ON DELETE RESTRICT,
                    reviewer_model_invocation_id TEXT CHECK(reviewer_model_invocation_id IS NULL OR (length(reviewer_model_invocation_id)=36 AND reviewer_model_invocation_id=lower(reviewer_model_invocation_id) AND substr(reviewer_model_invocation_id,9,1)='-' AND substr(reviewer_model_invocation_id,14,1)='-' AND substr(reviewer_model_invocation_id,19,1)='-' AND substr(reviewer_model_invocation_id,24,1)='-' AND length(replace(reviewer_model_invocation_id,'-',''))=32 AND replace(reviewer_model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES model_invocations(id) ON DELETE RESTRICT,
                    reviewer_provider TEXT,
                    reviewer_model TEXT,
                    reviewer_custody_id TEXT CHECK(reviewer_custody_id IS NULL OR (length(reviewer_custody_id)=36 AND reviewer_custody_id=lower(reviewer_custody_id) AND substr(reviewer_custody_id,9,1)='-' AND substr(reviewer_custody_id,14,1)='-' AND substr(reviewer_custody_id,19,1)='-' AND substr(reviewer_custody_id,24,1)='-' AND length(replace(reviewer_custody_id,'-',''))=32 AND replace(reviewer_custody_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    reviewer_custody_generation INTEGER,
                    evidence_commit_sha TEXT CHECK(evidence_commit_sha IS NULL OR (length(evidence_commit_sha) IN (40,64) AND evidence_commit_sha=lower(evidence_commit_sha) AND evidence_commit_sha NOT GLOB '*[^0-9a-f]*')),
                    evidence_parent_sha TEXT CHECK(evidence_parent_sha IS NULL OR (length(evidence_parent_sha) IN (40,64) AND evidence_parent_sha=lower(evidence_parent_sha) AND evidence_parent_sha NOT GLOB '*[^0-9a-f]*')),
                    review_json_path TEXT,
                    manifest_v2_path TEXT,
                    review_handoff_event_id INTEGER REFERENCES conversation_events(id) ON DELETE RESTRICT,
                    review_handoff_digest TEXT CHECK(review_handoff_digest IS NULL OR (length(review_handoff_digest)=71 AND substr(review_handoff_digest,1,7)='sha256:' AND substr(review_handoff_digest,8)=lower(substr(review_handoff_digest,8)) AND substr(review_handoff_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    manifest_schema_version INTEGER CHECK(manifest_schema_version IS NULL OR manifest_schema_version=2),
                    manifest_raw_bytes BLOB,
                    manifest_digest TEXT CHECK(manifest_digest IS NULL OR (length(manifest_digest)=71 AND substr(manifest_digest,1,7)='sha256:' AND substr(manifest_digest,8)=lower(substr(manifest_digest,8)) AND substr(manifest_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    manifest_source_head TEXT CHECK(manifest_source_head IS NULL OR (length(manifest_source_head) IN (40,64) AND manifest_source_head=lower(manifest_source_head) AND manifest_source_head NOT GLOB '*[^0-9a-f]*')),
                    source_ref_before TEXT NOT NULL CHECK(length(source_ref_before) IN (40,64) AND source_ref_before=lower(source_ref_before) AND source_ref_before NOT GLOB '*[^0-9a-f]*'),
                    source_ref_after TEXT NOT NULL CHECK(length(source_ref_after) IN (40,64) AND source_ref_after=lower(source_ref_after) AND source_ref_after NOT GLOB '*[^0-9a-f]*'),
                    source_worktree_head_before TEXT NOT NULL CHECK(length(source_worktree_head_before) IN (40,64) AND source_worktree_head_before=lower(source_worktree_head_before) AND source_worktree_head_before NOT GLOB '*[^0-9a-f]*'),
                    source_worktree_head_after TEXT NOT NULL CHECK(length(source_worktree_head_after) IN (40,64) AND source_worktree_head_after=lower(source_worktree_head_after) AND source_worktree_head_after NOT GLOB '*[^0-9a-f]*'),
                    review_policy_digest TEXT NOT NULL CHECK(length(review_policy_digest)=71 AND substr(review_policy_digest,1,7)='sha256:' AND substr(review_policy_digest,8)=lower(substr(review_policy_digest,8)) AND substr(review_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verification_policy_digest TEXT NOT NULL CHECK(length(verification_policy_digest)=71 AND substr(verification_policy_digest,1,7)='sha256:' AND substr(verification_policy_digest,8)=lower(substr(verification_policy_digest,8)) AND substr(verification_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    accepted_by TEXT NOT NULL,
                    accepted_at TEXT NOT NULL,
                    CHECK(source_ref_before=reviewed_source_head AND source_ref_after=reviewed_source_head AND source_worktree_head_before=reviewed_source_head AND source_worktree_head_after=reviewed_source_head),
                    CHECK((disposition='required_independent' AND review_schema_version=1 AND review_raw_bytes IS NOT NULL AND review_digest IS NOT NULL AND normalized_review_json IS NOT NULL AND finding_set_digest IS NOT NULL AND reviewer_session_id IS NOT NULL AND reviewer_model_invocation_id IS NOT NULL AND reviewer_provider IS NOT NULL AND reviewer_custody_id IS NOT NULL AND reviewer_custody_generation IS NOT NULL AND evidence_commit_sha IS NOT NULL AND evidence_parent_sha=reviewed_source_head AND review_json_path IS NOT NULL AND manifest_v2_path IS NOT NULL AND review_handoff_event_id IS NOT NULL AND review_handoff_digest IS NOT NULL AND manifest_schema_version=2 AND manifest_raw_bytes IS NOT NULL AND manifest_digest IS NOT NULL AND manifest_source_head=reviewed_source_head)
                       OR (disposition!='required_independent' AND review_schema_version IS NULL AND review_raw_bytes IS NULL AND review_digest IS NULL AND normalized_review_json IS NULL AND finding_set_digest IS NULL AND reviewer_session_id IS NULL AND reviewer_model_invocation_id IS NULL AND reviewer_provider IS NULL AND reviewer_model IS NULL AND reviewer_custody_id IS NULL AND reviewer_custody_generation IS NULL AND evidence_commit_sha IS NULL AND evidence_parent_sha IS NULL AND review_json_path IS NULL AND manifest_v2_path IS NULL AND review_handoff_event_id IS NULL AND review_handoff_digest IS NULL AND manifest_schema_version IS NULL AND manifest_raw_bytes IS NULL AND manifest_digest IS NULL AND manifest_source_head IS NULL))
                );

                CREATE TABLE closure_integration_queue (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    evidence_id TEXT NOT NULL UNIQUE CHECK(length(evidence_id)=36 AND evidence_id=lower(evidence_id) AND substr(evidence_id,9,1)='-' AND substr(evidence_id,14,1)='-' AND substr(evidence_id,19,1)='-' AND substr(evidence_id,24,1)='-' AND length(replace(evidence_id,'-',''))=32 AND replace(evidence_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_evidence(id) ON DELETE RESTRICT,
                    source_head TEXT NOT NULL CHECK(length(source_head) IN (40,64) AND source_head=lower(source_head) AND source_head NOT GLOB '*[^0-9a-f]*'),
                    state TEXT NOT NULL CHECK(state IN ('eligible_k2','claimed','completed','blocked')),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE closure_target_leases (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    repository_identity TEXT NOT NULL,
                    target_ref TEXT NOT NULL CHECK(substr(target_ref,1,11)='refs/heads/' AND length(target_ref)>11),
                    program_id TEXT NOT NULL CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    generation INTEGER NOT NULL CHECK(generation>0),
                    state TEXT NOT NULL CHECK(state IN ('held','released','expired')),
                    holder_boot_id TEXT NOT NULL CHECK(length(holder_boot_id)=36 AND holder_boot_id=lower(holder_boot_id) AND substr(holder_boot_id,9,1)='-' AND substr(holder_boot_id,14,1)='-' AND substr(holder_boot_id,19,1)='-' AND substr(holder_boot_id,24,1)='-' AND length(replace(holder_boot_id,'-',''))=32 AND replace(holder_boot_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    expires_at TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE UNIQUE INDEX idx_closure_target_leases_held
                    ON closure_target_leases(repository_identity,target_ref)
                    WHERE state='held';

                CREATE TABLE closure_integration_attempts (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    lease_id TEXT CHECK(lease_id IS NULL OR (length(lease_id)=36 AND lease_id=lower(lease_id) AND substr(lease_id,9,1)='-' AND substr(lease_id,14,1)='-' AND substr(lease_id,19,1)='-' AND substr(lease_id,24,1)='-' AND length(replace(lease_id,'-',''))=32 AND replace(lease_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_target_leases(id) ON DELETE RESTRICT,
                    candidate_receipt_id TEXT CHECK(candidate_receipt_id IS NULL OR (length(candidate_receipt_id)=36 AND candidate_receipt_id=lower(candidate_receipt_id) AND substr(candidate_receipt_id,9,1)='-' AND substr(candidate_receipt_id,14,1)='-' AND substr(candidate_receipt_id,19,1)='-' AND substr(candidate_receipt_id,24,1)='-' AND length(replace(candidate_receipt_id,'-',''))=32 AND replace(candidate_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')),
                    phase TEXT NOT NULL CHECK(phase IN ('reserved','staging_cas_pending','candidate_gate_pending','candidate_verified','destination_preflight','detach_pending','detached','destination_cas_pending','destination_applied','final_gate_pending','reattach_pending','reattached','complete','conflict','reconciliation_blocked')),
                    staging_old_head TEXT CHECK(staging_old_head IS NULL OR (length(staging_old_head) IN (40,64) AND staging_old_head=lower(staging_old_head) AND staging_old_head NOT GLOB '*[^0-9a-f]*')),
                    staging_new_head TEXT CHECK(staging_new_head IS NULL OR (length(staging_new_head) IN (40,64) AND staging_new_head=lower(staging_new_head) AND staging_new_head NOT GLOB '*[^0-9a-f]*')),
                    destination_old_head TEXT CHECK(destination_old_head IS NULL OR (length(destination_old_head) IN (40,64) AND destination_old_head=lower(destination_old_head) AND destination_old_head NOT GLOB '*[^0-9a-f]*')),
                    destination_new_head TEXT CHECK(destination_new_head IS NULL OR (length(destination_new_head) IN (40,64) AND destination_new_head=lower(destination_new_head) AND destination_new_head NOT GLOB '*[^0-9a-f]*')),
                    checkout_path TEXT,
                    request_id TEXT CHECK(request_id IS NULL OR (length(request_id)=36 AND request_id=lower(request_id) AND substr(request_id,9,1)='-' AND substr(request_id,14,1)='-' AND substr(request_id,19,1)='-' AND substr(request_id,24,1)='-' AND length(replace(request_id,'-',''))=32 AND replace(request_id,'-','') NOT GLOB '*[^0-9a-f]*')),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE integration_receipts (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    parent_receipt_id TEXT CHECK(parent_receipt_id IS NULL OR (length(parent_receipt_id)=36 AND parent_receipt_id=lower(parent_receipt_id) AND substr(parent_receipt_id,9,1)='-' AND substr(parent_receipt_id,14,1)='-' AND substr(parent_receipt_id,19,1)='-' AND substr(parent_receipt_id,24,1)='-' AND length(replace(parent_receipt_id,'-',''))=32 AND replace(parent_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    program_id TEXT NOT NULL CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    attempt_id TEXT CHECK(attempt_id IS NULL OR (length(attempt_id)=36 AND attempt_id=lower(attempt_id) AND substr(attempt_id,9,1)='-' AND substr(attempt_id,14,1)='-' AND substr(attempt_id,19,1)='-' AND substr(attempt_id,24,1)='-' AND length(replace(attempt_id,'-',''))=32 AND replace(attempt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_integration_attempts(id) ON DELETE RESTRICT,
                    receipt_kind TEXT NOT NULL CHECK(receipt_kind IN ('candidate_verified','integrated','gate_failed','conflict','accepted_no_change','integrated_after_recheck','quarantined_for_forward_repair','discarded')),
                    method TEXT NOT NULL CHECK(method='fast_forward'),
                    source_base_sha TEXT NOT NULL CHECK(length(source_base_sha) IN (40,64) AND source_base_sha=lower(source_base_sha) AND source_base_sha NOT GLOB '*[^0-9a-f]*'),
                    source_head TEXT NOT NULL CHECK(length(source_head) IN (40,64) AND source_head=lower(source_head) AND source_head NOT GLOB '*[^0-9a-f]*'),
                    staging_pre_head TEXT CHECK(staging_pre_head IS NULL OR (length(staging_pre_head) IN (40,64) AND staging_pre_head=lower(staging_pre_head) AND staging_pre_head NOT GLOB '*[^0-9a-f]*')),
                    staging_post_head TEXT CHECK(staging_post_head IS NULL OR (length(staging_post_head) IN (40,64) AND staging_post_head=lower(staging_post_head) AND staging_post_head NOT GLOB '*[^0-9a-f]*')),
                    expected_destination_pre_head TEXT CHECK(expected_destination_pre_head IS NULL OR (length(expected_destination_pre_head) IN (40,64) AND expected_destination_pre_head=lower(expected_destination_pre_head) AND expected_destination_pre_head NOT GLOB '*[^0-9a-f]*')),
                    destination_pre_head TEXT CHECK(destination_pre_head IS NULL OR (length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*')),
                    destination_post_head TEXT CHECK(destination_post_head IS NULL OR (length(destination_post_head) IN (40,64) AND destination_post_head=lower(destination_post_head) AND destination_post_head NOT GLOB '*[^0-9a-f]*')),
                    evidence_id TEXT CHECK(evidence_id IS NULL OR (length(evidence_id)=36 AND evidence_id=lower(evidence_id) AND substr(evidence_id,9,1)='-' AND substr(evidence_id,14,1)='-' AND substr(evidence_id,19,1)='-' AND substr(evidence_id,24,1)='-' AND length(replace(evidence_id,'-',''))=32 AND replace(evidence_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_evidence(id) ON DELETE RESTRICT,
                    review_schema_version INTEGER,
                    review_digest TEXT CHECK(review_digest IS NULL OR (length(review_digest)=71 AND substr(review_digest,1,7)='sha256:' AND substr(review_digest,8)=lower(substr(review_digest,8)) AND substr(review_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    review_embedded_source_head TEXT CHECK(review_embedded_source_head IS NULL OR (length(review_embedded_source_head) IN (40,64) AND review_embedded_source_head=lower(review_embedded_source_head) AND review_embedded_source_head NOT GLOB '*[^0-9a-f]*')),
                    manifest_schema_version INTEGER,
                    manifest_digest TEXT CHECK(manifest_digest IS NULL OR (length(manifest_digest)=71 AND substr(manifest_digest,1,7)='sha256:' AND substr(manifest_digest,8)=lower(substr(manifest_digest,8)) AND substr(manifest_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    manifest_embedded_source_head TEXT CHECK(manifest_embedded_source_head IS NULL OR (length(manifest_embedded_source_head) IN (40,64) AND manifest_embedded_source_head=lower(manifest_embedded_source_head) AND manifest_embedded_source_head NOT GLOB '*[^0-9a-f]*')),
                    review_policy_digest TEXT NOT NULL CHECK(length(review_policy_digest)=71 AND substr(review_policy_digest,1,7)='sha256:' AND substr(review_policy_digest,8)=lower(substr(review_policy_digest,8)) AND substr(review_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verification_policy_digest TEXT NOT NULL CHECK(length(verification_policy_digest)=71 AND substr(verification_policy_digest,1,7)='sha256:' AND substr(verification_policy_digest,8)=lower(substr(verification_policy_digest,8)) AND substr(verification_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    gate_evidence_json TEXT,
                    disposition TEXT NOT NULL,
                    actor TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    CHECK((receipt_kind='candidate_verified' AND staging_pre_head IS NOT NULL AND staging_post_head IS NOT NULL AND expected_destination_pre_head IS NOT NULL)
                       OR receipt_kind!='candidate_verified'),
                    CHECK((receipt_kind IN ('integrated','gate_failed','conflict','integrated_after_recheck') AND destination_pre_head IS NOT NULL)
                       OR receipt_kind NOT IN ('integrated','gate_failed','conflict','integrated_after_recheck'))
                );

                CREATE TABLE closure_receipt_consumptions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    candidate_receipt_id TEXT NOT NULL UNIQUE CHECK(length(candidate_receipt_id)=36 AND candidate_receipt_id=lower(candidate_receipt_id) AND substr(candidate_receipt_id,9,1)='-' AND substr(candidate_receipt_id,14,1)='-' AND substr(candidate_receipt_id,19,1)='-' AND substr(candidate_receipt_id,24,1)='-' AND length(replace(candidate_receipt_id,'-',''))=32 AND replace(candidate_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    integration_attempt_id TEXT NOT NULL UNIQUE CHECK(length(integration_attempt_id)=36 AND integration_attempt_id=lower(integration_attempt_id) AND substr(integration_attempt_id,9,1)='-' AND substr(integration_attempt_id,14,1)='-' AND substr(integration_attempt_id,19,1)='-' AND substr(integration_attempt_id,24,1)='-' AND length(replace(integration_attempt_id,'-',''))=32 AND replace(integration_attempt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_integration_attempts(id) ON DELETE RESTRICT,
                    consumed_at TEXT NOT NULL
                );

                CREATE TABLE closure_final_gate_attempt_receipts (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    integration_attempt_id TEXT NOT NULL CHECK(length(integration_attempt_id)=36 AND integration_attempt_id=lower(integration_attempt_id) AND substr(integration_attempt_id,9,1)='-' AND substr(integration_attempt_id,14,1)='-' AND substr(integration_attempt_id,19,1)='-' AND substr(integration_attempt_id,24,1)='-' AND length(replace(integration_attempt_id,'-',''))=32 AND replace(integration_attempt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_integration_attempts(id) ON DELETE RESTRICT,
                    ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 1 AND 3),
                    receipt_phase TEXT NOT NULL CHECK(receipt_phase IN ('started','terminal')),
                    failed_destination_receipt_id TEXT CHECK(failed_destination_receipt_id IS NULL OR (length(failed_destination_receipt_id)=36 AND failed_destination_receipt_id=lower(failed_destination_receipt_id) AND substr(failed_destination_receipt_id,9,1)='-' AND substr(failed_destination_receipt_id,14,1)='-' AND substr(failed_destination_receipt_id,19,1)='-' AND substr(failed_destination_receipt_id,24,1)='-' AND length(replace(failed_destination_receipt_id,'-',''))=32 AND replace(failed_destination_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    gated_head TEXT NOT NULL CHECK(length(gated_head) IN (40,64) AND gated_head=lower(gated_head) AND gated_head NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    reason TEXT CHECK(reason IS NULL OR reason IN ('initial','environmental','flaky')),
                    rationale TEXT,
                    runner_result_json TEXT,
                    disposition TEXT CHECK(disposition IS NULL OR disposition IN ('passed','failed','interrupted_by_restart')),
                    actor TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(integration_attempt_id,ordinal,receipt_phase),
                    CHECK((receipt_phase='started' AND runner_result_json IS NULL AND disposition IS NULL) OR (receipt_phase='terminal' AND runner_result_json IS NOT NULL AND disposition IS NOT NULL))
                );

                CREATE TABLE closure_gate_failure_settlements (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    failed_destination_receipt_id TEXT NOT NULL UNIQUE CHECK(length(failed_destination_receipt_id)=36 AND failed_destination_receipt_id=lower(failed_destination_receipt_id) AND substr(failed_destination_receipt_id,9,1)='-' AND substr(failed_destination_receipt_id,14,1)='-' AND substr(failed_destination_receipt_id,19,1)='-' AND substr(failed_destination_receipt_id,24,1)='-' AND length(replace(failed_destination_receipt_id,'-',''))=32 AND replace(failed_destination_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    expected_current_destination_head TEXT NOT NULL CHECK(length(expected_current_destination_head) IN (40,64) AND expected_current_destination_head=lower(expected_current_destination_head) AND expected_current_destination_head NOT GLOB '*[^0-9a-f]*'),
                    observed_current_destination_head TEXT CHECK(observed_current_destination_head IS NULL OR (length(observed_current_destination_head) IN (40,64) AND observed_current_destination_head=lower(observed_current_destination_head) AND observed_current_destination_head NOT GLOB '*[^0-9a-f]*')),
                    quarantine_ref TEXT NOT NULL UNIQUE CHECK(substr(quarantine_ref,1,28)='refs/rsi/closure-quarantine/' AND length(quarantine_ref)>28),
                    quarantine_head TEXT NOT NULL CHECK(length(quarantine_head) IN (40,64) AND quarantine_head=lower(quarantine_head) AND quarantine_head NOT GLOB '*[^0-9a-f]*'),
                    reason TEXT NOT NULL CHECK(reason IN ('candidate_bad','policy_incompatible','forward_repair_required')),
                    rationale TEXT NOT NULL,
                    confirmation TEXT NOT NULL CHECK(confirmation='HUMAN-QUARANTINE'),
                    journal_phase TEXT NOT NULL CHECK(journal_phase IN ('reserved','quarantine_ref_pending','quarantine_ref_verified','receipt_appended','claim_released','complete','reconciliation_blocked')),
                    settlement_receipt_id TEXT CHECK(settlement_receipt_id IS NULL OR (length(settlement_receipt_id)=36 AND settlement_receipt_id=lower(settlement_receipt_id) AND substr(settlement_receipt_id,9,1)='-' AND substr(settlement_receipt_id,14,1)='-' AND substr(settlement_receipt_id,19,1)='-' AND substr(settlement_receipt_id,24,1)='-' AND length(replace(settlement_receipt_id,'-',''))=32 AND replace(settlement_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    destination_claim_released_at TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE closure_cleanup_proofs (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    proof_kind TEXT NOT NULL CHECK(proof_kind IN ('accepted_receipt','discard','quarantine_settlement')),
                    receipt_id TEXT CHECK(receipt_id IS NULL OR (length(receipt_id)=36 AND receipt_id=lower(receipt_id) AND substr(receipt_id,9,1)='-' AND substr(receipt_id,14,1)='-' AND substr(receipt_id,19,1)='-' AND substr(receipt_id,24,1)='-' AND length(replace(receipt_id,'-',''))=32 AND replace(receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    settlement_id TEXT CHECK(settlement_id IS NULL OR (length(settlement_id)=36 AND settlement_id=lower(settlement_id) AND substr(settlement_id,9,1)='-' AND substr(settlement_id,14,1)='-' AND substr(settlement_id,19,1)='-' AND substr(settlement_id,24,1)='-' AND length(replace(settlement_id,'-',''))=32 AND replace(settlement_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_gate_failure_settlements(id) ON DELETE RESTRICT,
                    observed_destination_head TEXT CHECK(observed_destination_head IS NULL OR (length(observed_destination_head) IN (40,64) AND observed_destination_head=lower(observed_destination_head) AND observed_destination_head NOT GLOB '*[^0-9a-f]*')),
                    destination_reachable INTEGER NOT NULL CHECK(destination_reachable IN (0,1)),
                    observed_source_head TEXT NOT NULL CHECK(length(observed_source_head) IN (40,64) AND observed_source_head=lower(observed_source_head) AND observed_source_head NOT GLOB '*[^0-9a-f]*'),
                    observed_staging_head TEXT NOT NULL CHECK(length(observed_staging_head) IN (40,64) AND observed_staging_head=lower(observed_staging_head) AND observed_staging_head NOT GLOB '*[^0-9a-f]*'),
                    quarantine_ref TEXT CHECK(quarantine_ref IS NULL OR (substr(quarantine_ref,1,28)='refs/rsi/closure-quarantine/' AND length(quarantine_ref)>28)),
                    quarantine_head TEXT CHECK(quarantine_head IS NULL OR (length(quarantine_head) IN (40,64) AND quarantine_head=lower(quarantine_head) AND quarantine_head NOT GLOB '*[^0-9a-f]*')),
                    preview_id TEXT NOT NULL UNIQUE CHECK(length(preview_id)=36 AND preview_id=lower(preview_id) AND substr(preview_id,9,1)='-' AND substr(preview_id,14,1)='-' AND substr(preview_id,19,1)='-' AND substr(preview_id,24,1)='-' AND length(replace(preview_id,'-',''))=32 AND replace(preview_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    inventory_json TEXT NOT NULL,
                    inventory_digest TEXT NOT NULL CHECK(length(inventory_digest)=71 AND substr(inventory_digest,1,7)='sha256:' AND substr(inventory_digest,8)=lower(substr(inventory_digest,8)) AND substr(inventory_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    expires_at TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    CHECK((proof_kind='accepted_receipt' AND receipt_id IS NOT NULL AND settlement_id IS NULL)
                       OR (proof_kind='discard' AND receipt_id IS NOT NULL AND settlement_id IS NULL)
                       OR (proof_kind='quarantine_settlement' AND receipt_id IS NOT NULL AND settlement_id IS NOT NULL AND quarantine_ref IS NOT NULL AND quarantine_head IS NOT NULL))
                );

                CREATE TABLE closure_cleanup_actions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    proof_id TEXT NOT NULL UNIQUE CHECK(length(proof_id)=36 AND proof_id=lower(proof_id) AND substr(proof_id,9,1)='-' AND substr(proof_id,14,1)='-' AND substr(proof_id,19,1)='-' AND substr(proof_id,24,1)='-' AND length(replace(proof_id,'-',''))=32 AND replace(proof_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_cleanup_proofs(id) ON DELETE RESTRICT,
                    preview_id TEXT NOT NULL CHECK(length(preview_id)=36 AND preview_id=lower(preview_id) AND substr(preview_id,9,1)='-' AND substr(preview_id,14,1)='-' AND substr(preview_id,19,1)='-' AND substr(preview_id,24,1)='-' AND length(replace(preview_id,'-',''))=32 AND replace(preview_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    preview_digest TEXT NOT NULL CHECK(length(preview_digest)=71 AND substr(preview_digest,1,7)='sha256:' AND substr(preview_digest,8)=lower(substr(preview_digest,8)) AND substr(preview_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    confirmation TEXT NOT NULL CHECK(confirmation='HUMAN-CLEANUP'),
                    expected_custody_generation INTEGER NOT NULL CHECK(expected_custody_generation>0),
                    worktree_root TEXT NOT NULL,
                    source_ref TEXT NOT NULL CHECK(substr(source_ref,1,11)='refs/heads/' AND length(source_ref)>11),
                    source_expected_sha TEXT NOT NULL CHECK(length(source_expected_sha) IN (40,64) AND source_expected_sha=lower(source_expected_sha) AND source_expected_sha NOT GLOB '*[^0-9a-f]*'),
                    staging_ref TEXT NOT NULL CHECK(substr(staging_ref,1,29)='refs/heads/rsi/closure-stage/' AND length(staging_ref)>29),
                    staging_expected_sha TEXT NOT NULL CHECK(length(staging_expected_sha) IN (40,64) AND staging_expected_sha=lower(staging_expected_sha) AND staging_expected_sha NOT GLOB '*[^0-9a-f]*'),
                    session_ids_json TEXT NOT NULL,
                    phase TEXT NOT NULL CHECK(phase IN ('worktree_remove_pending','worktree_removed','source_ref_delete_pending','source_ref_deleted','staging_ref_delete_pending','staging_ref_deleted','custody_settlement_pending','cleanup_complete','retryable_failure','blocked_partial_worktree','blocked_partial_ref','blocked_partial_custody')),
                    partial_failure_json TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE closure_operator_requests (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    method TEXT NOT NULL CHECK(method IN ('CreateClosureProgram','UpdateClosureProgram','LaunchClosureSource','RecordClosureEvidence','ResumeClosureFinalization','ApproveClosurePromotion','RecheckClosureFinalGate','SupersedeClosureGateFailure','RecordClosureDiscard','ExecuteClosureCleanup')),
                    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key)=36 AND idempotency_key=lower(idempotency_key) AND substr(idempotency_key,9,1)='-' AND substr(idempotency_key,14,1)='-' AND substr(idempotency_key,19,1)='-' AND substr(idempotency_key,24,1)='-' AND length(replace(idempotency_key,'-',''))=32 AND replace(idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8)=lower(substr(request_fingerprint,8)) AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    request_json TEXT NOT NULL,
                    result_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(method,idempotency_key)
                );

                CREATE TABLE closure_events (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    program_id TEXT NOT NULL CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    source_id TEXT CHECK(source_id IS NULL OR (length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    sequence INTEGER NOT NULL CHECK(sequence>0),
                    event_kind TEXT NOT NULL,
                    prior_program_state TEXT,
                    next_program_state TEXT NOT NULL,
                    prior_source_state TEXT,
                    next_source_state TEXT,
                    correlation_key TEXT CHECK(correlation_key IS NULL OR length(correlation_key)>0),
                    payload_json TEXT NOT NULL,
                    occurred_at TEXT NOT NULL,
                    UNIQUE(program_id,sequence),
                    UNIQUE(source_id,event_kind,correlation_key)
                );

                CREATE TRIGGER closure_programs_identity_immutable BEFORE UPDATE ON closure_programs
                WHEN (NEW.id!=OLD.id OR NEW.repository_root!=OLD.repository_root OR NEW.repository_identity!=OLD.repository_identity OR NEW.base_ref!=OLD.base_ref OR NEW.base_sha!=OLD.base_sha OR NEW.destination_ref!=OLD.destination_ref OR NEW.destination_pre_head!=OLD.destination_pre_head OR NEW.creation_idempotency_key!=OLD.creation_idempotency_key OR NEW.creation_request_fingerprint!=OLD.creation_request_fingerprint OR NEW.created_at!=OLD.created_at)
                 AND EXISTS (SELECT 1 FROM closure_sources WHERE program_id=OLD.id)
                BEGIN SELECT RAISE(ABORT,'Closure program identity is immutable'); END;
                CREATE TRIGGER closure_destination_release_guard BEFORE UPDATE OF destination_claim_state ON closure_programs
                WHEN OLD.destination_claim_state='held' AND NEW.destination_claim_state='released'
                 AND NOT (
                    (NEW.state='closed' AND EXISTS (
                        SELECT 1 FROM closure_sources s
                        JOIN closure_cleanup_actions a ON a.source_id=s.id
                        WHERE s.program_id=OLD.id AND a.phase='cleanup_complete'
                    ))
                    OR
                    (NEW.state IN ('quarantined_cleanup_pending','quarantined_closed') AND EXISTS (
                        SELECT 1 FROM closure_sources s
                        JOIN closure_gate_failure_settlements q ON q.source_id=s.id
                        JOIN integration_receipts r ON r.id=q.settlement_receipt_id
                        WHERE s.program_id=OLD.id
                          AND q.journal_phase IN ('claim_released','complete')
                          AND q.destination_claim_released_at IS NOT NULL
                          AND r.receipt_kind='quarantined_for_forward_repair'
                          AND r.source_id=s.id
                    ))
                 )
                BEGIN SELECT RAISE(ABORT,'Closure destination release requires cleanup or quarantine settlement'); END;
                CREATE TRIGGER closure_destination_claim_no_reacquire BEFORE UPDATE OF destination_claim_state ON closure_programs
                WHEN OLD.destination_claim_state='released' AND NEW.destination_claim_state='held'
                BEGIN SELECT RAISE(ABORT,'Closure destination claims cannot be reacquired'); END;
                CREATE TRIGGER closure_sources_identity_immutable BEFORE UPDATE ON closure_sources
                WHEN NEW.id!=OLD.id OR NEW.program_id!=OLD.program_id OR NEW.custody_id!=OLD.custody_id OR NEW.root_session_id!=OLD.root_session_id OR NEW.source_ref!=OLD.source_ref OR NEW.source_base_sha!=OLD.source_base_sha OR NEW.destination_ref!=OLD.destination_ref OR NEW.destination_pre_head!=OLD.destination_pre_head OR NEW.staging_ref!=OLD.staging_ref OR NEW.created_at!=OLD.created_at
                BEGIN SELECT RAISE(ABORT,'Closure source identity is immutable'); END;
                CREATE TRIGGER closure_source_launch_reservations_identity_immutable BEFORE UPDATE ON closure_source_launch_reservations
                WHEN NEW.id!=OLD.id OR NEW.program_id!=OLD.program_id OR NEW.idempotency_key!=OLD.idempotency_key OR NEW.request_fingerprint!=OLD.request_fingerprint OR NEW.request_json!=OLD.request_json OR NEW.source_id!=OLD.source_id OR NEW.root_session_id!=OLD.root_session_id OR NEW.custody_id!=OLD.custody_id OR NEW.custody_generation!=OLD.custody_generation OR NEW.branch_name!=OLD.branch_name OR NEW.repository_identity!=OLD.repository_identity OR NEW.source_ref!=OLD.source_ref OR NEW.source_base_sha!=OLD.source_base_sha OR NEW.destination_ref!=OLD.destination_ref OR NEW.destination_pre_head!=OLD.destination_pre_head OR NEW.staging_ref!=OLD.staging_ref OR NEW.created_at!=OLD.created_at OR OLD.completed_at IS NOT NULL OR NEW.completed_at IS NULL
                BEGIN SELECT RAISE(ABORT,'Closure source launch reservation identity is immutable'); END;
                CREATE TRIGGER closure_source_launch_reservations_no_delete BEFORE DELETE ON closure_source_launch_reservations BEGIN SELECT RAISE(ABORT,'Closure source launch reservations are retained'); END;
                CREATE TRIGGER closure_sources_custody_generation_forward BEFORE UPDATE OF custody_generation ON closure_sources
                WHEN NEW.custody_generation!=OLD.custody_generation
                 AND (NEW.custody_generation!=OLD.custody_generation+1 OR NOT EXISTS (
                    SELECT 1 FROM closure_source_sessions css
                    WHERE css.source_id=OLD.id AND css.custody_id=OLD.custody_id
                      AND css.custody_generation=NEW.custody_generation
                 ))
                BEGIN SELECT RAISE(ABORT,'Closure source custody generation requires the next immutable rotation binding'); END;

                CREATE TRIGGER closure_source_sessions_no_update BEFORE UPDATE ON closure_source_sessions BEGIN SELECT RAISE(ABORT,'Closure source session bindings are immutable'); END;
                CREATE TRIGGER closure_source_sessions_no_delete BEFORE DELETE ON closure_source_sessions BEGIN SELECT RAISE(ABORT,'Closure source session bindings are retained'); END;
                CREATE TRIGGER conversation_event_provenance_no_update BEFORE UPDATE ON conversation_event_provenance BEGIN SELECT RAISE(ABORT,'conversation event provenance is immutable'); END;
                CREATE TRIGGER conversation_event_provenance_no_delete BEFORE DELETE ON conversation_event_provenance BEGIN SELECT RAISE(ABORT,'conversation event provenance is retained'); END;
                CREATE TRIGGER closure_output_validations_no_update BEFORE UPDATE ON closure_output_validations BEGIN SELECT RAISE(ABORT,'Closure output validations are immutable'); END;
                CREATE TRIGGER closure_output_validations_no_delete BEFORE DELETE ON closure_output_validations BEGIN SELECT RAISE(ABORT,'Closure output validations are retained'); END;
                CREATE TRIGGER closure_evidence_no_update BEFORE UPDATE ON closure_evidence BEGIN SELECT RAISE(ABORT,'Closure evidence is immutable'); END;
                CREATE TRIGGER closure_evidence_no_delete BEFORE DELETE ON closure_evidence BEGIN SELECT RAISE(ABORT,'Closure evidence is retained'); END;
                CREATE TRIGGER integration_receipts_no_update BEFORE UPDATE ON integration_receipts BEGIN SELECT RAISE(ABORT,'Closure receipts are immutable'); END;
                CREATE TRIGGER integration_receipts_no_delete BEFORE DELETE ON integration_receipts BEGIN SELECT RAISE(ABORT,'Closure receipts are retained'); END;
                CREATE TRIGGER closure_receipt_consumptions_no_update BEFORE UPDATE ON closure_receipt_consumptions BEGIN SELECT RAISE(ABORT,'Closure receipt consumptions are immutable'); END;
                CREATE TRIGGER closure_receipt_consumptions_no_delete BEFORE DELETE ON closure_receipt_consumptions BEGIN SELECT RAISE(ABORT,'Closure receipt consumptions are retained'); END;
                CREATE TRIGGER closure_final_gate_attempt_receipts_no_update BEFORE UPDATE ON closure_final_gate_attempt_receipts BEGIN SELECT RAISE(ABORT,'Closure gate receipts are immutable'); END;
                CREATE TRIGGER closure_final_gate_attempt_receipts_no_delete BEFORE DELETE ON closure_final_gate_attempt_receipts BEGIN SELECT RAISE(ABORT,'Closure gate receipts are retained'); END;
                CREATE TRIGGER closure_cleanup_proofs_no_update BEFORE UPDATE ON closure_cleanup_proofs BEGIN SELECT RAISE(ABORT,'Closure cleanup proofs are immutable'); END;
                CREATE TRIGGER closure_cleanup_proofs_no_delete BEFORE DELETE ON closure_cleanup_proofs BEGIN SELECT RAISE(ABORT,'Closure cleanup proofs are retained'); END;
                CREATE TRIGGER closure_operator_requests_no_update BEFORE UPDATE ON closure_operator_requests BEGIN SELECT RAISE(ABORT,'Closure operator request receipts are immutable'); END;
                CREATE TRIGGER closure_operator_requests_no_delete BEFORE DELETE ON closure_operator_requests BEGIN SELECT RAISE(ABORT,'Closure operator request receipts are retained'); END;
                CREATE TRIGGER closure_events_no_update BEFORE UPDATE ON closure_events BEGIN SELECT RAISE(ABORT,'Closure events are immutable'); END;
                CREATE TRIGGER closure_events_no_delete BEFORE DELETE ON closure_events BEGIN SELECT RAISE(ABORT,'Closure events are retained'); END;
                ",
            )?;
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V84 Closure catalog foreign-key check failed".into(),
                ));
            }
            tx.execute("PRAGMA user_version = 84", [])?;
            tx.commit()?;
            tracing::info!("V84 migration complete: Closure Kernel catalog and event provenance");
        }

        Ok(())
    }
}
