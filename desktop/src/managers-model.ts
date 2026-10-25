// Manager surfaces (hub #3011, slice 9): pure models, no DOM. The tree
// (GetManagerTree), the board and decisions (GetHarnessManagerState), the
// scope and policy forms, the Budget panel and the fleet follow the TUI's
// crates/rsi/src/overlay/{manager_tree,manager_v2,gm_budget,fleet}.rs rules;
// every request built here is checked again by src-tauri/src/managers.rs and
// by the daemon. Tested in tests/managers.test.mjs.

type Json = Record<string, unknown>;

const str = (v: unknown): string | null => (typeof v === 'string' && v !== '' ? v : null);
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const shortId = (id: string | null | undefined): string => (id ? id.slice(0, 8) : '—');

// ---------- manager tree ----------

export type TreeKind = 'global' | 'portfolio' | 'project' | 'area' | 'epic';

export interface TreeSeat {
  session_id: string;
  status: string;
  model: string | null;
  context_fill_pct: number | null;
  updated_at: string;
}

export interface TreeGrant {
  grant_version: number;
  capabilities: string[];
  max_active_sessions: number;
  max_created_sessions: number;
  max_created_containers: number;
  max_direct_reports: number;
  max_spend_usd: number | null;
  reserved: Array<{ resource_kind: string; amount: number }>;
}

export interface TreeLoad {
  running_workers: number | null;
  direct_reports: number | null;
  pending_escalations: number | null;
  pending_decisions: number | null;
}

export interface TreeRow {
  key: string;
  parent_key: string | null;
  depth: number;
  kind: TreeKind;
  label: string;
  tier_label?: string | null;
  grantor?: string | null;
  project_id: string | null;
  node_id: string | null;
  epic_id: string | null;
  scope: string | null;
  seat: TreeSeat | null;
  focus_session_id: string | null;
  grant: TreeGrant | null;
  load: TreeLoad;
  complete: boolean;
}

export interface TreePage {
  rows: TreeRow[];
  next_after: string | null;
  total_rows: number;
  complete: boolean;
  global_grant_version: number | null;
}

/** Rows per GetManagerTree page (the TUI's page and `MANAGER_TREE_MAX_LIMIT`). */
export const TREE_PAGE = 200;

/** Keys of rows that have at least one child row. */
export function parentKeys(rows: readonly TreeRow[]): Set<string> {
  const out = new Set<string>();
  for (const r of rows) if (r.parent_key) out.add(r.parent_key);
  return out;
}

/** The rows a folded tree shows: a folded row hides every descendant. */
export function visibleTreeRows(rows: readonly TreeRow[], folded: ReadonlySet<string>): TreeRow[] {
  const hidden = new Set<string>();
  const out: TreeRow[] = [];
  for (const r of rows) {
    if (r.parent_key && (hidden.has(r.parent_key) || folded.has(r.parent_key))) {
      hidden.add(r.key);
      continue;
    }
    out.push(r);
  }
  return out;
}

/** Every row below `key` in the depth-first page. */
export function descendants(rows: readonly TreeRow[], key: string): TreeRow[] {
  const under = new Set([key]);
  const out: TreeRow[] = [];
  for (const r of rows) {
    if (r.parent_key && under.has(r.parent_key)) {
      under.add(r.key);
      out.push(r);
    }
  }
  return out;
}

/** `h`: fold an open parent, else move to the row's parent (TUI manager tree). */
export function foldOrParent(
  rows: readonly TreeRow[], folded: ReadonlySet<string>, key: string,
): { folded: Set<string>; select: string } {
  const next = new Set(folded);
  if (parentKeys(rows).has(key) && !folded.has(key)) {
    next.add(key);
    return { folded: next, select: key };
  }
  const parent = rows.find((r) => r.key === key)?.parent_key;
  return { folded: next, select: parent ?? key };
}

export const KIND_BADGE: Readonly<Record<TreeKind, string>> = {
  global: 'GLOBAL', portfolio: 'PORTFOLIO', project: 'PROJECT', area: 'AREA', epic: 'EPIC',
};

/** A count the daemon could not take is "?", never 0. */
export const count = (n: number | null | undefined): string => (n === null || n === undefined ? '?' : String(n));

export function treeBadge(r: TreeRow): string {
  return r.kind === 'portfolio' && r.tier_label ? r.tier_label.toUpperCase() : KIND_BADGE[r.kind];
}

/** The seat health column: status, context fill, model. */
export function seatHealth(seat: TreeSeat | null): string {
  if (!seat) return 'no seat';
  const fill = seat.context_fill_pct === null ? 'ctx ?' : `ctx ${Math.round(seat.context_fill_pct)}%`;
  return [seat.status, fill, seat.model ?? ''].filter(Boolean).join(' · ');
}

/** The load column: workers, reports, escalations, decisions. */
export function treeLoad(r: TreeRow): string {
  const l = r.load;
  return `w ${count(l.running_workers)} · rep ${count(l.direct_reports)} · esc ${count(l.pending_escalations)} · dec ${count(l.pending_decisions)}`;
}

/** Whether the row needs the operator (pending escalations or decisions). */
export function treeNeedsAttention(r: TreeRow): boolean {
  return (r.load.pending_escalations ?? 0) > 0 || (r.load.pending_decisions ?? 0) > 0;
}

export function treeGrantSummary(r: TreeRow): string {
  const g = r.grant;
  if (!g) return r.scope ?? '';
  const spend = g.max_spend_usd === null ? 'no spend cap' : `$${g.max_spend_usd.toFixed(2)}`;
  const reserved = g.reserved.map((x) => `${x.resource_kind} ${x.amount}`).join(', ');
  return `v${g.grant_version} · active ${g.max_active_sessions} · sessions ${g.max_created_sessions} · containers ${g.max_created_containers} · ${spend}${reserved ? ` · reserved ${reserved}` : ''}`;
}

export type TreeAction = 'appoint' | 'edit' | 'revoke' | 'open' | 'above' | 'move';

export interface SeatCandidate { id: string; project_id?: string | null; archived: boolean; leaf: boolean }

/** Why the focused session cannot take a seat (manager_tree/actions.rs candidate_problem). */
function candidateBlock(c: SeatCandidate | null, current: string | null, project: string | null): string | null {
  if (!c) return 'Select the session to appoint in the session list first';
  if (c.archived) return 'The focused session is archived';
  if (!c.leaf) return 'A Group or Epic cannot hold a manager seat';
  if (project && c.project_id && c.project_id !== project) return 'The focused session is in another project';
  if (c.id === current) return 'That session already holds this seat';
  return null;
}

/**
 * Why an in-tree action cannot run on this row (the TUI table under
 * "Manager Tree"), or null.
 */
export function treeActionBlock(r: TreeRow | null, action: TreeAction, focusSession: SeatCandidate | null): string | null {
  if (!r) return 'Select a row first (j/k)';
  if (action === 'open') {
    return r.focus_session_id ? null : 'This row has no seat or lead session to open';
  }
  if (r.kind === 'epic') return 'Epic rows have no manager action; open the lead with Enter';
  if (action === 'above' || action === 'move') {
    if (r.kind !== 'portfolio') return 'Only a portfolio row gains a manager above it or moves under a sibling';
    if (!r.node_id || !r.grant) return 'This portfolio row holds no active grant';
    return action === 'above' ? candidateBlock(focusSession, r.focus_session_id, null) : null;
  }
  if (action === 'revoke') {
    if (r.kind === 'project' && !r.focus_session_id) return 'This project has no manager to revoke';
    if (r.kind === 'area' && (!r.node_id || !r.project_id)) return 'This area row has no node identity';
    if (r.kind === 'portfolio' && !r.node_id) return 'This portfolio row has no node identity';
    return null;
  }
  if (action === 'edit') {
    switch (r.kind) {
      case 'project': return r.focus_session_id ? null : 'Appoint a manager first (a)';
      case 'area':
        if (!r.node_id || !r.project_id) return 'This area row has no node identity';
        if (!r.grant) return 'The node is revoked: nothing to change';
        if ((r.load.direct_reports ?? 0) > 0) return `The node has ${r.load.direct_reports} direct report(s); the daemon only edits a childless node’s grant`;
        return null;
      default: return r.grant ? null : 'This row holds no active grant to edit';
    }
  }
  // appoint
  if (r.kind === 'area') return 'No RPC moves an area seat: revoke, then delegate anew';
  if (r.kind === 'project') return candidateBlock(focusSession, r.focus_session_id, r.project_id);
  if (!r.grant) return 'This row holds no active grant';
  return candidateBlock(focusSession, r.focus_session_id, null);
}

// ---------- tree grant editors (manager_tree/actions.rs GlobalEditor, NodeEditor) ----------

/** The per-project caps the global and portfolio editors change (GrantCaps). */
export interface GrantCaps { max_active_sessions: number; max_created_sessions: number; max_created_containers: number; max_spend_usd: number | null; allow_create_groups: boolean }

export const capsOf = (p: Json): GrantCaps => ({
  max_active_sessions: num(p.max_active_sessions) ?? 1,
  max_created_sessions: num(p.max_created_sessions) ?? 0,
  max_created_containers: num(p.max_created_containers) ?? 0,
  max_spend_usd: num(p.max_spend_usd),
  allow_create_groups: p.allow_create_groups === true,
});

