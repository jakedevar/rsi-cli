// The terminal overlay slice's entries for the action registry (pure, no DOM):
// toggle the embedded shell (`Ctrl-\`, `:term` / `:terminal` / `:shell`) and
// end it. The registry collects SLICE through the generated
// actions/index.generated.ts; the DOM side (RUNTIME) is actions/terminal.ts. `Ctrl-\` is one of the TUI's global intercepts, not a
// Normal-mode key, so it is listed as an extra key and caught ahead of the
// key engine by actions/terminal.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionId, ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type TerminalActionId = 'toggle-terminal' | 'kill-terminal';

/** What the terminal actions can see (built by actions/terminal.ts). */
export interface TerminalSnapshot {
  /** The overlay is showing. */
  visible: boolean;
  /** The terminal the toggle would show (or shows) has a running shell. */
  running: boolean;
}

type Rule = (s: UiSnapshot) => string | null;

export const TERMINAL_RULES: Readonly<Record<TerminalActionId, Rule>> = {
  'toggle-terminal': () => null,
  'kill-terminal': (s) => (s.terminal?.running ? null : 'No terminal shell is running for this session'),
};

export const TERMINAL_ACTIONS: readonly ActionSpec[] = [
  {
    id: 'toggle-terminal',
    title: 'Terminal',
    group: 'view',
    menuBreak: true,
    extraKeys: ['Ctrl-\\'],
    detail: 'Embedded shell in the open session’s working directory; it keeps running when hidden (Esc: scrollback mode)',
    checked: (s) => s.terminal?.visible ?? false,
  },
  { id: 'kill-terminal', title: 'End terminal shell', group: 'view', detail: 'Kill the shell of the open session’s terminal' },
];

type Bindings = Readonly<Record<string, DesktopBinding<ActionId>>>;

/** TUI `:` commands (canonical names) the terminal slice runs; the engine resolves `:terminal` / `:shell`. */
export const TERMINAL_COMMAND_BINDINGS: Bindings = {
  term: { action: 'toggle-terminal' },
};

const IDS: ReadonlySet<string> = new Set<TerminalActionId>(['toggle-terminal', 'kill-terminal']);

export function isTerminalAction(id: string): id is TerminalActionId {
  return IDS.has(id);
}

declare module '../registry.js' {
  interface UiSnapshot {
    /** The terminal overlay (slice 12, actions/terminal.ts). */
    terminal?: TerminalSnapshot;
  }
}

export const SLICE: SliceSpec<TerminalActionId> = {
  name: 'terminal (slice 12, hub #3014)',
  order: 60,
  actions: TERMINAL_ACTIONS,
  rules: TERMINAL_RULES,
  commands: TERMINAL_COMMAND_BINDINGS,
};
