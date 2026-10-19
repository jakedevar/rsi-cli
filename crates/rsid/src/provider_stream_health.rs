//! #2756: typed evidence from a Claude session's `system/init` health fields
//! and its `system/api_retry` events.
//!
//! The Claude CLI skips a `--mcp-config` server that fails to start and still
//! "exits cleanly", so a worker whose `rsi-agent` server never connected has no
//! `rsi_control_*` tools and cannot hand off, yet looks healthy. Its `init`
//! event states the truth (`mcp_servers[]{name,status}`, `mcp_server_errors`,
//! `plugin_errors`) and `system/api_retry` names the failure category of every
//! retried API call. This module parses both from the stream events the
//! monitor already reads (no process, no probe) into visible `System` rows and
//! manager-notice requests. Only server and plugin NAMES leave the event:
//! error text and configuration are never echoed.

use crate::claude::{AGENT_MCP_SERVER_NAME, StreamEvent};
use rsi_common::types::{ConversationEvent, EventType, PROVIDER_HEALTH_METADATA_KEY};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Session event and manager-notice kind: the `rsi-agent` MCP server is
/// absent from a tokened worker's init or not `connected`.
pub(crate) const AGENT_MCP_UNAVAILABLE: &str = "agent_mcp_unavailable";
/// Session event and manager-notice kind: a plugin failed to load.
pub(crate) const PLUGIN_LOAD_FAILED: &str = "plugin_load_failed";
/// Session event kind: an explicit-delivery launch (#2767) whose init does not
/// list the delivered `rsi-skills` plugin.
pub(crate) const SKILL_PLUGIN_MISSING: &str = "skill_plugin_missing";
/// Session event kind: an MCP server other than `rsi-agent` reported an error.
pub(crate) const MCP_SERVER_ERROR: &str = "mcp_server_error";
/// Session event kind and histogram class of an `api_retry.error` string this
/// build does not know; counted like any other class.
pub(crate) const API_RETRY_UNKNOWN: &str = "api_retry_unknown";
/// Session event kind of a `system/api_retry` category report.
pub(crate) const API_RETRY: &str = "api_retry";
/// Session event kind of a `--fallback-model` that served part of a run (#2762).
pub(crate) const MODEL_FALLBACK_USED: &str = "model_fallback_used";
/// Session event kind of a Harness `provider_error` stream event (hub #2913):
/// the typed, closed-class record of a model call the provider refused.
pub(crate) const PROVIDER_ERROR: &str = "provider_error";
/// Class recorded when a `provider_error` names a class this build does not know.
const PROVIDER_ERROR_UNKNOWN_CLASS: &str = "unknown";

/// Longest server or plugin name carried into an event or notice.
const MAX_NAME_BYTES: usize = 64;
/// Most names one finding carries.
const MAX_NAMES: usize = 16;

/// The documented `system/api_retry.error` categories (Claude Code headless
/// reference), plus [`ApiRetryClass::Unknown`] for any other string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiRetryClass {
    AuthenticationFailed,
    OauthOrgNotAllowed,
    AccountOnHold,
    BillingError,
    RateLimit,
    Overloaded,
    InvalidRequest,
    ModelNotFound,
    ServerError,
    MaxOutputTokens,
    CloudCredentialError,
    Unknown,
}

impl ApiRetryClass {
    /// The class of an `error` string (ASCII case-insensitive).
    pub(crate) fn classify(error: &str) -> Self {
        match error.trim().to_ascii_lowercase().as_str() {
            "authentication_failed" => Self::AuthenticationFailed,
            "oauth_org_not_allowed" => Self::OauthOrgNotAllowed,
            "account_on_hold" => Self::AccountOnHold,
            "billing_error" => Self::BillingError,
            "rate_limit" => Self::RateLimit,
            "overloaded" => Self::Overloaded,
            "invalid_request" => Self::InvalidRequest,
            "model_not_found" => Self::ModelNotFound,
            "server_error" => Self::ServerError,
            "max_output_tokens" => Self::MaxOutputTokens,
            "cloud_credential_error" => Self::CloudCredentialError,
            _ => Self::Unknown,
        }
    }

    /// The class name used in events and the histogram.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::AuthenticationFailed => "authentication_failed",
            Self::OauthOrgNotAllowed => "oauth_org_not_allowed",
            Self::AccountOnHold => "account_on_hold",
            Self::BillingError => "billing_error",
            Self::RateLimit => "rate_limit",
            Self::Overloaded => "overloaded",
            Self::InvalidRequest => "invalid_request",
            Self::ModelNotFound => "model_not_found",
            Self::ServerError => "server_error",
            Self::MaxOutputTokens => "max_output_tokens",
            Self::CloudCredentialError => "cloud_credential_error",
            Self::Unknown => API_RETRY_UNKNOWN,
        }
    }
}

/// A name that is safe to carry: short and made of identifier characters, so a
/// message or a configuration fragment is never mistaken for one.
fn safe_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '/')))
    .then(|| name.to_string())
}

