// Local #248: the satellite-peers and the Devices/Projects dialogs follow #243's Tab rule. With Vim keys off Tab and
// Shift-Tab are the browser's focus order there too (the dialogs do not preventDefault them, so Tab can reach their
// buttons) and F6 / Shift-F6 switch the tab; with Vim keys on Tab and F6 both switch it. Both dialogs run against a
// minimal fake document (no browser needed) and the real modal-gate rule; text fields that use Tab for completion or
// field cycling (files-ops, budgets) are typing contexts and are left alone.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

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

FakeEl.prototype.showModal = function showModal() { this.open = true; };
FakeEl.prototype.close = function close() { this.open = false; this.onclose?.(); };
FakeEl.prototype.hasAttribute = function hasAttribute(k) { return k in this.attrs; };
FakeEl.prototype.toggleAttribute = function toggleAttribute(k, on) { if (on ?? !(k in this.attrs)) this.attrs[k] = ''; else delete this.attrs[k]; };
FakeEl.prototype.contains = function contains(n) { for (; n; n = n.parent) if (n === this) return true; return false; };

const REMOTE = {
  enabled: true, canonical_host: 'h', url: null, unit_installed: true, gateway_running: true, serve_route_present: true,
  funnel_off: true, serve_pending_command: null, tailscale: { reachable: true, backend_state: 'Running', version: '1', pinned_version: '1', version_qualified: true },
  peers: [{ id: 'n1', name: 'Phone', os: 'iOS', online: true, allowed: true }], projects: [{ id: 'p1', name: 'rsi', exposed: false }], errors: [],
};
const PEER = (label, id, observation) => ({
  config: { peer_id: id, label, expected_installation_id: null, enabled: true, read_enabled: true, dispatch_enabled: false },
  row_version: 1, links: [], observation, dispatch_scope: [], dispatch_project_scope: [],
});
const OBS = { state: 'connected', installation_id: null, incarnation_id: null, last_observed_at: null, last_error: null, cached_session_count: 0, stale: false };
let satPeers = [];
globalThis.__TAURI__ = {
  core: {
    async invoke(cmd) {
      if (cmd === 'list_satellite_peers') return { revision: 1, peers: satPeers };
      if (cmd === 'remote_get_status') return REMOTE;
      return null;
    },
  },
};

const { initVimKeys, setVimKeys } = await import('../dist/vimkeys.js');
const { openSatellites, openRemote } = await import('../dist/admin-ops.js');
const { formatKeyHint } = await import('../dist/key-hints.js');
initVimKeys();

const pump = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };
const btn = (root, label) => root.querySelectorAll('button').find((b) => b.textContent.startsWith(label));
const key = (dlg, k, shiftKey = false) => {
  const target = btn(dlg, 'Refresh') ?? btn(dlg, 'Close');
  const ev = { key: k, shiftKey, ctrlKey: false, metaKey: false, altKey: false, isComposing: false, target, prevented: false, preventDefault() { ev.prevented = true; }, stopPropagation() {} };
  dlg.onkeydown(ev);
  return ev;
};
const on = (dlg) => dlg.querySelectorAll('.admin-tabs button').filter((b) => b.className.split(/\s+/).includes('on')).map((b) => b.textContent);
const hint = (dlg) => dlg.querySelectorAll('.dialog-note').map((n) => n.textContent).find((t) => /cycles|switches/.test(t));

/** Each dialog: how to open it, the tab labels in cycle order, and the Tab phrase its hint teaches. */
const DIALOGS = [
  { name: 'satellite peers', id: 'admin-sat-dialog', open: openSatellites, tabs: ['Peers', 'Links', 'Cached sessions'], tabHint: /^Tab cycles peers, links and cached sessions/ },
  { name: 'Devices/Projects', id: 'admin-remote-dialog', open: openRemote, tabs: ['Devices', 'Projects'], tabHint: /^Tab switches Devices and Projects/ },
];

/** The tab the dialog shows now (the satellite dialog remembers its tab between opens). */
const current = (d, dlg) => d.tabs.indexOf(on(dlg)[0]);
const after = (d, i) => d.tabs[(i + 1) % d.tabs.length];

