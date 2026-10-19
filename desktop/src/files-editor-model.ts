// What the file editor adds to the viewer while editing (#134, hub #3012), pure
// (no DOM): bracket auto-pairing, the paste, the git gutter signs carried over
// the unsaved edits, keeping the cursor off folded lines and the visual
// selection as per-line decorations. The DOM side is files.ts. Mirrors
// crates/rsi/src/file_viewer.rs (`handle_auto_pair`, `try_paste_text_file_viewer`,
// `skip_folded_cursor`).
import { commonEnds } from './files-edit-model.js';
import type { GutterSign } from './files-model.js';
import { type Lines, offsetToPos } from './vim-text.js';

const PAIRS: Readonly<Record<string, string>> = { '{': '}', '(': ')', '[': ']' };
const CLOSERS = new Set(Object.values(PAIRS));

/** One change to the text box: replace [from, to) with `insert`, then put the caret at `caret`. */
export interface TextEdit { from: number; to: number; insert: string; caret: number }

/**
 * Bracket auto-pairing as the TUI does it (`handle_auto_pair`), brackets only:
 * an opener types the pair with the caret between (unless the closer is
 * already next), a closer types over a matching closer, and Backspace between
 * an empty pair removes both. `null` leaves the key to the text box. A
 * selection is replaced natively, so it is never paired.
 */
export function autoPairEdit(text: string, start: number, end: number, key: string): TextEdit | null {
  if (start !== end) return null;
  const next = text[start];
  if (key in PAIRS) {
    const close = PAIRS[key]!;
    if (next === close) return null;
    return { from: start, to: start, insert: key + close, caret: start + 1 };
  }
  if (CLOSERS.has(key)) return next === key ? { from: start, to: start, insert: '', caret: start + 1 } : null;
  if (key === 'Backspace') {
    const prev = start > 0 ? text[start - 1] : undefined;
    if (prev !== undefined && prev in PAIRS && next === PAIRS[prev]) return { from: start - 1, to: start + 1, insert: '', caret: start - 1 };
  }
  return null;
}

/** Clipboard text for the buffer: LF breaks, as a text box holds them. */
export const pastedText = (clip: string): string => clip.replace(/\r\n|\r/g, '\n');

/**
 * The git gutter signs of the saved text, carried over the unsaved edits: lines
 * kept at the top keep their sign, lines kept at the bottom move with their
 * line, and the lines changed in between show none until the next save.
 */
export function shiftSigns(
  signs: ReadonlyArray<GutterSign | null>, base: readonly string[], next: readonly string[],
): Array<GutterSign | null> {
  const { prefix, suffix } = commonEnds(base, next);
  const out: Array<GutterSign | null> = new Array(next.length).fill(null);
  for (let i = 0; i < prefix; i++) out[i] = signs[i] ?? null;
  for (let k = 1; k <= suffix; k++) out[next.length - k] = signs[base.length - k] ?? null;
  return out;
}

/**
 * Move a cursor that landed on a hidden (folded) line to a shown one: down onto
 * the first shown line after it, up onto the last shown line before it (the
 * fold's own line). `shown` is the ascending list of shown line indexes.
 */
export function skipHidden(shown: readonly number[], row: number, from: number): number {
  if (shown.length === 0 || shown.includes(row)) return row;
  if (row > from) return shown.find((r) => r > row) ?? shown[shown.length - 1]!;
  for (let i = shown.length - 1; i >= 0; i--) if (shown[i]! < row) return shown[i]!;
  return shown[0]!;
}

/** A selected stretch of one line, in code points; `to` past the line's length also marks the break. */
export interface RowSelection { row: number; from: number; to: number }

/**
 * The visual selection [start, end) (UTF-16 offsets into the text) as per-line
 * stretches for the renderer; a stretch that runs past a line's end covers its
 * line break too, so a selected empty line still shows.
 */
export function selectionRows(lines: Lines, text: string, start: number, end: number): RowSelection[] {
  if (end <= start) return [];
  const a = offsetToPos(lines, start);
  const b = offsetToPos(lines, Math.min(end, text.length));
  const out: RowSelection[] = [];
  for (let row = a.row; row <= b.row && row < lines.length; row++) {
    const len = lines[row]!.length;
    const from = row === a.row ? a.col : 0;
    const to = row === b.row ? b.col : len + 1;
    if (to > from) out.push({ row, from, to });
  }
  return out;
}
