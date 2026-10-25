// The Issues workspace's Dispatched and Sync tabs, associated sessions and the
// dependency candidate search (hub #3000 gap sweep G2), pure with no DOM.
// Mirrors crates/rsi/src/app/issues.rs (switch_issue_tab, associated_session_ids,
// the manual poll) and rsi_common::issue_workspace's tracker records.
// Unit-tested under tests/issues-tracker.test.mjs.
import { type Filters, defaultFilters, listRequest, parseIssueNumber } from './issues-model.js';

export type IssuesTab = 'local' | 'dispatched' | 'sync';
export const ISSUE_TABS: readonly IssuesTab[] = ['local', 'dispatched', 'sync'];
export const TAB_TITLES: Readonly<Record<IssuesTab, string>> = { local: 'Local', dispatched: 'Dispatched', sync: 'Sync' };
/** What each tab holds, in plain words (the tab buttons' tooltips). */
export const TAB_TIPS: Readonly<Record<IssuesTab, string>> = {
  local: 'Local: Issues in this project',
  dispatched: 'Dispatched: Issues handed to agents to work on',
  sync: 'Sync: links to an external tracker',
};

/** `[` / `]`: the previous / next tab, wrapping (switch_issue_tab). */
export function stepTab(tab: IssuesTab, dir: 1 | -1): IssuesTab {
  const i = ISSUE_TABS.indexOf(tab);
  return ISSUE_TABS[(i + dir + ISSUE_TABS.length) % ISSUE_TABS.length]!;
}

/** The starter `T` runs on the selected Issue (overlay/graph.rs DEFAULT_ISSUE_TOPOLOGY). */
export const DEFAULT_ISSUE_TOPOLOGY = 'issue-implement-review-land';

// ---------- Dispatched ----------

/** rsi_common::issue_workspace::IssueDispatchRecordV1. */
export interface DispatchRecord {
  issue_id: string;
  issue_identifier: string;
  tracker: string;
  session_id: string;
  dispatched_at: string;
  last_reconciled_at?: string | null;
  terminal_state?: string | null;
}

export function parseDispatched(v: unknown): DispatchRecord[] {
  if (!Array.isArray(v)) return [];
  return v.filter((r): r is DispatchRecord => !!r && typeof r === 'object'
    && typeof (r as DispatchRecord).session_id === 'string' && typeof (r as DispatchRecord).issue_id === 'string');
}

/** Selection is by session UUID plus tracker Issue id (the TUI's dispatched key). */
export function dispatchKey(r: DispatchRecord): string {
  return `${r.session_id}|${r.issue_id}`;
}

/** Newest dispatch first. */
export function sortDispatched(rows: readonly DispatchRecord[]): DispatchRecord[] {
  return [...rows].sort((a, b) => b.dispatched_at.localeCompare(a.dispatched_at) || dispatchKey(a).localeCompare(dispatchKey(b)));
}

export function dispatchLine(r: DispatchRecord): string {
  return `${r.issue_identifier} · ${r.tracker} · ${r.terminal_state ?? 'active'} · session ${r.session_id.slice(0, 8)}`;
}

/** Every field of one dispatch record, for its inspector. */
export function dispatchFields(r: DispatchRecord): Array<[string, string]> {
  return [
    ['Issue', r.issue_identifier],
    ['Tracker', r.tracker],
    ['Tracker id', r.issue_id],
    ['Session', r.session_id],
    ['Dispatched', r.dispatched_at],
    ['Reconciled', r.last_reconciled_at ?? '—'],
    ['Terminal state', r.terminal_state ?? '— (active)'],
  ];
}

/** j / k over the dispatched rows: the next key, clamped. */
export function moveDispatch(rows: readonly DispatchRecord[], cur: string | null, delta: number): string | null {
  if (rows.length === 0) return null;
  const keys = rows.map(dispatchKey);
  const i = cur === null ? -1 : keys.indexOf(cur);
  const from = i >= 0 ? i : delta > 0 ? -1 : keys.length;
  return keys[Math.max(0, Math.min(keys.length - 1, from + delta))] ?? null;
}

// ---------- Sync ----------

/** rsi_common::issue_workspace::IssueTrackerStatusV1. */
export interface TrackerStatus {
  enabled: boolean;
  tracker: string;
  last_poll_at?: string | null;
  next_poll_at?: string | null;
  dispatched_count: number;
  max_concurrent: number;
  poll_interval_ms: number;
  active_states: string[];
}

/** rsi_common::issue_workspace::IssueTrackerTickResultV1. */
export interface PollResult {
  issues_found: number;
  dispatched: number;
  skipped_claimed: number;
  skipped_blocked: number;
  errors: string[];
}

function seconds(ms: number): string {
  if (ms % 60_000 === 0) return `${ms / 60_000} min`;
  return `${Math.round(ms / 1000)} s`;
}

export function syncRows(s: TrackerStatus): Array<[string, string]> {
  return [
    ['Tracker', s.tracker || '—'],
    ['Polling', s.enabled ? 'enabled' : 'disabled'],
    ['Interval', seconds(s.poll_interval_ms)],
    ['Last poll', s.last_poll_at ?? 'never'],
    ['Next poll', s.next_poll_at ?? '—'],
    ['Dispatched', `${s.dispatched_count} of ${s.max_concurrent} concurrent`],
    ['Active states', s.active_states.length ? s.active_states.join(', ') : '—'],
  ];
}

/** The manual poll's result line; every error is listed after it (the TUI shows them all). */
export function pollSummary(r: PollResult): string {
  const errs = r.errors.length;
  return `Poll: ${r.issues_found} found, ${r.dispatched} dispatched, ${r.skipped_claimed} already claimed, ${r.skipped_blocked} blocked`
    + (errs ? `, ${errs} error${errs === 1 ? '' : 's'}` : '');
}

// ---------- associated sessions ----------

export interface TrackedSession {
  id: string;
  issue_tracker_id?: string | null;
}

/** The sessions launched for this Issue (issue_tracker_id = its UUID), by UUID (associated_session_ids). */
export function associatedSessions<S extends TrackedSession>(sessions: readonly S[], issueId: string): S[] {
  return sessions.filter((s) => s.issue_tracker_id === issueId).sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/** Keep the selected associated session while it is still associated, else the first (reconcile_associated_session_selection). */
export function reconcileAssociated(ids: readonly string[], cur: string | null): string | null {
  return cur !== null && ids.includes(cur) ? cur : (ids[0] ?? null);
}

// ---------- dependency candidates ----------

/** Candidates one search shows (a bounded page). */
export const CANDIDATE_LIMIT = 32;

/** What the dependency form's "other Issue" field asks for: a number, or a text search over every Issue. */
export function candidateQuery(text: string): { number: number } | { search: string } | { error: string } {
  const t = text.trim();
  if (t === '') return { error: 'Enter the other Issue’s number or words from its title' };
  const n = parseIssueNumber(t);
  return n !== null ? { number: n } : { search: t };
}

/** One bounded page of candidates for a text search, terminal and archived Issues included. */
export function candidateRequest(projectId: string, search: string): Record<string, unknown> {
  const f: Filters = { ...defaultFilters(), query: search, statuses: ['Open', 'InProgress', 'Closed', 'Cancelled'], archive: 'All' };
  return listRequest(projectId, f, { kind: 'start' }, CANDIDATE_LIMIT);
}
