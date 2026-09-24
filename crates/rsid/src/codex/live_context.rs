//! Incremental recovery of the usage notifications omitted by `exec --json`.
//!
//! Owned by the stdout task: no detached watcher outlives the invocation. Disk
//! work runs on the blocking pool with bounded reads, discovery, and buffers.

use super::{CodexTranscriptBoundary, CodexTranscriptWatermark, codex_context_event};
use crate::claude::StreamEvent;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_READ_BYTES: usize = 1024 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const DISCOVERY_ENTRIES_PER_POLL: usize = 256;

/// The actual stdout wait path: polling continues even if no JSONL items arrive.
/// Returning on stdout EOF also drops the reader; there is no watcher to strand.
pub(super) async fn next_line_with_context<R: tokio::io::AsyncRead + Unpin>(
    lines: &mut crate::process_control::BoundedLines<R>,
    context_reader: &mut Option<LiveContextReader>,
    thread_id: Option<&str>,
    context_tick: &mut tokio::time::Interval,
    event_tx: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> Result<Option<String>, crate::process_control::BoundedLineError> {
    loop {
        tokio::select! {
            line = lines.next_line() => return line,
            _ = context_tick.tick(), if thread_id.is_some() && context_reader.is_some() => {
                let Some(id) = thread_id else { continue; };
                let Some(mut reader) = context_reader.take() else { continue; };
                let id = id.to_string();
                match tokio::task::spawn_blocking(move || {
                    let event = reader.poll(&id);
                    (reader, event)
                }).await {
                    Ok((reader, event)) => {
                        *context_reader = Some(reader);
                        if let Some(event) = event
                            && event_tx.send(event).await.is_err()
                        {
                            return Ok(None);
                        }
                    }
                    Err(error) => tracing::warn!(%error, "Codex live context reader stopped"),
                }
            }
        }
    }
}

pub(super) struct LiveContextReader {
    root: Option<PathBuf>,
    discovery: Option<walkdir::IntoIter>,
    next_discovery: Instant,
    watermark: Option<CodexTranscriptWatermark>,
    disabled: bool,
    pending: Vec<u8>,
    skip_line: bool,
    last_info: Option<Value>,
    last_observed_at: Option<chrono::DateTime<chrono::FixedOffset>>,
}

impl LiveContextReader {
    pub(super) fn new(boundary: &CodexTranscriptBoundary) -> Self {
        Self {
            root: super::codex_sessions_dir(),
            discovery: None,
            next_discovery: Instant::now(),
            watermark: boundary.watermark().cloned(),
            disabled: matches!(boundary, CodexTranscriptBoundary::ResumeUnavailable),
            pending: Vec::new(),
            // A partial record begun before resume is not a new observation.
            skip_line: boundary
                .watermark()
                .is_some_and(|w| w.byte_offset > 0 && w.prefix_tail.last() != Some(&b'\n')),
            last_info: None,
            last_observed_at: None,
        }
    }

    /// Also used by terminal recovery so rereading a sample cannot refresh it.
    pub(super) fn accept(&mut self, event: StreamEvent) -> Option<StreamEvent> {
        if self.disabled {
            return None;
        }
        let info = event.data.get("info")?;
        let observed_at = event
            .data
            .get("observed_at")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        if observed_at
            .zip(self.last_observed_at)
            .is_some_and(|(at, prior)| at < prior)
        {
            // Terminal recovery can jump ahead of the incremental cursor.
            // Older backlog must not roll that observation backward.
            return None;
        }
        if self.last_info.as_ref() == Some(info) {
            return None;
        }
        self.last_info = Some(info.clone());
        self.last_observed_at = observed_at.or(self.last_observed_at);
        Some(event)
    }

    fn discover(&mut self, thread_id: &str) -> Option<PathBuf> {
        if self.discovery.is_none() {
            if Instant::now() < self.next_discovery {
                return None;
            }
            self.discovery = Some(
                walkdir::WalkDir::new(self.root.as_ref()?)
                    .max_depth(4)
                    .into_iter(),
            );
        }
        let suffix = format!("-{thread_id}.jsonl");
        for _ in 0..DISCOVERY_ENTRIES_PER_POLL {
            match self.discovery.as_mut()?.next() {
                Some(Ok(entry))
                    if entry.file_type().is_file()
                        && entry.file_name().to_string_lossy().ends_with(&suffix) =>
                {
                    let path = entry.into_path();
                    self.discovery = None;
                    return Some(path);
                }
                Some(_) => {}
                None => {
                    self.discovery = None;
                    self.next_discovery = Instant::now() + Duration::from_secs(5);
                    break;
                }
            }
        }
        None
    }

    pub(super) fn poll(&mut self, thread_id: &str) -> Option<StreamEvent> {
        if self.disabled {
            return None;
        }
        let path = match &self.watermark {
            Some(w) => w.path.clone(),
            None => self.discover(thread_id)?,
        };
        self.read_path(&path, thread_id).ok().flatten()
    }

    fn read_path(&mut self, path: &Path, thread_id: &str) -> std::io::Result<Option<StreamEvent>> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let offset = self.watermark.as_ref().map_or(0, |w| w.byte_offset);
        if let Some(w) = &self.watermark {
            let mut tail = vec![0; w.prefix_tail.len()];
            let identity_matches =
                metadata.dev() == w.device && metadata.ino() == w.inode && metadata.len() >= offset;
            let prefix_matches = identity_matches && {
                file.seek(SeekFrom::Start(offset - tail.len() as u64))?;
                file.read_exact(&mut tail)?;
                tail == w.prefix_tail
            };
            if !prefix_matches {
                // Do not replay another incarnation as fresh. The last valid
                // observation ages naturally to Stale in the monitor.
                self.disabled = true;
                self.pending.clear();
                return Ok(None);
            }
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_READ_BYTES as u64)
            .read_to_end(&mut bytes)?;
        let new_offset = offset + bytes.len() as u64;
        let mut latest = None;
        for byte in bytes {
            if byte == b'\n' {
                if !self.skip_line
                    && let Ok(value) = serde_json::from_slice::<Value>(&self.pending)
                    && let Some(event) = codex_context_event(&value, thread_id)
                    && let Some(event) = self.accept(event)
                {
                    latest = Some(event);
                }
                self.pending.clear();
                self.skip_line = false;
            } else if !self.skip_line {
                if self.pending.len() == MAX_LINE_BYTES {
                    self.pending.clear();
                    self.skip_line = true;
                } else {
                    self.pending.push(byte);
                }
            }
        }
        let tail_len = new_offset.min(super::CODEX_TRANSCRIPT_WATERMARK_TAIL_BYTES);
        file.seek(SeekFrom::Start(new_offset - tail_len))?;
        let mut tail = vec![0; tail_len as usize];
        file.read_exact(&mut tail)?;
        self.watermark = Some(CodexTranscriptWatermark {
            path: path.to_path_buf(),
            byte_offset: new_offset,
            device: metadata.dev(),
            inode: metadata.ino(),
            prefix_tail: tail,
        });
        Ok(latest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn token(total: u64) -> String {
        serde_json::json!({"type":"event_msg", "timestamp":"2026-09-22T04:53:49.505Z",
        "payload":{"type":"token_count","info":{
            "last_token_usage":{"total_tokens":total},"model_context_window":258400
        }}})
        .to_string()
            + "\n"
    }

    #[tokio::test]
    async fn live_context_stdout_wait_polls_usage_and_settles_on_eof() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rollout-thread.jsonl"), token(85_558)).unwrap();
        let mut reader = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        reader.root = Some(dir.path().to_path_buf());
        let (stdout_writer, stdout_reader) = tokio::io::duplex(1024);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let wait = tokio::spawn(async move {
            let mut lines = crate::process_control::BoundedLines::new(stdout_reader, 1024);
            let mut reader = Some(reader);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            next_line_with_context(&mut lines, &mut reader, Some("thread"), &mut tick, &tx).await
        });
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event.data["info"]["last_token_usage"]["total_tokens"],
            85_558
        );
        drop(stdout_writer);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            None
        );
        assert!(
            rx.recv().await.is_none(),
            "stdout owner releases the event channel on EOF"
        );
    }

    #[test]
    fn live_context_replacement_and_partial_resume_do_not_replay_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-thread.jsonl");
        std::fs::write(&path, &token(10).as_bytes()[..20]).unwrap();
        let mut previous = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        assert!(previous.read_path(&path, "thread").unwrap().is_none());
        let mut resumed = LiveContextReader::new(&CodexTranscriptBoundary::Resume(
            previous.watermark.unwrap(),
        ));
        let mut file = File::options().append(true).open(&path).unwrap();
        file.write_all(&token(10).as_bytes()[20..]).unwrap();
        assert!(resumed.read_path(&path, "thread").unwrap().is_none());
        file.write_all(token(20).as_bytes()).unwrap();
        assert_eq!(
            resumed.read_path(&path, "thread").unwrap().unwrap().data["info"]["last_token_usage"]["total_tokens"],
            20
        );
        std::fs::rename(&path, dir.path().join("previous.jsonl")).unwrap();
        std::fs::write(&path, token(30).repeat(3)).unwrap();
        assert!(resumed.read_path(&path, "thread").unwrap().is_none());
        assert!(resumed.disabled);
    }

    #[test]
    fn live_context_io_and_memory_stay_bounded_on_large_non_usage_output() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&vec![b'x'; MAX_READ_BYTES + MAX_LINE_BYTES])
            .unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(token(42).as_bytes()).unwrap();
        let mut reader = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        assert!(reader.read_path(file.path(), "thread").unwrap().is_none());
        assert_eq!(
            reader.watermark.as_ref().unwrap().byte_offset,
            MAX_READ_BYTES as u64
        );
        assert!(reader.pending.len() <= MAX_LINE_BYTES);
        assert_eq!(
            reader
                .read_path(file.path(), "thread")
                .unwrap()
                .unwrap()
                .data["info"]["last_token_usage"]["total_tokens"],
            42
        );
    }

    #[test]
    fn live_context_reads_before_terminal_and_ignores_duplicate_usage() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut reader = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        file.write_all(token(85_558).as_bytes()).unwrap();
        let event = reader.read_path(file.path(), "thread").unwrap().unwrap();
        assert_eq!(
            event.data["info"]["last_token_usage"]["total_tokens"],
            85_558
        );
        assert_eq!(event.data["observed_at"], "2026-09-22T04:53:49.505Z");
        assert!(reader.read_path(file.path(), "thread").unwrap().is_none());
        file.write_all(token(85_558).as_bytes()).unwrap();
        assert!(reader.read_path(file.path(), "thread").unwrap().is_none());
        file.write_all(token(90_000).as_bytes()).unwrap();
        assert!(reader.read_path(file.path(), "thread").unwrap().is_some());
    }

    #[test]
    fn live_context_terminal_recovery_fences_older_cursor_backlog() {
        let mut reader = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        let mut value: Value = serde_json::from_str(&token(100)).unwrap();
        let old = codex_context_event(&value, "thread").unwrap();
        value["timestamp"] = Value::String("2026-09-22T04:54:00.000Z".into());
        value["payload"]["info"]["last_token_usage"]["total_tokens"] = 200.into();
        assert!(
            reader
                .accept(codex_context_event(&value, "thread").unwrap())
                .is_some()
        );
        assert!(reader.accept(old).is_none());
        assert_eq!(
            reader.last_info.unwrap()["last_token_usage"]["total_tokens"],
            200
        );
    }

    #[test]
    fn live_context_handles_partial_oversized_compacted_and_zero_records() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut reader = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        let record = token(224_418);
        file.write_all(&record.as_bytes()[..35]).unwrap();
        assert!(reader.read_path(file.path(), "thread").unwrap().is_none());
        file.write_all(&record.as_bytes()[35..]).unwrap();
        assert!(reader.read_path(file.path(), "thread").unwrap().is_some());
        file.write_all(&vec![b'x'; MAX_LINE_BYTES + 10]).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(token(14_589).as_bytes()).unwrap();
        let compacted = reader.read_path(file.path(), "thread").unwrap().unwrap();
        assert_eq!(
            compacted.data["info"]["last_token_usage"]["total_tokens"],
            14_589
        );
        file.write_all(token(0).as_bytes()).unwrap();
        let zero = reader.read_path(file.path(), "thread").unwrap().unwrap();
        assert_eq!(zero.data["info"]["last_token_usage"]["total_tokens"], 0);
    }

    #[test]
    fn live_context_resume_uses_eof_boundary_and_rejects_rewrite() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(token(80_000).as_bytes()).unwrap();
        let mut prior = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        prior.read_path(file.path(), "thread").unwrap();
        let boundary = CodexTranscriptBoundary::Resume(prior.watermark.unwrap());
        let mut resumed = LiveContextReader::new(&boundary);
        assert!(resumed.read_path(file.path(), "thread").unwrap().is_none());
        file.write_all(token(90_000).as_bytes()).unwrap();
        assert!(resumed.read_path(file.path(), "thread").unwrap().is_some());
        file.as_file_mut().set_len(0).unwrap();
        file.as_file_mut().rewind().unwrap();
        file.write_all(token(10).as_bytes()).unwrap();
        assert!(resumed.read_path(file.path(), "thread").unwrap().is_none());
        assert!(resumed.disabled);
    }

    #[test]
    fn live_context_recorded_manager_trace_preserves_cached_and_compaction_totals() {
        let trace = include_str!(
            "../../../../thoughts/shared/handoffs/issue-542/manager-context.sanitized.jsonl"
        );
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut reader = LiveContextReader::new(&CodexTranscriptBoundary::Fresh);
        let mut samples = Vec::new();
        for line in trace.lines() {
            writeln!(file, "{line}").unwrap();
            if let Some(event) = reader.read_path(file.path(), "thread").unwrap() {
                let usage = crate::monitor::extract_codex_context_usage(&event).unwrap();
                assert_eq!(usage.context_window, Some(258_400));
                samples.push(usage.context_tokens);
            }
        }
        assert_eq!(samples.len(), 90);
        assert_eq!(samples[0], 23_433);
        assert!(samples.windows(2).any(|w| w == [224_418, 14_589]));
        assert!(samples.contains(&85_558));
        assert_eq!(samples.last(), Some(&148_049));
    }
}
