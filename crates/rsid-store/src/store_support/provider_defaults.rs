//! Per-provider default models, effort-level maps and error-class names the
//! store reads (moved down from `codex`, `claude`, `openrouter` and `pioneer`,
//! which re-export them so callers keep their paths).

pub const CODEX_TOOL_HISTORY_ERROR_CLASS: &str = "codex_resume_tool_history_invalid";
pub const CODEX_USAGE_LIMIT_STOP_REASON: &str = "provider_error:codex_usage_limit";
/// Closed stop reason for a provider startup that the provider rejected as
/// unauthorized (an invalidated or missing credential, HTTP 401). The credential
/// stays operator-owned; this only names the failure class (#1610).
pub const PROVIDER_AUTH_INVALID_STOP_REASON: &str = "provider_error:provider_auth_invalid";

/// Closed stop reason for a provider startup that failed because the provider
/// CLI is not logged in on the host (#2753): the fix is the host-side login
/// (`claude auth login`, `codex login`), not a credential rotation. Distinct
/// from [`PROVIDER_AUTH_INVALID_STOP_REASON`], a credential the provider
/// rejected.
pub const PROVIDER_LOGIN_REQUIRED_STOP_REASON: &str = "provider_error:provider_login_required";
/// Closed stop reason for a provider startup the provider refused for an
/// account billing fault (Claude `system/api_retry` `billing_error`, #2753).
pub const PROVIDER_BILLING_ERROR_STOP_REASON: &str = "provider_error:provider_billing_error";

/// The typed class of a provider authentication or account failure (#2753).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthClass {
    /// The CLI holds no login (or an expired one): the host must log in.
    LoginRequired,
    /// The provider rejected a credential (HTTP 401, revoked token, org or
    /// account policy).
    AuthInvalid,
    /// The account has a billing fault.
    Billing,
}

impl ProviderAuthClass {
    /// The closed stop reason this class carries.
    #[must_use]
    pub const fn stop_reason(self) -> &'static str {
        match self {
            Self::LoginRequired => PROVIDER_LOGIN_REQUIRED_STOP_REASON,
            Self::AuthInvalid => PROVIDER_AUTH_INVALID_STOP_REASON,
            Self::Billing => PROVIDER_BILLING_ERROR_STOP_REASON,
        }
    }
}

/// Whether a provider's terminal diagnostic text says the CLI holds no login:
/// Claude `Not logged in · Please run /login`, the documented `Login expired ·
/// Please run /login`, and the Codex `Missing bearer or basic authentication`
/// a logged-out `codex exec` ends on (#2753).
pub fn is_provider_login_required_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "not logged in",
        "please run /login",
        "login expired",
        "missing bearer or basic authentication",
        "provider_login_required",
    ]
    .iter()
    .any(|term| lower.contains(term))
}

/// Whether a provider's terminal diagnostic text reports a rejected credential
/// (HTTP 401 / invalidated token) rather than a missing login.
fn is_provider_auth_invalid_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["unauthorized (401)", "status 401", "401 unauthorized"]
        .iter()
        .any(|term| lower.contains(term))
        || lower.contains("invalidated oauth token")
        || lower.contains("provider_auth_invalid")
}

/// Classify authentication text: a missing login wins over the 401 it often
/// rides on (a logged-out Codex reports `status 401 ... Missing bearer`).
pub fn classify_provider_auth_text(text: &str) -> Option<ProviderAuthClass> {
    if is_provider_login_required_text(text) {
        Some(ProviderAuthClass::LoginRequired)
    } else if is_provider_auth_invalid_text(text) {
        Some(ProviderAuthClass::AuthInvalid)
    } else {
        None
    }
}

