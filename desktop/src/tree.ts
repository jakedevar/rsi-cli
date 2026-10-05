// Pure session-hierarchy builder (no DOM), unit-tested under tests/.
// Hierarchy comes from `parent_id`; Groups and Epics are containers (see
// `rsi_common::is_leaf_kind`). Filters apply to leaves; ancestors of a kept
// node stay visible. Sessions whose parent is not in the list are roots.
import { needsAttention } from './approvals-model.js';
import type { Session } from './api.js';
import { ACTIVE_STATUSES, HIDDEN_BY_DEFAULT, isActive, sessionLabel } from './model.js';

export const CONTAINER_KINDS = new Set(['Group', 'Epic']);

export function isContainer(s: Pick<Session, 'session_kind'>): boolean {
  return !!s.session_kind && CONTAINER_KINDS.has(s.session_kind);
}

export interface TreeFilter {
  projectId: string | null;
  text: string;
  showFinished: boolean;
  /** `archived` lists archived sessions (status Archived) with no finished filter. */
  mode?: 'active' | 'archived';
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

function passesLeaf(s: Session, f: TreeFilter): boolean {
  const archived = f.mode === 'archived';
  if (!archived && HIDDEN_BY_DEFAULT.has(s.status)) return false;
  if (!archived && !f.showFinished && !isActive(s) && s.status !== 'Interrupted' && s.status !== 'Failed') {
    return false;
  }
  if (f.projectId && s.project_id !== f.projectId) return false;
  const needle = f.text.trim().toLowerCase();
  if (!needle) return true;
  return sessionLabel(s).toLowerCase().includes(needle) ||
    s.id.startsWith(needle) ||
    s.provider.toLowerCase().includes(needle);
}

function byActivity(a: TreeNode, b: TreeNode): number {
  const need = Number(b.anyAttention) - Number(a.anyAttention);
  if (need !== 0) return need;
  const act = Number(b.anyActive) - Number(a.anyActive);
  if (act !== 0) return act;
  return b.session.updated_at.localeCompare(a.session.updated_at);
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
    children.sort(byActivity);
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
  out.sort(byActivity);
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
