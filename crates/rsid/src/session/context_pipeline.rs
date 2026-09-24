//! Multi-source context assembly pipeline with token budget enforcement.
//!
//! Replaces the single-source `build_memory_system_prompt()` with a composable
//! pipeline that gathers context from multiple sources in parallel, respects
//! a token budget (2% of context window), and assembles tagged blocks.

use crate::monitor::TokenCounter;
use rsi_common::types::Project;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// A single context block with metadata for budget-aware assembly.
struct ContextBlock {
    /// Display tag, e.g., "[Active Task]", "[Project Context]"
    tag: &'static str,
    /// The content to inject
    content: String,
    /// Pre-counted token cost (used for logging; actual budget enforcement re-counts)
    _tokens: u64,
    /// Lower = higher priority (included first when budget is tight)
    priority: u8,
}

/// Assembles structured context from multiple sources with token budget enforcement.
pub(super) struct ContextPipeline {
    token_counter: Arc<TokenCounter>,
    memory_handle: Option<crate::memory::worker::MemoryHandle>,
    store: Arc<tokio::sync::Mutex<crate::store::Store>>,
}

impl ContextPipeline {
    pub fn new(
        token_counter: Arc<TokenCounter>,
        memory_handle: Option<crate::memory::worker::MemoryHandle>,
        store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    ) -> Self {
        Self {
            token_counter,
            memory_handle,
            store,
        }
    }

    /// Assemble context from all available sources.
    ///
    /// Returns `None` if no sources produce content or budget is zero.
    /// Respects `total_budget_tokens` by including sources in priority order.
    ///
    /// All sources are gathered in parallel with per-source timeouts.
    /// Master timeout: 3 seconds. Any source that fails or times out is skipped.
    pub async fn assemble(
        &self,
        query: &str,
        working_dir: &Path,
        project: Option<&Project>,
        active_task: Option<&str>,
        workflow_content: Option<&str>,
        parent_session_id: Option<Uuid>,
        total_budget_tokens: u64,
    ) -> Option<String> {
        if total_budget_tokens == 0 {
            return None;
        }

        // Gather all sources in parallel with master timeout
        let blocks = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.gather_all(
                query,
                working_dir,
                project,
                active_task,
                workflow_content,
                parent_session_id,
            ),
        )
        .await
        {
            Ok(blocks) => blocks,
            Err(_) => {
                tracing::warn!("Context pipeline master timeout (3s) exceeded");
                return None;
            }
        };

        if blocks.is_empty() {
            return None;
        }

