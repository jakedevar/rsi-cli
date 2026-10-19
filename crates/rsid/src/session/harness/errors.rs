//! Safe provider failure classification shared by the HTTP adapters and loop.
//!
//! The classification values live in `rsid-harness`; this module adds the daemon's
//! structured-RPC wrapping and the response reader that needs `reqwest`.

use crate::error::DaemonError;
pub use rsid_harness::errors::{ProviderError, ProviderErrorClass, from_http};

/// Conversion between a classified provider failure and the daemon error carrying it.
pub trait ProviderErrorExt: Sized {
    fn into_daemon_error(self) -> DaemonError;
    fn from_daemon_error(error: &DaemonError) -> Option<Self>;
}

impl ProviderErrorExt for ProviderError {
    fn into_daemon_error(self) -> DaemonError {
        let data = serde_json::json!({"kind": "provider_error", "provider_error": self});
        DaemonError::StructuredRpc {
            rpc_code: -32000,
            message: format!("provider_error:{}", self.detail_code),
            data,
        }
    }

    fn from_daemon_error(error: &DaemonError) -> Option<Self> {
        match error {
            DaemonError::StructuredRpc { data, .. }
                if data.get("kind")?.as_str()? == "provider_error" =>
            {
                serde_json::from_value(data.get("provider_error")?.clone()).ok()
            }
            _ => None,
        }
    }
}

pub fn transport(error: DaemonError) -> DaemonError {
    match &error {
        DaemonError::Process(message) if message.contains(" HTTP error:") => ProviderError {
            class: ProviderErrorClass::Transient,
            http_status: None,
            retry_after_ms: None,
            detail_code: "transport_error".into(),
        }
        .into_daemon_error(),
        _ => error,
    }
}

pub async fn classify_response(response: reqwest::Response) -> DaemonError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    let bounded: String = body.chars().take(64 * 1024).collect();
    from_http(status, &bounded, retry_after.as_deref()).into_daemon_error()
}
