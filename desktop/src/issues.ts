// The Issues workspace (hub #3010): DOM side. Pure rules live in
// issues-model.ts and actions/issues.ts (unit-tested); this file reads the
// daemon through issues-api.ts and draws. Daemon text is set with textContent
// only. Keys inside the workspace are the TUI's Local table keys, resolved by
// issues-model.ts and dispatched as registry actions (commands.ts).
import type { IssuesActionId, IssuesSnapshot } from './actions/issues.js';
import { inTauri, api, invoke, type Session } from './api.js';
import { type Invocation, copyText, dispatch, snapshot } from './commands.js';
import { $, el, toast } from './dom.js';
import { issuesApi } from './issues-api.js';
import {
  type DepDirection, type DependencyItem, type Draft, type Filters, type Handoff, type Issue, type IssueEvent,
  type IssuePage, type KeyState, type MutationResult, type PageAnchor, type PendingWrite, type SavedView, type Section,
  SAVED_VIEW_TITLES, SECTION_TITLES, SECTIONS, SORT_TITLES, STATUSES,
  applySavedView, canEdit, checkDraft, classifyFailure, defaultFilters, dependencyEdge, depLabel, describeDependency,
  describeEvent, draftFromIssue, emptyDraft, filterChips, findByNumber, idleKeys, isArchived, issueRef, listRequest,
  moveSelection, newWrite, nextSection, nextSort, numberJumpFilters, operatorOnly, parseCsv, parseIssueNumber,
  parsePriorities, parseProvenance, parseStatuses, patchFrom, priorityLabel, readinessLabel, rebase, reconcileSelection,
  resolveIssuesKey, rowSummary, rowTone, shortTime, statusTargets, statusTitle, toggleOperatorOnly, writeBlock, writeBody,
  createBody,
} from './issues-model.js';
import {
  type DispatchRecord, type IssuesTab, type PollResult, type TrackerStatus, TAB_TITLES, ISSUE_TABS, DEFAULT_ISSUE_TOPOLOGY,
  associatedSessions, candidateQuery, candidateRequest, dispatchFields, dispatchKey, dispatchLine, moveDispatch,
  parseDispatched, pollSummary, reconcileAssociated, sortDispatched, stepTab, syncRows,
} from './issues-tracker-model.js';
import { openPicker } from './pickers.js';
import { contextMenuModel } from './registry.js';
import { select as openSessionRow, setView } from './sessions.js';
import { onUiChange, state, uiChanged } from './state.js';
import { focusOwnsKey, paneKeyAllowed } from './modal-gate.js';
import { isVimKeysOn } from './vimkeys.js';

const LS_PROJECT = 'rsi.desktop.issues.project';
const LS_MINE = 'rsi.desktop.issues.mine';
const DETAIL_DEBOUNCE_MS = 150;

interface Detail {
  issueId: string;
  blocked_by: DependencyItem[] | null;
  blocked_byNext: Record<string, unknown> | null;
  blocks: DependencyItem[] | null;
  blocksNext: Record<string, unknown> | null;
  events: IssueEvent[] | null;
  eventsNext: number | null;
  handoffs: Handoff[] | null;
  handoffsNext: number | null;
  handoffsTotal: number;
  error: string | null;
}

const ws = {
  active: false,
  projectId: null as string | null,
  filters: defaultFilters(),
  anchor: { kind: 'start' } as PageAnchor,
  page: null as IssuePage | null,
  selectedId: null as string | null,
  loading: false,
  loadError: null as string | null,
  epoch: 0,
  detailEpoch: 0,
  section: 'blocked_by' as Section,
  detail: null as Detail | null,
  retry: null as PendingWrite | null,
  keys: idleKeys() as KeyState,
  // G2: the Dispatched and Sync tabs and the Local inspector's associated sessions.
  tab: 'local' as IssuesTab,
  tabEpoch: 0,
  dispatched: null as DispatchRecord[] | null,
  dispatchedSel: null as string | null,
  dispatchedError: null as string | null,
  status: null as TrackerStatus | null,
  statusError: null as string | null,
  poll: null as PollResult | null,
  pollError: null as string | null,
  polling: false,
  assocSel: null as string | null,
};

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

function mintKey(): string {
  return crypto.randomUUID();
}

// ---------- snapshot ----------

function selectedIssue(): Issue | null {
  return ws.page?.rows.find((r) => r.issue.id === ws.selectedId)?.issue ?? null;
}

export function issuesSnapshot(): IssuesSnapshot {
  return {
    active: ws.active,
    hasProject: ws.projectId !== null || state.projects.length > 0,
    issue: selectedIssue(),
    nextPage: !!ws.page?.next_cursor,
    previousPage: !!ws.page?.previous_cursor,
    operatorOnly: operatorOnly(ws.filters),
    tab: ws.tab,
    session: ws.tab === 'dispatched' ? selectedDispatch() !== null : ws.tab === 'local' ? associatedIds().length > 0 : false,
  };
}

// ---------- G2: tabs, tracker reads, associated sessions ----------

function selectedDispatch(): DispatchRecord | null {
  return ws.dispatched?.find((r) => dispatchKey(r) === ws.dispatchedSel) ?? null;
}

type TrackedSession = Session & { issue_tracker_id?: string | null };

/** The sessions launched for the selected Issue, both lists, by UUID. */
function associatedIds(): string[] {
  const issue = selectedIssue();
  if (!issue) return [];
  const all = [...state.sessions, ...state.archived.filter((a) => !state.sessions.some((s) => s.id === a.id))] as TrackedSession[];
  return associatedSessions(all, issue.id).map((s) => s.id);
}

function sessionTitle(id: string): string {
  const s = [...state.sessions, ...state.archived].find((x) => x.id === id);
  return s ? `${s.title?.trim() || s.query.split('\n')[0]?.slice(0, 60) || id.slice(0, 8)} · ${s.status}` : `session ${id.slice(0, 8)}`;
}

function inspectorFocused(): boolean {
  return document.activeElement instanceof HTMLElement && document.activeElement.closest('#issues-inspector') !== null;
}

async function loadTab(): Promise<void> {
  const epoch = ++ws.tabEpoch;
  try {
    if (ws.tab === 'dispatched') {
      const rows = sortDispatched(parseDispatched(await invoke<unknown>('dispatched_issues')));
      if (epoch !== ws.tabEpoch) return;
      ws.dispatched = rows;
      ws.dispatchedError = null;
      if (!rows.some((r) => dispatchKey(r) === ws.dispatchedSel)) ws.dispatchedSel = rows[0] ? dispatchKey(rows[0]) : null;
    } else if (ws.tab === 'sync') {
      const st = await invoke<TrackerStatus>('issue_tracker_status');
      if (epoch !== ws.tabEpoch) return;
      ws.status = st;
      ws.statusError = null;
    }
  } catch (e) {
    if (epoch !== ws.tabEpoch) return;
    if (ws.tab === 'dispatched') ws.dispatchedError = String(e); else ws.statusError = String(e);
  }
  renderAll();
  uiChanged();
}

function switchTab(dir: 1 | -1): void {
  setTab(stepTab(ws.tab, dir));
}

function setTab(tab: IssuesTab): void {
  ws.tab = tab;
  ws.keys = idleKeys();
  renderAll();
  uiChanged();
  if (tab === 'local') void loadPage(ws.anchor, 'first', ws.selectedId);
  else void loadTab();
}

