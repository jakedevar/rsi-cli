// Slice 4 (hub #3006) actions: projects, sort, labels, folds, hierarchy,
// attention queue, search, session info (U is slice 6's unarchive-session).
// Pure (no DOM): the registry collects SLICE through the generated
// actions/index.generated.ts; the DOM `run` side is sessionlist-run.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';
import type { FoldActionId } from './folds.js';

export type SessionListActionId =
  | 'choose-project' | 'project-new' | 'project-edit' | 'project-delete' | 'change-session-project'
  | 'choose-sort' | 'choose-label' | 'session-info'
  | 'ascend' | 'fold-open' | 'fold-close' | 'fold-toggle' | 'fold-open-all' | 'fold-close-all'
  | 'label-next' | 'label-prev' | 'move-to-parent' | 'move-to-root'
  | 'attention-list' | 'attention-slot' | 'search-next' | 'search-prev' | 'clear-search';

/** What the slice-4 rules can see of the session list (built by sessionlist-run.ts). */
export interface ListSnapshot {
  /** The list cursor row has children (it can fold). */
  cursorFoldable: boolean;
  /** The cursor row's parent is in the list. */
  cursorHasParent: boolean;
  /** Rows with children in the list (zM / zR have something to do). */
  foldable: number;
  /** The last pane the operator used was the conversation, not the list. */
  paneDetail: boolean;
  /** Sessions that need the operator themselves. */
  attentionQueue: number;
  /** The list filter text. */
  search: string;
  /** A session is open and a filter is set (kept for the snapshot's readers; n / N use `findActive`). */
  detailSearchable: boolean;
  /** A conversation find (actions/find-run.ts) is open or remembered: n / N step its matches. */
  findActive?: boolean;
  projects: number;
  /** More than one label group among the rows. */
  labelGroups: boolean;
  openHasParent: boolean;
}

export const SESSIONLIST_ACTIONS: readonly ActionSpec[] = [
  // Session
  { id: 'session-info', title: 'Session info', group: 'session', menuBreak: true, contexts: ['session-row'], detail: 'Id, provider, model, project, label' },
  { id: 'choose-label', title: 'Set session label…', group: 'session', contexts: ['session-row'], detail: 'Pick, create, edit or delete a label' },
  { id: 'change-session-project', title: 'Change session project…', group: 'session', contexts: ['session-row'] },
  { id: 'move-to-parent', title: 'Move under a Group or Epic…', group: 'session', contexts: ['session-row'], detail: 'The daemon checks what each container accepts' },
  { id: 'move-to-root', title: 'Move to top level', group: 'session', contexts: ['session-row'] },
  // Go: list navigation
  { id: 'ascend', title: 'Up to parent', group: 'go', menuBreak: true, detail: 'Move the cursor to the enclosing Group or Epic' },
  // Folds (zo zc za zM zR): one spec per id in folds.ts, shared with slice 5.
  { id: 'label-next', title: 'Next label group', group: 'go', menuBreak: true },
  { id: 'label-prev', title: 'Previous label group', group: 'go' },
  { id: 'attention-list', title: 'Attention queue…', group: 'go', menuBreak: true, detail: 'Sessions waiting for you, numbered' },
  { id: 'attention-slot', title: 'Jump to attention slot', group: 'go', detail: 'Jump to the Nth waiting session (<Space>1 to <Space>9)' },
  { id: 'search-next', title: 'Next match', group: 'go', menuBreak: true, detail: 'The next match of the conversation find (/ with the keys in the transcript, Cmd-F), else the next session that matches the list filter' },
  { id: 'search-prev', title: 'Previous match', group: 'go' },
  { id: 'clear-search', title: 'Clear filter', group: 'go', extraKeys: ['Esc'], detail: 'Esc in the list clears a confirmed filter' },
  // File: the project actions sit right after New session… (registry menuAfter).
  { id: 'project-new', title: 'New project…', group: 'file', menuAfter: 'launch', menuBreak: true, detail: 'A folder with your code that sessions work in' },
  { id: 'project-edit', title: 'Edit project…', group: 'file', menuAfter: 'project-new', detail: ':project-edit <name> edits that project' },
  { id: 'project-delete', title: 'Delete project…', group: 'file', menuAfter: 'project-edit', detail: ':project-delete <name>' },
  // View
  { id: 'choose-project', title: 'Choose project…', group: 'view', menuBreak: true, detail: 'Filter the list to one project; :project <name> switches directly' },
  { id: 'choose-sort', title: 'Sort sessions…', group: 'view', detail: 'Longest idle, recently active, created, grouped by label' },
];

