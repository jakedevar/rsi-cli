// The vim text editor of the message box and the overlay text fields (pure,
// no DOM). It mirrors the TUI's shared handler, crates/rsi/src/vim_textarea:
// counts, motions, f/t/F/T with ; and ,, operators d c y with motions and
// text objects, visual and visual-line mode, direct edits, undo / redo and
// dot repeat. Insert-mode typing is the text field's own; the DOM adapter
// (vim-field.ts) hands the edited text back with setText before the next
// normal-mode key and calls leaveInsert on Esc. Tokens are keyengine.ts's
// (`h`, `C-r`, `Esc`, `Left`). Unit-tested under tests/vim-editor.test.mjs.
import {
  type CharSearch, type Lines, type ObjectKind, type Pos, type SearchDir, charSearch, cmp, firstNonBlank,
  fromLines, insertedText, isObjectKey, linewiseObject, matchBracket, offsetToPos, paragraphBack, paragraphForward,
  posToOffset, reverseDir, textObject, toLines, wordBack, wordEnd, wordForward,
} from './vim-text.js';

export type VimMode = 'insert' | 'normal' | 'visual' | 'visual-line';

/**
 * What a key did: `consumed` (handled, stay in the mode), `insert` (the key
 * entered insert mode), `unhandled` (not an editor key: the caller passes it on).
 */
export type VimOutcome = 'consumed' | 'insert' | 'unhandled';

type Op = 'd' | 'c' | 'y';
type Kind = 'exclusive' | 'inclusive' | 'linewise';

interface Motion {
  to: Pos;
  kind: Kind;
}

interface Snapshot {
  text: string;
  cursor: Pos;
}

/** The last change, for `.` (state.rs `ChangeRecord`): its keys and any typed text. */
export interface ChangeRecord {
  keys: string[];
  inserted: string | null;
}

/** Undo depth kept per field. */
export const MAX_UNDO = 200;
/** Largest count honoured (`99999x` stops here). */
export const MAX_VIM_COUNT = 9999;

const MOTION_KEYS = new Set(['h', 'j', 'k', 'l', 'w', 'b', 'e', '0', '$', '^', 'G', '{', '}', '%', ';', ',', 'Left', 'Right', 'Up', 'Down']);

export class VimEditor {
  private lines: Lines;
  private cur: Pos = { row: 0, col: 0 };
  private m: VimMode = 'insert';
  private anchor: Pos | null = null;
  /** The unnamed register; `linewise` when it holds whole lines (yy, dd, V). */
  private reg = { text: '', linewise: false };
  /** Set by a yank or delete for the caller to copy to the system clipboard. */
  private lastYank: string | null = null;

  private count = '';
  private op: Op | null = null;
  private opCount = 1;
  private gPending = false;
  private replacePending = false;
  private objPending: ObjectKind | null = null;
  private searchPending: SearchDir | null = null;
  private lastSearch: CharSearch | null = null;
  private desiredCol: number | null = null;

  private lastChange: ChangeRecord | null = null;
  private recording: string[] | null = null;
  private replaying = false;
  private insertStart: string | null = null;

  private undoStack: Snapshot[] = [];
  private redoStack: Snapshot[] = [];

  /**
   * A one-line field (an `<input>`): the text never gets a second line, so `o`
   * / `O` append / insert at the line's ends, `p` flattens newlines to spaces
   * and `J` has nothing to join. Everything else is the multi-line editor.
   */
  private readonly single: boolean;

  constructor(text = '', mode: VimMode = 'insert', opts: { singleLine?: boolean } = {}) {
    this.single = opts.singleLine === true;
    this.lines = toLines(this.single ? text.replace(/\r?\n/g, ' ') : text);
    this.m = mode;
    if (mode !== 'insert') this.insertStart = null;
  }

  // ---------- state ----------

  get text(): string {
    return fromLines(this.lines);
  }

  get mode(): VimMode {
    return this.m;
  }

  get cursor(): Pos {
    return { ...this.cur };
  }

