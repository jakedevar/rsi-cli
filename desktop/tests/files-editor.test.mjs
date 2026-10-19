// The editor's additions to the viewer (#134, hub #3012): auto-pairing, paste, the git
// signs over unsaved edits, folds and the visual selection, in the TUI's terms.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { autoPairEdit, pastedText, selectionRows, shiftSigns, skipHidden } from '../dist/files-editor-model.js';
import { commonEnds } from '../dist/files-edit-model.js';
import { toLines } from '../dist/vim-text.js';
import { viewerCommand } from '../dist/files-model.js';
import { Folds, bracketPartner, foldRegions, highlightLine, languageOf, lexStates, findMatches, segments } from '../dist/viewer-model.js';

test('auto-pair types the pair with the caret between, and not when the closer is already next', () => {
  assert.deepEqual(autoPairEdit('', 0, 0, '('), { from: 0, to: 0, insert: '()', caret: 1 });
  assert.deepEqual(autoPairEdit('ab', 1, 1, '{'), { from: 1, to: 1, insert: '{}', caret: 2 });
  assert.deepEqual(autoPairEdit('x', 1, 1, '['), { from: 1, to: 1, insert: '[]', caret: 2 });
  assert.equal(autoPairEdit('f()', 2, 2, '('), null, 'a ")" is next: type the "(" as is');
  assert.equal(autoPairEdit('ab', 0, 0, 'a'), null, 'other keys are the text box\'s');
  assert.equal(autoPairEdit('"', 0, 0, '"'), null, 'brackets only, as the TUI does');
  assert.equal(autoPairEdit('ab', 0, 2, '('), null, 'a selection is replaced natively');
});

test('auto-pair: a closer types over the same closer; Backspace between an empty pair removes both', () => {
  assert.deepEqual(autoPairEdit('()', 1, 1, ')'), { from: 1, to: 1, insert: '', caret: 2 });
  assert.equal(autoPairEdit('(x', 2, 2, ')'), null);
  assert.equal(autoPairEdit('(]', 1, 1, ')'), null);
  assert.deepEqual(autoPairEdit('a()b', 2, 2, 'Backspace'), { from: 1, to: 3, insert: '', caret: 1 });
  assert.deepEqual(autoPairEdit('{}', 1, 1, 'Backspace'), { from: 0, to: 2, insert: '', caret: 0 });
  assert.equal(autoPairEdit('(x)', 2, 2, 'Backspace'), null, 'not an empty pair');
  assert.equal(autoPairEdit('(]', 1, 1, 'Backspace'), null, 'not a matching pair');
  assert.equal(autoPairEdit('(', 0, 0, 'Backspace'), null);
  // A multibyte character before the pair does not shift it.
  assert.deepEqual(autoPairEdit('é()', 2, 2, 'Backspace'), { from: 1, to: 3, insert: '', caret: 1 });
});

test(':set autopair and :set noautopair are commands of the editor', () => {
  assert.deepEqual(viewerCommand('set autopair'), { kind: 'autopair', on: true });
  assert.deepEqual(viewerCommand(':set noautopair'), { kind: 'autopair', on: false });
});

test('a paste keeps LF breaks, as a text box holds them', () => {
  assert.equal(pastedText('a\r\nb\rc\nd'), 'a\nb\nc\nd');
});

test('the git signs follow the lines through unsaved edits', () => {
  const base = ['a', 'b', 'c', 'd', 'e'];
  const signs = [null, 'added', null, 'modified', 'removed'];
  assert.deepEqual(shiftSigns(signs, base, base), signs, 'untouched text keeps every sign');
  // A line inserted in the middle: the signs below move with their lines, the new line has none.
  assert.deepEqual(shiftSigns(signs, base, ['a', 'b', 'NEW', 'c', 'd', 'e']), [null, 'added', null, null, 'modified', 'removed']);
  // A line deleted at the top.
  assert.deepEqual(shiftSigns(signs, base, ['b', 'c', 'd', 'e']), ['added', null, 'modified', 'removed']);
  // An edited line loses its sign until the next save.
  assert.deepEqual(shiftSigns(signs, base, ['a', 'B!', 'c', 'd', 'e']), [null, null, null, 'modified', 'removed']);
  assert.deepEqual(commonEnds(['x'], ['x']), { prefix: 1, suffix: 0 });
});

test('a cursor on a hidden line steps to a shown one in the direction it moved', () => {
  const shown = [0, 1, 5, 6];
  assert.equal(skipHidden(shown, 3, 1), 5, 'down: the first shown line after the fold');
  assert.equal(skipHidden(shown, 3, 5), 1, 'up: the fold\'s own line');
  assert.equal(skipHidden(shown, 5, 1), 5);
  assert.equal(skipHidden([], 3, 1), 3);
  assert.equal(skipHidden([2, 3], 1, 4), 2, 'above everything shown');
});

test('the visual selection becomes per-line stretches (a stretch past the end covers the break)', () => {
  const text = 'abc\n\nxyz';
  const lines = toLines(text);
  assert.deepEqual(selectionRows(lines, text, 1, 2), [{ row: 0, from: 1, to: 2 }]);
  assert.deepEqual(selectionRows(lines, text, 1, 7), [
    { row: 0, from: 1, to: 4 }, { row: 1, from: 0, to: 1 }, { row: 2, from: 0, to: 2 },
  ]);
  assert.deepEqual(selectionRows(lines, text, 3, 3), []);
});

test('the editor draws the buffer with the viewer\'s own colours, search hits, bracket match and folds', () => {
  const src = 'fn main() {\n    let x = (1);\n}\n';
  const lines = src.split('\n');
  const lang = languageOf('src/main.rs');
  assert.equal(lang, 'rust');
  const states = lexStates(lang, lines);
  assert.ok(highlightLine(lang, lines[0], states[0]).spans.some((s) => s.tok === 'keyword'), 'fn is a keyword');
  // The text box's text typed into the buffer is highlighted from the first keystroke on.
  const typed = ['pub fn a() {', '}', ''];
  assert.ok(highlightLine(lang, typed[0], lexStates(lang, typed)[0]).spans.some((s) => s.tok === 'keyword'));
  assert.deepEqual(foldRegions(lang, lines), [[0, 2]]);
  const folds = new Folds();
  folds.setRegions(foldRegions(lang, lines));
  folds.set(0, true);
  assert.deepEqual(folds.display(lines.length), [0, 3], 'the interior of the closed fold is hidden');
  assert.equal(folds.summaryAt(0), 2);
  assert.deepEqual(bracketPartner(toLines(src), { row: 0, col: 7 }), { row: 0, col: 8 });
  assert.equal(findMatches(lines, 'let').length, 1);
  const segs = segments('ab', [], [{ from: 1, to: 2, cls: 'caret' }, { from: 0, to: 1, cls: 'select' }]);
  assert.deepEqual(segs.map((s) => [s.text, s.decos]), [['a', ['select']], ['b', ['caret']]]);
  // The caret at the end of a line is a blank cell.
  assert.deepEqual(segments('ab', [], [{ from: 2, to: 3, cls: 'caret' }]).at(-1), { text: ' ', tok: null, decos: ['caret'] });
});
