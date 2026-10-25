//! Operator provider spend grants (#3417): the spend charge, the launches an
//! active grant admits and the refusal an exhausted one names. The types and
//! the rules are `rsi_common::provider_grant`; this module reads the spend
//! from `model_invocations` (settled `estimated_cost_usd`, the increment rule
//! of `openrouter_spend`) and files the one operator to-do.

use super::Store;
use crate::error::Result;
use chrono::{DateTime, Utc};
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::provider_grant::{
    ManagerProviderGrantV2, ProviderGrantStateV1, ProviderGrantStatusV1, provider_grant_raise_path,
};
use rsi_common::types::SessionProvider;
use uuid::Uuid;

/// The `model_invocations.provider` values one grant's provider bills under.
/// OpenRouter sessions on the Harness route record `Harness`, so the
/// OpenRouter charge counts both (the safe direction for a spend limit).
fn billed_provider_names(provider: SessionProvider) -> Vec<&'static str> {
    match provider {
        SessionProvider::OpenRouter => vec!["OpenRouter", "Harness"],
        other => vec![super::row_mappers::session_provider_to_str(other)],
    }
}

/// One to-do key per project, provider and budget: a raise changes the key,
/// so a later exhaustion of the raised grant files a fresh item.
fn exhausted_todo_key(project: Uuid, status: &ProviderGrantStatusV1) -> String {
    format!(
        "provider_grant_exhausted:{project}:{:?}:{}",
        status.provider, status.budget_usd
    )
}

impl Store {
    /// What `grant` has spent since it was granted: the granting project's
    /// own settled usage of the provider (its sessions and its bench runs),
    /// never another project's.
    pub fn provider_grant_spend_usd(
        &self,
        project: Uuid,
        grant: &ManagerProviderGrantV2,
    ) -> Result<f64> {
        let since = grant.granted_at.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        self.provider_spend_settled_since_for_project(
            &billed_provider_names(grant.provider),
            since,
            Some(project),
        )
    }

    /// The live status of each of `project`'s grants at `now`.
    pub fn provider_grant_statuses(
        &self,
        project: Uuid,
        grants: &[ManagerProviderGrantV2],
        now: DateTime<Utc>,
    ) -> Result<Vec<ProviderGrantStatusV1>> {
        grants
            .iter()
            .map(|grant| Ok(grant.status(self.provider_grant_spend_usd(project, grant)?, now)))
            .collect()
    }

    /// The grants of the project's live (not revoked) policy.
    pub fn project_provider_grants(&self, project: Uuid) -> Result<Vec<ManagerProviderGrantV2>> {
        Ok(self
            .get_harness_manager_policy(project)?
            .filter(|policy| !policy.revoked)
            .map(|policy| policy.policy.provider_grants)
            .unwrap_or_default())
    }

    /// The wildcard launch choices the project's ACTIVE grants admit now. An
    /// expired or spent grant contributes nothing, so it behaves like no
    /// grant.
    pub fn project_provider_grant_launches(
        &self,
        project: Uuid,
    ) -> Result<Vec<ManagerLaunchChoiceV2>> {
        let grants = self.project_provider_grants(project)?;
        if grants.is_empty() {
            return Ok(Vec::new());
        }
        let now = Utc::now();
        let mut launches = Vec::new();
        for (grant, status) in grants
            .iter()
            .zip(self.provider_grant_statuses(project, &grants, now)?)
        {
            if status.state == ProviderGrantStateV1::Active {
                launches.extend(grant.launch_choices());
            }
        }
        Ok(launches)
    }

    /// The USD left under the project's active grant that admits `launch`.
    pub fn provider_grant_remaining_usd(
        &self,
        project: Uuid,
        launch: &ManagerLaunchChoiceV2,
    ) -> Result<Option<f64>> {
        let grants = self.project_provider_grants(project)?;
        let now = Utc::now();
        for status in self.provider_grant_statuses(project, &grants, now)? {
            if status.provider == launch.provider
                && status.state == ProviderGrantStateV1::Active
                && status.allows_effort(launch.effort.as_deref())
            {
                return Ok(Some(status.remaining_usd));
            }
        }
        Ok(None)
    }

    /// Why the project's provider grant, if any, does not admit `launch`:
    /// the refusal text that names the grant, the spend and the screen that
    /// raises it. A spent grant also files the ONE operator to-do (a replay
    /// changes nothing). `None` when the provider has no grant, or the grant
    /// admits the launch.
    #[must_use]
    pub fn provider_grant_refusal_hint(
        &self,
        project: Uuid,
        launch: &ManagerLaunchChoiceV2,
    ) -> Option<String> {
        let grants = self.project_provider_grants(project).ok()?;
        let grant = grants
            .iter()
            .find(|grant| grant.provider == launch.provider)?;
        let status = self
            .provider_grant_statuses(project, std::slice::from_ref(grant), Utc::now())
            .ok()?
            .pop()?;
        let refusal = status.refusal_for(launch.effort.as_deref())?;
        if status.state == ProviderGrantStateV1::Exhausted {
            self.file_provider_grant_exhausted_todo(project, &status);
        }
        Some(refusal)
    }

    /// The operator to-do for a spent grant. Best effort: a refusal must
    /// never fail because the to-do could not be written.
    fn file_provider_grant_exhausted_todo(&self, project: Uuid, status: &ProviderGrantStatusV1) {
        let key = exhausted_todo_key(project, status);
        let title = format!("{} (project {project})", status.summary);
        let how = format!(
            "Agents on this project are refused new {:?} launches and bench jobs until the provider grant is raised or renewed: {}",
            status.provider,
            provider_grant_raise_path(status.provider)
        );
        if let Err(error) = self.operator_todo_system_notice(&key, Some(project), &title, &how, 2) {
            tracing::warn!(%error, key, "provider grant exhausted to-do not filed");
        }
    }
}
