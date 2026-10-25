// Pure model of the Issues workspace (hub #3010): filters and saved views, the
// bounded page requests, the lifecycle rules, drafts and their patches, the
// retry identity of a write, and the workspace key sequences. No DOM. The
// rules mirror the TUI's Issues pane (crates/rsi/src/ui/issues.rs,
// app/issues.rs) and the daemon's typed contracts (rsi-common issue_workspace).

import type { Platform } from './keyengine.js';
import { hintLabel } from './keylabel.js';

export type IssueStatus = 'Open' | 'InProgress' | 'Closed' | 'Cancelled';
export type Readiness = 'ready' | 'blocked' | 'not_eligible';
export type ReadinessFilter = 'any' | 'ready' | 'blocked';
export type ArchiveFilter = 'Active' | 'Archived' | 'All';
export type SortKey = 'updated_desc' | 'display_number_asc' | 'priority_asc';
export type Provenance = 'operator' | 'session' | 'idea' | 'finding';
export type SavedView = 'ready' | 'blocked' | 'mine' | 'recent';

export const STATUSES: readonly IssueStatus[] = ['Open', 'InProgress', 'Closed', 'Cancelled'];
export const PROVENANCES: readonly Provenance[] = ['operator', 'session', 'idea', 'finding'];
export const OPERATOR_LABELS = ['operator-request', 'operator-decision'] as const;
export const PAGE_LIMIT = 64;

export interface Issue {
  id: string;
  project_id: string;
  display_number: number;
  title: string;
  body: string;
  status: IssueStatus;
  priority?: number | null;
  labels: string[];
  assignee?: string | null;
  created_by_session_id?: string | null;
  idea_id?: string | null;
  created_at: string;
  updated_at: string;
  closed_at?: string | null;
  row_version: number;
  archived_at?: string | null;
}

export interface IssueRow {
  issue: Issue;
  readiness: Readiness;
  open_blocker_count: number;
  dependent_count: number;
}

/** Opaque to the UI: sent back unchanged. */
export type PageCursor = Record<string, unknown>;

export interface IssuePage {
  rows: IssueRow[];
  previous_cursor?: PageCursor | null;
  next_cursor?: PageCursor | null;
}

export interface DependencyItem { dependency: { depends_on_id: string; issue_id: string }; related_issue: Issue }
export interface DependencyPage { items: DependencyItem[]; previous_cursor?: unknown; next_cursor?: PageCursor | null }

export interface IssueEvent {
  sequence: number;
  operation: string;
  actor_kind?: string;
  actor_label?: string | null;
  occurred_at: string;
  resulting_row_version: number;
  issue: Issue;
}
export interface EventPage { events: IssueEvent[]; next_after_sequence?: number | null }
export interface Handoff { sequence: number; author_session_id: string; body: string; created_at: string }
export interface HandoffPage { handoffs: Handoff[]; next_after_sequence?: number | null; total: number }
export interface MutationResult { issue: Issue; event: IssueEvent; deduplicated: boolean }

// ---------- filters ----------

export interface Filters {
  statuses: IssueStatus[];
  priorities: number[];
  readiness: ReadinessFilter;
  assignee: string | null;
  unassigned: boolean;
  labels_all: string[];
  labels_any: string[];
  provenance: Provenance[];
  query: string | null;
  archive: ArchiveFilter;
  sort: SortKey;
}

export function defaultFilters(): Filters {
  return {
    statuses: [], priorities: [], readiness: 'any', assignee: null, unassigned: false,
    labels_all: [], labels_any: [], provenance: [], query: null, archive: 'Active', sort: 'updated_desc',
  };
}

/** Whether the one-key operator filter (`O`) is on: exactly the two operator labels. */
export function operatorOnly(f: Filters): boolean {
  return f.labels_any.length === 2 && OPERATOR_LABELS.every((l) => f.labels_any.includes(l));
}

export function toggleOperatorOnly(f: Filters): Filters {
  return { ...f, labels_any: operatorOnly(f) ? [] : [...OPERATOR_LABELS] };
}

