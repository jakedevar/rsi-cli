// The Actions palette and the Help overlay with Vim keys off (local #227): the rows show only
// keys that still work, the empty state does not teach a Vim key, Help has a real search box
// that filters with the switch off, opens on Getting started and its footer follows the switch.
// Both DOM modules run against a minimal fake document (no browser needed); a fake that renders
// toasts needs replaceChildren and querySelector, which this one has.
import { test } from 'node:test';
import assert from 'node:assert/strict';

function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel);
  if (!m) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name, val] = /^\[([\w-]+)(?:="([^"]*)")?\]$/.exec(a) ?? [];
    if (name === 'open') { if (!n.open) return false; continue; }
    if (name.startsWith('data-')) {
      const key = name.slice(5).replace(/-(\w)/g, (_, c) => c.toUpperCase());
      if (!(key in n.dataset) || (val !== undefined && n.dataset[key] !== val)) return false;
      continue;
    }
    if (!(name in n.attrs) || (val !== undefined && n.attrs[name] !== val)) return false;
  }
  return true;
}
const matches = (n, selector) => selector.split(',').map((s) => s.trim()).some((sel) => matchesSimple(n, sel.split(/\s+/).at(-1)));

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.open = false;
    this.scrollTop = 0; this.offsetTop = 0; this.tabIndex = 0;
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
    };
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...kids) {
    for (const k of kids) {
      if (typeof k === 'string') { this.append(Object.assign(new FakeEl('#text'), { _text: k })); continue; }
      if (k.tag === '#frag') { this.append(...k.children); continue; }
      k.parent = this; this.children.push(k);
    }
  }
  replaceChildren(...kids) { this._text = ''; this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  removeAttribute(k) { delete this.attrs[k]; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  fire(t, ev = {}) { const e = { target: this, preventDefault() { e.prevented = true; }, stopPropagation() {}, ...ev }; for (const f of this.listeners[t] ?? []) f(e); return e; }
  closest(sel) { for (let n = this; n; n = n.parent) if (n.tag !== '#text' && matches(n, sel)) return n; return null; }
  focus() { doc.activeElement = this; }
  select() {}
  scrollIntoView() {}
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  showModal() { this.open = true; }
  close() { this.open = false; this.onclose?.(); }
}

const ids = new Map();
const mk = (tag, id) => { const e = new FakeEl(tag); e.id = id; ids.set(id, e); return e; };
const body = new FakeEl('body');
const doc = {
  documentElement: { dataset: {} }, title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener() {},
  contains: () => true,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => ids.get(id) ?? body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
const palette = mk('dialog', 'palette');
const input = mk('input', 'palette-input');
const list = mk('ul', 'palette-list');
const toast = mk('div', 'toast');
palette.append(input, list);
body.append(palette, toast);
const storage = new Map();
globalThis.window = globalThis;
globalThis.document = doc;
globalThis.localStorage = { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)), removeItem: (k) => storage.delete(k) };
globalThis.addEventListener = () => {};
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};

const { openPalette } = await import('../dist/palette.js');
const { openKeyHelp } = await import('../dist/keyhelp.js');
const { initPalette } = await import('../dist/palette.js');
const { isVimOnlyHint } = await import('../dist/key-hints.js');
initPalette();

const snap = (over = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null, ...over,
});
const type = (text) => { input.value = text; input.fire('input'); };
const rows = () => list.querySelectorAll('.palette-item');
const chips = () => rows().flatMap((r) => r.querySelectorAll('kbd').map((k) => k.textContent));
const titleOf = (r) => r.querySelector('.palette-title').textContent;

test('with Vim keys off no palette row shows a Vim-only key', () => {
  openPalette(snap({ vimKeys: false }));
  assert.ok(rows().length > 10, 'the palette lists the actions');
  for (const k of chips()) assert.ok(!isVimOnlyHint(k), `chip ${k}`);
  assert.ok(chips().includes('Ctrl-K') || chips().includes('Cmd-K'), 'the palette’s own chord stays');
  for (const q of ['next session', 'archive', 'issue']) {
    type(q);
    for (const k of chips()) assert.ok(!isVimOnlyHint(k), `"${q}": chip ${k}`);
  }
});

test('with Vim keys on the rows keep their Vim key chips', () => {
  openPalette(snap());
  type('next session');
  assert.ok(chips().includes('j'));
});