        // Assemble blocks within budget (already sorted by priority)
        self.assemble_within_budget(blocks, total_budget_tokens)
    }

    /// Gather context from all sources in parallel.
    ///
    /// `project` carries both the project metadata (used by project_card /
    /// project_files) and the scope for memory retrieval. When the session
    /// has a project, memory search is restricted to that project; when
    /// there is no project, `gather_memory` falls back to global search
    /// only if the caller explicitly opted in (default behavior here is to
    /// skip memory entirely when no project is provided — see comments at
    /// the call site in `launch.rs` / `rotation.rs`).
    async fn gather_all(
        &self,
        query: &str,
        working_dir: &Path,
        project: Option<&Project>,
        active_task: Option<&str>,
        workflow_content: Option<&str>,
        parent_session_id: Option<Uuid>,
    ) -> Vec<ContextBlock> {
        // Active task is synchronous — no I/O needed
        let active_task_block = active_task.map(|task| {
            let tokens = self.token_counter.count(task) + 5; // tag overhead
            ContextBlock {
                tag: "[Active Task]",
                content: task.to_string(),
                _tokens: tokens,
                priority: 0,
            }
        });

        // Workflow content from FLYWHEEL.md (pre-rendered, synchronous)
        let workflow_block = workflow_content.filter(|c| !c.is_empty()).map(|content| {
            let tokens = self.token_counter.count(content) + 5;
            ContextBlock {
                tag: "[Project Workflow]",
                content: content.to_string(),
                _tokens: tokens,
                priority: 1,
            }
        });

        // Run I/O sources in parallel (cards are fast SQLite lookups, ~100us)
        let project_card_fut = self.gather_project_card(project);
        let user_card_fut = self.gather_user_card();
        let project_files_fut = self.gather_project_files(project);
        // Memory scope: project-scoped agent delivery is strict — when the
        // session has a project, only that project's indexed transcripts
        // qualify (plan §Scope Semantics, D3). Global memory files are
        // intentionally excluded from project agents in this pass.
        let memory_fut = self.gather_memory(query, project.map(|p| p.id));
        let git_fut = Self::gather_git_log(working_dir, &self.token_counter);
        let summary_fut = self.gather_session_summary(parent_session_id);

        let (project_card, user_card, project_files, memory, git, summary) = tokio::join!(
            project_card_fut,
            user_card_fut,
            project_files_fut,
            memory_fut,
            git_fut,
            summary_fut,
        );

        let mut blocks = Vec::with_capacity(9);

        if let Some(block) = active_task_block {
            blocks.push(block);
        }
        if let Some(block) = workflow_block {
            blocks.push(block);
        }
        if let Some(block) = project_card {
            blocks.push(block);
        }
        if let Some(block) = user_card {
            blocks.push(block);
        }
        if let Some(block) = summary {
            blocks.push(block);
        }
        if let Some(block) = project_files {
            blocks.push(block);
        }
        if let Some(block) = memory {
            blocks.push(block);
        }
        if let Some(block) = git {
            blocks.push(block);
        }

        // Sort by priority (stable — preserves insertion order for equal priorities)
        blocks.sort_by_key(|b| b.priority);
        blocks
    }

    /// Load project card facts from SQLite. Fast (~100us), no timeout needed.
    async fn gather_project_card(&self, project: Option<&Project>) -> Option<ContextBlock> {
        let project = project?;
        let store = self.store.clone();
        let project_id = project.id.to_string();

        let card = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_entity_card("project", &project_id).ok().flatten()
        })
        .await
        .ok()
        .flatten()?;

        if card.facts.is_empty() {
            return None;
        }

        let content = card.facts.join("\n");
        let tokens = self.token_counter.count(&content) + 5;

        Some(ContextBlock {
            tag: "[Project Card]",
            content,
            _tokens: tokens,
            priority: 2, // After active task (0) and workflow (1)
        })
    }

    /// Load user card facts from SQLite. Fast (~100us), no timeout needed.
    async fn gather_user_card(&self) -> Option<ContextBlock> {
        let store = self.store.clone();

        let card = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_entity_card("user", "self").ok().flatten()
        })
        .await
        .ok()
        .flatten()?;

        if card.facts.is_empty() {
            return None;
        }

        let content = card.facts.join("\n");
        let tokens = self.token_counter.count(&content) + 5;

        Some(ContextBlock {
            tag: "[User Preferences]",
            content,
            _tokens: tokens,
            priority: 3, // After project card (2)
        })
    }

    /// Load the latest long summary for a parent session from the store.
    /// Used for context rotation handoffs — the child session gets the parent's
    /// historical context via this block.
    /// Timeout: 100ms (fast local DB read, no LLM call).
    async fn gather_session_summary(
        &self,
        parent_session_id: Option<Uuid>,
    ) -> Option<ContextBlock> {
        let session_id = parent_session_id?;
        let counter = Arc::clone(&self.token_counter);
        let store = Arc::clone(&self.store);

        match tokio::time::timeout(std::time::Duration::from_millis(100), async move {
            let store_guard = store.lock().await;
            store_guard.get_latest_summary(session_id, rsi_common::types::SummaryKind::Long)
        })
        .await
        {
            Ok(Ok(Some(summary))) if !summary.content.is_empty() => {
                let tokens = counter.count(&summary.content) + 5;
                tracing::debug!(
                    session_id = %session_id,
                    token_count = summary.token_count,
                    "Pipeline: injecting parent session long summary"
                );
                Some(ContextBlock {
                    tag: "[Session History]",
                    content: summary.content,
                    _tokens: tokens,
                    priority: 0, // Same priority as active task — critical for rotation handoffs
                })
            }
            Ok(Ok(Some(_))) | Ok(Ok(None)) => None,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "Failed to load session summary for context pipeline");
                None
            }
            Err(_) => {
                tracing::debug!("Session summary load timed out (100ms)");
                None
            }
        }
    }

    /// Read project context files from disk.
    /// Timeout: 500ms. Reads files relative to project path.
    async fn gather_project_files(&self, project: Option<&Project>) -> Option<ContextBlock> {
        let project = project?;
        let context_files = project.context_files.as_ref()?;
        if context_files.is_empty() {
            return None;
        }
        let project_path = project.path.as_ref()?;

        let project_path = project_path.clone();
        let files = context_files.clone();
        let counter = Arc::clone(&self.token_counter);

        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::task::spawn_blocking(move || {
                let mut buf = String::new();
                let mut total_tokens: u64 = 0;
                for file in &files {
                    let full_path = project_path.join(file);
                    match std::fs::read_to_string(&full_path) {
                        Ok(content) => {
                            let file_name = file.to_string_lossy();
                            let header = format!("--- {} ---\n", file_name);
                            let file_tokens = counter.count(&content) + counter.count(&header);
                            buf.push_str(&header);
                            buf.push_str(&content);
                            buf.push_str("\n\n");
                            total_tokens += file_tokens;
                        }
                        Err(e) => {
                            tracing::debug!(
                                path = %full_path.display(),
                                error = %e,
                                "Skipping unreadable project context file"
                            );
                        }
                    }
                }
                if buf.is_empty() {
                    None
                } else {
                    Some((buf.trim_end().to_string(), total_tokens))
                }
            }),
        )
        .await
        {
            Ok(Ok(Some((content, tokens)))) => Some(ContextBlock {
                tag: "[Project Context]",
                content,
                _tokens: tokens + 5, // tag overhead
                priority: 4,         // After cards (2, 3)
            }),
            Ok(Ok(None)) => None,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Project context file read task panicked");
                None
            }
            Err(_) => {
                tracing::warn!("Project context file read timed out (500ms)");
                None
            }
        }
    }

    /// Search memory for relevant context.
    ///
    /// When `project_id` is `Some`, only memory indexed under that project
    /// is eligible — strict project isolation for agent delivery (plan §D3).
    /// When `None`, no memory is injected; manual surfaces should call the
    /// unscoped global search path directly instead of relying on agent
    /// context to leak global memory into project-less sessions.
    ///
    /// Timeout: 2s (matches existing build_memory_system_prompt behavior).
    async fn gather_memory(&self, query: &str, project_id: Option<Uuid>) -> Option<ContextBlock> {
        let handle = self.memory_handle.as_ref()?;

        if project_id.is_none() {
            tracing::debug!(
                "Pipeline: skipping memory injection — session has no project_id; \
                 global agent injection is intentionally disabled (plan §D3)"
            );
            return None;
        }

        let results = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handle.search(query, Some(6), Some(0.35), project_id),
        )
        .await
        {
            Ok(Ok(r)) if !r.is_empty() => r,
            Ok(Ok(_)) => return None,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Memory search failed; proceeding without memory context");
                return None;
            }
            Err(_) => {
                tracing::warn!("Memory search timed out (2s); proceeding without memory context");
                return None;
            }
        };

        let mut buf = String::new();
        for r in &results {
            buf.push_str(&format!(
                "Source: {} (lines {}-{})\n{}\n\n",
                r.path, r.start_line, r.end_line, r.snippet
            ));
        }
        let content = buf.trim_end().to_string();
        let tokens = self.token_counter.count(&content) + 5;

        tracing::debug!(
            count = results.len(),
            query_prefix = &query[..query.len().min(60)],
            project_id = ?project_id,
            "Pipeline: memory source produced {} results",
            results.len()
        );

        Some(ContextBlock {
            tag: "[Relevant Memory]",
            content,
            _tokens: tokens,
            priority: 5, // After cards and project files
        })
    }

    /// Get recent git log from working directory.
    /// Timeout: 1s. Returns None if not a git repo or git is unavailable.
    async fn gather_git_log(
        working_dir: &Path,
        counter: &Arc<TokenCounter>,
    ) -> Option<ContextBlock> {
        let working_dir = working_dir.to_path_buf();
        let counter = Arc::clone(counter);

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::task::spawn_blocking(move || {
                std::process::Command::new("git")
                    .args(["log", "--oneline", "-10"])
                    .current_dir(&working_dir)
                    .output()
            }),
        )
        .await
        {
            Ok(Ok(Ok(output))) if output.status.success() => output,
            Ok(Ok(Ok(_))) => return None, // non-zero exit (not a git repo, etc.)
            Ok(Ok(Err(_))) => return None, // git not found
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "Git log task panicked");
                return None;
            }
            Err(_) => {
                tracing::debug!("Git log timed out (1s)");
                return None;
            }
        };

        let content = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if content.is_empty() {
            return None;
        }

        let tokens = counter.count(&content) + 5; // tag overhead

        Some(ContextBlock {
            tag: "[Recent Git Activity]",
            content,
            _tokens: tokens,
            priority: 6, // Lowest priority
        })
    }

    /// Assemble blocks into final output, respecting token budget.
    /// Includes blocks in priority order until budget is exhausted.
    fn assemble_within_budget(&self, blocks: Vec<ContextBlock>, budget: u64) -> Option<String> {
        let mut parts = Vec::new();
        let mut remaining = budget;
        // Per-block (tag, actual_tokens) for blocks that land in the output
        // untruncated — the raw material for S8 layer telemetry. Only
        // fully-included blocks are recorded so the reported counts stay honest;
        // the truncated-tail block below is a budget-exhaustion edge and is left
        // out rather than counted at its full, pre-truncation size.
        let mut included: Vec<(&'static str, u64)> = Vec::new();

        for block in blocks {
            // Re-count tokens accurately (gather may have estimated)
            let actual_tokens =
                self.token_counter.count(&block.content) + self.token_counter.count(block.tag) + 2; // newlines

            if actual_tokens > remaining {
                // Try to include a truncated version if it's large
                if actual_tokens > budget / 2 && remaining > 100 {
                    // Truncate content to fit remaining budget
                    if let Some(truncated) =
                        self.truncate_to_tokens(&block.content, remaining.saturating_sub(20))
                    {
                        parts.push(format!("{}\n{}\n[...truncated]", block.tag, truncated));
                        break; // budget exhausted after truncation
                    }
                }
                // Skip this block entirely if it doesn't fit
                tracing::debug!(
                    tag = block.tag,
                    tokens = actual_tokens,
                    remaining = remaining,
                    "Skipping context block — exceeds remaining budget"
                );
                continue;
            }

            parts.push(format!("{}\n{}", block.tag, block.content));
            remaining = remaining.saturating_sub(actual_tokens);
            included.push((block.tag, actual_tokens));
        }

        // S8 per-layer token telemetry (observability only — the output above is
        // already final and is not touched here). L3/L4 flow through this
        // pipeline and are counted; L0/L1/L2 are assembled upstream/externally
        // and are reported as explicit uncounted markers, never faked as 0 (a 0
        // would falsely assert the layer is empty when it is merely counted
        // elsewhere). Emitted at DEBUG under a dedicated target so default log
        // output is unchanged and the signal is opt-in
        // (RUST_LOG=context_telemetry=debug).
        let telemetry = layer_telemetry(&included);
        tracing::debug!(
            target: "context_telemetry",
            layer = "L3",
            token_count = telemetry.l3_tokens,
            block_count = telemetry.l3_blocks,
            "context layer tokens"
        );
        tracing::debug!(
            target: "context_telemetry",
            layer = "L4",
            token_count = telemetry.l4_tokens,
            block_count = telemetry.l4_blocks,
            "context layer tokens"
        );
        tracing::debug!(
            target: "context_telemetry",
            layer = "L0",
            status = Layer::L0.uncounted_status().unwrap_or(""),
            "context layer external/uncounted"
        );
        tracing::debug!(
            target: "context_telemetry",
            layer = "L1",
            status = Layer::L1.uncounted_status().unwrap_or(""),
            "context layer assembled upstream"
        );
        tracing::debug!(
            target: "context_telemetry",
            layer = "L2",
            status = Layer::L2.uncounted_status().unwrap_or(""),
            "context layer assembled upstream"
        );
        tracing::debug!(
            target: "context_telemetry",
            l3_tokens = telemetry.l3_tokens,
            l4_tokens = telemetry.l4_tokens,
            counted_total = telemetry.counted_total,
            block_count = telemetry.block_count,
            "context assembly telemetry summary"
        );

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    /// Truncate text to approximately fit within a token budget.
    /// Returns None if the text is too short to meaningfully truncate.
    fn truncate_to_tokens(&self, text: &str, max_tokens: u64) -> Option<String> {
        if max_tokens < 50 {
            return None;
        }
        // Use a simple heuristic: ~4 chars per token for English text.
        let approx_chars = (max_tokens * 4) as usize;
        if approx_chars >= text.len() {
            return Some(text.to_string());
        }
        // Find the last newline before the cutoff point for clean truncation
        let truncated = &text[..approx_chars.min(text.len())];
        let cut_point = truncated.rfind('\n').unwrap_or(truncated.len());
        if cut_point < 50 {
            return None;
        }
        Some(text[..cut_point].to_string())
    }
}

/// Context layer in the L0–L4 assembly taxonomy (S8 telemetry).
///
/// Classification rule (the single source of truth for `layer_of`): **L3** is
/// stable project/user *reference* loaded independent of this run's query
/// (project card, user preferences, project files, workflow); **L4** is per-run
/// *working* context that varies with this run (active task, rotation session
/// history, query-driven memory, working-dir git). L0 (CLAUDE.md/AGENTS.md) is
/// provider-CLI-loaded and daemon-invisible; L1 (orchestration router) and L2
/// (kind preamble) are assembled upstream in `launch.rs`/`preamble.rs`, outside
/// this file. Only L3/L4 pass through this pipeline and are counted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    L0,
    L1,
    L2,
    L3,
    L4,
    /// Fallback for a tag not in the mapping table — folded into the L4 total so
    /// a newly-added block can never silently drop out of the counted sum.
    UnclassifiedL4,
}

