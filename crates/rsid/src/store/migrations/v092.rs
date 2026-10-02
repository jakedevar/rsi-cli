impl Store {
    fn migrate_v092(&self, version: i32) -> Result<()> {
        if version < 92 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 91 {
                return Err(DaemonError::Store(format!(
                    "V92 requires exact V91 source, found V{active_version}"
                )));
            }
            // V91 changes no DDL, so its authenticated source catalog is the
            // exact accepted V90 catalog plus the V91 version marker.
            h1_v91_validate_v90_catalog(&tx)?;
            h1_validate_exact_active_predecessors(&tx, 92, 91)?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterPreflight)?;

            tx.execute_batch(
                "CREATE TABLE epic_lead_generations (
                    epic_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(epic_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    generation INTEGER NOT NULL CHECK(generation >= 1),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
                );
                CREATE TABLE agent_successor_reservations (
                    reservation_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(reservation_id)),
                    predecessor_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(predecessor_session_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    epic_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(epic_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    candidate_session_id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(candidate_session_id)),
                    caller_key_digest TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(caller_key_digest)),
                    request_json TEXT NOT NULL CHECK(json_valid(request_json)),
                    request_fingerprint TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(request_fingerprint)),
                    candidate_kind TEXT NOT NULL CHECK(candidate_kind IN ('Story','Task','Bug','Feature','Refactor','Research')),
                    inherited_launch_json TEXT NOT NULL CHECK(json_valid(inherited_launch_json)),
                    expected_lead_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(expected_lead_session_id) AND expected_lead_session_id=predecessor_session_id),
                    expected_lead_generation INTEGER NOT NULL CHECK(expected_lead_generation >= 1),
                    state TEXT NOT NULL CHECK(state IN ('reserved','launching','committed','failed','uncertain')),
                    state_version INTEGER NOT NULL CHECK(state_version >= 1),
                    launch_attempt_id TEXT CHECK(launch_attempt_id IS NULL OR rsi_uuid_is_canonical(launch_attempt_id)),
                    model_invocation_id TEXT CHECK(model_invocation_id IS NULL OR rsi_uuid_is_canonical(model_invocation_id)),
                    establishment_evidence_json TEXT CHECK(establishment_evidence_json IS NULL OR json_valid(establishment_evidence_json)),
                    establishment_digest TEXT CHECK(establishment_digest IS NULL OR rsi_sha256_digest_is_canonical(establishment_digest)),
                    published_at TEXT CHECK(published_at IS NULL OR rsi_rfc3339_nanos_is_canonical(published_at)),
                    terminal_reason TEXT CHECK(terminal_reason IS NULL OR length(terminal_reason)<=256),
                    safe_error_class TEXT CHECK(safe_error_class IS NULL OR length(safe_error_class)<=128),
                    reserved_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(reserved_at)),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
                    UNIQUE(predecessor_session_id,caller_key_digest),
                    FOREIGN KEY(reservation_id,state_version)
                        REFERENCES agent_successor_transitions(reservation_id,state_version)
                        DEFERRABLE INITIALLY DEFERRED,
                    CHECK((launch_attempt_id IS NULL)=(model_invocation_id IS NULL)),
                    CHECK((establishment_evidence_json IS NULL)=(establishment_digest IS NULL)),
                    CHECK((terminal_reason IS NULL)=(safe_error_class IS NULL)),
                    CHECK(updated_at>=reserved_at),
                    CHECK(published_at IS NULL OR published_at>=reserved_at),
                    CHECK(
                        (state='reserved' AND launch_attempt_id IS NULL AND establishment_evidence_json IS NULL AND terminal_reason IS NULL)
                        OR (state='launching' AND launch_attempt_id IS NOT NULL AND establishment_evidence_json IS NULL AND terminal_reason IS NULL)
                        OR (state='uncertain' AND launch_attempt_id IS NOT NULL AND establishment_evidence_json IS NULL AND terminal_reason IS NOT NULL)
                        OR (state='committed' AND launch_attempt_id IS NOT NULL AND establishment_evidence_json IS NOT NULL AND terminal_reason IS NULL)
                        OR (state='failed' AND establishment_evidence_json IS NULL AND terminal_reason IS NOT NULL)
                    )
                );
                CREATE TABLE agent_successor_transitions (
                    transition_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(transition_id)),
                    reservation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(reservation_id)) REFERENCES agent_successor_reservations(reservation_id) ON DELETE RESTRICT,
                    state_version INTEGER NOT NULL CHECK(state_version >= 1),
                    from_state TEXT CHECK(from_state IS NULL OR from_state IN ('reserved','launching','uncertain')),
                    to_state TEXT NOT NULL CHECK(to_state IN ('reserved','launching','committed','failed','uncertain')),
                    authority_digest TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(authority_digest)),
                    launch_attempt_id TEXT CHECK(launch_attempt_id IS NULL OR rsi_uuid_is_canonical(launch_attempt_id)),
                    model_invocation_id TEXT CHECK(model_invocation_id IS NULL OR rsi_uuid_is_canonical(model_invocation_id)),
                    reason TEXT CHECK(reason IS NULL OR length(reason)<=256),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    UNIQUE(reservation_id,state_version),
                    CHECK((launch_attempt_id IS NULL)=(model_invocation_id IS NULL)),
                    CHECK(
                        (to_state='reserved' AND from_state IS NULL AND state_version=1 AND launch_attempt_id IS NULL AND reason IS NULL)
                        OR (to_state='launching' AND from_state='reserved' AND launch_attempt_id IS NOT NULL AND reason IS NULL)
                        OR (to_state='uncertain' AND from_state='launching' AND launch_attempt_id IS NOT NULL AND reason IS NOT NULL)
                        OR (to_state='committed' AND from_state IN ('launching','uncertain') AND launch_attempt_id IS NOT NULL AND reason IS NULL)
                        OR (to_state='failed' AND from_state IN ('reserved','launching','uncertain') AND reason IS NOT NULL)
                    )
                );
                CREATE INDEX idx_agent_successor_reconcile ON agent_successor_reservations(state,updated_at,reservation_id);
                CREATE INDEX idx_agent_successor_predecessor ON agent_successor_reservations(predecessor_session_id,reserved_at,reservation_id);
                CREATE INDEX idx_agent_successor_transitions_order ON agent_successor_transitions(reservation_id,state_version);

                CREATE TRIGGER epic_lead_generations_no_delete BEFORE DELETE ON epic_lead_generations BEGIN
                    SELECT RAISE(ABORT,'epic_lead_generation_no_delete');
                END;
                CREATE TRIGGER epic_lead_generations_epic_only_insert BEFORE INSERT ON epic_lead_generations
                WHEN NOT EXISTS(SELECT 1 FROM sessions WHERE id=NEW.epic_id AND session_kind='Epic') BEGIN
                    SELECT RAISE(ABORT,'epic_lead_generation_requires_epic');
                END;
                CREATE TRIGGER epic_lead_generations_forward_only BEFORE UPDATE ON epic_lead_generations
                WHEN NEW.epic_id!=OLD.epic_id OR NEW.generation!=OLD.generation+1 BEGIN
                    SELECT RAISE(ABORT,'epic_lead_generation_must_increment_once');
                END;
                CREATE TRIGGER sessions_epic_lead_generation_insert AFTER INSERT ON sessions WHEN NEW.session_kind='Epic' BEGIN
                    INSERT INTO epic_lead_generations(epic_id,generation,updated_at)
                    VALUES(NEW.id,1,strftime('%Y-%m-%dT%H:%M:%f000000Z','now'));
                END;
                CREATE TRIGGER sessions_epic_lead_generation_change AFTER UPDATE OF lead_session_id ON sessions
                WHEN OLD.session_kind='Epic' AND NEW.lead_session_id IS NOT OLD.lead_session_id BEGIN
                    UPDATE epic_lead_generations SET generation=generation+1,
                        updated_at=strftime('%Y-%m-%dT%H:%M:%f000000Z','now') WHERE epic_id=NEW.id;
                END;

                CREATE TRIGGER agent_successor_reservations_no_delete BEFORE DELETE ON agent_successor_reservations BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_no_delete');
                END;
                CREATE TRIGGER agent_successor_reservations_identity_immutable BEFORE UPDATE ON agent_successor_reservations
                WHEN NEW.reservation_id!=OLD.reservation_id OR NEW.predecessor_session_id!=OLD.predecessor_session_id
                  OR NEW.epic_id!=OLD.epic_id OR NEW.candidate_session_id!=OLD.candidate_session_id
                  OR NEW.caller_key_digest!=OLD.caller_key_digest OR NEW.request_json!=OLD.request_json
                  OR NEW.request_fingerprint!=OLD.request_fingerprint OR NEW.candidate_kind!=OLD.candidate_kind
                  OR NEW.inherited_launch_json!=OLD.inherited_launch_json OR NEW.expected_lead_session_id!=OLD.expected_lead_session_id
                  OR NEW.expected_lead_generation!=OLD.expected_lead_generation OR NEW.reserved_at!=OLD.reserved_at BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_identity_immutable');
                END;
                CREATE TRIGGER agent_successor_reservations_forward_state BEFORE UPDATE ON agent_successor_reservations
                WHEN NEW.state!=OLD.state AND (NEW.state_version!=OLD.state_version+1 OR NOT (
                    (OLD.state='reserved' AND NEW.state IN ('launching','failed')) OR
                    (OLD.state='launching' AND NEW.state IN ('committed','failed','uncertain')) OR
                    (OLD.state='uncertain' AND NEW.state IN ('committed','failed'))
                )) BEGIN SELECT RAISE(ABORT,'agent_successor_reservation_invalid_transition'); END;
                CREATE TRIGGER agent_successor_reservations_version_guard BEFORE UPDATE ON agent_successor_reservations
                WHEN NEW.state=OLD.state AND (
                    NEW.state_version!=OLD.state_version
                    OR NEW.launch_attempt_id IS NOT OLD.launch_attempt_id
                    OR NEW.model_invocation_id IS NOT OLD.model_invocation_id
                    OR NEW.terminal_reason IS NOT OLD.terminal_reason
                ) BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_version_without_transition');
                END;
                CREATE TRIGGER agent_successor_reservations_terminal BEFORE UPDATE ON agent_successor_reservations
                WHEN OLD.state IN ('committed','failed') AND (NEW.state!=OLD.state OR NEW.state_version!=OLD.state_version) BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_terminal');
                END;
                CREATE TRIGGER agent_successor_transitions_no_update BEFORE UPDATE ON agent_successor_transitions BEGIN
                    SELECT RAISE(ABORT,'agent_successor_transition_immutable');
                END;
                CREATE TRIGGER agent_successor_transitions_no_delete BEFORE DELETE ON agent_successor_transitions BEGIN
                    SELECT RAISE(ABORT,'agent_successor_transition_no_delete');
                END;
                CREATE TRIGGER agent_successor_transitions_match_current BEFORE INSERT ON agent_successor_transitions
                WHEN NOT EXISTS(
                    SELECT 1 FROM agent_successor_reservations reservation
                    WHERE reservation.reservation_id=NEW.reservation_id
                      AND reservation.state_version=NEW.state_version
                      AND reservation.state=NEW.to_state
                      AND reservation.launch_attempt_id IS NEW.launch_attempt_id
                      AND reservation.model_invocation_id IS NEW.model_invocation_id
                      AND reservation.terminal_reason IS NEW.reason
                      AND (
                          (NEW.state_version=1 AND NEW.from_state IS NULL)
                          OR EXISTS(
                              SELECT 1 FROM agent_successor_transitions previous
                              WHERE previous.reservation_id=NEW.reservation_id
                                AND previous.state_version=NEW.state_version-1
                                AND previous.to_state=NEW.from_state
                          )
                      )
                ) BEGIN
                    SELECT RAISE(ABORT,'agent_successor_transition_aggregate_mismatch');
                END;",
            )?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterSchema)?;
            tx.execute(
                "INSERT INTO epic_lead_generations(epic_id,generation,updated_at)
                 SELECT id,1,strftime('%Y-%m-%dT%H:%M:%f000000Z','now') FROM sessions WHERE session_kind='Epic'",
                [],
            )?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterLeadSeed)?;

            let missing_epics: i64 = tx.query_row(
                "SELECT count(*) FROM sessions s LEFT JOIN epic_lead_generations g ON g.epic_id=s.id
                 WHERE s.session_kind='Epic' AND g.epic_id IS NULL",
                [],
                |row| row.get(0),
            )?;
            if missing_epics != 0 {
                return Err(DaemonError::Store(format!(
                    "V92 lead-generation seed missed {missing_epics} Epic row(s)"
                )));
            }
            h1_v92_validate_catalog(&tx)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V92 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V92 successor-ledger foreign-key check found {foreign_key_errors} violation(s)"
                )));
            }
            h1_v92_migration_fault(H1V92MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 92", [])?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterUserVersion)?;
            h1_v92_migration_fault(H1V92MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V92 migration complete: master-successor ledger and lead fence");
        }

        Ok(())
    }
}
