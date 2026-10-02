impl Store {
    fn migrate_v117(&self, version: i32) -> Result<()> {
        // V117: exact, retained harness-manager notice subjects and explicit
        // recorded/delivered/retrieved/settled lifecycle evidence.
        if version < 117 {
            self.apply_manager_notices_v117_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v117-manager-notices-driver
    fn apply_manager_notices_v117_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 116 {
            return Err(DaemonError::Store(format!(
                "V117 requires exact V116 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "ALTER TABLE harness_manager_watches ADD COLUMN route_kind TEXT NOT NULL DEFAULT 'epic'
                CHECK(route_kind IN ('epic','manager_action'));
            CREATE TABLE harness_manager_action_notice_queue (
                operation_id TEXT NOT NULL
                    REFERENCES harness_manager_v2_operations(id),
                project_id TEXT NOT NULL REFERENCES projects(id),
                manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                operation_row_version INTEGER NOT NULL CHECK(operation_row_version > 0),
                receipt_json TEXT NOT NULL
                    CHECK(json_valid(receipt_json) AND json_type(receipt_json)='object'
                          AND length(CAST(receipt_json AS BLOB))<=65536),
                queued_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(queued_at)),
                reconciled_at TEXT
                    CHECK(reconciled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(reconciled_at)),
                retired_at TEXT
                    CHECK(retired_at IS NULL OR rsi_rfc3339_nanos_is_canonical(retired_at)),
                CHECK(reconciled_at IS NULL OR retired_at IS NULL),
                PRIMARY KEY(operation_id,operation_row_version)
            );
            CREATE INDEX harness_manager_action_notice_pending
                ON harness_manager_action_notice_queue(
                    project_id,manager_session_id,scope_version,queued_at,operation_id,
                    operation_row_version)
                WHERE reconciled_at IS NULL AND retired_at IS NULL;
            CREATE TRIGGER harness_manager_action_notice_queue_after_insert
            AFTER INSERT ON harness_manager_v2_operations
            WHEN NEW.kind='lifecycle_action' AND NEW.state NOT IN ('queued','running')
            BEGIN
                INSERT OR IGNORE INTO harness_manager_action_notice_queue(
                    operation_id,project_id,manager_session_id,scope_version,
                    operation_row_version,receipt_json,queued_at,retired_at)
                VALUES(NEW.id,NEW.project_id,NEW.manager_session_id,NEW.scope_version,
                    NEW.row_version,NEW.outcome_json,NEW.updated_at,
                    CASE WHEN EXISTS(
                        SELECT 1 FROM harness_manager_scopes scope
                        WHERE scope.project_id=NEW.project_id
                          AND scope.manager_session_id=NEW.manager_session_id
                          AND scope.row_version=NEW.scope_version
                    ) THEN NULL ELSE NEW.updated_at END);
            END;
            CREATE TRIGGER harness_manager_action_notice_queue_after_update
            AFTER UPDATE OF state,row_version ON harness_manager_v2_operations
            WHEN NEW.kind='lifecycle_action' AND NEW.state NOT IN ('queued','running')
            BEGIN
                INSERT OR IGNORE INTO harness_manager_action_notice_queue(
                    operation_id,project_id,manager_session_id,scope_version,
                    operation_row_version,receipt_json,queued_at,retired_at)
                VALUES(NEW.id,NEW.project_id,NEW.manager_session_id,NEW.scope_version,
                    NEW.row_version,NEW.outcome_json,NEW.updated_at,
                    CASE WHEN EXISTS(
                        SELECT 1 FROM harness_manager_scopes scope
                        WHERE scope.project_id=NEW.project_id
                          AND scope.manager_session_id=NEW.manager_session_id
                          AND scope.row_version=NEW.scope_version
                    ) THEN NULL ELSE NEW.updated_at END);
            END;
            CREATE TRIGGER harness_manager_action_notice_queue_forward
            BEFORE UPDATE ON harness_manager_action_notice_queue
            WHEN NEW.operation_id IS NOT OLD.operation_id
              OR NEW.project_id IS NOT OLD.project_id
              OR NEW.manager_session_id IS NOT OLD.manager_session_id
              OR NEW.scope_version IS NOT OLD.scope_version
              OR NEW.operation_row_version IS NOT OLD.operation_row_version
              OR NEW.receipt_json IS NOT OLD.receipt_json
              OR NEW.queued_at IS NOT OLD.queued_at
              OR (OLD.reconciled_at IS NOT NULL AND NEW.reconciled_at IS NOT OLD.reconciled_at)
              OR (OLD.retired_at IS NOT NULL AND NEW.retired_at IS NOT OLD.retired_at)
              OR (NEW.reconciled_at IS NOT NULL AND NEW.retired_at IS NOT NULL)
            BEGIN SELECT RAISE(ABORT,'manager action notice reconciliation is forward-only'); END;
            CREATE TRIGGER harness_manager_action_notice_queue_no_delete
            BEFORE DELETE ON harness_manager_action_notice_queue
            BEGIN SELECT RAISE(ABORT,'manager action notice reconciliation is retained'); END;
            INSERT INTO harness_manager_action_notice_queue(
                operation_id,project_id,manager_session_id,scope_version,
                operation_row_version,receipt_json,queued_at,retired_at)
            SELECT id,project_id,manager_session_id,scope_version,row_version,
                outcome_json,updated_at,
                CASE WHEN EXISTS(
                    SELECT 1 FROM harness_manager_scopes scope
                    WHERE scope.project_id=harness_manager_v2_operations.project_id
                      AND scope.manager_session_id=harness_manager_v2_operations.manager_session_id
                      AND scope.row_version=harness_manager_v2_operations.scope_version
                ) THEN NULL ELSE updated_at END
            FROM harness_manager_v2_operations
            WHERE kind='lifecycle_action' AND state NOT IN ('queued','running');
            CREATE TABLE harness_manager_notices (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE
                    CHECK(rsi_uuid_is_canonical(id)),
                job_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(job_id)),
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                epic_id TEXT
                    CHECK(epic_id IS NULL OR rsi_uuid_is_canonical(epic_id)),
                direction TEXT NOT NULL CHECK(direction IN ('to_manager','to_lead')),
                source_session_id TEXT
                    CHECK(source_session_id IS NULL OR rsi_uuid_is_canonical(source_session_id)),
                recipient_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(recipient_session_id)),
                kind TEXT NOT NULL CHECK(kind IN ('session_state','message','action_result','operator_answer','ledger_change')),
                subject_id TEXT NOT NULL CHECK(length(subject_id) BETWEEN 1 AND 512),
                subject_version TEXT NOT NULL CHECK(length(subject_version) BETWEEN 1 AND 256),
                state_json TEXT NOT NULL
                    CHECK(json_valid(state_json) AND json_type(state_json)='object'
                          AND length(CAST(state_json AS BLOB))<=65536),
                recorded_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(recorded_at)),
                queued_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(queued_at)),
                delivered_at TEXT CHECK(delivered_at IS NULL OR rsi_rfc3339_nanos_is_canonical(delivered_at)),
                retrieved_at TEXT CHECK(retrieved_at IS NULL OR rsi_rfc3339_nanos_is_canonical(retrieved_at)),
                settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
                retired_at TEXT CHECK(retired_at IS NULL OR rsi_rfc3339_nanos_is_canonical(retired_at)),
                UNIQUE(job_id,kind,subject_id,subject_version),
                CHECK((settled_at IS NULL) = (retrieved_at IS NULL)),
                CHECK(retired_at IS NULL OR (retrieved_at IS NULL AND settled_at IS NULL))
            );
            CREATE INDEX harness_manager_notices_live_recipient
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    recipient_session_id,retired_at,settled_at,sequence);
            CREATE INDEX harness_manager_notices_pending_scope_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_direction_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_epic_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,epic_id,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_subject_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,subject_id,sequence)
                WHERE kind='message' AND retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_request_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,json_extract(state_json,'$.request_id'),sequence)
                WHERE kind='message' AND retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_undelivered_direction_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL AND delivered_at IS NULL;
            CREATE INDEX harness_manager_notices_subject_lookup
                ON harness_manager_notices(kind,subject_id,subject_version,
                    project_id,manager_session_id,scope_version,epic_id,direction);
            CREATE INDEX harness_manager_notices_unsettled_job
                ON harness_manager_notices(job_id,retired_at,settled_at,delivered_at,sequence);
            CREATE TABLE harness_manager_notice_transport_candidates (
                job_id TEXT PRIMARY KEY NOT NULL
                    CHECK(rsi_uuid_is_canonical(job_id)),
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                epic_id TEXT
                    CHECK(epic_id IS NULL OR rsi_uuid_is_canonical(epic_id)),
                first_sequence INTEGER NOT NULL CHECK(first_sequence > 0),
                queued_at TEXT NOT NULL
                    CHECK(rsi_rfc3339_nanos_is_canonical(queued_at))
            );
            CREATE INDEX harness_manager_notice_transport_candidate_page
                ON harness_manager_notice_transport_candidates(
                    project_id,manager_session_id,scope_version,first_sequence,job_id);
            CREATE TRIGGER harness_manager_notice_transport_candidate_after_insert
            AFTER INSERT ON harness_manager_notices
            WHEN NEW.direction='to_manager' AND NEW.retired_at IS NULL
              AND NEW.settled_at IS NULL AND NEW.delivered_at IS NULL
            BEGIN
                INSERT OR IGNORE INTO harness_manager_notice_transport_candidates(
                    job_id,project_id,manager_session_id,scope_version,epic_id,
                    first_sequence,queued_at)
                VALUES(NEW.job_id,NEW.project_id,NEW.manager_session_id,NEW.scope_version,
                    NEW.epic_id,NEW.sequence,NEW.queued_at);
            END;
            CREATE TRIGGER harness_manager_notice_transport_candidate_after_update
            AFTER UPDATE OF delivered_at,settled_at,retired_at ON harness_manager_notices
            WHEN NOT EXISTS(
                SELECT 1 FROM harness_manager_notices pending
                     INDEXED BY harness_manager_notices_unsettled_job
                WHERE pending.job_id=NEW.job_id AND pending.retired_at IS NULL
                  AND pending.direction='to_manager' AND pending.settled_at IS NULL
                  AND pending.delivered_at IS NULL)
            BEGIN
                DELETE FROM harness_manager_notice_transport_candidates
                WHERE job_id=NEW.job_id;
            END;
            CREATE TRIGGER harness_manager_notice_transport_candidates_no_update
            BEFORE UPDATE ON harness_manager_notice_transport_candidates
            BEGIN SELECT RAISE(ABORT,'manager notice transport candidates are immutable'); END;
            CREATE TABLE harness_manager_notice_reconcile_cursors (
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                lane TEXT NOT NULL CHECK(length(lane) BETWEEN 1 AND 64),
                owner_id TEXT NOT NULL DEFAULT ''
                    CHECK(owner_id='' OR rsi_uuid_is_canonical(owner_id)),
                after_key TEXT NOT NULL DEFAULT '' CHECK(length(after_key) <= 512),
                cycle INTEGER NOT NULL DEFAULT 0 CHECK(cycle >= 0),
                updated_at TEXT NOT NULL
                    CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
                PRIMARY KEY(project_id,manager_session_id,scope_version,lane,owner_id)
            );
            CREATE INDEX harness_manager_notice_group_epic_page
                ON sessions(parent_id,session_kind,id);
            CREATE INDEX harness_manager_notice_entity_epic_page
                ON harness_manager_v2_entities(
                    project_id,manager_session_id,scope_version,kind,session_id);
            CREATE INDEX harness_manager_notice_record_page
                ON harness_manager_v2_records(
                    project_id,manager_session_id,scope_version,epic_id,kind,
                    archived,record_key);
            CREATE TABLE harness_manager_notice_scope_retirements (
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                retired_at TEXT NOT NULL
                    CHECK(rsi_rfc3339_nanos_is_canonical(retired_at)),
                completed_at TEXT
                    CHECK(completed_at IS NULL OR rsi_rfc3339_nanos_is_canonical(completed_at)),
                PRIMARY KEY(project_id,manager_session_id,scope_version)
            );
            CREATE INDEX harness_manager_notice_scope_retirements_pending
                ON harness_manager_notice_scope_retirements(
                    retired_at,project_id,manager_session_id,scope_version)
                WHERE completed_at IS NULL;
            CREATE TRIGGER harness_manager_notices_validate_update
            BEFORE UPDATE ON harness_manager_notices
            WHEN NEW.id IS NOT OLD.id OR NEW.job_id IS NOT OLD.job_id
              OR NEW.project_id IS NOT OLD.project_id
              OR NEW.manager_session_id IS NOT OLD.manager_session_id
              OR NEW.scope_version IS NOT OLD.scope_version
              OR NEW.epic_id IS NOT OLD.epic_id OR NEW.direction IS NOT OLD.direction
              OR NEW.source_session_id IS NOT OLD.source_session_id
              OR NEW.recipient_session_id IS NOT OLD.recipient_session_id
              OR NEW.kind IS NOT OLD.kind OR NEW.subject_id IS NOT OLD.subject_id
              OR NEW.subject_version IS NOT OLD.subject_version
              OR NEW.state_json IS NOT OLD.state_json
              OR NEW.recorded_at IS NOT OLD.recorded_at OR NEW.queued_at IS NOT OLD.queued_at
              OR (OLD.delivered_at IS NOT NULL AND NEW.delivered_at IS NOT OLD.delivered_at)
              OR (OLD.retrieved_at IS NOT NULL AND NEW.retrieved_at IS NOT OLD.retrieved_at)
              OR (OLD.settled_at IS NOT NULL AND NEW.settled_at IS NOT OLD.settled_at)
              OR (OLD.retired_at IS NOT NULL AND NEW.retired_at IS NOT OLD.retired_at)
              OR NEW.retrieved_at IS NOT NEW.settled_at
              OR (NEW.retired_at IS NOT NULL
                  AND (NEW.retrieved_at IS NOT NULL OR NEW.settled_at IS NOT NULL))
            BEGIN SELECT RAISE(ABORT,'manager notice identity and lifecycle are forward-only'); END;
            CREATE TRIGGER harness_manager_notices_no_delete
            BEFORE DELETE ON harness_manager_notices
            BEGIN SELECT RAISE(ABORT,'manager notices are retained'); END;
            CREATE TRIGGER harness_manager_notice_scope_retirements_forward
            BEFORE UPDATE ON harness_manager_notice_scope_retirements
            WHEN NEW.project_id IS NOT OLD.project_id
              OR NEW.manager_session_id IS NOT OLD.manager_session_id
              OR NEW.scope_version IS NOT OLD.scope_version
              OR NEW.retired_at IS NOT OLD.retired_at
              OR OLD.completed_at IS NOT NULL OR NEW.completed_at IS NULL
            BEGIN SELECT RAISE(ABORT,'manager notice scope retirement is forward-only'); END;
            CREATE TRIGGER harness_manager_notice_scope_retirements_no_delete
            BEFORE DELETE ON harness_manager_notice_scope_retirements
            BEGIN SELECT RAISE(ABORT,'manager notice scope retirements are retained'); END;",
        )?;
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V117 manager notice migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 117", [])?;
        tx.commit()?;
        tracing::info!("V117 migration complete: durable manager notices installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v117-manager-notices-driver
}
