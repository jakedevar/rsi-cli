//! Projection layer: `[ConversationEvent] -> [ToolGroup]`.
//!
//! The conversation transcript is a flat event stream in which a tool call
//! (`EventType::ToolUse`) and its outcome (`EventType::ToolResult`) are two
//! separate rows. For display they are one thing. This module owns that
//! reinterpretation and nothing else — it is deliberately terminal-free and
//! render-free so the pairing rules are unit-testable without a `Frame`.
//!
//! # Why not adjacency
//!
//! The previous grouping rule collapsed a *maximal run of consecutive tool
//! events* into one summary. That is a heuristic, and it breaks precisely where
//! agents are most interesting: a single assistant turn emitting N parallel
//! tool calls whose results come back interleaved or in one late batch. Runs
//! either fused unrelated calls or split a call from its own result.
//!
//! V96 added `ConversationEvent.tool_use_id`, so pairing is now exact. Rules:
//!
//! - **Pair strictly by id.** A `ToolResult` joins the `ToolUse` carrying the
//!   same `tool_use_id`, however far apart they sit in the stream.
//! - **Parallel calls batch.** Consecutive groupable `ToolUse` rows accumulate
//!   into one [`ToolGroup`] (one [`ToolPair`] each), so a fan-out turn collapses
//!   to a single summary. Absorbing a result does not close the batch.
//! - **Orphan call** (no result yet — still running, interrupted, or failed):
//!   kept as a pair with `result: None` and reported by
//!   [`ToolGroup::pending_count`]. Never dropped.
//! - **Orphan result** (no matching call — e.g. the call row was never
//!   persisted): kept as its own group with `call: None`. Never dropped.
//! - **Invisible rows are transparent.** A content-less `System` row (see
//!   [`is_transparent`]) paints nothing, so it neither joins a group nor ends
//!   one. The Claude CLI emits a stream of these around every tool call; taking
//!   them for separators is what rendered a Claude turn as a column of
//!   "1 tool call" summaries where the same turn from Codex read "N tool calls".
//! - **Pre-migration rows** (`tool_use_id == None`): unpairable. These fall back
//!   to the historical adjacency-run behavior ([`GroupKind::Legacy`]) so old
//!   transcripts render exactly as they did before. Pairings are never guessed.
//!
//! Collapse state (`collapsed_events` / `expanded_events` /
//! `expanded_tool_groups`) is intentionally *not* an input here. The projection
//! is a pure function of the event stream, which is what makes it cacheable
//! against `events_generation` and extensible in place on append; view state is
//! applied later, at height/render time. Collapsing is a view concern only — no
//! event is ever dropped or truncated by this module.

use rsi_common::types::{ConversationEvent, EventType};
use std::collections::HashMap;

/// One tool invocation as the user should see it.
///
/// At least one side is always `Some`; a pair with both sides `None` is never
/// constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPair {
    /// Index into `events` of the `ToolUse` row, if it exists.
    pub call: Option<usize>,
    /// Index into `events` of the matching `ToolResult` row, if it has arrived.
    pub result: Option<usize>,
}

/// How a group's members were associated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    /// Members were joined by `tool_use_id`. Pair counts are meaningful.
    Paired,
    /// Pre-migration rows with no `tool_use_id`; members are a contiguous
    /// adjacency run and each event is its own (unpaired) entry, reproducing
    /// the historical grouping and its "N tool calls" count exactly.
    Legacy,
}

/// A contiguous-in-display unit of tool activity that collapses as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolGroup {
    pub kind: GroupKind,
    /// The invocations in this group, in call order.
    pub pairs: Vec<ToolPair>,
    /// Every event index belonging to this group, ascending. `members[0]` is
    /// the group leader — the row that carries the collapsed summary and the
    /// group's whole rendered height.
    pub members: Vec<usize>,
}

impl ToolGroup {
    /// Event index of the row that renders the collapsed summary.
    pub fn leader_idx(&self) -> usize {
        self.members[0]
    }

    /// Number of tool calls the summary should advertise.
    pub fn call_count(&self) -> usize {
        self.pairs.len()
    }

