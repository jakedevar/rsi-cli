// Global Dashboard (hub #3366, slice D of #3314): the DOM side of `gd` /
// `:dashboard`, the desktop counterpart of the TUI view
// (crates/rsi/src/overlay/global_dashboard.rs). One full-screen view above
// every project: a row per project (the home project first) with a pinned
// TOTAL row, a detail band for the selected row, a visible incomplete badge
// when the daemon capped a read, and a side pane with the top-level manager's
// conversation. Reads: GetGlobalDashboard (managers.rs global_dashboard),
// GetGlobalManager, and the existing GetSession / GetConversation / SendMessage;
// the start action is the gm console's launch-and-appoint flow
// (manager-launch.ts), whose dialog is the confirm. Rules live in
// dashboard-model.ts; daemon text is set with textContent only.
import { api, type ConversationEvent } from './api.js';
import {
  AUTO_REFRESH_MS, DASHBOARD_KEYS, type DashboardKeyAction, type DashboardSort, type DashboardWindow,
  type GlobalDashboard, NOT_A_PROJECT, SORTS, SORT_LABELS, START_LABEL, type TopSeat, TYPE_LABEL, WINDOWS, alreadyLiveNotice,
  columns, conversationNotice, cycle, dashboardHints, dashboardRows, detailLines, incompleteText, liveSeat, noManagerNotice,
  notListedText, refreshFailedText, reselect, seatAction, seatFromGrant, snapshotAge, startHint, startHost, unreadableText,
} from './dashboard-model.js';
import { el, errorBlock, renderMessage, toast } from './dom.js';
import { rememberOpener, returnFocus } from './focus-return.js';
import { launchAndAppoint } from './manager-launch.js';
import { ask, runManagers } from './managers.js';
import { managersApi } from './managers-api.js';
import { focusOwnsKey, paneKeyAllowed, paneKeyName } from './modal-gate.js';
import { uiChanged } from './state.js';
import { isVimKeysOn } from './vimkeys.js';
import { syncListbox } from './listbox-a11y.js';

/** Below this table width the headers use the TUI's short forms (PM, Mgr, Wrk, Rev, WIP). */
const NARROW_TABLE_PX = 1100;

/** Conversation events the pane shows (the gm console's tail). */
const CONVERSATION_TAIL = 40;
/** At or above this window width the pane is shown by default (the TUI's 160 columns). */
const PANE_AUTO_MIN_PX = 1200;

const dv = {
  open: false,
  snapshot: null as GlobalDashboard | null,
  window: '24h' as DashboardWindow,
  sort: 'agents' as DashboardSort,
  selected: 0,
  loading: false,
  error: null as string | null,
  notice: null as string | null,
  seat: { kind: 'unknown' } as TopSeat,
  seatStatus: null as string | null,
  conv: { sessionId: null as string | null, events: [] as ConversationEvent[], error: null as string | null },
  /** `t`: true forces the pane, false hides it, null follows the window width. */
  paneOverride: null as boolean | null,
  pendingG: false,
  epoch: 0,
};

let root: HTMLElement | null = null;
const part = (cls: string): HTMLElement => root!.querySelector<HTMLElement>(`.${cls}`)!;
const errText = (e: unknown): string => String(e instanceof Error ? e.message : e);

export function dashboardOpen(): boolean {
  return dv.open;
}

/** `gd` / `:dashboard`: open the view, or refresh it when it is already open. */
export async function openDashboard(): Promise<void> {
  const o = overlay();
  if (!dv.open) {
    rememberOpener('dashboard');
    dv.open = true;
    dv.notice = null;
    dv.error = null;
    o.hidden = false;
    uiChanged();
  }
  part('dash-table-wrap').focus();
  render();
  await refresh();
}

function close(): void {
  dv.open = false;
  dv.epoch++;
  dv.pendingG = false;
  if (root) root.hidden = true;
  returnFocus('dashboard');
  uiChanged();
}

