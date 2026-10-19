// Manager surfaces (hub #3011, slice 9): DOM side. One full-screen view that
// shows the manager tree, the harness manager board (Board, Decisions, Inbox,
// Inspect, Policy), the scope form, the global manager workspace and its
// Budget panel, the fleet and the escalation / friction / portfolio lists.
// Pure rules are in managers-model.ts (unit-tested); daemon text is set with
// textContent only, and credential-like fields are never shown.
import type { ManagersActionId, ManagersSnapshot } from './actions/managers.js';
import { type Invocation, copyText } from './commands.js';
import { el, toast } from './dom.js';
import { runManagerCommand } from './manager-cmds.js';
import { type TreeContext, appointAbove, appointSeat, cancelMove, editArea, editGrant, moveUnder, pendingMove } from './manager-tree-ops.js';
import { FORM_CSS } from './mgr-form.js';
import { portfolioSummary } from './manager-cmds-model.js';
import { managersApi } from './managers-api.js';
import {
  AGENT_SORTS, BAND_ROWS, BOARD_BANDS, BOARD_SECTION_TITLES, BOARD_SECTIONS, type BoardBand, type BoardSection,
  type BudgetRow, type BudgetSnapshot, CAPABILITIES, CAP_META, FLEET_DIMENSIONS, FLEET_WINDOW_TITLES, type FleetOverview,
  GROUP_SORTS, INSPECT_SUBSECTIONS, type InspectSub, KPI_OVERVIEW_PAGES, type LaunchChoice, type ManagerConfig, type ManagerView,
  POLICY_FIELDS, PRESETS, type ScopeCandidate, HALTABLE, type LaunchRole, type NodeRef, type NodeWorkspace, type WorkspaceRow,
  nodeHeader, nodeRefId, nodeWorkspaceRows, seatActionBlock, hasAllKpis, kpiLines, kpiRows, launchCatalog, launchText, parseListField, pickLaunches,
  type ScopeDraft, STATE_PAGE, type StatePage, type StatePolicy, TREE_PAGE, type TreeRow, VIEW_KEYS, type Workspace,
  answerRequest, appointRequest, budgetEditBlock, budgetRows, clampIndex, cycle, decisionBlock, decisionOptions,
  decisionStatusLabel, defaultPolicy, describeCap, descendants, filterScope, fleetAgents, fleetGroups, foldOrParent,
  optionAnswer, parentKeys, policyProblems, policySaveRequest, policyValue, portfolioRaiseRequest, projectRaiseRequest,
  raiseTo, recordDetail, retryKey, rowSession, rowState, rowTitle, scopeClearRequest, scopeDraftForSeat, scopeMark,
  scopeSaveRequest, scopeSummary, seatHealth, sectionQuery, setPolicyScalar, stepPolicy, toggleCapability, toggleScope,
  treeActionBlock, treeBadge, treeGrantSummary, treeLoad, treeNeedsAttention, unavailableSelections, usageLine,
  visibleTreeRows, workspaceRows,
} from './managers-model.js';
import { select as openSessionRow } from './sessions.js';
import { selected, state, uiChanged } from './state.js';
import { api, type ConversationEvent } from './api.js';
import { launchAndAppoint } from './manager-launch.js';
import { focusOwnsKey, paneKeyAllowed } from './modal-gate.js';
import { isVimKeysOn } from './vimkeys.js';

/** Conversation events the console shows beside a seat. */
const CONVERSATION_TAIL = 40;
/** The console re-reads its snapshot this often while open (the TUI's AUTO_REFRESH). */
const AUTO_REFRESH_MS = 15_000;

type Json = Record<string, unknown>;

interface Item {
  id: string;
  cells: string[];
  tone?: 'attn' | 'dim' | 'head' | 'more';
  indent?: number;
}

const VIEW_TITLES: Readonly<Record<ManagerView, string>> = {
  tree: 'Manager tree', board: 'Harness manager', scope: 'Manager scope', workspace: 'Global manager workspace',
  budget: 'Above all projects / Budget', fleet: 'Fleet', lists: 'Managers',
};

const mv = {
  view: null as ManagerView | null,
  projectId: null as string | null,
  epoch: 0,
  cursor: 0,
  loading: false,
  error: null as string | null,
  notice: null as string | null,
  tree: { rows: [] as TreeRow[], next: null as string | null, total: 0, complete: true, globalVersion: null as number | null, folded: new Set<string>() },
  board: {
    section: 'board' as BoardSection, sub: 'overview' as InspectSub, page: null as StatePage | null, cursors: [] as Array<string | null>,
    bands: {} as Partial<Record<BoardBand, StatePage>>, option: 0, answerRetry: null as { body: string; key: string } | null,
    kpis: [] as Json[], kpisBeyond: false,
  },
  policy: {
    config: null as ManagerConfig | null, saved: null as StatePolicy | null, draft: defaultPolicy(), retry: null as { body: string; key: string } | null,
    classified: null as PolicyClass | null,
  },
  scope: { config: null as ManagerConfig | null, cands: [] as ScopeCandidate[], draft: { whole: true, groups: [], epics: [] } as ScopeDraft, filter: '', seatId: null as string | null, complete: true },
  workspace: null as Workspace | null,
  /** A node console (#1240): the node shown instead of the global workspace, and its snapshot. */
  node: { target: null as NodeRef | null, data: null as NodeWorkspace | null },
  /** The selected seat's conversation shown beside the console (Tab focuses it). */
  conv: { sessionId: null as string | null, events: [] as ConversationEvent[], error: null as string | null },
  haltArmed: null as { id: string; at: number } | null,
  budget: null as BudgetSnapshot | null,
  fleet: { data: null as FleetOverview | null, groups: false, filter: '', sort: 0, gsort: 0, reverse: false, dim: 0, win: 1 },
  lists: { title: '', rows: [] as Json[], load: null as null | (() => Promise<Json[]>) },
};

type PolicyClass = Awaited<ReturnType<typeof managersApi.policyClassify>>;

let root: HTMLElement | null = null;
const part = (cls: string): HTMLElement => root!.querySelector<HTMLElement>(`.${cls}`)!;
const mint = (): string => crypto.randomUUID();
const errText = (e: unknown): string => String(e instanceof Error ? e.message : e);

// ---------- snapshot ----------

export function seatCandidate(): { id: string; project_id?: string | null; archived: boolean; leaf: boolean } | null {
  const s = selected();
  if (!s) return null;
  return { id: s.id, project_id: s.project_id, archived: s.status === 'Archived', leaf: s.session_kind !== 'Group' && s.session_kind !== 'Epic' };
}

export function managersSnapshot(): ManagersSnapshot {
  const c = seatCandidate();
  return { view: mv.view, hasProject: state.projects.length > 0, seatCandidate: !!c && !c.archived && c.leaf };
}

// ---------- open / close ----------

export function chooseProject(): string | null {
  const known = (id: string | null | undefined) => (id && state.projects.some((p) => p.id === id) ? id : null);
  const filter = document.querySelector<HTMLSelectElement>('#project-filter')?.value;
  return known(mv.projectId) ?? known(selected()?.project_id) ?? known(filter) ?? state.projects[0]?.id ?? null;
}

function show(view: ManagerView): void {
  const o = overlay();
  if (mv.view !== view) mv.cursor = 0;
  mv.view = view;
  mv.epoch++;
  mv.error = null;
  mv.notice = null;
  o.hidden = false;
  part('mgr-list').focus();
  render();
  uiChanged();
}

function close(): void {
  mv.view = null;
  mv.epoch++;
  if (root) root.hidden = true;
  uiChanged();
}

async function guarded(what: string, f: (epoch: number) => Promise<void>): Promise<void> {
  const epoch = mv.epoch;
  mv.loading = true;
  render();
  try {
    await f(epoch);
    if (epoch === mv.epoch) mv.error = null;
  } catch (e) {
    if (epoch === mv.epoch) mv.error = `${what}: ${errText(e)}`;
  } finally {
    if (epoch === mv.epoch) { mv.loading = false; render(); }
  }
}

// ---------- loaders ----------

async function loadTree(append: boolean): Promise<void> {
  await guarded('Manager tree', async (epoch) => {
    const page = await managersApi.tree(append ? mv.tree.next : null, TREE_PAGE);
    if (epoch !== mv.epoch) return;
    mv.tree.rows = append ? [...mv.tree.rows, ...page.rows] : page.rows;
    mv.tree.next = page.next_after;
    mv.tree.total = page.total_rows;
    mv.tree.complete = page.complete;
    mv.tree.globalVersion = page.global_grant_version;
  });
}

async function loadBoard(): Promise<void> {
  const pid = mv.projectId;
  if (!pid) { mv.error = 'No project'; render(); return; }
  const b = mv.board;
  await guarded(BOARD_SECTION_TITLES[b.section], async (epoch) => {
    if (b.section === 'policy') {
      const [config, saved] = await Promise.all([managersApi.config(pid), managersApi.policy(pid)]);
      if (epoch !== mv.epoch) return;
      mv.policy = { config, saved, draft: structuredClone(saved?.policy ?? defaultPolicy()), retry: null, classified: null };
      if (!config) mv.notice = 'No manager is appointed here: select a session and use :manager appoint';
      void classifyDraft();
      return;
    }
    if (b.section === 'board') {
      const pages = await Promise.all(BOARD_BANDS.map((s) => managersApi.state(pid, s, null, STATE_PAGE)));
      if (epoch !== mv.epoch) return;
      b.bands = Object.fromEntries(BOARD_BANDS.map((s, i) => [s, pages[i]])) as Record<BoardBand, StatePage>;
      b.page = b.bands.decisions ?? null;
      // The KPI line: at most KPI_OVERVIEW_PAGES Overview pages, only while a program/product row is missing.
      const kpis = kpiRows(b.bands.overview);
      let cursor = b.bands.overview?.next_cursor ?? null;
      for (let read = 1; !hasAllKpis(kpis) && cursor && read < KPI_OVERVIEW_PAGES; read++) {
        const next = await managersApi.state(pid, 'overview', cursor, STATE_PAGE);
        if (epoch !== mv.epoch) return;
        kpis.push(...kpiRows(next));
        cursor = next.next_cursor;
      }
      b.kpis = kpis;
      b.kpisBeyond = !hasAllKpis(kpis) && !!cursor;
      return;
    }
    const section = sectionQuery(b.section, b.sub)!;
    const page = await managersApi.state(pid, section, b.cursors.at(-1) ?? null, STATE_PAGE);
    if (epoch !== mv.epoch) return;
    b.page = page;
  });
}

