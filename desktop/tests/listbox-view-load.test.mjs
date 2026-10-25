// Local #318 x #310: a list inside a view built from the shared load frame (src/view-load.ts) announces its current row
// after the first load and after a refused read followed by Retry, because the view's own `open(data)` builds the list and
// syncs it either way. The real loadView and syncListbox run against the fake document.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FakeEl, doc, body } from './fake-doc.mjs';

const { loadView } = await import('../dist/view-load.js');
const { syncListbox } = await import('../dist/listbox-a11y.js');

function buildView(items) {
  const dlg = new FakeEl('dialog');
  dlg.id = 'lb-view';
  body.append(dlg);
  const ul = new FakeEl('ul');
  ul.setAttribute('role', 'listbox');
  items.forEach((name, i) => {
    const li = new FakeEl('li');
    li.setAttribute('role', 'option');
    li.setAttribute('aria-selected', String(i === 1));
    li.dataset.index = String(i);
    li.textContent = name;
    ul.append(li);
  });
  dlg.append(ul);
  syncListbox(ul, dlg);
  return { dlg, ul };
}

const settle = () => new Promise((r) => setTimeout(r, 0));

test('the list in a loaded view announces its selected row', async () => {
  let built;
  await loadView({ key: 'lb', title: 'List', failed: 'Failed to load', read: async () => ['a', 'b'], open: (d) => { built = buildView(d); } });
  const id = built.dlg.getAttribute('aria-activedescendant');
  assert.ok(id, 'the owner points at a row');
  assert.equal(doc.getElementById(id).getAttribute('aria-selected'), 'true');
  assert.equal(doc.getElementById(id).textContent, 'b');
  built.dlg.remove();
});

test('after a refused read and Retry the list announces its selected row', async () => {
  let built; let calls = 0;
  const p = loadView({
    key: 'lb2', title: 'List', failed: 'Failed to load',
    read: async () => { calls += 1; if (calls === 1) throw new Error('rsid refused'); return ['a', 'b', 'c']; },
    open: (d) => { built = buildView(d); },
  });
  await p; await settle();
  assert.equal(built, undefined, 'a refused read builds nothing');
  const frame = doc.getElementById('view-load-dialog');
  const retry = frame.querySelectorAll('button').find((b) => b.textContent.startsWith('Retry'));
  retry.fire('click');
  await settle(); await settle();
  assert.ok(built, 'Retry builds the view');
  const id = built.dlg.getAttribute('aria-activedescendant');
  assert.equal(doc.getElementById(id).textContent, 'b');
  assert.equal(doc.getElementById(id).getAttribute('aria-selected'), 'true');
  built.dlg.remove();
});
