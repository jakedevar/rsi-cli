// The file viewer's reading tools (gap sweep G4, hub #3000), pure (no DOM):
// column motions, search (/ ? n N * #), folds (za zo zc zM zR), a small
// syntax highlighter for the TUI's tree-sitter languages, bracket matching and
// the segmentation the renderer draws. Mirrors crates/rsi/src/file_viewer.rs
// (search, folds, cursor skipping) and ui/treesitter.rs (the languages and
// their highlight roles). files.ts draws it; tests/viewer.test.mjs pins it.
//
// Columns are code points (vim-text.ts `Pos`); a renderer that slices a JS
// string converts with `utf16Col`.
import { type Lines, type Pos, firstNonBlank, matchBracket, paragraphBack, paragraphForward, wordBack, wordEnd, wordForward } from './vim-text.js';

// ---------- columns ----------

/** The UTF-16 offset of code-point column `col` in `line`. */
export function utf16Col(line: string, col: number): number {
  let off = 0;
  let cp = 0;
  for (const ch of line) {
    if (cp >= col) break;
    off += ch.length;
    cp++;
  }
  return off;
}

/** The code-point column of UTF-16 offset `off` in `line`. */
export function cpCol(line: string, off: number): number {
  let cp = 0;
  let at = 0;
  for (const ch of line) {
    if (at >= off) break;
    at += ch.length;
    cp++;
  }
  return cp;
}

/** The last column the normal-mode cursor may sit on (0 on an empty line). */
export function lastCol(lines: Lines, row: number): number {
  return Math.max(0, (lines[row]?.length ?? 0) - 1);
}

export type ColMotion = 'h' | 'l' | 'w' | 'b' | 'e' | 'W' | 'B' | 'E' | '0' | '^' | '$' | '{' | '}' | '%';

const isBlankChar = (c: string | undefined): boolean => c === undefined || /\s/u.test(c);

/** `W`: the start of the next WORD (blank-separated). */
function bigWordForward(lines: Lines, from: Pos): Pos {
  let { row, col } = from;
  const line = lines[row] ?? [];
  while (col < line.length && !isBlankChar(line[col])) col++;
  for (;;) {
    const l = lines[row] ?? [];
    while (col < l.length && isBlankChar(l[col])) col++;
    if (col < l.length) return { row, col };
    if (row + 1 >= lines.length) return { row, col: lastColOf(l) };
    row++;
    col = 0;
    if ((lines[row] ?? []).length === 0) return { row, col: 0 };
  }
}

const lastColOf = (l: readonly string[]): number => Math.max(0, l.length - 1);

/** `B`: the start of this or the previous WORD. */
function bigWordBack(lines: Lines, from: Pos): Pos {
  let { row, col } = from;
  col--;
  for (;;) {
    const l = lines[row] ?? [];
    while (col >= 0 && isBlankChar(l[col])) col--;
    if (col >= 0) {
      while (col > 0 && !isBlankChar(l[col - 1])) col--;
      return { row, col };
    }
    if (row === 0) return { row: 0, col: 0 };
    row--;
    const p = lines[row] ?? [];
    if (p.length === 0) return { row, col: 0 };
    col = p.length - 1;
  }
}

/** `E`: the end of this or the next WORD. */
function bigWordEnd(lines: Lines, from: Pos): Pos {
  let { row, col } = from;
  col++;
  for (;;) {
    const l = lines[row] ?? [];
    while (col < l.length && isBlankChar(l[col])) col++;
    if (col < l.length) {
      while (col + 1 < l.length && !isBlankChar(l[col + 1])) col++;
      return { row, col };
    }
    if (row + 1 >= lines.length) return { row, col: lastColOf(l) };
    row++;
    col = 0;
  }
}

/**
 * One cursor motion of the read-only viewer, `n` times, clamped to a
 * character (vim normal mode never rests past the last one). Vertical
 * motions keep their own column; this is the horizontal and word set the
 * TUI's vim textarea gives the viewer.
 */
export function colMotion(lines: Lines, from: Pos, m: ColMotion, n = 1): Pos {
  if (lines.length === 0) return { row: 0, col: 0 };
  let p: Pos = { row: Math.min(from.row, lines.length - 1), col: from.col };
  const times = Math.max(1, Math.min(n, 9999));
  switch (m) {
    case 'h': return { row: p.row, col: Math.max(0, Math.min(p.col, lastCol(lines, p.row)) - times) };
    case 'l': return { row: p.row, col: Math.min(lastCol(lines, p.row), p.col + times) };
    case '0': return { row: p.row, col: 0 };
    case '^': return { row: p.row, col: firstNonBlank(lines, p.row) };
    case '$': {
      const row = Math.min(lines.length - 1, p.row + times - 1);
      return { row, col: lastCol(lines, row) };
    }
    case '{': {
      let row = p.row;
      for (let i = 0; i < times; i++) row = paragraphBack(lines, row);
      return { row, col: 0 };
    }
    case '}': {
      let row = p.row;
      for (let i = 0; i < times; i++) row = paragraphForward(lines, row);
      return { row, col: 0 };
    }
    case '%': return matchBracket(lines, p) ?? p;
    default: break;
  }
  const step: (l: Lines, at: Pos) => Pos = m === 'w' ? wordForward : m === 'b' ? wordBack : m === 'e' ? wordEnd
    : m === 'W' ? bigWordForward : m === 'B' ? bigWordBack : bigWordEnd;
  for (let i = 0; i < times; i++) p = step(lines, p);
  return { row: p.row, col: Math.min(p.col, lastCol(lines, p.row)) };
}