async function loadScope(): Promise<void> {
  const pid = mv.projectId;
  if (!pid) return;
  await guarded('Manager scope', async (epoch) => {
    const config = await managersApi.config(pid);
    const cands: ScopeCandidate[] = [];
    let after: string | null = null;
    let complete = false;
    for (let i = 0; i < 16; i++) {
      const page = await managersApi.scopePage(pid, after);
      cands.push(...page.rows);
      if (!page.next_after_id) { complete = true; break; }
      after = page.next_after_id;
    }
    if (epoch !== mv.epoch) return;
    mv.scope.config = config;
    mv.scope.cands = cands;
    mv.scope.complete = complete;
    mv.scope.draft = scopeDraftForSeat(config, mv.scope.seatId);
    if (!mv.scope.seatId) mv.scope.seatId = config?.manager_session_id ?? null;
  });
}

async function loadWorkspace(): Promise<void> {
  const target = mv.node.target;
  await guarded(target ? 'Refresh failed (showing the last snapshot; r retries)' : 'Global manager workspace', async (epoch) => {
    if (target) {
      const n = await managersApi.nodeWorkspace(target.kind, nodeRefId(target)) as unknown as NodeWorkspace;
      if (epoch === mv.epoch) mv.node.data = n;
      return;
    }
    const w = await managersApi.workspace();
    if (epoch === mv.epoch) mv.workspace = w;
  });
}

function wsRows(): WorkspaceRow[] {
  return mv.node.target ? nodeWorkspaceRows(mv.node.data) : workspaceRows(mv.workspace);
}

/** Load the selected seat's recent conversation for the detail pane. */
async function loadConversation(sessionId: string): Promise<void> {
  mv.conv = { sessionId, events: [], error: null };
  try {
    const events = await api.getConversation(sessionId);
    if (mv.conv.sessionId === sessionId) mv.conv.events = events.slice(-CONVERSATION_TAIL);
  } catch (e) {
    if (mv.conv.sessionId === sessionId) mv.conv.error = errText(e);
  }
  if (mv.view === 'workspace') renderDetail();
}

async function loadBudget(): Promise<void> {
  await guarded('Budget', async (epoch) => {
    const b = await managersApi.budget(null);
    if (epoch === mv.epoch) mv.budget = b;
  });
}

async function loadFleet(): Promise<void> {
  await guarded('Fleet', async (epoch) => {
    const f = await managersApi.fleet();
    if (epoch === mv.epoch) mv.fleet.data = f;
  });
}

async function loadLists(): Promise<void> {
  const load = mv.lists.load;
  if (!load) return;
  await guarded(mv.lists.title, async (epoch) => {
    const rows = await load();
    if (epoch === mv.epoch) mv.lists.rows = rows;
  });
}

function reload(): void {
  switch (mv.view) {
    case 'tree': void loadTree(false); break;
    case 'board': void loadBoard(); break;
    case 'scope': void loadScope(); break;
    case 'workspace': void loadWorkspace(); break;
    case 'budget': void loadBudget(); break;
    case 'fleet': void loadFleet(); break;
    case 'lists': void loadLists(); break;
    default:
  }
}

// ---------- items per view ----------

function treeVisible(): TreeRow[] {
  return visibleTreeRows(mv.tree.rows, mv.tree.folded);
}

function boardItems(): Item[] {
  const b = mv.board;
  if (b.section === 'policy') {
    const c = mv.policy.classified;
    return POLICY_FIELDS.map((f) => {
      let value = policyValue(mv.policy.draft, f);
      if (f.kind === 'preset') value = c ? `matches ${c.preset}${c.validation_error ? ` (invalid: ${c.validation_error})` : ''}` : '…';
      if (f.kind === 'suggest') value = c?.suggestions.length ? c.suggestions.map((x) => `${x.field} ${x.from} → ${x.to}`).join(', ') : 'none needed';
      return { id: f.key, cells: [f.label, value], tone: f.kind === 'preset' || f.kind === 'suggest' || f.kind === 'advanced' ? 'head' : undefined };
    });
  }
  if (b.section === 'board') {
    const out: Item[] = [];
    for (const band of BOARD_BANDS) {
      const page = b.bands[band];
      out.push({ id: `head:${band}`, cells: [band.toUpperCase(), page ? `${page.rows.length}${page.next_cursor ? '+' : ''} row(s)` : 'not loaded'], tone: 'head' });
      for (const [i, row] of (page?.rows ?? []).slice(0, BAND_ROWS).entries()) {
        out.push({ id: `${band}:${i}`, cells: [rowTitle(row), band === 'decisions' ? decisionStatusLabel(row) : rowState(row)], indent: 1 });
      }
      if (page && (page.rows.length > BAND_ROWS || page.next_cursor)) {
        out.push({ id: `more:${band}`, cells: [`+more · Enter here opens full ${band}`, ''], tone: 'more', indent: 1 });
      }
    }
    return out;
  }
  return (b.page?.rows ?? []).map((row, i) => ({
    id: String(i),
    cells: [rowTitle(row), b.section === 'decisions' ? decisionStatusLabel(row) : rowState(row)],
    tone: rowState(row) === 'pending' ? 'attn' : undefined,
  }));
}

function scopeRows(): ScopeCandidate[] {
  const s = mv.scope;
  const gone = unavailableSelections(s.draft, s.cands).map((id) => ({ id, title: '(unavailable: select to remove)', kind: s.draft.groups.includes(id) ? 'Group' : 'Epic', group_id: null, group_title: null }));
  return [...filterScope(s.cands, s.filter), ...gone];
}

function budgetList(): BudgetRow[] {
  return budgetRows(mv.budget);
}

function items(): Item[] {
  switch (mv.view) {
    case 'tree':
      return treeVisible().map((r) => {
        const kids = parentKeys(mv.tree.rows).has(r.key);
        const fold = kids ? (mv.tree.folded.has(r.key) ? '▸ ' : '▾ ') : '  ';
        return {
          id: r.key, indent: r.depth, tone: treeNeedsAttention(r) ? 'attn' : undefined,
          cells: [`${fold}[${treeBadge(r)}] ${r.label}${r.complete ? '' : ' (incomplete)'}`, seatHealth(r.seat), treeGrantSummary(r), treeLoad(r)],
        };
      });
    case 'board': return boardItems();
    case 'scope':
      return scopeRows().map((c) => ({
        id: c.id, cells: [scopeMark(mv.scope.draft, c), c.kind, c.title, c.group_title ?? '', c.id.slice(0, 8)],
        indent: c.group_id ? 1 : 0,
      }));
    case 'workspace':
      return wsRows().map((r, i) => ({
        id: String(i), cells: [r.title, r.health, r.detail], indent: r.indent,
        tone: r.attention ? 'attn' : r.kind === 'predecessor' ? 'dim' : r.kind === 'child' ? 'head' : undefined,
      }));
    case 'budget':
      return budgetList().map((r, i) => ({
        id: String(i), tone: r.cap.binding ? 'attn' : undefined,
        cells: [r.project, `${r.node.kind} ${r.node.label}`, CAP_META[r.cap.field].label,
          `${describeCap(r.cap.field, r.cap.used)} / ${describeCap(r.cap.field, r.cap.limit)}${r.cap.binding ? ' (binding)' : ''}${r.cap.spend_unknown ? ' (spend unknown)' : ''}`],
      }));
    case 'fleet': {
      const f = mv.fleet;
      if (f.groups) {
        return fleetGroups(f.data, f.dim, f.win, f.filter, f.gsort, f.reverse).map((g) => ({
          id: g.key, cells: [g.label, `${g.active} active`, usageLine(g.windows[f.win], f.win)],
        }));
      }
      return fleetAgents(f.data, f.filter, f.sort, f.reverse).map((a) => ({
        id: a.session.id,
        cells: [a.project, a.role, `${a.session.provider}/${a.session.model ?? 'Unknown'}`, a.session.status,
          a.session.context_fill_pct === null || a.session.context_fill_pct === undefined ? 'ctx ?' : `ctx ${Math.round(a.session.context_fill_pct)}%`,
          a.session.title ?? a.session.query ?? ''],
      }));
    }
    case 'lists':
      return mv.lists.rows.map((row, i) => ({ id: String(i), cells: [rowTitle(row), rowState(row)] }));
    default: return [];
  }
}

// ---------- render ----------

function render(): void {
  if (!root || !mv.view) return;
  const list = items();
  mv.cursor = clampIndex(mv.cursor, list.length);
  part('mgr-title').textContent = mv.view === 'lists' ? mv.lists.title
    : mv.view === 'workspace' && mv.node.target ? `Manager console · ${mv.node.data?.label ?? nodeRefId(mv.node.target).slice(0, 8)}` : VIEW_TITLES[mv.view];
  renderHeader();
  const status = part('mgr-status');
  status.textContent = mv.error ?? mv.notice ?? (mv.loading ? 'Loading…' : '');
  status.className = `mgr-status${mv.error ? ' err' : ''}`;
  const ul = part('mgr-list');
  ul.replaceChildren(...list.map((it, i) => {
    const li = el('li', `mgr-item${i === mv.cursor ? ' sel' : ''}${it.tone ? ` ${it.tone}` : ''}`);
    li.dataset.index = String(i);
    li.style.paddingLeft = `${8 + (it.indent ?? 0) * 14}px`;
    for (const c of it.cells) li.append(el('span', 'mgr-cell', c));
    return li;
  }));
  if (!list.length && !mv.loading) ul.append(el('li', 'mgr-empty', mv.error ? '' : 'Nothing here'));
  ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  renderDetail();
  part('mgr-hints').textContent = hints();
}

function renderHeader(): void {
  const proj = part('mgr-project') as HTMLSelectElement;
  const scoped = mv.view === 'board' || mv.view === 'scope';
  proj.hidden = !scoped;
  if (scoped) {
    proj.replaceChildren(...state.projects.map((p) => { const o = el('option', undefined, p.name); o.value = p.id; return o; }));
    if (mv.projectId) proj.value = mv.projectId;
  }
  const tabs = part('mgr-tabs');
  tabs.replaceChildren();
  if (mv.view === 'board') {
    for (const [i, s] of BOARD_SECTIONS.entries()) {
      const t = el('button', `btn mgr-tab${mv.board.section === s ? ' on' : ''}`, `${i + 1} ${BOARD_SECTION_TITLES[s]}`);
      t.type = 'button';
      t.dataset.section = s;
      tabs.append(t);
    }
    if (mv.board.section === 'inspect') tabs.append(el('span', 'mgr-sub', `[ ${mv.board.sub} ]`));
  }
  const meta = part('mgr-meta');
  meta.textContent = headerMeta();
  const kpi = part('mgr-kpi');
  kpi.hidden = !(mv.view === 'board' && mv.board.section === 'board');
  kpi.textContent = kpi.hidden ? '' : `KPI · ${kpiLines(mv.board.kpis, mv.board.kpisBeyond).join(' | ')}`;
}

