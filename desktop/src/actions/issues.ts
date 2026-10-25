// The Issues workspace's slice of the action registry (hub #3010): action
// specs and availability rules, pure (no DOM). The registry collects SLICE
// through the generated actions/index.generated.ts; the DOM side is issues.ts
// (RUNTIME in actions/issues-run.ts). Keys inside the workspace (j/k, n, e, S,
// dd …) are the TUI's Local table keys (issues-model.ts ISSUES_KEYS); `<Space>i`,
// `:issues` and `:issue` are engine bindings (ISSUES_KEY_BINDINGS below).
import { type Issue, type WriteWhat, writeBlock } from '../issues-model.js';
import type { IssuesTab } from '../issues-tracker-model.js';
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type IssuesActionId =
  | 'issues-open' | 'issues-close' | 'issues-go-number'
  | 'issues-down' | 'issues-up' | 'issues-first' | 'issues-last' | 'issues-inspect'
  | 'issues-page-next' | 'issues-page-prev' | 'issues-refresh'
  | 'issues-search' | 'issues-filters' | 'issues-sort' | 'issues-operator'
  | 'issues-view-ready' | 'issues-view-blocked' | 'issues-view-mine' | 'issues-view-recent' | 'issues-view-all'
  | 'issues-new' | 'issues-edit' | 'issues-status' | 'issues-priority' | 'issues-assign' | 'issues-labels'
  | 'issues-deps' | 'issues-cancel' | 'issues-archive' | 'issues-restore'
  | 'issues-copy-uuid' | 'issues-copy-ref' | 'issues-section' | 'issues-more'
  | 'issues-tab-prev' | 'issues-tab-next' | 'issues-poll' | 'issues-topology' | 'issues-less' | 'issues-open-session';

/** What the Issues actions can see (built by issues.ts). */
export interface IssuesSnapshot {
  /** The workspace is on screen. */
  active: boolean;
  hasProject: boolean;
  /** The selected Issue of the Local table. */
  issue: Issue | null;
  nextPage: boolean;
  previousPage: boolean;
  operatorOnly: boolean;
  /** The workspace tab: Local, Dispatched or Sync (G2). */
  tab: IssuesTab;
  /** Dispatched: a dispatch record is selected. Local: the Issue has an associated session. */
  session: boolean;
}

const C = ['issue-row'] as const;

