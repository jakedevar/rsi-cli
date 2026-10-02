impl Store {
    fn migrate_v079(&self, version: i32) -> Result<()> {
        // The V78 DDL was amended after V78 had already shipped: three
        // transition foreign keys on `idea_program_run_gates` and
        // `idea_program_run_attempt_refs` gained `DEFERRABLE INITIALLY
        // DEFERRED`. A database that reached V78 before that amendment carries
        // a baseline no later gate can accept — V79 pins the post-amendment
        // fingerprint, so such a database fails the V79 gate on every boot,
        // rolls back, and takes the daemon down with it.
        //
        // Normalize those databases forward to the canonical V78 baseline here,
        // before V79 runs, so V79 still sees the exact source it was reviewed
        // against and stays byte-for-byte the reviewed migration. This is a
        // convergent repair, not a second target shape: it asserts the canonical
        // V78 fingerprint as its post-condition, so every database that survives
        // it holds precisely the schema a fresh V78 produces. Databases already
        // on the canonical baseline are left untouched, and databases already at
        // V79 never enter this branch.
        if version == 78 {
            self.repair_legacy_d05_v78_baseline()?;
        }

        // V79 is a forward-only custody repair for D05. It deliberately does
        // not edit the reviewed V78 DDL. Instead it validates every existing
        // value, adds composed claim witnesses, and installs strict guards for
        // all future ProgramRun identity/time writes.
        if version < 79 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 78 {
                return Err(DaemonError::Store(format!(
                    "V79 requires exact V78 source, found V{active_version}"
                )));
            }
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterExactSource)?;

            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_runs")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterRunRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_transitions")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterTransitionRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_gates")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterGateRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_budgets")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterBudgetRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_locks")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterLockRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_actions")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterActionRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_attempt_refs")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterAttemptRowsValidated)?;

            tx.execute(
                "ALTER TABLE idea_program_run_actions ADD COLUMN claim_run_version INTEGER
                 CHECK(claim_run_version IS NULL OR claim_run_version >= 0)",
                [],
            )?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterClaimRunColumn)?;
            tx.execute(
                "ALTER TABLE idea_program_run_actions ADD COLUMN claim_lease_generation INTEGER
                 CHECK(claim_lease_generation IS NULL OR claim_lease_generation >= 0)",
                [],
            )?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterClaimLeaseColumn)?;

            install_d05_v79_validation_triggers(&tx)?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterValidationTriggers)?;

            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V79 ProgramRun foreign-key check failed".into(),
                ));
            }
            let schema_fingerprint = program_runs::d05_schema_fingerprint(&tx)?;
            if schema_fingerprint != program_runs::D05_V79_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V79 ProgramRun semantic fingerprint mismatch: {schema_fingerprint}"
                )));
            }
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterFingerprint)?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 79", [])?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterUserVersion)?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V79 migration complete: ProgramRun custody repair");
        }

        Ok(())
    }

    /// Normalize a pre-amendment V78 `ProgramRun` baseline onto the canonical V78
    /// schema.
    ///
    /// V78 shipped, and was then edited in place to mark three transition
    /// foreign keys `DEFERRABLE INITIALLY DEFERRED`. Databases that ran the
    /// original DDL therefore hold a baseline that the V79 fingerprint gate
    /// rejects forever. This repair converges them onto the amended shape and
    /// proves it did so by asserting the canonical V78 fingerprint — the same
    /// constant [`program_runs::validate_d05_catalog`] enforces on the create
    /// path — before committing.
    ///
    /// The repair is idempotent, and it is atomic: any failure rolls the
    /// transaction back and leaves the database exactly as it was.
    fn repair_legacy_d05_v78_baseline(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if program_runs::d05_schema_fingerprint(&tx)? == program_runs::D05_V78_SCHEMA_FINGERPRINT {
            return Ok(());
        }

        for (table, canonical_ddl) in [
            ("idea_program_run_gates", D05_V78_GATES_TABLE_DDL),
            (
                "idea_program_run_attempt_refs",
                D05_V78_ATTEMPT_REFS_TABLE_DDL,
            ),
        ] {
            rebuild_d05_v78_table(&tx, table, canonical_ddl)?;
        }

        let repaired = program_runs::d05_schema_fingerprint(&tx)?;
        if repaired != program_runs::D05_V78_SCHEMA_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "legacy V78 ProgramRun baseline repair did not reach the canonical V78 schema: {repaired}"
            )));
        }
        tx.commit()?;
        tracing::info!("V78 ProgramRun baseline normalized to the canonical schema");
        Ok(())
    }
}

