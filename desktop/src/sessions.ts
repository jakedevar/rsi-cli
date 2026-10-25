// All text is rendered with textContent; no daemon string is ever interpreted as HTML.
// Daemon health, project and session list, selected-session header.
import { api, type Session } from './api.js';
import { explainNeedsRsid, renderApproval, renderControls, updateAttention } from './approvals.js';
import { PENDING_ARCHIVE_NOTE, archivePending } from './approvals-model.js';
import { refreshEvents, resetScroll } from './conversation.js';
import { $, el, errorTitle, toast } from './dom.js';
import { copyText } from './commands.js';
import { ARCHIVED_PLACEHOLDER, DEPARTED_PLACEHOLDER, continuePlaceholder, sendPlaceholder } from './keylabel.js';
import { currentPlatform } from './vimkeys.js';
import { activityFirst, reuseUnchanged, scopeHeader, sessionComparator } from './listmodel.js';
import { SHOW_FINISHED_KEY, emptyListView, filterDelayMs, filterRenderPlan, isActive, parseShowFinished, serializeShowFinished, sessionLabel, statusLabel } from './model.js';
import { headMeta } from './session-head-model.js';
import { keepFocusIn } from './focus-return.js';
import { renderSecondary } from './session-secondary.js';
import { isFinishedStatus } from './session-outcome-model.js';
import { renderOutcome } from './session-outcome.js';
import { collapsedIds, renderSessionTree } from './sessionTree.js';
import { RENDER_PAGE, currentSessions, isDeparted, isReadOnly, projectName, selected, state, uiChanged } from './state.js';
import { focusDaemonHelp, onDaemonDownChange, renderOnboarding, setDaemonLink } from './boot.js';
import { STALE_NOTE, type DaemonLink } from './onboarding-model.js';
import { buildTree, countHiddenMatches, countZoneSessions, flattenTree, type TreeNode, type TreeRow } from './tree.js';
import { projectLabel, sortProjects } from './project-model.js';

const ARCHIVED_POLL_MS = 15000;

/** Poll the daemon link; the result also drives the first-run guidance (boot.ts). */
export async function refreshHealth(): Promise<DaemonLink> {
  const conn = $('conn');
  let link: DaemonLink;
  try {
    const info = await api.daemonInfo();
    conn.textContent = info.connected ? 'daemon connected' : 'daemon unreachable';
    conn.className = `conn ${info.connected ? 'conn-ok' : 'conn-down'}`;
    conn.title = info.connected ? info.socket : `${info.socket}\n${errorTitle(info.error ?? '')}`;
    link = info.connected
      ? { kind: 'connected', socket: info.socket }
      : { kind: 'unreachable', socket: info.socket, ...(info.error ? { error: info.error } : {}) };
  } catch (e) {
    conn.textContent = 'backend error';
    conn.className = 'conn conn-down';
    conn.title = errorTitle(String(e));
    link = { kind: 'backend-error', error: String(e) };
  }
  setDaemonLink(link);
  makePillHelp(conn, link.kind !== 'connected');
  return link;
}

/** While rsid cannot be reached the pill is a button that moves to the Start rsid help (local #266). */
function makePillHelp(conn: HTMLElement, down: boolean): void {
  if (down) {
    conn.setAttribute('role', 'button');
    conn.tabIndex = 0;
    conn.dataset.help = '';
  } else {
    conn.removeAttribute('role');
    conn.removeAttribute('tabindex');
    delete conn.dataset.help;
  }
}

/** Wire the pill once: a click, Enter or Space opens the help while it is a button. */
export function wireConnPill(): void {
  const conn = $('conn');
  conn.addEventListener('click', () => { if (conn.dataset.help !== undefined) focusDaemonHelp(); });
  conn.addEventListener('keydown', (ev) => {
    if (conn.dataset.help === undefined || (ev.key !== 'Enter' && ev.key !== ' ')) return;
    ev.preventDefault();
    focusDaemonHelp();
  });
}

// The open session's header and controls change with the link (local #266).
onDaemonDownChange(() => renderHeader());

