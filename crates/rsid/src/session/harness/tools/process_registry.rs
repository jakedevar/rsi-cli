use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};

use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use tokio::sync::Mutex as TokioMutex;
use uuid::Uuid;

use super::exec::{self, ProcessRegistry};

#[async_trait::async_trait]
pub(crate) trait ProcessWakeController: Send + Sync {
    async fn arm(&self) -> Result<(), String>;
    async fn withdraw(&self);
}

pub(crate) struct HarnessProcessRegistryManager {
    registries: StdMutex<HashMap<Uuid, Arc<ProcessRegistry>>>,
    runtime: Arc<ProcessRegistryRuntime>,
}

struct ProcessRegistryRuntime {
    store: std::sync::OnceLock<Arc<TokioMutex<crate::store::Store>>>,
    scheduler: std::sync::OnceLock<crate::scheduler::SchedulerHandle>,
}

impl HarnessProcessRegistryManager {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            registries: StdMutex::new(HashMap::new()),
            runtime: Arc::new(ProcessRegistryRuntime {
                store: std::sync::OnceLock::new(),
                scheduler: std::sync::OnceLock::new(),
            }),
        })
    }

    pub(crate) fn install_store(&self, store: Arc<TokioMutex<crate::store::Store>>) {
        let _ = self.runtime.store.set(store);
    }

    pub(crate) fn install_scheduler_handle(&self, scheduler: crate::scheduler::SchedulerHandle) {
        let _ = self.runtime.scheduler.set(scheduler);
    }

    pub(crate) async fn clear_stale_wakes(&self) {
        let Some(store) = self.runtime.store.get() else {
            return;
        };
        let sessions = {
            let guard = store.lock().await;
            guard
                .load_all_session_ids()
                .map_err(|error| error.to_string())
        };
        let sessions = match sessions {
            Ok(session_ids) => session_ids,
            Err(error) => {
                tracing::warn!(%error, "Failed to load sessions for background process wake cleanup");
                return;
            }
        };
        for session_id in sessions {
            let result = store
                .lock()
                .await
                .cancel_internal_process_wake(session_id)
                .map_err(|error| error.to_string());
            if let Err(error) = result
                && !error.contains("wake_not_found")
            {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "Failed to withdraw stale background process wake"
                );
            }
        }
    }

    pub(crate) fn resolve(&self, session_id: Uuid) -> Arc<ProcessRegistry> {
        let mut registries = self
            .registries
            .lock()
            .expect("Harness process registry map lock");
        if let Some(registry) = registries.get(&session_id) {
            return Arc::clone(registry);
        }
        let registry = exec::default_process_registry();
        registry.set_wake_controller(Arc::new(ScheduledProcessWakeController {
            runtime: Arc::clone(&self.runtime),
            session_id,
        }));
        registries.insert(session_id, Arc::clone(&registry));
        registry
    }

    pub(crate) async fn shutdown_session(&self, session_id: Uuid) {
        let registry = self
            .registries
            .lock()
            .expect("Harness process registry map lock")
            .remove(&session_id);
        if let Some(registry) = registry {
            registry.shutdown_processes().await;
        }
    }

    pub(crate) async fn shutdown_all(&self) {
        let registries: Vec<Arc<ProcessRegistry>> = {
            self.registries
                .lock()
                .expect("Harness process registry map lock")
                .drain()
                .map(|(_, registry)| registry)
                .collect()
        };
        for registry in registries {
            registry.shutdown_processes().await;
        }
    }
}

impl ProcessRegistryRuntime {
    async fn arm(&self, session_id: Uuid) -> Result<(), String> {
        let store = self
            .store
            .get()
            .ok_or_else(|| "harness process store unavailable".to_string())?;
        let scheduler = self
            .scheduler
            .get()
            .ok_or_else(|| "harness process scheduler unavailable".to_string())?;
        let now = chrono::Utc::now();
        let job = ScheduledJob {
            id: Uuid::new_v4(),
            name: format!("background-process-{session_id}"),
            message: "Background command finished".to_string(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(session_id),
        };
        store
            .lock()
            .await
            .insert_scheduled_job_replacing_name(session_id, &job)
            .map_err(|error| error.to_string())?;
        if let Err(error) = scheduler.check_now().await {
            return Err(error.to_string());
        }
        Ok(())
    }

    async fn withdraw(&self, session_id: Uuid) {
        let Some(store) = self.store.get() else {
            return;
        };
        let result = store
            .lock()
            .await
            .cancel_internal_process_wake(session_id)
            .map_err(|error| error.to_string());
        if let Err(error) = result
            && !error.contains("wake_not_found")
        {
            tracing::warn!(session_id = %session_id, %error, "Failed to withdraw background process wake");
        }
    }
}

struct ScheduledProcessWakeController {
    runtime: Arc<ProcessRegistryRuntime>,
    session_id: Uuid,
}

#[async_trait::async_trait]
impl ProcessWakeController for ScheduledProcessWakeController {
    async fn arm(&self) -> Result<(), String> {
        self.runtime.arm(self.session_id).await
    }

    async fn withdraw(&self) {
        self.runtime.withdraw(self.session_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsid_store::test_support::make_test_session;

    fn process_wake(session_id: Uuid) -> ScheduledJob {
        let now = chrono::Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: format!("background-process-{session_id}"),
            message: "Background command finished".to_string(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(session_id),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn startup_cleanup_removes_terminal_session_process_wakes() {
        let store = crate::store::Store::open_in_memory().expect("store");
        let mut session = make_test_session();
        session.status = rsi_common::types::SessionStatus::Archived;
        store.insert_session(&session).expect("session");
        let job = process_wake(session.id);
        store.insert_scheduled_job(&job).expect("wake");

        let manager = HarnessProcessRegistryManager::new();
        manager.install_store(Arc::new(TokioMutex::new(store)));
        manager.clear_stale_wakes().await;

        let store = manager.runtime.store.get().expect("installed store");
        let remaining = store
            .lock()
            .await
            .list_owned_scheduled_jobs(session.id, false, 64)
            .expect("scheduled jobs");
        assert!(remaining.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn registry_manager_reuses_one_registry_across_turns() {
        let manager = HarnessProcessRegistryManager::new();
        let session_id = Uuid::new_v4();
        let first = manager.resolve(session_id);
        let second = manager.resolve(session_id);
        assert!(Arc::ptr_eq(&first, &second));

        manager.shutdown_session(session_id).await;
        let rebuilt = manager.resolve(session_id);
        assert!(!Arc::ptr_eq(&first, &rebuilt));
    }
}
