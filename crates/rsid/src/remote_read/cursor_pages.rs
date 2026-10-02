use super::{HistoryRange, ReadError, RemoteCursorSigner, Result};
use rsi_common::remote_read::{CursorV1, HistoryWindowV1, ReadRequestV1};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListPosition {
    after: Uuid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryPosition {
    sequence: i32,
    id: i64,
}

impl RemoteCursorSigner {
    /// Projects and sessions advance over examined UUIDs, including a key
    /// whose row was filtered from the visible page. Neither list is a frozen
    /// snapshot; the caller must derive `next` from its bounded source scan.
    pub fn sign_list_position(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        next: Option<Uuid>,
        has_more: bool,
    ) -> Result<Option<CursorV1>> {
        if !matches!(
            request,
            ReadRequestV1::RemoteListProjectsV1(_) | ReadRequestV1::RemoteListSessionsV1(_)
        ) || next.is_some() != has_more
        {
            return Err(ReadError::InvalidSource);
        }
        next.map(|after| self.sign(request, policy_scope, &ListPosition { after }))
            .transpose()
    }

    pub fn verify_list_position(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        cursor: &CursorV1,
    ) -> Result<Uuid> {
        if !matches!(
            request,
            ReadRequestV1::RemoteListProjectsV1(_) | ReadRequestV1::RemoteListSessionsV1(_)
        ) {
            return Err(ReadError::StaleCursor);
        }
        let position: ListPosition = self.verify(request, policy_scope, cursor)?;
        Ok(position.after)
    }

    /// The source's next edge is the oldest examined key for latest/older,
    /// and the newest examined key for newer/interval. It stays bound to the
    /// original typed window, even when a later page uses a narrower query.
    pub fn sign_history_position(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        next: Option<(i32, i64)>,
        has_more: bool,
    ) -> Result<Option<CursorV1>> {
        if next.is_some() != has_more {
            return Err(ReadError::InvalidSource);
        }
        next.map(|(sequence, id)| {
            history_range(request, (sequence, id))?;
            self.sign(request, policy_scope, &HistoryPosition { sequence, id })
        })
        .transpose()
    }

    pub fn verify_history_position(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        cursor: &CursorV1,
    ) -> Result<HistoryRange> {
        if !matches!(request, ReadRequestV1::RemoteGetHistoryPageV1(_)) {
            return Err(ReadError::StaleCursor);
        }
        let position: HistoryPosition = self.verify(request, policy_scope, cursor)?;
        history_range(request, (position.sequence, position.id)).map_err(|_| ReadError::StaleCursor)
    }
}

fn history_range(request: &ReadRequestV1, edge: (i32, i64)) -> Result<HistoryRange> {
    if edge.1 <= 0 {
        return Err(ReadError::InvalidSource);
    }
    let ReadRequestV1::RemoteGetHistoryPageV1(params) = request else {
        return Err(ReadError::InvalidSource);
    };
    let wire_key = |key: &rsi_common::remote_read::EventKeyV1| (key.sequence, key.id.get());
    match &params.window {
        HistoryWindowV1::Latest {} => Ok(HistoryRange::Older { anchor: edge }),
        HistoryWindowV1::Older { anchor } if edge < wire_key(anchor) => {
            Ok(HistoryRange::Older { anchor: edge })
        }
        HistoryWindowV1::Newer { anchor, through }
            if edge > wire_key(anchor)
                && through.as_ref().is_none_or(|upper| edge < wire_key(upper)) =>
        {
            Ok(HistoryRange::Newer {
                anchor: edge,
                through: through.as_ref().map(wire_key),
            })
        }
        HistoryWindowV1::Interval {
            lower_exclusive,
            upper_inclusive,
        } if edge > wire_key(lower_exclusive) && edge < wire_key(upper_inclusive) => {
            Ok(HistoryRange::Interval {
                lower_exclusive: edge,
                upper_inclusive: wire_key(upper_inclusive),
            })
        }
        // Locate has a separate bounded relocation calculation and cannot be
        // continued through an invented keyset edge.
        _ => Err(ReadError::InvalidSource),
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::remote_read::list_projects;
    use rusqlite::Connection;
    use serde_json::json;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn signed_project_cursor_keeps_filtered_empty_continuation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT NOT NULL)")
            .unwrap();
        let ids = [
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        ];
        for id in [ids[0], ids[2]] {
            conn.execute(
                "INSERT INTO projects(id,name) VALUES(?1,'visible')",
                [id.to_string()],
            )
            .unwrap();
        }
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteListProjectsV1",
            "params":{"project_ids":ids,"limit":1}
        }))
        .unwrap();
        let signer = RemoteCursorSigner::new(Uuid::new_v4());
        let first = list_projects(&conn, &ids, None, 1).unwrap();
        let cursor = signer
            .sign_list_position(&request, [1; 32], first.next, first.has_more)
            .unwrap()
            .unwrap();
        let empty = list_projects(
            &conn,
            &ids,
            Some(
                signer
                    .verify_list_position(&request, [1; 32], &cursor)
                    .unwrap(),
            ),
            1,
        )
        .unwrap();
        assert!(empty.items.is_empty());
        let empty_cursor = signer
            .sign_list_position(&request, [1; 32], empty.next, empty.has_more)
            .unwrap()
            .unwrap();
        let last = list_projects(
            &conn,
            &ids,
            Some(
                signer
                    .verify_list_position(&request, [1; 32], &empty_cursor)
                    .unwrap(),
            ),
            1,
        )
        .unwrap();
        assert_eq!(last.items[0].id, ids[2]);
        assert!(
            signer
                .sign_list_position(&request, [1; 32], last.next, last.has_more)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn signed_history_cursor_keeps_window_and_lossless_large_id() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteGetHistoryPageV1",
            "params":{"project_id":project,"session_id":session,"window":{"kind":"latest"},"limit":25}
        }))
        .unwrap();
        let signer = RemoteCursorSigner::new(Uuid::new_v4());
        let edge = (42, 9_007_199_254_740_993_i64);
        let cursor = signer
            .sign_history_position(&request, [2; 32], Some(edge), true)
            .unwrap()
            .unwrap();
        assert!(matches!(
            signer.verify_history_position(&request, [2; 32], &cursor),
            Ok(HistoryRange::Older { anchor }) if anchor == edge
        ));
        let changed_window: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteGetHistoryPageV1",
            "params":{"project_id":project,"session_id":session,
                "window":{"kind":"older","anchor":{"sequence":43,"id":"9007199254740994"}},"limit":25}
        }))
        .unwrap();
        assert!(matches!(
            signer.verify_history_position(&changed_window, [2; 32], &cursor),
            Err(ReadError::StaleCursor)
        ));
        assert!(matches!(
            signer.sign_history_position(&request, [2; 32], None, true),
            Err(ReadError::InvalidSource)
        ));

        let newer: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteGetHistoryPageV1",
            "params":{"project_id":project,"session_id":session,
                "window":{"kind":"newer",
                    "anchor":{"sequence":40,"id":"9007199254740991"},
                    "through":{"sequence":50,"id":"9007199254741001"}},"limit":25}
        }))
        .unwrap();
        let newer_cursor = signer
            .sign_history_position(&newer, [2; 32], Some(edge), true)
            .unwrap()
            .unwrap();
        assert!(matches!(
            signer.verify_history_position(&newer, [2; 32], &newer_cursor),
            Ok(HistoryRange::Newer { anchor, through })
                if anchor == edge && through == Some((50, 9_007_199_254_741_001))
        ));
        assert!(matches!(
            signer.sign_history_position(&newer, [2; 32], Some((50, 9_007_199_254_741_001)), true),
            Err(ReadError::InvalidSource)
        ));
    }
}
