// The DOM `run` side of the slice-4 actions (actions/sessionlist.ts): project
// and label pickers and forms, sort, folds, hierarchy, attention queue, search
// and session info. commands.ts calls runSessionList() for any action id its
// own switch does not handle. Daemon text is set with textContent only.
import { type Project, type Session, type SessionLabel, invoke } from '../api.js';
import { confirmDialog } from '../approvals.js';
import { $, toast } from '../dom.js';
import {
  type FoldOp, LABEL_COLORS, PROJECT_COLORS, SORT_ORDERS, applyFold, ascendTarget, attentionQueue, attentionSlot,
  findProject, labelBoundary, labelCounts, matchIndices, parentChoices, parseProjectArg, parseSortOrder,
  sessionInfoRows, sortInfo, stepMatch, validateLabel, validateProject,
} from '../listmodel.js';
import { type PickItem, openForm, openInfo, openPicker } from '../pickers.js';
import type { ActionId } from '../registry.js';
import {
  refreshProjects, refreshSessions, renderSessionList, select,
  visibleContainers, visibleRows,
} from '../sessions.js';
import { collapsedIds, setCollapsed } from '../sessionTree.js';
import { currentSessions, selected, state } from '../state.js';
import { type SliceRuntime, dispatch } from '../commands.js';
import { type ListSnapshot, SLICE } from './sessionlist.js';
import { focusOwnsKey } from '../modal-gate.js';

/** The slice of commands.ts's Invocation these actions read. */
interface Inv {
  count?: number | null;
  arg?: string;
}

const SORT_KEY = 'rsi.desktop.sortOrder';

const listApi = {
  createProject: (name: string, path: string | null, color: string) =>
    invoke<Project>('create_project', { name, path, color }),
  updateProject: (id: string, name: string, path: string | null, color: string) =>
    invoke<Project>('update_project', { id, name, path, color }),
  deleteProject: (id: string) => invoke<unknown>('delete_project', { id }),
  listLabels: () => invoke<SessionLabel[]>('list_labels'),
  createLabel: (name: string, description: string | null, color: string) =>
    invoke<SessionLabel>('create_label', { name, description, color }),
  updateLabel: (id: string, name: string, description: string | null, color: string) =>
    invoke<SessionLabel>('update_label', { id, name, description, color }),
  deleteLabel: (id: string) => invoke<unknown>('delete_label', { id }),
  setSessionLabel: (sessionId: string, groupId: string | null) =>
    invoke<unknown>('update_session_label', { sessionId, groupId }),
  setSessionProject: (sessionId: string, projectId: string | null) =>
    invoke<unknown>('update_session_project', { sessionId, projectId }),
  setSessionParent: (sessionId: string, newParentId: string | null) =>
    invoke<unknown>('set_session_parent', { sessionId, newParentId }),
};

// ---------- state the rules read ----------

let lastPane: 'list' | 'detail' = 'list';

function cursor(): string | null {
  return state.cursorId ?? state.selectedId;
}

export function listSnapshot(): ListSnapshot {
  const rows = visibleRows();
  const cur = cursor();
  const row = rows.find((r) => r.session.id === cur);
  const first = rows[0]?.session.group_id ?? null;
  const needle = $<HTMLInputElement>('search').value;
  return {
    cursorFoldable: !!row?.hasChildren,
    cursorHasParent: ascendTarget(rows, cur) !== null,
    foldable: visibleContainers().length,
    paneDetail: lastPane === 'detail' && selected() !== undefined,
    attentionQueue: attentionQueue(rows).length,
    search: needle,
    detailSearchable: selected() !== undefined && needle.trim() !== '',
    projects: state.projects.length,
    labelGroups: rows.some((r) => (r.session.group_id ?? null) !== first),
    openHasParent: !!selected()?.parent_id,
  };
}

// ---------- shared helpers ----------

function moveCursor(id: string | null): void {
  if (!id) return;
  state.cursorId = id;
  renderSessionList();
  for (const li of $('session-list').querySelectorAll<HTMLElement>('.session-item')) {
    if (li.dataset.id === id) { li.scrollIntoView({ block: 'nearest' }); break; }
  }
}

function failed(what: string, e: unknown): void {
  toast(`${what}: ${e instanceof Error ? e.message : String(e)}`);
}

