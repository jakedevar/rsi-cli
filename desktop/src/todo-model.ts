// The operator to-do panel and pop-up, pure rules (hub #3015 slice 13B). A port
// of the TUI's crates/rsi/src/todo_panel.rs (TodoPanel, TodoLink, labels) and
// the key rules of overlay/todo_popup.rs. The list lives in the daemon
// (OperatorTodoList / Close / Reopen); this holds what the UI adds: the panel's
// open choice, the cursor, the catch-up baseline and the undo window.
// Tested in tests/todo.test.mjs; the DOM is src/todo.ts.

export type TodoStatus = 'open' | 'done' | 'dismissed';

/** `OperatorTodoV1` as the daemon sends it (the fields the UI reads). */
export interface TodoItem {
  id: string;
  scope: string;
  project_id: string | null;
  title: string;
  how: string;
  link: string | null;
  priority: number;
  status: TodoStatus;
  closed_by: string | null;
  created_at: string;
  closed_at: string | null;
}

/** `OperatorTodoListResultV1`. */
export interface TodoList {
  open: TodoItem[];
  recently_closed: TodoItem[];
  truncated: boolean;
}

/** `OPERATOR_TODO_OPERATOR`: the operator's own scope and closer name. */
export const TODO_OPERATOR = 'operator';
/** `OPERATOR_TODO_UNDO_WINDOW_SECS`. */
export const UNDO_WINDOW_SECS = 60;
/** The TUI re-reads the list every 5 s (app/todo.rs TODO_REFRESH_INTERVAL). */
export const TODO_REFRESH_MS = 5_000;

/**
 * Milliseconds of an RFC 3339 time. rsid writes nanoseconds; the fraction is
 * cut to milliseconds first because not every webview's Date.parse takes more.
 */
export function timeMs(s: string | null | undefined): number {
  if (!s) return NaN;
  return Date.parse(s.replace(/(\.\d{3})\d+/, '$1'));
}

function str(v: unknown): string | null {
  return typeof v === 'string' ? v : null;
}

function parseItem(v: unknown): TodoItem | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const id = str(o.id);
  const title = str(o.title);
  const created = str(o.created_at);
  if (!id || title === null || !created) return null;
  const status = o.status === 'done' || o.status === 'dismissed' ? o.status : 'open';
  return {
    id,
    scope: str(o.scope) ?? '',
    project_id: str(o.project_id),
    title,
    how: str(o.how) ?? '',
    link: str(o.link),
    priority: typeof o.priority === 'number' ? o.priority : 3,
    status,
    closed_by: str(o.closed_by),
    created_at: created,
    closed_at: str(o.closed_at),
  };
}

/** A daemon `OperatorTodoList` reply, or null when it is not one. */
export function parseTodoList(v: unknown): TodoList | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  if (!Array.isArray(o.open)) return null;
  const items = (a: unknown): TodoItem[] =>
    (Array.isArray(a) ? a : []).map(parseItem).filter((i): i is TodoItem => i !== null);
  return { open: items(o.open), recently_closed: items(o.recently_closed), truncated: o.truncated === true };
}

/** A reply carrying one item (`{todo}` of Close / Reopen), or the item itself. */
export function parseTodoItem(v: unknown): TodoItem | null {
  if (v && typeof v === 'object' && 'todo' in v) return parseItem((v as { todo: unknown }).todo);
  return parseItem(v);
}

// ---------- links ----------

