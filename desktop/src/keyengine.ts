// Pure modal key engine (no DOM): Normal mode, numeric counts, multi-key
// chords (`g`, `z`, `]`, `<Space>` leader tree ...), the `:` command line with
// completion, and `Esc`. The keymap comes from the TUI's generated tables
// (keymap.generated.ts); keybindings.ts says which rows the desktop can run.
// Unit-tested under tests/keyengine.test.mjs; vimkeys.ts is the DOM adapter.
import type { CommandRow, NormalKeyRow, ReservedKeyRow } from './keymap.generated.js';

// ---------- key tokens ----------

/** Canonical key token: a printable character ("g", "G", "?"), a named key
 * ("Enter", "Esc", "Space", "F3", "Up"), or modifiers + key ("C-n", "C-A-g",
 * "S-Tab"). Modifier order is always C, A, S. */
export type KeyToken = string;

const NAMED: Record<string, string> = {
  Enter: 'Enter', Escape: 'Esc', Esc: 'Esc', Backspace: 'Backspace', Tab: 'Tab', ' ': 'Space',
  Spacebar: 'Space', Delete: 'Delete', Del: 'Delete', Home: 'Home', End: 'End',
  PageUp: 'PageUp', PageDown: 'PageDown', ArrowUp: 'Up', ArrowDown: 'Down',
  ArrowLeft: 'Left', ArrowRight: 'Right', Up: 'Up', Down: 'Down', Left: 'Left', Right: 'Right',
};
const NAMED_TOKENS = new Set([...Object.values(NAMED), ...Array.from({ length: 12 }, (_, i) => `F${i + 1}`)]);
const MODIFIER_KEYS = new Set([
  'Shift', 'Control', 'Alt', 'AltGraph', 'Meta', 'OS', 'Super', 'Hyper', 'CapsLock', 'Fn', 'NumLock',
  'Dead', 'Process', 'Compose', 'Unidentified',
]);

/** Notation from keybindings.md (`<Space>gp`, `Ctrl-Alt-G`, `gg`, `F3`) to tokens. */
export function parseKeys(notation: string): KeyToken[] {
  const chord = notation.match(/^((?:(?:Ctrl|Alt|Shift)-)+)(.+)$/);
  if (chord) {
    const mods = new Set(chord[1]!.split('-'));
    return [chordToken(mods.has('Ctrl'), mods.has('Alt'), mods.has('Shift'), chord[2]!)];
  }
  if (NAMED_TOKENS.has(notation)) return [notation];
  const out: KeyToken[] = [];
  let rest = notation;
  while (rest.length > 0) {
    if (rest.startsWith('<Space>')) { out.push('Space'); rest = rest.slice(7); continue; }
    const ch = String.fromCodePoint(rest.codePointAt(0)!);
    out.push(ch);
    rest = rest.slice(ch.length);
  }
  return out;
}

function chordToken(ctrl: boolean, alt: boolean, shift: boolean, key: string): KeyToken {
  const named = NAMED_TOKENS.has(key);
  const k = named ? key : key.toLowerCase();
  // Shift is part of a printable key itself ("G"), so it is only spelled out
  // for named keys or alongside Ctrl/Alt ("C-S-n").
  const s = shift && (named || ctrl || alt);
  return `${ctrl ? 'C-' : ''}${alt ? 'A-' : ''}${s ? 'S-' : ''}${k}`;
}

/** Display form of a token, in keybindings.md notation. */
export function displayToken(t: KeyToken): string {
  if (t === 'Space') return '<Space>';
  const m = t.match(/^((?:[CAS]-)+)(.+)$/);
  if (!m) return t;
  const names: Record<string, string> = { C: 'Ctrl', A: 'Alt', S: 'Shift' };
  const mods = m[1]!.split('-').filter(Boolean).map((x) => names[x]);
  const key = m[2]!.length === 1 ? m[2]!.toUpperCase() : m[2]!;
  return [...mods, key].join('-');
}

export function displayKeys(tokens: readonly KeyToken[]): string {
  return tokens.map(displayToken).join('');
}

export type Platform = 'mac' | 'other';

/** From navigator.userAgentData.platform, navigator.platform or the user agent. */
export function detectPlatform(platformString: string | undefined): Platform {
  return /mac|iphone|ipad/i.test(platformString ?? '') ? 'mac' : 'other';
}