  /** The cursor as a UTF-16 offset (a textarea's selectionStart). */
  cursorOffset(): number {
    return posToOffset(this.lines, this.cur);
  }

  /** The visual selection as UTF-16 [start, end), or null outside visual mode. */
  selection(): { start: number; end: number } | null {
    if (!this.anchor || (this.m !== 'visual' && this.m !== 'visual-line')) return null;
    const [a, b] = cmp(this.anchor, this.cur) <= 0 ? [this.anchor, this.cur] : [this.cur, this.anchor];
    if (this.m === 'visual-line') {
      return {
        start: posToOffset(this.lines, { row: a.row, col: 0 }),
        end: posToOffset(this.lines, { row: b.row, col: this.lines[b.row]!.length }),
      };
    }
    const endLen = (this.lines[b.row]![b.col] ?? ' ').length;
    return { start: posToOffset(this.lines, a), end: Math.min(posToOffset(this.lines, b) + endLen, this.text.length) };
  }

  /** No command is half-typed (state.rs `is_idle`). */
  idle(): boolean {
    return this.count === '' && this.op === null && !this.gPending && !this.replacePending
      && this.objPending === null && this.searchPending === null && this.m !== 'visual' && this.m !== 'visual-line';
  }

  /** The keys typed so far of an unfinished command (`2d`, `ci`), for a hint. */
  pending(): string {
    return `${this.count}${this.op ?? ''}${this.gPending ? 'g' : ''}${this.replacePending ? 'r' : ''}${this.objPending ?? ''}${this.searchPending ?? ''}`;
  }

  /** The text a `y` copied since the last call (for the system clipboard, as the TUI's OSC 52). */
  takeYank(): string | null {
    const y = this.lastYank;
    this.lastYank = null;
    return y;
  }

  /** The last recorded change (tests and the adapter's hint). */
  lastChangeRecord(): ChangeRecord | null {
    return this.lastChange ? { keys: [...this.lastChange.keys], inserted: this.lastChange.inserted } : null;
  }

  /** The register's text (what `p` puts). */
  register(): string {
    return this.reg.text;
  }

  /** Put text in the register (a paste from outside, e.g. the system clipboard). */
  setRegister(text: string, linewise = false): void {
    this.reg = { text, linewise };
  }

  /**
   * The field's text and cursor, as typed in insert mode or changed by the
   * page. A different text drops a half-typed command.
   */
  setText(text: string, offset?: number): void {
    if (text !== this.text) {
      this.lines = toLines(text);
      if (this.m !== 'insert') this.resetPending();
    }
    if (offset !== undefined) this.cur = offsetToPos(this.lines, offset);
    if (this.m !== 'insert') this.clampNormal();
  }

  /** Drop any half-typed command and a visual selection (Esc, focus loss). */
  resetPending(): void {
    this.count = '';
    this.op = null;
    this.opCount = 1;
    this.gPending = false;
    this.replacePending = false;
    this.objPending = null;
    this.searchPending = null;
    this.desiredCol = null;
    if (this.m === 'visual' || this.m === 'visual-line') {
      this.m = 'normal';
      this.anchor = null;
    }
    if (!this.replaying && this.m !== 'insert') this.recording = null;
  }

  // ---------- insert mode ----------

  /** Enter insert mode the way `i`, `a`, `A`, `I`, `o` or `O` does (also from outside). */
  enterInsert(how: 'i' | 'a' | 'A' | 'I' | 'o' | 'O' = 'i'): void {
    if (this.m === 'insert') return;
    this.resetPending();
    this.startChange(how);
    this.applyInsertEntry(how);
  }

  private applyInsertEntry(entry: string): void {
    const how = this.single ? (entry === 'o' ? 'A' : entry === 'O' ? 'I' : entry) : entry;
    const len = this.lineLen();
    switch (how) {
      case 'a': this.cur.col = Math.min(this.cur.col + 1, len); break;
      case 'A': this.cur.col = len; break;
      case 'I': this.cur.col = firstNonBlank(this.lines, this.cur.row); break;
      case 'o':
        this.lines.splice(this.cur.row + 1, 0, []);
        this.cur = { row: this.cur.row + 1, col: 0 };
        break;
      case 'O':
        this.lines.splice(this.cur.row, 0, []);
        this.cur = { row: this.cur.row, col: 0 };
        break;
      default: break;
    }
    this.toInsert();
  }

