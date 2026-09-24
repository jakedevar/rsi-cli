use crate::error::{DaemonError, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Inject text into a running session subprocess's stdin.
#[async_trait::async_trait]
pub trait StdinInjector: Send + Sync {
    /// Write prompt to stdin. Returns Ok(true) if written, Ok(false) if unsupported.
    async fn inject_stdin(&self, text: &str) -> Result<bool>;
}

/// Stdin injector for Claude CLI sessions (writes to ChildStdin).
pub struct ClaudeStdinInjector {
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
}

impl ClaudeStdinInjector {
    pub fn new(stdin: tokio::process::ChildStdin) -> Self {
        Self {
            stdin: Arc::new(Mutex::new(stdin)),
        }
    }
}

#[async_trait::async_trait]
impl StdinInjector for ClaudeStdinInjector {
    async fn inject_stdin(&self, text: &str) -> Result<bool> {
        use tokio::io::AsyncWriteExt;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| DaemonError::Process(format!("stdin write failed: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| DaemonError::Process(format!("stdin newline failed: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| DaemonError::Process(format!("stdin flush failed: {e}")))?;
        Ok(true)
    }
}

/// No-op stdin injector for providers that don't support stdin input.
pub struct NoopStdinInjector;

#[async_trait::async_trait]
impl StdinInjector for NoopStdinInjector {
    async fn inject_stdin(&self, _text: &str) -> Result<bool> {
        Ok(false)
    }
}

/// Memory flush settings extracted from MemoryConfig.
#[derive(Debug, Clone)]
pub struct MemoryFlushSettings {
    pub enabled: bool,
    pub soft_threshold_tokens: u64,
    pub reserve_floor_tokens: u64,
}

impl Default for MemoryFlushSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            soft_threshold_tokens: 4000,
            reserve_floor_tokens: 20_000,
        }
    }
}

pub const MEMORY_FLUSH_PROMPT: &str = "Pre-compaction memory flush. Store durable memories now (use memory/YYYY-MM-DD.md; create memory/ if needed). IMPORTANT: If the file already exists, APPEND new content only and do not overwrite existing entries. If nothing to store, acknowledge briefly and continue.";

pub const MEMORY_FLUSH_SYSTEM_PROMPT: &str = "Pre-compaction memory flush turn. The session is near auto-compaction; capture durable memories to disk.";

/// Determine whether a memory flush should run before context rotation.
///
/// Returns true when:
/// 1. Flush is enabled
/// 2. The token count is non-zero
/// 3. `total_tokens` exceeds the resolved active budget minus the reserves
/// 4. A flush hasn't already run at the current compaction count
pub fn should_run_memory_flush(
    total_tokens: u64,
    context_budget: &rsi_common::ResolvedContextBudget,
    settings: &MemoryFlushSettings,
    memory_flush_compaction_count: Option<u32>,
    current_compaction_count: u32,
) -> bool {
    if !settings.enabled {
        return false;
    }
    if total_tokens == 0 {
        return false;
    }
    if settings.soft_threshold_tokens == 0 {
        return false;
    }

    // Already flushed at this compaction depth
    if memory_flush_compaction_count == Some(current_compaction_count) {
        return false;
    }

    let threshold = context_budget
        .active_tokens
        .saturating_sub(settings.reserve_floor_tokens)
        .saturating_sub(settings.soft_threshold_tokens);

    if threshold == 0 {
        return false;
    }

    total_tokens >= threshold
}