/** Re-read the snapshot and the seat; a failure keeps the last snapshot with an actionable error. */
async function refresh(): Promise<void> {
  const epoch = ++dv.epoch;
  dv.loading = true;
  render();
  const [snap, grant] = await Promise.allSettled([managersApi.dashboard(dv.window), managersApi.globalGrant()]);
  if (epoch !== dv.epoch) return;
  dv.loading = false;
  if (snap.status === 'fulfilled') {
    dv.selected = reselect({ snapshot: dv.snapshot, sort: dv.sort, selected: dv.selected }, { snapshot: snap.value, sort: dv.sort });
    dv.snapshot = snap.value;
    dv.error = null;
  } else {
    dv.error = refreshFailedText(errText(snap.reason), !!dv.snapshot, isVimKeysOn());
  }
  // A failed grant read keeps the seat we already knew.
  if (grant.status === 'fulfilled') dv.seat = seatFromGrant(grant.value as { state?: string; seat_session_id?: string } | null);
  await loadSeat(epoch);
  render();
}

async function loadSeat(epoch: number): Promise<void> {
  if (dv.seat.kind !== 'seat') { dv.seatStatus = null; dv.conv = { sessionId: null, events: [], error: null }; return; }
  const id = dv.seat.sessionId;
  try {
    const s = await api.getSession(id);
    if (epoch !== dv.epoch) return;
    dv.seatStatus = s.status;
  } catch (e) {
    if (epoch !== dv.epoch) return;
    dv.seat = { kind: 'absent' };
    dv.seatStatus = null;
    dv.error = `Top-level manager session unavailable: ${errText(e)}; ${startHint(isVimKeysOn())}`;
    return;
  }
  if (!liveSeat(dv.seat, dv.seatStatus)) return;
  try {
    const events = await api.getConversation(id);
    if (epoch !== dv.epoch) return;
    dv.conv = { sessionId: id, events: events.slice(-CONVERSATION_TAIL), error: null };
  } catch (e) {
    if (epoch === dv.epoch) dv.conv = { sessionId: id, events: [], error: errText(e) };
  }
}

function paneVisible(): boolean {
  return dv.paneOverride ?? window.innerWidth >= PANE_AUTO_MIN_PX;
}

// ---------- render ----------

function render(): void {
  if (!root || !dv.open) return;
  const s = dv.snapshot;
  part('dash-meta').textContent = [
    `Window ${dv.window}`, `sorted by ${SORT_LABELS[dv.sort]}`,
    s ? `as of ${snapshotAge(s.as_of, Date.now())}` : '', dv.loading ? 'refreshing…' : '',
  ].filter(Boolean).join(' · ');
  for (const b of root.querySelectorAll<HTMLButtonElement>('.dash-win')) b.classList.toggle('on', b.dataset.window === dv.window);
  const badge = part('dash-incomplete');
  const inc = incompleteText(s);
  badge.hidden = !inc;
  badge.textContent = inc ? 'Incomplete' : '';
  badge.title = inc ?? '';
  const status = part('dash-status');
  if (dv.error) renderMessage(status, dv.error); else status.textContent = inc ?? dv.notice ?? '';
  status.className = `dash-status${dv.error ? ' err' : inc ? ' warn' : ''}`;
  renderTable();
  renderDetail();
  renderPane();
  part('dash-hints').textContent = dashboardHints(isVimKeysOn());
  renderActions();
}

/** The header buttons that depend on the selection: Open project applies to a project row, not All projects. */
function renderActions(): void {
  const row = dashboardRows(dv.snapshot, dv.sort)[dv.selected];
  const open = root!.querySelector<HTMLButtonElement>('button[data-dash="open"]')!;
  const ok = !!row?.id;
  open.disabled = !ok;
  open.title = ok ? `Open ${row.name} in the session list` : row ? 'Select a project row to open it (this row adds up every project)' : 'No project to open yet';
  root!.querySelector<HTMLButtonElement>('button[data-dash="sort"]')!.disabled = !dv.snapshot;
}

