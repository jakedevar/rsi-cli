// The one action list (pure, no DOM). The vim key engine, the command palette,
// the app menu bar, the right-click menus and the button tooltips all read it:
// a feature added here shows up everywhere at once. Key hints come from the
// slice-2 keymap (keybindings.ts), so the TUI's notation is the only source.
// Unit-tested under tests/registry.test.mjs; commands.ts holds the DOM `run`
// side, palette.ts and menus.ts are thin adapters.
//
// Slices register without editing this file: each src/actions/*.ts module that
// exports `SLICE` (a SliceSpec) is collected by the generated
// actions/index.generated.ts (scripts/slices.mjs); see desktop/README.md.
import { SLICES, type SliceActionId } from './actions/index.generated.js';
import { COMMAND_BINDINGS, CORE_COMMAND_BINDINGS, CORE_KEY_BINDINGS, KEY_BINDINGS } from './keybindings.js';
import { type DesktopBinding, type KeyEventLike, type Platform, VIM_KEYS_TIP } from './keyengine.js';
import { ACTION_KEYWORDS } from './action-keywords.js';
import { formatKeyTip, isVimOnlyHint } from './key-hints.js';
import { hintLabel } from './keylabel.js';
import { isActive } from './model.js';
import { PAUSE_BUTTON, RESUME_BUTTON, STOP_BUTTON } from './session-head-model.js';

export type CoreActionId =
  | 'cursor-down' | 'cursor-up' | 'cursor-first' | 'cursor-last' | 'open'
  | 'next-attention' | 'prev-attention' | 'focus-search'
  | 'refresh' | 'launch' | 'quit'
  | 'view-active' | 'view-archived' | 'toggle-finished' | 'toggle-tools' | 'toggle-vim-keys'
  | 'palette' | 'command-line' | 'help'
  | 'soft-interrupt' | 'hard-pause' | 'clear-pause' | 'archive'
  | 'quick-continue' | 'continue' | 'focus-composer' | 'answer-question' | 'copy-session-id'
  | 'copy-event' | 'toggle-event';
export type ActionId = CoreActionId | SliceActionId;

export type MenuGroup = 'file' | 'session' | 'go' | 'issues' | 'view' | 'help';
export type ContextKind = 'session-row' | 'event' | 'issue-row';

/** What the actions can see of the UI (built by commands.ts from the DOM). */
export interface UiSnapshot {
  /** The open session (where session actions apply), or null. */
  open: {
    status: string;
    /** Archived view or an archived session: no composer, no interrupt. */
    readOnly: boolean;
    pauseLevel: 'none' | 'soft' | 'hard';
    hasQuestion: boolean;
  } | null;
  /** The list cursor is on a visible row. */
  cursorOnRow: boolean;
  rowCount: number;
  /** Visible rows that need the operator. */
  attentionRows: number;
  view: 'active' | 'archived';
  vimKeys: boolean;
  hideTools: boolean;
  showFinished: boolean;
  /** A modal dialog other than the palette is open (launch, confirm). */
  modalOpen: boolean;
  /** The right-clicked conversation event, for its context menu. */
  event?: { foldable: boolean; hasText: boolean } | null;
  // A slice adds its own optional field from its module with
  // `declare module '../registry.js' { interface UiSnapshot { mine?: MySnapshot } }`
  // and fills it from its RUNTIME.snapshot().
}

/** Null when the action can run now, else the reason it cannot. */
export type Rule = (s: UiSnapshot) => string | null;

/**
 * One slice's registry entries (pure, no DOM), exported as `SLICE` from a
 * src/actions/*.ts module. `Id` is the union of the action ids it adds.
 */
export interface SliceSpec<Id extends string = string> {
  /** Shown in registration errors, e.g. "detail (slice 5, hub #3007)". */
  name: string;
  /** Registry position: lower first in menus, the palette and snapshots. */
  order: number;
  /** In menu order within each group. */
  actions: readonly ActionSpec[];
  /** One rule per action it adds (a rule may also take over a core action's). */
  rules: Readonly<Partial<Record<ActionId, Rule>>>;
  /** TUI Normal-mode keys (keybindings.md notation). */
  keys?: Readonly<Record<string, DesktopBinding<ActionId>>>;
  /** TUI `:` commands (canonical names). */
  commands?: Readonly<Record<string, DesktopBinding<ActionId>>>;
  /** `:` commands the TUI parses ahead of its command table (`pending`, `drain`); the rest of the line is the argument. */
  prefixCommands?: Readonly<Record<string, DesktopBinding<ActionId>>>;
  /**
   * Key tokens (keybindings.md notation, e.g. 'C-h', 'C-S-Left', 'C-\\') this slice
   * routes in its own raw window/document keydown listener instead of the
   * registry. sliceProblems() fails when two slices declare one token or a
   * declared token is bound by another slice's `keys`, and
   * tests/slices.test.mjs fails a slice that has such a listener but declares none.
   */
  rawKeys?: readonly string[];
  /** Type-only marker that carries `Id`; never set. */
  readonly ids?: Id;
}

export interface ActionSpec {
  id: ActionId;
  /** Menu, palette and tooltip text. No `&` (muda reads it as a mnemonic). */
  title: string;
  group: MenuGroup;
  /** One more line for the palette and the tooltip. */
  detail?: string;
  /** Desktop-only keys the TUI keymap does not have (the engine never sees them). */
  extraKeys?: readonly string[];
  /** Start a separated block in its menu. */
  menuBreak?: boolean;
  /** Not in the app menu bar (still in the palette). */
  noMenu?: boolean;
  /** A check item: the tick follows the snapshot. */
  checked?: (s: UiSnapshot) => boolean;
  /** Right-click menus it appears in. */
  contexts?: readonly ContextKind[];
  /** Needs a right-clicked target: only in context menus. */
  contextOnly?: boolean;
  /** Menu placement: right after this action in the same group (default: registry order). */
  menuAfter?: ActionId;
  /** Everyday words the palette also matches ("help", "files", "todo"); merged with action-keywords.ts (local #227). */
  keywords?: readonly string[];
  /** The palette's detail line with Vim keys off, when the generic key-stripping cannot make the detail read well. */
  plainDetail?: string;
}

