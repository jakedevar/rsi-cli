// The terminal overlay's pure logic (slice 12, hub #3014): which shell the
// toggle shows, insert / normal mode key routing (docs/keybindings.md
// "Terminal Overlay (Embedded Shell)"), scrollback motions and the grid-size
// math. No DOM, no xterm.js; tested under tests/terminal.test.mjs. The DOM and
// PTY side is actions/terminal.ts over src-tauri/src/pty.rs.
import type { KeyEventLike, Platform } from './keyengine.js';

/** Insert: keys go to the shell. Normal: vim-style scrollback (Esc enters it). */
export type TermMode = 'insert' | 'normal';

/** One shell: its PTY id once spawned, the directory, and its exit. */
export interface TermEntry {
  key: string;
  /** PTY id from pty_spawn; null while spawning or after the shell exited. */
  ptyId: string | null;
  cwd: string;
  /** Set when the shell exited (code null when unknown). */
  exited: { code: number | null } | null;
}

export interface TermState {
  /** Terminals by key (an open session's id, or NO_SESSION_KEY). */
  entries: Readonly<Record<string, TermEntry>>;
  /** The key the overlay shows, or null when hidden. */
  visible: string | null;
  mode: TermMode;
}

/** The terminal used while no session is open (it starts in the home directory). */
export const NO_SESSION_KEY = 'home';

export const EMPTY_TERMS: TermState = { entries: {}, visible: null, mode: 'insert' };

/** The terminal key for the open session (null when none is open). */
export function terminalKey(openSessionId: string | null | undefined): string {
  return openSessionId ? openSessionId : NO_SESSION_KEY;
}

/** The shell is up (spawned, not exited). */
export function isRunning(e: TermEntry | undefined): boolean {
  return !!e && e.ptyId !== null && e.exited === null;
}

export type ToggleStep =
  | { kind: 'hide'; state: TermState }
  /** Show `key`; `spawn` when it has no running shell (first open, or after exit). */
  | { kind: 'show'; state: TermState; key: string; spawn: boolean };

/**
 * `Ctrl-\` / `:term`: hide a shown overlay (the shell keeps running), else
 * show the open session's terminal, starting a shell when it has none.
 * Opening always lands in insert mode, as in the TUI.
 */
export function toggle(s: TermState, openSessionId: string | null | undefined, cwd: string): ToggleStep {
  if (s.visible !== null) return { kind: 'hide', state: { ...s, visible: null } };
  const key = terminalKey(openSessionId);
  const cur = s.entries[key];
  const spawn = !isRunning(cur) && !(cur && cur.ptyId === null && cur.exited === null);
  const entry: TermEntry = spawn ? { key, ptyId: null, cwd, exited: null } : cur!;
  return { kind: 'show', key, spawn, state: { entries: { ...s.entries, [key]: entry }, visible: key, mode: 'insert' } };
}

/** pty_spawn answered for `key`. */
export function spawned(s: TermState, key: string, ptyId: string, cwd: string): TermState {
  const e = s.entries[key];
  if (!e) return s;
  return { ...s, entries: { ...s.entries, [key]: { ...e, ptyId, cwd, exited: null } } };
}

/** pty_spawn failed: forget the entry and hide the overlay if it showed it. */
export function spawnFailed(s: TermState, key: string): TermState {
  const { [key]: _gone, ...rest } = s.entries;
  return { ...s, entries: rest, visible: s.visible === key ? null : s.visible };
}

/** A `pty-exit` event: the key whose shell it was (null for an unknown id) and the new state. */
export function exited(s: TermState, ptyId: string, code: number | null): { key: string | null; state: TermState } {
  const hit = Object.values(s.entries).find((e) => e.ptyId === ptyId);
  if (!hit) return { key: null, state: s };
  return { key: hit.key, state: { ...s, entries: { ...s.entries, [hit.key]: { ...hit, ptyId: null, exited: { code } } } } };
}

/** The key whose shell is `ptyId` (routes `pty-output`). */
export function keyForPty(s: TermState, ptyId: string): string | null {
  return Object.values(s.entries).find((e) => e.ptyId === ptyId)?.key ?? null;
}

