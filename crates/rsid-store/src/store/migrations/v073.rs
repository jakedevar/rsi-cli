impl Store {
    fn migrate_v073(&self, version: i32) -> Result<()> {
        if version < 73 {
            let tx = self.conn.unchecked_transaction()?;
            add_column_if_not_exists_tx(&tx, "sessions", "model_invocation_id", "TEXT")?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS model_invocations (
                    id TEXT PRIMARY KEY,
                    purpose TEXT NOT NULL,
                    invocation_kind TEXT NOT NULL,
                    foreground TEXT NOT NULL,
                    paid_risk TEXT NOT NULL,
                    admission_status TEXT NOT NULL,
                    status TEXT NOT NULL,
                    provider TEXT,
                    model TEXT,
                    backend TEXT,
                    model_tier TEXT,
                    effort TEXT,
                    trigger_source TEXT NOT NULL,
                    session_id TEXT,
                    project_id TEXT,
                    workflow_id TEXT,
                    scheduled_job_id TEXT,
                    issue_tracker_id TEXT,
                    issue_identifier TEXT,
                    topology_node_id TEXT,
                    recursive_graph_id TEXT,
                    recursive_task_id TEXT,
                    recursive_attempt_id TEXT,
                    operator TEXT,
                    parent_invocation_id TEXT,
                    retry_of_invocation_id TEXT,
                    dedup_key TEXT,
                    request_fingerprint TEXT,
                    policy_snapshot_json TEXT NOT NULL DEFAULT '{}',
                    error_class TEXT,
                    reserved_input_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_output_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                    reserved_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER,
                    output_tokens INTEGER,
                    cache_creation_tokens INTEGER,
                    cache_read_tokens INTEGER,
                    reasoning_tokens INTEGER,
                    embedding_input_count INTEGER,
                    wall_time_ms INTEGER,
                    estimated_cost_usd REAL,
                    usage_confidence TEXT NOT NULL DEFAULT 'unavailable',
                    baseline_input_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_output_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                    baseline_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    started_at TEXT,
                    completed_at TEXT
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_model_invocations_dedup
                    ON model_invocations(dedup_key) WHERE dedup_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_model_invocations_session_id
                    ON model_invocations(session_id);
                CREATE INDEX IF NOT EXISTS idx_model_invocations_purpose
                    ON model_invocations(purpose);

                CREATE TABLE IF NOT EXISTS model_budget_policies (
                    policy_key TEXT PRIMARY KEY,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT,
                    model_tier TEXT,
                    effort TEXT,
                    ceiling_model_tier TEXT,
                    ceiling_effort TEXT,
                    max_calls INTEGER,
                    max_total_tokens INTEGER,
                    max_input_tokens INTEGER,
                    max_output_tokens INTEGER,
                    max_cache_creation_tokens INTEGER,
                    max_cache_read_tokens INTEGER,
                    max_reasoning_tokens INTEGER,
                    max_embedding_inputs INTEGER,
                    max_wall_time_ms INTEGER,
                    max_concurrency INTEGER,
                    max_retries INTEGER,
                    max_calls_per_window INTEGER,
                    rate_window_seconds INTEGER,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS model_budget_counters (
                    counter_key TEXT PRIMARY KEY,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT NOT NULL,
                    model_tier TEXT NOT NULL,
                    effort TEXT,
                    call_count INTEGER NOT NULL DEFAULT 0,
                    active_count INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    embedding_inputs INTEGER NOT NULL DEFAULT 0,
                    wall_time_ms INTEGER NOT NULL DEFAULT 0,
                    rate_window_started_at TEXT,
                    rate_window_call_count INTEGER NOT NULL DEFAULT 0,
                    updated_at TEXT NOT NULL
                );
                ",
            )?;

            for (column, col_type) in [
                ("reserved_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("reserved_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "reserved_cache_creation_tokens",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("reserved_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("reserved_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "reserved_embedding_input_count",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("reserved_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
                ("baseline_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("baseline_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "baseline_cache_creation_tokens",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("baseline_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("baseline_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "baseline_embedding_input_count",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("baseline_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
            ] {
                add_column_if_not_exists_tx(&tx, "model_invocations", column, col_type)?;
            }

            if sqlite_table_exists_tx(&tx, "sessions")? {
                let exprs = v73_session_exprs(&tx)?;
                let backfill_sql = format!(
                    "
                    INSERT INTO model_invocations (
                        id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                        provider, model, backend, model_tier, effort, trigger_source,
                        session_id, project_id, workflow_id, scheduled_job_id,
                        issue_tracker_id, issue_identifier, policy_snapshot_json,
                        input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                        wall_time_ms, estimated_cost_usd, usage_confidence, dedup_key, created_at, started_at, completed_at
                    )
                    SELECT
                        lower(substr(hex(randomblob(16)),1,8) || '-' ||
                              substr(hex(randomblob(16)),1,4) || '-' ||
                              substr(hex(randomblob(16)),1,4) || '-' ||
                              substr(hex(randomblob(16)),1,4) || '-' ||
                              substr(hex(randomblob(16)),1,12)),
                        'session.launch.fresh',
                        'session_lifecycle',
                        'foreground',
                        'paid_capable',
                        'admitted',
                        CASE
                            WHEN {status_expr} = 'Failed' THEN 'failed'
                            WHEN {status_expr} IN ('Completed', 'Archived', 'Interrupted') THEN 'completed'
                            ELSE 'completed'
                        END,
                        {provider_expr},
                        {model_expr},
                        {backend_expr},
                        {model_tier_expr},
                        {effort_expr},
                        'legacy_backfill',
                        id,
                        {project_id_expr},
                        {workflow_id_expr},
                        {scheduled_job_id_expr},
                        {issue_tracker_id_expr},
                        {issue_identifier_expr},
                        json('{{\"source\":\"v73_backfill\"}}'),
                        {total_input_tokens_expr},
                        {total_output_tokens_expr},
                        {total_cache_creation_tokens_expr},
                        {total_cache_read_tokens_expr},
                        {work_time_ms_expr},
                        {cost_usd_expr},
                        CASE
                            WHEN {total_input_tokens_expr} IS NOT NULL OR {total_output_tokens_expr} IS NOT NULL THEN 'measured'
                            ELSE 'unavailable'
                        END,
                        'legacy-session:' || id,
                        {created_at_expr},
                        {created_at_expr},
                        {updated_at_expr}
                    FROM sessions
                    WHERE {session_kind_filter}
                      AND model_invocation_id IS NULL
                      AND NOT EXISTS (
                          SELECT 1
                          FROM model_invocations
                          WHERE dedup_key = 'legacy-session:' || sessions.id
                      );

                    UPDATE sessions
                    SET model_invocation_id = (
                        SELECT id FROM model_invocations
                        WHERE model_invocations.session_id = sessions.id
                          AND model_invocations.dedup_key = 'legacy-session:' || sessions.id
                        LIMIT 1
                    )
                    WHERE model_invocation_id IS NULL
                      AND {session_kind_filter};
                    ",
                    status_expr = exprs.status_expr,
                    provider_expr = exprs.provider_expr,
                    model_expr = exprs.model_expr,
                    backend_expr = exprs.backend_expr,
                    model_tier_expr = exprs.model_tier_expr,
                    effort_expr = exprs.effort_expr,
                    project_id_expr = exprs.project_id_expr,
                    workflow_id_expr = exprs.workflow_id_expr,
                    scheduled_job_id_expr = exprs.scheduled_job_id_expr,
                    issue_tracker_id_expr = exprs.issue_tracker_id_expr,
                    issue_identifier_expr = exprs.issue_identifier_expr,
                    total_input_tokens_expr = exprs.total_input_tokens_expr,
                    total_output_tokens_expr = exprs.total_output_tokens_expr,
                    total_cache_creation_tokens_expr = exprs.total_cache_creation_tokens_expr,
                    total_cache_read_tokens_expr = exprs.total_cache_read_tokens_expr,
                    work_time_ms_expr = exprs.work_time_ms_expr,
                    cost_usd_expr = exprs.cost_usd_expr,
                    created_at_expr = exprs.created_at_expr,
                    updated_at_expr = exprs.updated_at_expr,
                    session_kind_filter = exprs.session_kind_filter,
                );
                tx.execute_batch(&backfill_sql)?;
            }

            tx.execute(
                "INSERT INTO daemon_settings (key, value, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO NOTHING",
                params![
                    crate::store::model_control::KEY_MODEL_CONTROL_MODE,
                    "normal",
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )?;
            tx.execute("PRAGMA user_version = 73", [])?;
            tx.commit()?;
        }

        self.repair_v73_model_control_schema()?;

        Ok(())
    }

    fn repair_v73_model_control_schema(&self) -> Result<()> {
        // IMMEDIATE, not DEFERRED. This repair runs unconditionally on every
        // `Store::open()` (outside every `if version < N` gate), and its first
        // statement is a read (`pragma_table_info` via
        // `add_column_if_not_exists_tx`) followed by DDL writes. A DEFERRED
        // transaction therefore takes a WAL read snapshot and then tries to
        // upgrade read -> write, which SQLite reports as SQLITE_BUSY_SNAPSHOT
        // (extended code 517) — and it deliberately does NOT invoke the busy
        // handler for that upgrade, because sleeping while holding a read
        // snapshot could deadlock. That made `PRAGMA busy_timeout` inert and
        // let two concurrent opens of the same database fail instantly.
        // BEGIN IMMEDIATE takes the write lock up front, which DOES honor
        // busy_timeout, so concurrent openers serialize instead of racing.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        add_column_if_not_exists_tx(&tx, "sessions", "model_invocation_id", "TEXT")?;
        tx.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS daemon_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS model_invocations (
                id TEXT PRIMARY KEY,
                purpose TEXT NOT NULL,
                invocation_kind TEXT NOT NULL,
                foreground TEXT NOT NULL,
                paid_risk TEXT NOT NULL,
                admission_status TEXT NOT NULL,
                status TEXT NOT NULL,
                provider TEXT,
                model TEXT,
                backend TEXT,
                model_tier TEXT,
                effort TEXT,
                trigger_source TEXT NOT NULL,
                session_id TEXT,
                project_id TEXT,
                workflow_id TEXT,
                scheduled_job_id TEXT,
                issue_tracker_id TEXT,
                issue_identifier TEXT,
                topology_node_id TEXT,
                recursive_graph_id TEXT,
                recursive_task_id TEXT,
                recursive_attempt_id TEXT,
                operator TEXT,
                parent_invocation_id TEXT,
                retry_of_invocation_id TEXT,
                dedup_key TEXT,
                request_fingerprint TEXT,
                policy_snapshot_json TEXT NOT NULL DEFAULT '{}',
                error_class TEXT,
                reserved_input_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_output_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                reserved_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_creation_tokens INTEGER,
                cache_read_tokens INTEGER,
                reasoning_tokens INTEGER,
                embedding_input_count INTEGER,
                wall_time_ms INTEGER,
                estimated_cost_usd REAL,
                usage_confidence TEXT NOT NULL DEFAULT 'unavailable',
                baseline_input_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_output_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                baseline_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_model_invocations_dedup
                ON model_invocations(dedup_key) WHERE dedup_key IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_model_invocations_session_id
                ON model_invocations(session_id);
            CREATE INDEX IF NOT EXISTS idx_model_invocations_purpose
                ON model_invocations(purpose);

            CREATE TABLE IF NOT EXISTS model_budget_policies (
                policy_key TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                purpose TEXT,
                model_tier TEXT,
                effort TEXT,
                ceiling_model_tier TEXT,
                ceiling_effort TEXT,
                max_calls INTEGER,
                max_total_tokens INTEGER,
                max_input_tokens INTEGER,
                max_output_tokens INTEGER,
                max_cache_creation_tokens INTEGER,
                max_cache_read_tokens INTEGER,
                max_reasoning_tokens INTEGER,
                max_embedding_inputs INTEGER,
                max_wall_time_ms INTEGER,
                max_concurrency INTEGER,
                max_retries INTEGER,
                max_calls_per_window INTEGER,
                rate_window_seconds INTEGER,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS model_budget_counters (
                counter_key TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                purpose TEXT NOT NULL,
                model_tier TEXT NOT NULL,
                effort TEXT,
                call_count INTEGER NOT NULL DEFAULT 0,
                active_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                embedding_inputs INTEGER NOT NULL DEFAULT 0,
                wall_time_ms INTEGER NOT NULL DEFAULT 0,
                rate_window_started_at TEXT,
                rate_window_call_count INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );
            ",
        )?;
        for (column, col_type) in [
            ("reserved_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("reserved_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "reserved_cache_creation_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("reserved_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("reserved_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "reserved_embedding_input_count",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("reserved_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
            ("baseline_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("baseline_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "baseline_cache_creation_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("baseline_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("baseline_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "baseline_embedding_input_count",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("baseline_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            add_column_if_not_exists_tx(&tx, "model_invocations", column, col_type)?;
        }
        for (column, col_type) in [
            ("ceiling_model_tier", "TEXT"),
            ("ceiling_effort", "TEXT"),
            ("max_cache_creation_tokens", "INTEGER"),
            ("max_cache_read_tokens", "INTEGER"),
            ("max_reasoning_tokens", "INTEGER"),
            ("max_calls_per_window", "INTEGER"),
            ("rate_window_seconds", "INTEGER"),
        ] {
            add_column_if_not_exists_tx(&tx, "model_budget_policies", column, col_type)?;
        }
        for (column, col_type) in [
            ("cache_creation_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("rate_window_started_at", "TEXT"),
            ("rate_window_call_count", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            add_column_if_not_exists_tx(&tx, "model_budget_counters", column, col_type)?;
        }
        if sqlite_table_exists_tx(&tx, "sessions")? {
            let exprs = v73_session_exprs(&tx)?;
            let backfill_sql = format!(
                "
                INSERT INTO model_invocations (
                    id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                    provider, model, backend, model_tier, effort, trigger_source,
                    session_id, project_id, workflow_id, scheduled_job_id,
                    issue_tracker_id, issue_identifier, policy_snapshot_json,
                    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                    wall_time_ms, estimated_cost_usd, usage_confidence, dedup_key, created_at, started_at, completed_at
                )
                SELECT
                    lower(substr(hex(randomblob(16)),1,8) || '-' ||
                          substr(hex(randomblob(16)),1,4) || '-' ||
                          substr(hex(randomblob(16)),1,4) || '-' ||
                          substr(hex(randomblob(16)),1,4) || '-' ||
                          substr(hex(randomblob(16)),1,12)),
                    'session.launch.fresh',
                    'session_lifecycle',
                    'foreground',
                    'paid_capable',
                    'admitted',
                    CASE
                        WHEN {status_expr} = 'Failed' THEN 'failed'
                        WHEN {status_expr} IN ('Completed', 'Archived', 'Interrupted') THEN 'completed'
                        ELSE 'completed'
                    END,
                    {provider_expr},
                    {model_expr},
                    {backend_expr},
                    {model_tier_expr},
                    {effort_expr},
                    'legacy_backfill',
                    id,
                    {project_id_expr},
                    {workflow_id_expr},
                    {scheduled_job_id_expr},
                    {issue_tracker_id_expr},
                    {issue_identifier_expr},
                    json('{{\"source\":\"v73_backfill\",\"confidence\":\"stale\"}}'),
                    {total_input_tokens_expr},
                    {total_output_tokens_expr},
                    {total_cache_creation_tokens_expr},
                    {total_cache_read_tokens_expr},
                    {work_time_ms_expr},
                    {cost_usd_expr},
                    CASE
                        WHEN {total_input_tokens_expr} IS NOT NULL
                          OR {total_output_tokens_expr} IS NOT NULL
                          OR {total_cache_creation_tokens_expr} IS NOT NULL
                          OR {total_cache_read_tokens_expr} IS NOT NULL
                        THEN 'stale'
                        ELSE 'unavailable'
                    END,
                    'legacy-session:' || id,
                    {created_at_expr},
                    {created_at_expr},
                    {updated_at_expr}
                FROM sessions
                WHERE {session_kind_filter}
                  AND model_invocation_id IS NULL
                  AND NOT EXISTS (
                      SELECT 1
                      FROM model_invocations
                      WHERE dedup_key = 'legacy-session:' || sessions.id
                  );

                UPDATE sessions
                SET model_invocation_id = (
                    SELECT id FROM model_invocations
                    WHERE model_invocations.session_id = sessions.id
                      AND model_invocations.dedup_key = 'legacy-session:' || sessions.id
                    LIMIT 1
                )
                WHERE model_invocation_id IS NULL
                  AND {session_kind_filter};
                ",
                status_expr = exprs.status_expr,
                provider_expr = exprs.provider_expr,
                model_expr = exprs.model_expr,
                backend_expr = exprs.backend_expr,
                model_tier_expr = exprs.model_tier_expr,
                effort_expr = exprs.effort_expr,
                project_id_expr = exprs.project_id_expr,
                workflow_id_expr = exprs.workflow_id_expr,
                scheduled_job_id_expr = exprs.scheduled_job_id_expr,
                issue_tracker_id_expr = exprs.issue_tracker_id_expr,
                issue_identifier_expr = exprs.issue_identifier_expr,
                total_input_tokens_expr = exprs.total_input_tokens_expr,
                total_output_tokens_expr = exprs.total_output_tokens_expr,
                total_cache_creation_tokens_expr = exprs.total_cache_creation_tokens_expr,
                total_cache_read_tokens_expr = exprs.total_cache_read_tokens_expr,
                work_time_ms_expr = exprs.work_time_ms_expr,
                cost_usd_expr = exprs.cost_usd_expr,
                created_at_expr = exprs.created_at_expr,
                updated_at_expr = exprs.updated_at_expr,
                session_kind_filter = exprs.session_kind_filter,
            );
            tx.execute_batch(&backfill_sql)?;
        }
        tx.execute(
            "INSERT INTO daemon_settings (key, value, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO NOTHING",
            params![
                crate::store::model_control::KEY_MODEL_CONTROL_MODE,
                "normal",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

struct V73SessionExprs {
    status_expr: String,
    provider_expr: String,
    model_expr: String,
    backend_expr: String,
    model_tier_expr: String,
    effort_expr: String,
    project_id_expr: String,
    workflow_id_expr: String,
    scheduled_job_id_expr: String,
    issue_tracker_id_expr: String,
    issue_identifier_expr: String,
    total_input_tokens_expr: String,
    total_output_tokens_expr: String,
    total_cache_creation_tokens_expr: String,
    total_cache_read_tokens_expr: String,
    work_time_ms_expr: String,
    cost_usd_expr: String,
    created_at_expr: String,
    updated_at_expr: String,
    session_kind_filter: String,
}

fn v73_session_exprs(tx: &rusqlite::Transaction<'_>) -> Result<V73SessionExprs> {
    let has = |column: &str| column_exists_tx(tx, "sessions", column);
    let provider_expr = if has("provider")? {
        "lower(provider)".to_string()
    } else {
        "NULL".to_string()
    };
    let model_expr = if has("model")? {
        "model".to_string()
    } else {
        "NULL".to_string()
    };
    let effort_expr = if has("effort")? {
        "effort".to_string()
    } else {
        "NULL".to_string()
    };
    let backend_expr = if has("provider")? {
        "lower(provider)".to_string()
    } else {
        "NULL".to_string()
    };
    let model_tier_expr = if has("provider")? && has("model")? {
        "CASE
            WHEN lower(provider) = 'local' THEN 'local'
            WHEN model LIKE 'gpt-5%' OR model LIKE 'claude-%' OR model LIKE 'gemini-%' THEN 'premium'
            ELSE 'standard'
         END"
        .to_string()
    } else if has("provider")? {
        "CASE WHEN lower(provider) = 'local' THEN 'local' ELSE 'standard' END".to_string()
    } else {
        "'standard'".to_string()
    };
    Ok(V73SessionExprs {
        status_expr: if has("status")? {
            "status".to_string()
        } else {
            "'Completed'".to_string()
        },
        provider_expr,
        model_expr,
        backend_expr,
        model_tier_expr,
        effort_expr,
        project_id_expr: if has("project_id")? {
            "project_id".to_string()
        } else {
            "NULL".to_string()
        },
        workflow_id_expr: if has("workflow_id")? {
            "workflow_id".to_string()
        } else {
            "NULL".to_string()
        },
        scheduled_job_id_expr: if has("scheduled_job_id")? {
            "scheduled_job_id".to_string()
        } else {
            "NULL".to_string()
        },
        issue_tracker_id_expr: if has("issue_tracker_id")? {
            "issue_tracker_id".to_string()
        } else {
            "NULL".to_string()
        },
        issue_identifier_expr: if has("issue_identifier")? {
            "issue_identifier".to_string()
        } else {
            "NULL".to_string()
        },
        total_input_tokens_expr: if has("total_input_tokens")? {
            "total_input_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        total_output_tokens_expr: if has("total_output_tokens")? {
            "total_output_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        total_cache_creation_tokens_expr: if has("total_cache_creation_tokens")? {
            "total_cache_creation_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        total_cache_read_tokens_expr: if has("total_cache_read_tokens")? {
            "total_cache_read_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        work_time_ms_expr: if has("work_time_ms")? {
            "work_time_ms".to_string()
        } else if has("duration_ms")? {
            "duration_ms".to_string()
        } else {
            "NULL".to_string()
        },
        cost_usd_expr: if has("cost_usd")? {
            "cost_usd".to_string()
        } else {
            "NULL".to_string()
        },
        created_at_expr: if has("created_at")? {
            "created_at".to_string()
        } else {
            "CURRENT_TIMESTAMP".to_string()
        },
        updated_at_expr: if has("updated_at")? {
            "updated_at".to_string()
        } else if has("created_at")? {
            "created_at".to_string()
        } else {
            "CURRENT_TIMESTAMP".to_string()
        },
        session_kind_filter: if has("session_kind")? {
            "session_kind NOT IN ('Group', 'Epic')".to_string()
        } else {
            "1 = 1".to_string()
        },
    })
}
