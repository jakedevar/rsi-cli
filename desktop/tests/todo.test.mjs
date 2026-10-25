// The operator to-do panel (hub #3015 slice 13B): the <Space>d chords and
// :todo, the TodoPanel port (order, cursor, catch-up, undo window), links,
// labels, the pop-up keys and the Tauri wiring (request shapes are pinned by
// the Rust tests in src-tauri/src/todo.rs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, appMenuModel, menuItems, keyHints, unavailable } from '../dist/registry.js';
import { TODO_ACTIONS, TODO_KEY_BINDINGS, TODO_RULES } from '../dist/actions/todo.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  NO_ITEM, NOTHING_TO_UNDO, TodoPanel, ageLabel, badgeText, closeTarget, closeToast, copiedMessage, linkLabel,
  parseLink, parseTodoItem, parseTodoList, popupKey, priorityLabel, sourceLabel, timeMs,
} from '../dist/todo-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const T0 = Date.parse('2026-10-18T10:00:00Z');
const at = (min) => new Date(T0 + min * 60_000).toISOString().replace('.000Z', '.123456789Z');
const snap = (over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, ...over,
});

let n = 0;
function item(title, priority, createdMin, over = {}) {
  n += 1;
  return {
    id: `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`, scope: 'project:p', project_id: null, title,
    how: `run ${title}`, link: null, priority, status: 'open', closed_by: null, created_at: at(createdMin), closed_at: null, ...over,
  };
}
const list = (open, closed = []) => ({ open, recently_closed: closed, truncated: false });

test('the chords and :todo run the to-do actions', () => {
  const ids = TODO_ACTIONS.map((a) => a.id);
  for (const id of ids) { assert.ok(ACTIONS.some((a) => a.id === id), id); assert.equal(typeof TODO_RULES[id], 'function'); }
  for (const [keys, action] of [
    ['t', 'todo-toggle'], ['j', 'todo-next'], ['k', 'todo-prev'], ['o', 'todo-open-link'], ['d', 'todo-done'],
    ['x', 'todo-dismiss'], ['y', 'todo-copy'], ['c', 'todo-copy'], ['v', 'todo-view'], ['u', 'todo-undo'], ['a', 'todo-ack'],
  ]) {
    const e = new KeyEngine(desktopKeymap());
    e.feed('Space');
    assert.equal(e.feed('d').kind, 'pending');
    assert.deepEqual(e.feed(keys), { kind: 'run', action, count: null, keys: `<Space>d${keys}` });
  }
  assert.equal(Object.keys(TODO_KEY_BINDINGS).length, 11);
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('todo').action, 'todo-toggle');
  assert.deepEqual(keyHints('todo-copy'), ['<Space>dy', '<Space>dc']);
  const view = appMenuModel('other', snap({ todo: { open: true, selected: false, undo: false } })).find((m) => m.id === 'view');
  const toggle = menuItems(view.items).find((i) => i.id === 'act:todo-toggle');
  assert.equal(toggle.checked, true);
});

test('item actions need a selection and undo needs a live close', () => {
  assert.equal(unavailable('todo-toggle', snap()), null);
  assert.equal(unavailable('todo-next', snap()), null);
  for (const id of ['todo-open-link', 'todo-done', 'todo-dismiss', 'todo-copy', 'todo-view']) {
    assert.equal(unavailable(id, snap()), NO_ITEM, id);
    assert.equal(unavailable(id, snap({ todo: { open: true, selected: true, undo: false } })), null, id);
  }
  assert.equal(unavailable('todo-undo', snap({ todo: { open: true, selected: true, undo: false } })), NOTHING_TO_UNDO);
  assert.equal(unavailable('todo-undo', snap({ todo: { open: false, selected: false, undo: true } })), null);
});

test('open items are most urgent then newest first; the cursor follows its item', () => {
  const p = new TodoPanel(true, null);
  const low = item('low', 4, 1); const urgentOld = item('urgent-old', 1, 1); const urgentNew = item('urgent-new', 1, 5);
  p.apply(list([low, urgentOld, urgentNew]));
  assert.deepEqual(p.items().map((i) => i.title), ['urgent-new', 'urgent-old', 'low']);
  assert.equal(p.selected().title, 'urgent-new');
  p.selectNext(); p.selectNext(); p.selectNext();
  assert.equal(p.selected().title, 'low', 'clamped at the end');
  p.apply(list([low, urgentNew]));
  assert.equal(p.selected().title, 'low', 'the selection follows its item across a refresh');
  p.apply(list([urgentNew]));
  assert.equal(p.selected().title, 'urgent-new', 'falls back to the first row');
  p.apply(list([]));
  assert.equal(p.selected(), undefined);
  p.selectPrev();
  assert.equal(p.selectedIndex(), null);
});

