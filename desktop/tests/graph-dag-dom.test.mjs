// Graph review and the DAG browser with Vim keys off (local #231): the empty canvas names its
// buttons, Esc closes Graph review, and the DAG's cancel / recovery / run / inspect controls are
// buttons that open the same typed-confirmation control as their keys. Both DOM modules run
// against a minimal fake document (no browser needed); a fake that renders toasts needs
// replaceChildren and querySelector, which this one has.
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
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};
Object.defineProperty(globalThis, 'navigator', { value: { platform: 'Linux x86_64', userAgent: 'test' }, configurable: true });

// The daemon, as the webview's Tauri bridge sees it.
const calls = [];
// One saved workflow, offered only to the test that opens it (Graph review's field editor).
const SAVED = { id: 'd0000010-0000-0000-0000-000000000001', title: 'Saved plan', stage: 'draft', artifact_path: null, project_id: null, created_at: '2026-10-20T10:00:00Z', updated_at: '2026-10-20T10:00:00Z' };
const SAVED_DEFINITION = {
  version: '1.0', name: 'Saved plan', description: 'two nodes',
  nodes: ['Plan', 'Build'].map((name, i) => ({ id: `n${i + 1}`, name, type: 'action', instructions: `${name} the change`, inputs: [], outputs: [], tags: [] })),
  edges: [{ source: 'n1', target: 'n2' }],
};
let offerSaved = false;
const GRAPH = { id: 'g1111111-0000-0000-0000-000000000000', title: 'Plan', status: 'active', execution_mode: 'fake', updated_at: '2026-10-20T10:00:00Z' };
globalThis.__TAURI__ = {
  core: {
    async invoke(cmd, args) {
      calls.push([cmd, args]);
      switch (cmd) {
        case 'graph_capabilities': return Object.fromEntries(['recursive_dag_inspection', 'recursive_dag_cancellation_control', 'recursive_dag_recovery_control', 'recursive_dag_recovery_status', 'recursive_dag_scheduler_control', 'recursive_dag_run_inspection'].map((k) => [k, true]));
        case 'graph_list_recursive': return [GRAPH];
        case 'dag_get_graph': return { graph: GRAPH, nodes: [], attempts: [], edges: [] };
        case 'dag_cancel_graph': return { id: 'c1', status: 'applied' };
        case 'graph_list_workflows': return offerSaved ? [SAVED] : [];
        case 'graph_get_workflow': return { document: { workflow: SAVED, definition: SAVED_DEFINITION } };
        case 'graph_starters': return [];
        default: return Array.isArray(args) ? [] : null;
      }
    },
  },
};

const { initVimKeys, setVimKeys } = await import('../dist/vimkeys.js');
const { initDag, openDag, closeDag, isDagOpen } = await import('../dist/dag.js');
const { initGraphReview, openGraphReview, closeGraphReview } = await import('../dist/graphs.js');
initVimKeys();
initDag();
initGraphReview();

const pump = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };
const keydown = (key, extra = {}) => {
  const ev = { key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, isComposing: false, target: doc.activeElement, preventDefault() {}, stopImmediatePropagation() {}, stopPropagation() {}, ...extra };
  for (const f of winListeners.keydown ?? []) f(ev);
};
const view = (id) => doc.getElementById(id);
const btn = (root, label) => root.querySelectorAll('button').find((b) => b.textContent.startsWith(label));
const dagAction = (id) => view('dag-view').querySelector(`[data-dag-action="${id}"]`);

test('the empty Graph review canvas names its buttons with Vim keys off and keeps the key lines with them on', async () => {
  setVimKeys(false);
  await openGraphReview();
  await pump();
  const canvas = view('graph-view').querySelector('.graph-empty');
  assert.equal(canvas.textContent, 'Empty graphClick Starter to begin from a template or Saved to open a saved workflow.');
  closeGraphReview();
  setVimKeys(true);
  await openGraphReview();
  await pump();
  assert.equal(view('graph-view').querySelector('.graph-empty').textContent, 'Empty graph' + 't  open a starter topology' + 'o  open a saved workflow' + 'q  close');
  closeGraphReview();
});