/** The subset of KeyboardEvent the engine reads (works in WebKitGTK, WKWebView, WebView2). */
export interface KeyEventLike {
  key: string;
  code?: string;
  ctrlKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  metaKey?: boolean;
  isComposing?: boolean;
  defaultPrevented?: boolean;
}

/** macOS Cmd chords that stay with the system (edit menu, window, app). */
const MAC_SYSTEM_CMD = new Set(['c', 'v', 'x', 'a', 'z', 'q', 'w', 'h', 'm', ',', '`']);
/** Ctrl chords that stay with the webview on Linux and Windows (clipboard, undo). */
const OTHER_SYSTEM_CTRL = new Set(['c', 'v', 'x', 'a', 'z', 'y']);

function baseKey(ev: KeyEventLike): string {
  const k = ev.key;
  // A modifier can turn the key into another glyph (macOS Option-g = "©") or
  // a control character; the physical code names the intended letter/digit.
  if (/^[a-zA-Z0-9]$/.test(k)) return k;
  const code = ev.code ?? '';
  const letter = code.match(/^Key([A-Z])$/);
  if (letter) return letter[1]!.toLowerCase();
  const digit = code.match(/^Digit([0-9])$/);
  if (digit) return digit[1]!;
  return k;
}

/**
 * Map a keyboard event to a token, or null when the engine must not see it
 * (modifier-only keys, IME composition, system chords). On macOS, Cmd-<key>
 * acts as the TUI's Ctrl-<key> unless it is a system chord (Cmd-C/V/X/A/Z/Q/W
 * /H/M stay native); the physical Ctrl key works too. On Linux and Windows the
 * Super/Windows key is never ours and Ctrl-C/V/X/A/Z/Y stay with the webview.
 */
export function keyToken(ev: KeyEventLike, platform: Platform): KeyToken | null {
  if (ev.isComposing || MODIFIER_KEYS.has(ev.key)) return null;
  let ctrl = !!ev.ctrlKey;
  const alt = !!ev.altKey;
  const shift = !!ev.shiftKey;
  if (ev.metaKey) {
    if (platform !== 'mac' || ctrl) return null;
    const k = baseKey(ev).toLowerCase();
    if (MAC_SYSTEM_CMD.has(k)) return null;
    ctrl = true;
  }
  const named = NAMED[ev.key] ?? (/^F([1-9]|1[0-2])$/.test(ev.key) ? ev.key : undefined);
  if (named) {
    if (named === 'Space' && !ctrl && !alt) return 'Space';
    return chordToken(ctrl, alt, shift, named);
  }
  if (!ctrl && !alt) {
    // Printable: the key already carries Shift ("G", "?", ":").
    return [...ev.key].length === 1 ? ev.key : null;
  }
  const k = baseKey(ev);
  if ([...k].length !== 1) return null;
  if (ctrl && !alt && platform === 'other' && OTHER_SYSTEM_CTRL.has(k.toLowerCase())) return null;
  return chordToken(ctrl, alt, shift && /^[a-zA-Z]$/.test(k), k);
}

// ---------- keymap ----------

/** What the desktop does for a TUI key or command (see keybindings.ts). */
export interface DesktopBinding<A extends string = string> {
  action: A;
  /** Press twice within CONFIRM_MS (the TUI's double-press guard). */
  confirm?: 'twice';
  /** A fixed argument the key passes to the action (`<Space>3` -> slot "3"). */
  arg?: string;
}

export interface KeyLeaf<A extends string = string> {
  keys: string;
  title: string;
  /** The desktop action; undefined = the TUI has it, the desktop not yet. */
  binding?: DesktopBinding<A>;
  /** A retired TUI chord kept as a no-op, with the reason. */
  retired?: string;
}

export interface KeyNode<A extends string = string> {
  children: Map<KeyToken, KeyNode<A>>;
  leaf?: KeyLeaf<A>;
}

export interface Keymap<A extends string = string> {
  root: KeyNode<A>;
  commands: readonly CommandRow[];
  commandBindings: Readonly<Record<string, DesktopBinding<A>>>;
  /** Commands the TUI parses ahead of its table (commands.rs `parse_command`: `:pending …`, `:drain …`). */
  prefixCommands?: Readonly<Record<string, DesktopBinding<A>>>;
}

