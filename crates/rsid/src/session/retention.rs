//! Periodic bounded logical archival of delivered, idle terminal sessions.

use super::SessionManager;
use crate::error::Result;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

fn retention_ticker() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

pub async fn run_retention_loop(manager: Arc<SessionManager>) {
    let mut interval = retention_ticker();
    let mut after = None;
    loop {
        interval.tick().await;
        if let Err(error) = manager.run_retention_pass(&mut after).await {
            tracing::warn!(%error, "Session retention pass deferred");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test(start_paused = true)]
    async fn retention_ticker_runs_on_boot_then_every_ten_minutes() {
        let mut ticker = retention_ticker();
        let first = ticker.tick().await;
        let second = ticker.tick().await;
        assert_eq!(second - first, SWEEP_INTERVAL);
        let third = ticker.tick().await;
        assert_eq!(third - second, SWEEP_INTERVAL);
    }
}

impl SessionManager {
    pub(crate) async fn run_retention_pass(&self, after: &mut Option<String>) -> Result<()> {
        if !self
            .runtime_config
            .session_retention_enabled
            .load(Ordering::Relaxed)
        {
            return Ok(());
        }
        let (candidates, next) = self
            .store
            .lock()
            .await
            .retention_candidates(after.as_deref())?;
        *after = next;
        let mut archived = Vec::new();
        let mut held = BTreeMap::<&'static str, usize>::new();
        for id in candidates {
            if !self
                .runtime_config
                .session_retention_enabled
                .load(Ordering::Relaxed)
            {
                *after = None;
                break;
            }
            let hours = self
                .runtime_config
                .session_retention_window_hours
                .load(Ordering::Relaxed) as i64;
            let guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
            if self.active.read().await.contains_key(&id) {
                *held.entry("active").or_default() += 1;
                continue;
            }
            let result = self.store.lock().await.retention_archive_one(id, hours);
            drop(guard);
            match result {
                Ok(None) => {
                    tracing::info!(%id, "Session retention archived");
                    archived.push(id);
                }
                Ok(Some(reason)) => {
                    tracing::debug!(%id, reason, "Session retention held");
                    *held.entry(reason).or_default() += 1;
                }
                Err(error) => {
                    *held.entry("error").or_default() += 1;
                    tracing::warn!(%id, %error, "Session retention candidate deferred");
                }
            }
        }
        self.finish_agent_archive(&archived).await;
        tracing::info!(
            archived = archived.len(),
            ?held,
            "Session retention pass complete"
        );
        Ok(())
    }
}