export const SESSIONLIST_KEYS: Readonly<Record<string, DesktopBinding<SessionListActionId>>> = {
  '<Space>p': { action: 'choose-project' },
  '<Space>C': { action: 'change-session-project' },
  '<Space>s': { action: 'choose-sort' },
  F3: { action: 'session-info' },
  ']g': { action: 'label-next' },
  '[g': { action: 'label-prev' },
  zo: { action: 'fold-open' },
  zc: { action: 'fold-close' },
  za: { action: 'fold-toggle' },
  zR: { action: 'fold-open-all' },
  zM: { action: 'fold-close-all' },
  '-': { action: 'ascend' },
  H: { action: 'ascend' },
  mp: { action: 'move-to-parent' },
  mo: { action: 'move-to-root' },
  // U, <Space>n and :alerts are slice 6's (unarchive-session, open-notifications).
  n: { action: 'search-next' },
  N: { action: 'search-prev' },
  ...Object.fromEntries(
    [1, 2, 3, 4, 5, 6, 7, 8, 9].map((n) => [`<Space>${n}`, { action: 'attention-slot', arg: String(n) }]),
  ),
};

export const SESSIONLIST_COMMANDS: Readonly<Record<string, DesktopBinding<SessionListActionId>>> = {
  projects: { action: 'choose-project' },
  project: { action: 'choose-project' },
  'project-new': { action: 'project-new' },
  'project-edit': { action: 'project-edit' },
  'project-delete': { action: 'project-delete' },
  group: { action: 'choose-label' },
};

// ---------- availability ----------

type Rule = (s: UiSnapshot) => string | null;
const NEED_OPEN = 'Open a session first: click one in the list (j/k, then Enter)';
const open = (more?: (o: NonNullable<UiSnapshot['open']>, l: ListSnapshot) => string | null): Rule =>
  (s) => (s.open ? (more ? more(s.open, s.list ?? EMPTY) : null) : NEED_OPEN);
const writable = (o: NonNullable<UiSnapshot['open']>): string | null => (o.readOnly ? 'This session is read-only' : null);

export const EMPTY: ListSnapshot = {
  cursorFoldable: false, cursorHasParent: false, foldable: 0, paneDetail: false, attentionQueue: 0,
  search: '', detailSearchable: false, projects: 0, labelGroups: false, openHasParent: false,
};
const list = (more: (l: ListSnapshot, s: UiSnapshot) => string | null): Rule => (s) => more(s.list ?? EMPTY, s);

const NO_SEARCH = 'Type a filter (/) first, or search a conversation with / (keys in the transcript) or Cmd-F';
const NOT_FOLDABLE = 'The cursor is not on a session with children';
const NO_FOLDS = 'No session has children to fold';

export const SESSIONLIST_RULES: Readonly<Record<Exclude<SessionListActionId, FoldActionId>, Rule>> = {
  'choose-project': () => null,
  'project-new': () => null,
  'project-edit': list((l) => (l.projects > 0 ? null : 'There are no projects yet')),
  'project-delete': list((l) => (l.projects > 0 ? null : 'There are no projects yet')),
  'choose-sort': () => null,
  'session-info': open(),
  'choose-label': open((o) => writable(o)),
  'change-session-project': open((o) => writable(o)),
  'move-to-parent': open((o) => writable(o)),
  'move-to-root': open((o, l) => writable(o) ?? (l.openHasParent ? null : 'The open session is already at the top level')),
  ascend: list((l) => (l.cursorHasParent ? null : 'The cursor row has no parent in the list')),
  'label-next': list((l) => (l.labelGroups ? null : 'No session has a different label')),
  'label-prev': list((l) => (l.labelGroups ? null : 'No session has a different label')),
  'attention-list': () => null,
  'attention-slot': list((l) => (l.attentionQueue > 0 ? null : 'No session is waiting for you')),
  // They step the conversation find's matches while one is active, else the filtered list's rows (G2).
  'search-next': list((l) => (l.search.trim() !== '' || l.findActive ? null : NO_SEARCH)),
  'search-prev': list((l) => (l.search.trim() !== '' || l.findActive ? null : NO_SEARCH)),
  'clear-search': list((l) => (l.search.trim() !== '' ? null : 'No filter is active')),
};

/** The list side of the fold keys; actions/folds.ts picks the pane. */
export const SESSIONLIST_FOLD_RULES: Readonly<Record<FoldActionId, Rule>> = {
  'fold-open': list((l) => (l.cursorFoldable ? null : NOT_FOLDABLE)),
  'fold-close': list((l) => (l.cursorFoldable ? null : NOT_FOLDABLE)),
  'fold-toggle': list((l) => (l.cursorFoldable ? null : NOT_FOLDABLE)),
  'fold-open-all': list((l) => (l.foldable > 0 ? null : NO_FOLDS)),
  'fold-close-all': list((l) => (l.foldable > 0 ? null : NO_FOLDS)),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** Session-list state for the slice-4 actions (actions/sessionlist-run.ts). */
    list?: ListSnapshot;
  }
}

export const SLICE: SliceSpec<SessionListActionId> = {
  name: 'session list (slice 4, hub #3006)',
  order: 30,
  actions: SESSIONLIST_ACTIONS,
  rules: SESSIONLIST_RULES,
  // Esc clears a confirmed filter, from the document keydown listener in actions/sessionlist-run.ts.
  rawKeys: ['Esc'],
  keys: SESSIONLIST_KEYS,
  commands: SESSIONLIST_COMMANDS,
};
