//! First-run AWS setup check (Issue #1407).
//!
//! The operator enters the AWS region (daemon setting `bedrock_region`) and
//! the Bedrock credential (key vault, `SetProviderCredential`), then calls the
//! operator-only `VerifyBedrockSetup`. It resolves the region and credential
//! the way a launch does and makes one live `InvokeModel` call (one output
//! token) on a Bedrock Claude model. The result is a [`BedrockSetupCheck`]:
//! stage, status, a stable detail code and fixed text. The credential and the
//! response body never reach it, and nothing here logs either.

use crate::error::{DaemonError, Result};
use crate::vault::{ProbeOutcome, SecretString, Slot, VaultHandle};
use rsi_common::provider_credentials::{CredentialCheckClass, CredentialState};
use rsi_common::provider_profile::{
    AWS_ONLY_DEFAULT_MODEL, BedrockSetupCheck, BedrockSetupStage, METHOD_VERIFY_BEDROCK_SETUP,
    VerifyBedrockSetupParams, is_bedrock_claude_model,
};

/// Stable refusal code for a session-attributed call.
pub const OPERATOR_ONLY_REFUSAL: &str = "bedrock_setup_operator_only";

/// The live Bedrock call behind a trait, so tests stub it (no network).
#[async_trait::async_trait]
pub trait BedrockInvokeProbe: Send + Sync {
    async fn invoke(&self, region: &str, model: &str, secret: &SecretString) -> ProbeOutcome;
}

/// Production probe: `POST /model/<model>/invoke` on `bedrock-runtime`.
#[derive(Clone, Debug, Default)]
pub struct HttpBedrockInvokeProbe {
    /// `None` = `https://bedrock-runtime.<region>.amazonaws.com`.
    pub base: Option<String>,
}

#[async_trait::async_trait]
impl BedrockInvokeProbe for HttpBedrockInvokeProbe {
    async fn invoke(&self, region: &str, model: &str, secret: &SecretString) -> ProbeOutcome {
        let Some(http) = crate::vault::check::probe_client() else {
            return ProbeOutcome::Failed;
        };
        let url = match &self.base {
            Some(base) => format!("{base}/model/{}/invoke", model.replace(':', "%3A")),
            None => crate::bedrock::runtime_invoke_url(region, model, false),
        };
        let request = http
            .post(url)
            .bearer_auth(secret.expose())
            .json(&serde_json::json!({
                "anthropic_version": "bedrock-2023-05-31",
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "ping"}],
            }));
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return ProbeOutcome::Timeout,
            Err(error) if error.is_connect() => return ProbeOutcome::Connect,
            Err(_) => return ProbeOutcome::Failed,
        };
        crate::vault::check::read_probe_response(response).await
    }
}

/// Whether `model` has Bedrock's model-id shape (bounded, `[a-z0-9.:-]`).
fn model_id_echo_safe(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && model.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b':')
        })
}

/// Fixed operator-facing text for a detail code. Never built from a body.
#[must_use]
pub fn setup_message(detail_code: &str) -> &'static str {
    match detail_code {
        "ok" => "Bedrock answered: the region, credential and model work.",
        "not_bedrock_claude_model" => {
            "The model is not a Bedrock Claude model id ([<geo>.]anthropic.claude-*)."
        }
        "region_missing" => {
            "No valid AWS region: set bedrock_region (or AWS_REGION) to a region such as us-east-1."
        }
        "credential_cleared" => {
            "The Bedrock credential was cleared from the key vault; set it again."
        }
        "credential_missing" => "No Bedrock credential: store a Bedrock API key in the key vault.",
        "http_401" | "unknown_key_body" => "Bedrock rejected the credential (not authenticated).",
        "model_access_denied" => {
            "The credential authenticated but may not invoke this model; enable model access in \
             the Bedrock console or pick another model."
        }
        "http_402" | "credit_exhausted_body" => "Bedrock reports the account out of credit.",
        "rate_limited" => "Bedrock throttled the check; try again shortly.",
        "server_error" => "Bedrock returned a server error; try again shortly.",
        "timeout" => "The Bedrock call timed out; check the network and region.",
        "connect_error" => "Could not connect to Bedrock; check the network and region.",
        _ => "The Bedrock call did not succeed; check the region, credential and model access.",
    }
}

fn result(
    stage: BedrockSetupStage,
    region: Option<String>,
    model: &str,
    vault: &VaultHandle,
    http_status: Option<u16>,
    detail_code: &str,
    ok: bool,
) -> BedrockSetupCheck {
    let metadata = vault.metadata(Slot::Bedrock);
    BedrockSetupCheck {
        ok,
        stage,
        region,
        // Only a Bedrock Claude model id is echoed: anything else (a key
        // pasted into the wrong field has model-id shape too) is not.
        model: if model_id_echo_safe(model) && is_bedrock_claude_model(model) {
            model.to_string()
        } else {
            "<invalid model id>".to_string()
        },
        credential_state: metadata.state,
        credential_fingerprint: metadata.fingerprint,
        http_status,
        detail_code: detail_code.to_string(),
        message: setup_message(detail_code).to_string(),
    }
}

