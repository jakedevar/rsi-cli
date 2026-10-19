// Hub #3106 / Issue #116: the desktop vim editor against the TUI's grammar
// (docs/keybindings.md "Vim Text Editing", crates/rsi/src/vim_textarea/tests.rs):
// counts, text objects, one-line fields, and the viewer's colon line.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { VimEditor } from '../dist/vim-editor.js';
import { viewerCommand } from '../dist/files-model.js';

function ed(marked, opts) {
  const at = marked.indexOf('|');
  const text = marked.replace('|', '');
  const e = new VimEditor(text, 'normal', opts);
  e.setText(text, at < 0 ? 0 : at);
  return e;
}
function feed(e, keys) {
  for (const m of keys.matchAll(/<([^>]+)>|./gsu)) e.key(m[1] ?? m[0]);
  return e;
}
function show(e) {
  const o = e.cursorOffset();
  return `${e.text.slice(0, o)}|${e.text.slice(o)}`;
}
const run = (marked, keys, opts) => show(feed(ed(marked, opts), keys));

// ---------- counts ----------

const COUNTS = [
  ['|a b c d e f', '3w', 'a b c |d e f'],
  ['|a b c d e f', '2dw', '|c d e f'],
  ['|a b c d e f', 'd2w', '|c d e f'],
  ['|a b c d e f', '2d2w', '|e f'],
  ['|abcdef', '3x', '|def'],
  ['|abcdef', '3~', 'ABC|def'],
  ['|abcdef', '3rX', 'XX|Xdef'],
  ['|abcdef', '9x', '|'],
  ['|1\n2\n3\n4\n5\n6', '5j', '1\n2\n3\n4\n5\n|6'],
  ['|1\n2\n3\n4', '2dd', '|3\n4'],
  ['|1\n2\n3\n4', '3yyGp', '1\n2\n3\n4\n|1\n2\n3'],
  ['|1\n2\n3\n4', '3G', '1\n2\n|3\n4'],
  ['|1\n2\n3\n4', '3gg', '1\n2\n|3\n4'],
  ['|a\nb\nc\nd', '3J', 'a b c| d'],
  ['x|abcabcabc', '2fb', 'xabca|bcabc'],
    ['|abcdef', '2l', 'ab|cdef'],
  ['|abc def', '1000x', '|'],
];
for (const [from, keys, want] of COUNTS) {
  test(`count: ${JSON.stringify(from)} ${keys}`, () => assert.equal(run(from, keys), want));
}

test('the digit 0 continues a count and is the line start otherwise', () => {
  assert.equal(run('ab|cdef', '0'), '|abcdef');
  assert.equal(run('|1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11', '10j'), '1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n|11');
});

// ---------- text objects ----------

const OBJECTS = [
  ['one |two three', 'diw', 'one | three'],
  ['one |two three', 'daw', 'one |three'],
  ['say "he|llo" now', 'di"', 'say "|" now'],
  ['say "he|llo" now', 'da"', 'say | now'],
  ["say 'he|llo' now", "di'", "say '|' now"],
  ['f(a, |b)', 'di(', 'f(|)'],
  ['f(a, |b)', 'dab', '|f'],
  ['f(a, |b)', 'di)', 'f(|)'],
  ['f(a, |b)', 'dib', 'f(|)'],
  ['{ a |b }', 'di{', '{|}'],
  ['{ a |b }', 'daB', '|'],
  ['[1, |2]', 'di[', '[|]'],
  ['[1, |2]', 'da]', '|'],
  ['x(a(|b)c)y', 'di(', 'x(a(|)c)y'],
  ['one. tw|o. three.', 'dis', 'one. | three.'],
  ['one. tw|o. three.', 'das', 'one. |three.'],
  ['a\n\nb |c\nd\n\ne', 'dip', 'a\n\n|\ne'],
  ['a\n\nb |c\nd\n\ne', 'dap', 'a\n\n|e'],
  ['say "he|llo" now', 'yi"P', 'say "hell|ohello" now'],
  ['no quotes |here', 'di"', 'no quotes |here'],
];
for (const [from, keys, want] of OBJECTS) {
  test(`object: ${JSON.stringify(from)} ${keys}`, () => assert.equal(run(from, keys), want));
}