const CAP_LABELS: ReadonlyArray<[keyof GrantCaps, string]> = [
  ['max_active_sessions', 'max active sessions'], ['max_created_sessions', 'max created sessions'],
  ['max_created_containers', 'max created containers'], ['max_spend_usd', 'max spend (USD)'], ['allow_create_groups', 'may create groups'],
];
const capText = (v: number | boolean | null): string => (v === null ? 'none' : typeof v === 'boolean' ? (v ? 'on' : 'off') : String(v));

/** Read the editor's typed caps, refusing what ManagerPolicyV2::validate refuses. */
export function capsFromForm(v: Record<string, string | boolean>): GrantCaps | { error: string } {
  const int = (k: string, label: string, lo: number, hi: number): number | string => {
    const t = String(v[k] ?? '').trim();
    const n = Number(t);
    return /^\d+$/.test(t) && n >= lo && n <= hi ? n : `${label} must be a whole number ${lo}-${hi}.`;
  };
  const active = int('max_active_sessions', 'max active sessions', 1, 100);
  const sessions = int('max_created_sessions', 'max created sessions', 0, 1024);
  const containers = int('max_created_containers', 'max created containers', 0, 64);
  for (const x of [active, sessions, containers]) if (typeof x === 'string') return { error: x };
  const spendText = String(v.max_spend_usd ?? '').trim().replace(/^\$/, '');
  let spend: number | null = null;
  if (spendText && !['none', 'off', 'unlimited'].includes(spendText)) {
    spend = Number(spendText);
    if (!Number.isFinite(spend) || spend <= 0) return { error: 'Spend must be a positive USD amount, or blank for none.' };
  }
  return { max_active_sessions: active as number, max_created_sessions: sessions as number, max_created_containers: containers as number, max_spend_usd: spend, allow_create_groups: v.allow_create_groups === true };
}

/** Every known project with whether the grant covers it, plus granted ids the desktop does not know, by name. */
export function editorProjects(projects: ReadonlyArray<{ id: string; name: string }>, granted: readonly string[]): Array<{ id: string; name: string; granted: boolean }> {
  const out = projects.map((p) => ({ id: p.id, name: p.name, granted: granted.includes(p.id) }));
  for (const id of granted) if (!out.some((p) => p.id === id)) out.push({ id, name: shortId(id), granted: true });
  return out.sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
}

/** The review of a grant edit (confirm_global_edit): what changes, and whether it narrows anything. */
export function grantEditReview(
  projects: ReadonlyArray<{ id: string; name: string }>, before: { ids: readonly string[]; caps: GrantCaps }, after: { ids: readonly string[]; caps: GrantCaps },
): { lines: string[]; destructive: boolean; scope: string } | { error: string } {
  if (!after.ids.length) return { error: 'Keep at least one project; x revokes the grant.' };
  const name = (id: string) => projects.find((p) => p.id === id)?.name ?? shortId(id);
  const added = after.ids.filter((id) => !before.ids.includes(id)).map(name);
  const removed = before.ids.filter((id) => !after.ids.includes(id)).map(name);
  const lines: string[] = [];
  for (const [k, label] of CAP_LABELS) {
    if (before.caps[k] !== after.caps[k]) lines.push(`${label}: ${capText(before.caps[k])} → ${capText(after.caps[k])}`);
  }
  const capsChanged = lines.length > 0;
  if (!capsChanged && !added.length && !removed.length) return { error: 'No change to save.' };
  if (added.length) lines.push(`+ grant: ${added.join(', ')}`);
  if (removed.length) lines.push(`- remove: ${removed.join(', ')} (their seats leave this manager’s supervision)`);
  const narrowed = after.caps.max_active_sessions < before.caps.max_active_sessions
    || after.caps.max_created_sessions < before.caps.max_created_sessions
    || after.caps.max_created_containers < before.caps.max_created_containers;
  const scope = capsChanged && (added.length || removed.length) ? 'projects and caps' : capsChanged ? 'caps' : 'projects';
  return { lines, destructive: removed.length > 0 || narrowed, scope };
}

/** The area editor's fields and bounds (NODE_FIELDS, NODE_BOUNDS). */
export const NODE_FIELDS: ReadonlyArray<{ label: string; min: number; max: number }> = [
  { label: 'max active sessions', min: 1, max: 100 }, { label: 'max created sessions', min: 0, max: 1024 }, { label: 'max direct reports', min: 1, max: 64 },
];

/** Typed area allowances, or the bound one breaks; and the review lines against the originals. */
export function nodeEditValues(raw: readonly string[], original: readonly number[]): { values: [number, number, number]; lines: string[]; destructive: boolean } | { error: string } {
  const values: number[] = [];
  for (const [i, f] of NODE_FIELDS.entries()) {
    const t = (raw[i] ?? '').trim();
    const n = Number(t);
    if (!/^\d+$/.test(t) || n < f.min || n > f.max) return { error: `${f.label} must be a whole number ${f.min}-${f.max}.` };
    values.push(n);
  }
  const lines = NODE_FIELDS.flatMap((f, i) => (values[i] === original[i] ? [] : [`${f.label}: ${original[i]} → ${values[i]}`]));
  if (!lines.length) return { error: 'No change to save.' };
  lines.push('The daemon refuses a grant that is not strictly narrower than the parent’s.');
  return { values: values as [number, number, number], lines, destructive: values.some((v, i) => v < (original[i] ?? 0)) };
}

// ---------- harness manager board, decisions, inbox, inspect ----------

export type BoardSection = 'board' | 'decisions' | 'inbox' | 'inspect' | 'policy';
export const BOARD_SECTIONS: readonly BoardSection[] = ['board', 'decisions', 'inbox', 'inspect', 'policy'];
export const BOARD_SECTION_TITLES: Readonly<Record<BoardSection, string>> = {
  board: 'Board', decisions: 'Decisions', inbox: 'Inbox', inspect: 'Inspect', policy: 'Policy',
};
/** Inspect subsections in the TUI's `[` / `]` order. */
export const INSPECT_SUBSECTIONS = ['overview', 'workers', 'work', 'topology', 'resources', 'actions', 'events', 'health', 'friction'] as const;
export type InspectSub = typeof INSPECT_SUBSECTIONS[number];
/** The four first pages the Board composes (Overview, Decisions, Requests, Health). */
export const BOARD_BANDS = ['overview', 'decisions', 'requests', 'health'] as const;
export type BoardBand = typeof BOARD_BANDS[number];
/** Rows a Board band shows before its `+more` line. */
export const BAND_ROWS = 6;
export const STATE_PAGE = 32;

/** Overview pages the Board reads at most to find the program and product KPI rows. */
export const KPI_OVERVIEW_PAGES = 4;

/** The Overview KPI rows of a page (`type: overview`). */
export const kpiRows = (page: StatePage | null | undefined): Json[] => (page?.rows ?? []).filter((r) => rowText(r, 'type') === 'overview');

export const hasAllKpis = (kpis: readonly Json[]): boolean => ['program', 'product'].every((k) => kpis.some((r) => rowText(r, 'kind') === k));

function kpiValue(row: Json, key: string): string {
  const v = field(row, key);
  return typeof v === 'number' ? String(v) : 'unknown';
}

/** The KPI line, one entry per program/product row (board.rs kpi_lines). */
export function kpiLines(kpis: readonly Json[], beyond: boolean): string[] {
  const lines = kpis.map((r) => {
    let line = `${rowText(r, 'kind') ?? 'work'} accepted ${kpiValue(r, 'accepted')} · integrated ${kpiValue(r, 'integrated')} / ${kpiValue(r, 'denominator')} · ready ${kpiValue(r, 'ready')} · partial ${kpiValue(r, 'partial')} · unknown ${kpiValue(r, 'unknown')}`;
    const missing = field(r, 'missing_work_scope');
    if (missing === true) line += ' · missing work scope';
    else if (missing !== false) line += ' · work scope unknown';
    return line;
  });
  if (beyond) lines.push(`KPIs beyond ${KPI_OVERVIEW_PAGES} Overview pages not loaded · Enter on a SIGNALS row, then n pages Overview`);
  else if (!lines.length) lines.push('KPIs unknown: the daemon returned no overview KPI rows.');
  return lines;
}

export function cycle<T>(list: readonly T[], at: T, delta: number): T {
  const i = list.indexOf(at);
  return list[((i < 0 ? 0 : i) + delta + list.length * 4) % list.length]!;
}

/** The daemon section (`ManagerInspectSectionV2`) a full section reads. */
export function sectionQuery(section: BoardSection, sub: InspectSub): string | null {
  switch (section) {
    case 'decisions': return 'decisions';
    case 'inbox': return 'requests';
    case 'inspect': return sub;
    default: return null;
  }
}

export interface StatePolicy {
  project_id: string;
  manager_session_id: string;
  scope_version: number;
  row_version: number;
  policy: Json;
  updated_at: string;
  revoked: boolean;
}

export interface StatePage {
  observed_at: string;
  scope_version: number;
  policy: StatePolicy | null;
  section: string;
  rows: Json[];
  next_cursor: string | null;
  complete: boolean;
}

