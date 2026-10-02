impl Store {
    fn migrate_v088(&self, version: i32) -> Result<()> {
        if version < 88 {
            // V88 changes claim CHECKs and the general claim/authority state
            // guards. Authenticate the complete deployed V87 origin catalog,
            // then rebuild all five mutually-referencing relations without
            // rewriting any historical V85/V86/V87 literal.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 87 {
                return Err(DaemonError::Store(format!(
                    "V88 requires exact V87 source, found V{active_version}"
                )));
            }
            origin_authority::validate_v87_catalog(&tx)?;
            let source_integrity: String =
                tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if source_integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V88 requires integrity_check=ok for exact V87 source, got {source_integrity}"
                )));
            }
            let source_foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if source_foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 requires clean V87 foreign keys, found {source_foreign_key_errors} violation(s)"
                )));
            }
            let incoherent_session_bindings: i64 = tx.query_row(
                "SELECT count(*)
                 FROM sessions s
                 WHERE s.execution_origin_claim_id IS NOT NULL
                   AND NOT (
                     EXISTS (
                       SELECT 1
                       FROM execution_origin_claims c
                       JOIN execution_origin_authorities a
                         ON a.authority_kind=c.authority_kind
                        AND a.authority_uuid=c.authority_uuid
                       JOIN execution_origin_events e
                         ON e.authority_kind=a.authority_kind
                        AND e.authority_uuid=a.authority_uuid
                        AND e.sequence=a.event_sequence
                       WHERE c.claim_id=s.execution_origin_claim_id
                         AND c.claimant_session_id=s.id
                         AND c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')
                         AND a.owner_session_id=s.id
                         AND a.active_claim_id=c.claim_id
                         AND a.phase=CASE
                           WHEN c.phase='recovery_quarantined' THEN 'quarantined'
                           WHEN c.phase IN ('settled','failed','abandoned','quarantined') THEN 'settling'
                           ELSE c.phase
                         END
                         AND a.claim_generation=c.expected_claim_generation+1
                         AND a.owner_generation=c.expected_owner_generation+
                           CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                         AND a.boot_id=c.boot_id
                         AND e.claim_id=c.claim_id
                         AND e.to_phase=a.phase
                         AND e.to_owner_generation=a.owner_generation
                         AND e.provider_absence_evidence IS c.absence_evidence
                         AND e.event_kind=CASE a.phase
                           WHEN 'claimed' THEN 'claimed'
                           WHEN 'launch_ready' THEN 'launch_ready'
                           WHEN 'launching' THEN 'launching'
                           WHEN 'provider_live' THEN 'provider_live'
                           WHEN 'quarantined' THEN 'quarantined'
                           WHEN 'settling' THEN 'settlement_prepared'
                         END
                         AND e.from_owner_generation=CASE
                           WHEN a.phase='claimed' THEN c.expected_owner_generation
                           ELSE a.owner_generation
                         END
                         AND (
                           (a.phase='claimed' AND e.from_phase='idle')
                           OR (a.phase='launch_ready' AND e.from_phase='claimed')
                           OR (a.phase='launching' AND e.from_phase='launch_ready')
                           OR (a.phase='provider_live' AND e.from_phase='launching')
                           OR (a.phase='quarantined' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live'))
                           OR (a.phase='settling' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live','quarantined'))
                         )
                         AND (
                           (s.status=c.authorized_session_status
                            AND s.execution_origin_write_seq=c.authorized_session_write_seq)
                           OR
                           (c.phase='provider_live'
                            AND c.authorized_session_status IN ('Running','WaitingApproval')
                            AND c.authorized_session_write_seq=s.execution_origin_write_seq+1
                            AND s.status IN ('Starting','Running','WaitingApproval'))
                         )
                         AND (
                           (a.authority_kind='ordinary' AND s.sandbox_custody_id IS NULL)
                           OR
                           (a.authority_kind='sandbox'
                            AND s.sandbox_custody_id=a.authority_uuid
                            AND EXISTS (
                              SELECT 1
                              FROM sandbox_custody_roots root
                              WHERE root.custody_id=a.authority_uuid
                                AND root.owner_session_id=s.id
                                AND root.generation=a.owner_generation
                                AND root.state='live'
                                AND root.validation_state='verified'
                                AND root.validated_generation=root.generation
                                AND root.effect_boot_id=c.boot_id
                                AND root.reserved_effects=0
                                AND root.active_effects=1
                            ))
                         )
                     )
                     OR EXISTS (
                       SELECT 1
                       FROM execution_origin_claims c
                       JOIN execution_origin_authorities a
                         ON a.authority_kind=c.authority_kind
                        AND a.authority_uuid=c.authority_uuid
                       JOIN execution_origin_receipts rcp
                         ON rcp.claim_id=c.claim_id
                        AND rcp.request_key=c.request_key
                        AND rcp.authority_kind=c.authority_kind
                        AND rcp.authority_uuid=c.authority_uuid
                        AND rcp.requested_origin_session_id=c.requested_origin_session_id
                        AND rcp.source_session_id IS c.source_session_id
                        AND rcp.claimant_session_id IS c.claimant_session_id
                        AND rcp.scheduled_job_id IS c.scheduled_job_id
                        AND rcp.scheduled_fire_at IS c.scheduled_fire_at
                        AND rcp.outcome=c.phase
                       JOIN execution_origin_events settled
                         ON settled.authority_kind=a.authority_kind
                        AND settled.authority_uuid=a.authority_uuid
                        AND settled.sequence=a.event_sequence
                       JOIN execution_origin_events prepared
                         ON prepared.authority_kind=a.authority_kind
                        AND prepared.authority_uuid=a.authority_uuid
                        AND prepared.sequence=a.event_sequence-1
                       JOIN model_invocations terminal
                         ON terminal.id=c.terminal_model_invocation_id
                        AND terminal.session_id=s.id
                        AND terminal.status IN ('completed','failed','cancelled','denied')
                       WHERE c.claim_id=s.execution_origin_claim_id
                         AND c.claimant_session_id=s.id
                         AND c.phase IN ('settled','failed','abandoned','quarantined')
                         AND ((c.phase='settled' AND c.prepared_terminal_status='Completed')
                           OR (c.phase='failed' AND c.prepared_terminal_status='Failed')
                           OR (c.phase IN ('abandoned','quarantined') AND c.prepared_terminal_status='Interrupted'))
                         AND c.prepared_stop_reason IS NOT NULL
                         AND c.prepared_session_write_seq=s.execution_origin_write_seq+1
                         AND c.prepared_provider_evidence IS NOT NULL
                         AND c.authorized_session_status=s.status
                         AND c.authorized_session_write_seq=s.execution_origin_write_seq
                         AND a.owner_session_id=s.id
                         AND a.active_claim_id IS NULL
                         AND a.phase='idle'
                         AND a.claim_generation=c.expected_claim_generation+1
                         AND a.owner_generation=c.expected_owner_generation+
                           CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                         AND a.boot_id IS NULL
                         AND settled.claim_id=c.claim_id
                         AND settled.from_phase='settling'
                         AND settled.to_phase='idle'
                         AND settled.from_owner_generation=a.owner_generation
                         AND settled.to_owner_generation=a.owner_generation
                         AND settled.provider_absence_evidence=c.prepared_provider_evidence
                         AND settled.event_kind='settled'
                         AND prepared.claim_id=c.claim_id
                         AND prepared.to_phase='settling'
                         AND prepared.to_owner_generation=a.owner_generation
                         AND prepared.provider_absence_evidence=c.prepared_provider_evidence
                         AND prepared.event_kind='settlement_prepared'
                         AND (c.prepared_c5_key IS NULL OR EXISTS (
                           SELECT 1
                           FROM daemon_settings ds
                           WHERE ds.key=c.prepared_c5_key
                             AND ds.value=c.prepared_c5_value
                             AND ds.updated_at=c.prepared_c5_at
                         ))
                         AND (
                           (a.authority_kind='ordinary' AND s.sandbox_custody_id IS NULL)
                           OR
                           (a.authority_kind='sandbox'
                            AND s.sandbox_custody_id=a.authority_uuid
                            AND EXISTS (
                              SELECT 1
                              FROM sandbox_custody_roots root
                              WHERE root.custody_id=a.authority_uuid
                                AND root.owner_session_id=s.id
                                AND root.generation=a.owner_generation
                                AND root.state='live'
                                AND root.validation_state='verified'
                                AND root.validated_generation=root.generation
                                AND root.effect_boot_id IS NULL
                                AND root.reserved_effects=0
                                AND root.active_effects=0
                            ))
                         )
                     )
                   )",
                [],
                |row| row.get(0),
            )?;
            let incoherent_active_authorities: i64 = tx.query_row(
                "SELECT count(*)
                 FROM execution_origin_authorities a
                 WHERE a.active_claim_id IS NOT NULL
                   AND NOT EXISTS (
                     SELECT 1
                     FROM execution_origin_claims c
                     WHERE c.claim_id=a.active_claim_id
                       AND c.authority_kind=a.authority_kind
                       AND c.authority_uuid=a.authority_uuid
                       AND c.claimant_session_id=a.owner_session_id
                       AND c.expected_owner_generation
                           + CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                           = a.owner_generation
                       AND c.expected_claim_generation+1=a.claim_generation
                       AND c.boot_id=a.boot_id
                       AND a.phase=CASE
                           WHEN c.phase='recovery_quarantined' THEN 'quarantined'
                           WHEN c.phase IN ('settled','failed','abandoned','quarantined') THEN 'settling'
                           ELSE c.phase
                       END
                       AND c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')
                   )",
                [],
                |row| row.get(0),
            )?;
            let incoherent_active_predecessors: i64 = tx.query_row(
                "SELECT count(*)
                 FROM execution_origin_authorities a
                 WHERE a.active_claim_id IS NOT NULL
                   AND NOT EXISTS (
                     SELECT 1
                     FROM execution_origin_claims c
                     JOIN sessions s
                       ON s.id=a.owner_session_id
                     JOIN execution_origin_events e
                       ON e.authority_kind=a.authority_kind
                      AND e.authority_uuid=a.authority_uuid
                      AND e.sequence=a.event_sequence
                     WHERE c.claim_id=a.active_claim_id
                       AND c.authority_kind=a.authority_kind
                       AND c.authority_uuid=a.authority_uuid
                       AND c.claimant_session_id=a.owner_session_id
                       AND c.expected_owner_generation
                           + CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                           = a.owner_generation
                       AND c.expected_claim_generation+1=a.claim_generation
                       AND c.boot_id=a.boot_id
                       AND a.phase=CASE
                           WHEN c.phase='recovery_quarantined' THEN 'quarantined'
                           WHEN c.phase IN ('settled','failed','abandoned','quarantined') THEN 'settling'
                           ELSE c.phase
                       END
                       AND c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')
                       AND e.claim_id=c.claim_id
                       AND e.to_phase=a.phase
                       AND e.to_owner_generation=a.owner_generation
                       AND e.provider_absence_evidence IS c.absence_evidence
                       AND e.event_kind=CASE a.phase
                           WHEN 'claimed' THEN 'claimed'
                           WHEN 'launch_ready' THEN 'launch_ready'
                           WHEN 'launching' THEN 'launching'
                           WHEN 'provider_live' THEN 'provider_live'
                           WHEN 'quarantined' THEN 'quarantined'
                           WHEN 'settling' THEN 'settlement_prepared'
                       END
                       AND e.from_owner_generation=CASE
                           WHEN a.phase='claimed' THEN c.expected_owner_generation
                           ELSE a.owner_generation
                       END
                       AND (
                           (a.phase='claimed' AND e.from_phase='idle')
                           OR (a.phase='launch_ready' AND e.from_phase='claimed')
                           OR (a.phase='launching' AND e.from_phase='launch_ready')
                           OR (a.phase='provider_live' AND e.from_phase='launching')
                           OR (a.phase='quarantined' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live'))
                           OR (a.phase='settling' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live','quarantined'))
                       )
                       AND (
                           (s.execution_origin_claim_id IS NULL
                            AND s.status='Starting'
                            AND c.phase='claimed'
                            AND c.authorized_session_status='Starting'
                            AND c.authorized_session_write_seq=s.execution_origin_write_seq+1)
                           OR
                           (s.execution_origin_claim_id=c.claim_id
                            AND (
                                (s.status=c.authorized_session_status
                                 AND s.execution_origin_write_seq=c.authorized_session_write_seq)
                                OR
                                (c.phase='provider_live'
                                 AND c.authorized_session_status IN ('Running','WaitingApproval')
                                 AND c.authorized_session_write_seq=s.execution_origin_write_seq+1
                                 AND s.status IN ('Starting','Running','WaitingApproval'))
                            ))
                       )
                       AND (
                           (a.authority_kind='ordinary'
                            AND s.sandbox_custody_id IS NULL
                            AND s.sandbox_kind IS NULL
                            AND s.sandbox_root IS NULL
                            AND s.sandbox_branch IS NULL
                            AND s.sandbox_cleanup_state IS NULL)
                           OR
                           (a.authority_kind='sandbox'
                            AND s.sandbox_custody_id=a.authority_uuid
                            AND s.sandbox_kind='GitWorktree'
                            AND s.sandbox_cleanup_state='Live'
                            AND EXISTS (
                              SELECT 1
                              FROM sandbox_custody_roots root
                              WHERE root.custody_id=a.authority_uuid
                                AND root.owner_session_id=s.id
                                AND root.sandbox_root=s.sandbox_root
                                AND root.sandbox_branch=s.sandbox_branch
                                AND root.generation=a.owner_generation
                                AND root.state='live'
                                AND root.validation_state='verified'
                                AND root.validated_generation=root.generation
                                AND root.effect_boot_id=c.boot_id
                                AND root.reserved_effects=0
                                AND root.active_effects=1
                            ))
                       )
                   )",
                [],
                |row| row.get(0),
            )?;
            if incoherent_active_authorities != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 cannot classify {incoherent_active_authorities} active V87 authority/claim binding(s)"
                )));
            }
            // Preserve the fourth-repair diagnostic precedence: a malformed
            // non-null Session binding is owned by its sealed classifier once
            // the legacy authority/claim tuple itself is coherent.
            if incoherent_session_bindings != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 cannot classify {incoherent_session_bindings} V87 Session origin binding(s)"
                )));
            }
            // The complementary scan owns only the remaining active-authority
            // negative space, including the exact unbound pre-bind shape.
            if incoherent_active_predecessors != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 cannot classify {incoherent_active_predecessors} active V87 authority/claim binding(s)"
                )));
            }
            h1_validate_exact_active_predecessors(&tx, 88, 87)?;
            let source_relation_counts = [
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_authorities",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                tx.query_row("SELECT count(*) FROM execution_origin_claims", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_members", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_events", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_receipts",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ];
            h1_v88_migration_fault(H1V88MigrationFault::AfterPreflight)?;
            tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
            tx.execute_batch(origin_authority::V88_REBUILD_RENAME_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterRename)?;
            tx.execute_batch(&origin_authority::v88_catalog_table_sql())?;
            tx.execute_batch(origin_authority::V88_REBUILD_UPGRADE_COPY_SQL)?;
            let destination_relation_counts = [
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_authorities",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                tx.query_row("SELECT count(*) FROM execution_origin_claims", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_members", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_events", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_receipts",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ];
            if destination_relation_counts != source_relation_counts {
                return Err(DaemonError::Store(format!(
                    "V88 five-relation copy count mismatch: source={source_relation_counts:?}, destination={destination_relation_counts:?}"
                )));
            }
            tx.execute_batch(origin_authority::V88_REBUILD_DROP_V87_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterCopy)?;
            tx.execute_batch(origin_authority::V85_INDEX_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterIndexes)?;
            tx.execute_batch(origin_authority::V85_TRIGGER_SQL)?;
            tx.execute_batch(
                "DROP TRIGGER execution_origin_authority_transition;
                 DROP TRIGGER execution_origin_claim_rank;
                 DROP TRIGGER execution_origin_claim_prepared_immutable;
                 DROP TRIGGER sessions_execution_origin_write_guard;",
            )?;
            tx.execute_batch(origin_authority::V88_STATE_TRIGGER_SQL)?;
            tx.execute_batch(origin_authority::V87_SESSION_TRIGGER_SQL)?;
            tx.execute_batch(origin_authority::V86_TRIGGER_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterTriggers)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V88 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V88 static-state-machine catalog foreign-key check failed".into(),
                ));
            }
            h1_v88_migration_fault(H1V88MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 88", [])?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterUserVersion)?;
            h1_v88_migration_fault(H1V88MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V88 migration complete: static execution-origin state machine");
        }

        Ok(())
    }
}