test('ciw then typed text, undone by u', () => {
  const e = ed('one |two three');
  feed(e, 'ciw');
  e.insertText('X');
  e.key('Esc');
  assert.equal(show(e), 'one |X three');
  feed(e, 'u');
  assert.equal(e.text, 'one two three');
});

test('v + an object selects it and an operator applies', () => {
  const e = ed('say "he|llo" now');
  feed(e, 'vi"');
  assert.equal(e.mode, 'visual');
  assert.deepEqual(e.selection(), { start: 5, end: 10 });
  feed(e, 'd');
  assert.equal(e.text, 'say "" now');
});

// ---------- modes ----------

test('the four modes and their transitions', () => {
  const e = ed('|abc\ndef');
  assert.equal(e.mode, 'normal');
  feed(e, 'v');
  assert.equal(e.mode, 'visual');
  feed(e, 'V');
  assert.equal(e.mode, 'visual-line');
  feed(e, '<Esc>');
  assert.equal(e.mode, 'normal');
  feed(e, 'i');
  assert.equal(e.mode, 'insert');
  e.leaveInsert();
  assert.equal(e.mode, 'normal');
});

test('visual line d removes whole lines and y copies them', () => {
  const e = ed('|a\nb\nc');
  feed(e, 'Vjd');
  assert.equal(e.text, 'c');
  assert.equal(e.register(), 'a\nb\n');
});

test('dot repeats a change with typed text', () => {
  const e = ed('|aa bb cc');
  feed(e, 'cw');
  e.insertText('X');
  e.key('Esc');
  feed(e, 'w.');
  assert.equal(e.text, 'X X cc');
});

// ---------- one-line fields ----------

const ONE = { singleLine: true };

test('a one-line field never gains a line: o and O open at the ends, J is a no-op', () => {
  const o = ed('ab|c', ONE);
  feed(o, 'o');
  assert.equal(o.mode, 'insert');
  assert.equal(o.text, 'abc');
  assert.equal(o.cursorOffset(), 3);
  const big = ed('  ab|c', ONE);
  feed(big, 'O');
  assert.equal(big.text, '  abc');
  assert.equal(big.cursorOffset(), 2);
  assert.equal(run('ab|c', 'J', ONE), 'ab|c');
});

test('a one-line field flattens a linewise put and a multi-line paste', () => {
  assert.equal(run('|abc', 'yyp', ONE), 'aab|cbc');
  const e = ed('|ab', ONE);
  e.setRegister('x\ny\n', true);
  feed(e, 'p');
  assert.equal(e.text, 'ax yb');
  const t = new VimEditor('a\nb', 'insert', ONE);
  assert.equal(t.text, 'a b');
});

test('a one-line field keeps counts, objects and the dot', () => {
  assert.equal(run('|one two three', '2dw', ONE), '|three');
  assert.equal(run('say "he|llo"', 'ci"', ONE), 'say "|"');
  assert.equal(run('|a b c', 'dw.', ONE), '|c');
  assert.equal(run('a|bc', 'dd', ONE), '|');
  assert.equal(run('a|bc', 'jk', ONE), 'a|bc');
});

// ---------- the viewer's colon line (the TUI's file_viewer_commands.rs forms; saving is #113) ----------

test('colon commands: quit, goto, reload and the save forms', () => {
  for (const c of ['q', ':q', 'quit']) assert.deepEqual(viewerCommand(c), { kind: 'quit', force: false }, c);
  for (const c of ['q!', 'quit!', ':q!']) assert.deepEqual(viewerCommand(c), { kind: 'quit', force: true }, c);
  assert.deepEqual(viewerCommand('42'), { kind: 'goto', line: 42 });
  assert.deepEqual(viewerCommand('e!'), { kind: 'reload', force: true });
  assert.deepEqual(viewerCommand('e'), { kind: 'reload', force: false });
  assert.deepEqual(viewerCommand('w'), { kind: 'save', force: false, close: false });
  assert.deepEqual(viewerCommand('w!'), { kind: 'save', force: true, close: false });
  for (const c of ['wq', 'x']) assert.deepEqual(viewerCommand(c), { kind: 'save', force: false, close: true }, c);
  assert.deepEqual(viewerCommand(''), { kind: 'none' });
  assert.equal(viewerCommand('bogus').kind, 'say');
});
