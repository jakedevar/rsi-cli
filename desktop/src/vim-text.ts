// Pure text helpers for the vim editor (vim-editor.ts): positions in code
// points, UTF-16 offsets for the DOM, character classes, word motions,
// paragraph and bracket motions, f/t/F/T search and the text objects. They
// mirror crates/rsi/src/vim_textarea/motions.rs and crates/rsi/src/text_objects.rs.
// No DOM; unit-tested under tests/vim-editor.test.mjs.

/** A cursor: row and column, the column in code points. */
export interface Pos {
  row: number;
  col: number;
}

/** A range of text, end inclusive (text_objects.rs `TextRange`). */
export interface TextRange {
  start: Pos;
  end: Pos;
}

/** The buffer: one code-point array per line. */
export type Lines = string[][];

export function toLines(text: string): Lines {
  return text.split('\n').map((l) => [...l]);
}

export function fromLines(lines: Lines): string {
  return lines.map((l) => l.join('')).join('\n');
}

export const cmp = (a: Pos, b: Pos): number => a.row - b.row || a.col - b.col;

/** The UTF-16 offset of a position (what a textarea's selectionStart counts). */
export function posToOffset(lines: Lines, p: Pos): number {
  let off = 0;
  for (let r = 0; r < p.row && r < lines.length; r++) off += lines[r]!.join('').length + 1;
  const line = lines[p.row] ?? [];
  for (let c = 0; c < p.col && c < line.length; c++) off += line[c]!.length;
  return off;
}

/** The position of a UTF-16 offset (an offset inside a surrogate pair rounds down). */
export function offsetToPos(lines: Lines, offset: number): Pos {
  let left = Math.max(0, offset);
  for (let r = 0; r < lines.length; r++) {
    const line = lines[r]!;
    let c = 0;
    for (; c < line.length; c++) {
      const w = line[c]!.length;
      if (left < w) return { row: r, col: c };
      left -= w;
    }
    if (left === 0 || r === lines.length - 1) return { row: r, col: c };
    left -= 1; // the newline
  }
  return { row: 0, col: 0 };
}

export type CharClass = 'word' | 'space' | 'punct';

/** text_objects.rs `char_class`: alphanumerics and `_` are words. */
export function charClass(ch: string): CharClass {
  if (/^[\p{L}\p{N}_]$/u.test(ch)) return 'word';
  if (/^\s$/u.test(ch)) return 'space';
  return 'punct';
}

const isBlank = (line: readonly string[]): boolean => line.every((c) => /\s/u.test(c));

/** The class at p; past the end of a line it is a line break (`nl`). */
function classAt(lines: Lines, p: Pos): CharClass | 'nl' {
  const ch = lines[p.row]?.[p.col];
  return ch === undefined ? 'nl' : charClass(ch);
}

function lastPos(lines: Lines): Pos {
  const row = lines.length - 1;
  return { row, col: Math.max(0, lines[row]!.length - 1) };
}

/** The next character position, crossing line breaks (an empty line is one stop). */
function next(lines: Lines, p: Pos): Pos | null {
  const len = lines[p.row]!.length;
  if (p.col + 1 < len) return { row: p.row, col: p.col + 1 };
  if (p.row + 1 < lines.length) return { row: p.row + 1, col: 0 };
  return null;
}

function prev(lines: Lines, p: Pos): Pos | null {
  if (p.col > 0) return { row: p.row, col: Math.min(p.col - 1, Math.max(0, lines[p.row]!.length - 1)) };
  if (p.row > 0) return { row: p.row - 1, col: Math.max(0, lines[p.row - 1]!.length - 1) };
  return null;
}

const emptyLine = (lines: Lines, p: Pos): boolean => lines[p.row]!.length === 0;

/** `w`: the start of the next word; an empty line is a word. At the last word it stops at the end. */
export function wordForward(lines: Lines, from: Pos): Pos {
  let p: Pos | null = from;
  const cls = classAt(lines, from);
  if (cls === 'word' || cls === 'punct') {
    while (p && p.row === from.row && classAt(lines, p) === cls) {
      const n = next(lines, p);
      if (!n) return { row: p.row, col: lines[p.row]!.length };
      p = n;
    }
  } else {
    p = next(lines, from);
    if (!p) return from;
  }
  // Skip blanks and line breaks; an empty line stops.
  while (p) {
    if (emptyLine(lines, p) && cmp(p, from) > 0) return p;
    if (classAt(lines, p) !== 'space') return p;
    const n = next(lines, p);
    if (!n) return { row: p.row, col: lines[p.row]!.length };
    p = n;
  }
  return from;
}

