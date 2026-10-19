// Recursive DAG browser (hub #3013, slice 11): the TUI's `:dag` overlay
// (crates/rsi/src/overlay/recursive_dag.rs, ui/overlay/recursive_dag.rs and
// types/recursive_dag.rs) as pure state: panels, the rows each panel lists,
// keys, the exact read requests and the gated control inputs. dag.ts runs the
// requests and draws it. Daemon rows stay loosely typed (`Row`): the browser
// shows what the readback holds and never invents a field.

export type Row = Record<string, unknown>;

export const PANELS = ['graphs', 'tasks', 'runs', 'recovery', 'cancellations', 'heartbeats', 'interrupts', 'live', 'artifacts'] as const;
export type Panel = typeof PANELS[number];
export const PANEL_LABEL: Record<Panel, string> = {
  graphs: 'Graphs', tasks: 'Tasks', runs: 'Runs', recovery: 'Recovery', cancellations: 'Cancellations',
  heartbeats: 'Heartbeats', interrupts: 'Interrupts', live: 'Live', artifacts: 'Artifacts',
};

export const GRAPH_LIMIT = 100;
export const TASK_LIMIT = 200;
export const RUN_LIMIT = 12;
export const EVENT_LIMIT = 80;
export const LIVE_LIMIT = 24;
export const ARTIFACT_LIMIT = 24;
export const REASON_LIMIT = 240;

/** GetDaemonCapabilities: the flags the browser reads. */
export type Caps = Record<string, unknown>;
export const cap = (c: Caps | null, name: string): boolean => c?.[name] === true;

export interface ArtifactPage { status: 'ready' | 'disabled' | 'error'; items: Row[]; nextCursor: string | null; hasMore: boolean; warnings: string[]; message: string | null }

export interface Detail {
  /** GetRecursiveTaskGraph: `{graph, nodes, edges, attempts, …}`. */
  graph: Row;
  operational: Row | null;
  runs: Row[];
  runDetail: Row | null;
  runEvents: Row[];
  cancellations: Row[];
  recovery: Row | null;
  heartbeats: Row[];
  liveRecovery: Row | null;
  live: Row[];
  validations: Row[];
  artifacts: ArtifactPage;
  warnings: string[];
}

export type Control =
  | { kind: 'none' }
  | { kind: 'cancel-prefix' }
  | { kind: 'reason'; target: 'graph' | 'run'; input: string; error: string | null }
  | { kind: 'budget'; maxGraphs: string; timeBudget: string; field: 'max_graphs' | 'time_budget_ms'; error: string | null }
  | { kind: 'steps'; live: boolean; input: string; error: string | null };

export type InspectorView = 'metadata' | 'links' | 'preview' | 'test' | 'diff';

export interface DagState {
  status: 'loading' | 'ready' | 'error' | 'disabled';
  message: string | null;
  caps: Caps | null;
  projectId: string | null;
  graphs: Row[];
  /** The graph whose detail is loaded. */
  hydrated: string | null;
  detail: Detail | null;
  panel: Panel;
  cursor: Record<Panel, number>;
  inspector: { open: boolean; view: InspectorView };
  control: Control;
  gates: boolean;
  loadedAt: string | null;
  busy: boolean;
}

const ZERO = Object.fromEntries(PANELS.map((p) => [p, 0])) as Record<Panel, number>;

export function initialDag(projectId: string | null): DagState {
  return {
    status: 'loading', message: null, caps: null, projectId, graphs: [], hydrated: null, detail: null,
    panel: 'graphs', cursor: { ...ZERO }, inspector: { open: false, view: 'metadata' }, control: { kind: 'none' },
    gates: false, loadedAt: null, busy: false,
  };
}

// ---------- field access ----------

export const str = (o: unknown, k: string): string | null => {
  const v = (o as Row | null | undefined)?.[k];
  return typeof v === 'string' ? v : typeof v === 'number' ? String(v) : null;
};
export const arr = (o: unknown, k: string): Row[] => {
  const v = (o as Row | null | undefined)?.[k];
  return Array.isArray(v) ? (v.filter((x) => x && typeof x === 'object') as Row[]) : [];
};
export const obj = (o: unknown, k: string): Row | null => {
  const v = (o as Row | null | undefined)?.[k];
  return v && typeof v === 'object' && !Array.isArray(v) ? (v as Row) : null;
};
/** Ids arrive as a bare UUID or `{0: uuid}`-free newtypes serialized transparently; keep the string. */
export const idOf = (o: unknown, k = 'id'): string | null => str(o, k);
export const short = (id: string | null | undefined): string => (id ? id.slice(0, 8) : '-');

