//! Amazon Bedrock `InvokeModelWithResponseStream` reader for Claude models.
//!
//! Bedrock frames the stream as `application/vnd.amazon.eventstream` binary
//! messages instead of SSE. Each `chunk` event carries
//! `{"bytes": "<base64 Anthropic stream event JSON>"}`; the decoded event is
//! the same JSON the direct Messages API sends as SSE `data:`, with its type
//! in the `type` field. `exception` messages carry a Bedrock error.
//!
//! Frame layout (all integers big-endian):
//! `total_len:u32 | headers_len:u32 | prelude_crc:u32 | headers | payload | message_crc:u32`.
//! CRCs are not verified: the stream already runs over TLS, and a corrupt
//! frame still fails JSON/base64 decoding.

use super::errors::{ProviderError, ProviderErrorClass};
use super::sse::{AnthropicStreamResult, AnthropicStreamState, stream_disconnect};
use super::types::StreamChunk;
use crate::error::DaemonError;
use base64::Engine as _;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const PRELUDE_LEN: usize = 12;
const MIN_MESSAGE_LEN: usize = PRELUDE_LEN + 4;
/// Bedrock stream messages are small; this bounds a corrupt length prefix.
const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// One decoded event-stream message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EventStreamMessage {
    pub message_type: Option<String>,
    pub event_type: Option<String>,
    pub exception_type: Option<String>,
    pub payload: Vec<u8>,
}

/// Pop one complete message from the front of `buffer`, or `Ok(None)` when
/// more bytes are needed.
pub(crate) fn decode_message(buffer: &mut Vec<u8>) -> Result<Option<EventStreamMessage>, ()> {
    if buffer.len() < PRELUDE_LEN {
        return Ok(None);
    }
    let total_len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    let headers_len = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
    if !(MIN_MESSAGE_LEN..=MAX_MESSAGE_LEN).contains(&total_len)
        || headers_len > total_len - MIN_MESSAGE_LEN
    {
        return Err(());
    }
    if buffer.len() < total_len {
        return Ok(None);
    }
    let message: Vec<u8> = buffer.drain(..total_len).collect();
    let headers = &message[PRELUDE_LEN..PRELUDE_LEN + headers_len];
    let payload = message[PRELUDE_LEN + headers_len..total_len - 4].to_vec();
    let mut decoded = EventStreamMessage {
        message_type: None,
        event_type: None,
        exception_type: None,
        payload,
    };
    for (name, value) in parse_headers(headers)? {
        match name.as_str() {
            ":message-type" => decoded.message_type = value,
            ":event-type" => decoded.event_type = value,
            ":exception-type" => decoded.exception_type = value,
            _ => {}
        }
    }
    Ok(Some(decoded))
}

/// Header `(name, string value)` pairs; non-string values are skipped.
fn parse_headers(mut bytes: &[u8]) -> Result<Vec<(String, Option<String>)>, ()> {
    fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8], ()> {
        if bytes.len() < count {
            return Err(());
        }
        let (head, tail) = bytes.split_at(count);
        *bytes = tail;
        Ok(head)
    }
    fn length_prefixed<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], ()> {
        let len = take(bytes, 2)?;
        take(bytes, u16::from_be_bytes([len[0], len[1]]) as usize)
    }
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        let name_len = take(&mut bytes, 1)?[0] as usize;
        let name = String::from_utf8_lossy(take(&mut bytes, name_len)?).into_owned();
        let value = match take(&mut bytes, 1)?[0] {
            0 | 1 => None,
            2 => take(&mut bytes, 1).map(|_| None)?,
            3 => take(&mut bytes, 2).map(|_| None)?,
            4 => take(&mut bytes, 4).map(|_| None)?,
            5 | 8 => take(&mut bytes, 8).map(|_| None)?,
            6 => length_prefixed(&mut bytes).map(|_| None)?,
            7 => Some(String::from_utf8_lossy(length_prefixed(&mut bytes)?).into_owned()),
            9 => take(&mut bytes, 16).map(|_| None)?,
            _ => return Err(()),
        };
        headers.push((name, value));
    }
    Ok(headers)
}

fn malformed_stream() -> DaemonError {
    ProviderError {
        class: ProviderErrorClass::Transient,
        http_status: None,
        retry_after_ms: None,
        detail_code: "bedrock_stream_malformed".into(),
    }
    .into_daemon_error()
}

/// Map a Bedrock stream exception to the Harness provider-error classes.
fn stream_exception(exception_type: &str, payload: &[u8]) -> DaemonError {
    let message = serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let class = match exception_type {
        "throttlingException" => ProviderErrorClass::RateLimited,
        "serviceUnavailableException" | "modelNotReadyException" => ProviderErrorClass::Overloaded,
        "internalServerException" | "modelStreamErrorException" | "modelTimeoutException" => {
            ProviderErrorClass::Transient
        }
        "accessDeniedException" => ProviderErrorClass::Auth,
        "validationException"
            if message.contains("too long") || message.contains("context length") =>
        {
            ProviderErrorClass::ContextTooLong
        }
        _ => ProviderErrorClass::BadRequest,
    };
    ProviderError {
        class,
        http_status: None,
        retry_after_ms: None,
        detail_code: format!("bedrock_{exception_type}"),
    }
    .into_daemon_error()
}

