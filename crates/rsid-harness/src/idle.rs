//! Stream idle timeout (#3163).
//!
//! A model response may stream for far longer than any fixed total timeout, so
//! the only clock a stream gets is the silence between two reads: it restarts on
//! every read from the connection and ends the stream when it runs out. The
//! readers see an ordinary truncated stream (a transient `stream_disconnected`);
//! [`IdleGuard::classify`] renames that failure `stream_idle_timeout` so the
//! cause stays visible in the event stream and the logs.

use crate::errors::ProviderError;
use futures::{Stream, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Default time a stream may stay silent before it is treated as dead.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Detail code of a stream that went silent for [`STREAM_IDLE_TIMEOUT`].
pub const STREAM_IDLE_DETAIL: &str = "stream_idle_timeout";

/// Remembers whether the guarded stream ended because it went idle.
#[derive(Debug, Clone, Default)]
pub struct IdleGuard(Arc<AtomicBool>);

impl IdleGuard {
    /// True once the guarded stream was cut off for silence.
    pub fn fired(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Name an idle cut-off as such; every other failure is returned unchanged.
    pub fn classify(&self, error: ProviderError) -> ProviderError {
        if self.fired() && error.detail_code == crate::sse::stream_disconnect().detail_code {
            ProviderError::transient(STREAM_IDLE_DETAIL)
        } else {
            error
        }
    }
}

/// Wrap `stream` so it ends when no item arrives within `idle`.
pub fn with_idle_timeout<S, B, E>(
    stream: S,
    idle: Duration,
) -> (impl Stream<Item = Result<B, E>>, IdleGuard)
where
    S: Stream<Item = Result<B, E>>,
{
    let guard = IdleGuard::default();
    let flag = Arc::clone(&guard.0);
    let guarded = futures::stream::unfold(Some(Box::pin(stream)), move |state| {
        let flag = Arc::clone(&flag);
        async move {
            let mut inner = state?;
            match tokio::time::timeout(idle, inner.next()).await {
                Ok(Some(item)) => Some((item, Some(inner))),
                Ok(None) => None,
                Err(_) => {
                    flag.store(true, Ordering::Release);
                    None
                }
            }
        }
    });
    (guarded, guard)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sse::read_openai_sse_stream;
    use crate::types::StreamChunk;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    type Item = Result<Vec<u8>, std::io::Error>;

    fn data(json: &str) -> Vec<u8> {
        format!("data: {json}\n\n").into_bytes()
    }

    fn text_chunk(text: &str) -> Vec<u8> {
        data(&format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}"#
        ))
    }

    /// A byte stream whose pieces arrive after the given gaps.
    fn paced(pieces: Vec<(Duration, Vec<u8>)>) -> impl Stream<Item = Item> {
        futures::stream::unfold(pieces.into_iter(), |mut pieces| async move {
            let (gap, bytes) = pieces.next()?;
            tokio::time::sleep(gap).await;
            Some((Ok(bytes), pieces))
        })
    }

    async fn read(
        pieces: Vec<(Duration, Vec<u8>)>,
        idle: Duration,
    ) -> (
        Result<(String, usize), ProviderError>,
        IdleGuard,
        Vec<StreamChunk>,
    ) {
        let (stream, guard) = with_idle_timeout(paced(pieces), idle);
        let (tx, mut rx) = mpsc::channel(64);
        let outcome = read_openai_sse_stream(stream, &tx, &CancellationToken::new())
            .await
            .map(|result| (result.content, result.tool_calls.len()))
            .map_err(|error| guard.classify(error));
        drop(tx);
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        (outcome, guard, chunks)
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_silent_past_the_idle_timeout_is_cut_off_and_named() {
        let (outcome, guard, chunks) = read(
            vec![
                (Duration::ZERO, text_chunk("partial")),
                (Duration::from_secs(301), text_chunk("never read")),
            ],
            STREAM_IDLE_TIMEOUT,
        )
        .await;
        let error = outcome.unwrap_err();
        assert!(guard.fired());
        assert_eq!(error.detail_code, STREAM_IDLE_DETAIL);
        assert!(error.retryable());
        // The partial output was forwarded before the silence began.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].delta_text, "partial");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_but_flowing_stream_longer_than_the_old_total_timeout_completes() {
        // Eight pieces 100 s apart: 800 s in all, never silent for the 300 s limit.
        let mut pieces: Vec<(Duration, Vec<u8>)> = (0..7)
            .map(|n| (Duration::from_secs(100), text_chunk(&format!("{n};"))))
            .collect();
        pieces.push((
            Duration::from_secs(100),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                .to_vec(),
        ));
        let started = tokio::time::Instant::now();
        let (outcome, guard, chunks) = read(pieces, STREAM_IDLE_TIMEOUT).await;
        let (content, _) = outcome.unwrap();
        assert!(!guard.fired());
        assert_eq!(content, "0;1;2;3;4;5;6;");
        assert_eq!(chunks.len(), 7);
        assert!(started.elapsed() >= Duration::from_secs(800));
    }

    #[test]
    fn only_a_disconnect_is_renamed_and_only_after_the_guard_fired() {
        let guard = IdleGuard::default();
        let disconnect = crate::sse::stream_disconnect();
        assert_eq!(guard.classify(disconnect.clone()), disconnect);
        guard.0.store(true, Ordering::Release);
        assert_eq!(guard.classify(disconnect).detail_code, STREAM_IDLE_DETAIL);
        let other = ProviderError::transient("server_error");
        assert_eq!(guard.classify(other.clone()), other);
    }
}
