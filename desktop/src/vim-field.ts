// DOM adapter between a text field and its VimEditor (vim-editor.ts). With
// Vim keys on, every <textarea> (the message box, the continue / launch
// prompts, the input modal, any dialog's multi-line box) and every
// <input data-vim="on"> (a one-line text field: vimInput() marks it) is a vim
// field: insert mode types natively, Esc enters normal mode, and normal /
// visual keys run the editor. With Vim keys off the fields stay plain.
// `data-vim="off"` opts a field out; a password or secret field, a search /
// filter box, the `:` line and the terminal's hidden xterm textarea are never
// one. Field text is only ever assigned as a value (no HTML).
import { type KeyEventLike, type Platform, keyToken } from './keyengine.js';
import { type VimMode, type VimOutcome, VimEditor } from './vim-editor.js';

export type VimField = HTMLTextAreaElement | HTMLInputElement;

interface FieldState {
  ed: VimEditor;
  /** The selection we last set, to tell a click or a script move from our own. */
  set: { start: number; end: number } | null;
}

const fields = new WeakMap<VimField, FieldState>();
const listeners: Array<(el: VimField, mode: VimMode) => void> = [];

/** Is this element a field the vim editor drives (when Vim keys are on)? */
export function isVimField(el: Element | null | undefined): el is VimField {
  if (!el) return false;
  const tag = el.tagName;
  const input = el as HTMLInputElement;
  if (input.disabled || input.readOnly) return false;
  if ((el as HTMLElement).dataset.vim === 'off' || (el as HTMLElement).hasAttribute('data-secret')) return false;
  if (el.closest('.xterm')) return false;
  if (tag === 'TEXTAREA') return true;
  return tag === 'INPUT' && (el as HTMLElement).dataset.vim === 'on' && (input.type === 'text' || input.type === 'search');
}

/**
 * Does this field hold one line (an `<input>`, or a wrapping one-line `<textarea data-line="single">`, form-field.ts)?
 * `o` / `O` / `J` / `p` then never add a line.
 */
export function isSingleLine(el: VimField): boolean {
  return el.tagName === 'INPUT' || el.dataset.line === 'single';
}

/** Called whenever a field's mode changes (the mode badge). */
export function onVimMode(cb: (el: VimField, mode: VimMode) => void): void {
  listeners.push(cb);
}

function state(el: VimField): FieldState {
  let s = fields.get(el);
  if (!s) {
    s = { ed: new VimEditor(el.value, 'insert', { singleLine: isSingleLine(el) }), set: null };
    fields.set(el, s);
  }
  return s;
}

/** The editor of a field (created in insert mode on first use). */
export function editorOf(el: VimField): VimEditor {
  return state(el).ed;
}

export function modeOf(el: VimField): VimMode {
  return fields.get(el)?.ed.mode ?? 'insert';
}

/** Bring the editor up to date with what the field shows (typing, a click, a script). */
function sync(el: VimField, s: FieldState): void {
  const start = el.selectionStart ?? 0;
  const end = el.selectionEnd ?? start;
  const moved = !s.set || s.set.start !== start || s.set.end !== end;
  if (el.value !== s.ed.text || moved) s.ed.setText(el.value, start);
}

/** Show the editor's text, cursor and selection in the field. */
function render(el: VimField, s: FieldState): void {
  const ed = s.ed;
  if (el.value !== ed.text) {
    el.value = ed.text;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }
  let start = ed.cursorOffset();
  let end = start;
  const sel = ed.selection();
  if (sel) ({ start, end } = sel);
  // Normal mode shows a block cursor: the character under the cursor selected.
  else if (ed.mode === 'normal' && start < el.value.length && el.value[start] !== '\n') end = start + (el.value.codePointAt(start)! > 0xffff ? 2 : 1);
  el.setSelectionRange(start, end, sel && ed.cursorOffset() < sel.end - 1 ? 'backward' : 'forward');
  s.set = { start, end };
  el.dataset.vimMode = ed.mode;
  for (const cb of listeners) cb(el, ed.mode);
}

/** Put a field in insert mode the way i / a / A / I / o / O does. */
export function enterInsert(el: VimField, how: 'i' | 'a' | 'A' | 'I' | 'o' | 'O' = 'i'): void {
  const s = state(el);
  sync(el, s);
  if (s.ed.mode !== 'insert') s.ed.enterInsert(how);
  else if (how === 'A' || how === 'o' || how === 'O' || how === 'a') {
    // Already typing: still honour the key's place.
    s.ed.key('Esc');
    s.ed.enterInsert(how);
  }
  render(el, s);
}

/** Put a field in normal mode (the TUI's `e`, or a script). */
export function enterNormal(el: VimField): void {
  const s = state(el);
  sync(el, s);
  if (s.ed.mode === 'insert') s.ed.leaveInsert();
  render(el, s);
}

/** Forget a field's half-typed command (focus left it). */
export function resetField(el: VimField): void {
  const s = fields.get(el);
  if (!s) return;
  s.ed.resetPending();
  if (el.dataset.vimMode && el.dataset.vimMode !== s.ed.mode) el.dataset.vimMode = s.ed.mode;
}

export interface VimKeyResult {
  outcome: VimOutcome | 'pass';
  /** The key token the editor saw. */
  token: string | null;
  /** Text a `y` copied, for the system clipboard. */
  yank: string | null;
}

/**
 * One keydown on a vim field. Insert mode only takes Esc; normal and visual
 * mode run the editor. `pass`: not the editor's key at all (IME, a system
 * chord). The caller decides what an `unhandled` key does.
 */
export function vimKey(el: VimField, ev: KeyEventLike, platform: Platform): VimKeyResult {
  if (ev.isComposing) return { outcome: 'pass', token: null, yank: null };
  const s = state(el);
  if (s.ed.mode === 'insert') {
    const plainEsc = ev.key === 'Escape' && !ev.ctrlKey && !ev.altKey && !ev.metaKey && !ev.shiftKey;
    if (!plainEsc) return { outcome: 'pass', token: null, yank: null };
    sync(el, s);
    s.ed.leaveInsert();
    render(el, s);
    return { outcome: 'consumed', token: 'Esc', yank: null };
  }
  const token = keyToken(ev, platform);
  if (token === null) return { outcome: 'pass', token: null, yank: null };
  sync(el, s);
  const outcome = s.ed.key(token);
  if (outcome !== 'unhandled') render(el, s);
  return { outcome, token, yank: s.ed.takeYank() };
}

/** Collapse a normal-mode block cursor before the field takes a paste, and type in insert mode. */
export function prepareForTyping(el: VimField): void {
  const s = fields.get(el);
  if (!s || s.ed.mode === 'insert') return;
  sync(el, s);
  s.ed.enterInsert('i');
  render(el, s);
}