function headerMeta(): string {
  switch (mv.view) {
    case 'tree': {
      const t = mv.tree;
      return `${t.rows.length} of ${t.total}${t.complete ? '' : '+ (incomplete: a lower bound)'} row(s)${t.next ? ' · n loads more' : ''}`;
    }
    case 'board': {
      const b = mv.board;
      if (b.section === 'policy') {
        const p = mv.policy;
        const saved = p.saved ? `saved v${p.saved.row_version}${p.saved.revoked ? ' (REVOKED: s regrants this draft)' : ''}` : 'no saved policy (Status defaults)';
        const dirty = JSON.stringify(p.saved?.policy ?? defaultPolicy()) !== JSON.stringify(p.draft) ? ' · draft changed (s saves, r discards)' : '';
        return `${p.config ? `scope v${p.config.row_version}` : 'no manager'} · ${saved}${dirty}`;
      }
      const page = b.section === 'board' ? b.bands.overview : b.page;
      if (!page) return '';
      const pol = page.policy ? `${String(page.policy.policy.mode ?? '?')} v${page.policy.row_version}${page.policy.revoked ? ' (revoked)' : ''}` : 'no policy';
      return `scope v${page.scope_version} · policy ${pol} · observed ${page.observed_at}${page.complete ? '' : ' · incomplete'}${b.section !== 'board' ? ` · page ${b.cursors.length + 1}${page.next_cursor ? ' (n next)' : ''}` : ''}`;
    }
    case 'scope': {
      const s = mv.scope;
      const seat = s.seatId ? `seat ${s.seatId.slice(0, 8)}` : 'no seat';
      return `${seat} · ${scopeSummary(s.draft)} · ${s.cands.length} candidate(s)${s.complete ? '' : '+'}${s.filter ? ` · filter “${s.filter}”` : ''}`;
    }
    case 'workspace': {
      if (mv.node.target) return mv.node.data ? nodeHeader(mv.node.data) : '';
      const w = mv.workspace;
      if (!w) return '';
      return w.grant ? `grant v${w.grant.grant_version} · ${w.grant.state} · origin ${w.grant.operator_origin}${w.missing_project_ids.length ? ` · ${w.missing_project_ids.length} missing project(s)` : ''}` : 'No global manager grant';
    }
    case 'budget': return mv.budget ? `${mv.budget.node_label} · ${mv.budget.resets}` : '';
    case 'fleet': {
      const f = mv.fleet;
      const d = f.data;
      if (!d) return '';
      const sort = f.groups ? GROUP_SORTS[f.gsort] : AGENT_SORTS[f.sort];
      return `${f.groups ? `usage by ${FLEET_DIMENSIONS[f.dim]}` : `${d.agents.length} active agent(s)`} · window ${FLEET_WINDOW_TITLES[f.win]} · sort ${sort}${f.reverse ? ' ↓' : ''}${f.filter ? ` · filter “${f.filter}”` : ''} · total ${usageLine(d.totals[f.win], f.win)}${d.agents_truncated || d.usage_truncated ? ' · truncated' : ''} · as of ${d.as_of}`;
    }
    default: return '';
  }
}

function kv(rows: Array<[string, string]>): HTMLElement {
  const dl = el('dl', 'info-rows');
  for (const [k, v] of rows) dl.append(el('dt', undefined, k), el('dd', undefined, v));
  return dl;
}

function selectedBoardRow(): { row: Json; page: StatePage } | null {
  const b = mv.board;
  const it = items()[mv.cursor];
  if (!it) return null;
  if (b.section === 'board') {
    const [band, idx] = it.id.split(':');
    const page = b.bands[band as BoardBand];
    const row = page?.rows[Number(idx)];
    return row && page ? { row, page } : null;
  }
  const row = b.page?.rows[Number(it.id)];
  return row && b.page ? { row, page: b.page } : null;
}

function renderDetail(): void {
  const pane = part('mgr-detail');
  pane.replaceChildren();
  const i = mv.cursor;
  switch (mv.view) {
    case 'tree': {
      const r = treeVisible()[i];
      if (!r) return;
      pane.append(el('h3', undefined, `${treeBadge(r)} ${r.label}`), kv([
        ['Key', r.key], ['Grantor', r.grantor ?? '—'], ['Scope', r.scope ?? '—'], ['Seat', seatHealth(r.seat)],
        ['Seat session', r.seat?.session_id ?? '—'], ['Grant', treeGrantSummary(r) || '—'],
        ['Capabilities', r.grant?.capabilities.join(', ') || '—'], ['Load', treeLoad(r)],
        ['Descendants', String(descendants(mv.tree.rows, r.key).length)],
      ]));
      return;
    }
    case 'board': {
      if (mv.board.section === 'policy') {
        const f = POLICY_FIELDS[i];
        const problems = policyProblems(mv.policy.draft);
        if (f) pane.append(el('h3', undefined, f.label), el('p', 'mgr-note', POLICY_HELP[f.kind]));
        if (problems.length) pane.append(el('h4', undefined, 'Problems'), ...problems.map((p) => el('p', 'mgr-err', p)));
        return;
      }
      const sel = selectedBoardRow();
      if (!sel) return;
      if (mv.board.section === 'decisions' || items()[i]?.id.startsWith('decisions:')) {
        const { options, recommended } = decisionOptions(sel.row);
        pane.append(el('h3', undefined, rowTitle(sel.row)), el('p', 'mgr-note', decisionStatusLabel(sel.row)));
        const block = decisionBlock(sel.row, sel.page);
        if (options.length) {
          const ol = el('ol', 'mgr-options');
          for (const [n, o] of options.entries()) {
            const li = el('li', n === mv.board.option ? 'sel' : '', `${o.label}${o.text ? `: ${o.text}` : ''}${n === recommended ? '  (recommended)' : ''}`);
            ol.append(li);
          }
          pane.append(ol);
        }
        pane.append(el('p', 'mgr-note', block ?? 'h/l highlight · Enter answers · y accepts the recommendation · a writes an answer'));
      }
      pane.append(kv(recordDetail(sel.row)));
      return;
    }
    case 'scope': {
      const c = scopeRows()[i];
      if (c) pane.append(el('h3', undefined, c.title), kv([['Kind', c.kind], ['Group', c.group_title ?? '—'], ['ID', c.id], ['Mark', scopeMark(mv.scope.draft, c)]]));
      pane.append(el('p', 'mgr-note', '[x] explicit · [+] inherited. Space toggles, a selects the whole project, Enter saves; an empty explicit selection revokes supervision.'));
      return;
    }
    case 'workspace': {
      const r = wsRows()[i];
      if (!r) return;
      pane.append(el('h3', undefined, r.title), kv([['Seat', r.health], ['Signals', r.detail], ['Session', r.sessionId ?? '—']]));
      if (r.kind === 'child') { pane.append(el('p', 'mgr-note', 'Enter opens this node’s console; Backspace there returns here.')); return; }
      if (!r.sessionId) return;
      if (mv.conv.sessionId !== r.sessionId) { void loadConversation(r.sessionId); return; }
      pane.append(el('h4', undefined, `Conversation (last ${CONVERSATION_TAIL} events) · Tab focuses, i types`));
      if (mv.conv.error) pane.append(el('p', 'mgr-err', mv.conv.error));
      for (const ev of mv.conv.events) {
        if (!ev.content) continue;
        const who = ev.role ?? ev.event_type;
        const block = el('div', `mgr-conv ${ev.role === 'User' ? 'user' : ''}`);
        block.append(el('b', undefined, `${who}${ev.tool_name ? ` · ${ev.tool_name}` : ''}`), el('pre', undefined, ev.content.length > 1200 ? `${ev.content.slice(0, 1200)}…` : ev.content));
        pane.append(block);
      }
      return;
    }
    case 'budget': {
      const r = budgetList()[i];
      if (!r) return;
      pane.append(el('h3', undefined, `${r.node.label}: ${CAP_META[r.cap.field].label}`), kv([
        ['Kind', r.node.kind], ['Project', r.project], ['Used / limit', `${describeCap(r.cap.field, r.cap.used)} / ${describeCap(r.cap.field, r.cap.limit)}`],
        ['Binding', r.cap.binding ? 'yes: the lowest headroom on this chain' : 'no'], ['Counted since', r.node.counted_since ?? '—'],
        ['Fence', `v${r.node.version} · epoch ${r.node.epoch}`],
      ]), el('p', 'mgr-note', budgetEditBlock(r) ?? `+ raises one step (${CAP_META[r.cap.field].step}); Enter types a new limit.`));
      return;
    }
    case 'fleet': {
      const f = mv.fleet;
      if (f.groups) {
        const g = fleetGroups(f.data, f.dim, f.win, f.filter, f.gsort, f.reverse)[i];
        if (g) pane.append(el('h3', undefined, g.label), kv(FLEET_WINDOW_TITLES.map((t, w) => [t, usageLine(g.windows[w], w)])));
        return;
      }
      const a = fleetAgents(f.data, f.filter, f.sort, f.reverse)[i];
      if (a) {
        pane.append(el('h3', undefined, a.session.title ?? a.session.query ?? a.session.id), kv([
          ['Project', a.project], ['Role', a.role], ['Session', a.session.id], ['Model', `${a.session.provider}/${a.session.model ?? 'Unknown'}/${a.session.effort ?? 'default'}`],
          ['Status', a.session.status], ['Turn started', a.turn_started_at ?? '—'],
        ]));
      }
      return;
    }
    case 'lists': {
      const row = mv.lists.rows[i];
      if (row) pane.append(el('h3', undefined, rowTitle(row)), kv(recordDetail(row)));
      return;
    }
    default:
  }
}

const POLICY_HELP: Readonly<Record<string, string>> = {
  preset: 'Enter applies Observe, Execute or Full project control to the draft (kept until s saves). Any other edit makes it Custom.',
  suggest: 'Enter raises only the named zero allowances the matched preset needs.',
  advanced: 'Enter edits every field of the policy as JSON; the draft changes only when the daemon’s validator accepts it.',
  json: 'Enter edits this list as JSON (ids, or the daemon’s objects); the whole policy is checked before it reaches the draft.',
  launches: 'Enter opens the launch picker: check catalog choices or type Provider/model[/effort]. Empty means any valid choice.',
  cycle: 'Enter or Space steps to the next value.', bool: 'Enter or Space toggles.', caps: 'Enter opens the capability checklist.',
  int: 'Enter types a new value.', spend: 'Enter types a new cap; blank removes it.',
};

