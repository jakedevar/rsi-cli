//! "Budgets" settings category add/edit form overlay.
//!
//! Mirrors the shape of `hook_form.rs` (plain String fields edited via
//! push/pop, Tab/BackTab cycling, Enter to submit, Esc to cancel) generalized
//! to TWO cyclable index fields (`scope_kind`, `model_tier`) instead of one.
//! Submit does NOT write locally — it enqueues `LcAction::SubmitBudgetPolicy`
//! for the daemon RPC round-trip (`UpdateModelControlPolicy` with
//! `replace_policies: true`); see
//! `action_handler::daemon_config::submit_model_budget_policy`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::model_control_budgets::{MODEL_TIERS, SCOPE_KINDS, model_tier_index, scope_kind_index};
use crate::types::OverlayState;
use rsi_common::model_control::{
    BudgetScopeKind, ModelBudgetPolicy, ModelInvocationPurpose, ModelTier,
};

const FIELD_COUNT: usize = 9;
const FIELD_SCOPE_KIND: usize = 0;
const FIELD_SCOPE_ID: usize = 1;
const FIELD_PURPOSE: usize = 2;
const FIELD_MODEL_TIER: usize = 3;
const FIELD_MAX_TOTAL_TOKENS: usize = 4;
const FIELD_MAX_CONCURRENCY: usize = 5;
const FIELD_MAX_CALLS_PER_WINDOW: usize = 6;
const FIELD_RATE_WINDOW_SECONDS: usize = 7;
const FIELD_ALERT_THRESHOLD_RATIO: usize = 8;

/// Open the Budget Policy form overlay. Pass `None` to add a new policy, or
/// `Some((idx, policy))` to edit the policy at `idx` in the current
/// `app.cached_model_control_status.policies` snapshot.
pub fn open_budget_policy_form(app: &mut App, editing: Option<(usize, &ModelBudgetPolicy)>) {
    let (
        scope_kind_idx,
        scope_id,
        purpose,
        model_tier_idx,
        max_total_tokens,
        max_concurrency,
        max_calls_per_window,
        rate_window_seconds,
        alert_threshold_ratio,
        editing_idx,
        original,
    ) = match editing {
        Some((idx, policy)) => (
            scope_kind_index(policy.scope_kind),
            policy.scope_id.clone().unwrap_or_default(),
            policy
                .purpose
                .map(|p| p.as_str().to_string())
                .unwrap_or_default(),
            model_tier_index(policy.model_tier),
            policy
                .max_total_tokens
                .map(|v| v.to_string())
                .unwrap_or_default(),
            policy
                .max_concurrency
                .map(|v| v.to_string())
                .unwrap_or_default(),
            policy
                .max_calls_per_window
                .map(|v| v.to_string())
                .unwrap_or_default(),
            policy
                .rate_window_seconds
                .map(|v| v.to_string())
                .unwrap_or_default(),
            policy
                .alert_threshold_ratio
                .map(|v| v.to_string())
                .unwrap_or_default(),
            Some(idx),
            Some(policy.clone()),
        ),
        None => (
            0,
            String::new(),
            String::new(),
            0,
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            None,
            None,
        ),
    };

    app.overlay = OverlayState::BudgetPolicyForm {
        focused_field: FIELD_SCOPE_KIND,
        scope_kind_idx,
        scope_id,
        purpose,
        model_tier_idx,
        max_total_tokens,
        max_concurrency,
        max_calls_per_window,
        rate_window_seconds,
        alert_threshold_ratio,
        editing: editing_idx,
        original,
    };
}

