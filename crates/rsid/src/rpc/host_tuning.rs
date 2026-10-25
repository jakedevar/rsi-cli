//! Operator-owned host tuning and the manager's bounded saved-preset switch.
use super::*;
use rsi_common::host_tuning::*;
use std::collections::BTreeMap;

fn invalid(message: impl Into<String>) -> DaemonError {
    DaemonError::InvalidParam(message.into())
}
fn operator_only(request: &RpcRequest) -> Result<()> {
    if request.session_token.is_some() {
        return Err(DaemonError::PolicyDenied(
            "host_tuning_operator_only".into(),
        ));
    }
    Ok(())
}
fn actual(config: &serde_json::Value) -> Values {
    RESOURCE_KEYS
        .iter()
        .filter_map(|(key, _)| config[*key].as_u64().map(|v| ((*key).into(), v)))
        .collect()
}

impl RpcServer {
    async fn read_tuning_state(&self) -> Result<TuningState> {
        let store = self.session_manager.store().lock().await;
        store.get_daemon_setting(STORAGE_KEY)?.map_or_else(
            || Ok(TuningState::default()),
            |raw| {
                serde_json::from_str(&raw).map_err(|error| {
                    DaemonError::Store(format!("host tuning state unreadable: {error}"))
                })
            },
        )
    }
    async fn save_tuning_state(&self, state: &mut TuningState) -> Result<()> {
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("host tuning revision overflow"))?;
        self.session_manager
            .store()
            .lock()
            .await
            .set_daemon_setting(STORAGE_KEY, &serde_json::to_string(state)?)?;
        Ok(())
    }
    async fn tuning_view(&self, state: TuningState) -> Result<TuningView> {
        let data = rsi_common::identity::data_dir();
        let sandbox = self
            .session_manager
            .sandbox_allocator()
            .base_dir()
            .to_path_buf();
        let probe =
            tokio::task::spawn_blocking(move || crate::host_tuning_probe::probe(&data, &sandbox))
                .await
                .map_err(|e| DaemonError::Store(e.to_string()))?;
        Ok(TuningView {
            state, probe, actual: actual(&self.runtime_config.to_json()),
            apply_classes: RESOURCE_KEYS.iter().map(|(key, class)| ((*key).into(), (*class).into())).collect(),
            notes: vec![
                "Reserve is a host preference; Slice A does not derive settings or enforce a reserve automatically.".into(),
                "Live-session cap is guidance only; manager grants still govern concurrency.".into(),
                "Test threads are a scoped_test job option, not a saved daemon setting; the runner adapts them to load.".into(),
                "Worker quotas apply to new scopes; existing scopes keep their launch quota. Build CPU slice enforcement requires Linux/systemd; inspect settings_result.build_slice.".into(),
                "RAM is host MemTotal/MemAvailable, not a cgroup memory allowance.".into(),
            ],
        })
    }
    pub(super) async fn handle_get_host_tuning(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        operator_only(request)?;
        let _guard = self.session_manager.daemon_config_update.lock().await;
        Ok(serde_json::to_value(
            self.tuning_view(self.read_tuning_state().await?).await?,
        )?)
    }
    pub(super) async fn handle_configure_host_tuning(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        operator_only(request)?;
        let params: ConfigureHostTuning =
            serde_json::from_value(request.params.clone()).map_err(|e| invalid(e.to_string()))?;
        params.configuration.validate().map_err(invalid)?;
        // Validate the complete candidate without publishing scratch globals.
        // Apply also passes RuntimeConfig's shared batch validator.
        for values in params.configuration.presets.values() {
            validate_resource_values(values)?;
        }
        for (key, bounds) in &params.configuration.bounds {
            validate_resource_values(&BTreeMap::from([(key.clone(), bounds.min)]))?;
            validate_resource_values(&BTreeMap::from([(key.clone(), bounds.max)]))?;
        }
        let _guard = self.session_manager.daemon_config_update.lock().await;
        let mut state = self.read_tuning_state().await?;
        if state.revision != params.expected_revision {
            return Err(invalid(
                "host_tuning_revision_changed: refresh before saving",
            ));
        }
        // An operator may tighten bounds while an undo exists. Undo is then
        // refused until they explicitly widen the bound again; never bypass it.
        if state.configuration != params.configuration {
            state.configuration = params.configuration;
            state.active = None;
            self.save_tuning_state(&mut state).await?;
        }
        Ok(serde_json::to_value(self.tuning_view(state).await?)?)
    }
    pub(super) async fn handle_switch_host_preset(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: SwitchHostPreset =
            serde_json::from_value(request.params.clone()).map_err(|e| invalid(e.to_string()))?;
        params.validate().map_err(invalid)?;
        let caller = if request.method == "AgentSwitchHostPreset" {
            Some(self.resolve_caller_session_id(request).await?)
        } else {
            operator_only(request)?;
            None
        };
        let _guard = self.session_manager.daemon_config_update.lock().await;
        let mut state = self.read_tuning_state().await?;
        let before = actual(&self.runtime_config.to_json());
        let (target, next_active, undoing) = match params {
            SwitchHostPreset::Apply { name } => {
                if state.pending && state.undo.as_ref().is_some_and(|undo| undo.target != name) {
                    return Err(invalid(
                        "host_tuning_pending: undo or retry the interrupted switch first",
                    ));
                }
                let values = state
                    .configuration
                    .presets
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| invalid("host_tuning_preset_unknown"))?;
                (values, Some(name), false)
            }
            SwitchHostPreset::Undo {} => match &state.undo {
                Some(undo) => (undo.values.clone(), undo.previous_active.clone(), true),
                None => {
                    if let Some(caller) = caller {
                        self.authorize_preset(caller, &Values::new()).await?;
                    }
                    return Ok(serde_json::to_value(SwitchReceipt {
                        changed: false,
                        complete: true,
                        error: None,
                        settings_result: None,
                        view: self.tuning_view(state).await?,
                    })?);
                }
            },
        };
        state
            .configuration
            .validate_values(&target)
            .map_err(invalid)?;
        validate_resource_values(&target)?;
        if let Some(caller) = caller {
            self.authorize_preset(caller, &target).await?;
        }
        let changed = target
            .iter()
            .any(|(key, value)| before.get(key) != Some(value));
        let updates: Vec<_> = target
            .iter()
            .filter(|(key, value)| state.pending || before.get(*key) != Some(*value))
            .map(|(key, value)| super::settings::UpdateDaemonConfigParams {
                field: key.clone(),
                value: serde_json::json!(value),
            })
            .collect();
        if updates.is_empty() && !state.pending && !undoing && state.active == next_active {
            return Ok(serde_json::to_value(SwitchReceipt {
                changed: false,
                complete: true,
                error: None,
                settings_result: None,
                view: self.tuning_view(state).await?,
            })?);
        }
        if !undoing && !state.pending {
            let prior = target
                .keys()
                .map(|key| {
                    before
                        .get(key)
                        .map(|value| (key.clone(), *value))
                        .ok_or_else(|| invalid(format!("{key}: read-back unavailable")))
                })
                .collect::<Result<Values>>()?;
            state.undo = Some(UndoRecord {
                values: prior,
                previous_active: state.active.clone(),
                target: next_active.clone().unwrap_or_default(),
            });
        }
        state.active = None;
        state.pending = true;
        // Durable before effects: a crash can always recover with undo.
        self.save_tuning_state(&mut state).await?;
        let applied = if !updates.is_empty() {
            self.apply_daemon_config_batch_locked(updates).await
        } else {
            Ok(serde_json::json!({"ok":true,"count":0}))
        };
        let readback = actual(&self.runtime_config.to_json());
        let (settings_result, error) = match applied {
            Ok(report) => {
                let effect_error = report
                    .get("build_slice")
                    .filter(|r| r["state"] == "unavailable" || r["parent_state"] == "unavailable")
                    .map(|r| {
                        format!(
                            "Settings saved; build CPU slice unavailable: {}",
                            r["detail"]
                        )
                    });
                (Some(report), effect_error)
            }
            Err(error) => (None, Some(error.to_string())),
        };
        let matches = target
            .iter()
            .all(|(key, value)| readback.get(key) == Some(value));
        let complete = error.is_none() && matches;
        if complete {
            state.pending = false;
            state.active = next_active.filter(|name| {
                state.configuration.presets.get(name).is_some_and(|preset| {
                    preset
                        .iter()
                        .all(|(key, value)| readback.get(key) == Some(value))
                })
            });
            if undoing {
                state.undo = None;
            }
        }
        // If receipt persistence fails, the earlier pending record still
        // contains the original undo and the returned error names actual values.
        if let Err(error) = self.save_tuning_state(&mut state).await {
            return Ok(serde_json::to_value(SwitchReceipt {
                changed,
                complete: false,
                error: Some(format!("{error}; pending undo retained")),
                settings_result,
                view: self.tuning_view(self.read_tuning_state().await?).await?,
            })?);
        }
        Ok(serde_json::to_value(SwitchReceipt {
            changed,
            complete,
            error: error.or_else(|| {
                (!matches).then(|| "resource read-back differs from requested values".into())
            }),
            settings_result,
            view: self.tuning_view(state).await?,
        })?)
    }
    async fn authorize_preset(&self, caller: Uuid, values: &Values) -> Result<()> {
        use crate::store::harness_manager_v2::ManagerCallerV1;
        use rsi_common::agent_control_schema::AgentControlVerbV1;
        let store = self.session_manager.store().lock().await;
        let projection = store.agent_authority_projection(caller)?;
        if !projection
            .verbs
            .contains(&AgentControlVerbV1::SwitchHostPreset)
        {
            return Err(DaemonError::PolicyDenied(
                "host_tuning_manager_required".into(),
            ));
        }
        let bounds = if let Some(grant) = store.portfolio_seat_grant(caller)? {
            grant.project_policy.daemon_setting_bounds
        } else {
            match store.resolve_manager_caller(caller, None)? {
                ManagerCallerV1::Legacy {
                    config,
                    is_manager: true,
                } => {
                    store
                        .get_harness_manager_policy(config.project_id)?
                        .ok_or_else(|| invalid("host_tuning_manager_required"))?
                        .policy
                        .daemon_setting_bounds
                }
                ManagerCallerV1::Area(authority) => authority.grant.policy.daemon_setting_bounds,
                _ => {
                    return Err(DaemonError::PolicyDenied(
                        "host_tuning_manager_required".into(),
                    ));
                }
            }
        };
        // Saved tuning bounds always bind; any explicit manager-policy bound
        // additionally narrows them. Missing manager bounds do not grant raw writes.
        for (key, value) in values {
            if let Some(bound) = bounds.iter().find(|bound| &bound.key == key)
                && (bound.is_disabled() || !(bound.min..=bound.max).contains(value))
            {
                return Err(DaemonError::PolicyDenied(format!(
                    "host_tuning_manager_bound: {key}"
                )));
            }
        }
        Ok(())
    }
}