for (const d of DIALOGS) {
  test(`${d.name}: Vim keys off leaves Tab and Shift-Tab to the browser and F6 / Shift-F6 switch the tab`, async () => {
    setVimKeys(false);
    await d.open();
    await pump();
    const dlg = doc.getElementById(d.id);
    const start = current(d, dlg);
    assert.ok(start >= 0, 'a tab is selected');
    for (const shift of [false, true]) {
      const ev = key(dlg, 'Tab', shift);
      assert.equal(ev.prevented, false, `Tab shift=${shift}: the browser moves focus`);
      assert.deepEqual(on(dlg), [d.tabs[start]], `Tab shift=${shift}: the tab did not change`);
    }
    const f6 = key(dlg, 'F6');
    assert.equal(f6.prevented, true, 'F6 is claimed');
    assert.deepEqual(on(dlg), [after(d, start)], 'F6 switches the tab');
    const shifted = key(dlg, 'F6', true);
    assert.equal(shifted.prevented, true, 'Shift-F6 is claimed');
    assert.ok(on(dlg).length === 1, "Shift-F6 leaves one tab selected");
    dlg.close();
  });

  test(`${d.name}: Vim keys on, Tab and F6 both switch the tab and are claimed`, async () => {
    setVimKeys(true);
    await d.open();
    await pump();
    const dlg = doc.getElementById(d.id);
    const start = current(d, dlg);
    const tab = key(dlg, 'Tab');
    assert.equal(tab.prevented, true, 'Tab is claimed');
    assert.deepEqual(on(dlg), [after(d, start)], 'Tab switches the tab');
    const f6 = key(dlg, 'F6');
    assert.equal(f6.prevented, true, 'F6 is claimed');
    assert.deepEqual(on(dlg), [after(d, start + 1)], 'F6 switches it again');
    dlg.close();
  });

  test(`${d.name}: the hint teaches Tab as written and F6 with Vim keys off`, async () => {
    await d.open();
    await pump();
    const dlg = doc.getElementById(d.id);
    const raw = hint(dlg);
    assert.match(raw, d.tabHint, 'the source text names Tab');
    assert.equal(formatKeyHint(raw).split(/\s/)[0], 'F6', 'with Vim keys off the phrase names F6');
    dlg.close();
  });
}

test('both handlers ask tabIsNative first and read F6 through paneKeyName; typing contexts keep their own Tab', () => {
  const read = (f) => readFileSync(fileURLToPath(new URL(`../src/${f}`, import.meta.url)), 'utf8');
  const admin = read('admin-ops.ts');
  assert.equal([...admin.matchAll(/tabIsNative\(isVimKeysOn\(\), ev\)/g)].length, 2);
  assert.equal([...admin.matchAll(/paneKeyName\(ev\) === 'Tab'/g)].length, 2);
  assert.doesNotMatch(admin, /ev\.key === 'Tab'\) \{ ev\.preventDefault/);
  assert.match(read('key-hints.ts'), /cycles peers\| switches Devices/);
  // files-ops path completion and budgets still use Tab for their field.
  assert.match(read('files-ops.ts'), /ev\.key === 'Tab'\) \{ ev\.preventDefault\(\); ev\.stopPropagation\(\); void complete\(\)/, 'path completion keeps Tab');
  assert.match(read('budgets.ts'), /Tab \/ Shift-Tab fields/, 'budgets keeps its Tab field cycling');
});

test('the Satellite registry shows each peer row its activity, and the selected peer again in the detail (#323)', async () => {
  satPeers = [
    PEER('mac-mini', '15151515-1515-4515-8515-151515151515', { ...OBS, sessions_running: 1, workers_parked: 2, open_jobs: 2 }),
    PEER('laptop', '16161616-1616-4616-8616-161616161616', OBS),
  ];
  try {
    await openSatellites();
    await pump();
    const dlg = doc.getElementById('admin-sat-dialog');
    btn(dlg, 'Peers').click(); // the dialog remembers the tab the earlier tests left it on
    const rows = dlg.querySelectorAll('.admin-row').map((r) => r.textContent);
    assert.equal(rows.length, 2);
    assert.match(rows[0], /mac-mini.*running: 1 · parked: 2 · open jobs: 2$/);
    assert.match(rows[1], /laptop.*running: unknown · parked: unknown · open jobs: unknown$/);
    assert.match(dlg.textContent, /peer 15151515[^]*running: 1 · parked: 2 · open jobs: 2/, 'the selected peer shows it in the detail too');
    dlg.close();
  } finally { satPeers = []; }
});