test('catch-up: new since the last visit, agent closes are news, the operator’s are not', () => {
  const first = new TodoPanel(false, null);
  first.apply(list([item('a', 3, 10)], [item('c', 3, 1, { status: 'done', closed_by: 'session:x', closed_at: at(20) })]));
  assert.equal(first.newCount(), 0, 'a first run has nothing new');
  assert.equal(first.closedSince(), null);
  assert.deepEqual(first.closedSinceVisit(), []);

  const p = new TodoPanel(true, T0 + 5 * 60_000);
  assert.equal(p.closedSince(), new Date(T0 + 5 * 60_000).toISOString());
  p.apply(list(
    [item('old', 3, 1), item('fresh', 3, 9)],
    [item('agent', 3, 1, { status: 'done', closed_by: 'session:x', closed_at: at(7) }),
      item('mine', 3, 1, { status: 'dismissed', closed_by: 'operator', closed_at: at(8) })],
  ));
  assert.equal(p.newCount(), 1);
  assert.deepEqual(p.closedSinceVisit().map((i) => i.title), ['agent']);
  assert.ok(p.hasCatchUp());
  assert.equal(badgeText(p), '☐ 2 todo +1 ✓1');
  p.touchVisit(T0 + 60 * 60_000);
  assert.equal(p.newCount(), 1, 'touching the visit does not move this run’s baseline');
  p.acknowledge(T0 + 60 * 60_000);
  assert.equal(p.newCount(), 0);
  assert.deepEqual(p.closedSinceVisit(), []);
  assert.equal(p.lastVisitMs, T0 + 60 * 60_000);
  assert.equal(badgeText(p), '☐ 2 todo');
  assert.equal(badgeText(new TodoPanel()), '');
});

test('a close removes the item at once, moves the cursor and is undoable for 60 s', () => {
  const p = new TodoPanel(true, null);
  const a = item('a', 2, 3); const b = item('b', 2, 2); const c = item('c', 2, 1);
  p.apply(list([a, b, c]));
  p.selectNext();
  assert.deepEqual(closeTarget(p, 'done'), { id: b.id, status: 'done' });
  p.recordClose(p.selected(), 'done', 1_000);
  p.removeOpen(b.id);
  assert.equal(p.selected().title, 'c', 'the cursor moves to the neighbour');
  assert.equal(p.undoAvailable(1_000 + 59_000).title, 'b');
  assert.equal(p.undoAvailable(1_000 + 60_000), null);
  assert.equal(p.expireUndo(1_000 + 59_000), false);
  assert.equal(p.takeUndo(1_000 + 10_000).id, b.id);
  assert.equal(p.takeUndo(1_000 + 10_000), null, 'taking it leaves none');
  p.recordClose(a, 'dismissed', 2_000);
  assert.equal(p.expireUndo(2_000 + 61_000), true);
  assert.equal(p.takeUndo(2_000 + 1_000), null);
  p.restoreOpen(b);
  assert.equal(p.selected().id, b.id, 'a restored item returns and is selected');
  assert.deepEqual(p.items().map((i) => i.title), ['a', 'b', 'c']);
  assert.equal(closeTarget(new TodoPanel(), 'done'), null);
  assert.equal(closeToast('done'), 'To-do marked done · <Space>du undoes it for 60 s');
  assert.equal(closeToast('dismissed'), 'To-do dismissed · <Space>du undoes it for 60 s');
});

