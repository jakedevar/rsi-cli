// Pane, zone and window navigation (hub #3000 gap sweep G2) for the action
// registry, pure (no DOM): the Jobs zone (gj), Backspace, close pane
// (<Space>q), the project "tabs" (> <), session normal mode (e), the default
// launch model (Ctrl-M), lazygit and btop in the embedded terminal, and
// <Space>T. gf1..gf9 and i / a / o / O are gap sweep G1's (actions/input.ts).
// The rules are nav-model.ts; the DOM side is actions/nav-run.ts, which also
// claims the core gg / G and zone keys where the TUI gives them a
// pane-specific meaning.
import type { DesktopBinding } from '../keyengine.js';
import { type BackStep, type CloseTarget, type Zone, closeTarget } from '../nav-model.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type NavActionId =
  | 'view-jobs' | 'ascend-or-back' | 'close-pane' | 'next-tab' | 'prev-tab' | 'session-normal'
  | 'model-dropdown' | 'git-panel' | 'system-monitor';

/** What the navigation rules see (built by actions/nav-run.ts). */
export interface NavSnapshot {
  zone: Zone;
  /** Backspace's step now (null: at the root with the list's keys). */
  back: BackStep;
  projects: number;
}

export const NAV_ACTIONS: readonly ActionSpec[] = [
  { id: 'view-jobs', title: 'Jobs: scheduled-job sessions', group: 'view', checked: (s) => s.view === 'active' && s.nav?.zone === 'jobs', detail: 'The sessions scheduled jobs launched (gs returns to Active)' },
  { id: 'next-tab', title: 'Next project', group: 'view', detail: 'Steps the project filter forward (the TUI’s next workspace tab)' },
  { id: 'prev-tab', title: 'Previous project', group: 'view', detail: 'Steps the project filter back' },
  { id: 'model-dropdown', title: 'Default model for new sessions…', group: 'file', noMenu: true, detail: 'Pick the provider and model the New session dialog starts with; :model <id> sets it directly (physical Ctrl-M on macOS)' },
  { id: 'ascend-or-back', title: 'Back or up a level', group: 'go', menuBreak: true, detail: 'From the transcript, back to the list; in the list, up one container' },
  { id: 'close-pane', title: 'Close pane', group: 'go', detail: 'Closes the terminal, file viewer or explorer, graph, manager or Issues view, else the open session' },
  { id: 'session-normal', title: 'Open session, keys in the transcript', group: 'go', detail: 'Opens the cursor session and gives the transcript the keys, not the message box' },
  { id: 'git-panel', title: 'Git panel (lazygit)', group: 'view', menuBreak: true, detail: 'Runs lazygit in the embedded terminal, in the session’s directory' },
  { id: 'system-monitor', title: 'System monitor (btop)', group: 'view', detail: 'Runs btop in the embedded terminal when it is installed' },
];

type Rule = (s: UiSnapshot) => string | null;

export const NAV_RULES: Readonly<Record<NavActionId, Rule>> = {
  'view-jobs': () => null,
  'ascend-or-back': (s) => (s.nav?.back ? null : 'At the top level'),
  'close-pane': (s) => (closeTarget(s) ? null : 'No pane to close'),
  'next-tab': (s) => ((s.nav?.projects ?? 0) > 0 ? null : 'There are no projects yet'),
  'prev-tab': (s) => ((s.nav?.projects ?? 0) > 0 ? null : 'There are no projects yet'),
  'session-normal': (s) => (s.cursorOnRow || s.open ? null : 'No session under the cursor (j/k to move)'),
  'model-dropdown': () => null,
  'git-panel': () => null,
  'system-monitor': () => null,
};

const b = (action: NavActionId, arg?: string): DesktopBinding<NavActionId> => (arg ? { action, arg } : { action });

/** TUI Normal-mode keys (keybindings.md notation). */
export const NAV_KEYS: Readonly<Record<string, DesktopBinding<NavActionId | 'open'>>> = {
  gj: b('view-jobs'),
  Backspace: b('ascend-or-back'),
  '<Space>q': b('close-pane'),
  '>': b('next-tab'),
  '<': b('prev-tab'),
  e: b('session-normal'),
  'Ctrl-M': b('model-dropdown'),
  '<Space>gg': b('git-panel'),
  '<Space>bb': b('system-monitor'),
  // The desktop has one window and no tabs: <Space>T opens the session in place.
  '<Space>T': { action: 'open' },
};

/** TUI `:` commands (canonical names). */
export const NAV_COMMANDS: Readonly<Record<string, DesktopBinding<NavActionId>>> = {
  close: b('close-pane'),
  tabnext: b('next-tab'),
  tabprev: b('prev-tab'),
  model: b('model-dropdown'),
};

/** The core actions a CloseTarget dispatches (all but closing the open session). */
export type CloseAction = Exclude<CloseTarget, 'session'>;

declare module '../registry.js' {
  interface UiSnapshot {
    /** Navigation state (actions/nav-run.ts). */
    nav?: NavSnapshot;
  }
}

export const SLICE: SliceSpec<NavActionId> = {
  name: 'pane and zone navigation (G2, hub #3000)',
  order: 36,
  actions: NAV_ACTIONS,
  rules: NAV_RULES,
  // routeNavKey (nav-model.ts), run from the window keydown listener in actions/nav-run.ts; C-w arms h/j/k/l.
  rawKeys: ['C-d', 'C-u', 'C-f', 'C-b', 'C-w'],
  keys: NAV_KEYS,
  commands: NAV_COMMANDS,
};
