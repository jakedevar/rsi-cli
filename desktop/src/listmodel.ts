// Pure session-list model for slice 4 (hub #3006): sort orders, label groups,
// folds, hierarchy and attention navigation, project and label form rules, the
// session-info rows. No DOM; unit-tested under tests/listmodel.test.mjs.
import type { Project, Session, SessionLabel } from './api.js';
import { needsAttention } from './approvals-model.js';
import { type ContextFields, contextRows } from './context-model.js';
import { sessionLabel } from './model.js';
import type { TreeRow } from './tree.js';

// ---------- sort orders (crates/rsi/src/app/mod.rs SortOrder) ----------

export type SortOrderId = 'stalest' | 'freshest' | 'oldest-created' | 'newest-created' | 'by-label';

export interface SortOrderInfo {
  id: SortOrderId;
  label: string;
  description: string;
}

/** Same names and order as the TUI's sort picker. */
export const SORT_ORDERS: readonly SortOrderInfo[] = [
  { id: 'stalest', label: 'Longest idle', description: 'Idle the longest at the top' },
  { id: 'freshest', label: 'Recently active', description: 'Latest activity at the top' },
  { id: 'oldest-created', label: 'Oldest first', description: 'Earliest created at the top' },
  { id: 'newest-created', label: 'Newest first', description: 'Most recently created at the top' },
  { id: 'by-label', label: 'Grouped by label', description: 'Gathered under each label' },
];

export const DEFAULT_SORT: SortOrderId = 'stalest';

export function parseSortOrder(raw: string | null | undefined): SortOrderId {
  return SORT_ORDERS.find((o) => o.id === raw)?.id ?? DEFAULT_SORT;
}

export function sortInfo(id: SortOrderId): SortOrderInfo {
  return SORT_ORDERS.find((o) => o.id === id) ?? SORT_ORDERS[0]!;
}

const cmpText = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);
const stamp = (s: string | null | undefined): string => s ?? '';

export type LabelName = (groupId: string) => string | undefined;

/**
 * Comparator for one sort order. Pinned sessions go first in the newest-first
 * orders and last in the oldest-first ones, like the TUI; among pinned ones the
 * latest pin leads.
 */
export function sessionComparator(order: SortOrderId, labelName: LabelName = () => undefined) {
  const newestFirst = order === 'freshest' || order === 'newest-created' || order === 'by-label';
  return (a: Session, b: Session): number => {
    const pa = a.pinned_at ?? null;
    const pb = b.pinned_at ?? null;
    if (pa && pb) return cmpText(pb, pa);
    if (pa) return newestFirst ? -1 : 1;
    if (pb) return newestFirst ? 1 : -1;
    switch (order) {
      case 'stalest': return cmpText(stamp(a.updated_at), stamp(b.updated_at));
      case 'freshest': return cmpText(stamp(b.updated_at), stamp(a.updated_at));
      case 'oldest-created': return cmpText(stamp(a.created_at), stamp(b.created_at));
      case 'newest-created': return cmpText(stamp(b.created_at), stamp(a.created_at));
      case 'by-label': {
        const ga = a.group_id ?? null;
        const gb = b.group_id ?? null;
        if (ga && gb && ga === gb) return cmpText(stamp(a.updated_at), stamp(b.updated_at));
        if (ga && !gb) return -1;
        if (!ga && gb) return 1;
        if (ga && gb) {
          const byName = cmpText((labelName(ga) ?? '').toLowerCase(), (labelName(gb) ?? '').toLowerCase());
          return byName !== 0 ? byName : cmpText(ga, gb);
        }
        return cmpText(stamp(a.updated_at), stamp(b.updated_at));
      }
    }
  };
}

/** Label grouping replaces the needs-you / in-flight sections. */
export function activityFirst(order: SortOrderId): boolean {
  return order !== 'by-label';
}

/** The scope header: `⇅ Longest idle · 20 sessions`. */
export function scopeHeader(order: SortOrderId, count: number): string {
  return `⇅ ${sortInfo(order).label} · ${count} session${count === 1 ? '' : 's'}`;
}

// ---------- label groups ----------

const labelKey = (r: TreeRow): string | null => r.session.group_id ?? null;

/**
 * `]g` / `[g`: the first row of the next (or previous) label group. Forward
 * stops at the end; backward lands on the first row of the previous group.
 */