/** null when loaded, else the error text (boot reports it). */
export async function refreshLabels(): Promise<string | null> {
  try {
    state.labels = await listApi.listLabels();
  } catch (e) {
    return String(e);
  }
  renderSessionList();
  return null;
}

function projectFilter(): string {
  return $<HTMLSelectElement>('project-filter').value;
}

function setProjectFilter(id: string): void {
  $<HTMLSelectElement>('project-filter').value = id;
  renderSessionList();
  toast(id ? `Project: ${state.projects.find((p) => p.id === id)?.name ?? id}` : 'All projects', true);
}

const projectItem = (p: Project, current: boolean): PickItem => ({
  id: p.id, label: p.name, color: p.color, ...(p.path ? { detail: p.path } : {}), current,
});

// ---------- projects ----------

function openProjectForm(p: Project | null): void {
  openForm({
    title: p ? 'Edit project' : 'New project',
    submit: 'Save',
    fields: [
      { name: 'name', label: 'Name', value: p?.name ?? '', type: 'text' },
      { name: 'path', label: 'Path', value: p?.path ?? '', type: 'text', placeholder: 'optional' },
      { name: 'color', label: 'Color', value: (p?.color ?? PROJECT_COLORS[0]!.hex).toLowerCase(), type: 'color', palette: PROJECT_COLORS },
    ],
    validate: (v) => validateProject({ name: v.name ?? '', path: v.path ?? '', color: v.color ?? '' }),
    onSubmit: async (v) => {
      const path = (v.path ?? '').trim() || null;
      const made = p
        ? await listApi.updateProject(p.id, v.name!, path, v.color!)
        : await listApi.createProject(v.name!, path, v.color!);
      await refreshProjects();
      void refreshSessions();
      toast(`${p ? 'Saved' : 'Created'} project ${made.name}`, true);
    },
  });
}

async function deleteProject(p: Project): Promise<void> {
  if (!(await confirmDialog(`Delete project “${p.name}”?`, 'Delete'))) return;
  try {
    await listApi.deleteProject(p.id);
    if (projectFilter() === p.id) $<HTMLSelectElement>('project-filter').value = '';
    await refreshProjects();
    void refreshSessions();
    toast(`Deleted project ${p.name}`, true);
  } catch (e) {
    failed('Delete project failed', e);
  }
}

/** A project picker that hands the choice to `then`. */
function pickProject(title: string, then: (p: Project) => void): void {
  if (state.projects.length === 0) { toast('There are no projects yet'); return; }
  openPicker({
    title,
    hint: 'Type to filter · Enter choose · Esc cancel',
    filterable: true,
    items: state.projects.map((p) => projectItem(p, false)),
    onPick: (it) => {
      const p = state.projects.find((x) => x.id === it.id);
      if (p) then(p);
    },
  });
}

function projectPicker(): void {
  const cur = projectFilter();
  const projectOf = (it: PickItem | null): Project | undefined => state.projects.find((p) => p.id === it?.id);
  openPicker({
    title: 'Choose project',
    hint: 'Type to filter · Enter show that project · Esc cancel',
    filterable: true,
    items: [
      { id: '', label: 'All projects', current: cur === '' },
      ...state.projects.map((p) => projectItem(p, p.id === cur)),
    ],
    emptyText: 'No project matches',
    buttons: [
      { text: 'New', ctrl: 'n', run: () => { openProjectForm(null); } },
      { text: 'Edit', ctrl: 'e', needsItem: 'Pick a project to edit', run: (it) => {
        const p = projectOf(it);
        if (p) openProjectForm(p);
      } },
      { text: 'Delete', ctrl: 'd', danger: true, needsItem: 'Pick a project to delete', run: (it) => {
        const p = projectOf(it);
        if (p) void deleteProject(p);
      } },
    ],
    onPick: (it) => setProjectFilter(it.id),
  });
}

