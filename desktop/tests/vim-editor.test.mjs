import { test } from 'node:test';
import assert from 'node:assert/strict';
import { VimEditor } from '../dist/vim-editor.js';
import {
  charSearch, insertedText, matchBracket, offsetToPos, posToOffset, textObject, toLines, wordBack, wordEnd, wordForward,
} from '../dist/vim-text.js';

/** An editor in normal mode; `|` in the text marks the cursor. */
function ed(marked) {
  const at = marked.indexOf('|');
  const text = marked.replace('|', '');
  const e = new VimEditor(text, 'normal');
  e.setText(text, at < 0 ? 0 : at);
  return e;
}

/** Feed keys: plain characters, `<Esc>`, `<C-r>`, `<Left>`, `<Space>`. */
function feed(e, keys) {
  const out = [];
  for (const m of keys.matchAll(/<([^>]+)>|./gsu)) out.push(e.key(m[1] ?? m[0]));
  return out;
}

/** The text with `|` at the cursor. */
function show(e) {
  const t = e.text;
  const o = e.cursorOffset();
  return `${t.slice(0, o)}|${t.slice(o)}`;
}

function run(marked, keys) {
  const e = ed(marked);
  feed(e, keys);
  return show(e);
}

// ---------- helpers (vim-text.ts) ----------

test('offsets round-trip through code points, surrogate pairs included', () => {
  const lines = toLines('a😀b\ncd');
  assert.deepEqual(offsetToPos(lines, 3), { row: 0, col: 2 });
  assert.equal(posToOffset(lines, { row: 0, col: 2 }), 3);
  assert.deepEqual(offsetToPos(lines, 5), { row: 1, col: 0 });
  assert.equal(posToOffset(lines, { row: 1, col: 1 }), 6);
});

test('word motions cross lines and stop on empty lines', () => {
  const lines = toLines('foo bar\n\nbaz.qux');
  assert.deepEqual(wordForward(lines, { row: 0, col: 0 }), { row: 0, col: 4 });
  assert.deepEqual(wordForward(lines, { row: 0, col: 4 }), { row: 1, col: 0 });
  assert.deepEqual(wordForward(lines, { row: 1, col: 0 }), { row: 2, col: 0 });
  assert.deepEqual(wordForward(lines, { row: 2, col: 0 }), { row: 2, col: 3 });
  assert.deepEqual(wordEnd(lines, { row: 0, col: 0 }), { row: 0, col: 2 });
  assert.deepEqual(wordEnd(lines, { row: 0, col: 2 }), { row: 0, col: 6 });
  assert.deepEqual(wordBack(lines, { row: 2, col: 4 }), { row: 2, col: 3 });
  assert.deepEqual(wordBack(lines, { row: 2, col: 0 }), { row: 1, col: 0 });
  assert.deepEqual(wordBack(lines, { row: 1, col: 0 }), { row: 0, col: 4 });
});

test('character search finds on the line and a repeat skips an adjacent till target', () => {
  const lines = toLines('a,b,c');
  assert.equal(charSearch(lines, { row: 0, col: 0 }, { dir: 'f', ch: ',' }, false), 1);
  assert.equal(charSearch(lines, { row: 0, col: 0 }, { dir: 't', ch: ',' }, false), 0);
  assert.equal(charSearch(lines, { row: 0, col: 0 }, { dir: 't', ch: ',' }, true), 2);
  assert.equal(charSearch(lines, { row: 0, col: 4 }, { dir: 'F', ch: ',' }, false), 3);
  assert.equal(charSearch(lines, { row: 0, col: 4 }, { dir: 'T', ch: ',' }, false), 4);
  assert.equal(charSearch(lines, { row: 0, col: 4 }, { dir: 'f', ch: ',' }, false), null);
});

test('matching bracket nests and spans lines', () => {
  const lines = toLines('f(a, (b))\n{\n}');
  assert.deepEqual(matchBracket(lines, { row: 0, col: 0 }), { row: 0, col: 8 });
  assert.deepEqual(matchBracket(lines, { row: 0, col: 8 }), { row: 0, col: 1 });
  assert.deepEqual(matchBracket(lines, { row: 1, col: 0 }), { row: 2, col: 0 });
});

