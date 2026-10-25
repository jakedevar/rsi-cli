// Pure session-hierarchy builder (no DOM), unit-tested under tests/.
// Hierarchy comes from `parent_id`; Groups and Epics are containers (see
// `rsi_common::is_leaf_kind`). Filters apply to leaves; ancestors of a kept
// node stay visible. Sessions whose parent is not in the list are roots.
import { needsAttention } from './approvals-model.js';
import type { Session } from './api.js';
import { ACTIVE_STATUSES, HIDDEN_BY_DEFAULT, isActive, matchesFilterText, passesFinished, type HiddenMatches } from './model.js';

export const CONTAINER_KINDS = new Set(['Group', 'Epic']);

export function isContainer(s: Pick<Session, 'session_kind'>): boolean {
  return !!s.session_kind && CONTAINER_KINDS.has(s.session_kind);
}

export interface TreeFilter {
  projectId: string | null;
  text: string;
  showFinished: boolean;
  /** The open session: never hidden by the finished filter (it stays listed when it finishes). */
  keepId?: string | null;
  /** `archived` lists archived sessions (status Archived) with no finished filter. */
  mode?: 'active' | 'archived';
  /**
   * The Active view's zone (G2, app/sessions.rs): `main` leaves out the root
   * sessions scheduled jobs launched, `jobs` lists only them, every status
   * shown. Omitted: no zone filter.
   */
  zone?: 'main' | 'jobs';
  /** Sort order (slice 4). Omitted: needs-you, then active, then newest update. */
  order?: { cmp: (a: Session, b: Session) => number; activityFirst: boolean };
}

export interface TreeNode {
  session: Session;
  children: TreeNode[];
  /** True when this session or any descendant is active. */
  anyActive: boolean;
  /** True when this session or any descendant needs the operator. */
  anyAttention: boolean;
}

export interface TreeRow {
  session: Session;
  depth: number;
  hasChildren: boolean;
  expanded: boolean;
  /** Direct (visible) children. */
  childCount: number;
  /** Descendants (visible), all levels. */
  descendantCount: number;
  /** Status to show: Running when any descendant is active, else own status. */
  aggStatus: string;
  anyActive: boolean;
  anyAttention: boolean;
}

/** A root session a scheduled job launched (the Jobs zone, G2). */
function isJob(s: Session): boolean {
  return !!s.scheduled_job_id && !s.parent_id;
}

function inZone(s: Session, f: Pick<TreeFilter, 'zone'>): boolean {
  return f.zone === 'jobs' ? isJob(s) : f.zone === 'main' ? !isJob(s) : true;
}

function passesLeaf(s: Session, f: TreeFilter): boolean {
  const archived = f.mode === 'archived';
  if (!archived && !inZone(s, f)) return false;
  if (!archived && HIDDEN_BY_DEFAULT.has(s.status)) return false;
  if (!archived && f.zone !== 'jobs' && !passesFinished(s, f.showFinished, f.keepId)) return false;
  if (f.projectId && s.project_id !== f.projectId) return false;
  return matchesFilterText(s, f.text.trim().toLowerCase());
}

/**
 * How many sessions the current filter text matches that the other filters hide, so an empty list can
 * say so and offer them: finished ones (Show completed off), archived ones, and other projects'.
 * `archived` is the Archived tab's list; either list may repeat a session.
 */
export function countHiddenMatches(active: Session[], archived: Session[], f: TreeFilter): HiddenMatches {
  const needle = f.text.trim().toLowerCase();
  const out: HiddenMatches = { finished: 0, archived: 0, otherProjects: 0 };
  const seenArchived = new Set<string>();
  const countArchived = (s: Session): void => {
    // The Archived tab lists every zone together, so Jobs offers no archived hint.
    if (seenArchived.has(s.id) || f.zone === 'jobs' || !matchesFilterText(s, needle)) return;
    seenArchived.add(s.id);
    if (!f.projectId || s.project_id === f.projectId) out.archived += 1;
  };
  for (const s of active) {
    if (s.status === 'Archived') { countArchived(s); continue; }
    if (HIDDEN_BY_DEFAULT.has(s.status) || !inZone(s, f) || !matchesFilterText(s, needle)) continue;
    if (f.zone !== 'jobs' && !passesFinished(s, f.showFinished, f.keepId)) {
      if (!f.projectId || s.project_id === f.projectId) out.finished += 1;
    } else if (f.projectId && s.project_id !== f.projectId) {
      out.otherProjects += 1;
    }
  }
  for (const s of archived) if (s.status === 'Archived') countArchived(s);
  return out;
}