/** `e`: the end of this or the next word. */
export function wordEnd(lines: Lines, from: Pos): Pos {
  let p = next(lines, from);
  if (!p) return from;
  while (p && (classAt(lines, p) === 'space' || emptyLine(lines, p))) {
    const n = next(lines, p);
    if (!n) return p;
    p = n;
  }
  const cls = classAt(lines, p);
  for (;;) {
    const n: Pos | null = p.col + 1 < lines[p.row]!.length ? { row: p.row, col: p.col + 1 } : null;
    if (!n || classAt(lines, n) !== cls) return p;
    p = n;
  }
}

/** `b`: the start of this or the previous word; an empty line is a word. */
export function wordBack(lines: Lines, from: Pos): Pos {
  let p = prev(lines, from);
  if (!p) return { row: 0, col: 0 };
  while (p && classAt(lines, p) === 'space' && !emptyLine(lines, p)) {
    const n = prev(lines, p);
    if (!n) return { row: 0, col: 0 };
    p = n;
  }
  if (emptyLine(lines, p)) return { row: p.row, col: 0 };
  const cls = classAt(lines, p);
  while (p.col > 0 && classAt(lines, { row: p.row, col: p.col - 1 }) === cls) p = { row: p.row, col: p.col - 1 };
  return p;
}

/** `^`: the first non-blank column of a row. */
export function firstNonBlank(lines: Lines, row: number): number {
  const line = lines[row] ?? [];
  const i = line.findIndex((c) => !/\s/u.test(c));
  return i < 0 ? 0 : i;
}

/** `{`: the previous blank line (motions.rs `move_paragraph_backward`). */
export function paragraphBack(lines: Lines, row: number): number {
  if (row === 0) return 0;
  let inContent = !isBlank(lines[row]!);
  for (let i = row - 1; i >= 0; i--) {
    if (isBlank(lines[i]!)) {
      if (inContent) return i;
    } else {
      inContent = true;
    }
  }
  return 0;
}

/** `}`: the next blank line, else the last line (`move_paragraph_forward`). */
export function paragraphForward(lines: Lines, row: number): number {
  if (row + 1 >= lines.length) return row;
  let inContent = !isBlank(lines[row]!);
  for (let i = row + 1; i < lines.length; i++) {
    if (isBlank(lines[i]!)) {
      if (inContent) return i;
    } else {
      inContent = true;
    }
  }
  return lines.length - 1;
}

const PAIRS: Readonly<Record<string, [string, string, boolean]>> = {
  '(': ['(', ')', true], '[': ['[', ']', true], '{': ['{', '}', true],
  ')': ['(', ')', false], ']': ['[', ']', false], '}': ['{', '}', false],
};

/** `%`: the bracket matching the nearest one at or after the cursor on its line (else before it). */
export function matchBracket(lines: Lines, from: Pos): Pos | null {
  const chars = lines[from.row]!;
  let at = -1;
  for (let i = from.col; i < chars.length; i++) if (PAIRS[chars[i]!]) { at = i; break; }
  if (at < 0) for (let i = Math.min(from.col, chars.length) - 1; i >= 0; i--) if (PAIRS[chars[i]!]) { at = i; break; }
  if (at < 0) return null;
  const [open, close, forward] = PAIRS[chars[at]!]!;
  let depth = 0;
  if (forward) {
    for (let r = from.row; r < lines.length; r++) {
      const l = lines[r]!;
      for (let c = r === from.row ? at : 0; c < l.length; c++) {
        if (l[c] === open) depth++;
        else if (l[c] === close && --depth === 0) return { row: r, col: c };
      }
    }
  } else {
    for (let r = from.row; r >= 0; r--) {
      const l = lines[r]!;
      for (let c = r === from.row ? at : l.length - 1; c >= 0; c--) {
        if (l[c] === close) depth++;
        else if (l[c] === open && --depth === 0) return { row: r, col: c };
      }
    }
  }
  return { row: from.row, col: at };
}