/** A row field, or its `payload`'s when the row nests one (decisions.rs `field`). */
export function field(row: Json, name: string): unknown {
  const v = row[name];
  if (v !== undefined && v !== null) return v;
  const p = row.payload;
  if (p && typeof p === 'object') {
    const pv = (p as Json)[name];
    if (pv !== undefined && pv !== null) return pv;
  }
  return undefined;
}

export const rowText = (row: Json, name: string): string | null => str(field(row, name));

/** One line naming a record of any section. */
export function rowTitle(row: Json): string {
  for (const k of ['question', 'title', 'message', 'summary', 'subject', 'reason', 'body', 'label', 'key', 'decision_key', 'work_key', 'epic_id', 'session_id']) {
    const v = rowText(row, k);
    if (v) return (v.split('\n')[0] ?? v).trim();
  }
  return rowText(row, 'type') ?? 'record';
}

export function rowState(row: Json): string {
  return rowText(row, 'status') ?? rowText(row, 'state') ?? rowText(row, 'lead_state') ?? rowText(row, 'fence_state') ?? 'unknown';
}

/** The session `o` opens for a record. */
export function rowSession(row: Json): string | null {
  for (const k of ['session_id', 'lead_session_id', 'target_session_id', 'source_session_id', 'worker_session_id']) {
    const v = rowText(row, k);
    if (v && /^[0-9a-f-]{36}$/i.test(v)) return v;
  }
  const lead = field(row, 'lead');
  if (lead && typeof lead === 'object') return rowSession(lead as Json);
  return null;
}

/** Decision status in the TUI's words (decisions.rs `status_label`). */
export function decisionStatusLabel(row: Json): string {
  if (rowText(row, 'type') === 'operator_gate') return 'approval gate';
  const labels: Record<string, string> = {
    pending: 'needs answer', answer_queued: 'answer sending', answered: 'answered', declined: 'declined',
    withdrawn: 'withdrawn', archived: 'archived', target_unavailable: 'target gone',
    scope_revoked: 'scope revoked', resolved_externally: 'resolved elsewhere',
  };
  return labels[rowState(row)] ?? 'unknown state';
}

export interface DecisionOption { label: string; text: string }

/** `A: text` (or the label alone): the answer an option sends (decisions.rs). */
export const optionAnswer = (o: DecisionOption): string => (o.text ? `${o.label}: ${o.text}` : o.label);

const OPTION_LINE = /^\s*(?:[-*]\s*)?(?:\*\*)?(?:option\s+)?([A-Z0-9]{1,2})(?:\*\*)?\s*[).:\]-]\s+(.+)$/i;

/**
 * The record's options and recommendation: the daemon's structured
 * `options: [{label, detail?, recommended}]` when present, else option lines
 * in the question (`A) text`, `- B: text`) with a `(recommended)` marker or a
 * `Recommend: B` line.
 */
export function decisionOptions(row: Json): { options: DecisionOption[]; recommended: number | null } {
  const items = field(row, 'options');
  if (Array.isArray(items)) {
    const options: DecisionOption[] = [];
    let recommended: number | null = null;
    for (const it of items) {
      if (!it || typeof it !== 'object') continue;
      const label = str((it as Json).label)?.trim();
      if (!label) continue;
      if ((it as Json).recommended === true && recommended === null) recommended = options.length;
      options.push({ label, text: (str((it as Json).detail) ?? '').trim() });
    }
    if (options.length) return { options, recommended };
  }
  const options: DecisionOption[] = [];
  let recommended: number | null = null;
  let named: string | null = null;
  for (const line of (rowText(row, 'question') ?? '').split('\n')) {
    const rec = /recommend\w*\s*[:\-—]?\s*(?:option\s+|answer\s+|choice\s+)?\(?([A-Z0-9]{1,2})\b/i.exec(line);
    const m = OPTION_LINE.exec(line);
    if (m) {
      let text = m[2]!.trim();
      const marked = /\s*[([]?(?:recommended|recommend|★)[)\]]?\s*/i;
      if (marked.test(text) && /recommend|★/i.test(text)) {
        text = text.replace(marked, ' ').trim();
        if (recommended === null) recommended = options.length;
      }
      options.push({ label: m[1]!.toUpperCase(), text });
    } else if (rec && named === null) {
      named = rec[1]!.toUpperCase();
    }
  }
  if (recommended === null && named !== null) {
    const i = options.findIndex((o) => o.label === named);
    if (i >= 0) recommended = i;
  }
  return { options, recommended };
}

/**
 * Why the selected record cannot be answered now, or null (board.rs
 * `decision_draft`): it must be pending, carry its key, version and digest,
 * and the page must hold a current, unrevoked operator policy.
 */
export function decisionBlock(row: Json | null, page: StatePage | null): string | null {
  if (!row || !page) return 'Select a pending decision.';
  if (rowText(row, 'route_state') === 'unavailable') {
    return rowText(row, 'next_action') ?? 'Open this session to inspect its unresolved gate.';
  }
  const status = rowText(row, 'status') ?? rowText(row, 'state');
  if (!status) return 'Decision state is unknown. Refresh before answering.';
  if (status !== 'pending') return 'This decision is not pending. Refresh to inspect its outcome.';
  if (!(rowText(row, 'key') ?? rowText(row, 'decision_key'))) return 'Decision identity unavailable.';
  if (!((num(field(row, 'row_version')) ?? 0) > 0)) return 'Decision version unavailable.';
  if (!rowText(row, 'target_digest')) return 'Exact decision target digest unavailable.';
  const p = page.policy;
  if (!p || p.revoked || p.scope_version !== page.scope_version) {
    return 'A current operator policy is required. Use :manager policy.';
  }
  return null;
}

/** `AnswerHarnessManagerDecision` for the record (call decisionBlock first). */
export function answerRequest(projectId: string, row: Json, page: StatePage, answer: string, idempotencyKey: string): Json {
  return {
    project_id: projectId,
    fence: { scope_version: page.scope_version, policy_version: page.policy?.row_version ?? 0 },
    decision_key: rowText(row, 'key') ?? rowText(row, 'decision_key'),
    expected_row_version: num(field(row, 'row_version')),
    target_digest: rowText(row, 'target_digest'),
    answer,
    idempotency_key: idempotencyKey,
  };
}

/** Field names whose values are never shown (a credential is never displayed). */
export const SECRET_FIELD = /(^|_)(secret|password|passwd|credentials?|api_?key|authorization|cookie|bearer)($|_)|(^|_)token$/i;

/** The labelled detail lines of a record (scalars only; nested values summarised; secrets redacted). */
export function recordDetail(row: Json): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  const add = (prefix: string, o: Json) => {
    for (const [k, v] of Object.entries(o)) {
      if (SECRET_FIELD.test(k)) { out.push([prefix + k, '(hidden)']); continue; }
      if (k === 'payload' && v && typeof v === 'object' && !Array.isArray(v)) { add('', v as Json); continue; }
      if (v === null || v === undefined) { out.push([prefix + k, 'unknown']); continue; }
      if (typeof v === 'object') {
        out.push([prefix + k, Array.isArray(v) ? `${v.length} item(s)` : JSON.stringify(v, (key, x) => (key && SECRET_FIELD.test(key) ? '(hidden)' : x)).slice(0, 240)]);
        continue;
      }
      out.push([prefix + k, String(v)]);
    }
  };
  add('', row);
  return out;
}

// ---------- scope form ----------

export interface ScopeCandidate {
  id: string;
  title: string;
  kind: string;
  group_id: string | null;
  group_title: string | null;
}

export interface ManagerConfig {
  project_id: string;
  manager_session_id: string;
  current_session_id?: string | null;
  epic_ids: string[];
  scope_mode?: 'project' | 'selected';
  selected_epic_ids?: string[] | null;
  group_ids?: string[];
  row_version: number;
  updated_at: string;
}

export interface ScopeDraft { whole: boolean; groups: string[]; epics: string[] }

export const SCOPE_MAX = 32;

export function isRevoked(c: ManagerConfig): boolean {
  return c.scope_mode !== 'project' && (c.group_ids ?? []).length === 0 && c.epic_ids.length === 0;
}

/** The draft a scope form opens with: a new or revoked appointment covers the whole project. */
export function scopeDraftFrom(c: ManagerConfig | null): ScopeDraft {
  if (!c || c.scope_mode === 'project' || isRevoked(c)) return { whole: true, groups: [], epics: [] };
  return { whole: false, groups: [...(c.group_ids ?? [])], epics: [...(c.selected_epic_ids ?? c.epic_ids)] };
}

/** `:manager appoint`: a new seat covers the whole project; reopening the same seat keeps its selection. */
export function scopeDraftForSeat(c: ManagerConfig | null, seatId: string | null): ScopeDraft {
  return seatId && c && seatId !== c.manager_session_id ? { whole: true, groups: [], epics: [] } : scopeDraftFrom(c);
}

const isGroup = (c: ScopeCandidate): boolean => c.kind === 'Group';

/** `[x]` explicit, `[+]` inherited (whole project or its Group), `[ ]` not covered. */
export function scopeMark(d: ScopeDraft, c: ScopeCandidate): '[x]' | '[+]' | '[ ]' {
  if (d.whole) return '[+]';
  if (isGroup(c)) return d.groups.includes(c.id) ? '[x]' : '[ ]';
  if (d.epics.includes(c.id)) return '[x]';
  return c.group_id && d.groups.includes(c.group_id) ? '[+]' : '[ ]';
}