/** `P` on the Sync tab: one manual poll, every error shown, then Sync and Dispatched refresh. */
async function runPoll(): Promise<void> {
  if (ws.polling) return;
  ws.polling = true;
  renderAll();
  try {
    ws.poll = await invoke<PollResult>('trigger_issue_poll');
    ws.pollError = null;
    toast(pollSummary(ws.poll), ws.poll.errors.length === 0);
  } catch (e) {
    ws.pollError = String(e);
    toast(`Poll failed: ${String(e)}`);
  } finally {
    ws.polling = false;
  }
  await loadTab();
  try {
    ws.dispatched = sortDispatched(parseDispatched(await invoke<unknown>('dispatched_issues')));
  } catch {
    // the Dispatched tab reloads when it is opened
  }
  renderAll();
}

/** Open a session from the workspace: close it, find the session in Active or Archived, open it. */
async function openSession(id: string): Promise<void> {
  closeWorkspace();
  if (state.sessions.some((s) => s.id === id)) {
    if (state.view !== 'active') await setView('active');
    openSessionRow(id);
    return;
  }
  if (state.view !== 'archived') await setView('archived');
  if (state.archived.some((s) => s.id === id)) { openSessionRow(id); return; }
  toast(`Session ${id.slice(0, 8)} is not in the session lists`);
}

function renderTabs(): void {
  const box = document.getElementById('issues-tabs');
  if (!box) return;
  box.replaceChildren(...ISSUE_TABS.map((t) => {
    const b = el('button', `seg${t === ws.tab ? ' active' : ''}`, TAB_TITLES[t]);
    b.type = 'button';
    b.dataset.tab = t;
    b.setAttribute('role', 'tab');
    b.setAttribute('aria-selected', String(t === ws.tab));
    return b;
  }));
}

function renderTrackerList(): void {
  const ul = $('issues-list');
  if (ws.tab === 'dispatched') {
    const rows = ws.dispatched;
    if (!rows || rows.length === 0) {
      ul.replaceChildren(el('li', 'issues-empty', ws.dispatchedError ? 'Could not load dispatched Issues.' : rows ? 'No Issue has been dispatched.' : 'Loading…'));
      return;
    }
    ul.replaceChildren(...rows.map((r) => {
      const key = dispatchKey(r);
      const li = el('li', `issue-item${key === ws.dispatchedSel ? ' selected' : ''}${r.terminal_state ? ' done' : ''}`);
      li.dataset.dispatch = key;
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(key === ws.dispatchedSel));
      li.append(el('span', 'ref', r.issue_identifier), el('span', 'ititle', dispatchLine(r)), el('span', 'istatus', shortTime(r.dispatched_at)));
      return li;
    }));
    ul.querySelector<HTMLElement>('.selected')?.scrollIntoView({ block: 'nearest' });
    return;
  }
  const st = ws.status;
  if (!st) {
    ul.replaceChildren(el('li', 'issues-empty', ws.statusError ? `Could not load the tracker status: ${ws.statusError}` : 'Loading…'));
    return;
  }
  ul.replaceChildren(...syncRows(st).map(([k, v]) => {
    const li = el('li', 'issue-item');
    li.append(el('span', 'ref', k), el('span', 'ititle', v));
    return li;
  }));
}

function renderTrackerInspector(): void {
  const box = $('issues-inspector');
  if (ws.tab === 'dispatched') {
    const r = selectedDispatch();
    if (!r) { box.replaceChildren(el('div', 'issues-empty', 'Select a dispatched Issue (j / k).')); return; }
    const meta = el('div', 'iv-meta');
    meta.append(...dispatchFields(r).map(([k, v]) => kv(k, v)));
    const open = el('button', 'btn primary', 'Open session');
    open.type = 'button';
    open.dataset.action = 'issues-open-session';
    box.replaceChildren(el('div', 'iv-head', `${r.issue_identifier} (${r.tracker})`), meta, open,
      el('div', 'meta', 'Enter in the inspector opens the session, recovering it from the archive when needed.'));
    return;
  }
  const poll = el('button', 'btn primary', ws.polling ? 'Polling…' : 'Run poll now (P)');
  poll.type = 'button';
  poll.dataset.action = 'issues-poll';
  poll.disabled = ws.polling;
  const out: HTMLElement[] = [el('div', 'iv-head', 'Tracker sync'), poll];
  if (ws.pollError) out.push(el('div', 'meta err', ws.pollError));
  if (ws.poll) {
    out.push(el('div', 'meta', pollSummary(ws.poll)));
    const errs = el('ul', 'iv-list');
    for (const e of ws.poll.errors) errs.append(el('li', 'err', e));
    out.push(errs);
  }
  box.replaceChildren(...out);
}

function associatedBlock(): HTMLElement | null {
  const ids = associatedIds();
  ws.assocSel = reconcileAssociated(ids, ws.assocSel);
  if (ids.length === 0) return null;
  const wrap = el('div', 'iv-assoc');
  wrap.append(el('div', 'k', 'Sessions'));
  const ul = el('ul', 'iv-list');
  for (const id of ids) {
    const li = el('li', `assoc${id === ws.assocSel ? ' selected' : ''}`, sessionTitle(id));
    li.dataset.session = id;
    li.title = 'Open this session (Enter in the inspector)';
    ul.append(li);
  }
  wrap.append(ul);
  return wrap;
}

// ---------- open / close ----------

function projectName(id: string | null): string {
  return state.projects.find((p) => p.id === id)?.name ?? '';
}

function fillProjects(): void {
  const sel = $<HTMLSelectElement>('issues-project');
  sel.replaceChildren(...state.projects.map((p) => {
    const o = el('option', undefined, p.name);
    o.value = p.id;
    return o;
  }));
  if (ws.projectId) sel.value = ws.projectId;
}

function chooseProject(): string | null {
  const known = (id: string | null | undefined) => (id && state.projects.some((p) => p.id === id) ? id : null);
  return known(ws.projectId) ?? known(storage()?.getItem(LS_PROJECT)) ?? known($<HTMLSelectElement>('project-filter').value)
    ?? state.projects[0]?.id ?? null;
}

async function openWorkspace(): Promise<void> {
  if (ws.active) { $('issues-list').focus(); return; }
  const pid = chooseProject();
  if (!pid) { toast('No projects yet: the Issues workspace is per project'); return; }
  ws.active = true;
  ws.projectId = pid;
  fillProjects();
  $('issues-view').hidden = false;
  document.querySelector<HTMLElement>('.layout')!.hidden = true;
  renderAll();
  uiChanged();
  if (ws.tab !== 'local') void loadTab();
  await loadPage({ kind: 'start' }, 'first', ws.selectedId);
}

function closeWorkspace(): void {
  ws.active = false;
  ws.epoch++;
  ws.detailEpoch++;
  $('issues-view').hidden = true;
  document.querySelector<HTMLElement>('.layout')!.hidden = false;
  uiChanged();
}

// ---------- loading ----------