/** A saved view starts from clean filters; Mine needs the operator's assignee name. */
export function applySavedView(view: SavedView, mine: string | null): Filters | { needs: 'assignee' } {
  const f = defaultFilters();
  switch (view) {
    case 'ready': return { ...f, readiness: 'ready' };
    case 'blocked': return { ...f, readiness: 'blocked' };
    case 'mine': return mine ? { ...f, assignee: mine } : { needs: 'assignee' };
    case 'recent': return { ...f, sort: 'updated_desc' };
  }
}

export const SAVED_VIEW_TITLES: Readonly<Record<SavedView, string>> = {
  ready: 'Ready', blocked: 'Blocked', mine: 'Mine', recent: 'Recent',
};

/** Active filters as short chips for the header (nothing for a default filter). */
export function filterChips(f: Filters): string[] {
  const out: string[] = [];
  if (f.query) out.push(`“${f.query}”`);
  if (f.statuses.length) out.push(`status ${f.statuses.join('|')}`);
  if (f.priorities.length) out.push(`priority ${f.priorities.map((p) => `P${p}`).join('|')}`);
  if (f.readiness !== 'any') out.push(f.readiness);
  if (f.unassigned) out.push('unassigned');
  else if (f.assignee) out.push(`@${f.assignee}`);
  if (f.labels_all.length) out.push(`labels ${f.labels_all.join(',')}`);
  if (operatorOnly(f)) out.push('operator');
  else if (f.labels_any.length) out.push(`any of ${f.labels_any.join(',')}`);
  if (f.provenance.length) out.push(`from ${f.provenance.join('|')}`);
  if (f.archive !== 'Active') out.push(f.archive === 'All' ? 'incl. archived' : 'archived only');
  if (f.sort !== 'updated_desc') out.push(f.sort === 'priority_asc' ? 'by priority' : 'by number');
  return out;
}

export const SORT_CYCLE: readonly SortKey[] = ['updated_desc', 'display_number_asc', 'priority_asc'];
export function nextSort(s: SortKey): SortKey {
  return SORT_CYCLE[(SORT_CYCLE.indexOf(s) + 1) % SORT_CYCLE.length]!;
}
export const SORT_TITLES: Readonly<Record<SortKey, string>> = {
  updated_desc: 'recently updated', display_number_asc: 'issue number', priority_asc: 'priority',
};

// ---------- parsing form text ----------

export function parseCsv(text: string): string[] {
  return text.split(',').map((s) => s.trim()).filter((s) => s !== '');
}

/** `open, inprogress` / `Closed` → statuses (case and separators are forgiven; unknown words are reported). */
export function parseStatuses(text: string): { value: IssueStatus[]; bad: string[] } {
  const value: IssueStatus[] = [];
  const bad: string[] = [];
  for (const raw of text.split(/[\s,|]+/).filter(Boolean)) {
    const key = raw.toLowerCase().replace(/[^a-z]/g, '');
    const hit = STATUSES.find((s) => s.toLowerCase() === key) ?? (key === 'progress' ? 'InProgress' : undefined);
    if (!hit) bad.push(raw);
    else if (!value.includes(hit)) value.push(hit);
  }
  return { value, bad };
}

export function parsePriorities(text: string): { value: number[]; bad: string[] } {
  const value: number[] = [];
  const bad: string[] = [];
  for (const raw of text.split(/[\s,|]+/).filter(Boolean)) {
    const n = Number(raw.replace(/^p/i, ''));
    if (Number.isInteger(n) && n >= 1 && n <= 4) { if (!value.includes(n)) value.push(n); } else bad.push(raw);
  }
  return { value, bad };
}

export function parseProvenance(text: string): { value: Provenance[]; bad: string[] } {
  const value: Provenance[] = [];
  const bad: string[] = [];
  for (const raw of text.split(/[\s,|]+/).filter(Boolean)) {
    const hit = PROVENANCES.find((p) => p === raw.toLowerCase());
    if (!hit) bad.push(raw);
    else if (!value.includes(hit)) value.push(hit);
  }
  return { value, bad };
}