// ---------- search ----------

export interface Match { row: number; start: number; end: number }

/** Matches beyond this are not collected (a one-character pattern in a 4 MiB file). */
export const MAX_MATCHES = 20_000;

/**
 * Every occurrence of `pattern` (literal, case-sensitive, overlapping, as the
 * TUI's `recompute_search_matches`), columns in code points.
 */
export function findMatches(lines: readonly string[], pattern: string): Match[] {
  const out: Match[] = [];
  if (pattern === '') return out;
  const width = [...pattern].length;
  for (let row = 0; row < lines.length; row++) {
    const line = lines[row]!;
    let from = 0;
    for (;;) {
      const at = line.indexOf(pattern, from);
      if (at < 0) break;
      const start = cpCol(line, at);
      out.push({ row, start, end: start + width });
      if (out.length >= MAX_MATCHES) return out;
      from = at + 1;
    }
  }
  return out;
}

const before = (m: Match, p: Pos): boolean => m.row < p.row || (m.row === p.row && m.start < p.col);

/**
 * The match a search lands on from the cursor (`jump_to_first_match_from_cursor`):
 * forward the first at or after it, backward the last before it, wrapping.
 * `strict` skips the match under the cursor (`*` / `#`).
 */
export function matchFrom(matches: readonly Match[], cursor: Pos, forward: boolean, strict = false): number {
  if (matches.length === 0) return -1;
  if (forward) {
    const i = matches.findIndex((m) => (strict ? m.row > cursor.row || (m.row === cursor.row && m.start > cursor.col) : !before(m, cursor)));
    return i < 0 ? 0 : i;
  }
  for (let i = matches.length - 1; i >= 0; i--) if (before(matches[i]!, cursor)) return i;
  return matches.length - 1;
}

/** `n` / `N`: the next or previous match, wrapping (`advance_search_match`). */
export function advanceMatch(current: number, count: number, forward: boolean): number {
  if (count <= 0) return -1;
  if (current < 0) return forward ? 0 : count - 1;
  return forward ? (current + 1) % count : (current + count - 1) % count;
}

const isWordChar = (c: string | undefined): boolean => c !== undefined && /[\p{L}\p{N}_]/u.test(c);

/** The word under the cursor for `*` / `#` (letters, digits and `_`), or null. */
export function wordUnderCursor(lines: Lines, p: Pos): string | null {
  const l = lines[p.row];
  if (!l || !isWordChar(l[p.col])) return null;
  let s = p.col;
  let e = p.col;
  while (s > 0 && isWordChar(l[s - 1])) s--;
  while (e + 1 < l.length && isWordChar(l[e + 1])) e++;
  return l.slice(s, e + 1).join('');
}

// ---------- folds ----------

/** A foldable region: `start` stays visible, `start+1 ..= end` hide when folded (FoldState). */
export type Region = readonly [number, number];

export class Folds {
  regions: Region[] = [];
  private folded = new Set<string>();
  private cache: number[] | null = null;
  private cacheLines = -1;

  static key(r: Region): string { return `${r[0]}:${r[1]}`; }

  setRegions(regions: Region[]): void {
    this.regions = regions;
    const keep = new Set(regions.map(Folds.key));
    for (const k of [...this.folded]) if (!keep.has(k)) this.folded.delete(k);
    this.cache = null;
  }

  isFolded(r: Region): boolean { return this.folded.has(Folds.key(r)); }

  foldedCount(): number { return this.folded.size; }

  /** The innermost region starting on `row` (`fold_region_at_cursor`). */
  regionAt(row: number): Region | null {
    let best: Region | null = null;
    for (const r of this.regions) if (r[0] === row && (!best || r[1] - r[0] < best[1] - best[0])) best = r;
    return best;
  }

  private change(f: () => void): void { f(); this.cache = null; }

  /** `za`: toggle the region on the row; false when none starts there. */
  toggle(row: number): boolean {
    const r = this.regionAt(row);
    if (!r) return false;
    const k = Folds.key(r);
    this.change(() => { if (this.folded.has(k)) this.folded.delete(k); else this.folded.add(k); });
    return true;
  }

  /** `zc` (close) / `zo` (open) on the row. */
  set(row: number, close: boolean): boolean {
    const r = this.regionAt(row);
    if (!r) return false;
    this.change(() => { if (close) this.folded.add(Folds.key(r)); else this.folded.delete(Folds.key(r)); });
    return true;
  }

  /** `zM`. */
  closeAll(): void { this.change(() => { this.folded = new Set(this.regions.map(Folds.key)); }); }

  /** `zR`. */
  openAll(): void { this.change(() => { this.folded.clear(); }); }

