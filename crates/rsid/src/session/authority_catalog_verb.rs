//! `AgentGetAuthorityCatalog`: the agent's callable operator's manual.
//!
//! Every RSI-managed session may call it. The caller is transport-bound (RPC
//! token or native-tool registration); the answer is rendered from one durable
//! [`agent_authority_projection`](crate::store::Store::agent_authority_projection)
//! read, the same projection role-gated verbs are advertised from. It is
//! read-only and grants nothing: every other verb keeps its own guard.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_authority_catalog::{
    AgentAuthorityCatalogV1, AgentGetAuthorityCatalogRequestV1,
};
use uuid::Uuid;

impl AgentControlHandle {
    /// # Errors
    /// `authority_catalog_unknown_verb` for an unknown `verb`, the projection's
    /// caller refusal, or a persistence error.
    pub async fn agent_get_authority_catalog(
        &self,
        caller: Uuid,
        request: AgentGetAuthorityCatalogRequestV1,
    ) -> Result<AgentAuthorityCatalogV1> {
        let requested = request
            .requested_verb()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let projection = self.store.lock().await.agent_authority_projection(caller)?;
        super::preamble::render_authority_catalog(caller, &projection, requested)
    }
}