  private toInsert(): void {
    this.m = 'insert';
    this.anchor = null;
    if (!this.replaying) this.insertStart = this.insertStart ?? this.text;
  }

  /** Type text at the cursor (insert mode, and the dot replay of a typed insert). */
  insertText(s: string): void {
    const add = toLines(this.single ? s.replace(/\r?\n/g, ' ') : s);
    const line = this.lines[this.cur.row]!;
    const head = line.slice(0, this.cur.col);
    const tail = line.slice(this.cur.col);
    if (add.length === 1) {
      this.lines[this.cur.row] = [...head, ...add[0]!, ...tail];
      this.cur.col += add[0]!.length;
    } else {
      const last = add[add.length - 1]!;
      this.lines.splice(this.cur.row, 1, [...head, ...add[0]!], ...add.slice(1, -1), [...last, ...tail]);
      this.cur = { row: this.cur.row + add.length - 1, col: last.length };
    }
  }

  /**
   * Esc in insert mode: back to normal, the cursor one left as in vim, and
   * the change that entered insert (with what was typed) becomes `.`.
   */
  leaveInsert(): void {
    if (this.m !== 'insert') return;
    const before = this.insertStart;
    this.insertStart = null;
    this.m = 'normal';
    if (this.cur.col > 0) this.cur.col -= 1;
    this.clampNormal();
    const now = this.text;
    if (this.replaying) return;
    // An insert that changed nothing leaves no undo step.
    const top = this.undoStack[this.undoStack.length - 1];
    if (top && top.text === now && before !== null && before === now) this.undoStack.pop();
    if (this.recording) {
      const typed = before === null ? '' : insertedText(before, now);
      this.lastChange = { keys: this.recording, inserted: typed === '' ? null : typed };
      this.recording = null;
    }
  }

  // ---------- keys ----------

  /** One key in normal or visual mode (Esc also leaves insert mode). */
  key(token: string): VimOutcome {
    if (this.m === 'insert') {
      if (token === 'Esc') { this.leaveInsert(); return 'consumed'; }
      return 'unhandled';
    }
    const out = this.dispatch(token);
    if ((this.m as VimMode) !== 'insert') this.clampNormal();
    return out;
  }

  private dispatch(t: string): VimOutcome {
    if (this.replacePending) return this.finishReplace(t);
    if (this.objPending) return this.finishObject(t);
    if (this.searchPending) return this.finishSearch(t);
    if (this.gPending) {
      this.gPending = false;
      const c = this.count;
      this.count = '';
      if (t !== 'g') { this.cancelOp(); return 'consumed'; }
      this.record(t);
      const row = c === '' ? 0 : Math.min(Number(c), this.lines.length) - 1;
      return this.motionDone({ to: { row, col: 0 }, kind: 'linewise' }, true);
    }
    if (/^[1-9]$/.test(t) || (t === '0' && this.count !== '')) {
      this.count = String(Math.min(Number(this.count + t), MAX_VIM_COUNT));
      return 'consumed';
    }
    if (t === 'Esc') {
      const busy = !this.idle();
      this.cancelOp();
      this.resetPending();
      return busy ? 'consumed' : 'unhandled';
    }
    if (this.m === 'visual' || this.m === 'visual-line') return this.visualKey(t);
    if (this.op) return this.opKey(t);
    return this.normalKey(t);
  }

  private takeCount(): number {
    const n = this.count === '' ? 1 : Number(this.count);
    this.count = '';
    return n;
  }

  // ----- normal mode -----