/** `#12`, `12`, `issue 12` → 12. */
export function parseIssueNumber(text: string): number | null {
  const m = text.trim().match(/^(?:issue\s*)?#?(\d{1,9})$/i);
  const n = m ? Number(m[1]) : NaN;
  return Number.isInteger(n) && n >= 1 ? n : null;
}

// ---------- page requests ----------

export type PageAnchor =
  | { kind: 'start' }
  | { kind: 'end' }
  | { kind: 'next'; cursor: PageCursor }
  | { kind: 'previous'; cursor: PageCursor };

/** The `ListIssuesPage` request (start = first page forward, end = last page backward). */
export function listRequest(projectId: string, f: Filters, anchor: PageAnchor, limit = PAGE_LIMIT): Record<string, unknown> {
  const req: Record<string, unknown> = {
    project_id: projectId,
    statuses: f.statuses,
    priorities: f.priorities,
    readiness: f.readiness,
    unassigned: f.unassigned,
    labels_all: f.labels_all,
    provenance: f.provenance,
    archive: f.archive,
    sort: f.sort,
    limit,
  };
  if (f.assignee && !f.unassigned) req.assignee = f.assignee;
  if (f.labels_any.length) req.labels_any = f.labels_any;
  if (f.query && f.query.trim() !== '') req.query = f.query.trim();
  switch (anchor.kind) {
    case 'start': req.direction = 'forward'; break;
    case 'end': req.direction = 'backward'; break;
    case 'next': req.direction = 'forward'; req.cursor = anchor.cursor; break;
    case 'previous': req.direction = 'backward'; req.cursor = anchor.cursor; break;
  }
  return req;
}

/** `:issue 12` / `#12`: finds the Issue wherever it lives, archived included. */
export function numberJumpFilters(n: number): Filters {
  return { ...defaultFilters(), query: `#${n}`, archive: 'All' };
}

export type Move =
  | { kind: 'select'; id: string }
  | { kind: 'fetch'; anchor: 'next' | 'previous' }
  | { kind: 'none' };

/**
 * `j` / `k`: move by Issue id; at an edge fetch the adjacent page when the
 * server has one, else stay.
 */
export function moveSelection(page: IssuePage | null, selectedId: string | null, delta: number): Move {
  const rows = page?.rows ?? [];
  if (!page || rows.length === 0) return { kind: 'none' };
  const at = selectedId === null ? -1 : rows.findIndex((r) => r.issue.id === selectedId);
  if (at < 0) return { kind: 'select', id: (delta > 0 ? rows[0] : rows[rows.length - 1])!.issue.id };
  const to = at + delta;
  if (to >= rows.length) return page.next_cursor ? { kind: 'fetch', anchor: 'next' } : { kind: 'none' };
  if (to < 0) return page.previous_cursor ? { kind: 'fetch', anchor: 'previous' } : { kind: 'none' };
  return { kind: 'select', id: rows[to]!.issue.id };
}

/** After a page loads: keep the selection when it is still there, else pick an edge row. */
export function reconcileSelection(page: IssuePage, selectedId: string | null, prefer: 'first' | 'last'): string | null {
  if (page.rows.length === 0) return null;
  if (selectedId && page.rows.some((r) => r.issue.id === selectedId)) return selectedId;
  return (prefer === 'first' ? page.rows[0] : page.rows[page.rows.length - 1])!.issue.id;
}

// ---------- lifecycle rules ----------

export const isTerminal = (i: Issue): boolean => i.status === 'Closed' || i.status === 'Cancelled';
export const isArchived = (i: Issue): boolean => !!i.archived_at;
/** Content edits, priority, assignee, labels and cancel need an active, unarchived Issue. */
export const canEdit = (i: Issue): boolean => !isTerminal(i) && !isArchived(i);
export const canArchive = (i: Issue): boolean => isTerminal(i) && !isArchived(i);
export const canRestore = (i: Issue): boolean => isArchived(i);

/** The statuses a write may move this Issue to (the shared lifecycle matrix); none while archived. */
export function statusTargets(i: Issue): IssueStatus[] {
  if (isArchived(i)) return [];
  switch (i.status) {
    case 'Open': return ['InProgress', 'Closed', 'Cancelled'];
    case 'InProgress': return ['Open', 'Closed', 'Cancelled'];
    case 'Closed':
    case 'Cancelled': return ['Open'];
  }
}

export function statusTitle(s: IssueStatus): string {
  return s === 'InProgress' ? 'In progress' : s;
}

/** The Filters form's Status checkbox group: [serde enum string, plain label]. */
export const STATUS_CHOICES: ReadonlyArray<readonly [IssueStatus, string]> = STATUSES.map((s) => [s, statusTitle(s)] as const);

/** The checked Status boxes as the exact serde enum strings, in list order; none checked = [] (the active ones). */
export function statusesFromChecked(checked: readonly string[]): IssueStatus[] {
  return STATUSES.filter((s) => checked.includes(s));
}

/** The Issue list's empty row: a next step when the project has no Issues, the filters when some are set. */
export function emptyIssuesText(f: Filters): string {
  return filterChips(f).length === 0 ? 'No Issues in this project yet. Click New Issue to add one.' : 'No Issues match these filters.';
}

/** The retry banner: what may have failed and how to try again (the retry replays the same idempotency key). */
export function retryBannerText(label: string, platform: Platform = 'other'): string {
  return `Saving “${label}” may have failed. Try again (${hintLabel('Ctrl-Enter', platform)})`;
}

export type WriteWhat = 'edit' | 'status' | 'cancel' | 'archive' | 'restore';

/** Why a write is not offered, or null. */
export function writeBlock(i: Issue | null, what: WriteWhat): string | null {
  if (!i) return 'Select an Issue first (j/k)';
  switch (what) {
    case 'edit':
      return canEdit(i) ? null
        : isArchived(i) ? 'Archived Issues are read-only; restore it first'
          : 'Only an open or in-progress Issue can be edited; reopen it first (S)';
    case 'status': return statusTargets(i).length > 0 ? null : 'Restore the archived Issue before changing its status';
    case 'cancel': return canEdit(i) ? null : 'Only an open or in-progress Issue can be cancelled';
    case 'archive': return canArchive(i) ? null : isArchived(i) ? 'Already archived' : 'Close or cancel the Issue before archiving it';
    case 'restore': return canRestore(i) ? null : 'This Issue is not archived';
  }
}

// ---------- visible action buttons ----------

/** One labelled button in the Issue detail pane. `action` is the same action id the key, palette and menu run. */
export interface IssueButton {
  readonly action: 'issues-edit' | 'issues-status' | 'issues-archive' | 'issues-restore';
  readonly label: string;
  readonly title: string;
}

/**
 * The buttons the detail pane shows for this Issue, in plain words. Each one opens the form the key does, so the
 * same checks and confirmations apply; a button is listed only when its action can run on this Issue now.
 */
export function issueButtons(i: Issue): IssueButton[] {
  const out: IssueButton[] = [];
  if (canEdit(i)) out.push({ action: 'issues-edit', label: 'Edit…', title: 'Change the title, description, priority, labels or assignee' });
  if (statusTargets(i).length > 0) out.push({ action: 'issues-status', label: 'Change status…', title: `Now ${statusTitle(i.status)}; choose where to move it` });
  if (canArchive(i)) out.push({ action: 'issues-archive', label: 'Archive…', title: 'Hide this finished Issue from the default list; you can restore it' });
  if (canRestore(i)) out.push({ action: 'issues-restore', label: 'Restore', title: 'Bring this archived Issue back to the list' });
  return out;
}

// ---------- display ----------

export const priorityLabel = (p: number | null | undefined): string => (p ? `P${p}` : '—');
export const issueRef = (i: Issue): string => `#${i.display_number}`;

export function readinessLabel(r: IssueRow): string {
  switch (r.readiness) {
    case 'ready': return 'ready';
    case 'blocked': return r.open_blocker_count > 0 ? `blocked by ${r.open_blocker_count}` : 'blocked';
    default: return '';
  }
}

/** One list line: `#12 P2 In progress  Title`. */
export function rowSummary(r: IssueRow): string {
  const i = r.issue;
  return `${issueRef(i)} ${priorityLabel(i.priority)} ${statusTitle(i.status)}${isArchived(i) ? ' (archived)' : ''}  ${i.title}`;
}

export function rowTone(r: IssueRow): 'ready' | 'blocked' | 'done' | 'plain' {
  if (isTerminal(r.issue) || isArchived(r.issue)) return 'done';
  return r.readiness === 'ready' ? 'ready' : r.readiness === 'blocked' ? 'blocked' : 'plain';
}

/** Server-supplied ISO time as `YYYY-MM-DD HH:MM` (UTC), or the raw text if it is not a date. */
export function shortTime(iso: string | null | undefined): string {
  if (!iso) return '—';
  const m = iso.match(/^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2})/);
  return m ? `${m[1]} ${m[2]}` : iso;
}

