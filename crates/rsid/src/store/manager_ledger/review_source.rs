//! Issue #599 S1: the session whose live custody backs a DB-native review.
//!
//! The recorded work author stays the review's identity (reservation keys,
//! self-review refusal). Only the custody holder may differ: when the author
//! was rotated away, its sandbox is owned by a rotation tip. That tip is
//! resolved from durable custody rows, never from titles or agent input.
use super::{HarnessManagerConfigV1, OptionalExtension, Result, Store, Uuid, params, refused};
use rsi_common::types::{Session, SessionStatus};
use std::path::Path;

/// Bound on custody transfer hops walked from the author to the tip.
const TRANSFER_LINEAGE_LIMIT: usize = 64;

fn unavailable() -> crate::error::DaemonError {
    refused("manager_review_source_unavailable")
}

impl Store {
    /// Return `author` when it still holds its own live custody (or never had
    /// sandbox custody, preserving today's observation path). Otherwise
    /// return the single live rotation tip that owns the author's custody
    /// root at the same sandbox root, proven by an unbroken chain of
    /// `transferred` custody events from the author, inside the same Epic.
    /// Anything else is `manager_review_source_unavailable`.
    pub(crate) fn manager_review_source_holder(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        author: &Session,
    ) -> Result<Session> {
        let custody_id: Option<String> = self
            .conn
            .query_row(
                "SELECT sandbox_custody_id FROM sessions WHERE id=?1",
                [author.id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let author_live = !matches!(
            author.status,
            SessionStatus::Archived | SessionStatus::Deleted
        );
        if author_live && (custody_id.is_none() || self.live_custody_for_session(author.id).is_ok())
        {
            return Ok(author.clone());
        }
        let custody_id = custody_id.ok_or_else(unavailable)?;
        let mut claimants = self.conn.prepare(
            "SELECT id FROM sessions
              WHERE sandbox_custody_id=?1 AND id<>?2
                AND status NOT IN ('Archived','Deleted')
              ORDER BY id LIMIT 2",
        )?;
        let claimants: Vec<String> = claimants
            .query_map(params![custody_id, author.id.to_string()], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        // Exactly one live claimant. Two unarchived sessions sharing one
        // sandbox is ambiguous custody, never a choice for the daemon to make.
        let [tip] = claimants.as_slice() else {
            return Err(unavailable());
        };
        let tip = Uuid::parse_str(tip).map_err(|_| unavailable())?;
        let tip = self.get_session(tip)?.ok_or_else(unavailable)?;
        if !rsi_common::is_leaf_kind(tip.session_kind)
            || tip.project_id != author.project_id
            || author.sandbox_root.is_none()
            || tip.sandbox_root != author.sandbox_root
            || self.manager_v2_descendant_epic(config, tip.id)? != epic
        {
            return Err(unavailable());
        }
        let custody = self
            .live_custody_for_session(tip.id)
            .map_err(|_| unavailable())?;
        if custody.custody_id.to_string() != custody_id
            || author.sandbox_root.as_deref() != Some(Path::new(&custody.sandbox_root))
        {
            return Err(unavailable());
        }
        let mut current = author.id;
        for _ in 0..TRANSFER_LINEAGE_LIMIT {
            let next: Option<String> = self
                .conn
                .query_row(
                    "SELECT to_owner_session_id FROM sandbox_custody_events
                      WHERE custody_id=?1 AND event_kind='transferred'
                        AND from_owner_session_id=?2
                      ORDER BY sequence DESC LIMIT 1",
                    params![custody_id, current.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            current = next
                .and_then(|id| Uuid::parse_str(&id).ok())
                .ok_or_else(unavailable)?;
            if current == tip.id {
                return Ok(tip);
            }
        }
        Err(unavailable())
    }
}
