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
import { COMMANDS, NORMAL_KEYS } from './keymap.generated.js';
import { OVERLAY_TABLES } from './overlay-help.generated.js';
import { type MenuGroup, type UiSnapshot, actionSpec, isActionId, paletteEntries } from './registry.js';

// Gap sweep G4: a third view, Symbols (`s`), and the overlay key tables (the
// TUI's OVERLAY_HELP_ROUTES, generated into overlay-help.generated.ts).
export type HelpView = 'contextual' | 'all' | 'symbols';
export const HELP_VIEWS: readonly HelpView[] = ['contextual', 'all', 'symbols'];
export const VIEW_TITLES: Readonly<Record<HelpView, string>> = { contextual: 'Contextual', all: 'All Commands', symbols: 'Symbols' };

export interface HelpRow {
  /** The heading it is listed under. */
  section: string;
  keys: string[];
  title: string;
  detail: string;
  /** False for a TUI key the desktop does not run yet. */
  bound: boolean;
}

const GROUP_TITLES: Readonly<Record<MenuGroup, string>> = {
  file: 'File', session: 'Session', go: 'Go', issues: 'Issues', view: 'View', help: 'Help',
};

/** The actions that can run in the captured snapshot, grouped by menu, with their keys. */
export function contextualRows(s: UiSnapshot, platform: Platform): HelpRow[] {
  const order: MenuGroup[] = ['file', 'session', 'go', 'issues', 'view', 'help'];
  const entries = paletteEntries(s, platform).filter((e) => e.enabled);
  return order.flatMap((g) => entries.filter((e) => e.group === g).map((e) => ({
    section: GROUP_TITLES[g], keys: e.keys, title: e.title, detail: e.detail ?? '', bound: true,
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
 * An overlay's own keys (the TUI's generated overlay tables), listed under its
 * name. They describe the overlay's handler; the desktop runs them inside it.
 */
export function overlayRows(names: readonly string[]): HelpRow[] {
  return names.flatMap((name) => {
    const t = OVERLAY_TABLES.find((x) => x.name === name);
    return t ? t.rows.map((r) => ({ section: `${name} (overlay keys)`, keys: [r.keys], title: r.action, detail: r.detail, bound: true })) : [];
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
  ['New session prompt', '▮▯', 'Effort', 'The effort level along the model’s ladder (Ctrl-E steps it).'],
  ['New session prompt', '⊡ sandbox', 'Sandbox', 'The launch runs in its own git worktree (Ctrl-B toggles).'],
  ['New session prompt', '★ manager', 'Manager', 'The launched session will be appointed the project manager, with the named policy.'],
];

export function symbolRows(): HelpRow[] {
  return SYMBOLS.map(([section, glyph, title, detail]) => ({ section, keys: [glyph], title, detail, bound: true }));
}

/** Every row of `view` that matches all of the query's terms. */
export function filterRows(rows: readonly HelpRow[], query: string): HelpRow[] {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return [...rows];
  return rows.filter((r) => {
    const hay = [r.section, r.title, r.detail, ...r.keys].join(' ').toLowerCase();
    return terms.every((t) => hay.includes(t));
  });
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