/** `MM-DD HH:MM` in UTC, as the TUI formats timestamps. */
export function ts(v: string | null | undefined): string {
  if (!v) return '-';
  const m = v.match(/^\d{4}-(\d{2})-(\d{2})T(\d{2}):(\d{2})/);
  return m ? `${m[1]}-${m[2]} ${m[3]}:${m[4]}` : v;
}

/** Enum values print as the TUI's Debug names (`in_progress` → `InProgress`). */
export function word(v: unknown): string {
  if (typeof v !== 'string' || v === '') return '-';
  return v.split('_').map((p) => p.charAt(0).toUpperCase() + p.slice(1)).join('');
}

// ---------- rows per panel (types/recursive_dag.rs) ----------

/** The graph inventory, newest first (load_recursive_dag_browser sorts by updated_at desc). */
export function sortGraphs(graphs: readonly Row[]): Row[] {
  return [...graphs].sort((a, b) => (str(b, 'updated_at') ?? '').localeCompare(str(a, 'updated_at') ?? '')).slice(0, GRAPH_LIMIT);
}

/** Task tree rows `[path, task]`: root first, children by created_at then title, then orphans (task_tree_rows). */
export function taskTreeRows(detail: Detail): Array<{ path: string; task: Row }> {
  const nodes = arr(detail.graph, 'nodes');
  const byId = new Map<string, Row>();
  const kids = new Map<string, Row[]>();
  for (const t of nodes) {
    const id = idOf(t);
    if (id) byId.set(id, t);
    const parent = str(t, 'parent_task_id') ?? '';
    if (!kids.has(parent)) kids.set(parent, []);
    kids.get(parent)!.push(t);
  }
  for (const list of kids.values()) {
    list.sort((a, b) => (str(a, 'created_at') ?? '').localeCompare(str(b, 'created_at') ?? '') || (str(a, 'title') ?? '').localeCompare(str(b, 'title') ?? ''));
  }
  const rows: Array<{ path: string; task: Row }> = [];
  const seen = new Set<string>();
  const walk = (id: string | null, path: string) => {
    if (!id || seen.has(id)) return;
    seen.add(id);
    const t = byId.get(id);
    if (!t) return;
    rows.push({ path, task: t });
    (kids.get(id) ?? []).forEach((c, i) => walk(idOf(c), `${path}.${i + 1}`));
  };
  walk(str(obj(detail.graph, 'graph'), 'root_task_id'), '1');
  let orphan = 2;
  for (const t of nodes) {
    const id = idOf(t);
    if (id && !seen.has(id)) { walk(id, String(orphan)); orphan += 1; }
  }
  return rows.slice(0, TASK_LIMIT);
}

function dedupe(lists: Row[][], key: (r: Row) => string | null): Row[] {
  const seen = new Set<string>();
  const out: Row[] = [];
  for (const list of lists) {
    for (const r of list) {
      const k = key(r);
      if (k === null || seen.has(k)) continue;
      seen.add(k);
      out.push(r);
    }
  }
  return out;
}

export function cancellationRows(d: Detail): Row[] {
  return dedupe([d.cancellations, arr(d.operational, 'open_cancellation_requests'), arr(d.runDetail, 'cancellation_requests')], (r) => idOf(r)).slice(0, LIVE_LIMIT);
}

export function deferredRows(d: Detail): Row[] {
  const key = (r: Row) => str(r, 'graph_id') ?? str(r, 'raw_graph_id');
  const fromLive = obj(d.liveRecovery, 'deferred_graph');
  return dedupe([arr(d.recovery, 'deferred_graphs'), fromLive ? [fromLive] : []], key);
}

export function heartbeatRows(d: Detail): Row[] {
  const key = (r: Row) => str(r, 'live_attempt_id');
  const fromLive = d.live.map((a) => obj(a, 'heartbeat')).filter((x): x is Row => !!x);
  return dedupe([d.heartbeats, arr(d.operational, 'active_live_heartbeats'), arr(d.runDetail, 'live_heartbeat_states'), fromLive, arr(d.liveRecovery, 'heartbeat_states')], key).slice(0, LIVE_LIMIT);
}