async function loadPage(anchor: PageAnchor, prefer: 'first' | 'last', keepId: string | null): Promise<void> {
  const pid = ws.projectId;
  if (!pid) return;
  const epoch = ++ws.epoch;
  ws.loading = true;
  renderStatus();
  try {
    const page = await issuesApi.listPage(listRequest(pid, ws.filters, anchor));
    if (epoch !== ws.epoch) return;
    ws.page = page;
    ws.anchor = anchor;
    ws.loadError = null;
    const before = ws.selectedId;
    ws.selectedId = reconcileSelection(page, keepId, prefer);
    if (ws.selectedId !== before) ws.detail = null;
  } catch (e) {
    if (epoch !== ws.epoch) return;
    ws.loadError = String(e);
  } finally {
    if (epoch === ws.epoch) ws.loading = false;
  }
  renderAll();
  scheduleDetail(0);
  uiChanged();
}

let detailTimer: number | undefined;
function scheduleDetail(delay = DETAIL_DEBOUNCE_MS): void {
  window.clearTimeout(detailTimer);
  detailTimer = window.setTimeout(() => void loadSection(false), delay);
}

async function loadSection(more: boolean): Promise<void> {
  const issue = selectedIssue();
  const pid = ws.projectId;
  if (!issue || !pid || !ws.active) return;
  if (ws.detail?.issueId !== issue.id) {
    ws.detail = {
      issueId: issue.id, blocked_by: null, blocked_byNext: null, blocks: null, blocksNext: null,
      events: null, eventsNext: null, handoffs: null, handoffsNext: null, handoffsTotal: 0, error: null,
    };
  }
  const d = ws.detail;
  const section = ws.section;
  const epoch = ++ws.detailEpoch;
  try {
    switch (section) {
      case 'blocked_by':
      case 'blocks': {
        if (d[section] !== null && !more) break;
        const cursor = more ? d[`${section}Next`] : null;
        if (more && !cursor) { toast('No more'); break; }
        const page = await issuesApi.dependencies(pid, issue.id, section, cursor);
        if (epoch !== ws.detailEpoch) return;
        d[section] = [...(more ? d[section] ?? [] : []), ...page.items];
        d[`${section}Next`] = page.next_cursor ?? null;
        break;
      }
      case 'events': {
        if (d.events !== null && !more) break;
        if (more && d.eventsNext === null) { toast('No more'); break; }
        const page = await issuesApi.events(pid, issue.id, more ? d.eventsNext : null);
        if (epoch !== ws.detailEpoch) return;
        d.events = [...(more ? d.events ?? [] : []), ...page.events];
        d.eventsNext = page.next_after_sequence ?? null;
        break;
      }
      case 'handoffs': {
        if (d.handoffs !== null && !more) break;
        if (more && d.handoffsNext === null) { toast('No more'); break; }
        const page = await issuesApi.handoffs(pid, issue.id, more ? d.handoffsNext : null);
        if (epoch !== ws.detailEpoch) return;
        d.handoffs = [...(more ? d.handoffs ?? [] : []), ...page.handoffs];
        d.handoffsNext = page.next_after_sequence ?? null;
        d.handoffsTotal = page.total;
        break;
      }
    }
    d.error = null;
  } catch (e) {
    if (epoch !== ws.detailEpoch) return;
    d.error = String(e);
  }
  renderInspector();
}

// ---------- rendering ----------

function renderAll(): void {
  renderTabs();
  renderHeader();
  renderList();
  renderInspector();
  renderStatus();
}

function renderHeader(): void {
  const chips = $('issues-chips');
  chips.replaceChildren(...filterChips(ws.filters).map((c) => el('span', 'chip', c)));
  $('issues-sort').textContent = `Sort: ${SORT_TITLES[ws.filters.sort]}`;
}

function renderStatus(): void {
  const s = $('issues-status');
  const rows = ws.page?.rows.length ?? 0;
  s.className = ws.loadError ? 'meta err' : 'meta';
  if (ws.loadError) s.textContent = ws.loadError;
  else if (ws.loading) s.textContent = 'Loading…';
  else if (ws.tab === 'dispatched') s.textContent = `${ws.dispatched?.length ?? 0} dispatched`;
  else if (ws.tab === 'sync') s.textContent = ws.status ? `${ws.status.tracker || 'tracker'} ${ws.status.enabled ? 'polling' : 'disabled'}` : '';
  else s.textContent = `${rows} Issue${rows === 1 ? '' : 's'} on this page${ws.page?.next_cursor ? ' · more →' : ''}`;
  const retry = $('issues-retry');
  retry.hidden = ws.retry === null;
  if (ws.retry) retry.textContent = `“${ws.retry.label}” may not have been applied. Retry the identical request (Ctrl-Enter)`;
  $<HTMLButtonElement>('issues-page-prev').disabled = ws.tab !== 'local' || !ws.page?.previous_cursor;
  $<HTMLButtonElement>('issues-page-next').disabled = ws.tab !== 'local' || !ws.page?.next_cursor;
}

function renderList(): void {
  if (ws.tab !== 'local') { renderTrackerList(); return; }
  const ul = $('issues-list');
  const rows = ws.page?.rows ?? [];
  if (rows.length === 0) {
    const empty = el('li', 'issues-empty', ws.loading ? 'Loading…' : ws.loadError ? 'Could not load Issues.' : 'No Issues match these filters.');
    ul.replaceChildren(empty);
    return;
  }
  ul.replaceChildren(...rows.map((r) => {
    const li = el('li', `issue-item ${rowTone(r)}${r.issue.id === ws.selectedId ? ' selected' : ''}`);
    li.dataset.id = r.issue.id;
    li.setAttribute('role', 'option');
    li.setAttribute('aria-selected', String(r.issue.id === ws.selectedId));
    li.append(
      el('span', 'ref', issueRef(r.issue)),
      el('span', `prio p${r.issue.priority ?? 0}`, priorityLabel(r.issue.priority)),
      el('span', 'ititle', r.issue.title),
      el('span', 'istatus', `${statusTitle(r.issue.status)}${isArchived(r.issue) ? ' · archived' : ''}`),
    );
    const note = readinessLabel(r);
    if (note) li.append(el('span', 'iready', note));
    li.title = rowSummary(r);
    return li;
  }));
  ul.querySelector<HTMLElement>('.selected')?.scrollIntoView({ block: 'nearest' });
}

function kv(label: string, value: string): HTMLElement {
  const d = el('div', 'kv');
  d.append(el('span', 'k', label), el('span', 'v', value));
  return d;
}

function renderInspector(): void {
  if (ws.tab !== 'local') { renderTrackerInspector(); return; }
  const box = $('issues-inspector');
  const issue = selectedIssue();
  if (!issue) {
    box.replaceChildren(el('div', 'issues-empty', 'Select an Issue (j / k).'));
    return;
  }
  const head = el('div', 'iv-head');
  head.append(el('h2', undefined, `${issueRef(issue)}  ${issue.title}`));
  const meta = el('div', 'iv-meta');
  meta.append(
    kv('Status', `${statusTitle(issue.status)}${isArchived(issue) ? ' (archived)' : ''}`),
    kv('Priority', priorityLabel(issue.priority)),
    kv('Assignee', issue.assignee || '—'),
    kv('Labels', issue.labels.length ? issue.labels.join(', ') : '—'),
    kv('Version', `v${issue.row_version}`),
    kv('Created', shortTime(issue.created_at)),
    kv('Updated', shortTime(issue.updated_at)),
  );
  if (issue.closed_at) meta.append(kv('Closed', shortTime(issue.closed_at)));
  if (issue.archived_at) meta.append(kv('Archived', shortTime(issue.archived_at)));
  const body = el('pre', 'iv-body', issue.body === '' ? '(no description)' : issue.body);

  const tabs = el('div', 'iv-tabs');
  tabs.setAttribute('role', 'tablist');
  for (const s of SECTIONS) {
    const b = el('button', `seg${s === ws.section ? ' active' : ''}`, SECTION_TITLES[s]);
    b.type = 'button';
    b.dataset.section = s;
    b.setAttribute('role', 'tab');
    b.setAttribute('aria-selected', String(s === ws.section));
    tabs.append(b);
  }
  const assoc = associatedBlock();
  box.replaceChildren(head, meta, body, ...(assoc ? [assoc] : []), tabs, sectionBody(issue));
}

