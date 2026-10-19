// The manager surfaces' slice of the action registry (hub #3011, slice 9):
// action specs and availability rules, pure (no DOM). The DOM side is
// managers.ts (RUNTIME in actions/managers-run.ts). The entry points are the
// TUI's keys and `:` commands; keys inside an open view are the TUI overlay
// keys (managers-model.ts VIEW_KEYS), handled by the view.
import type { DesktopBinding } from '../keyengine.js';
import type { ManagerView } from '../managers-model.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type ManagersActionId =
  | 'mgr-tree' | 'mgr-board' | 'mgr-decisions' | 'mgr-inbox' | 'mgr-inspect' | 'mgr-policy'
  | 'mgr-scope' | 'mgr-appoint' | 'mgr-clear'
  | 'mgr-global' | 'mgr-global-grant' | 'mgr-global-revoke' | 'mgr-budget' | 'mgr-fleet'
  | 'mgr-portfolio' | 'mgr-escalations' | 'mgr-friction'
  | 'mgr-global-appoint' | 'mgr-node' | 'mgr-restart'
  | 'mgr-refresh' | 'mgr-close';

/** What the manager actions can see (built by managers.ts). */
export interface ManagersSnapshot {
  /** The open manager view, or null. */
  view: ManagerView | null;
  hasProject: boolean;
  /** A leaf, unarchived session is selected in the session list (the seat to appoint). */
  seatCandidate: boolean;
}

export const MANAGERS_ACTIONS: readonly ActionSpec[] = [
  { id: 'mgr-tree', title: 'Manager tree', group: 'view', menuBreak: true, detail: 'The global grant, portfolio nodes, project seats, area nodes and led Epics (:manager tree)' },
  { id: 'mgr-board', title: 'Manager board', group: 'view', detail: 'The project manager’s board: decisions, requests, leads and health (<Space>gb)' },
  { id: 'mgr-decisions', title: 'Manager decisions', group: 'view', detail: 'Decisions waiting on the operator (<Space>gd)' },
  { id: 'mgr-inbox', title: 'Manager inbox', group: 'view', noMenu: true, detail: 'Lead requests and replies (:manager inbox)' },
  { id: 'mgr-inspect', title: 'Manager inspect', group: 'view', noMenu: true, detail: 'Workers, work, topology, resources, actions, events (:manager inspect)' },
  { id: 'mgr-policy', title: 'Manager policy…', group: 'view', detail: 'Operator-owned manager mode, capabilities and limits (<Space>gp)' },
  { id: 'mgr-scope', title: 'Manager scope…', group: 'view', detail: 'The Groups and Epics the project manager covers (:manager scope)' },
  { id: 'mgr-appoint', title: 'Appoint selected session as manager…', group: 'view', noMenu: true, detail: 'The selected session becomes the project manager (:manager appoint)' },
  { id: 'mgr-clear', title: 'Clear manager scope…', group: 'view', noMenu: true, detail: 'Revokes supervision; asks first (:manager clear)' },
  { id: 'mgr-global', title: 'Global manager workspace', group: 'view', detail: 'The grant, the global seat and each project’s PM health (gm)' },
  { id: 'mgr-global-grant', title: 'Show global manager grant', group: 'view', noMenu: true, detail: ':manager global' },
  { id: 'mgr-global-appoint', title: 'Appoint selected session as global manager…', group: 'view', noMenu: true, detail: ':manager global appoint [--defaults] [projects]: the focused session over the named projects (default: every project)' },
  { id: 'mgr-global-revoke', title: 'Revoke global manager…', group: 'view', noMenu: true, detail: 'The seat keeps its session but loses its authority; asks first' },
  { id: 'mgr-budget', title: 'Manager budget panel', group: 'view', detail: 'Every cap of the global node and each PM as used / limit (gm, then b)' },
  { id: 'mgr-fleet', title: 'Fleet workspace', group: 'view', detail: 'Active agents and usage rates across all projects (:fleet)' },
  { id: 'mgr-portfolio', title: 'Manager portfolio nodes', group: 'view', noMenu: true, detail: 'Managers above project level (:manager portfolio)' },
  { id: 'mgr-escalations', title: 'Manager escalations', group: 'view', noMenu: true, detail: 'Escalations at the top of their chain (:manager escalations)' },
  { id: 'mgr-node', title: 'Manager area nodes', group: 'view', noMenu: true, detail: ':manager node [list|get <UUID>|configure <JSON>|revoke <UUID> <epoch>]' },
  { id: 'mgr-restart', title: 'Operator quiet-point restart', group: 'view', noMenu: true, detail: ':manager restart [status|now|cancel]' },
  { id: 'mgr-friction', title: 'Manager friction rollup', group: 'view', noMenu: true, detail: 'Refusals and timeouts by signature (:manager friction [hours])' },
  { id: 'mgr-refresh', title: 'Refresh manager view', group: 'view', noMenu: true, detail: 'r inside the view' },
  { id: 'mgr-close', title: 'Close manager view', group: 'view', noMenu: true, detail: 'Esc or q inside the view' },
];

