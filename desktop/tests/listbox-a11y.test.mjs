// Screen-reader wiring of the keyboard lists (local #318): after ArrowDown / j the element that holds focus points at the
// current option (aria-activedescendant) and that option is aria-selected and is the highlighted row. One shared helper
// (src/listbox-a11y.ts) does it; this file tests the helper, the palette, a filter picker and a plain picker through a fake
// document, then walks the source so a keyboard list added later cannot skip the helper.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { FakeEl, doc, mount } from './fake-doc.mjs';

const palette = mount('dialog', 'palette');
const input = mount('input', 'palette-input', palette);
const plist = mount('ul', 'palette-list', palette);
plist.setAttribute('role', 'listbox');
plist.setAttribute('aria-label', 'Actions');
mount('div', 'toast');

const { syncListbox, listRows, currentRow } = await import('../dist/listbox-a11y.js');
const { openPalette, initPalette } = await import('../dist/palette.js');
const { openPicker } = await import('../dist/pickers.js');
initPalette();

const snap = (over = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null, ...over,
});
const key = (el, k, more = {}) => el.fire('keydown', { key: k, ...more });
const optionOf = (focused) => doc.getElementById(focused.getAttribute('aria-activedescendant'));
const text = (n) => n.textContent.replace(/\s+/g, ' ').trim();

/** The announced row is the selected, highlighted one, and it is the only selected one. */
function assertAnnounced(owner, list) {
  const id = owner.getAttribute('aria-activedescendant');
  assert.ok(id, 'the focused element has an aria-activedescendant');
  const opt = doc.getElementById(id);
  assert.ok(opt, `the id ${id} names a row on the page`);
  assert.equal(opt.getAttribute('aria-selected'), 'true');
  assert.ok(opt.className.split(/\s+/).includes('sel'), 'it is the highlighted row');
  assert.deepEqual(listRows(list).filter((r) => r.getAttribute('aria-selected') === 'true'), [opt], 'it is the only selected row');
  return opt;
}

function row(list, role, selected, key2) {
  const li = new FakeEl('li');
  li.setAttribute('role', role);
  li.setAttribute('aria-selected', String(selected));
  if (key2 !== undefined) li.dataset.key = key2;
  list.append(li);
  return li;
}

test('syncListbox: a filter input becomes the combobox that controls the list and names the selected row', () => {
  const list = new FakeEl('ul');
  const box = new FakeEl('input');
  row(list, 'option', false, 'a');
  row(list, 'option', true, 'b');
  const id = syncListbox(list, box, { label: 'Things' });
  assert.equal(list.getAttribute('role'), 'listbox');
  assert.equal(list.getAttribute('aria-label'), 'Things');
  assert.ok(list.id);
  assert.equal(box.getAttribute('role'), 'combobox');
  assert.equal(box.getAttribute('aria-controls'), list.id);
  assert.equal(box.getAttribute('aria-expanded'), 'true');
  assert.equal(box.getAttribute('aria-autocomplete'), 'list');
  assert.equal(box.getAttribute('aria-activedescendant'), id);
  assert.equal(currentRow(list), list.children[1]);
  assert.equal(list.children[1].id, id);
});

test('syncListbox: an existing name and role are kept; a list that holds focus itself gets no combobox role', () => {
  const list = new FakeEl('ul');
  list.setAttribute('role', 'tree');
  list.setAttribute('aria-label', 'Project files');
  row(list, 'treeitem', true, 'src/a.ts');
  syncListbox(list, list, { label: 'Other' });
  assert.equal(list.getAttribute('role'), 'tree');
  assert.equal(list.getAttribute('aria-label'), 'Project files');
  assert.equal(list.getAttribute('aria-activedescendant'), `${list.id}-osrc_a_ts`, 'a path key becomes a safe id');
  assert.equal(list.getAttribute('aria-controls'), null);
});

test('syncListbox: ids survive a re-render that rebuilds the rows, and a list with no current row clears the pointer', () => {
  const list = new FakeEl('ul');
  const box = new FakeEl('input');
  row(list, 'option', false, 'x');
  const first = row(list, 'option', true, 'y');
  const id1 = syncListbox(list, box);
  list.replaceChildren();
  row(list, 'option', false, 'x');
  const second = row(list, 'option', true, 'y');
  assert.notEqual(first, second, 'the nodes are new');
  assert.equal(syncListbox(list, box), id1, 'the id is the same string');
  list.replaceChildren(new FakeEl('li'));
  assert.equal(syncListbox(list, box), null);
  assert.equal(box.getAttribute('aria-activedescendant'), null);
});

test('syncListbox: rows without a key take the position, and a note row is not a row', () => {
  const list = new FakeEl('ul');
  const note = new FakeEl('li');
  note.setAttribute('role', 'note');
  list.append(note);
  const a = row(list, 'option', false);
  const b = row(list, 'option', true);
  syncListbox(list, list);
  assert.equal(note.id, '');
  assert.notEqual(a.id, b.id);
  assert.equal(list.getAttribute('aria-activedescendant'), b.id);
});

test('palette: ArrowDown, Ctrl-N, Up and Ctrl-P move the announced option with the highlight', () => {
  openPalette(snap());
  assert.equal(input.getAttribute('role'), 'combobox');
  assert.equal(input.getAttribute('aria-expanded'), 'true');
  assert.equal(input.getAttribute('aria-controls'), plist.id);
  assert.equal(plist.getAttribute('aria-label'), 'Actions');
  const first = assertAnnounced(input, plist);
  assert.equal(first.dataset.index, '0');
  key(input, 'ArrowDown');
  const second = assertAnnounced(input, plist);
  assert.equal(second.dataset.index, '1');
  assert.notEqual(second.id, first.id);
  assert.ok(text(second).length > 0 && text(second) === text(plist.querySelector('.sel')), 'the announced option is the highlighted row');
  key(input, 'n', { ctrlKey: true });
  assert.equal(assertAnnounced(input, plist).dataset.index, '2');
  key(input, 'ArrowUp');
  assert.equal(assertAnnounced(input, plist).dataset.index, '1');
  key(input, 'p', { ctrlKey: true });
  assert.equal(assertAnnounced(input, plist).dataset.index, '0');
});

