// Sidebar session tree rendering. Daemon strings are only ever set through
// textContent. Collapsed state is kept in memory per session id.
import type { Session, SessionLabel } from './api.js';
import { needsAttention } from './approvals-model.js';
import { relativeTime, sessionLabel } from './model.js';
import { type OpsSession, rowMarks } from './sessionops-model.js';
import { toggleCollapsed, type TreeRow } from './tree.js';

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
}

function span(cls: string, text?: string): HTMLSpanElement {
  const e = document.createElement('span');
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function rowNode(r: TreeRow, ctx: SessionListContext, now: number): HTMLLIElement {
  const s = r.session;
  const li = document.createElement('li');
  li.className = `session-item st-${r.aggStatus}${s.id === ctx.selectedId ? ' selected' : ''}${s.id === ctx.cursorId ? ' cursor' : ''}${r.hasChildren ? ' container' : ''}${r.anyAttention ? ' attn' : ''}`;
  li.dataset.id = s.id;
  li.style.paddingLeft = `${12 + r.depth * 14}px`;
  li.title = `${s.status} · ${s.provider}${s.model ? ` · ${s.model}` : ''}\n${s.id}`;

  const twisty = span('twisty', r.hasChildren ? (r.expanded ? '▾' : '▸') : '');
  if (r.hasChildren) {
    twisty.setAttribute('role', 'button');
    twisty.setAttribute('aria-label', r.expanded ? 'Collapse' : 'Expand');
    twisty.addEventListener('click', (ev) => {
      ev.stopPropagation();
      collapsed = toggleCollapsed(collapsed, s.id);
      ctx.onToggle();
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
  const sub = [r.aggStatus, s.provider, ctx.projectName(s.project_id), s.short_summary]
    .filter(Boolean).join(' · ');
  li.append(span('sub', sub));
  const label = ctx.labelOf?.(s);
  if (label) {
    const chip = span('label-chip', label.name);
    chip.style.setProperty('--chip', label.color);
    li.append(chip);
  }
  if (r.anyAttention) {
    const own = needsAttention(s);
    li.append(span('attn-badge', own ? (s.pending_question ? 'Question' : 'Needs you') : 'Child needs you'));
  }
  return li;
}

/** Replace the list contents with the tree rows (or an empty-state row). */
export function renderSessionTree(list: HTMLElement, rows: TreeRow[], ctx: SessionListContext): void {
  const now = ctx.now ?? Date.now();
  if (rows.length === 0) {
    const li = document.createElement('li');
    li.className = 'session-empty';
    li.textContent = 'No sessions match.';
    list.replaceChildren(li);
    return;
  }
  const nodes: HTMLLIElement[] = [];
  let group: string | null | undefined;
  for (const r of rows) {
    if (ctx.groupHeaders && r.depth === 0) {
      const g = r.session.group_id ?? null;
      if (g !== group) {
        group = g;
        const head = document.createElement('li');
        head.className = 'label-head';
        head.textContent = (g && ctx.labelOf?.(r.session)?.name) || 'No label';
        nodes.push(head);
      }
    }
    nodes.push(rowNode(r, ctx, now));
  }
  list.replaceChildren(...nodes);
}
