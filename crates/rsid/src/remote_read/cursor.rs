use super::{ReadError, Result};
use chrono::{DateTime, Duration, Utc};
use rsi_common::remote_read::{CursorKindV1, CursorV1, OpaqueToken, ReadRequestV1};
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;
use zeroize::Zeroize;

const DOMAIN: &[u8] = b"rsi-remote-read-cursor-v1\0";
const MAX_POSITION_BYTES: usize = 384;
const BODY_HEADER_BYTES: usize = 1 + 8 + 16 + 32 + 32 + 2;
const TTL: Duration = Duration::minutes(15);

/// A daemon-boot signer. The RPC owner must retain one instance for the boot
/// and pass the digest of its current authorized policy scope on every call.
/// A new boot key invalidates old cursors. Neither this key nor positions are
/// durable; a cursor is a bounded continuation hint, not a snapshot lease.
pub struct RemoteCursorSigner {
    daemon_epoch: Uuid,
    key: [u8; 32],
}

impl Drop for RemoteCursorSigner {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl RemoteCursorSigner {
    pub fn new(daemon_epoch: Uuid) -> Self {
        Self {
            daemon_epoch,
            key: rand::random(),
        }
    }

    pub fn sign<T: Serialize>(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        position: &T,
    ) -> Result<CursorV1> {
        self.sign_at(request, policy_scope, position, Utc::now())
    }

    pub fn verify<T: DeserializeOwned>(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        cursor: &CursorV1,
    ) -> Result<T> {
        self.verify_at(request, policy_scope, cursor, Utc::now())
    }

    fn sign_at<T: Serialize>(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        position: &T,
        now: DateTime<Utc>,
    ) -> Result<CursorV1> {
        let kind = request_kind(request)?;
        let position = serde_json::to_vec(position).map_err(|_| ReadError::InvalidSource)?;
        if position.len() > MAX_POSITION_BYTES {
            return Err(ReadError::ResourceLimit);
        }
        let expiry = now
            .checked_add_signed(TTL)
            .ok_or(ReadError::InvalidSource)?
            .timestamp();
        let mut body = Vec::with_capacity(BODY_HEADER_BYTES + position.len());
        body.push(1);
        body.extend_from_slice(&expiry.to_be_bytes());
        body.extend_from_slice(self.daemon_epoch.as_bytes());
        body.extend_from_slice(&policy_scope);
        body.extend_from_slice(&request_digest(request)?);
        body.extend_from_slice(&(position.len() as u16).to_be_bytes());
        body.extend_from_slice(&position);
        let mac = self.mac(&body);
        let token = format!("v1_{}_{}", hex::encode(body), hex::encode(mac));
        let token = OpaqueToken::new(token).map_err(|_| ReadError::ResourceLimit)?;
        Ok(match kind {
            CursorKindV1::Projects => CursorV1::Projects { token },
            CursorKindV1::Sessions => CursorV1::Sessions { token },
            CursorKindV1::History => CursorV1::History { token },
            CursorKindV1::Decisions => CursorV1::Decisions { token },
        })
    }

    fn verify_at<T: DeserializeOwned>(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        cursor: &CursorV1,
        now: DateTime<Utc>,
    ) -> Result<T> {
        if cursor.kind() != request_kind(request)? {
            return Err(ReadError::StaleCursor);
        }
        let token = match cursor {
            CursorV1::Projects { token }
            | CursorV1::Sessions { token }
            | CursorV1::History { token }
            | CursorV1::Decisions { token } => token.as_str(),
        };
        let mut parts = token.split('_');
        let (Some("v1"), Some(body_hex), Some(mac_hex), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ReadError::StaleCursor);
        };
        if body_hex.len() > (BODY_HEADER_BYTES + MAX_POSITION_BYTES) * 2 || mac_hex.len() != 64 {
            return Err(ReadError::StaleCursor);
        }
        let body = hex::decode(body_hex).map_err(|_| ReadError::StaleCursor)?;
        let supplied_mac = hex::decode(mac_hex).map_err(|_| ReadError::StaleCursor)?;
        if body.len() < BODY_HEADER_BYTES
            || body[0] != 1
            || supplied_mac.len() != 32
            || hex::encode(&body) != body_hex
            || hex::encode(&supplied_mac) != mac_hex
        {
            return Err(ReadError::StaleCursor);
        }
        let expected_mac = self.mac(&body);
        let difference = supplied_mac
            .iter()
            .zip(expected_mac)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            });
        if difference != 0 {
            return Err(ReadError::StaleCursor);
        }
        let expiry = i64::from_be_bytes(body[1..9].try_into().unwrap());
        let position_len = u16::from_be_bytes(body[89..91].try_into().unwrap()) as usize;
        if expiry <= now.timestamp()
            || body[9..25] != *self.daemon_epoch.as_bytes()
            || body[25..57] != policy_scope
            || body[57..89] != request_digest(request)?
            || position_len > MAX_POSITION_BYTES
            || body.len() != BODY_HEADER_BYTES + position_len
        {
            return Err(ReadError::StaleCursor);
        }
        serde_json::from_slice(&body[BODY_HEADER_BYTES..]).map_err(|_| ReadError::StaleCursor)
    }

    fn mac(&self, body: &[u8]) -> [u8; 32] {
        let mut input = Vec::with_capacity(DOMAIN.len() + body.len());
        input.extend_from_slice(DOMAIN);
        input.extend_from_slice(body);
        *blake3::keyed_hash(&self.key, &input).as_bytes()
    }
}

