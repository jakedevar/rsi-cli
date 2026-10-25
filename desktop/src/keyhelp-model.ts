// The keybindings help overlay's pure model (no DOM; tests/keyhelp.test.mjs).
// It is generated from the same sources the key engine runs on: the TUI
// keymap (keymap.generated.ts, from docs/keybindings.md), the desktop's key
// and `:` command bindings (keybindings.ts, core plus every slice) and the
// action registry (registry.ts) with its availability rules.
//
// Views, as in the TUI's help (Tab / Shift-Tab cycle them):
// - Contextual: the desktop actions that can run now, with their keys, by menu.
// - All Commands: every TUI Normal-mode key and `:` command, with what the
//   desktop does with it (the action it runs, or "not in the desktop yet").
// Search is case-insensitive and needs every space-separated term to match the
// action name, description, keys or aliases.
import { COMMAND_BINDINGS, KEY_BINDINGS } from './keybindings.js';
import type { Platform } from './keyengine.js';
import { MANAGER_EXPLAINER } from './manager-actions-model.js';
import { PROJECT_EXPLAINER } from './onboarding-model.js';
import { COMMANDS, NORMAL_KEYS } from './keymap.generated.js';
import { OVERLAY_TABLES, type OverlayKeyRow, type OverlayTable } from './overlay-help.generated.js';
import { formatKeyTip, isVimOnlyHint } from './key-hints.js';
import { type MenuGroup, type UiSnapshot, actionSpec, displayHint, isActionId, paletteEntries } from './registry.js';

// Gap sweep G4: a third view, Symbols (`s`), and the overlay key tables (the
// TUI's OVERLAY_HELP_ROUTES, generated into overlay-help.generated.ts).
export type HelpView = 'contextual' | 'all' | 'symbols' | 'start';
export const HELP_VIEWS: readonly HelpView[] = ['contextual', 'all', 'symbols', 'start'];
export const VIEW_TITLES: Readonly<Record<HelpView, string>> = { contextual: 'Contextual', all: 'All Commands', symbols: 'Symbols', start: 'Getting started' };

export interface HelpRow {
  /** The heading it is listed under. */
  section: string;
  keys: string[];
  title: string;
  detail: string;
  /** False for a TUI key the desktop does not run yet. */
  bound: boolean;
  /** Everyday words the search also matches (the palette's, local #227). */
  keywords?: readonly string[];
}

const GROUP_TITLES: Readonly<Record<MenuGroup, string>> = {
  file: 'File', session: 'Session', go: 'Go', issues: 'Issues', view: 'View', help: 'Help',
};

/** The actions that can run in the captured snapshot, grouped by menu, with their keys. */
export function contextualRows(s: UiSnapshot, platform: Platform): HelpRow[] {
  const order: MenuGroup[] = ['file', 'session', 'go', 'issues', 'view', 'help'];
  const entries = paletteEntries(s, platform).filter((e) => e.enabled);
  return order.flatMap((g) => entries.filter((e) => e.group === g).map((e) => ({
    section: GROUP_TITLES[g], keys: e.keys, title: e.title, detail: e.detail ?? '', bound: true, keywords: e.keywords,
  })));
}

function desktopTitle(action: string): string {
  return isActionId(action) ? actionSpec(action).title : action;
}

/** Every TUI Normal-mode key and `:` command with the desktop's answer to it. */
export function allRows(): HelpRow[] {
  const keys: HelpRow[] = NORMAL_KEYS.map((k) => {
    const b = KEY_BINDINGS[k.keys];
    return {
      section: 'Normal-mode keys', keys: [k.keys], title: k.title,
      detail: b ? `${k.description} Desktop: ${desktopTitle(b.action)}.` : `${k.description} Not in the desktop yet.`,
      bound: !!b,
    };
  });
  const cmds: HelpRow[] = COMMANDS.map((c) => {
    const b = COMMAND_BINDINGS[c.name];
    const spell = [`:${c.name}`, ...c.aliases.map((a) => `:${a}`)];
    return {
      section: 'Commands', keys: spell, title: c.title,
      detail: b ? `${c.description} Desktop: ${desktopTitle(b.action)}.` : `${c.description} Not in the desktop yet.`,
      bound: !!b,
    };
  });
  return [...keys, ...cmds];
}

