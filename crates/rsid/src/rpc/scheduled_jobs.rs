use super::*;

impl RpcServer {
    pub(super) async fn handle_create_scheduled_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::CreateScheduledJobParams =
            serde_json::from_value(request.params.clone())?;
        let now = chrono::Utc::now();
        rsi_common::schedule::validate_creatable(&params.schedule, now)
            .map_err(|e| DaemonError::Rpc(e))?;
        let next_fire_at = rsi_common::schedule::initial_next_fire_at(&params.schedule, now);

        let job = rsi_common::types::ScheduledJob {
            id: Uuid::new_v4(),
            name: params.name,
            message: params.message,
            schedule: params.schedule,
            last_fired_at: None,
            next_fire_at,
            enabled: true,
            working_dir: params.working_dir,
            provider: params.provider,
            model: params.model,
            project_id: params.project_id,
            created_at: now,
            updated_at: now,
            wake_mode: rsi_common::types::WakeMode::Fresh,
            wake_session_id: None,
        };

        let store = self.session_manager.store();
        let guard = store.lock().await;
        guard.insert_scheduled_job(&job)?;
        drop(guard);

        // Nudge scheduler to re-check (picks up newly created jobs immediately)
        if let Some(ref handle) = self.scheduler_handle {
            let _ = handle.check_now().await;
        }

        Ok(serde_json::to_value(&job)?)
    }

    /// Operator-only paged read (Issue #954 B). No params (an older client)
    /// means the default filter and the first page; see
    /// `Store::list_scheduled_jobs_page`.
    pub(super) async fn handle_list_scheduled_jobs(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::ListScheduledJobsParams = match &request.params {
            serde_json::Value::Null => Default::default(),
            value => serde_json::from_value(value.clone())?,
        };
        let limit = params
            .limit
            .unwrap_or(rsi_common::rpc::LIST_SCHEDULED_JOBS_DEFAULT_LIMIT)
            .clamp(1, rsi_common::rpc::LIST_SCHEDULED_JOBS_MAX_LIMIT);
        let store = self.session_manager.store();
        let guard = store.lock().await;
        let (jobs, next_cursor) = guard.list_scheduled_jobs_page(
            chrono::Utc::now(),
            params.include_history,
            limit as usize,
            params.cursor.as_deref(),
        )?;
        Ok(serde_json::to_value(
            &rsi_common::rpc::ListScheduledJobsResult {
                jobs,
                next_cursor,
                include_history: params.include_history,
            },
        )?)
    }

    /// Operator-only read: due wakes currently held while children run (#794 S3).
    pub(super) async fn handle_list_scheduled_job_holds(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let store = self.session_manager.store();
        let guard = store.lock().await;
        let holds = guard.list_scheduled_job_holds(chrono::Utc::now())?;
        Ok(serde_json::to_value(&holds)?)
    }

    pub(super) async fn handle_update_scheduled_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::UpdateScheduledJobParams =
            serde_json::from_value(request.params.clone())?;

        let store = self.session_manager.store();
        let guard = store.lock().await;
        if guard.program_guard_owner_for_job_id(&params.id)?.is_some() {
            return Err(DaemonError::InvalidParam(
                "closed program guards can only be rearmed by explicit program registration".into(),
            ));
        }

        // Validate new schedule against the resulting enabled state
        if let Some(ref schedule) = params.schedule {
            let now = chrono::Utc::now();
            let existing = guard.get_scheduled_job(&params.id)?.ok_or_else(|| {
                DaemonError::Rpc(format!("scheduled job {} not found", params.id))
            })?;
            let resulting_enabled = params.enabled.unwrap_or(existing.enabled);
            if resulting_enabled {
                rsi_common::schedule::validate_creatable(schedule, now)
                    .map_err(|e| DaemonError::Rpc(e))?;
            }
        }

        let mut update = crate::store::scheduled_jobs::ScheduledJobUpdate {
            name: params.name,
            message: params.message,
            schedule: None,
            enabled: params.enabled,
            next_fire_at: None,
        };

        if let Some(schedule) = params.schedule {
            let now = chrono::Utc::now();
            let next = rsi_common::schedule::initial_next_fire_at(&schedule, now);
            update.schedule = Some(schedule);
            update.next_fire_at = Some(next);
        }

        guard.update_scheduled_job(&params.id, &update)?;
        let job = guard.get_scheduled_job(&params.id)?;
        drop(guard);

        if let Some(ref handle) = self.scheduler_handle {
            let _ = handle.check_now().await;
        }

        Ok(serde_json::to_value(&job)?)
    }

    pub(super) async fn handle_delete_scheduled_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::DeleteScheduledJobParams =
            serde_json::from_value(request.params.clone())?;
        let store = self.session_manager.store();
        let guard = store.lock().await;
        guard.delete_scheduled_job(&params.id)?;
        Ok(serde_json::json!({"deleted": true}))
    }

    pub(super) async fn handle_toggle_scheduled_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::ToggleScheduledJobParams =
            serde_json::from_value(request.params.clone())?;
        let store = self.session_manager.store();
        let guard = store.lock().await;
        if guard.program_guard_owner_for_job_id(&params.id)?.is_some() {
            return Err(DaemonError::InvalidParam(
                "closed program guards can only be rearmed by explicit program registration".into(),
            ));
        }
        let new_state = guard.toggle_scheduled_job(&params.id)?;
        drop(guard);

        if let Some(ref handle) = self.scheduler_handle {
            let _ = handle.check_now().await;
        }

        Ok(serde_json::json!({"enabled": new_state}))
    }

    pub(super) async fn handle_trigger_scheduled_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::DeleteScheduledJobParams =
            serde_json::from_value(request.params.clone())?;
        {
            let store = self.session_manager.store();
            let guard = store.lock().await;
            if guard.program_guard_owner_for_job_id(&params.id)?.is_some() {
                return Err(DaemonError::InvalidParam(
                    "closed program guards can only be triggered after explicit program registration"
                        .into(),
                ));
            }
        }
        match &self.scheduler_handle {
            Some(handle) => {
                handle.trigger_now(params.id).await?;
                Ok(serde_json::json!({"triggered": true}))
            }
            None => Err(DaemonError::InvalidParam("Scheduler not enabled".into())),
        }
    }
}
