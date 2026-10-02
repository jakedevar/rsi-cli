impl Store {
    fn migrate_v083(&self, version: i32) -> Result<()> {
        if version < 83 {
            // V83 is deliberately additive. The V80--V82 catalog is accepted
            // history and remains untouched; every legacy session insert is
            // covered by the projection trigger below.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 82 {
                return Err(DaemonError::Store(format!(
                    "V83 requires exact V82 source, found V{active_version}"
                )));
            }
            let v82_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
            if v82_fingerprint != agent_coordination::AGENT_MESSAGE_V82_PINNED_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V83 requires the exact accepted V82 catalog, found {v82_fingerprint}"
                )));
            }
            tx.execute_batch(
                "
                CREATE TABLE sandbox_custody_roots (
                    custody_id TEXT PRIMARY KEY,
                    canonical_repo_dir TEXT NOT NULL,
                    sandbox_root TEXT NOT NULL UNIQUE,
                    sandbox_branch TEXT NOT NULL,
                    repository_identity TEXT NOT NULL,
                    source_commit TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('live','purged','failed','quarantined')),
                    owner_session_id TEXT UNIQUE REFERENCES sessions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
                    generation INTEGER NOT NULL CHECK (generation > 0),
                    event_sequence INTEGER NOT NULL CHECK (event_sequence > 0),
                    validation_state TEXT NOT NULL CHECK (validation_state IN ('verified','unverified','invalid')),
                    validated_generation INTEGER,
                    validated_at TEXT,
                    validation_error_code TEXT,
                    effect_boot_id TEXT,
                    reserved_effects INTEGER NOT NULL DEFAULT 0 CHECK (reserved_effects >= 0),
                    active_effects INTEGER NOT NULL DEFAULT 0 CHECK (active_effects >= 0),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    tombstoned_at TEXT,
                    UNIQUE (custody_id, generation),
                    CHECK ((state='live' AND owner_session_id IS NOT NULL AND tombstoned_at IS NULL)
                        OR (state IN ('purged','failed','quarantined') AND owner_session_id IS NULL)),
                    CHECK ((validation_state='verified' AND validated_generation=generation AND validated_at IS NOT NULL AND validation_error_code IS NULL)
                        OR (validation_state='unverified' AND validated_generation IS NULL AND validated_at IS NULL AND validation_error_code IS NULL)
                        OR (validation_state='invalid' AND validated_generation=generation AND validated_at IS NOT NULL AND validation_error_code IS NOT NULL)),
                    CHECK ((reserved_effects=0 AND active_effects=0 AND effect_boot_id IS NULL)
                        OR ((reserved_effects>0 OR active_effects>0) AND effect_boot_id IS NOT NULL)),
                    CHECK (state='live' OR (reserved_effects=0 AND active_effects=0))
                );
                ALTER TABLE sessions ADD COLUMN sandbox_custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;
                CREATE TABLE sandbox_custody_events (
                    event_id TEXT PRIMARY KEY,
                    custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    sequence INTEGER NOT NULL CHECK (sequence > 0),
                    event_kind TEXT NOT NULL CHECK (event_kind IN ('allocated','transferred','validation_failed','tombstoned','failed','quarantined')),
                    cause TEXT NOT NULL CHECK (cause IN ('fresh_launch','agent_spawn_child','rotation','retry','agent_fresh','codex_app_server_replacement','recursive_live','startup_reconciliation','purge','cleanup_failure','effect_revalidation')),
                    from_generation INTEGER,
                    to_generation INTEGER NOT NULL CHECK (to_generation > 0),
                    from_owner_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
                    to_owner_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
                    origin_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
                    scheduled_job_id TEXT REFERENCES scheduled_jobs(id) ON DELETE RESTRICT,
                    prior_state TEXT,
                    next_state TEXT NOT NULL CHECK (next_state IN ('live','purged','failed','quarantined')),
                    error_code TEXT,
                    occurred_at TEXT NOT NULL,
                    UNIQUE (custody_id, sequence),
                    CHECK (prior_state IS NULL OR prior_state IN ('live','purged','failed','quarantined')),
                    CHECK ((event_kind='allocated' AND from_generation IS NULL AND to_generation=1)
                        OR (event_kind='validation_failed' AND from_generation=to_generation)
                        OR (event_kind NOT IN ('allocated','validation_failed') AND from_generation IS NOT NULL AND to_generation=from_generation+1)),
                    CHECK ((event_kind='allocated' AND from_owner_session_id IS NULL AND to_owner_session_id IS NOT NULL AND next_state='live')
                        OR (event_kind='transferred' AND from_owner_session_id IS NOT NULL AND to_owner_session_id IS NOT NULL AND from_owner_session_id!=to_owner_session_id AND next_state='live')
                        OR (event_kind='validation_failed' AND from_owner_session_id IS NOT NULL AND to_owner_session_id=from_owner_session_id AND from_generation=to_generation AND error_code IS NOT NULL)
                        OR (event_kind IN ('tombstoned','failed','quarantined') AND to_owner_session_id IS NULL AND next_state!='live')),
                    CHECK (cause!='agent_fresh' OR (origin_session_id IS NOT NULL AND scheduled_job_id IS NOT NULL))
                );
                CREATE TABLE session_execution_projections (
                    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
                    schema_version INTEGER NOT NULL CHECK (schema_version=1),
                    projection_version INTEGER NOT NULL CHECK (projection_version=1),
                    execution_state TEXT NOT NULL CHECK (execution_state IN ('ordinary_unsandboxed','live_sandboxed','historical_purged','historical_transferred','historical_cleanup_failed','quarantined','invalid')),
                    freshness TEXT NOT NULL CHECK (freshness IN ('verified','unverified','invalid')),
                    canonical_repo_dir TEXT NOT NULL,
                    effective_cwd TEXT,
                    custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    custody_generation INTEGER,
                    validated_at TEXT,
                    error_code TEXT,
                    updated_at TEXT NOT NULL,
                    CHECK ((effective_cwd IS NOT NULL AND freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed'))
                        OR (effective_cwd IS NULL AND NOT (freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed')))),
                    CHECK ((freshness='verified' AND validated_at IS NOT NULL AND error_code IS NULL)
                        OR (freshness='unverified' AND validated_at IS NULL AND error_code IS NULL AND effective_cwd IS NULL)
                        OR (freshness='invalid' AND validated_at IS NOT NULL AND error_code IS NOT NULL AND effective_cwd IS NULL))
                );
                CREATE INDEX idx_sandbox_custody_roots_owner ON sandbox_custody_roots(owner_session_id);
                CREATE INDEX idx_sandbox_custody_roots_state ON sandbox_custody_roots(state, validation_state);
                CREATE INDEX idx_sandbox_custody_events_order ON sandbox_custody_events(custody_id, sequence);
                CREATE INDEX idx_sandbox_custody_events_to_owner ON sandbox_custody_events(custody_id, to_owner_session_id);
                CREATE INDEX idx_sessions_sandbox_custody_id_id ON sessions(sandbox_custody_id, id);
                CREATE INDEX idx_sessions_sandbox_root_id ON sessions(sandbox_root, id);
                CREATE INDEX idx_sessions_startup_unlinked_root
                    ON sessions(sandbox_root)
                    WHERE sandbox_root IS NOT NULL AND sandbox_custody_id IS NULL;
                CREATE INDEX idx_sessions_startup_unlinked_rootless
                    ON sessions(id)
                    WHERE sandbox_root IS NULL AND sandbox_custody_id IS NULL;
                CREATE INDEX idx_session_execution_projections_custody ON session_execution_projections(custody_id, session_id);

                CREATE TRIGGER sandbox_custody_roots_no_delete BEFORE DELETE ON sandbox_custody_roots BEGIN SELECT RAISE(ABORT, 'sandbox custody roots are immutable history'); END;
                CREATE TRIGGER sandbox_custody_events_no_delete BEFORE DELETE ON sandbox_custody_events BEGIN SELECT RAISE(ABORT, 'sandbox custody events are immutable history'); END;
                CREATE TRIGGER session_execution_projections_no_delete BEFORE DELETE ON session_execution_projections BEGIN SELECT RAISE(ABORT, 'sandbox custody projections are retained history'); END;
                CREATE TRIGGER sandbox_custody_events_immutable BEFORE UPDATE ON sandbox_custody_events BEGIN SELECT RAISE(ABORT, 'sandbox custody events are immutable'); END;
                CREATE TRIGGER sandbox_custody_roots_identity_immutable BEFORE UPDATE ON sandbox_custody_roots
                WHEN NEW.custody_id != OLD.custody_id OR NEW.canonical_repo_dir != OLD.canonical_repo_dir OR NEW.sandbox_root != OLD.sandbox_root OR NEW.sandbox_branch != OLD.sandbox_branch OR NEW.repository_identity != OLD.repository_identity OR NEW.source_commit != OLD.source_commit OR NEW.created_at != OLD.created_at
                BEGIN SELECT RAISE(ABORT, 'sandbox custody root identity is immutable'); END;
                CREATE TRIGGER sandbox_custody_roots_monotonic BEFORE UPDATE ON sandbox_custody_roots
                WHEN NEW.generation < OLD.generation OR NEW.event_sequence < OLD.event_sequence
                BEGIN SELECT RAISE(ABORT, 'sandbox custody generation and event sequence are forward only'); END;
                CREATE TRIGGER sandbox_custody_roots_transition_event BEFORE UPDATE ON sandbox_custody_roots
                WHEN (NEW.owner_session_id IS NOT OLD.owner_session_id OR NEW.state != OLD.state) AND (NEW.generation != OLD.generation + 1 OR NEW.event_sequence != OLD.event_sequence + 1 OR NOT EXISTS (SELECT 1 FROM sandbox_custody_events e WHERE e.custody_id=OLD.custody_id AND e.sequence=NEW.event_sequence AND e.to_generation=NEW.generation))
                BEGIN SELECT RAISE(ABORT, 'sandbox custody transition requires next immutable event'); END;
                CREATE TRIGGER sessions_execution_projection_after_insert AFTER INSERT ON sessions BEGIN
                    INSERT INTO session_execution_projections (session_id,schema_version,projection_version,execution_state,freshness,canonical_repo_dir,effective_cwd,custody_id,custody_generation,validated_at,error_code,updated_at)
                    VALUES (NEW.id,1,1,CASE WHEN NEW.sandbox_kind IS NULL AND NEW.sandbox_root IS NULL AND NEW.sandbox_branch IS NULL AND NEW.sandbox_cleanup_state IS NULL THEN 'ordinary_unsandboxed' WHEN NEW.sandbox_cleanup_state='Purged' THEN 'historical_purged' WHEN NEW.sandbox_cleanup_state='Failed' THEN 'historical_cleanup_failed' WHEN NEW.sandbox_kind IS NOT NULL AND NEW.sandbox_root IS NOT NULL AND NEW.sandbox_branch IS NOT NULL AND NEW.sandbox_cleanup_state='Live' THEN 'live_sandboxed' ELSE 'invalid' END,'unverified',NEW.working_dir,NULL,NULL,NULL,NULL,NULL,NEW.updated_at);
                END;
                INSERT INTO session_execution_projections (session_id,schema_version,projection_version,execution_state,freshness,canonical_repo_dir,effective_cwd,custody_id,custody_generation,validated_at,error_code,updated_at)
                SELECT id,1,1,CASE WHEN sandbox_kind IS NULL AND sandbox_root IS NULL AND sandbox_branch IS NULL AND sandbox_cleanup_state IS NULL THEN 'ordinary_unsandboxed' WHEN sandbox_cleanup_state='Purged' THEN 'historical_purged' WHEN sandbox_cleanup_state='Failed' THEN 'historical_cleanup_failed' WHEN sandbox_kind IS NOT NULL AND sandbox_root IS NOT NULL AND sandbox_branch IS NOT NULL AND sandbox_cleanup_state='Live' THEN 'live_sandboxed' ELSE 'invalid' END,'unverified',working_dir,NULL,NULL,NULL,NULL,NULL,updated_at FROM sessions;
                ",
            )?;
            tx.execute("PRAGMA user_version = 83", [])?;
            tx.commit()?;
            tracing::info!(
                "V83 migration complete: sandbox custody roots and execution projections"
            );
        }

        Ok(())
    }
}