test('Esc closes Graph review with Vim keys off; with them on it keeps the TUI meaning', async () => {
  setVimKeys(false);
  await openGraphReview();
  await pump();
  assert.equal(view('graph-view').hidden, false);
  keydown('Escape', { target: view('graph-view') });
  assert.equal(view('graph-view').hidden, true, 'Esc closed the review');
  assert.equal(layout.hidden, false);
  setVimKeys(true);
  await openGraphReview();
  await pump();
  keydown('Escape', { target: view('graph-view') });
  assert.equal(view('graph-view').hidden, false, 'with Vim keys on Esc does nothing here');
  keydown('q', { target: view('graph-view') });
  assert.equal(view('graph-view').hidden, true, 'q still closes it');
});

test('the Recursive and Interrupt buttons say what they do and the status line has no camera jargon', async () => {
  setVimKeys(false);
  await openGraphReview();
  await pump();
  const root = view('graph-view');
  assert.equal(btn(root, 'Recursive').title, 'Browse recursive graphs (read-only)');
  assert.equal(btn(root, 'Interrupt').title, 'Stop the running workflow');
  const status = root.querySelector('.graph-status').textContent;
  assert.match(status, /^node 0\/0 · view follows the running node · /);
  assert.doesNotMatch(status, /camera/);
  closeGraphReview();
});

test('DAG browser with Vim keys off: Cancel graph… opens its reason box by click, and Enter submits it', async () => {
  setVimKeys(false);
  openDag(null);
  await pump();
  assert.equal(isDagOpen(), true);
  const cancel = dagAction('cancel-graph');
  assert.ok(cancel, 'the action row has Cancel graph…');
  assert.equal(cancel.textContent, 'Cancel graph…');
  assert.equal(cancel.disabled, false, 'an active hydrated graph can be cancelled');
  assert.equal(dagAction('cancel-run').disabled, true, 'no scheduler run is selected');
  assert.match(dagAction('cancel-run').title, /^Unavailable: no scheduler run is selected/);
  assert.equal(dagAction('inspect').disabled, true);
  cancel.click();
  const box = view('dag-view').querySelector('.dag-control');
  assert.ok(box, 'the typed-confirmation control opened');
  assert.match(box.textContent, /^Cancel graph g1111111: reason/);
  const input = box.querySelector('input');
  input.value = 'no longer needed';
  input.fire('input');
  input.fire('keydown', { key: 'Enter' });
  await pump();
  const sent = calls.find(([c]) => c === 'dag_cancel_graph');
  assert.deepEqual(sent?.[1], { graphId: GRAPH.id, reason: 'no longer needed' });
  closeDag();
});

test('DAG Esc still closes the browser, and the Recovery budget… button follows the gate', async () => {
  setVimKeys(false);
  openDag(null);
  await pump();
  assert.equal(dagAction('recover').disabled, true, 'no deferred recovery work is visible');
  assert.match(dagAction('recover').title, /no deferred recovery work/);
  keydown('Escape', { target: view('dag-view') });
  assert.equal(isDagOpen(), false);
});

test('Graph review\'s field editor: one Esc cancels the edit (the box is plain, as the TUI\'s), with Vim keys on', async () => {
  setVimKeys(true);
  offerSaved = true;
  await openGraphReview();
  await pump();
  const root = view('graph-view');
  const press = (k) => keydown(k, { target: root });
  press('o');
  await pump();
  press('Enter');
  await pump();
  press('Enter');
  press('i');
  await pump();
  const input = root.querySelector('.gv-edit-input');
  assert.ok(input, 'i opens the name box');
  assert.equal(input.dataset.vim, 'off', 'it is a plain box: no Vim insert mode to leave first');
  assert.match(root.textContent, /Enter saves, Esc cancels/);
  doc.activeElement = input;
  const esc = input.fire('keydown', { key: 'Escape' });
  assert.equal(esc.prevented, true);
  assert.equal(root.querySelector('.gv-edit-input'), null, 'the first Esc closed the field');
  assert.doesNotMatch(root.textContent, /Enter saves, Esc cancels/);
  offerSaved = false;
  closeGraphReview();
});