/** Space on a row: narrows a whole-project draft to the selection. */
export function toggleScope(d: ScopeDraft, c: ScopeCandidate): { draft: ScopeDraft } | { error: string } {
  const base: ScopeDraft = d.whole ? { whole: false, groups: [], epics: [] } : { ...d, groups: [...d.groups], epics: [...d.epics] };
  if (isGroup(c)) {
    if (base.groups.includes(c.id)) base.groups = base.groups.filter((g) => g !== c.id);
    else if (base.groups.length >= SCOPE_MAX) return { error: `At most ${SCOPE_MAX} Groups` };
    else base.groups.push(c.id);
    return { draft: base };
  }
  if (c.group_id && base.groups.includes(c.group_id)) {
    return { error: 'Deselect the Group before choosing its Epics' };
  }
  if (base.epics.includes(c.id)) base.epics = base.epics.filter((e) => e !== c.id);
  else if (base.epics.length >= SCOPE_MAX) return { error: `At most ${SCOPE_MAX} individual Epics` };
  else base.epics.push(c.id);
  return { draft: base };
}

/** `/`: name, parent Group name or ID. Selected rows outside the results stay selected. */
export function filterScope(cands: readonly ScopeCandidate[], query: string): ScopeCandidate[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...cands];
  return cands.filter((c) => c.title.toLowerCase().includes(q) || (c.group_title ?? '').toLowerCase().includes(q) || c.id.startsWith(q));
}

/** Selected ids the candidate list no longer has (kept visible for removal). */
export function unavailableSelections(d: ScopeDraft, cands: readonly ScopeCandidate[]): string[] {
  const known = new Set(cands.map((c) => c.id));
  return [...d.groups, ...d.epics].filter((id) => !known.has(id));
}

export function scopeSummary(d: ScopeDraft): string {
  if (d.whole) return 'whole project, including future Epics';
  if (!d.groups.length && !d.epics.length) return 'nothing: saving revokes supervision';
  return `${d.groups.length} group(s), ${d.epics.length} epic(s)`;
}

/** `ConfigureHarnessManager`: whole project omits both lists; an empty selection revokes. */
export function scopeSaveRequest(projectId: string, sessionId: string, d: ScopeDraft, expectedRowVersion: number): Json {
  const req: Json = { project_id: projectId, session_id: sessionId, expected_row_version: expectedRowVersion };
  if (!d.whole) {
    req.epic_ids = [...d.epics];
    if (d.groups.length) req.group_ids = [...d.groups];
  }
  return req;
}

/** `:manager clear`: the explicit empty selection. */
export const scopeClearRequest = (c: ManagerConfig): Json =>
  scopeSaveRequest(c.project_id, c.manager_session_id, { whole: false, groups: [], epics: [] }, c.row_version);

/** Tree `a`: the focused session takes the seat, keeping a live scope (manager_tree/actions.rs). */
export function appointRequest(projectId: string, sessionId: string, live: ManagerConfig | null): Json {
  const keep = live && !isRevoked(live) ? scopeDraftFrom(live) : { whole: true, groups: [], epics: [] };
  return scopeSaveRequest(projectId, sessionId, keep, live?.row_version ?? 0);
}

// ---------- policy form ----------

export const MODES = ['status', 'monitor', 'execute'] as const;
export const REVIEWER_TIERS = ['local', 'standard', 'premium'] as const;
export const CAPABILITIES = [
  'work_plan', 'lead_control', 'topology', 'session_create', 'lead_assign', 'integration', 'self_succession',
  'git_effect', 'session_control', 'issue_coordinate', 'operator_delegation', 'automation', 'storage_control',
  'daemon_settings', 'deploy',
] as const;

/** `ManagerPolicyV2::default()` (Status mode), the start of a first policy. */
export function defaultPolicy(): Json {
  return {
    mode: 'status', paused: false, paused_epic_ids: [], capabilities: [], group_ids: [],
    allow_create_groups: false, max_created_containers: 0, max_created_sessions: 0, max_active_sessions: 4,
    provider_limits: [], allowed_launches: [], max_recovery_attempts: 0, retry_delay_seconds: 60,
    request_timeout_seconds: 900, max_spend_usd: null, minimum_tier2_reviewer_tier: 'standard',
    daemon_setting_bounds: [],
  };
}

/**
 * `preset`, `suggest` and `advanced` are the TUI form's action rows (Observe /
 * Execute / Full / Custom, "Use suggested allowances", Advanced); `json` rows
 * edit a list field as JSON and `launches` opens the launch picker. Each is
 * checked by the rsi-common validator (manager_ops.rs) before it reaches the draft.
 */
export type PolicyFieldKind = 'cycle' | 'bool' | 'int' | 'spend' | 'caps' | 'json' | 'launches' | 'preset' | 'suggest' | 'advanced';
export interface PolicyField {
  key: string;
  label: string;
  kind: PolicyFieldKind;
  min?: number;
  max?: number;
  choices?: readonly string[];
}

/** The rows the desktop form edits, in the TUI's order. */
export const POLICY_FIELDS: readonly PolicyField[] = [
  { key: 'preset', label: 'Preset (Observe / Execute / Full project control)', kind: 'preset' },
  { key: 'suggest', label: 'Use suggested allowances', kind: 'suggest' },
  { key: 'mode', label: 'Mode', kind: 'cycle', choices: MODES },
  { key: 'paused', label: 'Operator pause', kind: 'bool' },
  { key: 'capabilities', label: 'Capabilities', kind: 'caps' },
  { key: 'allow_create_groups', label: 'May create Groups', kind: 'bool' },
  { key: 'max_active_sessions', label: 'Max active sessions', kind: 'int', min: 1, max: 100 },
  { key: 'max_created_sessions', label: 'Created sessions quota', kind: 'int', min: 0, max: 1024 },
  { key: 'max_created_containers', label: 'Created containers quota', kind: 'int', min: 0, max: 64 },
  { key: 'max_recovery_attempts', label: 'Retry count', kind: 'int', min: 0, max: 32 },
  { key: 'retry_delay_seconds', label: 'Retry delay (s)', kind: 'int', min: 1, max: 86400 },
  { key: 'request_timeout_seconds', label: 'Request timeout (s)', kind: 'int', min: 30, max: 604800 },
  { key: 'max_spend_usd', label: 'Spend cap (USD, blank = none)', kind: 'spend' },
  { key: 'minimum_tier2_reviewer_tier', label: 'Tier-2 reviewer tier', kind: 'cycle', choices: REVIEWER_TIERS },
  { key: 'paused_epic_ids', label: 'Paused Epics', kind: 'json' },
  { key: 'group_ids', label: 'Group grants', kind: 'json' },
  { key: 'provider_limits', label: 'Provider ceilings', kind: 'json' },
  { key: 'allowed_launches', label: 'Allowed launches (empty = any)', kind: 'launches' },
  { key: 'daemon_setting_bounds', label: 'Daemon setting bounds', kind: 'json' },
  { key: 'advanced', label: 'Advanced: every field as JSON', kind: 'advanced' },
];

/** The preset choices the Preset row offers, as manager_ops.rs names them. */
export const PRESETS: ReadonlyArray<{ id: 'observe' | 'execute' | 'full'; label: string }> = [
  { id: 'observe', label: 'Observe' }, { id: 'execute', label: 'Execute' }, { id: 'full', label: 'Full project control' },
];

export interface LaunchChoice { provider: string; model: string; effort: string | null }

const LAUNCH_PROVIDERS = ['Claude', 'Codex', 'Pioneer', 'OpenRouter', 'Bedrock', 'Local', 'Antigravity', 'CodexAppServer', 'Harness'];
const LAUNCH_EFFORTS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

/** `Provider/model[/effort]` (portfolio_command.rs parse_launch): a trailing effort word is the effort. */
export function parseLaunch(spelling: string): LaunchChoice | null {
  const t = spelling.trim();
  const slash = t.indexOf('/');
  if (slash < 0) return null;
  const provider = LAUNCH_PROVIDERS.find((p) => p.toLowerCase() === t.slice(0, slash).trim().toLowerCase());
  if (!provider) return null;
  const rest = t.slice(slash + 1);
  const last = rest.lastIndexOf('/');
  const effort = last >= 0 && LAUNCH_EFFORTS.includes(rest.slice(last + 1)) ? rest.slice(last + 1) : null;
  const model = effort ? rest.slice(0, last) : rest;
  return model ? { provider, model, effort } : null;
}

export const launchText = (l: LaunchChoice): string => `${l.provider}/${l.model}/${l.effort ?? 'any effort'}`;
const launchKey = (l: LaunchChoice): string => `${l.provider}/${l.model}/${l.effort ?? ''}`;

/** The catalog the launch picker offers: the given choices plus the draft's own, de-duplicated in order. */
export function launchCatalog(...lists: ReadonlyArray<readonly LaunchChoice[]>): LaunchChoice[] {
  const out: LaunchChoice[] = [];
  for (const l of lists.flat()) if (!out.some((x) => launchKey(x) === launchKey(l))) out.push({ provider: l.provider, model: l.model, effort: l.effort ?? null });
  return out;
}

// ---- launches a global or portfolio grant above a project manager does not cover (hub #3270, desktop #324) ----