fn d05_v79_uuid_guard(column: &str, optional: bool) -> String {
    let guard = format!(
        "length(NEW.{column})=36 AND NEW.{column}!='00000000-0000-0000-0000-000000000000' \
         AND NEW.{column}=lower(NEW.{column}) \
         AND substr(NEW.{column},9,1)='-' AND substr(NEW.{column},14,1)='-' \
         AND substr(NEW.{column},19,1)='-' AND substr(NEW.{column},24,1)='-' \
         AND replace(NEW.{column},'-','') NOT GLOB '*[^0-9a-f]*'"
    );
    if optional {
        format!("(NEW.{column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

fn d05_v79_timestamp_guard(column: &str, optional: bool) -> String {
    let guard = format!(
        "length(NEW.{column})=30 \
         AND strftime('%Y-%m-%dT%H:%M:%S',NEW.{column})=substr(NEW.{column},1,19) \
         AND substr(NEW.{column},20,1)='.' \
         AND substr(NEW.{column},21,9) NOT GLOB '*[^0-9]*' \
         AND substr(NEW.{column},30,1)='Z'"
    );
    if optional {
        format!("(NEW.{column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

fn install_d05_v79_validation_triggers(tx: &Transaction<'_>) -> Result<()> {
    struct GuardSpec {
        table: &'static str,
        required_uuids: &'static [&'static str],
        optional_uuids: &'static [&'static str],
        required_times: &'static [&'static str],
        optional_times: &'static [&'static str],
        mutable: bool,
        extra: Option<&'static str>,
    }

    let specs = [
        GuardSpec {
            table: "idea_program_runs",
            required_uuids: &["id", "project_id", "idea_id", "controller_session_id"],
            optional_uuids: &[],
            required_times: &["created_at", "updated_at"],
            optional_times: &["settled_at", "cancelled_at", "failed_at"],
            mutable: true,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_transitions",
            required_uuids: &["id", "program_run_id", "idea_event_id"],
            optional_uuids: &["actor_session_id"],
            required_times: &["created_at"],
            optional_times: &[],
            mutable: false,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_gates",
            required_uuids: &["id", "program_run_id", "transition_id"],
            optional_uuids: &["evaluator_session_id"],
            required_times: &["created_at"],
            optional_times: &[],
            mutable: false,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_budgets",
            required_uuids: &["program_run_id"],
            optional_uuids: &[],
            required_times: &["updated_at"],
            optional_times: &[],
            mutable: true,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_locks",
            required_uuids: &[
                "id",
                "project_id",
                "program_run_id",
                "requesting_transition_id",
                "controller_session_id",
            ],
            optional_uuids: &["owner_boot_id"],
            required_times: &["requested_at"],
            optional_times: &["acquired_at", "heartbeat_at", "expires_at", "released_at"],
            mutable: true,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_actions",
            required_uuids: &[
                "id",
                "program_run_id",
                "creating_transition_id",
                "controller_session_id",
            ],
            optional_uuids: &[
                "claim_boot_id",
                "external_model_invocation_id",
                "external_session_id",
                "scheduled_job_id",
            ],
            required_times: &["not_before", "created_at", "updated_at"],
            optional_times: &[
                "claimed_at",
                "claim_expires_at",
                "published_at",
                "acknowledged_at",
            ],
            mutable: true,
            extra: Some(
                "((NEW.state='reserved' AND NEW.claim_run_version IS NULL AND NEW.claim_lease_generation IS NULL) \
                  OR (NEW.state IN ('claimed','published') AND NEW.claim_run_version IS NOT NULL AND NEW.claim_lease_generation IS NOT NULL) \
                  OR (NEW.state='acknowledged' AND ((NEW.claim_run_version IS NULL AND NEW.claim_lease_generation IS NULL) \
                    OR (NEW.claim_run_version IS NOT NULL AND NEW.claim_lease_generation IS NOT NULL))) \
                  OR NEW.state IN ('failed','cancelled'))",
            ),
        },
        GuardSpec {
            table: "idea_program_run_attempt_refs",
            required_uuids: &[
                "id",
                "program_run_id",
                "action_id",
                "creating_transition_id",
            ],
            optional_uuids: &[
                "session_id",
                "model_invocation_id",
                "completing_transition_id",
            ],
            required_times: &["created_at", "updated_at"],
            optional_times: &["observed_at"],
            mutable: true,
            extra: None,
        },
    ];

    for spec in specs {
        let mut guards = Vec::new();
        guards.extend(
            spec.required_uuids
                .iter()
                .map(|column| d05_v79_uuid_guard(column, false)),
        );
        guards.extend(
            spec.optional_uuids
                .iter()
                .map(|column| d05_v79_uuid_guard(column, true)),
        );
        guards.extend(
            spec.required_times
                .iter()
                .map(|column| d05_v79_timestamp_guard(column, false)),
        );
        guards.extend(
            spec.optional_times
                .iter()
                .map(|column| d05_v79_timestamp_guard(column, true)),
        );
        if let Some(extra) = spec.extra {
            guards.push(extra.to_string());
        }
        let predicate = guards.join(" AND ");
        for operation in if spec.mutable {
            &["INSERT", "UPDATE"][..]
        } else {
            &["INSERT"][..]
        } {
            let suffix = operation.to_ascii_lowercase();
            tx.execute_batch(&format!(
                "CREATE TRIGGER {table}_v79_validate_{suffix} BEFORE {operation} ON {table} \
                 WHEN NOT ({predicate}) BEGIN SELECT RAISE(ABORT,'V79 invalid ProgramRun identity, timestamp, or state'); END;",
                table = spec.table,
            ))?;
        }
    }
    Ok(())
}