function renderTable(): void {
  const wrap = part('dash-table-wrap');
  wrap.replaceChildren();
  const s = dv.snapshot;
  if (!s) {
    wrap.append(el('p', 'dash-empty', dv.error ? unreadableText(isVimKeysOn()) : 'Loading the dashboard…'));
    return;
  }
  const rows = dashboardRows(s, dv.sort);
  dv.selected = Math.min(Math.max(0, dv.selected), rows.length - 1);
  const width = wrap.clientWidth;
  const cols = columns(s.window, width > 0 && width < NARROW_TABLE_PX);
  const table = el('table', 'dash-table');
  table.setAttribute('aria-label', 'Projects');
  const head = el('tr');
  for (const c of cols) {
    const th = el('th', c.numeric ? 'num' : undefined, c.header);
    th.title = c.title;
    th.scope = 'col';
    head.append(th);
  }
  table.append(el('thead'), el('tbody'));
  table.tHead!.append(head);
  if (!s.projects.length) {
    const tr = el('tr', 'dash-empty-row');
    const td = el('td', 'dash-empty', 'No projects yet. Create a project, then its agents, tokens, cost and Issues show here.');
    td.colSpan = cols.length;
    tr.append(td);
    table.tBodies[0]!.append(tr);
  }
  rows.forEach((r, i) => {
    const tr = el('tr', ['dash-row', r.isHome ? 'home' : '', r.isTotal ? 'total' : '', i === dv.selected ? 'sel' : ''].filter(Boolean).join(' '));
    tr.dataset.index = String(i);
    tr.setAttribute('role', 'row');
    tr.setAttribute('aria-selected', String(i === dv.selected));
    for (const c of cols) {
      const cell = r.cells[c.key];
      const td = el('td', [c.numeric ? 'num' : '', cell.dim ? 'dim' : ''].filter(Boolean).join(' ') || undefined, cell.text);
      if (cell.title) td.title = cell.title;
      tr.append(td);
    }
    table.tBodies[0]!.append(tr);
  });
  wrap.append(table);
  // The wrapper holds focus and the arrows move the highlighted row: a grid whose focus owner points at it (local #330).
  syncListbox(table, wrap, { role: 'grid' });
  table.tBodies[0]!.querySelector('tr.sel')?.scrollIntoView({ block: 'nearest' });
}

function renderDetail(): void {
  const band = part('dash-detail');
  band.replaceChildren(...detailLines(dv.snapshot, dv.sort, dv.selected).map((line, i) => el(i === 0 ? 'h3' : 'p', undefined, line)));
}

function renderPane(): void {
  const pane = part('dash-pane');
  const body = root!.querySelector<HTMLElement>('.dash-body')!;
  const show = paneVisible();
  pane.hidden = !show;
  body.classList.toggle('with-pane', show);
  if (!show) return;
  pane.replaceChildren(el('h3', undefined, 'Top-level manager'));
  const action = seatAction(dv.seat, dv.seatStatus, isVimKeysOn());
  pane.append(el('p', action.kind === 'start' ? 'dash-cta' : 'dash-note', action.text));
  if (action.kind === 'start') {
    const b = el('button', 'btn dash-start', `${START_LABEL} (n)`);
    b.type = 'button';
    b.dataset.dash = 'start';
    pane.append(b);
    return;
  }
  if (action.kind !== 'type') return;
  const b = el('button', 'btn', `${TYPE_LABEL} (i)`);
  b.type = 'button';
  b.dataset.dash = 'type';
  pane.append(b);
  if (dv.conv.error) pane.append(errorBlock('p', 'dash-err', dv.conv.error));
  const log = el('div', 'dash-conv-log');
  log.tabIndex = -1;
  log.setAttribute('aria-label', 'Top-level manager conversation');
  for (const ev of dv.conv.events) {
    if (!ev.content) continue;
    const block = el('div', `dash-conv ${ev.role === 'User' ? 'user' : ''}`);
    block.append(el('b', undefined, `${ev.role ?? ev.event_type}${ev.tool_name ? ` · ${ev.tool_name}` : ''}`),
      el('pre', undefined, ev.content.length > 1200 ? `${ev.content.slice(0, 1200)}…` : ev.content));
    log.append(block);
  }
  if (!dv.conv.events.length && !dv.conv.error) log.append(el('p', 'dash-note', 'No messages yet.'));
  pane.append(log);
  log.scrollTop = log.scrollHeight;
}

// ---------- actions ----------

