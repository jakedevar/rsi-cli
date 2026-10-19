// The operator to-do panel, badge and pop-up, DOM side (hub #3015 slice 13B,
// TUI #1747/#2950). The list is read every 5 s through `operator_todo_list`
// (OperatorTodoList); closes and undos go through `operator_todo_close` /
// `operator_todo_reopen` (src-tauri/src/todo.rs). The rules are in
// todo-model.ts (pure, tested); the registry entries in actions/todo.ts.
// Daemon text is set with textContent only.
import { invoke } from './api.js';
import { copyText, dispatch, type Invocation } from './commands.js';
import { $, el, toast } from './dom.js';
import { select, setView } from './sessions.js';
import { projectName, state, uiChanged } from './state.js';
import {
  type PopupState, type TodoItem, TODO_REFRESH_MS, TodoPanel, NO_ITEM, NOTHING_TO_UNDO, ageLabel, badgeText,
  closeTarget, closeToast, copiedMessage, linkLabel, parseLink, parseTodoItem, parseTodoList, popupKey,
  priorityLabel, sourceLabel, UNDO_WINDOW_SECS,
} from './todo-model.js';
import { dialogEl, focusDialog, overlayKey } from './actions/detail.js';
import type { TodoActionId, TodoSnapshot } from './actions/todo.js';
import { focusOwnsKey } from './modal-gate.js';

const STORE_KEY = 'rsi-desktop.todo';

const CSS = `
.todo-panel { width: 320px; min-width: 240px; display: flex; flex-direction: column; background: var(--mantle); border-left: 1px solid var(--surface0); }
.todo-panel[hidden] { display: none; }
.todo-head { display: flex; align-items: center; gap: 6px; padding: 8px 10px; border-bottom: 1px solid var(--surface0); }
.todo-head .todo-count { font-weight: 700; }
.todo-head .todo-new { color: var(--green); font-weight: 700; }
.todo-head .spacer { flex: 1; }
.todo-list { list-style: none; margin: 0; padding: 6px; overflow: auto; flex: 1; }
.todo-item { padding: 6px 8px; margin-bottom: 4px; border-radius: 6px; cursor: pointer; }
.todo-item.sel { background: var(--surface0); }
.todo-title { font-weight: 600; word-break: break-word; }
.todo-item.new .todo-title { color: var(--green); }
.todo-urgent { color: var(--yellow); margin-right: 4px; }
.todo-tag { color: var(--green); font-size: 11px; margin-left: 4px; }
.todo-how { font-family: var(--mono); font-size: 12px; color: var(--text); white-space: pre-wrap; word-break: break-all; max-height: 3em; overflow: hidden; margin-top: 2px; }
.todo-how::before { content: "$ "; color: var(--overlay0); }
.todo-meta { font-size: 11px; color: var(--overlay0); margin-top: 2px; }
.todo-empty { padding: 10px; color: var(--overlay0); }
.todo-closed-head { margin: 8px 8px 4px; color: var(--green); font-weight: 700; font-size: 12px; }
.todo-closed { padding: 2px 8px; color: var(--green); font-size: 12px; }
.todo-foot { padding: 6px 10px; border-top: 1px solid var(--surface0); font-size: 11px; color: var(--overlay0); }
.todo-badge { cursor: pointer; font-size: 12px; color: var(--overlay0); padding: 2px 6px; border-radius: 4px; }
.todo-badge.news { color: var(--yellow); font-weight: 700; }
.todo-badge[hidden] { display: none; }
.todo-popup { width: min(720px, 92vw); }
.todo-popup h2 { margin: 0 0 6px; font-size: 15px; word-break: break-word; }
.todo-popup-meta { font-size: 12px; color: var(--subtext); margin-bottom: 8px; }
.todo-popup-how { max-height: 46vh; overflow: auto; margin: 0 0 8px; padding: 8px; font-family: var(--mono); font-size: 12px; white-space: pre-wrap; word-break: break-all; background: var(--crust); border-radius: 6px; }
.todo-confirm { color: var(--yellow); font-weight: 700; min-height: 18px; margin-bottom: 6px; }
`;

