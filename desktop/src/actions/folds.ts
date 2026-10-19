// The fold keys (zo zc za zM zR) shared by slice 4 (session list, hub #3006)
// and slice 5 (transcript, hub #3007). Both slices bound the same TUI keys to
// the same action ids, so one spec per id lives here and each run goes to the
// pane that has the keys, as in the TUI: the transcript once the keys moved
// there (Shift-Down, ]u, a click on an event; slice 5's focus), else the list.
// Each slice keeps its own pane rule (DETAIL_FOLD_RULES / SESSIONLIST_FOLD_RULES);
// the keys stay in both slices' key tables (same key, same action). The DOM
// side is actions/folds-run.ts.
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';
import { DETAIL_FOLD_RULES } from './detail-spec.js';
import { SESSIONLIST_FOLD_RULES } from './sessionlist.js';

export type FoldActionId = 'fold-open' | 'fold-close' | 'fold-toggle' | 'fold-close-all' | 'fold-open-all';

export const FOLD_IDS: ReadonlySet<string> = new Set<FoldActionId>([
  'fold-open', 'fold-close', 'fold-toggle', 'fold-close-all', 'fold-open-all',
]);

export function isFoldAction(id: string): id is FoldActionId {
  return FOLD_IDS.has(id);
}

export const FOLD_ACTIONS: readonly ActionSpec[] = [
  { id: 'fold-open', title: 'Open fold', group: 'view', menuBreak: true, detail: 'List: show the children. Transcript: expand the selected event (a tool call with its result)' },
  { id: 'fold-close', title: 'Close fold', group: 'view', detail: 'List: hide the children. Transcript: collapse the selected event (a tool call with its result)' },
  { id: 'fold-toggle', title: 'Toggle fold', group: 'view', contexts: ['session-row'], detail: 'List: the cursor row. Transcript: the selected event' },
  { id: 'fold-close-all', title: 'Close all folds', group: 'view', detail: 'Every list container, or every transcript event' },
  { id: 'fold-open-all', title: 'Open all folds', group: 'view', detail: 'Every list container, or every transcript event' },
];

/**
 * The pane the fold keys act on. The transcript when slice 5 says the keys
 * are there; without a transcript snapshot, slice 4's last-used pane; and the
 * transcript when there is no list state at all.
 */
export function foldPane(s: UiSnapshot): 'list' | 'transcript' {
  if (!s.list) return 'transcript';
  if (s.detail) return s.detail.focus === 'transcript' ? 'transcript' : 'list';
  return s.list.paneDetail ? 'transcript' : 'list';
}

type Rule = (s: UiSnapshot) => string | null;
const byPane = (id: FoldActionId): Rule =>
  (s) => (foldPane(s) === 'transcript' ? DETAIL_FOLD_RULES[id](s) : SESSIONLIST_FOLD_RULES[id](s));

export const FOLD_RULES: Readonly<Record<FoldActionId, Rule>> = {
  'fold-open': byPane('fold-open'),
  'fold-close': byPane('fold-close'),
  'fold-toggle': byPane('fold-toggle'),
  'fold-close-all': byPane('fold-close-all'),
  'fold-open-all': byPane('fold-open-all'),
};

export const SLICE: SliceSpec<FoldActionId> = {
  name: 'folds (slices 4 and 5)',
  order: 40,
  actions: FOLD_ACTIONS,
  rules: FOLD_RULES,
};