/** The core actions (slices 1-3), in menu order within each group. */
const CORE_ACTIONS: readonly ActionSpec[] = [
  // File
  { id: 'launch', title: 'New session…', group: 'file', detail: 'Open the launch dialog' },
  { id: 'refresh', title: 'Refresh', group: 'file', menuBreak: true, detail: 'Reload health, projects and sessions' },
  { id: 'quit', title: 'Quit RSI Desktop', group: 'file', menuBreak: true },
  // Session (acts on the open session)
  // Plain words first (local #212); the TUI terms (interrupt, hard pause, clear pause) stay in the keys and `:` commands.
  { id: 'soft-interrupt', title: STOP_BUTTON.label, group: 'session', detail: STOP_BUTTON.help, contexts: ['session-row'] },
  { id: 'hard-pause', title: PAUSE_BUTTON.label, group: 'session', detail: PAUSE_BUTTON.help, contexts: ['session-row'] },
  { id: 'clear-pause', title: RESUME_BUTTON.label, group: 'session', detail: RESUME_BUTTON.help, contexts: ['session-row'] },
  { id: 'archive', title: 'Archive session…', group: 'session', menuBreak: true, contexts: ['session-row'] },
  { id: 'quick-continue', title: 'Send “continue”', group: 'session', menuBreak: true, contexts: ['session-row'] },
  { id: 'continue', title: 'Continue with a message', group: 'session', detail: ':continue <text> sends it directly' },
  { id: 'focus-composer', title: 'Message this session', group: 'session', detail: 'Focus the message box', contexts: ['session-row', 'event'] },
  { id: 'answer-question', title: 'Answer waiting question', group: 'session', contexts: ['session-row'] },
  { id: 'copy-session-id', title: 'Copy session ID', group: 'session', menuBreak: true, contexts: ['session-row'] },
  // Go (the session list cursor)
  { id: 'cursor-down', title: 'Next session', group: 'go', detail: 'Move the list cursor down' },
  { id: 'cursor-up', title: 'Previous session', group: 'go', detail: 'Move the list cursor up' },
  { id: 'cursor-first', title: 'First session', group: 'go', detail: 'With a count N: row N' },
  { id: 'cursor-last', title: 'Last session', group: 'go', detail: 'With a count N: row N' },
  { id: 'open', title: 'Open session under cursor', group: 'go' },
  { id: 'next-attention', title: 'Next session needing you', group: 'go', menuBreak: true },
  { id: 'prev-attention', title: 'Previous session needing you', group: 'go' },
  { id: 'focus-search', title: 'Filter sessions', group: 'go', menuBreak: true },
  // View
  { id: 'view-active', title: 'Active sessions', group: 'view', checked: (s) => s.view === 'active' },
  { id: 'view-archived', title: 'Archived sessions', group: 'view', checked: (s) => s.view === 'archived' },
  { id: 'toggle-finished', title: 'Show completed sessions', group: 'view', checked: (s) => s.showFinished },
  { id: 'toggle-tools', title: 'Show tool calls', group: 'view', menuBreak: true, checked: (s) => !s.hideTools, contexts: ['event'] },
  { id: 'toggle-vim-keys', title: 'Vim keys', group: 'view', checked: (s) => s.vimKeys, detail: VIM_KEYS_TIP },
  { id: 'palette', title: 'Command palette…', group: 'view', menuBreak: true, extraKeys: ['Ctrl-K'], detail: 'Search every action' },
  { id: 'command-line', title: 'Command line', group: 'view', extraKeys: [':'], detail: 'Type a TUI : command (needs Vim keys)' },
  // Help
  { id: 'help', title: 'Keyboard shortcuts', group: 'help', detail: 'Lists every action and its keys; type to search' },
  // Conversation event (right-click only)
  { id: 'copy-event', title: 'Copy message text', group: 'session', contextOnly: true, contexts: ['event'] },
  { id: 'toggle-event', title: 'Expand or collapse', group: 'session', contextOnly: true, contexts: ['event'] },
];

/** In menu order within each group: the core actions, then each slice's in SLICES order. */
export const ACTIONS: readonly ActionSpec[] = [...CORE_ACTIONS, ...SLICES.flatMap((s) => s.actions)];

const BY_ID: ReadonlyMap<ActionId, ActionSpec> = new Map(ACTIONS.map((a) => [a.id, a]));

export function actionSpec(id: ActionId): ActionSpec {
  const a = BY_ID.get(id);
  if (!a) throw new Error(`unknown action ${id}`);
  return a;
}

export function isActionId(id: string): id is ActionId {
  return BY_ID.has(id as ActionId);
}

// ---------- keys ----------

/** Every key that runs the action: desktop-only keys, TUI keys, then `:` commands. */
export function keyHints(id: ActionId): string[] {
  const keys = Object.entries(KEY_BINDINGS).filter(([, b]) => b.action === id).map(([k]) => k);
  const cmds = Object.entries(COMMAND_BINDINGS).filter(([, b]) => b.action === id).map(([n]) => `:${n}`);
  return [...(BY_ID.get(id)?.extraKeys ?? []), ...keys, ...cmds];
}

/** A hint as the operator presses it: on macOS Cmd stands in for Ctrl. */
export function displayHint(key: string, platform: Platform): string {
  return hintLabel(key, platform);
}

/** The native accelerator for a plain Ctrl-<letter, digit or comma> hint (CmdOrCtrl+N, CmdOrCtrl+, for Settings). */
export function accelerator(id: ActionId): string | undefined {
  for (const k of keyHints(id)) {
    const m = k.match(/^Ctrl-([A-Z0-9,])$/);
    if (m) return `CmdOrCtrl+${m[1]}`;
  }
  return undefined;
}

/** Button tooltip: title, keys, detail. */
export function tooltipText(id: ActionId, platform: Platform): string {
  const a = actionSpec(id);
  const keys = keyHints(id).map((k) => displayHint(k, platform));
  const head = keys.length > 0 ? `${a.title} (${keys.join(', ')})` : a.title;
  return a.detail ? `${head}\n${a.detail}` : head;
}