/// Classify the `error` of a Claude `system/api_retry` event (#2753).
pub fn classify_api_retry_error(error: &str) -> Option<ProviderAuthClass> {
    match error.trim().to_ascii_lowercase().as_str() {
        "authentication_failed" => Some(ProviderAuthClass::LoginRequired),
        "oauth_org_not_allowed" | "account_on_hold" => Some(ProviderAuthClass::AuthInvalid),
        "billing_error" => Some(ProviderAuthClass::Billing),
        _ => None,
    }
}

/// Whether a provider's terminal diagnostic text reports an authentication
/// failure (a rejected credential or a missing login) rather than a task or
/// quota error.
pub fn is_provider_auth_failure_text(text: &str) -> bool {
    is_provider_login_required_text(text) || is_provider_auth_invalid_text(text)
}

/// Stable code of a launch refused because the provider withdrew the model
/// (#2735): the typed `model_unavailable` class.
pub const MODEL_UNAVAILABLE_CODE: &str = "model_unavailable";

/// Prefix of the closed stop reason a session carries when the provider refused
/// its model as unsupported or unavailable; the model name follows (possibly
/// empty when the provider text named none, then `sessions.model` is the
/// model). Retry and capacity consumers match on it exactly.
pub const MODEL_UNAVAILABLE_STOP_REASON_PREFIX: &str = "provider_error:model_unavailable:";

/// The older, bare Claude Code stop reason for `unrecognized_model` (#1484).
/// The model is the session's own `model`.
pub const CLAUDE_MODEL_UNRECOGNIZED_STOP_REASON: &str = "provider_model_unrecognized";

/// Closed stop reason of a Claude CLI run that stopped at its `--max-budget-usd`
/// cap (#2762): the result's `subtype` is `error_max_budget_usd` and its
/// `terminal_reason` `budget_exhausted` (both read from Claude Code 2.1.301's
/// result schema; the session is not retried, a retry would spend again).
pub const CLAUDE_BUDGET_EXCEEDED_STOP_REASON: &str = "provider_budget_exceeded";

/// Closed stop reason of a Harness session the OpenRouter spend ceiling ended
/// at a model call (#3154). Not retried automatically: a retry would spend
/// again past the line the operator set.
pub const OPENROUTER_SPEND_CAP_STOP_REASON: &str =
    rsi_common::openrouter_spend::SPEND_CAP_REACHED_CODE;

/// How long a model stays unavailable after its last refusal when no later
/// launch of it got past startup: after this one probe launch is admitted
/// again, so a restored model heals without an operator.
pub const MODEL_UNAVAILABLE_TTL_SECS: i64 = 6 * 60 * 60;

/// Longest model name a stop reason carries.
const MODEL_UNAVAILABLE_MAX_MODEL_CHARS: usize = 128;

/// Detect a provider refusal that names the selected model as unsupported or
/// unavailable (the Codex 400 `The '<model>' model is not supported when using
/// Codex with a ChatGPT account`, an OpenAI `model_not_found`). Returns the
/// model the text names, empty when it names none. Auth, quota and capacity
/// text never match.
pub fn model_unavailable_in_text(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if is_provider_auth_failure_text(&lower) {
        return None;
    }
    let supported = "model is not supported when using";
    if let Some(at) = lower.find(supported) {
        return Some(quoted_token_before(&text[..at]).unwrap_or_default());
    }
    for marker in [
        "model_not_found",
        "does not exist or you do not have access",
    ] {
        if lower.contains(marker) {
            let named = lower
                .find("the model ")
                .and_then(|at| quoted_token_after(&text[at + "the model ".len()..]));
            return Some(named.unwrap_or_default());
        }
    }
    None
}

fn is_quote(c: char) -> bool {
    matches!(c, '\'' | '"' | '`')
}

/// The quoted token that ends `text` (ignoring trailing spaces), if any.
fn quoted_token_before(text: &str) -> Option<String> {
    let text = text.trim_end();
    let close = text.chars().last().filter(|c| is_quote(*c))?;
    let body = &text[..text.len() - close.len_utf8()];
    let open = body.rfind(close)?;
    clean_model_token(&body[open + close.len_utf8()..])
}