function runProject(id: 'choose-project' | 'project-new' | 'project-edit' | 'project-delete', inv: Inv): void {
  const arg = inv.arg?.trim() ?? '';
  switch (id) {
    case 'choose-project': {
      if (arg === '') { projectPicker(); return; }
      const r = findProject(state.projects, arg);
      if ('error' in r) toast(r.error); else setProjectFilter(r.project.id);
      return;
    }
    case 'project-new': {
      if (arg === '') { openProjectForm(null); return; }
      const { name, path } = parseProjectArg(arg);
      void listApi.createProject(name, path || null, PROJECT_COLORS[0]!.hex)
        .then(async (p) => { await refreshProjects(); toast(`Created project ${p.name}`, true); })
        .catch((e: unknown) => failed('Create project failed', e));
      return;
    }
    case 'project-edit': {
      if (arg !== '') {
        const r = findProject(state.projects, arg);
        if ('error' in r) toast(r.error); else openProjectForm(r.project);
        return;
      }
      const cur = state.projects.find((p) => p.id === projectFilter());
      if (cur) openProjectForm(cur); else pickProject('Edit which project?', openProjectForm);
      return;
    }
    case 'project-delete': {
      if (arg !== '') {
        const r = findProject(state.projects, arg);
        if ('error' in r) toast(r.error); else void deleteProject(r.project);
        return;
      }
      pickProject('Delete which project?', (p) => void deleteProject(p));
    }
  }
}

function changeSessionProject(s: Session): void {
  openPicker({
    title: 'Change session project',
    hint: 'Type to filter · Enter move the session · Esc cancel',
    filterable: true,
    items: [
      { id: '', label: '(no project)', current: !s.project_id },
      ...state.projects.map((p) => projectItem(p, p.id === s.project_id)),
    ],
    onPick: (it) => {
      listApi.setSessionProject(s.id, it.id || null)
        .then(() => { void refreshSessions(); toast(it.id ? `Moved to ${it.label}` : 'Removed from its project', true); })
        .catch((e: unknown) => failed('Change project failed', e));
    },
  });
}

// ---------- labels ----------

function openLabelForm(l: SessionLabel | null, done: () => void): void {
  openForm({
    title: l ? 'Edit label' : 'New label',
    submit: 'Save',
    fields: [
      { name: 'name', label: 'Name', value: l?.name ?? '', type: 'text' },
      { name: 'description', label: 'Description', value: l?.description ?? '', type: 'text', placeholder: 'optional' },
      { name: 'color', label: 'Color', value: (l?.color ?? LABEL_COLORS[0]!.hex).toLowerCase(), type: 'color', palette: LABEL_COLORS },
    ],
    validate: (v) => validateLabel({ name: v.name ?? '', description: v.description ?? '', color: v.color ?? '' }),
    onSubmit: async (v) => {
      const desc = (v.description ?? '').trim() || null;
      if (l) await listApi.updateLabel(l.id, v.name!, desc, v.color!);
      else await listApi.createLabel(v.name!, desc, v.color!);
      await refreshLabels();
      window.setTimeout(done, 0); // after the form has closed
    },
  });
}

function labelPicker(s: Session): void {
  const counts = labelCounts(currentSessions());
  const labelOf = (it: PickItem | null): SessionLabel | undefined => state.labels.find((l) => l.id === it?.id);
  const reopen = (): void => { const again = selected(); if (again) labelPicker(again); };
  const sorted = [...state.labels].sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
  openPicker({
    title: 'Set session label',
    hint: 'Type to filter · Enter assign · Esc cancel',
    filterable: true,
    items: [
      ...sorted.map((l): PickItem => ({
        id: l.id, label: l.name, color: l.color, current: l.id === s.group_id,
        detail: `${counts.get(l.id) ?? 0} session${(counts.get(l.id) ?? 0) === 1 ? '' : 's'}${l.description ? ` · ${l.description}` : ''}`,
      })),
      { id: '', label: '(none)', current: !s.group_id, detail: `${counts.get('') ?? 0} unlabeled` },
    ],
    buttons: [
      { text: 'New', ctrl: 'n', run: () => { openLabelForm(null, reopen); } },
      { text: 'Edit', ctrl: 'e', needsItem: 'Pick a label to edit', run: (it) => {
        const l = labelOf(it);
        if (l) openLabelForm(l, reopen);
      } },
      { text: 'Delete', ctrl: 'd', danger: true, needsItem: 'Pick a label to delete', run: (it) => {
        const l = labelOf(it);
        if (!l) return;
        void (async () => {
          if (!(await confirmDialog(`Delete label “${l.name}”?`, 'Delete'))) return;
          try {
            await listApi.deleteLabel(l.id);
            await refreshLabels();
            void refreshSessions();
            toast(`Deleted label ${l.name}`, true);
          } catch (e) {
            failed('Delete label failed', e);
          }
        })();
      } },
    ],
    onPick: (it) => {
      listApi.setSessionLabel(s.id, it.id || null)
        .then(() => { void refreshSessions(); toast(it.id ? `Label: ${it.label}` : 'Label cleared', true); })
        .catch((e: unknown) => failed('Set label failed', e));
    },
  });
}