/**
 * The palette chord: Cmd-K on macOS (also from a text field; physical Ctrl-K
 * there stays the text fields' kill-line), Ctrl-K elsewhere. Works with the
 * Vim keys toggle off.
 */
export function isPaletteChord(ev: KeyEventLike, platform: Platform, typing: boolean): boolean {
  if (ev.isComposing || ev.altKey || ev.shiftKey) return false;
  const k = ev.key.toLowerCase() === 'k' || ev.code === 'KeyK';
  if (!k) return false;
  if (platform === 'mac') return (!!ev.metaKey && !ev.ctrlKey) || (!!ev.ctrlKey && !ev.metaKey && !typing);
  return !!ev.ctrlKey && !ev.metaKey;
}

/**
 * The Settings chord (local #269): Cmd-, on macOS and Ctrl-, elsewhere, as desktop apps do. It works with the
 * Vim keys off and from a text field (a comma with a modifier is never typed text); a shifted or Alt chord is not it.
 */
export function isSettingsChord(ev: KeyEventLike, platform: Platform): boolean {
  if (ev.isComposing || ev.altKey || ev.shiftKey) return false;
  if (ev.key !== ',' && ev.code !== 'Comma') return false;
  // macOS: Cmd, or the physical Ctrl key as the key engine reads it (a comma is no text-field chord there).
  if (platform === 'mac') return !!ev.metaKey !== !!ev.ctrlKey;
  return !!ev.ctrlKey && !ev.metaKey;
}

/**
 * The New session chord (#305): Cmd-N on macOS, Ctrl-N elsewhere, as the File menu shows it. It works with the
 * Vim keys off and from the message box. Physical Ctrl-N on macOS stays a text field's next-line key and the
 * pickers' move-down key; a shifted or Alt chord is not it.
 */
export function isNewSessionChord(ev: KeyEventLike, platform: Platform): boolean {
  if (ev.isComposing || ev.altKey || ev.shiftKey) return false;
  if (ev.key.toLowerCase() !== 'n' && ev.code !== 'KeyN') return false;
  if (platform === 'mac') return !!ev.metaKey && !ev.ctrlKey;
  return !!ev.ctrlKey && !ev.metaKey;
}

// ---------- availability ----------

const LIVE = new Set(['Starting', 'Running', 'WaitingApproval']);
const NEED_OPEN = 'Open a session first: click one in the list (j/k, then Enter)';

const needOpen = (more?: (o: NonNullable<UiSnapshot['open']>) => string | null): Rule =>
  (s) => (s.open ? (more ? more(s.open) : null) : NEED_OPEN);
const writable = (o: NonNullable<UiSnapshot['open']>) => (o.readOnly ? 'This session is read-only' : null);
const rows: Rule = (s) => (s.rowCount > 0 ? null : 'No sessions in the list');

const CORE_RULES: Readonly<Partial<Record<ActionId, Rule>>> = {
  'cursor-down': rows,
  'cursor-up': rows,
  'cursor-first': rows,
  'cursor-last': rows,
  open: (s) => (s.cursorOnRow ? null : 'No session under the cursor (j/k to move)'),
  'next-attention': (s) => (s.attentionRows > 0 ? null : 'No session is waiting for you'),
  'prev-attention': (s) => (s.attentionRows > 0 ? null : 'No session is waiting for you'),
  'focus-search': () => null,
  refresh: () => null,
  launch: () => null,
  quit: () => null,
  'view-active': () => null,
  'view-archived': () => null,
  'toggle-finished': (s) => (s.view === 'archived' ? 'The Archived view lists every archived session' : null),
  'toggle-tools': () => null,
  'toggle-vim-keys': () => null,
  palette: () => null,
  'command-line': (s) => (s.vimKeys ? null : 'Turn on Vim keys to use the : command line'),
  help: () => null,
  'soft-interrupt': needOpen((o) => writable(o) ?? (isActive(o) ? null : 'The open session is not running')),
  'hard-pause': needOpen((o) => (LIVE.has(o.status) ? null : 'The open session is not running')),
  'clear-pause': needOpen((o) => (o.pauseLevel !== 'none' ? null : 'The open session is not paused')),
  archive: needOpen((o) => (o.status === 'Archived' || o.status === 'Deleted' ? 'The open session is already archived' : null)),
  'quick-continue': needOpen(writable),
  continue: needOpen(writable),
  // 'focus-composer' is ruled in actions/input-spec.ts (i/a/o/O open the session under the cursor first).
  // 'answer-question' is ruled in actions/lifecycle-spec.ts (it opens the question modal).
  'copy-session-id': needOpen(),
  'copy-event': (s) => (s.event?.hasText ? null : 'This event has no text'),
  'toggle-event': (s) => (s.event?.foldable ? null : 'This event has nothing to fold'),
};

/** Every action's rule; sliceProblems() checks that each action has exactly one. */
const RULES: Readonly<Partial<Record<ActionId, Rule>>> = Object.assign({}, CORE_RULES, ...SLICES.map((s) => s.rules));

/**
 * What is wrong with the slice registrations, empty when nothing (tested in
 * tests/slices.test.mjs): an action id registered twice, an action without
 * a rule or with two, a rule for no action, and a key or `:` command that two
 * registrations bind to different actions or that overrides a core binding,
 * and a raw-routed key (`rawKeys`) that two slices declare or that another
 * slice binds in the registry.
 */