/// The quoted token that starts `text`, if any.
fn quoted_token_after(text: &str) -> Option<String> {
    let open = text.chars().next().filter(|c| is_quote(*c))?;
    let body = &text[open.len_utf8()..];
    let close = body.find(open)?;
    clean_model_token(&body[..close])
}

fn clean_model_token(token: &str) -> Option<String> {
    let token = token.trim().trim_matches('\\');
    let valid = !token.is_empty()
        && token.chars().count() <= MODEL_UNAVAILABLE_MAX_MODEL_CHARS
        && token.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '[' | ']')
        });
    valid.then(|| token.to_string())
}

/// The closed stop reason for a refused model.
pub fn model_unavailable_stop_reason(model: &str) -> String {
    let model = clean_model_token(model).unwrap_or_default();
    format!("{MODEL_UNAVAILABLE_STOP_REASON_PREFIX}{model}")
}

/// Whether a stop reason is the typed `model_unavailable` class; the inner
/// option is the model the reason names (`None` when it names none, then the
/// session's own model applies).
pub fn model_unavailable_stop_reason_model(reason: &str) -> Option<Option<String>> {
    if reason == CLAUDE_MODEL_UNRECOGNIZED_STOP_REASON {
        return Some(None);
    }
    let model = reason.strip_prefix(MODEL_UNAVAILABLE_STOP_REASON_PREFIX)?;
    Some(Some(model.trim().to_string()).filter(|model| !model.is_empty()))
}

/// Whether a session `stop_reason` reports a transient provider capacity
/// failure (model "at capacity", HTTP 503, overloaded, service unavailable)
/// rather than an auth, invalid-model, quota or task error (#1930). Matches
/// the bounded `provider_error:<provider>:<detail>` text and never the closed
/// identities (`codex_usage_limit`, `provider_auth_invalid`, ...).
pub fn is_provider_capacity_stop_reason(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    let Some(detail) = lower.strip_prefix("provider_error:") else {
        return false;
    };
    if is_provider_auth_failure_text(detail)
        || [
            "usage_limit",
            "usage limit",
            "quota",
            "credit",
            "invalid model",
            "invalid_model",
            "model not found",
            "does not exist",
            "unsupported model",
        ]
        .iter()
        .any(|term| detail.contains(term))
    {
        return false;
    }
    [
        "at capacity",
        "(503)",
        " 503",
        "503 ",
        "service unavailable",
        "overloaded",
    ]
    .iter()
    .any(|term| detail.contains(term))
}

pub fn codex_reasoning_effort(effort: Option<&str>) -> Option<&'static str> {
    match effort {
        Some("low") => Some("low"),
        Some("medium") => Some("medium"),
        Some("high") => Some("high"),
        Some("xhigh") => Some("xhigh"),
        Some("max") => Some("max"),
        Some("ultra") => Some("ultra"),
        _ => None,
    }
}

/// Map an effort string onto the value the Claude CLI accepts, or `None` when
/// the CLI would reject it.
///
/// The accepted set is `low`, `medium`, `high`, `xhigh`, `max` (observed from
/// `claude --help`, CLI 2.1.259). Deliberately exact-match: the CLI is
/// case-sensitive, so a near-miss is a miss.
pub fn claude_effort_level(effort: Option<&str>) -> Option<&'static str> {
    match effort {
        Some("low") => Some("low"),
        Some("medium") => Some("medium"),
        Some("high") => Some("high"),
        Some("xhigh") => Some("xhigh"),
        Some("max") => Some("max"),
        _ => None,
    }
}

/// The Claude CLI's live effort ladder for a model id or alias: what its
/// `initialize` reply listed in `supportedEffortLevels`, or `None` when no
/// probe described the model. The daemon owns the probe and its cache
/// (`rsid::claude::model_discovery`), so it installs this source at start
/// ([`install_claude_effort_ladder_source`]); the store cannot depend on it.
pub type ClaudeEffortLadderSource = fn(&str) -> Option<Vec<String>>;