/// Handle key events inside the `BudgetPolicyForm` overlay.
pub(super) fn handle_budget_policy_form_key(app: &mut App, key: KeyEvent) {
    let focused_field = match &app.overlay {
        OverlayState::BudgetPolicyForm { focused_field, .. } => *focused_field,
        _ => return,
    };

    match key.code {
        KeyCode::BackTab => cycle_field(app, false),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => cycle_field(app, false),
        KeyCode::Tab => cycle_field(app, true),
        KeyCode::Up | KeyCode::Down if focused_field == FIELD_SCOPE_KIND => {
            cycle_scope_kind(app, matches!(key.code, KeyCode::Down));
        }
        KeyCode::Up | KeyCode::Down if focused_field == FIELD_MODEL_TIER => {
            cycle_model_tier(app, matches!(key.code, KeyCode::Down));
        }
        KeyCode::Enter => submit_budget_policy_form(app),
        KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(c) => append_to_field(app, focused_field, c),
        KeyCode::Backspace => pop_from_field(app, focused_field),
        _ => {}
    }
}

fn cycle_field(app: &mut App, forward: bool) {
    if let OverlayState::BudgetPolicyForm { focused_field, .. } = &mut app.overlay {
        if forward {
            *focused_field = (*focused_field + 1) % FIELD_COUNT;
        } else {
            *focused_field = (*focused_field + FIELD_COUNT - 1) % FIELD_COUNT;
        }
    }
}

fn cycle_scope_kind(app: &mut App, forward: bool) {
    if let OverlayState::BudgetPolicyForm { scope_kind_idx, .. } = &mut app.overlay {
        let len = SCOPE_KINDS.len();
        *scope_kind_idx = if forward {
            (*scope_kind_idx + 1) % len
        } else {
            (*scope_kind_idx + len - 1) % len
        };
    }
}

fn cycle_model_tier(app: &mut App, forward: bool) {
    if let OverlayState::BudgetPolicyForm { model_tier_idx, .. } = &mut app.overlay {
        let len = MODEL_TIERS.len();
        *model_tier_idx = if forward {
            (*model_tier_idx + 1) % len
        } else {
            (*model_tier_idx + len - 1) % len
        };
    }
}

fn append_to_field(app: &mut App, field: usize, c: char) {
    if let OverlayState::BudgetPolicyForm {
        scope_id,
        purpose,
        max_total_tokens,
        max_concurrency,
        max_calls_per_window,
        rate_window_seconds,
        alert_threshold_ratio,
        ..
    } = &mut app.overlay
    {
        match field {
            FIELD_SCOPE_ID => scope_id.push(c),
            FIELD_PURPOSE => purpose.push(c),
            FIELD_MAX_TOTAL_TOKENS => {
                if c.is_ascii_digit() {
                    max_total_tokens.push(c);
                }
            }
            FIELD_MAX_CONCURRENCY => {
                if c.is_ascii_digit() {
                    max_concurrency.push(c);
                }
            }
            FIELD_MAX_CALLS_PER_WINDOW => {
                if c.is_ascii_digit() {
                    max_calls_per_window.push(c);
                }
            }
            FIELD_RATE_WINDOW_SECONDS => {
                if c.is_ascii_digit() {
                    rate_window_seconds.push(c);
                }
            }
            FIELD_ALERT_THRESHOLD_RATIO => {
                if c.is_ascii_digit() || c == '.' {
                    alert_threshold_ratio.push(c);
                }
            }
            _ => {} // FIELD_SCOPE_KIND / FIELD_MODEL_TIER are cycle fields, no text edit
        }
    }
}

fn pop_from_field(app: &mut App, field: usize) {
    if let OverlayState::BudgetPolicyForm {
        scope_id,
        purpose,
        max_total_tokens,
        max_concurrency,
        max_calls_per_window,
        rate_window_seconds,
        alert_threshold_ratio,
        ..
    } = &mut app.overlay
    {
        match field {
            FIELD_SCOPE_ID => {
                scope_id.pop();
            }
            FIELD_PURPOSE => {
                purpose.pop();
            }
            FIELD_MAX_TOTAL_TOKENS => {
                max_total_tokens.pop();
            }
            FIELD_MAX_CONCURRENCY => {
                max_concurrency.pop();
            }
            FIELD_MAX_CALLS_PER_WINDOW => {
                max_calls_per_window.pop();
            }
            FIELD_RATE_WINDOW_SECONDS => {
                rate_window_seconds.pop();
            }
            FIELD_ALERT_THRESHOLD_RATIO => {
                alert_threshold_ratio.pop();
            }
            _ => {}
        }
    }
}