export function interruptRows(d: Detail): Row[] {
  const fromLive = d.live.map((a) => obj(a, 'latest_interrupt')).filter((x): x is Row => !!x);
  return dedupe([arr(d.operational, 'active_live_interrupts'), arr(d.runDetail, 'live_interrupts'), fromLive], (r) => idOf(r)).slice(0, LIVE_LIMIT);
}

/** Rows the active panel moves over. */
export function panelRows(s: DagState, panel: Panel = s.panel): Row[] {
  if (panel === 'graphs') return s.graphs;
  const d = s.detail;
  if (!d) return [];
  switch (panel) {
    case 'tasks': return taskTreeRows(d).map((r) => r.task);
    case 'runs': return d.runs.slice(0, RUN_LIMIT);
    case 'recovery': return deferredRows(d);
    case 'cancellations': return cancellationRows(d);
    case 'heartbeats': return heartbeatRows(d);
    case 'interrupts': return interruptRows(d);
    case 'live': return d.live.slice(0, LIVE_LIMIT);
    case 'artifacts': return d.artifacts.items;
  }
}

export function taskCounts(d: Detail): Record<'ready' | 'running' | 'blocked' | 'failed' | 'cancelled' | 'succeeded', number> {
  const c = { ready: 0, running: 0, blocked: 0, failed: 0, cancelled: 0, succeeded: 0 };
  for (const t of arr(d.graph, 'nodes')) {
    const s = word(t.status);
    if (s === 'Ready') c.ready += 1;
    else if (s === 'Running') c.running += 1;
    else if (s === 'Blocked' || s === 'BlockedOnChildren') c.blocked += 1;
    else if (s === 'Failed') c.failed += 1;
    else if (s === 'Cancelled') c.cancelled += 1;
    else if (s === 'Succeeded') c.succeeded += 1;
  }
  return c;
}

/** The selected inventory row differs from the hydrated graph (Enter loads it). */
export function cursorChanged(s: DagState): boolean {
  const g = s.graphs[s.cursor.graphs];
  return !!g && idOf(g) !== s.hydrated;
}

export const selectedGraphId = (s: DagState): string | null => s.hydrated ?? idOf(s.graphs[s.cursor.graphs]);

// ---------- readback shaping (the params themselves are built in src-tauri/src/graphs.rs) ----------

/** The run the Runs-dependent reads follow: the active run, else the newest (load_selected_graph). */
export function focusRunId(operational: Row | null, runs: readonly Row[]): string | null {
  return idOf(obj(operational, 'active_run')) ?? idOf(runs[0]);
}

/** Artifact summaries of another graph are dropped with a warning, as in the TUI. */
export function artifactPage(page: Row | null, graphId: string, enabled: boolean, error: string | null): ArtifactPage {
  if (!enabled) return { status: 'disabled', items: [], nextCursor: null, hasMore: false, warnings: [], message: 'artifact summary pagination capability disabled' };
  if (error !== null || !page) return { status: 'error', items: [], nextCursor: null, hasMore: false, warnings: [], message: `artifact summary readback failed: ${error ?? 'no page'}` };
  const all = arr(page, 'items');
  const items = all.filter((i) => str(i, 'graph_id') === graphId).slice(0, ARTIFACT_LIMIT);
  const warnings = arr(page, 'warnings').map((w) => `${str(w, 'code') ?? 'warning'}: ${str(w, 'message') ?? ''}`);
  const dropped = all.length - all.filter((i) => str(i, 'graph_id') === graphId).length;
  if (dropped > 0) warnings.push(`dropped ${dropped} artifact summary row(s) from a different graph`);
  const nextCursor = str(page, 'next_cursor');
  const hasMore = page.has_more === true;
  if (hasMore && !nextCursor) warnings.push('artifact summary page has more rows but no next cursor');
  return { status: 'ready', items, nextCursor, hasMore, warnings, message: null };
}

// ---------- controls (gated, never unbounded) ----------

/** `max_steps`: required, digits only, positive. */
export function parseMaxSteps(input: string): number | { error: string } {
  const t = input.trim();
  if (t === '') return { error: 'max_steps is required' };
  if (!/^\d+$/.test(t)) return { error: 'max_steps must be digits only' };
  const n = Number(t);
  if (!Number.isSafeInteger(n) || n <= 0 || n > 0xffffffff) return { error: 'max_steps must be a positive integer' };
  return n;
}