function hints(): string {
  switch (mv.view) {
    case 'tree': return 'j/k g/G move · h/l Space H/L fold · Enter open · o project · p preview · a appoint · e edit · x revoke · A appoint above · m move · n more · r refresh · Esc close';
    case 'board': return mv.board.section === 'policy'
      ? '1-5 · Tab/Shift-Tab sections · j/k fields · Enter/Space edit · s save · r reload · Esc close'
      : '1-5 · Tab/Shift-Tab sections · [ ] inspect · j/k · Enter open/answer · h/l option · y recommended · a answer · X archive stale · o session · n/p page · PgUp/PgDn detail · r · Esc';
    case 'scope': return 'j/k · Space toggle · a whole project · / filter · Enter save · Esc cancel';
    case 'workspace': return 'j/k · Enter/l open (a child: its console) · Backspace parent · i type · t talk to the seat · s status · x/X halt · a archive earlier seat · n launch and appoint · Tab conversation · p project · b budget · T tree · F fleet · : commands · r · Esc';
    case 'budget': return 'j/k · + raise one step · Enter type a limit · r refresh · Esc back';
    case 'fleet': return 'j/k · / filter · Tab agents/usage · s sort · S reverse · b group · w window · Enter open · r · Esc';
    default: return 'j/k · Enter open session · r refresh · Esc close';
  }
}

// ---------- dialogs ----------

export interface AskSpec {
  title: string;
  lines?: string[];
  input?: { value: string; placeholder?: string; multiline?: boolean };
  checks?: { options: readonly string[]; on: readonly string[] };
  ok: string;
  destructive?: boolean;
}

/** One modal: text, checkboxes or a plain confirm. Resolves null on cancel. */
export function ask(spec: AskSpec): Promise<{ text: string; checked: string[] } | null> {
  return new Promise((resolve) => {
    const d = el('dialog', 'pick mgr-dialog');
    const form = el('form');
    form.method = 'dialog';
    form.append(el('h2', undefined, spec.title));
    for (const l of spec.lines ?? []) form.append(el('p', 'mgr-note', l));
    let input: HTMLInputElement | HTMLTextAreaElement | null = null;
    if (spec.input) {
      input = spec.input.multiline ? el('textarea') : el('input');
      if (input instanceof HTMLInputElement) { input.type = 'text'; input.dataset.vim = 'on'; }
      input.value = spec.input.value;
      input.placeholder = spec.input.placeholder ?? '';
      if (input instanceof HTMLTextAreaElement) input.rows = 6;
      form.append(input);
    }
    const boxes: HTMLInputElement[] = [];
    if (spec.checks) {
      const wrap = el('div', 'mgr-checks');
      for (const o of spec.checks.options) {
        const lab = el('label');
        const cb = el('input');
        cb.type = 'checkbox';
        cb.value = o;
        cb.checked = spec.checks.on.includes(o);
        boxes.push(cb);
        lab.append(cb, document.createTextNode(` ${o}`));
        wrap.append(lab);
      }
      form.append(wrap);
    }
    const actions = el('div', 'pick-actions');
    const cancel = el('button', 'btn', 'Cancel');
    cancel.type = 'button';
    const ok = el('button', `btn primary${spec.destructive ? ' danger' : ''}`, spec.ok);
    ok.type = 'submit';
    actions.append(cancel, ok);
    form.append(actions);
    if (!spec.input) form.append(el('p', 'pick-hint', spec.destructive ? 'y sends · n or Esc cancels (Enter does not confirm)' : 'y or Enter sends · n or Esc cancels'));
    d.append(form);
    document.body.append(d);
    let done = false;
    const finish = (v: { text: string; checked: string[] } | null) => {
      if (done) return;
      done = true;
      d.close();
      d.remove();
      resolve(v);
    };
    const accept = () => finish({ text: input?.value ?? '', checked: boxes.filter((b) => b.checked).map((b) => b.value) });
    cancel.addEventListener('click', () => finish(null));
    form.addEventListener('submit', (ev) => {
      ev.preventDefault();
      if (spec.destructive && !spec.input && ev.submitter !== ok) return;
      accept();
    });
    d.addEventListener('cancel', (ev) => { ev.preventDefault(); finish(null); });
    d.addEventListener('keydown', (ev) => {
      if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
      if (input && document.activeElement === input) {
        if (ev.key === 'Enter' && (input instanceof HTMLInputElement || ev.ctrlKey || ev.metaKey)) { ev.preventDefault(); accept(); }
        return;
      }
      if (ev.key === 'y') { ev.preventDefault(); accept(); } else if (ev.key === 'n') { ev.preventDefault(); finish(null); } else if (ev.key === 'Enter' && spec.destructive && document.activeElement !== ok) { ev.preventDefault(); }
    });
    d.showModal();
    (input ?? (spec.destructive ? cancel : ok)).focus();
  });
}

// ---------- shared actions ----------

function openSession(id: string | null): void {
  if (!id) { toast('This row has no session to open'); return; }
  if (!state.sessions.some((s) => s.id === id) && !state.archived.some((s) => s.id === id)) {
    toast(`Session ${id.slice(0, 8)} is not in the session list (check the project filter)`);
    return;
  }
  close();
  openSessionRow(id);
}

function openProject(projectId: string | null): void {
  const f = document.querySelector<HTMLSelectElement>('#project-filter');
  if (!projectId || !f) { toast('This row has no project'); return; }
  f.value = projectId;
  f.dispatchEvent(new Event('change'));
  close();
}

async function write(what: string, f: () => Promise<unknown>, after: () => void): Promise<void> {
  try {
    await f();
    mv.notice = what;
    mv.error = null;
    toast(what, true);
  } catch (e) {
    mv.error = `The daemon refused: ${errText(e)}`;
    toast(mv.error);
  }
  after();
}

// ---------- tree ----------

function treeRow(): TreeRow | null {
  return treeVisible()[mv.cursor] ?? null;
}

function selectTreeKey(key: string): void {
  mv.cursor = Math.max(0, treeVisible().findIndex((r) => r.key === key));
}

async function treeAppoint(r: TreeRow): Promise<void> {
  const block = treeActionBlock(r, 'appoint', seatCandidate());
  if (block) { toast(block); return; }
  const cand = seatCandidate()!;
  const pid = r.project_id!;
  let live: ManagerConfig | null;
  try { live = await managersApi.config(pid); } catch (e) { toast(`Manager: ${errText(e)}`); return; }
  const req = appointRequest(pid, cand.id, live);
  const ok = await ask({
    title: r.focus_session_id ? `Replace the manager seat of ${r.label}` : `Appoint a manager for ${r.label}`,
    lines: [`Seat: ${r.focus_session_id?.slice(0, 8) ?? 'none'} → ${cand.id.slice(0, 8)}.`, `Scope: ${'epic_ids' in req ? 'kept selection' : 'whole project'}.`, `ConfigureHarnessManager (expects row v${String(req.expected_row_version)})`],
    ok: r.focus_session_id ? 'Replace' : 'Appoint', destructive: !!r.focus_session_id,
  });
  if (ok) await write(`Manager seat of ${r.label} saved`, () => managersApi.configureScope(req), () => void loadTree(false));
}

async function treeRevoke(r: TreeRow): Promise<void> {
  const block = treeActionBlock(r, 'revoke', null);
  if (block) { toast(block); return; }
  const impact = `${descendants(mv.tree.rows, r.key).length} row(s) below it are affected.`;
  try {
    if (r.kind === 'project') {
      const c = await managersApi.config(r.project_id!);
      if (!c) { toast('No manager to revoke'); return; }
      if (await ask({ title: `Revoke the manager of ${r.label}`, lines: [impact, 'Clears the scope (ConfigureHarnessManager with an empty selection).'], ok: 'Revoke', destructive: true })) {
        await write(`Manager of ${r.label} revoked`, () => managersApi.configureScope(scopeClearRequest(c)), () => void loadTree(false));
      }
    } else if (r.kind === 'portfolio' && r.node_id) {
      const node = await managersApi.portfolioNode(r.node_id);
      if (!node) { toast('The node is gone; refreshing'); void loadTree(false); return; }
      if (r.grant && r.grant.grant_version !== node.grant.grant_version) { toast('The tree was stale; refreshed'); void loadTree(false); return; }
      if (await ask({ title: `Revoke ${node.tier_label} node ${r.label}`, lines: [impact, 'Operator-granted children move up a level; the node’s own appointees are revoked with their subtree.'], ok: 'Revoke', destructive: true })) {
        await write(`${r.label} revoked`, () => managersApi.revokePortfolio({ node_id: node.node_id, expected_grant_version: node.grant.grant_version, expected_authority_epoch: node.authority_epoch, idempotency_key: mint() }), () => void loadTree(false));
      }
    } else if (r.kind === 'global') {
      const v = mv.tree.globalVersion;
      if (!v) { toast('No active global grant version'); return; }
      if (await ask({ title: 'Revoke the global manager', lines: [impact, `RevokeGlobalManager (expects grant v${v})`], ok: 'Revoke', destructive: true })) {
        await write('Global manager revoked', () => managersApi.revokeGlobal(v, mint()), () => void loadTree(false));
      }
    } else if (r.kind === 'area' && r.node_id && r.project_id) {
      const node = await managersApi.areaNode(r.project_id, r.node_id);
      if (!node) { toast('The node is gone; refreshing'); void loadTree(false); return; }
      if (await ask({ title: `Revoke area node ${r.label}`, lines: [impact], ok: 'Revoke', destructive: true })) {
        await write(`${r.label} revoked`, () => managersApi.revokeArea({ project_id: r.project_id, node_id: r.node_id, expected_grant_version: node.grant_version, expected_authority_epoch: node.authority_epoch, idempotency_key: mint() }), () => void loadTree(false));
      }
    }
  } catch (e) {
    toast(`Revoke: ${errText(e)}`);
  }
}

function treeContext(): TreeContext {
  return {
    rows: mv.tree.rows, globalVersion: mv.tree.globalVersion, candidate: seatCandidate(),
    reload: (notice) => { if (notice) mv.notice = notice; void loadTree(false); },
  };
}

