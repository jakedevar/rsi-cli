// The file viewer's reading tools (gap sweep G4, hub #3000): column motions,
// search, folds, the highlighter, segments and the keys that reach them.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  Folds, advanceMatch, bracketPartner, colMotion, cpCol, displayIndex, findMatches, foldRegions, highlightLine,
  languageOf, lexStates, matchFrom, segments, utf16Col, wordUnderCursor,
} from '../dist/viewer-model.js';
import { NO_VIEWER_KEYS, viewerCommand, viewerKey } from '../dist/files-model.js';
import { toLines } from '../dist/vim-text.js';

const run = (keys) => {
  let st = NO_VIEWER_KEYS;
  let cmd;
  for (const k of keys) ({ cmd, next: st } = viewerKey(st, typeof k === 'string' ? { key: k } : k));
  return cmd;
};

test('column motions: h l 0 ^ $ w b e W B E % and counts, clamped to a character', () => {
  const L = toLines('  let foo = bar(1);\n\nnext.line here');
  const at = (row, col) => ({ row, col });
  assert.deepEqual(colMotion(L, at(0, 5), 'h'), at(0, 4));
  assert.deepEqual(colMotion(L, at(0, 5), 'h', 9), at(0, 0));
  assert.deepEqual(colMotion(L, at(0, 5), 'l', 99), at(0, 18), 'l stops on the last character');
  assert.deepEqual(colMotion(L, at(0, 9), '0'), at(0, 0));
  assert.deepEqual(colMotion(L, at(0, 9), '^'), at(0, 2));
  assert.deepEqual(colMotion(L, at(0, 0), '$'), at(0, 18));
  assert.deepEqual(colMotion(L, at(0, 2), 'w'), at(0, 6));
  assert.deepEqual(colMotion(L, at(0, 2), 'w', 2), at(0, 10));
  assert.deepEqual(colMotion(L, at(0, 10), 'b'), at(0, 6));
  assert.deepEqual(colMotion(L, at(0, 6), 'e'), at(0, 8));
  assert.deepEqual(colMotion(L, at(2, 0), 'W'), at(2, 10), 'W skips punctuation inside a WORD');
  assert.deepEqual(colMotion(L, at(2, 10), 'B'), at(2, 0));
  assert.deepEqual(colMotion(L, at(2, 0), 'E'), at(2, 8));
  assert.deepEqual(colMotion(L, at(0, 14), '%'), at(0, 17), '% jumps to the bracket matching the next one on the line');
  assert.deepEqual(colMotion(L, at(1, 0), 'l'), at(1, 0), 'an empty line keeps column 0');
  assert.deepEqual(colMotion(L, at(0, 3), '}'), at(1, 0));
});

test('code-point columns convert to and from UTF-16 offsets', () => {
  const line = 'a😀b';
  assert.equal(utf16Col(line, 2), 3);
  assert.equal(cpCol(line, 3), 2);
  assert.equal(utf16Col(line, 9), line.length);
});

test('search finds every overlapping, case-sensitive hit and lands like the TUI', () => {
  const lines = ['fn foo() {', '    foo(); Foo', '}', 'aaa'];
  const m = findMatches(lines, 'foo');
  assert.deepEqual(m, [{ row: 0, start: 3, end: 6 }, { row: 1, start: 4, end: 7 }]);
  assert.equal(findMatches(lines, 'aa').length, 2, 'overlapping hits count');
  assert.deepEqual(findMatches(lines, ''), []);
  // Forward: the first at or after the cursor, wrapping.
  assert.equal(matchFrom(m, { row: 0, col: 3 }, true), 0);
  assert.equal(matchFrom(m, { row: 0, col: 4 }, true), 1);
  assert.equal(matchFrom(m, { row: 2, col: 0 }, true), 0, 'wraps to the top');
  // Backward: the last before it, wrapping to the end.
  assert.equal(matchFrom(m, { row: 1, col: 4 }, false), 0);
  assert.equal(matchFrom(m, { row: 0, col: 0 }, false), 1);
  // * skips the hit under the cursor.
  assert.equal(matchFrom(m, { row: 0, col: 3 }, true, true), 1);
  assert.equal(advanceMatch(1, 2, true), 0);
  assert.equal(advanceMatch(0, 2, false), 1);
  assert.equal(advanceMatch(-1, 2, false), 1);
  assert.equal(advanceMatch(0, 0, true), -1);
});

test('the word under the cursor is letters, digits and underscores', () => {
  const L = toLines('let my_var2 = x;');
  assert.equal(wordUnderCursor(L, { row: 0, col: 6 }), 'my_var2');
  assert.equal(wordUnderCursor(L, { row: 0, col: 3 }), null);
  assert.equal(wordUnderCursor(L, { row: 0, col: 12 }), null);
});