// ---------- sort, info, hierarchy ----------

function sortPicker(): void {
  openPicker({
    title: 'Sort sessions',
    hint: 'j/k or arrows choose · Enter apply · Esc cancel',
    vimKeys: true,
    items: SORT_ORDERS.map((o) => ({ id: o.id, label: o.label, detail: o.description, current: o.id === state.sortOrder })),
    onPick: (it) => {
      state.sortOrder = parseSortOrder(it.id);
      try { window.localStorage.setItem(SORT_KEY, state.sortOrder); } catch { /* storage unavailable */ }
      renderSessionList();
      toast(`Sorted: ${sortInfo(state.sortOrder).label}`, true);
    },
  });
}

function sessionInfo(s: Session): void {
  openInfo({
    title: 'Session info',
    rows: sessionInfoRows(s, state.projects, state.labels),
    hint: 'R rating · G label · Esc close',
    buttons: [
      { text: 'Rating (R)', key: 'R', run: () => dispatch('rate-session', { source: 'key' }) },
      { text: 'Label (G)', key: 'G', run: () => labelPicker(s) },
    ],
  });
}

function parentPicker(s: Session): void {
  openPicker({
    title: 'Move under a Group or Epic',
    hint: 'Type to filter · Enter move · Esc cancel',
    filterable: true,
    items: parentChoices(currentSessions(), s.id).map((c) => ({ id: c.id ?? '', label: c.label, current: c.current })),
    onPick: (it) => {
      listApi.setSessionParent(s.id, it.id || null)
        .then(() => { void refreshSessions(); toast(it.id ? 'Moved' : 'Moved to the top level', true); })
        .catch((e: unknown) => failed('Move failed', e));
    },
  });
}

// ---------- folds ----------

// The list folds only: commands.ts sends a fold key here when the list has the
// keys and to slice 5's transcript folds otherwise (actions/folds.ts).
function fold(op: FoldOp): void {
  const r = applyFold(op, collapsedIds(), cursor(), visibleContainers());
  if (!r.applied) { toast('The cursor is not on a session with children'); return; }
  setCollapsed(r.collapsed);
  renderSessionList();
}

// ---------- attention queue ----------

function attentionPicker(): void {
  const queue = attentionQueue(visibleRows());
  openPicker({
    title: 'Attention queue',
    hint: 'Enter open · Esc close',
    emptyText: 'No session is waiting for you',
    items: queue.map((s, i) => ({
      id: s.id,
      label: `${i + 1}. ${s.title?.trim() || s.query.split('\n')[0]?.trim().slice(0, 80) || s.id.slice(0, 8)}`,
      detail: s.pending_question ? 'Question' : s.status,
    })),
    onPick: (it) => { moveCursor(it.id); select(it.id); },
  });
}

// ---------- search ----------

let searchAt: number | null = null;
let searchEpoch = -1;
let searchNeedle = '';

function stepSearch(dir: 1 | -1): void {
  const needle = $<HTMLInputElement>('search').value;
  if (state.selectionEpoch !== searchEpoch || needle !== searchNeedle) {
    searchAt = null;
    searchEpoch = state.selectionEpoch;
    searchNeedle = needle;
  }
  const evs = [...$('events').querySelectorAll<HTMLElement>('.ev')];
  const matches = matchIndices(evs.map((e) => e.textContent ?? ''), needle);
  const next = stepMatch(matches, searchAt, dir);
  for (const e of evs) e.classList.remove('search-hit');
  if (next === null) { toast('No match in this conversation'); return; }
  searchAt = next;
  const hit = evs[next]!;
  hit.classList.add('search-hit');
  hit.scrollIntoView({ block: 'center' });
  toast(`Match ${matches.indexOf(next) + 1} of ${matches.length}`, true);
}