export interface KeymapSource<A extends string = string> {
  normal: readonly NormalKeyRow[];
  reserved: readonly ReservedKeyRow[];
  commands: readonly CommandRow[];
  /** Desktop bindings keyed by the TUI notation (`gg`, `<Space>a`). */
  keyBindings: Readonly<Record<string, DesktopBinding<A>>>;
  /** Desktop bindings keyed by the canonical command name (`archive`, `pause clear`). */
  commandBindings: Readonly<Record<string, DesktopBinding<A>>>;
  /** First words the TUI parses before its command table; the rest of the line is the argument. */
  prefixCommands?: Readonly<Record<string, DesktopBinding<A>>>;
}

function insert<A extends string>(root: KeyNode<A>, tokens: KeyToken[], leaf: KeyLeaf<A>): boolean {
  let node = root;
  for (const t of tokens) {
    if (node.leaf) return false;
    let next = node.children.get(t);
    if (!next) { next = { children: new Map() }; node.children.set(t, next); }
    node = next;
  }
  if (node.leaf || node.children.size > 0) return false;
  node.leaf = leaf;
  return true;
}

/** Build the chord trie. Throws when a desktop binding names a key or command
 * the TUI does not document, or when two TUI keys overlap as key and prefix. */
export function buildKeymap<A extends string>(src: KeymapSource<A>): Keymap<A> {
  const root: KeyNode<A> = { children: new Map() };
  const known = new Set(src.normal.map((r) => r.keys));
  for (const k of Object.keys(src.keyBindings)) {
    if (!known.has(k)) throw new Error(`desktop binds ${k}, which docs/keybindings.md does not list`);
  }
  const names = new Set(src.commands.map((c) => c.name));
  for (const k of Object.keys(src.commandBindings)) {
    if (!names.has(k)) throw new Error(`desktop binds :${k}, which docs/keybindings.md does not list`);
  }
  for (const r of src.normal) {
    const leaf: KeyLeaf<A> = { keys: r.keys, title: r.title, binding: src.keyBindings[r.keys] };
    if (!insert(root, parseKeys(r.keys), leaf)) throw new Error(`key ${r.keys} overlaps another key`);
  }
  // Retired no-ops only fill free slots: a live key always wins.
  for (const r of src.reserved) {
    if (r.kind === 'no-op') insert(root, parseKeys(r.keys), { keys: r.keys, title: r.why, retired: r.why });
  }
  for (const k of Object.keys(src.prefixCommands ?? {})) {
    if (names.has(k)) throw new Error(`:${k} is in the TUI command table; bind it as a command`);
  }
  return { root, commands: src.commands, commandBindings: src.commandBindings, prefixCommands: src.prefixCommands ?? {} };
}

// ---------- engine ----------

export const CONFIRM_MS = 5000;
/**
 * A chord key waits this long for the next one (keybindings.md "Key Sequence
 * Timing"); a slower next key starts a new sequence. vim's default timeoutlen.
 */
export const SEQUENCE_TIMEOUT_MS = 1000;
export const MAX_COUNT = 9999;
/** Completions the command line shows at once. */
export const MAX_COMPLETIONS = 14;

export interface Hint {
  /** The next key, displayed (`p`, `<Space>`, `Ctrl-N`). */
  key: string;
  title: string;
  /** True when pressing it runs a desktop action (or opens more desktop keys). */
  bound: boolean;
  /** True when the key opens a further chord level. */
  prefix: boolean;
}

export interface CommandLineView {
  text: string;
  /** Completion candidates for the typed text (names, then aliases). */
  completions: string[];
  /** The candidate Tab selected, or -1. */
  selected: number;
}

export interface EngineView {
  mode: 'normal' | 'command';
  /** The keys typed so far, with any count prefix (`3`, `<Space>g`). */
  pending: string;
  hints: Hint[];
  command?: CommandLineView;
}

export type Outcome<A extends string = string> =
  /** Not the engine's key: let the webview handle it. */
  | { kind: 'pass' }
  /** A chord prefix or count: waiting for the next key. */
  | { kind: 'pending' }
  | { kind: 'run'; action: A; count: number | null; keys: string; arg?: string }
  /** First press of a press-twice key. */
  | { kind: 'confirm'; action: A; keys: string; title: string }
  /** The TUI binds it; the desktop does not have the feature yet. */
  | { kind: 'missing'; keys: string; title: string }
  | { kind: 'retired'; keys: string; why: string }
  /** Nothing in the TUI binds this sequence. */
  | { kind: 'unbound'; keys: string }
  /** The command line opened, changed or completed. */
  | { kind: 'command' }
  /** Esc (or Backspace on an empty line) dropped the pending keys or command line. */
  | { kind: 'cancel' }
  | { kind: 'error'; message: string };

