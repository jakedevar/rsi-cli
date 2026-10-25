// Rules for the Issue editor's and project form's text fields (hub #3421, desktop #317): form-text.ts is the pure
// side of the wrapping one-line field (form-field.ts); the real-key evidence is the vim-fields suite.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  ISSUE_TITLE_COUNTER_FROM, ISSUE_TITLE_LIMIT, charCount, counterText, fitPaste, flattenLines, settleEdit, takeChars,
  titleLimitHint, typingRefused,
} from '../dist/form-text.js';
import { SCENARIOS } from '../scripts/smoke-vim.mjs';

test('the title limit and counter match the TUI (200, counter from 150) and the hint says what was refused', () => {
  assert.equal(ISSUE_TITLE_LIMIT, 200);
  assert.equal(ISSUE_TITLE_COUNTER_FROM, 150);
  assert.equal(titleLimitHint(), 'TITLE LIMIT — titles are cut at 200 characters; input refused');
  assert.equal(counterText(149, 200, 150), '');
  assert.equal(counterText(150, 200, 150), '150/200');
  assert.equal(counterText(200, 200, 150), '200/200');
});

test('characters are code points, as the TUI counts them', () => {
  assert.equal(charCount('a😀b'), 3);
  assert.equal(takeChars('a😀b', 2), 'a😀');
  assert.equal(takeChars('abc', 0), '');
});

test('a one-line field never keeps a line break: every kind becomes one space', () => {
  assert.equal(flattenLines('a\nb\r\nc\rd'), 'a b c d');
});

test('typing is refused only when it grows the text past the limit', () => {
  assert.equal(typingRefused('x'.repeat(199), 0, 'a', 200), false);
  assert.equal(typingRefused('x'.repeat(200), 0, 'a', 200), true);
  assert.equal(typingRefused('x'.repeat(200), 1, 'a', 200), false, 'replacing a selected character keeps the length');
  assert.equal(typingRefused('x'.repeat(250), 1, '', 200), false, 'a longer older title can always be shortened');
  assert.equal(typingRefused('x'.repeat(250), 0, 'a', null), false, 'no limit, no refusal');
});

test('a paste is flattened for a one-line field and cut to what fits', () => {
  assert.deepEqual(fitPaste('a\nb', 'xy', 0, null, true), { text: 'a b', cut: false });
  assert.deepEqual(fitPaste('a\nb', 'xy', 0, null, false), { text: 'a\nb', cut: false });
  assert.deepEqual(fitPaste('abcdef', 'x'.repeat(197), 0, 200, true), { text: 'abc', cut: true });
  assert.deepEqual(fitPaste('abcdef', 'x'.repeat(200), 0, 200, true), { text: '', cut: true });
  assert.deepEqual(fitPaste('ab', 'x'.repeat(200), 2, 200, true), { text: 'ab', cut: false }, 'the selection makes room');
});

test('an edit that already happened is flattened, and one that passes the limit is undone whole', () => {
  assert.deepEqual(settleEdit('ab', 'a\nb', true, null), { text: 'a b', refused: false });
  assert.deepEqual(settleEdit('ab', 'a\nb', false, null), { text: 'a\nb', refused: false });
  const full = 'x'.repeat(200);
  assert.deepEqual(settleEdit(full, `${full}y`, true, 200), { text: full, refused: true });
  assert.deepEqual(settleEdit(full, full.slice(1), true, 200), { text: full.slice(1), refused: false });
  const old = 'x'.repeat(250);
  assert.deepEqual(settleEdit(old, old.slice(1), true, 200), { text: old.slice(1), refused: false }, 'shortening an older, longer title is allowed');
  assert.equal(settleEdit(old, `${old}y`, true, 200).refused, true);
});

test('the vim-fields suite covers every Issue editor field, the project form, the Esc contract and the title cap', () => {
  const ids = SCENARIOS.map((s) => s.id);
  for (const want of ['issue-editor-fields', 'issue-form-esc-contract', 'title-limit', 'project-form-fields', 'issue-filter-dependency-fields', 'form-fields-vim-off']) {
    assert.ok(ids.includes(want), `${want} is a scenario`);
  }
});
