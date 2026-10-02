use super::*;

/// RPC params for `UpdateDaemonConfig`.
#[derive(Debug, Deserialize)]
pub struct UpdateDaemonConfigParams {
    pub field: String,
    pub value: serde_json::Value,
}

pub(super) const RSID_SCOPE_CONFIG_FIELDS: [&str; 8] = [
    "rsid_scope_memory_high_mib",
    "rsid_scope_memory_max_mib",
    "rsid_scope_memory_swap_max_mib",
    "rsid_scope_cpu_weight",
    "worker_scope_memory_high_mib",
    "worker_scope_memory_max_mib",
    "worker_scope_memory_swap_max_mib",
    "worker_scope_cpu_weight",
];

pub(super) fn is_rsid_scope_config_field(field: &str) -> bool {
    RSID_SCOPE_CONFIG_FIELDS.contains(&field)
}

pub(super) fn validated_rsid_scope_settings_env(
    runtime_config: &RuntimeConfig,
) -> std::io::Result<String> {
    let settings = runtime_config.to_json();
    let number = |field: &str| {
        settings
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{field} must be an unsigned integer"),
                )
            })
    };
    let high = number(RSID_SCOPE_CONFIG_FIELDS[0])?;
    let max = number(RSID_SCOPE_CONFIG_FIELDS[1])?;
    let swap_max = number(RSID_SCOPE_CONFIG_FIELDS[2])?;
    let cpu_weight = number(RSID_SCOPE_CONFIG_FIELDS[3])?;
    let worker_high = number(RSID_SCOPE_CONFIG_FIELDS[4])?;
    let worker_max = number(RSID_SCOPE_CONFIG_FIELDS[5])?;
    let worker_swap_max = number(RSID_SCOPE_CONFIG_FIELDS[6])?;
    let worker_cpu_weight = number(RSID_SCOPE_CONFIG_FIELDS[7])?;
    let memory_min = crate::config::RSID_SCOPE_MEMORY_LIMIT_MIB_MIN;
    let memory_max = crate::config::RSID_SCOPE_MEMORY_LIMIT_MIB_MAX;
    let cpu_min = u64::from(crate::config::RSID_SCOPE_CPU_WEIGHT_MIN);
    let cpu_max = u64::from(crate::config::RSID_SCOPE_CPU_WEIGHT_MAX);
    if !(memory_min..=memory_max).contains(&high)
        || !(memory_min..=memory_max).contains(&max)
        || high >= max
        || swap_max > memory_max
        || !(cpu_min..=cpu_max).contains(&cpu_weight)
        || !(memory_min..=memory_max).contains(&worker_high)
        || !(memory_min..=memory_max).contains(&worker_max)
        || worker_high >= worker_max
        || worker_swap_max > memory_max
        || !(cpu_min..=cpu_max).contains(&worker_cpu_weight)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rsid scope settings are outside their validated limits",
        ));
    }
    Ok(format!(
        "{}={high}\n{}={max}\n{}={swap_max}\n{}={cpu_weight}\n{}={worker_high}\n{}={worker_max}\n{}={worker_swap_max}\n{}={worker_cpu_weight}\n",
        RSID_SCOPE_CONFIG_FIELDS[0],
        RSID_SCOPE_CONFIG_FIELDS[1],
        RSID_SCOPE_CONFIG_FIELDS[2],
        RSID_SCOPE_CONFIG_FIELDS[3],
        RSID_SCOPE_CONFIG_FIELDS[4],
        RSID_SCOPE_CONFIG_FIELDS[5],
        RSID_SCOPE_CONFIG_FIELDS[6],
        RSID_SCOPE_CONFIG_FIELDS[7],
    ))
}