/**
 * `ManagerLaunchChoiceV2::allows`: a granted launch admits the same provider and model, an exact effort admits only
 * itself, and an omitted effort admits every effort. (rsid also fails an effort off a known model's ladder; the
 * desktop does not keep the ladders, so an unknown spelling here counts as admitted and rsid has the last word.)
 */
const launchAllows = (granted: LaunchChoice, wanted: LaunchChoice): boolean => {
  if (granted.provider !== wanted.provider || granted.model !== wanted.model) return false;
  const g = granted.effort ?? null;
  const w = wanted.effort ?? null;
  return g === null ? true : w !== null && g === w;
};

/** `intersect_launches`: for each matching pair, the narrower entry, once. */
export function intersectLaunches(left: readonly LaunchChoice[], right: readonly LaunchChoice[]): LaunchChoice[] {
  const out: LaunchChoice[] = [];
  for (const a of left) {
    for (const b of right) {
      const pick = launchAllows(a, b) ? b : launchAllows(b, a) ? a : null;
      if (pick && !out.some((x) => launchKey(x) === launchKey(pick))) out.push({ provider: pick.provider, model: pick.model, effort: pick.effort ?? null });
    }
  }
  return out;
}

/** `portfolio_effective_launches`: the grant's launches narrowed by its project policy's list; a disjoint list falls back to the grant's. */
export function portfolioEffectiveLaunches(allowed: readonly LaunchChoice[], policy: Record<string, unknown> | null | undefined): LaunchChoice[] {
  const own = (policy?.allowed_launches as LaunchChoice[] | undefined) ?? [];
  const narrowed = own.length ? intersectLaunches(allowed, own) : [...allowed];
  return narrowed.length ? narrowed : [...allowed];
}

/**
 * The launches every live portfolio node covering `projectId` admits, intersected as rsid resolves a manager's effective
 * launches (`ancestor_launches_of`); null when no live node covers the project. A node with an empty launch list admits
 * nothing, as in rsid.
 */
export function ancestorLaunches(nodes: ReadonlyArray<Pick<PortfolioNode, 'state' | 'grant'>>, projectId: string): LaunchChoice[] | null {
  let out: LaunchChoice[] | null = null;
  for (const n of nodes) {
    if (n.state !== 'active' || !n.grant.project_ids.includes(projectId)) continue;
    const effective = portfolioEffectiveLaunches(n.grant.allowed_launches as LaunchChoice[], n.grant.project_policy);
    out = out === null ? effective : intersectLaunches(out, effective);
  }
  return out;
}

/** What a launch row says when the ancestor grants do not cover it. */
export const NOT_GRANTED_MARK = '✗ not granted by global manager';

/** The fix, in plain words (the same command the TUI names; the desktop's command line runs it too). */
export const NOT_GRANTED_HINT = 'A launch marked “✗ not granted by global manager” is outside what the global manager may start, so this project manager’s launches of that kind will be refused. To allow it, add it to the global manager’s grant: open the command line (:) and run “:manager portfolio launch add <node> <Provider/model>”.';

/** The catalog launches (by `launchText`) that `ancestors` do not cover; none when no ancestor restricts launches. */
export function uncoveredLaunchTexts(catalog: readonly LaunchChoice[], ancestors: readonly LaunchChoice[] | null): Set<string> {
  const out = new Set<string>();
  if (ancestors === null) return out;
  for (const l of catalog) if (!intersectLaunches([l], ancestors).length) out.add(launchText(l));
  return out;
}

/** The picked catalog entries (by their text) plus typed spellings; an unparsable spelling is an error. */
export function pickLaunches(catalog: readonly LaunchChoice[], checked: readonly string[], typed: string): { launches: LaunchChoice[] } | { error: string } {
  const out = catalog.filter((l) => checked.includes(launchText(l)));
  for (const w of typed.split(/[\s,]+/).filter(Boolean)) {
    const l = parseLaunch(w);
    if (!l) return { error: `Invalid launch ${w}; use Provider/model[/effort].` };
    out.push(l);
  }
  return { launches: launchCatalog(out) };
}

/** A list field typed as JSON: it must parse to an array. */
export function parseListField(raw: string, label: string): { value: unknown[] } | { error: string } {
  try {
    const v = JSON.parse(raw.trim() === '' ? '[]' : raw);
    return Array.isArray(v) ? { value: v } : { error: `${label} must be a JSON list` };
  } catch (e) {
    return { error: `${label}: ${e instanceof Error ? e.message : String(e)}` };
  }
}

export function policyValue(p: Json, f: PolicyField): string {
  const v = p[f.key];
  switch (f.kind) {
    case 'bool': return v ? 'yes' : 'no';
    case 'caps': return Array.isArray(v) && v.length ? v.join(', ') : 'none';
    case 'spend': return typeof v === 'number' ? v.toFixed(2) : 'none';
    case 'json': case 'launches':
      if (!Array.isArray(v) || v.length === 0) return f.key === 'allowed_launches' ? 'any valid choice' : 'none';
      return v.map((x) => (typeof x === 'object' && x ? Object.values(x as Json).filter((y) => y !== null).join('/') : shortId(String(x)))).join(', ');
    case 'preset': case 'suggest': case 'advanced': return '';
    default: return v === null || v === undefined ? 'unset' : String(v);
  }
}

/** Enter/Space on a cycle or bool field: the next value. */
export function stepPolicy(p: Json, f: PolicyField): Json {
  if (f.kind === 'bool') return { ...p, [f.key]: !p[f.key] };
  if (f.kind === 'cycle' && f.choices) return { ...p, [f.key]: cycle(f.choices, String(p[f.key]), 1) };
  return p;
}

/** Set a scalar from typed text; blank clears an optional spend cap. */
export function setPolicyScalar(p: Json, f: PolicyField, raw: string): { policy: Json } | { error: string } {
  const t = raw.trim();
  if (f.kind === 'spend') {
    if (t === '') return { policy: { ...p, max_spend_usd: null } };
    const n = Number(t);
    if (!Number.isFinite(n) || n <= 0) return { error: 'Spend cap must be a positive number, or blank for none' };
    return { policy: { ...p, max_spend_usd: n } };
  }
  if (f.kind !== 'int') return { error: `${f.label} is not typed` };
  if (!/^\d+$/.test(t)) return { error: `${f.label} must be a whole number` };
  const n = Number(t);
  if (n < (f.min ?? 0) || n > (f.max ?? Number.MAX_SAFE_INTEGER)) return { error: `${f.label} must be ${f.min}..${f.max}` };
  return { policy: { ...p, [f.key]: n } };
}

export function toggleCapability(p: Json, cap: string): Json {
  const caps = Array.isArray(p.capabilities) ? (p.capabilities as string[]) : [];
  return { ...p, capabilities: caps.includes(cap) ? caps.filter((c) => c !== cap) : [...caps, cap] };
}

/** What `ManagerPolicyV2::validate` would refuse among the fields the form edits. */
export function policyProblems(p: Json): string[] {
  const out: string[] = [];
  for (const f of POLICY_FIELDS) {
    if (f.kind !== 'int') continue;
    const v = p[f.key];
    if (typeof v !== 'number' || !Number.isInteger(v) || v < (f.min ?? 0) || v > (f.max ?? Infinity)) out.push(`${f.label} must be ${f.min}..${f.max}`);
  }
  const spend = p.max_spend_usd;
  if (spend !== null && spend !== undefined && !(typeof spend === 'number' && spend > 0)) out.push('Spend cap must be positive or none');
  const caps = Array.isArray(p.capabilities) ? (p.capabilities as string[]) : [];
  if (new Set(caps).size !== caps.length) out.push('A capability is listed twice');
  if (p.allow_create_groups && !caps.includes('topology')) out.push('May create Groups needs the topology capability');
  return out;
}

/** `ConfigureHarnessManagerPolicy`: the complete policy, fenced on the scope and policy versions observed. */
export function policySaveRequest(config: ManagerConfig, saved: StatePolicy | null, draft: Json, idempotencyKey: string): Json {
  return {
    project_id: config.project_id,
    expected_scope_version: config.row_version,
    expected_policy_version: saved?.row_version ?? 0,
    idempotency_key: idempotencyKey,
    policy: draft,
  };
}

/** An unchanged retry keeps its idempotency key; a changed request mints a new one. */
export function retryKey(prev: { body: string; key: string } | null, body: string, mint: () => string): { body: string; key: string } {
  return prev && prev.body === body ? prev : { body, key: mint() };
}

// ---------- budget panel ----------

export type CapField = 'created_sessions' | 'active_sessions' | 'created_containers' | 'spend_usd';
export const CAP_META: Readonly<Record<CapField, { label: string; policy: string; step: number; max: number }>> = {
  created_sessions: { label: 'created sessions', policy: 'max_created_sessions', step: 32, max: 1024 },
  active_sessions: { label: 'active sessions', policy: 'max_active_sessions', step: 1, max: 100 },
  created_containers: { label: 'containers', policy: 'max_created_containers', step: 4, max: 64 },
  spend_usd: { label: 'spend (USD)', policy: 'max_spend_usd', step: 10, max: 1_000_000 },
};

