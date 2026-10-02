impl Store {
    fn migrate_v049(&self, version: i32) -> Result<()> {
        // V49: Recursive DAG persistence foundation.
        //
        // SQLite constraints cover row-local shape, cheap uniqueness, and safe
        // references. Service/store validation remains authoritative for
        // aggregate invariants SQLite cannot enforce: exactly one root,
        // root_task_id matching the root node, acyclicity across all edge
        // kinds, parent_task_id/parent_child edge mirroring, same-graph edge
        // endpoints, child depth/scope rules, injection batch completeness,
        // lifecycle transition legality, retry budgets, recovery semantics, and
        // nonterminal transient work reconciliation.
        if version < 49 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_task_graphs (
                    id TEXT PRIMARY KEY,
                    root_task_id TEXT NOT NULL,
                    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
                    objective TEXT NOT NULL CHECK (length(trim(objective)) > 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'active', 'terminal', 'blocked', 'failed', 'cancelled', 'malformed'
                    )),
                    project_id TEXT REFERENCES projects(id),
                    workflow_id TEXT REFERENCES workflows(id),
                    topology_id TEXT REFERENCES topologies(id),
                    parent_session_id TEXT REFERENCES sessions(id),
                    source_execution_id TEXT,
                    source_eval_id TEXT,
                    execution_mode TEXT NOT NULL DEFAULT 'fake' CHECK (
                        execution_mode IN ('fake', 'live_session')
                    ),
                    max_depth INTEGER NOT NULL CHECK (max_depth >= 0),
                    max_fanout INTEGER NOT NULL CHECK (max_fanout > 0),
                    max_descendants INTEGER NOT NULL CHECK (max_descendants > 0),
                    step_limit INTEGER NOT NULL CHECK (step_limit > 0),
                    last_stop_reason TEXT,
                    malformed_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    recovered_at TEXT
                );

                CREATE TABLE IF NOT EXISTS recursive_task_nodes (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    parent_task_id TEXT REFERENCES recursive_task_nodes(id),
                    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
                    objective TEXT NOT NULL CHECK (length(trim(objective)) > 0),
                    scope TEXT NOT NULL CHECK (length(trim(scope)) > 0),
                    acceptance_criteria_json TEXT NOT NULL CHECK (json_valid(acceptance_criteria_json)),
                    depth INTEGER NOT NULL CHECK (depth >= 0),
                    scope_units INTEGER NOT NULL CHECK (scope_units > 0),
                    max_retries INTEGER NOT NULL CHECK (max_retries >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    decomposed_once INTEGER NOT NULL DEFAULT 0 CHECK (decomposed_once IN (0, 1)),
                    integration_strategy TEXT,
                    verification_strategy TEXT,
                    blocked_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_task_edges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    from_task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    to_task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL CHECK (kind IN ('parent_child', 'dependency')),
                    injection_batch_id TEXT REFERENCES recursive_injection_batches(id),
                    created_at TEXT NOT NULL,
                    CHECK (from_task_id <> to_task_id),
                    UNIQUE (graph_id, from_task_id, to_task_id, kind)
                );

                CREATE TABLE IF NOT EXISTS recursive_task_attempts (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                    retry_count INTEGER NOT NULL CHECK (retry_count >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'succeeded', 'decomposed', 'failed',
                        'blocked', 'cancelled', 'interrupted'
                    )),
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    failure_reason TEXT,
                    block_reason TEXT,
                    dependency_snapshot_json TEXT NOT NULL CHECK (json_valid(dependency_snapshot_json)),
                    executor_kind TEXT NOT NULL DEFAULT 'fake' CHECK (
                        executor_kind IN ('fake', 'live_session')
                    ),
                    session_id TEXT REFERENCES sessions(id),
                    workflow_execution_id TEXT,
                    CHECK (
                        (status = 'running' AND finished_at IS NULL)
                        OR (status <> 'running' AND finished_at IS NOT NULL)
                    ),
                    UNIQUE (task_id, phase, attempt_no)
                );

                CREATE TABLE IF NOT EXISTS recursive_injection_batches (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    parent_task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id),
                    reason_for_decomposition TEXT NOT NULL,
                    child_task_ids_json TEXT NOT NULL CHECK (json_valid(child_task_ids_json)),
                    edge_count INTEGER NOT NULL CHECK (edge_count > 0),
                    committed_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_lifecycle_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    from_status TEXT NOT NULL CHECK (from_status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    to_status TEXT NOT NULL CHECK (to_status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    reason TEXT,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_execution_artifacts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL CHECK (kind IN (
                        'inline', 'file', 'session_event', 'workflow_execution'
                    )),
                    label TEXT NOT NULL CHECK (length(trim(label)) > 0),
                    content TEXT,
                    uri TEXT,
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_graph_root
                    ON recursive_task_graphs(id, root_task_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_project
                    ON recursive_task_graphs(project_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_workflow
                    ON recursive_task_graphs(workflow_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_topology
                    ON recursive_task_graphs(topology_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_status
                    ON recursive_task_graphs(status, updated_at DESC);

                CREATE INDEX IF NOT EXISTS idx_recursive_nodes_graph_status
                    ON recursive_task_nodes(graph_id, status, updated_at);
                CREATE INDEX IF NOT EXISTS idx_recursive_nodes_parent
                    ON recursive_task_nodes(graph_id, parent_task_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_nodes_depth
                    ON recursive_task_nodes(graph_id, depth);

                CREATE INDEX IF NOT EXISTS idx_recursive_edges_from
                    ON recursive_task_edges(graph_id, from_task_id, kind);
                CREATE INDEX IF NOT EXISTS idx_recursive_edges_to
                    ON recursive_task_edges(graph_id, to_task_id, kind);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_one_parent_edge
                    ON recursive_task_edges(graph_id, to_task_id)
                    WHERE kind = 'parent_child';

                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_task_phase
                    ON recursive_task_attempts(task_id, phase, attempt_no);
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_running
                    ON recursive_task_attempts(graph_id, status, started_at)
                    WHERE status = 'running';
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_session
                    ON recursive_task_attempts(session_id)
                    WHERE session_id IS NOT NULL;

                CREATE INDEX IF NOT EXISTS idx_recursive_injections_parent
                    ON recursive_injection_batches(parent_task_id, committed_at);
                CREATE INDEX IF NOT EXISTS idx_recursive_events_task
                    ON recursive_lifecycle_events(task_id, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_events_graph
                    ON recursive_lifecycle_events(graph_id, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_artifacts_task
                    ON recursive_execution_artifacts(task_id, created_at);
                CREATE INDEX IF NOT EXISTS idx_recursive_artifacts_attempt
                    ON recursive_execution_artifacts(attempt_id);",
            )?;
            tracing::info!("V49 migration complete: recursive DAG persistence foundation");
            self.conn.pragma_update(None, "user_version", 49)?;
        }

        Ok(())
    }
}