function clearSearch(): void {
  const box = $<HTMLInputElement>('search');
  box.value = '';
  box.blur();
  renderSessionList();
}

// ---------- dispatch ----------

/** Run a slice-4 action. False when `id` is not one of them. */
export function runSessionList(id: ActionId, inv: Inv): boolean {
  const s = selected();
  switch (id) {
    case 'choose-project': case 'project-new': case 'project-edit': case 'project-delete':
      runProject(id, inv);
      return true;
    case 'change-session-project': if (s) changeSessionProject(s); return true;
    case 'choose-sort': sortPicker(); return true;
    case 'choose-label': if (s) labelPicker(s); return true;
    case 'session-info': if (s) sessionInfo(s); return true;
    case 'ascend': moveCursor(ascendTarget(visibleRows(), cursor())); return true;
    case 'fold-open': fold('open'); return true;
    case 'fold-close': fold('close'); return true;
    case 'fold-toggle': fold('toggle'); return true;
    case 'fold-open-all': fold('open-all'); return true;
    case 'fold-close-all': fold('close-all'); return true;
    case 'label-next': case 'label-prev': {
      const to = labelBoundary(visibleRows(), cursor(), id === 'label-next' ? 1 : -1);
      if (to) moveCursor(to); else toast('No more label groups');
      return true;
    }
    case 'move-to-parent': if (s) parentPicker(s); return true;
    case 'move-to-root':
      if (s) {
        listApi.setSessionParent(s.id, null)
          .then(() => { void refreshSessions(); toast('Moved to the top level', true); })
          .catch((e: unknown) => failed('Move failed', e));
      }
      return true;
    case 'attention-list': attentionPicker(); return true;
    case 'attention-slot': {
      const n = inv.arg ? Number(inv.arg) : inv.count ?? 1;
      const hit = attentionSlot(attentionQueue(visibleRows()), n);
      if (hit) { moveCursor(hit.id); select(hit.id); } else toast(`No session in attention slot ${n}`);
      return true;
    }
    case 'search-next':
    case 'search-prev': {
      const dir = id === 'search-next' ? 1 : -1;
      if (s) { stepSearch(dir); return true; }
      // No conversation open: the filtered rows are the matches (G2).
      const ids = visibleRows().map((r) => r.session.id);
      const at = ids.indexOf(cursor() ?? '');
      const to = ids[((at < 0 ? (dir > 0 ? -1 : 0) : at) + dir + ids.length) % Math.max(1, ids.length)];
      if (to) moveCursor(to); else toast('No session matches the filter');
      return true;
    }
    case 'clear-search': clearSearch(); return true;
    default: return false;
  }
}

// ---------- wiring ----------

export function initSessionList(): void {
  try {
    state.sortOrder = parseSortOrder(window.localStorage.getItem(SORT_KEY));
  } catch { /* storage unavailable: keep the default */ }
  // Which pane the last pointer or focus landed in (ListSnapshot.paneDetail).
  const track = (t: EventTarget | null): void => {
    const node = t as HTMLElement | null;
    if (node?.closest?.('.detail')) lastPane = 'detail';
    else if (node?.closest?.('.sidebar')) lastPane = 'list';
  };
  document.addEventListener('pointerdown', (e) => track(e.target), true);
  document.addEventListener('focusin', (e) => track(e.target));
  const box = $<HTMLInputElement>('search');
  box.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter') { ev.preventDefault(); box.blur(); lastPane = 'list'; } else if (ev.key === 'Escape') { ev.preventDefault(); clearSearch(); }
  });
  // Esc in the list clears a confirmed filter (after the vim keys had their turn).
  document.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key !== 'Escape' || ev.defaultPrevented) return;
    if (document.querySelector('dialog[open]') !== null || box.value.trim() === '') return;
    const t = document.activeElement as HTMLElement | null;
    if (t && (t.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t.tagName))) return;
    ev.preventDefault();
    clearSearch();
  });
}

/** The session-list slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => { runSessionList(id, inv); },
  snapshot: () => ({ list: listSnapshot() }),
  init: initSessionList,
  // Before the other slices: its document keydown listener (Esc clears the
  // filter) has registered ahead of the transcript's since slice 4.
  initOrder: 10,
};