export type TodoLink =
  | { kind: 'issue'; number: number }
  | { kind: 'session'; id: string }
  | { kind: 'url'; url: string };

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** `issue:#N`, `session:<uuid>` or an http(s) URL; anything else is shown, never opened. */
export function parseLink(link: string): TodoLink | null {
  const l = link.trim();
  if (l.startsWith('issue:')) {
    const n = l.slice(6).replace(/^#+/, '');
    return /^\d+$/.test(n) && Number.isSafeInteger(Number(n)) ? { kind: 'issue', number: Number(n) } : null;
  }
  if (l.startsWith('session:')) {
    const id = l.slice(8);
    return UUID_RE.test(id) ? { kind: 'session', id: id.toLowerCase() } : null;
  }
  if (l.startsWith('https://') || l.startsWith('http://')) return { kind: 'url', url: l };
  return null;
}

/** Short text for the item row. */
export function linkLabel(link: string): string {
  const p = parseLink(link);
  if (!p) return link;
  if (p.kind === 'issue') return `issue #${p.number}`;
  if (p.kind === 'session') return `session ${p.id.slice(0, 8)}`;
  return p.url;
}

// ---------- labels ----------

/** `urgent`, `high`, `normal`, `low` for a priority 1-4. */
export function priorityLabel(priority: number): string {
  return priority === 1 ? 'urgent' : priority === 2 ? 'high' : priority === 3 ? 'normal' : 'low';
}

/** `now`, `3m`, `2h`, `5d`. */
export function ageLabel(nowMs: number, created: string): string {
  const s = Math.max(0, Math.floor((nowMs - timeMs(created)) / 1000));
  if (!Number.isFinite(s) || s < 60) return 'now';
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86_400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86_400)}d`;
}

/** Who wrote the item: `you`, the project's name, `portfolio`, or the scope kind. */
export function sourceLabel(item: TodoItem, projectName: string | undefined): string {
  if (item.scope === TODO_OPERATOR) return 'you';
  const kind = item.scope.split(':')[0] || 'manager';
  if (kind === 'project' && projectName) return projectName;
  if (kind === 'portfolio') return 'portfolio';
  return kind;
}

/** The status line after a copy: the first 40 characters of the command. */
export function copiedMessage(text: string): string {
  const chars = [...text];
  const preview = chars.slice(0, 40).join('') + (chars.length > 40 ? '…' : '');
  return `Copied to-do command: ${preview}`;
}

/** The badge: `☐ 3 todo +1 ✓2`, empty when nothing is open. */
export function badgeText(panel: TodoPanel): string {
  const open = panel.openCount();
  if (open === 0) return '';
  const fresh = panel.newCount();
  const closed = panel.closedSinceVisit().length;
  return `☐ ${open} todo${fresh > 0 ? ` +${fresh}` : ''}${closed > 0 ? ` ✓${closed}` : ''}`;
}

export function closeToast(status: 'done' | 'dismissed'): string {
  return `${status === 'dismissed' ? 'To-do dismissed' : 'To-do marked done'} · <Space>du undoes it for ${UNDO_WINDOW_SECS} s`;
}

export const NOTHING_TO_UNDO = `Nothing to undo (a close can be undone for ${UNDO_WINDOW_SECS} s)`;
export const NO_ITEM = 'No to-do item selected';

// ---------- the panel ----------

export interface UndoEntry {
  id: string;
  title: string;
  status: 'done' | 'dismissed';
  closedAtMs: number;
}

/** Most urgent first, then newest first, then id. */
function sortOpen(items: TodoItem[]): void {
  items.sort((a, b) => a.priority - b.priority
    || timeMs(b.created_at) - timeMs(a.created_at)
    || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/** State of the operator to-do panel (TodoPanel in the TUI). */
export class TodoPanel {
  /** The operator wants the panel shown (remembered across runs). */
  open: boolean;
  /** The last time the panel was in view (remembered across runs). */
  lastVisitMs: number | null;
  /** Catch-up cutoff for this run: lastVisit at start, or the last acknowledgement. */
  private baselineMs: number | null;
  private openItems: TodoItem[] = [];
  private closedItems: TodoItem[] = [];
  private selectedId: string | null = null;
  private wasLoaded = false;
  private undo: UndoEntry | null = null;

  constructor(open = false, lastVisitMs: number | null = null) {
    this.open = open;
    this.lastVisitMs = lastVisitMs;
    this.baselineMs = lastVisitMs;
  }

  /** The `closed_since` to ask for (ISO), or null on a first run. */
  closedSince(): string | null {
    return this.baselineMs === null ? null : new Date(this.baselineMs).toISOString();
  }

  loaded(): boolean { return this.wasLoaded; }

  /** Replace the lists with a fresh read; the selection follows its item. */
  apply(list: TodoList): void {
    const open = [...list.open];
    sortOpen(open);
    this.openItems = open;
    this.closedItems = [...list.recently_closed];
    this.wasLoaded = true;
    if (!this.selectedId || !open.some((i) => i.id === this.selectedId)) this.selectedId = open[0]?.id ?? null;
  }

  /** Put a just-reopened item back without waiting for the next read, and select it. */
  restoreOpen(item: TodoItem): void {
    if (!this.openItems.some((i) => i.id === item.id)) {
      this.openItems.push(item);
      sortOpen(this.openItems);
    }
    this.selectedId = item.id;
  }

  /** Forget the daemon's lists (the connection was lost). */
  clear(): void {
    this.openItems = [];
    this.closedItems = [];
    this.selectedId = null;
    this.wasLoaded = false;
  }

  items(): readonly TodoItem[] { return this.openItems; }
  openCount(): number { return this.openItems.length; }

  isNew(item: TodoItem): boolean {
    return this.baselineMs !== null && timeMs(item.created_at) > this.baselineMs;
  }

  newCount(): number { return this.openItems.filter((i) => this.isNew(i)).length; }

  /** Items an agent closed since the baseline; the operator's own closes are not news. */
  closedSinceVisit(): TodoItem[] {
    const base = this.baselineMs;
    if (base === null) return [];
    return this.closedItems.filter((i) => i.closed_by !== TODO_OPERATOR && timeMs(i.closed_at) >= base);
  }

  hasCatchUp(): boolean { return this.newCount() > 0 || this.closedSinceVisit().length > 0; }

  selected(): TodoItem | undefined {
    return this.selectedId ? this.openItems.find((i) => i.id === this.selectedId) : undefined;
  }

  selectedIndex(): number | null {
    const i = this.openItems.findIndex((x) => x.id === this.selectedId);
    return i < 0 ? null : i;
  }

  select(id: string): void {
    if (this.openItems.some((i) => i.id === id)) this.selectedId = id;
  }

  selectNext(): void { this.step(1); }
  selectPrev(): void { this.step(-1); }

  private step(delta: number): void {
    if (this.openItems.length === 0) { this.selectedId = null; return; }
    const last = this.openItems.length - 1;
    const at = this.selectedIndex();
    const next = at === null ? 0 : Math.min(last, Math.max(0, at + delta));
    this.selectedId = this.openItems[next]!.id;
  }

  /** Drop a just-closed item now, moving the cursor to its neighbour. */
  removeOpen(id: string): void {
    const at = this.openItems.findIndex((i) => i.id === id);
    if (at < 0) return;
    this.openItems.splice(at, 1);
    if (this.selectedId === id) this.selectedId = this.openItems[Math.min(at, this.openItems.length - 1)]?.id ?? null;
  }

  /** Remember the operator's close for the undo window; only the latest is undoable. */
  recordClose(item: TodoItem, status: 'done' | 'dismissed', nowMs: number): void {
    this.undo = { id: item.id, title: item.title, status, closedAtMs: nowMs };
  }

  undoAvailable(nowMs: number): UndoEntry | null {
    return this.undo && nowMs - this.undo.closedAtMs < UNDO_WINDOW_SECS * 1000 ? this.undo : null;
  }

  /** Take the undoable close (leaving none), or null when there is none or it expired. */
  takeUndo(nowMs: number): UndoEntry | null {
    const e = this.undo;
    this.undo = null;
    return e && nowMs - e.closedAtMs < UNDO_WINDOW_SECS * 1000 ? e : null;
  }

  /** Forget an expired close; true when one was dropped. */
  expireUndo(nowMs: number): boolean {
    if (this.undo && nowMs - this.undo.closedAtMs >= UNDO_WINDOW_SECS * 1000) { this.undo = null; return true; }
    return false;
  }

  /** Caught up: clear every highlight and count from now. */
  acknowledge(nowMs: number): void {
    this.baselineMs = nowMs;
    this.lastVisitMs = nowMs;
    this.closedItems = [];
  }

  /** In view now: remembered for the next run; this run's baseline does not move. */
  touchVisit(nowMs: number): void { this.lastVisitMs = nowMs; }
}

/** The close `<Space>dd` / `<Space>dx` ask for, or null with nothing selected. */
export function closeTarget(panel: TodoPanel, status: 'done' | 'dismissed'): { id: string; status: 'done' | 'dismissed' } | null {
  const s = panel.selected();
  return s ? { id: s.id, status } : null;
}

// ---------- the pop-up's keys ----------

export interface PopupState {
  /** First visible row of the body. */
  scroll: number;
  /** A close waiting for `y`, or null while the item is shown. */
  confirm: 'done' | 'dismissed' | null;
}

export type PopupEffect =
  | 'none' | 'copy' | 'undo' | 'open-link' | 'next' | 'prev' | 'close' | 'confirm-close';

export interface PopupKey { key: string; ctrl?: boolean }

/** The TUI's pop-up keys: the new state and what the DOM must do. */
export function popupKey(st: PopupState, ev: PopupKey): { st: PopupState; effect: PopupEffect } {
  const k = ev.key;
  if (st.confirm) {
    if (k === 'y' || k === 'Y' || k === 'Enter') return { st, effect: 'confirm-close' };
    if (k === 'n' || k === 'N' || k === 'q' || k === 'Escape') return { st: { ...st, confirm: null }, effect: 'none' };
    return { st, effect: 'none' };
  }
  const scroll = (d: number): { st: PopupState; effect: PopupEffect } =>
    ({ st: { ...st, scroll: Math.max(0, st.scroll + d) }, effect: 'none' });
  if (ev.ctrl) {
    if (k === 'd') return scroll(10);
    if (k === 'u') return scroll(-10);
    return { st, effect: 'none' };
  }
  switch (k) {
    case 'y': return { st, effect: 'copy' };
    case 'd': return { st: { ...st, confirm: 'done' }, effect: 'none' };
    case 'x': return { st: { ...st, confirm: 'dismissed' }, effect: 'none' };
    case 'u': return { st, effect: 'undo' };
    case 'o': return { st, effect: 'open-link' };
    case 'n': return { st: { ...st, scroll: 0 }, effect: 'next' };
    case 'p': return { st: { ...st, scroll: 0 }, effect: 'prev' };
    case 'j': case 'ArrowDown': return scroll(1);
    case 'k': case 'ArrowUp': return scroll(-1);
    case 'PageDown': return scroll(10);
    case 'PageUp': return scroll(-10);
    case 'g': case 'Home': return { st: { ...st, scroll: 0 }, effect: 'none' };
    case 'q': case 'Escape': return { st, effect: 'close' };
    default: return { st, effect: 'none' };
  }
}