/**
 * Rows where the desktop differs from the TUI's generated overlay table (#340): table name, then the
 * generated row's keys, then the rows that stand in its place. The generated file is never edited.
 */
export const DESKTOP_OVERLAY_ROWS: Readonly<Record<string, Readonly<Record<string, readonly OverlayKeyRow[]>>>> = {
  // The desktop's large editor is a text box: Enter is a new line (insert mode; nothing in normal mode)
  // and only Ctrl-Enter (Cmd-Enter on macOS) sends, as the dialog's own hint says.
  'Input Modal': {
    'Enter / Ctrl-Enter': [
      { keys: 'Ctrl-Enter / Cmd-Enter', action: 'Send message', detail: '' },
      { keys: 'Enter', action: 'New line (insert mode); nothing in normal mode', detail: '' },
    ],
  },
};

/** An overlay table's rows as the desktop has them: the TUI's, with the desktop's differences swapped in. */
function desktopTableRows(t: OverlayTable): readonly OverlayKeyRow[] {
  const swaps = DESKTOP_OVERLAY_ROWS[t.name];
  return swaps ? t.rows.flatMap((r) => swaps[r.keys] ?? [r]) : t.rows;
}

/**
 * An overlay's own keys (the TUI's generated overlay tables, with the desktop's differences), listed under
 * its name. They describe the overlay's handler; the desktop runs them inside it.
 */
export function overlayRows(names: readonly string[]): HelpRow[] {
  return names.flatMap((name) => {
    const t = OVERLAY_TABLES.find((x) => x.name === name);
    return t ? desktopTableRows(t).map((r) => ({ section: `${name} (overlay keys)`, keys: [r.keys], title: r.action, detail: r.detail, bound: true })) : [];
  });
}

/** Every overlay table, for All Commands. */
export function allOverlayRows(): HelpRow[] {
  return overlayRows(OVERLAY_TABLES.map((t) => t.name)).map((r) => ({ ...r, section: `Overlay: ${r.section.replace(/ \(overlay keys\)$/, '')}` }));
}

/** The symbols the desktop draws, by where they appear (the TUI's Symbols legend, ui/glyphs.rs). */
export const SYMBOLS: ReadonlyArray<readonly [string, string, string, string]> = [
  // [location, glyph, name, explanation]
  ['Session list', '●', 'Status dot', 'The session status by colour: running, waiting for you, completed, failed, interrupted.'],
  ['Session list', '▸ ▾', 'Fold', 'A Group or Epic with children, folded or open (h / l, zo / zc).'],
  ['Session list', '◆', 'Pinned', 'The session is pinned to the top (P toggles).'],
  ['Session list', '☐', 'Testing needed', 'Marked for manual testing (<Space>t clears).'],
  ['Session list', '⊘', 'Rotation off', 'Automatic context rotation is disabled (<Space>r enables).'],
  ['Session list', '↻N/M', 'Retry budget', 'Automatic retries used of the budget (<Space>k cancels).'],
  ['Session list', '↺N', 'Rotation depth', 'How many times the context was rotated into a new session.'],
  ['Session list', '⇅', 'Sort', 'The list order and the visible session count (<Space>s changes it).'],
  ['Pickers', '✓', 'Current', 'The current choice (sort, label, project, theme, model).'],
  ['Pickers', '1-9', 'Direct pick', 'The number that picks the row (model dropdown).'],
  ['Transcript', '▸ tool', 'Tool call', 'A tool the agent called; click or zo to open its input.'],
  ['Transcript', '◂ result', 'Tool result', 'What the tool returned.'],
  ['Transcript', '⧗ running', 'Pending call', 'A call whose result has not arrived (still running, interrupted or failed).'],
  ['Transcript', '✓ answered', 'Answered call', 'A call whose result arrived (paired by tool_use_id).'],
  ['Transcript', 'N parallel calls', 'Batch', 'Consecutive calls of one turn; the first carries the batch size and how many still run.'],
  ['Transcript', '↳ tool #N', 'Result of', 'The result answers call #N, however far above it is.'],
  ['Transcript', '… thinking', 'Thinking', 'The model’s reasoning block.'],
  ['File explorer', '▸ ▾', 'Folder', 'A closed or open folder.'],
  ['File explorer', '↪', 'Symlink', 'A symbolic link.'],
  ['File viewer', '+', 'Added', 'The line is not in HEAD (git gutter, green).'],
  ['File viewer', '~', 'Modified', 'The line differs from HEAD (git gutter, yellow).'],
  ['File viewer', '-', 'Deleted', 'Lines were removed after this line (git gutter, red).'],
  ['File viewer', '▸', 'Closed fold', 'A folded region starts here; ··· N lines follows the text (za opens).'],
  ['File viewer', '▾', 'Foldable', 'A region that can fold starts here (za or zc closes it).'],
  ['Settings', '━━● ON / ○── OFF', 'Switch', 'A boolean setting (Space toggles).'],
  ['Settings', '◂ value ▸', 'Selector', 'A choice (h / l or + / - step it).'],
  ['Settings', '●', 'Staged', 'A change waiting for w (apply) or Esc (discard).'],
  ['Settings', '↻', 'Restart', 'The change applies after a daemon restart.'],
  ['New session prompt', '●○', 'Effort', 'The effort level along the model’s ladder (Ctrl-E steps it).'],
  ['New session prompt', 'sandbox', 'Sandbox', 'The launch runs in its own git worktree (Ctrl-B toggles).'],
  ['New session prompt', '★ manager', 'Manager', 'The launched session will be appointed the project manager, with the named policy.'],
];

