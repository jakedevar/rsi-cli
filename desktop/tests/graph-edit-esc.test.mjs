// Graph review's field editor (#339): with Vim keys on, one Esc cancels an edit of the node's name,
// as the box's own line and the "Graph Review / Edit Field" key table say. The name box used to be a
// Vim field, so the app's vim-field handler took the first Esc (insert -> normal mode) and a second
// Esc was needed. Runs the real Graph review DOM module against a minimal fake document that has
// createElementNS (the canvas is SVG) and a starter topology, so a starter can be opened and a node
// edited by key, as an operator would.
import { test } from 'node:test';
import assert from 'node:assert/strict';


function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel);
  if (!m) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name, val] = /^\[([\w-]+)(?:="([^"]*)")?\]$/.exec(a) ?? [];
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
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.checked = false;
    this.tabIndex = 0;
    const self = this;
    this.classList = {
      add(c) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); set.add(c); self.className = [...set].join(' '); },
      remove(c) { self.className = self.className.split(/\s+/).filter((x) => x && x !== c).join(' '); },
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
      contains(c) { return self.className.split(/\s+/).includes(c); },
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
  get ownerDocument() { return doc; }
  get tagName() { return this.tag.toUpperCase(); }
  hasAttribute(k) { return k in this.attrs; }
  get selectionStart() { return this._sel?.[0] ?? this.value.length; }
  get selectionEnd() { return this._sel?.[1] ?? this.value.length; }
  setSelectionRange(a, b) { this._sel = [a, b]; }
  dispatchEvent() {}
  after(...kids) { body.append(...kids); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); }
  replaceChildren(...kids) { this._text = ''; this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  removeAttribute(k) { delete this.attrs[k]; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  fire(t, ev = {}) { const e = { target: this, preventDefault() { e.prevented = true; }, stopPropagation() {}, ...ev }; for (const f of this.listeners[t] ?? []) f(e); return e; }
  click() { return this.fire('click'); }
  closest(sel) { for (let n = this; n; n = n.parent) if (n.tag !== '#text' && matches(n, sel)) return n; return null; }
  focus() { doc.activeElement = this; }
  blur() {}
  select() {}
  scrollIntoView() {}
  scrollBy() {}
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
}

const ids = new Map();
const mk = (tag, id) => { const e = new FakeEl(tag); e.id = id; ids.set(id, e); return e; };
const body = new FakeEl('body');
const winListeners = {};
const doc = {
  documentElement: { dataset: {} }, title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener(t, f) { (winListeners[`doc:${t}`] ??= []).push(f); },
  contains: () => true,
  createTreeWalker: () => ({ nextNode: () => null }),
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => ids.get(id) ?? body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
  createElementNS: (_ns, tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
const layout = new FakeEl('div'); layout.className = 'layout';
body.append(layout);
const toastEl = mk('div', 'toast');
body.append(toastEl);
for (const id of ['keyhint', 'cmdline']) { const e = mk('div', id); e.hidden = true; body.append(e); }
const vimBox = mk('input', 'vim-keys'); body.append(vimBox);
const storage = new Map();
globalThis.window = globalThis;
globalThis.document = doc;
globalThis.localStorage = { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)), removeItem: (k) => storage.delete(k) };
globalThis.addEventListener = (t, f) => { (winListeners[t] ??= []).push(f); };
const realSetTimeout = globalThis.setTimeout;
// Short deferrals (focus, blur) run; the review's poll timer never fires in a test.
globalThis.setTimeout = (f, ms = 0) => (ms >= 100 ? 0 : realSetTimeout(f, 0));
globalThis.clearTimeout = () => {};
globalThis.setInterval = () => 0;
globalThis.clearInterval = () => {};
globalThis.MutationObserver = class { observe() {} };
globalThis.HTMLInputElement = class { static [Symbol.hasInstance](x) { return x?.tag === "input"; } };
globalThis.HTMLTextAreaElement = class {};
Object.defineProperty(globalThis, 'navigator', { value: { platform: 'Linux x86_64', userAgent: 'test' }, configurable: true });

// The daemon, as the webview's Tauri bridge sees it.
const calls = [];
const GRAPH = { id: 'g1111111-0000-0000-0000-000000000000', title: 'Plan', status: 'active', execution_mode: 'fake', updated_at: '2026-10-20T10:00:00Z' };

const STARTER = { name: 'one', label: 'One', description: 'a starter', use_cases: [] };
const WORKFLOW = {
  version: '1.0', name: 'One', description: '', metadata: {}, edges: [{ source: 'a', target: 'b' }],
  nodes: [{ id: 'a', name: 'A', type: 'action', instructions: 'do a' }, { id: 'b', name: 'B', type: 'action', instructions: 'do b' }],
};
globalThis.__TAURI__ = {
  core: {
    async invoke(cmd, args) {
      switch (cmd) {
        case 'graph_starters': return [STARTER];
        case 'graph_starter_workflow': return WORKFLOW;
        case 'graph_list_workflows': return [];
        case 'graph_validate': return { executable: true, diagnostics: [] };
        default: return Array.isArray(args) ? [] : null;
      }
    },
  },
};

const { initVimKeys, setVimKeys } = await import('../dist/vimkeys.js');
const { initGraphReview, openGraphReview, closeGraphReview } = await import('../dist/graphs.js');
const { isVimField, vimKey } = await import('../dist/vim-field.js');
initVimKeys();
initGraphReview();

const pump = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };
const keydown = (key, extra = {}) => {
  const ev = { key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, isComposing: false, target: doc.activeElement, preventDefault() {}, stopImmediatePropagation() {}, stopPropagation() {}, ...extra };
  for (const f of winListeners.keydown ?? []) f(ev);
};
const view = () => doc.getElementById('graph-view');

/** Open the review, pick the starter, open the node detail and start the name edit (`i`). */
async function startNameEdit() {
  await openGraphReview();
  await pump();
  keydown('t', { target: view() });
  await pump();
  keydown('Enter', { target: view() });
  await pump();
  keydown('Enter', { target: view() });
  await pump();
  keydown('i', { target: view() });
  await pump();
  const input = view().querySelector('.gv-edit-input');
  assert.ok(input, 'the name box opened: ' + view().textContent);
  return input;
}

/**
 * One keydown on a field, the way the app routes it: the document-capture handler (actions/input.ts)
 * gives a Vim field's key to the Vim editor first and the field's own handler only sees what the
 * editor leaves; a plain field goes straight to its own handler.
 */
function keyOnField(input, key) {
  const ev = { key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, isComposing: false };
  if (isVimField(input) && vimKey(input, ev, 'linux').outcome === 'consumed') return { prevented: true, consumedByVim: true };
  return input.fire('keydown', ev);
}

test('with Vim keys on, one Esc cancels a node-name edit (the name box is a plain text box, not a Vim field)', async () => {
  setVimKeys(true);
  try {
    const input = await startNameEdit();
    assert.match(view().textContent, /Edit NameEnter saves, Esc cancels/);
    assert.equal(isVimField(input), false, 'a Vim field would take the first Esc (insert -> normal mode) and need a second');
    const e = keyOnField(input, 'Escape');
    await pump();
    assert.notEqual(e.consumedByVim, true, 'the Vim editor did not eat the Esc');
    assert.equal(e.prevented, true, 'the box consumed the Esc');
    assert.equal(view().querySelector('.gv-edit-input'), null, 'one Esc closed the box');
    assert.doesNotMatch(view().textContent, /Edit Name/);
    assert.match(view().textContent, /Instructions/, 'back on the node detail');
  } finally { closeGraphReview(); }
});