export function labelBoundary(rows: readonly TreeRow[], cursor: string | null, dir: 1 | -1): string | null {
  const i = cursor === null ? -1 : rows.findIndex((r) => r.session.id === cursor);
  if (rows.length === 0) return null;
  if (i < 0) return dir > 0 ? rows[0]!.session.id : rows[rows.length - 1]!.session.id;
  const here = labelKey(rows[i]!);
  if (dir > 0) {
    for (let j = i + 1; j < rows.length; j++) if (labelKey(rows[j]!) !== here) return rows[j]!.session.id;
    return null;
  }
  let j = i - 1;
  // Leave the current run first (the cursor may sit mid-group), then walk to the start of the previous run.
  while (j >= 0 && labelKey(rows[j]!) === here) j--;
  if (j < 0) return null;
  const prev = labelKey(rows[j]!);
  while (j > 0 && labelKey(rows[j - 1]!) === prev) j--;
  return rows[j]!.session.id;
}

/** Sessions per label id (`''` = unlabeled), for the label picker. */
export function labelCounts(sessions: readonly Session[]): Map<string, number> {
  const out = new Map<string, number>();
  for (const s of sessions) out.set(s.group_id ?? '', (out.get(s.group_id ?? '') ?? 0) + 1);
  return out;
}

// ---------- folds ----------

export type FoldOp = 'open' | 'close' | 'toggle' | 'close-all' | 'open-all';

/** Row ids that have children (the foldable ones) in a forest-ordered row list. */
export function foldableIds(rows: readonly TreeRow[]): string[] {
  return rows.filter((r) => r.hasChildren).map((r) => r.session.id);
}

/**
 * New collapsed set after a fold command. `containerIds` is every session that
 * has children, including ones inside a collapsed subtree (zM must close those
 * too). open / close / toggle act on the cursor row only and report whether it
 * is foldable.
 */
export function applyFold(
  op: FoldOp,
  collapsed: ReadonlySet<string>,
  cursor: string | null,
  containerIds: readonly string[],
): { collapsed: Set<string>; applied: boolean } {
  if (op === 'close-all') return { collapsed: new Set(containerIds), applied: containerIds.length > 0 };
  if (op === 'open-all') return { collapsed: new Set(), applied: containerIds.length > 0 };
  if (cursor === null || !containerIds.includes(cursor)) return { collapsed: new Set(collapsed), applied: false };
  const next = new Set(collapsed);
  const isClosed = next.has(cursor);
  if (op === 'open' || (op === 'toggle' && isClosed)) next.delete(cursor);
  else next.add(cursor);
  return { collapsed: next, applied: true };
}

// ---------- hierarchy ----------

/** `-` / `H`: the cursor row's parent when it is in the list. */
export function ascendTarget(rows: readonly TreeRow[], cursor: string | null): string | null {
  const row = rows.find((r) => r.session.id === cursor);
  const parent = row?.session.parent_id;
  return parent && rows.some((r) => r.session.id === parent) ? parent : null;
}

export const CONTAINER_KINDS = new Set(['Group', 'Epic']);

export interface ParentChoice {
  /** Session id, or null for the top level. */
  id: string | null;
  label: string;
  current: boolean;
}

/**
 * `mp`: where a session can move. Containers other than the session and its
 * own descendants, plus the top level. The daemon still checks the containment
 * matrix (Group ⊇ Epic ⊇ Story/Task/Bug) and answers with its refusal.
 */
export function parentChoices(sessions: readonly Session[], sessionId: string): ParentChoice[] {
  const kids = new Map<string, string[]>();
  for (const s of sessions) {
    if (s.parent_id) kids.set(s.parent_id, [...(kids.get(s.parent_id) ?? []), s.id]);
  }
  const below = new Set<string>([sessionId]);
  const queue = [sessionId];
  while (queue.length > 0) {
    for (const c of kids.get(queue.shift()!) ?? []) {
      if (!below.has(c)) { below.add(c); queue.push(c); }
    }
  }
  const me = sessions.find((s) => s.id === sessionId);
  const out: ParentChoice[] = [{ id: null, label: '(top level)', current: !me?.parent_id }];
  for (const s of sessions) {
    if (s.session_kind && CONTAINER_KINDS.has(s.session_kind) && !below.has(s.id)) {
      out.push({ id: s.id, label: `${s.session_kind}: ${sessionLabel(s)}`, current: me?.parent_id === s.id });
    }
  }
  return out;
}

