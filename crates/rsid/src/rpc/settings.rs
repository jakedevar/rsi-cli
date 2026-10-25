use super::*;

/// RPC params for `UpdateDaemonConfig`.
#[derive(Debug, Deserialize)]
pub struct UpdateDaemonConfigParams {
    pub field: String,
    pub value: serde_json::Value,
}

/// RPC params for the atomic form of `UpdateDaemonConfig` (#2236): one request
/// naming every field the operator applied from the Settings page. The request
/// holds either `field` + `value` or `updates`, never both.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateDaemonConfigBatchParams {
    pub updates: Vec<UpdateDaemonConfigParams>,
}

/// The most fields one atomic `UpdateDaemonConfig` may name.
pub(super) const UPDATE_DAEMON_CONFIG_BATCH_MAX: usize = 128;

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

/// #3154: the rows only arming `openrouter_spend_cap_usd` writes: the account
/// usage recorded at that moment and the time of the probe.
fn is_openrouter_spend_derived_field(field: &str) -> bool {
    matches!(
        field,
        "openrouter_spend_baseline_usd" | "openrouter_spend_armed_at"
    )
}

fn openrouter_spend_derived_refusal(field: &str) -> DaemonError {
    DaemonError::InvalidParam(format!(
        "Invalid value for field '{field}': written only by arming openrouter_spend_cap_usd, \
         which takes a fresh probe of the account usage"
    ))
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
        let credit = self.job_disk_credit(&params).await;
        let governor = crate::governor::Governor::global();
        let outcome = match &credit {
            Some((gb, ..)) => self
                .session_manager
                .acquire_resource_admission_with_credit(governor, &params, *gb),
            None => self
                .session_manager
                .acquire_resource_admission(governor, &params),
        }
        .map_err(DaemonError::InvalidParam)?;
        if let Some((gb, bytes, free, floor, target, log)) = credit
            && matches!(&outcome, crate::governor::AcquireOutcome::Granted { .. })
        {
            // This goes into the job log, alongside cargo-slot's grant. The
            // setting and every other ticket retain the original floor.
            let _ = tokio::task::spawn_blocking(move || {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(log) {
                    let _ = writeln!(file, "rsi-phase {} own-target-disk-credit: target={} reusable_bytes={bytes} credit_gib={gb:.3} free_gib={free:.3} floor_gib={floor}; quarter of own target, capped at 2 GiB and half the floor; reuse lowers only this job's estimated growth; physical and projected free space satisfy the floor", chrono::Utc::now().timestamp(), target.display());
                }
            }).await;
        }
        Ok(serde_json::to_value(outcome)?)
    }

    async fn job_disk_credit(
        &self,
        params: &crate::governor::AcquireParams,
    ) -> Option<(f64, u64, f64, u64, std::path::PathBuf, String)> {
        use crate::governor::own_target;
        if params.class == crate::governor::AdmissionClass::Lander {
            return None;
        }
        let floor = self.runtime_config.governor_policy().min_free_disk_gb;
        let pid = params.pid;
        let (free, (job_id, target, cwd)) = tokio::task::spawn_blocking(move || {
            let free = crate::host_pressure::disk_free_gb();
            // Below the physical floor no amount of target reuse admits work.
            // Above it, reuse may lower this job's projected growth.
            (crate::host_pressure::own_target_credit_gb(free, floor, 8 * 1_073_741_824) > 0.0)
                .then(|| own_target::process_context(pid).map(|context| (free, context)))
                .flatten()
        })
        .await
        .ok()??;
        let (job, sandbox) = {
            let store =
                crate::store_lock::lock(self.session_manager.store(), "rpc::job_disk_credit").await;
            let job = store.get_agent_job(job_id).ok()??.job;
            let sandbox = store
                .get_session(job.owner_session_id)
                .ok()??
                .sandbox_root?;
            (job, sandbox)
        };
        tokio::task::spawn_blocking(move || {
            let target =
                own_target::eligible_target(&job, std::path::Path::new(&sandbox), &target, &cwd)?;
            let bytes = own_target::reusable_bytes(&target);
            let credit = crate::host_pressure::own_target_credit_gb(free, floor, bytes);
            (credit > 0.0).then_some((credit, bytes, free, floor, target, job.log_path))
        })
        .await
        .ok()?
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
        let mut entry_ids = params.entry_ids;
        entry_ids.truncate(rsi_common::rolling_queue::ROLLING_QUEUE_WATCH_IDS_MAX);
        let store = self.session_manager.store().clone();
        let entries = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if entry_ids.is_empty() {
                return store.list_rolling_queue_entries(state, limit);
            }
            // #2068: a watch names its entries; the newest page can scroll
            // them away.
            let mut found = Vec::new();
            for id in entry_ids {
                if let Some(entry) = store.get_rolling_queue_entry(id)? {
                    found.push((entry, String::new()));
                }
            }
            Ok(found)
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
                gate_timeout_mins: load(&self.runtime_config.rolling_queue_gate_timeout_mins),
                entries: entries.into_iter().map(|(entry, _)| entry).collect(),
            },
        )?)
    }

    /// Operator-only `EnqueueRollingQueueSource` (#2055): the operator's path
    /// onto the merge queue, backed by the same enqueue logic as
    /// `AgentEnqueueLandingSource`.
    pub(super) async fn handle_enqueue_rolling_queue_source(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rolling_queue::EnqueueRollingQueueSourceParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {e}")))?;
        let enabled = self
            .runtime_config
            .rolling_queue_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        let response = self
            .session_manager
            .agent_control()
            .operator_enqueue_landing_source(params, enabled)
            .await?;
        Ok(serde_json::to_value(response)?)
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

    pub(super) async fn handle_get_daemon_info(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let schema = self
            .session_manager
            .store()
            .lock()
            .await
            .schema_user_version()?;
        let snapshot = tokio::task::spawn_blocking(move || {
            crate::daemon_info::DaemonInfoService::global().snapshot(schema)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))?;
        Ok(serde_json::to_value(snapshot)?)
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
            // #999: free space above the sandbox launch floor, so the manager
            // sees pressure before launches are refused, plus the latest scratch
            // reclaim pass.
            let configured_floor_gib = self
                .runtime_config
                .sandbox_min_free_gib
                .load(Ordering::Relaxed);
            if let Ok(stats) =
                nix::sys::statvfs::statvfs(self.session_manager.sandbox_allocator().base_dir())
            {
                let available =
                    (stats.blocks_available() as u64).saturating_mul(stats.fragment_size() as u64);
                let total = (stats.blocks() as u64).saturating_mul(stats.fragment_size() as u64);
                let floor_bytes =
                    crate::sandbox::effective_min_free_bytes(configured_floor_gib, total);
                object.insert(
                    "launch_floor".to_string(),
                    serde_json::json!({
                        "available_bytes": available,
                        "floor_bytes": floor_bytes,
                        "margin_bytes": i128::from(available) - i128::from(floor_bytes),
                    }),
                );
            }
            object.insert(
                "agent_scratch_reclaim".to_string(),
                serde_json::to_value(crate::agent_scratch_reclaim::last_report())?,
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
        config["billing_modes"] =
            serde_json::to_value(self.session_manager.store().lock().await.billing_modes()?)?;
        config["coordinator_context_budgets"] = serde_json::to_value(
            self.session_manager
                .agent_control()
                .coordinator_budgets()
                .await?,
        )?;
        // #1036: read-only summary for the Settings "Cloud spend" row.
        if let Some(map) = config.as_object_mut() {
            let report = self.cloud_spend_report();
            map.insert(
                "cloud_spend_status".to_string(),
                rsi_common::cloud_spend::summary_line(&report).into(),
            );
        }
        // #3154: the OpenRouter spend ceiling for the Settings row: cap, spent
        // since arming, remainder and state, from the last probe the daemon's
        // background loop keeps fresh (a settings read never waits on the
        // network).
        let spend = self.openrouter_spend_state(false).await;
        if let Some(map) = config.as_object_mut() {
            map.insert(
                "openrouter_spend_status".to_string(),
                rsi_common::openrouter_spend::summary_line(&spend).into(),
            );
            map.insert(
                "openrouter_spend".to_string(),
                serde_json::to_value(&spend)?,
            );
        }
        Ok(config)
    }

    /// #3154: the live state of the OpenRouter spend ceiling.
    pub(super) async fn openrouter_spend_state(
        &self,
        refresh: bool,
    ) -> rsi_common::openrouter_spend::OpenRouterSpendCapV1 {
        crate::openrouter_spend::SpendEnforcer::production(
            Arc::clone(&self.runtime_config),
            self.session_manager.store().clone(),
        )
        .state(refresh)
        .await
    }

    /// #3154: `UpdateDaemonConfig openrouter_spend_cap_usd`. Turning the cap on
    /// (from off) takes a FRESH probe of the account `total_usage` and records
    /// it as the baseline with the time; the stop line is baseline + cap. A
    /// cap that cannot get a baseline is not armed. Changing the amount of an
    /// armed cap keeps its baseline (the spend so far stays counted); `0`
    /// turns it off and forgets the baseline, so the next arming re-baselines.
    /// The baseline, the arming time and the cap are persisted (cap last) so
    /// the ceiling survives a restart.
    async fn update_openrouter_spend_cap(
        &self,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let enforcer = crate::openrouter_spend::SpendEnforcer::production(
            Arc::clone(&self.runtime_config),
            self.session_manager.store().clone(),
        );
        self.update_openrouter_spend_cap_with(value, &enforcer)
            .await
    }

    /// [`Self::update_openrouter_spend_cap`] with the enforcer injected, so a
    /// test arms against a mock OpenRouter account.
    pub(super) async fn update_openrouter_spend_cap_with(
        &self,
        value: &serde_json::Value,
        enforcer: &crate::openrouter_spend::SpendEnforcer<'_>,
    ) -> Result<serde_json::Value> {
        const FIELD: &str = "openrouter_spend_cap_usd";
        const ROWS: [&str; 4] = [
            "openrouter_spend_baseline_usd",
            "openrouter_spend_armed_at",
            FIELD,
            "openrouter_spend_warn_pct",
        ];
        let cap = crate::config::parse_openrouter_spend_cap_usd(value).map_err(|error| {
            DaemonError::InvalidParam(format!("Invalid value for field '{FIELD}': {error}"))
        })?;
        let previous = self.runtime_config.to_json();
        let previous_rows: Vec<(&str, serde_json::Value)> = ROWS
            .iter()
            .map(|field| (*field, previous[*field].clone()))
            .collect();
        let was_armed = {
            let config = self.runtime_config.openrouter_spend_config();
            config.cap_usd > 0.0 && config.baseline.is_some()
        };
        if cap > 0.0 && !was_armed {
            let probe = enforcer.fresh_baseline().await?;
            self.runtime_config
                .arm_openrouter_spend_cap(cap, probe.total_usage, probe.at);
        } else {
            self.runtime_config
                .update_field(FIELD, &serde_json::json!(cap))
                .map_err(|error| {
                    DaemonError::InvalidParam(format!("Invalid value for field '{FIELD}': {error}"))
                })?;
        }
        let persisted = {
            let guard = self.session_manager.store().lock().await;
            ROWS.iter().try_for_each(|field| {
                crate::store::daemon_settings::persist_runtime_config_field(
                    &guard,
                    &self.runtime_config,
                    field,
                )
                .map(|_| ())
            })
        };
        if let Err(error) = persisted {
            for (field, value) in &previous_rows {
                let _ = self.runtime_config.update_field(field, value);
            }
            let guard = self.session_manager.store().lock().await;
            for (field, _) in &previous_rows {
                let _ = crate::store::daemon_settings::persist_runtime_config_field(
                    &guard,
                    &self.runtime_config,
                    field,
                );
            }
            return Err(DaemonError::Store(format!(
                "Failed to persist daemon config field '{FIELD}': {error}"
            )));
        }
        Ok(serde_json::json!({
            "ok": true,
            "field": FIELD,
            "spend_cap": enforcer.state(false).await,
        }))
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

    /// hub #2765 / #281: check a `provider_launch_overrides` write against the
    /// installed provider CLIs. A Claude flag the installed claude CLI rejects
    /// as an unknown option, and a Codex config key the installed codex-cli does
    /// not know, are refused `provider_launch_override_unknown_key`. Runs before
    /// the config update lock (it may start a CLI). The static allowlist and
    /// deny list are checked by `update_field`; a value they refuse is left to
    /// them (the deny list stays authoritative: it never depends on the
    /// installed CLI). A CLI that cannot answer fails open: nothing is refused
    /// and the returned `launch_override_check` object says the Codex check was
    /// degraded. `None` when the write is not for this field.
    async fn check_launch_overrides_against_installed_cli(
        &self,
        field: &str,
        value: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        use rsi_common::provider_launch_overrides as overrides;
        if field != overrides::FIELD {
            return Ok(None);
        }
        let Ok(parsed) = overrides::normalize(value) else {
            return Ok(None);
        };
        let invalid = |error: overrides::LaunchOverrideError| {
            DaemonError::InvalidParam(format!("Invalid value for field '{field}': {error}"))
        };
        let contract = self.session_manager.provider_contract();
        if let Some(flag) = contract
            .claude_flags_unknown_to_installed_cli(&parsed.claude_flags())
            .await
            .first()
        {
            return Err(invalid(
                overrides::LaunchOverrideError::unknown_to_installed_claude(flag),
            ));
        }
        let codex = contract
            .codex_keys_unknown_to_installed_cli(&parsed.codex_keys())
            .await;
        if let Some(key) = codex.unknown.first() {
            return Err(invalid(
                overrides::LaunchOverrideError::unknown_to_installed_codex(key),
            ));
        }
        if let Some(reason) = codex.degraded {
            tracing::warn!(
                reason,
                "provider_launch_overrides Codex keys checked against the static allowlist only"
            );
        }
        Ok(Some(codex.to_json()))
    }

    /// The `launch_override_check` note of a single or batch write, refusing the
    /// write when the installed CLI does not know a key it names.
    async fn launch_override_check_of(
        &self,
        request: &RpcRequest,
    ) -> Result<Option<serde_json::Value>> {
        if request.params.get("updates").is_none() {
            let Ok(params) =
                serde_json::from_value::<UpdateDaemonConfigParams>(request.params.clone())
            else {
                return Ok(None);
            };
            return self
                .check_launch_overrides_against_installed_cli(&params.field, &params.value)
                .await;
        }
        let Ok(batch) =
            serde_json::from_value::<UpdateDaemonConfigBatchParams>(request.params.clone())
        else {
            return Ok(None);
        };
        let mut note = None;
        for update in &batch.updates {
            let checked = self
                .check_launch_overrides_against_installed_cli(&update.field, &update.value)
                .await?;
            note = note.or(checked);
        }
        Ok(note)
    }

    pub(super) async fn handle_update_daemon_config(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let note = self.launch_override_check_of(request).await?;
        let mut response = self.apply_update_daemon_config(request).await?;
        let (Some(note), Some(object)) = (note, response.as_object_mut()) else {
            return Ok(response);
        };
        object.insert("launch_override_check".into(), note);
        Ok(response)
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
    async fn apply_update_daemon_config(&self, request: &RpcRequest) -> Result<serde_json::Value> {
        if request.params.get("updates").is_some() {
            return self.handle_update_daemon_config_batch(request).await;
        }
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

        if params.field == "deploy_restarts_per_hour"
            || params.field == "supervisor_shutdown_grace_secs"
        {
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
                    .expect("previously validated deploy budget remains valid");
                return Err(DaemonError::Store(format!(
                    "Failed to persist daemon config field '{}': {}",
                    params.field, error
                )));
            }

            let snapshot_path =
                rsi_common::identity::data_path("deploy-restart-budget", "deploy-restart-budget");
            if let Err(error) = crate::deploy::write_supervisor_restart_budget_snapshot(
                &snapshot_path,
                &self.runtime_config,
            ) {
                self.runtime_config
                    .update_field(&params.field, &previous_value)
                    .expect("previously validated deploy budget remains valid");
                let rollback_db = {
                    let guard = store.lock().await;
                    crate::store::daemon_settings::persist_runtime_config_value(
                        &guard,
                        &params.field,
                        &previous_value,
                    )
                };
                let rollback_snapshot = crate::deploy::write_supervisor_restart_budget_snapshot(
                    &snapshot_path,
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
                    "Failed to refresh supervisor restart budget snapshot: {error}{rollback_details}"
                )));
            }
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

        if params.field == "openrouter_spend_cap_usd" {
            return self.update_openrouter_spend_cap(&params.value).await;
        }
        if is_openrouter_spend_derived_field(&params.field) {
            return Err(openrouter_spend_derived_refusal(&params.field));
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
                    // Mirror the caps for rsi-cloud spend. The scripts
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
                if let Some(report) =
                    crate::build_budget::after_setting_saved(&params.field, "setting").await
                {
                    // #2119: rewrite rsi-build.slice now, including for units
                    // already running in it. The setting is saved either way;
                    // the response says whether the slice carries it.
                    return Ok(serde_json::json!({
                        "ok": true,
                        "field": params.field,
                        "build_slice": report.to_json(),
                    }));
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

    /// The atomic form of `UpdateDaemonConfig`: `{"updates": [{"field", "value"}, ...]}`.
    ///
    /// All-or-nothing. Every value is validated before any live state moves
    /// (a bad value, an unknown field, a duplicate field or an invalid
    /// combination refuses the whole request and names the field). The
    /// durable rows are written in ONE SQLite transaction, the live values
    /// are published once, and each side effect that follows a save (the
    /// supervisor and rsid-scope snapshot files, the spend-caps file, the
    /// build CPU slice, the dreamer wake) runs at most once for the whole
    /// request. A failure after the live values moved restores the previous
    /// live values and durable rows before the error is returned.
    pub(super) async fn handle_update_daemon_config_batch(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateDaemonConfigBatchParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {e}")))?;
        let updates = params.updates;
        if updates.is_empty() {
            return Err(DaemonError::InvalidParam(
                "Invalid params: updates must name at least one field".into(),
            ));
        }
        if updates.len() > UPDATE_DAEMON_CONFIG_BATCH_MAX {
            return Err(DaemonError::InvalidParam(format!(
                "Invalid params: at most {UPDATE_DAEMON_CONFIG_BATCH_MAX} fields per update"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for update in &updates {
            if !seen.insert(update.field.as_str()) {
                return Err(DaemonError::InvalidParam(format!(
                    "Invalid params: field '{}' is named more than once",
                    update.field
                )));
            }
        }
        // #3154: arming the spend cap takes a fresh probe, which a batch
        // cannot do atomically; its baseline rows are written only by arming.
        if let Some(update) = updates.iter().find(|update| {
            update.field == "openrouter_spend_cap_usd"
                || is_openrouter_spend_derived_field(&update.field)
        }) {
            return Err(DaemonError::InvalidParam(format!(
                "Invalid value for field '{}': set the OpenRouter spend cap with its own \
                 UpdateDaemonConfig call (arming takes a fresh probe of the account usage); \
                 the baseline rows are written only by arming",
                update.field
            )));
        }
        let _update_guard = self.session_manager.daemon_config_update.lock().await;

        // Phase 1: validate everything before any live state moves.
        self.apply_daemon_config_batch_locked(updates).await
    }

    /// Caller holds daemon_config_update across snapshot, durable undo and apply.
    pub(super) async fn apply_daemon_config_batch_locked(
        &self,
        updates: Vec<UpdateDaemonConfigParams>,
    ) -> Result<serde_json::Value> {
        let invalid = |field: &str, error: String| {
            DaemonError::InvalidParam(format!("Invalid value for field '{field}': {error}"))
        };
        let scratch = RuntimeConfig::from_config(&crate::config::Config::default());
        let scope_in_batch = updates
            .iter()
            .any(|update| is_rsid_scope_config_field(&update.field));
        if scope_in_batch {
            // The scope limits are checked as a set (high below max, ...), so
            // the candidate starts from the live values.
            let live = self.runtime_config.to_json();
            for field in RSID_SCOPE_CONFIG_FIELDS {
                if let Some(value) = live.get(field) {
                    let _ = scratch.update_field(field, value);
                }
            }
        }
        let cache_before = self.runtime_config.sandbox_build_cache_reclaim_snapshot();
        let mut cache_next = cache_before;
        let mut cache_fields: Vec<&str> = Vec::new();
        let mut transcript_cap: Option<u64> = None;
        let mut codegraph: Option<bool> = None;
        let mut generic: Vec<&UpdateDaemonConfigParams> = Vec::new();
        for update in &updates {
            match RuntimeConfig::apply_sandbox_build_cache_field(
                cache_next,
                &update.field,
                &update.value,
            ) {
                Ok(Some(next)) => {
                    cache_next = next;
                    cache_fields.push(update.field.as_str());
                    continue;
                }
                Ok(None) => {}
                Err(error) => return Err(invalid(&update.field, error)),
            }
            if update.field == "completed_transcript_cache_max_bytes" {
                transcript_cap = Some(update.value.as_u64().ok_or_else(|| {
                    invalid(&update.field, "expected non-negative integer".into())
                })?);
                continue;
            }
            if update.field == "codegraph_indexing_enabled" {
                codegraph = Some(
                    update
                        .value
                        .as_bool()
                        .ok_or_else(|| invalid(&update.field, "expected bool".into()))?,
                );
                continue;
            }
            match scratch.update_field(&update.field, &update.value) {
                Ok(true) => generic.push(update),
                Ok(false) => {
                    return Err(DaemonError::InvalidParam(format!(
                        "Unknown config field: {}",
                        update.field
                    )));
                }
                Err(error) => return Err(invalid(&update.field, error)),
            }
        }
        if !cache_fields.is_empty() {
            // The watermark pair moves together: only the final combination
            // has to satisfy low < high.
            cache_next = cache_next.validate().map_err(|error| {
                DaemonError::InvalidParam(format!(
                    "Invalid value for fields {}: {error}",
                    cache_fields.join(", ")
                ))
            })?;
        }
        if scope_in_batch {
            validated_rsid_scope_settings_env(&scratch).map_err(|error| {
                DaemonError::InvalidParam(format!("Invalid rsid scope settings: {error}"))
            })?;
        }

        // Phase 2: move the live values, remembering how to put them back.
        let live_before = self.runtime_config.to_json();
        let mut applied: Vec<(&str, serde_json::Value)> = Vec::new();
        let undo = |applied: &[(&str, serde_json::Value)]| {
            for (field, previous) in applied.iter().rev() {
                if let Err(error) = self.runtime_config.update_field(field, previous) {
                    tracing::error!(
                        %field,
                        %error,
                        "could not restore a daemon config field after a refused update"
                    );
                }
            }
        };
        for update in &generic {
            let previous = live_before
                .get(&update.field)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match self
                .runtime_config
                .update_field(&update.field, &update.value)
            {
                Ok(true) => applied.push((update.field.as_str(), previous)),
                Ok(false) => {
                    undo(&applied);
                    return Err(DaemonError::InvalidParam(format!(
                        "Unknown config field: {}",
                        update.field
                    )));
                }
                Err(error) => {
                    undo(&applied);
                    return Err(invalid(&update.field, error));
                }
            }
        }

        // Phase 3: the durable rows, all in one transaction.
        let mut rows: Vec<(String, serde_json::Value)> = Vec::new();
        let mut previous_rows: Vec<(String, serde_json::Value)> = Vec::new();
        for (field, previous) in &applied {
            if let Some(value) = self.runtime_config.persisted_field_value(field) {
                rows.push(((*field).to_string(), value));
                previous_rows.push(((*field).to_string(), previous.clone()));
            }
        }
        if !cache_fields.is_empty() {
            rows.extend(
                crate::store::daemon_settings::sandbox_build_cache_batch_rows(
                    &cache_fields,
                    &cache_next,
                ),
            );
            previous_rows.extend(
                crate::store::daemon_settings::sandbox_build_cache_batch_rows(
                    &cache_fields,
                    &cache_before,
                ),
            );
        }
        if let Some(cap) = transcript_cap {
            rows.push((
                "completed_transcript_cache_max_bytes".into(),
                serde_json::json!(cap),
            ));
            previous_rows.push((
                "completed_transcript_cache_max_bytes".into(),
                serde_json::json!(
                    self.runtime_config
                        .completed_transcript_cache_max_bytes
                        .load(Ordering::Relaxed)
                ),
            ));
        }
        if let Some(enabled) = codegraph {
            rows.push((
                "codegraph_indexing_enabled".into(),
                serde_json::json!(enabled),
            ));
            previous_rows.push((
                "codegraph_indexing_enabled".into(),
                serde_json::json!(
                    self.runtime_config
                        .codegraph_indexing_enabled
                        .load(Ordering::Acquire)
                ),
            ));
        }
        // OFF is acknowledged only after any publication already holding this
        // gate has finished (the single-field rule).
        let _publication_guard = if codegraph.is_some() {
            Some(crate::codegraph::PUBLICATION_GATE.lock().await)
        } else {
            None
        };
        let store = self.session_manager.store().clone();
        let persisted = {
            let guard = store.lock().await;
            crate::store::daemon_settings::persist_runtime_config_batch(&guard, &rows)
        };
        if let Err(error) = persisted {
            undo(&applied);
            return Err(DaemonError::Store(format!(
                "Failed to persist daemon config fields: {error}; nothing was changed"
            )));
        }

        // Phase 4: the snapshot files other processes read. A failure here
        // rolls the whole request back.
        let deploy_snapshot_path =
            rsi_common::identity::data_path("deploy-restart-budget", "deploy-restart-budget");
        let write_snapshots = |deploy: bool, scope: bool| -> std::result::Result<(), String> {
            if deploy {
                crate::deploy::write_supervisor_restart_budget_snapshot(
                    &deploy_snapshot_path,
                    &self.runtime_config,
                )
                .map_err(|error| {
                    format!("Failed to refresh supervisor restart budget snapshot: {error}")
                })?;
            }
            if scope {
                write_rsid_scope_settings_snapshot(
                    &self.rsid_scope_settings_path,
                    &self.runtime_config,
                )
                .map_err(|error| {
                    format!("Failed to refresh rsid scope settings snapshot: {error}")
                })?;
            }
            Ok(())
        };
        let deploy_in_batch = generic.iter().any(|update| {
            matches!(
                update.field.as_str(),
                "deploy_restarts_per_hour" | "supervisor_shutdown_grace_secs"
            )
        });
        if let Err(message) = write_snapshots(deploy_in_batch, scope_in_batch) {
            undo(&applied);
            let rollback_db = {
                let guard = store.lock().await;
                crate::store::daemon_settings::persist_runtime_config_batch(&guard, &previous_rows)
            };
            let rollback_snapshot = write_snapshots(deploy_in_batch, scope_in_batch);
            let details = match (rollback_db, rollback_snapshot) {
                (Ok(_), Ok(())) => String::new(),
                (db, snapshot) => format!(
                    "; rollback db: {}; rollback snapshot: {}",
                    db.err().map_or_else(|| "ok".into(), |e| e.to_string()),
                    snapshot.err().unwrap_or_else(|| "ok".into()),
                ),
            };
            return Err(DaemonError::Store(format!(
                "{message}; the whole update was rolled back{details}"
            )));
        }

        // Phase 5: publish what was held back until the rows were durable.
        if !cache_fields.is_empty() {
            self.runtime_config
                .publish_sandbox_build_cache_config(cache_next);
        }
        if let Some(cap) = transcript_cap {
            self.session_manager
                .publish_completed_transcript_cache_cap(cap)
                .await;
        }
        if let Some(enabled) = codegraph {
            self.runtime_config
                .codegraph_indexing_enabled
                .store(enabled, Ordering::Release);
        }

        // Phase 6: once-per-request effects of a saved setting.
        let mut fields: Vec<&str> = updates.iter().map(|update| update.field.as_str()).collect();
        fields.sort_unstable();
        if updates.iter().any(|update| {
            matches!(
                update.field.as_str(),
                "cloud_spend_stop_line_usd" | "cloud_spend_daily_cap_usd"
            )
        }) {
            crate::cloud_spend::write_caps_file(
                &self.cloud_dir,
                &self.runtime_config.cloud_spend_caps(),
            )
            .map_err(|error| {
                DaemonError::Store(format!(
                    "Saved the update but could not write the spend caps file: {error}; \
                     the remote-gate scripts still use the previous caps"
                ))
            })?;
        }
        if updates.iter().any(|update| {
            matches!(
                update.field.as_str(),
                "dream_enabled"
                    | "dream_model"
                    | "dream_model_provider"
                    | "dream_model_base_url"
                    | "dream_model_api_key"
                    | "dream_observation_threshold"
                    | "dream_idle_secs"
                    | "dream_cooldown_secs"
            )
        }) && let Some(handle) = &self.dreamer_handle
        {
            handle.notify_control_change().await?;
        }
        let mut response = serde_json::json!({
            "ok": true,
            "fields": fields,
            "count": updates.len(),
        });
        for update in &updates {
            if let Some(report) =
                crate::build_budget::after_setting_saved(&update.field, "setting").await
            {
                response["build_slice"] = report.to_json();
                break;
            }
        }
        Ok(response)
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