/// Pure save-time validation, mirroring RuntimeConfig's closed numeric ranges.
/// Apply still uses RuntimeConfig and its coupled validation/persistence hooks.
fn validate_resource_values(values: &Values) -> Result<()> {
    for (key, value) in values {
        let (min, max) = match key.as_str() {
            "worker_cpu_quota_percent" => (
                0,
                u64::from(rsid_core::worker_cpu_quota::WORKER_CPU_QUOTA_PERCENT_MAX),
            ),
            "hub_build_cpu_percent" => (
                u64::from(rsid_core::build_slice::HUB_BUILD_CPU_PERCENT_MIN),
                u64::from(rsid_core::build_slice::HUB_BUILD_CPU_PERCENT_MAX),
            ),
            "host_load_admission_threshold" => (
                0,
                u64::from(rsi_common::agent_daemon_info::HOST_LOAD_THRESHOLD_MAX),
            ),
            "governor_build_slots" | "governor_check_slots" | "governor_lander_slots" => (1, 16),
            "governor_min_avail_mem_gb" | "governor_min_free_disk_gb" => (0, 4096),
            "agent_build_jobs" => (1, 32),
            "agent_build_slots" => (1, 64),
            "agent_build_sccache_cache_gib" => (1, 512),
            "sandbox_min_free_gib" => (0, 1024),
            "disk_pressure_min_free_gb" => (1, 4096),
            "target_pool_keep" => (
                crate::config::TARGET_POOL_KEEP_MIN,
                crate::config::TARGET_POOL_KEEP_MAX,
            ),
            "sandbox_build_cache_reclaim_ttl_secs" => (
                rsi_common::sandbox_storage::SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MIN,
                rsi_common::sandbox_storage::SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MAX,
            ),
            _ => return Err(invalid(format!("{key}: unsupported resource setting"))),
        };
        if !(min..=max).contains(value) {
            return Err(invalid(format!("{key}: expected {min}..{max}")));
        }
    }
    Ok(())
}