test('text objects: words, quotes, nested brackets, sentences, paragraphs', () => {
  const l = toLines('say "hi there" (a (b) c) end');
  assert.deepEqual(textObject(l, { row: 0, col: 1 }, 'i', 'w'), { start: { row: 0, col: 0 }, end: { row: 0, col: 2 } });
  assert.deepEqual(textObject(l, { row: 0, col: 1 }, 'a', 'w'), { start: { row: 0, col: 0 }, end: { row: 0, col: 3 } });
  assert.deepEqual(textObject(l, { row: 0, col: 7 }, 'i', '"'), { start: { row: 0, col: 5 }, end: { row: 0, col: 12 } });
  assert.deepEqual(textObject(l, { row: 0, col: 7 }, 'a', '"'), { start: { row: 0, col: 4 }, end: { row: 0, col: 13 } });
  assert.deepEqual(textObject(l, { row: 0, col: 22 }, 'i', 'b'), { start: { row: 0, col: 16 }, end: { row: 0, col: 22 } });
  assert.deepEqual(textObject(l, { row: 0, col: 19 }, 'a', '('), { start: { row: 0, col: 18 }, end: { row: 0, col: 20 } });
  assert.equal(textObject(l, { row: 0, col: 1 }, 'i', 'z'), null);
  const s = toLines('One. Two three. Four');
  assert.deepEqual(textObject(s, { row: 0, col: 7 }, 'i', 's'), { start: { row: 0, col: 5 }, end: { row: 0, col: 14 } });
  assert.deepEqual(textObject(s, { row: 0, col: 7 }, 'a', 's'), { start: { row: 0, col: 5 }, end: { row: 0, col: 15 } });
  const p = toLines('a\nb\n\nc');
  assert.deepEqual(textObject(p, { row: 1, col: 0 }, 'i', 'p'), { start: { row: 0, col: 0 }, end: { row: 1, col: 0 } });
  assert.deepEqual(textObject(p, { row: 1, col: 0 }, 'a', 'p'), { start: { row: 0, col: 0 }, end: { row: 2, col: 0 } });
});

test('insertedText is what an insert session typed', () => {
  assert.equal(insertedText('ab', 'aXYb'), 'XY');
  assert.equal(insertedText('aa', 'aaa'), 'a');
  assert.equal(insertedText('abc', 'ab'), '');
});

// ---------- motions and counts ----------

test('h j k l with counts, and j/k keep the wanted column across a short line', () => {
  assert.equal(run('ab|cdef', '2l'), 'abcd|ef');
  assert.equal(run('ab|cdef', '9l'), 'abcde|f');
  assert.equal(run('abc|def', '2h'), 'a|bcdef');
  const e = ed('abcdef\nx\nabcdef');
  feed(e, '4l');
  feed(e, 'j');
  assert.equal(show(e), 'abcdef\n|x\nabcdef');
  feed(e, 'j');
  assert.equal(show(e), 'abcdef\nx\nabcd|ef');
  assert.equal(run('a\nb\nc|', '<Up><Up>'), '|a\nb\nc');
});

test('word, line and buffer motions', () => {
  assert.equal(run('|one two three', 'w'), 'one |two three');
  assert.equal(run('|one two three', '2w'), 'one two |three');
  assert.equal(run('one two |three', 'b'), 'one |two three');
  assert.equal(run('|one two', 'e'), 'on|e two');
  assert.equal(run('  ab|cd', '0'), '|  abcd');
  assert.equal(run('  ab|cd', '^'), '  |abcd');
  assert.equal(run('|abcd', '$'), 'abc|d');
  assert.equal(run('a\nb\n|c', 'gg'), '|a\nb\nc');
  assert.equal(run('|a\nb\nc', 'G'), 'a\nb\n|c');
  assert.equal(run('|a\nb\nc', '2G'), 'a\n|b\nc');
  assert.equal(run('a\nb\n|c', '2gg'), 'a\n|b\nc');
  assert.equal(run('|a\nb\n\nc\n\nd', '}'), 'a\nb\n|\nc\n\nd');
  assert.equal(run('|a\nb\n\nc\n\nd', '2}'), 'a\nb\n\nc\n|\nd');
  assert.equal(run('a\n\nb|c', '{'), 'a\n|\nbc');
  assert.equal(run('|x(a[b]c)', '%'), 'x(a[b]c|)');
});