export function describeEvent(e: IssueEvent): string {
  const who = e.actor_label || e.actor_kind || 'unknown';
  return `${e.sequence}. ${e.operation.replace(/_/g, ' ')} by ${who} → v${e.resulting_row_version} · ${shortTime(e.occurred_at)}`;
}

export function describeDependency(d: DependencyItem): string {
  const i = d.related_issue;
  return `${issueRef(i)} ${statusTitle(i.status)}${isArchived(i) ? ' (archived)' : ''}  ${i.title}`;
}

// ---------- drafts and patches ----------

export interface Draft {
  title: string;
  body: string;
  /** '' = unset. */
  priority: string;
  /** Comma-separated. */
  labels: string;
  assignee: string;
}

export const emptyDraft = (): Draft => ({ title: '', body: '', priority: '', labels: '', assignee: '' });

export function draftFromIssue(i: Issue): Draft {
  return {
    title: i.title, body: i.body, priority: i.priority ? String(i.priority) : '',
    labels: i.labels.join(', '), assignee: i.assignee ?? '',
  };
}

export interface DraftFields { title: string; body: string; priority: number | null; labels: string[]; assignee: string | null }

/** Validate a draft; returns the typed fields or the first problem. */
export function checkDraft(d: Draft): { ok: true; value: DraftFields } | { ok: false; error: string } {
  const title = d.title.trim();
  if (title === '') return { ok: false, error: 'A title is required' };
  if (title.length > 1024) return { ok: false, error: 'The title is longer than 1024 characters' };
  let priority: number | null = null;
  if (d.priority.trim() !== '') {
    const p = parsePriorities(d.priority);
    if (p.bad.length > 0 || p.value.length !== 1) return { ok: false, error: 'Priority is 1 (urgent) to 4 (low), or empty' };
    priority = p.value[0]!;
  }
  const labels = [...new Set(parseCsv(d.labels))];
  if (labels.length > 64) return { ok: false, error: 'At most 64 labels' };
  if (labels.some((l) => l.length > 128)) return { ok: false, error: 'A label is longer than 128 characters' };
  const assignee = d.assignee.trim() === '' ? null : d.assignee.trim();
  if (assignee && assignee.length > 256) return { ok: false, error: 'The assignee is longer than 256 characters' };
  return { ok: true, value: { title, body: d.body, priority, labels, assignee } };
}