/// The names an `mcp_server_errors` or `plugin_errors` array refers to. An
/// item is an object (its `name`, `server`, `server_name`, `plugin`,
/// `plugin_name` or `id`) or a string (the token before the first `:` or
/// space). Anything else an item says (messages, commands, arguments) is
/// dropped; an item with no safe name counts as `unnamed`.
fn error_names(items: Option<&Value>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in items.and_then(Value::as_array).into_iter().flatten() {
        let name = match item {
            Value::String(text) => text
                .split(|c: char| c == ':' || c.is_whitespace())
                .next()
                .and_then(safe_name),
            Value::Object(map) => [
                "name",
                "server",
                "server_name",
                "plugin",
                "plugin_name",
                "id",
            ]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str).and_then(safe_name)),
            _ => None,
        }
        .unwrap_or_else(|| "unnamed".to_string());
        if !names.contains(&name) && names.len() < MAX_NAMES {
            names.push(name);
        }
    }
    names
}

/// What a tokened worker's `rsi-agent` MCP server looks like at init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentMcpState {
    /// The init does not list servers at all (an older CLI): nothing to judge.
    Unknown,
    Connected,
    /// Listed with a status that is still settling.
    Pending,
    /// Missing from the list (`status: None`) or listed with a status other
    /// than `connected` (`failed`, `needs-auth`, ...).
    Unavailable {
        status: Option<String>,
    },
}

/// The health fields of one `system/init` event.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct InitHealth {
    /// `None` when the event has no `mcp_servers` array.
    mcp_servers: Option<Vec<(String, String)>>,
    mcp_server_errors: Vec<String>,
    plugin_errors: Vec<String>,
    /// What `plugins` and `plugin_errors` say about the plugin an explicit
    /// skills delivery (#2767) passes.
    skill_plugin: rsi_common::provider_skill_delivery::PluginInitVerdict,
}

impl InitHealth {
    /// Parse a `system/init` event, `None` for any other event.
    pub(crate) fn parse(stream_event: &StreamEvent) -> Option<Self> {
        if !is_system_subtype(stream_event, "init") {
            return None;
        }
        let data = &stream_event.data;
        let mcp_servers = data
            .get("mcp_servers")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("name").and_then(Value::as_str)?;
                        let status = item
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        Some((name.trim().to_string(), status.trim().to_ascii_lowercase()))
                    })
                    .collect()
            });
        Some(Self {
            mcp_servers,
            mcp_server_errors: error_names(data.get("mcp_server_errors")),
            plugin_errors: error_names(data.get("plugin_errors")),
            skill_plugin: rsi_common::provider_skill_delivery::plugin_init_verdict(data),
        })
    }

    /// The state of the `rsi-agent` server.
    pub(crate) fn agent_mcp_state(&self) -> AgentMcpState {
        let Some(servers) = &self.mcp_servers else {
            return AgentMcpState::Unknown;
        };
        match servers
            .iter()
            .find(|(name, _)| name == AGENT_MCP_SERVER_NAME)
        {
            None => AgentMcpState::Unavailable { status: None },
            Some((_, status)) if status == "connected" => AgentMcpState::Connected,
            Some((_, status)) if status == "pending" => AgentMcpState::Pending,
            Some((_, status)) => AgentMcpState::Unavailable {
                status: Some(safe_name(status).unwrap_or_else(|| "unknown".to_string())),
            },
        }
    }
}

/// One typed finding of an init event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HealthFinding {
    pub(crate) kind: &'static str,
    pub(crate) names: Vec<String>,
    pub(crate) content: String,
    pub(crate) metadata: Value,
    /// Whether the project manager is told (a manager notice); every finding
    /// is also a session event.
    pub(crate) notify_manager: bool,
}