/// Parse the optional `purpose` free-text field against the real
/// `ModelInvocationPurpose` enum via its serde string representation (there
/// is no hand-rolled `FromStr` on that type — see
/// `rsi_common::model_control::ModelInvocationPurpose`; deserializing a JSON
/// string is the only parse path that can't drift from the real enum).
fn parse_purpose(trimmed: &str) -> Result<Option<ModelInvocationPurpose>, ()> {
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_value::<ModelInvocationPurpose>(serde_json::Value::String(trimmed.to_string()))
        .map(Some)
        .map_err(|_| ())
}

fn parse_optional_u64(raw: &str, field_name: &str) -> Result<Option<u64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{field_name} must be a non-negative integer"))
}

fn parse_optional_u32(raw: &str, field_name: &str) -> Result<Option<u32>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u32>()
        .map(Some)
        .map_err(|_| format!("{field_name} must be a non-negative integer"))
}

/// All-`None` base policy for the "new policy" case. `ModelBudgetPolicy` has
/// no `Default` impl (every field is a plain `Option<T>` with no
/// `#[serde(default)]` shortcut — confirmed against
/// `rsi_common::model_control`), so this literal is the one place that must
/// be kept in sync with the struct's field list.
fn all_none_policy() -> ModelBudgetPolicy {
    ModelBudgetPolicy {
        scope_kind: BudgetScopeKind::Global,
        scope_id: None,
        purpose: None,
        model_tier: None,
        effort: None,
        ceiling_model_tier: None,
        ceiling_effort: None,
        max_calls: None,
        max_total_tokens: None,
        max_input_tokens: None,
        max_output_tokens: None,
        max_cache_creation_tokens: None,
        max_cache_read_tokens: None,
        max_reasoning_tokens: None,
        max_embedding_inputs: None,
        max_wall_time_ms: None,
        max_concurrency: None,
        max_retries: None,
        max_calls_per_window: None,
        rate_window_seconds: None,
        alert_threshold_ratio: None,
    }
}