/** null when loaded, else the error text (boot reports it). */
export async function refreshProjects(): Promise<string | null> {
  try {
    state.projects = sortProjects(await api.listProjects());
  } catch (e) {
    return String(e);
  }
  for (const id of ['project-filter']) {
    const sel = $<HTMLSelectElement>(id);
    const keep = sel.value;
    sel.replaceChildren(el('option', undefined, 'All projects'));
    (sel.firstChild as HTMLOptionElement).value = '';
    for (const p of state.projects) {
      const o = el('option', undefined, projectLabel(p));
      o.value = p.id;
      sel.append(o);
    }
    sel.value = keep;
  }
  // First-run guidance depends on whether any project exists.
  if (!state.selectedId) renderOnboarding();
  return null;
}

function filter() {
  return {
    projectId: $<HTMLSelectElement>('project-filter').value || null,
    text: $<HTMLInputElement>('search').value,
    showFinished: $<HTMLInputElement>('show-finished').checked,
    keepId: state.selectedId,
    zone: state.zone,
  };
}

function prefStorage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

/** Restore the remembered Show completed choice (default on) and remember every later change. */
export function initShowFinished(): void {
  const box = $<HTMLInputElement>('show-finished');
  let raw: string | null = null;
  try { raw = prefStorage()?.getItem(SHOW_FINISHED_KEY) ?? null; } catch { raw = null; }
  box.checked = parseShowFinished(raw);
  box.addEventListener('change', () => storeShowFinished(box.checked));
}

function storeShowFinished(on: boolean): void {
  try { prefStorage()?.setItem(SHOW_FINISHED_KEY, serializeShowFinished(on)); } catch { /* no storage: this window only */ }
}

/** The Show completed checkbox, set from a command (it is remembered like a click). */
export function setShowFinished(on: boolean): void {
  $<HTMLInputElement>('show-finished').checked = on;
  storeShowFinished(on);
  renderSessionList();
}

export async function refreshSessions(): Promise<string | null> {
  if (state.sessionsInFlight) { state.sessionsDirty = true; return null; }
  state.sessionsInFlight = true;
  let failure: string | null = null;
  try {
    state.sessions = reuseUnchanged(state.sessions, await api.listSessions());
    await adoptDeparted();
    if (state.view === 'archived' && Date.now() - state.archivedFetchedAt > ARCHIVED_POLL_MS) {
      await refreshArchived();
    }
    renderSessionList();
    renderHeader();
    updateAttention(state.sessions);
  } catch (e) {
    $('conn').className = 'conn conn-down';
    $('conn').textContent = 'daemon unreachable';
    $('conn').title = errorTitle(String(e));
    failure = String(e);
  } finally {
    state.sessionsInFlight = false;
    if (state.sessionsDirty) { state.sessionsDirty = false; void refreshSessions(); }
  }
  return failure;
}

/**
 * The open session is no longer in the Active list. When rsid says it is archived or deleted (the TUI or an agent did it),
 * the pane shows that read-only instead of keeping its last drawing (local #309). Any other answer leaves things as they are:
 * a session just launched may not be in the list yet.
 */
async function adoptDeparted(): Promise<void> {
  const id = state.selectedId;
  if (!id || state.view !== 'active' || state.sessions.some((x) => x.id === id)) { state.departed = null; return; }
  if (state.departed?.id === id) return;
  try {
    const real = await api.getSession(id);
    if (state.selectedId !== id || state.view !== 'active' || state.sessions.some((x) => x.id === id)) return;
    if (real.status === 'Archived' || real.status === 'Deleted') state.departed = real;
  } catch { /* rsid cannot say: keep what is drawn; the next poll asks again */ }
}

export async function refreshArchived(): Promise<void> {
  try {
    state.archived = await api.listArchivedSessions();
    state.archivedFetchedAt = Date.now();
  } catch (e) {
    toast(`Could not load archived sessions: ${String(e)}`);
  }
}

let lastRows: TreeRow[] = [];
let lastContainers: string[] = [];