impl Layer {
    /// Layers assembled outside this file report an explicit status marker
    /// instead of a numeric `token_count`. Counted layers (L3/L4) return `None`;
    /// L0/L1/L2 return a reason string — never a fake `0`, which would falsely
    /// assert the layer is empty when it is merely counted elsewhere/externally.
    const fn uncounted_status(self) -> Option<&'static str> {
        match self {
            Layer::L0 => Some("external_uncounted"),
            Layer::L1 | Layer::L2 => Some("uncounted_upstream"),
            Layer::L3 | Layer::L4 | Layer::UnclassifiedL4 => None,
        }
    }
}

/// Map a context block tag to its assembly layer (see [`Layer`] for the rule).
///
/// Unknown tags fall back to `UnclassifiedL4` and emit a drift `warn!` so a
/// newly-added block is visible and still counted rather than silently dropped.
fn layer_of(tag: &str) -> Layer {
    match tag {
        "[Project Workflow]" | "[Project Card]" | "[User Preferences]" | "[Project Context]" => {
            Layer::L3
        }
        "[Active Task]" | "[Session History]" | "[Relevant Memory]" | "[Recent Git Activity]" => {
            Layer::L4
        }
        _ => {
            tracing::warn!(
                target: "context_telemetry",
                tag,
                "context block tag not mapped to a layer"
            );
            Layer::UnclassifiedL4
        }
    }
}