  private normalKey(t: string): VimOutcome {
    const hadCount = this.count !== '';
    const n = this.takeCount();
    switch (t) {
      case 'v': case 'V':
        this.anchor = { ...this.cur };
        this.m = t === 'v' ? 'visual' : 'visual-line';
        return 'consumed';
      case 'i': case 'a': case 'A': case 'I': case 'o': case 'O':
        this.startChange(t);
        this.applyInsertEntry(t);
        return 'insert';
      case 'x': {
        if (this.lineLen() === 0) return 'consumed';
        this.startChange(t);
        const end = Math.min(this.cur.col + n, this.lineLen());
        this.yankDelete(this.cur, { row: this.cur.row, col: end }, false, 'd');
        this.finishChange();
        return 'consumed';
      }
      case 's': {
        this.startChange(t);
        const end = Math.min(this.cur.col + n, this.lineLen());
        this.yankDelete(this.cur, { row: this.cur.row, col: end }, false, 'c');
        this.toInsert();
        return 'insert';
      }
      case 'D': case 'C': {
        this.startChange(t);
        this.yankDelete(this.cur, { row: this.cur.row, col: this.lineLen() }, false, t === 'D' ? 'd' : 'c');
        if (t === 'C') { this.toInsert(); return 'insert'; }
        this.finishChange();
        return 'consumed';
      }
      case 'r':
        this.startChange(t);
        this.replacePending = true;
        if (hadCount) this.count = String(n);
        return 'consumed';
      case 'J':
        this.startChange(t);
        for (let i = 0; i < Math.max(1, n); i++) this.joinLine();
        this.finishChange();
        return 'consumed';
      case '~': {
        this.startChange(t);
        const line = this.lines[this.cur.row]!;
        for (let i = 0; i < n && this.cur.col < line.length; i++) {
          const c = line[this.cur.col]!;
          const up = c.toUpperCase();
          line[this.cur.col] = c === up ? c.toLowerCase() : up;
          this.cur.col += 1;
        }
        this.finishChange();
        return 'consumed';
      }
      case 'p': case 'P':
        if (this.reg.text === '') return 'consumed';
        this.startChange(t);
        for (let i = 0; i < n; i++) this.put(t === 'P');
        this.finishChange();
        return 'consumed';
      case 'u':
        for (let i = 0; i < n; i++) this.undo();
        return 'consumed';
      case 'C-r':
        for (let i = 0; i < n; i++) this.redo();
        return 'consumed';
      case 'd': case 'c': case 'y':
        this.recording = null;
        this.startChange(t);
        this.op = t;
        this.opCount = n;
        return 'consumed';
      case 'g':
        this.gPending = true;
        if (hadCount) this.count = String(n);
        return 'consumed';
      case 'f': case 't': case 'F': case 'T':
        this.searchPending = t;
        if (hadCount) this.count = String(n);
        return 'consumed';
      case '.':
        this.dotRepeat();
        return 'consumed';
      default: {
        const m = this.motion(t, n, hadCount);
        if (m === undefined) return 'unhandled';
        if (m) this.moveTo(m, t);
        return 'consumed';
      }
    }
  }

  /** j/k keep the wanted column across short lines (vim's curswant). */
  private moveTo(m: Motion, key: string): void {
    const vertical = key === 'j' || key === 'k' || key === 'Up' || key === 'Down';
    if (!vertical) this.desiredCol = null;
    this.cur = { ...m.to };
  }

