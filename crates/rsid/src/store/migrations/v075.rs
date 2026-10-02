impl Store {
    fn migrate_v075(&self, version: i32) -> Result<()> {
        // V75: additive Idea identity and compatibility kernel (D01).
        if version < 75 {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS captures (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    creator_kind TEXT NOT NULL
                        CHECK(creator_kind IN ('operator', 'session', 'system')),
                    creator_id TEXT NOT NULL CHECK(length(trim(creator_id)) > 0),
                    captured_at TEXT NOT NULL CHECK(length(trim(captured_at)) > 0),
                    source_kind TEXT NOT NULL CHECK(source_kind IN (
                        'operator_input', 'session_artifact',
                        'imported_artifact', 'legacy_reference'
                    )),
                    raw_content_digest TEXT NOT NULL CHECK(
                        length(raw_content_digest) = 71
                        AND substr(raw_content_digest, 1, 7) = 'sha256:'
                        AND substr(raw_content_digest, 8) NOT GLOB '*[^0-9a-f]*'
                    ),
                    storage_policy_id TEXT NOT NULL
                        CHECK(length(trim(storage_policy_id)) > 0),
                    content_ref TEXT NOT NULL CHECK(
                        length(content_ref) = 77
                        AND substr(content_ref, 1, 13) = 'cas://sha256/'
                        AND substr(content_ref, 14) NOT GLOB '*[^0-9a-f]*'
                        AND content_ref = 'cas://sha256/' || substr(raw_content_digest, 8)
                    ),
                    UNIQUE(id, project_id),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_captures_project_captured_at
                    ON captures(project_id, captured_at, id);
                CREATE TRIGGER IF NOT EXISTS captures_immutable_update
                BEFORE UPDATE ON captures BEGIN
                    SELECT RAISE(ABORT, 'captures are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS captures_no_delete
                BEFORE DELETE ON captures BEGIN
                    SELECT RAISE(ABORT, 'captures cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS ideas (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    slug TEXT NOT NULL CHECK(
                        length(trim(slug)) > 0
                        AND length(CAST(slug AS BLOB)) <= 128
                    ),
                    sigil TEXT CHECK(
                        sigil IS NULL
                        OR (length(trim(sigil)) > 0
                            AND length(CAST(sigil AS BLOB)) <= 64)
                    ),
                    genesis_capture_id TEXT NOT NULL CHECK(
                        length(genesis_capture_id) = 36
                        AND genesis_capture_id = lower(genesis_capture_id)
                    ),
                    genesis_span_start INTEGER,
                    genesis_span_end INTEGER,
                    genesis_span_digest TEXT,
                    title TEXT NOT NULL CHECK(
                        length(trim(title)) > 0
                        AND length(CAST(title AS BLOB)) <= 512
                    ),
                    description TEXT NOT NULL
                        CHECK(length(CAST(description AS BLOB)) <= 65536),
                    portfolio_summary TEXT NOT NULL
                        CHECK(length(CAST(portfolio_summary AS BLOB)) <= 2048),
                    lifecycle TEXT NOT NULL CHECK(lifecycle IN (
                        'Open', 'Parked', 'Completed', 'Abandoned', 'Superseded'
                    )),
                    stage TEXT NOT NULL CHECK(stage IN (
                        'Captured', 'Shaping', 'Researching', 'Planned',
                        'Implementing', 'Integrating', 'Verifying', 'Released'
                    )),
                    priority INTEGER NOT NULL,
                    autonomy_policy TEXT NOT NULL CHECK(autonomy_policy IN (
                        'CaptureOnly', 'Research', 'PlanAndWait', 'Sandbox',
                        'IntegrateIdeaBranch', 'PromoteProjectTarget', 'ExternalEffects'
                    )),
                    integration_target_ref TEXT NOT NULL
                        CHECK(length(trim(integration_target_ref)) > 0),
                    program_template_policy_id TEXT CHECK(
                        program_template_policy_id IS NULL
                        OR length(trim(program_template_policy_id)) > 0
                    ),
                    current_controller_session_id TEXT CHECK(
                        current_controller_session_id IS NULL
                        OR (length(current_controller_session_id) = 36
                            AND current_controller_session_id = lower(current_controller_session_id))
                    ),
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch >= 0),
                    row_version INTEGER NOT NULL CHECK(row_version >= 0),
                    next_event_sequence INTEGER NOT NULL CHECK(next_event_sequence > 0),
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    updated_at TEXT NOT NULL CHECK(length(trim(updated_at)) > 0),
                    terminal_at TEXT,
                    superseded_at TEXT,
                    UNIQUE(id, project_id),
                    UNIQUE(project_id, slug),
                    CHECK(
                        (genesis_span_start IS NULL
                         AND genesis_span_end IS NULL
                         AND genesis_span_digest IS NULL)
                        OR
                        (genesis_span_start IS NOT NULL
                         AND genesis_span_end IS NOT NULL
                         AND genesis_span_digest IS NOT NULL
                         AND genesis_span_start >= 0
                         AND genesis_span_end > genesis_span_start
                         AND length(genesis_span_digest) = 71
                         AND substr(genesis_span_digest, 1, 7) = 'sha256:'
                         AND substr(genesis_span_digest, 8) NOT GLOB '*[^0-9a-f]*')
                    ),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(genesis_capture_id, project_id)
                        REFERENCES captures(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(current_controller_session_id)
                        REFERENCES sessions(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_ideas_project_state
                    ON ideas(project_id, lifecycle, stage, priority, id);
                CREATE INDEX IF NOT EXISTS idx_ideas_controller_session
                    ON ideas(current_controller_session_id);
                CREATE TRIGGER IF NOT EXISTS ideas_genesis_immutable
                BEFORE UPDATE ON ideas
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.genesis_capture_id != OLD.genesis_capture_id
                  OR NEW.genesis_span_start IS NOT OLD.genesis_span_start
                  OR NEW.genesis_span_end IS NOT OLD.genesis_span_end
                  OR NEW.genesis_span_digest IS NOT OLD.genesis_span_digest
                BEGIN
                    SELECT RAISE(ABORT, 'idea identity and genesis are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS ideas_no_delete
                BEFORE DELETE ON ideas BEGIN
                    SELECT RAISE(ABORT, 'ideas cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_events (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    idea_id TEXT NOT NULL
                        CHECK(length(idea_id) = 36 AND idea_id = lower(idea_id)),
                    sequence INTEGER NOT NULL CHECK(sequence > 0),
                    event_type TEXT NOT NULL CHECK(event_type IN (
                        'created', 'decision_recorded', 'projection_changed',
                        'lifecycle_transitioned', 'stage_transitioned',
                        'autonomy_changed', 'scope_changed', 'controller_reserved',
                        'controller_assigned', 'controller_released', 'question_asked',
                        'question_answered', 'issue_linked', 'artifact_sealed',
                        'finding_recorded', 'verdict_recorded', 'program_transitioned',
                        'gate_transitioned', 'integration_recorded', 'release_recorded',
                        'relationship_changed', 'split', 'superseded', 'failed', 'abandoned'
                    )),
                    actor_kind TEXT NOT NULL
                        CHECK(actor_kind IN ('operator', 'session', 'system')),
                    actor_id TEXT NOT NULL CHECK(length(trim(actor_id)) > 0),
                    controller_session_id TEXT CHECK(
                        controller_session_id IS NULL
                        OR (length(controller_session_id) = 36
                            AND controller_session_id = lower(controller_session_id))
                    ),
                    controller_epoch INTEGER CHECK(
                        controller_epoch IS NULL OR controller_epoch >= 0
                    ),
                    expected_row_version INTEGER NOT NULL
                        CHECK(expected_row_version >= 0),
                    resulting_row_version INTEGER NOT NULL
                        CHECK(resulting_row_version >= 0),
                    idempotency_key TEXT NOT NULL
                        CHECK(length(trim(idempotency_key)) > 0),
                    occurred_at TEXT NOT NULL CHECK(length(trim(occurred_at)) > 0),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
                    artifact_digests_json TEXT NOT NULL
                        CHECK(json_valid(artifact_digests_json)),
                    evidence_digests_json TEXT NOT NULL
                        CHECK(json_valid(evidence_digests_json)),
                    UNIQUE(id, project_id),
                    UNIQUE(id, idea_id, project_id),
                    UNIQUE(idea_id, sequence),
                    UNIQUE(idea_id, idempotency_key),
                    FOREIGN KEY(idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id)
                        REFERENCES sessions(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_events_idea_sequence
                    ON idea_events(idea_id, sequence);
                CREATE TRIGGER IF NOT EXISTS idea_events_append_only_update
                BEFORE UPDATE ON idea_events BEGIN
                    SELECT RAISE(ABORT, 'idea events are append-only');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_events_no_delete
                BEFORE DELETE ON idea_events BEGIN
                    SELECT RAISE(ABORT, 'idea events cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_relationships (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    source_idea_id TEXT NOT NULL,
                    target_idea_id TEXT NOT NULL,
                    kind TEXT NOT NULL
                        CHECK(kind IN ('depends_on', 'supersedes', 'derived_from')),
                    created_event_id TEXT NOT NULL,
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    removed_event_id TEXT,
                    removed_at TEXT,
                    CHECK(source_idea_id != target_idea_id),
                    CHECK(
                        (removed_event_id IS NULL AND removed_at IS NULL)
                        OR (removed_event_id IS NOT NULL AND removed_at IS NOT NULL)
                    ),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(source_idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(target_idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(created_event_id, source_idea_id, project_id)
                        REFERENCES idea_events(id, idea_id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(removed_event_id, source_idea_id, project_id)
                        REFERENCES idea_events(id, idea_id, project_id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_relationships_source
                    ON idea_relationships(source_idea_id, kind, target_idea_id);
                CREATE INDEX IF NOT EXISTS idx_idea_relationships_target
                    ON idea_relationships(target_idea_id, kind, source_idea_id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_idea_relationships_active
                    ON idea_relationships(source_idea_id, target_idea_id, kind)
                    WHERE removed_at IS NULL;
                CREATE TRIGGER IF NOT EXISTS idea_relationships_identity_immutable
                BEFORE UPDATE ON idea_relationships
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.source_idea_id != OLD.source_idea_id
                  OR NEW.target_idea_id != OLD.target_idea_id
                  OR NEW.kind != OLD.kind
                  OR NEW.created_event_id != OLD.created_event_id
                  OR NEW.created_at != OLD.created_at
                  OR (OLD.removed_event_id IS NOT NULL
                      AND (NEW.removed_event_id IS NOT OLD.removed_event_id
                           OR NEW.removed_at IS NOT OLD.removed_at))
                BEGIN
                    SELECT RAISE(ABORT, 'relationship identity and creation are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_relationships_no_delete
                BEFORE DELETE ON idea_relationships BEGIN
                    SELECT RAISE(ABORT, 'idea relationships cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_collections (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    slug TEXT NOT NULL CHECK(
                        length(trim(slug)) > 0
                        AND length(CAST(slug AS BLOB)) <= 128
                    ),
                    name TEXT NOT NULL CHECK(
                        length(trim(name)) > 0
                        AND length(CAST(name AS BLOB)) <= 512
                    ),
                    description TEXT CHECK(
                        description IS NULL
                        OR length(CAST(description AS BLOB)) <= 65536
                    ),
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    updated_at TEXT NOT NULL CHECK(length(trim(updated_at)) > 0),
                    retired_at TEXT,
                    UNIQUE(id, project_id),
                    UNIQUE(project_id, slug),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_collections_project
                    ON idea_collections(project_id, retired_at, slug, id);
                CREATE TRIGGER IF NOT EXISTS idea_collections_no_delete
                BEFORE DELETE ON idea_collections BEGIN
                    SELECT RAISE(ABORT, 'idea collections cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_collection_memberships (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    collection_id TEXT NOT NULL,
                    idea_id TEXT NOT NULL,
                    added_at TEXT NOT NULL CHECK(length(trim(added_at)) > 0),
                    removed_at TEXT,
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(collection_id, project_id)
                        REFERENCES idea_collections(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_memberships_collection
                    ON idea_collection_memberships(collection_id, removed_at, idea_id);
                CREATE INDEX IF NOT EXISTS idx_idea_memberships_idea
                    ON idea_collection_memberships(idea_id, removed_at, collection_id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_idea_memberships_active
                    ON idea_collection_memberships(collection_id, idea_id)
                    WHERE removed_at IS NULL;
                CREATE TRIGGER IF NOT EXISTS idea_memberships_identity_immutable
                BEFORE UPDATE ON idea_collection_memberships
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.collection_id != OLD.collection_id
                  OR NEW.idea_id != OLD.idea_id
                  OR NEW.added_at != OLD.added_at
                  OR (OLD.removed_at IS NOT NULL
                      AND NEW.removed_at IS NOT OLD.removed_at)
                BEGIN
                    SELECT RAISE(ABORT, 'membership identity and creation are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_memberships_no_delete
                BEFORE DELETE ON idea_collection_memberships BEGIN
                    SELECT RAISE(ABORT, 'idea collection memberships cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_compatibility_mappings (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    legacy_source_kind TEXT NOT NULL CHECK(legacy_source_kind IN (
                        'session_group', 'session_epic', 'session_label', 'session_child'
                    )),
                    legacy_source_id TEXT NOT NULL CHECK(
                        length(legacy_source_id) = 36
                        AND legacy_source_id = lower(legacy_source_id)
                    ),
                    idea_id TEXT,
                    collection_id TEXT,
                    status TEXT NOT NULL
                        CHECK(status IN ('pending', 'mapped', 'blocked', 'excluded')),
                    provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json)),
                    disposition TEXT,
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    updated_at TEXT NOT NULL CHECK(length(trim(updated_at)) > 0),
                    mapped_at TEXT,
                    UNIQUE(legacy_source_kind, legacy_source_id),
                    CHECK(idea_id IS NULL OR collection_id IS NULL),
                    CHECK(
                        (status = 'mapped'
                         AND (idea_id IS NOT NULL OR collection_id IS NOT NULL)
                         AND mapped_at IS NOT NULL)
                        OR
                        (status != 'mapped'
                         AND idea_id IS NULL
                         AND collection_id IS NULL
                         AND mapped_at IS NULL)
                    ),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(collection_id, project_id)
                        REFERENCES idea_collections(id, project_id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_compatibility_project_status
                    ON idea_compatibility_mappings(project_id, status, id);
                CREATE INDEX IF NOT EXISTS idx_idea_compatibility_idea
                    ON idea_compatibility_mappings(idea_id);
                CREATE INDEX IF NOT EXISTS idx_idea_compatibility_collection
                    ON idea_compatibility_mappings(collection_id);
                CREATE TRIGGER IF NOT EXISTS idea_compatibility_source_immutable
                BEFORE UPDATE ON idea_compatibility_mappings
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.legacy_source_kind != OLD.legacy_source_kind
                  OR NEW.legacy_source_id != OLD.legacy_source_id
                  OR NEW.created_at != OLD.created_at
                BEGIN
                    SELECT RAISE(ABORT, 'compatibility source identity is immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_compatibility_no_delete
                BEFORE DELETE ON idea_compatibility_mappings BEGIN
                    SELECT RAISE(ABORT, 'idea compatibility mappings cannot be deleted');
                END;
                ",
            )?;
            tx.execute("PRAGMA user_version = 75", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
