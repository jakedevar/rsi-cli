//! Operator provider spend grants (#3417).
//!
//! One operator action per paid provider: a grant on the project policy names
//! a provider, a USD budget, an optional expiry and the efforts it allows.
//! While it has budget left, agents in scope may launch any model of that
//! provider at those efforts and submit bench jobs that bill that provider,
//! with no per-model `allowed_launches` tuple. The daemon charges settled
//! `estimated_cost_usd` against the grant, refuses new launches and bench jobs
//! at the budget (naming the grant, the spend and the screen that raises it)
//! and files one operator to-do. A grant is a real-money control: only the
//! operator RPC and the TUI policy screen write it; agent verbs read it.
//!
//! The grant never widens a list by itself: it expands to wildcard-model
//! [`ManagerLaunchChoiceV2`] entries (`model = "*"`) only while it is active,
//! so the existing exact-tuple entries keep working unchanged and every
//! ancestor grant still narrows the result.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::harness_manager_v2::{ManagerLaunchChoiceV2, text};
use crate::types::SessionProvider;

/// The model of a launch choice an active provider grant expands to: every
/// model of the provider. Rejected in an operator-written `allowed_launches`
/// list, so a provider-wide launch exists only under a budgeted grant.
pub const PROVIDER_GRANT_ANY_MODEL: &str = "*";
/// The most provider grants one policy carries.
pub const MANAGER_V2_MAX_PROVIDER_GRANTS: usize = 8;
/// The largest budget one grant may carry, in USD.
pub const MANAGER_V2_MAX_PROVIDER_GRANT_USD: f64 = 1_000_000.0;
/// The most efforts one grant lists.
pub const MANAGER_V2_MAX_PROVIDER_GRANT_EFFORTS: usize = 8;

/// Refusal: a launch whose provider grant is spent.
pub const PROVIDER_GRANT_EXHAUSTED: &str = "manager_v2_provider_grant_exhausted";
/// Refusal: a launch whose provider grant expired.
pub const PROVIDER_GRANT_EXPIRED: &str = "manager_v2_provider_grant_expired";
/// Refusal: a launch at an effort the provider grant does not allow.
pub const PROVIDER_GRANT_EFFORT: &str = "manager_v2_provider_grant_effort";
/// Refusal: an agent-reachable policy write that carries a provider grant.
pub const PROVIDER_GRANT_OPERATOR_ONLY: &str = "manager_v2_provider_grant_operator_only";

/// Where the operator creates, raises and renews a grant.
pub const PROVIDER_GRANT_SCREEN: &str = "TUI: :manager board > p (Policy) > Provider grants";

/// The screen row an operator edits to raise one provider's grant.
#[must_use]
pub fn provider_grant_raise_path(provider: SessionProvider) -> String {
    format!("{PROVIDER_GRANT_SCREEN} > {provider:?} grant budget (USD)")
}

/// The refusal for an agent-reachable write that tries to create or raise a
/// grant. It starts with [`PROVIDER_GRANT_OPERATOR_ONLY`] and names the exact
/// screen and key, so it can stand wherever a stable `&'static str` refusal
/// code is returned.
pub const PROVIDER_GRANT_OPERATOR_ONLY_MESSAGE: &str = concat!(
    "manager_v2_provider_grant_operator_only: provider spend grants (policy key provider_grants) are operator-only; ",
    "the operator sets them at TUI: :manager board > p (Policy) > Provider grants, or with ConfigureHarnessManagerPolicy"
);

/// [`PROVIDER_GRANT_OPERATOR_ONLY_MESSAGE`] as an owned string.
#[must_use]
pub fn provider_grant_operator_only_message() -> String {
    PROVIDER_GRANT_OPERATOR_ONLY_MESSAGE.to_string()
}

/// One operator spend grant for a paid provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerProviderGrantV2 {
    pub provider: SessionProvider,
    /// Total USD the grant may spend since `granted_at`. A raise keeps the
    /// spend so far.
    pub budget_usd: f64,
    /// The grant stops admitting at this time; an expired grant behaves like
    /// no grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// The efforts a launch under the grant may use.
    pub efforts: Vec<String>,
    /// Saving this grant also turns the daemon setting `bench.enabled` on, so
    /// "enable this provider's bench work" is one save.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enable_bench: bool,
    /// When spend starts counting. The daemon assigns it when the grant is
    /// first saved and keeps it across raises; a client value is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<DateTime<Utc>>,
}

