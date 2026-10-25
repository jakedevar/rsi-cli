// The session list's standard keys (local #226), DOM side. `#session-list` is
// one tab stop (role tree, tabindex 0, aria-activedescendant on the cursor row,
// set by sessionTree.ts). While it has focus the keys in sessionlist-keys-model.ts
// move the cursor, fold and open, with Vim keys on or off; ArrowDown in the
// Filter field moves into the list. The Vim letters keep their own bindings
// (vimkeys.ts); a half-typed chord or `:` line keeps its next key.
import { $ } from './dom.js';
import { renderSessionList, select, visibleRows } from './sessions.js';
import { collapsedIds, setCollapsed } from './sessionTree.js';
import { activeRow, isListKey, listKey, rowDomId } from './sessionlist-keys-model.js';
import { state } from './state.js';
import { vimKeysBusy } from './vimkeys.js';

const list = (): HTMLElement => $('session-list');

/** Whether the focused session list owns this key (the global Left/Right step yields to it). */
export function sessionListOwnsKey(ev: KeyboardEvent, active: Element | null): boolean {
  return active !== null && active === document.getElementById('session-list') && isListKey(ev) && !vimKeysBusy();
}

/** The rows that fit in the list, for PageUp/PageDown. */
function pageRows(ul: HTMLElement): number {
  const row = ul.querySelector<HTMLElement>('.session-item');
  const h = row?.offsetHeight ?? 0;
  return h > 0 ? Math.max(1, Math.floor(ul.clientHeight / h) - 1) : 10;
}

function reveal(id: string): void {
  document.getElementById(rowDomId(id))?.scrollIntoView?.({ block: 'nearest' });
}

function moveTo(id: string): void {
  state.cursorId = id;
  renderSessionList();
  reveal(id);
}

function onKey(ev: KeyboardEvent): void {
  const ul = list();
  if (ev.defaultPrevented || ev.isComposing || ev.target !== ul || !sessionListOwnsKey(ev, document.activeElement)) return;
  const o = listKey(ev, visibleRows(), state.cursorId, state.selectedId, pageRows(ul));
  if (o.kind === 'pass') return;
  ev.preventDefault();
  ev.stopPropagation();
  switch (o.kind) {
    case 'move': moveTo(o.id); break;
    case 'open': select(o.id); reveal(o.id); break;
    case 'fold': {
      const next = new Set(collapsedIds());
      if (o.collapse) next.add(o.id); else next.delete(o.id);
      setCollapsed(next);
      state.cursorId = o.id;
      renderSessionList();
      reveal(o.id);
      break;
    }
    default: break;
  }
}

/**
 * Tab into the list lands on the open session, else the first row: put the cursor
 * there so it shows. The rows are marked in place, not re-rendered: a mouse press
 * focuses the list too, and replacing the row under it would lose its click.
 */
function onFocus(): void {
  const rows = visibleRows();
  if (rows.length === 0 || activeRow(rows, state.cursorId, state.selectedId)) return;
  const id = rows[0]!.session.id;
  state.cursorId = id;
  const ul = list();
  for (const li of ul.querySelectorAll<HTMLElement>('.session-item')) {
    const on = li.dataset.id === id;
    li.classList.toggle('cursor', on);
    li.setAttribute('aria-selected', String(on));
  }
  ul.setAttribute('aria-activedescendant', rowDomId(id));
}

/** ArrowDown in the Filter field moves into the list (Enter there keeps its meaning). */
function onSearchKey(ev: KeyboardEvent): void {
  if (ev.defaultPrevented || ev.key !== 'ArrowDown' || ev.ctrlKey || ev.metaKey || ev.altKey || ev.shiftKey) return;
  ev.preventDefault();
  list().focus();
}

export function initSessionListKeys(): void {
  const ul = list();
  ul.setAttribute('role', 'tree');
  ul.tabIndex = 0;
  ul.addEventListener('keydown', onKey);
  ul.addEventListener('focus', onFocus);
  $('search').addEventListener('keydown', onSearchKey);
}