// ---------- attention queue ----------

/** Sessions that need the operator themselves, in list order: slot 1 is the first. */
export function attentionQueue(rows: readonly TreeRow[]): Session[] {
  return rows.filter((r) => needsAttention(r.session)).map((r) => r.session);
}

/** `<Space>N`: the Nth queue entry, 1-based. */
export function attentionSlot(queue: readonly Session[], n: number): Session | null {
  return Number.isInteger(n) && n >= 1 ? queue[n - 1] ?? null : null;
}

// ---------- search ----------

/** Indices of the texts that contain `needle` (case-insensitive); none for a blank needle. */
export function matchIndices(texts: readonly string[], needle: string): number[] {
  const n = needle.trim().toLowerCase();
  if (n === '') return [];
  const out: number[] = [];
  texts.forEach((t, i) => { if (t.toLowerCase().includes(n)) out.push(i); });
  return out;
}

/** `n` / `N`: the match after (or before) `from`, wrapping; `from` null starts at the ends. */
export function stepMatch(matches: readonly number[], from: number | null, dir: 1 | -1): number | null {
  if (matches.length === 0) return null;
  if (from === null) return dir > 0 ? matches[0]! : matches[matches.length - 1]!;
  if (dir > 0) return matches.find((m) => m > from) ?? matches[0]!;
  for (let i = matches.length - 1; i >= 0; i--) if (matches[i]! < from) return matches[i]!;
  return matches[matches.length - 1]!;
}

// ---------- projects and labels ----------

export interface ColorChoice { name: string; hex: string }

/** Same palettes and order as the TUI forms. */
export const PROJECT_COLORS: readonly ColorChoice[] = [
  { name: 'Blue', hex: '#89b4fa' }, { name: 'Green', hex: '#a6e3a1' }, { name: 'Peach', hex: '#fab387' },
  { name: 'Mauve', hex: '#cba6f7' }, { name: 'Red', hex: '#f38ba8' }, { name: 'Yellow', hex: '#f9e2af' },
  { name: 'Teal', hex: '#94e2d5' }, { name: 'Pink', hex: '#f5c2e7' },
];
export const LABEL_COLORS: readonly ColorChoice[] = [
  { name: 'Mauve', hex: '#cba6f7' }, { name: 'Green', hex: '#a6e3a1' }, { name: 'Peach', hex: '#fab387' },
  { name: 'Blue', hex: '#89b4fa' }, { name: 'Red', hex: '#f38ba8' }, { name: 'Yellow', hex: '#f9e2af' },
  { name: 'Teal', hex: '#94e2d5' }, { name: 'Pink', hex: '#f5c2e7' },
];

/** Left / Right in the color field: the next palette entry, wrapping. */
export function cycleColor(palette: readonly ColorChoice[], hex: string, dir: 1 | -1): string {
  const i = palette.findIndex((c) => c.hex === hex.toLowerCase());
  const at = i < 0 ? 0 : (i + dir + palette.length) % palette.length;
  return palette[at]!.hex;
}

export interface ProjectDraft { name: string; path: string; color: string }
export interface LabelDraft { name: string; description: string; color: string }

const HEX = /^#[0-9a-fA-F]{6}$/;
/** A folder path an operator can paste: absolute, home-relative or a drive / UNC path. */
const FULL_PATH = /^(\/|~(\/|\\|$)|[A-Za-z]:[\\/]|\\\\)/;

/** Null when the draft can be saved. The daemon owns uniqueness. */
export function validateProject(d: ProjectDraft, existing: readonly string[] = []): string | null {
  const name = d.name.trim();
  const path = d.path.trim();
  if (name === '') return 'Name is required: give the project a short name, for example the name of its folder.';
  if (/[\u0000-\u001f]/.test(d.name + d.path)) return 'The name and folder cannot contain control characters such as line breaks or tabs.';
  if (existing.some((n) => n.trim().toLowerCase() === name.toLowerCase())) return `A project named “${name}” already exists: pick another name, or edit that one.`;
  if (path !== '' && !FULL_PATH.test(path)) return 'The folder must be a full path such as /Users/you/code/app (starting with / or ~/, or a drive letter such as C:\\).';
  if (!HEX.test(d.color)) return 'Pick a color for the project.';
  return null;
}