  /**
   * A motion's target, null when it cannot move (no match), undefined when
   * the key is not a motion.
   */
  private motion(t: string, n: number, hadCount: boolean): Motion | null | undefined {
    const c = this.cur;
    const len = this.lineLen();
    switch (t) {
      case 'h': case 'Left':
        return { to: { row: c.row, col: Math.max(0, c.col - n) }, kind: 'exclusive' };
      case 'l': case 'Right':
        return { to: { row: c.row, col: Math.min(c.col + n, this.op ? len : Math.max(0, len - 1)) }, kind: 'exclusive' };
      case 'j': case 'Down': case 'k': case 'Up': {
        const down = t === 'j' || t === 'Down';
        const row = Math.max(0, Math.min(this.lines.length - 1, c.row + (down ? n : -n)));
        const want = this.desiredCol ?? c.col;
        this.desiredCol = want;
        const max = Math.max(0, (this.lines[row]?.length ?? 0) - 1);
        return { to: { row, col: Math.min(want, max) }, kind: 'linewise' };
      }
      case 'w': {
        let p = c;
        for (let i = 0; i < n; i++) p = wordForward(this.lines, p);
        return { to: p, kind: 'exclusive' };
      }
      case 'b': {
        let p = c;
        for (let i = 0; i < n; i++) p = wordBack(this.lines, p);
        return { to: p, kind: 'exclusive' };
      }
      case 'e': {
        let p = c;
        for (let i = 0; i < n; i++) p = wordEnd(this.lines, p);
        return { to: p, kind: 'inclusive' };
      }
      case '0': return { to: { row: c.row, col: 0 }, kind: 'exclusive' };
      case '^': return { to: { row: c.row, col: firstNonBlank(this.lines, c.row) }, kind: 'exclusive' };
      case '$': return { to: { row: c.row, col: Math.max(0, len - 1) }, kind: 'inclusive' };
      case 'G': {
        const row = hadCount ? Math.min(n, this.lines.length) - 1 : this.lines.length - 1;
        return { to: { row, col: this.op ? 0 : Math.min(c.col, Math.max(0, (this.lines[row]?.length ?? 1) - 1)) }, kind: 'linewise' };
      }
      case '{': case '}': {
        let row = c.row;
        for (let i = 0; i < n; i++) row = t === '{' ? paragraphBack(this.lines, row) : paragraphForward(this.lines, row);
        const atEnd = t === '}' && row === this.lines.length - 1 && this.lines[row]!.some((x) => /\S/u.test(x));
        return { to: { row, col: atEnd ? Math.max(0, this.lines[row]!.length - (this.op ? 0 : 1)) : 0 }, kind: 'exclusive' };
      }
      case '%': {
        const p = matchBracket(this.lines, c);
        return p ? { to: p, kind: 'inclusive' } : null;
      }
      case ';': case ',': {
        if (!this.lastSearch) return null;
        const s = t === ';' ? this.lastSearch : { dir: reverseDir(this.lastSearch.dir), ch: this.lastSearch.ch };
        return this.searchMotion(s, n, true);
      }
      default:
        return undefined;
    }
  }

  private searchMotion(s: CharSearch, n: number, repeat: boolean): Motion | null {
    let p = this.cur;
    let moved = false;
    for (let i = 0; i < n; i++) {
      const col = charSearch(this.lines, p, s, repeat || i > 0);
      if (col === null) break;
      p = { row: p.row, col };
      moved = true;
    }
    if (!moved) return null;
    return { to: p, kind: s.dir === 'f' || s.dir === 't' ? 'inclusive' : 'exclusive' };
  }

  private finishSearch(t: string): VimOutcome {
    const dir = this.searchPending!;
    this.searchPending = null;
    if ([...t].length !== 1 && t !== 'Space') { this.cancelOp(); return 'consumed'; }
    const ch = t === 'Space' ? ' ' : t;
    this.record(t);
    const s: CharSearch = { dir, ch };
    this.lastSearch = s;
    const m = this.searchMotion(s, this.takeCount(), false);
    if (!m) { this.cancelOp(); return 'consumed'; }
    return this.motionDone(m, false);
  }

  /** A motion finished: move (normal / visual) or apply the pending operator. */
  private motionDone(m: Motion, fromG: boolean): VimOutcome {
    if (this.op) return this.applyMotion(m, fromG);
    this.desiredCol = null;
    this.cur = fromG ? { row: m.to.row, col: 0 } : { ...m.to };
    return 'consumed';
  }

  // ----- operators -----

