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
import type { DesktopBinding, KeyEventLike, Platform } from './keyengine.js';
import { isActive } from './model.js';

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
}

/** The core actions (slices 1-3), in menu order within each group. */
const CORE_ACTIONS: readonly ActionSpec[] = [
  // File
  { id: 'launch', title: 'New session…', group: 'file', detail: 'Open the launch dialog' },
  { id: 'refresh', title: 'Refresh', group: 'file', menuBreak: true, detail: 'Reload health, projects and sessions' },
  { id: 'quit', title: 'Quit RSI Desktop', group: 'file', menuBreak: true },
  // Session (acts on the open session)
  { id: 'soft-interrupt', title: 'Interrupt (soft pause)', group: 'session', detail: 'Interrupt the running turn', contexts: ['session-row'] },
  { id: 'hard-pause', title: 'Hard pause', group: 'session', detail: 'Interrupt and hold until cleared', contexts: ['session-row'] },
  { id: 'clear-pause', title: 'Clear pause', group: 'session', contexts: ['session-row'] },
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
  { id: 'toggle-tools', title: 'Show tool events', group: 'view', menuBreak: true, checked: (s) => !s.hideTools, contexts: ['event'] },
  { id: 'toggle-vim-keys', title: 'Vim keys', group: 'view', checked: (s) => s.vimKeys, detail: 'Drive the app with the TUI keys while focus is not in a text field' },
  { id: 'palette', title: 'Command palette…', group: 'view', menuBreak: true, extraKeys: ['Ctrl-K'], detail: 'Search every action' },
  { id: 'command-line', title: 'Command line', group: 'view', extraKeys: [':'], detail: 'Type a TUI : command (needs Vim keys)' },
  // Help
  { id: 'help', title: 'Keyboard shortcuts', group: 'help', detail: 'Lists every desktop action with its keys (the command palette)' },
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
  return platform === 'mac' ? key.replace(/^Ctrl-/, 'Cmd-') : key;
}