pub(super) fn write_rsid_scope_settings_snapshot(
    path: &std::path::Path,
    runtime_config: &RuntimeConfig,
) -> std::io::Result<()> {
    use std::io::Write;

    let contents = validated_rsid_scope_settings_env(runtime_config)?;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rsid scope settings path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

impl RpcServer {
    /// Operator-only read of the rolling merge queue and its live settings.
    pub(super) async fn handle_acquire_admission(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: crate::governor::AcquireParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {e}")))?;
        let policy = self.runtime_config.governor_policy();
        let outcome = crate::governor::Governor::global()
            .acquire(&policy, &params)
            .map_err(DaemonError::InvalidParam)?;
        Ok(serde_json::to_value(outcome)?)
    }

    pub(super) async fn handle_release_admission(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: crate::governor::ReleaseParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {e}")))?;
        let policy = self.runtime_config.governor_policy();
        let released = crate::governor::Governor::global().release(&policy, params.lease_id);
        Ok(serde_json::json!({ "released": released }))
    }

    pub(super) async fn handle_get_resource_governor(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let policy = self.runtime_config.governor_policy();
        Ok(serde_json::to_value(
            crate::governor::Governor::global().snapshot(&policy),
        )?)
    }

    pub(super) async fn handle_get_rolling_queue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rolling_queue::GetRollingQueueParams = if request.params.is_null() {
            rsi_common::rolling_queue::GetRollingQueueParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?
        };
        let limit = (params.limit.unwrap_or(50) as usize)
            .clamp(1, rsi_common::rolling_queue::ROLLING_QUEUE_LIST_MAX);
        let state = params.state;
        let store = self.session_manager.store().clone();
        let entries = tokio::task::spawn_blocking(move || {
            store
                .blocking_lock()
                .list_rolling_queue_entries(state, limit)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {e}")))??;
        let load =
            |field: &std::sync::atomic::AtomicU32| field.load(std::sync::atomic::Ordering::Relaxed);
        Ok(serde_json::to_value(
            rsi_common::rolling_queue::RollingQueueResponse {
                enabled: self
                    .runtime_config
                    .rolling_queue_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                batch_size: load(&self.runtime_config.rolling_queue_batch_size),
                speculation_depth: load(&self.runtime_config.rolling_queue_speculation_depth),
                entries: entries.into_iter().map(|(entry, _)| entry).collect(),
            },
        )?)
    }

    pub(super) async fn handle_restart_daemon_drain(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "RestartDaemonDrain takes no parameters".into(),
            ));
        }
        self.session_manager.request_drain_restart();
        let mut status = self.session_manager.drain_restart_status().await;
        match self.session_manager.notify_managers_of_drain().await {
            Ok(count) => status["manager_notices_queued"] = serde_json::json!(count),
            Err(error) => {
                tracing::warn!(%error, "DRAIN manager notice deferred");
                status["manager_notice_error"] = serde_json::json!(error.to_string());
            }
        }
        Ok(status)
    }

    pub(super) async fn handle_get_drain_restart_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "GetDrainRestartStatus takes no parameters".into(),
            ));
        }
        Ok(self.session_manager.drain_restart_status().await)
    }

    pub(super) async fn handle_get_health_status(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let status = self.session_manager.get_health_status().await;
        let mut value = serde_json::to_value(&status)?;
        // #1014: gate margins and the admission queue ride along on health.
        if let Some(object) = value.as_object_mut() {
            let policy = self.runtime_config.governor_policy();
            object.insert(
                "resource_governor".to_string(),
                serde_json::to_value(crate::governor::Governor::global().snapshot(&policy))?,
            );
        }
        Ok(value)
    }

    pub(super) async fn handle_get_daemon_capabilities(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::rpc::DaemonCapabilities;
        let mut caps = DaemonCapabilities::default();
        let live_scheduler_control = self
            .runtime_config
            .recursive_dag_live_scheduler_control_enabled
            .load(Ordering::Relaxed);
        caps.issue_tracker = self.issue_tracker_manager.is_some();
        caps.scheduler = self.scheduler_handle.is_some();
        caps.recursive_dag_run_inspection = true;
        caps.recursive_dag_recovery_status = true;
        caps.recursive_dag_recovery_control = self
            .runtime_config
            .recursive_dag_recovery_controls_enabled
            .load(Ordering::Relaxed);
        caps.recursive_dag_scheduler_control = self
            .runtime_config
            .recursive_dag_scheduler_controls_enabled
            .load(Ordering::Relaxed);
        caps.recursive_dag_live_scheduler_control = live_scheduler_control;
        caps.recursive_dag_cancellation_control = self
            .runtime_config
            .recursive_dag_cancellation_controls_enabled
            .load(Ordering::Relaxed);
        caps.recursive_dag_live_status_inspection = true;
        caps.recursive_dag_live_validation_inspection = true;
        caps.recursive_dag_artifact_lookup = true;
        caps.recursive_dag_artifact_list_pagination = true;
        caps.recursive_dag_artifact_preview_inspection = true;
        caps.recursive_dag_live_execution = live_scheduler_control;
        caps.recursive_dag_background_loop = false;
        caps.satellite_session_read = true;
        caps.gv_render_recursive_origin = self
            .runtime_config
            .gv_render_recursive_origin
            .load(Ordering::Relaxed);
        caps.gv_info_dashboard = self
            .runtime_config
            .gv_info_dashboard
            .load(Ordering::Relaxed);
        Ok(serde_json::to_value(caps)?)
    }

    pub(super) async fn handle_get_daemon_config(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let mut config = self.runtime_config.to_json();
        // #1036: read-only summary for the Settings "Cloud spend" row.
        if let Some(map) = config.as_object_mut() {
            let report = self.cloud_spend_report();
            map.insert(
                "cloud_spend_status".to_string(),
                rsi_common::cloud_spend::summary_line(&report).into(),
            );
        }
        Ok(config)
    }

    pub(super) fn cloud_spend_report(&self) -> rsi_common::cloud_spend::CloudSpendReport {
        crate::cloud_spend::report(
            &self.cloud_dir,
            &self.runtime_config.cloud_spend_caps(),
            chrono::Utc::now().date_naive(),
        )
    }

    /// #1036: per-run and per-day remote-gate spend against the operator's
    /// caps. Operator-only and read-only; editing the caps goes through
    /// `UpdateDaemonConfig` (`cloud_spend_stop_line_usd`,
    /// `cloud_spend_daily_cap_usd`).
    pub(super) async fn handle_get_cloud_spend(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "GetCloudSpend takes no parameters".into(),
            ));
        }
        Ok(serde_json::to_value(self.cloud_spend_report())?)
    }

    /// Update a single runtime-mutable daemon config field by name.
    ///
    /// String-valued fields validate their input — e.g. `codex_sandbox_mode`
    /// accepts only `"read-only" | "workspace-write" | "danger-full-access"`
    /// (case-insensitive; underscore aliases like `danger_full_access` are
    /// normalized). Invalid inputs return `InvalidParam`; the lock is not
    /// mutated on rejection. Unknown field names return `InvalidParam`
    /// with the field name echoed.
    ///
    /// Mutations take effect on the next read from the affected lock — for
    /// `codex_sandbox_mode` that's the next fresh `CodexClient::launch()`
    /// (every Codex spawn re-reads at launch time; no daemon restart
    /// required).
    pub(super) async fn handle_update_daemon_config(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateDaemonConfigParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;

        let _update_guard = self.session_manager.daemon_config_update.lock().await;
        if let Some(prepared) = self
            .runtime_config
            .prepare_sandbox_build_cache_update(&params.field, &params.value)
            .map_err(|error| {
                DaemonError::InvalidParam(format!(
                    "Invalid value for field '{}': {}",
                    params.field, error
                ))
            })?
        {
            let store = self.session_manager.store().clone();
            let store = store.lock().await;
            crate::store::daemon_settings::persist_sandbox_build_cache_config_update(
                &store,
                &params.field,
                prepared,
            )
            .map_err(|error| {
                DaemonError::Store(format!(
                    "Failed to persist daemon config field '{}': {}",
                    params.field, error
                ))
            })?;
            drop(store);
            self.runtime_config
                .publish_sandbox_build_cache_config(prepared);
            return Ok(serde_json::json!({ "ok": true, "field": params.field }));
        }

        if params.field == "completed_transcript_cache_max_bytes" {
            let cap = params.value.as_u64().ok_or_else(|| {
                DaemonError::InvalidParam(
                    "Invalid value for field 'completed_transcript_cache_max_bytes': expected non-negative integer"
                        .into(),
                )
            })?;
            // Persist before publication. The cache mutex is also the read
            // admission boundary, so an acknowledged shrink (including zero)
            // has already evicted excess entries and no queued reader can
            // re-admit using an older cap.
            let store = self.session_manager.store().clone();
            let guard = store.lock().await;
            crate::store::daemon_settings::persist_runtime_config_value(
                &guard,
                &params.field,
                &params.value,
            )
            .map_err(|error| {
                DaemonError::Store(format!(
                    "Failed to persist daemon config field '{}': {}",
                    params.field, error
                ))
            })?;
            drop(guard);
            self.session_manager
                .publish_completed_transcript_cache_cap(cap)
                .await;
            return Ok(serde_json::json!({ "ok": true, "field": params.field }));
        }

        if is_rsid_scope_config_field(&params.field) {
            let previous_value = self.runtime_config.to_json()[&params.field].clone();
            self.runtime_config
                .update_field(&params.field, &params.value)
                .map_err(|error| {
                    DaemonError::InvalidParam(format!(
                        "Invalid value for field '{}': {}",
                        params.field, error
                    ))
                })?;

            let store = self.session_manager.store().clone();
            let persist_result = {
                let guard = store.lock().await;
                crate::store::daemon_settings::persist_runtime_config_field(
                    &guard,
                    &self.runtime_config,
                    &params.field,
                )
            };
            if let Err(error) = persist_result {
                self.runtime_config
                    .update_field(&params.field, &previous_value)
                    .expect("previously validated rsid scope setting remains valid");
                return Err(DaemonError::Store(format!(
                    "Failed to persist daemon config field '{}': {}",
                    params.field, error
                )));
            }

            if let Err(error) = write_rsid_scope_settings_snapshot(
                &self.rsid_scope_settings_path,
                &self.runtime_config,
            ) {
                self.runtime_config
                    .update_field(&params.field, &previous_value)
                    .expect("previously validated rsid scope setting remains valid");
                let rollback_db = {
                    let guard = store.lock().await;
                    crate::store::daemon_settings::persist_runtime_config_value(
                        &guard,
                        &params.field,
                        &previous_value,
                    )
                };
                let rollback_snapshot = write_rsid_scope_settings_snapshot(
                    &self.rsid_scope_settings_path,
                    &self.runtime_config,
                );
                let rollback_details = match (rollback_db, rollback_snapshot) {
                    (Ok(_), Ok(())) => String::new(),
                    (db, snapshot) => format!(
                        "; rollback db: {}; rollback snapshot: {}",
                        db.err().map_or_else(|| "ok".into(), |e| e.to_string()),
                        snapshot.map_or_else(|e| e.to_string(), |_| "ok".into()),
                    ),
                };
                return Err(DaemonError::Store(format!(
                    "Failed to refresh rsid scope settings snapshot: {error}{rollback_details}"
                )));
            }
        }

        if params.field == "codegraph_indexing_enabled" {
            let enabled = params.value.as_bool().ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "Invalid value for field '{}': expected bool",
                    params.field
                ))
            })?;
            // OFF is acknowledged only after any publication already holding
            // this gate has finished. A waiting worker rechecks OFF under it.
            let _publication_guard = crate::codegraph::PUBLICATION_GATE.lock().await;
            let store = self.session_manager.store().clone();
            let store = store.lock().await;
            crate::store::daemon_settings::persist_runtime_config_value(
                &store,
                &params.field,
                &params.value,
            )
            .map_err(|error| {
                DaemonError::Store(format!(
                    "Failed to persist daemon config field '{}': {}",
                    params.field, error
                ))
            })?;
            drop(store);
            self.runtime_config
                .codegraph_indexing_enabled
                .store(enabled, std::sync::atomic::Ordering::Release);
            return Ok(serde_json::json!({ "ok": true, "field": params.field }));
        }

        match self
            .runtime_config
            .update_field(&params.field, &params.value)
        {
            Ok(true) => {
                // Write-through durable daemon settings. RuntimeConfig remains
                // the live source of truth; SQLite backs user-initiated changes
                // across daemon rebuilds/restarts.
                if crate::config::is_persisted_runtime_config_field(&params.field) {
                    let store = self.session_manager.store().clone();
                    let guard = store.lock().await;
                    if let Err(e) = crate::store::daemon_settings::persist_runtime_config_field(
                        &guard,
                        &self.runtime_config,
                        &params.field,
                    ) {
                        tracing::error!(
                            field = %params.field,
                            error = %e,
                            "Failed to persist daemon config field to daemon_settings"
                        );
                        return Err(DaemonError::Store(format!(
                            "Failed to persist daemon config field '{}': {}",
                            params.field, e
                        )));
                    }
                }
                if matches!(
                    params.field.as_str(),
                    "cloud_spend_stop_line_usd" | "cloud_spend_daily_cap_usd"
                ) {
                    // Mirror the caps for scripts/cloud-spend.py. The scripts
                    // keep the previous caps if this fails, so say so.
                    crate::cloud_spend::write_caps_file(
                        &self.cloud_dir,
                        &self.runtime_config.cloud_spend_caps(),
                    )
                    .map_err(|error| {
                        DaemonError::Store(format!(
                            "Saved '{}' but could not write the spend caps file: {error}; \
                             the remote-gate scripts still use the previous caps",
                            params.field
                        ))
                    })?;
                }
                if matches!(
                    params.field.as_str(),
                    "dream_enabled"
                        | "dream_model"
                        | "dream_model_provider"
                        | "dream_model_base_url"
                        | "dream_model_api_key"
                        | "dream_observation_threshold"
                        | "dream_idle_secs"
                        | "dream_cooldown_secs"
                ) && let Some(handle) = &self.dreamer_handle
                {
                    handle.notify_control_change().await?;
                }
                Ok(serde_json::json!({ "ok": true, "field": params.field }))
            }
            Ok(false) => Err(DaemonError::InvalidParam(format!(
                "Unknown config field: {}",
                params.field
            ))),
            Err(e) => Err(DaemonError::InvalidParam(format!(
                "Invalid value for field '{}': {}",
                params.field, e
            ))),
        }
    }

    /// Return the most recent stall-classifier verdict + metadata for the
    /// requested `session_id`, or `null` if the session is not in the
    /// active map or has never been classified.
    ///
    /// Response shape (when present):
    /// ```json
    /// {
    ///   "session_id": "<uuid>",
    ///   "verdict": "Finished" | "NeedsUser" | "StalledContinue" | "StalledCheckTeam",
    ///   "classified_at": "<rfc3339>",
    ///   "count": <u32>
    /// }
    /// ```
    /// `null` (JSON) when the session has no recorded classification —
    /// either because the classifier never ran or because the session left
    /// the active map (terminal status, restart). RSI-0XX Phase 5: the
    /// TUI's `gV` keybinding uses this to render the per-session
    /// classifier overlay.
    pub(super) async fn handle_get_classification_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        #[derive(serde::Deserialize)]
        struct Params {
            session_id: Uuid,
        }
        let params: Params = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let active = self.session_manager.active();
        let guard = active.read().await;
        let tracked = match guard.get(&params.session_id) {
            Some(t) => t,
            None => return Ok(serde_json::Value::Null),
        };
        let (verdict, classified_at) = match (tracked.last_verdict, tracked.last_classified_at) {
            (Some(v), Some(t)) => (v, t),
            _ => return Ok(serde_json::Value::Null),
        };
        Ok(serde_json::json!({
            "session_id": params.session_id,
            "verdict": verdict.as_str(),
            "classified_at": classified_at.to_rfc3339(),
            "count": tracked.classification_count,
        }))
    }
}
