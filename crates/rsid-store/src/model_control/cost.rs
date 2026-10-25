//! Per-invocation cost resolution (#584, #585).
//!
//! One rule decides `model_invocations.estimated_cost_usd` for every provider
//! path that settles through [`crate::model_control::call_control`]:
//!
//! 1. a **provider-reported** cost (for example OpenRouter `usage.cost`) is
//!    used verbatim;
//! 2. a loopback `Local` model costs a real `0.0`;
//! 3. otherwise the cost is **estimated** from token usage with a price from
//!    [`price_for_model`] (documented list prices only);
//! 4. a model with no known price stays `None` (NULL). A price is never
//!    invented.
//!
//! The session total (`sessions.cost_usd`) for these providers is the sum of
//! the session's settled invocation costs, so both columns always agree.
//!
//! # Session-cumulative rows (#2593, #3534)
//!
//! A `session.launch.fresh` / `session.continue.resume` / `session.rotate.child`
//! row (the last is the first row of a rotation successor, and its resumes
//! continue the same running total; leaving it out charged that first figure
//! twice, #3534) is settled from the
//! **session's running totals** (`session_invocation_completion`), so its
//! `estimated_cost_usd` (and its token and wall-time columns) is the session's
//! CUMULATIVE figure at that moment, not that invocation's own cost. The rows
//! of one session rise monotonically and the last one equals
//! `sessions.cost_usd`; summing them overstates spend by an order of magnitude.
//! Every other purpose (a `model.call`, a manager answer, a background call)
//! settles its own cost. The stored column keeps this meaning (released rows
//! cannot be rewritten); a consumer that adds costs up reads the increment
//! tables below instead of the raw column, restricted to the sessions it needs
//! (`cost_increment_for_session_sql!`, `cost_increment_for_ids_sql!`, #2939,
//! #330).

use crate::model_control::call_control::ModelCallUsage;

/// Purposes whose settled figures are the session's running totals (module
/// docs). The single list behind [`COST_INCREMENT_SQL`].
#[cfg(test)]
pub(crate) const SESSION_CUMULATIVE_PURPOSES: [&str; 3] = [
    "session.launch.fresh",
    "session.continue.resume",
    "session.rotate.child",
];

/// A derived table `(invocation_id, cost_increment_usd)` with one row per
/// `model_invocations` row: that invocation's OWN cost.
///
/// * a purpose outside [`SESSION_CUMULATIVE_PURPOSES`] (or one with no session)
///   keeps its recorded cost;
/// * a session-cumulative row is the difference to the previous priced
///   session-cumulative row of the same session (admitted rows, ordered by
///   `created_at`, `id`); the first row, and a row whose figure dropped below
///   its predecessor (the session total restarted), count their own value;
/// * a NULL cost stays NULL (unpriced), and never takes part as a predecessor.
///
/// This is the WHOLE-STORE window: it sorts every admitted session-cumulative
/// invocation (~0.2 s and a temp-file spill on a 70k-row hub). No production
/// consumer reads it any more (#330): each reads the session-scoped variants
/// below, and this text stays as the oracle the equivalence tests compare them
/// against ([`COST_INCREMENT_SQL`]). A new whole-store aggregate that really
/// needs it must stay off hot periodic paths and re-expose it from here.
#[cfg(test)]
macro_rules! cost_increment_sql {
    () => {
        "SELECT m.id AS invocation_id,
        CASE WHEN m.estimated_cost_usd IS NULL THEN NULL
             WHEN l.prev_cost IS NULL OR m.estimated_cost_usd < l.prev_cost THEN m.estimated_cost_usd
             ELSE m.estimated_cost_usd - l.prev_cost END AS cost_increment_usd
     FROM model_invocations m
     LEFT JOIN (
        SELECT id, LAG(estimated_cost_usd) OVER (
                   PARTITION BY session_id ORDER BY created_at, id) AS prev_cost
        FROM model_invocations
        WHERE admission_status = 'admitted' AND session_id IS NOT NULL
          AND estimated_cost_usd IS NOT NULL
          AND purpose IN ('session.launch.fresh', 'session.continue.resume', 'session.rotate.child')
     ) l ON l.id = m.id"
    };
}