/** Every session in the shown forest that has children, folded or not (zM closes them all). */
export function visibleContainers(): readonly string[] {
  return lastContainers;
}

function containerIds(forest: TreeNode[]): string[] {
  const out: string[] = [];
  const walk = (n: TreeNode): void => {
    if (n.children.length > 0) out.push(n.session.id);
    n.children.forEach(walk);
  };
  forest.forEach(walk);
  return out;
}

export function labelById(id: string | null | undefined) {
  return id ? state.labels.find((l) => l.id === id) : undefined;
}

/** The "⇅ Longest idle · 20 sessions" line above the list. */
function renderScope(count: number): void {
  const scope = document.getElementById('list-scope');
  if (scope) scope.textContent = scopeHeader(state.sortOrder, count);
}

/** The rows the session list shows now, top to bottom (for vim-key motions). */
export function visibleRows(): readonly TreeRow[] {
  return lastRows;
}

export function renderSessionList(): void {
  pendingFilter = false;
  if (filterTimer !== null) { window.clearTimeout(filterTimer); filterTimer = null; }
  const f = filter();
  const order = {
    cmp: sessionComparator(state.sortOrder, (id) => labelById(id)?.name),
    activityFirst: activityFirst(state.sortOrder),
  };
  const tree = { ...f, mode: state.view, order };
  const forest = buildTree(currentSessions(), tree);
  const rows = flattenTree(forest, collapsedIds(), { forceExpand: f.text.trim() !== '' });
  lastRows = rows;
  lastContainers = containerIds(forest);
  renderScope(rows.length);
  const empty = rows.length === 0 ? emptyView(tree) : undefined;
  renderSessionTree($('session-list'), rows, {
    selectedId: state.selectedId,
    cursorId: state.cursorId,
    projectName,
    onToggle: renderSessionList,
    labelOf: (x) => labelById(x.group_id),
    groupHeaders: state.sortOrder === 'by-label',
    emptyText: empty?.text,
    emptyActions: empty?.actions,
    onEmptyAction: runEmptyAction,
    knownIds: new Set(currentSessions().map((x) => x.id)),
  });
  uiChanged();
  lastRenderEnd = performance.now();
}

/** The empty row's text and buttons, counting what the filters hide (local #272). */
function emptyView(tree: ReturnType<typeof filter> & { mode: 'active' | 'archived' }) {
  const archivedView = state.view === 'archived';
  const hidden = archivedView ? undefined : countHiddenMatches(state.sessions, state.archived, tree);
  if (!archivedView) probeArchived();
  return emptyListView({
    view: state.view,
    zone: state.zone,
    total: countZoneSessions(currentSessions(), { zone: state.zone, mode: state.view }),
    text: tree.text,
    projectOn: !!tree.projectId,
    finishedHidden: !tree.showFinished,
    hidden: hidden ?? { finished: 0, archived: 0, otherProjects: 0 },
  });
}

let archivedProbeAt = 0;

/** An empty Active list wants the archived matches too; fetch the Archived list quietly, at most once per poll period. */
function probeArchived(): void {
  const now = Date.now();
  if (state.zone === 'jobs' || now - state.archivedFetchedAt <= ARCHIVED_POLL_MS || now - archivedProbeAt <= ARCHIVED_POLL_MS) return;
  archivedProbeAt = now;
  void api.listArchivedSessions().then((list) => {
    state.archived = list;
    state.archivedFetchedAt = Date.now();
    renderSessionList();
  }, () => { /* the hint just omits archived matches */ });
}

function runEmptyAction(id: string): void {
  switch (id) {
    case 'show-finished': setShowFinished(true); return;
    case 'show-archived': void setView('archived'); return;
    case 'clear-filter': {
      const box = $<HTMLInputElement>('search');
      box.value = '';
      renderSessionList();
      box.focus();
      return;
    }
    case 'all-projects': $<HTMLSelectElement>('project-filter').value = ''; renderSessionList(); return;
  }
}

let filterTimer: number | null = null;
let pendingFilter = false;
let lastRenderEnd = 0;

