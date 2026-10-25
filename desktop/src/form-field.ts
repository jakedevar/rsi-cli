// A one-line text field that wraps (hub #3421, desktop #317). The Issue editor's title, labels, assignee,
// dependency and filter inputs and the project form's name and folder are short values that can still be
// longer than the field is wide; an <input> scrolls sideways and hides the rest. This is a <textarea> that
// grows with its text and is held to one line of content: Enter submits the form (as in an <input>), no way
// of editing puts a line break in it, and an optional limit refuses typing and cuts a paste past it, with a
// live counter and a hint. It is an ordinary vim field (vim-field.ts): modes, counts, text objects and the
// `:` line come from the one VimEditor, and with Vim keys off it is plain editing.
import { el } from './dom.js';
import {
  ISSUE_TITLE_COUNTER_FROM, charCount, counterText, fitPaste, flattenLines, settleEdit, titleLimitHint, typingRefused,
} from './form-text.js';

export interface LineFieldOpts {
  name: string;
  value: string;
  placeholder?: string;
  /** The most characters the field holds; typing past it is refused and a paste is cut. */
  max?: number;
  /** The counter shows from this many characters (default 150 under a limit). */
  counterFrom?: number;
}

export interface LineField {
  field: HTMLTextAreaElement;
  /** The counter / hint line under a limited field; null when the field has no limit. */
  status: HTMLElement | null;
}

/** The text last accepted into a field, to undo an edit that passes its limit. */
const accepted = new WeakMap<HTMLTextAreaElement, string>();

/** Is this textarea a one-line field? */
export function isLineField(e: Element | null | undefined): e is HTMLTextAreaElement {
  return e instanceof HTMLTextAreaElement && e.dataset.line === 'single';
}

const selectedText = (ta: HTMLTextAreaElement): string => ta.value.slice(ta.selectionStart ?? 0, ta.selectionEnd ?? 0);

/** Make the field as tall as its wrapped text (CSS bounds it; past the bound it scrolls). */
function grow(ta: HTMLTextAreaElement): void {
  if (!ta.isConnected) return;
  ta.style.height = 'auto';
  if (ta.scrollHeight === 0) return; // not laid out yet (a closed dialog)
  ta.style.height = `${ta.scrollHeight + (ta.offsetHeight - ta.clientHeight)}px`;
}

/** Tell the field its text was set from outside (a reload): forget the old text and redraw. */
export function resyncLineField(ta: HTMLTextAreaElement): void {
  ta.dispatchEvent(new CustomEvent('rsi-resync'));
}

/** Put the field's text in and place the caret at its end, as a form field opens. */
function caretToEnd(ta: HTMLTextAreaElement): void {
  const n = ta.value.length;
  ta.setSelectionRange(n, n);
}

function insertAtSelection(ta: HTMLTextAreaElement, text: string): void {
  ta.setRangeText(text, ta.selectionStart ?? ta.value.length, ta.selectionEnd ?? ta.value.length, 'end');
  ta.dispatchEvent(new Event('input', { bubbles: true }));
}

export function lineField(o: LineFieldOpts): LineField {
  const ta = el('textarea', 'line-field');
  ta.name = o.name;
  ta.rows = 1;
  ta.wrap = 'soft';
  ta.dataset.line = 'single';
  ta.spellcheck = false;
  ta.autocomplete = 'off';
  if (o.placeholder) ta.placeholder = o.placeholder;
  ta.value = flattenLines(o.value);
  accepted.set(ta, ta.value);

  const max = o.max ?? null;
  const from = o.counterFrom ?? ISSUE_TITLE_COUNTER_FROM;
  const status = max === null ? null : el('div', 'line-status');
  status?.setAttribute('role', 'status');
  const paint = (refused: boolean): void => {
    if (!status || max === null) return;
    const text = refused ? titleLimitHint(max) : counterText(charCount(ta.value), max, from);
    status.textContent = text;
    status.dataset.state = refused ? 'refused' : 'count';
    status.hidden = text === '';
    ta.toggleAttribute('data-limit-refused', refused);
  };
  paint(false);

  /** Settle the field after an edit that already happened; true when it was undone for passing the limit. */
  const settle = (): boolean => {
    const prev = accepted.get(ta) ?? '';
    const r = settleEdit(prev, ta.value, true, max);
    if (r.text !== ta.value) {
      const at = Math.min(ta.selectionStart ?? r.text.length, r.text.length);
      ta.value = r.text;
      ta.setSelectionRange(at, at);
    }
    accepted.set(ta, ta.value);
    paint(r.refused);
    grow(ta);
    return r.refused;
  };

  ta.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Enter' || ev.isComposing) return;
    // Enter submits, as in an <input>; no key adds a line.
    ev.preventDefault();
    if (!ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey) ta.form?.requestSubmit();
  });
  ta.addEventListener('beforeinput', (ev) => {
    if (ev.inputType === 'insertLineBreak' || ev.inputType === 'insertParagraph') { ev.preventDefault(); return; }
    if (max === null || ev.isComposing || !ev.data) return;
    if ((ev.inputType === 'insertText' || ev.inputType === 'insertReplacementText')
      && typingRefused(ta.value, charCount(selectedText(ta)), ev.data, max)) {
      ev.preventDefault();
      paint(true);
    }
  });
  ta.addEventListener('paste', (ev) => {
    const raw = ev.clipboardData?.getData('text/plain');
    if (raw === undefined || raw === '') return;
    const fit = fitPaste(raw, ta.value, charCount(selectedText(ta)), max, true);
    // An untouched paste keeps the browser's own undo history.
    if (fit.text === raw && !fit.cut) return;
    ev.preventDefault();
    if (fit.text !== '') insertAtSelection(ta, fit.text);
    if (fit.cut) paint(true);
  });
  ta.addEventListener('input', (ev) => {
    if ((ev as InputEvent).isComposing) return;
    if (settle()) ev.stopPropagation(); // an undone edit changed nothing: the form is not dirty for it
  });
  ta.addEventListener('compositionend', () => { settle(); });
  ta.addEventListener('rsi-resync', () => {
    ta.value = flattenLines(ta.value);
    accepted.set(ta, ta.value);
    paint(false);
    grow(ta);
  });

  // A field opens with the caret at the end of its text; a click keeps the place that was clicked.
  let pointer = false;
  ta.addEventListener('pointerdown', () => { pointer = true; });
  ta.addEventListener('focus', () => {
    if (!pointer) caretToEnd(ta);
    pointer = false;
    grow(ta);
  });
  ta.addEventListener('blur', () => { pointer = false; });
  if (typeof ResizeObserver !== 'undefined') {
    let width = -1;
    new ResizeObserver(() => {
      if (ta.clientWidth === width) return;
      width = ta.clientWidth;
      grow(ta);
    }).observe(ta);
  }
  return { field: ta, status };
}