function sectionBody(issue: Issue): HTMLElement {
  const wrap = el('div', 'iv-section');
  const d = ws.detail?.issueId === issue.id ? ws.detail : null;
  if (!d) { wrap.append(el('div', 'issues-empty', 'Loading…')); return wrap; }
  if (d.error) wrap.append(el('div', 'meta err', d.error));
  const list = el('ul', 'iv-list');
  const more = (has: boolean) => {
    if (!has) return;
    const b = el('button', 'btn', 'Load more');
    b.type = 'button';
    b.dataset.action = 'issues-more';
    wrap.append(b);
  };
  switch (ws.section) {
    case 'blocked_by':
    case 'blocks': {
      const items = d[ws.section];
      if (items === null) { wrap.append(el('div', 'issues-empty', 'Loading…')); break; }
      if (items.length === 0) wrap.append(el('div', 'issues-empty', ws.section === 'blocks' ? 'Blocks no other Issue.' : 'Not blocked by any Issue.'));
      for (const it of items) {
        const li = el('li', 'dep', describeDependency(it));
        li.dataset.number = String(it.related_issue.display_number);
        li.title = 'Go to this Issue';
        list.append(li);
      }
      wrap.append(list);
      more(d[`${ws.section}Next`] !== null);
      break;
    }
    case 'events': {
      if (d.events === null) { wrap.append(el('div', 'issues-empty', 'Loading…')); break; }
      for (const e of d.events) list.append(el('li', 'ev-line', describeEvent(e)));
      wrap.append(list);
      more(d.eventsNext !== null);
      break;
    }
    case 'handoffs': {
      if (d.handoffs === null) { wrap.append(el('div', 'issues-empty', 'Loading…')); break; }
      if (d.handoffs.length === 0) wrap.append(el('div', 'issues-empty', 'No handoffs published.'));
      for (const h of d.handoffs) {
        const det = el('details', 'handoff');
        det.append(el('summary', undefined, `#${h.sequence} · ${shortTime(h.created_at)} · session ${h.author_session_id.slice(0, 8)}`), el('pre', 'iv-body', h.body));
        list.append(det);
      }
      wrap.append(list);
      if (d.handoffs.length > 0) wrap.append(el('div', 'meta', `${d.handoffs.length} of ${d.handoffsTotal}`));
      more(d.handoffsNext !== null);
      break;
    }
  }
  return wrap;
}

// ---------- selection ----------

function select(id: string): void {
  if (ws.selectedId === id) return;
  ws.selectedId = id;
  ws.detail = null;
  renderList();
  renderInspector();
  scheduleDetail();
  uiChanged();
}

function move(delta: number): void {
  const m = moveSelection(ws.page, ws.selectedId, delta);
  if (m.kind === 'select') select(m.id);
  else if (m.kind === 'fetch' && ws.page) {
    const cursor = m.anchor === 'next' ? ws.page.next_cursor : ws.page.previous_cursor;
    if (cursor) void loadPage({ kind: m.anchor, cursor }, m.anchor === 'next' ? 'first' : 'last', null);
  }
}

// ---------- forms ----------

interface Field {
  name: string;
  label: string;
  kind: 'text' | 'textarea' | 'select';
  value: string;
  options?: Array<[string, string]>;
  hint?: string;
}

interface StaleHelp {
  /** Replace the form's base and fields with the latest server version. */
  reload: () => Record<string, string>;
  /** Resend the same edit against the latest version under a new key. */
  rebase: () => Promise<FormResult>;
}

type FormResult = { done: true } | { done: false; error: string; stale?: StaleHelp };

interface FormSpec {
  title: string;
  intro?: string;
  fields: Field[];
  submit: string;
  danger?: boolean;
  run: (values: Record<string, string>) => Promise<FormResult> | FormResult;
}

function fieldNode(f: Field): HTMLElement {
  const label = el('label', 'ff');
  label.append(el('span', undefined, f.label));
  let input: HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement;
  if (f.kind === 'select') {
    const sel = el('select');
    for (const [v, t] of f.options ?? []) {
      const o = el('option', undefined, t);
      o.value = v;
      sel.append(o);
    }
    sel.value = f.value;
    input = sel;
  } else if (f.kind === 'textarea') {
    const ta = el('textarea');
    ta.rows = 8;
    ta.value = f.value;
    input = ta;
  } else {
    const inp = el('input');
    inp.type = 'text';
    inp.dataset.vim = 'on';
    inp.value = f.value;
    inp.autocomplete = 'off';
    inp.spellcheck = false;
    input = inp;
  }
  input.name = f.name;
  label.append(input);
  if (f.hint) label.append(el('small', undefined, f.hint));
  return label;
}

function openForm(spec: FormSpec): void {
  const dialog = $<HTMLDialogElement>('issue-dialog');
  const form = $<HTMLFormElement>('issue-form');
  let busy = false;
  let dirty = false;
  let armed = false;

  const error = el('div', 'form-error');
  error.hidden = true;
  const staleBar = el('div', 'stale-bar');
  staleBar.hidden = true;
  const fieldsBox = el('div', 'form-fields');
  const nodes = spec.fields.map(fieldNode);
  fieldsBox.append(...nodes);
  const cancel = el('button', 'btn', 'Cancel');
  cancel.type = 'button';
  const submit = el('button', `btn ${spec.danger ? 'danger' : 'primary'}`, spec.submit);
  submit.type = 'submit';
  const actions = el('div', 'actions');
  actions.append(cancel, submit);
  form.replaceChildren(el('h2', undefined, spec.title), ...(spec.intro ? [el('p', 'form-intro', spec.intro)] : []), fieldsBox, error, staleBar, actions);

  const control = (name: string) => form.elements.namedItem(name) as HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement | null;
  const values = (): Record<string, string> => {
    const out: Record<string, string> = {};
    for (const f of spec.fields) out[f.name] = control(f.name)?.value ?? '';
    return out;
  };
  const showError = (msg: string) => { error.textContent = msg; error.hidden = false; };

  function showStale(h: StaleHelp): void {
    const reload = el('button', 'btn', 'Reload latest (Ctrl-L)');
    reload.type = 'button';
    const rebaseBtn = el('button', 'btn', 'Rebase my edit onto latest (Ctrl-R)');
    rebaseBtn.type = 'button';
    reload.addEventListener('click', () => {
      const next = h.reload();
      for (const [k, v] of Object.entries(next)) { const c = control(k); if (c) c.value = v; }
      dirty = false;
      staleBar.hidden = true;
      error.hidden = true;
    });
    rebaseBtn.addEventListener('click', () => void settle(h.rebase()));
    staleBar.replaceChildren(reload, rebaseBtn);
    staleBar.hidden = false;
  }

  async function settle(p: Promise<FormResult> | FormResult): Promise<void> {
    busy = true;
    submit.disabled = true;
    try {
      const r = await p;
      if (r.done) { dialog.close(); return; }
      showError(r.error);
      if (r.stale) showStale(r.stale); else staleBar.hidden = true;
    } catch (e) {
      showError(String(e));
    } finally {
      busy = false;
      submit.disabled = false;
    }
  }

  form.onsubmit = (ev) => {
    ev.preventDefault();
    if (busy) return;
    error.hidden = true;
    staleBar.hidden = true;
    void settle(spec.run(values()));
  };
  form.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) { ev.preventDefault(); form.requestSubmit(); return; }
    // A stale form: Ctrl-L reloads the latest projection, Ctrl-R rebases the draft onto it (G2).
    if (ev.ctrlKey && !ev.metaKey && !ev.altKey && !staleBar.hidden) {
      const k = ev.key.toLowerCase();
      const btn = staleBar.querySelectorAll<HTMLButtonElement>('button')[k === 'l' ? 0 : k === 'r' ? 1 : -1];
      if (btn) { ev.preventDefault(); btn.click(); }
    }
  };
  form.oninput = () => { dirty = true; armed = false; };
  cancel.onclick = () => dialog.close();
  // A dirty form needs Esc twice, as in the TUI.
  dialog.oncancel = (ev) => {
    if (dirty && !armed && !busy) {
      ev.preventDefault();
      armed = true;
      showError('UNSAVED CHANGES: press Esc again to discard');
    }
  };
  dialog.onclose = () => { uiChanged(); };
  dialog.showModal();
  (nodes[0]?.querySelector('input, textarea, select') as HTMLElement | null)?.focus();
  if (!nodes[0]) submit.focus();
  uiChanged();
}

