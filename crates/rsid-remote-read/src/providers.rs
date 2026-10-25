//! The RSI Remote v2 providers read (hub #3796): each provider's plan windows,
//! overage posture and 1h/24h spend for the phone app's usage dashboard. Labels
//! and numbers only.
//!
//! One bounded statement reads `provider_rate_limit_windows` (a table keyed by
//! `(provider, window_key)`, read through a hard row cap). Spend is not read
//! here: the cost-increment rule lives in `rsid-store` and a spend aggregate
//! does not fit the Remote read budget, so the daemon owner supplies it as a
//! value (`ProviderSpendView`) taken from its own cache. Every string that
//! reaches the wire passes the label allowlist, every number is finite, and
//! the response is built field by field from typed inputs, so no column that is
//! not named here can reach the phone.

use super::Result;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use rsi_common::remote_manager_v2::{
    PROVIDER_LABEL_BYTES, PROVIDER_MODELS_MAX, PROVIDER_WINDOWS_MAX, PROVIDERS_MAX,
    PROVIDERS_WINDOW_SOURCE_ROWS, ProviderModelSpendV2, ProviderRowV2, ProviderSpendV2,
    ProviderWindowV2, ProvidersResponseV2,
};
use rsi_common::types::SessionProvider;
use rusqlite::Connection;
use std::collections::BTreeMap;

/// The rate-limit read: only rows newer than the operator's invalidation mark
/// for the provider (the same rule the daemon applies to `GetProviderRateLimits`).
pub const PROVIDER_WINDOWS_SQL: &str =
    "SELECT provider, window_key, utilization, resets_at_epoch, status, rate_limit_type,
            overage_status, is_using_overage, observed_at
     FROM provider_rate_limit_windows
     WHERE observed_at > COALESCE((SELECT value FROM daemon_settings
         WHERE key = 'provider_rate_limits.invalidated_at.' || provider), '')
     LIMIT ?1";

#[derive(Debug, Clone)]
struct WindowRow {
    provider: String,
    window_key: String,
    utilization: f64,
    resets_at_epoch: Option<i64>,
    status: Option<String>,
    rate_limit_type: Option<String>,
    overage_status: Option<String>,
    is_using_overage: bool,
    observed_at: DateTime<Utc>,
}

/// What the bounded Store read returned.
#[derive(Debug, Clone)]
pub struct ProvidersSource {
    rows: Vec<WindowRow>,
    incomplete: bool,
}

/// Read the provider rate-limit rows under a hard row cap.
///
/// # Errors
///
/// Returns the SQL error of a failed or interrupted statement.
pub fn providers_source(conn: &Connection) -> Result<ProvidersSource> {
    let limit = i64::try_from(PROVIDERS_WINDOW_SOURCE_ROWS + 1).unwrap_or(i64::MAX);
    let mut statement = conn.prepare_cached(PROVIDER_WINDOWS_SQL)?;
    let mut rows = statement.query([limit])?;
    let mut source = ProvidersSource {
        rows: Vec::new(),
        incomplete: false,
    };
    while let Some(row) = rows.next()? {
        if source.rows.len() == PROVIDERS_WINDOW_SOURCE_ROWS {
            source.incomplete = true;
            break;
        }
        let observed_at: String = row.get(8)?;
        // An unreadable row is skipped like the daemon's own rate-limit read:
        // this is advisory telemetry and one stale row must not fail the call.
        let Ok(observed_at) = DateTime::parse_from_rfc3339(&observed_at) else {
            continue;
        };
        let is_using_overage: i64 = row.get(7)?;
        source.rows.push(WindowRow {
            provider: row.get(0)?,
            window_key: row.get(1)?,
            utilization: row.get(2)?,
            resets_at_epoch: row.get(3)?,
            status: row.get(4)?,
            rate_limit_type: row.get(5)?,
            overage_status: row.get(6)?,
            is_using_overage: is_using_overage != 0,
            observed_at: observed_at.with_timezone(&Utc),
        });
    }
    Ok(source)
}

/// Spend of one `(provider, model)` pair, as the daemon owner measured it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSpendRow {
    pub provider: String,
    pub model: String,
    pub cost_1h_usd: f64,
    pub cost_24h_usd: f64,
}