/// The findings of an init event. `expects_agent_mcp` is whether the session
/// was launched with the `rsi-agent` MCP server (it holds an authority token);
/// a session that never asked for the server is not judged on it.
///
/// `expects_skill_plugin` is whether the launch delivered the `rsi-skills`
/// plugin (#2767); a missing plugin with no `plugin_errors` entry is then a
/// session row (a plugin that failed to load is `plugin_load_failed`).
pub(crate) fn init_findings(
    health: &InitHealth,
    expects_agent_mcp: bool,
    expects_skill_plugin: bool,
) -> Vec<HealthFinding> {
    let mut findings = Vec::new();
    if expects_agent_mcp {
        let (status, settled) = match health.agent_mcp_state() {
            AgentMcpState::Unavailable { status } => (Some(status), true),
            AgentMcpState::Pending => (Some(Some("pending".to_string())), false),
            AgentMcpState::Connected | AgentMcpState::Unknown => (None, false),
        };
        if let Some(status) = status {
            let shown = status.as_deref().unwrap_or("absent");
            findings.push(HealthFinding {
                kind: AGENT_MCP_UNAVAILABLE,
                names: vec![AGENT_MCP_SERVER_NAME.to_string()],
                content: format!(
                    "[claude] {AGENT_MCP_UNAVAILABLE}: the {AGENT_MCP_SERVER_NAME} MCP server is \
                     not connected (status: {shown}); this worker has no rsi_control_* tools and \
                     cannot hand off"
                ),
                metadata: json!({
                    "kind": AGENT_MCP_UNAVAILABLE,
                    "server": AGENT_MCP_SERVER_NAME,
                    "status": shown,
                }),
                // A pending server may still connect: the session row is
                // evidence enough until it does not.
                notify_manager: settled,
            });
        }
    }
    let other_servers: Vec<String> = health
        .mcp_server_errors
        .iter()
        .filter(|name| name.as_str() != AGENT_MCP_SERVER_NAME)
        .cloned()
        .collect();
    if !other_servers.is_empty() {
        findings.push(HealthFinding {
            kind: MCP_SERVER_ERROR,
            content: format!(
                "[claude] {MCP_SERVER_ERROR}: MCP server(s) reported errors: {}",
                other_servers.join(", ")
            ),
            metadata: json!({"kind": MCP_SERVER_ERROR, "servers": other_servers}),
            names: other_servers,
            notify_manager: false,
        });
    }
    if !health.plugin_errors.is_empty() {
        findings.push(HealthFinding {
            kind: PLUGIN_LOAD_FAILED,
            content: format!(
                "[claude] {PLUGIN_LOAD_FAILED}: plugin(s) failed to load: {}",
                health.plugin_errors.join(", ")
            ),
            metadata: json!({"kind": PLUGIN_LOAD_FAILED, "plugins": health.plugin_errors}),
            names: health.plugin_errors.clone(),
            notify_manager: true,
        });
    }
    if expects_skill_plugin
        && health.skill_plugin == rsi_common::provider_skill_delivery::PluginInitVerdict::Missing
    {
        let plugin = rsi_common::provider_skill_delivery::PLUGIN_NAME;
        findings.push(HealthFinding {
            kind: SKILL_PLUGIN_MISSING,
            names: vec![plugin.to_string()],
            content: format!(
                "[claude] {SKILL_PLUGIN_MISSING}: the {plugin} plugin this launch delivered \
                 (provider_skill_delivery=explicit) is not listed in the session init"
            ),
            metadata: json!({"kind": SKILL_PLUGIN_MISSING, "plugin": plugin}),
            notify_manager: false,
        });
    }
    findings
}

fn is_system_subtype(stream_event: &StreamEvent, subtype: &str) -> bool {
    stream_event.event_type == "system"
        && stream_event.data.get("subtype").and_then(Value::as_str) == Some(subtype)
}

/// Whether an event is a Claude `system/init`.
pub(crate) fn is_init_event(stream_event: &StreamEvent) -> bool {
    is_system_subtype(stream_event, "init")
}

/// The retry notice of a `system/api_retry` event (a top-level `api_retry`
/// type is accepted too), `None` for any other event.
fn parse_api_retry(stream_event: &StreamEvent) -> Option<(ApiRetryClass, Option<String>, Value)> {
    let is_retry =
        is_system_subtype(stream_event, "api_retry") || stream_event.event_type == "api_retry";
    if !is_retry {
        return None;
    }
    let data = &stream_event.data;
    let error = data
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let class = ApiRetryClass::classify(error);
    // An unknown string is kept (bounded, identifier characters only) so the
    // next category can be added from evidence.
    let raw = if class == ApiRetryClass::Unknown {
        safe_name(error)
    } else {
        None
    };
    let mut detail = serde_json::Map::new();
    for key in ["attempt", "max_retries", "retry_delay_ms", "error_status"] {
        if let Some(number) = data.get(key).and_then(Value::as_u64) {
            detail.insert(key.to_string(), json!(number));
        }
    }
    Some((class, raw, Value::Object(detail)))
}

/// The closed `class` values of `rsid_harness::errors::ProviderErrorClass`
/// (snake_case on the wire); a `provider_error` event carries one of them.
const PROVIDER_ERROR_CLASSES: &[&str] = &[
    "auth",
    "credit_exhausted",
    "rate_limited",
    "overloaded",
    "transient",
    "context_too_long",
    "bad_request",
];

/// The typed record of a Harness `provider_error` event (hub #2913), `None`
/// for any other event. Only the closed class, the numeric status and delay and
/// a bounded identifier-shaped `detail_code` are carried, never free text.
fn parse_provider_error(stream_event: &StreamEvent) -> Option<(String, Value)> {
    if stream_event.event_type != PROVIDER_ERROR {
        return None;
    }
    let data = &stream_event.data;
    let class = data
        .get("class")
        .and_then(Value::as_str)
        .filter(|class| PROVIDER_ERROR_CLASSES.contains(class))
        .unwrap_or(PROVIDER_ERROR_UNKNOWN_CLASS);
    let mut detail = serde_json::Map::new();
    for key in ["http_status", "retry_after_ms"] {
        if let Some(number) = data.get(key).and_then(Value::as_u64) {
            detail.insert(key.to_string(), json!(number));
        }
    }
    if let Some(code) = data
        .get("detail_code")
        .and_then(Value::as_str)
        .and_then(safe_name)
    {
        detail.insert("detail_code".to_string(), json!(code));
    }
    Some((class.to_string(), Value::Object(detail)))
}

/// What one stream event produced.
#[derive(Debug, Default)]
pub(crate) struct Observation {
    /// Visible `System` rows to persist, already sequenced.
    pub(crate) events: Vec<ConversationEvent>,
    /// Findings that also go to the project manager.
    pub(crate) notices: Vec<HealthFinding>,
}