test('help, files, todo and dag preselect an enabled row; Enter would run it', () => {
  openPalette(snap({ vimKeys: false, files: { hasRoot: true, explorer: false, viewer: false, picker: false, showHidden: false, followPreview: false, target: false, editing: false } }));
  for (const [q, title] of [['help', /Keyboard shortcuts|Open manual/], ['files', /File explorer/], ['todo', /Operator to-do panel/], ['dag', /Recursive DAG browser/]]) {
    type(q);
    const first = rows()[0];
    assert.ok(first, `"${q}" finds something`);
    assert.match(titleOf(first), title, q);
    assert.equal(first.dataset.index, '0');
    assert.equal(first.getAttribute('aria-selected'), 'true');
    assert.ok(!first.className.includes('disabled'), `"${q}": the preselected row can run`);
  }
});

test('the empty-query list says what is hidden, and an unmatched word suggests plain ones', () => {
  openPalette(snap({ vimKeys: false, open: null, cursorOnRow: false, rowCount: 0 }));
  assert.match(list.querySelector('.palette-empty').textContent, /more actions work once their screen is open/);
  type('zzzzqqq');
  const empty = list.querySelector('.palette-empty').textContent;
  assert.match(empty, /Try one plain word/);
  assert.ok(!empty.includes(':'), 'no ":" command taught with Vim keys off');
  openPalette(snap());
  type('zzzzqqq');
  assert.match(list.querySelector('.palette-empty').textContent, /start with :/);
});

test('"portfolio" and "manager" find Appoint a portfolio manager…', () => {
  openPalette(snap({ vimKeys: false }));
  for (const q of ['portfolio', 'portfolio manager']) {
    type(q);
    assert.ok(rows().some((r) => titleOf(r) === 'Appoint a portfolio manager…'), q);
  }
  type('manager');
  assert.ok(rows().some((r) => titleOf(r) === 'Appoint a portfolio manager…'), 'manager');
});

// ---------- Help ----------

const help = () => doc.getElementById('keys-help');
const helpRows = () => help().querySelectorAll('.keys-help-row');
const searchBox = () => help().querySelector('.keys-help-search');
const typeHelp = (text) => { searchBox().value = text; searchBox().fire('input'); };
const activeTab = () => help().querySelector('.seg.active').textContent;

test('Help with Vim keys off opens on Getting started with a focused search box and a footer without Vim keys', () => {
  storage.clear();
  openKeyHelp(snap({ vimKeys: false }));
  assert.equal(activeTab(), 'Getting started');
  assert.equal(searchBox().tag, 'input');
  assert.equal(doc.activeElement, searchBox(), 'typing is the way in');
  const foot = help().querySelector('.keys-help-foot').textContent;
  assert.match(foot, /Type to search/);
  assert.doesNotMatch(foot, /j\/k|g\/G|\bq closes|s symbols|\/ search/);
});

test('the Help search filters rows with Vim keys off, in Getting started and in Contextual', () => {
  openKeyHelp(snap({ vimKeys: false }));
  const all = helpRows().length;
  assert.ok(all >= 6);
  typeHelp('session');
  const some = helpRows();
  assert.ok(some.length > 0 && some.length < all, 'the search narrows Getting started');
  for (const r of some) assert.match(r.textContent.toLowerCase(), /session/);
  typeHelp('zzzz-no-such-thing');
  assert.equal(helpRows().length, 0);
  assert.match(help().querySelector('.pick-empty').textContent, /Nothing matches/);
  typeHelp('');
  help().querySelectorAll('.seg').find((b) => b.textContent === 'Contextual').fire('click');
  typeHelp('shortcuts');
  assert.ok(helpRows().some((r) => r.textContent.includes('Keyboard shortcuts')), 'a synonym finds the entry (help → shortcuts)');
  typeHelp('stop');
  assert.ok(helpRows().some((r) => r.querySelector('.t').textContent === 'Stop this turn'));
  for (const r of helpRows()) for (const k of r.querySelectorAll('kbd')) assert.ok(!isVimOnlyHint(k.textContent), `Contextual chip ${k.textContent}`);
});

test('Help with Vim keys on opens on Contextual after the first time, keeps its Vim footer, and "/" focuses the box', () => {
  storage.clear();
  openKeyHelp(snap());
  assert.equal(activeTab(), 'Getting started', 'the first time');
  help().close();
  openKeyHelp(snap());
  assert.equal(activeTab(), 'Contextual');
  assert.match(help().querySelector('.keys-help-foot').textContent, /j\/k scroll/);
  help().onkeydown({ key: '/', target: help(), preventDefault() {}, stopPropagation() {} });
  assert.equal(doc.activeElement, searchBox());
});