export interface ResolvedCommand {
  row: CommandRow;
  arg: string;
}

/** Find the command a line names (longest name or alias at a word boundary). */
export function resolveCommand(commands: readonly CommandRow[], line: string): ResolvedCommand | { error: string } {
  const t = line.trim().replace(/^:/, '').trim();
  if (t === '') return { error: 'empty command' };
  let best: { row: CommandRow; len: number } | undefined;
  for (const row of commands) {
    for (const n of [row.name, ...row.aliases]) {
      if ((t === n || t.startsWith(`${n} `)) && (!best || n.length > best.len)) best = { row, len: n.length };
    }
  }
  if (!best) return { error: `Not an rsi command: :${t}` };
  const arg = t.slice(best.len).trim();
  if (arg && best.row.arg === 'none') return { error: `:${best.row.name} takes no argument` };
  if (!arg && best.row.arg === 'required') return { error: `:${best.row.name} needs an argument` };
  return { row: best.row, arg };
}

/** Command names, then aliases, that start with the typed text. */
export function completeCommand(commands: readonly CommandRow[], typed: string): string[] {
  const t = typed.replace(/^:/, '').trimStart();
  const names = commands.map((c) => c.name).filter((n) => n.startsWith(t) && n !== t);
  const aliases = commands.flatMap((c) => c.aliases).filter((n) => n.startsWith(t) && n !== t && !names.includes(n));
  return [...names, ...aliases];
}

function leafCount(node: KeyNode): number {
  let n = node.leaf && !node.leaf.retired ? 1 : 0;
  for (const c of node.children.values()) n += leafCount(c);
  return n;
}

function anyBound(node: KeyNode): boolean {
  if (node.leaf?.binding) return true;
  for (const c of node.children.values()) if (anyBound(c)) return true;
  return false;
}

export class KeyEngine<A extends string = string> {
  private enabled = true;
  private mode: 'normal' | 'command' = 'normal';
  private keys: KeyToken[] = [];
  private count = '';
  private armed: { keys: string; at: number } | undefined;
  private line = '';
  private typed = '';
  private pick = -1;
  private lastAt = 0;

  constructor(private readonly keymap: Keymap<A>) {}

  /** Off: every key passes to the webview and pending state is dropped. */
  setEnabled(on: boolean): void {
    this.enabled = on;
    if (!on) this.reset();
  }

  isEnabled(): boolean {
    return this.enabled;
  }

  /** True while a chord, count or command line is in progress. */
  busy(): boolean {
    return this.mode === 'command' || this.keys.length > 0 || this.count !== '';
  }

  reset(): void {
    this.mode = 'normal';
    this.keys = [];
    this.count = '';
    this.line = '';
    this.typed = '';
    this.pick = -1;
  }

  view(): EngineView {
    if (this.mode === 'command') {
      const completions = completeCommand(this.keymap.commands, this.typed);
      return {
        mode: 'command', pending: '', hints: [],
        command: { text: this.line, completions, selected: this.pick },
      };
    }
    return { mode: 'normal', pending: this.count + displayKeys(this.keys), hints: this.hints() };
  }

  /** The next keys after the pending prefix (which-key); empty when idle. */
  hints(): Hint[] {
    if (this.keys.length === 0) return [];
    const node = this.node(this.keys);
    if (!node) return [];
    const out: Hint[] = [];
    for (const [t, child] of node.children) {
      if (child.leaf?.retired) continue;
      const prefix = !child.leaf;
      out.push({
        key: displayToken(t),
        title: prefix ? `+${leafCount(child)} keys` : child.leaf!.title,
        bound: anyBound(child),
        prefix,
      });
    }
    return out;
  }

  /**
   * Drop a chord prefix typed more than SEQUENCE_TIMEOUT_MS ago (a count and
   * the command line stay). True when something was dropped.
   */
  expire(now: number): boolean {
    if (this.mode !== 'normal' || this.keys.length === 0 || now - this.lastAt <= SEQUENCE_TIMEOUT_MS) return false;
    this.keys = [];
    return true;
  }