/// Resolve the region and credential like a Bedrock launch and make one live
/// call with `probe`. Secret-free by construction (see the module doc).
pub async fn verify(
    vault: &VaultHandle,
    model: Option<&str>,
    probe: &dyn BedrockInvokeProbe,
) -> BedrockSetupCheck {
    use BedrockSetupStage::{Credential, Invoke, Model, Region};
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(AWS_ONLY_DEFAULT_MODEL);
    if !model_id_echo_safe(model) || !is_bedrock_claude_model(model) {
        return result(
            Model,
            None,
            model,
            vault,
            None,
            "not_bedrock_claude_model",
            false,
        );
    }
    let Ok(region) = crate::bedrock::region_from(vault) else {
        return result(Region, None, model, vault, None, "region_missing", false);
    };
    let resolved = match crate::bedrock::credential_from(vault) {
        Ok(resolved) => resolved,
        Err(_) => {
            let code = if vault.state(Slot::Bedrock) == CredentialState::Cleared {
                "credential_cleared"
            } else {
                "credential_missing"
            };
            return result(Credential, Some(region), model, vault, None, code, false);
        }
    };
    let outcome = probe.invoke(&region, model, &resolved.secret).await;
    drop(resolved);
    let check = crate::vault::check::classify(Slot::Bedrock, &outcome, chrono::Utc::now(), None);
    let ok = check.class == CredentialCheckClass::Valid;
    result(
        Invoke,
        Some(region),
        model,
        vault,
        check.http_status,
        &check.detail_code,
        ok,
    )
}

