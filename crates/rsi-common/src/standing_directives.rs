//! Standing operator directives (#2649): durable, scope-keyed instructions that
//! outlive every manager seat rotation.
//!
//! A directive such as "keep the hub busy overnight and send ONE morning
//! summary" used to live only in handoff prose, retyped by each successor seat,
//! so every rotation was a lossy copy. The daemon now holds it as state on the
//! manager scope (a project, an area or a portfolio node), not on a session:
//!
//! - every seat of that scope receives the active directives verbatim in its
//!   authority-catalog guidance, and a context-cap successor receives them in
//!   its launch prompt, with no handoff text involved;
//! - `AgentManagerOverview` / `AgentGlobalOverview` list them with author,
//!   provenance and timestamp (the node's own, and one list per covered
//!   project);
//! - only an operator-attributed path writes them: the operator
//!   (`StandingDirectiveSet`, `StandingDirectiveClear`, `StandingDirectiveList`,
//!   all operator-only and outside `AGENT_VERBS`), or a parent manager relaying
//!   an operator instruction to a scope strictly below its own through
//!   `AgentStandingDirective`, which must carry provenance;
//! - a directive may expire ("while away overnight") or be cleared; both leave
//!   the row in place (audit), only an active directive is delivered.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manager_tier_routing::ManagerNodeRefV1;

/// The operator-only directive RPCs (never in an agent catalog).
pub const STANDING_DIRECTIVE_OPERATOR_METHODS: [&str; 3] = [
    "StandingDirectiveSet",
    "StandingDirectiveClear",
    "StandingDirectiveList",
];

/// Longest directive text (bytes). Eight of them fit in one seat prompt.
pub const STANDING_DIRECTIVE_MAX_TEXT_BYTES: usize = 2048;
/// Longest provenance note (bytes).
pub const STANDING_DIRECTIVE_MAX_PROVENANCE_BYTES: usize = 512;
/// Longest idempotency key (bytes).
pub const STANDING_DIRECTIVE_MAX_KEY_BYTES: usize = 128;
/// Most active directives one scope may hold.
pub const STANDING_DIRECTIVE_MAX_ACTIVE_PER_SCOPE: usize = 8;
/// Longest time to live (seconds): 90 days. A directive with no
/// `expires_in_secs` runs until it is cleared.
pub const STANDING_DIRECTIVE_MAX_TTL_SECS: u64 = 90 * 24 * 3600;
/// `author` of a directive the operator wrote.
pub const STANDING_DIRECTIVE_OPERATOR: &str = "operator";
/// Prefix of `author` and `cleared_by` for an agent session.
pub const STANDING_DIRECTIVE_SESSION_PREFIX: &str = "session:";

/// The request is malformed (blank or oversized text or provenance, a nil id,
/// a ttl outside `1..=STANDING_DIRECTIVE_MAX_TTL_SECS`).
pub const STANDING_DIRECTIVE_INVALID_REQUEST: &str = "standing_directive_invalid_request";
/// The caller holds no manager seat, or the target scope is not strictly below
/// the caller's own, or the directive was written by the operator and only the
/// operator may clear it.
pub const STANDING_DIRECTIVE_NOT_AUTHORIZED: &str = "standing_directive_not_authorized";
/// No directive matches.
pub const STANDING_DIRECTIVE_NOT_FOUND: &str = "standing_directive_not_found";
/// A reused idempotency key carries a different request.
pub const STANDING_DIRECTIVE_IDEMPOTENCY_CONFLICT: &str = "standing_directive_idempotency_conflict";
/// The scope already holds the most active directives.
pub const STANDING_DIRECTIVE_LIMIT: &str = "standing_directive_limit";

/// One standing directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingDirectiveV1 {
    pub id: Uuid,
    /// The manager scope it binds: `project:<id>`, `area:<id>` or
    /// `portfolio:<id>`.
    pub scope: String,
    /// The operator's words, verbatim.
    pub text: String,
    /// `operator`, or `session:<uuid>` of the parent manager that relayed it.
    pub author: String,
    /// Where the operator said it (a relayed directive always carries one).
    pub provenance: Option<String>,
    pub created_at: DateTime<Utc>,
    /// `None`: it runs until cleared.
    pub expires_at: Option<DateTime<Utc>>,
    pub cleared_at: Option<DateTime<Utc>>,
    /// `operator` or `session:<uuid>`.
    pub cleared_by: Option<String>,
}