  /** Open every fold hiding `row` (a search hit or `:N` inside a fold). */
  reveal(row: number): void {
    const hide = this.regions.filter((r) => this.folded.has(Folds.key(r)) && row > r[0] && row <= r[1]);
    if (hide.length) this.change(() => { for (const r of hide) this.folded.delete(Folds.key(r)); });
  }

  /** Hidden lines below `row` when a closed fold starts there (`fold_summary_at`), else null. */
  summaryAt(row: number): number | null {
    let best: number | null = null;
    for (const r of this.regions) if (r[0] === row && this.folded.has(Folds.key(r))) best = Math.max(best ?? 0, r[1] - r[0]);
    return best;
  }

  /** The line indexes left on screen, in order (folded interiors removed). */
  display(lineCount: number): number[] {
    if (this.cache && this.cacheLines === lineCount) return this.cache;
    const hidden = new Uint8Array(Math.max(0, lineCount));
    for (const r of this.regions) {
      if (!this.folded.has(Folds.key(r))) continue;
      for (let i = r[0] + 1; i <= r[1] && i < lineCount; i++) hidden[i] = 1;
    }
    const out: number[] = [];
    for (let i = 0; i < lineCount; i++) if (!hidden[i]) out.push(i);
    this.cache = out;
    this.cacheLines = lineCount;
    return out;
  }
}

/** The display index of `line`, or the nearest shown line at or before it. */
export function displayIndex(display: readonly number[], line: number): number {
  let lo = 0;
  let hi = display.length - 1;
  let best = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (display[mid]! <= line) { best = mid; lo = mid + 1; } else hi = mid - 1;
  }
  return best;
}

// ---------- languages ----------

export type Lang =
  | 'rust' | 'python' | 'javascript' | 'typescript' | 'bash' | 'json' | 'toml' | 'yaml' | 'go' | 'c' | 'cpp' | 'markdown';

/** The TUI's tree-sitter registry (ui/treesitter.rs), by extension. */
const EXTENSIONS: Readonly<Record<string, Lang>> = {
  rs: 'rust', py: 'python', pyi: 'python', js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'javascript',
  ts: 'typescript', mts: 'typescript', cts: 'typescript', tsx: 'typescript',
  sh: 'bash', bash: 'bash', zsh: 'bash', fish: 'bash', json: 'json', jsonc: 'json', toml: 'toml', yaml: 'yaml', yml: 'yaml',
  go: 'go', c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', cxx: 'cpp', hpp: 'cpp', hxx: 'cpp', md: 'markdown', markdown: 'markdown',
};

export function languageOf(path: string): Lang | null {
  const name = path.split('/').pop() ?? '';
  const dot = name.lastIndexOf('.');
  if (dot <= 0) return null;
  return EXTENSIONS[name.slice(dot + 1).toLowerCase()] ?? null;
}

export const isMarkdown = (path: string): boolean => languageOf(path) === 'markdown';

/** Highlight roles (ui/treesitter.rs `ts_highlight_to_style`; the renderer maps each to a palette colour). */
export type Tok =
  | 'keyword' | 'function' | 'string' | 'number' | 'type' | 'comment' | 'operator' | 'constant' | 'property'
  | 'punctuation' | 'attribute' | 'tag' | 'variable' | 'heading' | 'emphasis' | 'strong' | 'code' | 'link';

/** A highlighted run of one line, columns in UTF-16 offsets. */
export interface Span { from: number; to: number; tok: Tok }

interface Spec {
  line: string[];
  block?: readonly [string, string];
  nestBlock?: boolean;
  quotes: string;
  /** Delimiters of strings that may span lines. */
  multi?: readonly string[];
  keywords: ReadonlySet<string>;
  types: ReadonlySet<string>;
  constants: ReadonlySet<string>;
  /** A capitalised identifier is a type (Rust, Go, TS). */
  capsTypes?: boolean;
  /** Rust `name!`, C `#include`, Python `@decorator`, Rust `#[attr]`. */
  macros?: 'rust' | 'c' | 'python' | null;
  /** `'a'` is a char literal and `'a` a lifetime (Rust). */
  lifetimes?: boolean;
}

const set = (s: string): ReadonlySet<string> => new Set(s.split(/\s+/).filter(Boolean));

const C_LIKE_TYPES = set('int char short long float double void unsigned signed bool size_t uint8_t uint16_t uint32_t uint64_t int8_t int16_t int32_t int64_t');