function openProject(): void {
  const row = dashboardRows(dv.snapshot, dv.sort)[dv.selected];
  if (!row) return;
  if (!row.id) { dv.notice = NOT_A_PROJECT; render(); return; }
  const f = document.querySelector<HTMLSelectElement>('#project-filter');
  if (!f || ![...f.options].some((o) => o.value === row.id)) { toast(notListedText(row.name, isVimKeysOn())); return; }
  f.value = row.id;
  f.dispatchEvent(new Event('change'));
  close();
}

function focusConversation(): void {
  if (!paneVisible()) { dv.paneOverride = true; render(); }
  const log = root!.querySelector<HTMLElement>('.dash-conv-log');
  if (!log) {
    dv.notice = noManagerNotice(dv.seat, isVimKeysOn());
    render();
    return;
  }
  log.focus();
  dv.notice = conversationNotice(isVimKeysOn());
  status();
}

function status(): void {
  if (!root) return;
  const s = part('dash-status');
  if (!dv.error) { s.textContent = dv.notice ?? ''; s.className = 'dash-status'; }
}

async function typeToSeat(): Promise<void> {
  const id = liveSeat(dv.seat, dv.seatStatus);
  if (!id) { dv.notice = noManagerNotice(dv.seat, isVimKeysOn()); render(); return; }
  const got = await ask({ title: 'Message the top-level manager', input: { value: '', placeholder: 'Ctrl-Enter sends', multiline: true }, ok: 'Send' });
  if (!got?.text.trim()) return;
  try {
    await api.sendMessage(id, got.text);
    toast('Sent to the top-level manager', true);
    await refresh();
  } catch (e) {
    dv.error = `Send failed: ${errText(e)}`;
    render();
  }
}

/** `n`: start a top-level manager through the gm console's appoint flow, hosted in the home project. */
async function startSeat(): Promise<void> {
  if (dv.seat.kind === 'unknown') { dv.notice = 'Still reading the top-level manager; try again in a moment'; render(); return; }
  if (liveSeat(dv.seat, dv.seatStatus)) {
    dv.notice = alreadyLiveNotice(isVimKeysOn());
    render();
    return;
  }
  try {
    const out = await launchAndAppoint({ kind: 'global' }, startHost(dv.snapshot));
    if (!out) return;
    dv.notice = out.message;
    toast(out.message, true);
    await refresh();
  } catch (e) {
    dv.error = errText(e);
    render();
  }
}

function move(to: number): void {
  const n = dashboardRows(dv.snapshot, dv.sort).length;
  if (!n) return;
  dv.selected = Math.min(Math.max(0, to), n - 1);
  dv.notice = null;
  render();
}

export function dashboardKey(action: DashboardKeyAction): void {
  switch (action) {
    case 'down': move(dv.selected + 1); return;
    case 'up': move(dv.selected - 1); return;
    case 'first': move(0); return;
    case 'last': move(Number.MAX_SAFE_INTEGER); return;
    case 'open': openProject(); return;
    case 'window': dv.window = cycle(WINDOWS, dv.window); void refresh(); return;
    case 'sort': {
      const sort = cycle(SORTS, dv.sort);
      dv.selected = reselect({ snapshot: dv.snapshot, sort: dv.sort, selected: dv.selected }, { snapshot: dv.snapshot, sort });
      dv.sort = sort;
      render();
      return;
    }
    case 'pane': dv.paneOverride = !paneVisible(); render(); return;
    case 'type': void typeToSeat(); return;
    case 'start': void startSeat(); return;
    case 'conversation': focusConversation(); return;
    case 'refresh': void refresh(); return;
    case 'close': close(); return;
    case 'workspace': close(); runManagers('mgr-global', { source: 'button' }); return;
  }
}

// ---------- wiring ----------