/// The spend the owner has to offer: rows, or why it has none.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderSpendView {
    /// `reason` is one of `warming`, `stale`, `unavailable`.
    Unavailable(&'static str),
    /// `truncated`: the usage read stopped at its row cap, so the sums are
    /// lower bounds and are withheld.
    Available {
        rows: Vec<ProviderSpendRow>,
        truncated: bool,
    },
}

/// A label that may reach the phone: non-empty, at most
/// [`PROVIDER_LABEL_BYTES`], and only ASCII letters, digits and `_ - . : /`
/// (provider verdicts, window keys, model ids and reason codes). Anything else
/// (a URL with a query, an e-mail, free text, a path with spaces) is dropped.
fn label(text: &str) -> Option<String> {
    let ok = !text.is_empty()
        && text.len() <= PROVIDER_LABEL_BYTES
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/'))
        && !text.contains("..")
        && !text.starts_with('/')
        && !text.contains("//");
    ok.then(|| text.to_owned())
}

fn optional_label(text: Option<&str>) -> Option<String> {
    text.and_then(label)
}

/// USD to four decimals; non-finite becomes zero and a negative figure (an
/// increment that fell below its predecessor) is floored at zero.
fn usd(value: f64) -> f64 {
    if value.is_finite() {
        (value.max(0.0) * 10_000.0).round() / 10_000.0
    } else {
        0.0
    }
}