    /// Calls still awaiting a result. Always 0 for [`GroupKind::Legacy`], where
    /// resolution is unknowable rather than merely absent.
    pub fn pending_count(&self) -> usize {
        if self.kind == GroupKind::Legacy {
            return 0;
        }
        self.pairs
            .iter()
            .filter(|p| p.call.is_some() && p.result.is_none())
            .count()
    }
}

/// The projected view of an event stream, plus the continuation state needed to
/// extend it in place when polling appends new events.
#[derive(Debug, Clone, Default)]
pub struct Projection {
    groups: Vec<ToolGroup>,
    /// Per event index: which group it belongs to, if any.
    roles: Vec<Option<usize>>,
    /// Unresolved `tool_use_id` -> (group index, pair index).
    pending: HashMap<String, (usize, usize)>,
    /// Group currently accepting more parallel calls, if any.
    open_paired: Option<usize>,
    /// Legacy adjacency run currently being extended, if any.
    open_legacy: Option<usize>,
}

impl Projection {
    /// Build from scratch.
    pub fn build(events: &[ConversationEvent]) -> Self {
        let mut p = Self::default();
        p.extend(events);
        p
    }

    /// Number of events already projected.
    pub fn events_len(&self) -> usize {
        self.roles.len()
    }

    pub fn groups(&self) -> &[ToolGroup] {
        &self.groups
    }

    /// The group containing `event_idx`, if the event is part of one.
    pub fn group_of(&self, event_idx: usize) -> Option<&ToolGroup> {
        let g = (*self.roles.get(event_idx)?)?;
        self.groups.get(g)
    }

    /// Whether `event_idx` is the summary-carrying leader of its group.
    pub fn is_leader(&self, event_idx: usize) -> bool {
        self.group_of(event_idx)
            .is_some_and(|g| g.leader_idx() == event_idx)
    }

    /// Project `events[self.events_len()..]`, mutating earlier groups only to
    /// attach newly arrived results to calls they already own.
    ///
    /// Callers must guarantee that `events[..self.events_len()]` is structurally
    /// unchanged (see `height::refresh_tool_projection`, which fingerprints the
    /// tail and rebuilds otherwise).
    pub fn extend(&mut self, events: &[ConversationEvent]) {
        if events.len() < self.roles.len() {
            // Truncation is not an append; caller should have rebuilt. Be safe.
            *self = Self::default();
        }

        for (idx, event) in events.iter().enumerate().skip(self.roles.len()) {
            let role = self.project_one(idx, event);
            debug_assert_eq!(self.roles.len(), idx, "roles must stay index-parallel");
            self.roles.push(role);
        }
    }

    fn project_one(&mut self, idx: usize, event: &ConversationEvent) -> Option<usize> {
        if is_transparent(event) {
            // Renders nothing, so it separates nothing: pass it over without
            // closing an open batch. Historical Claude transcripts carry one or
            // more of these between every call and its result; treating them as
            // separators is what collapsed those turns to "1 tool call" each.
            return None;
        }

        if !is_groupable(event) {
            // A passthrough row (ordinary message, or a compact input-less
            // ToolUse that renders as its own one-line card) visually separates
            // tool activity, so it ends any open batch.
            self.open_paired = None;
            self.open_legacy = None;
            return None;
        }

        match (event.event_type, event.tool_use_id.as_deref()) {
            (EventType::ToolUse, Some(id)) => {
                self.open_legacy = None;
                let g = match self.open_paired {
                    Some(g) => g,
                    None => self.push_group(GroupKind::Paired),
                };
                self.open_paired = Some(g);
                let pair_idx = self.groups[g].pairs.len();
                self.groups[g].pairs.push(ToolPair {
                    call: Some(idx),
                    result: None,
                });
                self.groups[g].members.push(idx);
                self.pending.insert(id.to_string(), (g, pair_idx));
                Some(g)
            }
            (EventType::ToolResult, Some(id)) => {
                if let Some((g, pair_idx)) = self.pending.remove(id) {
                    // Absorbed by its own call, wherever that call lives. This
                    // does not disturb an open batch: interleaved
                    // call/result/call sequences stay one group.
                    self.groups[g].pairs[pair_idx].result = Some(idx);
                    self.groups[g].members.push(idx);
                    Some(g)
                } else {
                    // Orphan result: rendered on its own, never dropped.
                    self.open_paired = None;
                    self.open_legacy = None;
                    let g = self.push_group(GroupKind::Paired);
                    self.groups[g].pairs.push(ToolPair {
                        call: None,
                        result: Some(idx),
                    });
                    self.groups[g].members.push(idx);
                    Some(g)
                }
            }
            // Pre-migration rows: unpairable, so reproduce adjacency runs.
            _ => {
                self.open_paired = None;
                let g = match self.open_legacy {
                    Some(g) => g,
                    None => self.push_group(GroupKind::Legacy),
                };
                self.open_legacy = Some(g);
                self.groups[g]
                    .pairs
                    .push(if event.event_type == EventType::ToolUse {
                        ToolPair {
                            call: Some(idx),
                            result: None,
                        }
                    } else {
                        ToolPair {
                            call: None,
                            result: Some(idx),
                        }
                    });
                self.groups[g].members.push(idx);
                Some(g)
            }
        }
    }

