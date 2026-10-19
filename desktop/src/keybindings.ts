// Which TUI keys and `:` commands the desktop can run today (pure, no DOM).
// Every other row of the TUI's keymap stays in the engine and answers
// "not in the desktop yet". buildKeymap throws if a key or command here is not
// in docs/keybindings.md, so a TUI rename fails `npm test` instead of drifting.
// A slice's keys and commands come from its SLICE (actions/index.generated.ts);
// registry.ts sliceProblems() refuses one that rebinds a core or another slice's key.
import { SLICES } from './actions/index.generated.js';
import { buildKeymap, type DesktopBinding, type Keymap } from './keyengine.js';
import { COMMANDS, NORMAL_KEYS, RESERVED_KEYS } from './keymap.generated.js';
import type { ActionId } from './registry.js';

/** Keys run registry actions (registry.ts); the ids are the same strings. */
export type DesktopAction = ActionId;

const b = (action: DesktopAction, confirm?: 'twice'): DesktopBinding<DesktopAction> =>
  (confirm ? { action, confirm } : { action });

/** The core TUI Normal-mode keys (keybindings.md notation) the desktop runs. */
export const CORE_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<DesktopAction>>> = {
  '?': b('help'),
  'Ctrl-Alt-G': b('help'),
  j: b('cursor-down'),
  k: b('cursor-up'),
  gg: b('cursor-first'),
  G: b('cursor-last'),
  Enter: b('open'),
  L: b('open'),
  l: b('open'),
  ']a': b('next-attention'),
  '[a': b('prev-attention'),
  '/': b('focus-search'),
  r: b('refresh'),
  gs: b('view-active'),
  ga: b('view-archived'),
  x: b('soft-interrupt'),
  X: b('hard-pause', 'twice'),
  '<Space>hc': b('clear-pause', 'twice'),
  '<Space>a': b('archive'),
  '<Space>c': b('quick-continue'),
  // The key is the argument: i inserts at the cursor, a appends, o / O open a line (actions/input.ts).
  i: { action: 'focus-composer', arg: 'i' },
  a: { action: 'focus-composer', arg: 'a' },
  o: { action: 'focus-composer', arg: 'o' },
  O: { action: 'focus-composer', arg: 'O' },
  '<Space>gq': b('answer-question'),
  'Ctrl-N': b('launch'),
  '<Space>N': b('launch'),
  '<Space>;': b('palette'),
  ZQ: b('quit'),
  ZZ: b('quit'),
};

/** The core TUI `:` commands (canonical names) the desktop runs. */
export const CORE_COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<DesktopAction>>> = {
  refresh: b('refresh'),
  sessions: b('view-active'),
  archives: b('view-archived'),
  kill: b('soft-interrupt'),
  'pause clear': b('clear-pause'),
  archive: b('archive'),
  continue: b('continue'),
  blank: b('launch'),
  help: b('help'),
  commands: b('palette'),
  quit: b('quit'),
};

/** Every Normal-mode key the desktop runs: the core keys, then each slice's. */
export const KEY_BINDINGS: Readonly<Record<string, DesktopBinding<DesktopAction>>> = Object.assign(
  {}, CORE_KEY_BINDINGS, ...SLICES.map((s) => s.keys ?? {}),
);

/** Every `:` command the desktop runs: the core commands, then each slice's. */
export const COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<DesktopAction>>> = Object.assign(
  {}, CORE_COMMAND_BINDINGS, ...SLICES.map((s) => s.commands ?? {}),
);

/** `:` commands the TUI parses ahead of its table, from each slice. */
export const PREFIX_COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<DesktopAction>>> = Object.assign(
  {}, ...SLICES.map((s) => s.prefixCommands ?? {}),
);

export function desktopKeymap(): Keymap<DesktopAction> {
  return buildKeymap({
    normal: NORMAL_KEYS,
    reserved: RESERVED_KEYS,
    commands: COMMANDS,
    keyBindings: KEY_BINDINGS,
    commandBindings: COMMAND_BINDINGS,
    prefixCommands: PREFIX_COMMAND_BINDINGS,
  });
}

// ---------- session-list cursor (pure) ----------

/** Move the cursor by `delta` rows, clamped. With no cursor, `j` lands on the
 * first row and `k` on the last. */
export function stepCursor(ids: readonly string[], cur: string | null, delta: number): string | null {
  if (ids.length === 0) return null;
  const i = cur === null ? -1 : ids.indexOf(cur);
  const from = i >= 0 ? i : delta > 0 ? -1 : ids.length;
  const to = Math.max(0, Math.min(ids.length - 1, from + delta));
  return ids[to] ?? null;
}

/** `gg` / `G`: row N (1-based) with a count, else the first / last row. */
export function jumpCursor(ids: readonly string[], count: number | null, end: 'first' | 'last'): string | null {
  if (ids.length === 0) return null;
  if (count !== null) return ids[Math.max(0, Math.min(ids.length - 1, count - 1))] ?? null;
  return (end === 'first' ? ids[0] : ids[ids.length - 1]) ?? null;
}

/** `]a` / `[a`: the next / previous flagged row after the cursor, wrapping. */
export function nextFlagged(
  ids: readonly string[], flagged: readonly boolean[], cur: string | null, dir: 1 | -1,
): string | null {
  const n = ids.length;
  if (n === 0) return null;
  const i = cur === null ? -1 : ids.indexOf(cur);
  const start = i >= 0 ? i : dir > 0 ? -1 : n;
  for (let step = 1; step <= n; step++) {
    const j = (((start + dir * step) % n) + n) % n;
    if (flagged[j]) return ids[j] ?? null;
  }
  return null;
}
