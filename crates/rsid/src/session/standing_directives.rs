//! `AgentStandingDirective` and the delivery reads of standing operator
//! directives (#2649).
//!
//! Authority, scope derivation, provenance and idempotency live in the store
//! (`Store::standing_directive_agent_actor`, `standing_directive_set`,
//! `standing_directive_clear`). The caller is the token-resolved session; its
//! manager seat decides which scopes it may write, and no author comes from
//! request JSON. The operator-only RPCs (`StandingDirectiveSet/Clear/List`) are
//! handled in `rpc/standing_directives.rs`.

use chrono::Utc;
use rsi_common::standing_directives::{
    AgentStandingDirectiveCommandV1, AgentStandingDirectiveRequestV1,
    AgentStandingDirectiveResultV1, render_standing_directives,
};
use uuid::Uuid;

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::standing_directives::StandingDirectiveSetInput;

impl AgentControlHandle {
    /// `AgentStandingDirective`: relay a standing operator directive to a
    /// manager scope strictly below the caller's own, or clear one a manager
    /// relayed.
    ///
    /// # Errors
    /// `standing_directive_not_authorized`, `standing_directive_invalid_request`,
    /// `standing_directive_not_found`, `standing_directive_limit`,
    /// `standing_directive_idempotency_conflict`, `manager_tier_target_unknown`
    /// or a persistence error.
    pub async fn agent_standing_directive(
        &self,
        caller: Uuid,
        request: AgentStandingDirectiveRequestV1,
    ) -> Result<AgentStandingDirectiveResultV1> {
        let command = request
            .command()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let store = self.store.lock().await;
        let actor = store.standing_directive_agent_actor(caller)?;
        let write = match command {
            AgentStandingDirectiveCommandV1::Set {
                scope,
                text,
                provenance,
                kind,
                expires_in_secs,
                idempotency_key,
            } => store.standing_directive_set(
                &actor,
                &StandingDirectiveSetInput {
                    scope,
                    text: &text,
                    provenance: provenance.as_deref(),
                    kind,
                    expires_in_secs,
                    idempotency_key: &idempotency_key,
                },
                Utc::now(),
            )?,
            AgentStandingDirectiveCommandV1::Clear { directive_id } => {
                store.standing_directive_clear(&actor, directive_id, Utc::now())?
            }
        };
        Ok(AgentStandingDirectiveResultV1 {
            directive: write.directive,
            deduplicated: write.deduplicated,
        })
    }
}

impl AgentControlHandle {
    /// The directives active on `scope` now, for an overview. A read failure
    /// is logged and reads as none: an overview must not fail on this list.
    pub(super) async fn active_standing_directives(
        &self,
        scope: rsi_common::manager_tier_routing::ManagerNodeRefV1,
    ) -> Vec<rsi_common::standing_directives::StandingDirectiveV1> {
        match self
            .store
            .lock()
            .await
            .active_standing_directives(scope, Utc::now())
        {
            Ok(directives) => directives,
            Err(error) => {
                tracing::warn!(scope = %scope.as_ref_text(), %error, "standing directive read failed");
                Vec::new()
            }
        }
    }
}

/// The active directives of the manager seat `session` holds, rendered for a
/// seat to read; `None` for a session that holds no seat or when nothing is
/// active. A read failure is logged and reads as nothing: a seat's launch or
/// catalog call must not fail on this note.
pub(super) fn seat_directives_block(store: &Store, session: Uuid) -> Option<String> {
    match store.standing_directives_for_seat(session, Utc::now()) {
        Ok(directives) => render_standing_directives(&directives),
        Err(error) => {
            tracing::warn!(%session, %error, "standing directive read failed");
            None
        }
    }
}

#[cfg(test)]
#[path = "standing_directives_tests.rs"]
mod tests;