const panel = loadPanel();
let reading = false;
/** Bumped to drop a read that started before a reopen (it would hide the item again). */
let readEpoch = 0;
let nextReadAt = 0;
let ui: { aside: HTMLElement; list: HTMLUListElement; head: HTMLElement; foot: HTMLElement; badge: HTMLElement } | null = null;
let popupRedraw: (() => void) | null = null;

function storage(): Storage | null {
  try { return window.localStorage; } catch { return null; }
}

function loadPanel(): TodoPanel {
  try {
    const raw = storage()?.getItem(STORE_KEY);
    const o = raw ? JSON.parse(raw) as { open?: unknown; lastVisit?: unknown } : {};
    return new TodoPanel(o.open === true, typeof o.lastVisit === 'number' ? o.lastVisit : null);
  } catch {
    return new TodoPanel();
  }
}

function persist(): void {
  try { storage()?.setItem(STORE_KEY, JSON.stringify({ open: panel.open, lastVisit: panel.lastVisitMs })); } catch { /* private mode */ }
}

export function todoSnapshot(): TodoSnapshot {
  return { open: panel.open, selected: !!panel.selected(), undo: !!panel.undoAvailable(Date.now()) };
}

/** For tests and other views: the panel state. */
export function todoPanel(): TodoPanel {
  return panel;
}

// ---------- reading ----------

function read(): void {
  if (reading) return;
  reading = true;
  const epoch = readEpoch;
  nextReadAt = Date.now() + TODO_REFRESH_MS;
  invoke<unknown>('operator_todo_list', { closedSince: panel.closedSince() }).then((v) => {
    if (epoch !== readEpoch) return;
    const list = parseTodoList(v);
    if (list) { panel.apply(list); render(); }
  }, () => { /* the TUI logs an unavailable read at debug level and keeps the last list */ })
    .finally(() => { reading = false; });
}

function readSoon(): void {
  nextReadAt = 0;
}

function tick(): void {
  const now = Date.now();
  if (panel.expireUndo(now)) { render(); uiChanged(); } else if (panel.open && panel.undoAvailable(now)) renderFoot(now);
  if (panel.open) { panel.touchVisit(now); persist(); }
  if (now >= nextReadAt) read();
}

// ---------- the panel ----------

function itemRow(item: TodoItem, now: number, isSel: boolean): HTMLLIElement {
  const li = el('li', `todo-item${isSel ? ' sel' : ''}${panel.isNew(item) ? ' new' : ''}`);
  li.setAttribute('role', 'option');
  li.setAttribute('aria-selected', String(isSel));
  const title = el('div', 'todo-title');
  if (item.priority <= 2) title.append(el('span', 'todo-urgent', '!'));
  title.append(document.createTextNode(item.title));
  if (panel.isNew(item)) title.append(el('span', 'todo-tag', 'NEW'));
  const meta = [ageLabel(now, item.created_at), sourceLabel(item, projectName(item.project_id))];
  if (item.link) meta.push(linkLabel(item.link));
  li.append(title, el('div', 'todo-how', item.how), el('div', 'todo-meta', meta.join(' · ')));
  li.title = `${priorityLabel(item.priority)} · double-click opens it`;
  li.addEventListener('click', () => { panel.select(item.id); render(); uiChanged(); });
  li.addEventListener('dblclick', () => { panel.select(item.id); openPopup(null); });
  return li;
}

function render(): void {
  if (!ui) return;
  const now = Date.now();
  ui.aside.hidden = !panel.open;
  const text = badgeText(panel);
  ui.badge.hidden = text === '';
  ui.badge.textContent = text;
  ui.badge.classList.toggle('news', panel.hasCatchUp());
  ui.badge.title = 'Operator to-do (<Space>dt shows or hides the panel)';
  if (panel.open) {
    const fresh = panel.newCount();
    const count = el('span', 'todo-count', `To-do · ${panel.openCount()} open`);
    const parts: HTMLElement[] = [count];
    if (fresh > 0) parts.push(el('span', 'todo-new', `· ${fresh} new`));
    const ack = el('button', 'btn', 'Caught up');
    ack.type = 'button';
    ack.title = 'Clear the highlights (<Space>da)';
    ack.addEventListener('click', () => acknowledge());
    const hide = el('button', 'btn', 'Hide');
    hide.type = 'button';
    hide.addEventListener('click', () => toggle());
    ui.head.replaceChildren(...parts, el('span', 'spacer'), ack, hide);
    const sel = panel.selected()?.id;
    const rows: HTMLElement[] = panel.items().map((i) => itemRow(i, now, i.id === sel));
    if (rows.length === 0) rows.push(el('li', 'todo-empty', panel.loaded() ? 'Nothing needs you.' : 'Loading…'));
    const closed = panel.closedSinceVisit();
    if (closed.length > 0) {
      rows.push(el('li', 'todo-closed-head', 'Closed since your last visit'));
      for (const c of closed) rows.push(el('li', 'todo-closed', `${c.status === 'dismissed' ? '×' : '✓'} ${c.title} (${c.status === 'dismissed' ? 'dismissed' : 'done'})`));
    }
    ui.list.replaceChildren(...rows);
    ui.list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    renderFoot(now);
  }
  popupRedraw?.();
}