  private opKey(t: string): VimOutcome {
    const op = this.op!;
    const n = this.takeCount() * this.opCount;
    this.record(t);
    if (t === op) {
      const last = Math.min(this.cur.row + n - 1, this.lines.length - 1);
      return this.applyLines(this.cur.row, last);
    }
    if (t === 'i' || t === 'a') { this.objPending = t; this.count = n > 1 ? String(n) : ''; return 'consumed'; }
    if (t === 'f' || t === 't' || t === 'F' || t === 'T') { this.searchPending = t; this.count = n > 1 ? String(n) : ''; return 'consumed'; }
    if (t === 'g') { this.gPending = true; return 'consumed'; }
    if (!MOTION_KEYS.has(t)) { this.cancelOp(); return 'consumed'; }
    // vim: `cw` on a word changes to its end, like `ce`.
    if (op === 'c' && t === 'w') {
      const cls = this.lines[this.cur.row]![this.cur.col];
      if (cls !== undefined && /\S/u.test(cls)) {
        let p = this.cur;
        for (let i = 0; i < n; i++) p = i === 0 && this.atWordEnd(p) ? p : wordEnd(this.lines, p);
        return this.applyMotion({ to: p, kind: 'inclusive' }, false);
      }
    }
    const m = this.motion(t, n, false);
    if (!m) { this.cancelOp(); return 'consumed'; }
    // `dw` at the last word of a line stops at the line end.
    if (t === 'w' && m.to.row > this.cur.row) {
      const len = this.lineLen();
      if (this.cur.col < len) m.to = { row: this.cur.row, col: len };
    }
    return this.applyMotion(m, false);
  }

  private atWordEnd(p: Pos): boolean {
    const line = this.lines[p.row]!;
    const c = line[p.col];
    const n = line[p.col + 1];
    if (c === undefined) return false;
    if (n === undefined) return true;
    const cls = (x: string) => (/^[\p{L}\p{N}_]$/u.test(x) ? 'w' : /\s/u.test(x) ? 's' : 'p');
    return cls(c) !== cls(n);
  }

  private applyMotion(m: Motion, fromG: boolean): VimOutcome {
    if (m.kind === 'linewise' || fromG) {
      const [a, b] = [this.cur.row, m.to.row].sort((x, y) => x - y) as [number, number];
      return this.applyLines(a, b);
    }
    let [s, e] = cmp(this.cur, m.to) <= 0 ? [this.cur, m.to] : [m.to, this.cur];
    if (m.kind === 'inclusive') e = { row: e.row, col: e.col + 1 };
    return this.applyRange(s, e);
  }

  /** Apply the pending operator to [s, e) charwise. */
  private applyRange(s: Pos, e: Pos): VimOutcome {
    const op = this.op!;
    this.op = null;
    this.opCount = 1;
    if (op === 'y') {
      this.yankDelete(s, e, false, 'y');
      this.cur = { ...s };
      this.finishChange();
      return 'consumed';
    }
    this.yankDelete(s, e, false, op);
    if (op === 'c') { this.toInsert(); return 'insert'; }
    this.finishChange();
    return 'consumed';
  }

  /** Apply the pending operator to whole lines a..b. */
  private applyLines(a: number, b: number): VimOutcome {
    const op = this.op!;
    this.op = null;
    this.opCount = 1;
    const text = `${this.lines.slice(a, b + 1).map((l) => l.join('')).join('\n')}\n`;
    this.reg = { text, linewise: true };
    if (op === 'y') this.lastYank = text;
    if (op === 'y') {
      this.cur = { row: a, col: this.cur.row === a ? this.cur.col : 0 };
      this.finishChange();
      return 'consumed';
    }
    if (op === 'c') {
      this.lines.splice(a, b - a + 1, []);
      this.cur = { row: a, col: 0 };
      this.toInsert();
      return 'insert';
    }
    this.lines.splice(a, b - a + 1);
    if (this.lines.length === 0) this.lines.push([]);
    const row = Math.min(a, this.lines.length - 1);
    this.cur = { row, col: firstNonBlank(this.lines, row) };
    this.finishChange();
    return 'consumed';
  }