const sameList = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

/** Only what changed against the Issue: absent = unchanged, `null` = clear (priority, assignee). */
export function patchFrom(issue: Issue, v: DraftFields): Record<string, unknown> {
  const patch: Record<string, unknown> = {};
  if (v.title !== issue.title) patch.title = v.title;
  if (v.body !== issue.body) patch.body = v.body;
  if (!sameList(v.labels, issue.labels)) patch.labels = v.labels;
  if (v.priority !== (issue.priority ?? null)) patch.priority = v.priority;
  if (v.assignee !== (issue.assignee ?? null)) patch.assignee = v.assignee;
  return patch;
}

export function createBody(v: DraftFields): Record<string, unknown> {
  const body: Record<string, unknown> = { title: v.title, body: v.body, labels: v.labels };
  if (v.priority !== null) body.priority = v.priority;
  if (v.assignee !== null) body.assignee = v.assignee;
  return body;
}

// ---------- writes: retry identity ----------

export type WriteKind = 'create' | 'update' | 'status' | 'archive' | 'restore';

/**
 * One operator write. The idempotency key is minted once: a retry after an
 * uncertain failure sends the identical request, so the daemon deduplicates it.
 * A new key is minted only by a fresh request (`rebase`).
 */
export interface PendingWrite {
  kind: WriteKind;
  issueId: string | null;
  expectedRowVersion: number | null;
  idempotencyKey: string;
  /** The patch / status / draft fields exactly as first sent. */
  payload: Record<string, unknown>;
  /** Display text for the confirmation line. */
  label: string;
}