test('palette: typing re-ranks the rows and the pointer follows; no match clears it', () => {
  openPalette(snap());
  input.value = 'help';
  input.fire('input');
  const opt = assertAnnounced(input, plist);
  assert.match(text(opt), /help|shortcuts|manual/i);
  input.value = 'zzzzqqq';
  input.fire('input');
  assert.equal(input.getAttribute('aria-activedescendant'), null);
});

const dialogs = () => doc.body.children.filter((c) => c.tag === 'dialog' && c.className.includes('pick-list-dialog'));
const items = (n) => Array.from({ length: n }, (_, i) => ({ id: `id${i}`, label: `Item ${i}`, current: i === 1 }));

test('openPicker with a filter box: the box is the combobox, Down moves the announced option, typing narrows it', () => {
  const dlg = openPicker({ title: 'Choose project', hint: 'x', items: items(4), filterable: true, onPick() {} });
  assert.ok(dialogs().includes(dlg));
  const box = dlg.querySelector('input');
  const list = dlg.querySelector('ul');
  assert.equal(doc.activeElement, box, 'focus stays on the filter box');
  assert.equal(list.getAttribute('aria-label'), 'Choose project');
  assert.equal(box.getAttribute('role'), 'combobox');
  assert.equal(box.getAttribute('aria-controls'), list.id);
  let opt = assertAnnounced(box, list);
  assert.equal(text(opt), '✓Item 1', 'the current choice starts selected');
  dlg.fire('keydown', { key: 'ArrowDown', target: box });
  opt = assertAnnounced(box, list);
  assert.equal(text(opt), 'Item 2');
  dlg.fire('keydown', { key: 'ArrowUp', target: box });
  dlg.fire('keydown', { key: 'ArrowUp', target: box });
  assert.equal(text(assertAnnounced(box, list)), 'Item 0');
  box.value = 'item 3';
  box.fire('input');
  assert.equal(text(assertAnnounced(box, list)), 'Item 3');
  box.value = 'nothing like it';
  box.fire('input');
  assert.equal(box.getAttribute('aria-activedescendant'), null);
  dlg.close();
});

test('openPicker without a filter box: the list holds focus and carries the pointer; j and k move it', () => {
  const dlg = openPicker({ title: 'Sort by', hint: 'x', items: items(3), vimKeys: true, onPick() {} });
  const list = dlg.querySelector('ul');
  assert.equal(doc.activeElement, list, 'focus is on the list');
  assert.equal(list.getAttribute('role'), 'listbox');
  assert.equal(list.getAttribute('aria-label'), 'Sort by');
  assert.equal(text(assertAnnounced(list, list)), '✓Item 1');
  dlg.fire('keydown', { key: 'j', target: list });
  assert.equal(text(assertAnnounced(list, list)), 'Item 2');
  dlg.fire('keydown', { key: 'k', target: list });
  dlg.fire('keydown', { key: 'k', target: list });
  assert.equal(text(assertAnnounced(list, list)), 'Item 0');
  assert.equal(list.getAttribute('role'), 'listbox');
  dlg.close();
});

// ---------- every keyboard list in the source goes through the helper ----------

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const SRC = here('../src/');
const files = readdirSync(SRC, { recursive: true }).filter((f) => f.endsWith('.ts') && !f.endsWith('.generated.ts'));
const read = (f) => readFileSync(SRC + f, 'utf8');

/** Lists that are deliberately outside the helper, each with the reason. */
const OWN_MECHANISM = {
  'sessionTree.ts': 'the session tree sets aria-activedescendant itself (local #226)',
  'sessionlist-keys.ts': 'moves the session tree cursor and its aria-activedescendant (local #226)',
  'admin-dom.ts': 'builds listBox() / rowEl(); each caller that draws them calls syncListbox (checked below)',
};

test('every module that makes a listbox / tree / option calls syncListbox (or says why not)', () => {
  const makers = files.filter((f) => /setAttribute\('role', '(listbox|tree|option|treeitem)'\)/.test(read(f)));
  assert.ok(makers.length >= 15, `found ${makers.length} list modules`);
  const missing = makers.filter((f) => !OWN_MECHANISM[f] && !/syncListbox\(/.test(read(f)));
  assert.deepEqual(missing, [], 'these modules draw a role=listbox / option list but never call syncListbox (src/listbox-a11y.ts)');
  for (const f of makers) if (!OWN_MECHANISM[f]) assert.match(read(f), /from '\.{1,2}\/listbox-a11y\.js'/, f);
});

test('every module that draws admin listBox() rows calls syncListbox after drawing them', () => {
  const callers = files.filter((f) => f !== 'admin-dom.ts' && /\blistBox\(/.test(read(f)));
  assert.ok(callers.length >= 4, `found ${callers.length} callers`);
  for (const f of callers) {
    const src = read(f);
    const draws = (src.match(/\.scrollIntoView\(/g) ?? []).length;
    assert.ok((src.match(/syncListbox\(/g) ?? []).length >= Math.min(draws, 1), `${f} syncs its list`);
  }
});

test('the static lists in index.html are named', () => {
  const html = readFileSync(here('../static/index.html'), 'utf8');
  for (const m of html.matchAll(/<ul\b[^>]*role="listbox"[^>]*>/g)) assert.match(m[0], /aria-label="[^"]+"/, m[0]);
});
