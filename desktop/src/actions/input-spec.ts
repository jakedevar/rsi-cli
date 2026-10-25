// Gap sweep G1 (hub #3000): the input bar, vim text editing, global keys,
// recent files and operator messages, registry side (pure, no DOM). The
// registry collects SLICE through the generated actions/index.generated.ts;
// the DOM side is actions/input.ts, the rules input-model.ts and vim-editor.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type InputActionId =
  | 'open-recent-file' | 'jump-back' | 'jump-forward' | 'pane-left' | 'pane-right'
  | 'sidebar-wider' | 'sidebar-narrower' | 'column-left' | 'column-right'
  | 'pending-messages' | 'drain-restart' | 'toggle-submit-on-enter'
  | 'input-modal' | 'compile-prompt' | 'fix-grammar';

/** What the input actions can see. */
export interface InputSnapshot {
  /** Distinct files the open session's tool events touched (gf1..gf9). */
  recentFiles: number;
  /** The message box has text. */
  draft: boolean;
  submitOnEnter: boolean;
  /** The jumplist has an older / newer session. */
  back: boolean;
  forward: boolean;
}

type Rule = (s: UiSnapshot) => string | null;

const NEED_OPEN = 'Open a session first: click one in the list (j/k, then Enter)';
const needOpen: Rule = (s) => (s.open ? null : NEED_OPEN);
const needDraft: Rule = (s) => {
  if (!s.open) return NEED_OPEN;
  if (s.open.readOnly) return 'This session is read-only';
  return s.input?.draft ? null : 'The message box is empty';
};

export const INPUT_ACTIONS: readonly ActionSpec[] = [
  // Session
  { id: 'input-modal', title: 'Edit message in a large editor', group: 'session', menuBreak: true, extraKeys: ['Ctrl-G'], detail: 'The TUI’s Ctrl+G input modal; closing it keeps the text in the message box' },
  { id: 'compile-prompt', title: 'Compile prompt', group: 'session', extraKeys: ['Ctrl-Y'], detail: 'Send the draft to the prompt compiler and preview the result' },
  { id: 'fix-grammar', title: 'Fix grammar', group: 'session', extraKeys: ['Ctrl-Shift-G'], detail: 'Spelling and grammar only, previewed before it replaces the draft' },
  { id: 'pending-messages', title: 'Pending messages…', group: 'session', detail: 'Queued follow-ups not yet delivered; edit or withdraw them here (:pending)' },
  { id: 'drain-restart', title: 'Restart daemon after turns settle', group: 'session', detail: 'Waits for the running turns to finish, then restarts the daemon (:drain; :drain status shows each session)' },
  // Go
  { id: 'open-recent-file', title: 'Open recent file…', group: 'go', menuBreak: true, detail: 'gf1..gf9 open the Nth file the session’s tools touched' },
  { id: 'jump-back', title: 'Back in session history', group: 'go', menuBreak: true, extraKeys: ['Ctrl-O'] },
  { id: 'jump-forward', title: 'Forward in session history', group: 'go', extraKeys: ['Ctrl-I'] },
  { id: 'pane-left', title: 'Focus pane to the left', group: 'go', extraKeys: ['Ctrl-H', 'Ctrl-Left'], noMenu: true, detail: 'In the session list Ctrl-H/L switch zones' },
  { id: 'pane-right', title: 'Focus pane to the right', group: 'go', extraKeys: ['Ctrl-L', 'Ctrl-Right'], noMenu: true },
  // View
  { id: 'sidebar-wider', title: 'Widen session list', group: 'view', menuBreak: true, extraKeys: ['Ctrl-Shift-Right'] },
  { id: 'sidebar-narrower', title: 'Narrow session list', group: 'view', extraKeys: ['Ctrl-Shift-Left'] },
  { id: 'column-left', title: 'Move transcript column left', group: 'view', extraKeys: ['Ctrl-Left'], noMenu: true, detail: 'Snaps to Left Aligned at the edge (transcript focused)' },
  { id: 'column-right', title: 'Move transcript column right', group: 'view', extraKeys: ['Ctrl-Right'], noMenu: true },
  { id: 'toggle-submit-on-enter', title: 'Submit on Enter', group: 'view', checked: (s) => s.input?.submitOnEnter ?? true, detail: 'Enter sends the message box and Shift+Enter adds a line; off, Ctrl+Enter sends' },
];

export const INPUT_RULES: Readonly<Record<InputActionId | 'focus-composer', Rule>> = {
  // Taken over from the core: from the list, i/a/o/O open the session under the cursor first.
  'focus-composer': (s) => {
    if (!s.open) return s.cursorOnRow ? null : NEED_OPEN;
    return s.open.readOnly ? 'This session is read-only' : null;
  },
  'open-recent-file': (s) => {
    if (!s.open) return NEED_OPEN;
    return (s.input?.recentFiles ?? 0) > 0 ? null : 'No recent files: this session’s tools have not read or edited a file';
  },
  'jump-back': (s) => (s.input?.back ? null : 'No older session in the jumplist'),
  'jump-forward': (s) => (s.input?.forward ? null : 'No newer session in the jumplist'),
  'pane-left': () => null,
  'pane-right': () => null,
  'sidebar-wider': () => null,
  'sidebar-narrower': () => null,
  'column-left': needOpen,
  'column-right': needOpen,
  'pending-messages': needOpen,
  'drain-restart': () => null,
  'toggle-submit-on-enter': () => null,
  'input-modal': (s) => {
    if (!s.open) return NEED_OPEN;
    return s.open.readOnly ? 'This session is read-only' : null;
  },
  'compile-prompt': needDraft,
  'fix-grammar': needDraft,
};

const b = (action: InputActionId, arg?: string): DesktopBinding<InputActionId> => (arg ? { action, arg } : { action });

/** gf1..gf9 (keybindings.md "Recent Files"). */
export const INPUT_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<InputActionId>>> = Object.fromEntries(
  Array.from({ length: 9 }, (_, i) => [`gf${i + 1}`, b('open-recent-file', String(i + 1))]),
);

/** `:pending …` and `:drain …`, which the TUI parses ahead of its command table. */
export const INPUT_PREFIX_COMMANDS: Readonly<Record<string, DesktopBinding<InputActionId>>> = {
  pending: b('pending-messages'),
  drain: b('drain-restart'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The message box, recent files and the jumplist (gap sweep G1, actions/input.ts). */
    input?: InputSnapshot;
  }
}

export const SLICE: SliceSpec<InputActionId> = {
  name: 'input (gap sweep G1, hub #3000)',
  order: 95,
  actions: INPUT_ACTIONS,
  rules: INPUT_RULES,
  // routeGlobal (input-model.ts), run from the document keydown listener in actions/input.ts.
  rawKeys: [
    'C-o', 'C-i', 'C-h', 'C-l', 'C-0', 'Left', 'Right', 'S-Left', 'S-Right', 'C-Tab', 'C-S-Tab',
    'C-Left', 'C-Right', 'C-Up', 'C-Down', 'C-S-Left', 'C-S-Right', 'C-S-Up', 'C-S-Down',
  ],
  keys: INPUT_KEY_BINDINGS,
  prefixCommands: INPUT_PREFIX_COMMANDS,
};