export function validateLabel(d: LabelDraft): string | null {
  if (d.name.trim() === '') return 'Name is required';
  if (/[\u0000-\u001f]/.test(d.name + d.description)) return 'Name and description cannot contain control characters';
  if (!HEX.test(d.color)) return 'Pick a color';
  return null;
}

/** `:project-new <name> [path]`: the first word is the name, the rest is the path. */
export function parseProjectArg(arg: string | undefined): { name: string; path: string } {
  const t = (arg ?? '').trim();
  const at = t.search(/\s/);
  return at < 0 ? { name: t, path: '' } : { name: t.slice(0, at), path: t.slice(at).trim() };
}

export type ProjectLookup = { project: Project } | { error: string };

/** `:project <name>`: exact name, then a unique prefix, then a unique substring (case-insensitive). */
export function findProject(projects: readonly Project[], query: string): ProjectLookup {
  const q = query.trim().toLowerCase();
  if (q === '') return { error: 'Name a project' };
  const exact = projects.filter((p) => p.name.toLowerCase() === q);
  if (exact.length === 1) return { project: exact[0]! };
  const tiers = [
    projects.filter((p) => p.name.toLowerCase().startsWith(q)),
    projects.filter((p) => p.name.toLowerCase().includes(q)),
  ];
  for (const hits of tiers) {
    if (hits.length === 1) return { project: hits[0]! };
    if (hits.length > 1) return { error: `“${query.trim()}” matches ${hits.length} projects: ${hits.map((p) => p.name).join(', ')}` };
  }
  return { error: `No project named “${query.trim()}”` };
}

/** Type-to-filter in a picker: case-insensitive substring of the name, order kept. */
export function filterByName<T>(items: readonly T[], text: string, name: (t: T) => string): T[] {
  const q = text.trim().toLowerCase();
  return q === '' ? [...items] : items.filter((t) => name(t).toLowerCase().includes(q));
}

// ---------- session info (F3) ----------

const shortId = (id: string): string => id.slice(0, 8);

function local(iso: string): string {
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) return iso;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${t.getFullYear()}-${p(t.getMonth() + 1)}-${p(t.getDate())} ${p(t.getHours())}:${p(t.getMinutes())}:${p(t.getSeconds())}`;
}

/** The TUI's session-info rows, in its order. */
export function sessionInfoRows(
  s: Session, projects: readonly Project[], labels: readonly SessionLabel[],
): Array<[string, string]> {
  const rows: Array<[string, string]> = [
    ['title', s.title?.trim() || '(untitled)'],
    ['id', s.id],
    ['kind', s.session_kind ?? 'Standard'],
    ['status', s.status],
    ['provider', s.provider],
    ['model', s.model ?? '(none)'],
    ['dir', s.working_dir],
    ['project', projects.find((p) => p.id === s.project_id)?.name ?? 'none'],
    ['created', local(s.created_at)],
    // The context-budget rows after `created`, as the TUI's F3 panel (G2).
    ...contextRows(s as ContextFields),
  ];
  if (s.parent_id) rows.push(['parent', shortId(s.parent_id)]);
  if (s.session_kind && CONTAINER_KINDS.has(s.session_kind) && s.lead_session_id) rows.push(['lead', shortId(s.lead_session_id)]);
  if (s.sandbox_root) rows.push(['sandbox', s.sandbox_root]);
  rows.push(['rating', s.rating != null ? `${s.rating}/10` : 'unrated']);
  rows.push(['label', labels.find((l) => l.id === s.group_id)?.name ?? 'none']);
  rows.push(['tags', s.tags && s.tags.length > 0 ? s.tags.join(', ') : 'none']);
  if (s.description) rows.push(['description', s.description]);
  return rows;
}

/**
 * The new session list with each session that did not change since `prev` replaced by its previous object, so
 * rows built from it can be reused (local #275). A session that did change (or is new) keeps its fresh object.
 * Equal means the same JSON: a field the daemon adds or reorders only costs a rebuild.
 */
export function reuseUnchanged<T extends { id: string }>(prev: readonly T[], next: readonly T[]): T[] {
  if (prev.length === 0) return next.slice();
  const old = new Map<string, { s: T; json?: string }>();
  for (const s of prev) old.set(s.id, { s });
  return next.map((s) => {
    const o = old.get(s.id);
    if (!o) return s;
    if (o.s === s) return s;
    o.json ??= JSON.stringify(o.s);
    return JSON.stringify(s) === o.json ? o.s : s;
  });
}