type Rule = (s: UiSnapshot) => string | null;

const project: Rule = (s) => (s.managers?.hasProject ? null : 'No project yet: a project manager is per project');
const open: Rule = (s) => (s.managers?.view ? null : 'No manager view is open');

export const MANAGERS_RULES: Readonly<Record<ManagersActionId, Rule>> = {
  'mgr-tree': () => null,
  'mgr-board': project,
  'mgr-decisions': project,
  'mgr-inbox': project,
  'mgr-inspect': project,
  'mgr-policy': project,
  'mgr-scope': project,
  'mgr-appoint': (s) => project(s) ?? (s.managers?.seatCandidate ? null : 'Select the session to appoint in the session list first'),
  'mgr-clear': project,
  'mgr-global': () => null,
  'mgr-global-grant': () => null,
  'mgr-global-revoke': () => null,
  'mgr-global-appoint': (s) => (s.managers?.seatCandidate ? null : 'Select the session to appoint in the session list first'),
  'mgr-node': () => null,
  'mgr-restart': () => null,
  'mgr-budget': () => null,
  'mgr-fleet': () => null,
  'mgr-portfolio': () => null,
  'mgr-escalations': () => null,
  'mgr-friction': () => null,
  'mgr-refresh': open,
  'mgr-close': open,
};

const IDS: ReadonlySet<string> = new Set(MANAGERS_ACTIONS.map((a) => a.id));
export const isManagersAction = (id: string): id is ManagersActionId => IDS.has(id);

const b = (action: ManagersActionId): DesktopBinding<ManagersActionId> => ({ action });

/** TUI Normal-mode keys (keybindings.md notation). */
export const MANAGERS_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<ManagersActionId>>> = {
  '<Space>gp': b('mgr-policy'),
  '<Space>gb': b('mgr-board'),
  '<Space>gd': b('mgr-decisions'),
  gm: b('mgr-global'),
};

/** TUI `:` commands (canonical names; aliases resolve through the keymap). */
export const MANAGERS_COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<ManagersActionId>>> = {
  manager: b('mgr-board'),
  'manager board': b('mgr-board'),
  'manager decisions': b('mgr-decisions'),
  'manager inbox': b('mgr-inbox'),
  'manager inspect': b('mgr-inspect'),
  'manager policy': b('mgr-policy'),
  'manager scope': b('mgr-scope'),
  'manager appoint': b('mgr-appoint'),
  'manager clear': b('mgr-clear'),
  'manager tree': b('mgr-tree'),
  'global-manager': b('mgr-global'),
  'manager global': b('mgr-global-grant'),
  'manager global revoke': b('mgr-global-revoke'),
  'manager global appoint': b('mgr-global-appoint'),
  'manager node': b('mgr-node'),
  'manager restart': b('mgr-restart'),
  fleet: b('mgr-fleet'),
  'manager portfolio': b('mgr-portfolio'),
  'manager escalations': b('mgr-escalations'),
  'manager friction': b('mgr-friction'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The manager surfaces (slice 9, managers.ts). */
    managers?: ManagersSnapshot;
  }
}

export const SLICE: SliceSpec<ManagersActionId> = {
  name: 'managers (slice 9, hub #3011)',
  order: 55,
  actions: MANAGERS_ACTIONS,
  rules: MANAGERS_RULES,
  keys: MANAGERS_KEY_BINDINGS,
  commands: MANAGERS_COMMAND_BINDINGS,
};
