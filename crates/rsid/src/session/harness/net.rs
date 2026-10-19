//! HTTP timeouts of the direct-API model providers (#3163).
//!
//! A model response can legitimately stream for much longer than any fixed
//! total timeout, so a streamed call is bounded by a connect timeout, the
//! stream idle timeout (`rsid_harness::idle`, reset on every read) and one
//! generous overall cap. A blocking call has no stream to watch, so it keeps a
//! plain total timeout.

use crate::error::DaemonError;
use std::time::Duration;

/// Time allowed to establish the connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on one streamed call, however steadily it flows.
pub const OVERALL_CAP: Duration = Duration::from_secs(3600);
/// Total timeout of a call that does not stream.
pub const BLOCKING_TIMEOUT: Duration = Duration::from_secs(300);

/// The client every direct-API provider uses.
pub fn client() -> Result<reqwest::Client, DaemonError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(OVERALL_CAP)
        .build()
        .map_err(|e| DaemonError::Process(format!("HTTP client build error: {e}")))
}

/// The total timeout of one request: the generous overall cap for a streamed
/// call, the shorter blocking timeout otherwise. Set in the request chain so
/// the builder reaches `bind_http` unshadowed.
pub const fn total_timeout(streaming: bool) -> Duration {
    if streaming {
        OVERALL_CAP
    } else {
        BLOCKING_TIMEOUT
    }
}