function treeKey(action: string): void {
  const r = treeRow();
  switch (action) {
    case 'mgr-fold': if (r) { const f = foldOrParent(mv.tree.rows, mv.tree.folded, r.key); mv.tree.folded = f.folded; selectTreeKey(f.select); } break;
    case 'mgr-unfold': if (r) mv.tree.folded.delete(r.key); break;
    case 'mgr-toggle-fold': if (r && parentKeys(mv.tree.rows).has(r.key)) { if (!mv.tree.folded.delete(r.key)) mv.tree.folded.add(r.key); } break;
    case 'mgr-fold-all': {
      let top = r;
      while (top?.parent_key) top = mv.tree.rows.find((x) => x.key === top!.parent_key) ?? null;
      mv.tree.folded = parentKeys(mv.tree.rows);
      if (top) selectTreeKey(top.key);
      break;
    }
    case 'mgr-unfold-all': { const key = r?.key; mv.tree.folded = new Set(); if (key) selectTreeKey(key); break; }
    case 'mgr-page-down': mv.cursor += 20; break;
    case 'mgr-page-up': mv.cursor -= 20; break;
    case 'mgr-enter':
      if (!r) break;
      if (r.kind === 'global') { void openWorkspace(); return; }
      if (r.kind === 'portfolio' && r.node_id) { void openNodeConsole({ kind: 'portfolio', node_id: r.node_id }); return; }
      if (r.kind === 'area' && r.node_id) { void openNodeConsole({ kind: 'area', node_id: r.node_id }); return; }
      if (r.kind === 'project' && r.project_id) { void openNodeConsole({ kind: 'project', project_id: r.project_id }); return; }
      openSession(r.focus_session_id);
      return;
    case 'mgr-open-project': openProject(r?.project_id ?? null); return;
    case 'mgr-preview':
      if (r) {
        const fc = seatCandidate();
        const av = (['appoint', 'edit', 'revoke', 'above', 'move'] as const).map((a) => `${a}: ${treeActionBlock(r, a, fc) ?? 'available'}`);
        mv.notice = `${r.label}: ${descendants(mv.tree.rows, r.key).length} descendant(s) · ${av.join(' · ')}`;
      }
      break;
    case 'mgr-appoint': {
      if (!r) return;
      if (r.kind === 'project') { void treeAppoint(r); return; }
      const block = treeActionBlock(r, 'appoint', seatCandidate());
      if (block) { toast(block); return; }
      void appointSeat(treeContext(), r);
      return;
    }
    case 'mgr-edit': {
      const block = treeActionBlock(r, 'edit', null);
      if (block) { toast(block); return; }
      if (r!.kind === 'area') { void editArea(treeContext(), r!); return; }
      if (r!.kind !== 'project') { void editGrant(treeContext(), r!); return; }
      mv.projectId = r!.project_id;
      void openScope(null);
      return;
    }
    case 'mgr-above': case 'mgr-move': {
      const block = treeActionBlock(r, action === 'mgr-above' ? 'above' : 'move', seatCandidate());
      if (block) { toast(block); return; }
      if (action === 'mgr-above') void appointAbove(treeContext(), r!);
      else void moveUnder(treeContext(), r!).then(() => { mv.notice = pendingMove() ? `Moving ${pendingMove()}: m on a sibling moves it there; Esc cancels` : mv.notice; render(); });
      return;
    }
    case 'mgr-revoke': if (r) void treeRevoke(r); return;
    case 'mgr-next-page': if (mv.tree.next) void loadTree(true); else toast('This is the whole tree'); return;
    default:
  }
  render();
}

// ---------- board ----------

function setSection(s: BoardSection): void {
  mv.board.section = s;
  mv.board.cursors = [];
  mv.board.page = null;
  mv.board.option = 0;
  mv.cursor = 0;
  mv.epoch++;
  mv.notice = null;
  void loadBoard();
}

async function answer(row: Json, page: StatePage, text: string): Promise<void> {
  const pid = mv.projectId!;
  const body = JSON.stringify([pid, rowTitle(row), row.row_version ?? (row.payload as Json | undefined)?.row_version, text]);
  mv.board.answerRetry = retryKey(mv.board.answerRetry, body, mint);
  const req = answerRequest(pid, row, page, text, mv.board.answerRetry.key);
  try {
    await managersApi.answer(req);
    mv.board.answerRetry = null;
    mv.notice = 'Answer submitted. A receipt is not delivery: refresh (r) to observe the outcome.';
    toast('Answer submitted', true);
    void loadBoard();
  } catch (e) {
    mv.error = `Answer refused: ${errText(e)} (the draft and retry key are kept; a stale record needs r first)`;
    render();
  }
}

async function answerSelected(how: 'option' | 'recommended' | 'write'): Promise<void> {
  const sel = selectedBoardRow();
  const block = decisionBlock(sel?.row ?? null, sel?.page ?? null);
  if (block || !sel) { toast(block ?? 'Select a pending decision.'); return; }
  const { options, recommended } = decisionOptions(sel.row);
  if (how === 'recommended') {
    if (recommended === null) { toast('This decision names no recommendation'); return; }
    await answer(sel.row, sel.page, optionAnswer(options[recommended]!));
    return;
  }
  if (how === 'option' && options.length) {
    await answer(sel.row, sel.page, optionAnswer(options[clampIndex(mv.board.option, options.length)]!));
    return;
  }
  const got = await ask({ title: `Answer: ${rowTitle(sel.row)}`, input: { value: '', placeholder: 'Your answer (Ctrl-Enter sends)', multiline: true }, ok: 'Send answer' });
  if (got && got.text.trim()) await answer(sel.row, sel.page, got.text.trim());
}

async function archiveStale(): Promise<void> {
  try {
    const list = await managersApi.staleDecisions(mv.projectId);
    if (!list.rows.length) { toast(`No pending decision older than ${list.older_than_days} day(s)`, true); return; }
    const ok = await ask({
      title: `Archive ${list.rows.length} stale decision(s)?`,
      lines: [`Pending records older than ${list.older_than_days} day(s) or whose manager is gone. Archiving never deletes.${list.complete ? '' : ' More remain after this page.'}`],
      ok: 'Archive', destructive: true,
    });
    if (!ok) return;
    await write(`${list.rows.length} stale decision(s) archived`, () => managersApi.archiveStale({ items: list.rows.map((r) => r.decision), older_than_days: list.older_than_days }), () => void loadBoard());
  } catch (e) {
    toast(`Stale decisions: ${errText(e)}`);
  }
}

/** Re-read which preset the draft matches and what it would suggest (manager_ops.rs over rsi-common). */
async function classifyDraft(): Promise<void> {
  const p = mv.policy;
  const draft = p.draft;
  try {
    const c = await managersApi.policyClassify(draft, p.config?.scope_mode ?? 'project');
    if (p.draft === draft) { p.classified = c; render(); }
  } catch {
    if (p.draft === draft) { p.classified = null; render(); }
  }
}

function setDraft(draft: Json): void {
  mv.policy.draft = draft;
  mv.policy.classified = null;
  render();
  void classifyDraft();
}

/** Check a whole draft with the daemon's validator before it replaces the form's draft. */
async function checkedDraft(draft: Json): Promise<boolean> {
  try {
    setDraft(await managersApi.policyCheck(draft));
    return true;
  } catch (e) {
    toast(errText(e));
    return false;
  }
}

async function editLaunches(): Promise<void> {
  const p = mv.policy;
  const current = (p.draft.allowed_launches as LaunchChoice[] | undefined) ?? [];
  let defaults: LaunchChoice[] = [];
  let granted: LaunchChoice[] = [];
  try { defaults = (await managersApi.defaultGrant()).launches as unknown as LaunchChoice[]; } catch { /* the catalog keeps the draft's own */ }
  // The global grant's launch list is the ceiling every PM below it inherits.
  try { granted = ((await managersApi.globalGrant()) as { allowed_launches?: LaunchChoice[] } | null)?.allowed_launches ?? []; } catch { /* none */ }
  const catalog = launchCatalog(current, granted, defaults);
  const got = await ask({
    title: 'Allowed launches',
    lines: ['Checked choices are allowed; none checked means any valid choice, including future models. Type more as Provider/model[/effort].'],
    checks: { options: catalog.map(launchText), on: current.map((l) => launchText({ ...l, effort: l.effort ?? null })) },
    input: { value: '', placeholder: 'Codex/gpt-6-astra/high Claude/claude-opus-5-5' },
    ok: 'Apply',
  });
  if (!got) return;
  const picked = pickLaunches(catalog, got.checked, got.text);
  if ('error' in picked) { toast(picked.error); return; }
  await checkedDraft({ ...p.draft, allowed_launches: picked.launches });
}

async function editPolicyField(): Promise<void> {
  const f = POLICY_FIELDS[mv.cursor];
  if (!f) return;
  const p = mv.policy;
  const scope = p.config?.scope_mode ?? 'project';
  switch (f.kind) {
    case 'bool': case 'cycle': setDraft(stepPolicy(p.draft, f)); return;
    case 'preset': {
      const got = await ask({ title: 'Apply a preset', lines: ['Prepares the draft for one save (s). Mode, capabilities and the allowances the preset needs change; explicit limits are kept.'], checks: { options: PRESETS.map((x) => x.label), on: [] }, ok: 'Apply' });
      const pick = PRESETS.find((x) => got?.checked.includes(x.label));
      if (!got) return;
      if (!pick || got.checked.length !== 1) { toast('Check exactly one preset'); return; }
      try { setDraft(await managersApi.policyPreset(p.draft, pick.id, scope, !!p.saved)); } catch (e) { toast(errText(e)); }
      return;
    }
    case 'suggest':
      try { setDraft(await managersApi.policySuggestions(p.draft, scope)); } catch (e) { toast(errText(e)); }
      return;
    case 'advanced': {
      const got = await ask({ title: 'Advanced: the whole policy', input: { value: JSON.stringify(p.draft, null, 2), multiline: true }, ok: 'Check and apply' });
      if (!got) return;
      let v: unknown;
      try { v = JSON.parse(got.text); } catch (e) { toast(`Policy JSON: ${errText(e)}`); return; }
      if (!v || typeof v !== 'object' || Array.isArray(v)) { toast('The policy must be a JSON object'); return; }
      await checkedDraft(v as Json);
      return;
    }
    case 'json': {
      const got = await ask({ title: f.label, input: { value: JSON.stringify(p.draft[f.key] ?? [], null, 2), multiline: true }, ok: 'Check and apply' });
      if (!got) return;
      const r = parseListField(got.text, f.label);
      if ('error' in r) { toast(r.error); return; }
      await checkedDraft({ ...p.draft, [f.key]: r.value });
      return;
    }
    case 'launches': await editLaunches(); return;
    case 'caps': {
      const got = await ask({ title: 'Capabilities', checks: { options: CAPABILITIES, on: (p.draft.capabilities as string[] | undefined) ?? [] }, ok: 'Apply' });
      if (!got) return;
      let draft: Json = { ...p.draft, capabilities: [] };
      for (const c of got.checked) draft = toggleCapability(draft, c);
      setDraft(draft);
      return;
    }
    default: {
      const raw = p.draft[f.key];
      const got = await ask({ title: f.label, input: { value: raw === null || raw === undefined ? '' : String(raw), placeholder: f.kind === 'spend' ? 'blank = no cap' : `${f.min}..${f.max}` }, ok: 'Apply' });
      if (!got) return;
      const r = setPolicyScalar(p.draft, f, got.text);
      if ('error' in r) { toast(r.error); return; }
      setDraft(r.policy);
    }
  }
}

