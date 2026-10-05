impl Store {
    fn migrate_v080(&self, version: i32) -> Result<()> {
        // V80 adds the durable coordination aggregate used by agent spawn,
        // progress, automatic terminal-watch repair, and Phase 2 messaging.
        // It is forward-only and accepts exactly the audited V79 catalog.
        if version < 80 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 79 {
                return Err(DaemonError::Store(format!(
                    "V80 requires exact V79 source, found V{active_version}"
                )));
            }
            let v79_fingerprint = program_runs::d05_schema_fingerprint(&tx)?;
            if v79_fingerprint != program_runs::D05_V79_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V80 requires exact V79 ProgramRun catalog, found {v79_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterExactSource,
            )?;

            agent_coordination::install_v80_schema(&tx)?;
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterSchema,
            )?;

            let schema_fingerprint = agent_coordination::v80_schema_fingerprint(&tx)?;
            if schema_fingerprint != agent_coordination::AGENT_COORDINATION_V80_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V80 agent coordination semantic fingerprint mismatch: {schema_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterFingerprint,
            )?;

            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V80 agent coordination foreign-key check failed".into(),
                ));
            }
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterForeignKeyCheck,
            )?;

            tx.execute("PRAGMA user_version = 80", [])?;
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterUserVersion,
            )?;
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::BeforeCommit,
            )?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V80 migration complete: agent coordination");
        }

        Ok(())
    }
}