function renderFoot(now: number): void {
  if (!ui) return;
  const undo = panel.undoAvailable(now);
  ui.foot.textContent = undo
    ? `<Space>du undoes “${undo.title}” (${Math.max(0, UNDO_WINDOW_SECS - Math.floor((now - undo.closedAtMs) / 1000))} s)`
    : '<Space>dj/dk move · dv open · dd done · dx dismiss · dy copy · do link';
}

function toggle(): void {
  panel.open = !panel.open;
  persist();
  render();
  uiChanged();
}

function move(delta: number): void {
  if (!panel.open) { toggle(); return; }
  if (delta >= 0) panel.selectNext(); else panel.selectPrev();
  render();
  uiChanged();
}

function acknowledge(): void {
  panel.acknowledge(Date.now());
  persist();
  render();
  readSoon();
  uiChanged();
}

// ---------- item actions ----------

function closeSelected(status: 'done' | 'dismissed'): void {
  const t = closeTarget(panel, status);
  const item = panel.selected();
  if (!t || !item) { toast(NO_ITEM); return; }
  panel.recordClose(item, status, Date.now());
  panel.removeOpen(t.id);
  render();
  uiChanged();
  invoke<unknown>('operator_todo_close', { todoId: t.id, status: t.status }).then(() => readSoon(), (e: unknown) => {
    toast(`To-do update failed: ${String(e)}`);
    readSoon();
  });
  readSoon();
  toast(closeToast(status), true);
}

function undo(): void {
  const entry = panel.takeUndo(Date.now());
  if (!entry) { toast(NOTHING_TO_UNDO); return; }
  toast(`Restoring to-do: ${entry.title}`, true);
  uiChanged();
  invoke<unknown>('operator_todo_reopen', { todoId: entry.id }).then((v) => {
    readEpoch += 1;
    const restored = parseTodoItem(v);
    if (restored) {
      panel.restoreOpen(restored);
      toast(`Restored to-do: ${restored.title}`, true);
    }
    render();
    readSoon();
    uiChanged();
  }, (e: unknown) => {
    toast(`Undo failed: ${String(e)}`);
    readSoon();
  });
}

function copySelected(): void {
  const item = panel.selected();
  if (!item) { toast(NO_ITEM); return; }
  void copyText(item.how, copiedMessage(item.how).replace(/^Copied /, ''));
}

async function openLink(): Promise<void> {
  const item = panel.selected();
  if (!item) { toast(NO_ITEM); return; }
  if (!item.link) { toast('This to-do has no link'); return; }
  const link = parseLink(item.link);
  if (!link) { toast(`Cannot open link: ${item.link}`); return; }
  if (link.kind === 'issue') { dispatch('issues-go-number', { source: 'key', arg: String(link.number) }); return; }
  if (link.kind === 'session') {
    if (state.sessions.some((s) => s.id === link.id)) { if (state.view !== 'active') await setView('active'); select(link.id); return; }
    if (state.view !== 'archived') await setView('archived');
    if (state.archived.some((s) => s.id === link.id)) { select(link.id); return; }
    toast('Linked session is unavailable');
    return;
  }
  // The system browser (open_url, src-tauri/src/nav.rs); the clipboard when no opener runs.
  try {
    await invoke<unknown>('open_url', { url: link.url });
    toast(`Opened ${link.url}`, true);
  } catch (e) {
    await copyText(link.url, `link ${link.url} (no browser opened: ${String(e)}; paste it there)`);
  }
}