async function savePolicy(): Promise<void> {
  const p = mv.policy;
  if (!p.config) { toast('No manager is appointed: :manager appoint first'); return; }
  const problems = policyProblems(p.draft);
  if (problems.length) { toast(problems[0]!); return; }
  const body = JSON.stringify([p.config.row_version, p.saved?.row_version ?? 0, p.draft]);
  p.retry = retryKey(p.retry, body, mint);
  try {
    const saved = await managersApi.configurePolicy(policySaveRequest(p.config, p.saved, p.draft, p.retry.key));
    p.saved = saved;
    setDraft(structuredClone(saved.policy));
    p.retry = null;
    mv.notice = `Policy saved (v${saved.row_version})`;
    mv.error = null;
    toast(mv.notice, true);
  } catch (e) {
    mv.error = `Policy save refused: ${errText(e)} (the draft and retry key are kept; r reloads)`;
  }
  render();
}

function boardKey(action: string): void {
  const b = mv.board;
  switch (action) {
    case 'mgr-section-1': case 'mgr-section-2': case 'mgr-section-3': case 'mgr-section-4': case 'mgr-section-5':
      setSection(BOARD_SECTIONS[Number(action.slice(-1)) - 1]!);
      return;
    case 'mgr-section-next': setSection(cycle(BOARD_SECTIONS, b.section, 1)); return;
    case 'mgr-section-prev': setSection(cycle(BOARD_SECTIONS, b.section, -1)); return;
    case 'mgr-detail-down': case 'mgr-detail-up': {
      const pane = part('mgr-detail');
      pane.scrollBy(0, (action === 'mgr-detail-down' ? 1 : -1) * Math.max(40, pane.clientHeight * 0.9));
      return;
    }
    case 'mgr-sub-prev': case 'mgr-sub-next':
      if (b.section !== 'inspect') return;
      b.sub = cycle(INSPECT_SUBSECTIONS, b.sub, action === 'mgr-sub-next' ? 1 : -1);
      setSection('inspect');
      return;
    case 'mgr-option-prev': b.option = Math.max(0, b.option - 1); break;
    case 'mgr-option-next': b.option += 1; { const s = selectedBoardRow(); if (s) b.option = clampIndex(b.option, decisionOptions(s.row).options.length); } break;
    case 'mgr-enter': {
      if (b.section === 'policy') { void editPolicyField(); return; }
      if (b.section === 'board') {
        const it = items()[mv.cursor];
        if (!it) return;
        const edge = it.id.startsWith('head:') || it.id.startsWith('more:');
        const [first, second] = it.id.split(':');
        const band = edge ? second : first;
        const index = edge ? 0 : Number(second);
        const target: Record<string, [BoardSection, InspectSub]> = { decisions: ['decisions', b.sub], requests: ['inbox', b.sub], overview: ['inspect', 'overview'], health: ['inspect', 'health'] };
        const [section, sub] = target[band ?? ''] ?? ['board', b.sub];
        b.sub = sub;
        setSection(section);
        mv.cursor = index;
        return;
      }
      if (b.section === 'decisions') { void answerSelected('option'); return; }
      const s = selectedBoardRow();
      if (s) openSession(rowSession(s.row));
      return;
    }
    case 'mgr-accept-recommended': void answerSelected('recommended'); return;
    case 'mgr-answer':
      if (b.section === 'decisions' || items()[mv.cursor]?.id.startsWith('decisions:')) void answerSelected('write');
      return;
    case 'mgr-archive-stale': if (b.section === 'decisions') void archiveStale(); return;
    case 'mgr-open-session': { const s = selectedBoardRow(); openSession(s ? rowSession(s.row) : null); return; }
    case 'mgr-next-page':
      if (b.section === 'board' || b.section === 'policy') return;
      if (!b.page?.next_cursor) { toast('This is the last page'); return; }
      b.cursors.push(b.page.next_cursor);
      mv.cursor = 0;
      void loadBoard();
      return;
    case 'mgr-prev-page':
      if (!b.cursors.length) { toast('This is the first page'); return; }
      b.cursors.pop();
      mv.cursor = 0;
      void loadBoard();
      return;
    case 'mgr-save': if (b.section === 'policy') void savePolicy(); return;
    case 'mgr-space': if (b.section === 'policy') void editPolicyField(); return;
    default:
  }
  render();
}

// ---------- scope ----------

async function saveScope(): Promise<void> {
  const s = mv.scope;
  const pid = mv.projectId;
  if (!pid) return;
  const seat = s.seatId ?? s.config?.manager_session_id ?? null;
  if (!seat) { toast('No manager to scope: select a session and use :manager appoint'); return; }
  const req = scopeSaveRequest(pid, seat, s.draft, s.config?.row_version ?? 0);
  const revoke = !s.draft.whole && !s.draft.groups.length && !s.draft.epics.length;
  if (revoke && !(await ask({ title: 'Revoke supervision?', lines: ['The explicit selection is empty: saving revokes this manager’s scope.'], ok: 'Revoke', destructive: true }))) return;
  try {
    const out = await managersApi.configureScope(req);
    const policy = (out.policy ?? null) as Json | null;
    const outcome = policy ? ` · policy: ${String(policy.outcome ?? policy.state ?? JSON.stringify(policy)).slice(0, 120)}` : '';
    toast(`Scope saved${outcome}`, true);
    mv.notice = `Scope saved (v${String(out.row_version)})${outcome}`;
    s.seatId = null;
    void loadScope();
  } catch (e) {
    mv.error = `Scope save refused: ${errText(e)} (on a stale revision, cancel and reopen to review the latest scope)`;
    render();
  }
}

function scopeKey(action: string): void {
  const s = mv.scope;
  switch (action) {
    case 'mgr-toggle': {
      const c = scopeRows()[mv.cursor];
      if (!c) return;
      const r = toggleScope(s.draft, c);
      if ('error' in r) toast(r.error); else s.draft = r.draft;
      break;
    }
    case 'mgr-whole-project': s.draft = { whole: true, groups: [], epics: [] }; break;
    case 'mgr-filter':
      void ask({ title: 'Filter Groups and Epics', input: { value: s.filter, placeholder: 'name, Group name or ID' }, ok: 'Filter' }).then((got) => {
        if (got) { s.filter = got.text; mv.cursor = 0; render(); }
      });
      return;
    case 'mgr-save': void saveScope(); return;
    default:
  }
  render();
}

// ---------- workspace and budget ----------

async function commitRaise(r: BudgetRow, to: number | null): Promise<void> {
  const field = r.cap.field;
  const ok = await ask({
    title: `Set ${r.node.label} ${CAP_META[field].label}`,
    lines: [`${describeCap(field, r.cap.limit)} → ${describeCap(field, to)} (${r.node.kind}: ${r.node.kind === 'portfolio' ? 'this node and everything under it' : 'this PM’s own launches'}).`, mv.budget?.resets ?? ''],
    ok: 'Save',
  });
  if (!ok) return;
  try {
    let version: number;
    if (r.node.kind === 'portfolio') {
      const node = await managersApi.portfolioNode(r.node.node_id);
      if (!node) throw new Error('The portfolio node is gone; press r to reload');
      const built = portfolioRaiseRequest(r.node, node, field, to, mint());
      if ('error' in built) throw new Error(built.error);
      version = (await managersApi.configurePortfolio(built.request, true)).grant.grant_version;
    } else {
      const current = await managersApi.policy(r.node.node_id);
      if (!current) throw new Error('The project has no PM policy; press r to reload');
      const built = projectRaiseRequest(r.node, current, field, to, mint());
      if ('error' in built) throw new Error(built.error);
      version = (await managersApi.configurePolicy(built.request)).row_version;
    }
    await loadBudget();
    const back = budgetRows(mv.budget).find((x) => x.node.kind === r.node.kind && x.node.node_id === r.node.node_id && x.cap.field === field)?.cap.limit ?? null;
    if (back === to) mv.notice = `Saved ${r.node.kind} ${r.node.label} ${CAP_META[field].label}: ${describeCap(field, r.cap.limit)} → ${describeCap(field, to)} (v${version}); read back ${describeCap(field, back)}`;
    else mv.error = `Saved v${version}, but the panel reads back ${describeCap(field, back)} (expected ${describeCap(field, to)}); press r`;
  } catch (e) {
    mv.error = `The daemon refused: ${errText(e)}`;
  }
  render();
}

function budgetKey(action: string): void {
  const r = budgetList()[mv.cursor] ?? null;
  if (action === 'mgr-raise' || action === 'mgr-type-limit') {
    const block = budgetEditBlock(r);
    if (block) { toast(block); return; }
    if (action === 'mgr-raise') {
      const t = raiseTo(r!.cap, null);
      if ('to' in t) void commitRaise(r!, t.to);
      return;
    }
    void ask({ title: `New ${CAP_META[r!.cap.field].label} limit`, input: { value: describeCap(r!.cap.field, r!.cap.limit).replace('none', '') }, ok: 'Review' }).then((got) => {
      if (!got) return;
      const t = raiseTo(r!.cap, got.text);
      if ('error' in t) toast(t.error); else void commitRaise(r!, t.to);
    });
  }
}

async function seatSession(r: WorkspaceRow): Promise<{ status: string } | null> {
  try { return await api.getSession(r.sessionId!); } catch (e) { toast(`Session ${r.sessionId!.slice(0, 8)} unavailable: ${errText(e)}; press r`); return null; }
}

async function typeToSeat(r: WorkspaceRow | null): Promise<void> {
  const block = seatActionBlock(r, 'type');
  if (block) { toast(block); return; }
  const s = await seatSession(r!);
  if (!s) return;
  if (s.status === 'Archived' || s.status === 'Deleted') { toast(`Session ${r!.sessionId!.slice(0, 8)} is ${s.status} and read-only; n launches a manager`); return; }
  const got = await ask({ title: `Message ${r!.title}`, input: { value: '', placeholder: 'Ctrl-Enter sends', multiline: true }, ok: 'Send' });
  if (!got?.text.trim()) return;
  try {
    await api.sendMessage(r!.sessionId!, got.text);
    toast(`Sent to ${r!.title}`, true);
    void loadConversation(r!.sessionId!);
  } catch (e) {
    toast(`Send failed: ${errText(e)}`);
  }
}

