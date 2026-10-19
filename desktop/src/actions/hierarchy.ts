// Session hierarchy creation and Epic lead (hub #3000 gap sweep G2) for the
// action registry, pure (no DOM): the five creation chords open the one
// create-entity form with the kind chosen, and gL sets the Epic lead. The
// rules are hierarchy-model.ts; the DOM side is actions/hierarchy-run.ts.
import type { DesktopBinding } from '../keyengine.js';
import { type EntityKind, type HierSession, createBlock, leadBlock } from '../hierarchy-model.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type HierarchyActionId =
  | 'create-group' | 'create-epic' | 'create-story' | 'create-task' | 'create-bug' | 'set-epic-lead';

/** The kind each creation action pre-selects. */
export const CREATE_KIND: Readonly<Record<Exclude<HierarchyActionId, 'set-epic-lead'>, EntityKind>> = {
  'create-group': 'Group',
  'create-epic': 'Epic',
  'create-story': 'Story',
  'create-task': 'Task',
  'create-bug': 'Bug',
};

/** What the hierarchy rules see (built by actions/hierarchy-run.ts). */
export interface HierSnapshot {
  /** The list cursor's session, else the open one. */
  row: HierSession | null;
  /** The container a creation chord targets (null: the top level). */
  scope: HierSession | null;
  /** The row's parent, for gL. */
  parent: HierSession | null;
}

export const HIERARCHY_ACTIONS: readonly ActionSpec[] = [
  { id: 'create-group', title: 'Create Group…', group: 'session', menuBreak: true, detail: 'A top-level Group container' },
  { id: 'create-epic', title: 'Create Epic…', group: 'session', detail: 'An Epic under the Group the cursor is on' },
  { id: 'create-story', title: 'Create Story…', group: 'session', detail: 'A Story leaf under the Epic the cursor is in' },
  { id: 'create-task', title: 'Create Task…', group: 'session', detail: 'A Task leaf under the Epic the cursor is in' },
  { id: 'create-bug', title: 'Create Bug…', group: 'session', detail: 'A Bug leaf under the Epic the cursor is in' },
  { id: 'set-epic-lead', title: 'Set as Epic lead', group: 'session', contexts: ['session-row'], detail: 'Make the session the lead of its parent Epic' },
];

type Rule = (s: UiSnapshot) => string | null;

const create = (kind: EntityKind): Rule => (s) => createBlock(kind, s.hier?.scope ?? null);

export const HIERARCHY_RULES: Readonly<Record<HierarchyActionId, Rule>> = {
  'create-group': create('Group'),
  'create-epic': create('Epic'),
  'create-story': create('Story'),
  'create-task': create('Task'),
  'create-bug': create('Bug'),
  'set-epic-lead': (s) => leadBlock(s.hier?.row ?? null, s.hier?.parent ?? undefined),
};

const b = (action: HierarchyActionId): DesktopBinding<HierarchyActionId> => ({ action });

/** TUI Normal-mode keys (keybindings.md notation). */
export const HIERARCHY_KEYS: Readonly<Record<string, DesktopBinding<HierarchyActionId>>> = {
  '<Space>G': b('create-group'),
  '<Space>E': b('create-epic'),
  '<Space>gS': b('create-story'),
  '<Space>gT': b('create-task'),
  '<Space>B': b('create-bug'),
  gL: b('set-epic-lead'),
};

/** TUI `:` commands (canonical names). */
export const HIERARCHY_COMMANDS: Readonly<Record<string, DesktopBinding<HierarchyActionId>>> = {
  lead: b('set-epic-lead'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** Hierarchy targets (actions/hierarchy-run.ts). */
    hier?: HierSnapshot;
  }
}

export const SLICE: SliceSpec<HierarchyActionId> = {
  name: 'hierarchy creation and lead (G2, hub #3000)',
  order: 34,
  actions: HIERARCHY_ACTIONS,
  rules: HIERARCHY_RULES,
  keys: HIERARCHY_KEYS,
  commands: HIERARCHY_COMMANDS,
};
