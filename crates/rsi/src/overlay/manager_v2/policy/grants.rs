//! The provider spend grant rows of the Policy screen (#3417): one
//! operator-only grant per paid provider (budget, expiry, efforts and the
//! `bench.enabled` tick), with its live spend beside it.

use super::{Field, PROVIDERS, PolicyState, usage::UsageView};
use chrono::{DateTime, NaiveDate, Utc};
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::provider_grant::{
    MANAGER_V2_MAX_PROVIDER_GRANT_EFFORTS, MANAGER_V2_MAX_PROVIDER_GRANT_USD,
    MANAGER_V2_MAX_PROVIDER_GRANTS, ManagerProviderGrantV2, format_grant_usd,
};
use rsi_common::types::SessionProvider;

/// The efforts a new grant starts with.
const NEW_GRANT_EFFORTS: [&str; 1] = ["high"];

/// The grant of `PROVIDERS[i]` in a policy.
pub(super) fn grant_of(p: &ManagerPolicyV2, i: usize) -> Option<&ManagerProviderGrantV2> {
    p.provider_grants
        .iter()
        .find(|g| g.provider == PROVIDERS[i])
}

/// Whether the field's grant value differs between two policies. The
/// daemon-assigned `granted_at` is not an operator edit.
pub(super) fn grant_field_modified(d: &ManagerPolicyV2, o: &ManagerPolicyV2, field: Field) -> bool {
    match field {
        Field::GrantBudget(i) => {
            grant_of(d, i).map(|g| g.budget_usd.to_bits())
                != grant_of(o, i).map(|g| g.budget_usd.to_bits())
        }
        Field::GrantEfforts(i) => {
            grant_of(d, i).map(|g| &g.efforts) != grant_of(o, i).map(|g| &g.efforts)
        }
        Field::GrantExpiry(i) => {
            grant_of(d, i).map(|g| g.expires_at) != grant_of(o, i).map(|g| g.expires_at)
        }
        Field::GrantBench(i) => {
            grant_of(d, i).map(|g| g.enable_bench) != grant_of(o, i).map(|g| g.enable_bench)
        }
        _ => false,
    }
}

/// The display (and saved-marker) value of one grant row.
pub(super) fn grant_field_value(p: &ManagerPolicyV2, field: Field) -> Option<String> {
    Some(match field {
        Field::GrantBudget(i) => grant_of(p, i).map_or_else(
            || "none".into(),
            |g| {
                let text = format_grant_usd(g.budget_usd);
                text.trim_start_matches('$').to_string()
            },
        ),
        Field::GrantEfforts(i) => grant_of(p, i).map_or_else(String::new, |g| g.efforts.join(", ")),
        Field::GrantExpiry(i) => grant_of(p, i)
            .and_then(|g| g.expires_at)
            .map_or_else(|| "none".into(), expiry_text),
        Field::GrantBench(i) => grant_of(p, i).is_some_and(|g| g.enable_bench).to_string(),
        _ => return None,
    })
}

fn expiry_text(at: DateTime<Utc>) -> String {
    if at.time() == chrono::NaiveTime::MIN {
        at.format("%Y-%m-%d").to_string()
    } else {
        at.format("%Y-%m-%d %H:%MZ").to_string()
    }
}

/// `2026-12-01` (midnight UTC) or an RFC 3339 time.
fn parse_expiry(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|t| t.and_utc());
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

impl PolicyState {
    /// Provider grant rows are listed when the section is expanded, or when
    /// this provider has a grant (or an edit on it).
    pub(super) fn grant_listed(&self, i: usize) -> bool {
        self.provider_grants_open
            || grant_of(&self.draft, i).is_some()
            || grant_of(&self.opened_draft, i).is_some()
    }

    /// The live spend line of one provider's grant, or why there is none yet.
    pub(super) fn grant_status_text(&self, i: usize) -> String {
        let provider = PROVIDERS[i];
        let Some(grant) = grant_of(&self.draft, i) else {
            return "no grant · launches need an exact allowed-model tuple".into();
        };
        let unsaved = (grant_of(&self.opened_draft, i).is_none())
            .then_some("not saved yet · spend counts from the first save");
        if let Some(unsaved) = unsaved {
            return format!(
                "{provider:?}: {} budget · {unsaved}",
                format_grant_usd(grant.budget_usd)
            );
        }
        match &self.usage {
            UsageView::Loaded(usage) => usage
                .grants
                .iter()
                .find(|status| status.provider == provider)
                .map_or_else(
                    || format!("{provider:?}: spend unavailable · 5 refreshes"),
                    |status| status.summary.clone(),
                ),
            _ => format!(
                "{provider:?}: {} budget · spend loading · 5 refreshes",
                format_grant_usd(grant.budget_usd)
            ),
        }
    }

