//! Height computation for conversation events.
//!
//! Single source of truth for per-event rendered heights.
//! Uses `Paragraph::line_count(width)` for word-wrap-accurate results.

use crate::profiling;
use crate::types::{CachedRenderedEvent, RenderCacheKey, SessionState};
use crate::ui::content;
use ratatui::widgets::{Paragraph, Wrap};
use rsi_common::types::{ConversationEvent, EventType};
use std::collections::hash_map::Entry;

/// Trailing separator row baked into every bordered card's cached height.
///
/// Cards are stacked with zero space between `event_offsets`, so without a
/// reserved gap row two consecutive same-colored bubbles (e.g. two tool
/// results, both `surface0`) read as one continuous block — the border
/// glyphs alone aren't enough contrast to separate them. The render loop
/// (`crate::ui::session`) renders each card into `event_height - EVENT_CARD_GAP`
/// rows, leaving this trailing row painted with the pane's base background
/// (already filled once per frame before any card renders).
pub(crate) const EVENT_CARD_GAP: usize = 1;

/// Compute the rendered height of a single event in terminal lines.
///
/// Uses `Paragraph::line_count(width)` for word-wrap-accurate results.
/// This remains available for tests that call the height helper directly.
pub fn event_rendered_height(
    event: &ConversationEvent,
    is_collapsed: bool,
    is_expanded: bool,
    is_cursor: bool,
    show_system_events: bool,
    show_thinking_events: bool,
    show_tool_results: bool,
    width: u16,
    event_idx: usize,
    total_events: usize,
) -> usize {
    // Invisible filler occupies no lines even with system events shown: there
    // is nothing to paint, and a blank card between a tool call and its result
    // would split the group summary it belongs inside.
    if crate::ui::tool_projection::is_transparent(event) {
        return 0;
    }
    if event.event_type == EventType::System && !show_system_events {
        return 0;
    }
    if event.event_type == EventType::Thinking && !show_thinking_events {
        return 0;
    }
    if event.event_type == EventType::ToolResult && !show_tool_results {
        return 0;
    }

    let ctx = content::EventRenderContext {
        is_collapsed,
        is_expanded,
        is_cursor,
        is_last_event: total_events > 0 && event_idx == total_events - 1,
        model_name: content::model_for_sequence(&[], event.sequence, None),
        pipeline_commands: Vec::new(),
        max_width: width,
    };

    let lines = content::build_event_lines(event, &ctx);
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Pre-render step: update cached event heights if stale.
/// Call this before `render_session_detail()` each frame.
///
/// The cache is considered stale when any of these change:
/// - Number of events (new event added)
/// - Terminal width (resize causes different word wrapping)
/// - Events generation (content updated during streaming, or events replaced)
///
/// Cursor movement does not invalidate heights because selection only changes
/// styling; each event keeps the same geometry.
///
/// Fold state changes are handled by `invalidate_heights()` which clears
/// the vectors, making the length check fail.
pub fn update_event_heights(state: &mut SessionState, width: u16) {
    let timer = profiling::start_timer();
    refresh_tool_projection(state);
    if state.last_height_generation == state.events_generation
        && state.last_render_width == width
        && state.event_heights.len() == state.events.len()
    {
        // Geometry is unchanged, but the activity-indicator slack is not part
        // of the cache key: a freshly appended event arms the formulation
        // animation, which hides the indicator while this cache is built, and
        // the indicator returns ~500ms later with no new generation. Re-derive
        // the slack from the cached offsets so the tail keeps its separator
        // row above the indicator instead of butting against it until the
        // next full recompute.
        let content_height = state
            .event_offsets
            .last()
            .zip(state.event_heights.last())
            .map_or(0, |(offset, height)| offset + height);
        state.total_content_height = content_height + activity_indicator_slack(state);
        return;
    }

    state.event_heights.clear();
    state.event_offsets.clear();
    let mut offset = 0;
    let mut in_thinking_run = false;

    let cursor_idx = state.current_event_index;
    let total_events = state.events.len();

    for i in 0..total_events {
        let is_cursor = cursor_idx == Some(i);
        let event_type = state.events[i].event_type;
        let sequence = state.events[i].sequence;

        if crate::ui::tool_projection::is_transparent(&state.events[i])
            || (event_type == EventType::System && !state.show_system_events)
        {
            state.event_heights.push(0);
            state.event_offsets.push(offset);
            continue;
        }

        if event_type == EventType::ToolResult && !state.show_tool_results {
            state.event_heights.push(0);
            state.event_offsets.push(offset);
            continue;
        }

        if !state.show_thinking_events && event_type == EventType::Thinking {
            if !in_thinking_run {
                in_thinking_run = true;
                state.event_heights.push(2);
                state.event_offsets.push(offset);
                offset += 2;
            } else {
                state.event_heights.push(0);
                state.event_offsets.push(offset);
            }
            continue;
        } else {
            in_thinking_run = false;
        }

        // --- Tool group collapsing ---
        // Tool activity is grouped by `ui::tool_projection`: a ToolUse and the
        // ToolResult carrying the same `tool_use_id` are one unit, and the
        // parallel calls of a single turn share one group (pre-migration rows
        // with no id keep the historical adjacency-run grouping). A collapsed
        // group renders as one quiet summary line on its leader plus the trailing
        // separator row; every other
        // member takes height 0 — folded away in the view only, never dropped
        // from `events`.
        let is_effectively_collapsed = is_event_effectively_collapsed(state, &state.events[i]);
        let group_state = state
            .tool_projection
            .group_of(i)
            .map(|group| (group.leader_idx() == i, group_is_collapsed(state, group)));

        if let Some((is_leader, collapsed)) = group_state
            && is_effectively_collapsed
        {
            if collapsed {
                if is_leader {
                    let h = 1 + EVENT_CARD_GAP;
                    state.event_heights.push(h);
                    state.event_offsets.push(offset);
                    offset += h;
                } else {
                    // Absorbed into the group summary.
                    state.event_heights.push(0);
                    state.event_offsets.push(offset);
                }
            } else {
                // Group expanded — show this item as an individual collapsed card.
                let height = ensure_render_entry(state, i, width, is_cursor).unwrap_or_else(|| {
                    event_rendered_height(
                        &state.events[i],
                        true,
                        false,
                        is_cursor,
                        state.show_system_events,
                        state.show_thinking_events,
                        state.show_tool_results,
                        width,
                        i,
                        total_events,
                    )
                });
                state.event_heights.push(height);
                state.event_offsets.push(offset);
                offset += height;
            }
            continue;
        }

        if event_type == EventType::ToolUse && state.events[i].tool_input.is_none() {
            let h = 1 + EVENT_CARD_GAP;
            state.event_heights.push(h);
            state.event_offsets.push(offset);
            offset += h;
            continue;
        }

        // Every event uses a four-column horizontal inset: role rail + breathing
        // room for messages, border + padding for operational cards.
        let render_width = width.saturating_sub(4);
        let height = ensure_render_entry(state, i, render_width, is_cursor).unwrap_or_else(|| {
            let event = &state.events[i];
            let is_collapsed = is_event_effectively_collapsed(state, event);
            let is_expanded = state.expanded_events.contains(&sequence);
            let ctx = content::EventRenderContext {
                is_collapsed,
                is_expanded,
                is_cursor,
                is_last_event: total_events > 0 && i == total_events - 1,
                model_name: content::model_for_sequence(
                    &state.model_segments,
                    event.sequence,
                    state.session.model.as_deref(),
                ),
                pipeline_commands: Vec::new(),
                max_width: render_width,
            };
            let lines = content::build_event_lines(event, &ctx);
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .line_count(render_width)
        });
        // Conversation messages use a borderless role rail, while operational events
        // retain card borders. Both reserve one quiet separator row.
        let display_height = if event_type == EventType::Message {
            height + EVENT_CARD_GAP
        } else {
            height + 2 + EVENT_CARD_GAP
        };

        state.event_heights.push(display_height);
        state.event_offsets.push(offset);
        offset += display_height;
    }

    state.total_content_height = offset + activity_indicator_slack(state);
    state.last_render_width = width;
    state.last_height_generation = state.events_generation;
    state.last_cursor_index = state.current_event_index;

    // Evict stale cache entries from previous generations to bound memory.
    // Entries with outdated generation will never be served (generation check in
    // cached_render_event / ensure_render_entry), so they're dead weight.
    let generation = state.events_generation;
    let cache_len_before = state.render_cache.len();
    state.render_cache.retain(|_, v| v.generation == generation);
    let _evicted = cache_len_before.saturating_sub(state.render_cache.len());

    if let Some(start) = timer {
        let elapsed = start.elapsed();
        tracing::trace!(
            target = "rsi::profile",
            session_id = %state.session.id,
            width,
            events = total_events,
            cache_entries = state.render_cache.len(),
            evicted = _evicted,
            ms = elapsed.as_secs_f64() * 1000.0,
            "height::update_event_heights"
        );
    }
}

/// Extra scrollable row reserved above the activity indicator while the
/// session is live. Mirrors the `show_loading_bar` predicate in
/// `ui::mod` / `render_session_detail`, including the formulation-animation
/// hold-off that temporarily hides the indicator.
fn activity_indicator_slack(state: &SessionState) -> usize {
    let is_active = matches!(
        state.session.status,
        rsi_common::types::SessionStatus::Running | rsi_common::types::SessionStatus::Starting
    );
    let formulation_active = state
        .formulation
        .as_ref()
        .map(|f| {
            chrono::Utc::now().timestamp_millis() - f.started_at_ms
                < crate::ui::session::FORMULATION_ANIMATION_MS
        })
        .unwrap_or(false);
    let show_loading_bar = is_active && state.last_content_area.height > 5 && !formulation_active;
    usize::from(show_loading_bar)
}

fn ensure_render_entry(
    state: &mut SessionState,
    event_idx: usize,
    width: u16,
    is_cursor: bool,
) -> Option<usize> {
    let event = state.events.get(event_idx)?;
    let (key, ctx) = render_plan(state, event_idx, width, is_cursor)?;
    let generation = state.events_generation;

    let height = match state.render_cache.entry(key) {
        Entry::Occupied(mut entry) => {
            if entry.get().generation == generation {
                profiling::record_cache_hit();
                entry.get().height
            } else {
                let cached = build_cache_entry(event, &ctx, width, generation);
                let height = cached.height;
                entry.insert(cached);
                profiling::record_cache_miss();
                height
            }
        }
        Entry::Vacant(vacant) => {
            let cached = build_cache_entry(event, &ctx, width, generation);
            let height = cached.height;
            vacant.insert(cached);
            profiling::record_cache_miss();
            height
        }
    };

    Some(height)
}

pub fn cached_render_event<'a>(
    state: &'a SessionState,
    event_idx: usize,
    width: u16,
    is_cursor: bool,
) -> Option<&'a CachedRenderedEvent> {
    let (key, _) = render_plan(state, event_idx, width, is_cursor)?;
    state
        .render_cache
        .get(&key)
        .filter(|entry| entry.generation == state.events_generation)
}

