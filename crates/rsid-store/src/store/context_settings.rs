//! Typed persistence for context-cap rotations and succession requests (#457).
//!
//! Preserve the existing daemon_settings keys and serde representation. The
//! daemon owns rotation policy and transactions; the store owns row encoding.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rsi_common::daemon_handoff::CoordinatorSeatV1;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapState {
    /// Crossed; waiting for the seat's idle boundary.
    Due,
    /// The daemon wrote the handoff and requested the rotation.
    Rotating,
    /// A successor holds the seat.
    Rotated,
    /// The rotation could not run; the operator was told.
    Failed,
    /// No longer applicable (seat passed, cap turned off).
    Cleared,
}

/// One durable cap rotation per seat crossing. Rows are kept, never deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapRotation {
    pub state: CapState,
    pub seat: CoordinatorSeatV1,
    pub measured_tokens: u64,
    pub cap_tokens: u64,
    pub crossed_at: DateTime<Utc>,
    pub attempts: u32,
    pub requested_at: Option<DateTime<Utc>>,
    pub successor: Option<Uuid>,
    pub reason: Option<String>,
    /// Stable operation identity of the one rotation this record requests,
    /// written with the handoff and the `Rotating` marker (#1142 F2). Every
    /// dispatch of the request, including a post-restart one, uses it.
    #[serde(default)]
    pub rotation_id: Option<String>,
    /// The idle incarnation the request was planned against (#1142 F1).
    #[serde(default)]
    pub fence: Option<IdleFence>,
    /// The daemon process that last dispatched the request. A request whose
    /// dispatcher is another process is reconciled once by the next pass.
    #[serde(default)]
    pub dispatched_by: Option<String>,
}

/// The exact idle incarnation of a predecessor: its row version and provider
/// session. Any continuation, status change or metadata write changes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleFence {
    pub row_version: String,
    pub claude_session_id: Option<String>,
}

/// A session whose seat passes through authority-preserving succession.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum CoordinatorRole {
    /// The live lineage tip of its project's appointed manager.
    Manager,
    /// The current lead of an Epic.
    Lead { epic_id: Uuid },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Due,
    Cleared,
}

/// One durable succession request per seat and threshold crossing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessionRequest {
    pub state: RequestState,
    pub coordinator: CoordinatorRole,
    pub context_pct: f64,
    pub threshold_pct: f64,
    pub crossed_at: DateTime<Utc>,
    pub deliveries: u32,
    pub last_delivered_at: Option<DateTime<Utc>>,
    pub escalated: bool,
    pub cleared_reason: Option<String>,
}

enum ContextSetting {
    CapRotation,
    CapHandoff,
    SuccessionRequest,
}

impl ContextSetting {
    fn prefix(&self) -> &'static str {
        match self {
            Self::CapRotation => "context_cap_due:",
            Self::CapHandoff => "context_cap_handoff:",
            Self::SuccessionRequest => "context_succession_due:",
        }
    }
}

/// The one constructor for these existing per-session settings keys.
fn context_setting_key(setting: ContextSetting, session_id: Uuid) -> String {
    format!("{}{session_id}", setting.prefix())
}

impl Store {
    pub fn get_context_cap_rotation(&self, session_id: Uuid) -> Result<Option<CapRotation>> {
        self.get_daemon_setting(&context_setting_key(
            ContextSetting::CapRotation,
            session_id,
        ))?
        .map(|raw| serde_json::from_str(&raw).map_err(|e| DaemonError::Store(e.to_string())))
        .transpose()
    }

    pub fn set_context_cap_rotation(&self, session_id: Uuid, record: &CapRotation) -> Result<()> {
        self.set_daemon_setting(
            &context_setting_key(ContextSetting::CapRotation, session_id),
            &serde_json::to_string(record)?,
        )
    }

    /// The handoff is already rendered and validated by the daemon. Store it
    /// verbatim, without JSON re-encoding or normalization.
    pub fn set_context_cap_handoff(&self, session_id: Uuid, document: &str) -> Result<()> {
        self.set_daemon_setting(
            &context_setting_key(ContextSetting::CapHandoff, session_id),
            document,
        )
    }

