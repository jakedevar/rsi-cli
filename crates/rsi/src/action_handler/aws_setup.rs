//! First-run AWS setup (Issue #1407): `:aws-setup`.
//!
//! - `:aws-setup <region>` writes the daemon setting `bedrock_region`, then
//!   opens the masked Provider Keys form for the Bedrock slot. The key goes
//!   only to the key vault through `SetProviderCredential` (see
//!   `action_handler::provider_credentials` for the secret-handling
//!   contract); once it is stored the setup is verified.
//! - `:aws-setup verify [model]` runs the live check alone.
//! - `:aws-setup` shows the profile, the region and the usage.
//!
//! The verification result (`BedrockSetupCheck`) is secret-free; it is shown
//! as a notification and never logged with a credential.

use crate::app::App;
use rsi_common::provider_credentials::ProviderCredentialSlot;
use rsi_common::provider_profile::{
    BEDROCK_REGION_FIELD, BedrockSetupCheck, BedrockSetupStage, PROVIDER_PROFILE_FIELD,
};

use super::daemon_config::require_authoritative_config;

/// Shown once when the TUI first sees `aws_only` with no region set.
pub const FIRST_RUN_PROMPT: &str = "AWS-only provider profile: run :aws-setup <region> (for example :aws-setup us-east-1) to store the Bedrock key in the key vault and verify it";

const USAGE: &str =
    "Usage: :aws-setup <region> (then enter the Bedrock key) | :aws-setup verify [model]";

/// One line describing a setup check. Built only from the secret-free
/// result fields.
#[must_use]
pub fn describe_check(check: &BedrockSetupCheck) -> String {
    let region = check.region.as_deref().unwrap_or("none");
    if check.ok {
        return format!(
            "AWS setup verified: region {region}, model {}, Bedrock key from {:?}",
            check.model, check.credential_state
        );
    }
    let stage = match check.stage {
        BedrockSetupStage::Model => "model",
        BedrockSetupStage::Region => "region",
        BedrockSetupStage::Credential => "credential",
        BedrockSetupStage::Invoke => "Bedrock call",
    };
    let status = check
        .http_status
        .map_or_else(String::new, |status| format!(", HTTP {status}"));
    format!(
        "AWS setup failed at {stage}: {} ({}{status}; region {region}, model {})",
        check.message, check.detail_code, check.model
    )
}

/// Run `VerifyBedrockSetup` and notify the secret-free result.
#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn verify_bedrock_setup(app: &mut App, model: Option<String>) {
    app.aws_setup_verify_pending = false;
    match app.client.verify_bedrock_setup(model).await {
        Ok(check) if check.ok => app.notify_success(describe_check(&check)),
        Ok(check) => app.notify_error(describe_check(&check)),
        Err(error) => app.notify_error(format!("AWS setup check failed: {error}")),
    }
}

/// `:aws-setup [<region> | verify [model]]`.
#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn aws_setup(app: &mut App, args: &str) {
    if !require_authoritative_config(app) {
        return;
    }
    let mut words = args.split_whitespace();
    match words.next() {
        None => match app.client.get_daemon_config().await {
            Ok(config) => {
                let profile = config[PROVIDER_PROFILE_FIELD].as_str().unwrap_or("all");
                let region = config[BEDROCK_REGION_FIELD]
                    .as_str()
                    .filter(|region| !region.is_empty())
                    .unwrap_or("unset (environment fallback)");
                app.notify(format!(
                    "Provider profile: {profile}; Bedrock region: {region}. {USAGE}"
                ));
            }
            Err(error) => app.notify_error(format!("Failed to read the AWS setup: {error}")),
        },
        Some("verify") => {
            let model = words.next().map(str::to_string);
            verify_bedrock_setup(app, model).await;
        }
        Some(region) => {
            if words.next().is_some() {
                app.notify_error(USAGE);
                return;
            }
            let value = serde_json::json!(region);
            if let Err(error) = rsi_common::provider_profile::normalize_bedrock_region(&value) {
                app.notify_error(format!("AWS setup: {error}"));
                return;
            }
            match app
                .client
                .update_daemon_config("bedrock_region", value)
                .await
            {
                Ok(()) => {
                    app.aws_setup_verify_pending = true;
                    crate::overlay::provider_credential_form::open_provider_credential_form(
                        app,
                        ProviderCredentialSlot::Bedrock,
                        false,
                    );
                    app.notify(format!(
                        "Bedrock region set to {region}. Enter the Bedrock API key (stored in the key vault only); Esc keeps the stored key, then run :aws-setup verify"
                    ));
                }
                Err(error) => {
                    app.notify_error(format!("Failed to set the Bedrock region: {error}"));
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rsi_common::provider_credentials::CredentialState;

    const SECRET: &str = "bedrock-api-key-tui-canary-0003";

    fn check(ok: bool, stage: BedrockSetupStage, detail: &str) -> BedrockSetupCheck {
        BedrockSetupCheck {
            ok,
            stage,
            region: Some("us-east-1".into()),
            model: rsi_common::provider_profile::AWS_ONLY_DEFAULT_MODEL.into(),
            credential_state: CredentialState::Vault,
            credential_fingerprint: Some("ab12ef34".into()),
            http_status: (!ok).then_some(401),
            detail_code: detail.into(),
            message: "fixed text".into(),
        }
    }

    #[test]
    fn describe_check_reports_success_and_failure_from_metadata_only() {
        let ok = describe_check(&check(true, BedrockSetupStage::Invoke, "ok"));
        assert!(ok.starts_with("AWS setup verified"), "{ok}");
        assert!(ok.contains("us-east-1"), "{ok}");
        let failed = describe_check(&check(false, BedrockSetupStage::Invoke, "http_401"));
        assert!(
            failed.starts_with("AWS setup failed at Bedrock call"),
            "{failed}"
        );
        assert!(failed.contains("http_401, HTTP 401"), "{failed}");
        for text in [ok, failed] {
            assert!(!text.contains(SECRET));
        }
    }

    #[tokio::test]
    async fn malformed_region_is_refused_before_any_rpc() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        aws_setup(&mut app, "US East").await;
        assert!(!app.aws_setup_verify_pending);
        assert!(matches!(app.overlay, crate::types::OverlayState::None));
    }
}