impl StandingDirectiveV1 {
    /// Whether the directive is delivered at `now`.
    #[must_use]
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.cleared_at.is_none() && self.expires_at.is_none_or(|expires| expires > now)
    }
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.contains('\0')
}

fn valid_key(key: &str) -> bool {
    valid_text(key, STANDING_DIRECTIVE_MAX_KEY_BYTES)
}

fn valid_ttl(ttl: Option<u64>) -> bool {
    ttl.is_none_or(|secs| (1..=STANDING_DIRECTIVE_MAX_TTL_SECS).contains(&secs))
}

fn node_id(node: &ManagerNodeRefV1) -> Uuid {
    match node {
        ManagerNodeRefV1::Portfolio { node_id } | ManagerNodeRefV1::Area { node_id } => *node_id,
        ManagerNodeRefV1::Project { project_id } => *project_id,
    }
}

/// What `AgentStandingDirective` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandingDirectiveActionV1 {
    /// Relay `text` to `scope`.
    Set,
    /// Clear `directive_id`.
    Clear,
}

/// `AgentStandingDirective {action, ...}`: a parent manager relays an operator
/// instruction to a scope strictly below its own (`set`), or clears one a
/// manager relayed (`clear`). A `set` needs `scope`, `text`, `provenance` (where
/// the operator said it) and `idempotency_key`, and may carry
/// `expires_in_secs`; a `clear` needs `directive_id` and nothing else. An
/// operator-written directive only the operator clears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStandingDirectiveRequestV1 {
    pub action: StandingDirectiveActionV1,
    /// `project:<uuid>`, `area:<uuid>` or `portfolio:<uuid>`.
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub provenance: Option<String>,
    #[serde(default)]
    pub expires_in_secs: Option<u64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub directive_id: Option<Uuid>,
}

/// A request already checked: the shape the daemon acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStandingDirectiveCommandV1 {
    Set {
        scope: ManagerNodeRefV1,
        text: String,
        provenance: String,
        expires_in_secs: Option<u64>,
        idempotency_key: String,
    },
    Clear {
        directive_id: Uuid,
    },
}

impl AgentStandingDirectiveRequestV1 {
    /// Check the request and return what to do.
    ///
    /// # Errors
    /// [`STANDING_DIRECTIVE_INVALID_REQUEST`]: a field of the other action is
    /// present, a required field is missing, or a size or ttl bound is broken.
    pub fn command(&self) -> Result<AgentStandingDirectiveCommandV1, &'static str> {
        match self.action {
            StandingDirectiveActionV1::Set => {
                let scope = self
                    .scope
                    .as_deref()
                    .and_then(ManagerNodeRefV1::parse_ref_text)
                    .filter(|scope| !node_id(scope).is_nil());
                let (Some(scope), Some(text), Some(provenance), Some(key), None) = (
                    scope,
                    self.text.as_deref(),
                    self.provenance.as_deref(),
                    self.idempotency_key.as_deref(),
                    self.directive_id,
                ) else {
                    return Err(STANDING_DIRECTIVE_INVALID_REQUEST);
                };
                if valid_text(text, STANDING_DIRECTIVE_MAX_TEXT_BYTES)
                    && valid_text(provenance, STANDING_DIRECTIVE_MAX_PROVENANCE_BYTES)
                    && valid_ttl(self.expires_in_secs)
                    && valid_key(key)
                {
                    Ok(AgentStandingDirectiveCommandV1::Set {
                        scope,
                        text: text.to_string(),
                        provenance: provenance.to_string(),
                        expires_in_secs: self.expires_in_secs,
                        idempotency_key: key.to_string(),
                    })
                } else {
                    Err(STANDING_DIRECTIVE_INVALID_REQUEST)
                }
            }
            StandingDirectiveActionV1::Clear => match (
                self.directive_id,
                &self.scope,
                &self.text,
                &self.provenance,
                self.expires_in_secs,
                &self.idempotency_key,
            ) {
                (Some(id), None, None, None, None, None) if !id.is_nil() => {
                    Ok(AgentStandingDirectiveCommandV1::Clear { directive_id: id })
                }
                _ => Err(STANDING_DIRECTIVE_INVALID_REQUEST),
            },
        }
    }

    /// # Errors
    /// [`STANDING_DIRECTIVE_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        self.command().map(drop)
    }
}