test('links parse by kind; unknown text is shown, never opened', () => {
  assert.deepEqual(parseLink('issue:#12'), { kind: 'issue', number: 12 });
  assert.deepEqual(parseLink('issue:7'), { kind: 'issue', number: 7 });
  assert.deepEqual(parseLink('session:E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE'), { kind: 'session', id: 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae' });
  assert.deepEqual(parseLink(' https://example.com/x '), { kind: 'url', url: 'https://example.com/x' });
  assert.equal(parseLink('issue:#abc'), null);
  assert.equal(parseLink('session:nope'), null);
  assert.equal(parseLink('ftp://x'), null);
  assert.equal(linkLabel('session:e2cae1d4-1833-48eb-a7e8-6f1dd480fdae'), 'session e2cae1d4');
  assert.equal(linkLabel('issue:#3'), 'issue #3');
  assert.equal(linkLabel('see the docs'), 'see the docs');
});

test('labels: priority words, ages, sources and the copy toast', () => {
  assert.deepEqual([1, 2, 3, 4].map(priorityLabel), ['urgent', 'high', 'normal', 'low']);
  assert.equal(ageLabel(T0 + 30_000, '2026-10-18T10:00:00Z'), 'now');
  assert.equal(ageLabel(T0 + 3 * 60_000, '2026-10-18T10:00:00Z'), '3m');
  assert.equal(ageLabel(T0 + 2 * 3_600_000, '2026-10-18T10:00:00Z'), '2h');
  assert.equal(ageLabel(T0 + 5 * 86_400_000, '2026-10-18T10:00:00Z'), '5d');
  assert.equal(sourceLabel(item('x', 3, 0, { scope: 'operator' }), 'rsi'), 'you');
  assert.equal(sourceLabel(item('x', 3, 0, { scope: 'project:abc' }), 'rsi'), 'rsi');
  assert.equal(sourceLabel(item('x', 3, 0, { scope: 'portfolio:global' }), undefined), 'portfolio');
  assert.equal(sourceLabel(item('x', 3, 0, { scope: 'area:ops' }), undefined), 'area');
  assert.equal(copiedMessage('make test'), 'Copied to-do command: make test');
  assert.equal(copiedMessage('x'.repeat(41)), `Copied to-do command: ${'x'.repeat(40)}…`);
  assert.equal(timeMs('2026-10-18T10:00:00.123456789Z'), T0 + 123);
});

test('daemon replies parse; a bad row is dropped', () => {
  const l = parseTodoList({ open: [item('a', 2, 0), { id: 'x' }], recently_closed: 'no', truncated: true });
  assert.equal(l.open.length, 1);
  assert.deepEqual(l.recently_closed, []);
  assert.equal(l.truncated, true);
  assert.equal(parseTodoList({}), null);
  assert.equal(parseTodoItem({ todo: item('b', 3, 0) }).title, 'b');
  assert.equal(parseTodoItem(item('c', 3, 0)).title, 'c');
  assert.equal(parseTodoItem({ todo: null }), null);
});

test('the pop-up keys mirror the TUI, and a close always asks y/n first', () => {
  const st = { scroll: 0, confirm: null };
  assert.equal(popupKey(st, { key: 'y' }).effect, 'copy');
  assert.deepEqual(popupKey(st, { key: 'd' }), { st: { scroll: 0, confirm: 'done' }, effect: 'none' });
  assert.deepEqual(popupKey(st, { key: 'x' }).st.confirm, 'dismissed');
  assert.equal(popupKey(st, { key: 'u' }).effect, 'undo');
  assert.equal(popupKey(st, { key: 'o' }).effect, 'open-link');
  assert.equal(popupKey(st, { key: 'n' }).effect, 'next');
  assert.equal(popupKey(st, { key: 'p' }).effect, 'prev');
  assert.equal(popupKey(st, { key: 'j' }).st.scroll, 1);
  assert.equal(popupKey({ scroll: 3, confirm: null }, { key: 'k' }).st.scroll, 2);
  assert.equal(popupKey(st, { key: 'k' }).st.scroll, 0, 'never above the top');
  assert.equal(popupKey(st, { key: 'd', ctrl: true }).st.scroll, 10);
  assert.equal(popupKey({ scroll: 12, confirm: null }, { key: 'u', ctrl: true }).st.scroll, 2);
  assert.equal(popupKey({ scroll: 12, confirm: null }, { key: 'g' }).st.scroll, 0);
  assert.equal(popupKey(st, { key: 'q' }).effect, 'close');
  assert.equal(popupKey(st, { key: 'Escape' }).effect, 'close');
  const asking = { scroll: 0, confirm: 'dismissed' };
  assert.equal(popupKey(asking, { key: 'y' }).effect, 'confirm-close');
  assert.equal(popupKey(asking, { key: 'Enter' }).effect, 'confirm-close');
  assert.deepEqual(popupKey(asking, { key: 'n' }), { st: { scroll: 0, confirm: null }, effect: 'none' });
  assert.equal(popupKey(asking, { key: 'Escape' }).st.confirm, null);
  assert.equal(popupKey(asking, { key: 'x' }).effect, 'none', 'other keys wait at the prompt');
});

test('the Tauri commands forward the operator-only methods and are in the handler list', () => {
  const rs = readFileSync(here('../src-tauri/src/todo.rs'), 'utf8');
  for (const [cmd, method] of [
    ['operator_todo_list', 'OperatorTodoList'], ['operator_todo_close', 'OperatorTodoClose'], ['operator_todo_reopen', 'OperatorTodoReopen'],
  ]) {
    const i = rs.indexOf(`pub async fn ${cmd}(`);
    assert.ok(i > 0, cmd);
    assert.ok(rs.indexOf(`"${method}"`, i) > i, `${cmd} forwards ${method}`);
  }
  const handlers = readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8');
  for (const cmd of ['operator_todo_list', 'operator_todo_close', 'operator_todo_reopen']) assert.match(handlers, new RegExp(`todo::${cmd}\\b`));
  assert.match(readFileSync(here('../src-tauri/src/modules.generated.rs'), 'utf8'), /^pub mod todo;$/m);
  const ts = readFileSync(here('../src/todo.ts'), 'utf8');
  assert.match(ts, /invoke<unknown>\('operator_todo_list', \{ closedSince: panel\.closedSince\(\) \}\)/);
  assert.match(ts, /invoke<unknown>\('operator_todo_close', \{ todoId: t\.id, status: t\.status \}\)/);
  assert.match(ts, /invoke<unknown>\('operator_todo_reopen', \{ todoId: entry\.id \}\)/);
  // A close never goes out without the pop-up's y/n: only the confirm path calls closeSelected.
  assert.equal(ts.split('closeSelected(').length - 1, 2, 'one definition, one call (confirm-close)');
});
