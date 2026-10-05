impl Store {
    fn migrate_v108(&self, version: i32) -> Result<()> {
        // V108: current answer queues and metadata accounting remain indexed
        // as retained native decision history grows. No historical row is removed.
        if version < 108 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE INDEX harness_manager_v2_metadata_budget
                    ON harness_manager_v2_records(project_id,manager_session_id,scope_version)
                    WHERE NOT (kind='decision_generation' OR (kind IN ('decision','decision_target') AND substr(record_key,1,9)='approval:') OR (kind='decision_delivery' AND json_extract(payload_json,'$.target.kind') IS 'appserver_approval') OR (kind='decision_retrieval' AND (substr(record_key,1,17)='manager:approval:' OR substr(record_key,1,14)='lead:approval:')));
                 CREATE INDEX harness_manager_v2_queued_answers
                    ON harness_manager_v2_records(project_id,manager_session_id,scope_version,record_key)
                    WHERE kind='decision_delivery' AND json_extract(payload_json,'$.state')='queued';
                 CREATE INDEX harness_manager_v2_running_answers
                    ON harness_manager_v2_records(project_id,record_key)
                    WHERE kind='decision_delivery' AND json_extract(payload_json,'$.state')='running';
                 CREATE INDEX harness_manager_v2_operator_inbox
                    ON harness_manager_v2_records(project_id,manager_session_id,scope_version,epic_id,record_key)
                    WHERE kind='decision' AND json_extract(payload_json,'$.delivery.state')='available_in_scoped_inbox';",
            )?;
            tx.execute("PRAGMA user_version = 108", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
