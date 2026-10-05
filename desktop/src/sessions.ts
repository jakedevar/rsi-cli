// All text is rendered with textContent; no daemon string is ever interpreted as HTML.
// Daemon health, project and session list, selected-session header.
import { api } from './api.js';
import { renderApproval, renderControls, updateAttention } from './approvals.js';
import { refreshEvents } from './conversation.js';
import { $, el, toast } from './dom.js';
import { isActive, sessionLabel } from './model.js';
import { collapsedIds, renderSessionTree } from './sessionTree.js';
import { RENDER_PAGE, currentSessions, isReadOnly, projectName, selected, state } from './state.js';
import { buildTree, flattenTree } from './tree.js';

const ARCHIVED_POLL_MS = 15000;

export async function refreshHealth(): Promise<void> {
  const conn = $('conn');
  try {
    const info = await api.daemonInfo();
    conn.textContent = info.connected ? 'daemon connected' : 'daemon unreachable';
    conn.className = `conn ${info.connected ? 'conn-ok' : 'conn-down'}`;
    conn.title = info.connected ? info.socket : `${info.socket}\n${info.error ?? ''}`;
  } catch (e) {
    conn.textContent = 'backend error';
    conn.className = 'conn conn-down';
    conn.title = String(e);
  }
}

export async function refreshProjects(): Promise<void> {
  try {
    state.projects = await api.listProjects();
  } catch {
    return;
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
}

function filter() {
  return {
    projectId: $<HTMLSelectElement>('project-filter').value || null,
    text: $<HTMLInputElement>('search').value,
    showFinished: $<HTMLInputElement>('show-finished').checked,
  };
}

export async function refreshSessions(): Promise<void> {
  if (state.sessionsInFlight) { state.sessionsDirty = true; return; }
  state.sessionsInFlight = true;
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
  } finally {
    state.sessionsInFlight = false;
    if (state.sessionsDirty) { state.sessionsDirty = false; void refreshSessions(); }
  }
}

export async function refreshArchived(): Promise<void> {
  try {
    state.archived = await api.listArchivedSessions();
    state.archivedFetchedAt = Date.now();
  } catch (e) {
    toast(`Could not load archived sessions: ${String(e)}`);
  }
}

export function renderSessionList(): void {
  const f = filter();
  const forest = buildTree(currentSessions(), { ...f, mode: state.view });
  const rows = flattenTree(forest, collapsedIds(), { forceExpand: f.text.trim() !== '' });
  renderSessionTree($('session-list'), rows, {
    selectedId: state.selectedId,
    projectName,
    onToggle: renderSessionList,
  });
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
  $('session-view').hidden = !state.selectedId;
  renderApproval(s);
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
  if (state.selectedId === id) return;
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