/// Where a grant stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderGrantStateV1 {
    /// Budget left, not expired: launches are admitted.
    Active,
    Expired,
    /// The spend reached the budget.
    Exhausted,
}

impl ManagerProviderGrantV2 {
    /// The grant's state given what it has spent.
    #[must_use]
    pub fn state_at(&self, spent_usd: f64, now: DateTime<Utc>) -> ProviderGrantStateV1 {
        if self.expires_at.is_some_and(|expires| expires <= now) {
            ProviderGrantStateV1::Expired
        } else if spent_usd >= self.budget_usd {
            ProviderGrantStateV1::Exhausted
        } else {
            ProviderGrantStateV1::Active
        }
    }

    /// The wildcard launch choices an active grant expands to: any model of
    /// the provider at each allowed effort.
    #[must_use]
    pub fn launch_choices(&self) -> Vec<ManagerLaunchChoiceV2> {
        self.efforts
            .iter()
            .map(|effort| ManagerLaunchChoiceV2 {
                provider: self.provider,
                model: PROVIDER_GRANT_ANY_MODEL.to_string(),
                effort: Some(effort.clone()),
            })
            .collect()
    }

    /// Shape check; the operator RPC and every store save run it.
    ///
    /// # Errors
    /// `manager_v2_invalid_provider_grant`.
    pub fn validate(&self) -> Result<(), &'static str> {
        const INVALID: &str = "manager_v2_invalid_provider_grant";
        if !self.budget_usd.is_finite()
            || self.budget_usd <= 0.0
            || self.budget_usd > MANAGER_V2_MAX_PROVIDER_GRANT_USD
            || self.efforts.is_empty()
            || self.efforts.len() > MANAGER_V2_MAX_PROVIDER_GRANT_EFFORTS
            || self
                .efforts
                .iter()
                .enumerate()
                .any(|(i, effort)| self.efforts[..i].contains(effort))
        {
            return Err(INVALID);
        }
        for effort in &self.efforts {
            text(effort, 32).map_err(|_| INVALID)?;
        }
        Ok(())
    }

    /// The live status given the spend so far.
    #[must_use]
    pub fn status(&self, spent_usd: f64, now: DateTime<Utc>) -> ProviderGrantStatusV1 {
        let spent_usd = if spent_usd.is_finite() {
            spent_usd.max(0.0)
        } else {
            0.0
        };
        let state = self.state_at(spent_usd, now);
        let mut status = ProviderGrantStatusV1 {
            provider: self.provider,
            budget_usd: self.budget_usd,
            spent_usd,
            remaining_usd: (self.budget_usd - spent_usd).max(0.0),
            efforts: self.efforts.clone(),
            expires_at: self.expires_at,
            granted_at: self.granted_at,
            enable_bench: self.enable_bench,
            state,
            summary: String::new(),
        };
        status.summary = status.render();
        status
    }
}

/// Validates a policy's grants: bounded, one per provider.
///
/// # Errors
/// `manager_v2_invalid_provider_grant`.
pub fn validate_provider_grants(grants: &[ManagerProviderGrantV2]) -> Result<(), &'static str> {
    if grants.len() > MANAGER_V2_MAX_PROVIDER_GRANTS {
        return Err("manager_v2_invalid_provider_grant");
    }
    for (i, grant) in grants.iter().enumerate() {
        grant.validate()?;
        if grants[..i].iter().any(|old| old.provider == grant.provider) {
            return Err("manager_v2_invalid_provider_grant");
        }
    }
    Ok(())
}

/// One grant with its live spend, as the policy screen and the inspect
/// `resources` row show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderGrantStatusV1 {
    pub provider: SessionProvider,
    pub budget_usd: f64,
    pub spent_usd: f64,
    pub remaining_usd: f64,
    pub efforts: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub granted_at: Option<DateTime<Utc>>,
    pub enable_bench: bool,
    pub state: ProviderGrantStateV1,
    /// `OpenRouter: $12.40 of $100 used, expires 2026-12-01`.
    pub summary: String,
}

/// `$12.40`, or `$100` for a whole amount.
#[must_use]
pub fn format_grant_usd(amount: f64) -> String {
    if (amount - amount.round()).abs() < 0.005 {
        format!("${amount:.0}")
    } else {
        format!("${amount:.2}")
    }
}