/// Per-session state: the init is judged once, the retry categories are
/// counted for the whole run.
#[derive(Debug, Default)]
pub(crate) struct StreamHealth {
    init_judged: bool,
    retries: BTreeMap<&'static str, u64>,
    /// The last fallback reported, so a later `result` that still lists the
    /// same models in its cumulative `modelUsage` adds no second row.
    fallback_reported: Option<crate::monitor::ModelFallback>,
    /// Whether this run's launch delivered the `rsi-skills` plugin (#2767).
    expects_skill_plugin: bool,
}

impl StreamHealth {
    /// Record that the launch delivered the `rsi-skills` plugin, so the init is
    /// judged on listing it. Set before the init event is observed.
    pub(crate) fn expect_skill_plugin(&mut self, expected: bool) {
        self.expects_skill_plugin = expected;
    }

    /// Whether [`Self::observe`] needs `expects_agent_mcp` for this event.
    pub(crate) fn wants_expectation(&self, stream_event: &StreamEvent) -> bool {
        !self.init_judged && is_init_event(stream_event)
    }

    /// The per-class retry counts so far.
    #[cfg(test)]
    pub(crate) fn retry_histogram(&self) -> &BTreeMap<&'static str, u64> {
        &self.retries
    }

    /// #2762: the typed `model_fallback_used{from,to}` row for a fallback a
    /// `result` reports, once per distinct pair per run. `None` when none ran
    /// or this pair was already reported.
    pub(crate) fn observe_model_fallback(
        &mut self,
        fallback: Option<crate::monitor::ModelFallback>,
        session_id: Uuid,
        sequence: &mut i32,
    ) -> Option<ConversationEvent> {
        let fallback = fallback?;
        if self.fallback_reported.as_ref() == Some(&fallback) {
            return None;
        }
        let event = health_event(
            session_id,
            sequence,
            format!(
                "[claude] model fallback used: {} -> {}",
                fallback.from, fallback.to
            ),
            json!({
                "kind": MODEL_FALLBACK_USED,
                "from": fallback.from,
                "to": fallback.to,
            }),
        );
        self.fallback_reported = Some(fallback);
        Some(event)
    }

    /// Read one stream event. `expects_agent_mcp` is consulted for an init only.
    pub(crate) fn observe(
        &mut self,
        stream_event: &StreamEvent,
        session_id: Uuid,
        sequence: &mut i32,
        expects_agent_mcp: bool,
    ) -> Observation {
        let mut observation = Observation::default();
        if self.wants_expectation(stream_event) {
            self.init_judged = true;
            if let Some(health) = InitHealth::parse(stream_event) {
                for finding in init_findings(&health, expects_agent_mcp, self.expects_skill_plugin)
                {
                    observation.events.push(health_event(
                        session_id,
                        sequence,
                        finding.content.clone(),
                        finding.metadata.clone(),
                    ));
                    if finding.notify_manager {
                        observation.notices.push(finding);
                    }
                }
            }
        } else if let Some((class, detail)) = parse_provider_error(stream_event) {
            let status = detail
                .get("http_status")
                .map(|status| format!(" (HTTP {status})"))
                .unwrap_or_default();
            observation.events.push(health_event(
                session_id,
                sequence,
                format!("[harness] provider error: {class}{status}"),
                json!({
                    "kind": PROVIDER_ERROR,
                    "source": "stream",
                    "class": class,
                    "detail": detail,
                }),
            ));
        } else if let Some((class, raw, detail)) = parse_api_retry(stream_event) {
            let count = self.retries.entry(class.name()).or_insert(0);
            *count += 1;
            let count = *count;
            // One row on the first sighting of a class and then each time its
            // count doubles: the transcript stays short under a retry storm
            // and the latest row carries the whole histogram.
            if count.is_power_of_two() {
                let content = format!(
                    "[claude] API retry: {} (seen {count} time(s) this session)",
                    class.name()
                );
                observation.events.push(health_event(
                    session_id,
                    sequence,
                    content,
                    json!({
                        "kind": API_RETRY,
                        "class": class.name(),
                        "count": count,
                        "raw": raw,
                        "detail": detail,
                        "histogram": self.retries,
                    }),
                ));
            }
        }
        observation
    }
}

/// Longest override text shown in a launch-overrides row's `content`; the
/// metadata keeps every entry whole.
const LAUNCH_OVERRIDES_CONTENT_CHARS: usize = 300;

/// hub #2765: the session record of the operator launch overrides one run
/// applied (`metadata.provider_health.kind = "launch_overrides"`, with the
/// provider, the canonical `key=value` entries and the exact CLI arguments).
/// Written on the run's first event, only when something was applied.
pub(crate) fn launch_overrides_event(
    session_id: Uuid,
    sequence: &mut i32,
    applied: &crate::provider_launch_overrides::AppliedLaunchOverrides,
) -> ConversationEvent {
    let joined = applied.entries.join(" ");
    let mut shown: String = joined
        .chars()
        .take(LAUNCH_OVERRIDES_CONTENT_CHARS)
        .collect();
    if shown.len() < joined.len() {
        shown.push_str("...");
    }
    health_event(
        session_id,
        sequence,
        format!(
            "Operator launch overrides applied ({}): {shown}",
            applied.provider.name()
        ),
        json!({
            "kind": "launch_overrides",
            "provider": applied.provider.name(),
            "entries": applied.entries,
            "args": applied.args,
        }),
    )
}

