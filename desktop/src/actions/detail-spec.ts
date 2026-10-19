// The session-detail slice's entries for the action registry (pure, no DOM):
// event navigation, folding, visibility toggles, yank, rename, rating, model
// switch and the prompt preview. The registry collects SLICE through the
// generated actions/index.generated.ts; the DOM side is actions/detail.ts.
// Key hints come from the TUI keymap via DETAIL_KEY_BINDINGS / DETAIL_COMMAND_BINDINGS.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';
import type { FoldActionId } from './folds.js';

export type DetailActionId =
  | 'event-next' | 'event-prev' | 'user-next' | 'user-prev'
  | 'scroll-up' | 'scroll-down'
  | 'fold-open' | 'fold-close' | 'fold-toggle' | 'fold-close-all' | 'fold-open-all'
  | 'toggle-system' | 'toggle-thinking'
  | 'yank' | 'rename-session' | 'rate-session' | 'switch-model' | 'prompt-preview';

/** What the detail actions can see of the open session's transcript. */
export interface DetailSnapshot {
  /** Events loaded for the open session. */
  events: number;
  /** Events the transcript shows now. */
  shown: number;
  /** User messages the transcript shows now. */
  userMessages: number;
  /** An event is selected. */
  cursor: boolean;
  /** Where the keys are: the session list or the transcript. */
  focus: 'list' | 'transcript';
  hideSystem: boolean;
  hideThinking: boolean;
}

type Rule = (s: UiSnapshot) => string | null;

const NEED_OPEN = 'Open a session first (j/k, then Enter)';
const NO_EVENTS = 'This session has no events to move through';

const needOpen: Rule = (s) => (s.open ? null : NEED_OPEN);
const needEvents: Rule = (s) => {
  if (!s.open) return NEED_OPEN;
  return (s.detail?.shown ?? 0) > 0 ? null : NO_EVENTS;
};

export const DETAIL_ACTIONS: readonly ActionSpec[] = [
  // Session
  { id: 'rename-session', title: 'Rename session…', group: 'session', menuBreak: true, detail: 'Edit the title of the open session', contexts: ['session-row'] },
  { id: 'rate-session', title: 'Rate session…', group: 'session', detail: 'Rate the open session 1-10 (:rate N sets it directly)', contexts: ['session-row'] },
  { id: 'switch-model', title: 'Switch model or effort…', group: 'session', detail: 'Queue a model and effort for the next turn', contexts: ['session-row'] },
  { id: 'prompt-preview', title: 'Preview prompt', group: 'session', detail: 'Show the full launch prompt; j/k previews the next session', contexts: ['session-row'] },
  { id: 'yank', title: 'Copy event or session ID', group: 'session', menuBreak: true, detail: 'The selected event’s text in the transcript, else the session ID' },
  // Go: transcript motion
  { id: 'event-next', title: 'Next event', group: 'go', menuBreak: true, extraKeys: ['Shift-Down'], detail: 'Select the next event in the transcript' },
  { id: 'event-prev', title: 'Previous event', group: 'go', extraKeys: ['Shift-Up'], detail: 'Select the previous event in the transcript' },
  { id: 'user-next', title: 'Next user message', group: 'go', detail: 'Wraps to the first one' },
  { id: 'user-prev', title: 'Previous user message', group: 'go', detail: 'Wraps to the last one' },
  { id: 'scroll-down', title: 'Scroll transcript down', group: 'go', extraKeys: ['Down'], noMenu: true, detail: 'Three lines; in the message box only while the draft is one line' },
  { id: 'scroll-up', title: 'Scroll transcript up', group: 'go', extraKeys: ['Up'], noMenu: true, detail: 'Three lines; in the message box only while the draft is one line' },
  // View: folds and visibility
  // Folds (zo zc za zM zR): one spec per id in folds.ts, shared with slice 4.
  { id: 'toggle-system', title: 'Show system events', group: 'view', menuBreak: true, checked: (s) => !(s.detail?.hideSystem ?? false) },
  { id: 'toggle-thinking', title: 'Show thinking events', group: 'view', checked: (s) => !(s.detail?.hideThinking ?? false) },
];

export const DETAIL_RULES: Readonly<Record<Exclude<DetailActionId, FoldActionId>, Rule>> = {
  'event-next': needEvents,
  'event-prev': needEvents,
  'user-next': (s) => {
    if (!s.open) return NEED_OPEN;
    return (s.detail?.userMessages ?? 0) > 0 ? null : 'This session has no user messages';
  },
  'user-prev': (s) => {
    if (!s.open) return NEED_OPEN;
    return (s.detail?.userMessages ?? 0) > 0 ? null : 'This session has no user messages';
  },
  'scroll-up': needOpen,
  'scroll-down': needOpen,
  'toggle-system': needOpen,
  'toggle-thinking': needOpen,
  yank: needOpen,
  'rename-session': needOpen,
  'rate-session': needOpen,
  'switch-model': (s) => {
    if (!s.open) return NEED_OPEN;
    return s.open.readOnly ? 'This session is read-only' : null;
  },
  'prompt-preview': (s) => (s.open || s.cursorOnRow ? null : 'No session to preview (j/k to pick one)'),
};

type Bindings = Readonly<Record<string, DesktopBinding<DetailActionId>>>;
const b = (action: DetailActionId): DesktopBinding<DetailActionId> => ({ action });

/** TUI Normal-mode keys the detail slice runs (keybindings.md notation). */
export const DETAIL_KEY_BINDINGS: Bindings = {
  ']u': b('user-next'),
  '[u': b('user-prev'),
  zo: b('fold-open'),
  zc: b('fold-close'),
  za: b('fold-toggle'),
  zM: b('fold-close-all'),
  zR: b('fold-open-all'),
  zs: b('toggle-system'),
  zt: b('toggle-thinking'),
  yy: b('yank'),
  F2: b('rename-session'),
  '<Space>mm': b('switch-model'),
  p: b('prompt-preview'),
};

/** TUI `:` commands (canonical names) the detail slice runs. */
export const DETAIL_COMMAND_BINDINGS: Bindings = {
  rate: b('rate-session'),
};

/** The transcript side of the fold keys; actions/folds.ts picks the pane. */
export const DETAIL_FOLD_RULES: Readonly<Record<FoldActionId, Rule>> = {
  'fold-open': (s) => (s.open ? (s.detail?.cursor ? null : 'Select an event first (Shift-Down, ]u)') : NEED_OPEN),
  'fold-close': (s) => (s.open ? (s.detail?.cursor ? null : 'Select an event first (Shift-Down, ]u)') : NEED_OPEN),
  'fold-toggle': (s) => (s.open ? (s.detail?.cursor ? null : 'Select an event first (Shift-Down, ]u)') : NEED_OPEN),
  'fold-close-all': needEvents,
  'fold-open-all': needEvents,
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The open session's transcript (slice 5, actions/detail.ts). */
    detail?: DetailSnapshot;
  }
}

export const SLICE: SliceSpec<DetailActionId> = {
  name: 'detail (slice 5, hub #3007)',
  order: 10,
  actions: DETAIL_ACTIONS,
  rules: DETAIL_RULES,
  keys: DETAIL_KEY_BINDINGS,
  commands: DETAIL_COMMAND_BINDINGS,
};
