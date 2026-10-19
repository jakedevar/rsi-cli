// Tool projection (gap sweep G4, hub #3000), pure (no DOM): which ToolResult
// answers which ToolUse, as crates/rsi/src/ui/tool_projection.rs pairs them.
// Pair strictly by `tool_use_id`, however far apart the rows are; consecutive
// calls batch (a fan-out turn); a call with no result yet is pending; a result
// with no call is an orphan; content-less System rows are transparent; rows
// without an id are never paired (no guessing). The transcript keeps one node
// per event (selection, folds and copy act on events); conversation.ts writes
// these facts into the call's and the result's summaries.
import type { ConversationEvent } from './api.js';

export interface CallFacts {
  /** Sequence of the matching ToolResult, or null while it is pending. */
  result: number | null;
  /** The batch leader's sequence (the first call of a run of parallel calls). */
  leader: number;
  /** A pre-migration row without a tool_use_id: never paired. */
  legacy: boolean;
}

export interface ResultFacts {
  /** Sequence of the ToolUse it answers, or null (an orphan result, or no id). */
  call: number | null;
  /** The call's tool name. */
  tool: string | null;
  legacy: boolean;
}

export interface BatchFacts {
  /** Calls in the batch. */
  calls: number;
  /** Of them, still without a result. */
  pending: number;
}

export interface Projection {
  calls: Map<number, CallFacts>;
  results: Map<number, ResultFacts>;
  /** By leader sequence; only batches of two or more calls. */
  batches: Map<number, BatchFacts>;
}

/** A row that paints nothing: a System row with no content (`is_transparent`). */
export function isTransparent(e: ConversationEvent): boolean {
  return e.event_type !== 'ToolUse' && e.event_type !== 'ToolResult' && e.event_type !== 'Message' && e.event_type !== 'Thinking'
    && e.content.trim() === '';
}

export function project(events: readonly ConversationEvent[]): Projection {
  const calls = new Map<number, CallFacts>();
  const results = new Map<number, ResultFacts>();
  const byId = new Map<string, ConversationEvent>();
  const members = new Map<number, number[]>();
  let leader: number | null = null;
  for (const e of events) {
    if (isTransparent(e)) continue;
    if (e.event_type === 'ToolUse') {
      if (leader === null) leader = e.sequence;
      calls.set(e.sequence, { result: null, leader, legacy: !e.tool_use_id });
      const list = members.get(leader) ?? [];
      list.push(e.sequence);
      members.set(leader, list);
      if (e.tool_use_id) byId.set(e.tool_use_id, e);
      continue;
    }
    if (e.event_type === 'ToolResult') {
      // Absorbing a result does not close the batch.
      const call = e.tool_use_id ? byId.get(e.tool_use_id) : undefined;
      if (call) {
        const facts = calls.get(call.sequence)!;
        if (facts.result === null) facts.result = e.sequence;
        results.set(e.sequence, { call: call.sequence, tool: call.tool_name ?? null, legacy: false });
      } else {
        results.set(e.sequence, { call: null, tool: e.tool_name ?? null, legacy: !e.tool_use_id });
      }
      continue;
    }
    leader = null;
  }
  const batches = new Map<number, BatchFacts>();
  for (const [lead, seqs] of members) {
    if (seqs.length < 2) continue;
    batches.set(lead, { calls: seqs.length, pending: seqs.filter((s) => { const c = calls.get(s)!; return !c.legacy && c.result === null; }).length });
  }
  return { calls, results, batches };
}

/** The note a call's summary carries: its state, and on a batch leader the batch size. */
export function callNote(p: Projection, seq: number): string {
  const c = p.calls.get(seq);
  if (!c) return '';
  const own = c.legacy ? '' : c.result === null ? '⧗ running' : '✓ answered';
  const b = c.leader === seq ? p.batches.get(seq) : undefined;
  if (!b) return own;
  return [own, `${b.calls} parallel calls${b.pending ? `, ${b.pending} running` : ''}`].filter(Boolean).join(' · ');
}

/** The note a result's summary carries: the call it answers. */
export function resultNote(p: Projection, seq: number): string {
  const r = p.results.get(seq);
  if (!r || r.legacy) return '';
  return r.call === null ? 'no matching call' : `↳ ${r.tool ?? 'tool'} #${r.call}`;
}
