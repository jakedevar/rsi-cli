// The session-lifecycle slice's entries for the action registry (pure, no DOM):
// delete to trash, unarchive, downgrade a pause, the trash browser, the
// notification browser, recent completions and the question modal.
// The registry collects SLICE through the generated actions/index.generated.ts;
// the DOM side is actions/lifecycle.ts. Key hints come from the TUI keymap via
// LIFECYCLE_KEY_BINDINGS / LIFECYCLE_COMMAND_BINDINGS (spread into keybindings.ts).
import type { DesktopBinding } from '../keyengine.js';
import { deleteBlock, downgradeBlock, unarchiveBlock } from '../lifecycle-model.js';
import type { ActionId, ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type LifecycleActionId =
  | 'delete-session' | 'unarchive-session' | 'downgrade-pause'
  | 'open-trash' | 'open-notifications' | 'open-recent';

/** What the lifecycle actions can see (built by actions/lifecycle.ts). */
export interface LifecycleSnapshot {
  /** The session a lifecycle action acts on (the open one, else the list cursor's). */
  target: { status: string } | null;
  /** Sessions with a question waiting (the open one first). */
  questions: number;
  /** Notifications still on the status line. */
  notices: number;
  /** Finished sessions the recent-completions list would show. */
  completions: number;
}

type Rule = (s: UiSnapshot) => string | null;

const NEED_TARGET = 'Open a session or move the cursor onto one first (j/k)';

/** Rules for the actions this slice adds, plus the one it takes over from the base list. */
export const LIFECYCLE_RULES: Readonly<Record<LifecycleActionId | 'answer-question', Rule>> = {
  'delete-session': (s) => (s.lifecycle?.target ? deleteBlock(s.lifecycle.target.status) : NEED_TARGET),
  'unarchive-session': (s) => (s.lifecycle?.target ? unarchiveBlock(s.lifecycle.target.status) : NEED_TARGET),
  'downgrade-pause': (s) => {
    if (!s.open) return 'Open a session first: click one in the list (j/k, then Enter)';
    return downgradeBlock(s.open.pauseLevel);
  },
  'open-trash': () => null,
  'open-notifications': () => null,
  'open-recent': (s) => ((s.lifecycle?.completions ?? 0) > 0 ? null : 'No session has finished yet'),
  // `<Space>gq` opens the question panel for the open session, else the first session with a question.
  'answer-question': (s) => {
    if (s.open?.hasQuestion || (s.lifecycle?.questions ?? 0) > 0) return null;
    return s.open ? 'The open session has no question waiting' : 'Open a session first: click one in the list (j/k, then Enter)';
  },
};

export const LIFECYCLE_ACTIONS: readonly ActionSpec[] = [
  { id: 'delete-session', title: 'Delete session…', group: 'session', menuBreak: true, detail: 'Move the session to the trash (DD, or :delete)', contexts: ['session-row'] },
  { id: 'unarchive-session', title: 'Unarchive session', group: 'session', detail: 'Restore an archived session to the active list', contexts: ['session-row'] },
  { id: 'downgrade-pause', title: 'Downgrade hard pause to soft', group: 'session', detail: 'A granted manager may then restart the session', contexts: ['session-row'] },
  { id: 'open-trash', title: 'Trash…', group: 'go', menuBreak: true, detail: 'Browse deleted sessions; U restores one, D twice purges it' },
  { id: 'open-notifications', title: 'Notifications…', group: 'go', detail: 'Alerts and recent notices; Enter opens the linked session' },
  { id: 'open-recent', title: 'Recent completions…', group: 'go', detail: 'The six latest finished sessions' },
];

type Bindings = Readonly<Record<string, DesktopBinding<ActionId>>>;
const b = (action: ActionId): DesktopBinding<ActionId> => ({ action });

/** TUI Normal-mode keys the lifecycle slice runs (keybindings.md notation). */
export const LIFECYCLE_KEY_BINDINGS: Bindings = {
  DD: b('delete-session'),
  U: b('unarchive-session'),
  '<Space>hs': b('downgrade-pause'),
  '<Space>gX': b('open-trash'),
  '<Space>n': b('open-notifications'),
  gr: b('open-recent'),
};

/** TUI `:` commands (canonical names) the lifecycle slice runs. */
export const LIFECYCLE_COMMAND_BINDINGS: Bindings = {
  delete: b('delete-session'),
  'pause soft': b('downgrade-pause'),
  alerts: b('open-notifications'),
};

const IDS: ReadonlySet<string> = new Set<LifecycleActionId>([
  'delete-session', 'unarchive-session', 'downgrade-pause', 'open-trash', 'open-notifications', 'open-recent',
]);

export function isLifecycleAction(id: string): id is LifecycleActionId {
  return IDS.has(id);
}

declare module '../registry.js' {
  interface UiSnapshot {
    /** Delete, trash, notifications and questions (slice 6, actions/lifecycle.ts). */
    lifecycle?: LifecycleSnapshot;
  }
}

export const SLICE: SliceSpec<LifecycleActionId> = {
  name: 'lifecycle (slice 6, hub #3008)',
  order: 20,
  actions: LIFECYCLE_ACTIONS,
  rules: LIFECYCLE_RULES,
  keys: LIFECYCLE_KEY_BINDINGS,
  commands: LIFECYCLE_COMMAND_BINDINGS,
};