// ---------- writes ----------

function sendWrite(w: PendingWrite): Promise<MutationResult> {
  const pid = ws.projectId;
  if (!pid) return Promise.reject(new Error('no project'));
  const body = writeBody(w);
  const id = w.issueId ?? '';
  switch (w.kind) {
    case 'create': return issuesApi.create(pid, body);
    case 'update': return issuesApi.update(pid, id, body);
    case 'status': return issuesApi.setStatus(pid, id, body);
    case 'archive': return issuesApi.archive(pid, id, body);
    case 'restore': return issuesApi.restore(pid, id, body);
  }
}

interface WriteForm {
  /** The Issue the edit is based on (null for create). */
  initial: Issue | null;
  build: (base: Issue | null, v: Record<string, string>) => PendingWrite | { error: string };
  fields: (base: Issue) => Record<string, string>;
}

/**
 * Submit handler with the TUI's retry rules: an uncertain failure freezes the
 * request (same key, same bytes) while the fields stay unchanged; a typed
 * rejection drops it; a stale version offers reload / rebase.
 */
function writeHandler(cfg: WriteForm): (v: Record<string, string>) => Promise<FormResult> {
  let base = cfg.initial;
  let frozen: PendingWrite | null = null;
  let frozenSig = '';

  async function attempt(w: PendingWrite, sig: string): Promise<FormResult> {
    try {
      const r = await sendWrite(w);
      frozen = null;
      toast(r.deduplicated ? `${w.label} (already applied)` : w.label, true);
      ws.retry = null;
      void loadPage(ws.anchor, 'first', r.issue.id);
      return { done: true };
    } catch (e) {
      const f = classifyFailure(String(e));
      if (f.kind === 'uncertain') {
        frozen = w;
        frozenSig = sig;
        return { done: false, error: `${f.message}. The write may or may not have been applied: submit again to retry the identical request.` };
      }
      frozen = null;
      if (f.kind === 'stale' && w.issueId && ws.projectId) {
        const row = await issuesApi.get(ws.projectId, w.issueId).catch(() => null);
        if (row) {
          return {
            done: false,
            error: f.message,
            stale: {
              reload: () => { base = row.issue; return cfg.fields(row.issue); },
              rebase: () => attempt(rebase(w, row.issue, mintKey), sig),
            },
          };
        }
      }
      return { done: false, error: f.message };
    }
  }

  return async (v): Promise<FormResult> => {
    const sig = JSON.stringify(v);
    if (frozen && sig === frozenSig) return attempt(frozen, sig);
    const w = cfg.build(base, v);
    if ('error' in w) return { done: false, error: w.error };
    return attempt(w, sig);
  };
}

/** A write with no form (restore): failure keeps the request for Ctrl-Enter. */
async function directWrite(w: PendingWrite): Promise<void> {
  try {
    const r = await sendWrite(w);
    ws.retry = null;
    toast(r.deduplicated ? `${w.label} (already applied)` : w.label, true);
    void loadPage(ws.anchor, 'first', r.issue.id);
  } catch (e) {
    const f = classifyFailure(String(e));
    if (f.kind === 'uncertain') {
      ws.retry = w;
      toast(`${f.message}. Not sure it was applied: Ctrl-Enter retries the identical request`);
    } else {
      ws.retry = null;
      toast(f.message);
    }
    renderStatus();
  }
}

function retryDirect(): void {
  if (ws.retry) void directWrite(ws.retry);
}

// ---------- the write forms ----------

const PRIORITY_OPTIONS: Array<[string, string]> = [['', 'Unset'], ['1', 'P1 urgent'], ['2', 'P2 high'], ['3', 'P3 normal'], ['4', 'P4 low']];

const DRAFT_FIELDS: Record<keyof Draft, Field> = {
  title: { name: 'title', label: 'Title', kind: 'text', value: '' },
  body: { name: 'body', label: 'Description', kind: 'textarea', value: '' },
  priority: { name: 'priority', label: 'Priority', kind: 'select', value: '', options: PRIORITY_OPTIONS },
  labels: { name: 'labels', label: 'Labels', kind: 'text', value: '', hint: 'Comma-separated' },
  assignee: { name: 'assignee', label: 'Assignee', kind: 'text', value: '', hint: 'Empty unassigns' },
};

function draftFields(names: Array<keyof Draft>, from: Draft): Field[] {
  return names.map((n) => ({ ...DRAFT_FIELDS[n], value: from[n] }));
}

function newIssue(): void {
  const pid = ws.projectId;
  if (!pid) return;
  openForm({
    title: `New Issue in ${projectName(pid)}`,
    fields: draftFields(['title', 'body', 'priority', 'labels', 'assignee'], emptyDraft()),
    submit: 'Create',
    run: writeHandler({
      initial: null,
      build: (_b, v) => {
        const c = checkDraft(v as unknown as Draft);
        return c.ok ? newWrite('create', null, createBody(c.value), 'Issue created', mintKey) : { error: c.error };
      },
      fields: () => ({}),
    }),
  });
}

/** Edit some of an Issue's draft fields; only the changed ones are sent. */
function editFields(issue: Issue, names: Array<keyof Draft>, title: string): void {
  const fields = (base: Issue) => Object.fromEntries(names.map((n) => [n, draftFromIssue(base)[n]]));
  openForm({
    title: `${title} ${issueRef(issue)}`,
    intro: issue.title,
    fields: draftFields(names, draftFromIssue(issue)),
    submit: 'Save',
    run: writeHandler({
      initial: issue,
      build: (base, v) => {
        if (!base) return { error: 'No Issue' };
        const merged = { ...draftFromIssue(base), ...v } as Draft;
        const c = checkDraft(merged);
        if (!c.ok) return { error: c.error };
        const patch = patchFrom(base, c.value);
        // Only what this form edits goes out, even if the Issue moved on.
        for (const k of Object.keys(patch)) if (!(names as string[]).includes(k)) delete patch[k];
        if (Object.keys(patch).length === 0) return { error: 'Nothing changed' };
        return newWrite('update', base, { patch }, `${issueRef(base)} updated`, mintKey);
      },
      fields,
    }),
  });
}