    /// The daemon setting `bench.enabled`, as the usage read last saw it.
    pub(super) fn bench_setting_text(&self) -> String {
        match &self.usage {
            UsageView::Loaded(usage) => match usage.bench_enabled {
                Some(true) => "on".into(),
                Some(false) => "off · a grant with bench ticked turns it on at save".into(),
                None => "unknown".into(),
            },
            _ => "loading · 5 refreshes".into(),
        }
    }

    /// The expander row's value: each grant and how to expand.
    pub(super) fn grants_summary(&self) -> String {
        let set = self
            .draft
            .provider_grants
            .iter()
            .map(|g| {
                format!(
                    "{:?} {} ({})",
                    g.provider,
                    format_grant_usd(g.budget_usd),
                    g.efforts.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        match (set.is_empty(), self.provider_grants_open) {
            (true, false) => "none set · Enter to set one".into(),
            (true, true) => "none set · Enter collapse".into(),
            (false, false) => format!("{set} · Enter to show every provider"),
            (false, true) => format!("{set} · Enter collapse"),
        }
    }

    /// Client-side mirror of the daemon's grant validation for one row.
    pub(super) fn grant_field_error(&self, field: Field) -> Option<String> {
        match field {
            Field::GrantBudget(i) => grant_of(&self.draft, i)
                .filter(|g| {
                    !g.budget_usd.is_finite()
                        || g.budget_usd <= 0.0
                        || g.budget_usd > MANAGER_V2_MAX_PROVIDER_GRANT_USD
                })
                .map(|_| {
                    format!("must be above 0 and at most {MANAGER_V2_MAX_PROVIDER_GRANT_USD}")
                }),
            Field::GrantEfforts(i) => grant_of(&self.draft, i)
                .filter(|g| g.validate().is_err())
                .map(|_| "needs 1-8 distinct efforts".into()),
            _ => None,
        }
    }

    /// Enter on a grant toggle (the tick for `bench.enabled`).
    pub(super) fn toggle_grant_bench(&mut self, i: usize) {
        if let Some(grant) = self
            .draft
            .provider_grants
            .iter_mut()
            .find(|g| g.provider == PROVIDERS[i])
        {
            grant.enable_bench = !grant.enable_bench;
        } else {
            self.error = Some("Set a grant budget first.".into());
        }
    }

    /// Text entry on a grant row.
    pub(super) fn apply_grant_text(&mut self, field: Field, value: &str) -> Result<(), String> {
        match field {
            Field::GrantBudget(i) => self.set_grant_budget(i, value),
            Field::GrantEfforts(i) => {
                let efforts = value
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let grant = self.grant_mut(i)?;
                grant.efforts = efforts;
                grant.validate().map_err(|_| {
                    format!(
                        "Enter 1-{MANAGER_V2_MAX_PROVIDER_GRANT_EFFORTS} distinct efforts such as high, medium."
                    )
                })
            }
            Field::GrantExpiry(i) => {
                let expires_at = if value.is_empty() || value.eq_ignore_ascii_case("none") {
                    None
                } else {
                    Some(parse_expiry(value).ok_or(
                        "Enter a date such as 2026-12-01 (UTC), an RFC 3339 time, or blank for no expiry.",
                    )?)
                };
                self.grant_mut(i)?.expires_at = expires_at;
                Ok(())
            }
            _ => Err("This field is a toggle.".into()),
        }
    }

    fn grant_mut(&mut self, i: usize) -> Result<&mut ManagerProviderGrantV2, String> {
        self.draft
            .provider_grants
            .iter_mut()
            .find(|g| g.provider == PROVIDERS[i])
            .ok_or_else(|| "Set the grant budget first.".to_string())
    }

    fn set_grant_budget(&mut self, i: usize, value: &str) -> Result<(), String> {
        let provider = PROVIDERS[i];
        if value.is_empty() || value.eq_ignore_ascii_case("none") {
            self.draft
                .provider_grants
                .retain(|g| g.provider != provider);
            return Ok(());
        }
        let budget = value
            .trim_start_matches('$')
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= MANAGER_V2_MAX_PROVIDER_GRANT_USD)
            .ok_or("Enter a USD budget above 0; blank removes the grant.")?;
        if let Some(grant) = self
            .draft
            .provider_grants
            .iter_mut()
            .find(|g| g.provider == provider)
        {
            grant.budget_usd = budget;
            return Ok(());
        }
        if self.draft.provider_grants.len() >= MANAGER_V2_MAX_PROVIDER_GRANTS {
            return Err(format!(
                "At most {MANAGER_V2_MAX_PROVIDER_GRANTS} provider grants; blank another first."
            ));
        }
        self.draft.provider_grants.push(ManagerProviderGrantV2 {
            provider,
            budget_usd: budget,
            expires_at: None,
            efforts: NEW_GRANT_EFFORTS.map(str::to_string).to_vec(),
            // OpenRouter is the provider bench runs bill: its grant turns
            // bench.enabled on in the same save unless unticked.
            enable_bench: provider == SessionProvider::OpenRouter,
            granted_at: None,
        });
        Ok(())
    }
}