test('fold regions: bracket pairs and block comments spanning lines, never inside strings', () => {
  const src = ['fn foo() {', '    let s = "{";', '    bar(', '        1,', '    );', '}', '/*', ' * doc', ' */'];
  const r = foldRegions('rust', src);
  assert.deepEqual(r, [[0, 5], [2, 4], [6, 8]]);
  assert.deepEqual(foldRegions(null, src), [], 'an unknown language has no folds');
  const py = ['def f():', '    x = 1', '', '    return x', 'y = 2'];
  assert.deepEqual(foldRegions('python', py), [[0, 3]]);
  const md = ['# A', 'text', '## B', 'more', '```', 'code', '```', '# C', 'end'];
  assert.deepEqual(foldRegions('markdown', md), [[0, 6], [2, 6], [4, 6], [7, 8]]);
});

test('folds hide their interior, pick the innermost region and reveal a hidden line', () => {
  const f = new Folds();
  f.setRegions([[0, 5], [0, 2], [3, 4]]);
  assert.deepEqual(f.regionAt(0), [0, 2], 'innermost region on the row');
  assert.equal(f.toggle(0), true);
  assert.deepEqual(f.display(7), [0, 3, 4, 5, 6]);
  assert.equal(f.summaryAt(0), 2);
  assert.equal(f.set(3, true), true);
  assert.deepEqual(f.display(7), [0, 3, 5, 6]);
  assert.equal(f.toggle(1), false, 'no fold starts on row 1');
  f.reveal(4);
  assert.deepEqual(f.display(7), [0, 3, 4, 5, 6]);
  f.closeAll();
  assert.deepEqual(f.display(7), [0, 6]);
  assert.equal(f.foldedCount(), 3);
  f.openAll();
  assert.equal(f.display(7).length, 7);
  f.toggle(3);
  f.setRegions([[0, 2]]);
  assert.equal(f.foldedCount(), 0, 'a region that no longer exists drops its fold');
  assert.equal(displayIndex([0, 3, 4, 9], 5), 2);
  assert.equal(displayIndex([0, 3, 4, 9], 9), 3);
});

test('languages follow the TUI tree-sitter registry by extension', () => {
  assert.equal(languageOf('src/main.rs'), 'rust');
  assert.equal(languageOf('a/b.tsx'), 'typescript');
  assert.equal(languageOf('x.yml'), 'yaml');
  assert.equal(languageOf('README.md'), 'markdown');
  assert.equal(languageOf('Makefile'), null);
  assert.equal(languageOf('.bashrc'), null);
});

const toks = (lang, line, state = '') => highlightLine(lang, line, state).spans.map((s) => [line.slice(s.from, s.to), s.tok]);

test('the highlighter tags keywords, strings, numbers, comments, types and functions', () => {
  assert.deepEqual(toks('rust', 'pub fn main() -> Result<u8> { let x = 42; // done'), [
    ['pub', 'keyword'], ['fn', 'keyword'], ['main', 'function'], ['(', 'punctuation'], [')', 'punctuation'],
    ['-', 'operator'], ['>', 'operator'], ['Result', 'type'], ['<', 'operator'], ['u8', 'type'], ['>', 'operator'],
    ['{', 'punctuation'], ['let', 'keyword'], ['=', 'operator'], ['42', 'number'], [';', 'punctuation'], ['// done', 'comment'],
  ]);
  assert.deepEqual(toks('rust', '#[derive(Debug)] println!("hi {}", a.b)').slice(0, 3), [
    ['#[derive(Debug)]', 'attribute'], ['println!', 'function'], ['(', 'punctuation'],
  ]);
  assert.deepEqual(toks('rust', "fn f<'a>(c: char) { 'x' }").filter(([, t]) => t === 'type' || t === 'string'), [
    ["'a", 'type'], ['char', 'type'], ["'x'", 'string'],
  ]);
  assert.deepEqual(toks('python', '@cache\ndef'.split('\n')[0]), [['@cache', 'attribute']]);
  assert.deepEqual(toks('json', '{"a": [1, true, "s"]}').map(([, t]) => t), [
    'punctuation', 'property', 'punctuation', 'punctuation', 'number', 'punctuation', 'constant', 'punctuation', 'string', 'punctuation', 'punctuation',
  ]);
  assert.deepEqual(toks('toml', '[package] # c'), [['[package]', 'tag'], ['# c', 'comment']]);
  assert.deepEqual(toks('toml', 'name = "rsi"'), [['name', 'property'], ['=', 'punctuation'], ['"rsi"', 'string']]);
  assert.deepEqual(toks('yaml', '  key: 3 # n'), [['key', 'property'], ['3', 'number'], ['# n', 'comment']]);
  assert.deepEqual(toks('markdown', '## Title'), [['## Title', 'heading']]);
  assert.deepEqual(toks('markdown', '- see `code` and **bold**'), [['-', 'punctuation'], ['`code`', 'code'], ['**bold**', 'strong']]);
  assert.deepEqual(toks('bash', 'echo "$HOME" # x').map(([, t]) => t), ['string', 'comment']);
});