/// #2758: the visible `System` row that shows an operator the effective
/// sandbox / permission mode a policy-bearing CLI session runs under. Written
/// once per monitor run, from the session's resolved policy.
pub(crate) fn access_policy_event(
    session_id: Uuid,
    sequence: &mut i32,
    provider: rsi_common::types::SessionProvider,
    mode: rsi_common::harness_tool_policy::CliAccessMode,
) -> ConversationEvent {
    health_event(
        session_id,
        sequence,
        mode.describe(provider),
        json!({"kind": "access_policy", "cli_access": mode.as_str()}),
    )
}

/// #2763: the visible `System` row that records the subagent governance a
/// Claude or Codex session was launched under. Written once per monitor run
/// and only when an operator cap is set (the defaults pass nothing, so there is
/// nothing to show); the metadata carries the typed values.
pub(crate) fn subagent_caps_event(
    session_id: Uuid,
    sequence: &mut i32,
    provider: rsi_common::types::SessionProvider,
    caps: &crate::store_support::provider_settings::SubagentCaps,
) -> Option<ConversationEvent> {
    if caps.is_default() {
        return None;
    }
    let provider_label = rsi_common::harness_tool_policy::provider_label(provider);
    Some(health_event(
        session_id,
        sequence,
        format!(
            "Subagent governance ({provider_label}): {}",
            caps.describe()
        ),
        json!({
            "kind": "subagent_caps",
            "provider": provider_label,
            "subagent_model": caps.model,
            "max_concurrent_subagents": caps.max_concurrent,
        }),
    ))
}

/// #3141: the visible `System` row that records which engine an OpenRouter
/// launch took and the operator choice (or default) behind it. Written once on
/// the run's first event; the metadata carries the typed values.
pub(crate) fn api_route_event(
    session_id: Uuid,
    sequence: &mut i32,
    selection: &crate::api_route_selection::RouteSelection,
) -> ConversationEvent {
    health_event(
        session_id,
        sequence,
        selection.describe(),
        json!({
            "kind": crate::api_route_selection::ROUTE_SELECTED_KIND,
            "provider": "openrouter",
            "engine": selection.engine,
            "configured_route": selection.configured_route,
            "source": selection.source,
            "fallback_cause": selection.fallback_cause,
        }),
    )
}