/// Per-layer token telemetry for one assembly. L3/L4 are counted here; L0–L2
/// report status markers only (see [`Layer::uncounted_status`]). This is a pure
/// aggregation of `(tag, recount)` tuples — the testable value the tracing
/// events wrap.
#[derive(Debug, Default)]
struct LayerTelemetry {
    l3_tokens: u64,
    l4_tokens: u64,
    l3_blocks: usize,
    l4_blocks: usize,
    counted_total: u64,
    block_count: usize,
}

/// Aggregate per-block `(tag, recount)` tuples into per-layer sums. Unclassified
/// tags fold into the L4 total (drift-safe), and `counted_total = l3 + l4`.
fn layer_telemetry(included: &[(&'static str, u64)]) -> LayerTelemetry {
    let mut t = LayerTelemetry::default();
    for &(tag, tokens) in included {
        match layer_of(tag) {
            Layer::L3 => {
                t.l3_tokens += tokens;
                t.l3_blocks += 1;
            }
            Layer::L4 | Layer::UnclassifiedL4 => {
                t.l4_tokens += tokens;
                t.l4_blocks += 1;
            }
            // L0/L1/L2 are assembled outside this file and never appear here.
            Layer::L0 | Layer::L1 | Layer::L2 => {}
        }
    }
    t.counted_total = t.l3_tokens + t.l4_tokens;
    t.block_count = t.l3_blocks + t.l4_blocks;
    t
}

/// Calculate the context-injection allowance from an already resolved active
/// window. This is deliberately not a provider compaction limit.
pub(super) fn context_injection_allowance(window: u64) -> u64 {
    // 2% of context window, minimum 200 tokens
    (window / 50).max(200)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Arc<tokio::sync::Mutex<crate::store::Store>> {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        Arc::new(tokio::sync::Mutex::new(store))
    }

    #[test]
    fn context_injection_allowance_is_bounded_and_distinct() {
        assert_eq!(context_injection_allowance(1_000_000), 20_000);
        assert_eq!(context_injection_allowance(128_000), 2_560);
        assert_eq!(context_injection_allowance(1), 200);
    }

    #[tokio::test]
    async fn test_pipeline_no_sources_returns_none() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        // No active_task, no project, non-git dir, no memory, no workflow
        let result = pipeline
            .assemble(
                "test query",
                Path::new("/tmp/nonexistent-dir-12345"),
                None,
                None,
                None,
                None,
                4000,
            )
            .await;
        assert!(
            result.is_none(),
            "Pipeline with no sources should return None"
        );
    }

    #[tokio::test]
    async fn test_pipeline_active_task_only() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        let result = pipeline
            .assemble(
                "test query",
                Path::new("/tmp/nonexistent-dir-12345"),
                None,
                Some("Implementing context injection"),
                None,
                None,
                4000,
            )
            .await;

        let output = result.expect("Should return Some when active_task is set");
        assert!(
            output.contains("[Active Task]"),
            "Output should contain [Active Task] tag"
        );
        assert!(
            output.contains("Implementing context injection"),
            "Output should contain task text"
        );
    }

    #[tokio::test]
    async fn test_pipeline_zero_budget_returns_none() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        let result = pipeline
            .assemble("test", Path::new("/tmp"), None, Some("task"), None, None, 0)
            .await;
        assert!(result.is_none(), "Zero budget should return None");
    }

    #[tokio::test]
    async fn test_pipeline_budget_truncation() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        // With very small budget, active task should still fit (it's priority 0)
        let result = pipeline
            .assemble(
                "test",
                Path::new("/tmp/nonexistent-dir-12345"),
                None,
                Some("short task"),
                None,
                None,
                50,
            )
            .await;

        // Should still include the active task since it's tiny
        let output = result.expect("Should return Some for small active task");
        assert!(output.contains("[Active Task]"));
    }

    #[tokio::test]
    async fn test_pipeline_git_log_in_git_repo() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        // Current directory is a git repo
        let cwd = std::env::current_dir().unwrap();
        let result = pipeline
            .assemble("test query", &cwd, None, None, None, None, 4000)
            .await;

        // Should have git activity since we're in a git repo
        if let Some(ref output) = result {
            assert!(
                output.contains("[Recent Git Activity]"),
                "Git repo should produce git activity block"
            );
        }
    }

    #[tokio::test]
    async fn test_pipeline_project_context_files() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        // Create a temp dir with a test file
        let dir = tempfile::tempdir().unwrap();
        let test_file = dir.path().join("test.md");
        std::fs::write(&test_file, "# Test Context\nSome project info.").unwrap();

        let project = Project {
            id: uuid::Uuid::new_v4(),
            name: "test-project".to_string(),
            path: Some(dir.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: Some(vec![std::path::PathBuf::from("test.md")]),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let result = pipeline
            .assemble(
                "test query",
                dir.path(),
                Some(&project),
                None,
                None,
                None,
                4000,
            )
            .await;

        let output = result.expect("Should return Some with project context files");
        assert!(
            output.contains("[Project Context]"),
            "Output should contain [Project Context] tag"
        );
        assert!(
            output.contains("Test Context"),
            "Output should contain file contents"
        );
    }

    #[tokio::test]
    async fn test_pipeline_workflow_content() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        let result = pipeline
            .assemble(
                "test query",
                Path::new("/tmp/nonexistent-dir-12345"),
                None,
                None,
                Some("# Project Workflow\nAlways use TDD."),
                None,
                4000,
            )
            .await;

        let output = result.expect("Should return Some with workflow content");
        assert!(
            output.contains("[Project Workflow]"),
            "Output should contain [Project Workflow] tag"
        );
        assert!(
            output.contains("Always use TDD"),
            "Output should contain workflow text"
        );
    }

    /// One captured `context_telemetry` tracing event, reduced to the fields S8
    /// emits. Populated by [`CapturingLayer`] from the REAL `assemble_within_budget`
    /// emission path (not a reconstruction), so the pinning test fails if the
    /// telemetry wiring inside that function is removed.
    #[derive(Default)]
    struct CapturedEvent {
        layer: Option<String>,
        status: Option<String>,
        token_count: Option<u64>,
        block_count: Option<u64>,
        l3_tokens: Option<u64>,
        l4_tokens: Option<u64>,
        counted_total: Option<u64>,
    }

    struct FieldVisitor<'a>(&'a mut CapturedEvent);

    impl tracing::field::Visit for FieldVisitor<'_> {
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            match field.name() {
                "token_count" => self.0.token_count = Some(value),
                "block_count" => self.0.block_count = Some(value),
                "l3_tokens" => self.0.l3_tokens = Some(value),
                "l4_tokens" => self.0.l4_tokens = Some(value),
                "counted_total" => self.0.counted_total = Some(value),
                _ => {}
            }
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "layer" => self.0.layer = Some(value.to_string()),
                "status" => self.0.status = Some(value.to_string()),
                _ => {}
            }
        }
        fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
    }

    /// A `tracing_subscriber::Layer` (already a workspace dep — no new dependency)
    /// that captures `context_telemetry`-target events into a shared vec.
    struct CapturingLayer {
        events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "context_telemetry" {
                return;
            }
            let mut captured = CapturedEvent::default();
            event.record(&mut FieldVisitor(&mut captured));
            self.events.lock().unwrap().push(captured);
        }
    }

    #[test]
    fn test_layer_telemetry_pinning() {
        use tracing_subscriber::layer::SubscriberExt;

        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(Arc::clone(&counter), None, test_store());

        // Fixed assembly: two L4 blocks, two L3 blocks, and one deliberately
        // unknown tag to exercise the drift guard. Budget is huge so no
        // skip/truncate path runs — every block is included untruncated.
        let blocks = vec![
            ContextBlock {
                tag: "[Active Task]",
                content: "fixed active task content".to_string(),
                _tokens: 0,
                priority: 0,
            },
            ContextBlock {
                tag: "[Session History]",
                content: "fixed session history content".to_string(),
                _tokens: 0,
                priority: 0,
            },
            ContextBlock {
                tag: "[Project Workflow]",
                content: "fixed project workflow content".to_string(),
                _tokens: 0,
                priority: 1,
            },
            ContextBlock {
                tag: "[Project Card]",
                content: "fixed project card content".to_string(),
                _tokens: 0,
                priority: 2,
            },
            ContextBlock {
                tag: "[Totally Unknown Block]",
                content: "fixed unknown block content".to_string(),
                _tokens: 0,
                priority: 7,
            },
        ];

        // Behavior-preservation guard: pin the exact string pre-S8 code produces.
        let expected: String = blocks
            .iter()
            .map(|b| format!("{}\n{}", b.tag, b.content))
            .collect::<Vec<_>>()
            .join("\n\n");

        // Expected per-layer/total values, derived from the fixed literal content
        // via the exact formula the choke-point uses (`context_pipeline.rs` recount
        // = count(content) + count(tag) + 2). These are the ORACLE; the values
        // under test are captured from the real emission path below.
        let recount =
            |tag: &str, content: &str| -> u64 { counter.count(content) + counter.count(tag) + 2 };
        let expected_l3 = recount("[Project Workflow]", "fixed project workflow content")
            + recount("[Project Card]", "fixed project card content");
        // L4 includes the unknown-tag block (drift guard folds it into L4).
        let expected_l4 = recount("[Active Task]", "fixed active task content")
            + recount("[Session History]", "fixed session history content")
            + recount("[Totally Unknown Block]", "fixed unknown block content");
        let expected_total = expected_l3 + expected_l4;

        // Capture the tracing events emitted by the REAL `assemble_within_budget`
        // path (MINOR-1): if the telemetry emission block is deleted, no events are
        // captured and the `find`s below panic — this test then fails, ratcheting
        // the wiring. `tracing-subscriber` is already an rsid dependency.
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<CapturedEvent>::new()));
        let subscriber = tracing_subscriber::registry().with(CapturingLayer {
            events: std::sync::Arc::clone(&events),
        });
        let output = tracing::subscriber::with_default(subscriber, || {
            pipeline
                .assemble_within_budget(blocks, 1_000_000)
                .expect("fixed blocks under a huge budget must assemble")
        });
        assert_eq!(
            output, expected,
            "assembly string must be byte-for-byte unchanged (observability only)"
        );

        let events = events.lock().unwrap();
        let by_layer = |name: &str| -> &CapturedEvent {
            events
                .iter()
                .find(|e| e.layer.as_deref() == Some(name))
                .unwrap_or_else(|| {
                    panic!("no context_telemetry per-layer event for {name} — real emission path not exercised")
                })
        };
        // The summary event is the only one carrying `l3_tokens`.
        let summary = events
            .iter()
            .find(|e| e.l3_tokens.is_some())
            .expect("no context_telemetry summary event — real emission path not exercised");

        let l3 = by_layer("L3");
        let l4 = by_layer("L4");

        // (1) L3 and L4 are each counted with a positive token total.
        assert!(l3.token_count.unwrap() > 0, "L3 must count > 0 tokens");
        assert!(l4.token_count.unwrap() > 0, "L4 must count > 0 tokens");

        // (MINOR-2) L3/L4 boundary is pinned to EXACT expected per-layer totals:
        // reclassifying any tag across the boundary shifts these sums and breaks
        // the test even though both layers stay non-empty and the total holds.
        assert_eq!(
            l3.token_count,
            Some(expected_l3),
            "L3 per-layer token_count must equal recount([Project Workflow]) + recount([Project Card])"
        );
        assert_eq!(
            l4.token_count,
            Some(expected_l4),
            "L4 per-layer token_count must equal recount([Active Task] + [Session History] + unknown)"
        );
        assert_eq!(l3.block_count, Some(2), "L3 must count exactly 2 blocks");
        assert_eq!(
            l4.block_count,
            Some(3),
            "L4 must count exactly 3 blocks (incl. the unknown-tag fallback)"
        );

        // (2) Exact-by-construction sum invariant (tolerance 0), read from the
        // captured summary event.
        assert_eq!(
            summary.l3_tokens,
            Some(expected_l3),
            "summary l3_tokens must match the L3 oracle"
        );
        assert_eq!(
            summary.l4_tokens,
            Some(expected_l4),
            "summary l4_tokens must match the L4 oracle"
        );
        assert_eq!(
            summary.counted_total,
            Some(expected_total),
            "counted_total must equal l3 + l4 == Σ recount(block)"
        );
        assert_eq!(
            summary.block_count,
            Some(5),
            "all five included blocks (incl. the unknown one) must be counted"
        );

        // (5) Drift guard: the unknown tag classifies into the L4 fallback bucket.
        assert_eq!(layer_of("[Totally Unknown Block]"), Layer::UnclassifiedL4);

        // (3) L0 reported external/uncounted — explicit marker, never a fake 0.
        let l0 = by_layer("L0");
        assert_eq!(
            l0.status.as_deref(),
            Some("external_uncounted"),
            "L0 must carry an explicit uncounted status marker"
        );
        assert_eq!(
            l0.token_count, None,
            "L0 must NOT carry a numeric token_count (never a fake 0)"
        );
        // (4) L1/L2 reported uncounted_upstream (present as status, absent as
        // counted numbers).
        assert_eq!(by_layer("L1").status.as_deref(), Some("uncounted_upstream"));
        assert_eq!(by_layer("L2").status.as_deref(), Some("uncounted_upstream"));
        assert_eq!(by_layer("L1").token_count, None);
        assert_eq!(by_layer("L2").token_count, None);

        // Counted layers carry no status marker (they are numeric, not faked).
        assert_eq!(Layer::L3.uncounted_status(), None);
        assert_eq!(Layer::L4.uncounted_status(), None);
    }

    #[tokio::test]
    async fn test_pipeline_workflow_priority_ordering() {
        let counter = Arc::new(TokenCounter::new());
        let pipeline = ContextPipeline::new(counter, None, test_store());

        // Both active task and workflow content
        let result = pipeline
            .assemble(
                "test query",
                Path::new("/tmp/nonexistent-dir-12345"),
                None,
                Some("My active task"),
                Some("Workflow instructions"),
                None,
                4000,
            )
            .await;

        let output = result.expect("Should return Some with both sources");
        // Active task (priority 0) should come before workflow (priority 1)
        let task_pos = output.find("[Active Task]").unwrap();
        let wf_pos = output.find("[Project Workflow]").unwrap();
        assert!(
            task_pos < wf_pos,
            "Active task should appear before workflow"
        );
    }
}