const CSS = `
.dash-view { position: fixed; inset: 0; z-index: 41; display: flex; flex-direction: column; background: var(--base); color: var(--text); }
.dash-view[hidden] { display: none; }
.dash-head { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding: 8px 12px; border-bottom: 1px solid var(--surface1); background: var(--mantle); }
.dash-title { font-weight: 600; color: var(--mauve); }
.dash-win.on { background: var(--surface1); color: var(--text); }
.dash-incomplete { font-size: 12px; font-weight: 600; padding: 1px 8px; border-radius: 10px; background: var(--yellow); color: var(--on-yellow); }
.dash-incomplete[hidden] { display: none; }
.dash-meta { font-size: 12px; color: var(--subtext); flex: 1 1 auto; }
.dash-status { padding: 4px 12px; font-size: 12px; color: var(--green); min-height: 16px; }
.dash-status.err { color: var(--red); }
.dash-status.warn { color: var(--yellow); }
.dash-body { flex: 1; display: grid; grid-template-columns: minmax(0, 1fr); min-height: 0; }
.dash-body.with-pane { grid-template-columns: minmax(0, 3fr) minmax(280px, 2fr); }
.dash-main { display: flex; flex-direction: column; min-height: 0; min-width: 0; }
.dash-table-wrap { flex: 1; overflow: auto; outline: none; }
.dash-table-wrap:focus-visible { outline: 1px solid var(--mauve); }
.dash-table { border-collapse: collapse; width: 100%; font-family: var(--mono); font-size: 12px; white-space: nowrap; }
.dash-table th { position: sticky; top: 0; background: var(--mantle); color: var(--blue); text-align: left; padding: 4px 8px; font-weight: 600; cursor: help; }
.dash-table td { padding: 3px 8px; border-left: 3px solid transparent; }
.dash-table .num { text-align: right; }
.dash-table td.dim { color: var(--muted); }
.dash-row { cursor: pointer; }
.dash-row td:first-child { border-left-color: transparent; }
.dash-row.sel { background: var(--surface0); }
.dash-row.sel td:first-child { border-left-color: var(--mauve); }
.dash-row.home td:first-child { color: var(--teal); font-weight: 600; }
.dash-row.total td { border-top: 1px solid var(--surface1); font-weight: 600; }
.dash-empty { padding: 12px; color: var(--muted); font-family: inherit; white-space: normal; }
.dash-detail { border-top: 1px solid var(--surface1); padding: 6px 12px; font-size: 12px; max-height: 30%; overflow: auto; }
.dash-detail h3 { margin: 0 0 4px; font-size: 13px; color: var(--blue); }
.dash-detail p { margin: 2px 0; font-family: var(--mono); }
.dash-pane { border-left: 1px solid var(--surface1); padding: 10px 14px; display: flex; flex-direction: column; min-height: 0; gap: 6px; }
.dash-pane[hidden] { display: none; }
.dash-pane h3 { margin: 0; font-size: 14px; color: var(--blue); }
.dash-cta { color: var(--yellow); margin: 0; }
.dash-note { color: var(--subtext); font-size: 12px; margin: 0; }
.dash-err { color: var(--red); font-size: 12px; margin: 0; }
.dash-conv-log { flex: 1; overflow: auto; outline: none; }
.dash-conv-log:focus { outline: 1px solid var(--mauve); }
.dash-conv { margin: 6px 0; font-size: 12px; }
.dash-conv pre { white-space: pre-wrap; margin: 2px 0 0; font-family: var(--mono); }
.dash-conv.user b { color: var(--blue); }
.dash-hints { padding: 4px 12px; font-size: 11px; color: var(--muted); border-top: 1px solid var(--surface1); }
`;

