//! Operator-only key-vault RPC handlers (#694 K1).
//!
//! `rpc.rs` dispatches the six methods in
//! [`rsi_common::provider_credentials::OPERATOR_METHODS`] here. They are
//! absent from `AGENT_VERBS`, `READ_VERBS`, native provider tools and the
//! agent CLI catalog; the pre-dispatch attribution gate already refuses a
//! tokened caller, and [`handle`] refuses one again (defense in depth).
//!
//! Every response is serialized from a metadata type that has no secret
//! field. Request-parse errors never echo serde diagnostics, which could
//! quote a mistyped secret.

use super::{VaultError, VaultHandle};
use crate::error::{DaemonError, Result};
use rsi_common::provider_credentials::{
    METHOD_CHECK, METHOD_CLEAR, METHOD_IMPORT, METHOD_LIST, METHOD_ROTATE, METHOD_SET,
    OPERATOR_METHODS, ProviderCredentialSlotParams, SetProviderCredentialParams,
};
use serde::Deserialize;

/// Stable refusal code for a session-attributed call.
pub const OPERATOR_ONLY_REFUSAL: &str = "provider_credential_operator_only";

/// Whether `method` is one of the vault's operator methods.
#[must_use]
pub fn is_operator_method(method: &str) -> bool {
    OPERATOR_METHODS.contains(&method)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

fn parse<T: for<'de> Deserialize<'de>>(method: &str, params: &serde_json::Value) -> Result<T> {
    serde_json::from_value(params.clone()).map_err(|_| {
        DaemonError::InvalidParam(format!(
            "invalid {method} params; see rsi_common::provider_credentials for the request shape"
        ))
    })
}

fn parse_empty(method: &str, params: &serde_json::Value) -> Result<()> {
    if params.is_null() {
        return Ok(());
    }
    parse::<NoParams>(method, params).map(|_| ())
}

fn vault_error(error: VaultError) -> DaemonError {
    match error {
        VaultError::Store(error) => DaemonError::Store(format!("key vault: {error}")),
        error @ (VaultError::InvalidSecret(_) | VaultError::NothingToRotate(_)) => {
            DaemonError::InvalidParam(error.to_string())
        }
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(DaemonError::from)
}

/// Dispatch one operator vault method. `session_token_present` is the raw
/// transport fact; any attributed caller is refused before parsing.
///
/// # Errors
///
/// Returns `PolicyDenied` for a tokened caller, `InvalidParam` for a
/// malformed request, invalid secret or missing rotation entry, and `Store`
/// for a vault persistence failure. No error carries a secret.
pub async fn handle(
    vault: &VaultHandle,
    method: &str,
    session_token_present: bool,
    params: &serde_json::Value,
) -> Result<serde_json::Value> {
    if session_token_present {
        return Err(DaemonError::PolicyDenied(format!(
            "{OPERATOR_ONLY_REFUSAL}: `{method}` is operator-only and is not available to session-attributed callers"
        )));
    }
    match method {
        METHOD_SET | METHOD_ROTATE => {
            let request: SetProviderCredentialParams = parse(method, params)?;
            let slot = request.slot;
            let secret = zeroize::Zeroizing::new(request.secret);
            if method == METHOD_SET {
                vault.set(slot, &secret).map_err(vault_error)?;
            } else {
                vault.rotate(slot, &secret).map_err(vault_error)?;
            }
            drop(secret);
            // Set/Rotate run a check right away (secret-free result).
            vault.check_now(slot).await;
            to_value(&vault.metadata(slot))
        }
        METHOD_CLEAR => {
            let request: ProviderCredentialSlotParams = parse(method, params)?;
            vault.clear(request.slot).map_err(vault_error)?;
            to_value(&vault.metadata(request.slot))
        }
        METHOD_CHECK => {
            let request: ProviderCredentialSlotParams = parse(method, params)?;
            vault.check_now(request.slot).await;
            to_value(&vault.metadata(request.slot))
        }
        METHOD_LIST => {
            parse_empty(method, params)?;
            to_value(&vault.list())
        }
        METHOD_IMPORT => {
            parse_empty(method, params)?;
            to_value(&vault.import_from_env().map_err(vault_error)?)
        }
        other => Err(DaemonError::InvalidParam(format!(
            "unknown key-vault method `{other}`"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vault::check::ProbeOutcome;
    use crate::vault::check::tests::{ScriptedProbe, http};
    use crate::vault::{Slot, VaultHandleBuilder, VaultSettings};
    use rsi_common::provider_credentials::{
        CredentialCheckClass, CredentialState, ImportProviderCredentialsResult,
        ListProviderCredentialsResult, ProviderCredentialMetadata,
    };
    use std::sync::Arc;

    const SECRET: &str = "sk-test-operator-rpc-0001";

    fn vault(
        root: &std::path::Path,
        probe: ScriptedProbe,
        env: &'static [(&'static str, &'static str)],
    ) -> VaultHandle {
        VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
            .dir(root.join("vault"))
            .env(move |name| {
                env.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            })
            .probe(Arc::new(probe))
            .open()
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn every_method_refuses_a_tokened_caller_before_parsing() {
        let root = tempfile::tempdir().unwrap();
        let vault = vault(root.path(), ScriptedProbe::default(), &[]);
        for method in OPERATOR_METHODS {
            let error = handle(
                &vault,
                method,
                true,
                &serde_json::json!({"slot": "openrouter", "secret": SECRET}),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(&error, DaemonError::PolicyDenied(message) if message.contains(OPERATOR_ONLY_REFUSAL)),
                "{method}: {error}"
            );
        }
        // Nothing was written.
        assert_eq!(vault.state(Slot::Openrouter), CredentialState::Absent);
        assert!(!root.path().join("vault").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn set_check_list_clear_round_trip_never_returns_the_secret() {
        let root = tempfile::tempdir().unwrap();
        let vault = vault(
            root.path(),
            ScriptedProbe::new([
                http(
                    200,
                    r#"{"data":{"limit":10,"usage":1,"limit_remaining":9}}"#,
                ),
                http(402, ""),
            ]),
            &[],
        );
        let set = handle(
            &vault,
            METHOD_SET,
            false,
            &serde_json::json!({"slot": "openrouter", "secret": SECRET}),
        )
        .await
        .unwrap();
        assert!(!set.to_string().contains(SECRET));
        let set: ProviderCredentialMetadata = serde_json::from_value(set).unwrap();
        assert_eq!(set.state, CredentialState::Vault);
        assert_eq!(set.check.unwrap().class, CredentialCheckClass::Valid);

        let check = handle(
            &vault,
            METHOD_CHECK,
            false,
            &serde_json::json!({"slot": "openrouter"}),
        )
        .await
        .unwrap();
        let check: ProviderCredentialMetadata = serde_json::from_value(check).unwrap();
        assert_eq!(check.check.unwrap().class, CredentialCheckClass::Exhausted);

        let list = handle(&vault, METHOD_LIST, false, &serde_json::Value::Null)
            .await
            .unwrap();
        assert!(!list.to_string().contains(SECRET));
        let list: ListProviderCredentialsResult = serde_json::from_value(list).unwrap();
        assert_eq!(list.credentials.len(), Slot::ALL.len());

        let cleared = handle(
            &vault,
            METHOD_CLEAR,
            false,
            &serde_json::json!({"slot": "openrouter"}),
        )
        .await
        .unwrap();
        let cleared: ProviderCredentialMetadata = serde_json::from_value(cleared).unwrap();
        assert_eq!(cleared.state, CredentialState::Cleared);
        assert!(cleared.cleared_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn rotate_import_and_parse_errors_are_secret_free() {
        let root = tempfile::tempdir().unwrap();
        let vault = vault(
            root.path(),
            ScriptedProbe::new([ProbeOutcome::Timeout]),
            &[("GROQ_API_KEY", "sk-test-operator-groq")],
        );
        let error = handle(
            &vault,
            METHOD_ROTATE,
            false,
            &serde_json::json!({"slot": "anthropic", "secret": SECRET}),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DaemonError::InvalidParam(_)));
        assert!(!error.to_string().contains(SECRET));

        // A secret mistakenly placed in the slot field is never echoed.
        let error = handle(
            &vault,
            METHOD_SET,
            false,
            &serde_json::json!({"slot": SECRET, "secret": SECRET}),
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains(SECRET), "{error}");

        let error = handle(
            &vault,
            METHOD_LIST,
            false,
            &serde_json::json!({"unexpected": SECRET}),
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains(SECRET), "{error}");

        let imported = handle(&vault, METHOD_IMPORT, false, &serde_json::json!({}))
            .await
            .unwrap();
        assert!(!imported.to_string().contains("sk-test-operator-groq"));
        let imported: ImportProviderCredentialsResult = serde_json::from_value(imported).unwrap();
        assert_eq!(imported.imported.len(), 1);
        assert_eq!(imported.imported[0].slot, Slot::Groq);
        assert_eq!(vault.state(Slot::Groq), CredentialState::Vault);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn operator_method_set_is_exactly_the_six_vault_methods() {
        for method in OPERATOR_METHODS {
            assert!(is_operator_method(method));
        }
        assert!(!is_operator_method("GetDaemonConfig"));
    }
}