  /** Open the `:` line (`:` itself and the registry's command-line action). */
  openCommandLine(): Outcome<A> {
    if (!this.enabled) return { kind: 'pass' };
    this.reset();
    this.mode = 'command';
    return { kind: 'command' };
  }

  /** Text pasted while the command line is open. */
  insertText(text: string): Outcome<A> {
    if (!this.enabled || this.mode !== 'command') return { kind: 'pass' };
    this.setLine(this.line + text.replace(/[\r\n]+/g, ' '));
    return { kind: 'command' };
  }

  feed(token: KeyToken, now = 0): Outcome<A> {
    if (!this.enabled) return { kind: 'pass' };
    if (this.mode === 'command') return this.feedCommand(token);
    this.expire(now);
    this.lastAt = now;
    if (token === 'Esc') {
      const wasBusy = this.busy();
      this.reset();
      this.armed = undefined;
      return wasBusy ? { kind: 'cancel' } : { kind: 'pass' };
    }
    if (this.keys.length === 0) {
      if (/^[1-9]$/.test(token) || (token === '0' && this.count !== '')) {
        this.count = String(Math.min(Number(this.count + token), MAX_COUNT));
        return { kind: 'pending' };
      }
      if (token === ':') return this.openCommandLine();
    }
    const seq = [...this.keys, token];
    const node = this.node(seq);
    if (!node) {
      const keys = displayKeys(seq);
      const wasChord = this.keys.length > 0 || this.count !== '';
      this.reset();
      // A lone unknown named or modified key (Tab, Ctrl-R, arrows) belongs to
      // the webview; an unknown printable key or chord gets a notice.
      if (!wasChord && token.length > 1) return { kind: 'pass' };
      return { kind: 'unbound', keys };
    }
    if (!node.leaf) {
      this.keys = seq;
      return { kind: 'pending' };
    }
    const count = this.count === '' ? null : Number(this.count);
    this.reset();
    return this.resolveLeaf(node.leaf, count, now);
  }

  private resolveLeaf(leaf: KeyLeaf<A>, count: number | null, now: number): Outcome<A> {
    if (leaf.retired) { this.armed = undefined; return { kind: 'retired', keys: leaf.keys, why: leaf.retired }; }
    if (!leaf.binding) { this.armed = undefined; return { kind: 'missing', keys: leaf.keys, title: leaf.title }; }
    const b = leaf.binding;
    if (b.confirm === 'twice') {
      const armed = this.armed;
      if (armed && armed.keys === leaf.keys && now - armed.at <= CONFIRM_MS) {
        this.armed = undefined;
        return { kind: 'run', action: b.action, count, keys: leaf.keys, ...(b.arg ? { arg: b.arg } : {}) };
      }
      this.armed = { keys: leaf.keys, at: now };
      return { kind: 'confirm', action: b.action, keys: leaf.keys, title: leaf.title };
    }
    this.armed = undefined;
    return { kind: 'run', action: b.action, count, keys: leaf.keys, ...(b.arg ? { arg: b.arg } : {}) };
  }

  private node(seq: readonly KeyToken[]): KeyNode<A> | undefined {
    let node: KeyNode<A> | undefined = this.keymap.root;
    for (const t of seq) {
      node = node.children.get(t);
      if (!node) return undefined;
    }
    return node;
  }

  private setLine(text: string): void {
    this.line = text;
    this.typed = text;
    this.pick = -1;
  }

  private cycle(step: number): void {
    const c = completeCommand(this.keymap.commands, this.typed);
    if (c.length === 0) return;
    this.pick = this.pick < 0 ? (step > 0 ? 0 : c.length - 1) : (this.pick + step + c.length) % c.length;
    this.line = c[this.pick]!;
  }