export interface BudgetCap { field: CapField; used: number; limit: number | null; binding?: boolean; spend_unknown?: boolean }
export interface BudgetNode {
  kind: 'portfolio' | 'project' | 'delegate';
  node_id: string;
  label: string;
  version: number;
  epoch: number;
  counted_since: string | null;
  caps: BudgetCap[];
}
export interface BudgetSnapshot {
  node_id: string;
  node_label: string;
  projects: Array<{ project_id: string; project_name: string; chain: BudgetNode[] }>;
  resets: string;
}

export interface BudgetRow { project: string; node: BudgetNode; cap: BudgetCap }

export function budgetRows(s: BudgetSnapshot | null): BudgetRow[] {
  if (!s) return [];
  return s.projects.flatMap((p) => p.chain.flatMap((node) => node.caps.map((cap) => ({ project: p.project_name, node, cap }))));
}

/** `BudgetCapFieldV1::normalize`: clamp, whole counts, active at least 1, spend 0 is no cap. */
export function normalizeCap(field: CapField, value: number): number | null {
  const v = Math.min(Math.max(value, 0), CAP_META[field].max);
  if (field === 'spend_usd') return v > 0 ? v : null;
  if (field === 'active_sessions') return Math.max(Math.round(v), 1);
  return Math.round(v);
}

export function describeCap(field: CapField, v: number | null): string {
  if (v === null) return 'none';
  return field === 'spend_usd' ? v.toFixed(2) : v.toFixed(0);
}

/** `+` raises one step; typed digits set a new limit. */
export function raiseTo(cap: BudgetCap, typed: string | null): { to: number | null } | { error: string } {
  if (typed === null) return { to: normalizeCap(cap.field, (cap.limit ?? cap.used) + CAP_META[cap.field].step) };
  const n = Number(typed.trim());
  if (typed.trim() === '' || !Number.isFinite(n) || n < 0) return { error: 'Type a number (0 removes a spend cap)' };
  return { to: normalizeCap(cap.field, n) };
}

/** Why a cap cannot be edited here (a delegate's allowance lives in the tree). */
export function budgetEditBlock(r: BudgetRow | null): string | null {
  if (!r) return 'Select a cap first (j/k)';
  if (r.node.kind === 'delegate') return 'A delegate’s allowance is edited in :manager tree (e on its row)';
  return null;
}

/** Write one cap into a policy object (`BudgetCapFieldV1::apply`). */
export function applyCap(policy: Json, field: CapField, to: number | null): Json {
  const key = CAP_META[field].policy;
  if (field === 'spend_usd') return { ...policy, [key]: to };
  return { ...policy, [key]: to ?? (field === 'active_sessions' ? 1 : 0) };
}

/** The PM path: `ConfigureHarnessManagerPolicy` with one cap changed, after the fence check. */
export function projectRaiseRequest(node: BudgetNode, current: StatePolicy, field: CapField, to: number | null, key: string): { request: Json } | { error: string } {
  if (current.row_version !== node.version || current.scope_version !== node.epoch) {
    return { error: `The PM policy changed since the panel loaded (v${node.version} → v${current.row_version}); press r and retry` };
  }
  return {
    request: {
      project_id: current.project_id, expected_scope_version: current.scope_version,
      expected_policy_version: current.row_version, idempotency_key: key, policy: applyCap(current.policy, field, to),
    },
  };
}

export interface PortfolioNode {
  node_id: string;
  tier_label: string;
  state: string;
  authority_epoch: number;
  parent_node_id: string | null;
  max_direct_reports: number;
  child_policy: Json | null;
  grant: { grant_version: number; seat_session_id: string; project_ids: string[]; allowed_launches: unknown[]; project_policy: Json };
}

/** The portfolio path (`raise_portfolio_request`): the node's whole grant with one cap changed. */
export function portfolioRaiseRequest(node: BudgetNode, current: PortfolioNode, field: CapField, to: number | null, key: string): { request: Json } | { error: string } {
  if (current.grant.grant_version !== node.version || current.authority_epoch !== node.epoch) {
    return { error: `The grant changed since the panel loaded (v${node.version} → v${current.grant.grant_version}); press r and retry` };
  }
  return {
    request: {
      node_id: current.node_id, parent_node_id: current.parent_node_id, tier_label: current.tier_label,
      seat_session_id: current.grant.seat_session_id, project_ids: current.grant.project_ids,
      allowed_launches: current.grant.allowed_launches, policy: applyCap(current.grant.project_policy, field, to),
      child_policy: current.child_policy, max_direct_reports: current.max_direct_reports,
      expected_node_grant_version: current.grant.grant_version, expected_authority_epoch: current.authority_epoch,
      idempotency_key: key,
    },
  };
}

// ---------- fleet ----------

export const FLEET_WINDOWS = [300, 3600, 86400] as const;
export const FLEET_WINDOW_TITLES = ['5 min', '1 h', '24 h'] as const;
export const FLEET_DIMENSIONS = ['project', 'provider', 'model'] as const;
export const AGENT_SORTS = ['project', 'provider', 'model', 'status', 'context'] as const;
export const GROUP_SORTS = ['group', 'active', 'tokens', 'cost', 'errors'] as const;

export interface FleetUsage { invocations: number; errors: number; input: number; output: number; cache_read: number; cache_write: number; cost: number; unknown_usage: number }
export interface FleetAgent {
  session: { id: string; provider: string; model?: string | null; effort?: string | null; status: string; title?: string | null; query?: string; context_fill_pct?: number | null };
  project: string;
  role: string;
  turn_started_at: string | null;
}
export interface FleetGroup { dimension: string; key: string; label: string; active: number; windows: FleetUsage[] }
export interface FleetOverview { as_of: string; agents: FleetAgent[]; groups: FleetGroup[]; totals: FleetUsage[]; agents_truncated: boolean; usage_truncated: boolean }

export const tokensPerMinute = (u: FleetUsage, seconds: number): number => ((u.input + u.output + u.cache_read + u.cache_write) * 60) / seconds;
export const costPerHour = (u: FleetUsage, seconds: number): number => (u.cost * 3600) / seconds;
export const errorPct = (u: FleetUsage): number => (u.invocations === 0 ? 0 : (u.errors * 100) / u.invocations);

const cmp = (a: string | number, b: string | number): number => (a < b ? -1 : a > b ? 1 : 0);

export function fleetAgents(o: FleetOverview | null, filter: string, sort: number, reverse: boolean): FleetAgent[] {
  if (!o) return [];
  const q = filter.toLowerCase();
  const rows = o.agents.filter((a) => [
    a.project, a.role, a.session.provider, a.session.model ?? 'Unknown', a.session.effort ?? 'default',
    a.session.status, a.session.title ?? a.session.query ?? '',
  ].join(' ').toLowerCase().includes(q));
  const keyOf = (a: FleetAgent): string | number => {
    switch (sort) {
      case 0: return a.project;
      case 1: return a.session.provider;
      case 2: return a.session.model ?? '';
      case 3: return a.session.status;
      default: return a.session.context_fill_pct ?? -1;
    }
  };
  rows.sort((a, b) => {
    const o2 = cmp(keyOf(a), keyOf(b)) || cmp(a.session.id, b.session.id);
    return reverse ? -o2 : o2;
  });
  return rows;
}

export function fleetGroups(o: FleetOverview | null, dimension: number, windowIdx: number, filter: string, sort: number, reverse: boolean): FleetGroup[] {
  if (!o) return [];
  const dim = FLEET_DIMENSIONS[dimension] ?? 'project';
  const q = filter.toLowerCase();
  const rows = o.groups.filter((g) => g.dimension === dim && g.label.toLowerCase().includes(q));
  const u = (g: FleetGroup): FleetUsage => g.windows[windowIdx]!;
  rows.sort((a, b) => {
    let o2: number;
    switch (sort) {
      case 0: o2 = cmp(a.label, b.label); break;
      case 1: o2 = b.active - a.active; break;
      case 2: o2 = tokensPerMinute(u(b), 300) - tokensPerMinute(u(a), 300); break;
      case 3: o2 = u(b).cost - u(a).cost; break;
      default: o2 = errorPct(u(b)) - errorPct(u(a));
    }
    o2 = o2 || cmp(a.key, b.key);
    return reverse ? -o2 : o2;
  });
  return rows;
}

export function usageLine(u: FleetUsage | undefined, windowIdx: number): string {
  if (!u) return 'no usage';
  const s = FLEET_WINDOWS[windowIdx] ?? 300;
  const partial = u.unknown_usage > 0 ? ` · ${u.unknown_usage} partial` : '';
  return `${Math.round(tokensPerMinute(u, s))} tok/min · $${costPerHour(u, s).toFixed(2)}/h · ${u.invocations} calls · ${errorPct(u).toFixed(1)}% err${partial}`;
}

// ---------- global manager workspace ----------