export function symbolRows(): HelpRow[] {
  return SYMBOLS.map(([section, glyph, title, detail]) => ({ section, keys: [glyph], title, detail, bound: true }));
}

/**
 * The Getting started view (local #218, from #214): what a project, a session and a manager
 * are, then the first three things to do. Plain words; the keys are the secondary column and
 * follow the platform (Cmd on macOS).
 */
export function gettingStartedRows(platform: Platform): HelpRow[] {
  const key = (k: string): string => displayHint(k, platform);
  const row = (section: string, title: string, detail: string, keys: string[] = []): HelpRow => ({ section, keys, title, detail, bound: true });
  return [
    row('What these are', 'Project', PROJECT_EXPLAINER),
    row('What these are', 'Session', 'One conversation with an AI coding agent. It runs in the daemon (rsid), so it keeps going when you close this window.'),
    row('What these are', 'Manager', MANAGER_EXPLAINER),
    row('First three things to do', '1. Create a project', 'Pick the folder with your code (Actions… › New project) so new sessions start in the right place. You can also start a session without one.', [key('Ctrl-K')]),
    row('First three things to do', '2. Start a session', 'Click New session, describe the task, choose a provider and send.', [key('Ctrl-N')]),
    row('First three things to do', '3. Find any action', 'Open the Actions… palette and type a few letters of what you want. Every feature is also in the menu bar.', [key('Ctrl-K')]),
    // Local #242: say how to reach the buttons from the keyboard; on a Mac, Option-Tab is the system's own way.
    row('Keyboard', 'Move between controls', `With Vim keys off, Tab moves between buttons, links and fields and Shift-Tab goes back; Enter or Space presses the focused button. With Vim keys on, Tab switches panes.${platform === 'mac' ? ' Option-Tab also works in other Mac apps.' : ''}`, ['Tab']),
    row('Vim keys', 'Vim keys switch', 'The Vim keys switch is in the top bar (and in Settings › Workspace › Editing Mode). On: single letters such as j, k and a act on the app while you are not typing in a text field. Off: use the buttons and menus; Enter, Esc and the Actions… palette still work.'),
  ];
}

/** Every row of `view` that matches all of the query's terms. */
export function filterRows(rows: readonly HelpRow[], query: string): HelpRow[] {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return [...rows];
  return rows.filter((r) => {
    const hay = [r.section, r.title, r.detail, ...r.keys, ...(r.keywords ?? [])].join(' ').toLowerCase();
    return terms.every((t) => hay.includes(t));
  });
}

/**
 * The key tables for Vim keys off (local #227): a row that only a Vim key or a `:` command runs
 * goes, a row with a working key (Ctrl-K, Enter, F2) keeps just that key, and the detail loses
 * its Vim key mentions. `keepRowsWithoutKeys` is for Contextual, whose rows are actions you can
 * also pick in Actions… (the action stays, its keys are the secondary column).
 */