test('count digits: 1-9 start a count, 0 continues it, bare 0 is the line start', () => {
  assert.equal(run('|' + 'x'.repeat(30), '10l'), 'x'.repeat(10) + '|' + 'x'.repeat(20));
  assert.equal(run('abc|d', '0'), '|abcd');
});

test('f t F T with ; and ,', () => {
  assert.equal(run('|a,b,c,d', 'f,'), 'a|,b,c,d');
  assert.equal(run('|a,b,c,d', 'f,;'), 'a,b|,c,d');
  assert.equal(run('|a,b,c,d', '2f,'), 'a,b|,c,d');
  assert.equal(run('|a,b,c,d', 'f,;,'), 'a|,b,c,d');
  assert.equal(run('|a,b,c,d', 't,'), '|a,b,c,d');
  assert.equal(run('|ab,c', 't,'), 'a|b,c');
  assert.equal(run('|ab,cd,e', 't,;'), 'ab,c|d,e');
  assert.equal(run('a,b,c|d', 'F,'), 'a,b|,cd');
  assert.equal(run('a,b,c|d', 'T,'), 'a,b,|cd');
  assert.equal(run('|abc', 'fz'), '|abc');
});

// ---------- mode transitions ----------

test('i a A I o O enter insert mode at vim positions', () => {
  const at = (marked, k) => { const e = ed(marked); assert.equal(e.key(k), 'insert'); assert.equal(e.mode, 'insert'); return show(e); };
  assert.equal(at('ab|c', 'i'), 'ab|c');
  assert.equal(at('ab|c', 'a'), 'abc|');
  assert.equal(at('|abc', 'A'), 'abc|');
  assert.equal(at('  ab|c', 'I'), '  |abc');
  assert.equal(at('a|b\nc', 'o'), 'ab\n|\nc');
  assert.equal(at('ab\nc|d', 'O'), 'ab\n|\ncd');
});

test('Esc leaves insert mode one column left; Esc in idle normal mode is not the editor’s', () => {
  const e = ed('|ab');
  feed(e, 'A');
  e.insertText('cd');
  assert.equal(e.key('Esc'), 'consumed');
  assert.equal(e.mode, 'normal');
  assert.equal(show(e), 'abc|d');
  assert.equal(e.key('Esc'), 'unhandled');
  feed(e, '2');
  assert.equal(e.key('Esc'), 'consumed');
  assert.equal(e.idle(), true);
});

test('keys the editor does not know are unhandled so the caller can pass them on', () => {
  const e = ed('|abc');
  assert.equal(e.key('Space'), 'unhandled');
  assert.equal(e.key('Enter'), 'unhandled');
  assert.equal(e.key('C-k'), 'unhandled');
  assert.equal(e.key('l'), 'consumed');
});

// ---------- direct editing ----------

test('x s r D C J ~ with counts', () => {
  assert.equal(run('a|bcd', 'x'), 'a|cd');
  assert.equal(run('a|bcd', '3x'), '|a');
  const s = ed('a|bcd');
  assert.equal(s.key('2'), 'consumed');
  assert.equal(s.key('s'), 'insert');
  assert.equal(show(s), 'a|d');
  assert.equal(run('a|bc', 'rX'), 'a|Xc');
  assert.equal(run('a|bcd', '2rX'), 'aX|Xd');
  assert.equal(run('a|bcd', 'rX<Esc>'), 'a|Xcd');
  assert.equal(run('ab|cd', 'D'), 'a|b');
  const c = ed('ab|cd');
  assert.equal(c.key('C'), 'insert');
  assert.equal(show(c), 'ab|');
  assert.equal(run('|ab\n  cd', 'J'), 'ab| cd');
  assert.equal(run('|a\nb\nc', '2J'), 'a b| c');
  assert.equal(run('|ab\n', 'J'), 'a|b');
  assert.equal(run('|aBc', '~'), 'A|Bc');
  assert.equal(run('|aBc', '3~'), 'Ab|C');
});