// ---------- the pop-up ----------

function openPopup(confirm: 'done' | 'dismissed' | null): void {
  if (!panel.selected()) { toast(NO_ITEM); return; }
  let st: PopupState = { scroll: 0, confirm };
  const dlg = dialogEl('todo-popup');
  dlg.classList.add('todo-popup');
  const title = el('h2');
  const meta = el('div', 'todo-popup-meta');
  const how = el('pre', 'todo-popup-how');
  const prompt = el('div', 'todo-confirm');
  const keys = el('div', 'dialog-note');
  const draw = (): void => {
    const item = panel.selected();
    if (!item) { dlg.close(); return; }
    title.textContent = item.title;
    const bits = [priorityLabel(item.priority), sourceLabel(item, projectName(item.project_id)), ageLabel(Date.now(), item.created_at)];
    if (item.link) bits.push(linkLabel(item.link));
    meta.textContent = bits.join(' · ');
    how.textContent = item.how;
    how.scrollTop = st.scroll * 18;
    prompt.textContent = st.confirm ? `${st.confirm === 'done' ? 'Mark done' : 'Dismiss'} “${item.title}”? y/n` : '';
    keys.textContent = st.confirm
      ? 'y or Enter confirms, n or Esc cancels.'
      : 'y yank · d done · x dismiss · u undo · o open link · n/p next/previous · j/k scroll · Ctrl-D/U ten rows · g top · q/Esc close';
  };
  popupRedraw = draw;
  dlg.onclose = () => { if (popupRedraw === draw) popupRedraw = null; };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    if (!st.confirm && overlayKey(ev, dlg)) return;
    const r = popupKey(st, { key: ev.key, ctrl: ev.ctrlKey || ev.metaKey });
    if ((ev.key === 'd' || ev.key === 'x') && !ev.ctrlKey && !ev.metaKey && !st.confirm && !panel.selected()) { ev.preventDefault(); toast(NO_ITEM); return; }
    if (r.effect === 'none' && r.st === st) return;
    ev.preventDefault();
    const was = st;
    st = r.st;
    switch (r.effect) {
      case 'copy': copySelected(); break;
      case 'undo': undo(); break;
      case 'open-link': dlg.close(); void openLink(); return;
      case 'next': panel.selectNext(); render(); uiChanged(); break;
      case 'prev': panel.selectPrev(); render(); uiChanged(); break;
      case 'close': dlg.close(); return;
      case 'confirm-close': dlg.close(); if (was.confirm) closeSelected(was.confirm); return;
      default: break;
    }
    draw();
  };
  const box = el('div', 'launch-form');
  box.append(title, meta, how, prompt, keys);
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  focusDialog(dlg);
}

// ---------- dispatch and wiring ----------

export function runTodo(id: TodoActionId, _inv: Invocation): void {
  switch (id) {
    case 'todo-toggle': toggle(); return;
    case 'todo-next': move(1); return;
    case 'todo-prev': move(-1); return;
    case 'todo-open-link': void openLink(); return;
    case 'todo-done': openPopup('done'); return;
    case 'todo-dismiss': openPopup('dismissed'); return;
    case 'todo-copy': copySelected(); return;
    case 'todo-view': openPopup(null); return;
    case 'todo-undo': undo(); return;
    case 'todo-ack': acknowledge(); return;
  }
}

export function initTodo(): void {
  const style = document.createElement('style');
  style.id = 'todo-style';
  style.textContent = CSS;
  document.head.append(style);
  const aside = el('aside', 'todo-panel');
  aside.id = 'todo-panel';
  aside.setAttribute('aria-label', 'Operator to-do');
  const head = el('div', 'todo-head');
  const list = el('ul', 'todo-list');
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Open to-do items');
  const foot = el('div', 'todo-foot');
  foot.dataset.keyHint = ''; // the leader-key footer follows the Vim keys switch (key-hints.ts)
  aside.append(head, list, foot);
  document.querySelector('.layout')?.append(aside);
  const badge = el('span', 'todo-badge');
  badge.id = 'todo-badge';
  badge.addEventListener('click', () => toggle());
  $('conn').after(badge);
  ui = { aside, list, head, foot, badge };
  render();
  read();
  window.setInterval(tick, 1000);
}