async function haltSeat(r: WorkspaceRow | null, hard: boolean): Promise<void> {
  const block = seatActionBlock(r, 'halt');
  if (block) { toast(block); return; }
  const s = await seatSession(r!);
  if (!s) return;
  if (!HALTABLE.includes(s.status)) { toast(`${r!.title} ${r!.sessionId!.slice(0, 8)} is ${s.status}; only a starting, running or waiting session can be halted`); return; }
  const id = r!.sessionId!;
  if (hard) {
    const armed = mv.haltArmed;
    if (!armed || armed.id !== id || Date.now() - armed.at > 5000) {
      mv.haltArmed = { id, at: Date.now() };
      toast('INTERRUPT NOW cancels the active turn. Press X again within 5 seconds to confirm.');
      return;
    }
    mv.haltArmed = null;
  }
  try {
    await api.pauseSession(id, hard ? 'hard' : 'soft');
    mv.notice = `Halt requested for ${r!.title} ${id.slice(0, 8)}`;
  } catch (e) {
    mv.error = `Interrupt failed: ${errText(e)}`;
  }
  void loadWorkspace();
}

async function archiveSeat(r: WorkspaceRow | null): Promise<void> {
  const block = seatActionBlock(r, 'archive');
  if (block) { toast(block); return; }
  if (!(await ask({ title: `Archive ${r!.title}?`, lines: ['An earlier seat session; archiving is reversible from the Archived view.'], ok: 'Archive' }))) return;
  await write(`${r!.title} archived`, () => api.archiveSession(r!.sessionId!), () => void loadWorkspace());
}

async function launchSeat(r: WorkspaceRow | null): Promise<void> {
  let role: LaunchRole;
  if (r?.level === 'project' && r.projectId) {
    role = { kind: 'project', projectId: r.projectId, name: state.projects.find((p) => p.id === r.projectId)?.name ?? r.title };
  } else if (mv.node.target) {
    toast('Here n launches a project manager; appoint portfolio and area seats from the manager tree (a)');
    return;
  } else {
    role = { kind: 'global' };
  }
  try {
    const out = await launchAndAppoint(role, chooseProject());
    if (!out) return;
    mv.notice = out.message;
    toast(out.message, true);
    await loadWorkspace();
    const at = wsRows().findIndex((x) => x.sessionId === out.sessionId);
    if (at >= 0) mv.cursor = at;
    render();
  } catch (e) {
    mv.error = errText(e);
    render();
  }
}

function focusConversation(): void {
  const pane = part('mgr-detail');
  pane.focus();
  mv.notice = 'Conversation: j/k, Ctrl-D/U scroll, g/G ends; Tab or Esc returns to the seats';
  render();
}

function workspaceKey(action: string): void {
  const r = wsRows()[mv.cursor] ?? null;
  switch (action) {
    case 'mgr-enter':
      if (!r) { toast('Nothing to open: n launches a manager, or focus a session and run :manager global appoint'); return; }
      if (r.kind === 'child' && r.node) { void openNodeConsole(r.node); return; }
      if (r.kind === 'missing') { toast(r.detail); return; }
      if (!r.sessionId && r.kind === 'project') { openProject(r.projectId); return; }
      if (!r.sessionId) { toast(mv.node.target ? 'This node has no seat session to open; appoint one from the manager tree (a)' : 'No global manager session to open; n launches one'); return; }
      openSession(r.sessionId);
      return;
    case 'mgr-open-project': openProject(r?.projectId ?? null); return;
    case 'mgr-budget': void openBudget(); return;
    case 'mgr-tree': void openTree(); return;
    case 'mgr-fleet': void openFleet(); return;
    case 'mgr-type': void typeToSeat(r); return;
    case 'mgr-talk': mv.cursor = 0; render(); void typeToSeat(wsRows()[0] ?? null); return;
    case 'mgr-status': {
      const block = seatActionBlock(r, 'status');
      if (block) { toast(block); return; }
      void seatSession(r!).then((s) => {
        if (s) mv.notice = `${r!.title} ${r!.sessionId!.slice(0, 8)}${r!.kind === 'predecessor' ? ' (earlier session, read only)' : ''}: status ${s.status}, seat ${r!.health || 'unknown'}`;
        render();
      });
      return;
    }
    case 'mgr-halt': void haltSeat(r, false); return;
    case 'mgr-halt-now': void haltSeat(r, true); return;
    case 'mgr-archive-seat': void archiveSeat(r); return;
    case 'mgr-launch': void launchSeat(r); return;
    case 'mgr-conversation': focusConversation(); return;
    case 'mgr-parent': {
      const parent = mv.node.data?.parent ?? null;
      if (parent) void openNodeConsole(parent);
      else toast('This console has no manager node above it here.');
      return;
    }
    default:
  }
}

// ---------- fleet ----------

/** When the fleet's first `y` of `yy` was pressed. */
let lastY = 0;
/** `s` was pressed: the next 1-5 picks a sort column (the TUI's Fleet Sort). */
let sortPending = false;

function fleetKey(action: string): void {
  const f = mv.fleet;
  switch (action) {
    case 'mgr-focus': f.groups = !f.groups; mv.cursor = 0; break;
    case 'mgr-sort':
      sortPending = true;
      mv.notice = `Sort by: ${(f.groups ? GROUP_SORTS : AGENT_SORTS).map((c, i) => `${i + 1} ${c}`).join(' · ')} (Esc cancels)`;
      break;
    case 'mgr-sort-1': case 'mgr-sort-2': case 'mgr-sort-3': case 'mgr-sort-4': case 'mgr-sort-5': {
      if (!sortPending) return;
      sortPending = false;
      mv.notice = null;
      const n = Number(action.slice(-1)) - 1;
      if (f.groups) f.gsort = n; else f.sort = n;
      break;
    }
    case 'mgr-reverse': f.reverse = !f.reverse; break;
    case 'mgr-dimension': f.dim = (f.dim + 1) % FLEET_DIMENSIONS.length; f.groups = true; mv.cursor = 0; break;
    case 'mgr-window': f.win = (f.win + 1) % FLEET_WINDOW_TITLES.length; break;
    case 'mgr-filter':
      void ask({ title: 'Filter agents and usage groups', input: { value: f.filter }, ok: 'Filter' }).then((got) => {
        if (got) { f.filter = got.text; mv.cursor = 0; render(); }
      });
      return;
    case 'mgr-copy': {
      const now = Date.now();
      const id = f.groups ? null : items()[mv.cursor]?.id;
      if (id && now - lastY <= 800) { lastY = 0; void copyText(id, 'session UUID'); return; }
      lastY = now;
      return;
    }
    case 'mgr-enter':
      if (f.groups) { f.groups = false; mv.cursor = 0; break; }
      openSession(items()[mv.cursor]?.id ?? null);
      return;
    default:
  }
  render();
}

// ---------- openers ----------

export async function openTree(): Promise<void> {
  show('tree');
  await loadTree(false);
}

async function openBoard(section: BoardSection, sub?: InspectSub): Promise<void> {
  mv.projectId = chooseProject();
  if (!mv.projectId) { toast('No projects yet'); return; }
  if (sub) mv.board.sub = sub;
  show('board');
  setSection(section);
}

async function openScope(seatId: string | null): Promise<void> {
  mv.projectId = mv.projectId ?? chooseProject();
  if (!mv.projectId) { toast('No projects yet'); return; }
  mv.scope.seatId = seatId;
  mv.scope.filter = '';
  show('scope');
  await loadScope();
  if (seatId) mv.notice = `Appointing ${seatId.slice(0, 8)}: Enter saves the scope with this session as the manager`;
  render();
}

async function openWorkspace(): Promise<void> {
  mv.node = { target: null, data: null };
  show('workspace');
  await loadWorkspace();
}

/** #1240: the console of one manager node (from the tree, or a child row). */
async function openNodeConsole(node: NodeRef): Promise<void> {
  mv.node = { target: node, data: null };
  mv.cursor = 0;
  show('workspace');
  await loadWorkspace();
}

async function openBudget(): Promise<void> {
  show('budget');
  await loadBudget();
}

async function openFleet(): Promise<void> {
  show('fleet');
  await loadFleet();
}

export async function openList(title: string, load: () => Promise<Json[]>): Promise<void> {
  mv.lists = { title, rows: [], load };
  show('lists');
  await loadLists();
}

/** After a manager command: re-read the workspace when it is open (the TUI's refresh_if_open). */
export async function refreshWorkspaceIfOpen(): Promise<void> {
  if (mv.view === 'workspace') await loadWorkspace();
}

async function clearScope(): Promise<void> {
  const pid = chooseProject();
  if (!pid) return;
  try {
    const c = await managersApi.config(pid);
    if (!c) { toast('No manager is appointed in this project'); return; }
    if (await ask({ title: 'Clear the manager scope?', lines: ['Revokes supervision (ConfigureHarnessManager with an empty selection).'], ok: 'Clear', destructive: true })) {
      await write('Manager scope cleared', () => managersApi.configureScope(scopeClearRequest(c)), () => render());
    }
  } catch (e) { toast(`Manager: ${errText(e)}`); }
}

async function revokeGlobal(): Promise<void> {
  try {
    const g = await managersApi.globalGrant();
    if (!g || g.state !== 'active') { toast('There is no active global manager grant'); return; }
    if (await ask({ title: 'Revoke the global manager?', lines: [`Grant v${g.grant_version}, seat ${g.seat_session_id.slice(0, 8)}: the seat keeps its session but loses its authority.`], ok: 'Revoke', destructive: true })) {
      await write('Global manager revoked', () => managersApi.revokeGlobal(g.grant_version, mint()), () => { if (mv.view === 'workspace') void loadWorkspace(); });
    }
  } catch (e) { toast(`Global manager: ${errText(e)}`); }
}

// ---------- dispatch ----------