/** Menu order. All are in the Issues menu; the palette lists them all. */
export const ISSUES_ACTIONS: readonly ActionSpec[] = [
  { id: 'issues-open', title: 'Issues workspace', group: 'issues', detail: 'List, inspect and edit this project’s Issues' },
  { id: 'issues-go-number', title: 'Go to Issue number…', group: 'issues', detail: ':issue 12 opens #12, archived included' },
  { id: 'issues-close', title: 'Close Issues workspace', group: 'issues', extraKeys: ['Esc', 'q'] },
  // Move
  { id: 'issues-down', title: 'Next Issue in the table', group: 'issues', menuBreak: true, extraKeys: ['j'], detail: 'At the page edge, loads the next page' },
  { id: 'issues-up', title: 'Previous Issue in the table', group: 'issues', extraKeys: ['k'], detail: 'At the page edge, loads the previous page' },
  { id: 'issues-first', title: 'First Issue page', group: 'issues', extraKeys: ['gg'] },
  { id: 'issues-last', title: 'Last Issue page', group: 'issues', extraKeys: ['G'] },
  { id: 'issues-page-next', title: 'Next page of Issues', group: 'issues' },
  { id: 'issues-page-prev', title: 'Previous page of Issues', group: 'issues' },
  { id: 'issues-inspect', title: 'Inspect Issue', group: 'issues', extraKeys: ['Enter'], contexts: C },
  { id: 'issues-section', title: 'Next history section', group: 'issues', extraKeys: ['Tab'], detail: 'Blocked by, Blocks, Events, Handoffs' },
  { id: 'issues-more', title: 'Load more history', group: 'issues', extraKeys: ['PageDown'], detail: 'The next page of the open section' },
  { id: 'issues-refresh', title: 'Refresh Issues', group: 'issues', extraKeys: ['r'] },
  // Filter
  { id: 'issues-search', title: 'Text search in Issues…', group: 'issues', menuBreak: true, extraKeys: ['/'], detail: 'Text search; resets to the first page' },
  { id: 'issues-filters', title: 'Filters…', group: 'issues', extraKeys: ['f'], detail: 'Status, priority, readiness, assignee, labels, provenance, archive' },
  { id: 'issues-sort', title: 'Cycle sort', group: 'issues', extraKeys: ['s'], detail: 'Updated, number, priority' },
  { id: 'issues-operator', title: 'Operator requests only', group: 'issues', extraKeys: ['O'], detail: 'operator-request and operator-decision labels', checked: (s) => !!s.issues?.operatorOnly },
  { id: 'issues-view-ready', title: 'Saved view: Ready', group: 'issues' },
  { id: 'issues-view-blocked', title: 'Saved view: Blocked', group: 'issues' },
  { id: 'issues-view-mine', title: 'Saved view: Mine', group: 'issues', detail: 'Asks for your assignee name once' },
  { id: 'issues-view-recent', title: 'Saved view: Recent', group: 'issues' },
  { id: 'issues-view-all', title: 'Clear filters', group: 'issues' },
  // Write
  { id: 'issues-new', title: 'New Issue…', group: 'issues', menuBreak: true, extraKeys: ['n'] },
  { id: 'issues-edit', title: 'Edit Issue…', group: 'issues', extraKeys: ['e'], contexts: C, detail: 'Title, body, priority, assignee, labels' },
  { id: 'issues-status', title: 'Change status…', group: 'issues', extraKeys: ['S'], contexts: C, detail: 'Also reopens a closed or cancelled Issue' },
  { id: 'issues-priority', title: 'Change priority…', group: 'issues', extraKeys: ['p'], contexts: C },
  { id: 'issues-assign', title: 'Assign or unassign…', group: 'issues', extraKeys: ['a'], contexts: C },
  { id: 'issues-labels', title: 'Edit labels…', group: 'issues', extraKeys: ['l'], contexts: C },
  { id: 'issues-deps', title: 'Edit dependencies…', group: 'issues', extraKeys: ['b'], contexts: C, detail: 'Blocked by and Blocks' },
  { id: 'issues-cancel', title: 'Cancel Issue…', group: 'issues', menuBreak: true, extraKeys: ['dd'], contexts: C, detail: 'Sets status Cancelled; never deletes' },
  { id: 'issues-archive', title: 'Archive this Issue…', group: 'issues', extraKeys: ['A'], contexts: C, detail: 'A closed or cancelled Issue' },
  { id: 'issues-restore', title: 'Restore archived Issue', group: 'issues', extraKeys: ['U'], contexts: C },
  { id: 'issues-copy-uuid', title: 'Copy Issue UUID', group: 'issues', menuBreak: true, extraKeys: ['yy'], contexts: C },
  { id: 'issues-copy-ref', title: 'Copy Issue number', group: 'issues', extraKeys: ['y#'], contexts: C, detail: 'Copies #N' },
  // G2: tabs, tracker poll, topology starter, history paging, sessions.
  { id: 'issues-tab-prev', title: 'Previous Issues tab', group: 'issues', menuBreak: true, extraKeys: ['['], detail: 'Local, Dispatched, Sync' },
  { id: 'issues-tab-next', title: 'Next Issues tab', group: 'issues', extraKeys: [']'], detail: 'Local, Dispatched, Sync' },
  { id: 'issues-poll', title: 'Run tracker poll now', group: 'issues', extraKeys: ['P'], detail: 'Sync tab: one manual poll, then Sync and Dispatched refresh' },
  { id: 'issues-topology', title: 'Run topology on Issue', group: 'issues', extraKeys: ['T'], contexts: C, detail: 'Starts issue-implement-review-land on the selected Issue under its project’s Epic' },
  { id: 'issues-less', title: 'Previous history page', group: 'issues', extraKeys: ['PageUp'], detail: 'Back up the open history section' },
  { id: 'issues-open-session', title: 'Open associated session', group: 'issues', contexts: C, detail: 'Enter in the inspector: the session launched for the Issue, or the dispatched one' },
];

type Rule = (s: UiSnapshot) => string | null;

const NEED_WORKSPACE = 'Open the Issues workspace first (choose “Issues workspace” in Actions; <Space>i)';
const inside = (more?: (i: IssuesSnapshot) => string | null): Rule => (s) => {
  const i = s.issues;
  if (!i?.active) return NEED_WORKSPACE;
  return more ? more(i) : null;
};
const selection: Rule = inside((i) => (i.issue ? null : 'Select an Issue first (j/k)'));
const writes = (what: WriteWhat): Rule => inside((i) => writeBlock(i.issue, what));