/** A cancellation reason: trimmed, non-empty, at most 240 characters. */
export function parseReason(input: string): string | { error: string } {
  const t = input.trim();
  if (t === '') return { error: 'a reason is required' };
  if ([...t].length > REASON_LIMIT) return { error: `the reason must be ${REASON_LIMIT} characters or fewer` };
  return t;
}

/** Recovery budget: `max_graphs` required and positive; `time_budget_ms` optional digits (0 allowed). */
export function parseBudget(maxGraphs: string, timeBudget: string): { maxGraphs: number; timeBudgetMs: number | null } | { error: string } {
  const m = maxGraphs.trim();
  if (!/^\d+$/.test(m) || Number(m) <= 0 || Number(m) > 0xffffffff) return { error: 'max_graphs must be a positive integer' };
  const t = timeBudget.trim();
  if (t !== '' && (!/^\d+$/.test(t) || !Number.isSafeInteger(Number(t)))) return { error: 'time_budget_ms must be digits only' };
  return { maxGraphs: Number(m), timeBudgetMs: t === '' ? null : Number(t) };
}

/** scheduler_run_status_accepts_cancellation. */
const RUN_CANCELLABLE = new Set(['Running', 'Cancelling']);

/** Why a control cannot open now, or null (cancellation_control_gate_message and friends). */
export function controlGate(s: DagState, which: 'fake' | 'live' | 'cancel-graph' | 'cancel-run' | 'recover'): string | null {
  if (s.status !== 'ready') return 'the browser is not loaded';
  const flag = { fake: 'recursive_dag_scheduler_control', live: 'recursive_dag_live_scheduler_control', 'cancel-graph': 'recursive_dag_cancellation_control', 'cancel-run': 'recursive_dag_cancellation_control', recover: 'recursive_dag_recovery_control' }[which];
  if (!cap(s.caps, flag)) return `${flag} capability disabled`;
  const d = s.detail;
  if (which === 'recover') {
    if (!cap(s.caps, 'recursive_dag_recovery_status')) return 'recovery status capability disabled';
    if (!d || !deferredWorkVisible(d)) return 'no deferred recovery work is visible';
    return null;
  }
  if (!d || !s.hydrated) return 'no graph is loaded (Enter loads the selected graph)';
  if (which === 'cancel-run') {
    const run = d.runs[s.cursor.runs] ?? null;
    if (!run) return 'no scheduler run is selected (Runs panel)';
    if (!RUN_CANCELLABLE.has(word(run.status))) return `scheduler run ${short(idOf(run))} is ${word(run.status)}, not active`;
    return null;
  }
  if (which === 'cancel-graph') {
    // selected_graph_cancellation_target: only an Active graph that is neither malformed nor quarantined.
    const g = obj(d.graph, 'graph');
    const status = word(g?.status);
    if (status !== 'Active') return `selected graph is not cancellable: ${status}`;
    if (str(g, 'malformed_reason') || str(g, 'quarantined_at')) return 'selected graph is malformed or quarantined';
  }
  return null;
}

export function deferredWorkVisible(d: Detail): boolean {
  return deferredRows(d).length > 0 || Number(d.recovery?.deferred_graph_count ?? 0) > 0;
}

// ---------- keys ----------

export interface DagKey { key: string; ctrl?: boolean; shift?: boolean }
export type DagEffect =
  | { kind: 'none' }
  | { kind: 'close' }
  | { kind: 'load'; graphId: string | null }
  | { kind: 'more-artifacts' }
  | { kind: 'submit'; control: Exclude<Control, { kind: 'none' } | { kind: 'cancel-prefix' }> }
  | { kind: 'scroll'; by: number }
  | { kind: 'toast'; text: string };

const out = (state: DagState, effect: DagEffect = { kind: 'none' }) => ({ state, effect, handled: true });

function move(s: DagState, to: number): DagState {
  const n = panelRows(s).length;
  return { ...s, cursor: { ...s.cursor, [s.panel]: n === 0 ? 0 : Math.max(0, Math.min(to, n - 1)) } };
}

function stepPanel(s: DagState, by: number): DagState {
  const i = PANELS.indexOf(s.panel);
  return { ...s, panel: PANELS[(i + by + PANELS.length) % PANELS.length]!, inspector: { ...s.inspector, open: false } };
}

