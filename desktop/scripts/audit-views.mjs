// The main views the audit walks (local #298, from audit 2 / #290's 38-view pass): every palette action that opens a
// screen, panel or dialog of its own. The registry (src/registry.ts) does not mark which actions those are, so the list
// is kept here and tests/audit-views.test.mjs fails when an action that looks like it opens something is in neither
// AUDIT_VIEWS nor AUDIT_SKIPPED, so a new view cannot be left out of an audit unnoticed.

/** Action ids that open a view or dialog on their own, from the session list with one session open. */
export const AUDIT_VIEWS = [
  'launch', 'settings-open', 'settings-hooks', 'settings-skills', 'theme-picker', 'theme-roles', 'help', 'cm-manual',
  'issues-open', 'issues-new', 'mgr-dashboard', 'mgr-global', 'mgr-fleet', 'mgr-board', 'mgr-tree', 'mgr-inbox', 'mgr-budget',
  'mgr-escalations', 'mgr-policy', 'mgr-decisions', 'mgr-inspect', 'mgr-portfolio', 'mgr-node', 'mgr-friction', 'mgr-restart', 'mgr-scope',
  'admin-diagnostics', 'admin-schedules', 'admin-remote', 'admin-satellites', 'admin-mcp', 'admin-provider-keys', 'admin-scratch', 'admin-settlement',
  'open-providers', 'open-bridges', 'open-hooks', 'toggle-terminal', 'files-find', 'files-explorer', 'graph-open', 'dag-open', 'esp-open',
  'todo-toggle', 'open-trash', 'open-notifications', 'open-recent', 'attention-list', 'emergency-stop', 'memory-search', 'dialectic',
  'pending-messages', 'switch-model', 'model-dropdown', 'choose-project', 'choose-sort', 'choose-label', 'rename-session', 'rate-session',
  'session-info', 'prompt-preview', 'prompt-creator', 'cm-cold-archive', 'project-new', 'card', 'card-user', 'text-area-background',
];

/** A view that is only available from inside another one: open `before` first (by id), then the view. */
export const AUDIT_BEFORE = { 'issues-new': 'issues-open' };

/**
 * Actions that look like they open something (a title ending in an ellipsis, or an `-open`, `open-`, `admin-` or `mgr-` id) but are
 * not a view of their own, with the reason. A rule is an id or a RegExp over ids.
 */
export const AUDIT_SKIPPED = [
  [/^issues-(?!open$|new$)/, 'acts on the selected Issue or the Issues table; the workspace itself is issues-open'],
  [/^topology-/, 'acts on a selected Epic or Issue'],
  [/^(graph|dag)-/, 'a command inside the graph or DAG view (graph-open and dag-open open them)'],
  [/^files-/, 'a command on the file tree or an open file (files-explorer and files-find open them)'],
  [/^todo-/, 'a command on the to-do panel (todo-toggle opens it)'],
  [/^mgr-(refresh|close|appoint|clear|global-grant|global-appoint|global-revoke|portfolio-appoint)$/, 'a command in a manager view, a confirm, or an appointment dialog that needs a selected session'],
  [/^(project-edit|project-delete)$/, 'needs a project selected; project-new opens the same form'],
  [/^(archive|delete-session|unarchive-session)$/, 'a confirm for the open session'],
  [/^(change-session-project|move-to-parent|create-group|create-epic|create-story|create-task|create-bug|land-session)$/, 'needs a selected session or container'],
  [/^(cm-|fold-|settings-(apply|discard|refresh))/, 'a command, not a view'],
  [/^(input-modal|compile-prompt|fix-grammar|ai-command|ai-chat|continue)$/, 'works on the message box text'],
  [/^find-in-conversation$/, 'opens the find bar inside the conversation, not a screen of its own'],
  [/^(open|open-recent-file|command-line|palette|session-normal)$/, 'moves focus or opens a session, not a screen of its own'],
  [/^(view-jobs|git-panel|system-monitor)$/, 'a list filter or an external terminal program'],
];

const skipped = (id) => AUDIT_SKIPPED.some(([rule]) => (rule instanceof RegExp ? rule.test(id) : rule === id));

/** True when the action looks like it opens something (the signal the coverage test uses). */
export function looksLikeAView(action) {
  return action.title.endsWith('…') || /(-open$|^open-|^admin-|^mgr-)/.test(action.id);
}

/**
 * The view actions as the built app registers them: `[{ id, title, before? }]`, in AUDIT_VIEWS order (`before`: the title of the view to open first). Throws when a listed id
 * is no longer a registered action (a renamed view must be renamed here too).
 */
export async function auditViews(registry) {
  const byId = new Map(registry.ACTIONS.map((a) => [a.id, a]));
  const missing = AUDIT_VIEWS.filter((id) => !byId.has(id));
  if (missing.length) throw new Error(`AUDIT_VIEWS lists actions the app no longer registers: ${missing.join(', ')}`);
  return AUDIT_VIEWS.map((id) => ({ id, title: byId.get(id).title, ...(AUDIT_BEFORE[id] ? { before: byId.get(AUDIT_BEFORE[id])?.title } : {}) }));
}

/** Registered actions that look like views but are neither audited nor skipped with a reason. */
export function unclassifiedViews(registry) {
  return registry.ACTIONS.filter((a) => looksLikeAView(a) && !AUDIT_VIEWS.includes(a.id) && !skipped(a.id)).map((a) => a.id);
}
