// The session list's standard keys (local #226), pure side. The list is one
// tab stop (an ARIA tree with aria-activedescendant on the cursor row); while
// it has focus, Up/Down, Home/End and PageUp/PageDown move the cursor,
// Left/Right fold and unfold a container row (or step to its parent / first
// child, the WAI-ARIA tree pattern) and Enter opens the session. These are
// standard keys, so they work with the Vim keys switch off or on; the Vim
// letters (j/k/gg/G/h/l) keep their own bindings. No DOM; unit-tested under
// tests/sessionlist-keys.test.mjs. The DOM side is sessionlist-keys.ts.
import { statusLabel } from './model.js';
import type { TreeRow } from './tree.js';

export interface ListKeyEvent {
  readonly key: string;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly altKey?: boolean;
  readonly shiftKey?: boolean;
}

export type ListKeyOutcome =
  /** Not a list key: leave it to the other handlers. */
  | { kind: 'pass' }
  /** Move the cursor to this row. */
  | { kind: 'move'; id: string }
  /** Open the session under the cursor. */
  | { kind: 'open'; id: string }
  /** Fold (collapse) or unfold (expand) this container row. */
  | { kind: 'fold'; id: string; collapse: boolean }
  /** A list key with nothing to do (at an edge, empty list): swallow it. */
  | { kind: 'none' };

/** The keys the focused list owns. */
export const LIST_KEYS: ReadonlySet<string> = new Set([
  'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End', 'PageUp', 'PageDown', 'Enter',
]);

/** Whether the focused list takes this key (no modifier: Shift/Ctrl chords keep their global meaning). */
export function isListKey(ev: ListKeyEvent): boolean {
  return LIST_KEYS.has(ev.key) && !ev.ctrlKey && !ev.metaKey && !ev.altKey && !ev.shiftKey;
}

/** The row the keys act on: the cursor, else the open session, when it is shown. */
export function activeRow(rows: readonly TreeRow[], cursorId: string | null, selectedId: string | null): TreeRow | undefined {
  const at = cursorId ?? selectedId;
  return at === null ? undefined : rows.find((r) => r.session.id === at);
}

const idAt = (rows: readonly TreeRow[], i: number): string | null =>
  rows[Math.max(0, Math.min(rows.length - 1, i))]?.session.id ?? null;

/**
 * What a key does on the focused list. `page` is how many rows PageUp/PageDown
 * move (the rows that fit in the list, at least one).
 */
export function listKey(
  ev: ListKeyEvent, rows: readonly TreeRow[], cursorId: string | null, selectedId: string | null, page: number,
): ListKeyOutcome {
  if (!isListKey(ev)) return { kind: 'pass' };
  if (rows.length === 0) return { kind: 'none' };
  const row = activeRow(rows, cursorId, selectedId);
  const i = row ? rows.indexOf(row) : -1;
  const move = (to: string | null): ListKeyOutcome =>
    to !== null && to !== row?.session.id ? { kind: 'move', id: to } : { kind: 'none' };
  const step = Math.max(1, Math.floor(page));
  switch (ev.key) {
    case 'ArrowDown': return move(i < 0 ? idAt(rows, 0) : idAt(rows, i + 1));
    case 'ArrowUp': return move(i < 0 ? idAt(rows, rows.length - 1) : idAt(rows, i - 1));
    case 'Home': return move(idAt(rows, 0));
    case 'End': return move(idAt(rows, rows.length - 1));
    case 'PageDown': return move(idAt(rows, i < 0 ? step - 1 : i + step));
    case 'PageUp': return move(idAt(rows, i < 0 ? 0 : i - step));
    case 'Enter': return row ? { kind: 'open', id: row.session.id } : { kind: 'none' };
    case 'ArrowRight': {
      if (!row) return move(idAt(rows, 0));
      if (!row.hasChildren) return { kind: 'none' };
      if (!row.expanded) return { kind: 'fold', id: row.session.id, collapse: false };
      const child = rows[i + 1];
      return child && child.depth > row.depth ? move(child.session.id) : { kind: 'none' };
    }
    case 'ArrowLeft': {
      if (!row) return move(idAt(rows, 0));
      if (row.hasChildren && row.expanded) return { kind: 'fold', id: row.session.id, collapse: true };
      const parent = row.session.parent_id;
      return parent && rows.some((r) => r.session.id === parent) ? move(parent) : { kind: 'none' };
    }
    default: return { kind: 'pass' };
  }
}

/**
 * What a screen reader reads for a row: the session name and its plain status,
 * then why it needs the operator, if it does.
 */
export function rowAccessibleName(name: string, status: string, attention: string | null): string {
  return [name, statusLabel(status), attention].filter((x): x is string => !!x).join(', ');
}

/** The DOM id of a row, for aria-activedescendant. */
export function rowDomId(sessionId: string): string {
  return `session-row-${sessionId}`;
}
