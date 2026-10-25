// Pure rules for the text fields of the Issue editor and the project form (hub #3421, desktop #317).
// The TUI keeps these in crates/rsi/src/form_text.rs; the desktop mirrors them: a one-line field wraps
// on screen but never holds a newline, and a field with a limit refuses input past it. The DOM side
// (form-field.ts) applies them to a textarea; the Vim editing itself stays in vim-editor.ts.

/** The longest Issue title the editor accepts, in characters. The daemon's own limit is larger (issues-model.ts). */
export const ISSUE_TITLE_LIMIT = 200;

/** The title counter shows from this many characters. */
export const ISSUE_TITLE_COUNTER_FROM = 150;

/** The hint shown when the limit refuses input (the TUI's wording). */
export function titleLimitHint(max: number = ISSUE_TITLE_LIMIT): string {
  return `TITLE LIMIT — titles are cut at ${max} characters; input refused`;
}

/** Characters as a person counts them (code points, like the TUI's `chars()`), not UTF-16 units. */
export function charCount(s: string): number {
  return Array.from(s).length;
}

/** The first `n` characters of `s`. */
export function takeChars(s: string, n: number): string {
  if (n <= 0) return '';
  let out = '';
  let i = 0;
  for (const c of s) {
    if (i++ >= n) break;
    out += c;
  }
  return out;
}

/** A one-line field's text: every line break becomes a space, so offsets stay put (`\r\n` counts as one). */
export function flattenLines(s: string): string {
  return s.replace(/\r\n|\r|\n/g, ' ');
}

/** The counter text under a limited field: empty below `from`, else `n/max`. */
export function counterText(len: number, max: number, from: number): string {
  return len >= from ? `${len}/${max}` : '';
}

export interface Fit {
  /** What goes into the field. */
  text: string;
  /** Part of the input was cut or the whole edit was refused. */
  cut: boolean;
}

/**
 * Room for pasted text: a paste into `current` (replacing the selection of `selected` characters) keeps only
 * what fits under `max`. A field already over its limit takes nothing more but may still be shortened.
 */
export function fitPaste(pasted: string, current: string, selected: number, max: number | null, single: boolean): Fit {
  let text = single ? flattenLines(pasted.replace(/\r\n?/g, '\n')) : pasted.replace(/\r\n?/g, '\n');
  if (max === null) return { text, cut: false };
  const room = Math.max(0, max - (charCount(current) - selected));
  if (charCount(text) <= room) return { text, cut: false };
  text = takeChars(text, room);
  return { text, cut: true };
}

/** Would typing `typed` over a selection of `selected` characters pass the limit (and grow the text)? */
export function typingRefused(current: string, selected: number, typed: string, max: number | null): boolean {
  if (max === null) return false;
  const next = charCount(current) - selected + charCount(typed);
  return next > max && next > charCount(current);
}

/**
 * Settle a field after any edit that already happened (a Vim command, a drop, a script): a one-line field
 * loses its line breaks, and an edit that grew the text past the limit is undone whole (the TUI's
 * "refused"). An edit that only shortens a text already over the limit is allowed, so an older, longer
 * title can always be cut down.
 */
export function settleEdit(prev: string, next: string, single: boolean, max: number | null): { text: string; refused: boolean } {
  const text = single ? flattenLines(next) : next;
  if (max !== null) {
    const n = charCount(text);
    if (n > max && n > charCount(prev)) return { text: single ? flattenLines(prev) : prev, refused: true };
  }
  return { text, refused: false };
}