/// `AgentStandingDirective` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStandingDirectiveResultV1 {
    pub directive: StandingDirectiveV1,
    /// A replay of an earlier `set` under the same idempotency key, or a
    /// `clear` of an already cleared directive.
    pub deduplicated: bool,
}

/// Operator-only `StandingDirectiveSet {scope, text, expires_in_secs?,
/// idempotency_key}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandingDirectiveSetRequestV1 {
    pub scope: ManagerNodeRefV1,
    pub text: String,
    #[serde(default)]
    pub expires_in_secs: Option<u64>,
    pub idempotency_key: String,
}

impl StandingDirectiveSetRequestV1 {
    /// # Errors
    /// [`STANDING_DIRECTIVE_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        if !node_id(&self.scope).is_nil()
            && valid_text(&self.text, STANDING_DIRECTIVE_MAX_TEXT_BYTES)
            && valid_ttl(self.expires_in_secs)
            && valid_key(&self.idempotency_key)
        {
            Ok(())
        } else {
            Err(STANDING_DIRECTIVE_INVALID_REQUEST)
        }
    }
}

/// Operator-only `StandingDirectiveClear {directive_id}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandingDirectiveClearRequestV1 {
    pub directive_id: Uuid,
}

impl StandingDirectiveClearRequestV1 {
    /// # Errors
    /// [`STANDING_DIRECTIVE_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.directive_id.is_nil() {
            Err(STANDING_DIRECTIVE_INVALID_REQUEST)
        } else {
            Ok(())
        }
    }
}

/// Operator-only `StandingDirectiveList {scope?, include_inactive?}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandingDirectiveListRequestV1 {
    /// One scope; every scope when absent.
    #[serde(default)]
    pub scope: Option<ManagerNodeRefV1>,
    /// Also return cleared and expired directives.
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingDirectiveListResultV1 {
    pub directives: Vec<StandingDirectiveV1>,
}

/// Result of the operator `StandingDirectiveSet` / `StandingDirectiveClear`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingDirectiveWriteResultV1 {
    pub directive: StandingDirectiveV1,
    pub deduplicated: bool,
}

