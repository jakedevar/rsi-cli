//! #3566: exact, expiring operator consent for one non-forced main push.
//! Files are local cooperative-hook state, not agent-mintable RPC authority.
//! Keep AuthorizeMainPush out of every agent/read/hook verb catalog.

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeParams {
    idempotency_key: Uuid,
    remote_url: String,
    #[serde(rename = "ref")]
    target_ref: String,
    target_sha: String,
    expires_at: DateTime<Utc>,
    operator_note: String,
}

fn authorize(dir: &Path, params: AuthorizeParams) -> Result<serde_json::Value> {
    let now = Utc::now();
    let invalid = |message: &str| DaemonError::InvalidParam(message.into());
    if params.remote_url.trim().is_empty()
        || params.remote_url.trim() != params.remote_url
        || params.remote_url.chars().any(char::is_control)
    {
        return Err(invalid(
            "remote_url must be the exact nonempty Git push URL",
        ));
    }
    if params.target_ref != "refs/heads/main" {
        return Err(invalid("ref must be refs/heads/main"));
    }
    if params.target_sha.len() != 40
        || !params
            .target_sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || params.target_sha.bytes().all(|b| b == b'0')
    {
        return Err(invalid(
            "target_sha must be a nonzero lowercase 40-hex commit SHA",
        ));
    }
    if params.expires_at <= now || params.expires_at > now + chrono::Duration::hours(24) {
        return Err(invalid("expires_at must be in the next 24 hours"));
    }
    if params.operator_note.trim().is_empty() || params.operator_note.len() > 4096 {
        return Err(invalid(
            "operator_note must contain the operator's consent (1-4096 bytes)",
        ));
    }
    let id = params.idempotency_key;
    let mut record = serde_json::json!({
        "version": 1,
        "id": id,
        "remote_url": params.remote_url,
        "ref": params.target_ref,
        "target_sha": params.target_sha,
        "created_at": now.to_rfc3339_opts(SecondsFormat::Nanos, true),
        "expires_at": params.expires_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        "operator_note": params.operator_note,
    });
    std::fs::create_dir_all(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // A partial file fails closed in the hook. Keep grants and .used receipts;
    // deleting a receipt would restore authority and is never a cleanup step.
    let path = dir.join(format!("{id}.json"));
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            record["created_at"] = existing["created_at"].clone();
            return if existing == record {
                // Never rewrite the grant or its .used receipt on retry.
                Ok(existing)
            } else {
                Err(invalid(
                    "idempotency_key already binds a different main-push grant",
                ))
            };
        }
        Err(error) => return Err(error.into()),
    };
    file.write_all(serde_json::to_string_pretty(&record)?.as_bytes())?;
    file.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(record)
}

impl RpcServer {
    pub(super) async fn handle_authorize_main_push(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if request.session_token.is_some() {
            return Err(DaemonError::PolicyDenied(
                "main_push_consent_operator_only: only the operator can authorize a main push"
                    .into(),
            ));
        }
        let params = serde_json::from_value(request.params.clone()).map_err(|error| {
            DaemonError::InvalidParam(format!("invalid AuthorizeMainPush: {error}"))
        })?;
        let dir = self.git_push_consent_dir.clone();
        tokio::task::spawn_blocking(move || authorize(&dir, params))
            .await
            .map_err(|error| DaemonError::Process(error.to_string()))?
    }
}