fn request_kind(request: &ReadRequestV1) -> Result<CursorKindV1> {
    Ok(match request {
        ReadRequestV1::RemoteListProjectsV1(_) => CursorKindV1::Projects,
        ReadRequestV1::RemoteListSessionsV1(_) => CursorKindV1::Sessions,
        ReadRequestV1::RemoteGetHistoryPageV1(_) => CursorKindV1::History,
        ReadRequestV1::RemoteGetDecisionsV1(_) => CursorKindV1::Decisions,
        _ => return Err(ReadError::InvalidSource),
    })
}

/// Bind every typed request parameter except the token being verified. This
/// includes the project set, project/session identity, mode, window, limit and
/// selected decision. Field ordering is fixed by serde_json's object map.
fn request_digest(request: &ReadRequestV1) -> Result<[u8; 32]> {
    let mut value = serde_json::to_value(request).map_err(|_| ReadError::InvalidSource)?;
    value
        .get_mut("params")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or(ReadError::InvalidSource)?
        .remove("cursor");
    let bytes = serde_json::to_vec(&value).map_err(|_| ReadError::InvalidSource)?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Position {
        after: Uuid,
        bucket: u8,
    }

    fn request(project: Uuid, limit: u32) -> ReadRequestV1 {
        serde_json::from_value(json!({
            "method": "RemoteListSessionsV1",
            "params": {"project_id": project, "limit": limit}
        }))
        .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cursor_binds_request_scope_boot_expiry_and_position() {
        let epoch = Uuid::new_v4();
        let project = Uuid::new_v4();
        let read_request = request(project, 50);
        let signer = RemoteCursorSigner::new(epoch);
        let now = Utc::now();
        let position = Position {
            after: Uuid::new_v4(),
            bucket: 2,
        };
        let cursor = signer
            .sign_at(&read_request, [7; 32], &position, now)
            .unwrap();
        let mut resumed_value = serde_json::to_value(&read_request).unwrap();
        resumed_value["params"]["cursor"] = serde_json::to_value(&cursor).unwrap();
        let resumed_request: ReadRequestV1 = serde_json::from_value(resumed_value).unwrap();
        assert_eq!(
            signer
                .verify_at::<Position>(&resumed_request, [7; 32], &cursor, now)
                .unwrap(),
            position
        );
        for (changed_request, scope) in [
            (request(Uuid::new_v4(), 50), [7; 32]),
            (request(project, 51), [7; 32]),
            (request(project, 50), [8; 32]),
        ] {
            assert!(matches!(
                signer.verify_at::<Position>(&changed_request, scope, &cursor, now),
                Err(ReadError::StaleCursor)
            ));
        }
        assert!(matches!(
            signer.verify_at::<Position>(&read_request, [7; 32], &cursor, now + TTL),
            Err(ReadError::StaleCursor)
        ));
        assert!(matches!(
            RemoteCursorSigner::new(epoch).verify_at::<Position>(
                &read_request,
                [7; 32],
                &cursor,
                now
            ),
            Err(ReadError::StaleCursor)
        ));
        let bad_token = match cursor {
            CursorV1::Sessions { token } => {
                let mut token = token.as_str().to_owned();
                token.replace_range(10..11, if &token[10..11] == "a" { "b" } else { "a" });
                CursorV1::Sessions {
                    token: OpaqueToken::new(token).unwrap(),
                }
            }
            _ => unreachable!(),
        };
        assert!(matches!(
            signer.verify_at::<Position>(&read_request, [7; 32], &bad_token, now),
            Err(ReadError::StaleCursor)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cursor_refuses_oversized_position_and_wrong_method() {
        let signer = RemoteCursorSigner::new(Uuid::new_v4());
        let request = request(Uuid::new_v4(), 50);
        assert!(matches!(
            signer.sign(&request, [0; 32], &"x".repeat(MAX_POSITION_BYTES)),
            Err(ReadError::ResourceLimit)
        ));
        let cursor = signer.sign(&request, [0; 32], &1_u32).unwrap();
        let wrong_method: ReadRequestV1 = serde_json::from_value(json!({
            "method": "RemoteListProjectsV1",
            "params": {"project_ids": [], "limit": 50}
        }))
        .unwrap();
        assert!(matches!(
            signer.verify::<u32>(&wrong_method, [0; 32], &cursor),
            Err(ReadError::StaleCursor)
        ));
    }
}
