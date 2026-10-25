// With Vim keys off the session list works from the keyboard (local #226, from the #221 audit):
// the list is one tab stop (an ARIA tree, aria-activedescendant on the cursor row); its rows carry
// role treeitem, aria-selected, aria-level, aria-expanded and a name + plain status label; Up/Down,
// Home/End and PageUp/PageDown move the cursor, Left/Right fold a container (or step to its parent
// / first child), Enter opens. The headless smoke (scripts/smoke.mjs `session-list-keyboard`) drives
// the same keys in a real page with Vim keys off.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { isListKey, listKey, rowAccessibleName, rowDomId } from '../dist/sessionlist-keys-model.js';
import { buildTree, flattenTree } from '../dist/tree.js';
import { El } from './fake-dom.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

const base = { provider: 'Claude', query: 'q', working_dir: '/w', created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z' };
const s = (id, status, extra = {}) => ({ ...base, id, status, title: `t-${id}`, ...extra });
const F = { projectId: null, text: '', showFinished: true };
const lead = s('lead', 'Running', { session_kind: 'Epic' });
const w1 = s('w1', 'WaitingApproval', { parent_id: 'lead' });
const w2 = s('w2', 'Running', { parent_id: 'lead' });
const solo = s('solo', 'Completed');
const rowsOf = (collapsed = new Set()) => flattenTree(buildTree([lead, w1, w2, solo], F), collapsed);
const key = (k, mods = {}) => ({ key: k, ...mods });

test('the list rows are the tree in order (fixture check)', () => {
  assert.deepEqual(rowsOf().map((r) => r.session.id), ['lead', 'w1', 'w2', 'solo']);
});

test('Up/Down step the cursor and stop at the ends; with no cursor Down is the first row', () => {
  const rows = rowsOf();
  assert.deepEqual(listKey(key('ArrowDown'), rows, null, null, 10), { kind: 'move', id: 'lead' });
  assert.deepEqual(listKey(key('ArrowUp'), rows, null, null, 10), { kind: 'move', id: 'solo' });
  assert.deepEqual(listKey(key('ArrowDown'), rows, 'lead', null, 10), { kind: 'move', id: 'w1' });
  assert.deepEqual(listKey(key('ArrowUp'), rows, 'w2', null, 10), { kind: 'move', id: 'w1' });
  assert.deepEqual(listKey(key('ArrowDown'), rows, 'solo', null, 10), { kind: 'none' });
  assert.deepEqual(listKey(key('ArrowUp'), rows, 'lead', null, 10), { kind: 'none' });
  // The open session is the cursor when there is no separate cursor.
  assert.deepEqual(listKey(key('ArrowDown'), rows, null, 'w1', 10), { kind: 'move', id: 'w2' });
});

test('Home/End go to the first/last row; PageUp/PageDown move a page, clamped', () => {
  const rows = rowsOf();
  assert.deepEqual(listKey(key('Home'), rows, 'w2', null, 10), { kind: 'move', id: 'lead' });
  assert.deepEqual(listKey(key('End'), rows, 'lead', null, 10), { kind: 'move', id: 'solo' });
  assert.deepEqual(listKey(key('PageDown'), rows, 'lead', null, 2), { kind: 'move', id: 'w2' });
  assert.deepEqual(listKey(key('PageDown'), rows, 'w2', null, 2), { kind: 'move', id: 'solo' });
  assert.deepEqual(listKey(key('PageUp'), rows, 'solo', null, 2), { kind: 'move', id: 'w1' });
  assert.deepEqual(listKey(key('PageUp'), rows, 'w1', null, 10), { kind: 'move', id: 'lead' });
});

test('Enter opens the row under the cursor; ArrowDown then Enter from the first row opens the second', () => {
  const rows = rowsOf();
  const moved = listKey(key('ArrowDown'), rows, 'lead', null, 10);
  assert.deepEqual(moved, { kind: 'move', id: 'w1' });
  assert.deepEqual(listKey(key('Enter'), rows, moved.id, null, 10), { kind: 'open', id: 'w1' });
});

test('Left folds an open container, Right unfolds a folded one, then steps to its first child', () => {
  assert.deepEqual(listKey(key('ArrowLeft'), rowsOf(), 'lead', null, 10), { kind: 'fold', id: 'lead', collapse: true });
  const folded = rowsOf(new Set(['lead']));
  assert.deepEqual(folded.map((r) => r.session.id), ['lead', 'solo']);
  assert.deepEqual(listKey(key('ArrowRight'), folded, 'lead', null, 10), { kind: 'fold', id: 'lead', collapse: false });
  assert.deepEqual(listKey(key('ArrowRight'), rowsOf(), 'lead', null, 10), { kind: 'move', id: 'w1' });
});

test('Left on a child steps to its parent; on a top-level leaf or a folded container it does nothing', () => {
  assert.deepEqual(listKey(key('ArrowLeft'), rowsOf(), 'w2', null, 10), { kind: 'move', id: 'lead' });
  assert.deepEqual(listKey(key('ArrowLeft'), rowsOf(), 'solo', null, 10), { kind: 'none' });
  assert.deepEqual(listKey(key('ArrowLeft'), rowsOf(new Set(['lead'])), 'lead', null, 10), { kind: 'none' });
  assert.deepEqual(listKey(key('ArrowRight'), rowsOf(), 'w1', null, 10), { kind: 'none' });
});

test('modified keys and letters are not list keys (Shift-Left still opens, j/k stay with Vim keys)', () => {
  assert.equal(isListKey(key('ArrowDown')), true);
  assert.equal(isListKey(key('Enter')), true);
  assert.equal(isListKey(key('ArrowLeft', { shiftKey: true })), false);
  assert.equal(isListKey(key('ArrowDown', { ctrlKey: true })), false);
  assert.equal(isListKey(key('j')), false);
  assert.deepEqual(listKey(key('j'), rowsOf(), 'lead', null, 10), { kind: 'pass' });
  assert.deepEqual(listKey(key('ArrowDown'), [], null, null, 10), { kind: 'none' });
});

test('a row reads its name and plain status, then why it needs you', () => {
  assert.equal(rowAccessibleName('Fix login', 'Running', null), 'Fix login, Running');
  assert.equal(rowAccessibleName('Fix login', 'WaitingApproval', 'Needs you'), 'Fix login, Waiting for you, Needs you');
  assert.equal(rowDomId('abc'), 'session-row-abc');
});

// ---------- the rendered rows (fake DOM) ----------

async function render(rows, ctx) {
  globalThis.document = { createElement: (t) => new El(t) };
  const { renderSessionTree } = await import('../dist/sessionTree.js');
  const ul = new El('ul');
  renderSessionTree(ul, rows, { projectName: () => undefined, onToggle() {}, now: Date.parse('2026-10-02T00:00:00Z'), ...ctx });
  return ul;
}

test('the list is one tab stop: a tree whose active descendant is the cursor row', async () => {
  const ul = await render(rowsOf(), { selectedId: 'lead', cursorId: 'w2' });
  assert.equal(ul.getAttribute('role'), 'tree');
  assert.equal(ul.tabIndex, 0);
  assert.equal(ul.getAttribute('aria-activedescendant'), 'session-row-w2');
  const byId = Object.fromEntries(ul.children.map((li) => [li.dataset.id, li]));
  assert.equal(byId.w2.id, 'session-row-w2');
  assert.deepEqual(ul.children.map((li) => li.getAttribute('aria-selected')), ['false', 'false', 'true', 'false']);
  assert.ok(ul.children.every((li) => li.getAttribute('role') === 'treeitem'));
  assert.equal(byId.lead.getAttribute('aria-current'), 'true');
  assert.equal(byId.lead.getAttribute('aria-expanded'), 'true');
  assert.equal(byId.lead.getAttribute('aria-level'), '1');
  assert.equal(byId.w1.getAttribute('aria-level'), '2');
  assert.equal(byId.lead.getAttribute('aria-label'), 't-lead, Running, Child needs you');
  assert.equal(byId.w1.getAttribute('aria-label'), 't-w1, Waiting for you, Needs you');
  assert.equal(byId.w2.getAttribute('aria-label'), 't-w2, Running');
});

test('with no cursor the open session is the active row; a folded container says so', async () => {
  const ul = await render(rowsOf(new Set(['lead'])), { selectedId: 'solo', cursorId: null });
  assert.equal(ul.getAttribute('aria-activedescendant'), 'session-row-solo');
  assert.equal(ul.children[0].getAttribute('aria-expanded'), 'false');
  assert.equal(ul.children[1].getAttribute('aria-selected'), 'true');
});

test('the page wires the list keys and the list is a focusable tree in the markup', () => {
  const html = readFileSync(here('../static/index.html'), 'utf8');
  assert.match(html, /<ul id="session-list" class="session-list" role="tree" tabindex="0" aria-label="Sessions">/);
  const main = readFileSync(here('../src/main.ts'), 'utf8');
  assert.match(main, /initSessionListKeys\(\);/);
  // The global Left/Right step yields to the focused list (actions/input.ts).
  assert.match(readFileSync(here('../src/actions/input.ts'), 'utf8'), /if \(sessionListOwnsKey\(ev, active\)\) return;/);
  assert.match(readFileSync(here('../static/styles.css'), 'utf8'), /\.session-list:focus \.session-item\[aria-selected="true"\] \{ outline: 2px solid/);
});