function statusForm(issue: Issue): void {
  const targets = statusTargets(issue);
  openForm({
    title: `Status of ${issueRef(issue)}`,
    intro: `${issue.title}\nNow ${statusTitle(issue.status)}. ${issue.status === 'Closed' || issue.status === 'Cancelled' ? 'Choose Open to reopen it.' : ''}`,
    fields: [{
      name: 'status', label: 'Move to', kind: 'select', value: targets[0] ?? '',
      options: targets.map((t) => [t, statusTitle(t)] as [string, string]),
    }],
    submit: 'Change status',
    run: writeHandler({
      initial: issue,
      build: (base, v) => {
        if (!base) return { error: 'No Issue' };
        if (!STATUSES.includes(v.status as never)) return { error: 'Pick a status' };
        return newWrite('status', base, { status: v.status }, `${issueRef(base)} is now ${statusTitle(v.status as never)}`, mintKey);
      },
      fields: (base) => ({ status: statusTargets(base)[0] ?? '' }),
    }),
  });
}

function confirmForm(issue: Issue, what: 'cancel' | 'archive'): void {
  const cancel = what === 'cancel';
  openForm({
    title: cancel ? `Cancel ${issueRef(issue)}?` : `Archive ${issueRef(issue)}?`,
    intro: cancel
      ? `${issue.title}\nSets the status to Cancelled. It never deletes or archives the Issue.`
      : `${issue.title}\nArchiving hides it from the default list. It can be restored (U).`,
    fields: [],
    submit: cancel ? 'Cancel Issue' : 'Archive',
    danger: true,
    run: writeHandler({
      initial: issue,
      build: (base) => {
        if (!base) return { error: 'No Issue' };
        return cancel
          ? newWrite('status', base, { status: 'Cancelled' }, `${issueRef(base)} cancelled`, mintKey)
          : newWrite('archive', base, {}, `${issueRef(base)} archived`, mintKey);
      },
      fields: () => ({}),
    }),
  });
}

/** Add or remove one dependency edge to Issue #n (the candidate picker's choice). */
async function applyDependency(issue: Issue, op: string, direction: DepDirection, n: number): Promise<void> {
  const pid = ws.projectId;
  if (!pid) return;
  try {
    const page = await issuesApi.listPage(listRequest(pid, numberJumpFilters(n), { kind: 'start' }, 16));
    const other = findByNumber(page, n);
    if (!other) { toast(`No Issue #${n} in this project`); return; }
    const edge = dependencyEdge(direction, issue.id, other.issue.id);
    if (!edge) { toast('An Issue cannot depend on itself'); return; }
    const r = op === 'remove'
      ? await issuesApi.removeDependency(pid, edge.issueId, edge.dependsOnId)
      : await issuesApi.addDependency(pid, edge.issueId, edge.dependsOnId);
    toast(r.changed ? `${issueRef(issue)} ${op === 'remove' ? 'no longer ' : ''}${depLabel(direction).toLowerCase()} #${n}` : 'No change', r.changed);
    if (ws.detail) { ws.detail.blocked_by = null; ws.detail.blocks = null; }
    void loadPage(ws.anchor, 'first', issue.id);
  } catch (e) {
    toast(classifyFailure(String(e)).message);
  }
}

function dependencyForm(issue: Issue): void {
  const pid = ws.projectId;
  if (!pid) return;
  openForm({
    title: `Dependencies of ${issueRef(issue)}`,
    intro: issue.title,
    fields: [
      { name: 'op', label: 'Action', kind: 'select', value: 'add', options: [['add', 'Add'], ['remove', 'Remove']] },
      { name: 'direction', label: 'Relation', kind: 'select', value: 'blocked_by', options: [['blocked_by', 'Blocked by'], ['blocks', 'Blocks']] },
      { name: 'number', label: 'Other Issue', kind: 'text', value: '', hint: 'A number (12 or #12), or words to search a page of candidates; archived and closed Issues count' },
    ],
    submit: 'Apply',
    run: async (v) => {
      const q = candidateQuery(v.number ?? '');
      if ('error' in q) return { done: false, error: q.error };
      if ('search' in q) {
        // A bounded candidate page to choose from (the TUI's dependency candidate search).
        try {
          const page = await issuesApi.listPage(candidateRequest(pid, q.search));
          const rows = page.rows.filter((r) => r.issue.id !== issue.id);
          if (rows.length === 0) return { done: false, error: `No other Issue matches “${q.search}”` };
          window.setTimeout(() => openPicker({
            title: `${depLabel(v.direction as DepDirection)}: pick the other Issue`,
            hint: 'Type to filter · Enter apply · Esc cancel',
            filterable: true,
            items: rows.map((r) => ({ id: String(r.issue.display_number), label: `${issueRef(r.issue)} ${r.issue.title}`, detail: statusTitle(r.issue.status) })),
            onPick: (it) => { void applyDependency(issue, v.op ?? 'add', v.direction as DepDirection, Number(it.id)); },
          }), 0);
          return { done: true };
        } catch (e) {
          return { done: false, error: classifyFailure(String(e)).message };
        }
      }
      const n = q.number;
      try {
        const page = await issuesApi.listPage(listRequest(pid, numberJumpFilters(n), { kind: 'start' }, 16));
        const other = findByNumber(page, n);
        if (!other) return { done: false, error: `No Issue #${n} in this project` };
        const edge = dependencyEdge(v.direction as DepDirection, issue.id, other.issue.id);
        if (!edge) return { done: false, error: 'An Issue cannot depend on itself' };
        const r = v.op === 'remove'
          ? await issuesApi.removeDependency(pid, edge.issueId, edge.dependsOnId)
          : await issuesApi.addDependency(pid, edge.issueId, edge.dependsOnId);
        toast(r.changed ? `${issueRef(issue)} ${v.op === 'remove' ? 'no longer ' : ''}${depLabel(v.direction as DepDirection).toLowerCase()} #${n}` : 'No change', r.changed);
        if (ws.detail) { ws.detail.blocked_by = null; ws.detail.blocks = null; }
        void loadPage(ws.anchor, 'first', issue.id);
        return { done: true };
      } catch (e) {
        return { done: false, error: classifyFailure(String(e)).message };
      }
    },
  });
}

function applyFilters(f: Filters, keep: string | null = ws.selectedId): void {
  ws.filters = f;
  void loadPage({ kind: 'start' }, 'first', keep);
}

function askMine(then: () => void): void {
  openForm({
    title: 'Mine: your assignee name',
    intro: 'The saved view lists Issues assigned to this name. It is remembered on this machine.',
    fields: [{ name: 'name', label: 'Assignee', kind: 'text', value: storage()?.getItem(LS_MINE) ?? '' }],
    submit: 'Use',
    run: (v) => {
      const name = (v.name ?? '').trim();
      if (name === '') return { done: false, error: 'Enter a name' };
      storage()?.setItem(LS_MINE, name);
      window.setTimeout(then, 0);
      return { done: true };
    },
  });
}