/// Apply one decoded message to the Anthropic stream state.
async fn apply_message(
    message: EventStreamMessage,
    state: &mut AnthropicStreamState,
    chunk_tx: &mpsc::Sender<StreamChunk>,
) -> Result<(), DaemonError> {
    match message.message_type.as_deref() {
        Some("exception") | Some("error") => {
            let exception = message.exception_type.as_deref().unwrap_or("unknown");
            return Err(stream_exception(exception, &message.payload));
        }
        Some("event") | None => {}
        Some(_) => return Ok(()),
    }
    if message.event_type.as_deref() != Some("chunk") {
        return Ok(());
    }
    let envelope: serde_json::Value =
        serde_json::from_slice(&message.payload).map_err(|_| malformed_stream())?;
    let encoded = envelope
        .get("bytes")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(malformed_stream)?;
    let event = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| malformed_stream())?;
    let event: serde_json::Value =
        serde_json::from_slice(&event).map_err(|_| malformed_stream())?;
    let event_type = event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    state.apply(&event_type, &event, chunk_tx).await;
    Ok(())
}

/// Read a Bedrock Claude response stream.
pub(crate) async fn read_bedrock_anthropic_stream(
    resp: reqwest::Response,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<AnthropicStreamResult, DaemonError> {
    let mut state = AnthropicStreamState::default();
    let mut buffer: Vec<u8> = Vec::new();
    let mut byte_stream = resp.bytes_stream();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Ok(state.cancelled()),
            chunk = byte_stream.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
            Some(Err(_)) => return Err(stream_disconnect()),
            None => break,
        }
        while let Some(message) = decode_message(&mut buffer).map_err(|()| malformed_stream())? {
            apply_message(message, &mut state, chunk_tx).await?;
        }
    }
    if !buffer.is_empty() {
        return Err(stream_disconnect());
    }
    state.finish()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Encode one event-stream message with string headers (CRCs zeroed).
    fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut encoded_headers = Vec::new();
        for (name, value) in headers {
            encoded_headers.push(name.len() as u8);
            encoded_headers.extend_from_slice(name.as_bytes());
            encoded_headers.push(7);
            encoded_headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            encoded_headers.extend_from_slice(value.as_bytes());
        }
        let total = PRELUDE_LEN + encoded_headers.len() + payload.len() + 4;
        let mut message = Vec::new();
        message.extend_from_slice(&(total as u32).to_be_bytes());
        message.extend_from_slice(&(encoded_headers.len() as u32).to_be_bytes());
        message.extend_from_slice(&[0; 4]);
        message.extend_from_slice(&encoded_headers);
        message.extend_from_slice(payload);
        message.extend_from_slice(&[0; 4]);
        message
    }

    fn chunk(event: serde_json::Value) -> Vec<u8> {
        let bytes = base64::engine::general_purpose::STANDARD.encode(event.to_string());
        frame(
            &[(":message-type", "event"), (":event-type", "chunk")],
            serde_json::json!({ "bytes": bytes }).to_string().as_bytes(),
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn decodes_split_frames_and_skips_non_string_headers() {
        let mut stream = frame(&[(":event-type", "chunk")], b"{}");
        stream.extend(frame(
            &[(":message-type", "exception")],
            b"{\"message\":\"x\"}",
        ));
        let mut buffer = stream[..5].to_vec();
        assert_eq!(decode_message(&mut buffer), Ok(None));
        buffer.extend_from_slice(&stream[5..]);
        let first = decode_message(&mut buffer).unwrap().unwrap();
        assert_eq!(first.event_type.as_deref(), Some("chunk"));
        assert_eq!(first.payload, b"{}");
        let second = decode_message(&mut buffer).unwrap().unwrap();
        assert_eq!(second.message_type.as_deref(), Some("exception"));
        assert!(buffer.is_empty());

        // A byte-valued header (type 2) is skipped, not misparsed.
        let headers = [
            &[4u8][..],
            b"flag",
            &[2, 9],
            &[11],
            b":event-type",
            &[7, 0, 5],
            b"chunk",
        ]
        .concat();
        assert_eq!(
            parse_headers(&headers).unwrap(),
            vec![
                ("flag".to_string(), None),
                (":event-type".to_string(), Some("chunk".to_string()))
            ]
        );
        let mut corrupt = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(decode_message(&mut corrupt), Err(()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn chunk_events_drive_anthropic_text_and_tool_use() {
        let events = [
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 11}}}),
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
            serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell"}}),
            serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}}),
            serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
            serde_json::json!({"type": "message_stop"}),
        ];
        let mut stream = Vec::new();
        for event in events {
            stream.extend(chunk(event));
        }
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = AnthropicStreamState::default();
        while let Some(message) = decode_message(&mut stream).unwrap() {
            apply_message(message, &mut state, &tx).await.unwrap();
        }
        let (content, tool_calls, usage, stop) = state.finish().unwrap();
        assert_eq!(content, "hi");
        assert_eq!(rx.recv().await.unwrap().delta_text, "hi");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "shell");
        assert_eq!(tool_calls[0].arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(stop.as_deref(), Some("tool_use"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn exceptions_map_to_provider_error_classes() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = AnthropicStreamState::default();
        let mut stream = frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
            ],
            b"{\"message\":\"Too many requests\"}",
        );
        let message = decode_message(&mut stream).unwrap().unwrap();
        let error = apply_message(message, &mut state, &tx).await.unwrap_err();
        let provider_error = ProviderError::from_daemon_error(&error).unwrap();
        assert_eq!(provider_error.class, ProviderErrorClass::RateLimited);
        assert!(provider_error.retryable());
        assert_eq!(provider_error.detail_code, "bedrock_throttlingException");
    }
}
