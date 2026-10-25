// The manager views' action bars and confirm copy (local #223, from the #221 audit). With
// Vim keys off (the new-install default) a pane keeps only the standard keys, so every
// VIEW_KEYS action that is not a standard key needs a visible button. Each bar entry names
// the slice action it runs (the same id the key resolves to), the TUI wording and the key,
// shown as "(a)" and dropped by the key-hints pass when the switch is off. Pure:
// tests/manager-actions.test.mjs pins the coverage against VIEW_KEYS and the copy.
import type { BoardSection, ManagerView } from './managers-model.js';

/** One button of a view's action bar. */
export interface BarAction {
  /** The slice action the button runs: the same id its key resolves to in VIEW_KEYS. */
  readonly action: string;
  /** The TUI wording. */
  readonly label: string;
  /** The VIEW_KEYS key that runs the same action. */
  readonly key: string;
  /** The key as the label shows it, when it differs from `key` ("yy" for the copy chord); "" shows none. */
  readonly shown?: string;
}

const a = (action: string, label: string, key: string, shown?: string): BarAction => (shown === undefined ? { action, label, key } : { action, label, key, shown });

/** A button label: "Appoint (a)", "Toggle (Space)", "Open (Enter)". */
export function barLabel(b: BarAction): string {
  if (b.shown === '') return b.label;
  const k = b.shown ?? (b.key === ' ' ? 'Space' : b.key);
  return `${b.label} (${k})`;
}

const BOARD_PAGES = [a('mgr-prev-page', 'Previous page', 'p'), a('mgr-next-page', 'Next page', 'n')];

/** The board's bar, per section (the board's own tabs switch sections). */
export const BOARD_BARS: Readonly<Record<BoardSection, readonly BarAction[]>> = {
  board: [
    a('mgr-enter', 'Open', 'Enter'), a('mgr-answer', 'Answer…', 'a'), a('mgr-accept-recommended', 'Accept recommendation', 'y'),
    a('mgr-open-session', 'Open session', 'o'),
  ],
  decisions: [
    a('mgr-enter', 'Send highlighted option', 'Enter'), a('mgr-answer', 'Answer…', 'a'), a('mgr-accept-recommended', 'Accept recommendation', 'y'),
    a('mgr-archive-stale', 'Archive stale…', 'X'), a('mgr-open-session', 'Open session', 'o'), ...BOARD_PAGES,
  ],
  inbox: [a('mgr-open-session', 'Open session', 'o'), ...BOARD_PAGES],
  inspect: [a('mgr-sub-prev', 'Previous view', '['), a('mgr-sub-next', 'Next view', ']'), a('mgr-open-session', 'Open session', 'o'), ...BOARD_PAGES],
  policy: [a('mgr-space', 'Edit field', ' '), a('mgr-save', 'Save policy', 's'), a('mgr-refresh', 'Discard changes', 'r')],
};

/** Every other view's bar. */
export const VIEW_BARS: Readonly<Record<Exclude<ManagerView, 'board'>, readonly BarAction[]>> = {
  tree: [
    a('mgr-enter', 'Open', 'Enter'), a('mgr-toggle-fold', 'Fold / unfold', ' '), a('mgr-fold-all', 'Fold all', 'H'), a('mgr-unfold-all', 'Unfold all', 'L'),
    a('mgr-appoint', 'Appoint', 'a'), a('mgr-above', 'Appoint above', 'A'), a('mgr-edit', 'Edit', 'e'), a('mgr-revoke', 'Revoke', 'x'),
    a('mgr-move', 'Move', 'm'), a('mgr-preview', 'Preview', 'p'), a('mgr-open-project', 'Open project', 'o'), a('mgr-next-page', 'Load more', 'n'),
  ],
  scope: [a('mgr-toggle', 'Toggle', ' '), a('mgr-whole-project', 'Whole project', 'a'), a('mgr-filter', 'Filter…', '/'), a('mgr-save', 'Save', 'Enter')],
  workspace: [
    a('mgr-enter', 'Open', 'Enter'), a('mgr-type', 'Type to this seat…', 'i'), a('mgr-talk', 'Talk to the seat…', 't'), a('mgr-status', 'Status', 's'),
    a('mgr-halt', 'Halt', 'x'), a('mgr-halt-now', 'Interrupt now…', 'X'), a('mgr-archive-seat', 'Archive earlier seat…', 'a'),
    a('mgr-launch', 'Launch and appoint…', 'n'), a('mgr-conversation', 'Conversation', 'Tab'), a('mgr-parent', 'Up a level', 'Backspace'),
    a('mgr-open-project', 'Open project', 'p'), a('mgr-budget', 'Budget', 'b'), a('mgr-tree', 'Manager tree', 'T'), a('mgr-fleet', 'Fleet', 'F'),
  ],
  budget: [a('mgr-raise', 'Raise one step', '+'), a('mgr-type-limit', 'Set a limit…', 'Enter')],
  fleet: [
    a('mgr-enter', 'Open', 'Enter'), a('mgr-filter', 'Filter…', '/'), a('mgr-focus', 'Agents / usage', 'Tab'), a('mgr-sort', 'Sort…', 's'),
    a('mgr-reverse', 'Reverse', 'S'), a('mgr-dimension', 'Group', 'b'), a('mgr-window', 'Window', 'w'), a('mgr-copy', 'Copy session ID', 'y', 'yy'),
  ],
  lists: [a('mgr-open-session', 'Open session', 'o')],
};

