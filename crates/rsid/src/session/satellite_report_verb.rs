//! `AgentReportToHub` (#1103): queue one short typed report for the hub
//! manager. Satellite side only.
//!
//! Authority is the current, unrevoked appointed manager that is also the operator-declared
//! seat (or its rotation tip), resolved from the token-bound caller. Every
//! refusal is the same `satellite_report_not_authorized`. The report is
//! informational: it carries no authority on the hub.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::satellite_dispatch::{
    AgentReportToHubReceiptV1, AgentReportToHubRequestV1, SATELLITE_REPORT_NOT_AUTHORIZED,
};
use uuid::Uuid;

impl AgentControlHandle {
    /// # Errors
    /// A stable `satellite_report_*` refusal, or a persistence error.
    pub async fn agent_report_to_hub(
        &self,
        caller: Uuid,
        request: AgentReportToHubRequestV1,
    ) -> Result<AgentReportToHubReceiptV1> {
        let store = self.store.lock().await;
        let is_manager = store
            .agent_authority_projection(caller)
            .map(|projection| projection.is_manager)
            .unwrap_or(false);
        // A revoked scope or policy keeps `is_manager` true but ends the
        // appointment: require it to be current and unrevoked.
        if !is_manager || !store.manager_appointment_active(caller).unwrap_or(false) {
            return Err(DaemonError::PolicyDenied(
                SATELLITE_REPORT_NOT_AUTHORIZED.into(),
            ));
        }
        store.queue_hub_report(caller, &request)
    }
}