export function sliceProblems(slices: readonly SliceSpec[] = SLICES): string[] {
  const problems: string[] = [];
  const owner = new Map<string, string>(CORE_ACTIONS.map((a) => [a.id, 'core']));
  for (const s of slices) {
    for (const a of s.actions) {
      const was = owner.get(a.id);
      if (was) problems.push(`action ${a.id} is registered by both ${was} and ${s.name}`);
      else owner.set(a.id, s.name);
    }
  }
  const ruled = new Map<string, string>(Object.keys(CORE_RULES).map((id) => [id, 'core']));
  for (const s of slices) {
    for (const id of Object.keys(s.rules)) {
      const was = ruled.get(id);
      if (was) problems.push(`action ${id} has a rule in both ${was} and ${s.name}`);
      else ruled.set(id, s.name);
    }
  }
  for (const id of owner.keys()) if (!ruled.has(id)) problems.push(`action ${id} (${owner.get(id)}) has no rule`);
  for (const [id, by] of ruled) if (!owner.has(id)) problems.push(`${by} has a rule for ${id}, which no one registers`);
  const bindings = (kind: string, core: Readonly<Record<string, DesktopBinding<ActionId>>>, of: (s: SliceSpec) => SliceSpec['keys']) => {
    const bound = new Map<string, { action: string; by: string }>(Object.entries(core).map(([k, b]) => [k, { action: b.action, by: 'core' }]));
    for (const s of slices) {
      for (const [k, b] of Object.entries(of(s) ?? {})) {
        const was = bound.get(k);
        if (!was) { bound.set(k, { action: b.action, by: s.name }); continue; }
        if (was.by === 'core') problems.push(`${kind} ${k} of ${s.name} overrides the core binding (${was.action})`);
        else if (was.action !== b.action) problems.push(`${kind} ${k} is bound to ${was.action} by ${was.by} and to ${b.action} by ${s.name}`);
      }
    }
    for (const [k, b] of bound) if (!owner.has(b.action)) problems.push(`${kind} ${k} (${b.by}) runs unknown action ${b.action}`);
  };
  bindings('key', CORE_KEY_BINDINGS, (s) => s.keys);
  bindings(':command', CORE_COMMAND_BINDINGS, (s) => s.commands);
  // Keys a slice routes in its own raw keydown listener never reach the registry maps above.
  const raw = new Map<string, string>();
  for (const s of slices) {
    for (const k of new Set(s.rawKeys ?? [])) {
      const was = raw.get(k);
      if (was) problems.push(`raw key ${k} is routed by both ${was} and ${s.name}`);
      else raw.set(k, s.name);
    }
  }
  for (const s of slices) {
    const own = new Set(s.rawKeys ?? []);
    if (own.size === 0) continue;
    for (const o of slices) {
      if (o === s) continue;
      // `extraKeys` are display hints for keys a pane handles itself, and panes share Esc and q, so only `keys` count.
      const bound = o.keys ?? {};
      for (const k of own) if (k in bound) problems.push(`raw key ${k} of ${s.name} is also bound in the registry by ${o.name}`);
    }
  }
  return problems;
}

/** Actions that still make sense over an open dialog. */
const OVER_MODAL = new Set<ActionId>(['quit', 'toggle-vim-keys']);

/** Null when the action can run now, else the reason it cannot. */
export function unavailable(id: ActionId, s: UiSnapshot): string | null {
  if (s.modalOpen && !OVER_MODAL.has(id)) return 'Close the open dialog first';
  const rule = RULES[id];
  if (!rule) throw new Error(`action ${id} has no rule`);
  return rule(s);
}

// ---------- palette ----------

export interface PaletteEntry {
  id: ActionId;
  title: string;
  group: MenuGroup;
  detail?: string;
  /** Display hints (platform-adjusted); with Vim keys off only the keys that still work (local #227). */
  keys: string[];
  /** Every hint, shown or not: the exact-key search (`gg`, `:archive`) and the Tab argument edit read these. */
  searchKeys: string[];
  /** Everyday words that also find it (action-keywords.ts and the spec's own). */
  keywords: string[];
  enabled: boolean;
  /** Why it is disabled. */
  why?: string;
}

/** Every action the palette lists (all but right-click-only ones), registry order. */
export function paletteEntries(s: UiSnapshot, platform: Platform): PaletteEntry[] {
  const plain = (text: string): string => (s.vimKeys ? text : formatKeyTip(text));
  return ACTIONS.filter((a) => !a.contextOnly).map((a) => {
    const why = unavailable(a.id, s);
    const all = keyHints(a.id).map((k) => displayHint(k, platform));
    return {
      id: a.id,
      title: a.title,
      group: a.group,
      ...(a.detail ? { detail: s.vimKeys ? a.detail : a.plainDetail ?? formatKeyTip(a.detail) } : {}),
      keys: s.vimKeys ? all : all.filter((k) => !isVimOnlyHint(k)),
      searchKeys: all,
      keywords: [...(a.keywords ?? []), ...(ACTION_KEYWORDS[a.id] ?? [])],
      enabled: why === null,
      ...(why === null ? {} : { why: plain(why) }),
    };
  });
}

export interface Ranked {
  entry: PaletteEntry;
  /** 0 = title prefix (best) … 8 = scattered letters. */
  tier: number;
  /** Matched title character indices, for highlighting. */
  hits: number[];
}

const fold = (t: string) => t.toLowerCase();
const range = (from: number, n: number) => Array.from({ length: n }, (_, i) => from + i);

/** Word starts: index 0 and every letter or digit after a non-alphanumeric. */
function wordStarts(t: string): number[] {
  const out: number[] = [];
  for (let i = 0; i < t.length; i++) {
    if (/[\p{L}\p{N}:]/u.test(t[i]!) && (i === 0 || !/[\p{L}\p{N}]/u.test(t[i - 1]!))) out.push(i);
  }
  return out;
}

/** Letters of q in order inside t (spaces in q ignored), preferring word starts. */
function subsequence(t: string, q: string): number[] | null {
  const need = [...q.replace(/\s+/g, '')];
  const hits: number[] = [];
  let i = 0;
  for (const c of need) {
    const at = t.indexOf(c, i);
    if (at < 0) return null;
    hits.push(at);
    i = at + 1;
  }
  return hits;
}

/** "files" -> "file", "todos" -> "todo": a plural query still finds the singular word. */
const stem = (w: string): string => (w.length > 3 && w.endsWith('s') && !w.endsWith('ss') ? w.slice(0, -1) : w);

/** Whether text has a word that starts with q (or its singular), at a word start. */
function wordPrefix(text: string, q: string): boolean {
  const t = fold(text);
  const forms = q === stem(q) ? [q] : [q, stem(q)];
  return wordStarts(t).some((i) => forms.some((f) => t.startsWith(f, i)));
}