/** The native accelerator for a plain Ctrl-<letter or digit> hint (CmdOrCtrl+N). */
export function accelerator(id: ActionId): string | undefined {
  for (const k of keyHints(id)) {
    const m = k.match(/^Ctrl-([A-Z0-9])$/);
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

// ---------- availability ----------

const LIVE = new Set(['Starting', 'Running', 'WaitingApproval']);
const NEED_OPEN = 'Open a session first (j/k, then Enter)';

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
 * registrations bind to different actions or that overrides a core binding.
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
  /** Display hints (platform-adjusted). */
  keys: string[];
  enabled: boolean;
  /** Why it is disabled. */
  why?: string;
}

/** Every action the palette lists (all but right-click-only ones), registry order. */
export function paletteEntries(s: UiSnapshot, platform: Platform): PaletteEntry[] {
  return ACTIONS.filter((a) => !a.contextOnly).map((a) => {
    const why = unavailable(a.id, s);
    return {
      id: a.id,
      title: a.title,
      group: a.group,
      ...(a.detail ? { detail: a.detail } : {}),
      keys: keyHints(a.id).map((k) => displayHint(k, platform)),
      enabled: why === null,
      ...(why === null ? {} : { why }),
    };
  });
}

export interface Ranked {
  entry: PaletteEntry;
  /** 0 = title prefix (best) … 5 = scattered letters. */
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

/** `raw` keeps case for keys (`x` and `X` are different TUI keys); `q` is folded. */
function match(e: PaletteEntry, raw: string, q: string): Omit<Ranked, 'entry'> | null {
  const t = fold(e.title);
  if (t.startsWith(q)) return { tier: 0, hits: range(0, q.length) };
  if (e.keys.includes(raw)) return { tier: 1, hits: [] };
  const keys = e.keys.map(fold);
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
  const sub = t.indexOf(q);
  if (sub >= 0) return { tier: 3, hits: range(sub, q.length) };
  if (keys.some((k) => k.startsWith(q))) return { tier: 4, hits: [] };
  const scattered = subsequence(t, q);
  if (scattered) return { tier: 5, hits: scattered };
  return null;
}

/**
 * Fuzzy palette search. Title prefix matches rank first, then an exact key
 * (`gg`, `:archive`), a word prefix ("pause" in "Hard pause"), a substring, a
 * key prefix (`:arc`), and last the letters in order ("nxs" → "Next session").
 * Ties keep the earliest match, then the shorter title, then registry order.
 * An empty query lists everything in registry order.
 */
export function rankPalette(entries: readonly PaletteEntry[], query: string): Ranked[] {
  const raw = query.trim();
  const q = fold(raw);
  if (q === '') return entries.map((entry) => ({ entry, tier: 0, hits: [] }));
  const out: Array<Ranked & { order: number }> = [];
  entries.forEach((entry, order) => {
    const m = match(entry, raw, q);
    if (m) out.push({ entry, ...m, order });
  });
  out.sort((a, b) => a.tier - b.tier
    || (a.hits[0] ?? 0) - (b.hits[0] ?? 0)
    || a.entry.title.length - b.entry.title.length
    || a.order - b.order);
  return out.map(({ order: _order, ...r }) => r);
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
  | { kind: 'predefined'; item: PredefinedItem };

export interface Submenu {
  id: string;
  text: string;
  items: MenuNode[];
}

export const APP_MENU_PREFIX = 'act:';
export const CONTEXT_MENU_PREFIX = 'ctx:';

function itemNode(a: ActionSpec, s: UiSnapshot, prefix: string, withAccel: boolean): MenuNode {
  const accel = withAccel ? accelerator(a.id) : undefined;
  return {
    kind: 'item',
    id: `${prefix}${a.id}`,
    text: a.title,
    enabled: unavailable(a.id, s) === null,
    ...(accel ? { accelerator: accel } : {}),
    ...(a.checked ? { checked: a.checked(s) } : {}),
  };
}

function groupItems(group: MenuGroup, s: UiSnapshot, skip: ReadonlySet<ActionId> = new Set()): MenuNode[] {
  const out: MenuNode[] = [];
  for (const a of ACTIONS) {
    if (a.group !== group || a.contextOnly || a.noMenu || skip.has(a.id)) continue;
    if (a.menuBreak && out.length > 0) out.push({ kind: 'separator' });
    out.push(itemNode(a, s, APP_MENU_PREFIX, true));
  }
  return out;
}

const pre = (...items: Array<PredefinedItem | '-'>): MenuNode[] =>
  items.map((i) => (i === '-' ? { kind: 'separator' } : { kind: 'predefined', item: i }));

/**
 * The menu bar. macOS: the app menu (About, Services, Hide, Quit), an Edit
 * menu (without it Cmd-C/V do not reach text fields) and a Window menu around
 * the registry's File / Session / Go / View / Help. Linux and Windows: the
 * registry menus only, with Quit at the end of File.
 */
export function appMenuModel(platform: Platform, s: UiSnapshot, appName = 'RSI Desktop'): Submenu[] {
  const mac = platform === 'mac';
  const menus: Submenu[] = [];
  if (mac) {
    menus.push({ id: 'app', text: appName, items: pre('about', '-', 'services', '-', 'hide', 'hide_others', 'show_all', '-', 'quit') });
  }
  menus.push({ id: 'file', text: 'File', items: groupItems('file', s, mac ? new Set(['quit']) : new Set()) });
  if (mac) menus.push({ id: 'edit', text: 'Edit', items: pre('undo', 'redo', '-', 'cut', 'copy', 'paste', 'select_all') });
  menus.push(
    { id: 'session', text: 'Session', items: groupItems('session', s) },
    { id: 'go', text: 'Go', items: groupItems('go', s) },
    { id: 'issues', text: 'Issues', items: groupItems('issues', s) },
    { id: 'view', text: 'View', items: groupItems('view', s) },
  );
  if (mac) menus.push({ id: 'window', text: 'Window', items: pre('minimize', 'maximize', '-', 'close_window') });
  menus.push({ id: 'help', text: 'Help', items: groupItems('help', s) });
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

/** Enabled / checked state of every registry item in a menu, by item id. */
export function menuState(menus: readonly Submenu[]): Record<string, ItemState> {
  const out: Record<string, ItemState> = {};
  for (const m of menus) {
    for (const n of m.items) {
      if (n.kind === 'item') out[n.id] = n.checked === undefined ? { enabled: n.enabled } : { enabled: n.enabled, checked: n.checked };
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
