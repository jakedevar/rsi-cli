import { bytes, check, identity } from './codec.js';
import type { TypeMap, EventKeyV1, HistoryWindowV1, HistoryPositionV1, HistoryEventV1, ObservationV1, DecisionDisplayV1, RetainedDecisionDisplayV1, DecisionSummaryV1, QuestionV1, PreviewStateV1, ReadResponseV1, SourceCoverageV1, CoverageStateV1 } from './generated.js';

export const METHODS = ['RemoteGetInfoV1', 'RemoteListProjectsV1', 'RemoteListSessionsV1', 'RemoteGetSessionV1', 'RemoteGetHistoryPageV1', 'RemoteGetDecisionsV1'] as const;
const pendingSources = ['question_publications', 'tracked_question_slot', 'session_question_slot', 'durable_question_fallback', 'native_runtime', 'native_publications', 'native_historical_fallback', 'legacy_approvals'];
const unique = (a: readonly unknown[]): boolean => new Set(a).size === a.length;
const range = (n: number, max: number): boolean => n >= 1 && n <= max;
export const keyCompare = (a: EventKeyV1, b: EventKeyV1): number => a.sequence !== b.sequence ? Math.sign(a.sequence - b.sequence) : BigInt(a.id) < BigInt(b.id) ? -1 : a.id === b.id ? 0 : 1;
const keyEq = (a: EventKeyV1, b: EventKeyV1): boolean => keyCompare(a, b) === 0;
export function validateWindow(v: HistoryWindowV1): void {
  switch (v.kind) {
    case 'newer': if (v.through) check(keyCompare(v.anchor, v.through) < 0); break;
    case 'interval': check(keyCompare(v.lower_exclusive, v.upper_inclusive) < 0); break;
    case 'locate': check(v.event_id === v.old_key.id && v.offset <= 8192); break;
  }
}
function pendingCoverage(v: readonly SourceCoverageV1[]): boolean { return v.length <= 8 && unique(v.map(s => s.source)) && unique(v.map(s => s.observation_order)) && v.every(s => pendingSources.includes(s.source)); }
function display(v: DecisionDisplayV1 | RetainedDecisionDisplayV1): void {
  if (v.kind === 'native_approval') check(v.method.source_field === 'method' && v.description.source_field === 'description' && v.method.source_extent === 'bounded_snapshot' && v.description.source_extent === 'bounded_snapshot');
  if (v.kind === 'legacy_approval') check(v.tool_name.source_field === 'tool_name');
}
function questionProjection(questions: readonly QuestionV1[], omitted: string, state: PreviewStateV1): void {
  check(questions.length <= 8 && (state === 'unavailable' || questions.length > 0));
  if (state === 'complete') check(omitted === '0' && questions.every(q => q.omitted_options === '0'));
}
function retainedDisplay(v: RetainedDecisionDisplayV1): void {
  display(v);
  if (v.kind === 'generic_questions') questionProjection(v.questions, v.omitted_questions, v.details_state);
  check(textBytes(v) <= 8192);
}
function currentCoverage(v: ObservationV1, state: CoverageStateV1): void {
  if (state !== 'complete') check(!v.complete && v.degraded.includes(state));
}
export function locateResult(window: HistoryWindowV1, position: HistoryPositionV1, items: readonly HistoryEventV1[]): void {
  if (window.kind !== 'locate' || position.state === 'reset') return;
  if (position.state === 'relocated') check(position.event_id === window.event_id && keyEq(position.old_key, window.old_key));
  else check(items.some(e => e.id === window.event_id && e.sequence === window.old_key.sequence));
}
function decisionSources(d: DecisionSummaryV1): void {
  const p = d.id.split(':'), primary = p[0] === 'question' ? 'question_publications'
    : p[0] === 'question-fallback' ? 'durable_question_fallback'
    : p[0] === 'question-slot' ? (p[3] === 'tracked' ? 'tracked_question_slot' : 'session_question_slot')
    : p[0] === 'legacy' ? 'legacy_approvals' : null;
  check(primary === null || d.source_observations.some(s => s.source === primary));
  for (const s of d.source_observations) {
    const allowed = d.kind === 'generic_questions' ? (p[0] === 'question-slot' ? ['tracked_question_slot', 'session_question_slot'] : p[0] === 'question' ? ['question_publications', 'durable_question_fallback'] : [primary])
      : d.kind === 'native_approval' ? pendingSources.slice(4, 7) : ['legacy_approvals'];
    check(allowed.includes(s.source));
    if (s.source !== 'native_runtime') check(s.writer_live === null && s.writer_capacity === null && s.witness_present === null);
    if (p[0] === 'question-slot' && p[2] !== 'completed') check(s.spawn_generation === null || s.spawn_generation === p[2]);
  }
  // Shared generic projections require complete evidence; source equality is a
  // producer obligation, never inferred here from a displayed text prefix.
  if (['question-slot', 'question'].includes(p[0]!) && d.source_observations.length > 1) check(!d.disagreement && d.details_state === 'complete' && d.omitted_source_observations === '0' && d.omitted_questions === '0' && d.questions.every(q => q.omitted_options === '0') && d.source_observations.every(s => s.state === 'complete'));
}
function publication(d: DecisionSummaryV1): void {
  if (d.publication_state?.state !== 'known') return;
  const value = d.publication_state.value;
  if (d.kind === 'native_approval') check(['unresolved', 'published', 'enqueued', 'expired', 'superseded'].includes(value));
  else {
    const open = d.kind === 'generic_questions' ? ['unresolved', 'published'] : ['Pending'];
    const closed = d.kind === 'generic_questions' ? ['cleared'] : ['Approved', 'Denied'];
    check(open.includes(value) || closed.includes(value));
    check(d.closure_state === (open.includes(value) ? 'open' : 'closed'));
  }
}
export const unavailableLabels = {
  method: 'Native approval — method unavailable', description: 'Description unavailable',
  tool_name: 'Legacy approval — tool name unavailable', query: 'Query unavailable', model: 'Model unavailable',
};
export function textBytes(v: unknown): number {
  if (typeof v === 'string') return bytes(v);
  if (Array.isArray(v)) return v.reduce<number>((n, x: unknown) => n + textBytes(x), 0);
  if (v !== null && typeof v === 'object') return Object.values(v).reduce<number>((n, x: unknown) => n + textBytes(x), 0);
  return 0;
}
function observation(v: ObservationV1): void {
  check(v.version === '1.0' && v.coverage.length <= 14 && v.degraded.length <= 8 && unique(v.degraded)
    && unique(v.coverage.map(s => s.source)) && unique(v.coverage.map(s => s.observation_order)));
  check(!v.complete || v.coverage.every(s => s.state === 'complete'));
  check(!v.complete || (v.next_cursor !== null) === v.coverage.some(s => s.has_more));
  for (const c of v.coverage) if (c.state !== 'complete') check(v.degraded.includes(c.state));
  check(v.complete || v.degraded.length > 0);
}
function page(v: ObservationV1, n: number, max: number, kind: string): void {
  observation(v);
  check(v.projection_limits.page_items <= max && n <= v.projection_limits.page_items && (v.next_cursor === null || v.next_cursor.kind === kind));
}
function token(v: string): void { check(/^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/.test(v)); }