/** Sessions this tab could list with no filter on: the empty text's "yet" versus "match". */
export function countZoneSessions(all: Session[], f: Pick<TreeFilter, 'zone' | 'mode'>): number {
  if (f.mode === 'archived') return all.length;
  return all.filter((s) => !HIDDEN_BY_DEFAULT.has(s.status) && inZone(s, f)).length;
}

function byActivity(a: TreeNode, b: TreeNode): number {
  const need = Number(b.anyAttention) - Number(a.anyAttention);
  if (need !== 0) return need;
  const act = Number(b.anyActive) - Number(a.anyActive);
  if (act !== 0) return act;
  return b.session.updated_at.localeCompare(a.session.updated_at);
}

/** Needs-you and in-flight sections first (unless label grouping), then the chosen order. */
function nodeOrder(order: TreeFilter['order']): (a: TreeNode, b: TreeNode) => number {
  if (!order) return byActivity;
  return (a, b) => {
    if (order.activityFirst) {
      const need = Number(b.anyAttention) - Number(a.anyAttention);
      if (need !== 0) return need;
      const act = Number(b.anyActive) - Number(a.anyActive);
      if (act !== 0) return act;
    }
    return order.cmp(a.session, b.session);
  };
}

/**
 * Build the visible forest. A node is kept when it passes the filter as a leaf
 * (a childless container passes only through the same filter), or when any
 * descendant is kept. Parent cycles are broken so every kept session shows once.
 */
export function buildTree(all: Session[], f: TreeFilter): TreeNode[] {
  const byId = new Map<string, Session>();
  for (const s of all) byId.set(s.id, s);
  const kids = new Map<string, Session[]>();
  const roots: Session[] = [];
  for (const s of byId.values()) {
    const p = s.parent_id;
    if (p && p !== s.id && byId.has(p)) {
      const list = kids.get(p) ?? [];
      list.push(s);
      kids.set(p, list);
    } else {
      roots.push(s);
    }
  }

  const byOrder = nodeOrder(f.order);
  const visited = new Set<string>();
  const build = (s: Session): TreeNode | null => {
    visited.add(s.id);
    const children: TreeNode[] = [];
    for (const c of kids.get(s.id) ?? []) {
      if (visited.has(c.id)) continue;
      const n = build(c);
      if (n) children.push(n);
    }
    // A container is only shown for its descendants, unless it is empty and passes.
    const keep = children.length > 0 || passesLeaf(s, f);
    if (!keep) return null;
    children.sort(byOrder);
    const anyActive = isActive(s) || children.some((c) => c.anyActive);
    const anyAttention = needsAttention(s) || children.some((c) => c.anyAttention);
    return { session: s, children, anyActive, anyAttention };
  };

  const out: TreeNode[] = [];
  for (const r of roots) {
    const n = build(r);
    if (n) out.push(n);
  }
  // Parent cycles leave unreachable sessions; surface them as roots.
  for (const s of byId.values()) {
    if (visited.has(s.id)) continue;
    const n = build(s);
    if (n) out.push(n);
  }
  out.sort(byOrder);
  return out;
}

function countDescendants(n: TreeNode): number {
  return n.children.reduce((acc, c) => acc + 1 + countDescendants(c), 0);
}

/**
 * Flatten to display rows. Collapsed ids hide their subtree; an active text
 * filter ignores collapse so matches are never hidden.
 */
export function flattenTree(
  forest: TreeNode[],
  collapsed: ReadonlySet<string>,
  opts: { forceExpand?: boolean } = {},
): TreeRow[] {
  const rows: TreeRow[] = [];
  const walk = (n: TreeNode, depth: number): void => {
    const hasChildren = n.children.length > 0;
    const expanded = hasChildren && (opts.forceExpand || !collapsed.has(n.session.id));
    rows.push({
      session: n.session,
      depth,
      hasChildren,
      expanded,
      childCount: n.children.length,
      descendantCount: countDescendants(n),
      aggStatus: n.anyActive && !ACTIVE_STATUSES.has(n.session.status) ? 'Running' : n.session.status,
      anyActive: n.anyActive,
      anyAttention: n.anyAttention,
    });
    if (expanded) for (const c of n.children) walk(c, depth + 1);
  };
  for (const n of forest) walk(n, 0);
  return rows;
}

/** Toggle `id` in the collapsed set, returning the new set. */
export function toggleCollapsed(collapsed: ReadonlySet<string>, id: string): Set<string> {
  const next = new Set(collapsed);
  if (!next.delete(id)) next.add(id);
  return next;
}
