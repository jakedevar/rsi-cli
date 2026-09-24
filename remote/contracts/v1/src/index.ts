import { WireDocumentV1 } from './generated.js';
import type { ReadRequestV1, ReadResponseV1, ViewStateV1, BoundReadV1, BoundResponseV1 } from './generated.js';
import { bytes, check, InvalidWire } from './codec.js';
import { validateResponse, locateResult } from './semantic.js';
export * from './generated.js';
export { InvalidWire } from './codec.js';

// A small JSON lexical reader catches duplicate keys and number spellings BEFORE
// JSON.parse can erase them. Its input, depth and node count are bounded.
function strictJson(raw: string): unknown {
  let i = 0, nodes = 0;
  const ws = (): void => { while (/^[\t\n\r ]$/.test(raw[i] ?? '')) i++; };
  const string = (): string => {
    const start = i; check(raw[i++] === '"');
    while (i < raw.length) {
      const c = raw[i++];
      if (c === '"') { const s: unknown = JSON.parse(raw.slice(start, i)); check(typeof s === 'string' && s.isWellFormed()); return s; }
      if (c === '\\') i++;
    }
    throw new InvalidWire();
  };
  const value = (depth: number): void => {
    check(depth <= 32 && ++nodes <= 32768); ws();
    if (raw[i] === '"') { string(); return; }
    if (raw[i] === '{' || raw[i] === '[') {
      const object = raw[i++] === '{', end = object ? '}' : ']', keys = new Set<string>(); ws();
      if (raw[i] === end) { i++; return; }
      for (;;) {
        if (object) { ws(); const key = string(); check(!keys.has(key)); keys.add(key); ws(); check(raw[i++] === ':'); }
        value(depth + 1); ws(); if (raw[i] === end) { i++; return; } check(raw[i++] === ',');
      }
    }
    const m = /^(?:true|false|null|0|-?[1-9][0-9]*)/.exec(raw.slice(i)); check(m); i += m[0].length;
  };
  value(0); ws(); check(i === raw.length);
  return JSON.parse(raw) as unknown;
}
const requestTypes = new Set(['request', 'view_request', 'create_view', 'attach_view', 'select_view', 'ack_view', 'close_view', 'bound_read', 'create_app_session']);
export function decode(raw: string | Uint8Array): WireDocumentV1 {
  try {
    const s = typeof raw === 'string' ? raw : new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(raw);
    check(s.isWellFormed() && bytes(s) <= 524288);
    const document = WireDocumentV1(strictJson(s));
    const size = bytes(JSON.stringify(document));
    check(size <= (requestTypes.has(document.type) ? 16384 : 524288));
    if (requestTypes.has(document.type)) check(bytes(s) <= 16384);
    if (document.type === 'response') validateResponse(document.value, size);
    if (document.type === 'bound_response') validateResponse(document.value.read, size);
    return document;
  } catch { throw new InvalidWire(); }
}
export function encode(document: WireDocumentV1): string { return JSON.stringify(decode(JSON.stringify(WireDocumentV1(document)))); }

export function canonical(value: unknown): string {
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (value !== null && typeof value === 'object') return '{' + Object.entries(value).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([k, v]) => JSON.stringify(k) + ':' + canonical(v)).join(',') + '}';
  return JSON.stringify(value);
}
const equal = (a: unknown, b: unknown): boolean => canonical(a) === canonical(b);
/** Structural correlation only; no authorization, source lookup or CAS effect. */
export function validateExchange(q: ReadRequestV1, r: ReadResponseV1): void {
  check(q.method === r.method);
  switch (q.method) {
    case 'RemoteGetInfoV1': break;
    case 'RemoteListProjectsV1': check(r.method === q.method && r.result.items.length <= q.params.limit && r.result.items.every(p => q.params.project_ids.includes(p.id))); break;
    case 'RemoteListSessionsV1': check(r.method === q.method && r.result.project.id === q.params.project_id && r.result.items.length <= q.params.limit); break;
    case 'RemoteGetSessionV1': check(r.method === q.method && r.result.item.summary.id === q.params.session_id && r.result.item.summary.project_id === q.params.project_id); break;
    case 'RemoteGetHistoryPageV1':
      check(r.method === q.method && r.result.project_id === q.params.project_id && r.result.session_id === q.params.session_id && r.result.items.length <= q.params.limit && equal(r.result.window, q.params.window));
      locateResult(q.params.window, r.result.position, r.result.items); break;
    case 'RemoteGetDecisionsV1': {
      check(r.method === q.method); const s = r.result.selected, id = s.state === 'none' ? null : s.state === 'present' ? s.decision.id : s.id;
      check(r.result.project_id === q.params.project_id && r.result.session_id === q.params.session_id && r.result.mode === q.params.mode && r.result.items.length <= q.params.limit && id === q.params.selected_decision_id); break;
    }
  }
}
export function validateBoundRead(view: ViewStateV1, q: BoundReadV1): void {
  check(view.ready && equal(view.binding, q.binding));
  if (q.read.method === 'RemoteGetInfoV1' || q.read.method === 'RemoteListProjectsV1') return;
  check(view.selection.kind === 'project' && view.selection.project_id === q.read.params.project_id);
  if (q.read.method === 'RemoteListSessionsV1') return;
  check(view.selection.session && view.selection.session.session_id === q.read.params.session_id);
  if (q.read.method === 'RemoteGetDecisionsV1') check(view.selection.session.selected_decision_id === q.read.params.selected_decision_id);
}
export function validateBoundResponse(q: BoundReadV1, r: BoundResponseV1): void {
  check(equal(q.binding, r.binding) && q.request_id === r.request_id && q.page_key === r.page_key && equal(q.native, r.native));
  if(q.read.method==='RemoteListProjectsV1')check(r.read.method===q.read.method&&r.read.result.items.length<=q.read.params.limit);
  else validateExchange(q.read, r.read);
}
/** Bounded plain-text fixture hints, not a UI renderer or authorization signal. */
export function displayHints(d: WireDocumentV1): string[] {
  const hints = new Set<string>();
  function walk(v: unknown): void {
    if (typeof v === 'string') hints.add(v);
    else if (Array.isArray(v)) v.forEach(walk);
    else if (v !== null && typeof v === 'object') {
      const o = v as Record<string, unknown>;
      if (o.state === 'unknown' && typeof o.label === 'string') hints.add(`Unknown: ${o.label}`);
      if (o.state === 'truncated' || o.details_state === 'truncated') hints.add('Truncated preview');
      if (o.source_extent === 'bounded_snapshot') hints.add('Source-provided preview');
      if (o.identity_class === 'slot') hints.add('Occurrence unknown');
      if (o.kind === 'generic_questions' && Object.hasOwn(o, 'details_state')) { hints.add('Answer not observed'); if (o.details_state === 'unavailable') hints.add('Question details unavailable'); }
      const labels: Record<string, string> = { native_runtime: 'Native runtime', native_publications: 'Native publication', native_historical_fallback: 'Native historical fallback', legacy_approvals: 'Legacy approval' };
      if (typeof o.source === 'string' && labels[o.source]) hints.add(labels[o.source]!);
      Object.values(o).forEach(walk);
    }
  }
  walk(d);
  // UTF-8 lexicographic order matches Rust for the valid Unicode strings here;
  // use code points so supplementary characters do not sort as UTF-16 units.
  return [...hints].sort((a, b) => { const x = [...a], y = [...b]; for (let i = 0; i < Math.min(x.length, y.length); i++) { const delta = x[i]!.codePointAt(0)! - y[i]!.codePointAt(0)!; if (delta) return delta; } return x.length - y.length; });
}