  private finishObject(t: string): VimOutcome {
    const kind = this.objPending!;
    this.objPending = null;
    this.count = '';
    if (!isObjectKey(t)) { this.cancelOp(); return 'consumed'; }
    this.record(t);
    const r = textObject(this.lines, this.cur, kind, t);
    if (!r || cmp(r.start, { row: r.end.row, col: r.end.col + 1 }) >= 0) {
      // No object here, or an empty one (`ci(` on `()` changes nothing but enters insert).
      if (r && this.op === 'c') {
        this.op = null;
        this.cur = { ...r.start };
        this.toInsert();
        return 'insert';
      }
      this.cancelOp();
      return 'consumed';
    }
    if (this.m === 'visual' || this.m === 'visual-line') {
      this.anchor = { ...r.start };
      this.cur = { ...r.end };
      if (linewiseObject(t)) this.m = 'visual-line';
      return 'consumed';
    }
    if (linewiseObject(t)) return this.applyLines(r.start.row, r.end.row);
    return this.applyRange(r.start, { row: r.end.row, col: r.end.col + 1 });
  }

  private finishReplace(t: string): VimOutcome {
    this.replacePending = false;
    const n = this.takeCount();
    const ch = t === 'Space' ? ' ' : t;
    if ([...ch].length !== 1) { this.recording = null; this.dropChange(); return 'consumed'; }
    const line = this.lines[this.cur.row]!;
    if (this.cur.col + n > line.length) { this.recording = null; this.dropChange(); return 'consumed'; }
    this.record(t);
    for (let i = 0; i < n; i++) line[this.cur.col + i] = ch;
    this.cur.col += n - 1;
    this.finishChange();
    return 'consumed';
  }

  /** Esc or a key no operator takes: drop the operator and its undo step. */
  private cancelOp(): void {
    if (this.op || this.replacePending) this.dropChange();
    this.op = null;
    this.opCount = 1;
    this.count = '';
    if (!this.replaying) this.recording = null;
  }

  // ----- visual mode -----

  private visualKey(t: string): VimOutcome {
    const hadCount = this.count !== '';
    const n = this.takeCount();
    const line = this.m === 'visual-line';
    switch (t) {
      case 'v': case 'V': {
        const want = t === 'v' ? 'visual' : 'visual-line';
        if (this.m === want) { this.m = 'normal'; this.anchor = null; } else this.m = want;
        return 'consumed';
      }
      case 'd': case 'x': case 'c': case 's': case 'y': {
        const op: Op = t === 'x' ? 'd' : t === 's' ? 'c' : (t as Op);
        const [a, b] = cmp(this.anchor!, this.cur) <= 0 ? [this.anchor!, this.cur] : [this.cur, this.anchor!];
        this.m = 'normal';
        this.anchor = null;
        this.recording = null;
        this.pushUndo();
        this.op = op;
        const out = line ? this.applyLines(a.row, b.row) : this.applyRange(a, { row: b.row, col: b.col + 1 });
        if (op === 'y') this.dropChange();
        return out;
      }
      case 'i': case 'a':
        this.objPending = t;
        return 'consumed';
      case 'g':
        this.gPending = true;
        return 'consumed';
      case 'f': case 't': case 'F': case 'T':
        this.searchPending = t;
        if (hadCount) this.count = String(n);
        return 'consumed';
      default: {
        const m = this.motion(t, n, hadCount);
        if (m) this.moveTo(m, t);
        return 'consumed';
      }
    }
  }

  // ---------- edits ----------

  private lineLen(row = this.cur.row): number {
    return this.lines[row]?.length ?? 0;
  }

  /** Copy (and for d / c remove) [s, e) into the register. */
  private yankDelete(s: Pos, e: Pos, linewise: boolean, op: Op): void {
    const text = this.slice(s, e);
    if (text !== '' || op === 'y') this.reg = { text, linewise };
    if (op === 'y') this.lastYank = text;
    if (op === 'y') return;
    const head = this.lines[s.row]!.slice(0, s.col);
    const tail = (this.lines[e.row] ?? []).slice(e.col);
    this.lines.splice(s.row, e.row - s.row + 1, [...head, ...tail]);
    this.cur = { ...s };
  }

  private slice(s: Pos, e: Pos): string {
    if (s.row === e.row) return this.lines[s.row]!.slice(s.col, e.col).join('');
    const parts = [this.lines[s.row]!.slice(s.col).join('')];
    for (let r = s.row + 1; r < e.row; r++) parts.push(this.lines[r]!.join(''));
    parts.push((this.lines[e.row] ?? []).slice(0, e.col).join(''));
    return parts.join('\n');
  }