impl ProviderGrantStatusV1 {
    /// The live line: `OpenRouter: $12.40 of $100 used, expires 2026-12-01`,
    /// with the state appended once it is not active.
    #[must_use]
    pub fn render(&self) -> String {
        let spent = format!("${:.2}", self.spent_usd);
        let expiry = self.expires_at.map_or_else(
            || "no expiry".to_string(),
            |at| format!("expires {}", at.format("%Y-%m-%d")),
        );
        let state = match self.state {
            ProviderGrantStateV1::Active => "",
            ProviderGrantStateV1::Expired => " · EXPIRED",
            ProviderGrantStateV1::Exhausted => " · EXHAUSTED",
        };
        format!(
            "{:?}: {spent} of {} used, {expiry}{state}",
            self.provider,
            format_grant_usd(self.budget_usd)
        )
    }

    /// Whether a launch at `effort` is inside an active grant.
    #[must_use]
    pub fn allows_effort(&self, effort: Option<&str>) -> bool {
        effort.is_some_and(|effort| self.efforts.iter().any(|allowed| allowed == effort))
    }

    /// The refusal a launch at `effort` meets under this grant, or `None`
    /// when the grant admits it. Names the grant, the spend and the fix.
    #[must_use]
    pub fn refusal_for(&self, effort: Option<&str>) -> Option<String> {
        let fix = provider_grant_raise_path(self.provider);
        match self.state {
            ProviderGrantStateV1::Expired => Some(format!(
                "{PROVIDER_GRANT_EXPIRED}: the {:?} grant expired ({}); the operator renews it at {fix}",
                self.provider, self.summary
            )),
            ProviderGrantStateV1::Exhausted => Some(format!(
                "{PROVIDER_GRANT_EXHAUSTED}: the {:?} grant is spent ({}; since {}); the operator raises it at {fix}",
                self.provider,
                self.summary,
                self.granted_at.map_or_else(
                    || "the start".to_string(),
                    |at| at.to_rfc3339_opts(SecondsFormat::Secs, true)
                ),
            )),
            ProviderGrantStateV1::Active if !self.allows_effort(effort) => Some(format!(
                "{PROVIDER_GRANT_EFFORT}: the {:?} grant ({}) allows efforts [{}] only, not {}; the operator widens them at {PROVIDER_GRANT_SCREEN} > {:?} grant efforts",
                self.provider,
                self.summary,
                self.efforts.join(", "),
                effort.unwrap_or("an unspecified effort"),
                self.provider,
            )),
            ProviderGrantStateV1::Active => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn grant() -> ManagerProviderGrantV2 {
        ManagerProviderGrantV2 {
            provider: SessionProvider::OpenRouter,
            budget_usd: 100.0,
            expires_at: Some(Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap()),
            efforts: vec!["high".into()],
            enable_bench: true,
            granted_at: Some(Utc.with_ymd_and_hms(2026, 10, 20, 0, 0, 0).unwrap()),
        }
    }

    #[test]
    fn the_summary_reads_spend_of_budget_and_expiry() {
        let now = Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap();
        assert_eq!(
            grant().status(12.4, now).summary,
            "OpenRouter: $12.40 of $100 used, expires 2026-12-01"
        );
        let mut open = grant();
        open.expires_at = None;
        assert_eq!(
            open.status(0.0, now).summary,
            "OpenRouter: $0.00 of $100 used, no expiry"
        );
    }

    #[test]
    fn an_active_grant_expands_to_one_wildcard_choice_per_effort() {
        let mut g = grant();
        g.efforts = vec!["high".into(), "medium".into()];
        let choices = g.launch_choices();
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|c| c.model == PROVIDER_GRANT_ANY_MODEL));
        let launch = |model: &str, effort: &str| ManagerLaunchChoiceV2 {
            provider: SessionProvider::OpenRouter,
            model: model.into(),
            effort: Some(effort.into()),
        };
        assert!(
            choices
                .iter()
                .any(|c| c.allows(&launch("z-ai/glm-5.3-flash", "high")))
        );
        assert!(
            choices
                .iter()
                .all(|c| !c.allows(&launch("any/model", "low")))
        );
        let other = ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            ..launch("m", "high")
        };
        assert!(choices.iter().all(|c| !c.allows(&other)));
    }

    #[test]
    fn spent_and_expired_grants_stop_admitting_and_name_the_fix() {
        let now = Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap();
        assert_eq!(grant().state_at(99.99, now), ProviderGrantStateV1::Active);
        assert_eq!(
            grant().state_at(100.0, now),
            ProviderGrantStateV1::Exhausted
        );
        let later = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        assert_eq!(grant().state_at(1.0, later), ProviderGrantStateV1::Expired);

        let spent = grant().status(100.0, now);
        let refusal = spent.refusal_for(Some("high")).unwrap();
        assert!(refusal.starts_with(PROVIDER_GRANT_EXHAUSTED), "{refusal}");
        assert!(refusal.contains("$100.00 of $100 used"), "{refusal}");
        assert!(
            refusal.contains("OpenRouter grant budget (USD)"),
            "{refusal}"
        );

        let wrong = grant().status(1.0, now).refusal_for(Some("low")).unwrap();
        assert!(wrong.starts_with(PROVIDER_GRANT_EFFORT), "{wrong}");
        assert!(wrong.contains("[high]"), "{wrong}");
        assert_eq!(grant().status(1.0, now).refusal_for(Some("high")), None);
        let expired = grant().status(1.0, later);
        assert!(
            expired
                .refusal_for(Some("high"))
                .unwrap()
                .starts_with(PROVIDER_GRANT_EXPIRED)
        );
    }

    #[test]
    fn grants_validate_budget_efforts_and_uniqueness() {
        assert_eq!(validate_provider_grants(&[grant()]), Ok(()));
        let mut bad = grant();
        bad.budget_usd = 0.0;
        assert!(validate_provider_grants(&[bad]).is_err());
        let mut bad = grant();
        bad.efforts.clear();
        assert!(validate_provider_grants(&[bad]).is_err());
        let mut bad = grant();
        bad.efforts = vec!["high".into(), "high".into()];
        assert!(validate_provider_grants(&[bad]).is_err());
        assert!(validate_provider_grants(&[grant(), grant()]).is_err());
    }

    #[test]
    fn an_operator_only_refusal_names_the_screen_and_the_key() {
        let message = provider_grant_operator_only_message();
        assert!(message.starts_with(PROVIDER_GRANT_OPERATOR_ONLY));
        assert!(message.contains("provider_grants"));
        assert!(message.contains(PROVIDER_GRANT_SCREEN.trim_start_matches("TUI: ")));
    }
    #[test]
    fn a_provider_wide_entry_exists_only_as_a_grant_expansion() {
        use crate::harness_manager_v2::ManagerPolicyV2;
        let wide = grant().launch_choices().remove(0);
        assert!(wide.is_provider_wide());
        assert!(wide.validate().is_err(), "an operator list cannot carry it");
        let policy = ManagerPolicyV2 {
            allowed_launches: vec![wide],
            ..ManagerPolicyV2::default()
        };
        assert!(policy.validate().is_err());
        let granted = ManagerPolicyV2 {
            provider_grants: vec![grant()],
            ..ManagerPolicyV2::default()
        };
        assert_eq!(granted.validate(), Ok(()));
    }

    #[test]
    fn a_policy_saved_before_grants_reads_with_none() {
        use crate::harness_manager_v2::ManagerPolicyV2;
        let old: ManagerPolicyV2 = serde_json::from_str("{}").unwrap();
        assert!(old.provider_grants.is_empty());
        let json = serde_json::to_value(&old).unwrap();
        assert_eq!(json["provider_grants"], serde_json::json!([]));
    }

    #[test]
    fn an_ancestor_exact_list_still_narrows_a_provider_wide_entry() {
        let wide = grant().launch_choices().remove(0);
        let exact = ManagerLaunchChoiceV2 {
            provider: SessionProvider::OpenRouter,
            model: "deepseek/deepseek-v4.1-flash".into(),
            effort: Some("high".into()),
        };
        assert_eq!(
            crate::grant_narrowing::intersect_launches(
                std::slice::from_ref(&wide),
                std::slice::from_ref(&exact)
            ),
            vec![exact]
        );
    }

    #[test]
    fn a_policy_handed_down_never_carries_a_grant() {
        use crate::harness_manager_v2::ManagerPolicyV2;
        let handed = ManagerPolicyV2 {
            provider_grants: vec![grant()],
            ..ManagerPolicyV2::default()
        };
        assert_eq!(
            crate::grant_narrowing::handed_policy_narrows(
                &handed,
                &[],
                &ManagerPolicyV2::default()
            ),
            Err(PROVIDER_GRANT_OPERATOR_ONLY_MESSAGE)
        );
    }
}