/// Replace "YYYY-MM-DD" placeholder in the flush prompt with today's date.
pub fn format_flush_prompt(prompt: &str) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    prompt.replace("YYYY-MM-DD", &today)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(active_tokens: u64) -> rsi_common::ResolvedContextBudget {
        rsi_common::ResolvedContextBudget::new(
            active_tokens,
            rsi_common::ContextCapacity::default(),
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::RuntimeTelemetry,
                source_version: None,
                source_digest: None,
                observed_at: None,
                confidence: rsi_common::CapabilityConfidence::Authoritative,
            },
        )
        .expect("positive memory-flush budget")
    }

    fn should_run_for_window(
        total_tokens: u64,
        context_window: u64,
        settings: &MemoryFlushSettings,
        memory_flush_compaction_count: Option<u32>,
        current_compaction_count: u32,
    ) -> bool {
        should_run_memory_flush(
            total_tokens,
            &budget(context_window),
            settings,
            memory_flush_compaction_count,
            current_compaction_count,
        )
    }

    fn default_settings() -> MemoryFlushSettings {
        MemoryFlushSettings {
            enabled: true,
            soft_threshold_tokens: 4000,
            reserve_floor_tokens: 20_000,
        }
    }

    #[test]
    fn test_flush_disabled() {
        let mut settings = default_settings();
        settings.enabled = false;
        assert!(!should_run_for_window(100_000, 200_000, &settings, None, 0));
    }

    #[test]
    fn test_flush_zero_tokens() {
        assert!(!should_run_for_window(
            0,
            200_000,
            &default_settings(),
            None,
            0
        ));
    }

    #[test]
    fn test_flush_below_threshold() {
        // context_window=200000, reserve=20000, soft=4000 → threshold = 176000
        assert!(!should_run_for_window(
            100_000,
            200_000,
            &default_settings(),
            None,
            0
        ));
    }

    #[test]
    fn test_flush_at_threshold() {
        // threshold = 200000 - 20000 - 4000 = 176000
        assert!(should_run_for_window(
            176_000,
            200_000,
            &default_settings(),
            None,
            0
        ));
    }

    #[test]
    fn test_flush_above_threshold() {
        assert!(should_run_for_window(
            190_000,
            200_000,
            &default_settings(),
            None,
            0
        ));
    }

    #[test]
    fn test_flush_double_flush_guard() {
        // Already flushed at compaction count 2
        assert!(!should_run_for_window(
            190_000,
            200_000,
            &default_settings(),
            Some(2),
            2
        ));
    }

    #[test]
    fn test_flush_different_compaction_count() {
        // Flushed at count 1, now at count 2 → should flush again
        assert!(should_run_for_window(
            190_000,
            200_000,
            &default_settings(),
            Some(1),
            2
        ));
    }

    #[test]
    fn test_flush_no_previous_flush() {
        assert!(should_run_for_window(
            190_000,
            200_000,
            &default_settings(),
            None,
            0
        ));
    }

    #[test]
    fn test_flush_zero_soft_threshold() {
        let mut settings = default_settings();
        settings.soft_threshold_tokens = 0;
        assert!(!should_run_for_window(190_000, 200_000, &settings, None, 0));
    }

    #[test]
    fn test_flush_large_context_window() {
        // 1M context window
        let settings = MemoryFlushSettings {
            enabled: true,
            soft_threshold_tokens: 10_000,
            reserve_floor_tokens: 50_000,
        };
        // threshold = 1_000_000 - 50_000 - 10_000 = 940_000
        assert!(!should_run_for_window(
            900_000, 1_000_000, &settings, None, 0
        ));
        assert!(should_run_for_window(
            940_000, 1_000_000, &settings, None, 0
        ));
    }

    #[test]
    fn test_format_flush_prompt_replaces_date() {
        let result = format_flush_prompt(MEMORY_FLUSH_PROMPT);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(result.contains(&today));
        assert!(!result.contains("YYYY-MM-DD"));
    }

    #[test]
    fn test_format_flush_prompt_no_placeholder() {
        let input = "No date placeholder here";
        let result = format_flush_prompt(input);
        assert_eq!(result, input);
    }

    // -------------------------------------------------------------------------
    // StdinInjector tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_claude_stdin_injection() {
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let injector = ClaudeStdinInjector::new(stdin);
        assert!(injector.inject_stdin("test prompt").await.unwrap());
        drop(injector);
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn test_noop_stdin_injector() {
        let injector = NoopStdinInjector;
        assert!(!injector.inject_stdin("ignored").await.unwrap());
    }
}
