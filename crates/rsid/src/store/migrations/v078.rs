impl Store {
    fn migrate_v078(&self, version: i32) -> Result<()> {
        // V78: D05 durable serial ProgramRun kernel. The exact-V77 guard is
        // temporal evidence: schema drift requires a new reviewed migration,
        // never an opportunistic renumber or partial repair.
        if version < 78 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 77 {
                return Err(crate::error::DaemonError::Store(format!(
                    "V78 requires exact V77 source, found V{active_version}"
                )));
            }
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterExactSource)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_runs (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    project_id TEXT NOT NULL CHECK(length(project_id)=36 AND project_id=lower(project_id)),
                    idea_id TEXT NOT NULL CHECK(length(idea_id)=36 AND idea_id=lower(idea_id)),
                    template_key TEXT NOT NULL CHECK(length(CAST(template_key AS BLOB)) BETWEEN 1 AND 64),
                    template_version INTEGER NOT NULL CHECK(template_version > 0),
                    template_digest TEXT NOT NULL CHECK(
                        length(template_digest)=71 AND substr(template_digest,1,7)='sha256:'
                        AND substr(template_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    template_json TEXT NOT NULL CHECK(json_valid(template_json) AND length(CAST(template_json AS BLOB)) <= 1048576),
                    status TEXT NOT NULL CHECK(status IN (
                        'pending','ready','running','awaiting_gate','retry_pending',
                        'blocked','settled','cancelled','failed')),
                    cursor_ordinal INTEGER,
                    cursor_key TEXT,
                    cursor_phase TEXT,
                    revision_no INTEGER NOT NULL CHECK(revision_no >= 0),
                    controller_session_id TEXT NOT NULL CHECK(length(controller_session_id)=36 AND controller_session_id=lower(controller_session_id)),
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    idea_row_version INTEGER NOT NULL CHECK(idea_row_version >= 0),
                    row_version INTEGER NOT NULL CHECK(row_version >= 0),
                    next_transition_sequence INTEGER NOT NULL CHECK(next_transition_sequence > 0),
                    creation_idempotency_key TEXT NOT NULL UNIQUE CHECK(length(CAST(creation_idempotency_key AS BLOB)) BETWEEN 1 AND 256),
                    creation_request_fingerprint TEXT NOT NULL CHECK(
                        length(creation_request_fingerprint)=71 AND substr(creation_request_fingerprint,1,7)='sha256:'
                        AND substr(creation_request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    creation_request_json TEXT NOT NULL CHECK(json_valid(creation_request_json) AND length(CAST(creation_request_json AS BLOB)) <= 1048576),
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND substr(updated_at,30,1)='Z'),
                    settled_at TEXT CHECK(settled_at IS NULL OR (length(settled_at)=30 AND substr(settled_at,30,1)='Z')),
                    cancelled_at TEXT CHECK(cancelled_at IS NULL OR (length(cancelled_at)=30 AND substr(cancelled_at,30,1)='Z')),
                    failed_at TEXT CHECK(failed_at IS NULL OR (length(failed_at)=30 AND substr(failed_at,30,1)='Z')),
                    UNIQUE(id, project_id),
                    CHECK((cursor_ordinal IS NULL AND cursor_key IS NULL AND cursor_phase IS NULL)
                       OR (cursor_ordinal >= 0 AND cursor_key IS NOT NULL AND cursor_phase IS NOT NULL
                           AND length(CAST(cursor_key AS BLOB)) BETWEEN 1 AND 64
                           AND length(CAST(cursor_phase AS BLOB)) BETWEEN 1 AND 64)),
                    CHECK((status='settled' AND settled_at IS NOT NULL AND cancelled_at IS NULL AND failed_at IS NULL)
                       OR (status='cancelled' AND cancelled_at IS NOT NULL AND settled_at IS NULL AND failed_at IS NULL)
                       OR (status='failed' AND failed_at IS NOT NULL AND settled_at IS NULL AND cancelled_at IS NULL)
                       OR (status NOT IN ('settled','cancelled','failed') AND settled_at IS NULL AND cancelled_at IS NULL AND failed_at IS NULL)),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_id,project_id) REFERENCES ideas(id,project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id) REFERENCES sessions(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterRuns)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_transitions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    program_run_id TEXT NOT NULL CHECK(length(program_run_id)=36 AND program_run_id=lower(program_run_id)),
                    sequence INTEGER NOT NULL CHECK(sequence > 0),
                    operation TEXT NOT NULL CHECK(operation IN (
                        'create','locks_granted','action_claimed','attempt_terminal_observed',
                        'attempt_output_committed','gate_evaluated','retry_scheduled',
                        'wake_acknowledged','operator_unblocked','operator_cancelled',
                        'budget_exhausted','controller_rebound','reconciled_quarantine')),
                    from_status TEXT CHECK(from_status IS NULL OR from_status IN (
                        'pending','ready','running','awaiting_gate','retry_pending','blocked')),
                    to_status TEXT NOT NULL CHECK(to_status IN (
                        'pending','ready','running','awaiting_gate','retry_pending',
                        'blocked','settled','cancelled','failed')),
                    old_cursor_ordinal INTEGER,
                    old_cursor_key TEXT,
                    old_cursor_phase TEXT,
                    new_cursor_ordinal INTEGER,
                    new_cursor_key TEXT,
                    new_cursor_phase TEXT,
                    old_revision_no INTEGER NOT NULL CHECK(old_revision_no >= 0),
                    new_revision_no INTEGER NOT NULL CHECK(new_revision_no >= 0),
                    actor_kind TEXT NOT NULL CHECK(actor_kind IN ('operator','controller','scheduler','system')),
                    actor_session_id TEXT CHECK(actor_session_id IS NULL OR (length(actor_session_id)=36 AND actor_session_id=lower(actor_session_id))),
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    expected_run_version INTEGER NOT NULL CHECK(expected_run_version >= 0),
                    resulting_run_version INTEGER NOT NULL CHECK(resulting_run_version = expected_run_version + 1),
                    expected_idea_version INTEGER NOT NULL CHECK(expected_idea_version >= 0),
                    resulting_idea_version INTEGER NOT NULL CHECK(resulting_idea_version = expected_idea_version + 1),
                    idea_event_id TEXT NOT NULL UNIQUE CHECK(length(idea_event_id)=36 AND idea_event_id=lower(idea_event_id)),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 256),
                    request_json TEXT NOT NULL CHECK(json_valid(request_json) AND length(CAST(request_json AS BLOB)) <= 1048576),
                    request_fingerprint TEXT NOT NULL CHECK(
                        length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:'
                        AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    UNIQUE(program_run_id,sequence),
                    UNIQUE(program_run_id,idempotency_key),
                    CHECK((operation='create' AND from_status IS NULL) OR (operation!='create' AND from_status IS NOT NULL)),
                    CHECK((old_cursor_ordinal IS NULL AND old_cursor_key IS NULL AND old_cursor_phase IS NULL)
                       OR (old_cursor_ordinal >= 0 AND old_cursor_key IS NOT NULL AND old_cursor_phase IS NOT NULL)),
                    CHECK((new_cursor_ordinal IS NULL AND new_cursor_key IS NULL AND new_cursor_phase IS NULL)
                       OR (new_cursor_ordinal >= 0 AND new_cursor_key IS NOT NULL AND new_cursor_phase IS NOT NULL)),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(actor_session_id) REFERENCES sessions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_event_id) REFERENCES idea_events(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterTransitions)?;

            tx.execute_batch(D05_V78_GATES_TABLE_DDL)?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterGates)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_budgets (
                    program_run_id TEXT NOT NULL,
                    dimension TEXT NOT NULL CHECK(dimension IN (
                        'productive_transitions','work_attempts','launch_retries',
                        'revisions','wake_reservations','action_publication_retries')),
                    limit_value INTEGER NOT NULL CHECK(limit_value > 0),
                    reserved_value INTEGER NOT NULL CHECK(reserved_value >= 0),
                    used_value INTEGER NOT NULL CHECK(used_value >= 0),
                    row_version INTEGER NOT NULL CHECK(row_version >= 0),
                    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND substr(updated_at,30,1)='Z'),
                    PRIMARY KEY(program_run_id,dimension),
                    CHECK(reserved_value + used_value <= limit_value),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterBudgets)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_locks (
                    queue_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    id TEXT NOT NULL UNIQUE CHECK(length(id)=36 AND id=lower(id)),
                    project_id TEXT NOT NULL CHECK(length(project_id)=36 AND project_id=lower(project_id)),
                    lock_key TEXT NOT NULL CHECK(length(CAST(lock_key AS BLOB)) BETWEEN 1 AND 128),
                    conflict_domain TEXT NOT NULL CHECK(conflict_domain IN ('idea_controller','dangerous_mutation')),
                    program_run_id TEXT NOT NULL,
                    requesting_transition_id TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('requested','held','released','expired','cancelled')),
                    controller_session_id TEXT NOT NULL,
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    lease_generation INTEGER NOT NULL CHECK(lease_generation > 0),
                    owner_boot_id TEXT CHECK(owner_boot_id IS NULL OR (length(owner_boot_id)=36 AND owner_boot_id=lower(owner_boot_id))),
                    requested_at TEXT NOT NULL CHECK(length(requested_at)=30 AND substr(requested_at,30,1)='Z'),
                    acquired_at TEXT,
                    heartbeat_at TEXT,
                    expires_at TEXT,
                    released_at TEXT,
                    release_reason TEXT CHECK(release_reason IS NULL OR length(CAST(release_reason AS BLOB)) <= 2048),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 256),
                    request_fingerprint TEXT NOT NULL CHECK(
                        length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:'
                        AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    UNIQUE(program_run_id,conflict_domain),
                    CHECK((state='requested' AND owner_boot_id IS NULL AND acquired_at IS NULL AND heartbeat_at IS NULL AND expires_at IS NULL AND released_at IS NULL)
                       OR (state='held' AND owner_boot_id IS NOT NULL AND acquired_at IS NOT NULL AND heartbeat_at IS NOT NULL AND expires_at IS NOT NULL AND released_at IS NULL)
                       OR (state IN ('released','expired','cancelled') AND released_at IS NOT NULL)),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(requesting_transition_id) REFERENCES idea_program_run_transitions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id) REFERENCES sessions(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterLocks)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_actions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    program_run_id TEXT NOT NULL,
                    creating_transition_id TEXT NOT NULL UNIQUE,
                    action_kind TEXT NOT NULL CHECK(action_kind IN ('work','wake')),
                    purpose TEXT NOT NULL CHECK(purpose IN ('execute_cursor','evaluate_gates','retry_wake')),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(CAST(payload_json AS BLOB)) <= 1048576),
                    request_fingerprint TEXT NOT NULL CHECK(
                        length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:'
                        AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    downstream_dedup_key TEXT NOT NULL UNIQUE CHECK(length(CAST(downstream_dedup_key AS BLOB)) BETWEEN 1 AND 256),
                    controller_session_id TEXT NOT NULL,
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    not_before TEXT NOT NULL CHECK(length(not_before)=30 AND substr(not_before,30,1)='Z'),
                    state TEXT NOT NULL CHECK(state IN ('reserved','claimed','published','acknowledged','failed','cancelled')),
                    claim_boot_id TEXT CHECK(claim_boot_id IS NULL OR (length(claim_boot_id)=36 AND claim_boot_id=lower(claim_boot_id))),
                    claim_generation INTEGER NOT NULL CHECK(claim_generation > 0),
                    claimed_at TEXT,
                    claim_expires_at TEXT,
                    publication_attempts INTEGER NOT NULL CHECK(publication_attempts >= 0),
                    max_publication_attempts INTEGER NOT NULL CHECK(max_publication_attempts > 0),
                    external_model_invocation_id TEXT CHECK(external_model_invocation_id IS NULL OR length(external_model_invocation_id)=36),
                    external_session_id TEXT CHECK(external_session_id IS NULL OR length(external_session_id)=36),
                    scheduled_job_id TEXT CHECK(scheduled_job_id IS NULL OR length(scheduled_job_id)=36),
                    last_error_class TEXT CHECK(last_error_class IS NULL OR length(CAST(last_error_class AS BLOB)) <= 64),
                    last_error_message TEXT CHECK(last_error_message IS NULL OR length(CAST(last_error_message AS BLOB)) <= 512),
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND substr(updated_at,30,1)='Z'),
                    published_at TEXT,
                    acknowledged_at TEXT,
                    CHECK((state='reserved' AND claim_boot_id IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL AND published_at IS NULL AND acknowledged_at IS NULL)
                       OR (state='claimed' AND claim_boot_id IS NOT NULL AND claimed_at IS NOT NULL AND claim_expires_at IS NOT NULL AND published_at IS NULL AND acknowledged_at IS NULL)
                       OR (state='published' AND claim_boot_id IS NOT NULL AND claimed_at IS NOT NULL AND published_at IS NOT NULL AND acknowledged_at IS NULL)
                       OR (state='acknowledged' AND published_at IS NOT NULL AND acknowledged_at IS NOT NULL)
                       OR state IN ('failed','cancelled')),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(creating_transition_id) REFERENCES idea_program_run_transitions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id) REFERENCES sessions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(external_session_id) REFERENCES sessions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(scheduled_job_id) REFERENCES scheduled_jobs(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterActions)?;

            tx.execute_batch(D05_V78_ATTEMPT_REFS_TABLE_DDL)?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterAttemptRefs)?;

            tx.execute_batch(
                "CREATE INDEX idx_idea_program_runs_project_status_updated ON idea_program_runs(project_id,status,updated_at,id);
                 CREATE INDEX idx_idea_program_runs_idea_created ON idea_program_runs(idea_id,created_at,id);
                 CREATE INDEX idx_idea_program_run_transitions_run_sequence ON idea_program_run_transitions(program_run_id,sequence DESC);
                 CREATE INDEX idx_idea_program_run_transitions_idea_event ON idea_program_run_transitions(idea_event_id);
                 CREATE INDEX idx_idea_program_run_transitions_created ON idea_program_run_transitions(created_at,id);
                 CREATE INDEX idx_idea_program_run_gates_latest ON idea_program_run_gates(program_run_id,cursor_ordinal,revision_no,gate_key,evaluation_no DESC);
                 CREATE INDEX idx_idea_program_run_locks_requested ON idea_program_run_locks(project_id,lock_key,queue_sequence) WHERE state='requested';
                 CREATE INDEX idx_idea_program_run_locks_run_state ON idea_program_run_locks(program_run_id,state);
                 CREATE INDEX idx_idea_program_run_actions_due ON idea_program_run_actions(state,not_before,id);
                 CREATE INDEX idx_idea_program_run_actions_expired_claim ON idea_program_run_actions(state,claim_expires_at,id);
                 CREATE INDEX idx_idea_program_run_attempts_run_cursor ON idea_program_run_attempt_refs(program_run_id,cursor_ordinal,revision_no,attempt_no DESC);",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterLookupIndexes)?;

            tx.execute_batch(
                "CREATE UNIQUE INDEX uidx_idea_program_runs_one_nonterminal_idea ON idea_program_runs(idea_id)
                    WHERE status NOT IN ('settled','cancelled','failed');
                 CREATE UNIQUE INDEX uidx_idea_program_run_locks_held ON idea_program_run_locks(project_id,lock_key) WHERE state='held';
                 CREATE UNIQUE INDEX uidx_idea_program_run_actions_one_active ON idea_program_run_actions(program_run_id)
                    WHERE state IN ('reserved','claimed','published');
                 CREATE UNIQUE INDEX uidx_idea_program_run_attempts_session ON idea_program_run_attempt_refs(session_id) WHERE session_id IS NOT NULL;
                 CREATE UNIQUE INDEX uidx_idea_program_run_attempts_model_invocation ON idea_program_run_attempt_refs(model_invocation_id) WHERE model_invocation_id IS NOT NULL;",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterPartialUniqueIndexes)?;

            tx.execute_batch(
                "CREATE TRIGGER idea_program_runs_no_delete BEFORE DELETE ON idea_program_runs BEGIN SELECT RAISE(ABORT,'ProgramRun rows cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_transitions_no_delete BEFORE DELETE ON idea_program_run_transitions BEGIN SELECT RAISE(ABORT,'ProgramRun transitions cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_gates_no_delete BEFORE DELETE ON idea_program_run_gates BEGIN SELECT RAISE(ABORT,'ProgramRun gates cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_budgets_no_delete BEFORE DELETE ON idea_program_run_budgets BEGIN SELECT RAISE(ABORT,'ProgramRun budgets cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_locks_no_delete BEFORE DELETE ON idea_program_run_locks BEGIN SELECT RAISE(ABORT,'ProgramRun locks cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_actions_no_delete BEFORE DELETE ON idea_program_run_actions BEGIN SELECT RAISE(ABORT,'ProgramRun actions cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_attempt_refs_no_delete BEFORE DELETE ON idea_program_run_attempt_refs BEGIN SELECT RAISE(ABORT,'ProgramRun attempts cannot be deleted'); END;",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterNoDeleteTriggers)?;

            tx.execute_batch(
                "CREATE TRIGGER idea_program_run_transitions_immutable BEFORE UPDATE ON idea_program_run_transitions BEGIN SELECT RAISE(ABORT,'ProgramRun transitions are immutable'); END;
                 CREATE TRIGGER idea_program_run_gates_immutable BEFORE UPDATE ON idea_program_run_gates BEGIN SELECT RAISE(ABORT,'ProgramRun gates are immutable'); END;",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterImmutableTriggers)?;

            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(crate::error::DaemonError::Store(
                    "V78 foreign-key check failed".to_string(),
                ));
            }
            program_runs::validate_d05_catalog(&tx)?;
            let schema_fingerprint = program_runs::d05_v78_schema_fingerprint(&tx)?;
            if schema_fingerprint.len() != 71 || !schema_fingerprint.starts_with("sha256:") {
                return Err(crate::error::DaemonError::Store(
                    "V78 semantic schema fingerprint failed".to_string(),
                ));
            }
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 78", [])?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterUserVersion)?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V78 migration complete: ProgramRun kernel");
        }

        Ok(())
    }
}