const SPECS: Readonly<Record<Exclude<Lang, 'json' | 'toml' | 'yaml' | 'markdown'>, Spec>> = {
  rust: {
    line: ['//'], block: ['/*', '*/'], nestBlock: true, quotes: '"\'', capsTypes: true, macros: 'rust', lifetimes: true,
    keywords: set('as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait type union unsafe use where while yield'),
    types: set('bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 String Vec Option Result Box'),
    constants: set('true false None Some Ok Err'),
  },
  python: {
    line: ['#'], quotes: '"\'', multi: ['"""', "'''"], macros: 'python',
    keywords: set('and as assert async await break class continue def del elif else except finally for from global if import in is lambda match case nonlocal not or pass raise return try while with yield'),
    types: set('int float str bytes bool list dict set tuple object type'),
    constants: set('True False None self cls'),
  },
  javascript: {
    line: ['//'], block: ['/*', '*/'], quotes: '"\'`', multi: ['`'],
    keywords: set('async await break case catch class const continue debugger default delete do else export extends finally for from function if import in instanceof let new of return static super switch this throw try typeof var void while with yield'),
    types: set(''), constants: set('true false null undefined NaN Infinity'), capsTypes: true,
  },
  typescript: {
    line: ['//'], block: ['/*', '*/'], quotes: '"\'`', multi: ['`'], capsTypes: true,
    keywords: set('abstract as async await break case catch class const continue declare default delete do else enum export extends finally for from function if implements import in infer instanceof interface keyof let namespace new of private protected public readonly return satisfies static super switch this throw try type typeof var void while with yield'),
    types: set('any boolean never number object string symbol unknown bigint void'),
    constants: set('true false null undefined NaN Infinity'),
  },
  bash: {
    line: ['#'], quotes: '"\'',
    keywords: set('if then else elif fi case esac for select while until do done in function time return exit local export readonly declare unset shift break continue source alias'),
    types: set(''), constants: set('true false'),
  },
  go: {
    line: ['//'], block: ['/*', '*/'], quotes: '"\'`', multi: ['`'],
    keywords: set('break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var'),
    types: set('bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8 uint16 uint32 uint64 uintptr any'),
    constants: set('true false nil iota'),
  },
  c: {
    line: ['//'], block: ['/*', '*/'], quotes: '"\'', macros: 'c',
    keywords: set('auto break case const continue default do else enum extern for goto if inline register restrict return sizeof static struct switch typedef union volatile while'),
    types: C_LIKE_TYPES, constants: set('NULL true false'),
  },
  cpp: {
    line: ['//'], block: ['/*', '*/'], quotes: '"\'', macros: 'c', capsTypes: true,
    keywords: set('alignas alignof auto break case catch class const constexpr const_cast continue decltype default delete do dynamic_cast else enum explicit export extern for friend goto if inline mutable namespace new noexcept operator private protected public register reinterpret_cast return sizeof static static_assert static_cast struct switch template this throw try typedef typeid typename union using virtual volatile while override final'),
    types: set(`${[...C_LIKE_TYPES].join(' ')} wchar_t char8_t char16_t char32_t std string vector`),
    constants: set('nullptr NULL true false'),
  },
};

/** The lexer state carried from one line to the next ('' = code). */
export type LexState = string;

const OPERATORS = '+-*/%=<>!&|^~?:';
const PUNCT = '()[]{},;.';

function identEnd(s: string, i: number): number {
  let j = i;
  while (j < s.length && /[\p{L}\p{N}_$]/u.test(s[j]!)) j++;
  return j;
}

