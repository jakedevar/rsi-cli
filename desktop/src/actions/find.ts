// Find in the open conversation (local #307), registry entries, pure (no DOM). The TUI's `/` in session detail
// searches the transcript and `n` / `N` step through the matches; the desktop adds a find bar for it that Cmd-F
// (Ctrl-F off macOS) opens with Vim keys off. The session-list filter stays the list's own. The DOM side is
// actions/find-run.ts; the matching is find-model.ts.
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type FindActionId = 'find-in-conversation';

const NEED_OPEN = 'Open a session first: click one in the list (j/k, then Enter)';

export const FIND_ACTIONS: readonly ActionSpec[] = [
  {
    id: 'find-in-conversation',
    title: 'Find in conversation…',
    group: 'go',
    menuBreak: true,
    extraKeys: ['Ctrl-F'],
    detail: 'Searches every event of the open conversation, earlier ones too; Enter and Shift+Enter step, Esc closes. / does the same with the keys in the transcript',
  },
];

export const FIND_RULES: Readonly<Record<FindActionId, (s: UiSnapshot) => string | null>> = {
  'find-in-conversation': (s) => (s.open ? null : NEED_OPEN),
};

export const SLICE: SliceSpec<FindActionId> = {
  name: 'find in conversation (local #307)',
  order: 31,
  actions: FIND_ACTIONS,
  rules: FIND_RULES,
  // Cmd-F (Ctrl-F off macOS with Vim keys off) from one window capture listener in actions/find-run.ts; with Vim keys
  // on, Ctrl-F stays the transcript's page-down (slice 'pane and zone navigation' routes C-f).
  rawKeys: ['Cmd-f'],
};