  private feedCommand(token: KeyToken): Outcome<A> {
    switch (token) {
      case 'Esc':
        this.reset();
        return { kind: 'cancel' };
      case 'Backspace':
        if (this.line === '') { this.reset(); return { kind: 'cancel' }; }
        this.setLine([...this.line].slice(0, -1).join(''));
        return { kind: 'command' };
      case 'Tab': case 'C-n': case 'Down':
        this.cycle(1);
        return { kind: 'command' };
      case 'S-Tab': case 'C-p': case 'Up':
        this.cycle(-1);
        return { kind: 'command' };
      case 'Enter': {
        const line = this.line;
        this.reset();
        if (line.trim() === '') return { kind: 'cancel' };
        return this.runCommand(line);
      }
      case 'Space':
        this.setLine(`${this.line} `);
        return { kind: 'command' };
      default:
        if ([...token].length === 1) {
          this.setLine(this.line + token);
          return { kind: 'command' };
        }
        // Other chords do nothing on the command line but stay consumed.
        return { kind: 'command' };
    }
  }

  /** Run one ex command line (also used by the palette in a later slice). */
  runCommand(line: string): Outcome<A> {
    const t = line.trim().replace(/^:/, '').trim();
    const first = t.split(/\s+/)[0] ?? '';
    const pre = this.keymap.prefixCommands?.[first];
    if (pre) {
      const arg = t.slice(first.length).trim();
      return { kind: 'run', action: pre.action, count: null, keys: `:${first}`, ...(arg ? { arg } : {}) };
    }
    const r = resolveCommand(this.keymap.commands, line);
    if ('error' in r) return { kind: 'error', message: r.error };
    const b = this.keymap.commandBindings[r.row.name];
    const keys = `:${r.row.name}`;
    if (!b) return { kind: 'missing', keys, title: r.row.title };
    const arg = r.arg || b.arg;
    return { kind: 'run', action: b.action, count: null, keys, ...(arg ? { arg } : {}) };
  }
}

// ---------- event routing ----------

/** The focused element, as far as the engine cares. */
export interface TargetLike {
  tagName?: string;
  isContentEditable?: boolean;
  /** HTMLInputElement.type */
  type?: string;
}

/** Inputs that take no typing; a clicked checkbox must not leave the keys dead. */
const NON_TEXT_INPUTS = new Set(['checkbox', 'radio', 'button', 'submit', 'reset', 'range', 'color', 'file', 'image']);

/** Text entry has the keyboard: text inputs, textareas, selects, contenteditable. */
export function isTypingTarget(t: TargetLike | null | undefined): boolean {
  if (!t) return false;
  if (t.isContentEditable) return true;
  const tag = (t.tagName ?? '').toUpperCase();
  if (tag === 'INPUT') return !NON_TEXT_INPUTS.has((t.type ?? 'text').toLowerCase());
  return tag === 'TEXTAREA' || tag === 'SELECT';
}

const ACTIVATABLE = new Set(['BUTTON', 'A', 'SUMMARY']);

export interface RouteEnv {
  platform: Platform;
  /** document.activeElement */
  target: TargetLike | null | undefined;
  /** A modal <dialog> is open; it owns the keyboard. */
  modalOpen: boolean;
  now: number;
}

/**
 * The single path from a keydown to the engine: off, typing, modal dialogs,
 * IME and system chords all pass through untouched. Enter on a focused button
 * or link keeps its native click while no chord or command line is pending.
 */
export function routeKey<A extends string>(engine: KeyEngine<A>, ev: KeyEventLike, env: RouteEnv): Outcome<A> {
  if (!engine.isEnabled() || ev.defaultPrevented || env.modalOpen || isTypingTarget(env.target)) {
    return { kind: 'pass' };
  }
  const token = keyToken(ev, env.platform);
  if (token === null) return { kind: 'pass' };
  const tag = (env.target?.tagName ?? '').toUpperCase();
  if (token === 'Enter' && ACTIVATABLE.has(tag) && !engine.busy()) return { kind: 'pass' };
  return engine.feed(token, env.now);
}

// ---------- setting ----------

export const VIM_KEYS_STORAGE_KEY = 'rsi.desktop.vimKeys';

export interface StorageLike {
  getItem(k: string): string | null;
  setItem(k: string, v: string): void;
}

/** The "Vim keys" setting; on unless the operator turned it off. */
export function readVimKeys(storage: StorageLike | undefined): boolean {
  try {
    return storage?.getItem(VIM_KEYS_STORAGE_KEY) !== 'off';
  } catch {
    return true;
  }
}

export function writeVimKeys(storage: StorageLike | undefined, on: boolean): void {
  try {
    storage?.setItem(VIM_KEYS_STORAGE_KEY, on ? 'on' : 'off');
  } catch {
    // Storage can be unavailable (private mode); the toggle still applies this run.
  }
}
