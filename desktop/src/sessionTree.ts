// Sidebar session tree rendering. Daemon strings are only ever set through
// textContent. Collapsed state is kept in memory per session id.
import type { Session, SessionLabel } from './api.js';
import { PENDING_ARCHIVE_NOTE, archivePending, needsAttention } from './approvals-model.js';
import { el } from './dom.js';
import { relativeTime, sessionLabel, statusLabel } from './model.js';
import { type OpsSession, rowMarks } from './sessionops-model.js';
import { rowAccessibleName, rowDomId } from './sessionlist-keys-model.js';
import { toggleCollapsed, type TreeRow } from './tree.js';
import { onColor, parseHex } from './theme-model.js';

let collapsed: ReadonlySet<string> = new Set();

export function collapsedIds(): ReadonlySet<string> {
  return collapsed;
}

/** Replace the collapsed set (fold commands zo / zc / za / zM / zR). */
export function setCollapsed(next: ReadonlySet<string>): void {
  collapsed = new Set(next);
}

export interface SessionListContext {
  selectedId: string | null;
  /** The vim-keys cursor row, when it differs from the open session. */
  cursorId?: string | null;
  projectName: (id: string | null | undefined) => string | undefined;
  /** Called after a collapse toggle so the caller can re-render. */
  onToggle: () => void;
  now?: number;
  /** The session's label, for its chip. */
  labelOf?: (s: Session) => SessionLabel | undefined;
  /** "Grouped by label": a header row before each label's first top-level row. */
  groupHeaders?: boolean;
  /** The empty row's text (model.ts emptyListView). */
  emptyText?: string;
  /** Buttons under the empty text (Show completed, Show archived, Clear filter, All projects). */
  emptyActions?: readonly { id: string; label: string }[];
  onEmptyAction?: (id: string) => void;
  /** Every session id the list could show (filtered out ones too): their built rows stay cached for the next filter change. */
  knownIds?: ReadonlySet<string>;
}