static CLAUDE_EFFORT_LADDER_SOURCE: std::sync::OnceLock<ClaudeEffortLadderSource> =
    std::sync::OnceLock::new();

/// #289: install the daemon's live Claude ladder lookup once per process. A
/// second install is ignored; a process that installs none (a test, a tool)
/// keeps the compiled `low..max` list.
pub fn install_claude_effort_ladder_source(source: ClaudeEffortLadderSource) {
    let _ = CLAUDE_EFFORT_LADDER_SOURCE.set(source);
}

/// The installed live ladder for `model`, or `None` (no source, or the model
/// is undescribed).
pub fn live_claude_effort_ladder(model: &str) -> Option<Vec<String>> {
    let source = CLAUDE_EFFORT_LADDER_SOURCE.get()?;
    source(model)
}

/// #289: whether `effort` is valid for a Claude model, the same way a launch
/// judges it (#2768, `rsid::claude::validated_claude_effort`): a known live
/// `ladder` is the whole truth (`ultracode` only when listed, a level it omits
/// refused); an unknown one falls back to the compiled [`claude_effort_level`].
pub fn claude_effort_valid(effort: &str, ladder: Option<&[String]>) -> bool {
    let Some(ladder) = ladder else {
        return claude_effort_level(Some(effort)).is_some();
    };
    ladder.iter().any(|level| level == effort)
}

pub const OPENROUTER_DEFAULT_MODEL: &str = "openai/gpt-5.2";

pub const PIONEER_DEFAULT_MODEL: &str = "claude-sonnet-5";

pub const PIONEER_RETIRED_AUTO_MODEL: &str = "pioneer/auto";

#[must_use]
pub fn pioneer_launch_model(model: Option<&str>) -> &str {
    match model {
        None | Some(PIONEER_RETIRED_AUTO_MODEL) => PIONEER_DEFAULT_MODEL,
        Some(model) => model,
    }
}