/** Forget a terminal (after `End terminal shell`). */
export function remove(s: TermState, key: string): TermState {
  const { [key]: _gone, ...rest } = s.entries;
  return { ...s, entries: rest, visible: s.visible === key ? null : s.visible, mode: s.visible === key ? 'insert' : s.mode };
}

/** Every running PTY id (window close / reload kills them). */
export function runningPtys(s: TermState): string[] {
  return Object.values(s.entries).filter(isRunning).map((e) => e.ptyId!);
}

// ---------- keys ----------

/**
 * `Ctrl-\` (on macOS also `Cmd-\`): toggles the overlay from anywhere, ahead
 * of the vim keys, text fields and the shell (the TUI's global intercept).
 */
export function isToggleChord(ev: KeyEventLike, platform: Platform): boolean {
  if (ev.isComposing || ev.altKey) return false;
  const backslash = ev.code === 'Backslash' || ev.key === '\\' || ev.key === '\x1c';
  if (!backslash) return false;
  if (platform === 'mac') return !!ev.ctrlKey !== !!ev.metaKey;
  return !!ev.ctrlKey && !ev.metaKey;
}

export type Scroll = { by: 'lines'; n: number } | { by: 'top' } | { by: 'bottom' };

export type TermKey =
  /** Let xterm.js take it (insert mode: it goes to the shell). */
  | { kind: 'pty' }
  /** Hide the overlay (the shell keeps running). */
  | { kind: 'toggle' }
  | { kind: 'mode'; mode: TermMode }
  | { kind: 'scroll'; scroll: Scroll }
  /** Leave it to the webview (macOS Cmd-C copies the selection). */
  | { kind: 'native' }
  /** Eat it: a normal-mode key with no meaning. */
  | { kind: 'swallow' };

/** macOS Cmd chords that stay with the system (copy, paste, select all, quit, …). */
const MAC_NATIVE = new Set(['c', 'v', 'a', 'x', 'q', 'w', 'h', 'm', 'k', ',', '`']);

/** Half a screen, at least one line (`Ctrl-d` / `Ctrl-u`). */
export function halfPage(rows: number): number {
  return Math.max(1, Math.floor(rows / 2));
}

/**
 * One keydown inside the focused terminal. Insert mode: `Ctrl-\` hides, Esc
 * enters normal mode, everything else goes to the shell (Ctrl-C is the
 * shell's interrupt). Normal mode: `i` / `a` back to insert, `j` / `k` (and
 * the arrows) a line, `Ctrl-d` / `Ctrl-u` half a page, `g` top, `G` bottom.
 */
export function routeTerminalKey(mode: TermMode, ev: KeyEventLike, platform: Platform, rows: number): TermKey {
  if (isToggleChord(ev, platform)) return { kind: 'toggle' };
  if (ev.isComposing) return mode === 'insert' ? { kind: 'pty' } : { kind: 'swallow' };
  const plain = !ev.ctrlKey && !ev.altKey && !ev.metaKey;
  if (platform === 'mac' && ev.metaKey && !ev.ctrlKey && MAC_NATIVE.has(ev.key.toLowerCase())) return { kind: 'native' };
  if (mode === 'insert') {
    if (ev.key === 'Escape' && plain && !ev.shiftKey) return { kind: 'mode', mode: 'normal' };
    return { kind: 'pty' };
  }
  // On macOS Cmd stands in for Ctrl, as everywhere else in the desktop.
  const ctrl = (!!ev.ctrlKey || (platform === 'mac' && !!ev.metaKey)) && !ev.altKey;
  const k = ev.key;
  if (ctrl && (k === 'd' || k === 'D' || ev.code === 'KeyD')) return { kind: 'scroll', scroll: { by: 'lines', n: halfPage(rows) } };
  if (ctrl && (k === 'u' || k === 'U' || ev.code === 'KeyU')) return { kind: 'scroll', scroll: { by: 'lines', n: -halfPage(rows) } };
  if (!plain) return { kind: 'swallow' };
  switch (k) {
    case 'i': case 'a': return { kind: 'mode', mode: 'insert' };
    case 'j': case 'ArrowDown': return { kind: 'scroll', scroll: { by: 'lines', n: 1 } };
    case 'k': case 'ArrowUp': return { kind: 'scroll', scroll: { by: 'lines', n: -1 } };
    case 'G': return { kind: 'scroll', scroll: { by: 'bottom' } };
    case 'g': return { kind: 'scroll', scroll: { by: 'top' } };
    default: return { kind: 'swallow' };
  }
}