/// Dispatch `VerifyBedrockSetup`. `session_token_present` is the raw transport
/// fact; any attributed caller is refused before parsing.
///
/// # Errors
///
/// `PolicyDenied` for a tokened caller, `InvalidParam` for a malformed
/// request (never echoing serde diagnostics). No error carries a secret.
pub async fn handle(
    vault: &VaultHandle,
    method: &str,
    session_token_present: bool,
    params: &serde_json::Value,
    probe: &dyn BedrockInvokeProbe,
) -> Result<serde_json::Value> {
    if session_token_present {
        return Err(DaemonError::PolicyDenied(format!(
            "{OPERATOR_ONLY_REFUSAL}: `{method}` is operator-only and is not available to session-attributed callers"
        )));
    }
    if method != METHOD_VERIFY_BEDROCK_SETUP {
        return Err(DaemonError::InvalidParam(format!(
            "unknown Bedrock setup method `{method}`"
        )));
    }
    let request: VerifyBedrockSetupParams = if params.is_null() {
        VerifyBedrockSetupParams::default()
    } else {
        serde_json::from_value(params.clone()).map_err(|_| {
            DaemonError::InvalidParam(format!(
                "invalid {method} params; expected {{\"model\"?: \"<bedrock claude model id>\"}}"
            ))
        })?
    };
    let check = verify(vault, request.model.as_deref(), probe).await;
    serde_json::to_value(&check).map_err(DaemonError::from)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vault::check::tests::http;
    use crate::vault::{VaultHandleBuilder, VaultSettings};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SECRET: &str = "bedrock-api-key-test-setup-canary-0001";

    /// Stub Bedrock: records what it was asked, answers a scripted outcome.
    struct StubInvoke {
        outcome: ProbeOutcome,
        calls: AtomicUsize,
        seen: parking_lot::Mutex<Vec<(String, String, bool)>>,
    }

    impl StubInvoke {
        fn new(outcome: ProbeOutcome) -> Self {
            Self {
                outcome,
                calls: AtomicUsize::new(0),
                seen: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl BedrockInvokeProbe for StubInvoke {
        async fn invoke(&self, region: &str, model: &str, secret: &SecretString) -> ProbeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .push((region.into(), model.into(), secret.expose() == SECRET));
            self.outcome.clone()
        }
    }

    fn vault(root: &std::path::Path, region: &str) -> VaultHandle {
        let settings = Arc::new(VaultSettings::default());
        *settings.bedrock_region.write() = region.to_string();
        // No environment, no token generator, no network check: the setup
        // check must use the vault entry and the setting.
        VaultHandleBuilder::new(settings)
            .dir(root.join("vault"))
            .env(|_| None)
            .dynamic(Arc::new(NoGenerator))
            .probe(Arc::new(
                crate::vault::check::tests::ScriptedProbe::default(),
            ))
            .open()
            .unwrap()
    }

    struct NoGenerator;

    impl crate::vault::DynamicSource for NoGenerator {
        fn available(&self, _slot: Slot) -> bool {
            false
        }
        fn generate(&self, _slot: Slot) -> Option<std::result::Result<SecretString, String>> {
            None
        }
    }

    fn assert_secret_free(value: &serde_json::Value) {
        let text = value.to_string();
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains("canary"), "{text}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn setup_check_success_uses_the_vault_credential_and_region_and_is_secret_free() {
        let root = tempfile::tempdir().unwrap();
        let vault = vault(root.path(), "eu-west-1");
        vault.set(Slot::Bedrock, SECRET).unwrap();
        let stub = StubInvoke::new(http(200, r#"{"content":[{"text":"pong"}]}"#));
        let value = handle(
            &vault,
            METHOD_VERIFY_BEDROCK_SETUP,
            false,
            &serde_json::Value::Null,
            &stub,
        )
        .await
        .unwrap();
        assert_secret_free(&value);
        let check: BedrockSetupCheck = serde_json::from_value(value).unwrap();
        assert!(check.ok);
        assert_eq!(check.stage, BedrockSetupStage::Invoke);
        assert_eq!(check.detail_code, "ok");
        assert_eq!(check.region.as_deref(), Some("eu-west-1"));
        assert_eq!(check.model, AWS_ONLY_DEFAULT_MODEL);
        assert_eq!(check.credential_state, CredentialState::Vault);
        assert!(check.credential_fingerprint.is_some());
        assert_eq!(
            stub.seen.lock().as_slice(),
            [(
                "eu-west-1".to_string(),
                AWS_ONLY_DEFAULT_MODEL.to_string(),
                true
            )]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn setup_check_failures_are_secret_free_and_name_the_stage() {
        let root = tempfile::tempdir().unwrap();
        // Rejected credential: the response body (which may echo request
        // detail) never reaches the result.
        let vault_a = vault(root.path(), "us-east-1");
        vault_a.set(Slot::Bedrock, SECRET).unwrap();
        let body = format!(r#"{{"message":"invalid key {SECRET}"}}"#);
        let stub = StubInvoke::new(http(401, &body));
        let value = handle(
            &vault_a,
            METHOD_VERIFY_BEDROCK_SETUP,
            false,
            &serde_json::json!({"model": "global.anthropic.claude-opus-5-5"}),
            &stub,
        )
        .await
        .unwrap();
        assert_secret_free(&value);
        let check: BedrockSetupCheck = serde_json::from_value(value).unwrap();
        assert!(!check.ok);
        assert_eq!(check.stage, BedrockSetupStage::Invoke);
        assert_eq!(check.detail_code, "http_401");
        assert_eq!(check.http_status, Some(401));
        assert_eq!(check.model, "global.anthropic.claude-opus-5-5");

        // No credential: refused before any call.
        let empty = tempfile::tempdir().unwrap();
        let vault_b = vault(empty.path(), "us-east-1");
        let stub = StubInvoke::new(http(200, "{}"));
        let check = verify(&vault_b, None, &stub).await;
        assert_eq!(check.stage, BedrockSetupStage::Credential);
        assert_eq!(check.detail_code, "credential_missing");
        assert!(!check.ok);
        assert_eq!(stub.calls.load(Ordering::SeqCst), 0);

        // A value that is not a Bedrock Claude model id is never echoed and
        // never sent.
        let check = verify(&vault_a, Some(SECRET), &stub).await;
        assert_eq!(check.stage, BedrockSetupStage::Model);
        assert_eq!(check.detail_code, "not_bedrock_claude_model");
        assert_secret_free(&serde_json::to_value(&check).unwrap());
        let check = verify(&vault_a, Some("global.openai.gpt-5.6-sol"), &stub).await;
        assert_eq!(check.stage, BedrockSetupStage::Model);
        assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn setup_check_refuses_tokened_callers_and_malformed_params_without_echo() {
        let root = tempfile::tempdir().unwrap();
        let vault = vault(root.path(), "us-east-1");
        let stub = StubInvoke::new(http(200, "{}"));
        let error = handle(
            &vault,
            METHOD_VERIFY_BEDROCK_SETUP,
            true,
            &serde_json::Value::Null,
            &stub,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(OPERATOR_ONLY_REFUSAL), "{error}");
        let error = handle(
            &vault,
            METHOD_VERIFY_BEDROCK_SETUP,
            false,
            &serde_json::json!({"secret": SECRET}),
            &stub,
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains(SECRET), "{error}");
        assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn http_probe_invokes_the_claude_model_with_the_bearer_key() {
        use wiremock::matchers::{header, method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/model/us.anthropic.claude-sonnet-5-v1%3A0/invoke"))
            .and(header("authorization", format!("Bearer {SECRET}").as_str()))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
        let probe = HttpBedrockInvokeProbe {
            base: Some(server.uri()),
        };
        let outcome = probe
            .invoke(
                "us-east-1",
                AWS_ONLY_DEFAULT_MODEL,
                &SecretString::new(SECRET.into()),
            )
            .await;
        assert!(matches!(outcome, ProbeOutcome::Http { status: 200, .. }));
    }
}