/**
 * 3 = a synonym spelled exactly as the query (plural or not), 4 = a word of one starts with it, else null.
 * `at` is the synonym's place in the action's list: the first words listed are the action's best names.
 */
function keywordTier(keywords: readonly string[], q: string): { tier: number; at: number } | null {
  let best: { tier: number; at: number } | null = null;
  keywords.forEach((k, at) => {
    const kw = fold(k);
    if (kw === q || stem(kw) === stem(q)) { if (!best || best.tier > 3 || best.at > at) best = { tier: 3, at }; }
    else if (!best && wordPrefix(kw, q)) best = { tier: 4, at };
  });
  return best;
}

/** `raw` keeps case for keys (`x` and `X` are different TUI keys); `q` is folded. */
function match(e: PaletteEntry, raw: string, q: string): (Omit<Ranked, 'entry'> & { kw?: number }) | null {
  const t = fold(e.title);
  if (t.startsWith(q)) return { tier: 0, hits: range(0, q.length) };
  if (e.searchKeys.includes(raw)) return { tier: 1, hits: [] };
  const keys = e.searchKeys.map(fold);
  const starts = wordStarts(t);
  const at = starts.find((i) => t.startsWith(q, i));
  if (at !== undefined) return { tier: 2, hits: range(at, q.length) };
  const words = q.split(/\s+/).filter(Boolean);
  if (words.length > 1) {
    const hits: number[] = [];
    if (words.every((w) => {
      const i = starts.find((s) => t.startsWith(w, s) && !hits.includes(s));
      if (i === undefined) return false;
      hits.push(...range(i, w.length));
      return true;
    })) return { tier: 2, hits: hits.sort((a, b) => a - b) };
  }
  // The action's everyday words (action-keywords.ts) and the group it sits under.
  const kw = keywordTier(e.keywords, q);
  if (kw !== null) return { tier: kw.tier, hits: [], kw: kw.at };
  const sub = t.indexOf(q);
  if (sub >= 0) return { tier: 5, hits: range(sub, q.length) };
  if (keys.some((k) => k.startsWith(q))) return { tier: 6, hits: [] };
  if (wordPrefix(e.detail ?? '', q) || fold(e.group) === q) return { tier: 7, hits: [] };
  const scattered = subsequence(t, q);
  if (scattered) return { tier: 8, hits: scattered };
  return null;
}

/**
 * Disabled actions an empty query hides: a family of four or more that waits on the same
 * screen ("Open the Issues workspace first", about 40 rows) collapses behind its opener, and
 * typing finds them again. Nothing hides while no action is enabled (a dialog is open).
 */
function collapsedIds(entries: readonly PaletteEntry[]): ReadonlySet<ActionId> {
  if (!entries.some((e) => e.enabled)) return new Set();
  const by = new Map<string, ActionId[]>();
  for (const e of entries) {
    if (e.enabled) continue;
    by.set(e.why ?? '', [...(by.get(e.why ?? '') ?? []), e.id]);
  }
  return new Set([...by.values()].filter((ids) => ids.length >= 4).flat());
}

/** How many actions an empty query hides behind their opener (the palette says so). */
export function collapsedCount(entries: readonly PaletteEntry[]): number {
  return collapsedIds(entries).size;
}

/**
 * Fuzzy palette search. Title prefix matches rank first, then an exact key
 * (`gg`, `:archive`), a word prefix ("pause" in "Hard pause"), an everyday word
 * ("help", "files", "todo"; action-keywords.ts), a substring, a key prefix
 * (`:arc`), a word of the detail or group, and last the letters in order ("nxs" →
 * "Next session"). An action that can run now ranks above one that cannot, whatever
 * the match (an exact key still comes first). Ties keep the earliest match, then the
 * shorter title, then registry order. An empty query lists the enabled actions in
 * registry order, then the disabled ones, with big context-only families collapsed.
 */
export function rankPalette(entries: readonly PaletteEntry[], query: string): Ranked[] {
  const raw = query.trim();
  const q = fold(raw);
  if (q === '') {
    const hidden = collapsedIds(entries);
    return [...entries.filter((e) => e.enabled), ...entries.filter((e) => !e.enabled && !hidden.has(e.id))]
      .map((entry) => ({ entry, tier: 0, hits: [] }));
  }
  const out: Array<Ranked & { order: number; kw?: number }> = [];
  entries.forEach((entry, order) => {
    const m = match(entry, raw, q);
    if (m) out.push({ entry, ...m, order });
  });
  const usable = (r: Ranked): number => (r.tier === 1 || r.entry.enabled ? 0 : 1);
  out.sort((a, b) => usable(a) - usable(b)
    || a.tier - b.tier
    || (a.kw ?? 0) - (b.kw ?? 0)
    || (a.hits[0] ?? 0) - (b.hits[0] ?? 0)
    || a.entry.title.length - b.entry.title.length
    || a.order - b.order);
  return out.map(({ order: _order, kw: _kw, ...r }) => r);
}

/** A palette query typed as a TUI command (`:archive`, `: continue go`), else null. */
export function commandQuery(query: string): string | null {
  const t = query.trim();
  if (!t.startsWith(':')) return null;
  const line = t.slice(1).trim();
  return line === '' ? null : line;
}

/** Move the palette highlight, wrapping. */
export function movePick(count: number, cur: number, delta: number): number {
  if (count <= 0) return -1;
  if (cur < 0) return delta >= 0 ? 0 : count - 1;
  return (((cur + delta) % count) + count) % count;
}

// ---------- native menus ----------

/** Built-in menu items the OS implements (names match src-tauri/src/menu.rs). */
export type PredefinedItem =
  | 'about' | 'services' | 'hide' | 'hide_others' | 'show_all' | 'quit'
  | 'undo' | 'redo' | 'cut' | 'copy' | 'paste' | 'select_all'
  | 'minimize' | 'maximize' | 'fullscreen' | 'close_window';

export type MenuNode =
  | { kind: 'item'; id: string; text: string; enabled: boolean; accelerator?: string; checked?: boolean }
  | { kind: 'separator' }
  | { kind: 'predefined'; item: PredefinedItem }
  /** A nested submenu (menu bar only), e.g. View > Files. */
  | { kind: 'submenu'; text: string; items: MenuNode[] };