/// The whole-store increment table restricted to the invocations of one
/// session, `?1`.
/// The predecessor window is partitioned by session, so restricting its input
/// to whole sessions yields exactly the rows the unrestricted table holds for
/// them, without sorting every admitted invocation of the store first (#2939:
/// the full window cost ~0.2 s and a temp-file spill per manager pass on the
/// hub). The unary `+` keeps the planner on the session index: most admitted
/// rows carry those purposes. Zero-argument macros (not one with a parameter)
/// so the protected-DML source audit can expand them. The scoped tables also
/// carry each invocation's `session_id`, `admission_status`, provider and model
/// (billing classification, #3533), so a consumer
/// that groups by session reads them here instead of joining
/// `model_invocations` again per invocation (#3043: the manager pass's spend
/// ledger, bounded per member by #2494).
macro_rules! cost_increment_for_session_sql {
    () => {
        "SELECT m.id AS invocation_id, m.session_id AS session_id,
        m.admission_status AS admission_status,
        CASE WHEN m.estimated_cost_usd IS NULL THEN NULL
             WHEN l.prev_cost IS NULL OR m.estimated_cost_usd < l.prev_cost THEN m.estimated_cost_usd
             ELSE m.estimated_cost_usd - l.prev_cost END AS cost_increment_usd, m.provider AS provider, m.model AS model
     FROM model_invocations m
     LEFT JOIN (
        SELECT id, LAG(estimated_cost_usd) OVER (
                   PARTITION BY session_id ORDER BY created_at, id) AS prev_cost
        FROM model_invocations
        WHERE admission_status = 'admitted' AND session_id IN (?1)
          AND estimated_cost_usd IS NOT NULL
          AND +purpose IN ('session.launch.fresh', 'session.continue.resume', 'session.rotate.child')
     ) l ON l.id = m.id
     WHERE m.session_id IN (?1)"
    };
}
pub(crate) use cost_increment_for_session_sql;

/// [`cost_increment_for_session_sql`] over the sessions of an `ids(id)` CTE the
/// embedding statement defines; the texts differ only in that set (a test
/// compares them).
macro_rules! cost_increment_for_ids_sql {
    () => {
        "SELECT m.id AS invocation_id, m.session_id AS session_id,
        m.admission_status AS admission_status,
        CASE WHEN m.estimated_cost_usd IS NULL THEN NULL
             WHEN l.prev_cost IS NULL OR m.estimated_cost_usd < l.prev_cost THEN m.estimated_cost_usd
             ELSE m.estimated_cost_usd - l.prev_cost END AS cost_increment_usd, m.provider AS provider, m.model AS model
     FROM model_invocations m
     LEFT JOIN (
        SELECT id, LAG(estimated_cost_usd) OVER (
                   PARTITION BY session_id ORDER BY created_at, id) AS prev_cost
        FROM model_invocations
        WHERE admission_status = 'admitted' AND session_id IN (SELECT id FROM ids)
          AND estimated_cost_usd IS NOT NULL
          AND +purpose IN ('session.launch.fresh', 'session.continue.resume', 'session.rotate.child')
     ) l ON l.id = m.id
     WHERE m.session_id IN (SELECT id FROM ids)"
    };
}
pub(crate) use cost_increment_for_ids_sql;

/// The same SQL as a `&str` value, for tests and non-literal contexts.
#[cfg(test)]
pub(crate) const COST_INCREMENT_SQL: &str = cost_increment_sql!();

/// USD per 1M tokens for one model. `cache_read` is `None` when the source
/// documents no cached-input rate; a usage with cache-read tokens then has no
/// sound estimate and resolves to `None` rather than a guess. The optional
/// `cache_write_per_mtok` is the 5-minute Anthropic write rate; existing
/// OpenAI/OpenRouter prices leave it `None`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: Option<f64>,
    pub cache_write_per_mtok: Option<f64>,
}

/// A price the daemon read from OpenRouter's own model discovery (#3154),
/// with the time of the fetch it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiscoveredPrice {
    pub price: ModelPrice,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// The prices OpenRouter's own discovery reported, by model id, each with the
/// time of the fetch it came from. A fetch upserts the models it lists, so a
/// narrower catalog query (the picker asks for text models only) never erases a
/// price an earlier, wider one read.
static DISCOVERED_PRICES: parking_lot::RwLock<
    Option<std::collections::HashMap<String, DiscoveredPrice>>,
> = parking_lot::RwLock::new(None);

/// Upsert `prices` fetched at `fetched_at` into the discovered price table.
pub fn publish_discovered_prices(
    prices: impl IntoIterator<Item = (String, ModelPrice)>,
    fetched_at: chrono::DateTime<chrono::Utc>,
) {
    let mut table = DISCOVERED_PRICES.write();
    let table = table.get_or_insert_with(std::collections::HashMap::new);
    for (model, price) in prices {
        table.insert(model, DiscoveredPrice { price, fetched_at });
    }
}

/// The discovered price of `model`, if a catalog fetch listed one.
#[must_use]
pub fn discovered_price(model: &str) -> Option<DiscoveredPrice> {
    DISCOVERED_PRICES.read().as_ref()?.get(model).copied()
}

/// When the newest catalog fetch happened, `None` before the first one.
#[must_use]
pub fn discovered_prices_fetched_at() -> Option<chrono::DateTime<chrono::Utc>> {
    DISCOVERED_PRICES
        .read()
        .as_ref()?
        .values()
        .map(|entry| entry.fetched_at)
        .max()
}

/// Test seam: forget the discovered table so a test sees only [`PRICES`].
#[cfg(any(test, feature = "test-seam"))]
pub fn clear_discovered_prices() {
    *DISCOVERED_PRICES.write() = None;
}