const BASE_RULES: Readonly<Record<IssuesActionId, Rule>> = {
  'issues-open': () => null,
  'issues-go-number': (s) => (s.issues?.hasProject ? null : 'No project to look in yet'),
  'issues-close': inside(),
  'issues-down': inside(),
  'issues-up': inside(),
  'issues-first': inside(),
  'issues-last': inside(),
  'issues-inspect': inside((i) => {
    if (i.tab === 'sync') return 'The Sync tab has nothing to inspect';
    if (i.tab === 'dispatched') return i.session ? null : 'Select a dispatched Issue first (j/k)';
    return i.issue ? null : 'Select an Issue first (j/k)';
  }),
  'issues-page-next': inside((i) => (i.nextPage ? null : 'This is the last page')),
  'issues-page-prev': inside((i) => (i.previousPage ? null : 'This is the first page')),
  'issues-refresh': inside(),
  'issues-search': inside(),
  'issues-filters': inside(),
  'issues-sort': inside(),
  'issues-operator': inside(),
  'issues-view-ready': inside(),
  'issues-view-blocked': inside(),
  'issues-view-mine': inside(),
  'issues-view-recent': inside(),
  'issues-view-all': inside(),
  'issues-new': inside((i) => (i.hasProject ? null : 'Pick a project first')),
  'issues-edit': writes('edit'),
  'issues-status': writes('status'),
  'issues-priority': writes('edit'),
  'issues-assign': writes('edit'),
  'issues-labels': writes('edit'),
  'issues-deps': selection,
  'issues-cancel': writes('cancel'),
  'issues-archive': writes('archive'),
  'issues-restore': writes('restore'),
  'issues-copy-uuid': selection,
  'issues-copy-ref': selection,
  'issues-tab-prev': inside(),
  'issues-tab-next': inside(),
  'issues-poll': inside((i) => (i.tab === 'sync' ? null : 'Switch to the Sync tab first (])')),
  'issues-topology': inside((i) => ((i.tab ?? 'local') !== 'local' ? 'This works on the Local tab ([ / ])' : i.issue ? null : 'Select an Issue first (j/k)')),
  'issues-less': inside((i) => ((i.tab ?? 'local') !== 'local' ? 'This works on the Local tab ([ / ])' : i.issue ? null : 'Select an Issue first (j/k)')),
  'issues-open-session': inside((i) => (i.session ? null : i.tab === 'dispatched' ? 'Select a dispatched Issue first (j/k)' : 'No session was launched for this Issue')),
  'issues-section': selection,
  'issues-more': selection,
};

/** Actions of the Local table only; on the Dispatched and Sync tabs they say so (G2). */
const LOCAL_ONLY: ReadonlySet<IssuesActionId> = new Set<IssuesActionId>([
  'issues-page-next', 'issues-page-prev', 'issues-search', 'issues-filters', 'issues-sort', 'issues-operator',
  'issues-view-ready', 'issues-view-blocked', 'issues-view-mine', 'issues-view-recent', 'issues-view-all',
  'issues-new', 'issues-edit', 'issues-status', 'issues-priority', 'issues-assign', 'issues-labels', 'issues-deps',
  'issues-cancel', 'issues-archive', 'issues-restore', 'issues-copy-uuid', 'issues-copy-ref', 'issues-section',
  'issues-more',
]);

export const ISSUES_RULES: Readonly<Record<IssuesActionId, Rule>> = Object.fromEntries(
  (Object.entries(BASE_RULES) as Array<[IssuesActionId, Rule]>).map(([id, rule]) => [
    id,
    LOCAL_ONLY.has(id)
      ? (s: UiSnapshot) => (s.issues?.active && (s.issues.tab ?? 'local') !== 'local' ? 'This works on the Local tab ([ / ])' : rule(s))
      : rule,
  ]),
) as Record<IssuesActionId, Rule>;

const IDS: ReadonlySet<string> = new Set(ISSUES_ACTIONS.map((a) => a.id));
export const isIssuesAction = (id: string): id is IssuesActionId => IDS.has(id);

/** TUI Normal-mode keys the Issues slice runs (the workspace's own keys are in issues-model.ts). */
export const ISSUES_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<IssuesActionId>>> = {
  '<Space>i': { action: 'issues-open' },
};

/** TUI `:` commands (canonical names) the Issues slice runs. */
export const ISSUES_COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<IssuesActionId>>> = {
  issues: { action: 'issues-open' },
  issue: { action: 'issues-go-number' },
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The Issues workspace (slice 8, issues.ts). */
    issues?: IssuesSnapshot;
  }
}

export const SLICE: SliceSpec<IssuesActionId> = {
  name: 'issues (slice 8, hub #3010)',
  order: 50,
  actions: ISSUES_ACTIONS,
  rules: ISSUES_RULES,
  keys: ISSUES_KEY_BINDINGS,
  commands: ISSUES_COMMAND_BINDINGS,
};