export interface Submenu {
  id: string;
  text: string;
  items: MenuNode[];
}

export const APP_MENU_PREFIX = 'act:';

/**
 * The caps src-tauri/src/menu.rs enforces (MAX_SUBMENUS, MAX_ITEMS, MAX_TEXT, MAX_ACTION_ID, MAX_DEPTH;
 * tests/registry.test.mjs pins these to the Rust constants). `items` counts one menu level, separators included.
 */
export const MENU_LIMITS = { submenus: 12, items: 64, text: 96, actionId: 48, depth: 3 } as const;

/** What a person can scan: no menu level in the bar has more entries than this (#305). */
export const MENU_SCAN_LIMIT = 30;

/** One place in a grouped menu: an action, a separator, or a nested submenu of actions. */
export type MenuLayoutEntry = ActionId | '-' | { readonly sub: string; readonly items: readonly (ActionId | '-')[] };

/**
 * How the big menus are arranged (#305), grouped as the TUI's keybinding help groups them
 * (docs/keybindings.md: Session Lifecycle, Session Hierarchy, Search, Folding, File Explorer, ...).
 * A group without a layout lists its actions in registry order. An action of the group that the layout
 * does not name still shows, at the end of the menu, so a new slice action is never lost;
 * layoutProblems() and tests/registry.test.mjs keep the layout honest.
 */
export const MENU_LAYOUT: Readonly<Partial<Record<MenuGroup, readonly MenuLayoutEntry[]>>> = {
  session: [
    'soft-interrupt', 'hard-pause', 'clear-pause', 'downgrade-pause', '-',
    'quick-continue', 'continue', 'focus-composer', 'answer-question', 'pending-messages', '-',
    { sub: 'Message tools', items: ['input-modal', 'compile-prompt', 'fix-grammar', 'prompt-preview', '-', 'ai-command', 'ai-chat'] },
    { sub: 'Organize', items: [
      'rename-session', 'rate-session', 'choose-label', 'change-session-project', '-',
      'move-to-parent', 'move-to-root', 'set-epic-lead', '-',
      'toggle-pin', 'toggle-testing', 'toggle-rotation', 'cancel-retry',
    ] },
    { sub: 'Create', items: ['create-group', 'create-epic', 'create-story', 'create-task', 'create-bug'] },
    { sub: 'Branch and context', items: ['switch-model', 'rotate-session', '-', 'commit-push', 'land-session'] },
    { sub: 'Topology', items: ['topology-run-epic', 'topology-resolve'] },
    '-',
    'session-info', 'copy-session-id', 'yank', '-',
    'archive', 'unarchive-session', 'delete-session', '-',
    'emergency-stop', 'drain-restart',
  ],
  go: [
    'cursor-down', 'cursor-up', 'cursor-first', 'cursor-last', 'open', '-',
    'next-attention', 'prev-attention', 'attention-list', 'attention-slot', '-',
    'ascend', 'ascend-or-back', 'close-pane', 'session-normal', 'jump-back', 'jump-forward', '-',
    { sub: 'Conversation', items: ['find-in-conversation', '-', 'event-next', 'event-prev', 'user-next', 'user-prev'] },
    { sub: 'Filter', items: ['focus-search', 'search-next', 'search-prev', 'clear-search'] },
    { sub: 'Label groups', items: ['label-next', 'label-prev'] },
    '-',
    'open-notifications', 'open-recent', 'open-recent-file', 'open-trash',
  ],
  issues: [
    'issues-open', 'issues-go-number', 'issues-refresh', 'issues-poll', 'issues-close', '-',
    'issues-new', 'issues-edit', 'issues-status', 'issues-priority', 'issues-assign', 'issues-labels', 'issues-deps', '-',
    'issues-cancel', 'issues-archive', 'issues-restore', '-',
    { sub: 'Move', items: [
      'issues-down', 'issues-up', 'issues-inspect', 'issues-open-session', '-',
      'issues-page-next', 'issues-page-prev', 'issues-first', 'issues-last', '-',
      'issues-tab-next', 'issues-tab-prev', 'issues-section', 'issues-more', 'issues-less',
    ] },
    { sub: 'Search and filter', items: [
      'issues-search', 'issues-filters', 'issues-sort', 'issues-operator', '-',
      'issues-view-ready', 'issues-view-blocked', 'issues-view-mine', 'issues-view-recent', 'issues-view-all',
    ] },
    { sub: 'Copy', items: ['issues-copy-ref', 'issues-copy-uuid'] },
    { sub: 'Topology', items: ['issues-topology', 'topology-run-issue'] },
  ],
  view: [
    'view-active', 'view-archived', 'toggle-finished', '-',
    'toggle-tools', 'toggle-system', 'toggle-thinking', '-',
    'palette', 'command-line', 'toggle-vim-keys', 'toggle-submit-on-enter', 'settings-open', '-',
    { sub: 'Session list', items: [
      'choose-project', 'next-tab', 'prev-tab', 'choose-sort', 'view-jobs', '-', 'sidebar-wider', 'sidebar-narrower',
    ] },
    { sub: 'Folds', items: ['fold-toggle', 'fold-open', 'fold-close', '-', 'fold-open-all', 'fold-close-all'] },
    { sub: 'Files', items: [
      'files-explorer', 'files-find', 'files-close-viewer', '-', 'files-edit', 'files-save', 'files-undo', '-',
      'files-add', 'files-mkdir', 'files-delete', '-',
      'files-toggle-hidden', 'files-follow-preview', 'files-refresh', 'files-collapse-all',
    ] },
    { sub: 'Managers', items: [
      'mgr-tree', 'mgr-board', 'mgr-decisions', 'mgr-budget', '-', 'mgr-policy', 'mgr-scope', '-',
      'mgr-global', 'mgr-dashboard', 'mgr-fleet', 'mgr-portfolio-appoint',
    ] },
    { sub: 'Daemon admin', items: [
      'admin-schedules', 'admin-provider-keys', 'admin-mcp', 'admin-satellites', 'admin-remote', '-',
      'admin-scratch', 'admin-settlement', 'admin-diagnostics', '-',
      'open-hooks', 'open-providers', 'open-bridges',
    ] },
    { sub: 'Memory and cards', items: ['memory-search', 'dialectic', '-', 'card', 'card-user', '-', 'prompt-creator'] },
    { sub: 'Operator to-do', items: ['todo-toggle', 'todo-view', 'todo-undo'] },
    { sub: 'Graphs', items: ['graph-open', 'dag-open', 'esp-open'] },
    { sub: 'Tools', items: ['toggle-terminal', 'kill-terminal', '-', 'git-panel', 'system-monitor'] },
    { sub: 'Theme', items: ['theme-picker', 'theme-roles', 'theme-reset'] },
  ],
};