/// Whether `model` is an Anthropic adaptive-thinking model (moved down from
/// `session::harness::providers::anthropic`, #1021 S3a).
pub fn uses_adaptive_thinking(model: &str) -> bool {
    // Bedrock IDs (`us.anthropic.claude-opus-5-5-v1:0`) name the same models.
    let model = crate::bedrock::anthropic_model_name(model).unwrap_or(model);
    // Entries are matched as prefixes, so `claude-fable-5` covers the
    // offered `claude-fable-5-1` and any later Fable 5.x alongside the
    // retired bare id — both reject `temperature`/`budget_tokens`.
    [
        "claude-fable-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-5",
    ]
    .iter()
    .any(|prefix| {
        model == *prefix
            || model
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}

#[cfg(test)]
mod model_unavailable_tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn the_codex_chatgpt_account_refusal_classifies_as_model_unavailable_naming_the_model() {
        // The shapes the Codex CLI reports: the bare message, and the JSON body
        // it is wrapped in (double quotes arrive escaped).
        for text in [
            "The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.",
            "{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-6-astra' model is not supported when using Codex with a ChatGPT account.\"}}",
            "The \\\"gpt-6-astra\\\" model is not supported when using Codex with a ChatGPT account.",
        ] {
            let model = model_unavailable_in_text(text).expect(text);
            assert!(
                model == "gpt-6.1-sol" || model == "gpt-6-astra",
                "{text} -> {model}"
            );
        }
        assert_eq!(
            model_unavailable_stop_reason("gpt-6.1-sol"),
            "provider_error:model_unavailable:gpt-6.1-sol"
        );
        assert_eq!(
            model_unavailable_stop_reason_model("provider_error:model_unavailable:gpt-6.1-sol"),
            Some(Some("gpt-6.1-sol".to_string()))
        );
        // A text that names no model still classifies; the session's model applies.
        assert_eq!(
            model_unavailable_in_text("This model is not supported when using Codex"),
            Some(String::new())
        );
        assert_eq!(
            model_unavailable_stop_reason_model("provider_error:model_unavailable:"),
            Some(None)
        );
        assert_eq!(
            model_unavailable_stop_reason_model(CLAUDE_MODEL_UNRECOGNIZED_STOP_REASON),
            Some(None)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn auth_quota_and_capacity_text_is_not_model_unavailable() {
        for text in [
            "unexpected status 401 Unauthorized: Incorrect API key",
            "unexpected status 429 Too Many Requests",
            "Selected model is at capacity. Please try a different model.",
            "You've hit your usage limit.",
            "invalid model",
        ] {
            assert_eq!(model_unavailable_in_text(text), None, "{text}");
        }
        for reason in [
            "provider_error:codex_usage_limit",
            "provider_error:provider_auth_invalid",
            "provider_error:unclassified",
            "provider_error:codex:invalid model",
        ] {
            assert_eq!(
                model_unavailable_stop_reason_model(reason),
                None,
                "{reason}"
            );
        }
    }
}

#[cfg(test)]
mod provider_auth_class_tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn observed_and_documented_login_texts_are_login_required() {
        for text in [
            // Observed: a logged-out `claude -p`.
            "Not logged in \u{b7} Please run /login",
            // Documented on expiry (CC-AUTH).
            "Login expired \u{b7} Please run /login",
            // Observed: a logged-out `codex exec` ends on this, usually riding
            // a 401, and the missing login wins.
            "Missing bearer or basic authentication in header",
            "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header",
            "provider_login_required",
        ] {
            assert_eq!(
                classify_provider_auth_text(text),
                Some(ProviderAuthClass::LoginRequired),
                "{text}"
            );
            assert!(is_provider_auth_failure_text(text), "{text}");
        }
        assert_eq!(
            ProviderAuthClass::LoginRequired.stop_reason(),
            "provider_error:provider_login_required"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_rejected_credential_stays_provider_auth_invalid() {
        for text in [
            "unexpected status 401 Unauthorized: Incorrect API key",
            "workspace routing discovery unauthorized (401)",
            "invalidated oauth token",
            "provider_auth_invalid",
        ] {
            assert_eq!(
                classify_provider_auth_text(text),
                Some(ProviderAuthClass::AuthInvalid),
                "{text}"
            );
        }
        assert_eq!(
            ProviderAuthClass::AuthInvalid.stop_reason(),
            PROVIDER_AUTH_INVALID_STOP_REASON
        );
        for text in [
            "unexpected status 429 Too Many Requests",
            "Selected model is at capacity.",
        ] {
            assert_eq!(classify_provider_auth_text(text), None, "{text}");
            assert!(!is_provider_auth_failure_text(text), "{text}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn api_retry_errors_map_to_login_credential_and_billing_classes() {
        for (error, class) in [
            ("authentication_failed", ProviderAuthClass::LoginRequired),
            ("oauth_org_not_allowed", ProviderAuthClass::AuthInvalid),
            ("account_on_hold", ProviderAuthClass::AuthInvalid),
            ("billing_error", ProviderAuthClass::Billing),
        ] {
            assert_eq!(classify_api_retry_error(error), Some(class), "{error}");
        }
        assert_eq!(classify_api_retry_error("rate_limit"), None);
        assert_eq!(
            ProviderAuthClass::Billing.stop_reason(),
            "provider_error:provider_billing_error"
        );
        // The login class is never a capacity or model refusal.
        assert!(!is_provider_capacity_stop_reason(
            PROVIDER_LOGIN_REQUIRED_STOP_REASON
        ));
        assert_eq!(
            model_unavailable_in_text("Not logged in \u{b7} Please run /login"),
            None
        );
    }
}