function overlay(): HTMLElement {
  if (root) return root;
  const o = el('section', 'dash-view');
  o.id = 'dashboard-view';
  o.hidden = true;
  o.setAttribute('aria-label', 'Global dashboard');
  const head = el('header', 'dash-head');
  const wins = WINDOWS.map((w) => {
    const b = el('button', 'btn dash-win', w);
    b.type = 'button';
    b.dataset.window = w;
    b.title = `Show the last ${w} (w cycles)`;
    return b;
  });
  const badge = el('span', 'dash-incomplete');
  badge.hidden = true;
  badge.setAttribute('role', 'status');
  const btn = (label: string, action: string, title?: string): HTMLButtonElement => {
    const b = el('button', 'btn', label);
    b.type = 'button';
    b.dataset.dash = action;
    if (title) b.title = title;
    return b;
  };
  // Every keyed action has a button (local #230): the key is secondary text, dropped with Vim keys off.
  head.append(
    el('span', 'dash-title', 'Dashboard: all projects'), ...wins, badge, el('span', 'dash-meta'),
    btn('Open project (Enter)', 'open'), btn('Manager workspace (gm)', 'workspace', 'Open the manager workspace (gm)'),
    btn('Sort (s)', 'sort', 'Change what the projects are ordered by (s)'),
    btn('Show/hide manager pane (t)', 'pane', 'Show or hide the top-level manager pane (t)'),
    btn('Refresh (r)', 'refresh'), btn('Close', 'close', 'Close the dashboard (Esc)'));
  const wrap = el('div', 'dash-table-wrap');
  wrap.tabIndex = 0;
  const main = el('div', 'dash-main');
  main.append(wrap, el('div', 'dash-detail'));
  const body = el('div', 'dash-body');
  body.append(main, el('aside', 'dash-pane'));
  o.append(head, el('div', 'dash-status'), body, el('div', 'dash-hints'));
  document.body.append(o);
  root = o;
  o.addEventListener('click', (ev) => {
    const t = ev.target as HTMLElement;
    const b = t.closest<HTMLElement>('button');
    if (b?.dataset.window) { dv.window = b.dataset.window as DashboardWindow; void refresh(); return; }
    if (b?.dataset.dash) { dashboardKey(b.dataset.dash as DashboardKeyAction); return; }
    const tr = t.closest<HTMLElement>('.dash-row');
    if (tr?.dataset.index) { move(Number(tr.dataset.index)); wrap.focus(); }
  });
  wrap.addEventListener('dblclick', (ev) => { if ((ev.target as HTMLElement).closest('.dash-row')) openProject(); });
  return o;
}

function isTyping(t: HTMLElement | null): boolean {
  return !!t?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
}

export function initDashboard(): void {
  const style = el('style');
  style.textContent = CSS;
  document.head.append(style);
  window.setInterval(() => { if (dv.open && !dv.loading && !document.querySelector('dialog[open]')) void refresh(); }, AUTO_REFRESH_MS);
  window.addEventListener('resize', () => { if (dv.open) render(); });
  // The Vim keys switch flips <html data-vim-keys>; the lines that name keys are read again then.
  if (typeof MutationObserver !== 'undefined' && document.documentElement) {
    new MutationObserver(() => { if (dv.open) render(); }).observe(document.documentElement, { attributes: true, attributeFilter: ['data-vim-keys'] });
  }
  // Capture phase: the open dashboard takes its keys before the global key engine and the managers view.
  window.addEventListener('keydown', (ev) => {
    if (!dv.open || !root) return;
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!paneKeyAllowed(isVimKeysOn(), ev)) return;
    if (document.querySelector('dialog[open]') || ev.isComposing) return;
    const log = root.querySelector<HTMLElement>('.dash-conv-log');
    if (log && document.activeElement === log) {
      const half = Math.max(40, log.clientHeight / 2);
      const scroll: Record<string, number> = { j: 40, ArrowDown: 40, k: -40, ArrowUp: -40, PageDown: log.clientHeight, PageUp: -log.clientHeight };
      if (ev.ctrlKey && (ev.key === 'd' || ev.key === 'u')) log.scrollBy(0, ev.key === 'd' ? half : -half);
      else if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
      else if (ev.key in scroll) log.scrollBy(0, scroll[ev.key]!);
      else if (ev.key === 'g') log.scrollTop = 0;
      else if (ev.key === 'G') log.scrollTop = log.scrollHeight;
      else if (ev.key === 'i') void typeToSeat();
      else if (paneKeyName(ev) === 'Tab' || ev.key === 'Escape') { part('dash-table-wrap').focus(); dv.notice = null; status(); }
      else return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      return;
    }
    if (ev.ctrlKey || ev.metaKey || ev.altKey || isTyping(document.activeElement as HTMLElement | null)) return;
    if (ev.key === 'Enter' && (document.activeElement as HTMLElement | null)?.closest('button')) return;
    // `g` then `m` opens the manager workspace (gm); `g` then `d` refreshes (gd again).
    if (dv.pendingG && (ev.key === 'm' || ev.key === 'd')) {
      dv.pendingG = false;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      if (ev.key === 'd') { void refresh(); return; }
      dashboardKey('workspace');
      return;
    }
    dv.pendingG = ev.key === 'g';
    const action = DASHBOARD_KEYS[paneKeyName(ev)];
    if (!action) return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
    dashboardKey(action);
  }, true);
}