export type SearchDir = 'f' | 't' | 'F' | 'T';

export interface CharSearch {
  dir: SearchDir;
  ch: string;
}

export const reverseDir = (d: SearchDir): SearchDir => ({ f: 'F', t: 'T', F: 'f', T: 't' } as const)[d];

/**
 * f/t/F/T on the cursor's line (motions.rs `execute_char_search`). A repeat
 * (`;` `,`) skips an adjacent target so `t` makes progress. Null: not found.
 */
export function charSearch(lines: Lines, from: Pos, s: CharSearch, repeat: boolean): number | null {
  const line = lines[from.row]!;
  if (s.dir === 'f' || s.dir === 't') {
    for (let i = from.col + 1; i < line.length; i++) {
      if (line[i] !== s.ch) continue;
      const target = s.dir === 't' ? i - 1 : i;
      if (repeat && target <= from.col) continue;
      return target;
    }
  } else {
    for (let i = Math.min(from.col, line.length) - 1; i >= 0; i--) {
      if (line[i] !== s.ch) continue;
      const target = s.dir === 'T' ? i + 1 : i;
      if (repeat && target >= from.col) continue;
      return target;
    }
  }
  return null;
}

// ---------- text objects (text_objects.rs) ----------

export type ObjectKind = 'i' | 'a';

/** `iw` / `aw`: `aw` takes trailing blanks, else leading ones. */
export function wordObject(lines: Lines, p: Pos, kind: ObjectKind): TextRange | null {
  const chars = lines[p.row];
  if (!chars || chars.length === 0) return null;
  const col = Math.min(p.col, chars.length - 1);
  const cls = charClass(chars[col]!);
  let start = col;
  while (start > 0 && charClass(chars[start - 1]!) === cls) start--;
  let end = col;
  while (end + 1 < chars.length && charClass(chars[end + 1]!) === cls) end++;
  if (kind === 'i') return { start: { row: p.row, col: start }, end: { row: p.row, col: end } };
  const blank = (c: string | undefined) => c === ' ' || c === '\t';
  let te = end;
  while (blank(chars[te + 1])) te++;
  if (te > end) return { start: { row: p.row, col: start }, end: { row: p.row, col: te } };
  let ls = start;
  while (ls > 0 && blank(chars[ls - 1])) ls--;
  return { start: { row: p.row, col: ls }, end: { row: p.row, col: end } };
}

/** Quotes pair up in order on the line: 0-1, 2-3, … (`find_quote_pair`). */
function quoteObject(lines: Lines, p: Pos, q: string, kind: ObjectKind): TextRange | null {
  const chars = lines[p.row] ?? [];
  const at = chars.flatMap((c, i) => (c === q ? [i] : []));
  for (let i = 0; i + 1 < at.length; i += 2) {
    const [o, c] = [at[i]!, at[i + 1]!];
    if (o <= p.col && p.col <= c) {
      return kind === 'i'
        ? { start: { row: p.row, col: o + 1 }, end: { row: p.row, col: c - 1 } }
        : { start: { row: p.row, col: o }, end: { row: p.row, col: c } };
    }
  }
  return null;
}

/** A nesting bracket pair around p, across lines (`bracket_object`). */
function bracketObject(lines: Lines, p: Pos, open: string, close: string, kind: ObjectKind): TextRange | null {
  let depth = 0;
  let o: Pos | null = null;
  outer: for (let r = p.row; r >= 0; r--) {
    const l = lines[r]!;
    for (let c = r === p.row ? Math.min(p.col, l.length - 1) : l.length - 1; c >= 0; c--) {
      if (l[c] === close && !(r === p.row && c === p.col)) depth++;
      else if (l[c] === open) {
        if (depth === 0) { o = { row: r, col: c }; break outer; }
        depth--;
      }
    }
  }
  if (!o) return null;
  depth = 0;
  let cl: Pos | null = null;
  outer2: for (let r = o.row; r < lines.length; r++) {
    const l = lines[r]!;
    for (let c = r === o.row ? o.col : 0; c < l.length; c++) {
      if (l[c] === open) depth++;
      else if (l[c] === close && --depth === 0) { cl = { row: r, col: c }; break outer2; }
    }
  }
  if (!cl) return null;
  if (kind === 'a') return { start: o, end: cl };
  return { start: { row: o.row, col: o.col + 1 }, end: { row: cl.row, col: cl.col - 1 } };
}