function savedView(view: SavedView): void {
  const f = applySavedView(view, storage()?.getItem(LS_MINE) ?? null);
  if ('needs' in f) { askMine(() => savedView(view)); return; }
  applyFilters(f);
  toast(`${SAVED_VIEW_TITLES[view]} view`, true);
}

function filtersForm(): void {
  const f = ws.filters;
  const view = (name: string, label: string) => [name, label] as [string, string];
  openForm({
    title: 'Filter Issues',
    intro: 'Applying resets to the first page.',
    fields: [
      { name: 'query', label: 'Text', kind: 'text', value: f.query ?? '' },
      { name: 'statuses', label: 'Status', kind: 'text', value: f.statuses.join(', '), hint: 'Open, InProgress, Closed, Cancelled; empty = active ones' },
      { name: 'priorities', label: 'Priority', kind: 'text', value: f.priorities.join(', '), hint: '1 to 4' },
      { name: 'readiness', label: 'Readiness', kind: 'select', value: f.readiness, options: [view('any', 'Any'), view('ready', 'Ready'), view('blocked', 'Blocked')] },
      { name: 'assignee', label: 'Assignee', kind: 'text', value: f.unassigned ? '-' : f.assignee ?? '', hint: '- means unassigned' },
      { name: 'labels', label: 'Labels (all of)', kind: 'text', value: f.labels_all.join(', ') },
      { name: 'provenance', label: 'Provenance', kind: 'text', value: f.provenance.join(', '), hint: 'operator, session, idea, finding' },
      { name: 'archive', label: 'Archive', kind: 'select', value: f.archive, options: [view('Active', 'Active only'), view('Archived', 'Archived only'), view('All', 'Both')] },
      { name: 'sort', label: 'Sort', kind: 'select', value: f.sort, options: (Object.keys(SORT_TITLES) as Array<keyof typeof SORT_TITLES>).map((k) => view(k, SORT_TITLES[k])) },
      { name: 'view', label: 'Saved view', kind: 'select', value: '', options: [view('', '(none: use the fields above)'), view('ready', 'Ready'), view('blocked', 'Blocked'), view('mine', 'Mine'), view('recent', 'Recent')] },
    ],
    submit: 'Apply',
    run: (v) => {
      if (v.view) { window.setTimeout(() => savedView(v.view as SavedView), 0); return { done: true }; }
      const st = parseStatuses(v.statuses ?? '');
      const pr = parsePriorities(v.priorities ?? '');
      const pv = parseProvenance(v.provenance ?? '');
      const bad = [...st.bad, ...pr.bad, ...pv.bad];
      if (bad.length > 0) return { done: false, error: `Not understood: ${bad.join(', ')}` };
      const assignee = (v.assignee ?? '').trim();
      const next: Filters = {
        ...ws.filters,
        query: (v.query ?? '').trim() === '' ? null : (v.query ?? '').trim(),
        statuses: st.value,
        priorities: pr.value,
        readiness: v.readiness as Filters['readiness'],
        unassigned: assignee === '-',
        assignee: assignee === '' || assignee === '-' ? null : assignee,
        labels_all: parseCsv(v.labels ?? ''),
        provenance: pv.value,
        archive: v.archive as Filters['archive'],
        sort: v.sort as Filters['sort'],
      };
      applyFilters(next);
      return { done: true };
    },
  });
}

function searchForm(): void {
  openForm({
    title: 'Search Issues',
    fields: [{ name: 'query', label: 'Text', kind: 'text', value: ws.filters.query ?? '', hint: 'Empty clears the search' }],
    submit: 'Search',
    run: (v) => {
      const q = (v.query ?? '').trim();
      applyFilters({ ...ws.filters, query: q === '' ? null : q });
      return { done: true };
    },
  });
}

function numberForm(): void {
  openForm({
    title: 'Go to Issue number',
    fields: [{ name: 'number', label: 'Number', kind: 'text', value: '', hint: '12 or #12; closed and archived Issues are found too' }],
    submit: 'Go',
    run: async (v) => {
      const n = parseIssueNumber(v.number ?? '');
      if (n === null) return { done: false, error: 'Enter an Issue number' };
      const found = await goToNumber(n);
      return found ? { done: true } : { done: false, error: `No Issue #${n} in this project` };
    },
  });
}

async function goToNumber(n: number): Promise<boolean> {
  const pid = ws.projectId;
  if (!pid) return false;
  const f = numberJumpFilters(n);
  try {
    const page = await issuesApi.listPage(listRequest(pid, f, { kind: 'start' }));
    const row = findByNumber(page, n);
    if (!row) return false;
    ws.epoch++;
    ws.filters = f;
    ws.page = page;
    ws.anchor = { kind: 'start' };
    ws.selectedId = row.issue.id;
    ws.detail = null;
    ws.loadError = null;
    renderAll();
    scheduleDetail(0);
    uiChanged();
    return true;
  } catch (e) {
    toast(`Could not look up #${n}: ${String(e)}`);
    return true;
  }
}

// ---------- action dispatch ----------

function need(): Issue | null {
  return selectedIssue();
}