export interface WorkspacePredecessor { session_id: string; project_id: string | null; status: string; provider: string; model: string | null; context_fill_pct: number | null; cost_usd: number | null; updated_at: string }
export interface WorkspaceSeat { session_id: string; project_id: string | null; status: string; provider: string; model: string | null; context_fill_pct: number | null; rotation_threshold_pct?: number | null; cost_usd: number | null; updated_at: string; pending_question: boolean; succession_refusal?: string | null; predecessors?: WorkspacePredecessor[]; predecessors_truncated?: boolean }
export interface WorkspaceProject {
  overview: {
    project_id: string; name: string; path: string | null;
    manager: (WorkspaceSeat & { scope_version: number; effort?: string | null }) | null;
    policy: { policy_version: number; mode: string; revoked: boolean; paused: boolean; capabilities: string[] } | null;
    issues: { open: number; in_progress: number; open_operator_requests: number };
    running_sessions: number; waiting_approval_sessions: number; pending_questions: number; pending_approvals: number;
  };
  scope_revoked: boolean;
}
export interface Workspace {
  grant: { grant_id: string; grant_version: number; seat_session_id: string; state: string; project_ids: string[]; project_policy: Json; operator_origin: string; updated_at: string } | null;
  seat: WorkspaceSeat | null;
  projects: WorkspaceProject[];
  missing_project_ids: string[];
}

/** A manager node reference (`ManagerNodeRefV1`). */
export type NodeRef = { kind: 'portfolio' | 'area'; node_id: string } | { kind: 'project'; project_id: string };
export const nodeRefId = (r: NodeRef): string => (r.kind === 'project' ? r.project_id : r.node_id);

export interface NodeChild {
  node: NodeRef; label: string; state: string; grant_version: number; seat: WorkspaceSeat | null; project_ids: string[];
  counts: { issues: { open: number; in_progress: number; open_operator_requests: number }; running_sessions: number; waiting_approval_sessions: number; pending_questions: number; pending_approvals: number } | null;
  pending_escalations: number;
}
export interface NodeWorkspace {
  node: NodeRef; label: string; state: string; parent: NodeRef | null;
  grant: (Workspace['grant'] & Json) | null; seat: WorkspaceSeat | null;
  children: NodeChild[]; children_truncated: boolean; projects: WorkspaceProject[]; missing_project_ids: string[];
  escalations: Json[]; escalations_truncated: boolean;
  fleet: { as_of: string; active: number; totals: FleetUsage[]; agents_truncated: boolean; usage_truncated: boolean };
}

export type WorkspaceRowKind = 'seat' | 'project' | 'predecessor' | 'child' | 'missing';
export interface WorkspaceRow {
  kind: WorkspaceRowKind; title: string; health: string; detail: string; sessionId: string | null; projectId: string | null; attention: boolean;
  /** A child node row opens that node's console. */
  node?: NodeRef;
  /** The seat level `n` launches for: the console's own seat, or a project's PM. */
  level?: 'console' | 'project';
  indent?: number;
}

function seatLine(s: WorkspaceSeat | null): string {
  if (!s) return 'no seat';
  const fill = s.context_fill_pct === null ? 'ctx ?' : `ctx ${Math.round(s.context_fill_pct)}%${s.rotation_threshold_pct ? `/${Math.round(s.rotation_threshold_pct)}%` : ''}`;
  const cost = s.cost_usd === null ? '' : `$${s.cost_usd.toFixed(2)}`;
  const extra = [s.pending_question ? 'question waiting' : '', s.succession_refusal ? `succession refused: ${s.succession_refusal}` : ''];
  return [s.status, `${s.provider}${s.model ? `/${s.model}` : ''}`, fill, cost, ...extra].filter(Boolean).join(' · ');
}

/** Earlier sessions of a seat (#1627): read-only history rows under it. */
function predecessorRows(s: WorkspaceSeat | null, indent: number): WorkspaceRow[] {
  const rows = (s?.predecessors ?? []).map((p): WorkspaceRow => ({
    kind: 'predecessor', title: `earlier seat ${p.session_id.slice(0, 8)}`, indent,
    health: [p.status, `${p.provider}${p.model ? `/${p.model}` : ''}`, p.cost_usd === null ? '' : `$${p.cost_usd.toFixed(2)}`].filter(Boolean).join(' · '),
    detail: `updated ${p.updated_at} · read only (a archives it)`, sessionId: p.session_id, projectId: p.project_id, attention: false,
  }));
  if (s?.predecessors_truncated) rows.push({ kind: 'predecessor', title: 'more earlier seats not listed', indent, health: '', detail: '', sessionId: null, projectId: null, attention: false });
  return rows;
}

function projectRow(p: WorkspaceProject, indent = 0): WorkspaceRow[] {
  const o = p.overview;
  const policy = o.policy ? `${o.policy.mode}${o.policy.paused ? ' (paused)' : ''}${o.policy.revoked ? ' (revoked)' : ''} v${o.policy.policy_version}` : 'no policy';
  return [{
    kind: 'project', title: o.name, level: 'project', indent,
    health: p.scope_revoked ? 'PM scope revoked' : seatLine(o.manager),
    detail: `${policy} · Issues ${o.issues.open} open / ${o.issues.in_progress} in progress / ${o.issues.open_operator_requests} operator · ${o.running_sessions} running · ${o.waiting_approval_sessions} waiting · ${o.pending_questions} questions · ${o.pending_approvals} approvals`,
    sessionId: o.manager?.session_id ?? null, projectId: o.project_id,
    attention: o.pending_questions > 0 || o.pending_approvals > 0 || o.issues.open_operator_requests > 0,
  }, ...predecessorRows(p.scope_revoked ? null : o.manager, indent + 1)];
}

const missingRows = (ids: readonly string[]): WorkspaceRow[] => ids.map((id) => ({
  kind: 'missing', title: `missing project ${id.slice(0, 8)}`, health: 'MISSING', detail: 'The project no longer exists; re-appoint the grant without it', sessionId: null, projectId: null, attention: true,
}));

/** The console's rows: the global seat (and its earlier sessions), then each granted project's PM. */
export function workspaceRows(w: Workspace | null): WorkspaceRow[] {
  if (!w) return [];
  const rows: WorkspaceRow[] = [{
    kind: 'seat', title: 'Global seat', health: seatLine(w.seat), level: 'console',
    detail: w.grant ? `grant v${w.grant.grant_version} · ${w.grant.state} · ${w.grant.project_ids.length} project(s)` : 'no global grant',
    sessionId: w.seat?.session_id ?? null, projectId: w.seat?.project_id ?? null, attention: !!w.seat?.pending_question,
  }, ...predecessorRows(w.seat, 1)];
  for (const p of w.projects) rows.push(...projectRow(p));
  rows.push(...missingRows(w.missing_project_ids ?? []));
  return rows;
}

/** A node console (#1240): its own seat, each child node as a digest, then the projects it manages. */
export function nodeWorkspaceRows(n: NodeWorkspace | null): WorkspaceRow[] {
  if (!n) return [];
  const rows: WorkspaceRow[] = [{
    kind: 'seat', title: `${n.label} seat`, health: n.seat ? seatLine(n.seat) : n.state.toUpperCase(), level: 'console',
    detail: n.grant ? `grant v${n.grant.grant_version} · ${n.state}` : n.state, sessionId: n.seat?.session_id ?? null,
    projectId: n.seat?.project_id ?? null, attention: !!n.seat?.pending_question,
  }, ...predecessorRows(n.seat, 1)];
  for (const c of n.children) {
    const counts = c.counts ? ` · ${c.counts.running_sessions} running · ${c.counts.pending_questions} questions · ${c.counts.pending_approvals} approvals` : '';
    rows.push({
      kind: 'child', title: `${c.node.kind} ${c.label}`, node: c.node, indent: 1,
      health: c.seat ? seatLine(c.seat) : c.state.toUpperCase(),
      detail: `grant v${c.grant_version} · ${c.project_ids.length} project(s)${counts} · ${c.pending_escalations} escalation(s)`,
      sessionId: c.seat?.session_id ?? null, projectId: c.seat?.project_id ?? null, attention: c.pending_escalations > 0 || (c.counts?.pending_questions ?? 0) > 0,
    });
  }
  if (n.children_truncated) rows.push({ kind: 'child', title: 'more child nodes not listed', health: '', detail: '', sessionId: null, projectId: null, attention: false, indent: 1 });
  for (const p of n.projects) rows.push(...projectRow(p, 1));
  rows.push(...missingRows(n.missing_project_ids ?? []));
  return rows;
}

const refText = (r: NodeRef | null): string => (r ? `${r.kind}:${nodeRefId(r).slice(0, 8)}` : 'the operator');

/** The node console's header: where it sits, its grant, its fleet and its escalations. */
export function nodeHeader(n: NodeWorkspace): string {
  const f = n.fleet;
  const rate = (u: FleetUsage | undefined, sec: number) => (u ? `${Math.round(tokensPerMinute(u, sec))} tok/min · $${costPerHour(u, sec).toFixed(2)}/h` : '?');
  const fleet = `fleet ${f.active} active · 5 min ${rate(f.totals[0], 300)} · 1 h ${rate(f.totals[1], 3600)} · 24 h ${f.totals[2]?.invocations ?? '?'} calls${f.agents_truncated || f.usage_truncated ? ' · truncated' : ''}`;
  const grant = n.grant ? `grant v${n.grant.grant_version} over ${n.grant.project_ids.length} project(s)` : 'no portfolio grant';
  return `${n.label} (${n.state}) reports to ${refText(n.parent)} · ${grant} · ${fleet} · ${n.escalations.length}${n.escalations_truncated ? '+' : ''} escalation(s) waiting`;
}