test('u undoes a change and Ctrl-R redoes it; an insert session is one step', () => {
  const e = ed('|one two');
  feed(e, 'dw');
  assert.equal(e.text, 'two');
  feed(e, 'u');
  assert.equal(e.text, 'one two');
  feed(e, '<C-r>');
  assert.equal(e.text, 'two');
  const i = ed('|ab');
  feed(i, 'ciw');
  i.insertText('xyz');
  feed(i, '<Esc>');
  assert.equal(i.text, 'xyz');
  feed(i, 'u');
  assert.equal(i.text, 'ab');
  const n = ed('|ab');
  feed(n, 'i<Esc>');
  feed(n, 'x');
  feed(n, 'u');
  feed(n, 'u');
  assert.equal(n.text, 'ab');
});

test('p puts after the cursor, linewise registers below; y copies for the clipboard', () => {
  const e = ed('|one two');
  feed(e, 'yw');
  assert.equal(e.takeYank(), 'one ');
  assert.equal(e.takeYank(), null);
  feed(e, '$p');
  assert.equal(e.text, 'one twoone ');
  const l = ed('|a\nb');
  feed(l, 'yyjp');
  assert.equal(show(l), 'a\nb\n|a');
  const d = ed('|a\nb');
  feed(d, 'ddp');
  assert.equal(d.text, 'b\na');
  assert.equal(d.takeYank(), null);
});

// ---------- operators ----------

test('d c y with motions and counts', () => {
  assert.equal(run('|one two three', 'dw'), '|two three');
  assert.equal(run('|one two three', '2dw'), '|three');
  assert.equal(run('|one two three', 'd2w'), '|three');
  assert.equal(run('one |two', 'db'), '|two');
  assert.equal(run('|one two', 'de'), '| two');
  assert.equal(run('o|ne two', 'd$'), '|o');
  assert.equal(run('one t|wo', 'd0'), '|wo');
  assert.equal(run('one |two', 'dw'), 'one| ');
  assert.equal(run('|a(b)c', 'df)'), '|c');
  assert.equal(run('|say "hi"', 'dt"'), '|"hi"');
  assert.equal(run('ab|cd', 'dh'), 'a|cd');
  assert.equal(run('ab|cd', 'dl'), 'ab|d');
  const c = ed('|one two');
  assert.deepEqual(feed(c, 'cw'), ['consumed', 'insert']);
  assert.equal(show(c), '| two');
  assert.equal(run('|one two', 'yw'), '|one two');
  assert.equal(run('one |two', 'yb'), '|one two');
});

test('dd cc yy and linewise motions', () => {
  assert.equal(run('a\n|b\nc', 'dd'), 'a\n|c');
  assert.equal(run('a\nb\n|c', 'dd'), 'a\n|b');
  assert.equal(run('|a\nb\nc', '2dd'), '|c');
  assert.equal(run('a\n|b\nc', 'dj'), '|a');
  assert.equal(run('a\nb\n|c', 'dk'), '|a');
  assert.equal(run('a\nb\n|c\nd', 'dgg'), '|d');
  assert.equal(run('a\n|b\nc', 'dG'), '|a');
  assert.equal(run('|only', 'dd'), '|');
  const c = ed('a\n|  b\nc');
  assert.deepEqual(feed(c, 'cc'), ['consumed', 'insert']);
  assert.equal(show(c), 'a\n|\nc');
  const y = ed('|a\nb');
  feed(y, 'yy');
  assert.equal(y.register(), 'a\n');
  assert.equal(y.text, 'a\nb');
});

test('operators compose with text objects', () => {
  assert.equal(run('one tw|o three', 'diw'), 'one | three');
  assert.equal(run('one tw|o three', 'daw'), 'one |three');
  assert.equal(run('say "h|i there" ok', 'di"'), 'say "|" ok');
  assert.equal(run('say "h|i there" ok', 'da"'), 'say | ok');
  assert.equal(run('f(a, (|b), c)', 'di('), 'f(a, (|), c)');
  assert.equal(run('x{ a |b }', 'diB'), 'x{|}');
  assert.equal(run('x[1, |2]', 'da['), '|x');
  assert.equal(run('One. Tw|o three. Four', 'das'), 'One. |Four');
  assert.equal(run('a\n|b\n\nc', 'dap'), '|c');
  const c = ed('fn(|)');
  assert.deepEqual(feed(c, 'ci('), ['consumed', 'consumed', 'insert']);
  assert.equal(show(c), 'fn(|)');
  const y = ed('x(a|bc)');
  feed(y, 'yi(');
  assert.equal(y.register(), 'abc');
  assert.equal(y.takeYank(), 'abc');
  assert.equal(run('one tw|o', 'dix'), 'one tw|o');
});