// Called only after the generated structural decoder has recursively constructed
// all fields. The assertions here implement cross-field semantics independently
// of Rust. These casts cannot skip the structural or primitive checks.
export function validateNamed(name: string, value: unknown): void {
  const as = <K extends keyof TypeMap>(): TypeMap[K] => value as TypeMap[K];
  switch (name) {
    case 'EventKeyV1': check(BigInt(as<'EventKeyV1'>().id) > 0n); break;
    case 'RemoteListProjectsV1': {
      const v = as<'RemoteListProjectsV1'>(); check(v.project_ids.length <= 32 && unique(v.project_ids) && range(v.limit, 50) && (!v.cursor || v.cursor.kind === 'projects')); break;
    }
    case 'RemoteListSessionsV1': { const v = as<'RemoteListSessionsV1'>(); check(range(v.limit, 100) && (!v.cursor || v.cursor.kind === 'sessions')); break; }
    case 'ProjectsDiscoveryV1': { const v=as<'ProjectsDiscoveryV1'>();check(range(v.limit,50)&&(!v.cursor||v.cursor.kind==='projects'));break; }
    case 'RemoteGetHistoryPageV1': { const v = as<'RemoteGetHistoryPageV1'>(); check(range(v.limit, 50) && (!v.cursor || v.cursor.kind === 'history')); validateWindow(v.window); break; }
    case 'RemoteGetDecisionsV1': { const v = as<'RemoteGetDecisionsV1'>(); check(range(v.limit, 32) && (!v.cursor || v.cursor.kind === 'decisions')); if(v.selected_decision_id) decisionScope(v.selected_decision_id,v.session_id); break; }
    case 'ProjectionLimitsV1': {
      const v = as<'ProjectionLimitsV1'>(); check(range(v.page_items, 100) && range(v.name_bytes, 4096) && range(v.event_text_bytes, 8192) && range(v.decision_text_bytes, 8192) && range(v.item_bytes, 65536) && range(v.envelope_bytes, 524288)); break;
    }
    case 'SourceCoverageV1': check(range(as<'SourceCoverageV1'>().observation_order, 32)); break;
    case 'ObservationV1': observation(as<'ObservationV1'>()); break;
    case 'AttentionV1': { const v = as<'AttentionV1'>(); check((!v.incomplete && v.live_signals_lower_bound === '0') || v.requires_local_action); break; }
    case 'SessionSummaryV1': { const v = as<'SessionSummaryV1'>(); check(v.parent_id !== v.id && v.continued_from !== v.id); break; }
    case 'DisplayFieldV1': {
      const v = as<'DisplayFieldV1'>();
      const max = v.source_field === 'method' || v.source_field === 'tool_name' ? 512 : v.source_field === 'description' ? 2048 : 4096;
      check(bytes(v.text) <= max);
      if (v.observed_bytes !== null && v.state !== 'unavailable') {
        check(BigInt(v.observed_bytes) >= BigInt(bytes(v.text)));
        if (v.state === 'complete') check(BigInt(v.observed_bytes) === BigInt(bytes(v.text)));
      }
      if (v.state === 'unavailable') check(v.text === unavailableLabels[v.source_field]);
      break;
    }
    case 'SequenceObservationV1': { const v = as<'SequenceObservationV1'>(); check(['active_sessions', 'completed_sessions', 'store_history'].includes(v.source) && (v.event_id === null || BigInt(v.event_id) > 0n)); break; }
    case 'SessionDetailV1': {
      const v = as<'SessionDetailV1'>(); check(v.own_title.startsWith(v.summary.own_title) && v.query.source_field === 'query' && v.model.source_field === 'model' && v.sequences.length <= 3 && unique(v.sequences.map(s => s.source)) && pendingCoverage(v.pending_coverage)); break;
    }
    case 'HistoryEventV1': {
      const v = as<'HistoryEventV1'>(); check(BigInt(v.id) > 0n);
      if (v.content_state === 'complete') check(!v.truncated && BigInt(v.content_bytes) === BigInt(bytes(v.text)));
      if (v.content_state === 'preview') check(v.truncated && BigInt(v.content_bytes) >= BigInt(bytes(v.text)));
      switch (v.pairing_state) {
        case 'exact': case 'ambiguous': check(v.tool_pair_key !== null && v.tool_pair_key === v.tool_id_display); break;
        case 'oversized': check(v.tool_pair_key === null && v.tool_id_display !== null); break;
        case 'missing': check(v.tool_pair_key === null && v.tool_id_display === null); break;
      }
      break;
    }
    case 'HistoryIntervalV1': { const v = as<'HistoryIntervalV1'>(); if (v.lower_exclusive) check(v.upper_inclusive && keyCompare(v.lower_exclusive, v.upper_inclusive) < 0); break; }
    case 'QuestionV1': check(as<'QuestionV1'>().options.length <= 8); break;
    case 'DecisionSourceObservationV1': {
      const v = as<'DecisionSourceObservationV1'>(); check(pendingSources.includes(v.source) && (v.writer_capacity === null || v.writer_capacity <= 64)); if (v.display_alternative) display(v.display_alternative); break;
    }
    case 'DecisionSummaryV1': {
      const v = as<'DecisionSummaryV1'>(), [cls, kind] = identity(v.id);
      check(cls === v.identity_class && kind === v.kind && v.display.kind === v.kind && !v.can_answer && range(v.source_observations.length, 8) && unique(v.source_observations.map(s => s.source)) && v.questions.length <= 8);
      display(v.display);
      decisionSources(v); publication(v);
      if (v.kind !== 'generic_questions') check(v.questions.length === 0 && v.omitted_questions === '0');
      if (v.kind === 'generic_questions') questionProjection(v.questions, v.omitted_questions, v.details_state);
      if (v.details_state === 'complete') check(v.omitted_questions === '0' && v.omitted_source_observations === '0' && v.questions.every(q => q.omitted_options === '0'));
      if (['open', 'ambiguous', 'unknown'].includes(v.closure_state) || v.disagreement || v.details_state !== 'complete' || v.source_observations.some(s => s.state !== 'complete')) check(v.requires_local_action);
      for (const s of v.source_observations) {
        if (s.display_alternative) check(v.disagreement && s.display_alternative.kind === v.kind);
      }
      check(textBytes(v) <= 8192); break;
    }
    case 'InfoV1': { const v = as<'InfoV1'>(); check(v.protocol === '1.0' && JSON.stringify(v.required_capabilities) === JSON.stringify(METHODS)); break; }
    case 'InfoResponseV1': { const v = as<'InfoResponseV1'>(); observation(v); check(v.daemon_epoch === v.item.daemon_boot_id && v.next_cursor === null && v.complete); break; }
    case 'ProjectsResponseV1': { const v = as<'ProjectsResponseV1'>(); page(v, v.items.length, 50, 'projects'); check(v.items.every((p, i) => i === 0 || v.items[i - 1]!.id < p.id)); break; }
    case 'SessionsResponseV1': { const v = as<'SessionsResponseV1'>(); page(v, v.items.length, 100, 'sessions'); check(v.items.every((s, i) => s.project_id === v.project.id && (i === 0 || v.items[i - 1]!.id < s.id))); break; }
    case 'SessionResponseV1': {
      const v = as<'SessionResponseV1'>(); observation(v); check(v.next_cursor === null);
      for (const c of v.item.pending_coverage) currentCoverage(v, c.state);
      if (v.item.pending_coverage.some(s => s.state !== 'complete')) check(v.item.summary.attention.incomplete && v.item.summary.attention.requires_local_action);
      break;
    }
    case 'HistoryResponseV1': {
      const v = as<'HistoryResponseV1'>(); page(v, v.items.length, 50, 'history'); validateWindow(v.window);
      check(unique(v.items.map(e => e.id)) && v.items.every((e, i) => i === 0 || keyCompare(v.items[i - 1]!, e) < 0));
      for (const e of v.items) {
        check(v.head && keyCompare(e, v.head) <= 0 && v.interval.upper_inclusive && keyCompare(e, v.interval.upper_inclusive) <= 0 && (!v.interval.lower_exclusive || keyCompare(e, v.interval.lower_exclusive) > 0));
        if (v.position.state === 'unchanged') {
          const w = v.window;
          if (w.kind === 'older') check(keyCompare(e, w.anchor) < 0);
          if (w.kind === 'newer') check(keyCompare(e, w.anchor) > 0 && (!w.through || keyCompare(e, w.through) <= 0));
          if (w.kind === 'interval') check(keyCompare(e, w.lower_exclusive) > 0 && keyCompare(e, w.upper_inclusive) <= 0);
        }
      }
      if (v.position.state === 'relocated') {
        const p = v.position;
        const event = v.items.find(e => e.id === p.event_id && e.sequence === p.new_key.sequence);
        check(p.event_id === p.old_key.id && p.event_id === p.new_key.id && !keyEq(p.old_key, p.new_key) && p.offset <= 8192 && event && p.offset <= bytes(event.text));
        const raw = new TextEncoder().encode(event.text);
        check(p.offset === raw.length || (raw[p.offset]! & 0xc0) !== 0x80);
      }
      locateResult(v.window, v.position, v.items);
      break;
    }
    case 'DecisionsResponseV1': {
      const v = as<'DecisionsResponseV1'>(); page(v, v.items.length, 32, 'decisions'); check(unique(v.items.map(d => d.id)));
      const s = v.selected;
      for (const d of [...v.items, ...(s.state === 'present' && !s.stale ? [s.decision] : [])]) for (const c of d.source_observations) currentCoverage(v, c.state);
      if (s.state === 'present' && s.stale) check(v.degraded.includes('stale'));
      if (s.state === 'unavailable') check(!v.complete && v.degraded.includes(s.reason) && ['busy','limited','unavailable','source_changed'].includes(s.reason));
      if (s.state === 'tombstone') check(identity(s.id)[0] === s.identity_class && s.message === 'Decision no longer available');
      if ((s.state === 'tombstone' || s.state === 'unavailable') && s.last_display) { retainedDisplay(s.last_display); check(s.last_display.kind === identity(s.id)[1]); }
      break;
    }
    case 'RetryGuidanceV1': { const v = as<'RetryGuidanceV1'>(); check((v.after_ms === null || v.after_ms <= 30000) && (v.action !== 'none' || v.after_ms === null)); break; }
    case 'SessionSelectionV1': {const v=as<'SessionSelectionV1'>();validateWindow(v.history_window);if(v.selected_decision_id)decisionScope(v.selected_decision_id,v.session_id);break;}
    case 'ViewBindingV1': check(BigInt(as<'ViewBindingV1'>().selection_generation) > 0n); break;
    case 'NativeBindingV1': { const v = as<'NativeBindingV1'>(); check(BigInt(v.window_generation) > 0n && BigInt(v.connection_generation) > 0n); break; }
    case 'ViewLeaseV1': { const v = as<'ViewLeaseV1'>(); check(v.duration_ms === (v.state === 'allocation' ? 10000 : 25000)); break; }
    case 'PageRevisionV1': { const v = as<'PageRevisionV1'>(); check(BigInt(v.entry_incarnation) > 0n && BigInt(v.page_revision) > BigInt(v.entry_incarnation)); break; }
    case 'BarrierV1': { const v = as<'BarrierV1'>(); check(v.pages.length <= 8 && unique(v.pages.map(p => p.slot)) && unique(v.pages.map(p => p.page_key))); break; }
    case 'ViewStateV1': {
      const v = as<'ViewStateV1'>(); check(!v.ready || (BigInt(v.binding.attachment_generation) > 0n && v.lease.state === 'attached'));
      if (v.selection.kind === 'none') check(v.barrier.pages.every(p => p.slot === 'info' || p.slot === 'projects'));
      if (v.selection.kind === 'project' && v.selection.session === null) check(v.barrier.pages.every(p => ['info', 'projects', 'sessions'].includes(p.slot)));
      break;
    }
    case 'CreateViewV1': check(as<'CreateViewV1'>().selection.kind === 'none'); break;
    case 'AllocatedViewV1': {
      const v = as<'AllocatedViewV1'>().state;
      check(v.selection.kind === 'none' && v.binding.selection_generation === '1' && v.binding.attachment_generation === '0' && !v.ready && v.lease.state === 'allocation' && v.barrier.sequence === '0' && v.barrier.pages.length === 0); break;
    }
    case 'AttachViewV1': { const v = as<'AttachViewV1'>(); check(v.expected_attachment_generation === v.binding.attachment_generation && BigInt(v.expected_attachment_generation) < 18446744073709551615n); break; }
    case 'SelectViewV1': { const v = as<'SelectViewV1'>(); check(BigInt(v.binding.attachment_generation) > 0n && v.expected_selection_generation === v.binding.selection_generation && BigInt(v.expected_selection_generation) < 18446744073709551615n); break; }
    case 'SelectionAckV1': { const v = as<'SelectionAckV1'>(); check(v.state.ready && BigInt(v.previous_selection_generation)>0n && BigInt(v.previous_selection_generation) + 1n === BigInt(v.state.binding.selection_generation)); break; }
    case 'AttachmentAckV1': { const v = as<'AttachmentAckV1'>(); check(!v.state.ready && BigInt(v.previous_attachment_generation) + 1n === BigInt(v.state.binding.attachment_generation)); break; }
    case 'ReadyV1': { const v = as<'ReadyV1'>(); check(v.state.ready && v.stream_id.gateway_epoch === v.state.binding.gateway_epoch && v.stream_id.view_epoch === v.state.binding.view_epoch && v.stream_id.sequence === v.state.barrier.sequence); break; }
    case 'AckViewV1': case 'BoundReadV1': case 'PageNoticeV1': case 'ResetNoticeV1': case 'StreamNoticeV1': check(BigInt(as<'AckViewV1'>().binding.attachment_generation) > 0n); break;
    case 'BoundResponseV1': { const v = as<'BoundResponseV1'>(); check(BigInt(v.binding.attachment_generation) > 0n && BigInt(v.entry_incarnation) > 0n && BigInt(v.page_revision) > BigInt(v.entry_incarnation)); break; }
    case 'BootstrapV1': token(as<'BootstrapV1'>().nonce); break;
    case 'CreateAppSessionV1': token(as<'CreateAppSessionV1'>().nonce); break;
    case 'AppSessionV1': token(as<'AppSessionV1'>().csrf_token); break;
  }
}