/**
 * The filter box's input handler. A short list re-renders on every key. On a long one (about 80 ms a
 * render at 3,000 sessions, local #275) the first key after a pause renders at once and keys typed
 * while it is busy wait for a pause, so a burst costs two renders, not one per key.
 */
export function onFilterInput(): void {
  if (filterRenderPlan(currentSessions().length, performance.now() - lastRenderEnd, filterTimer !== null) === 'now') { renderSessionList(); return; }
  pendingFilter = true;
  if (filterTimer !== null) window.clearTimeout(filterTimer);
  filterTimer = window.setTimeout(renderSessionList, filterDelayMs(currentSessions().length));
}

/** Render a pending debounced filter now (Enter, leaving the box, a command that reads the list). */
export function flushFilter(): void {
  if (pendingFilter) renderSessionList();
}

export async function setView(view: 'active' | 'archived'): Promise<void> {
  if (state.view === view) return;
  state.view = view;
  $('view-active').classList.toggle('active', view === 'active');
  $('view-archived').classList.toggle('active', view === 'archived');
  $('view-active').setAttribute('aria-pressed', String(view === 'active'));
  $('view-archived').setAttribute('aria-pressed', String(view === 'archived'));
  $('show-finished').closest('label')!.hidden = view === 'archived';
  // The open session belongs to the other list; close it.
  state.selectedId = null;
  state.departed = null;
  state.cursorId = null;
  state.selectionEpoch += 1;
  state.events = [];
  $('events').replaceChildren();
  if (view === 'archived') await refreshArchived();
  renderSessionList();
  renderHeader();
}

/** Where focus goes when the control a keyboard user was on (Stop, Restore, Archive, a banner button) goes away. */
const HEADER_FOCUS_FALLBACKS = ['message', 'events', 'session-list'] as const;

export function renderHeader(): void {
  keepFocusIn($('session-view'), drawHeader, HEADER_FOCUS_FALLBACKS);
}

function drawHeader(): void {
  const s = selected();
  $('empty').hidden = !!state.selectedId;
  if (!state.selectedId) renderOnboarding();
  $('session-view').hidden = !state.selectedId;
  renderApproval(s);
  uiChanged();
  if (!s) return;
  $('s-title').textContent = sessionLabel(s);
  const hm = headMeta(s, projectName(s.project_id));
  $('s-meta').replaceChildren(
    el('span', 'status-chip', hm.status),
    ...(archivePending(s) ? [el('span', 'pending-archive-chip', PENDING_ARCHIVE_NOTE)] : []),
    ...(state.daemonDown ? [el('span', 'stale-note', STALE_NOTE)] : []),
    ...hm.parts.map((v) => el('span', '', v)),
  );
  renderSecondary($('s-secondary'), s, (text, what) => { void copyText(text, what); });
  const ro = isReadOnly(s);
  $<HTMLButtonElement>('interrupt').disabled = ro || !isActive(s) || state.daemonDown;
  explainNeedsRsid($('interrupt'), state.daemonDown);
  $('composer').classList.toggle('readonly', ro);
  const box = $<HTMLTextAreaElement>('message');
  box.disabled = ro;
  box.placeholder = ro ? (s.status === 'Archived' || !isDeparted(s) ? ARCHIVED_PLACEHOLDER : DEPARTED_PLACEHOLDER)
    : isFinishedStatus(s.status) ? continuePlaceholder(currentPlatform()) : sendPlaceholder(currentPlatform());
  for (const b of $('composer-actions').querySelectorAll('button')) b.disabled = ro;
  renderControls(s);
  renderOutcome();
}

export function select(id: string): void {
  state.cursorId = id;
  if (state.selectedId === id) { renderSessionList(); return; }
  state.selectedId = id;
  state.departed = null;
  state.selectionEpoch += 1;
  state.events = [];
  state.renderLimit = RENDER_PAGE;
  state.renderedCount = 0;
  state.renderedFirstSeq = undefined;
  resetScroll();
  $('events').replaceChildren();
  renderSessionList();
  renderHeader();
  void refreshEvents(true);
}