function controlKey(s: DagState, k: DagKey): { state: DagState; effect: DagEffect; handled: boolean } | null {
  const c = s.control;
  if (c.kind === 'none') return null;
  if (c.kind === 'cancel-prefix') {
    if (k.key === 'Escape') return out({ ...s, control: { kind: 'none' } });
    if (k.key === 'g') {
      const why = controlGate(s, 'cancel-graph');
      return why ? out({ ...s, control: { kind: 'none' } }, { kind: 'toast', text: `Cancel graph unavailable: ${why}` }) : out({ ...s, control: { kind: 'reason', target: 'graph', input: '', error: null } });
    }
    if (k.key === 'r') {
      const why = controlGate(s, 'cancel-run');
      return why ? out({ ...s, control: { kind: 'none' } }, { kind: 'toast', text: `Cancel run unavailable: ${why}` }) : out({ ...s, control: { kind: 'reason', target: 'run', input: '', error: null } });
    }
    if (k.key === 't') return out({ ...s, control: { kind: 'none' } }, { kind: 'toast', text: 'Task cancellation is reserved: it needs daemon execution semantics' });
    return out(s);
  }
  // Text inputs are the DOM's own fields; Enter / Esc come here.
  if (k.key === 'Escape') return out({ ...s, control: { kind: 'none' } });
  if (k.key === 'Tab' && c.kind === 'budget') return out({ ...s, control: { ...c, field: c.field === 'max_graphs' ? 'time_budget_ms' : 'max_graphs' } });
  if (k.key === 'Enter') {
    let error: string | null = null;
    if (c.kind === 'steps') { const v = parseMaxSteps(c.input); if (typeof v !== 'number') error = v.error; }
    if (c.kind === 'reason') { const v = parseReason(c.input); if (typeof v !== 'string') error = v.error; }
    if (c.kind === 'budget') { const v = parseBudget(c.maxGraphs, c.timeBudget); if ('error' in v) error = v.error; }
    if (error) return out({ ...s, control: { ...c, error } });
    return out({ ...s, control: { kind: 'none' } }, { kind: 'submit', control: c });
  }
  return { state: s, effect: { kind: 'none' }, handled: false };
}

/** One key in the browser (handle_recursive_dag_key). `handled` false lets app keys through. */
export function dagKey(s: DagState, k: DagKey): { state: DagState; effect: DagEffect; handled: boolean } {
  const ctl = controlKey(s, k);
  if (ctl) return ctl;
  const key = k.key;
  if (k.ctrl) {
    if (key === 'd') return out(s, { kind: 'scroll', by: 10 });
    if (key === 'u') return out(s, { kind: 'scroll', by: -10 });
    return { state: s, effect: { kind: 'none' }, handled: false };
  }
  if (s.inspector.open) {
    const view: Record<string, InspectorView> = { m: 'metadata', l: 'links', p: 'preview', t: 'test', d: 'diff' };
    if (view[key]) return out({ ...s, inspector: { open: true, view: view[key]! } });
    if (key === 'Escape' || key === 'q') return out({ ...s, inspector: { ...s.inspector, open: false } });
  }
  switch (key) {
    case 'Escape': case 'q': return out(s, { kind: 'close' });
    case 'Tab': return out(stepPanel(s, k.shift ? -1 : 1));
    case 'l': case 'ArrowRight': return out(stepPanel(s, 1));
    case 'h': case 'ArrowLeft': return out(stepPanel(s, -1));
    case 'j': case 'ArrowDown': return out(move(s, s.cursor[s.panel] + 1));
    case 'k': case 'ArrowUp': return out(move(s, s.cursor[s.panel] - 1));
    case 'g': return out(move(s, 0));
    case 'G': return out(move(s, panelRows(s).length - 1));
    case 'r': return out(s, { kind: 'load', graphId: selectedGraphId(s) });
    case '!': return out({ ...s, gates: !s.gates });
    case 'Enter': {
      if (s.panel === 'graphs') return out(s, { kind: 'load', graphId: idOf(s.graphs[s.cursor.graphs]) });
      if (s.panel === 'artifacts') {
        const rows = panelRows(s);
        if (s.cursor.artifacts >= rows.length && s.detail?.artifacts.hasMore) return out(s, { kind: 'more-artifacts' });
        return rows.length > 0 ? out({ ...s, inspector: { open: true, view: 'metadata' } }) : out(s);
      }
      return out(s);
    }
    case 'R': case 'L': {
      const live = key === 'L';
      const why = controlGate(s, live ? 'live' : 'fake');
      if (why) return out(s, { kind: 'toast', text: `${live ? 'LIVE' : 'FAKE'} run unavailable: ${why}` });
      return out({ ...s, control: { kind: 'steps', live, input: '', error: null } });
    }
    case 'C': {
      if (!cap(s.caps, 'recursive_dag_cancellation_control')) return out(s, { kind: 'toast', text: 'Cancellation unavailable: recursive_dag_cancellation_control capability disabled' });
      return out({ ...s, control: { kind: 'cancel-prefix' } });
    }
    case 'c': {
      const why = controlGate(s, 'recover');
      if (why) return out(s, { kind: 'toast', text: `Recovery continuation unavailable: ${why}` });
      return out({ ...s, control: { kind: 'budget', maxGraphs: '', timeBudget: '', field: 'max_graphs', error: null } });
    }
    default: return { state: s, effect: { kind: 'none' }, handled: false };
  }
}