export const CONTEXT_MENU_PREFIX = 'ctx:';

/** Chords the webview keeps for text fields on Linux and Windows (keyengine.ts OTHER_SYSTEM_CTRL): no menu takes them. */
const OTHER_SYSTEM_ACCEL = /^CmdOrCtrl\+[CVXAZY]$/;

/** The accelerator a menu item shows on this platform: off macOS a text field's own Ctrl chord stays with the field. */
function menuAccelerator(id: ActionId, platform: Platform | undefined): string | undefined {
  const a = accelerator(id);
  return a && platform !== undefined && platform !== 'mac' && OTHER_SYSTEM_ACCEL.test(a) ? undefined : a;
}

function itemNode(a: ActionSpec, s: UiSnapshot, prefix: string, withAccel: boolean, platform?: Platform): MenuNode {
  const accel = withAccel ? menuAccelerator(a.id, platform) : undefined;
  return {
    kind: 'item',
    id: `${prefix}${a.id}`,
    text: a.title,
    enabled: unavailable(a.id, s) === null,
    ...(accel ? { accelerator: accel } : {}),
    ...(a.checked ? { checked: a.checked(s) } : {}),
  };
}

/** The actions of a group the menu bar lists, in menu order (menuAfter placement, Quit last). */
function groupActions(group: MenuGroup, skip: ReadonlySet<ActionId> = new Set()): ActionSpec[] {
  const inMenu = ACTIONS.filter((a) => a.group === group && !a.contextOnly && !a.noMenu && !skip.has(a.id));
  const ordered = inMenu.filter((a) => !a.menuAfter || !inMenu.some((t) => t.id === a.menuAfter));
  for (const a of inMenu) {
    if (a.menuAfter && !ordered.includes(a)) ordered.splice(ordered.findIndex((t) => t.id === a.menuAfter) + 1, 0, a);
  }
  // Quit is always the last File item.
  const quit = ordered.findIndex((a) => a.id === 'quit');
  if (quit >= 0) ordered.push(...ordered.splice(quit, 1));
  return ordered;
}

/** Separators only between entries: none first, last or twice in a row. */
function tidy(nodes: MenuNode[]): MenuNode[] {
  const out: MenuNode[] = [];
  for (const n of nodes) {
    if (n.kind === 'separator' && (out.length === 0 || out[out.length - 1]!.kind === 'separator')) continue;
    out.push(n);
  }
  while (out.length > 0 && out[out.length - 1]!.kind === 'separator') out.pop();
  return out;
}

function groupItems(group: MenuGroup, s: UiSnapshot, skip: ReadonlySet<ActionId>, platform: Platform): MenuNode[] {
  const actions = groupActions(group, skip);
  const node = (a: ActionSpec): MenuNode => itemNode(a, s, APP_MENU_PREFIX, true, platform);
  const layout = MENU_LAYOUT[group];
  if (!layout) {
    const out: MenuNode[] = [];
    for (const a of actions) {
      if (a.menuBreak && out.length > 0) out.push({ kind: 'separator' });
      out.push(node(a));
    }
    return out;
  }
  const listed = new Map(actions.map((a) => [a.id, a]));
  const used = new Set<ActionId>();
  const place = (e: ActionId | '-'): MenuNode | null => {
    if (e === '-') return { kind: 'separator' };
    const a = listed.get(e);
    if (!a || used.has(e)) return null;
    used.add(e);
    return node(a);
  };
  const out: MenuNode[] = [];
  for (const e of layout) {
    if (typeof e === 'object') {
      const items = tidy(e.items.map(place).filter((n): n is MenuNode => n !== null));
      if (items.length > 0) out.push({ kind: 'submenu', text: e.sub, items });
    } else {
      const n = place(e);
      if (n) out.push(n);
    }
  }
  // An action the layout does not name (a new slice) still shows, after the arranged ones.
  const rest = actions.filter((a) => !used.has(a.id));
  if (rest.length > 0) out.push({ kind: 'separator' }, ...rest.map(node));
  return tidy(out);
}

/**
 * What is wrong with MENU_LAYOUT, empty when nothing: a name that is no action, an action in another group,
 * a palette-only or right-click-only action (it would never show), or an action placed twice.
 */
export function layoutProblems(layout: Readonly<Partial<Record<MenuGroup, readonly MenuLayoutEntry[]>>> = MENU_LAYOUT): string[] {
  const problems: string[] = [];
  const seen = new Set<string>();
  for (const [group, entries] of Object.entries(layout)) {
    const names = (entries ?? []).flatMap((e) => (typeof e === 'object' ? e.items : [e])).filter((e) => e !== '-');
    for (const id of names) {
      const a = BY_ID.get(id as ActionId);
      if (!a) problems.push(`${group} menu names ${id}, which is no action`);
      else if (a.group !== group) problems.push(`${group} menu names ${id}, which is in the ${a.group} group`);
      else if (a.noMenu || a.contextOnly) problems.push(`${group} menu names ${id}, which is not a menu action`);
      if (seen.has(id)) problems.push(`${id} is placed twice in the menu layout`);
      seen.add(id);
    }
  }
  return problems;
}

/** Every action item in a menu, nested submenus included, in menu order. */
export function menuItems(nodes: readonly MenuNode[]): Array<Extract<MenuNode, { kind: 'item' }>> {
  return nodes.flatMap((n) => (n.kind === 'item' ? [n] : n.kind === 'submenu' ? menuItems(n.items) : []));
}

