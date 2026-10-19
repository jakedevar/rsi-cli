// Session operations (hub #3000 gap sweep G2) for the action registry, pure
// (no DOM): pin, testing-needed, auto-rotation, cancel retry, rotate, commit
// and push, and landing the session branch. The rules are sessionops-model.ts;
// the DOM side is actions/sessionops-run.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';
import { type OpsSession, commitBlock, rotateBlock, testingBlock } from '../sessionops-model.js';

export type SessionOpsActionId =
  | 'toggle-pin' | 'toggle-testing' | 'toggle-rotation' | 'cancel-retry'
  | 'rotate-session' | 'commit-push' | 'land-session';

/** What the session-op rules see (built by actions/sessionops-run.ts). */
export interface OpsSnapshot {
  /** The session a lifecycle toggle acts on: the open one, else the list cursor's. */
  target: OpsSession | null;
  /** The session `R` rotates: the open one when the transcript has the keys, else the target. */
  rotateTarget: OpsSession | null;
  /** Read-only target (Archived view or an archived session). */
  readOnly: boolean;
}

export const SESSIONOPS_ACTIONS: readonly ActionSpec[] = [
  { id: 'toggle-pin', title: 'Pin or unpin session', group: 'session', menuBreak: true, contexts: ['session-row'], detail: 'Pinned sessions stay at the top of the list (◆)' },
  { id: 'toggle-testing', title: 'Toggle testing-needed marker', group: 'session', contexts: ['session-row'], detail: 'Marks or clears manual testing on a leaf session (☐)' },
  { id: 'toggle-rotation', title: 'Toggle auto-rotation', group: 'session', contexts: ['session-row'], detail: 'Disables or re-enables automatic context rotation (⊘ while off)' },
  { id: 'cancel-retry', title: 'Cancel pending retry', group: 'session', contexts: ['session-row'], detail: 'Cancels the session’s pending automatic retry (↻N/M)' },
  { id: 'rotate-session', title: 'Rotate session context', group: 'session', menuBreak: true, contexts: ['session-row'], detail: 'Rotates into a fresh context, carrying a handoff forward' },
  { id: 'commit-push', title: 'Commit and push', group: 'session', contexts: ['session-row'], detail: 'Continues the open session with /ci_commit' },
  { id: 'land-session', title: 'Land session branch…', group: 'session', contexts: ['session-row'], detail: 'First press shows the commit; press again within 30 s to queue it' },
];

type Rule = (s: UiSnapshot) => string | null;

const NEED_TARGET = 'Open a session or move the cursor onto one first (j/k)';
const NEED_OPEN = 'Open a session first (j/k, then Enter)';

const target = (more?: (t: OpsSession, s: UiSnapshot) => string | null): Rule => (s) => {
  const t = s.ops?.target;
  if (!t) return NEED_TARGET;
  return more ? more(t, s) : null;
};
const writable = (s: UiSnapshot): string | null => (s.ops?.readOnly ? 'This session is read-only' : null);

export const SESSIONOPS_RULES: Readonly<Record<SessionOpsActionId, Rule>> = {
  'toggle-pin': target(),
  'toggle-testing': target((t, s) => writable(s) ?? testingBlock(t)),
  'toggle-rotation': target((_t, s) => writable(s)),
  'cancel-retry': target((_t, s) => writable(s)),
  'rotate-session': (s) => {
    const t = s.ops?.rotateTarget;
    if (!t) return NEED_TARGET;
    return writable(s) ?? rotateBlock(t);
  },
  'commit-push': (s) => {
    if (!s.open) return NEED_OPEN;
    if (s.open.readOnly) return 'This session is read-only';
    const t = s.ops?.target;
    return t ? commitBlock(t) : null;
  },
  'land-session': target(),
};

const b = (action: SessionOpsActionId): DesktopBinding<SessionOpsActionId> => ({ action });

/** TUI Normal-mode keys (keybindings.md notation). */
export const SESSIONOPS_KEYS: Readonly<Record<string, DesktopBinding<SessionOpsActionId>>> = {
  P: b('toggle-pin'),
  '<Space>t': b('toggle-testing'),
  '<Space>r': b('toggle-rotation'),
  '<Space>k': b('cancel-retry'),
  R: b('rotate-session'),
  '<Space>x': b('commit-push'),
  '<Space>gl': b('land-session'),
};

/** TUI `:` commands (canonical names). */
export const SESSIONOPS_COMMANDS: Readonly<Record<string, DesktopBinding<SessionOpsActionId>>> = {
  rotate: b('rotate-session'),
  land: b('land-session'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** Session-op targets (actions/sessionops-run.ts). */
    ops?: OpsSnapshot;
  }
}

export const SLICE: SliceSpec<SessionOpsActionId> = {
  name: 'session operations (G2, hub #3000)',
  order: 32,
  actions: SESSIONOPS_ACTIONS,
  rules: SESSIONOPS_RULES,
  keys: SESSIONOPS_KEYS,
  commands: SESSIONOPS_COMMANDS,
};
