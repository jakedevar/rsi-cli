// Command Mode rows (hub #3000 gap sweep G3) for the action registry, pure (no
// DOM): `:manual`, `:rotation-abandon`, `:manager-reseat`, `:cold-restore`,
// `:cold-archive`, `:context` and the modalkit window and tab commands, which
// become desktop substitutes. `:blank <text>` is the core `launch` action that
// actions/commandmode-run.ts claims when it carries text. The rules are
// commandmode-model.ts; the DOM side is actions/commandmode-run.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type CommandModeActionId =
  | 'cm-manual' | 'cm-rotation-abandon' | 'cm-manager-reseat' | 'cm-cold-restore' | 'cm-cold-archive' | 'cm-context'
  | 'cm-split' | 'cm-vsplit' | 'cm-only' | 'cm-tabnew' | 'cm-tabclose';

/** What the Command Mode rules see (built by actions/commandmode-run.ts). */
export interface CommandModeSnapshot {
  /** The session a session command acts on: the open one, else the list cursor's. */
  target: { id: string; project_id?: string | null; status: string } | null;
}

export const COMMANDMODE_ACTIONS: readonly ActionSpec[] = [
  { id: 'cm-manual', title: 'Open manual', group: 'help', menuBreak: true, detail: 'The generated rsi manual (:manual; :manual pdf prints it)' },
  { id: 'cm-rotation-abandon', title: 'Abandon blocked rotation', group: 'session', noMenu: true, detail: ':rotation-abandon [provider[/model]]; run twice within 10 s to confirm' },
  { id: 'cm-manager-reseat', title: 'Reseat failed manager', group: 'session', noMenu: true, detail: ':manager-reseat <provider>[/<model>] <reason>; run twice within 10 s to confirm' },
  { id: 'cm-cold-restore', title: 'Restore from cold storage', group: 'session', noMenu: true, detail: 'Puts the session’s rows back from the cold-storage archive (:cold-restore)' },
  { id: 'cm-cold-archive', title: 'Cold-storage archive', group: 'view', noMenu: true, detail: ':cold-archive [status|run|move|verify|compact]' },
  { id: 'cm-context', title: 'Set active task context', group: 'session', noMenu: true, detail: ':context <text> sets it (500 bytes at most); :context alone clears it' },
  { id: 'cm-split', title: 'Split pane horizontally', group: 'go', noMenu: true, detail: 'Substitute: moves the keys between the list and the session (:split)' },
  { id: 'cm-vsplit', title: 'Split pane vertically', group: 'go', noMenu: true, detail: 'Substitute: moves the keys between the list and the session (:vsplit)' },
  { id: 'cm-only', title: 'Close other panes', group: 'go', noMenu: true, detail: 'Substitute: closes the open overlay pane, as :close (:only)' },
  { id: 'cm-tabnew', title: 'Create tab', group: 'go', noMenu: true, detail: 'Substitute: the project picker (a desktop tab is the project filter) (:tabnew)' },
  { id: 'cm-tabclose', title: 'Close tab', group: 'go', noMenu: true, detail: 'Substitute: back to all projects (:tabclose)' },
];

type Rule = (s: UiSnapshot) => string | null;

const NO_SESSION = 'No session selected';
const target: Rule = (s) => (s.commandMode?.target ? null : NO_SESSION);
const free: Rule = () => null;

export const COMMANDMODE_RULES: Readonly<Record<CommandModeActionId, Rule>> = {
  'cm-manual': free,
  'cm-rotation-abandon': target,
  'cm-manager-reseat': target,
  'cm-cold-restore': target,
  'cm-cold-archive': free,
  'cm-context': target,
  'cm-split': free,
  'cm-vsplit': free,
  'cm-only': free,
  'cm-tabnew': free,
  'cm-tabclose': free,
};

const IDS: ReadonlySet<string> = new Set(COMMANDMODE_ACTIONS.map((a) => a.id));
export const isCommandModeAction = (id: string): id is CommandModeActionId => IDS.has(id);

const b = (action: CommandModeActionId): DesktopBinding<CommandModeActionId> => ({ action });

/** TUI `:` commands (canonical names; `:man`, `:ctx`, `:sp`, `:vs`, `:on`, `:tabe`, `:tabc` resolve through the keymap). */
export const COMMANDMODE_COMMANDS: Readonly<Record<string, DesktopBinding<CommandModeActionId>>> = {
  manual: b('cm-manual'),
  'rotation-abandon': b('cm-rotation-abandon'),
  'manager-reseat': b('cm-manager-reseat'),
  'cold-restore': b('cm-cold-restore'),
  'cold-archive': b('cm-cold-archive'),
  context: b('cm-context'),
  split: b('cm-split'),
  vsplit: b('cm-vsplit'),
  only: b('cm-only'),
  tabnew: b('cm-tabnew'),
  tabclose: b('cm-tabclose'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** Command Mode targets (actions/commandmode-run.ts). */
    commandMode?: CommandModeSnapshot;
  }
}

export const SLICE: SliceSpec<CommandModeActionId> = {
  name: 'command mode (gap sweep G3, hub #3000)',
  order: 38,
  actions: COMMANDMODE_ACTIONS,
  rules: COMMANDMODE_RULES,
  commands: COMMANDMODE_COMMANDS,
};
