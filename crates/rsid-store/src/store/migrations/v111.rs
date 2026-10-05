impl Store {
    fn migrate_v111(&self, version: i32) -> Result<()> {
        // V111: root-manager succession has a durable occurrence, independent
        // authority epoch and retained accounting. Existing receipt resolution
        // remains the sole current-manager authority pointer.
        if version < 111 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE manager_authority_epochs (
                    project_id TEXT PRIMARY KEY REFERENCES projects(id),
                    epoch INTEGER NOT NULL CHECK(typeof(epoch)='integer' AND epoch>0)
                 );
                 INSERT INTO manager_authority_epochs SELECT id,1 FROM projects;
                 CREATE TRIGGER manager_epoch_project AFTER INSERT ON projects BEGIN
                    INSERT INTO manager_authority_epochs VALUES(NEW.id,1);
                 END;
                 CREATE TRIGGER manager_epoch_scope AFTER UPDATE ON harness_manager_scopes BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=NEW.project_id;
                 END;
                 CREATE TRIGGER manager_epoch_scope_insert AFTER INSERT ON harness_manager_scopes BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=NEW.project_id;
                 END;
                 CREATE TRIGGER manager_epoch_edge AFTER INSERT ON harness_manager_rotation_edges BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=(SELECT project_id FROM sessions WHERE id=NEW.predecessor_session_id);
                 END;
                 CREATE TRIGGER manager_epoch_retire AFTER UPDATE OF retired_at ON harness_manager_rotation_edges
                 WHEN OLD.retired_at IS NULL AND NEW.retired_at IS NOT NULL BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=(SELECT project_id FROM sessions WHERE id=NEW.predecessor_session_id);
                 END;
                 CREATE TRIGGER manager_epoch_restore AFTER UPDATE OF status ON sessions
                 WHEN OLD.status='Archived' AND NEW.status<>'Archived' BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=NEW.project_id;
                 END;
                 CREATE TABLE manager_root_successions (
                    operation_id TEXT PRIMARY KEY REFERENCES harness_manager_v2_operations(id),
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    predecessor_session_id TEXT NOT NULL REFERENCES sessions(id),
                    candidate_session_id TEXT NOT NULL UNIQUE,
                    launch_attempt_id TEXT NOT NULL UNIQUE,
                    model_invocation_id TEXT NOT NULL UNIQUE,
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    policy_version INTEGER NOT NULL CHECK(policy_version>0),
                    authority_epoch INTEGER NOT NULL CHECK(authority_epoch>0),
                    creation_quantity INTEGER NOT NULL DEFAULT 1 CHECK(creation_quantity=1),
                    recovery_quantity INTEGER NOT NULL DEFAULT 0 CHECK(recovery_quantity=0),
                    frozen_json TEXT NOT NULL CHECK(json_valid(frozen_json) AND length(frozen_json)<=65536),
                    state TEXT NOT NULL CHECK(state IN ('reserved','executing','established','cleanup_required','committed','failed','blocked','revoked')),
                    row_version INTEGER NOT NULL CHECK(typeof(row_version)='integer' AND row_version>0),
                    claim_boot_id TEXT,
                    settled_json TEXT CHECK(settled_json IS NULL OR json_valid(settled_json)),
                    candidate_json TEXT CHECK(candidate_json IS NULL OR json_valid(candidate_json)),
                    admission_recorded INTEGER NOT NULL DEFAULT 0 CHECK(admission_recorded IN (0,1)),
                    effect_claimed INTEGER NOT NULL DEFAULT 0 CHECK(effect_claimed IN (0,1)),
                    establishment_json TEXT CHECK(establishment_json IS NULL OR json_valid(establishment_json)),
                    published_epoch INTEGER CHECK(published_epoch>0),
                    reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    CHECK(candidate_session_id<>predecessor_session_id)
                 );
                 CREATE UNIQUE INDEX manager_root_unresolved ON manager_root_successions(project_id,manager_session_id)
                    WHERE state IN ('reserved','executing','established','cleanup_required');
                 CREATE INDEX manager_root_recovery ON manager_root_successions(state,operation_id);
                 CREATE TRIGGER manager_root_immutable BEFORE UPDATE ON manager_root_successions
                 WHEN NEW.operation_id IS NOT OLD.operation_id OR NEW.project_id IS NOT OLD.project_id
                    OR NEW.manager_session_id IS NOT OLD.manager_session_id OR NEW.predecessor_session_id IS NOT OLD.predecessor_session_id
                    OR NEW.candidate_session_id IS NOT OLD.candidate_session_id OR NEW.launch_attempt_id IS NOT OLD.launch_attempt_id
                    OR NEW.model_invocation_id IS NOT OLD.model_invocation_id OR NEW.scope_version IS NOT OLD.scope_version
                    OR NEW.policy_version IS NOT OLD.policy_version OR NEW.authority_epoch IS NOT OLD.authority_epoch
                    OR NEW.creation_quantity IS NOT OLD.creation_quantity OR NEW.recovery_quantity IS NOT OLD.recovery_quantity
                    OR NEW.frozen_json IS NOT OLD.frozen_json OR NEW.created_at IS NOT OLD.created_at
                    OR NEW.admission_recorded<OLD.admission_recorded OR NEW.effect_claimed<OLD.effect_claimed
                    OR (OLD.settled_json IS NOT NULL AND NEW.settled_json IS NOT OLD.settled_json)
                    OR (OLD.candidate_json IS NOT NULL AND NEW.candidate_json IS NOT OLD.candidate_json)
                    OR (OLD.establishment_json IS NOT NULL AND NEW.establishment_json IS NOT OLD.establishment_json)
                    OR (OLD.published_epoch IS NOT NULL AND NEW.published_epoch IS NOT OLD.published_epoch)
                    OR NEW.row_version<>OLD.row_version+1
                 BEGIN SELECT RAISE(ABORT,'manager_root_immutable'); END;
                 CREATE TRIGGER manager_root_state BEFORE UPDATE OF state ON manager_root_successions
                 WHEN NOT (NEW.state=OLD.state OR
                    (OLD.state='reserved' AND NEW.state IN ('executing','blocked','revoked','failed')) OR
                    (OLD.state='executing' AND NEW.state='reserved'
                     AND OLD.claim_boot_id IS NOT NULL AND NEW.claim_boot_id IS NULL
                     AND OLD.admission_recorded=0 AND NEW.admission_recorded=0
                     AND OLD.effect_claimed=0 AND NEW.effect_claimed=0
                     AND OLD.settled_json IS NULL AND NEW.settled_json IS NULL
                     AND OLD.candidate_json IS NULL AND NEW.candidate_json IS NULL
                     AND OLD.establishment_json IS NULL AND NEW.establishment_json IS NULL
                     AND OLD.published_epoch IS NULL AND NEW.published_epoch IS NULL) OR
                    (OLD.state='executing' AND NEW.state IN ('established','cleanup_required','blocked','revoked','failed')) OR
                    (OLD.state='established' AND NEW.state IN ('committed','cleanup_required')) OR
                    (OLD.state='cleanup_required' AND NEW.state IN ('failed','revoked')))
                 BEGIN SELECT RAISE(ABORT,'manager_root_state'); END;
                 CREATE TRIGGER manager_root_no_delete BEFORE DELETE ON manager_root_successions
                 BEGIN SELECT RAISE(ABORT,'manager_root_retained'); END;
                 CREATE TABLE manager_root_transitions (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    operation_id TEXT NOT NULL REFERENCES manager_root_successions(operation_id),
                    row_version INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    reason TEXT,
                    created_at TEXT NOT NULL,
                    UNIQUE(operation_id,row_version)
                 );
                 CREATE TRIGGER manager_root_transition_insert AFTER INSERT ON manager_root_successions BEGIN
                    INSERT INTO manager_root_transitions(operation_id,row_version,state,reason,created_at)
                    VALUES(NEW.operation_id,NEW.row_version,NEW.state,NEW.reason,NEW.updated_at);
                 END;
                 CREATE TRIGGER manager_root_transition_update AFTER UPDATE ON manager_root_successions BEGIN
                    INSERT INTO manager_root_transitions(operation_id,row_version,state,reason,created_at)
                    VALUES(NEW.operation_id,NEW.row_version,NEW.state,NEW.reason,NEW.updated_at);
                 END;
                 CREATE TRIGGER manager_root_transition_no_update BEFORE UPDATE ON manager_root_transitions
                 BEGIN SELECT RAISE(ABORT,'manager_root_audit_retained'); END;
                 CREATE TRIGGER manager_root_transition_no_delete BEFORE DELETE ON manager_root_transitions
                 BEGIN SELECT RAISE(ABORT,'manager_root_audit_retained'); END;
                 CREATE TABLE manager_root_resource_origins (
                    session_id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    operation_id TEXT UNIQUE REFERENCES manager_root_successions(operation_id),
                    predecessor_session_id TEXT REFERENCES sessions(id),
                    model_invocation_id TEXT UNIQUE,
                    provider TEXT NOT NULL,
                    zero_origin INTEGER NOT NULL CHECK(zero_origin IN (0,1)),
                    known_floor_usd REAL NOT NULL CHECK(known_floor_usd>=0),
                    created_at TEXT NOT NULL,
                    CHECK((operation_id IS NULL AND zero_origin=0) OR (operation_id IS NOT NULL AND model_invocation_id IS NOT NULL))
                 );
                 CREATE INDEX manager_root_resource_project ON manager_root_resource_origins(project_id,session_id);
                 CREATE TRIGGER manager_root_resource_immutable BEFORE UPDATE ON manager_root_resource_origins
                 WHEN NEW.session_id IS NOT OLD.session_id OR NEW.project_id IS NOT OLD.project_id
                    OR NEW.manager_session_id IS NOT OLD.manager_session_id OR NEW.scope_version IS NOT OLD.scope_version
                    OR NEW.operation_id IS NOT OLD.operation_id OR NEW.predecessor_session_id IS NOT OLD.predecessor_session_id
                    OR NEW.model_invocation_id IS NOT OLD.model_invocation_id OR NEW.provider IS NOT OLD.provider
                    OR NEW.zero_origin IS NOT OLD.zero_origin OR NEW.known_floor_usd<OLD.known_floor_usd OR NEW.created_at IS NOT OLD.created_at
                 BEGIN SELECT RAISE(ABORT,'manager_root_resource_immutable'); END;
                 CREATE TRIGGER manager_root_resource_no_delete BEFORE DELETE ON manager_root_resource_origins
                 BEGIN SELECT RAISE(ABORT,'manager_root_resource_retained'); END;",
            )?;
            tx.execute("PRAGMA user_version = 111", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
