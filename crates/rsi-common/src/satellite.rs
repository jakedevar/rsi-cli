//! Versioned, bounded wire DTOs for operator-only satellite reads.
//!
//! These types describe identity and read data only. They do not authenticate a
//! peer; the configured transport and its local socket custody remain the
//! security boundary.

use crate::{SessionProvider, SessionStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use thiserror::Error;
use uuid::Uuid;

pub const SATELLITE_WIRE_VERSION_V1: u16 = 1;
pub const SATELLITE_PROTOCOL_MAJOR: u16 = 1;
pub const SATELLITE_PROTOCOL_MINOR: u16 = 0;
pub const SATELLITE_MAX_PAGE_SIZE: u16 = 100;
pub const SATELLITE_MAX_SESSIONS: u32 = 10_000;
pub const SATELLITE_MAX_CURSOR_BYTES: u16 = 512;
pub const SATELLITE_MAX_TITLE_BYTES: u16 = 512;
pub const SATELLITE_MAX_WORKING_DIR_BYTES: u16 = 4_096;
pub const SATELLITE_MAX_RESPONSE_BYTES: usize = 1_048_576;

/// UUID that must use the canonical lowercase hyphenated wire representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SatelliteUuidV1(pub Uuid);

impl Serialize for SatelliteUuidV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for SatelliteUuidV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let parsed = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
        if parsed.to_string() != value {
            return Err(serde::de::Error::custom(
                "UUID must be canonical lowercase hyphenated text",
            ));
        }
        Ok(Self(parsed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteProtocolVersionV1 {
    pub major: u16,
    pub minor: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteReadLimitsV1 {
    pub max_page_size: u16,
    pub max_sessions: u32,
    pub max_cursor_bytes: u16,
    pub max_title_bytes: u16,
    pub max_working_dir_bytes: u16,
    pub max_response_bytes: u32,
}

impl Default for SatelliteReadLimitsV1 {
    fn default() -> Self {
        Self {
            max_page_size: SATELLITE_MAX_PAGE_SIZE,
            max_sessions: SATELLITE_MAX_SESSIONS,
            max_cursor_bytes: SATELLITE_MAX_CURSOR_BYTES,
            max_title_bytes: SATELLITE_MAX_TITLE_BYTES,
            max_working_dir_bytes: SATELLITE_MAX_WORKING_DIR_BYTES,
            max_response_bytes: 1_048_576,
        }
    }
}

impl SatelliteReadLimitsV1 {
    /// # Errors
    /// Returns an error when any advertised limit exceeds the local ceiling or is zero.
    pub fn validate(&self) -> Result<(), SatelliteValidationError> {
        let default = Self::default();
        if self.max_page_size == 0
            || self.max_page_size > default.max_page_size
            || self.max_sessions == 0
            || self.max_sessions > default.max_sessions
            || self.max_cursor_bytes == 0
            || self.max_cursor_bytes > default.max_cursor_bytes
            || self.max_title_bytes == 0
            || self.max_title_bytes > default.max_title_bytes
            || self.max_working_dir_bytes == 0
            || self.max_working_dir_bytes > default.max_working_dir_bytes
            || self.max_response_bytes == 0
            || self.max_response_bytes > default.max_response_bytes
        {
            return Err(SatelliteValidationError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteCapabilitiesV1 {
    pub session_read: bool,
    pub limits: SatelliteReadLimitsV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteIdentityV1 {
    pub wire_version: u16,
    pub installation_id: SatelliteUuidV1,
    pub daemon_incarnation_id: SatelliteUuidV1,
    pub protocol: SatelliteProtocolVersionV1,
    pub capabilities: SatelliteCapabilitiesV1,
}

impl SatelliteIdentityV1 {
    /// # Errors
    /// Returns an error for an unsupported wire version, nil identity, or invalid read limits.
    pub fn validate(&self) -> Result<(), SatelliteValidationError> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1 {
            return Err(SatelliteValidationError::UnsupportedWireVersion(
                self.wire_version,
            ));
        }
        if self.installation_id.0.is_nil() || self.daemon_incarnation_id.0.is_nil() {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        self.capabilities.limits.validate()?;
        Ok(())
    }

    /// Minor versions are additive; a major mismatch or absent read feature is
    /// incompatible for the bounded session-list operation.
    #[must_use]
    pub const fn read_compatibility(&self) -> SatelliteCompatibility {
        if self.protocol.major != SATELLITE_PROTOCOL_MAJOR {
            SatelliteCompatibility::IncompatibleMajor {
                found: self.protocol.major,
                supported: SATELLITE_PROTOCOL_MAJOR,
            }
        } else if !self.capabilities.session_read {
            SatelliteCompatibility::MissingSessionRead
        } else {
            SatelliteCompatibility::Compatible
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SatelliteCompatibility {
    Compatible,
    MissingSessionRead,
    IncompatibleMajor { found: u16, supported: u16 },
}

/// Hub-owned registration identity combined with a peer-local session UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SatelliteSessionKeyV1 {
    pub peer_id: SatelliteUuidV1,
    pub remote_session_id: SatelliteUuidV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteSessionSummaryV1 {
    pub session_id: SatelliteUuidV1,
    pub title: Option<String>,
    pub provider: SessionProvider,
    pub status: SessionStatus,
    /// Display metadata from the remote host; never a local filesystem path.
    pub working_dir: Option<String>,
    #[serde(with = "canonical_timestamp")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "canonical_timestamp")]
    pub updated_at: DateTime<Utc>,
}

impl SatelliteSessionSummaryV1 {
    /// # Errors
    /// Returns an error when remote display metadata exceeds a limit or contains controls.
    pub fn validate(&self, limits: &SatelliteReadLimitsV1) -> Result<(), SatelliteValidationError> {
        if self.session_id.0.is_nil() {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.updated_at < self.created_at {
            return Err(SatelliteValidationError::InvalidTimestampOrder);
        }
        validate_display_text(self.title.as_deref(), usize::from(limits.max_title_bytes))?;
        validate_display_text(
            self.working_dir.as_deref(),
            usize::from(limits.max_working_dir_bytes),
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteSessionPageRequestV1 {
    pub wire_version: u16,
    pub limit: u16,
    pub cursor: Option<String>,
}

impl SatelliteSessionPageRequestV1 {
    /// # Errors
    /// Returns an error for an unsupported version, invalid limit, or invalid cursor.
    pub fn validate(&self, limits: &SatelliteReadLimitsV1) -> Result<(), SatelliteValidationError> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1 {
            return Err(SatelliteValidationError::UnsupportedWireVersion(
                self.wire_version,
            ));
        }
        if self.limit == 0 || self.limit > limits.max_page_size {
            return Err(SatelliteValidationError::InvalidPageLimit);
        }
        if self.cursor.as_ref().is_some_and(|cursor| {
            cursor.is_empty() || cursor.len() > usize::from(limits.max_cursor_bytes)
        }) {
            return Err(SatelliteValidationError::InvalidCursor);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteSessionPageV1 {
    pub wire_version: u16,
    pub installation_id: SatelliteUuidV1,
    pub daemon_incarnation_id: SatelliteUuidV1,
    pub snapshot_id: SatelliteUuidV1,
    pub snapshot_total_sessions: u32,
    pub snapshot_offset: u32,
    #[serde(with = "canonical_timestamp")]
    pub observed_at: DateTime<Utc>,
    pub sessions: Vec<SatelliteSessionSummaryV1>,
    pub next_cursor: Option<String>,
}

impl SatelliteSessionPageV1 {
    /// # Errors
    /// Returns an error for invalid pagination, limits, duplicate rows, or unsafe display text.
    pub fn validate(&self, limits: &SatelliteReadLimitsV1) -> Result<(), SatelliteValidationError> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1 {
            return Err(SatelliteValidationError::UnsupportedWireVersion(
                self.wire_version,
            ));
        }
        if self.installation_id.0.is_nil()
            || self.daemon_incarnation_id.0.is_nil()
            || self.snapshot_id.0.is_nil()
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.sessions.len() > usize::from(limits.max_page_size) {
            return Err(SatelliteValidationError::PageTooLarge);
        }
        let rows = u32::try_from(self.sessions.len())
            .map_err(|_| SatelliteValidationError::InvalidSnapshotCount)?;
        let page_end = self
            .snapshot_offset
            .checked_add(rows)
            .ok_or(SatelliteValidationError::InvalidSnapshotCount)?;
        if self.snapshot_total_sessions > limits.max_sessions
            || page_end > self.snapshot_total_sessions
        {
            return Err(SatelliteValidationError::InvalidSnapshotCount);
        }
        if self.sessions.is_empty() && page_end < self.snapshot_total_sessions {
            return Err(SatelliteValidationError::InvalidPagination);
        }
        if self.next_cursor.as_ref().is_some_and(|cursor| {
            cursor.is_empty() || cursor.len() > usize::from(limits.max_cursor_bytes)
        }) {
            return Err(SatelliteValidationError::InvalidCursor);
        }
        if self.next_cursor.is_some() == (page_end == self.snapshot_total_sessions) {
            return Err(SatelliteValidationError::InvalidPagination);
        }
        let mut session_ids = HashSet::with_capacity(self.sessions.len());
        for session in &self.sessions {
            if !session_ids.insert(session.session_id) {
                return Err(SatelliteValidationError::DuplicateSessionId);
            }
            session.validate(limits)?;
        }
        Ok(())
    }
}

/// Reject an oversized payload before JSON parsing allocates its value tree.
/// # Errors
/// Returns an error when the payload exceeds the local byte ceiling or is invalid JSON.
pub fn decode_satellite_json<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, SatelliteDecodeError> {
    decode_satellite_json_with_limit(bytes, SATELLITE_MAX_RESPONSE_BYTES)
}

/// Decode with the peer-advertised response limit, capped by the local ceiling.
/// # Errors
/// Returns an error when the payload exceeds either byte ceiling or is invalid JSON.
pub fn decode_satellite_json_with_limit<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    peer_limit: usize,
) -> Result<T, SatelliteDecodeError> {
    if bytes.len() > peer_limit.min(SATELLITE_MAX_RESPONSE_BYTES) {
        return Err(SatelliteDecodeError::PayloadTooLarge);
    }
    serde_json::from_slice(bytes).map_err(SatelliteDecodeError::Json)
}

fn validate_display_text(
    value: Option<&str>,
    max_bytes: usize,
) -> Result<(), SatelliteValidationError> {
    if let Some(value) = value
        && (value.len() > max_bytes || value.chars().any(char::is_control))
    {
        return Err(SatelliteValidationError::InvalidDisplayText);
    }
    Ok(())
}

mod canonical_timestamp {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_rfc3339_opts(SecondsFormat::Nanos, true))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let parsed = DateTime::parse_from_rfc3339(&value).map_err(serde::de::Error::custom)?;
        let utc = parsed.with_timezone(&Utc);
        if utc.to_rfc3339_opts(SecondsFormat::Nanos, true) != value {
            return Err(serde::de::Error::custom(
                "timestamp must be canonical RFC3339 UTC with nanoseconds",
            ));
        }
        Ok(utc)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SatelliteValidationError {
    #[error("unsupported satellite wire version {0}")]
    UnsupportedWireVersion(u16),
    #[error("satellite identity must be a non-nil UUID")]
    InvalidIdentity,
    #[error("satellite read limits are outside supported bounds")]
    InvalidLimits,
    #[error("satellite page limit is outside supported bounds")]
    InvalidPageLimit,
    #[error("satellite cursor is empty or exceeds its byte limit")]
    InvalidCursor,
    #[error("satellite page exceeds its declared row limit")]
    PageTooLarge,
    #[error("satellite snapshot count is outside supported bounds")]
    InvalidSnapshotCount,
    #[error("satellite snapshot offset and cursor do not describe a valid page")]
    InvalidPagination,
    #[error("satellite page contains duplicate session ids")]
    DuplicateSessionId,
    #[error("satellite display text exceeds its byte limit or contains controls")]
    InvalidDisplayText,
    #[error("satellite session timestamps are out of order")]
    InvalidTimestampOrder,
}

#[derive(Debug, Error)]
pub enum SatelliteDecodeError {
    #[error("satellite payload exceeds the response byte limit")]
    PayloadTooLarge,
    #[error("invalid satellite JSON: {0}")]
    Json(#[source] serde_json::Error),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uuid(value: &str) -> SatelliteUuidV1 {
        SatelliteUuidV1(Uuid::parse_str(value).unwrap())
    }

    fn identity() -> SatelliteIdentityV1 {
        SatelliteIdentityV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            installation_id: uuid("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            daemon_incarnation_id: uuid("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),
            protocol: SatelliteProtocolVersionV1 {
                major: SATELLITE_PROTOCOL_MAJOR,
                minor: SATELLITE_PROTOCOL_MINOR,
            },
            capabilities: SatelliteCapabilitiesV1 {
                session_read: true,
                limits: SatelliteReadLimitsV1::default(),
            },
        }
    }

    fn session(id: SatelliteUuidV1) -> SatelliteSessionSummaryV1 {
        SatelliteSessionSummaryV1 {
            session_id: id,
            title: Some("build task".into()),
            provider: SessionProvider::Codex,
            status: SessionStatus::Running,
            working_dir: Some("/remote/work/tree".into()),
            created_at: DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            updated_at: DateTime::from_timestamp(1_790_000_001, 0).unwrap(),
        }
    }

    fn page() -> SatelliteSessionPageV1 {
        SatelliteSessionPageV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            installation_id: identity().installation_id,
            daemon_incarnation_id: identity().daemon_incarnation_id,
            snapshot_id: uuid("cccccccc-cccc-4ccc-8ccc-cccccccccccc"),
            snapshot_total_sessions: 2,
            snapshot_offset: 0,
            observed_at: DateTime::from_timestamp(1_790_000_002, 0).unwrap(),
            sessions: vec![session(uuid("dddddddd-dddd-4ddd-8ddd-dddddddddddd"))],
            next_cursor: Some("opaque-v1".into()),
        }
    }

    #[test]
    fn identity_and_page_round_trip_with_canonical_identity() {
        let identity = identity();
        identity.validate().unwrap();
        let encoded = serde_json::to_vec(&identity).unwrap();
        let decoded: SatelliteIdentityV1 = decode_satellite_json(&encoded).unwrap();
        assert_eq!(decoded, identity);
        assert_eq!(
            decoded.read_compatibility(),
            SatelliteCompatibility::Compatible
        );

        let page = page();
        page.validate(&identity.capabilities.limits).unwrap();
        let encoded = serde_json::to_vec(&page).unwrap();
        let decoded: SatelliteSessionPageV1 = decode_satellite_json(&encoded).unwrap();
        assert_eq!(decoded, page);
    }

    #[test]
    fn rejects_noncanonical_uuid_and_unknown_enum_values() {
        let mut upper = serde_json::to_value(identity()).unwrap();
        upper["installation_id"] = json!("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA");
        assert!(serde_json::from_value::<SatelliteIdentityV1>(upper).is_err());

        let mut unknown = serde_json::to_value(page()).unwrap();
        unknown["sessions"][0]["status"] = json!("paused");
        assert!(serde_json::from_value::<SatelliteSessionPageV1>(unknown).is_err());

        let mut noncanonical_time = serde_json::to_value(page()).unwrap();
        noncanonical_time["observed_at"] = json!("2026-09-27T04:00:00Z");
        assert!(serde_json::from_value::<SatelliteSessionPageV1>(noncanonical_time).is_err());
    }

    #[test]
    fn rejects_invalid_versions_capability_limits_and_request_bounds() {
        let mut old_wire = identity();
        old_wire.wire_version = 0;
        assert_eq!(
            old_wire.validate(),
            Err(SatelliteValidationError::UnsupportedWireVersion(0))
        );

        let mut incompatible = identity();
        incompatible.protocol.major = SATELLITE_PROTOCOL_MAJOR + 1;
        assert!(matches!(
            incompatible.read_compatibility(),
            SatelliteCompatibility::IncompatibleMajor { .. }
        ));
        incompatible.protocol.major = SATELLITE_PROTOCOL_MAJOR;
        incompatible.protocol.minor += 1;
        assert_eq!(
            incompatible.read_compatibility(),
            SatelliteCompatibility::Compatible
        );
        incompatible.capabilities.session_read = false;
        assert_eq!(
            incompatible.read_compatibility(),
            SatelliteCompatibility::MissingSessionRead
        );

        let limits = SatelliteReadLimitsV1 {
            max_page_size: SATELLITE_MAX_PAGE_SIZE + 1,
            ..SatelliteReadLimitsV1::default()
        };
        assert_eq!(
            limits.validate(),
            Err(SatelliteValidationError::InvalidLimits)
        );
        let request = SatelliteSessionPageRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            limit: SATELLITE_MAX_PAGE_SIZE + 1,
            cursor: None,
        };
        assert_eq!(
            request.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidPageLimit)
        );
    }

    #[test]
    fn rejects_duplicate_rows_bad_display_fields_and_oversized_payloads() {
        let mut nil_identity = identity();
        nil_identity.installation_id = SatelliteUuidV1(Uuid::nil());
        assert_eq!(
            nil_identity.validate(),
            Err(SatelliteValidationError::InvalidIdentity)
        );

        let mut empty_page = page();
        empty_page.sessions.clear();
        assert_eq!(
            empty_page.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidPagination)
        );

        let mut duplicate_page = page();
        duplicate_page
            .sessions
            .push(duplicate_page.sessions[0].clone());
        duplicate_page.next_cursor = None;
        assert_eq!(
            duplicate_page.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::DuplicateSessionId)
        );

        let mut impossible_snapshot = page();
        impossible_snapshot.snapshot_total_sessions = 0;
        assert_eq!(
            impossible_snapshot.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidSnapshotCount)
        );

        let mut page = page();
        page.sessions[0].title = Some("bad\nlabel".into());
        assert_eq!(
            page.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidDisplayText)
        );

        let oversized = vec![b' '; SATELLITE_MAX_RESPONSE_BYTES + 1];
        assert!(matches!(
            decode_satellite_json::<SatelliteSessionPageV1>(&oversized),
            Err(SatelliteDecodeError::PayloadTooLarge)
        ));
        let small_payload = b"{}";
        assert!(matches!(
            decode_satellite_json_with_limit::<SatelliteIdentityV1>(small_payload, 1),
            Err(SatelliteDecodeError::PayloadTooLarge)
        ));
    }
}
