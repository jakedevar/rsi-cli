//! `AgentListWakes` (#2389): a session lists its own armed wakes. The store
//! scopes the read to an owner (`wake_session_id`); the owner is the caller
//! unless `session_id` names a live worker the caller's `AgentWorkerRoster`
//! scope covers (#2418, `Store::roster_authorizes_session`). The RPC handler
//! and the native tools share this entry. Read-only: nothing here writes.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_control_schema::{AgentListWakesParams, LIST_WAKES_DEFAULT_LIMIT};
use rsi_common::agent_wake_list::AgentListWakesResultV1;
use uuid::Uuid;

impl AgentControlHandle {
    /// # Errors
    /// `InvalidParam` for a `limit` outside 1..=256, `list_wakes_target_not_authorized` for a `session_id` outside the caller's
    /// roster scope, or a persistence error.
    pub async fn agent_list_wakes(
        &self,
        caller: Uuid,
        params: AgentListWakesParams,
    ) -> Result<AgentListWakesResultV1> {
        if !params.limit_is_valid() {
            return Err(DaemonError::InvalidParam(
                "limit must be between 1 and 256".into(),
            ));
        }
        let limit = params.limit.unwrap_or(LIST_WAKES_DEFAULT_LIMIT) as usize;
        // One extra row tells the caller whether the page was truncated.
        let mut wakes = {
            let store = self.store.lock().await;
            let _admission = super::rpc_admission::admit_current_rpc(caller).await?;
            store.list_wake_views_for_reader(
                caller,
                params.session_id,
                params.include_disabled,
                limit + 1,
            )?
        };
        let truncated = wakes.len() > limit;
        wakes.truncate(limit);
        Ok(AgentListWakesResultV1 {
            count: wakes.len(),
            wakes,
            truncated,
        })
    }
}