/** The overlay's status-line mode label, as the TUI shows it. */
export function modeLabel(mode: TermMode): string {
  return mode === 'insert' ? 'INSERT' : 'NORMAL';
}

/** The overlay's title: the directory, then why the shell is not running. */
export function overlayTitle(e: TermEntry | undefined): string {
  if (!e) return 'Terminal';
  if (e.exited) return `Terminal — ${e.cwd} — exited${e.exited.code === null ? '' : ` (code ${e.exited.code})`}; reopen for a new shell`;
  if (e.ptyId === null) return `Terminal — ${e.cwd} — starting…`;
  return `Terminal — ${e.cwd}`;
}

// ---------- size ----------

export interface GridSize { cols: number; rows: number }

/** The bounds pty.rs accepts (check_size). */
export const MIN_COLS = 2;
export const MIN_ROWS = 1;
export const MAX_COLS = 1000;
export const MAX_ROWS = 1000;
/** The size a shell starts at before the pane has been measured. */
export const DEFAULT_SIZE: GridSize = { cols: 80, rows: 24 };

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

/** A measured grid, made safe for the PTY: whole cells inside pty.rs's bounds; null when unmeasurable. */
export function clampGrid(g: { cols: number; rows: number } | null | undefined): GridSize | null {
  if (!g || !Number.isFinite(g.cols) || !Number.isFinite(g.rows) || g.cols <= 0 || g.rows <= 0) return null;
  return { cols: clamp(Math.floor(g.cols), MIN_COLS, MAX_COLS), rows: clamp(Math.floor(g.rows), MIN_ROWS, MAX_ROWS) };
}

/**
 * How many cells fit a pane (the fit addon's arithmetic, used when it cannot
 * measure): the width less padding and the scrollbar, the height less padding,
 * each divided by the cell size and rounded down. Null for a hidden pane.
 */
export function gridFor(
  pane: { width: number; height: number },
  cell: { width: number; height: number },
  chrome: { padX?: number; padY?: number; scrollbar?: number } = {},
): GridSize | null {
  if (cell.width <= 0 || cell.height <= 0) return null;
  const w = pane.width - (chrome.padX ?? 0) - (chrome.scrollbar ?? 0);
  const h = pane.height - (chrome.padY ?? 0);
  if (w <= 0 || h <= 0) return null;
  return clampGrid({ cols: w / cell.width, rows: h / cell.height });
}

/** The resize to send, or null when the grid did not change (no PTY round trip). */
export function resizeStep(prev: GridSize | null, next: GridSize | null): GridSize | null {
  if (!next) return null;
  if (prev && prev.cols === next.cols && prev.rows === next.rows) return null;
  return next;
}

/** The TUI's popup: 90% of the width and 80% of the height, centered (ui/overlay/terminal.rs popup_rect). */
export function overlayRect(viewW: number, viewH: number): { left: number; top: number; width: number; height: number } {
  const width = Math.max(20, Math.floor((viewW * 90) / 100));
  const height = Math.max(5, Math.floor((viewH * 80) / 100));
  return { left: Math.floor(Math.max(0, viewW - width) / 2), top: Math.floor(Math.max(0, viewH - height) / 2), width, height };
}

/** Split a paste for pty_write's 64 KiB limit, never inside a UTF-16 surrogate pair. */
export function chunkInput(data: string, maxBytes = 64 * 1024): string[] {
  const out: string[] = [];
  let cur = '';
  let bytes = 0;
  const enc = new TextEncoder();
  for (const ch of data) {
    const n = enc.encode(ch).length;
    if (bytes + n > maxBytes && cur !== '') { out.push(cur); cur = ''; bytes = 0; }
    cur += ch;
    bytes += n;
  }
  if (cur !== '') out.push(cur);
  return out;
}
