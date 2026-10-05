impl Store {
    fn migrate_v105(&self, version: i32) -> Result<()> {
        // V105: producer-bound pending-question publication. A reserved epoch
        // is an unresolved gate until its event and identity commit together.
        // Existing pending snapshots deliberately receive no inferred identity.
        if version < 105 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE pending_question_publications (
                    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    publication_id TEXT NOT NULL UNIQUE,
                    epoch INTEGER NOT NULL CHECK(epoch>0),
                    state TEXT NOT NULL CHECK(state IN ('unresolved','published','cleared')),
                    question_json TEXT,
                    conversation_event_id INTEGER REFERENCES conversation_events(id) ON DELETE CASCADE,
                    event_sequence INTEGER,
                    tool_use_id TEXT,
                    model_invocation_id TEXT REFERENCES model_invocations(id) ON DELETE CASCADE,
                    updated_at TEXT NOT NULL,
                    CHECK(state<>'published' OR
                        (question_json IS NOT NULL AND json_valid(question_json)
                         AND conversation_event_id IS NOT NULL AND event_sequence IS NOT NULL
                         AND tool_use_id IS NOT NULL AND length(tool_use_id)>0))
                 );",
            )?;
            tx.execute("PRAGMA user_version = 105", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
