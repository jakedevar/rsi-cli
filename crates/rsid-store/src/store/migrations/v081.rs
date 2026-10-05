impl Store {
    fn migrate_v081(&self, version: i32) -> Result<()> {
        if version < 81 {
            // Phase 2 exclusively owns V81 after an exact accepted-V80
            // semantic-fingerprint preflight (C-P2-02). The numbering is legal
            // only because the rejected V81 source was never integrated,
            // deployed, or opened against a real database.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 80 {
                return Err(DaemonError::Store(format!(
                    "V81 requires exact V80 source, found V{active_version}"
                )));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterExactSource)?;

            // The accepted V80 coordination catalog must be byte-exact before
            // the first DDL statement; a mismatch aborts without changing data
            // or `user_version`.
            let v80_fingerprint = agent_coordination::v80_schema_fingerprint(&tx)?;
            if v80_fingerprint != agent_coordination::AGENT_COORDINATION_V80_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V81 requires the exact accepted V80 coordination catalog, found {v80_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterV80Fingerprint)?;

            agent_coordination::install_v81_schema(&tx)?;
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterSchema)?;

            // `install_v81_schema` performs the copy and its row-count parity
            // check; this failpoint sits on the far side of both.
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterCopy)?;

            // Deferred foreign keys must be fully drained inside the
            // transaction, not merely counted.
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V81 agent message foreign-key check failed".into(),
                ));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterForeignKeyCheck)?;

            tx.execute(
                &format!(
                    "PRAGMA user_version = {}",
                    agent_coordination::AGENT_MESSAGE_V81_USER_VERSION
                ),
                [],
            )?;
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterUserVersion)?;

            // Fingerprinted AFTER the version write, deliberately (C-P2-17,
            // H21-P2-INT-REV-005). `v81_schema_fingerprint` absorbs the LIVE
            // `PRAGMA user_version`, so computing it here — rather than before
            // the write — is what makes the migrating connection and every
            // later reopen agree on one digest for one identical catalog,
            // while still giving the digest real catalog/version coverage.
            // A mismatch still aborts the whole transaction, so nothing is
            // durable: the version write rolls back with the DDL.
            let schema_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
            if schema_fingerprint
                == agent_coordination::AGENT_MESSAGE_REJECTED_V81_SCHEMA_FINGERPRINT
            {
                return Err(DaemonError::Store(
                    "V81 catalog matches the REJECTED V81 fingerprint and is forbidden".into(),
                ));
            }
            if schema_fingerprint != agent_coordination::AGENT_MESSAGE_V81_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V81 agent message semantic fingerprint mismatch: {schema_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterFingerprint)?;
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V81 migration complete: agent message mailbox");
        }

        Ok(())
    }
}
