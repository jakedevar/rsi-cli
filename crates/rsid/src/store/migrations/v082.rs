impl Store {
    fn migrate_v082(&self, version: i32) -> Result<()> {
        if version < 82 {
            // V82 REBUILDS `agent_message_delivery_attempts`, which four other
            // relations hold `ON DELETE RESTRICT` foreign keys into, and a
            // RESTRICT action fires immediately even when its constraint is
            // `DEFERRABLE INITIALLY DEFERRED`. `DROP TABLE` under
            // `foreign_keys=ON` performs an implicit `DELETE FROM`, so the
            // rebuild would abort against any live attempt row. `PRAGMA
            // foreign_keys` is a no-op INSIDE a transaction, so the toggle has
            // to bracket the migration from outside it — the V65 precedent.
            //
            // The pragma is restored on BOTH paths before the error propagates,
            // so a failed migration can never leave this connection with
            // enforcement silently disabled.
            self.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
            let outcome = self.apply_agent_message_v82_migration();
            self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            outcome?;
        }

        Ok(())
    }

    /// The V82 mailbox amendment (P2-06d), factored out of `init_schema` so the
    /// caller can restore `PRAGMA foreign_keys` on every exit path — including
    /// the early `return Err(...)`s below, which an inline block could not.
    ///
    /// Ordering mirrors the V81 block deliberately: exact-source check, exact
    /// SOURCE-catalog fingerprint, DDL, drained `PRAGMA foreign_key_check`,
    /// version write, then the RESULT fingerprint. The result fingerprint is
    /// computed AFTER the version write because `v81_schema_fingerprint`
    /// absorbs the live `PRAGMA user_version`; computing it before would pin a
    /// digest no later reopen could ever reproduce. A mismatch still aborts the
    /// whole transaction, so nothing is durable.
    fn apply_agent_message_v82_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 81 {
            return Err(DaemonError::Store(format!(
                "V82 requires exact V81 source, found V{active_version}"
            )));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterExactSource)?;

        // The SHIPPED V81 catalog must be byte-exact before the first DDL
        // statement. This is also what keeps the V81 pin honest work rather
        // than dead history: it is re-proved on every fresh open, and on the
        // operator's own database at the moment V82 runs.
        let v81_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
        if v81_fingerprint != agent_coordination::AGENT_MESSAGE_V81_PINNED_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V82 requires the exact shipped V81 mailbox catalog, found {v81_fingerprint}"
            )));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterV81Fingerprint)?;

        agent_coordination::install_v82_amendments(&tx)?;
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterSchema)?;

        // `install_v82_amendments` performs the rebuild copy and its row-count
        // parity check; this failpoint sits on the far side of both.
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterCopy)?;

        // Deferred foreign keys must be fully drained inside the transaction,
        // not merely counted. `pragma_foreign_key_check` is an explicit sweep
        // and is unaffected by `foreign_keys=OFF`, so this is a real referential
        // proof over the rebuilt relation and its four referrers.
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(
                "V82 agent message foreign-key check failed".into(),
            ));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterForeignKeyCheck)?;

        tx.execute(
            &format!(
                "PRAGMA user_version = {}",
                agent_coordination::AGENT_MESSAGE_V82_USER_VERSION
            ),
            [],
        )?;
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterUserVersion)?;

        let schema_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
        if schema_fingerprint != agent_coordination::AGENT_MESSAGE_V82_PINNED_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V82 agent message semantic fingerprint mismatch: {schema_fingerprint}"
            )));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterFingerprint)?;
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!(
            %schema_fingerprint,
            "V82 migration complete: reconciler no-effect backstop and sealed-uncertain acknowledgement guard"
        );
        Ok(())
    }
}