    pub fn get_context_cap_handoff(&self, session_id: Uuid) -> Result<Option<String>> {
        self.get_daemon_setting(&context_setting_key(ContextSetting::CapHandoff, session_id))
    }

    pub fn get_context_succession_request(
        &self,
        session_id: Uuid,
    ) -> Result<Option<SuccessionRequest>> {
        self.get_daemon_setting(&context_setting_key(
            ContextSetting::SuccessionRequest,
            session_id,
        ))?
        .map(|raw| serde_json::from_str(&raw).map_err(|e| DaemonError::Store(e.to_string())))
        .transpose()
    }

    pub fn set_context_succession_request(
        &self,
        session_id: Uuid,
        request: &SuccessionRequest,
    ) -> Result<()> {
        self.set_daemon_setting(
            &context_setting_key(ContextSetting::SuccessionRequest, session_id),
            &serde_json::to_string(request)?,
        )
    }

    pub fn context_cap_session_ids(&self) -> Result<Vec<Uuid>> {
        self.context_setting_session_ids(ContextSetting::CapRotation)
    }

    pub fn context_succession_session_ids(&self) -> Result<Vec<Uuid>> {
        self.context_setting_session_ids(ContextSetting::SuccessionRequest)
    }

    fn context_setting_session_ids(&self, setting: ContextSetting) -> Result<Vec<Uuid>> {
        // Keep the original LIKE query, ordering and suffix parsing. Read-back
        // uses the canonical key just as the daemon did before these accessors.
        let mut statement = self
            .conn
            .prepare("SELECT key FROM daemon_settings WHERE key LIKE ?1 ORDER BY key")?;
        let keys = statement
            .query_map([format!("{}%", setting.prefix())], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(keys
            .into_iter()
            .filter_map(|key| {
                key.strip_prefix(setting.prefix())
                    .and_then(|id| Uuid::parse_str(id).ok())
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cap_rotation_accessor_preserves_legacy_defaults_keys_and_json() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let key = "context_cap_due:01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(store.get_context_cap_rotation(id).unwrap(), None);
        // Pre-rotation-identity rows omit the three later defaulted fields.
        let legacy = r#"{"state":"due","seat":{"kind":"project_manager"},"measured_tokens":800000,"cap_tokens":600000,"crossed_at":"2026-10-20T01:02:03.123456789Z","attempts":0,"requested_at":null,"successor":null,"reason":null}"#;
        store.set_daemon_setting(key, legacy).unwrap();
        let mut record = store.get_context_cap_rotation(id).unwrap().unwrap();
        assert_eq!(record.state, CapState::Due);
        assert_eq!(record.seat, CoordinatorSeatV1::ProjectManager);
        assert_eq!(record.rotation_id, None);
        assert_eq!(record.fence, None);
        assert_eq!(record.dispatched_by, None);
        store.set_context_cap_rotation(id, &record).unwrap();
        assert_eq!(
            store.get_daemon_setting(key).unwrap().unwrap(),
            format!(
                "{}{}",
                legacy.trim_end_matches('}'),
                r#","rotation_id":null,"fence":null,"dispatched_by":null}"#
            )
        );
        record.state = CapState::Rotating;
        record.attempts = 1;
        record.requested_at = Some(record.crossed_at);
        record.successor = Some(Uuid::new_v4());
        record.reason = Some("rotation requested".into());
        record.rotation_id = Some("stable-rotation".into());
        record.fence = Some(IdleFence {
            row_version: "2026-10-20T01:02:03.123456789Z".into(),
            claude_session_id: Some("provider-session".into()),
        });
        record.dispatched_by = Some("boot".into());
        store.set_context_cap_rotation(id, &record).unwrap();
        assert_eq!(
            store.get_daemon_setting(key).unwrap(),
            Some(serde_json::to_string(&record).unwrap())
        );
        assert_eq!(store.get_context_cap_rotation(id).unwrap(), Some(record));
        assert_eq!(store.context_cap_session_ids().unwrap(), vec![id]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cap_handoff_accessor_preserves_legacy_and_new_document_bytes() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let key = "context_cap_handoff:01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(store.get_context_cap_handoff(id).unwrap(), None);
        let legacy = "  {\"handoff\":\"old\\ntext\"}\r\n";
        store.set_daemon_setting(key, legacy).unwrap();
        assert_eq!(
            store.get_context_cap_handoff(id).unwrap().as_deref(),
            Some(legacy)
        );
        let document = "# Handoff\n\nQuoted: \"text\" and \\ path\r\nUnicode: λ\n";
        store.set_context_cap_handoff(id, document).unwrap();
        assert_eq!(
            store.get_daemon_setting(key).unwrap().as_deref(),
            Some(document)
        );
        assert_eq!(
            store.get_context_cap_handoff(id).unwrap().as_deref(),
            Some(document)
        );
        store.set_context_cap_handoff(id, "").unwrap();
        assert_eq!(
            store.get_context_cap_handoff(id).unwrap(),
            Some(String::new())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn succession_request_accessor_preserves_legacy_keys_and_json() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let key = "context_succession_due:01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(store.get_context_succession_request(id).unwrap(), None);
        let legacy = r#"{"state":"due","coordinator":{"role":"lead","epic_id":"fedcba98-7654-3210-fedc-ba9876543210"},"context_pct":82.5,"threshold_pct":80.0,"crossed_at":"2026-10-20T01:02:03.123456789Z","deliveries":0,"last_delivered_at":null,"escalated":false,"cleared_reason":null}"#;
        store.set_daemon_setting(key, legacy).unwrap();
        let mut request = store.get_context_succession_request(id).unwrap().unwrap();
        assert_eq!(request.state, RequestState::Due);
        assert_eq!(
            request.coordinator,
            CoordinatorRole::Lead {
                epic_id: Uuid::parse_str("fedcba98-7654-3210-fedc-ba9876543210").unwrap(),
            }
        );
        store.set_context_succession_request(id, &request).unwrap();
        assert_eq!(
            store.get_daemon_setting(key).unwrap().as_deref(),
            Some(legacy)
        );
        request.state = RequestState::Cleared;
        request.coordinator = CoordinatorRole::Manager;
        request.deliveries = 3;
        request.last_delivered_at = Some(request.crossed_at);
        request.escalated = true;
        request.cleared_reason = Some("seat_passed".into());
        store.set_context_succession_request(id, &request).unwrap();
        assert_eq!(
            store.get_daemon_setting(key).unwrap(),
            Some(serde_json::to_string(&request).unwrap())
        );
        assert_eq!(
            store.get_context_succession_request(id).unwrap(),
            Some(request)
        );
        assert_eq!(store.context_succession_session_ids().unwrap(), vec![id]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn context_accessors_preserve_enumeration_and_malformed_row_errors() {
        let store = Store::open_in_memory().unwrap();
        let first = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let second = Uuid::parse_str("fedcba98-7654-3210-fedc-ba9876543210").unwrap();
        for prefix in ["context_cap_due:", "context_succession_due:"] {
            for suffix in [second.to_string(), "not-a-uuid".into(), first.to_string()] {
                store
                    .set_daemon_setting(&format!("{prefix}{suffix}"), "malformed")
                    .unwrap();
            }
        }
        // LIKE's underscore wildcards match these, but prefix parsing rejects
        // them, exactly as the original daemon-side loops did.
        store
            .set_daemon_setting(&format!("contextXcapXdue:{first}"), "malformed")
            .unwrap();
        store
            .set_daemon_setting(&format!("contextXsuccessionXdue:{first}"), "malformed")
            .unwrap();
        store.set_context_cap_handoff(second, "document").unwrap();
        assert_eq!(
            store.context_cap_session_ids().unwrap(),
            vec![first, second]
        );
        assert_eq!(
            store.context_succession_session_ids().unwrap(),
            vec![first, second]
        );
        assert!(matches!(
            store.get_context_cap_rotation(first),
            Err(DaemonError::Store(_))
        ));
        assert!(matches!(
            store.get_context_succession_request(first),
            Err(DaemonError::Store(_))
        ));
    }
}
