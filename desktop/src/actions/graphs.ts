// The graphs and topology slice's entries for the action registry (pure, no
// DOM; slice 11, hub #3013): graph review (`<Space>v`, `:graph`), the
// recursive DAG browser (`:dag`), the ESP Square (`<Space>gc`) and the
// topology actions (`gR`, `:topology run`, `:topology-resolve`). The DOM side
// is graphs.ts, dag.ts and esp.ts behind actions/graphs-run.ts. Keys inside a
// pane are its own (graph-model.ts graphKey, dag-model.ts dagKey,
// esp-model.ts espKey); the entries below put them in the palette (their
// keys in the detail text: as extraKeys they would shadow the app's global keys).
import type { DesktopBinding } from '../keyengine.js';
import type { ActionId, ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type GraphsActionId =
  | 'graph-open' | 'graph-close' | 'graph-pick-topology' | 'graph-pick-saved' | 'graph-pick-recursive'
  | 'graph-run' | 'graph-interrupt' | 'graph-undo' | 'graph-follow'
  | 'dag-open' | 'dag-close' | 'dag-refresh' | 'dag-gates'
  | 'esp-open'
  | 'topology-run-epic' | 'topology-run-issue' | 'topology-resolve';

/** What the slice's actions can see (built by actions/graphs-run.ts). */
export interface GraphsSnapshot {
  /** Which of the slice's panes is on screen. */
  pane: 'graph' | 'dag' | 'esp' | null;
  /** Graph review: the draft is the operator's own (not a bridged recursive view). */
  authored: boolean;
  /** Graph review: the draft's latest run is active. */
  executing: boolean;
  /** Graph review: the draft has nodes. */
  hasNodes: boolean;
  /** Graph review: an edit can be undone. */
  canUndo: boolean;
  /** The session under the list cursor (else the open one) is an Epic with a topology binding. */
  epicReady: string | null;
}

type Rule = (s: UiSnapshot) => string | null;

const NEED_GRAPH = 'Open graph review first (<Space>v or :graph)';
const inGraph = (more?: (g: GraphsSnapshot) => string | null): Rule => (s) => {
  const g = s.graphs;
  if (g?.pane !== 'graph') return NEED_GRAPH;
  return more ? more(g) : null;
};
const authoredIdle = (g: GraphsSnapshot) => (g.executing ? 'A run is active (x interrupts it)' : g.authored ? null : 'This graph is a read-only recursive view');
const inDag: Rule = (s) => (s.graphs?.pane === 'dag' ? null : 'Open the recursive DAG browser first (:dag)');

export const GRAPHS_RULES: Readonly<Record<GraphsActionId, Rule>> = {
  'graph-open': () => null,
  'graph-close': inGraph(),
  'graph-pick-topology': inGraph(authoredIdle),
  'graph-pick-saved': inGraph(authoredIdle),
  'graph-pick-recursive': inGraph(),
  'graph-run': inGraph((g) => (!g.authored ? 'A bridged recursive graph is read-only; it cannot run from here' : g.executing ? 'A run is already active (x interrupts it)' : g.hasNodes ? null : 'The graph is empty (t picks a starter topology)')),
  'graph-interrupt': inGraph((g) => (g.executing ? null : 'No active workflow execution')),
  'graph-undo': inGraph((g) => authoredIdle(g) ?? (g.canUndo ? null : 'Nothing to undo')),
  'graph-follow': inGraph(),
  'dag-open': () => null,
  'dag-close': inDag,
  'dag-refresh': inDag,
  'dag-gates': inDag,
  'esp-open': () => null,
  // gR's own toasts (no selection, not an Epic, no binding) come from the run, as in the TUI.
  'topology-run-epic': () => null,
  'topology-run-issue': (s) => (s.issues?.active && s.issues.issue ? null : 'Focus an Issue in the Issues workspace first (<Space>i)'),
  'topology-resolve': () => null,
};

/** Menu order. */
export const GRAPHS_ACTIONS: readonly ActionSpec[] = [
  { id: 'graph-open', title: 'Graph review', group: 'view', menuBreak: true, detail: 'The visual workflow graph: starter topologies, saved workflows and recursive graphs' },
  { id: 'dag-open', title: 'Recursive DAG browser', group: 'view', detail: 'Graphs, tasks, scheduler runs, recovery, cancellations, heartbeats, interrupts, live attempts and artifacts' },
  { id: 'esp-open', title: 'ESP Square', group: 'view', detail: 'The 3×3 guessing game (u i o / j k l / m , .)' },
  { id: 'graph-pick-topology', title: 'Graph: starter topology…', group: 'view', noMenu: true, detail: 't in graph review' },
  { id: 'graph-pick-saved', title: 'Graph: open saved workflow…', group: 'view', noMenu: true, detail: 'o in graph review' },
  { id: 'graph-pick-recursive', title: 'Graph: open recursive graph…', group: 'view', noMenu: true, detail: 'R in graph review; read-only, needs the gv_render_recursive_origin capability' },
  { id: 'graph-run', title: 'Graph: run workflow', group: 'view', noMenu: true, detail: 'r in graph review: saves the draft, checks it and starts it (spawns sessions)' },
  { id: 'graph-interrupt', title: 'Graph: interrupt run', group: 'view', noMenu: true, detail: 'x in graph review' },
  { id: 'graph-undo', title: 'Graph: undo edit', group: 'view', noMenu: true, detail: 'u in graph review' },
  { id: 'graph-follow', title: 'Graph: camera follows selection', group: 'view', noMenu: true, detail: 'c in graph review' },
  { id: 'graph-close', title: 'Close graph review', group: 'view', noMenu: true, detail: 'q in graph review' },
  { id: 'dag-refresh', title: 'DAG: reload graph', group: 'view', noMenu: true, detail: 'r in the DAG browser' },
  { id: 'dag-gates', title: 'DAG: show control gates', group: 'view', noMenu: true, detail: '! in the DAG browser' },
  { id: 'dag-close', title: 'Close recursive DAG browser', group: 'view', noMenu: true, detail: 'q or Esc in the DAG browser' },
  { id: 'topology-run-epic', title: 'Run Epic topology', group: 'session', menuBreak: true, contexts: ['session-row'], detail: 'ExecuteTopology on the Epic’s bound topology; its executors spawn under the Epic' },
  { id: 'topology-run-issue', title: 'Run a topology on the Issue…', group: 'issues', menuBreak: true, contexts: ['issue-row'], detail: ':topology run <name> [<epic>]' },
  { id: 'topology-resolve', title: 'Resolve preserved topology work…', group: 'session', detail: ':topology-resolve [<execution_id>] inspect|accept|retry|discard [<commit>]' },
];

type Bindings = Readonly<Record<string, DesktopBinding<ActionId>>>;
const b = (action: ActionId): DesktopBinding<ActionId> => ({ action });

/** TUI Normal-mode keys the slice runs (keybindings.md notation). */
export const GRAPHS_KEY_BINDINGS: Bindings = {
  '<Space>v': b('graph-open'),
  '<Space>gc': b('esp-open'),
  gR: b('topology-run-epic'),
};

/** TUI `:` commands (canonical names) the slice runs. */
export const GRAPHS_COMMAND_BINDINGS: Bindings = {
  graph: b('graph-open'),
  dag: b('dag-open'),
  topology: b('topology-run-issue'),
  'topology-resolve': b('topology-resolve'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** Graph review, the DAG browser and the ESP Square (slice 11, actions/graphs-run.ts). */
    graphs?: GraphsSnapshot;
  }
}

export const SLICE: SliceSpec<GraphsActionId> = {
  name: 'graphs (slice 11, hub #3013)',
  order: 110,
  actions: GRAPHS_ACTIONS,
  rules: GRAPHS_RULES,
  keys: GRAPHS_KEY_BINDINGS,
  commands: GRAPHS_COMMAND_BINDINGS,
};