function span(cls: string, text?: string): HTMLSpanElement {
  const e = document.createElement('span');
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

/** The context the live twisty handlers call, so a reused row never calls a stale render callback. */
let liveCtx: SessionListContext | null = null;

function rowNode(r: TreeRow, ctx: SessionListContext, now: number, active: string | null): HTMLLIElement {
  const s = r.session;
  const li = document.createElement('li');
  // An ARIA tree row (local #226): the list is one tab stop, this row its active descendant when the cursor is here.
  li.id = rowDomId(s.id);
  li.setAttribute('role', 'treeitem');
  li.setAttribute('aria-level', String(r.depth + 1));
  li.setAttribute('aria-selected', String(s.id === active));
  if (s.id === ctx.selectedId) li.setAttribute('aria-current', 'true');
  if (r.hasChildren) li.setAttribute('aria-expanded', String(r.expanded));
  li.className = `session-item st-${r.aggStatus}${s.id === ctx.selectedId ? ' selected' : ''}${s.id === ctx.cursorId ? ' cursor' : ''}${r.hasChildren ? ' container' : ''}${r.anyAttention ? ' attn' : ''}`;
  li.dataset.id = s.id;
  li.style.paddingLeft = `${12 + r.depth * 14}px`;
  li.title = `${statusLabel(s.status)} · ${s.provider}${s.model ? ` · ${s.model}` : ''}\n${s.id}`;

  const twisty = span('twisty', r.hasChildren ? (r.expanded ? '▾' : '▸') : '');
  if (r.hasChildren) {
    twisty.setAttribute('role', 'button');
    twisty.setAttribute('aria-label', r.expanded ? 'Collapse' : 'Expand');
    twisty.addEventListener('click', (ev) => {
      ev.stopPropagation();
      collapsed = toggleCollapsed(collapsed, s.id);
      (liveCtx ?? ctx).onToggle();
    });
  }
  const title = span('label', sessionLabel(s));
  // Pin, testing-needed, rotation and retry markers (G2, sessionops-model.ts) lead the title.
  title.prepend(...rowMarks(s as OpsSession).map((m) => {
    const mark = span(`row-mark ${m.cls}`, m.glyph);
    mark.title = m.title;
    return mark;
  }));
  li.append(twisty, span('dot'), title);
  if (r.hasChildren) {
    const badge = span('count', String(r.childCount));
    badge.title = `${r.childCount} child${r.childCount === 1 ? '' : 'ren'}, ${r.descendantCount} total${r.anyActive ? ' (some running)' : ''}`;
    li.append(badge);
  }
  li.append(span('age', relativeTime(s.updated_at, now)));
  const sub = [statusLabel(r.aggStatus), archivePending(s) ? PENDING_ARCHIVE_NOTE.toLowerCase() : null, s.provider, ctx.projectName(s.project_id), s.short_summary]
    .filter(Boolean).join(' · ');
  li.append(span('sub', sub));
  const label = ctx.labelOf?.(s);
  if (label) {
    const chip = span('label-chip', label.name);
    chip.style.setProperty('--chip', label.color);
    // A label's own colour: readable text on it, whatever the theme (4.5:1).
    const fill = parseHex(label.color);
    if (fill) chip.style.color = onColor(fill, '#000000', '#ffffff');
    li.append(chip);
  }
  let attention: string | null = null;
  if (r.anyAttention) {
    const own = needsAttention(s);
    attention = own ? (s.pending_question ? 'Question' : 'Needs you') : 'Child needs you';
    li.append(span('attn-badge', attention));
  }
  li.setAttribute('aria-label', rowAccessibleName(sessionLabel(s), r.aggStatus, [attention, archivePending(s) ? PENDING_ARCHIVE_NOTE.toLowerCase() : null].filter(Boolean).join(', ') || null));
  return li;
}

/** A built row, kept so a later render with the same row content reuses the node (local #275). */
interface CachedRow {
  li: HTMLLIElement;
  /** The session object the row was built from: a refreshed list brings new objects, so its rows rebuild. */
  session: Session;
  /** Everything else the row shows, as one string. */
  sig: string;
  /** Child count at build: a row something else decorated in place (the stop badge) rebuilds. */
  kids: number;
}

const rowCaches = new WeakMap<HTMLElement, Map<string, CachedRow>>();

/** Every row input besides the session object: tree position, selection, cursor, age, project and label. */
function rowSig(r: TreeRow, ctx: SessionListContext, now: number, active: string | null): string {
  const s = r.session;
  const label = ctx.labelOf?.(s);
  return [
    r.depth, r.hasChildren ? 1 : 0, r.expanded ? 1 : 0, r.childCount, r.descendantCount, r.aggStatus,
    r.anyActive ? 1 : 0, r.anyAttention ? 1 : 0,
    s.id === ctx.selectedId ? 1 : 0, s.id === ctx.cursorId ? 1 : 0, s.id === active ? 1 : 0,
    relativeTime(s.updated_at, now), ctx.projectName(s.project_id) ?? '', label?.name ?? '', label?.color ?? '',
  ].join('\u0001');
}

/** The row's node: the cached one when nothing it shows changed, else a fresh build. */
function cachedRowNode(cache: Map<string, CachedRow>, r: TreeRow, ctx: SessionListContext, now: number, active: string | null): HTMLLIElement {
  const sig = rowSig(r, ctx, now, active);
  const hit = cache.get(r.session.id);
  if (hit && hit.session === r.session && hit.sig === sig && hit.li.children.length === hit.kids) return hit.li;
  const li = rowNode(r, ctx, now, active);
  cache.set(r.session.id, { li, session: r.session, sig, kids: li.children.length });
  return li;
}

/** Drop cached rows of sessions that are gone (the caller's full list), or that are not shown when it gave none. */
function pruneCache(cache: Map<string, CachedRow>, rows: readonly TreeRow[], ctx: SessionListContext): void {
  if (cache.size <= rows.length) return;
  const shown = new Set(rows.map((r) => r.session.id));
  for (const id of cache.keys()) if (!shown.has(id) && !ctx.knownIds?.has(id)) cache.delete(id);
}

/**
 * Make `list` hold exactly `nodes`, in order, touching only what differs: nodes that left are removed first,
 * then each wanted node is inserted only where it is not already. A keystroke that narrows 3,000 rows to 270
 * removes 2,730 nodes and moves none.
 */
function reconcile(list: HTMLElement, nodes: readonly Node[]): void {
  const want = new Set<Node>(nodes);
  for (let n = list.firstChild; n;) {
    const next: ChildNode | null = n.nextSibling;
    if (!want.has(n)) list.removeChild(n);
    n = next;
  }
  let cur: ChildNode | null = list.firstChild;
  for (const node of nodes) {
    if (node === cur) { cur = cur.nextSibling; continue; }
    list.insertBefore(node, cur);
  }
}

/** Replace the list contents with the tree rows (or an empty-state row). */
export function renderSessionTree(list: HTMLElement, rows: TreeRow[], ctx: SessionListContext): void {
  const now = ctx.now ?? Date.now();
  // One tab stop for the whole list (local #226); sessionlist-keys.ts handles its keys.
  list.setAttribute('role', 'tree');
  list.tabIndex = 0;
  const activeId = ctx.cursorId ?? ctx.selectedId;
  const active = activeId !== null && rows.some((r) => r.session.id === activeId) ? activeId : null;
  if (active) list.setAttribute('aria-activedescendant', rowDomId(active));
  else list.removeAttribute('aria-activedescendant');
  if (rows.length === 0) {
    const li = document.createElement('li');
    li.className = 'session-empty';
    li.setAttribute('role', 'none');
    li.append(span('session-empty-text', ctx.emptyText ?? 'No sessions match.'));
    if (ctx.emptyActions?.length) {
      const bar = document.createElement('div');
      bar.className = 'session-empty-actions';
      for (const a of ctx.emptyActions) {
        const b = el('button', 'btn', a.label);
        b.type = 'button';
        b.dataset.empty = a.id;
        b.addEventListener('click', () => ctx.onEmptyAction?.(a.id));
        bar.append(b);
      }
      li.append(bar);
    }
    const cache = rowCaches.get(list);
    if (cache) pruneCache(cache, rows, ctx);
    list.replaceChildren(li);
    return;
  }
  liveCtx = ctx;
  let cache = rowCaches.get(list);
  if (!cache) { cache = new Map(); rowCaches.set(list, cache); }
  const nodes: HTMLLIElement[] = [];
  let group: string | null | undefined;
  for (const r of rows) {
    if (ctx.groupHeaders && r.depth === 0) {
      const g = r.session.group_id ?? null;
      if (g !== group) {
        group = g;
        const head = document.createElement('li');
        head.className = 'label-head';
        head.setAttribute('role', 'none');
        head.textContent = (g && ctx.labelOf?.(r.session)?.name) || 'No label';
        nodes.push(head);
      }
    }
    nodes.push(cachedRowNode(cache, r, ctx, now, active));
  }
  pruneCache(cache, rows, ctx);
  reconcile(list, nodes);
}
