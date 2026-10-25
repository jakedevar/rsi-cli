//! Server-Sent Events parser facade (the parsers live in `rsid-harness`, #2648).
//!
//! The daemon adapters hand the parsers a `reqwest` byte stream and convert the
//! classified provider failure into the daemon's structured error.

use super::errors::ProviderErrorExt;
use super::types::StreamChunk;
use crate::error::DaemonError;
use rsid_harness::idle::{STREAM_IDLE_TIMEOUT, with_idle_timeout};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[allow(unused_imports)] // Preserves the old module path for the adapters and `openai.rs`.
pub use rsid_harness::sse::{
    AccumulatedToolCall, AnthropicStreamResult, OpenAiStreamResult, ThinkStripFilter,
    apply_chat_usage, chat_warning, merge_tool_name, reported_cost_usd,
};

/// Read an OpenAI-format SSE stream, emitting chunks and accumulating the response.
pub async fn read_openai_sse_stream(
    resp: reqwest::Response,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<OpenAiStreamResult, DaemonError> {
    let (stream, idle) = with_idle_timeout(resp.bytes_stream(), STREAM_IDLE_TIMEOUT);
    rsid_harness::sse::read_openai_sse_stream(stream, chunk_tx, cancel)
        .await
        .map_err(|error| idle.classify(error).into_daemon_error())
}

/// Read an Anthropic-format SSE stream (`event: TYPE` + `data: {json}` pairs).
pub async fn read_anthropic_sse_stream(
    resp: reqwest::Response,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<AnthropicStreamResult, DaemonError> {
    let (stream, idle) = with_idle_timeout(resp.bytes_stream(), STREAM_IDLE_TIMEOUT);
    rsid_harness::sse::read_anthropic_sse_stream(stream, chunk_tx, cancel)
        .await
        .map_err(|error| idle.classify(error).into_daemon_error())
}