fn render_plan(
    state: &SessionState,
    event_idx: usize,
    width: u16,
    is_cursor: bool,
) -> Option<(RenderCacheKey, content::EventRenderContext)> {
    let event = state.events.get(event_idx)?;
    let is_collapsed = is_event_effectively_collapsed(state, event);
    let is_expanded = state.expanded_events.contains(&event.sequence);
    let is_last_event = state
        .events
        .len()
        .checked_sub(1)
        .map_or(false, |last| event_idx == last);

    let model_name = content::model_for_sequence(
        &state.model_segments,
        event.sequence,
        state.session.model.as_deref(),
    );
    let model_label_hash = hash_model_label(model_name.as_deref());
    let ctx = content::EventRenderContext {
        is_collapsed,
        is_expanded,
        is_cursor,
        is_last_event,
        model_name,
        pipeline_commands: Vec::new(),
        max_width: width,
    };
    let key = RenderCacheKey {
        sequence: event.sequence,
        width,
        is_collapsed,
        is_expanded,
        is_cursor,
        is_last_event,
        model_label_hash,
    };
    Some((key, ctx))
}

fn hash_model_label(model_name: Option<&str>) -> u64 {
    model_name
        .unwrap_or("")
        .bytes()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn build_cache_entry(
    event: &ConversationEvent,
    ctx: &content::EventRenderContext,
    width: u16,
    generation: u64,
) -> CachedRenderedEvent {
    let (lines, code_blocks, file_links, web_links) =
        content::build_event_lines_with_interaction_meta(event, ctx);
    let height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(width);
    CachedRenderedEvent {
        lines,
        height,
        generation,
        code_blocks,
        file_links,
        web_links,
    }
}

/// Refresh the cached tool-pairing projection for `state`.
///
/// Cheap and idempotent: it returns immediately while the projection already
/// covers `events`. On the common polling path (events appended, nothing else
/// touched) the projection is *extended* over the new tail rather than rebuilt,
/// which is what keeps this off the per-frame cost curve as transcripts grow.
/// A full rebuild happens only when the previously projected tail no longer
/// matches — i.e. the prefix was replaced, reordered, or truncated.
pub fn refresh_tool_projection(state: &mut SessionState) {
    let projected = state.tool_projection.events_len();
    if projected == state.events.len()
        && state.tool_projection_generation == state.events_generation
    {
        return;
    }

    let tail_intact = projected > 0
        && projected <= state.events.len()
        && state
            .events
            .get(projected - 1)
            .map(event_fingerprint)
            .as_ref()
            == state.tool_projection_tail.as_ref();

    if !tail_intact {
        state.tool_projection = crate::ui::tool_projection::Projection::default();
    }
    state.tool_projection.extend(&state.events);
    state.tool_projection_generation = state.events_generation;
    state.tool_projection_tail = state.events.last().map(event_fingerprint);
}

fn event_fingerprint(event: &ConversationEvent) -> (i32, EventType, Option<String>) {
    (event.sequence, event.event_type, event.tool_use_id.clone())
}

/// Whether `group` should render as a single collapsed summary.
///
/// A group collapses only when the user has not expanded it (`zo` records the
/// leader in `expanded_tool_groups`) *and* every member is still effectively
/// collapsed. The second clause is what makes `OpenAllFolds`/`zR` and an
/// individually expanded member win over the group default instead of being
/// silently re-folded.
pub(crate) fn group_is_collapsed(
    state: &SessionState,
    group: &crate::ui::tool_projection::ToolGroup,
) -> bool {
    !state
        .expanded_tool_groups
        .contains(&state.events[group.leader_idx()].sequence)
        && group.members.iter().all(|&idx| {
            state
                .events
                .get(idx)
                .is_some_and(|e| is_event_effectively_collapsed(state, e))
        })
}

/// Every event index in the tool group containing `event_idx`, ascending.
///
/// Unlike the adjacency run it replaces, this is not necessarily a contiguous
/// range: a result that arrived after later calls belongs to its own call's
/// group wherever it sits in the stream.
pub fn tool_group_members(state: &mut SessionState, event_idx: usize) -> Option<Vec<usize>> {
    refresh_tool_projection(state);
    state
        .tool_projection
        .group_of(event_idx)
        .map(|g| g.members.clone())
}

/// Sequence number of the leader (summary-carrying row) of the tool group
/// containing `event_idx`. This is the key used in `expanded_tool_groups`.
pub fn tool_group_leader_seq(state: &mut SessionState, event_idx: usize) -> Option<i32> {
    refresh_tool_projection(state);
    let leader = state.tool_projection.group_of(event_idx)?.leader_idx();
    Some(state.events[leader].sequence)
}

/// Whether `event` should render as collapsed by default (or via explicit user
/// action). Symmetric across `ToolUse` (with `tool_input`) and `ToolResult` —
/// both default to collapsed until their sequence is added to `expanded_events`
/// (fixes F-009's asymmetry, where only `ToolResult` used to default collapsed).
/// Guarded via `is_compact_tool_event` so an input-less `ToolUse` (rendered by
/// `render_compact_tool_event`, never collapsible) is never swept into the
/// tool-group collapse path.
pub(crate) fn is_event_effectively_collapsed(
    state: &SessionState,
    event: &ConversationEvent,
) -> bool {
    state.collapsed_events.contains(&event.sequence)
        || (matches!(event.event_type, EventType::ToolUse | EventType::ToolResult)
            && !crate::ui::session::is_compact_tool_event(event)
            && !state.expanded_events.contains(&event.sequence))
}

/// Invalidate the height/offset vectors, forcing recomputation on the next render.
///
/// Does NOT clear the render cache — fold state and cursor position are part of
/// the cache key, so changed fold/cursor simply results in different lookups.
/// Old entries remain and serve as fast paths if the user toggles fold back.
pub fn invalidate_heights(state: &mut SessionState) {
    state.event_heights.clear();
    state.event_offsets.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{ConversationEvent, EventType, Role};

    fn make_event(
        event_type: EventType,
        content: &str,
        tool_name: Option<&str>,
        tool_input: Option<serde_json::Value>,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type,
            role: Some(Role::Assistant),
            content: content.to_string(),
            tool_name: tool_name.map(|s| s.to_string()),
            tool_input: tool_input.map(Box::new),
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    // Operational cards retain padding; conversation messages deliberately use
    // compact header+body geometry because their role rail supplies separation.

    #[test]
    fn test_collapsed_event_returns_4() {
        // padding-aware-bounds (RSI-020): renamed from _returns_2 after b4f1cf74 added top + bottom padding rows.
        let event = make_event(EventType::ToolUse, "long content", Some("Read"), None);
        // Collapsed: top-pad + summary + blank + bottom-pad = 4
        let height = event_rendered_height(&event, true, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 4);
    }

    #[test]
    fn test_collapsed_tool_result_returns_4() {
        // padding-aware-bounds (RSI-020): renamed from _returns_2 after b4f1cf74 added top + bottom padding rows.
        let event = make_event(EventType::ToolResult, "result content", None, None);
        // Collapsed: top-pad + summary + blank + bottom-pad = 4
        let height = event_rendered_height(&event, true, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 4);
    }

    #[test]
    fn test_message_event_basic() {
        // Borderless message: 1 header + 1 content.
        let event = make_event(EventType::Message, "hello", None, None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 2);
    }

    #[test]
    fn test_message_event_multiline() {
        // Borderless message: 1 header + 3 content rows.
        let event = make_event(EventType::Message, "line1\nline2\nline3", None, None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 4);
    }

    #[test]
    fn test_tool_use_with_input() {
        // padding-aware-bounds (RSI-020): 1 top-pad + 1 header + 1 tool-name + 1 tool-input + 2 content + 1 blank = 7
        let event = make_event(
            EventType::ToolUse,
            "x\ny",
            Some("Read"),
            Some(serde_json::json!({"path": "/tmp"})),
        );
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 7);
    }

    #[test]
    fn test_tool_use_no_input() {
        // padding-aware-bounds (RSI-020): 1 top-pad + 1 header + 1 tool-name + 1 blank = 4
        let event = make_event(EventType::ToolUse, "", Some("Read"), None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 4);
    }

    #[test]
    fn test_tool_result_event() {
        // padding-aware-bounds (RSI-020): 1 top-pad + 1 header + 1 result-marker + 1 content + 1 blank = 5
        let event = make_event(EventType::ToolResult, "success", None, None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 5);
    }

    #[test]
    fn test_system_event() {
        // padding-aware-bounds (RSI-020): 1 top-pad + 1 header + 1 system-marker
        // + 1 content + 1 blank = 5. (The card was measured empty before empty
        // system rows became zero-height; the chrome is the same 4 lines.)
        let event = make_event(EventType::System, "context low", None, None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 5);
    }

    #[test]
    fn content_less_system_event_is_zero_height_even_when_shown() {
        // Claude CLI filler (see `ui::tool_projection::is_transparent`): an
        // empty card is nothing to read, and it would break the tool group it
        // sits inside in two.
        let event = make_event(EventType::System, "", None, None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 0);
    }

    #[test]
    fn test_empty_content_message() {
        // Empty borderless message still exposes its header.
        let event = make_event(EventType::Message, "", None, None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height, 1);
    }

    #[test]
    fn test_truncation_at_max_content_lines() {
        // Create content with more than MAX_CONTENT_LINES
        let long_content: String = (0..50)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let event = make_event(EventType::Message, &long_content, None, None);

        // Not collapsed: should show all lines (truncation only applies when in collapsed_events set)
        let height_uncollapsed =
            event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        assert_eq!(height_uncollapsed, 1 + 50);

        // Collapsed (in collapsed_events), not expanded: should be truncated
        // build_event_lines returns early if is_collapsed=true with just summary line
        // This is expected: when user collapses a single event, they only see the summary.
        // Truncation applies separately when content is too long to display unfolded.
        // For this test, we skip the is_collapsed=true case since it's not the truncation scenario.

        // Expanded: should show all lines
        let height_expanded =
            event_rendered_height(&event, false, true, false, true, true, true, 80, 0, 1);
        assert_eq!(height_expanded, 1 + 50);
    }

    #[test]
    fn test_code_block_with_language() {
        let content = "text\n```rust\nfn main() {}\n```\nmore";
        let event = make_event(EventType::Message, content, None, None);
        let height = event_rendered_height(&event, false, false, false, true, true, true, 80, 0, 1);
        // Borderless message: header + text + language + code + trailing text.
        assert_eq!(height, 5);
    }

    #[test]
    fn test_width_affects_height() {
        // A very long line that wraps at narrow widths
        let long_line = "a".repeat(200);
        let event = make_event(EventType::Message, &long_line, None, None);

        let height_wide =
            event_rendered_height(&event, false, false, false, true, true, true, 200, 0, 1);
        let height_narrow =
            event_rendered_height(&event, false, false, false, true, true, true, 40, 0, 1);

        // Narrow width should produce more lines due to wrapping
        assert!(height_narrow > height_wide);
    }

    // --- Invalidation tests ---

    fn make_test_state() -> SessionState {
        use rsi_common::types::{
            ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
        };
        let session = Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            session_kind: SessionKind::Standard,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: ContextUsageConfidence::default(),
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        };
        let mut state = SessionState::new(session);
        state.events = vec![
            make_event(EventType::Message, "hello", None, None),
            make_event(EventType::Message, "world", None, None),
        ];
        state
    }

    #[test]
    fn test_update_event_heights_populates_cache() {
        let mut state = make_test_state();
        assert!(state.event_heights.is_empty());
        assert!(state.event_offsets.is_empty());

        update_event_heights(&mut state, 80);

        assert_eq!(state.event_heights.len(), 2);
        assert_eq!(state.event_offsets.len(), 2);
        assert_eq!(state.event_offsets[0], 0);
        // Second event starts after first event's height
        assert_eq!(state.event_offsets[1], state.event_heights[0]);
    }

    #[test]
    fn test_cache_skips_when_valid() {
        let mut state = make_test_state();
        update_event_heights(&mut state, 80);

        let heights_before = state.event_heights.clone();

        // Call again with same width and generation — should be a no-op
        update_event_heights(&mut state, 80);

        assert_eq!(state.event_heights, heights_before);
    }

    #[test]
    fn cached_path_restores_activity_indicator_slack_after_formulation_ends() {
        let mut state = make_test_state();
        state.session.status = rsi_common::types::SessionStatus::Running;
        state.last_content_area = ratatui::layout::Rect::new(0, 0, 80, 40);

        // A freshly appended event arms the formulation animation, which
        // hides the activity indicator while the height cache is built.
        state.formulation = Some(crate::types::FormulationState {
            event_index: 1,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            target_height: 0,
        });
        update_event_heights(&mut state, 80);
        let content_height: usize = state.event_heights.iter().sum();
        assert_eq!(
            state.total_content_height, content_height,
            "indicator hidden during formulation: no slack row"
        );

        // Animation expires with no new events, width, or generation: the
        // cached path must still reserve the slack row above the indicator.
        state.formulation = None;
        update_event_heights(&mut state, 80);
        assert_eq!(
            state.total_content_height,
            content_height + 1,
            "tail separator must sit above the activity indicator"
        );

        // Session goes idle: slack disappears again on the cached path.
        state.session.status = rsi_common::types::SessionStatus::Completed;
        update_event_heights(&mut state, 80);
        assert_eq!(state.total_content_height, content_height);
    }

    #[test]
    fn test_resize_invalidates_cache() {
        let mut state = make_test_state();
        // Use a long line that wraps differently at different widths
        state.events = vec![make_event(EventType::Message, &"a".repeat(200), None, None)];

        update_event_heights(&mut state, 80);
        let height_at_80 = state.event_heights[0];

        // Resize to narrower width
        update_event_heights(&mut state, 40);
        let height_at_40 = state.event_heights[0];

        assert!(
            height_at_40 > height_at_80,
            "narrower width should produce taller events"
        );
        assert_eq!(state.last_render_width, 40);
    }

    #[test]
    fn test_generation_bump_invalidates_cache() {
        let mut state = make_test_state();
        update_event_heights(&mut state, 80);

        let heights_before = state.event_heights.clone();

        // Simulate streaming: replace event content and bump generation
        state.events[0] = make_event(EventType::Message, "hello\nworld\nextra line", None, None);
        state.events_generation += 1;

        update_event_heights(&mut state, 80);

        // Heights should have been recomputed (first event is now taller)
        assert_ne!(state.event_heights, heights_before);
        assert!(state.event_heights[0] > heights_before[0]);
    }

    #[test]
    fn test_generation_without_bump_keeps_stale_cache() {
        let mut state = make_test_state();
        update_event_heights(&mut state, 80);

        let heights_before = state.event_heights.clone();

        // Modify content WITHOUT bumping generation (simulates a bug)
        state.events[0] = make_event(EventType::Message, "hello\nworld\nextra line", None, None);

        update_event_heights(&mut state, 80);

        // Cache should still be stale (same generation, same count, same width)
        assert_eq!(state.event_heights, heights_before);
    }

    #[test]
    fn test_invalidate_heights_forces_recompute() {
        let mut state = make_test_state();
        update_event_heights(&mut state, 80);

        assert!(!state.event_heights.is_empty());

        invalidate_heights(&mut state);

        assert!(state.event_heights.is_empty());
        assert!(state.event_offsets.is_empty());

        // Should recompute on next call
        update_event_heights(&mut state, 80);
        assert_eq!(state.event_heights.len(), 2);
    }

    #[test]
    fn test_system_event_hidden_returns_zero_height() {
        let mut state = make_test_state();
        state.events = vec![
            make_event(EventType::Message, "hello", None, None),
            make_event(EventType::System, "info", None, None),
            make_event(EventType::Message, "world", None, None),
        ];
        state.show_system_events = false;

        update_event_heights(&mut state, 80);

        // System event (index 1) should have height 0
        assert_eq!(state.event_heights[1], 0);
        // Offsets: event 0 starts at 0, event 1 at height[0], event 2 at height[0] (same, since system is 0)
        assert_eq!(state.event_offsets[2], state.event_heights[0]);
    }

    #[test]
    fn test_system_event_shown_has_normal_height() {
        let mut state = make_test_state();
        state.events = vec![
            make_event(EventType::Message, "hello", None, None),
            make_event(EventType::System, "info", None, None),
        ];
        state.show_system_events = true;

        update_event_heights(&mut state, 80);

        // System event should have non-zero height when shown
        assert!(state.event_heights[1] > 0);
    }

    #[test]
    fn test_new_event_invalidates_cache() {
        let mut state = make_test_state();
        update_event_heights(&mut state, 80);

        assert_eq!(state.event_heights.len(), 2);

        // Add a new event (simulates new event arriving)
        state
            .events
            .push(make_event(EventType::System, "info", None, None));
        state.events_generation += 1;

        update_event_heights(&mut state, 80);

        assert_eq!(state.event_heights.len(), 3);
        assert_eq!(state.event_offsets.len(), 3);
    }

    // --- PI-4: is_event_effectively_collapsed truth table (D1, F-009) ---

    #[test]
    fn tool_use_with_input_defaults_collapsed() {
        // F-009 fix: ToolUse (with tool_input) must default to collapsed,
        // symmetric with ToolResult — previously only ToolResult defaulted
        // collapsed.
        let state = make_test_state();
        let mut event = make_event(
            EventType::ToolUse,
            "content",
            Some("Read"),
            Some(serde_json::json!({"path": "/tmp"})),
        );
        event.sequence = 5;
        assert!(
            is_event_effectively_collapsed(&state, &event),
            "a fresh ToolUse with tool_input must default to collapsed"
        );
    }

    #[test]
    fn tool_use_without_input_never_defaults_collapsed() {
        // Compact-row treatment (render_compact_tool_event) must never be
        // routed into the tool-group collapse path via the default-collapse
        // clause (the `is_compact_tool_event` guard).
        let state = make_test_state();
        let mut event = make_event(EventType::ToolUse, "", Some("Bash"), None);
        event.sequence = 6;
        assert!(
            !is_event_effectively_collapsed(&state, &event),
            "an input-less ToolUse (compact row) must never report collapsed by default"
        );
    }

    #[test]
    fn tool_result_defaults_collapsed_regression_pin() {
        // Unchanged pre-D1 behavior — pinned so a refactor cannot flip it.
        let state = make_test_state();
        let mut event = make_event(EventType::ToolResult, "result", None, None);
        event.sequence = 7;
        assert!(
            is_event_effectively_collapsed(&state, &event),
            "ToolResult must still default to collapsed"
        );
    }

    #[test]
    fn explicit_expanded_events_overrides_tool_use_default_collapse() {
        // Direction 1: an explicit `expanded_events` entry overrides the
        // default-collapsed formula toward "not collapsed".
        let mut state = make_test_state();
        let mut event = make_event(
            EventType::ToolUse,
            "content",
            Some("Read"),
            Some(serde_json::json!({"path": "/tmp"})),
        );
        event.sequence = 8;
        state.expanded_events.insert(8);
        assert!(
            !is_event_effectively_collapsed(&state, &event),
            "explicit expanded_events membership must override the collapsed-by-default formula"
        );
    }

    // --- Tool-group heights over the id-based projection ---
    //
    // These pin the contract the render loop depends on: in a collapsed group
    // exactly one row (the leader) reserves the summary card's `3 +
    // EVENT_CARD_GAP` lines and every other member reserves 0. A mismatch here
    // is a scroll/clipping bug, because `event_offsets` is what maps scroll
    // position to events.

    /// Summary line plus its separator row.
    const SUMMARY_H: usize = 1 + EVENT_CARD_GAP;

    fn tool_event(
        sequence: i32,
        event_type: EventType,
        tool_use_id: Option<&str>,
    ) -> ConversationEvent {
        let mut e = make_event(
            event_type,
            "content",
            Some("Read"),
            (event_type == EventType::ToolUse).then(|| serde_json::json!({"path": "/tmp"})),
        );
        e.sequence = sequence;
        e.tool_use_id = tool_use_id.map(|s| s.to_string());
        e
    }

    fn heights_for(events: Vec<ConversationEvent>) -> SessionState {
        let mut state = make_test_state();
        state.events = events;
        state.events_generation += 1;
        update_event_heights(&mut state, 80);
        state
    }

    #[test]
    fn collapsed_pair_reserves_exactly_one_summary_card() {
        let state = heights_for(vec![
            tool_event(1, EventType::ToolUse, Some("t1")),
            tool_event(2, EventType::ToolResult, Some("t1")),
        ]);

        assert_eq!(state.event_heights, vec![SUMMARY_H, 0]);
        assert_eq!(state.event_offsets, vec![0, SUMMARY_H]);
        assert_eq!(state.total_content_height, SUMMARY_H);
    }

    #[test]
    fn parallel_calls_collapse_to_a_single_summary() {
        // The case adjacency grouping could not express: one turn, three calls,
        // results returned out of order afterwards. All six rows are one group,
        // so exactly one summary card is reserved.
        let state = heights_for(vec![
            tool_event(1, EventType::ToolUse, Some("a")),
            tool_event(2, EventType::ToolUse, Some("b")),
            tool_event(3, EventType::ToolUse, Some("c")),
            tool_event(4, EventType::ToolResult, Some("c")),
            tool_event(5, EventType::ToolResult, Some("a")),
            tool_event(6, EventType::ToolResult, Some("b")),
        ]);

        assert_eq!(state.event_heights, vec![SUMMARY_H, 0, 0, 0, 0, 0]);
        assert_eq!(state.total_content_height, SUMMARY_H);
    }

    #[test]
    fn orphan_call_still_reserves_its_card() {
        // Interrupted or still-running: the result never arrives. The call must
        // remain visible, not vanish into a zero-height row.
        let state = heights_for(vec![tool_event(1, EventType::ToolUse, Some("a"))]);

        assert_eq!(state.event_heights, vec![SUMMARY_H]);
        assert_eq!(state.total_content_height, SUMMARY_H);
    }

    #[test]
    fn orphan_result_leads_its_own_card() {
        let state = heights_for(vec![tool_event(1, EventType::ToolResult, Some("ghost"))]);

        assert_eq!(state.event_heights, vec![SUMMARY_H]);
    }

    #[test]
    fn null_tool_use_id_keeps_historical_adjacency_heights() {
        // Pre-migration transcript: one contiguous run, one summary card,
        // exactly as before the projection existed.
        let state = heights_for(vec![
            tool_event(1, EventType::ToolUse, None),
            tool_event(2, EventType::ToolResult, None),
            tool_event(3, EventType::ToolUse, None),
            tool_event(4, EventType::ToolResult, None),
        ]);

        assert_eq!(state.event_heights, vec![SUMMARY_H, 0, 0, 0]);
    }

    #[test]
    fn separate_turns_reserve_separate_summary_cards() {
        let mut message = make_event(EventType::Message, "between", None, None);
        message.sequence = 3;
        let state = heights_for(vec![
            tool_event(1, EventType::ToolUse, Some("a")),
            tool_event(2, EventType::ToolResult, Some("a")),
            message,
            tool_event(4, EventType::ToolUse, Some("b")),
        ]);

        assert_eq!(state.event_heights[0], SUMMARY_H);
        assert_eq!(state.event_heights[1], 0);
        assert!(state.event_heights[2] > 0, "the message renders normally");
        assert_eq!(state.event_heights[3], SUMMARY_H);
    }

    #[test]
    fn expanding_a_group_gives_every_member_its_own_height() {
        // What `zo` does: every member leaves the collapsed set, so no row is
        // absorbed and the offsets stay a running sum of the heights.
        let events = vec![
            tool_event(1, EventType::ToolUse, Some("a")),
            tool_event(2, EventType::ToolUse, Some("b")),
            tool_event(3, EventType::ToolResult, Some("a")),
            tool_event(4, EventType::ToolResult, Some("b")),
        ];
        let mut state = make_test_state();
        state.events = events;
        state.events_generation += 1;
        for seq in 1..=4 {
            state.expanded_events.insert(seq);
        }
        state.expanded_tool_groups.insert(1);
        update_event_heights(&mut state, 80);

        assert!(
            state.event_heights.iter().all(|&h| h > 0),
            "no member may be absorbed once the group is expanded: {:?}",
            state.event_heights
        );
        let mut running = 0;
        for (i, &h) in state.event_heights.iter().enumerate() {
            assert_eq!(state.event_offsets[i], running, "offset drift at {}", i);
            running += h;
        }
        assert_eq!(state.total_content_height, running);
    }

    #[test]
    fn offsets_are_the_running_sum_of_heights_for_a_collapsed_group() {
        let mut message = make_event(EventType::Message, "after", None, None);
        message.sequence = 9;
        let state = heights_for(vec![
            tool_event(1, EventType::ToolUse, Some("a")),
            tool_event(2, EventType::ToolUse, Some("b")),
            tool_event(3, EventType::ToolResult, Some("a")),
            tool_event(4, EventType::ToolResult, Some("b")),
            message,
        ]);

        let mut running = 0;
        for (i, &h) in state.event_heights.iter().enumerate() {
            assert_eq!(state.event_offsets[i], running, "offset drift at {}", i);
            running += h;
        }
        assert_eq!(state.total_content_height, running);
    }

    #[test]
    fn projection_extends_on_append_without_a_rebuild() {
        // Polling appends; the cached projection must absorb the new result into
        // the existing group rather than starting a second one.
        let mut state = make_test_state();
        state.events = vec![tool_event(1, EventType::ToolUse, Some("a"))];
        state.events_generation += 1;
        update_event_heights(&mut state, 80);
        assert_eq!(state.tool_projection.groups().len(), 1);
        assert_eq!(state.tool_projection.groups()[0].pending_count(), 1);

        state
            .events
            .push(tool_event(2, EventType::ToolResult, Some("a")));
        state.events_generation += 1;
        invalidate_heights(&mut state);
        update_event_heights(&mut state, 80);

        assert_eq!(state.tool_projection.groups().len(), 1);
        assert_eq!(state.tool_projection.groups()[0].pending_count(), 0);
        assert_eq!(state.event_heights, vec![SUMMARY_H, 0]);
    }

    #[test]
    fn replacing_the_event_stream_rebuilds_the_projection() {
        // Session refresh / rotation replaces `events` wholesale; the stale
        // projection must not survive it.
        let mut state = make_test_state();
        state.events = vec![
            tool_event(1, EventType::ToolUse, Some("a")),
            tool_event(2, EventType::ToolResult, Some("a")),
        ];
        state.events_generation += 1;
        update_event_heights(&mut state, 80);

        state.events = vec![make_event(EventType::Message, "fresh", None, None)];
        state.events_generation += 1;
        invalidate_heights(&mut state);
        update_event_heights(&mut state, 80);

        assert_eq!(state.tool_projection.events_len(), 1);
        assert!(state.tool_projection.groups().is_empty());
        assert_eq!(state.event_heights.len(), 1);
    }

    #[test]
    fn explicit_collapsed_events_overrides_compact_tool_use_default() {
        // Direction 2: an explicit `collapsed_events` entry overrides the
        // is_compact_tool_event guard toward "collapsed" — the guard only
        // suppresses the AUTOMATIC default-collapse clause, not an explicit
        // user-driven collapse.
        let mut state = make_test_state();
        let mut event = make_event(EventType::ToolUse, "", Some("Bash"), None);
        event.sequence = 9;
        state.collapsed_events.insert(9);
        assert!(
            is_event_effectively_collapsed(&state, &event),
            "explicit collapsed_events membership must override the compact-guard default"
        );
    }
}