export function plainRows(rows: readonly HelpRow[], vimOn: boolean, keepRowsWithoutKeys = false): HelpRow[] {
  if (vimOn) return [...rows];
  return rows.flatMap((r) => {
    const keys = r.keys.filter((k) => !isVimOnlyHint(k));
    if (keys.length === 0 && r.keys.length > 0 && !keepRowsWithoutKeys) return [];
    return [{ ...r, keys, detail: formatKeyTip(r.detail) }];
  });
}

/** The view Help opens on: Getting started with Vim keys off or the first time (local #227). */
export function initialHelp(vimOn: boolean, seen: boolean): HelpState {
  return { ...INITIAL_HELP, view: vimOn && seen ? 'contextual' : 'start' };
}

/** The Help footer: the Vim keys only while the switch is on; Ctrl-K follows the platform. */
export function helpFooter(vimOn: boolean, platform: Platform): string {
  const palette = `${displayHint('Ctrl-K', platform)} opens Actions…`;
  return vimOn
    ? `Tab views · s symbols · / search · j/k scroll · Ctrl-d/u half page · g/G ends · Esc clears or closes · q closes · ${palette}`
    : `Type to search · click a view above · Esc clears the search or closes · ${palette}`;
}

// ---------- keys ----------

export interface HelpState {
  view: HelpView;
  query: string;
  /** Typing into the search line (after `/`). */
  searching: boolean;
  /** First visible row. */
  scroll: number;
}

export const INITIAL_HELP: HelpState = { view: 'contextual', query: '', searching: false, scroll: 0 };

export type HelpEffect = { kind: 'state'; st: HelpState } | { kind: 'close' } | { kind: 'none' };

export interface HelpKey { key: string; ctrl?: boolean; shift?: boolean; alt?: boolean; meta?: boolean }

const clamp = (n: number, total: number) => Math.max(0, Math.min(Math.max(0, total - 1), n));

/** One key in the help overlay; `total` is the number of rows the view shows now. */
export function helpKey(st: HelpState, k: HelpKey, total: number): HelpEffect {
  const set = (patch: Partial<HelpState>): HelpEffect => ({ kind: 'state', st: { ...st, ...patch } });
  if (st.searching) {
    if (k.key === 'Escape') return set({ searching: false, query: '', scroll: 0 });
    if (k.key === 'Enter') return set({ searching: false });
    if (k.key === 'Backspace') return set({ query: st.query.slice(0, -1), scroll: 0 });
    if (k.key.length === 1 && !k.ctrl && !k.meta && !k.alt) return set({ query: st.query + k.key, scroll: 0 });
    return { kind: 'none' };
  }
  if (k.ctrl) {
    switch (k.key.toLowerCase()) {
      case 'd': return set({ scroll: clamp(st.scroll + 15, total) });
      case 'u': return set({ scroll: clamp(st.scroll - 15, total) });
      case 'f': return set({ scroll: clamp(st.scroll + 30, total) });
      case 'b': return set({ scroll: clamp(st.scroll - 30, total) });
      default: return { kind: 'none' };
    }
  }
  switch (k.key) {
    case 'j': case 'ArrowDown': return set({ scroll: clamp(st.scroll + 1, total) });
    case 'k': case 'ArrowUp': return set({ scroll: clamp(st.scroll - 1, total) });
    case 'g': case 'Home': return set({ scroll: 0 });
    case 'G': case 'End': return set({ scroll: clamp(total - 1, total) });
    case 'PageDown': return set({ scroll: clamp(st.scroll + 30, total) });
    case 'PageUp': return set({ scroll: clamp(st.scroll - 30, total) });
    case 'Tab': {
      const i = HELP_VIEWS.indexOf(st.view);
      const n = HELP_VIEWS.length;
      return set({ view: HELP_VIEWS[(i + (k.shift ? n - 1 : 1)) % n]!, scroll: 0 });
    }
    case '/': return set({ searching: true });
    case 's': return set({ view: 'symbols', scroll: 0 });
    case 'Escape': return st.query ? set({ query: '', scroll: 0 }) : { kind: 'close' };
    case 'q': case '?': return { kind: 'close' };
    default: return { kind: 'none' };
  }
}