export function runIssues(id: IssuesActionId, inv: Invocation): void {
  const issue = need();
  switch (id) {
    case 'issues-open':
      void openWorkspace();
      return;
    case 'issues-close': closeWorkspace(); return;
    case 'issues-go-number': {
      const arg = inv.arg?.trim();
      const n = arg ? parseIssueNumber(arg) : null;
      if (arg && n === null) { toast(`“${arg}” is not an Issue number`); return; }
      void (async () => {
        if (!ws.active) await openWorkspace();
        if (n === null) { numberForm(); return; }
        if (!(await goToNumber(n))) toast(`No Issue #${n} in this project`);
      })();
      return;
    }
    case 'issues-down':
    case 'issues-up': {
      const delta = (id === 'issues-down' ? 1 : -1) * (inv.count ?? 1);
      if (ws.tab === 'dispatched') { ws.dispatchedSel = moveDispatch(ws.dispatched ?? [], ws.dispatchedSel, delta); renderAll(); uiChanged(); return; }
      if (ws.tab === 'sync') { $('issues-list').scrollBy({ top: delta * 40 }); return; }
      const ids = associatedIds();
      if (inspectorFocused() && ids.length > 0) {
        // The inspector's j / k move the associated-session selection (and scroll the fields).
        const at = Math.max(0, ids.indexOf(ws.assocSel ?? ''));
        ws.assocSel = ids[Math.max(0, Math.min(ids.length - 1, at + delta))] ?? null;
        renderInspector();
        $('issues-inspector').focus();
        return;
      }
      move(delta);
      return;
    }
    case 'issues-first':
    case 'issues-last': {
      const first = id === 'issues-first';
      if (ws.tab === 'dispatched') {
        const rows = ws.dispatched ?? [];
        const r = first ? rows[0] : rows[rows.length - 1];
        ws.dispatchedSel = r ? dispatchKey(r) : null;
        renderAll();
        uiChanged();
        return;
      }
      if (ws.tab === 'sync') { const l = $('issues-list'); l.scrollTop = first ? 0 : l.scrollHeight; return; }
      void loadPage({ kind: first ? 'start' : 'end' }, first ? 'first' : 'last', null);
      return;
    }
    case 'issues-page-next':
      if (ws.page?.next_cursor) void loadPage({ kind: 'next', cursor: ws.page.next_cursor }, 'first', null);
      return;
    case 'issues-page-prev':
      if (ws.page?.previous_cursor) void loadPage({ kind: 'previous', cursor: ws.page.previous_cursor }, 'last', null);
      return;
    case 'issues-inspect': {
      // Enter in the inspector opens the associated (or dispatched) session, as the TUI's IssueOpenSession.
      if (inspectorFocused()) {
        const sid = ws.tab === 'dispatched' ? selectedDispatch()?.session_id : ws.assocSel;
        if (sid) { void openSession(sid); return; }
      }
      if (ws.tab === 'local') scheduleDetail(0);
      $('issues-inspector').focus();
      return;
    }
    case 'issues-refresh':
      if (ws.tab !== 'local') { void loadTab(); return; }
      ws.detail = null;
      void loadPage(ws.anchor, 'first', ws.selectedId);
      return;
    case 'issues-tab-prev': switchTab(-1); return;
    case 'issues-tab-next': switchTab(1); return;
    case 'issues-poll': void runPoll(); return;
    case 'issues-topology': dispatch('topology-run-issue', { source: 'key', arg: `run ${DEFAULT_ISSUE_TOPOLOGY}` }); return;
    case 'issues-less': {
      const box = $('issues-inspector');
      box.scrollBy({ top: -Math.max(40, box.clientHeight - 40) });
      return;
    }
    case 'issues-open-session': {
      const sid = ws.tab === 'dispatched' ? selectedDispatch()?.session_id : (ws.assocSel ?? associatedIds()[0]);
      if (sid) void openSession(sid);
      return;
    }
    case 'issues-search': searchForm(); return;
    case 'issues-filters': filtersForm(); return;
    case 'issues-sort': applyFilters({ ...ws.filters, sort: nextSort(ws.filters.sort) }); toast(`Sorted by ${SORT_TITLES[nextSort(ws.filters.sort)]}`, true); return;
    case 'issues-operator': applyFilters(toggleOperatorOnly(ws.filters)); return;
    case 'issues-view-ready': savedView('ready'); return;
    case 'issues-view-blocked': savedView('blocked'); return;
    case 'issues-view-mine': savedView('mine'); return;
    case 'issues-view-recent': savedView('recent'); return;
    case 'issues-view-all': applyFilters(defaultFilters()); return;
    case 'issues-new': newIssue(); return;
    case 'issues-edit': if (issue) editFields(issue, ['title', 'body', 'priority', 'labels', 'assignee'], 'Edit'); return;
    case 'issues-priority': if (issue) editFields(issue, ['priority'], 'Priority of'); return;
    case 'issues-assign': if (issue) editFields(issue, ['assignee'], 'Assignee of'); return;
    case 'issues-labels': if (issue) editFields(issue, ['labels'], 'Labels of'); return;
    case 'issues-status': if (issue) statusForm(issue); return;
    case 'issues-deps': if (issue) dependencyForm(issue); return;
    case 'issues-cancel': if (issue) confirmForm(issue, 'cancel'); return;
    case 'issues-archive': if (issue) confirmForm(issue, 'archive'); return;
    case 'issues-restore':
      if (issue) void directWrite(newWrite('restore', issue, {}, `${issueRef(issue)} restored`, mintKey));
      return;
    case 'issues-copy-uuid': if (issue) void copyText(issue.id, 'issue UUID'); return;
    case 'issues-copy-ref': if (issue) void copyText(issueRef(issue), `issue ${issueRef(issue)}`); return;
    case 'issues-section':
      ws.section = nextSection(ws.section);
      renderInspector();
      void loadSection(false);
      return;
    case 'issues-more': void loadSection(true); return;
  }
}

// ---------- wiring ----------

function isTyping(t: HTMLElement | null): boolean {
  return !!t?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
}

export function initIssues(): void {
  $('issues-view').hidden = true;
  // The Local / Dispatched / Sync tab bar beside the title (G2).
  const title = document.querySelector('.issues-head h1');
  if (title && !document.getElementById('issues-tabs')) {
    const tabs = el('div', 'iv-tabs issues-tabs');
    tabs.id = 'issues-tabs';
    tabs.setAttribute('role', 'tablist');
    tabs.setAttribute('aria-label', 'Issues tabs');
    tabs.addEventListener('click', (ev) => {
      const t = (ev.target as HTMLElement).closest<HTMLElement>('[data-tab]')?.dataset.tab;
      if (t === 'local' || t === 'dispatched' || t === 'sync') setTab(t);
    });
    title.after(tabs);
  }
  const list = $('issues-list');
  list.addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.issue-item');
    if (li?.dataset.dispatch) { ws.dispatchedSel = li.dataset.dispatch; renderAll(); uiChanged(); return; }
    if (li?.dataset.id) select(li.dataset.id);
  });
  list.addEventListener('contextmenu', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.issue-item');
    if (!li?.dataset.id || !inTauri()) return;
    ev.preventDefault();
    select(li.dataset.id);
    api.popupContextMenu(contextMenuModel('issue-row', snapshot())).catch(() => undefined);
  });
  $('issues-inspector').addEventListener('click', (ev) => {
    const t = ev.target as HTMLElement;
    const tab = t.closest<HTMLElement>('[data-section]');
    if (tab?.dataset.section) {
      ws.section = tab.dataset.section as Section;
      renderInspector();
      void loadSection(false);
      return;
    }
    const assoc = t.closest<HTMLElement>('.assoc')?.dataset.session;
    if (assoc) { void openSession(assoc); return; }
    const dep = t.closest<HTMLElement>('.dep');
    const n = dep?.dataset.number ? Number(dep.dataset.number) : NaN;
    if (Number.isInteger(n)) void goToNumber(n);
  });
  $('issues-retry').addEventListener('click', retryDirect);
  $('issues-project').addEventListener('change', () => {
    const v = $<HTMLSelectElement>('issues-project').value;
    ws.projectId = v;
    storage()?.setItem(LS_PROJECT, v);
    ws.page = null;
    ws.selectedId = null;
    ws.detail = null;
    renderAll();
    void loadPage({ kind: 'start' }, 'first', null);
    uiChanged();
  });
  // Buttons keep focus after a click, which would swallow j/k.
  $('issues-view').addEventListener('click', (ev) => {
    const b = (ev.target as HTMLElement).closest('button');
    if (b) window.setTimeout(() => b.blur(), 0);
  });
  // Capture phase: the workspace takes its keys before the global key engine.
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!ws.active || ev.isComposing) return;
    if (!paneKeyAllowed(isVimKeysOn(), ev)) return;
    const target = document.activeElement as HTMLElement | null;
    if (document.querySelector('dialog[open]')) return;
    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey) && ws.retry) {
      ev.preventDefault();
      ev.stopImmediatePropagation();
      retryDirect();
      return;
    }
    if (ev.ctrlKey || ev.metaKey || ev.altKey || isTyping(target)) return;
    if (ev.key === 'Enter' && target?.closest('button')) return;
    const { result, next } = resolveIssuesKey(ws.keys, ev.key, Date.now());
    ws.keys = next;
    if (result.kind === 'none') return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
    if (result.kind === 'run') {
      dispatch(result.action, { source: 'key' });
    }
  }, true);
  onUiChange(() => { renderStatus(); });
}