fn window(row: &WindowRow) -> Option<ProviderWindowV2> {
    let key = label(&row.window_key)?;
    if !row.utilization.is_finite() {
        return None;
    }
    let resets_at = row
        .resets_at_epoch
        .and_then(|epoch| Utc.timestamp_opt(epoch, 0).single());
    Some(ProviderWindowV2 {
        key,
        utilization: row.utilization.clamp(0.0, 1.0),
        resets_at: resets_at.map(|at| at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        resets_at_epoch: resets_at.map(|at| at.timestamp()),
    })
}

fn provider_row(
    name: &str,
    rows: &[&WindowRow],
    spend: &ProviderSpendView,
    stale: &impl Fn(&str) -> Option<String>,
) -> ProviderRowV2 {
    let newest = rows.iter().copied().max_by_key(|row| row.observed_at);
    let mut windows: Vec<_> = rows.iter().filter_map(|row| window(row)).collect();
    windows.sort_by(|a, b| a.key.cmp(&b.key));
    windows.dedup_by(|a, b| a.key == b.key);
    let windows_truncated = windows.len() > PROVIDER_WINDOWS_MAX;
    windows.truncate(PROVIDER_WINDOWS_MAX);
    let windows_reason = windows.is_empty().then(|| "no_observation".to_owned());

    let (spend_block, spend_reason, models, models_truncated) = match spend {
        ProviderSpendView::Unavailable(reason) => (None, Some((*reason).to_owned()), vec![], false),
        ProviderSpendView::Available {
            truncated: true, ..
        } => (None, Some("truncated".to_owned()), vec![], false),
        ProviderSpendView::Available { rows, .. } => {
            let mine: Vec<_> = rows.iter().filter(|row| row.provider == name).collect();
            let total = ProviderSpendV2 {
                cost_1h_usd: usd(mine.iter().map(|row| usd(row.cost_1h_usd)).sum()),
                cost_24h_usd: usd(mine.iter().map(|row| usd(row.cost_24h_usd)).sum()),
            };
            let mut by_model: BTreeMap<String, (f64, f64)> = BTreeMap::new();
            for row in mine {
                let entry = by_model
                    .entry(label(&row.model).unwrap_or_else(|| "other".to_owned()))
                    .or_default();
                entry.0 += usd(row.cost_1h_usd);
                entry.1 += usd(row.cost_24h_usd);
            }
            let mut models: Vec<_> = by_model
                .into_iter()
                .map(|(model, (h1, h24))| ProviderModelSpendV2 {
                    model,
                    cost_1h_usd: usd(h1),
                    cost_24h_usd: usd(h24),
                })
                .collect();
            models.sort_by(|a, b| {
                b.cost_24h_usd
                    .total_cmp(&a.cost_24h_usd)
                    .then_with(|| a.model.cmp(&b.model))
            });
            let truncated = models.len() > PROVIDER_MODELS_MAX;
            models.truncate(PROVIDER_MODELS_MAX);
            (Some(total), None, models, truncated)
        }
    };

    ProviderRowV2 {
        provider: name.to_owned(),
        status: optional_label(newest.and_then(|row| row.status.as_deref())),
        binding_window: optional_label(newest.and_then(|row| row.rate_limit_type.as_deref())),
        overage_status: optional_label(newest.and_then(|row| row.overage_status.as_deref())),
        is_using_overage: newest.is_some_and(|row| row.is_using_overage),
        observed_at: newest.map(|row| row.observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        stale_reason: stale(name).as_deref().and_then(label),
        windows,
        windows_reason,
        windows_truncated,
        spend: spend_block,
        spend_reason,
        models,
        models_truncated,
    }
}

/// Assemble the wire response: one row per canonical provider, in canonical
/// order, each with whatever data exists and an explicit reason where none
/// does. `stale` returns the latest-refresh-failed code for a provider.
#[must_use]
pub fn providers_response(
    source: &ProvidersSource,
    spend: &ProviderSpendView,
    stale: impl Fn(&str) -> Option<String>,
    observed_at: DateTime<Utc>,
) -> ProvidersResponseV2 {
    let names = SessionProvider::CANONICAL_NAMES;
    let providers = names
        .iter()
        .take(PROVIDERS_MAX)
        .map(|name| {
            let rows: Vec<_> = source
                .rows
                .iter()
                .filter(|row| row.provider == *name)
                .collect();
            provider_row(name, &rows, spend, &stale)
        })
        .collect();
    ProvidersResponseV2 {
        observed_at: observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        providers,
        providers_truncated: names.len() > PROVIDERS_MAX,
        windows_incomplete: source.incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::collections::BTreeSet;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE provider_rate_limit_windows(provider TEXT NOT NULL,window_key TEXT NOT NULL,
               utilization REAL NOT NULL,resets_at_epoch INTEGER,status TEXT,rate_limit_type TEXT,
               overage_status TEXT,is_using_overage INTEGER NOT NULL DEFAULT 0,observed_at TEXT NOT NULL,
               observed_session_id TEXT,account_hint TEXT,PRIMARY KEY(provider,window_key));
             CREATE TABLE daemon_settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn put(
        conn: &Connection,
        provider: &str,
        key: &str,
        utilization: f64,
        reset: Option<i64>,
        status: &str,
        overage: &str,
        observed_at: &str,
    ) {
        conn.execute(
            "INSERT INTO provider_rate_limit_windows
               (provider,window_key,utilization,resets_at_epoch,status,rate_limit_type,overage_status,
                is_using_overage,observed_at,observed_session_id,account_hint)
             VALUES(?1,?2,?3,?4,?5,'five_hour',?6,1,?7,'11111111-2222-3333-4444-555555555555',
                    'operator@example.test')",
            params![provider, key, utilization, reset, status, overage, observed_at],
        )
        .unwrap();
    }

    const NOW: &str = "2026-10-22T12:00:00.000000000Z";

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn row(spend_rows: &[(&str, &str, f64, f64)]) -> ProviderSpendView {
        ProviderSpendView::Available {
            rows: spend_rows
                .iter()
                .map(|(provider, model, h1, h24)| ProviderSpendRow {
                    provider: (*provider).into(),
                    model: (*model).into(),
                    cost_1h_usd: *h1,
                    cost_24h_usd: *h24,
                })
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn every_provider_appears_and_one_without_data_says_why() {
        let conn = fixture();
        put(
            &conn,
            "Claude",
            "seven_day",
            0.25,
            Some(1_792_000_000),
            "allowed",
            "rejected",
            NOW,
        );
        put(
            &conn,
            "Claude",
            "five_hour",
            0.5,
            Some(1_791_990_000),
            "allowed",
            "rejected",
            NOW,
        );
        let source = providers_source(&conn).unwrap();
        let spend = row(&[
            ("Claude", "claude-opus", 1.25, 9.5),
            ("Codex", "gpt", 0.5, 2.0),
        ]);
        let response = providers_response(&source, &spend, |_| None, at(NOW));

        let names: Vec<_> = response
            .providers
            .iter()
            .map(|p| p.provider.as_str())
            .collect();
        assert_eq!(names, SessionProvider::CANONICAL_NAMES);
        assert!(!response.providers_truncated);
        assert!(!response.windows_incomplete);

        let claude = &response.providers[0];
        assert_eq!(claude.status.as_deref(), Some("allowed"));
        assert_eq!(claude.binding_window.as_deref(), Some("five_hour"));
        assert_eq!(claude.overage_status.as_deref(), Some("rejected"));
        assert!(claude.is_using_overage);
        assert_eq!(claude.observed_at.as_deref(), Some(NOW));
        let keys: Vec<_> = claude.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["five_hour", "seven_day"]);
        assert_eq!(claude.windows[0].utilization, 0.5);
        assert_eq!(claude.windows[0].resets_at_epoch, Some(1_791_990_000));
        assert_eq!(
            claude.windows[0].resets_at.as_deref(),
            Some(
                Utc.timestamp_opt(1_791_990_000, 0)
                    .unwrap()
                    .to_rfc3339_opts(SecondsFormat::Nanos, true)
                    .as_str()
            )
        );
        assert!(claude.windows_reason.is_none());
        assert_eq!(
            claude.spend,
            Some(ProviderSpendV2 {
                cost_1h_usd: 1.25,
                cost_24h_usd: 9.5
            })
        );
        assert_eq!(claude.models[0].model, "claude-opus");

        // Pioneer has neither windows nor spend rows: explicit null + reason.
        let pioneer = response
            .providers
            .iter()
            .find(|p| p.provider == "Pioneer")
            .unwrap();
        assert!(pioneer.windows.is_empty());
        assert_eq!(pioneer.windows_reason.as_deref(), Some("no_observation"));
        assert!(pioneer.status.is_none() && pioneer.observed_at.is_none());
        assert_eq!(
            pioneer.spend,
            Some(ProviderSpendV2 {
                cost_1h_usd: 0.0,
                cost_24h_usd: 0.0
            })
        );
    }

    #[test]
    fn unavailable_or_truncated_spend_is_null_with_a_reason() {
        let source = providers_source(&fixture()).unwrap();
        for (view, reason) in [
            (ProviderSpendView::Unavailable("warming"), "warming"),
            (ProviderSpendView::Unavailable("stale"), "stale"),
            (
                ProviderSpendView::Available {
                    rows: vec![],
                    truncated: true,
                },
                "truncated",
            ),
        ] {
            let response = providers_response(&source, &view, |_| None, at(NOW));
            for provider in &response.providers {
                assert!(provider.spend.is_none());
                assert_eq!(provider.spend_reason.as_deref(), Some(reason));
                assert!(provider.models.is_empty());
            }
        }
    }

    #[test]
    fn invalidated_rows_are_not_served() {
        let conn = fixture();
        put(
            &conn,
            "Claude",
            "five_hour",
            0.5,
            None,
            "allowed",
            "rejected",
            "2026-10-22T10:00:00.000000000Z",
        );
        conn.execute(
            "INSERT INTO daemon_settings(key,value) VALUES('provider_rate_limits.invalidated_at.Claude',?1)",
            [NOW],
        )
        .unwrap();
        let response = providers_response(
            &providers_source(&conn).unwrap(),
            &ProviderSpendView::Unavailable("warming"),
            |_| None,
            at(NOW),
        );
        assert!(response.providers[0].windows.is_empty());
        assert_eq!(
            response.providers[0].windows_reason.as_deref(),
            Some("no_observation")
        );
    }

    #[test]
    fn bad_numbers_and_unsafe_labels_never_reach_the_wire() {
        let conn = fixture();
        put(
            &conn,
            "Claude",
            "five_hour",
            7.0,
            Some(1_791_990_000),
            "see https://x.test/?k=1",
            "allowed",
            "2026-10-22T12:00:01.000000000Z", // the newest row decides the snapshot fields
        );
        put(
            &conn,
            "Claude",
            "weekly opus",
            0.1,
            None,
            "allowed",
            "ok",
            NOW,
        );
        put(
            &conn,
            "NotAProvider",
            "five_hour",
            0.1,
            None,
            "allowed",
            "ok",
            NOW,
        );
        let source = providers_source(&conn).unwrap();
        let spend = row(&[
            ("Claude", "/home/operator/model", f64::INFINITY, -3.0),
            ("Claude", "ok-model", 0.00004, 1.00004),
        ]);
        let response = providers_response(
            &source,
            &spend,
            |name| (name == "Claude").then(|| "probe_timeout".to_owned()),
            at(NOW),
        );
        let claude = &response.providers[0];
        // Out-of-range utilization is clamped; an unsafe verdict is dropped;
        // an unsafe key drops the window.
        assert_eq!(claude.windows.len(), 1);
        assert_eq!(claude.windows[0].utilization, 1.0);
        assert!(claude.status.is_none());
        assert_eq!(claude.stale_reason.as_deref(), Some("probe_timeout"));
        let models: Vec<_> = claude.models.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(models, ["ok-model", "other"]);
        assert_eq!(claude.spend.as_ref().unwrap().cost_24h_usd, 1.0);
        let text = serde_json::to_string(&response).unwrap();
        assert!(!text.contains("NaN"));
        assert!(!text.contains("NotAProvider"));
    }

    #[test]
    fn models_and_windows_are_bounded() {
        let conn = fixture();
        for n in 0..12 {
            put(
                &conn,
                "Claude",
                &format!("w{n:02}"),
                0.1,
                None,
                "allowed",
                "ok",
                NOW,
            );
        }
        let spend_rows: Vec<_> = (0..12)
            .map(|n| ProviderSpendRow {
                provider: "Claude".into(),
                model: format!("m{n:02}"),
                cost_1h_usd: 0.0,
                cost_24h_usd: f64::from(n),
            })
            .collect();
        let spend = ProviderSpendView::Available {
            rows: spend_rows,
            truncated: false,
        };
        let response =
            providers_response(&providers_source(&conn).unwrap(), &spend, |_| None, at(NOW));
        let claude = &response.providers[0];
        assert_eq!(claude.windows.len(), PROVIDER_WINDOWS_MAX);
        assert!(claude.windows_truncated);
        assert_eq!(claude.models.len(), PROVIDER_MODELS_MAX);
        assert!(claude.models_truncated);
        assert_eq!(claude.models[0].model, "m11");
    }

    #[test]
    fn source_stops_at_the_row_cap() {
        let conn = fixture();
        for n in 0..=PROVIDERS_WINDOW_SOURCE_ROWS {
            put(
                &conn,
                "Claude",
                &format!("w{n:04}"),
                0.1,
                None,
                "allowed",
                "ok",
                NOW,
            );
        }
        let source = providers_source(&conn).unwrap();
        assert!(source.incomplete);
        assert_eq!(source.rows.len(), PROVIDERS_WINDOW_SOURCE_ROWS);
        let response = providers_response(
            &source,
            &ProviderSpendView::Unavailable("warming"),
            |_| None,
            at(NOW),
        );
        assert!(response.windows_incomplete);
    }

    /// Positive allowlist (AGENTS.md rule 7): the serialized row carries exactly
    /// these fields and the secret-bearing columns the table holds never appear.
    #[test]
    fn serialized_rows_carry_only_the_allowed_fields_and_no_secrets() {
        let conn = fixture();
        put(
            &conn,
            "Claude",
            "five_hour",
            0.5,
            Some(1_791_990_000),
            "allowed",
            "rejected",
            NOW,
        );
        let spend = row(&[("Claude", "claude-opus", 1.0, 2.0)]);
        let response =
            providers_response(&providers_source(&conn).unwrap(), &spend, |_| None, at(NOW));
        let value = serde_json::to_value(&response).unwrap();
        let keys = |v: &serde_json::Value| -> BTreeSet<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };
        let expect = |names: &[&str]| -> BTreeSet<String> {
            names.iter().map(|s| (*s).to_owned()).collect()
        };
        assert_eq!(
            keys(&value),
            expect(&[
                "observed_at",
                "providers",
                "providers_truncated",
                "windows_incomplete"
            ])
        );
        let claude = &value["providers"][0];
        assert_eq!(
            keys(claude),
            expect(&[
                "provider",
                "status",
                "binding_window",
                "overage_status",
                "is_using_overage",
                "observed_at",
                "stale_reason",
                "windows",
                "windows_reason",
                "windows_truncated",
                "spend",
                "spend_reason",
                "models",
                "models_truncated",
            ])
        );
        assert_eq!(
            keys(&claude["windows"][0]),
            expect(&["key", "utilization", "resets_at", "resets_at_epoch"])
        );
        assert_eq!(
            keys(&claude["spend"]),
            expect(&["cost_1h_usd", "cost_24h_usd"])
        );
        assert_eq!(
            keys(&claude["models"][0]),
            expect(&["model", "cost_1h_usd", "cost_24h_usd"])
        );
        let text = value.to_string();
        for secret in [
            "operator@",
            "example.test",
            "11111111-2222",
            "observed_session",
        ] {
            assert!(!text.contains(secret), "providers leak {secret}: {text}");
        }
    }
}