/// Build the final policy, validate, and enqueue `LcAction::SubmitBudgetPolicy`.
/// Does NOT close the overlay on validation failure — the user fixes the
/// field and retries (mirrors `provider_form.rs`'s early-return pattern).
fn submit_budget_policy_form(app: &mut App) {
    let (
        scope_kind_idx,
        scope_id,
        purpose,
        model_tier_idx,
        max_total_tokens,
        max_concurrency,
        max_calls_per_window,
        rate_window_seconds,
        alert_threshold_ratio,
        editing,
        original,
    ) = match &app.overlay {
        OverlayState::BudgetPolicyForm {
            focused_field: _,
            scope_kind_idx,
            scope_id,
            purpose,
            model_tier_idx,
            max_total_tokens,
            max_concurrency,
            max_calls_per_window,
            rate_window_seconds,
            alert_threshold_ratio,
            editing,
            original,
        } => (
            *scope_kind_idx,
            scope_id.clone(),
            purpose.clone(),
            *model_tier_idx,
            max_total_tokens.clone(),
            max_concurrency.clone(),
            max_calls_per_window.clone(),
            rate_window_seconds.clone(),
            alert_threshold_ratio.clone(),
            *editing,
            original.clone(),
        ),
        _ => return,
    };

    let scope_kind = SCOPE_KINDS
        .get(scope_kind_idx)
        .map_or(BudgetScopeKind::Global, |(k, _)| *k);
    let model_tier: Option<ModelTier> = MODEL_TIERS.get(model_tier_idx).and_then(|(t, _)| *t);

    let scope_id_trimmed = scope_id.trim().to_string();
    if scope_kind != BudgetScopeKind::Global && scope_id_trimmed.is_empty() {
        app.notify_error(format!(
            "Budget policy requires scope_id for {scope_kind:?}"
        ));
        return;
    }

    let purpose_trimmed = purpose.trim();
    let Ok(purpose_parsed) = parse_purpose(purpose_trimmed) else {
        app.notify_error(format!("Invalid purpose: {purpose_trimmed}"));
        return;
    };

    let max_total_tokens_parsed = match parse_optional_u64(&max_total_tokens, "max_total_tokens") {
        Ok(v) => v,
        Err(msg) => {
            app.notify_error(msg);
            return;
        }
    };
    let max_concurrency_parsed = match parse_optional_u32(&max_concurrency, "max_concurrency") {
        Ok(v) => v,
        Err(msg) => {
            app.notify_error(msg);
            return;
        }
    };
    let max_calls_per_window_parsed =
        match parse_optional_u64(&max_calls_per_window, "max_calls_per_window") {
            Ok(v) => v,
            Err(msg) => {
                app.notify_error(msg);
                return;
            }
        };
    let rate_window_seconds_parsed =
        match parse_optional_u64(&rate_window_seconds, "rate_window_seconds") {
            Ok(v) => v,
            Err(msg) => {
                app.notify_error(msg);
                return;
            }
        };

    let alert_threshold_ratio_parsed = {
        let trimmed = alert_threshold_ratio.trim();
        if trimmed.is_empty() {
            None
        } else {
            match trimmed.parse::<f64>() {
                Ok(v) if v > 0.0 && v <= 1.0 => Some(v),
                _ => {
                    app.notify_error("alert_threshold_ratio must be in (0, 1]");
                    return;
                }
            }
        }
    };

    // Start from the full original policy (preserves the 12 hidden fields
    // this form doesn't expose: effort, ceiling_model_tier, ceiling_effort,
    // max_calls, max_input_tokens, max_output_tokens,
    // max_cache_creation_tokens, max_cache_read_tokens, max_reasoning_tokens,
    // max_embedding_inputs, max_wall_time_ms, max_retries). New policy = the
    // all-None base.
    let mut policy = original.unwrap_or_else(all_none_policy);
    policy.scope_kind = scope_kind;
    policy.scope_id = if scope_kind == BudgetScopeKind::Global {
        Some("global".to_string())
    } else {
        Some(scope_id_trimmed)
    };
    policy.purpose = purpose_parsed;
    policy.model_tier = model_tier;
    policy.max_total_tokens = max_total_tokens_parsed;
    policy.max_concurrency = max_concurrency_parsed;
    policy.max_calls_per_window = max_calls_per_window_parsed;
    policy.rate_window_seconds = rate_window_seconds_parsed;
    policy.alert_threshold_ratio = alert_threshold_ratio_parsed;

    // Mirror rsid's `validate_model_budget_policy` "at least one limit" rule
    // (crates/rsid/src/store/model_control.rs) EXACTLY — checked against the
    // final merged policy so a hidden field already set on `original` (e.g.
    // max_retries from some other tool) satisfies the check even though this
    // form doesn't expose it. Note max_cache_creation_tokens/
    // max_cache_read_tokens/max_reasoning_tokens/max_calls_per_window/
    // rate_window_seconds are deliberately NOT part of this union — the
    // daemon does not count them toward "at least one limit".
    let has_limit = policy.max_calls.is_some()
        || policy.max_total_tokens.is_some()
        || policy.max_input_tokens.is_some()
        || policy.max_output_tokens.is_some()
        || policy.max_embedding_inputs.is_some()
        || policy.max_wall_time_ms.is_some()
        || policy.max_concurrency.is_some()
        || policy.max_retries.is_some();
    if !has_limit {
        app.notify_error(
            "Budget policy must set at least one limit (tokens, concurrency, calls, retries, or wall time)",
        );
        return;
    }

    app.pending_lc_actions.push(LcAction::SubmitBudgetPolicy {
        policy,
        editing_index: editing,
    });
    app.overlay = OverlayState::None;
}