export function runManagers(id: ManagersActionId, inv: Invocation): void {
  switch (id) {
    case 'mgr-tree': void openTree(); return;
    case 'mgr-board': void openBoard('board'); return;
    case 'mgr-decisions': void openBoard('decisions'); return;
    case 'mgr-inbox': void openBoard('inbox'); return;
    case 'mgr-inspect': void openBoard('inspect'); return;
    case 'mgr-policy': void openBoard('policy'); return;
    case 'mgr-scope': mv.projectId = chooseProject(); void openScope(null); return;
    case 'mgr-appoint': {
      const c = seatCandidate();
      if (!c || c.archived || !c.leaf) { toast('Select an unarchived session to appoint first'); return; }
      mv.projectId = c.project_id ?? chooseProject();
      void openScope(c.id);
      return;
    }
    case 'mgr-clear': void clearScope(); return;
    case 'mgr-global': void openWorkspace(); return;
    case 'mgr-global-grant':
      if (inv.arg) { void runManagerCommand('global', inv.arg); return; }
      void openWorkspace();
      return;
    case 'mgr-global-appoint': void runManagerCommand('global', `appoint ${inv.arg ?? ''}`); return;
    case 'mgr-global-revoke': void revokeGlobal(); return;
    case 'mgr-budget': void openBudget(); return;
    case 'mgr-fleet': void openFleet(); return;
    case 'mgr-portfolio':
      if (inv.arg && inv.arg.trim() !== 'list') { void runManagerCommand('portfolio', inv.arg); return; }
      void openList('Portfolio nodes', async () => {
        const all = state.projects.map((p) => ({ id: p.id, name: p.name }));
        const nodes = (await managersApi.portfolioNodes(false)).nodes;
        if (!nodes.length) return [{ title: 'No portfolio nodes. Focus a session and run :manager portfolio appoint <label> [projects...].', state: '' }];
        return nodes.map((n) => ({ ...n, title: portfolioSummary(n as never, all) }) as unknown as Json);
      });
      return;
    case 'mgr-escalations': void runManagerCommand('escalations', inv.arg); return;
    case 'mgr-node': void runManagerCommand('node', inv.arg); return;
    case 'mgr-restart': void runManagerCommand('restart', inv.arg); return;
    case 'mgr-friction': {
      const hours = inv.arg ? Number(inv.arg) : null;
      if (inv.arg && !(Number.isInteger(hours) && hours! >= 1 && hours! <= 720)) { toast('Hours must be 1..720'); return; }
      void openList(`Friction (last ${hours ?? 24} h)`, async () => ((await managersApi.friction(hours)).rows as Json[] | undefined ?? []).map((r) => ({ title: `${String(r.kind)}: ${String(r.signature)}`, state: `${String(r.occurrences)}×`, ...r })));
      return;
    }
    case 'mgr-refresh': reload(); return;
    case 'mgr-close': close(); return;
  }
}

function viewKey(action: string): void {
  if (sortPending && action !== 'mgr-close' && !action.startsWith('mgr-sort-')) { sortPending = false; mv.notice = null; }
  switch (action) {
    case 'mgr-close':
      if (sortPending) { sortPending = false; mv.notice = null; render(); return; }
      if (mv.view === 'tree' && cancelMove()) { mv.notice = 'Move cancelled'; render(); return; }
      if (mv.view === 'budget') { void openWorkspace(); return; }
      close();
      return;
    case 'mgr-refresh': reload(); return;
    case 'mgr-down': mv.cursor += 1; mv.board.option = 0; render(); return;
    case 'mgr-up': mv.cursor -= 1; mv.board.option = 0; render(); return;
    case 'mgr-first': mv.cursor = 0; mv.board.option = 0; render(); return;
    case 'mgr-last': mv.cursor = items().length - 1; mv.board.option = 0; render(); return;
    default:
  }
  switch (mv.view) {
    case 'tree': treeKey(action); return;
    case 'board': boardKey(action); return;
    case 'scope': scopeKey(action); return;
    case 'workspace': workspaceKey(action); return;
    case 'budget': budgetKey(action); return;
    case 'fleet': fleetKey(action); return;
    case 'lists':
      if (action === 'mgr-open-session') { const row = mv.lists.rows[mv.cursor]; openSession(row ? rowSession(row) : null); }
      return;
    default:
  }
}

// ---------- wiring ----------

const CSS = `
.mgr-view { position: fixed; inset: 0; z-index: 40; display: flex; flex-direction: column; background: var(--base); color: var(--text); }
.mgr-view[hidden] { display: none; }
.mgr-head { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding: 8px 12px; border-bottom: 1px solid var(--surface1); background: var(--mantle); }
.mgr-title { font-weight: 600; color: var(--mauve); }
.mgr-meta { font-size: 12px; color: var(--subtext); flex: 1 1 100%; }
.mgr-kpi { font-size: 12px; color: var(--teal); flex: 1 1 100%; font-family: var(--mono); }
.mgr-kpi[hidden] { display: none; }
.mgr-tab.on { background: var(--surface1); color: var(--text); }
.mgr-sub { font-family: var(--mono); color: var(--peach); font-size: 12px; }
.mgr-status { padding: 4px 12px; font-size: 12px; color: var(--green); min-height: 16px; }
.mgr-status.err { color: var(--red); }
.mgr-body { flex: 1; display: grid; grid-template-columns: minmax(0, 3fr) minmax(260px, 2fr); min-height: 0; }
.mgr-list { list-style: none; margin: 0; padding: 0; overflow: auto; outline: none; border-right: 1px solid var(--surface1); }
.mgr-item { display: grid; grid-template-columns: minmax(180px, 2fr) repeat(auto-fit, minmax(80px, 1fr)); gap: 10px; padding: 3px 8px; font-size: 12px; font-family: var(--mono); cursor: pointer; white-space: nowrap; }
.mgr-item .mgr-cell { overflow: hidden; text-overflow: ellipsis; }
.mgr-item.sel { background: var(--surface0); }
.mgr-item.attn .mgr-cell:first-child { color: var(--yellow); }
.mgr-item.head { color: var(--blue); font-weight: 600; }
.mgr-item.more, .mgr-item.dim { color: var(--overlay0); }
.mgr-empty { padding: 12px; color: var(--overlay0); }
.mgr-detail { overflow: auto; padding: 10px 14px; font-size: 13px; }
.mgr-detail h3 { margin: 0 0 8px; font-size: 14px; color: var(--blue); }
.mgr-note { color: var(--subtext); font-size: 12px; }
.mgr-err { color: var(--red); font-size: 12px; margin: 2px 0; }
.mgr-options li.sel { color: var(--green); font-weight: 600; }
.mgr-detail:focus { outline: 1px solid var(--mauve); }
.mgr-conv { margin: 6px 0; font-size: 12px; }
.mgr-conv pre { white-space: pre-wrap; margin: 2px 0 0; font-family: var(--mono); }
.mgr-conv.user b { color: var(--blue); }
.mgr-hints { padding: 4px 12px; font-size: 11px; color: var(--overlay0); border-top: 1px solid var(--surface1); }
.mgr-checks { display: grid; grid-template-columns: 1fr 1fr; gap: 4px 12px; margin: 6px 0 10px; font-size: 13px; }
.mgr-dialog textarea { width: 100%; box-sizing: border-box; background: var(--base); color: var(--text); border: 1px solid var(--surface1); border-radius: 6px; padding: 6px 8px; margin-bottom: 8px; }
.btn.danger { background: var(--red); color: var(--crust); }
`;

function overlay(): HTMLElement {
  if (root) return root;
  const o = el('section', 'mgr-view');
  o.id = 'managers-view';
  o.hidden = true;
  o.setAttribute('aria-label', 'Managers');
  const head = el('header', 'mgr-head');
  const proj = el('select', 'mgr-project');
  proj.setAttribute('aria-label', 'Project');
  const refresh = el('button', 'btn', 'Refresh');
  refresh.type = 'button';
  refresh.dataset.mgr = 'mgr-refresh';
  const closeBtn = el('button', 'btn', 'Close');
  closeBtn.type = 'button';
  closeBtn.dataset.mgr = 'mgr-close';
  const kpi = el('div', 'mgr-kpi');
  kpi.hidden = true;
  head.append(el('span', 'mgr-title'), proj, el('span', 'mgr-tabs'), refresh, closeBtn, el('div', 'mgr-meta'), kpi);
  const list = el('ul', 'mgr-list');
  list.tabIndex = 0;
  const body = el('div', 'mgr-body');
  const detail = el('div', 'mgr-detail');
  detail.tabIndex = -1;
  body.append(list, detail);
  o.append(head, el('div', 'mgr-status'), body, el('div', 'mgr-hints'));
  document.body.append(o);
  root = o;
  proj.addEventListener('change', () => {
    mv.projectId = proj.value;
    mv.epoch++;
    list.focus();
    if (mv.view === 'board') setSection(mv.board.section);
    else if (mv.view === 'scope') void openScope(null);
  });
  head.addEventListener('click', (ev) => {
    const t = (ev.target as HTMLElement).closest<HTMLElement>('button');
    if (t?.dataset.section) setSection(t.dataset.section as BoardSection);
    else if (t?.dataset.mgr) viewKey(t.dataset.mgr);
  });
  list.addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.mgr-item');
    if (!li?.dataset.index) return;
    mv.cursor = Number(li.dataset.index);
    mv.board.option = 0;
    render();
  });
  list.addEventListener('dblclick', () => viewKey(mv.view === 'scope' ? 'mgr-toggle' : 'mgr-enter'));
  return o;
}

function isTyping(t: HTMLElement | null): boolean {
  return !!t?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
}

export function initManagers(): void {
  const style = el('style');
  style.textContent = CSS + FORM_CSS;
  window.setInterval(() => { if (mv.view === 'workspace' && !mv.loading && !document.querySelector('dialog[open]')) void loadWorkspace(); }, AUTO_REFRESH_MS);
  document.head.append(style);
  // Capture phase: an open manager view takes its keys before the global key engine.
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!paneKeyAllowed(isVimKeysOn(), ev)) return;
    // Tab put the keys in the workspace conversation: scroll it, Tab or Esc returns to the seats.
    const pane = root?.querySelector<HTMLElement>('.mgr-detail');
    if (mv.view && pane && document.activeElement === pane && !document.querySelector('dialog[open]')) {
      const half = Math.max(40, pane.clientHeight / 2);
      const scroll: Record<string, number> = { j: 40, ArrowDown: 40, k: -40, ArrowUp: -40, PageDown: pane.clientHeight, PageUp: -pane.clientHeight };
      if (ev.ctrlKey && (ev.key === 'd' || ev.key === 'u')) pane.scrollBy(0, ev.key === 'd' ? half : -half);
      else if (ev.key in scroll && !ev.ctrlKey) pane.scrollBy(0, scroll[ev.key]!);
      else if (ev.key === 'g') pane.scrollTop = 0;
      else if (ev.key === 'G') pane.scrollTop = pane.scrollHeight;
      else if (ev.key === 'Tab' || ev.key === 'Escape') { part('mgr-list').focus(); mv.notice = null; render(); }
      else return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      return;
    }
    if (!mv.view || ev.isComposing || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (document.querySelector('dialog[open]') || isTyping(document.activeElement as HTMLElement | null)) return;
    const name = ev.key === 'Tab' && ev.shiftKey ? 'S-Tab' : ev.key;
    const action = VIEW_KEYS[mv.view][name];
    if (!action) return;
    if (ev.key === 'Enter' && (document.activeElement as HTMLElement | null)?.closest('button')) return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
    viewKey(action);
  }, true);
}