/** `]a` / `[a`: the next / previous artifact row (only inside `:dag`). */
export function artifactStep(s: DagState, by: 1 | -1): DagState {
  const n = s.detail?.artifacts.items.length ?? 0;
  if (n === 0) return { ...s, panel: 'artifacts' };
  const at = Math.max(0, Math.min(n - 1, s.cursor.artifacts + by));
  return { ...s, panel: 'artifacts', cursor: { ...s.cursor, artifacts: at } };
}

/** The gate details `!` reveals (no RPC). */
export function gateLines(s: DagState): string[] {
  const yn = (b: boolean) => (b ? 'yes' : 'no');
  return [
    `fake scheduler: ${controlGate(s, 'fake') ?? 'available'}`,
    `live scheduler: ${controlGate(s, 'live') ?? 'available'}`,
    `cancel graph: ${controlGate(s, 'cancel-graph') ?? 'available'}`,
    `cancel run: ${controlGate(s, 'cancel-run') ?? 'available'}`,
    `recovery continuation: ${controlGate(s, 'recover') ?? 'available'}`,
    `background loop: ${yn(cap(s.caps, 'recursive_dag_background_loop'))} (never started from :dag)`,
  ];
}

/** The capability line of the header. */
export function capsLine(c: Caps | null): string {
  if (!c) return 'caps unknown';
  const yn = (k: string) => (cap(c, k) ? 'y' : 'n');
  return `inspect=${yn('recursive_dag_inspection')} run=${yn('recursive_dag_run_inspection')} recovery=${yn('recursive_dag_recovery_status')} live=${yn('recursive_dag_live_status_inspection')} validation=${yn('recursive_dag_live_validation_inspection')} artifacts=${yn('recursive_dag_artifact_list_pagination')}/${yn('recursive_dag_artifact_lookup')}/${yn('recursive_dag_artifact_preview_inspection')} fakectl=${yn('recursive_dag_scheduler_control')} livectl=${yn('recursive_dag_live_scheduler_control')} cancelctl=${yn('recursive_dag_cancellation_control')} recoverctl=${yn('recursive_dag_recovery_control')} liveexec=${yn('recursive_dag_live_execution')} background=${yn('recursive_dag_background_loop')}`;
}

export function footerText(s: DagState): string {
  const c = s.control;
  if (c.kind === 'reason') return `CANCEL ${c.target.toUpperCase()} reason input  Enter submit  Esc cancel`;
  if (c.kind === 'budget') return 'RECOVER budget input  digits edit  Tab field  Enter submit  Esc cancel';
  if (c.kind === 'cancel-prefix') return 'CANCEL prefix  g graph  r run  t reserved task  Esc cancel';
  if (c.kind === 'steps') return `${c.live ? 'LIVE' : 'FAKE'} max_steps input  digits edit  Enter submit  Esc cancel`;
  if (s.inspector.open) return 'inspector  ]a/[a artifact row  m metadata  l links  p preview  t test  d diff  q/Esc close inspector';
  return 'inspect  Tab/h/l panels  j/k nav  Enter load/inspect  ]a/[a artifacts  r refresh  R FAKE  L LIVE  C cancel prefix  c recover  ! gates  q/Esc close';
}
