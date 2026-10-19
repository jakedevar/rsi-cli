//! Amazon Bedrock `InvokeModelWithResponseStream` reader facade (the event-stream
//! decoder lives in `rsid-harness`, #2648).

use super::errors::ProviderErrorExt;
use super::sse::AnthropicStreamResult;
use super::types::StreamChunk;
use crate::error::DaemonError;
use rsid_harness::idle::{STREAM_IDLE_TIMEOUT, with_idle_timeout};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Read a Bedrock Claude response stream.
pub(crate) async fn read_bedrock_anthropic_stream(
    resp: reqwest::Response,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<AnthropicStreamResult, DaemonError> {
    let (stream, idle) = with_idle_timeout(resp.bytes_stream(), STREAM_IDLE_TIMEOUT);
    rsid_harness::bedrock_stream::read_bedrock_anthropic_stream(stream, chunk_tx, cancel)
        .await
        .map_err(|error| idle.classify(error).into_daemon_error())
}