const isSentenceEnd = (c: string): boolean => c === '.' || c === '!' || c === '?';

/** `is` / `as` over the buffer flattened with `\n` (`sentence_object`). */
function sentenceObject(lines: Lines, p: Pos, kind: ObjectKind): TextRange | null {
  const flat = lines.flatMap((l, i) => (i < lines.length - 1 ? [...l, '\n'] : l));
  let offset = 0;
  for (let r = 0; r < p.row; r++) offset += lines[r]!.length + 1;
  offset += p.col;
  if (offset >= flat.length) return null;
  let start = -1;
  for (let i = offset - 1; i >= 0; i--) {
    if (isSentenceEnd(flat[i]!)) {
      start = i + 1;
      while (start < offset && /\s/u.test(flat[start]!)) start++;
      break;
    }
  }
  if (start < 0) {
    start = 0;
    while (start < offset && /\s/u.test(flat[start]!)) start++;
  }
  let end = flat.length - 1;
  for (let i = offset; i < flat.length; i++) if (isSentenceEnd(flat[i]!)) { end = i; break; }
  if (kind === 'a') while (end + 1 < flat.length && /\s/u.test(flat[end + 1]!)) end++;
  const at = (off: number): Pos => {
    let left = off;
    for (let r = 0; r < lines.length; r++) {
      if (left <= lines[r]!.length) return { row: r, col: left };
      left -= lines[r]!.length + 1;
    }
    return lastPos(lines);
  };
  return { start: at(start), end: at(end) };
}

/** `ip` / `ap`: the run of non-blank (or blank) lines; `ap` takes trailing blank lines. */
function paragraphObject(lines: Lines, p: Pos, kind: ObjectKind): TextRange | null {
  if (p.row >= lines.length) return null;
  const blank = (r: number) => isBlank(lines[r]!);
  const want = blank(p.row);
  let s = p.row;
  while (s > 0 && blank(s - 1) === want) s--;
  let e = p.row;
  while (e + 1 < lines.length && blank(e + 1) === want) e++;
  if (kind === 'a' && !want) while (e + 1 < lines.length && blank(e + 1)) e++;
  return { start: { row: s, col: 0 }, end: { row: e, col: Math.max(0, lines[e]!.length - 1) } };
}

/** Whether a text object spans whole lines (`ip`, `ap`). */
export const linewiseObject = (key: string): boolean => key === 'p';

/** The text object a key names after `i` / `a`, or null (unknown key or none at the cursor). */
export function textObject(lines: Lines, p: Pos, kind: ObjectKind, key: string): TextRange | null {
  switch (key) {
    case 'w': return wordObject(lines, p, kind);
    case '"': case "'": case '`': return quoteObject(lines, p, key, kind);
    case '(': case ')': case 'b': return bracketObject(lines, p, '(', ')', kind);
    case '{': case '}': case 'B': return bracketObject(lines, p, '{', '}', kind);
    case '[': case ']': return bracketObject(lines, p, '[', ']', kind);
    case '<': case '>': return bracketObject(lines, p, '<', '>', kind);
    case 's': return sentenceObject(lines, p, kind);
    case 'p': return paragraphObject(lines, p, kind);
    default: return null;
  }
}

/** Is a key one that names a text object after `i` / `a`? */
export const isObjectKey = (key: string): boolean => /^[w"'`()b{}B[\]<>sp]$/.test(key);

/**
 * vim_textarea `finalize_insert_from_snapshot`: what an insert session typed,
 * from the text before and after it (common prefix and suffix removed).
 */
export function insertedText(before: string, after: string): string {
  const a = [...before];
  const b = [...after];
  if (b.length <= a.length) return '';
  let pre = 0;
  while (pre < a.length && a[pre] === b[pre]) pre++;
  let suf = 0;
  while (suf < a.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;
  return b.slice(pre, b.length - suf).join('');
}
