// The fold keys shared by slice 4 (list) and slice 5 (transcript): one spec
// per id, and the rule of the pane that has the keys (actions/folds.ts).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ACTIONS, appMenuModel, keyHints, unavailable } from '../dist/registry.js';
import { FOLD_ACTIONS, foldPane } from '../dist/actions/folds.js';
import { focusAfter } from '../dist/detail-model.js';

const list = (over = {}) => ({
  cursorFoldable: false, cursorHasParent: false, foldable: 0, paneDetail: false, attentionQueue: 0,
  search: '', detailSearchable: false, projects: 0, labelGroups: false, openHasParent: false, ...over,
});
const detail = (over = {}) => ({
  events: 4, shown: 4, userMessages: 1, cursor: false, focus: 'list', hideSystem: false, hideThinking: false, ...over,
});
const snap = (l = {}, d = {}) => ({
  open: { status: 'Completed', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null, list: list(l), detail: detail(d),
});

test('each fold action is registered once, in the View menu, with its TUI key', () => {
  const keys = { 'fold-open': 'zo', 'fold-close': 'zc', 'fold-toggle': 'za', 'fold-close-all': 'zM', 'fold-open-all': 'zR' };
  for (const a of FOLD_ACTIONS) {
    assert.equal(ACTIONS.filter((x) => x.id === a.id).length, 1, a.id);
    assert.deepEqual(keyHints(a.id), [keys[a.id]], a.id);
  }
  const view = appMenuModel('other', snap({ cursorFoldable: true, foldable: 1 })).find((m) => m.id === 'view').items;
  for (const id of Object.keys(keys)) assert.ok(view.some((n) => n.id === `act:${id}`), id);
});

test('the fold keys act on the list until the keys move to the transcript', () => {
  assert.equal(foldPane(snap()), 'list');
  assert.equal(foldPane(snap({}, { focus: 'transcript' })), 'transcript');
  assert.match(unavailable('fold-open', snap()), /not on a session with children/);
  assert.equal(unavailable('fold-open', snap({ cursorFoldable: true })), null);
  assert.match(unavailable('fold-close-all', snap()), /No session has children/);
  assert.equal(unavailable('fold-close-all', snap({ foldable: 2 })), null);
  // In the transcript the list's folds do not matter; the selected event does.
  assert.match(unavailable('fold-toggle', snap({ cursorFoldable: true }, { focus: 'transcript' })), /Select an event first/);
  assert.equal(unavailable('fold-toggle', snap({}, { focus: 'transcript', cursor: true })), null);
  assert.equal(unavailable('fold-open-all', snap({}, { focus: 'transcript' })), null);
});

test('a fold key keeps the keys in the pane it acted on', () => {
  for (const id of ['fold-open', 'fold-close', 'fold-toggle', 'fold-close-all', 'fold-open-all']) {
    assert.equal(focusAfter(id, 'list'), 'list', id);
    assert.equal(focusAfter(id, 'transcript'), 'transcript', id);
  }
});