/// Documented list prices, USD per 1M tokens, keyed by model id.
///
/// Source: OpenRouter `GET https://openrouter.ai/api/v1/models`, fetched
/// 2026-09-24T02:48Z (`pricing.prompt`, `pricing.completion`,
/// `pricing.input_cache_read`, converted per-token -> per-1M), recorded with
/// the same values in `scripts/bench-session-metrics.py`.
///
/// OpenAI GPT-6 family (#585): standard-tier list prices (requests up to
/// 272K input tokens; the >272K long-context tier is not applied because an
/// invocation row carries session-cumulative counters, so the standard rate is
/// a floor for very long sessions). Sources, fetched 2026-10-03:
/// <https://www.eesel.ai/blog/gpt-6-sol-pricing>,
/// <https://www.eesel.ai/blog/gpt-6-luna-pricing>,
/// <https://www.llmreference.com/model/gpt-6.1-sol/openai-api>,
/// <https://www.requesty.ai/models/openai/gpt-6-luna>.
///
/// Anthropic Claude (#3383 / laptop #480): standard global API list prices,
/// fetched 2026-10-20 from
/// <https://docs.anthropic.com/en/docs/about-claude/pricing>
/// (redirects to <https://platform.claude.com/docs/en/about-claude/pricing>).
/// The model-pricing table lists all four rates below, including 5-minute
/// cache writes. The wire uses ephemeral caching without a TTL and settlement
/// records no TTL, so use that default rather than the 1-hour write rate.
/// IDs are from `rsi_common::claude_catalog::CLAUDE_MODEL_CATALOG` plus the
/// Sonnet 5.5 / Haiku 4.5 fallback IDs accepted by the Claude launch path.
/// No Bedrock regional prices or OpenRouter aliases are inferred.
///
/// Anthropic token accounting, fetched 2026-10-20:
/// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
/// (redirects to <https://platform.claude.com/docs/en/build-with-claude/prompt-caching>).
/// `input_tokens` excludes both cache writes and cache reads.
///
/// Every unlisted model stays NULL (the store logs an "unpriced" warning at
/// settlement) until a cited price is added here.
const PRICES: &[(&str, ModelPrice)] = &[
    (
        "claude-fable-5-1",
        ModelPrice {
            input_per_mtok: 10.0,
            output_per_mtok: 50.0,
            cache_read_per_mtok: Some(0.25),
            cache_write_per_mtok: Some(12.5),
        },
    ),
    (
        "claude-opus-5-5",
        ModelPrice {
            input_per_mtok: 4.0,
            output_per_mtok: 20.0,
            cache_read_per_mtok: Some(0.2),
            cache_write_per_mtok: Some(5.0),
        },
    ),
    (
        "claude-opus-4-8",
        ModelPrice {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cache_read_per_mtok: Some(0.5),
            cache_write_per_mtok: Some(6.25),
        },
    ),
    (
        "claude-opus-4-5",
        ModelPrice {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cache_read_per_mtok: Some(0.5),
            cache_write_per_mtok: Some(6.25),
        },
    ),
    (
        "claude-sonnet-5",
        ModelPrice {
            input_per_mtok: 2.0,
            output_per_mtok: 10.0,
            cache_read_per_mtok: Some(0.2),
            cache_write_per_mtok: Some(2.5),
        },
    ),
    (
        "claude-haiku-4-5-20251001",
        ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
            cache_read_per_mtok: Some(0.1),
            cache_write_per_mtok: Some(1.25),
        },
    ),
    (
        "claude-sonnet-5-5",
        ModelPrice {
            input_per_mtok: 2.0,
            output_per_mtok: 10.0,
            cache_read_per_mtok: Some(0.1),
            cache_write_per_mtok: Some(2.5),
        },
    ),
    (
        "claude-haiku-4-5",
        ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
            cache_read_per_mtok: Some(0.1),
            cache_write_per_mtok: Some(1.25),
        },
    ),
    (
        "gpt-6.1-sol",
        ModelPrice {
            input_per_mtok: 2.0,
            output_per_mtok: 10.0,
            cache_read_per_mtok: Some(0.10),
            cache_write_per_mtok: None,
        },
    ),
    (
        "gpt-6-sol",
        ModelPrice {
            input_per_mtok: 2.0,
            output_per_mtok: 10.0,
            cache_read_per_mtok: Some(0.20),
            cache_write_per_mtok: None,
        },
    ),
    (
        "gpt-6-luna",
        ModelPrice {
            input_per_mtok: 0.10,
            output_per_mtok: 0.50,
            cache_read_per_mtok: Some(0.01),
            cache_write_per_mtok: None,
        },
    ),
    (
        "z-ai/glm-5.3-flashx",
        ModelPrice {
            input_per_mtok: 0.37,
            output_per_mtok: 1.25,
            cache_read_per_mtok: Some(0.09),
            cache_write_per_mtok: None,
        },
    ),
    (
        "z-ai/glm-5.3-flash",
        ModelPrice {
            input_per_mtok: 0.15,
            output_per_mtok: 0.50,
            cache_read_per_mtok: Some(0.03),
            cache_write_per_mtok: None,
        },
    ),
    (
        "deepseek/deepseek-v4.1-flash",
        ModelPrice {
            input_per_mtok: 0.14,
            output_per_mtok: 0.42,
            cache_read_per_mtok: Some(0.0042),
            cache_write_per_mtok: None,
        },
    ),
    (
        "qwen/qwen3.8-flash",
        ModelPrice {
            input_per_mtok: 0.15,
            output_per_mtok: 0.47,
            cache_read_per_mtok: Some(0.016),
            cache_write_per_mtok: None,
        },
    ),
    (
        "minimax/minimax-m3",
        ModelPrice {
            input_per_mtok: 0.30,
            output_per_mtok: 1.20,
            cache_read_per_mtok: Some(0.06),
            cache_write_per_mtok: None,
        },
    ),
    (
        "qwen/qwen3-coder-next",
        ModelPrice {
            input_per_mtok: 0.12,
            output_per_mtok: 0.80,
            cache_read_per_mtok: Some(0.07),
            cache_write_per_mtok: None,
        },
    ),
];