/** The bar a view shows (the board's depends on its section). */
export function barFor(view: ManagerView, section: BoardSection): readonly BarAction[] {
  return view === 'board' ? BOARD_BARS[section] : VIEW_BARS[view];
}

/**
 * Actions a bar need not offer: moving the cursor (arrows, or a click on the row), paging and
 * the detail scroll (PageUp/PageDown), the option highlight (arrows, or a click on the option),
 * the board sections (its tabs), the fleet's sort columns (the Sort… button lists them as
 * buttons), h/l fold-or-parent (Fold / unfold, and a click on the parent) and the header's
 * Refresh and Close buttons.
 */
export const NAV_ACTIONS: ReadonlySet<string> = new Set([
  'mgr-down', 'mgr-up', 'mgr-first', 'mgr-last', 'mgr-page-down', 'mgr-page-up', 'mgr-detail-down', 'mgr-detail-up',
  'mgr-option-prev', 'mgr-option-next', 'mgr-section-1', 'mgr-section-2', 'mgr-section-3', 'mgr-section-4', 'mgr-section-5',
  'mgr-section-next', 'mgr-section-prev', 'mgr-sort-1', 'mgr-sort-2', 'mgr-sort-3', 'mgr-sort-4', 'mgr-sort-5',
  'mgr-fold', 'mgr-unfold', 'mgr-refresh', 'mgr-close',
]);

// ---------- what a manager is ----------

/** One line on what a manager is (the keyhelp Getting started row and the tree and board headers). */
export const MANAGER_EXPLAINER =
  'A session appointed to look after a project: it launches workers, reviews what they hand back and lands the accepted work. You talk to it like any session.';

// ---------- confirm copy: plain words first, the RPC and versions under Details ----------

/** A confirm dialog's words: the title, plain-words lines and the technical Details (collapsed). */
export interface ConfirmCopy {
  readonly title: string;
  readonly lines: readonly string[];
  readonly details: string;
}

const short = (id: string | null | undefined): string => (id ? id.slice(0, 8) : 'none');
const KEEPS_SESSION = 'Its session keeps running but loses its authority.';

/** Revoke a project manager from the tree (an empty selection is the daemon's revoke). */
export function revokeProjectCopy(label: string, impact: string, rowVersion: number): ConfirmCopy {
  return {
    title: `Stop the manager of ${label}?`,
    lines: [`Stop this manager from supervising ${label}. ${KEEPS_SESSION}`, impact],
    details: `ConfigureHarnessManager with an empty selection (expects scope v${rowVersion})`,
  };
}

/** Clear the scope of a project's manager (:manager clear). */
export function clearScopeCopy(project: string, rowVersion: number): ConfirmCopy {
  return {
    title: `Stop the manager supervising ${project}?`,
    lines: [`Stop this manager from supervising ${project}. ${KEEPS_SESSION}`],
    details: `ConfigureHarnessManager with an empty selection (expects scope v${rowVersion})`,
  };
}

/** Revoke the global manager: from the tree (with the rows below it) or the command. */
export function revokeGlobalCopy(grantVersion: number, seatSessionId: string | null, impact?: string): ConfirmCopy {
  return {
    title: 'Revoke the global manager?',
    lines: [`Stop the global manager from supervising every project it covers. ${KEEPS_SESSION}`, ...(impact ? [impact] : [])],
    details: `RevokeGlobalManager (expects grant v${grantVersion}${seatSessionId ? ` · seat session ${seatSessionId}` : ''})`,
  };
}

/** Appoint (or replace) a project's manager from the tree. */
export function appointProjectCopy(label: string, current: string | null, candidate: string, wholeProject: boolean, rowVersion: number): ConfirmCopy {
  return {
    title: current ? `Replace the manager of ${label}?` : `Appoint a manager for ${label}?`,
    lines: [
      current
        ? `The selected session ${short(candidate)} becomes the manager of ${label}, replacing ${short(current)}.`
        : `The selected session ${short(candidate)} becomes the manager of ${label}.`,
      wholeProject ? 'It looks after the whole project.' : 'It keeps the Groups and Epics the current manager looks after.',
    ],
    details: `ConfigureHarnessManager (expects row v${rowVersion})`,
  };
}

/** The empty portfolio list: what a portfolio node is, then how to make one. */
export const NO_PORTFOLIO_NODES =
  'No portfolio nodes yet. A portfolio node is a manager that looks after several projects; to add one, select the session to appoint and choose “Appoint a portfolio manager…” in Actions (or type :manager portfolio appoint <label> [projects...]).';