    fn push_group(&mut self, kind: GroupKind) -> usize {
        self.groups.push(ToolGroup {
            kind,
            pairs: Vec::new(),
            members: Vec::new(),
        });
        self.groups.len() - 1
    }
}

/// Whether an event is invisible filler that must not split a tool batch.
///
/// A `System` row with no content, no tool name and no tool input paints
/// nothing under any view toggle — there is no text, label or payload to show.
/// The Claude CLI interleaves these (init/status/hook notices) between a tool
/// call and its result and between consecutive calls. Ingest no longer persists
/// them (`session::monitor::convert_recognized_stream_event`), but every
/// transcript recorded before that fix still carries them, so the projection
/// steps over them rather than ending a group on a row nobody can see.
pub fn is_transparent(event: &ConversationEvent) -> bool {
    event.event_type == EventType::System
        && event.content.is_empty()
        && event.tool_name.is_none()
        && event.tool_input.is_none()
}

/// Whether an event participates in tool grouping at all.
///
/// Compact input-less `ToolUse` rows are excluded: they render as their own
/// one-line card (`ui::session::render_compact_tool_event`) and were never
/// swept into the old adjacency runs either. Their ids are therefore not
/// registered, so a result naming such a call surfaces as an orphan result —
/// which is exactly how it rendered before this change.
pub fn is_groupable(event: &ConversationEvent) -> bool {
    matches!(event.event_type, EventType::ToolUse | EventType::ToolResult)
        && !crate::ui::session::is_compact_tool_event(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::Role;

    fn ev(
        sequence: i32,
        event_type: EventType,
        tool_use_id: Option<&str>,
        with_input: bool,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::nil(),
            sequence,
            event_type,
            role: Some(Role::Assistant),
            content: String::new(),
            tool_name: Some("Read".to_string()),
            tool_input: with_input.then(|| Box::new(serde_json::json!({"path": "/tmp"}))),
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: tool_use_id.map(|s| s.to_string()),
            metadata: None,
        }
    }

    fn call(sequence: i32, id: &str) -> ConversationEvent {
        ev(sequence, EventType::ToolUse, Some(id), true)
    }

    fn result(sequence: i32, id: &str) -> ConversationEvent {
        ev(sequence, EventType::ToolResult, Some(id), false)
    }

    fn message(sequence: i32) -> ConversationEvent {
        let mut e = ev(sequence, EventType::Message, None, false);
        e.tool_name = None;
        e
    }

    /// The content-less `System` row the Claude CLI interleaves around every
    /// tool call. Nothing to render: no text, no tool name, no input.
    fn filler(sequence: i32) -> ConversationEvent {
        let mut e = ev(sequence, EventType::System, None, false);
        e.role = None;
        e.tool_name = None;
        e
    }

    /// A visible System row — an ingest diagnostic — still separates.
    fn system_with_content(sequence: i32) -> ConversationEvent {
        let mut e = filler(sequence);
        e.content = "[ingest] unhandled block".to_string();
        e
    }

    #[test]
    fn claude_filler_rows_do_not_split_a_batch() {
        // Verbatim shape of a real Claude transcript: a content-less system row
        // between every call and its result, and between consecutive calls.
        // Before the transparency rule this projected six groups, each
        // advertising "1 tool call".
        let events = vec![
            call(1, "a"),
            filler(2),
            result(3, "a"),
            filler(4),
            filler(5),
            call(6, "b"),
            filler(7),
            result(8, "b"),
            call(9, "c"),
            filler(10),
            result(11, "c"),
        ];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 1, "filler must not open new groups");
        let g = &p.groups()[0];
        assert_eq!(g.call_count(), 3, "renders as \"3 tool calls\"");
        assert_eq!(g.pending_count(), 0);
        assert_eq!(g.leader_idx(), 0);
        // Filler joins no group, so it is never absorbed or dropped.
        for idx in [1, 3, 4, 6, 9] {
            assert!(
                p.group_of(idx).is_none(),
                "filler at {idx} must belong to no group"
            );
        }
    }

    #[test]
    fn filler_between_turns_still_leaves_message_as_the_separator() {
        let events = vec![
            call(1, "a"),
            filler(2),
            result(3, "a"),
            filler(4),
            message(5),
            filler(6),
            call(7, "b"),
            result(8, "b"),
        ];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 2, "an assistant message still splits");
        assert_eq!(p.groups()[0].call_count(), 1);
        assert_eq!(p.groups()[1].call_count(), 1);
        assert_eq!(p.groups()[1].leader_idx(), 6);
    }

    #[test]
    fn system_row_carrying_content_still_splits_the_batch() {
        let events = vec![
            call(1, "a"),
            result(2, "a"),
            system_with_content(3),
            call(4, "b"),
            result(5, "b"),
        ];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 2, "a visible row is a real separator");
    }

    #[test]
    fn normal_pair_becomes_one_group_with_one_pair() {
        let events = vec![call(1, "t1"), result(2, "t1")];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 1);
        let g = &p.groups()[0];
        assert_eq!(g.kind, GroupKind::Paired);
        assert_eq!(
            g.pairs,
            vec![ToolPair {
                call: Some(0),
                result: Some(1)
            }]
        );
        assert_eq!(g.members, vec![0, 1]);
        assert_eq!(g.call_count(), 1);
        assert_eq!(g.pending_count(), 0);
        assert!(p.is_leader(0));
        assert!(!p.is_leader(1));
    }

    #[test]
    fn parallel_calls_with_batched_results_pair_by_id_not_position() {
        // The exact shape adjacency grouping got wrong: three calls emitted in
        // one turn, results returned out of order in a later batch.
        let events = vec![
            call(1, "a"),
            call(2, "b"),
            call(3, "c"),
            result(4, "c"),
            result(5, "a"),
            result(6, "b"),
        ];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 1, "one turn's fan-out is one group");
        let g = &p.groups()[0];
        assert_eq!(g.call_count(), 3);
        assert_eq!(g.pending_count(), 0);
        assert_eq!(
            g.pairs,
            vec![
                ToolPair {
                    call: Some(0),
                    result: Some(4)
                },
                ToolPair {
                    call: Some(1),
                    result: Some(5)
                },
                ToolPair {
                    call: Some(2),
                    result: Some(3)
                },
            ]
        );
        assert_eq!(g.leader_idx(), 0);
        assert!(g.members.contains(&5));
    }

    #[test]
    fn interleaved_calls_and_results_stay_one_batch() {
        let events = vec![call(1, "a"), result(2, "a"), call(3, "b"), result(4, "b")];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 1);
        assert_eq!(p.groups()[0].call_count(), 2);
        assert_eq!(p.groups()[0].members, vec![0, 1, 2, 3]);
    }

    #[test]
    fn orphan_call_is_kept_and_reported_pending() {
        // Interrupted / still-running session: the result never arrives.
        let events = vec![call(1, "a"), call(2, "b"), result(3, "a")];
        let p = Projection::build(&events);

        let g = &p.groups()[0];
        assert_eq!(g.call_count(), 2);
        assert_eq!(g.pending_count(), 1);
        assert_eq!(g.pairs[1].call, Some(1));
        assert_eq!(g.pairs[1].result, None);
        assert!(p.group_of(1).is_some(), "orphan call must not be dropped");
    }

    #[test]
    fn orphan_result_renders_as_its_own_group() {
        let events = vec![result(1, "ghost")];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 1);
        let g = &p.groups()[0];
        assert_eq!(
            g.pairs,
            vec![ToolPair {
                call: None,
                result: Some(0)
            }]
        );
        assert_eq!(g.leader_idx(), 0, "an orphan result leads its own group");
    }

    #[test]
    fn orphan_result_does_not_steal_a_later_calls_group() {
        let events = vec![result(1, "ghost"), call(2, "a"), result(3, "a")];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 2);
        assert_eq!(p.groups()[0].pairs[0].call, None);
        assert_eq!(p.groups()[1].leader_idx(), 1);
        assert_eq!(p.groups()[1].pairs[0].result, Some(2));
    }

    #[test]
    fn null_tool_use_id_falls_back_to_adjacency_runs() {
        // Pre-migration transcript: no ids anywhere. Behavior must match the
        // historical maximal-run grouping, including its per-event count.
        let events = vec![
            ev(1, EventType::ToolUse, None, true),
            ev(2, EventType::ToolResult, None, false),
            ev(3, EventType::ToolUse, None, true),
            ev(4, EventType::ToolResult, None, false),
        ];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 1);
        let g = &p.groups()[0];
        assert_eq!(g.kind, GroupKind::Legacy);
        assert_eq!(g.members, vec![0, 1, 2, 3]);
        assert_eq!(g.call_count(), 4, "legacy counts every row, as before");
        assert_eq!(g.pending_count(), 0, "legacy resolution is never guessed");
    }

    #[test]
    fn legacy_run_is_broken_by_a_non_tool_event() {
        let events = vec![
            ev(1, EventType::ToolUse, None, true),
            message(2),
            ev(3, EventType::ToolUse, None, true),
        ];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 2);
        assert_eq!(p.group_of(1), None);
    }

    #[test]
    fn legacy_and_paired_rows_never_share_a_group() {
        let events = vec![
            ev(1, EventType::ToolUse, None, true),
            call(2, "a"),
            result(3, "a"),
        ];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 2);
        assert_eq!(p.groups()[0].kind, GroupKind::Legacy);
        assert_eq!(p.groups()[1].kind, GroupKind::Paired);
        assert_eq!(p.groups()[1].leader_idx(), 1);
    }

    #[test]
    fn message_between_turns_starts_a_new_group() {
        let events = vec![call(1, "a"), result(2, "a"), message(3), call(4, "b")];
        let p = Projection::build(&events);

        assert_eq!(p.groups().len(), 2);
        assert_eq!(p.groups()[1].leader_idx(), 3);
        assert_eq!(p.groups()[1].pending_count(), 1);
    }

    #[test]
    fn compact_input_less_tool_use_is_passthrough() {
        let events = vec![ev(1, EventType::ToolUse, Some("a"), false)];
        let p = Projection::build(&events);

        assert!(p.groups().is_empty());
        assert_eq!(p.group_of(0), None);
    }

    #[test]
    fn incremental_extend_matches_full_rebuild() {
        // Polling appends events one at a time; the incrementally-extended
        // projection must be indistinguishable from a from-scratch build,
        // including a result that lands long after its call.
        let events = vec![
            call(1, "a"),
            call(2, "b"),
            message(3),
            result(4, "a"),
            call(5, "c"),
            result(6, "c"),
            result(7, "b"),
            ev(8, EventType::ToolUse, None, true),
        ];

        let mut incremental = Projection::default();
        for n in 1..=events.len() {
            incremental.extend(&events[..n]);
        }
        let full = Projection::build(&events);

        assert_eq!(incremental.groups(), full.groups());
        assert_eq!(incremental.roles, full.roles);
        assert_eq!(incremental.events_len(), events.len());
    }

    #[test]
    fn roles_stay_index_parallel_with_events() {
        let events = vec![message(1), call(2, "a"), result(3, "a"), message(4)];
        let p = Projection::build(&events);
        assert_eq!(p.events_len(), events.len());
        assert_eq!(p.group_of(0), None);
        assert_eq!(p.group_of(3), None);
        assert!(p.group_of(1).is_some());
        assert!(p.group_of(2).is_some());
    }
}
