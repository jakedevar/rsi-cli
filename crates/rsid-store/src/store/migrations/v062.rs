impl Store {
    fn migrate_v062(&self, version: i32) -> Result<()> {
        // V62: Recursive topology-to-DAG linkage.
        //
        // T1 creates durable ownership/idempotency/snapshot records for
        // topology-derived recursive graphs and task links. This is schema
        // and graph-creation linkage only: no scheduler runs, live attempts,
        // sessions, workflow execution updates, or background behavior.
        if version < 62 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_topology_graph_links (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    execution_owner TEXT NOT NULL,
                    owner_key TEXT NOT NULL,
                    idempotency_key TEXT,
                    request_fingerprint TEXT NOT NULL,
                    topology_id TEXT NOT NULL REFERENCES topologies(id),
                    workflow_id TEXT REFERENCES workflows(id),
                    workflow_execution_id TEXT,
                    source_topology_node_id TEXT NOT NULL,
                    source_topology_iteration INTEGER NOT NULL CHECK (source_topology_iteration >= 0),
                    parent_session_id TEXT REFERENCES sessions(id),
                    project_id TEXT REFERENCES projects(id),
                    creation_mode TEXT NOT NULL,
                    include_prerequisite_closure INTEGER NOT NULL CHECK (include_prerequisite_closure IN (0, 1)),
                    topology_name_snapshot TEXT NOT NULL,
                    topology_updated_at_snapshot TEXT NOT NULL,
                    topology_snapshot_json TEXT NOT NULL CHECK (json_valid(topology_snapshot_json)),
                    selected_slice_json TEXT NOT NULL CHECK (json_valid(selected_slice_json)),
                    policy_snapshot_json TEXT NOT NULL CHECK (json_valid(policy_snapshot_json)),
                    workflow_execution_linkage_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(workflow_execution_linkage_json)),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_topology_task_links (
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    topology_id TEXT NOT NULL REFERENCES topologies(id),
                    topology_node_id TEXT NOT NULL,
                    topology_iteration INTEGER NOT NULL CHECK (topology_iteration >= 0),
                    topology_node_kind TEXT NOT NULL,
                    source_params_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(source_params_json)),
                    topology_node_snapshot_json TEXT NOT NULL CHECK (json_valid(topology_node_snapshot_json)),
                    policy_snapshot_json TEXT NOT NULL CHECK (json_valid(policy_snapshot_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (graph_id, topology_node_id, topology_iteration),
                    UNIQUE (task_id)
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_owner_key
                    ON recursive_topology_graph_links(owner_key);
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_owner_lookup
                    ON recursive_topology_graph_links(execution_owner, owner_key);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_idempotency
                    ON recursive_topology_graph_links(execution_owner, idempotency_key)
                    WHERE idempotency_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_source
                    ON recursive_topology_graph_links(
                        topology_id, source_topology_node_id,
                        source_topology_iteration, created_at, graph_id
                    );
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_workflow_execution
                    ON recursive_topology_graph_links(
                        workflow_execution_id, source_topology_node_id, graph_id
                    );
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_workflow
                    ON recursive_topology_graph_links(workflow_id, topology_id, graph_id);

                CREATE INDEX IF NOT EXISTS idx_recursive_topology_task_links_node
                    ON recursive_topology_task_links(
                        topology_id, topology_node_id, topology_iteration, graph_id
                    );",
            )?;
            tracing::info!("V62 migration complete: recursive topology graph linkage");
            self.conn.pragma_update(None, "user_version", 62)?;
        }

        Ok(())
    }
}