export function validateResponse(r: ReadResponseV1, size: number): void {
  const v = r.result;
  check(size <= v.projection_limits.envelope_bytes);
  if(['RemoteListProjectsV1','RemoteListSessionsV1','RemoteGetHistoryPageV1'].includes(r.method))check(v.complete);
  const items = 'items' in v ? v.items : [v.item];
  for (const item of items) check(bytes(JSON.stringify(item)) <= v.projection_limits.item_bytes);
  if ('project' in v) check(bytes(JSON.stringify(v.project)) <= v.projection_limits.item_bytes);
  const required = r.method === 'RemoteGetInfoV1' ? []
    : r.method === 'RemoteListProjectsV1' ? ['configured_projects', 'store_projects']
    : r.method === 'RemoteGetHistoryPageV1' ? ['store_history']
    : r.method === 'RemoteGetDecisionsV1' ? pendingSources : ['active_sessions', 'completed_sessions', 'store_sessions'];
  const configuredEmpty = r.method === 'RemoteListProjectsV1' && r.result.items.length === 0 && v.coverage.length === 1 && v.coverage[0]!.source === 'configured_projects' && v.coverage[0]!.state === 'complete' && v.coverage[0]!.lower_bound === '0' && !v.coverage[0]!.has_more && v.next_cursor === null;
  check(configuredEmpty || required.every(s => v.coverage.some(c => c.source === s)));
  switch (r.method) {
    case 'RemoteListProjectsV1': check(r.result.items.every(p => bytes(p.name) <= v.projection_limits.name_bytes)); break;
    case 'RemoteListSessionsV1': check(bytes(r.result.project.name) <= v.projection_limits.name_bytes && r.result.items.every(s => bytes(s.own_title) <= v.projection_limits.name_bytes)); break;
    case 'RemoteGetSessionV1': check(bytes(r.result.item.own_title) <= v.projection_limits.name_bytes && r.result.item.pending_coverage.length === 8); break;
    case 'RemoteGetHistoryPageV1': check(r.result.items.every(e => bytes(e.text) <= v.projection_limits.event_text_bytes)); break;
    case 'RemoteGetDecisionsV1': {
      const s = r.result.selected;
      for (const d of [...r.result.items, ...(s.state === 'present' ? [s.decision] : [])]) {
        check(textBytes(d) <= v.projection_limits.decision_text_bytes && bytes(JSON.stringify(d)) <= v.projection_limits.item_bytes);
        decisionScope(d.id, r.result.session_id);
      }
      if (s.state !== 'none') {
        decisionScope(s.state === 'present' ? s.decision.id : s.id, r.result.session_id);
        check(textBytes(s) <= v.projection_limits.decision_text_bytes && bytes(JSON.stringify(s)) <= v.projection_limits.item_bytes);
      }
      check(bytes(JSON.stringify(s)) <= 65536); break;
    }
  }
}

function decisionScope(id: string, session: string): void {
  const p = id.split(':'); check(!['question-slot', 'question-fallback'].includes(p[0]!) || p[1] === session);
}