test('an unknown key cancels a pending operator without changing anything', () => {
  const e = ed('|abc');
  feed(e, 'dz');
  assert.equal(e.text, 'abc');
  assert.equal(e.idle(), true);
  feed(e, 'd<Esc>x');
  assert.equal(e.text, 'bc');
  feed(e, 'u');
  assert.equal(e.text, 'abc');
});

// ---------- visual mode ----------

test('v selects characters, motions extend it, d deletes it', () => {
  const e = ed('a|bcdef');
  feed(e, 'vl');
  assert.equal(e.mode, 'visual');
  assert.deepEqual(e.selection(), { start: 1, end: 3 });
  feed(e, 'd');
  assert.equal(show(e), 'a|def');
  assert.equal(e.mode, 'normal');
  assert.equal(run('a|bcdef', 'vex'), '|a');
});

test('V selects lines; v and V toggle and switch; Esc leaves visual mode', () => {
  const e = ed('a\n|b\nc');
  feed(e, 'V');
  assert.deepEqual(e.selection(), { start: 2, end: 3 });
  feed(e, 'j');
  assert.deepEqual(e.selection(), { start: 2, end: 5 });
  feed(e, 'v');
  assert.equal(e.mode, 'visual');
  feed(e, 'V');
  assert.equal(e.mode, 'visual-line');
  feed(e, 'V');
  assert.equal(e.mode, 'normal');
  feed(e, 'v<Esc>');
  assert.equal(e.mode, 'normal');
  assert.equal(e.selection(), null);
  assert.equal(run('a\n|b\nc', 'Vjd'), '|a');
});

test('visual c and s change the selection, y yanks it, iw selects a word', () => {
  const c = ed('a|bcd');
  assert.deepEqual(feed(c, 'vlc'), ['consumed', 'consumed', 'insert']);
  assert.equal(show(c), 'a|d');
  const y = ed('a|bcd');
  feed(y, 'vly');
  assert.equal(y.register(), 'bc');
  assert.equal(y.takeYank(), 'bc');
  assert.equal(show(y), 'a|bcd');
  assert.equal(y.mode, 'normal');
  const w = ed('one tw|o three');
  feed(w, 'viw');
  assert.deepEqual(w.selection(), { start: 4, end: 7 });
  feed(w, 'd');
  assert.equal(w.text, 'one  three');
  assert.equal(run('ab|c,d', 'vf,d'), 'ab|d');
});

// ---------- dot repeat ----------

test('. repeats the last change: dw, x, r, a ciw with its typed text', () => {
  assert.equal(run('|a b c d', 'dw.'), '|c d');
  const x = ed('|abcdef');
  feed(x, '3x.');
  assert.equal(x.text, 'ef');
  assert.equal(run('|abc', 'rXl.'), 'X|Xc');
  const c = ed('|foo bar');
  feed(c, 'ciw');
  c.insertText('new');
  feed(c, '<Esc>');
  feed(c, 'w.');
  assert.equal(c.text, 'new new');
  assert.deepEqual(c.lastChangeRecord(), { keys: ['c', 'i', 'w'], inserted: 'new' });
  const a = ed('|x\ny');
  feed(a, 'A');
  a.insertText('!');
  feed(a, '<Esc>j.');
  assert.equal(a.text, 'x!\ny!');
});

test('. after a yank or a motion repeats the earlier change; one u undoes a whole repeat', () => {
  const e = ed('|a b c');
  feed(e, 'dwywl.');
  assert.equal(e.text, 'bc');
  assert.deepEqual(e.lastChangeRecord(), { keys: ['d', 'w'], inserted: null });
  const u = ed('|foo bar');
  feed(u, 'ciw');
  u.insertText('xy');
  feed(u, '<Esc>w.u');
  assert.equal(u.text, 'xy bar');
});

test('setText from the field resets a half-typed command and moves the cursor', () => {
  const e = ed('|abc');
  feed(e, 'd');
  e.setText('abcd', 4);
  assert.equal(e.idle(), true);
  assert.equal(show(e), 'abc|d');
  e.enterInsert('a');
  assert.equal(e.mode, 'insert');
  assert.equal(show(e), 'abcd|');
});