/** Why a workspace key cannot act on a row (the TUI console's refusals), or null. */
export function seatActionBlock(r: WorkspaceRow | null, action: 'type' | 'status' | 'halt' | 'archive'): string | null {
  if (!r || r.kind === 'missing') return `Select a seat row to ${action === 'type' ? 'talk to' : action === 'status' ? 'show' : action}`;
  if (r.kind === 'child' && action !== 'status') return 'A child node row opens its console (Enter); talk to its seat there';
  if (!r.sessionId) return `${r.title} has no session${action === 'type' ? ' to talk to' : ''}; n launches a manager`;
  if (action === 'type' && r.kind === 'predecessor') return 'An earlier seat session is read-only history; t talks to the current seat';
  if (action === 'archive' && r.kind !== 'predecessor') return `${r.title} is the current seat; revoke or replace it before archiving. Only earlier seat sessions can be archived here`;
  return null;
}

export const HALTABLE = ['Starting', 'Running', 'WaitingApproval'];

// ---------- keys inside a manager view ----------

export type ManagerView = 'tree' | 'board' | 'scope' | 'workspace' | 'budget' | 'fleet' | 'lists';

/** Each view's own keys (the TUI's overlay tables), resolved to slice actions. */
export const VIEW_KEYS: Readonly<Record<ManagerView, Readonly<Record<string, string>>>> = {
  tree: {
    j: 'mgr-down', ArrowDown: 'mgr-down', k: 'mgr-up', ArrowUp: 'mgr-up', g: 'mgr-first', G: 'mgr-last',
    PageDown: 'mgr-page-down', PageUp: 'mgr-page-up', h: 'mgr-fold', l: 'mgr-unfold', ' ': 'mgr-toggle-fold',
    H: 'mgr-fold-all', L: 'mgr-unfold-all', Enter: 'mgr-enter', o: 'mgr-open-project', p: 'mgr-preview',
    a: 'mgr-appoint', e: 'mgr-edit', x: 'mgr-revoke', A: 'mgr-above', m: 'mgr-move', n: 'mgr-next-page', r: 'mgr-refresh', Escape: 'mgr-close', q: 'mgr-close',
  },
  board: {
    1: 'mgr-section-1', 2: 'mgr-section-2', 3: 'mgr-section-3', 4: 'mgr-section-4', 5: 'mgr-section-5',
    Tab: 'mgr-section-next', 'S-Tab': 'mgr-section-prev', '[': 'mgr-sub-prev', ']': 'mgr-sub-next',
    PageDown: 'mgr-detail-down', PageUp: 'mgr-detail-up',
    j: 'mgr-down', ArrowDown: 'mgr-down', k: 'mgr-up', ArrowUp: 'mgr-up', g: 'mgr-first', G: 'mgr-last',
    h: 'mgr-option-prev', ArrowLeft: 'mgr-option-prev', l: 'mgr-option-next', ArrowRight: 'mgr-option-next',
    Enter: 'mgr-enter', y: 'mgr-accept-recommended', a: 'mgr-answer', X: 'mgr-archive-stale', o: 'mgr-open-session',
    n: 'mgr-next-page', p: 'mgr-prev-page', r: 'mgr-refresh', s: 'mgr-save', ' ': 'mgr-space', Escape: 'mgr-close', q: 'mgr-close',
  },
  scope: {
    j: 'mgr-down', ArrowDown: 'mgr-down', k: 'mgr-up', ArrowUp: 'mgr-up', g: 'mgr-first', G: 'mgr-last',
    ' ': 'mgr-toggle', a: 'mgr-whole-project', '/': 'mgr-filter', Enter: 'mgr-save', Escape: 'mgr-close', q: 'mgr-close',
  },
  workspace: {
    j: 'mgr-down', ArrowDown: 'mgr-down', k: 'mgr-up', ArrowUp: 'mgr-up', g: 'mgr-first', G: 'mgr-last',
    Enter: 'mgr-enter', l: 'mgr-enter', p: 'mgr-open-project', b: 'mgr-budget', T: 'mgr-tree', F: 'mgr-fleet',
    i: 'mgr-type', t: 'mgr-talk', s: 'mgr-status', x: 'mgr-halt', X: 'mgr-halt-now', a: 'mgr-archive-seat', n: 'mgr-launch',
    Tab: 'mgr-conversation', Backspace: 'mgr-parent',
    r: 'mgr-refresh', Escape: 'mgr-close', q: 'mgr-close',
  },
  budget: {
    j: 'mgr-down', ArrowDown: 'mgr-down', k: 'mgr-up', ArrowUp: 'mgr-up', g: 'mgr-first', G: 'mgr-last',
    '+': 'mgr-raise', '=': 'mgr-raise', Enter: 'mgr-type-limit', r: 'mgr-refresh', Escape: 'mgr-close', q: 'mgr-close', b: 'mgr-close',
  },
  fleet: {
    j: 'mgr-down', ArrowDown: 'mgr-down', k: 'mgr-up', ArrowUp: 'mgr-up', g: 'mgr-first', G: 'mgr-last',
    '/': 'mgr-filter', Tab: 'mgr-focus', s: 'mgr-sort', S: 'mgr-reverse', b: 'mgr-dimension', w: 'mgr-window', y: 'mgr-copy',
    1: 'mgr-sort-1', 2: 'mgr-sort-2', 3: 'mgr-sort-3', 4: 'mgr-sort-4', 5: 'mgr-sort-5',
    Enter: 'mgr-enter', r: 'mgr-refresh', Escape: 'mgr-close', q: 'mgr-close',
  },
  lists: {
    j: 'mgr-down', ArrowDown: 'mgr-down', k: 'mgr-up', ArrowUp: 'mgr-up', g: 'mgr-first', G: 'mgr-last',
    Enter: 'mgr-open-session', o: 'mgr-open-session', r: 'mgr-refresh', Escape: 'mgr-close', q: 'mgr-close',
  },
};

/** Clamp a cursor into `len` rows (−1 when empty). */
export function clampIndex(i: number, len: number): number {
  return len === 0 ? -1 : Math.min(Math.max(i, 0), len - 1);
}

// ---------- the workspace's launch-and-appoint form (global_manager_workspace_launch.rs) ----------

/** A global grant holds at most this many projects. */
export const MAX_SCOPE_PROJECTS = 64;

export type LaunchRole = { kind: 'global' } | { kind: 'project'; projectId: string; name: string };

/** The manager's first instruction for the role and scope (LaunchForm::default_prompt). */
export function defaultLaunchPrompt(role: LaunchRole, scopeNames: readonly string[]): string {
  return role.kind === 'global'
    ? `You are the operator-appointed global manager for: ${scopeNames.join(', ')}. Load the rsi-portfolio-manager skill, call AgentGetAuthorityCatalog to confirm your seat, then report a short portfolio digest.`
    : `You are the operator-appointed project manager for ${role.name}. Load the rsi-project-manager skill, call AgentGetAuthorityCatalog to verify your seat, then start the integrator loop.`;
}

/** The global seat's home: the chosen host when it is in scope, else the tab's project when in scope, else the first in scope. */
export function launchHost(scope: readonly string[], chosen: string | null, active: string | null): string | null {
  if (chosen && scope.includes(chosen)) return chosen;
  if (active && scope.includes(active)) return active;
  return scope[0] ?? null;
}

/** The scope a global launch starts with: the live grant's projects, else every project. */
export function launchScope(projects: ReadonlyArray<{ id: string }>, granted: readonly string[] | null): string[] {
  return projects.map((p) => p.id).filter((id) => !granted?.length || granted.includes(id));
}

export interface LaunchPlan { role: LaunchRole; launch: LaunchChoice; prompt: string; scope: string[]; home: string }

/** Validate the form (LaunchForm::validate); the error names the field to fix. */
export function validateLaunch(
  role: LaunchRole, launch: LaunchChoice | null, catalog: readonly LaunchChoice[], prompt: string, scope: readonly string[],
  host: string | null, projects: ReadonlyArray<{ id: string }>,
): LaunchPlan | { error: string } {
  if (!launch || !catalog.some((c) => launchText(c) === launchText(launch))) return { error: 'Pick a launch from the allowed catalog (Provider/Model/Effort)' };
  const text = prompt.trim();
  if (!text) return { error: 'Write the manager’s first instruction in Prompt' };
  const exists = (id: string) => projects.some((p) => p.id === id);
  if (role.kind === 'project') {
    if (!exists(role.projectId)) return { error: 'That project no longer exists; pick another seat' };
    return { role, launch, prompt: text, scope: [], home: role.projectId };
  }
  if (!scope.length) return { error: 'Choose at least one project in Scope' };
  if (scope.length > MAX_SCOPE_PROJECTS) return { error: `A global grant holds at most ${MAX_SCOPE_PROJECTS} projects; uncheck ${scope.length - MAX_SCOPE_PROJECTS}` };
  const gone = scope.find((id) => !exists(id));
  if (gone) return { error: `Project ${gone.slice(0, 8)} no longer exists; uncheck it or press r` };
  return { role, launch, prompt: text, scope: [...scope], home: launchHost(scope, host, null) ?? scope[0]! };
}

/** `LaunchSession` answers `{session_id}`. */
export const launchedSessionId = (r: unknown): string | null => {
  const id = r && typeof r === 'object' ? (r as Json).session_id : null;
  return typeof id === 'string' && id ? id : null;
};