const pre = (...items: Array<PredefinedItem | '-'>): MenuNode[] =>
  items.map((i) => (i === '-' ? { kind: 'separator' } : { kind: 'predefined', item: i }));

/**
 * The menu bar. macOS: the app menu (About, Services, Hide, Quit), an Edit
 * menu (without it Cmd-C/V do not reach text fields) and a Window menu around
 * the registry's File / Session / Go / Issues / View / Help. Linux and Windows: the
 * registry menus only, with Quit at the end of File. The big groups nest
 * submenus (MENU_LAYOUT) so every level fits MENU_SCAN_LIMIT (#305).
 */
export function appMenuModel(platform: Platform, s: UiSnapshot, appName = 'RSI Desktop'): Submenu[] {
  const mac = platform === 'mac';
  const menus: Submenu[] = [];
  if (mac) {
    // Settings… sits in the app menu on macOS (Cmd-,), so View does not list it twice.
    menus.push({
      id: 'app',
      text: appName,
      items: [
        ...pre('about', '-'),
        ...ACTIONS.filter((a) => a.id === 'settings-open').map((a) => itemNode(a, s, APP_MENU_PREFIX, true, platform)),
        ...pre('-', 'services', '-', 'hide', 'hide_others', 'show_all', '-', 'quit'),
      ],
    });
  }
  menus.push({ id: 'file', text: 'File', items: groupItems('file', s, mac ? new Set(['quit']) : new Set(), platform) });
  if (mac) menus.push({ id: 'edit', text: 'Edit', items: pre('undo', 'redo', '-', 'cut', 'copy', 'paste', 'select_all') });
  menus.push(
    { id: 'session', text: 'Session', items: groupItems('session', s, new Set(), platform) },
    { id: 'go', text: 'Go', items: groupItems('go', s, new Set(), platform) },
    { id: 'issues', text: 'Issues', items: groupItems('issues', s, new Set(), platform) },
    { id: 'view', text: 'View', items: groupItems('view', s, mac ? new Set<ActionId>(['settings-open']) : new Set(), platform) },
  );
  if (mac) menus.push({ id: 'window', text: 'Window', items: pre('minimize', 'maximize', '-', 'close_window') });
  menus.push({ id: 'help', text: 'Help', items: groupItems('help', s, new Set(), platform) });
  return menus;
}

/** A right-click menu: the actions for that target, enabled per the snapshot. */
export function contextMenuModel(kind: ContextKind, s: UiSnapshot): MenuNode[] {
  const out: MenuNode[] = [];
  let group: MenuGroup | undefined;
  // Target-specific actions first, then the general ones in registry order.
  const own = ACTIONS.filter((a) => a.contexts?.includes(kind));
  const sorted = [...own.filter((a) => a.contextOnly), ...own.filter((a) => !a.contextOnly)];
  for (const a of sorted) {
    const g = a.contextOnly ? undefined : a.group;
    if (out.length > 0 && (g !== group || a.menuBreak)) out.push({ kind: 'separator' });
    group = g;
    out.push(itemNode(a, s, CONTEXT_MENU_PREFIX, false));
  }
  return out;
}

export interface ItemState {
  enabled: boolean;
  checked?: boolean;
}

/** What src-tauri's set_app_menu installed (menu.rs MenuInstallReport). */
export interface MenuInstallReport {
  installed: string[];
  skipped: Array<{ id: string; reason: string }>;
}

/**
 * The runtime notice for a menu bar that did not fully install (#305), or null when every menu did.
 * `failure` is set_app_menu's refusal (no menu installed at all). menu.rs already printed the process-log line.
 */
export function menuInstallProblem(
  menus: readonly Submenu[], report: MenuInstallReport | null, platform: Platform, failure?: unknown,
): string | null {
  const name = (id: string): string => menus.find((m) => m.id === id)?.text ?? id;
  const palette = `the command palette (${displayHint('Ctrl-K', platform)})`;
  if (failure !== undefined) {
    const why = failure instanceof Error ? failure.message : String(failure);
    return `Menu bar not installed: ${why}. Every action is still in ${palette}.`;
  }
  const skipped = report?.skipped ?? [];
  if (skipped.length === 0) return null;
  const list = skipped.map((s) => `${name(s.id)} (${s.reason})`).join('; ');
  return `Menu bar installed without ${list}. Those actions are still in ${palette}.`;
}

/** Enabled / checked state of every registry item in a menu, by item id. */
export function menuState(menus: readonly Submenu[]): Record<string, ItemState> {
  const out: Record<string, ItemState> = {};
  for (const m of menus) {
    for (const n of menuItems(m.items)) {
      out[n.id] = n.checked === undefined ? { enabled: n.enabled } : { enabled: n.enabled, checked: n.checked };
    }
  }
  return out;
}

export type ActionSource = 'key' | 'menu' | 'context' | 'palette' | 'button';

/** A menu event id back to its action. */
export function parseMenuId(id: string): { action: ActionId; source: 'menu' | 'context' } | null {
  const m = id.match(/^(act|ctx):([a-z-]+)$/);
  if (!m || !isActionId(m[2]!)) return null;
  return { action: m[2] as ActionId, source: m[1] === 'act' ? 'menu' : 'context' };
}

export const DEDUPE_MS = 400;

/**
 * Drops the second half of one keystroke reported twice: a native menu
 * accelerator (Cmd-N, Ctrl-K) can fire alongside the webview's keydown on
 * some platforms. Only a key/menu pair of the same action counts.
 */
export class Deduper {
  private last: { id: ActionId; source: ActionSource; at: number } | undefined;

  admit(id: ActionId, source: ActionSource, now: number): boolean {
    const l = this.last;
    const pair = (a: ActionSource, b: ActionSource) => (a === 'key' && b === 'menu') || (a === 'menu' && b === 'key');
    if (l && l.id === id && pair(l.source, source) && now - l.at < DEDUPE_MS) {
      this.last = undefined;
      return false;
    }
    this.last = { id, source, at: now };
    return true;
  }
}