export function newWrite(
  kind: WriteKind, issue: Issue | null, payload: Record<string, unknown>, label: string, mintKey: () => string,
): PendingWrite {
  return {
    kind, issueId: issue?.id ?? null, expectedRowVersion: issue ? issue.row_version : null,
    idempotencyKey: mintKey(), payload, label,
  };
}

/** The request body the Tauri command takes (`write` / `draft`). */
export function writeBody(w: PendingWrite): Record<string, unknown> {
  if (w.kind === 'create') return { ...w.payload, idempotency_key: w.idempotencyKey };
  return { ...w.payload, expected_row_version: w.expectedRowVersion, idempotency_key: w.idempotencyKey };
}

/** The same edit against a newer version of the Issue, under a new key. */
export function rebase(w: PendingWrite, latest: Issue, mintKey: () => string): PendingWrite {
  return { ...w, expectedRowVersion: latest.row_version, idempotencyKey: mintKey() };
}

export type WriteFailure =
  | { kind: 'stale'; message: string }
  /** The daemon answered with a typed error: the write definitely did not happen. */
  | { kind: 'definitive'; code: string; message: string }
  /** Transport or timeout: the write may have happened. Retry sends the identical request. */
  | { kind: 'uncertain'; message: string };

const CODE_ADVICE: Readonly<Record<string, string>> = {
  invalid_request: 'The daemon refused the request as invalid',
  not_found_in_project: 'That Issue is not in this project (anymore)',
  idempotency_conflict: 'This retry key was already used for a different request',
  no_semantic_change: 'Nothing would change',
  invalid_transition: 'That status change is not allowed from the current status',
  not_terminal: 'Only a closed or cancelled Issue can be archived',
  not_archived: 'This Issue is not archived',
  dependency_cycle: 'That dependency would make a cycle',
  dependency_endpoint: 'That dependency endpoint is not valid',
  access_denied: 'Not permitted',
};

/** Sort an error string from the backend (`daemon error -32000: msg [code]`). */
export function classifyFailure(error: string): WriteFailure {
  const m = error.match(/\[([a-z_]+)\]\s*$/);
  const code = m?.[1];
  const message = error.replace(/^daemon error -?\d+:\s*/, '').replace(/\s*\[[a-z_]+\]\s*$/, '');
  if (!code) {
    const transport = /cannot reach rsid|i\/o error|did not answer|too large|malformed/.test(error);
    return transport ? { kind: 'uncertain', message: error } : { kind: 'definitive', code: 'rejected', message };
  }
  if (code === 'stale_version') {
    return { kind: 'stale', message: 'Someone changed this Issue first. Reload the latest, or rebase your edit onto it.' };
  }
  return { kind: 'definitive', code, message: CODE_ADVICE[code] ?? message };
}

// ---------- dependencies ----------

export type DepDirection = 'blocked_by' | 'blocks';
export function depLabel(d: DepDirection): string {
  return d === 'blocked_by' ? 'Blocked by' : 'Blocks';
}

/**
 * "Blocked by X" is the edge `this depends on X`; "Blocks X" is `X depends on
 * this` (the daemon's `issue_id` depends on `depends_on_id`). Null for the same Issue.
 */
export function dependencyEdge(
  direction: DepDirection, thisId: string, otherId: string,
): { issueId: string; dependsOnId: string } | null {
  if (thisId === otherId) return null;
  return direction === 'blocked_by'
    ? { issueId: thisId, dependsOnId: otherId }
    : { issueId: otherId, dependsOnId: thisId };
}

/** The row with exactly this display number (a `#N` text search can match more). */
export function findByNumber(page: IssuePage, n: number): IssueRow | undefined {
  return page.rows.find((r) => r.issue.display_number === n);
}

// ---------- inspector sections ----------