fn health_event(
    session_id: Uuid,
    sequence: &mut i32,
    content: String,
    health: Value,
) -> ConversationEvent {
    *sequence += 1;
    ConversationEvent {
        id: 0,
        session_id,
        sequence: *sequence,
        event_type: EventType::System,
        role: None,
        content,
        tool_name: None,
        tool_input: None,
        created_at: chrono::Utc::now(),
        offload_id: None,
        tool_use_id: None,
        metadata: Some(Box::new(json!({ PROVIDER_HEALTH_METADATA_KEY: health }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn wire(text: &str) -> StreamEvent {
        let data: Value = serde_json::from_str(text).expect("fixture json");
        StreamEvent {
            event_type: data["type"].as_str().expect("type").to_string(),
            data,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn init(extra: &str) -> StreamEvent {
        wire(&format!(
            r#"{{"type":"system","subtype":"init","session_id":"s",{extra}"model":"claude-opus-5"}}"#
        ))
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn kinds(observation: &Observation) -> Vec<String> {
        observation
            .events
            .iter()
            .map(|event| {
                event.metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY]["kind"]
                    .as_str()
                    .expect("kind")
                    .to_string()
            })
            .collect()
    }

    /// #2767: an explicit-delivery launch's init must list the `rsi-skills`
    /// plugin with no `plugin_errors`; a missing one is a session row, a
    /// failed one is `plugin_load_failed`, and a launch that delivered nothing
    /// is not judged.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn delivered_skill_plugin_is_judged_on_the_init_plugins_list() {
        let judge = |extra: &str, expects: bool| {
            let mut health = StreamHealth::default();
            health.expect_skill_plugin(expects);
            let mut sequence = 0;
            kinds(&health.observe(&init(extra), Uuid::nil(), &mut sequence, false))
        };
        let delivered = r#""plugins":[{"name":"rsi-skills","path":"/p"}],"plugin_errors":[],"#;
        assert!(judge(delivered, true).is_empty());
        assert_eq!(
            judge(r#""plugins":[{"name":"other"}],"#, true),
            vec![SKILL_PLUGIN_MISSING]
        );
        assert_eq!(
            judge(r#""mcp_servers":[],"#, true),
            vec![SKILL_PLUGIN_MISSING]
        );
        assert_eq!(
            judge(
                r#""plugins":[],"plugin_errors":[{"plugin":"rsi-skills","message":"boom"}],"#,
                true
            ),
            vec![PLUGIN_LOAD_FAILED]
        );
        assert!(judge(r#""plugins":[],"#, false).is_empty());
    }

    /// #2756 AC1: a failed or absent `rsi-agent` server is a session event and
    /// a manager notice; a connected one adds nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn failed_or_absent_agent_server_raises_agent_mcp_unavailable() {
        for (extra, status) in [
            (
                r#""mcp_servers":[{"name":"rsi-agent","status":"failed"}],"#,
                "failed",
            ),
            (
                r#""mcp_servers":[{"name":"rsi-agent","status":"needs-auth"}],"#,
                "needs-auth",
            ),
            (
                r#""mcp_servers":[{"name":"other","status":"connected"}],"#,
                "absent",
            ),
            (r#""mcp_servers":[],"#, "absent"),
        ] {
            let mut health = StreamHealth::default();
            let mut sequence = 6;
            let seen = health.observe(&init(extra), Uuid::nil(), &mut sequence, true);
            assert_eq!(kinds(&seen), vec![AGENT_MCP_UNAVAILABLE], "{extra}");
            assert_eq!(seen.notices.len(), 1, "{extra}");
            assert_eq!(seen.notices[0].names, vec!["rsi-agent".to_string()]);
            assert_eq!(seen.events[0].sequence, 7);
            assert!(seen.events[0].content.contains(status), "{extra}");
            assert!(seen.events[0].is_provider_health_notice());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn connected_agent_server_pending_and_unexpected_cases_add_nothing_extra() {
        let mut sequence = 0;
        let connected = init(r#""mcp_servers":[{"name":"rsi-agent","status":"connected"}],"#);
        let seen = StreamHealth::default().observe(&connected, Uuid::nil(), &mut sequence, true);
        assert!(seen.events.is_empty() && seen.notices.is_empty());
        assert_eq!(sequence, 0, "no sequence number is burned");

        // A session launched without the server is not judged on it.
        let failed = init(r#""mcp_servers":[{"name":"rsi-agent","status":"failed"}],"#);
        let seen = StreamHealth::default().observe(&failed, Uuid::nil(), &mut sequence, false);
        assert!(seen.events.is_empty() && seen.notices.is_empty());

        // An init that does not list servers (an older CLI) cannot be judged.
        let seen = StreamHealth::default().observe(&init(""), Uuid::nil(), &mut sequence, true);
        assert!(seen.events.is_empty() && seen.notices.is_empty());

        // A pending server is a session row, not yet a manager notice.
        let pending = init(r#""mcp_servers":[{"name":"rsi-agent","status":"pending"}],"#);
        let seen = StreamHealth::default().observe(&pending, Uuid::nil(), &mut sequence, true);
        assert_eq!(kinds(&seen), vec![AGENT_MCP_UNAVAILABLE]);
        assert!(seen.notices.is_empty());
    }

    /// The init is judged once per run; a re-announced init does not repeat it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_init_is_judged_once() {
        let failed = init(r#""mcp_servers":[{"name":"rsi-agent","status":"failed"}],"#);
        let mut health = StreamHealth::default();
        let mut sequence = 0;
        assert_eq!(
            health
                .observe(&failed, Uuid::nil(), &mut sequence, true)
                .events
                .len(),
            1
        );
        assert!(!health.wants_expectation(&failed));
        assert!(
            health
                .observe(&failed, Uuid::nil(), &mut sequence, true)
                .events
                .is_empty()
        );
    }

    /// #2756 AC2: errors name the server or plugin and never echo its
    /// configuration or message.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn server_and_plugin_errors_name_only_the_server_or_plugin() {
        let event = init(
            r#""mcp_servers":[{"name":"rsi-agent","status":"connected"}],
            "mcp_server_errors":[
              {"name":"files","error":"spawn failed: TOKEN=hunter2 /usr/bin/x --secret"},
              "docs: could not start npx -y docs --key=abc"],
            "plugin_errors":[{"plugin":"fmt-helper","message":"/home/u/.claude/p/config.json invalid"}],"#,
        );
        let mut health = StreamHealth::default();
        let mut sequence = 0;
        let seen = health.observe(&event, Uuid::nil(), &mut sequence, true);
        assert_eq!(kinds(&seen), vec![MCP_SERVER_ERROR, PLUGIN_LOAD_FAILED]);
        assert_eq!(
            seen.events[0].metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY]["servers"],
            json!(["files", "docs"])
        );
        assert_eq!(seen.notices.len(), 1);
        assert_eq!(seen.notices[0].kind, PLUGIN_LOAD_FAILED);
        assert_eq!(seen.notices[0].names, vec!["fmt-helper".to_string()]);
        for event in &seen.events {
            let text = format!("{} {:?}", event.content, event.metadata);
            for leaked in [
                "hunter2",
                "--secret",
                "npx",
                "--key",
                "config.json",
                "/home",
            ] {
                assert!(!text.contains(leaked), "{leaked} leaked: {text}");
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn an_error_item_with_no_safe_name_is_unnamed() {
        let event = init(r#""plugin_errors":[{"message":"boom"},"has spaces only"],"#);
        let health = InitHealth::parse(&event).expect("init");
        assert_eq!(
            health.plugin_errors,
            vec!["unnamed".to_string(), "has".to_string()]
        );
    }

    /// #2756 AC3: every documented category maps to its own class, an unknown
    /// string maps to `api_retry_unknown`, and both are counted.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn api_retry_categories_map_to_classes_and_unknown_is_counted() {
        for name in [
            "authentication_failed",
            "oauth_org_not_allowed",
            "account_on_hold",
            "billing_error",
            "rate_limit",
            "overloaded",
            "invalid_request",
            "model_not_found",
            "server_error",
            "max_output_tokens",
            "cloud_credential_error",
        ] {
            let class = ApiRetryClass::classify(name);
            assert_ne!(class, ApiRetryClass::Unknown, "{name}");
            assert_eq!(class.name(), name);
            assert_eq!(ApiRetryClass::classify(&name.to_uppercase()), class);
        }
        assert_eq!(ApiRetryClass::classify("brand_new"), ApiRetryClass::Unknown);
        assert_eq!(ApiRetryClass::Unknown.name(), API_RETRY_UNKNOWN);

        let mut health = StreamHealth::default();
        let mut sequence = 0;
        let retry = |error: &str| {
            wire(&format!(
                r#"{{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"retry_delay_ms":1000,"error_status":429,"error":"{error}"}}"#
            ))
        };
        let seen = health.observe(&retry("rate_limit"), Uuid::nil(), &mut sequence, true);
        assert_eq!(kinds(&seen), vec![API_RETRY]);
        let metadata =
            &seen.events[0].metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(metadata["class"], "rate_limit");
        assert_eq!(metadata["detail"]["error_status"], 429);
        assert_eq!(metadata["detail"]["max_retries"], 10);

        let seen = health.observe(&retry("brand_new"), Uuid::nil(), &mut sequence, true);
        let metadata =
            &seen.events[0].metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(metadata["class"], API_RETRY_UNKNOWN);
        assert_eq!(metadata["raw"], "brand_new");
        assert_eq!(metadata["histogram"][API_RETRY_UNKNOWN], 1);
        assert_eq!(health.retry_histogram()[API_RETRY_UNKNOWN], 1);

        // Rows are written at 1, 2, 4: the third and fifth retry stay silent.
        let rows: Vec<usize> = (0..5)
            .map(|_| {
                health
                    .observe(&retry("overloaded"), Uuid::nil(), &mut sequence, true)
                    .events
                    .len()
            })
            .collect();
        assert_eq!(rows, vec![1, 1, 0, 1, 0]);
        assert_eq!(health.retry_histogram()["overloaded"], 5);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn only_api_retry_events_are_read_as_retries() {
        let mut health = StreamHealth::default();
        let mut sequence = 0;
        for text in [
            r#"{"type":"system","subtype":"status","error":"rate_limit"}"#,
            r#"{"type":"assistant","error":"rate_limit"}"#,
        ] {
            let seen = health.observe(&wire(text), Uuid::nil(), &mut sequence, true);
            assert!(seen.events.is_empty(), "{text}");
        }
        let top_level = wire(r#"{"type":"api_retry","error":"server_error"}"#);
        assert_eq!(
            health
                .observe(&top_level, Uuid::nil(), &mut sequence, true)
                .events
                .len(),
            1
        );
    }

    /// hub #2913: a Harness `provider_error` event becomes one typed
    /// `provider_health` row. Fixture: the payload `agent_loop` sends (the
    /// `StreamEvent { event_type: "provider_error", data }` literal in
    /// `crates/rsid-harness/src/agent_loop.rs`), with a placeholder session tag.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_harness_provider_error_becomes_a_typed_provider_health_row() {
        let mut health = StreamHealth::default();
        let mut sequence = 4;
        let event = wire(
            r#"{"type":"provider_error","class":"rate_limited","http_status":429,"retry_after_ms":30000,"detail_code":"rate_limit_exceeded","session_id":"SESSION-TAG"}"#,
        );
        let seen = health.observe(&event, Uuid::nil(), &mut sequence, true);
        assert_eq!(kinds(&seen), [PROVIDER_ERROR]);
        assert_eq!(sequence, 5);
        let row = &seen.events[0];
        assert!(row.is_provider_health_notice());
        assert_eq!(
            row.content,
            "[harness] provider error: rate_limited (HTTP 429)"
        );
        let meta = &row.metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(meta["class"], "rate_limited");
        assert_eq!(meta["detail"]["http_status"], 429);
        assert_eq!(meta["detail"]["retry_after_ms"], 30000);
        assert_eq!(meta["detail"]["detail_code"], "rate_limit_exceeded");
        assert!(seen.notices.is_empty());
    }

    /// An unknown class and a free-text `detail_code` are never carried.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_provider_error_with_an_unknown_class_or_free_text_detail_is_bounded() {
        let mut health = StreamHealth::default();
        let mut sequence = 0;
        let event = wire(
            r#"{"type":"provider_error","class":"brand new class","detail_code":"secret sk-123 leaked here"}"#,
        );
        let seen = health.observe(&event, Uuid::nil(), &mut sequence, true);
        let row = &seen.events[0];
        assert_eq!(row.content, "[harness] provider error: unknown");
        let meta = &row.metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(meta["class"], "unknown");
        assert!(meta["detail"].get("detail_code").is_none());
    }

    /// hub #2765: the session record of applied launch overrides carries the
    /// provider, every entry and the exact arguments, and is a visible
    /// provider-health row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_launch_overrides_row_records_the_applied_entries_and_args() {
        use rsi_common::provider_launch_overrides::{OverrideProvider, normalize};
        let overrides = normalize(&json!({"codex": [
            "model_reasoning_summary=concise",
            "service_tier=fast",
        ]}))
        .unwrap();
        let applied =
            crate::provider_launch_overrides::resolve(&overrides, OverrideProvider::Codex).unwrap();
        let session = Uuid::new_v4();
        let mut sequence = 7;
        let event = launch_overrides_event(session, &mut sequence, &applied);
        assert_eq!((event.session_id, event.sequence), (session, 8));
        assert!(event.is_provider_health_notice());
        assert_eq!(
            event.content,
            "Operator launch overrides applied (codex): model_reasoning_summary=concise service_tier=fast"
        );
        let metadata = event.metadata.as_deref().unwrap();
        assert_eq!(
            metadata[PROVIDER_HEALTH_METADATA_KEY],
            json!({
                "kind": "launch_overrides",
                "provider": "codex",
                "entries": ["model_reasoning_summary=concise", "service_tier=fast"],
                "args": [
                    "-c", "model_reasoning_summary=\"concise\"",
                    "-c", "service_tier=\"fast\"",
                ],
            })
        );
    }

    /// #2763: a session record shows the effective subagent caps, only when an
    /// operator cap is set.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_subagent_caps_row_names_the_effective_caps_and_is_absent_by_default() {
        use crate::store_support::provider_settings::SubagentCaps;
        use rsi_common::types::SessionProvider;
        let session = Uuid::new_v4();
        let mut sequence = 7;
        assert!(
            subagent_caps_event(
                session,
                &mut sequence,
                SessionProvider::Claude,
                &SubagentCaps::default()
            )
            .is_none()
        );
        assert_eq!(sequence, 7, "no row, no sequence consumed");
        let row = subagent_caps_event(
            session,
            &mut sequence,
            SessionProvider::Codex,
            &SubagentCaps::from_settings("gpt-5.1-codex-mini", 3),
        )
        .expect("a set cap is recorded");
        assert_eq!(row.sequence, 8);
        assert!(row.is_provider_health_notice());
        assert_eq!(
            row.content,
            "Subagent governance (codex): subagent model gpt-5.1-codex-mini, at most 3 concurrent"
        );
        let health = &row.metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(health["kind"], "subagent_caps");
        assert_eq!(health["provider"], "codex");
        assert_eq!(health["subagent_model"], "gpt-5.1-codex-mini");
        assert_eq!(health["max_concurrent_subagents"], 3);
        let only_cap = subagent_caps_event(
            session,
            &mut sequence,
            SessionProvider::Claude,
            &SubagentCaps::from_settings("", 4),
        )
        .expect("a cap alone is recorded");
        assert_eq!(
            only_cap.content,
            "Subagent governance (claude): at most 4 concurrent"
        );
        let health = &only_cap.metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY];
        assert!(health["subagent_model"].is_null());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_access_policy_row_names_the_effective_mode_per_provider() {
        use rsi_common::harness_tool_policy::CliAccessMode;
        use rsi_common::types::SessionProvider;
        let session = Uuid::new_v4();
        let mut sequence = 4;
        let claude = access_policy_event(
            session,
            &mut sequence,
            SessionProvider::Claude,
            CliAccessMode::ReadOnly,
        );
        let codex = access_policy_event(
            session,
            &mut sequence,
            SessionProvider::Codex,
            CliAccessMode::ReadOnly,
        );
        assert_eq!((claude.sequence, codex.sequence), (5, 6));
        assert!(claude.is_provider_health_notice());
        assert!(
            claude.content.contains("--permission-mode dontAsk"),
            "{}",
            claude.content
        );
        assert!(
            claude.content.contains("Read, Grep, Glob"),
            "{}",
            claude.content
        );
        assert!(
            codex.content.contains("sandbox is read-only"),
            "{}",
            codex.content
        );
        assert_eq!(
            claude.metadata.as_ref().expect("metadata")[PROVIDER_HEALTH_METADATA_KEY]["cli_access"],
            "read_only"
        );
    }

    /// #2762: one typed `model_fallback_used{from,to}` row per distinct pair.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn model_fallback_row_is_typed_and_reported_once_per_pair() {
        let mut health = StreamHealth::default();
        let mut sequence = 0;
        let session = Uuid::new_v4();
        let fallback = || crate::monitor::ModelFallback {
            from: "claude-opus-5".to_string(),
            to: "claude-sonnet-5-5".to_string(),
        };
        let row = health
            .observe_model_fallback(Some(fallback()), session, &mut sequence)
            .expect("first sighting writes a row");
        assert!(row.is_provider_health_notice());
        let kind = row
            .metadata
            .as_ref()
            .and_then(|m| m.get(PROVIDER_HEALTH_METADATA_KEY))
            .cloned()
            .expect("provider_health metadata");
        assert_eq!(kind["kind"], MODEL_FALLBACK_USED);
        assert_eq!(kind["from"], "claude-opus-5");
        assert_eq!(kind["to"], "claude-sonnet-5-5");
        assert!(
            health
                .observe_model_fallback(Some(fallback()), session, &mut sequence)
                .is_none(),
            "the same pair in a later result adds no row"
        );
        assert!(
            health
                .observe_model_fallback(None, session, &mut sequence)
                .is_none()
        );
    }
}