function numberEnd(s: string, i: number): number {
  const m = /^(?:0[xX][0-9a-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?)[a-zA-Z0-9_]*/.exec(s.slice(i));
  return i + (m ? m[0].length : 1);
}

/** Scan a string from `i` (just after the opening quote) to its end; -1 when it runs past the line. */
function stringEnd(s: string, i: number, close: string): number {
  let j = i;
  while (j < s.length) {
    if (s[j] === '\\') { j += 2; continue; }
    if (s.startsWith(close, j)) return j + close.length;
    j++;
  }
  return -1;
}

function codeLine(spec: Spec, s: string, state: LexState): { spans: Span[]; state: LexState } {
  const spans: Span[] = [];
  const push = (from: number, to: number, tok: Tok) => { if (to > from) spans.push({ from, to, tok }); };
  let i = 0;
  // Continue a block comment or a multi-line string.
  if (state.startsWith('c')) {
    let depth = Number(state.slice(1)) || 1;
    const [open, close] = spec.block!;
    let j = 0;
    while (j < s.length && depth > 0) {
      if (spec.nestBlock && s.startsWith(open, j)) { depth++; j += open.length; continue; }
      if (s.startsWith(close, j)) { depth--; j += close.length; continue; }
      j++;
    }
    push(0, j, 'comment');
    if (depth > 0) return { spans, state: `c${depth}` };
    i = j;
  } else if (state.startsWith('s')) {
    const close = state.slice(1);
    const end = stringEnd(s, 0, close);
    if (end < 0) { push(0, s.length, 'string'); return { spans, state }; }
    push(0, end, 'string');
    i = end;
  }
  let prevIdent = '';
  while (i < s.length) {
    const ch = s[i]!;
    if (/\s/.test(ch)) { i++; continue; }
    if (spec.line.some((t) => s.startsWith(t, i))) {
      push(i, s.length, 'comment');
      return { spans, state: '' };
    }
    if (spec.block && s.startsWith(spec.block[0], i)) {
      let depth = 1;
      let j = i + spec.block[0].length;
      while (j < s.length && depth > 0) {
        if (spec.nestBlock && s.startsWith(spec.block[0], j)) { depth++; j += spec.block[0].length; continue; }
        if (s.startsWith(spec.block[1], j)) { depth--; j += spec.block[1].length; continue; }
        j++;
      }
      push(i, j, 'comment');
      if (depth > 0) return { spans, state: `c${depth}` };
      i = j;
      continue;
    }
    // Attributes, decorators and preprocessor lines.
    if (spec.macros === 'rust' && (s.startsWith('#[', i) || s.startsWith('#![', i))) {
      const close = s.indexOf(']', i);
      const end = close < 0 ? s.length : close + 1;
      push(i, end, 'attribute');
      i = end;
      continue;
    }
    if (spec.macros === 'c' && ch === '#' && s.slice(0, i).trim() === '') {
      const end = identEnd(s, i + 1);
      push(i, end, 'keyword');
      i = end;
      continue;
    }
    if (spec.macros === 'python' && ch === '@') {
      const end = identEnd(s, i + 1);
      push(i, end, 'attribute');
      i = end;
      continue;
    }
    // Strings (multi-line delimiters first).
    const multi = spec.multi?.find((d) => s.startsWith(d, i));
    if (multi) {
      const end = stringEnd(s, i + multi.length, multi);
      if (end < 0) { push(i, s.length, 'string'); return { spans, state: `s${multi}` }; }
      push(i, end, 'string');
      i = end;
      continue;
    }
    if (spec.macros === 'rust' && /^r#*"/.test(s.slice(i))) {
      const hashes = /^r(#*)"/.exec(s.slice(i))![1]!;
      const close = `"${hashes}`;
      const at = s.indexOf(close, i + 2 + hashes.length);
      if (at < 0) { push(i, s.length, 'string'); return { spans, state: `s${close}` }; }
      push(i, at + close.length, 'string');
      i = at + close.length;
      continue;
    }
    if (spec.quotes.includes(ch)) {
      if (ch === "'" && spec.lifetimes && !/^'(?:\\.|[^\\'])'/u.test(s.slice(i))) {
        // A lifetime or label ('a), not a char literal.
        const end = identEnd(s, i + 1);
        push(i, end, 'type');
        i = end;
        continue;
      }
      const end = stringEnd(s, i + 1, ch);
      if (end < 0) {
        push(i, s.length, 'string');
        // A plain Rust or C string may run on with a trailing backslash; others end at the line.
        return { spans, state: spec.macros === 'rust' && ch === '"' ? `s"` : '' };
      }
      push(i, end, 'string');
      i = end;
      continue;
    }
    if (/[0-9]/.test(ch)) {
      const end = numberEnd(s, i);
      push(i, end, 'number');
      i = end;
      continue;
    }
    if (/[\p{L}_$]/u.test(ch)) {
      const end = identEnd(s, i);
      const word = s.slice(i, end);
      let tok: Tok | null = null;
      if (spec.keywords.has(word)) tok = 'keyword';
      else if (spec.constants.has(word)) tok = 'constant';
      else if (spec.types.has(word)) tok = 'type';
      else if (spec.macros === 'rust' && s[end] === '!') { push(i, end + 1, 'function'); i = end + 1; prevIdent = word; continue; }
      else if (/^[A-Z][A-Z0-9_]+$/.test(word) && word.length > 1) tok = 'constant';
      else if (spec.capsTypes && /^[A-Z]/.test(word)) tok = 'type';
      else if (/^\s*\(/.test(s.slice(end)) || (spec.macros === 'rust' && s.startsWith('::<', end))) tok = 'function';
      else if (i > 0 && s[i - 1] === '.') tok = 'property';
      if (['fn', 'def', 'func', 'function'].includes(prevIdent) && tok === null) tok = 'function';
      if (tok) push(i, end, tok);
      prevIdent = word;
      i = end;
      continue;
    }
    if (OPERATORS.includes(ch)) { push(i, i + 1, 'operator'); i++; continue; }
    if (PUNCT.includes(ch)) { push(i, i + 1, 'punctuation'); i++; continue; }
    if (ch === '$' || ch === '@') { const end = identEnd(s, i + 1); push(i, Math.max(end, i + 1), 'variable'); i = Math.max(end, i + 1); continue; }
    i++;
  }
  return { spans, state: '' };
}

/** JSON: keys are properties, the rest strings, numbers and constants. */
function jsonLine(s: string, state: LexState): { spans: Span[]; state: LexState } {
  const spans: Span[] = [];
  let i = 0;
  if (state === 'c') {
    const end = s.indexOf('*/');
    if (end < 0) return { spans: [{ from: 0, to: s.length, tok: 'comment' }], state };
    spans.push({ from: 0, to: end + 2, tok: 'comment' });
    i = end + 2;
  }
  while (i < s.length) {
    const ch = s[i]!;
    if (s.startsWith('//', i)) { spans.push({ from: i, to: s.length, tok: 'comment' }); break; }
    if (s.startsWith('/*', i)) {
      const end = s.indexOf('*/', i + 2);
      if (end < 0) { spans.push({ from: i, to: s.length, tok: 'comment' }); return { spans, state: 'c' }; }
      spans.push({ from: i, to: end + 2, tok: 'comment' });
      i = end + 2;
      continue;
    }
    if (ch === '"') {
      const end = stringEnd(s, i + 1, '"');
      const to = end < 0 ? s.length : end;
      const key = /^\s*:/.test(s.slice(to));
      spans.push({ from: i, to, tok: key ? 'property' : 'string' });
      i = to;
      continue;
    }
    if (/[-0-9]/.test(ch)) { const end = numberEnd(s, ch === '-' ? i + 1 : i); spans.push({ from: i, to: end, tok: 'number' }); i = end; continue; }
    const word = /^(true|false|null)\b/.exec(s.slice(i));
    if (word) { spans.push({ from: i, to: i + word[1]!.length, tok: 'constant' }); i += word[1]!.length; continue; }
    if ('{}[],:'.includes(ch)) { spans.push({ from: i, to: i + 1, tok: 'punctuation' }); i++; continue; }
    i++;
  }
  return { spans, state: '' };
}

/** TOML and YAML: `#` comments, tables / keys, strings, numbers, booleans. */
function configLine(lang: 'toml' | 'yaml', s: string, state: LexState): { spans: Span[]; state: LexState } {
  const spans: Span[] = [];
  let i = 0;
  if (state.startsWith('s')) {
    const close = state.slice(1);
    const at = s.indexOf(close);
    if (at < 0) return { spans: [{ from: 0, to: s.length, tok: 'string' }], state };
    spans.push({ from: 0, to: at + close.length, tok: 'string' });
    i = at + close.length;
  } else {
    const lead = /^\s*/.exec(s)![0].length;
    if (lang === 'toml' && s[lead] === '[') {
      const end = s.indexOf(']', lead);
      const to = end < 0 ? s.length : (s[end + 1] === ']' ? end + 2 : end + 1);
      spans.push({ from: lead, to, tok: 'tag' });
      i = to;
    } else {
      const key = lang === 'toml' ? /^(\s*)([A-Za-z0-9_.\-"']+)\s*=/.exec(s) : /^(\s*(?:-\s+)?)([^\s#:][^#:]*?)\s*:(?:\s|$)/.exec(s);
      if (key) {
        const from = key[1]!.length;
        spans.push({ from, to: from + key[2]!.length, tok: 'property' });
        i = from + key[2]!.length;
      } else if (lang === 'yaml' && /^\s*-\s/.test(s)) {
        spans.push({ from: lead, to: lead + 1, tok: 'punctuation' });
        i = lead + 1;
      }
    }
  }
  while (i < s.length) {
    const ch = s[i]!;
    if (ch === '#') { spans.push({ from: i, to: s.length, tok: 'comment' }); break; }
    const multi = lang === 'toml' ? ['"""', "'''"].find((d) => s.startsWith(d, i)) : undefined;
    if (multi) {
      const at = s.indexOf(multi, i + 3);
      if (at < 0) { spans.push({ from: i, to: s.length, tok: 'string' }); return { spans, state: `s${multi}` }; }
      spans.push({ from: i, to: at + 3, tok: 'string' });
      i = at + 3;
      continue;
    }
    if (ch === '"' || ch === "'") {
      const end = ch === '"' ? stringEnd(s, i + 1, '"') : s.indexOf("'", i + 1) + 1 || -1;
      const to = end < 0 ? s.length : end;
      spans.push({ from: i, to, tok: 'string' });
      i = to;
      continue;
    }
    const word = /^(true|false|null|yes|no|on|off|~)(?![\w-])/i.exec(s.slice(i));
    if (word && (i === 0 || /[\s=:,[{]/.test(s[i - 1]!))) { spans.push({ from: i, to: i + word[1]!.length, tok: 'constant' }); i += word[1]!.length; continue; }
    if (/[-+0-9]/.test(ch) && (i === 0 || /[\s=:,[{]/.test(s[i - 1]!)) && /^[-+]?\d/.test(s.slice(i))) {
      const end = numberEnd(s, /[-+]/.test(ch) ? i + 1 : i);
      spans.push({ from: i, to: end, tok: 'number' });
      i = end;
      continue;
    }
    if ('[]{},=|>&*'.includes(ch)) { spans.push({ from: i, to: i + 1, tok: 'punctuation' }); i++; continue; }
    i++;
  }
  return { spans, state: '' };
}

/** Markdown source: headings, fences, lists, quotes, emphasis, inline code and links. */
function markdownLine(s: string, state: LexState): { spans: Span[]; state: LexState } {
  const fence = /^\s*(```|~~~)/.exec(s);
  if (state.startsWith('f')) {
    if (fence && fence[1] === state.slice(1)) return { spans: [{ from: 0, to: s.length, tok: 'punctuation' }], state: '' };
    return { spans: [{ from: 0, to: s.length, tok: 'code' }], state };
  }
  if (fence) return { spans: [{ from: 0, to: s.length, tok: 'punctuation' }], state: `f${fence[1]}` };
  const head = /^\s{0,3}#{1,6}\s/.exec(s);
  if (head) return { spans: [{ from: 0, to: s.length, tok: 'heading' }], state: '' };
  const spans: Span[] = [];
  const marker = /^(\s*)([-*+]|\d+[.)]|>)\s/.exec(s);
  if (marker) spans.push({ from: marker[1]!.length, to: marker[1]!.length + marker[2]!.length, tok: 'punctuation' });
  const re = /(`+)(.+?)\1|\*\*(.+?)\*\*|__(.+?)__|(?<![\w*])\*(?!\s)(.+?)\*(?!\w)|(?<!\w)_(?!\s)(.+?)_(?!\w)|\[([^\]]*)\]\(([^)]*)\)|<https?:[^>]+>/g;
  for (let m = re.exec(s); m; m = re.exec(s)) {
    const tok: Tok = m[1] ? 'code' : (m[3] ?? m[4]) !== undefined ? 'strong' : (m[5] ?? m[6]) !== undefined ? 'emphasis' : 'link';
    spans.push({ from: m.index, to: m.index + m[0].length, tok });
  }
  return { spans, state: '' };
}

/** One line's spans and the state the next line starts in. */
export function highlightLine(lang: Lang, line: string, state: LexState): { spans: Span[]; state: LexState } {
  switch (lang) {
    case 'json': return jsonLine(line, state);
    case 'toml': case 'yaml': return configLine(lang, line, state);
    case 'markdown': return markdownLine(line, state);
    default: return codeLine(SPECS[lang], line, state);
  }
}

/** Lines past this are not highlighted (plain text, as the TUI falls back on a parse failure). */
export const MAX_HIGHLIGHT_LINES = 50_000;
/** Longer lines are drawn plain (minified files). */
export const MAX_HIGHLIGHT_LINE = 4_000;

/**
 * The state each line starts in, for highlighting any visible window without
 * re-lexing from the top (one pass over the file at open).
 */
export function lexStates(lang: Lang, lines: readonly string[]): LexState[] {
  const out: LexState[] = [];
  let st: LexState = '';
  const n = Math.min(lines.length, MAX_HIGHLIGHT_LINES);
  for (let i = 0; i < n; i++) {
    out.push(st);
    const line = lines[i]!;
    st = line.length > MAX_HIGHLIGHT_LINE ? '' : highlightLine(lang, line, st).state;
  }
  return out;
}

// ---------- fold regions ----------

const OPEN = '([{';
const CLOSE = ')]}';

/**
 * Foldable regions in place of tree-sitter's nodes (file_viewer.rs
 * FOLDABLE_TYPES: blocks, bodies, argument and parameter lists, objects,
 * arrays, block comments): every bracket pair or block comment that spans
 * lines, found outside strings and comments; Python and YAML fold by
 * indentation; Markdown folds fenced code blocks and heading sections.
 */
export function foldRegions(lang: Lang | null, lines: readonly string[]): Region[] {
  const out: Region[] = [];
  if (lines.length > MAX_HIGHLIGHT_LINES) return out;
  if (lang === null) return out;
  if (lang === 'python' || lang === 'yaml') indentRegions(lines, out);
  if (lang === 'markdown') markdownRegions(lines, out);
  else if (lang !== 'yaml') bracketRegions(lang, lines, out);
  const unique = new Map<string, Region>();
  for (const r of out) if (r[1] > r[0]) unique.set(Folds.key(r), r);
  return [...unique.values()].sort((a, b) => a[0] - b[0] || b[1] - a[1]);
}

function bracketRegions(lang: Lang, lines: readonly string[], out: Region[]): void {
  const stack: Array<{ ch: string; row: number }> = [];
  let st: LexState = '';
  let commentStart = -1;
  for (let row = 0; row < lines.length; row++) {
    const line = lines[row]!;
    if (line.length > MAX_HIGHLIGHT_LINE) { st = ''; continue; }
    const began = st;
    const { spans, state } = highlightLine(lang, line, st);
    if (!began.startsWith('c') && state.startsWith('c')) commentStart = row;
    if (began.startsWith('c') && !state.startsWith('c') && commentStart >= 0) { out.push([commentStart, row]); commentStart = -1; }
    st = state;
    // Brackets outside strings and comments only.
    const masked = new Uint8Array(line.length);
    for (const sp of spans) if (sp.tok === 'string' || sp.tok === 'comment' || sp.tok === 'code') masked.fill(1, sp.from, sp.to);
    for (let i = 0; i < line.length; i++) {
      if (masked[i]) continue;
      const ch = line[i]!;
      if (OPEN.includes(ch)) stack.push({ ch, row });
      else if (CLOSE.includes(ch)) {
        const want = OPEN[CLOSE.indexOf(ch)]!;
        for (let k = stack.length - 1; k >= 0; k--) {
          if (stack[k]!.ch !== want) continue;
          const open = stack[k]!;
          stack.length = k;
          if (row > open.row) out.push([open.row, row]);
          break;
        }
      }
    }
  }
}

function indentRegions(lines: readonly string[], out: Region[]): void {
  const indent = (l: string): number => /^\s*/.exec(l)![0].replace(/\t/g, '    ').length;
  const blank = (l: string): boolean => l.trim() === '';
  for (let row = 0; row < lines.length; row++) {
    if (blank(lines[row]!)) continue;
    const base = indent(lines[row]!);
    let end = row;
    for (let j = row + 1; j < lines.length; j++) {
      if (blank(lines[j]!)) continue;
      if (indent(lines[j]!) <= base) break;
      end = j;
    }
    if (end > row) out.push([row, end]);
  }
}

function markdownRegions(lines: readonly string[], out: Region[]): void {
  let fence: { row: number; mark: string } | null = null;
  const heads: Array<{ row: number; level: number }> = [];
  const closeHeads = (level: number, row: number) => {
    while (heads.length && heads[heads.length - 1]!.level >= level) {
      const h = heads.pop()!;
      let end = row - 1;
      while (end > h.row && lines[end]!.trim() === '') end--;
      if (end > h.row) out.push([h.row, end]);
    }
  };
  for (let row = 0; row < lines.length; row++) {
    const line = lines[row]!;
    const f = /^\s*(```|~~~)/.exec(line);
    if (fence) {
      if (f && f[1] === fence.mark) { out.push([fence.row, row]); fence = null; }
      continue;
    }
    if (f) { fence = { row, mark: f[1]! }; continue; }
    const h = /^\s{0,3}(#{1,6})\s/.exec(line);
    if (h) {
      closeHeads(h[1]!.length, row);
      heads.push({ row, level: h[1]!.length });
    }
  }
  closeHeads(1, lines.length);
}

// ---------- segments ----------

/**
 * A decoration over code-point columns: a search hit, the current hit, the cursor,
 * the matching bracket, and in the editor the insert caret and the visual selection.
 */
export interface Deco { from: number; to: number; cls: 'match' | 'current' | 'cursor' | 'bracket' | 'caret' | 'select' }

/** A run of a line with one set of classes, as the renderer draws it. */
export interface Segment { text: string; tok: Tok | null; decos: Deco['cls'][] }

/**
 * Cut a line into runs at every span and decoration boundary. Spans are in
 * UTF-16 offsets (the lexer's), decorations in code points (the cursor's).
 */
export function segments(line: string, spans: readonly Span[], decos: readonly Deco[]): Segment[] {
  const cuts = new Set<number>([0, line.length]);
  for (const s of spans) { cuts.add(Math.max(0, Math.min(line.length, s.from))); cuts.add(Math.max(0, Math.min(line.length, s.to))); }
  const d16 = decos.map((d) => ({ cls: d.cls, from: utf16Col(line, d.from), to: d.to > [...line].length ? line.length + 1 : utf16Col(line, d.to) }));
  for (const d of d16) { cuts.add(Math.min(line.length, d.from)); cuts.add(Math.min(line.length, d.to)); }
  const points = [...cuts].sort((a, b) => a - b);
  const out: Segment[] = [];
  for (let k = 0; k + 1 < points.length; k++) {
    const from = points[k]!;
    const to = points[k + 1]!;
    if (to <= from) continue;
    const tok = spans.find((s) => s.from <= from && s.to >= to)?.tok ?? null;
    const cls = d16.filter((d) => d.from <= from && d.to >= to).map((d) => d.cls);
    out.push({ text: line.slice(from, to), tok, decos: cls });
  }
  // A cursor past the end of the text (an empty line) draws as one blank cell.
  const tail = d16.filter((d) => d.from >= line.length).map((d) => d.cls);
  if (tail.length) out.push({ text: ' ', tok: null, decos: tail });
  return out;
}

/** The bracket matching the one under the cursor, or null (the viewer's automatic highlight). */
export function bracketPartner(lines: Lines, p: Pos): Pos | null {
  const ch = lines[p.row]?.[p.col];
  if (ch === undefined || !'()[]{}'.includes(ch)) return null;
  const m = matchBracket(lines, p);
  return m && (m.row !== p.row || m.col !== p.col) ? m : null;
}

/** A fenced code block's language (` ```rust `, ` ```py `, ...), as the transcript's syntect lookup by token. */
const FENCES: Readonly<Record<string, Lang>> = {
  rust: 'rust', rs: 'rust', python: 'python', py: 'python', javascript: 'javascript', js: 'javascript', jsx: 'javascript',
  mjs: 'javascript', typescript: 'typescript', ts: 'typescript', tsx: 'typescript', bash: 'bash', sh: 'bash', shell: 'bash',
  zsh: 'bash', console: 'bash', json: 'json', jsonc: 'json', toml: 'toml', yaml: 'yaml', yml: 'yaml', go: 'go', golang: 'go',
  c: 'c', h: 'c', cpp: 'cpp', 'c++': 'cpp', cc: 'cpp', hpp: 'cpp', markdown: 'markdown', md: 'markdown',
};

export function fenceLanguage(name: string): Lang | null {
  return FENCES[name.trim().toLowerCase()] ?? null;
}

/** Highlight a whole code block: one span list per line. */
export function highlightBlock(lang: Lang, text: string): Array<{ line: string; spans: Span[] }> {
  const out: Array<{ line: string; spans: Span[] }> = [];
  let st: LexState = '';
  const lines = text.split('\n');
  for (let i = 0; i < lines.length && i < MAX_HIGHLIGHT_LINES; i++) {
    const line = lines[i]!;
    if (line.length > MAX_HIGHLIGHT_LINE) { out.push({ line, spans: [] }); st = ''; continue; }
    const r = highlightLine(lang, line, st);
    out.push({ line, spans: r.spans });
    st = r.state;
  }
  return out;
}
