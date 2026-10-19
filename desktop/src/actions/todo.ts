// The operator to-do panel's slice of the action registry (hub #3015 slice
// 13B, TUI #1747/#2950): the `<Space>d` chords and `:todo`. Pure (no DOM); the
// DOM side is src/todo.ts behind actions/todo-run.ts, the rules
// src/todo-model.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';
import { NO_ITEM, NOTHING_TO_UNDO } from '../todo-model.js';

export type TodoActionId =
  | 'todo-toggle' | 'todo-next' | 'todo-prev' | 'todo-open-link' | 'todo-done' | 'todo-dismiss'
  | 'todo-copy' | 'todo-view' | 'todo-undo' | 'todo-ack';

/** What the to-do actions can see (built by todo.ts). */
export interface TodoSnapshot {
  /** The panel is shown. */
  open: boolean;
  /** An open item is under the panel cursor. */
  selected: boolean;
  /** The operator's last close can still be undone. */
  undo: boolean;
}

export const TODO_ACTIONS: readonly ActionSpec[] = [
  {
    id: 'todo-toggle', title: 'Operator to-do panel', group: 'view', menuBreak: true, checked: (s) => !!s.todo?.open,
    detail: 'Show or hide the right-hand to-do panel; the choice is remembered (<Space>dt, :todo)',
  },
  { id: 'todo-view', title: 'Open to-do item…', group: 'view', detail: 'The selected item in full with its actions (<Space>dv)' },
  { id: 'todo-undo', title: 'Undo to-do close', group: 'view', detail: 'Reopen the item you just closed, within 60 s (<Space>du)' },
  { id: 'todo-next', title: 'To-do: next item', group: 'view', noMenu: true, detail: 'Opens the panel first when it is hidden' },
  { id: 'todo-prev', title: 'To-do: previous item', group: 'view', noMenu: true },
  { id: 'todo-open-link', title: 'To-do: open link', group: 'view', noMenu: true, detail: 'The linked Issue or session (a URL is copied)' },
  { id: 'todo-done', title: 'To-do: mark done…', group: 'view', noMenu: true, detail: 'Asks y/n in the pop-up first' },
  { id: 'todo-dismiss', title: 'To-do: dismiss…', group: 'view', noMenu: true, detail: 'Asks y/n in the pop-up first' },
  { id: 'todo-copy', title: 'To-do: copy command', group: 'view', noMenu: true, detail: 'Copies the item’s command text verbatim' },
  { id: 'todo-ack', title: 'To-do: caught up', group: 'view', noMenu: true, detail: 'Clear the new and closed-since-last-visit highlights' },
];

const always = (): null => null;
const item = (s: UiSnapshot): string | null => (s.todo?.selected ? null : NO_ITEM);

export const TODO_RULES: Readonly<Record<TodoActionId, (s: UiSnapshot) => string | null>> = {
  'todo-toggle': always,
  'todo-next': always,
  'todo-prev': always,
  'todo-open-link': item,
  'todo-done': item,
  'todo-dismiss': item,
  'todo-copy': item,
  'todo-view': item,
  'todo-undo': (s) => (s.todo?.undo ? null : NOTHING_TO_UNDO),
  'todo-ack': always,
};

const IDS: ReadonlySet<string> = new Set(TODO_ACTIONS.map((a) => a.id));
export const isTodoAction = (id: string): id is TodoActionId => IDS.has(id);

/** TUI Normal-mode keys (keybindings.md notation). `<Space>dy` and `<Space>dc` both copy. */
export const TODO_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<TodoActionId>>> = {
  '<Space>dt': { action: 'todo-toggle' },
  '<Space>dj': { action: 'todo-next' },
  '<Space>dk': { action: 'todo-prev' },
  '<Space>do': { action: 'todo-open-link' },
  '<Space>dd': { action: 'todo-done' },
  '<Space>dx': { action: 'todo-dismiss' },
  '<Space>dy': { action: 'todo-copy' },
  '<Space>dc': { action: 'todo-copy' },
  '<Space>dv': { action: 'todo-view' },
  '<Space>du': { action: 'todo-undo' },
  '<Space>da': { action: 'todo-ack' },
};

export const TODO_COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<TodoActionId>>> = {
  todo: { action: 'todo-toggle' },
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The operator to-do panel (slice 13B, todo.ts). */
    todo?: TodoSnapshot;
  }
}

export const SLICE: SliceSpec<TodoActionId> = {
  name: 'operator to-do (slice 13B, hub #3015)',
  order: 72,
  actions: TODO_ACTIONS,
  rules: TODO_RULES,
  keys: TODO_KEY_BINDINGS,
  commands: TODO_COMMAND_BINDINGS,
};
