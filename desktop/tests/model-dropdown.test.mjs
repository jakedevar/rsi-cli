// The model dropdown widget (gap sweep G4, hub #3000): the TUI's filter, keys
// and provider cycle (docs/keybindings.md "Model Dropdown (Widget)", "Model Picker").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  NEW_DROPDOWN, PROVIDER_ORDER, cycleProvider, dropWord, dropdownKey, filterModels, orderProviders, queryLine,
} from '../dist/model-dropdown-model.js';

const M = [
  { id: 'claude-opus-4-5', label: 'Claude Opus 4.5' },
  { id: 'claude-sonnet-5', label: 'Claude Sonnet 5' },
  { id: 'claude-haiku-4-5', label: 'claude-haiku-4-5' },
];

test('every term must match the name or id, ignoring case; punctuation is a fallback', () => {
  assert.deepEqual(filterModels(M, 'opus').map((h) => h.entry.id), ['claude-opus-4-5']);
  assert.deepEqual(filterModels(M, 'CLAUDE 5').map((h) => h.entry.id), ['claude-opus-4-5', 'claude-sonnet-5', 'claude-haiku-4-5'], 'catalog order kept');
  assert.deepEqual(filterModels(M, 'opus45').map((h) => h.entry.id), ['claude-opus-4-5'], 'opus45 finds claude-opus-4-5');
  assert.deepEqual(filterModels(M, 'haiku sonnet'), []);
  assert.equal(filterModels(M, '').length, 3);
  assert.deepEqual(filterModels(M, 'sonnet')[0].ranges, [[7, 13]], 'matched text is highlighted');
});

test('normal-mode keys move, jump, pick 1-9, cycle providers and cancel', () => {
  const k = (key, st = NEW_DROPDOWN, n = 3, more = {}) => dropdownKey(st, { key, ...more }, n);
  assert.equal(k('j').st.sel, 1);
  assert.equal(k('ArrowDown', { ...NEW_DROPDOWN, sel: 2 }).st.sel, 2, 'clamped at the last row');
  assert.equal(k('G').st.sel, 2);
  assert.equal(k('g', { ...NEW_DROPDOWN, sel: 2 }).st.sel, 0);
  assert.equal(k('End').st.sel, 2);
  assert.deepEqual(k('2').effect, { kind: 'pick', index: 1 });
  assert.deepEqual(k('9').effect, { kind: 'none' }, 'no ninth row');
  assert.deepEqual(k('Enter', { ...NEW_DROPDOWN, sel: 1 }).effect, { kind: 'pick', index: 1 });
  assert.deepEqual(k('Enter', NEW_DROPDOWN, 0).effect, { kind: 'none' }, 'Enter does nothing with no match');
  assert.deepEqual(k('Tab').effect, { kind: 'provider', by: 1 });
  assert.deepEqual(k('Tab', NEW_DROPDOWN, 3, { shiftKey: true }).effect, { kind: 'provider', by: -1 });
  assert.deepEqual(k('Escape').effect, { kind: 'close' });
  assert.deepEqual(k('q').effect, { kind: 'close' });
  assert.equal(k('/').st.typing, true);
});

test('typing a filter takes every printable key, Backspace on empty stops, Esc keeps the query', () => {
  let st = { ...NEW_DROPDOWN, typing: true };
  for (const c of ['j', 'k', 'q', '1', ' ']) st = dropdownKey(st, { key: c }, 3).st;
  assert.equal(st.query, 'jkq1 ');
  assert.equal(st.typing, true);
  st = dropdownKey(st, { key: 'w', ctrlKey: true }, 3).st;
  assert.equal(st.query, '');
  st = dropdownKey({ ...st, query: 'opus' }, { key: 'Backspace' }, 3).st;
  assert.equal(st.query, 'opu');
  st = dropdownKey(st, { key: 'u', ctrlKey: true }, 3).st;
  assert.equal(st.query, '');
  assert.equal(dropdownKey(st, { key: 'Backspace' }, 3).st.typing, false, 'Backspace on an empty query stops filtering');
  const esc = dropdownKey({ ...NEW_DROPDOWN, typing: true, query: 'op' }, { key: 'Escape' }, 1);
  assert.deepEqual([esc.st.typing, esc.st.query, esc.effect.kind], [false, 'op', 'none'], 'the first Esc keeps the filter');
  assert.deepEqual(dropdownKey(esc.st, { key: 'Escape' }, 1).effect, { kind: 'close' }, 'a second Esc closes');
  assert.deepEqual(dropdownKey({ ...NEW_DROPDOWN, typing: true }, { key: 'Enter' }, 2).effect, { kind: 'pick', index: 0 }, 'Enter works while typing');
  assert.equal(dropdownKey({ ...NEW_DROPDOWN, typing: true, sel: 0 }, { key: 'n', ctrlKey: true }, 2).st.sel, 1);
  assert.deepEqual(dropdownKey({ ...NEW_DROPDOWN, typing: true }, { key: 'Tab' }, 2).effect, { kind: 'provider', by: 1 }, 'Tab runs the query on the next provider');
});

test('a fixed catalog has no provider cycle and m closes it', () => {
  assert.deepEqual(dropdownKey(NEW_DROPDOWN, { key: 'Tab' }, 2, true).effect, { kind: 'none' });
  assert.deepEqual(dropdownKey(NEW_DROPDOWN, { key: 'm' }, 2, true).effect, { kind: 'close' });
  assert.deepEqual(dropdownKey(NEW_DROPDOWN, { key: 'm' }, 2, false).effect, { kind: 'none' });
});

test('providers follow the TUI cycle order and wrap', () => {
  assert.deepEqual(orderProviders(['Local', 'Zed', 'Claude', 'Codex', 'Harness']), ['Claude', 'Codex', 'Local', 'Harness', 'Zed']);
  assert.equal(PROVIDER_ORDER[0], 'Claude');
  assert.equal(cycleProvider(['Claude', 'Codex'], 'Codex', 1), 'Claude');
  assert.equal(cycleProvider(['Claude', 'Codex'], 'Claude', -1), 'Codex');
  assert.equal(cycleProvider(['Claude'], 'Gone', 1), 'Claude');
});

test('the query row shows the query and the match count', () => {
  assert.equal(queryLine(NEW_DROPDOWN, 3, 3), '3 models · / filters');
  assert.equal(queryLine({ ...NEW_DROPDOWN, typing: true, query: 'op' }, 1, 3), '/op▏  1 of 3');
  assert.equal(dropWord('claude opus'), 'claude');
});
