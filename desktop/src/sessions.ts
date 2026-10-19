// All text is rendered with textContent; no daemon string is ever interpreted as HTML.
// Daemon health, project and session list, selected-session header.
import { api } from './api.js';
import { renderApproval, renderControls, updateAttention } from './approvals.js';
import { refreshEvents } from './conversation.js';
import { $, el, toast } from './dom.js';
import { activityFirst, scopeHeader, sessionComparator } from './listmodel.js';
import { isActive, sessionLabel } from './model.js';
import { collapsedIds, renderSessionTree } from './sessionTree.js';
import { RENDER_PAGE, currentSessions, isReadOnly, projectName, selected, state, uiChanged } from './state.js';
import { renderOnboarding, setDaemonLink } from './boot.js';
import type { DaemonLink } from './onboarding-model.js';
import { buildTree, flattenTree, type TreeNode, type TreeRow } from './tree.js';

const ARCHIVED_POLL_MS = 15000;

/** Poll the daemon link; the result also drives the first-run guidance (boot.ts). */
export async function refreshHealth(): Promise<DaemonLink> {
  const conn = $('conn');
  let link: DaemonLink;
  try {
    const info = await api.daemonInfo();
    conn.textContent = info.connected ? 'daemon connected' : 'daemon unreachable';
    conn.className = `conn ${info.connected ? 'conn-ok' : 'conn-down'}`;
    conn.title = info.connected ? info.socket : `${info.socket}\n${info.error ?? ''}`;
    link = info.connected
      ? { kind: 'connected', socket: info.socket }
      : { kind: 'unreachable', socket: info.socket, ...(info.error ? { error: info.error } : {}) };
  } catch (e) {
    conn.textContent = 'backend error';
    conn.className = 'conn conn-down';
    conn.title = String(e);
    link = { kind: 'backend-error', error: String(e) };
  }
  setDaemonLink(link);
  return link;
}

/** null when loaded, else the error text (boot reports it). */
export async function refreshProjects(): Promise<string | null> {
  try {
    state.projects = await api.listProjects();
  } catch (e) {
    return String(e);
  }
  for (const id of ['project-filter']) {
    const sel = $<HTMLSelectElement>(id);
    const keep = sel.value;
    sel.replaceChildren(el('option', undefined, 'All projects'));
    (sel.firstChild as HTMLOptionElement).value = '';
    for (const p of state.projects) {
      const o = el('option', undefined, p.name);
      o.value = p.id;
      sel.append(o);
    }
    sel.value = keep;
  }
  return null;
}

function filter() {
  return {
    projectId: $<HTMLSelectElement>('project-filter').value || null,
    text: $<HTMLInputElement>('search').value,
    showFinished: $<HTMLInputElement>('show-finished').checked,
    zone: state.zone,
  };
}

export async function refreshSessions(): Promise<string | null> {
  if (state.sessionsInFlight) { state.sessionsDirty = true; return null; }
  state.sessionsInFlight = true;
  let failure: string | null = null;
  try {
    state.sessions = await api.listSessions();
    if (state.view === 'archived' && Date.now() - state.archivedFetchedAt > ARCHIVED_POLL_MS) {
      await refreshArchived();
    }
    renderSessionList();
    renderHeader();
    updateAttention(state.sessions);
  } catch (e) {
    $('conn').className = 'conn conn-down';
    $('conn').textContent = 'daemon unreachable';
    $('conn').title = String(e);
    failure = String(e);
  } finally {
    state.sessionsInFlight = false;
    if (state.sessionsDirty) { state.sessionsDirty = false; void refreshSessions(); }
  }
  return failure;
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
  const f = filter();
  const order = {
    cmp: sessionComparator(state.sortOrder, (id) => labelById(id)?.name),
    activityFirst: activityFirst(state.sortOrder),
  };
  const forest = buildTree(currentSessions(), { ...f, mode: state.view, order });
  const rows = flattenTree(forest, collapsedIds(), { forceExpand: f.text.trim() !== '' });
  lastRows = rows;
  lastContainers = containerIds(forest);
  renderScope(rows.length);
  renderSessionTree($('session-list'), rows, {
    selectedId: state.selectedId,
    cursorId: state.cursorId,
    projectName,
    onToggle: renderSessionList,
    labelOf: (x) => labelById(x.group_id),
    groupHeaders: state.sortOrder === 'by-label',
  });
  uiChanged();
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
  state.cursorId = null;
  state.selectionEpoch += 1;
  state.events = [];
  $('events').replaceChildren();
  if (view === 'archived') await refreshArchived();
  renderSessionList();
  renderHeader();
}

export function renderHeader(): void {
  const s = selected();
  $('empty').hidden = !!state.selectedId;
  if (!state.selectedId) renderOnboarding();
  $('session-view').hidden = !state.selectedId;
  renderApproval(s);
  uiChanged();
  if (!s) return;
  $('s-title').textContent = sessionLabel(s);
  const meta = $('s-meta');
  const parts: Array<[string, string, boolean?]> = [
    ['status', s.status],
    ['provider', s.provider],
    ['model', s.model ?? 'default'],
    ['project', projectName(s.project_id) ?? '—'],
    ['branch', s.git_branch ?? '—', true],
    ['dir', s.working_dir, true],
    ['id', s.id, true],
  ];
  meta.replaceChildren(...parts.map(([k, v, mono]) => {
    const span = el('span');
    span.append(`${k}: `, el('span', mono ? 'mono' : '', v));
    return span;
  }));
  const ro = isReadOnly(s);
  $<HTMLButtonElement>('interrupt').disabled = ro || !isActive(s);
  $('composer').classList.toggle('readonly', ro);
  const box = $<HTMLTextAreaElement>('message');
  box.disabled = ro;
  box.placeholder = ro ? 'Archived session (read-only)' : 'Message this session (Ctrl+Enter to send)';
  $('composer').querySelector('button')!.disabled = ro;
  renderControls(s);
}

export function select(id: string): void {
  state.cursorId = id;
  if (state.selectedId === id) { renderSessionList(); return; }
  state.selectedId = id;
  state.selectionEpoch += 1;
  state.events = [];
  state.renderLimit = RENDER_PAGE;
  state.renderedCount = 0;
  state.renderedFirstSeq = undefined;
  $('events').replaceChildren();
  renderSessionList();
  renderHeader();
  void refreshEvents(true);
}