  /** `J`: join the next line with one space unless either side is blank (motions.rs `join_lines`). */
  private joinLine(): void {
    const r = this.cur.row;
    if (r + 1 >= this.lines.length) return;
    const a = this.lines[r]!;
    const b = this.lines[r + 1]!;
    let k = 0;
    while (k < b.length && /\s/u.test(b[k]!)) k++;
    const rest = b.slice(k);
    const space = a.length > 0 && rest.length > 0 && !/\s/u.test(a[a.length - 1]!) ? [' '] : [];
    this.cur = { row: r, col: a.length };
    this.lines.splice(r, 2, [...a, ...space, ...rest]);
  }

  /** `p` / `P`: lines go below / above; text after / at the cursor. */
  private put(before: boolean): void {
    const { linewise } = this.single ? { linewise: false } : this.reg;
    const text = this.single ? this.reg.text.replace(/\n$/, '') : this.reg.text;
    if (linewise) {
      const add = toLines(text.endsWith('\n') ? text.slice(0, -1) : text);
      const at = before ? this.cur.row : this.cur.row + 1;
      this.lines.splice(at, 0, ...add);
      this.cur = { row: at, col: firstNonBlank(this.lines, at) };
      return;
    }
    if (!before && this.lineLen() > 0) this.cur.col += 1;
    this.insertText(text);
    this.cur.col = Math.max(0, this.cur.col - 1);
  }

  private clampNormal(): void {
    this.cur.row = Math.max(0, Math.min(this.cur.row, this.lines.length - 1));
    const max = Math.max(0, this.lineLen() - 1);
    this.cur.col = Math.max(0, Math.min(this.cur.col, max));
  }

  // ---------- undo ----------

  private snapshot(): Snapshot {
    return { text: this.text, cursor: { ...this.cur } };
  }

  private pushUndo(): void {
    if (this.replaying) return;
    this.undoStack.push(this.snapshot());
    if (this.undoStack.length > MAX_UNDO) this.undoStack.shift();
    this.redoStack = [];
  }

  /** Forget the undo step a change pushed (a cancelled operator or a yank). */
  private dropChange(): void {
    if (this.replaying) return;
    const top = this.undoStack[this.undoStack.length - 1];
    if (top && top.text === this.text) this.undoStack.pop();
  }

  private restore(s: Snapshot): void {
    this.lines = toLines(s.text);
    this.cur = { ...s.cursor };
    this.clampNormal();
  }

  undo(): void {
    const s = this.undoStack.pop();
    if (!s) return;
    this.redoStack.push(this.snapshot());
    this.restore(s);
  }

  redo(): void {
    const s = this.redoStack.pop();
    if (!s) return;
    this.undoStack.push(this.snapshot());
    this.restore(s);
  }

  // ---------- dot repeat ----------

  private startChange(key: string): void {
    this.pushUndo();
    if (this.replaying) return;
    this.recording = [key];
    this.insertStart = null;
  }

  private record(key: string): void {
    if (!this.replaying && this.recording) this.recording.push(key);
  }

  private finishChange(): void {
    if (this.replaying) return;
    if (this.recording && this.recording[0] !== 'y') this.lastChange = { keys: this.recording, inserted: null };
    if (this.recording?.[0] === 'y') this.dropChange();
    this.recording = null;
  }

  /** `.`: replay the last change's keys, then its typed text (normal.rs dot repeat). */
  private dotRepeat(): void {
    const change = this.lastChange;
    if (!change) return;
    this.pushUndo();
    this.replaying = true;
    try {
      for (const k of change.keys) this.key(k);
      if (this.m === 'insert') {
        if (change.inserted) this.insertText(change.inserted);
        this.leaveInsert();
      }
    } finally {
      this.replaying = false;
      this.recording = null;
      this.insertStart = null;
    }
    this.lastChange = change;
  }
}