test('multi-line comments, strings and fences carry their state to the next line', () => {
  assert.deepEqual(lexStates('rust', ['a /* x', 'still', '*/ b']), ['', 'c1', 'c1']);
  assert.deepEqual(toks('rust', 'still */ fn', 'c1'), [['still */', 'comment'], ['fn', 'keyword']]);
  assert.deepEqual(lexStates('rust', ['/* /* nested */', 'x */', 'y']), ['', 'c1', '']);
  assert.deepEqual(lexStates('python', ['s = """doc', 'more', '"""', 'x']), ['', 's"""', 's"""', '']);
  assert.deepEqual(lexStates('typescript', ['const t = `a', 'b`;', 'x']), ['', 's`', '']);
  assert.deepEqual(lexStates('markdown', ['```ts', 'let x', '```', 'text']), ['', 'f```', 'f```', '']);
  assert.deepEqual(toks('markdown', 'let x', 'f```'), [['let x', 'code']]);
});

test('segments cut a line at every span and decoration boundary', () => {
  const line = 'let x = 1;';
  const spans = highlightLine('rust', line, '').spans;
  const segs = segments(line, spans, [{ from: 4, to: 5, cls: 'cursor' }, { from: 0, to: 3, cls: 'match' }]);
  assert.equal(segs.map((s) => s.text).join(''), line);
  assert.deepEqual(segs[0], { text: 'let', tok: 'keyword', decos: ['match'] });
  assert.deepEqual(segs.find((s) => s.text === 'x'), { text: 'x', tok: null, decos: ['cursor'] });
  assert.deepEqual(segments('', [], [{ from: 0, to: 1, cls: 'cursor' }]), [{ text: ' ', tok: null, decos: ['cursor'] }]);
  const wide = segments('a😀b', [], [{ from: 1, to: 2, cls: 'cursor' }]);
  assert.deepEqual(wide.map((s) => [s.text, s.decos]), [['a', []], ['😀', ['cursor']], ['b', []]]);
});

test('the bracket under the cursor highlights its partner', () => {
  const L = toLines('f(a, [b])\n{\n}');
  assert.deepEqual(bracketPartner(L, { row: 0, col: 1 }), { row: 0, col: 8 });
  assert.deepEqual(bracketPartner(L, { row: 2, col: 0 }), { row: 1, col: 0 });
  assert.equal(bracketPartner(L, { row: 0, col: 2 }), null, 'only a bracket under the cursor');
});

test('viewer keys: column motions, search, folds and the preview', () => {
  assert.deepEqual(run(['w']), { kind: 'col', motion: 'w', n: 1 });
  assert.deepEqual(run(['3', 'l']), { kind: 'col', motion: 'l', n: 3 });
  assert.deepEqual(run(['0']), { kind: 'col', motion: '0', n: 1 }, 'a bare 0 is the motion, not a count');
  assert.deepEqual(run(['1', '0', 'j']), { kind: 'move', move: { by: 'line', n: 10 } });
  assert.deepEqual(run(['$']), { kind: 'col', motion: '$', n: 1 });
  assert.deepEqual(run(['/']), { kind: 'search', forward: true });
  assert.deepEqual(run(['?']), { kind: 'search', forward: false });
  assert.deepEqual(run(['2', 'n']), { kind: 'next-match', forward: true, n: 2 });
  assert.deepEqual(run(['N']), { kind: 'next-match', forward: false, n: 1 });
  assert.deepEqual(run(['*']), { kind: 'star', forward: true });
  assert.deepEqual(run(['#']), { kind: 'star', forward: false });
  for (const op of ['a', 'o', 'c', 'M', 'R']) assert.deepEqual(run(['z', op]), { kind: 'fold', op });
  assert.deepEqual(run(['z', 'x']), { kind: 'swallow' });
  assert.deepEqual(run([' ', 'm']), { kind: 'preview' });
  assert.deepEqual(run(['d']), { kind: 'edit', how: 'normal', replay: true }, 'an editing key starts the editor and runs there');
  assert.deepEqual(run([{ key: 's', ctrlKey: true }]), { kind: 'save' });
  assert.deepEqual(run([{ key: 'e', ctrlKey: true }]), { kind: 'move', move: { by: 'line', n: 1 } });
  assert.deepEqual(viewerCommand('set noautopair'), { kind: 'autopair', on: false });
});

test('fenced code blocks map their language and highlight line by line', async () => {
  const { fenceLanguage, highlightBlock } = await import('../dist/viewer-model.js');
  assert.equal(fenceLanguage('rs'), 'rust');
  assert.equal(fenceLanguage('Shell'), 'bash');
  assert.equal(fenceLanguage('c++'), 'cpp');
  assert.equal(fenceLanguage('brainfuck'), null);
  const rows = highlightBlock('python', 'x = """a\nb"""\ny = 1');
  assert.deepEqual(rows.map((r) => r.spans.map((s) => s.tok)), [['operator', 'string'], ['string'], ['operator', 'number']]);
});