/// The price for `model`: the one OpenRouter's own discovery reported (#3154),
/// else the documented list price in [`PRICES`], else `None`. A price is never
/// invented.
pub fn price_for_model(model: &str) -> Option<ModelPrice> {
    discovered_price(model)
        .map(|entry| entry.price)
        .or_else(|| {
            PRICES
                .iter()
                .find(|(id, _)| *id == model)
                .map(|(_, price)| *price)
        })
}

/// Whether `provider` is the loopback `Local` backend (free by construction).
fn is_local_provider(provider: &str) -> bool {
    provider.eq_ignore_ascii_case("local")
}

fn reported_cost(usage: &ModelCallUsage) -> Option<f64> {
    usage
        .estimated_cost_usd
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// Estimate from token usage with a documented price; `None` without one.
///
/// The OpenAI-compatible `input_tokens` already include cached prompt tokens
/// (`cache_read_tokens` is a subset), so uncached input is the difference.
/// Direct Claude/Harness Anthropic calls instead report plain input separately
/// from cache reads and writes. OpenRouter keeps its OpenAI-compatible counting
/// even when the routed model is Claude. Reasoning is already in output.
fn estimate_from_tokens(provider: &str, model: &str, usage: &ModelCallUsage) -> Option<f64> {
    let price = price_for_model(model)?;
    let input = usage.input_tokens?;
    let output = usage.output_tokens?;
    let cache_read = usage.cache_read_tokens.unwrap_or(0);
    let cache_read_cost = match (cache_read, price.cache_read_per_mtok) {
        (0, _) => 0.0,
        (tokens, Some(rate)) => tokens as f64 * rate,
        (_, None) => return None,
    };
    let anthropic = model.starts_with("claude-")
        && ["Harness", "Claude", "Anthropic"]
            .iter()
            .any(|name| provider.eq_ignore_ascii_case(name));
    let (uncached_input, cache_write_cost) = if anthropic {
        let write_cost = match (
            usage.cache_creation_tokens.unwrap_or(0),
            price.cache_write_per_mtok,
        ) {
            (0, _) => 0.0,
            (tokens, Some(rate)) => tokens as f64 * rate,
            (_, None) => return None,
        };
        (input, write_cost)
    } else {
        (input.saturating_sub(cache_read), 0.0)
    };
    let micro_usd = uncached_input as f64 * price.input_per_mtok
        + output as f64 * price.output_per_mtok
        + cache_read_cost
        + cache_write_cost;
    Some(micro_usd / 1_000_000.0)
}

/// Resolve the cost recorded for one settled invocation (module docs).
pub(crate) fn resolve_invocation_cost(
    provider: &str,
    model: Option<&str>,
    usage: &ModelCallUsage,
) -> Option<f64> {
    if let Some(cost) = reported_cost(usage) {
        return Some(cost);
    }
    if is_local_provider(provider) {
        return Some(0.0);
    }
    estimate_from_tokens(provider, model?, usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_invocation(
        store: &crate::store::Store,
        id: &str,
        purpose: &str,
        session: &str,
        cost: Option<f64>,
        minute: u32,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                               paid_risk, admission_status, status, provider,
                                               model, trigger_source, session_id,
                                               estimated_cost_usd, created_at)
                 VALUES(?1, ?2, 'session_lifecycle', 'foreground', 'paid_capable', 'admitted',
                        'completed', 'Claude', 'claude-opus-test', 'test', ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    purpose,
                    session,
                    cost,
                    format!("2026-10-14T10:{minute:02}:00.000000000Z")
                ],
            )
            .unwrap();
    }

    /// #3135: batch 4 (`z-ai/glm-5.3-flash`) failed every launch with `bench_task_cost_unknown`
    /// because the model had no documented price; a bench run needs one to bound a call.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn the_glm_flash_bench_model_has_a_documented_price() {
        let price = PRICES
            .iter()
            .find(|(id, _)| *id == "z-ai/glm-5.3-flash")
            .map(|(_, price)| *price)
            .expect("z-ai/glm-5.3-flash is documented");
        assert_eq!(price.input_per_mtok, 0.15);
        assert_eq!(price.output_per_mtok, 0.50);
        assert_eq!(price.cache_read_per_mtok, Some(0.03));
    }

    /// #2593: a launch/resume row's `estimated_cost_usd` is the SESSION's
    /// cumulative cost, so the pinned meaning of the stored column is "running
    /// total" and an invocation's own cost is the increment over the previous
    /// priced launch/resume row of the same session.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn session_launch_and_resume_costs_are_cumulative_and_read_as_increments() {
        let store = crate::store::Store::open_in_memory().expect("store");
        let a = "00000000-0000-0000-0000-00000000000a";
        let b = "00000000-0000-0000-0000-00000000000b";
        let launch = "session.launch.fresh";
        let resume = "session.continue.resume";
        // Session A: rising running totals, a restart (drop to 0.20), an
        // unpriced row that is nobody's predecessor, then more growth.
        insert_invocation(&store, "a1", launch, a, Some(0.61), 1);
        insert_invocation(&store, "a2", resume, a, Some(0.90), 2);
        insert_invocation(&store, "a3", resume, a, Some(0.93), 3);
        insert_invocation(&store, "a4", resume, a, Some(0.20), 4);
        insert_invocation(&store, "a5", resume, a, None, 5);
        insert_invocation(&store, "a6", resume, a, Some(0.50), 6);
        // A per-call purpose of the same session keeps its own cost.
        insert_invocation(&store, "a7", "session.turn", a, Some(2.0), 7);
        // Another session never differences against A.
        insert_invocation(&store, "b1", launch, b, Some(1.0), 2);
        insert_invocation(&store, "b2", resume, b, Some(1.5), 8);

        let mut stmt = store
            .conn
            .prepare(&format!(
                "SELECT invocation_id, cost_increment_usd FROM ({COST_INCREMENT_SQL})"
            ))
            .unwrap();
        let increments: std::collections::HashMap<String, Option<f64>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(increments.len(), 9, "one row per invocation");
        let near = |id: &str, want: Option<f64>| match (increments[id], want) {
            (Some(got), Some(want)) => assert!((got - want).abs() < 1e-9, "{id}: {got}"),
            (got, want) => assert_eq!(got, want, "{id}"),
        };
        near("a1", Some(0.61));
        near("a2", Some(0.29));
        near("a3", Some(0.03));
        near("a4", Some(0.20));
        near("a5", None);
        near("a6", Some(0.30));
        near("a7", Some(2.0));
        near("b1", Some(1.0));
        near("b2", Some(0.5));

        // The raw column overstates; the session total sums the increments and
        // equals the last running total plus the independent per-call cost.
        let raw_a: f64 = store
            .conn
            .query_row(
                "SELECT SUM(estimated_cost_usd) FROM model_invocations WHERE session_id = ?1",
                [a],
                |row| row.get(0),
            )
            .unwrap();
        assert!((raw_a - 5.14).abs() < 1e-9, "{raw_a}");
        let total = store
            .session_model_invocation_cost_total(a.parse().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            (total - (0.61 + 0.29 + 0.03 + 0.20 + 0.30 + 2.0)).abs() < 1e-9,
            "{total}"
        );
    }

    /// #2939: the session-scoped derived table gives every invocation of the
    /// scoped sessions exactly the increment of the unrestricted table, and
    /// does not visit the other sessions' invocations.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn session_scoped_increments_equal_the_unrestricted_increments_2939() {
        let store = crate::store::Store::open_in_memory().expect("store");
        let a = "00000000-0000-0000-0000-00000000000a";
        let b = "00000000-0000-0000-0000-00000000000b";
        let launch = "session.launch.fresh";
        let resume = "session.continue.resume";
        insert_invocation(&store, "a1", launch, a, Some(0.61), 1);
        insert_invocation(&store, "a2", resume, a, Some(0.90), 2);
        insert_invocation(&store, "a3", resume, a, Some(0.20), 3);
        insert_invocation(&store, "a4", resume, a, None, 4);
        insert_invocation(&store, "a5", "session.turn", a, Some(2.0), 5);
        insert_invocation(&store, "b1", launch, b, Some(1.0), 2);
        insert_invocation(&store, "b2", resume, b, Some(1.5), 8);
        let rows = |sql: &str, params: &[&str]| {
            let mut stmt = store.conn.prepare(sql).unwrap();
            stmt.query_map(rusqlite::params_from_iter(params), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
            })
            .unwrap()
            .collect::<Result<std::collections::BTreeMap<_, _>, _>>()
            .unwrap()
        };
        let full = rows(
            &format!("SELECT invocation_id, cost_increment_usd FROM ({COST_INCREMENT_SQL})"),
            &[],
        );
        let scoped = rows(
            concat!(
                "SELECT invocation_id, cost_increment_usd FROM (",
                cost_increment_for_session_sql!(),
                ")"
            ),
            &[a],
        );
        let want: std::collections::BTreeMap<_, _> = full
            .into_iter()
            .filter(|(id, _)| id.starts_with('a'))
            .collect();
        assert_eq!(want.len(), 5);
        assert_eq!(scoped, want);
        assert_eq!(
            cost_increment_for_ids_sql!().replace("SELECT id FROM ids", "?1"),
            cost_increment_for_session_sql!(),
            "the two scoped texts differ only in their session set"
        );
    }

    /// #3534: a rotation successor's first row (`session.rotate.child`) carries
    /// the session's running total and its resumes continue it, so a resume is
    /// charged its growth over that row, not its whole running total again.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_resume_after_a_rotation_row_is_charged_its_growth_only_3534() {
        let store = crate::store::Store::open_in_memory().expect("store");
        let a = "00000000-0000-0000-0000-00000000000a";
        insert_invocation(&store, "a1", "session.rotate.child", a, Some(0.97), 1);
        insert_invocation(&store, "a2", "session.continue.resume", a, Some(1.10), 2);
        insert_invocation(&store, "a3", "session.continue.resume", a, Some(1.50), 3);
        let increment = |id: &str| -> f64 {
            store
                .conn
                .query_row(
                    &format!(
                        "SELECT cost_increment_usd FROM ({COST_INCREMENT_SQL}) WHERE invocation_id = ?1"
                    ),
                    [id],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert!((increment("a1") - 0.97).abs() < 1e-9);
        assert!((increment("a2") - 0.13).abs() < 1e-9, "{}", increment("a2"));
        assert!((increment("a3") - 0.40).abs() < 1e-9);
        let total = store
            .session_model_invocation_cost_total(a.parse().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            (total - 1.50).abs() < 1e-9,
            "the session total is its last running total: {total}"
        );
    }

    /// `sessions` unrelated sessions of `rows` cumulative launch/resume rows
    /// each, none of them the sessions a consumer under test reads.
    fn seed_unrelated_sessions(store: &crate::store::Store, sessions: usize, rows: usize) {
        for session in 0..sessions {
            let session_id = format!("00000000-0000-0000-0001-{session:012}");
            for row in 0..rows {
                insert_invocation(
                    store,
                    &format!("u-{session}-{row}"),
                    "session.continue.resume",
                    &session_id,
                    Some(0.1 * (row + 1) as f64),
                    (row % 50) as u32,
                );
            }
        }
    }

    /// #330 (hub #2972): `session_model_invocation_cost_total` reads one
    /// session, so it sorts only that session's window. The total equals the sum
    /// of the unrestricted increments, and its VM-step count (not wall clock)
    /// does not grow with the invocations of unrelated sessions, while the
    /// unrestricted derived table does.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn session_cost_total_equals_the_unrestricted_sum_and_ignores_unrelated_sessions_330() {
        use crate::sandbox::startup_restore_tests::vm_ops_during;

        let a = "00000000-0000-0000-0000-00000000000a";
        let unrestricted_sum = |store: &crate::store::Store| -> f64 {
            store
                .conn
                .query_row(
                    &format!(
                        "SELECT SUM(ci.cost_increment_usd) FROM model_invocations mi
                         JOIN ({COST_INCREMENT_SQL}) ci ON ci.invocation_id = mi.id
                         WHERE mi.session_id = ?1 AND mi.admission_status = 'admitted'
                           AND ci.cost_increment_usd IS NOT NULL"
                    ),
                    [a],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let seed_target = |store: &crate::store::Store| {
            insert_invocation(store, "a1", "session.launch.fresh", a, Some(0.61), 1);
            insert_invocation(store, "a2", "session.continue.resume", a, Some(0.90), 2);
            insert_invocation(store, "a3", "session.continue.resume", a, Some(0.20), 3);
            insert_invocation(store, "a4", "session.continue.resume", a, None, 4);
            insert_invocation(store, "a5", "session.turn", a, Some(2.0), 5);
        };
        let measure = |unrelated_rows: usize| {
            let mut store = crate::store::Store::open_in_memory().expect("store");
            seed_target(&store);
            seed_unrelated_sessions(&store, 20, unrelated_rows);
            let (total, scoped_ops) = vm_ops_during(&mut store, |store| {
                store.session_model_invocation_cost_total(a.parse().unwrap())
            });
            let total = total.unwrap().unwrap();
            let want = unrestricted_sum(&store);
            assert!((total - want).abs() < 1e-9, "{total} vs {want}");
            assert!((total - (0.61 + 0.29 + 0.20 + 2.0)).abs() < 1e-9, "{total}");
            let (_, full_ops) = vm_ops_during(&mut store, |store| unrestricted_sum(store));
            (scoped_ops, full_ops)
        };
        let (scoped_small, full_small) = measure(2);
        let (scoped_large, full_large) = measure(30);
        eprintln!(
            "330 session total vm ops: scoped {scoped_small} -> {scoped_large}, \
             unrestricted {full_small} -> {full_large}"
        );
        assert!(
            scoped_large <= scoped_small + scoped_small / 20,
            "the scoped total must not grow with unrelated invocations: \
             {scoped_small} -> {scoped_large}"
        );
        assert!(
            full_large > full_small * 2,
            "the probe must see the unrestricted window grow: {full_small} -> {full_large}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn cumulative_purpose_list_matches_the_increment_sql() {
        for purpose in SESSION_CUMULATIVE_PURPOSES {
            assert!(
                COST_INCREMENT_SQL.contains(&format!("'{purpose}'")),
                "{purpose}"
            );
        }
        assert_eq!(
            COST_INCREMENT_SQL.matches("'session.").count(),
            SESSION_CUMULATIVE_PURPOSES.len()
        );
    }

    fn usage(input: u64, output: u64) -> ModelCallUsage {
        ModelCallUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..ModelCallUsage::default()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn provider_reported_cost_is_used_verbatim_over_any_estimate() {
        let mut call = usage(1_000_000, 1_000_000);
        call.estimated_cost_usd = Some(0.0123);
        let cost = resolve_invocation_cost("OpenRouter", Some("minimax/minimax-m3"), &call);
        assert_eq!(cost, Some(0.0123));
        // A reported cost is verbatim even when the model has no known price.
        let cost = resolve_invocation_cost("Codex", Some("unpriced-model"), &call);
        assert_eq!(cost, Some(0.0123));
    }

    /// #3154: an estimate uses the price OpenRouter's discovery reported when
    /// there is one, else the documented table; a provider-reported cost still
    /// wins over both.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn discovered_price_feeds_the_estimate_and_never_beats_a_reported_cost() {
        let model = "test-3154/discovered-only";
        assert_eq!(price_for_model(model), None);
        assert_eq!(
            resolve_invocation_cost("OpenRouter", Some(model), &usage(1_000_000, 1_000_000)),
            None,
            "an unpriced model stays NULL"
        );
        let at = chrono::Utc::now();
        publish_discovered_prices(
            [(
                model.to_string(),
                ModelPrice {
                    input_per_mtok: 0.20,
                    output_per_mtok: 0.60,
                    cache_read_per_mtok: Some(0.02),
                    cache_write_per_mtok: None,
                },
            )],
            at,
        );
        assert_eq!(discovered_price(model).unwrap().fetched_at, at);
        // 1M in at $0.20 + 1M out at $0.60.
        let cost = resolve_invocation_cost("OpenRouter", Some(model), &usage(1_000_000, 1_000_000))
            .unwrap();
        assert!((cost - 0.80).abs() < 1e-9, "{cost}");
        let mut reported = usage(1_000_000, 1_000_000);
        reported.estimated_cost_usd = Some(0.5);
        assert_eq!(
            resolve_invocation_cost("OpenRouter", Some(model), &reported),
            Some(0.5)
        );
        // A later fetch replaces that model's price and keeps the others.
        publish_discovered_prices(
            [(
                model.to_string(),
                ModelPrice {
                    input_per_mtok: 1.0,
                    output_per_mtok: 1.0,
                    cache_read_per_mtok: None,
                    cache_write_per_mtok: None,
                },
            )],
            at + chrono::Duration::minutes(5),
        );
        assert_eq!(price_for_model(model).unwrap().input_per_mtok, 1.0);
        assert_eq!(
            price_for_model("deepseek/deepseek-v4.1-flash")
                .unwrap()
                .input_per_mtok,
            0.14,
            "a model discovery never listed keeps its documented price"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn known_price_estimates_cost_from_tokens() {
        // minimax/minimax-m3: $0.30 in / $1.20 out per 1M tokens.
        let cost = resolve_invocation_cost(
            "OpenRouter",
            Some("minimax/minimax-m3"),
            &usage(2_000_000, 500_000),
        )
        .expect("known price estimates");
        assert!((cost - (0.60 + 0.60)).abs() < 1e-9, "cost was {cost}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn cache_read_tokens_are_billed_at_the_cached_rate_inside_the_prompt() {
        // 1M prompt tokens of which 600k are cache reads at $0.06; 400k plain
        // input at $0.30; 100k output at $1.20.
        let mut call = usage(1_000_000, 100_000);
        call.cache_read_tokens = Some(600_000);
        let cost = resolve_invocation_cost("OpenRouter", Some("minimax/minimax-m3"), &call)
            .expect("known price estimates");
        let expected = 0.4 * 0.30 + 0.6 * 0.06 + 0.1 * 1.20;
        assert!((cost - expected).abs() < 1e-9, "cost was {cost}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn claude_catalog_and_launch_fallbacks_have_complete_prices() {
        let models = rsi_common::claude_catalog::CLAUDE_MODEL_CATALOG
            .iter()
            .map(|spec| spec.id)
            .chain(["claude-sonnet-5-5", "claude-haiku-4-5"]);
        for model in models {
            let price = price_for_model(model).expect(model);
            assert!(price.input_per_mtok > 0.0, "{model}");
            assert!(price.output_per_mtok > 0.0, "{model}");
            assert!(price.cache_read_per_mtok.is_some(), "{model}");
            assert!(price.cache_write_per_mtok.is_some(), "{model}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn anthropic_cache_tokens_are_separate_from_plain_input() {
        let mut call = usage(1_000, 500);
        call.cache_creation_tokens = Some(2_000);
        call.cache_read_tokens = Some(6_000);
        for provider in ["Harness", "Claude", "Anthropic"] {
            let cost = resolve_invocation_cost(provider, Some("claude-sonnet-5"), &call)
                .expect("documented Claude price");
            // .002 input + .005 writes + .0012 reads + .005 output.
            assert!((cost - 0.0132).abs() < 1e-12, "{provider}: {cost}");
        }
        // Absent cache counters mean no cache activity, and zero plain input
        // is valid for a fully cached request.
        let plain = resolve_invocation_cost("Harness", Some("claude-sonnet-5"), &usage(1_000, 500))
            .expect("plain Claude call");
        assert!((plain - 0.007).abs() < 1e-12, "{plain}");
        call.input_tokens = Some(0);
        let cached = resolve_invocation_cost("Harness", Some("claude-sonnet-5"), &call)
            .expect("fully cached Claude call");
        assert!((cached - 0.0112).abs() < 1e-12, "{cached}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn openrouter_keeps_inclusive_input_accounting_even_for_a_claude_model() {
        let mut call = usage(10_000, 500);
        call.cache_creation_tokens = Some(2_000);
        call.cache_read_tokens = Some(6_000);
        let cost = resolve_invocation_cost("OpenRouter", Some("claude-sonnet-5"), &call)
            .expect("documented price");
        // The existing path bills 4k uncached + 6k cached input + 500 output;
        // cache creation does not add an Anthropic surcharge to this path.
        assert!((cost - 0.0142).abs() < 1e-12, "cost was {cost}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn anthropic_cache_usage_requires_documented_cache_rates() {
        let model = "claude-test-3383-no-cache-rates";
        publish_discovered_prices(
            [(
                model.to_string(),
                ModelPrice {
                    input_per_mtok: 2.0,
                    output_per_mtok: 10.0,
                    cache_read_per_mtok: None,
                    cache_write_per_mtok: None,
                },
            )],
            chrono::Utc::now(),
        );
        let mut call = usage(1_000, 500);
        let plain = resolve_invocation_cost("Harness", Some(model), &call).unwrap();
        assert!((plain - 0.007).abs() < 1e-12);
        call.cache_creation_tokens = Some(2_000);
        assert_eq!(resolve_invocation_cost("Harness", Some(model), &call), None);
        call.cache_creation_tokens = Some(0);
        call.cache_read_tokens = Some(6_000);
        assert_eq!(resolve_invocation_cost("Harness", Some(model), &call), None);
        call.estimated_cost_usd = Some(0.25);
        assert_eq!(
            resolve_invocation_cost("Harness", Some(model), &call),
            Some(0.25)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn unknown_price_stays_null_for_every_provider_path() {
        for (provider, model) in [
            ("Codex", "gpt-6-astra"),
            ("CodexAppServer", "gpt-6-terra"),
            ("Harness", "gpt-5.4"),
            ("OpenRouter", "vendor/unpriced-model"),
        ] {
            let cost = resolve_invocation_cost(provider, Some(model), &usage(1000, 1000));
            assert_eq!(cost, None, "{provider}/{model} has no documented price");
        }
        assert_eq!(
            resolve_invocation_cost("Harness", None, &usage(1000, 1000)),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn missing_usage_does_not_guess_a_cost() {
        let model = Some("minimax/minimax-m3");
        assert_eq!(
            resolve_invocation_cost("OpenRouter", model, &ModelCallUsage::default()),
            None
        );
        assert!(price_for_model("minimax/minimax-m3").is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn codex_gpt_6_1_sol_settles_with_a_documented_price_estimate() {
        // 1M input of which 400k cached, 100k output:
        // 600k*$2 + 400k*$0.10 + 100k*$10 per 1M = 1.2 + 0.04 + 1.0.
        let mut call = usage(1_000_000, 100_000);
        call.cache_read_tokens = Some(400_000);
        for provider in ["Codex", "CodexAppServer"] {
            let cost = resolve_invocation_cost(provider, Some("gpt-6.1-sol"), &call)
                .expect("gpt-6.1-sol is priced");
            assert!((cost - 2.24).abs() < 1e-9, "{provider}: {cost}");
        }
        let luna =
            resolve_invocation_cost("Codex", Some("gpt-6-luna"), &usage(1_000_000, 1_000_000))
                .expect("gpt-6-luna is priced");
        assert!((luna - 0.60).abs() < 1e-9, "{luna}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn local_models_cost_a_real_zero() {
        assert_eq!(
            resolve_invocation_cost("Local", Some("qwen3:14b"), &usage(5000, 700)),
            Some(0.0)
        );
        // Even with no usage at all: free is not unknown.
        assert_eq!(
            resolve_invocation_cost("Local", None, &ModelCallUsage::default()),
            Some(0.0)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn negative_or_non_finite_reported_cost_is_ignored() {
        let mut call = usage(1_000_000, 0);
        call.estimated_cost_usd = Some(f64::NAN);
        let cost = resolve_invocation_cost("OpenRouter", Some("minimax/minimax-m3"), &call);
        assert!((cost.expect("non-finite falls back to estimate") - 0.30).abs() < 1e-9);
        call.estimated_cost_usd = Some(-1.0);
        let cost = resolve_invocation_cost("Codex", Some("gpt-6-astra"), &call);
        assert_eq!(cost, None);
    }
}