/// The directives block a seat reads: the daemon's own words, then each
/// directive verbatim with its author, provenance and timestamps. `None` when
/// nothing is active. One renderer serves the authority catalog and the
/// context-cap successor prompt, so a seat sees the same text either way.
#[must_use]
pub fn render_standing_directives(directives: &[StandingDirectiveV1]) -> Option<String> {
    if directives.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Standing operator directives\n\n\
         The daemon holds these for your manager scope. They were set by the operator, or relayed \
         by a parent manager with provenance, and they bind this seat and every successor until \
         they expire or the operator clears them. They are carried by the daemon: do not copy \
         them into handoffs, and do not drop one because a handoff omits it.\n",
    );
    for (index, directive) in directives.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. [{}] set {} by {}",
            index + 1,
            directive.id,
            directive.created_at.format("%Y-%m-%dT%H:%MZ"),
            directive.author,
        ));
        if let Some(provenance) = &directive.provenance {
            out.push_str(&format!(" (provenance: {provenance})"));
        }
        match directive.expires_at {
            Some(expires) => {
                out.push_str(&format!("; expires {}", expires.format("%Y-%m-%dT%H:%MZ")))
            }
            None => out.push_str("; until cleared"),
        }
        out.push_str(&format!(":\n{}\n", directive.text.trim()));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directive(text: &str, provenance: Option<&str>) -> StandingDirectiveV1 {
        StandingDirectiveV1 {
            id: Uuid::from_u128(7),
            scope: "project:00000000-0000-0000-0000-000000000001".into(),
            text: text.into(),
            author: STANDING_DIRECTIVE_OPERATOR.into(),
            provenance: provenance.map(str::to_string),
            created_at: "2026-10-15T07:35:00Z".parse().unwrap(),
            expires_at: None,
            cleared_at: None,
            cleared_by: None,
        }
    }

    #[test]
    fn activity_follows_clear_and_expiry() {
        let now: DateTime<Utc> = "2026-10-15T12:00:00Z".parse().unwrap();
        let mut item = directive("x", None);
        assert!(item.is_active_at(now));
        item.expires_at = Some("2026-10-15T13:00:00Z".parse().unwrap());
        assert!(item.is_active_at(now));
        item.expires_at = Some(now);
        assert!(!item.is_active_at(now), "expiry is exclusive");
        item.expires_at = None;
        item.cleared_at = Some(now);
        assert!(!item.is_active_at(now));
    }

    #[test]
    fn the_rendered_block_carries_every_term_verbatim() {
        let text = "Overnight: keep the hub busy; send ONE morning summary.";
        let block = render_standing_directives(&[directive(text, Some("operator, 07:35Z"))])
            .expect("block");
        assert!(block.contains(text), "{block}");
        assert!(block.contains("operator, 07:35Z"), "{block}");
        assert!(block.contains("2026-10-15T07:35Z"), "{block}");
        assert!(render_standing_directives(&[]).is_none());
    }

    #[test]
    fn validation_bounds_text_provenance_ttl_and_nil_ids() {
        let scope = ManagerNodeRefV1::Project {
            project_id: Uuid::from_u128(1),
        };
        let set = |text: &str, provenance: &str, ttl: Option<u64>| {
            AgentStandingDirectiveRequestV1 {
                action: StandingDirectiveActionV1::Set,
                scope: Some(scope.as_ref_text()),
                text: Some(text.into()),
                provenance: Some(provenance.into()),
                expires_in_secs: ttl,
                idempotency_key: Some("k".into()),
                directive_id: None,
            }
            .validate()
        };
        assert!(set("keep going", "operator 07:35Z", Some(3600)).is_ok());
        assert!(set(" ", "p", None).is_err());
        assert!(
            set("t", "", None).is_err(),
            "a relay always names provenance"
        );
        assert!(set("t", "p", Some(0)).is_err());
        assert!(set("t", "p", Some(STANDING_DIRECTIVE_MAX_TTL_SECS + 1)).is_err());
        assert!(
            set(
                &"x".repeat(STANDING_DIRECTIVE_MAX_TEXT_BYTES + 1),
                "p",
                None
            )
            .is_err()
        );
        let clear = |id: Uuid, extra: Option<&str>| {
            AgentStandingDirectiveRequestV1 {
                action: StandingDirectiveActionV1::Clear,
                scope: None,
                text: extra.map(str::to_string),
                provenance: None,
                expires_in_secs: None,
                idempotency_key: None,
                directive_id: Some(id),
            }
            .command()
        };
        assert!(clear(Uuid::from_u128(2), None).is_ok());
        assert!(clear(Uuid::nil(), None).is_err());
        assert!(clear(Uuid::from_u128(2), Some("t")).is_err());
        let bad = serde_json::from_value::<AgentStandingDirectiveRequestV1>(serde_json::json!({
            "action": "set", "scope": scope.as_ref_text(),
            "text": "t", "provenance": "p", "idempotency_key": "k", "author": "operator"
        }));
        assert!(bad.is_err(), "no author comes from request JSON");
        let nil_scope = AgentStandingDirectiveRequestV1 {
            action: StandingDirectiveActionV1::Set,
            scope: Some(format!("project:{}", Uuid::nil())),
            text: Some("t".into()),
            provenance: Some("p".into()),
            expires_in_secs: None,
            idempotency_key: Some("k".into()),
            directive_id: None,
        };
        assert!(nil_scope.validate().is_err());
    }
}