export type Section = 'blocked_by' | 'blocks' | 'events' | 'handoffs';
export const SECTIONS: readonly Section[] = ['blocked_by', 'blocks', 'events', 'handoffs'];
export const SECTION_TITLES: Readonly<Record<Section, string>> = {
  blocked_by: 'Blocked by', blocks: 'Blocks', events: 'Events', handoffs: 'Handoffs',
};
export function nextSection(s: Section, delta = 1): Section {
  return SECTIONS[(SECTIONS.indexOf(s) + delta + SECTIONS.length) % SECTIONS.length]!;
}

// ---------- copy ----------

/** `yy` copies the UUID, `y#` copies `#N`. */
export function copyText(i: Issue, what: 'uuid' | 'ref'): { text: string; toast: string } {
  return what === 'uuid'
    ? { text: i.id, toast: `issue ${i.id} copied` }
    : { text: issueRef(i), toast: `issue ${issueRef(i)} copied` };
}

// ---------- workspace keys ----------

export type IssuesKeyAction =
  | 'issues-close' | 'issues-down' | 'issues-up' | 'issues-first' | 'issues-last' | 'issues-inspect'
  | 'issues-refresh' | 'issues-search' | 'issues-filters' | 'issues-sort' | 'issues-operator'
  | 'issues-new' | 'issues-edit' | 'issues-status' | 'issues-priority' | 'issues-assign' | 'issues-labels'
  | 'issues-deps' | 'issues-cancel' | 'issues-archive' | 'issues-restore'
  | 'issues-copy-uuid' | 'issues-copy-ref' | 'issues-section' | 'issues-more'
  | 'issues-tab-prev' | 'issues-tab-next' | 'issues-poll' | 'issues-topology' | 'issues-less';

/** Single keys and two-key sequences of the Local table, as the TUI binds them. */
export const ISSUES_KEYS: Readonly<Record<string, IssuesKeyAction>> = {
  Escape: 'issues-close', q: 'issues-close',
  j: 'issues-down', ArrowDown: 'issues-down', k: 'issues-up', ArrowUp: 'issues-up',
  gg: 'issues-first', G: 'issues-last', Enter: 'issues-inspect',
  r: 'issues-refresh', '/': 'issues-search', f: 'issues-filters', s: 'issues-sort', O: 'issues-operator',
  n: 'issues-new', e: 'issues-edit', S: 'issues-status', p: 'issues-priority', a: 'issues-assign',
  l: 'issues-labels', b: 'issues-deps', dd: 'issues-cancel', A: 'issues-archive', U: 'issues-restore',
  yy: 'issues-copy-uuid', 'y#': 'issues-copy-ref', Tab: 'issues-section', PageDown: 'issues-more',
  // G2: the Dispatched and Sync tabs, the manual poll, the topology starter, the previous history page.
  '[': 'issues-tab-prev', ']': 'issues-tab-next', P: 'issues-poll', T: 'issues-topology', PageUp: 'issues-less',
};

/** Keys that only begin a two-key sequence. */
const SEQUENCE_STARTS: ReadonlySet<string> = new Set(['g', 'd', 'y']);

export type KeyResult =
  | { kind: 'run'; action: IssuesKeyAction }
  | { kind: 'pending'; pending: string }
  | { kind: 'none' };

export const ISSUES_CHORD_MS = 1500;

export interface KeyState { pending: string; at: number }
export const idleKeys = (): KeyState => ({ pending: '', at: 0 });

/** Resolve one key; `g g`, `d d`, `y y` and `y #` are sequences. A broken sequence restarts on the new key. */
export function resolveIssuesKey(st: KeyState, key: string, now: number): { result: KeyResult; next: KeyState } {
  if (st.pending !== '' && now - st.at <= ISSUES_CHORD_MS) {
    const hit = ISSUES_KEYS[st.pending + key];
    if (hit) return { result: { kind: 'run', action: hit }, next: idleKeys() };
  }
  if (SEQUENCE_STARTS.has(key)) return { result: { kind: 'pending', pending: key }, next: { pending: key, at: now } };
  const single = ISSUES_KEYS[key];
  return single
    ? { result: { kind: 'run', action: single }, next: idleKeys() }
    : { result: { kind: 'none' }, next: idleKeys() };
}
