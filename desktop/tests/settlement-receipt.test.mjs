// Local #276: the Source-worktree settlement dialog shows what the TUI receipt shows (items with phase and refusal, the
// after-observation, the unproven survivors of #149), in plain words for someone who has never seen the TUI. The dialog
// runs against a minimal fake document and a fake Tauri bridge serving fixtures; nothing reaches a real rsid.
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

const RUN = '11111111-1111-4111-8111-111111111111';
const REPO = 'github.com/o/r';
const counts = { observed: 4, eligible: 2, retained: 1, settled: 1, refused: 1, recovery_required: 1, unattempted: 1 };
const COHORTS = [{ repository_identity: REPO, canonical_repo_dir: '/repo', live_roots: 1, terminal_roots: 3 }];
const RUNS = [{ run_id: RUN, repository_identity: REPO, state: 'partial', counts, created_at: '2026-10-20T10:00:00Z' }];
const item = (sequence, phase, extra = {}) => ({ sequence, session_id: RUN, custody_id: RUN, source_ref: `refs/heads/rsi/work-${sequence}`, phase, ...extra });
const RECEIPT = {
  run_id: RUN, repository_identity: REPO, canonical_repo_dir: '/repo', state: 'partial', counts, updated_at: '2026-10-20T10:05:00Z',
  attribution: { origin: 'operator' }, terminal_error: 'database busy',
  items: [
    item(0, 'settled'),
    item(1, 'refused', { refusal_code: 'source_ref_in_use', after_observation: 'branch still checked out at /other' }),
    item(2, 'recovery_required'),
    item(3, 'unattempted'),
  ],
  unproven_survivors: [
    { sequence: 0, pid: 4242, start_time_micros: 1, executable: '/opt/hardened/bin/claude', sandbox_root: '/sandboxes/live', reason: 'environment_withheld_by_macos' },
    { sequence: 1, pid: 4343, start_time_micros: 2, reason: 'environment_withheld_by_macos' },
  ],
};

const AUDIT = {
  repository_identity: REPO, canonical_repo_dir: '/repo', target_ref: 'refs/heads/rolling', target_oid: 'a'.repeat(40), plan_digest: 'b'.repeat(64),
  authorization_phrase: null, applyable: false, counts, run_id: null,
  items: [
    { session_id: RUN, sandbox_root: '/sandboxes/a', source_ref: 'refs/heads/rsi/a', disposition: 'eligible_integrated_ancestor', proof: 'integrated_ancestor' },
    { session_id: '22222222-2222-4222-8222-222222222222', sandbox_root: '/sandboxes/b', source_ref: 'refs/heads/rsi/b', disposition: 'dirty_worktree', proof: 'none', diagnostic: '3 files changed' },
  ],
};
const calls = [];
let receipt = RECEIPT;
globalThis.__TAURI__ = {
  core: {
    async invoke(cmd, args) {
      calls.push(cmd);
      if (cmd === 'list_source_worktree_cohorts') return COHORTS;
      if (cmd === 'list_source_worktree_settlement_runs') return RUNS;
      if (cmd === 'audit_source_worktree_cohort') return AUDIT;
      if (cmd === 'get_source_worktree_settlement_run') {
        if (receipt instanceof Error) throw receipt;
        return receipt;
      }
      throw new Error(`unexpected command ${cmd}`);
    },
  },
};

const { initVimKeys } = await import('../dist/vimkeys.js');
const { openSettlement } = await import('../dist/admin-ops.js');
initVimKeys();

const pump = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };
const open = async () => {
  await openSettlement();
  await pump();
  return doc.getElementById('admin-settle-dialog');
};
const text = (dlg, sel) => dlg.querySelectorAll(sel).map((n) => n.textContent);

test('the settlement dialog lists each root with its status, plain reason and observation, and the run facts', async () => {
  receipt = RECEIPT;
  const dlg = await open();
  assert.ok(calls.includes('get_source_worktree_settlement_run'), 'the full receipt is read with the existing RPC');
  const items = dlg.querySelectorAll('.admin-item');
  assert.equal(items.length, 4);
  assert.deepEqual(items.map((n) => n.className.split(/\s+/).at(-1)), ['ok', 'bad', 'warn', 'muted']);
  assert.match(items[0].textContent, /#0 refs\/heads\/rsi\/work-0/);
  assert.match(items[0].textContent, /Settled/);
  assert.match(items[1].textContent, /Refused/);
  assert.match(items[1].textContent, /Reason: Another work folder still uses the branch/);
  assert.match(items[1].textContent, /branch still checked out at \/other/);
  assert.match(items[2].textContent, /Needs recovery/);
  assert.match(items[2].textContent, /Audit this repository again to see what is left to recover/);
  assert.match(items[3].textContent, /Not attempted/);
  const facts = dlg.querySelectorAll('.admin-grid').map((g) => g.textContent).join('|');
  assert.match(facts, /Started byYou \(the operator\)/);
  assert.match(facts, /OutcomePartly finished/);
  assert.match(facts, /1 settled · 1 refused · 1 need recovery · 1 not attempted/);
  assert.ok(text(dlg, '.admin-warn').includes('Last error: database busy'));
  assert.equal(items[1].textContent, '#1 refs/heads/rsi/work-1RefusedReason: Another work folder still uses the branchbranch still checked out at /other');
});

test('the settlement dialog lists both unproven survivors with pid, executable, root, reason, what it means and the by-hand note', async () => {
  receipt = RECEIPT;
  const dlg = await open();
  const warn = text(dlg, '.admin-warn');
  assert.ok(warn.includes('pid 4242 · /opt/hardened/bin/claude · root /sandboxes/live · environment withheld by macOS'));
  assert.ok(warn.includes('pid 4343 · <executable unknown> · cohort repository /repo · environment withheld by macOS'));
  const notes = text(dlg, '.dialog-note');
  assert.ok(notes.some((t) => /stop these by hand if they belong to it/i.test(t)));
  assert.ok(notes.some((t) => /What this means: macOS would not let settlement read/.test(t)));
  assert.ok(text(dlg, 'h3').includes('Programs left running (unproven survivors)'));
});

test('a run without items or survivors says so, and shows no survivors section', async () => {
  receipt = { ...RECEIPT, items: [], unproven_survivors: undefined, terminal_error: null, attribution: null };
  const dlg = await open();
  assert.equal(dlg.querySelectorAll('.admin-item').length, 0);
  assert.ok(text(dlg, '.dialog-note').includes('This run recorded no roots.'));
  assert.deepEqual(text(dlg, 'h3'), [`Latest run ${RUN}`, 'Roots']);
  assert.match(dlg.querySelectorAll('.admin-grid').map((g) => g.textContent).join('|'), /Unknown \(a run from before origins were recorded\)/);
});

test('a receipt for another repository is refused and a failed read is said plainly', async () => {
  receipt = { ...RECEIPT, repository_identity: 'github.com/x/y' };
  let dlg = await open();
  assert.equal(dlg.querySelectorAll('.admin-item').length, 0);
  assert.ok(text(dlg, '.dialog-note').some((t) => /Run details not shown: The repository does not match the selected cohort/.test(t)));
  receipt = new Error('rsid is down');
  dlg = await open();
  assert.ok(text(dlg, '.dialog-note').some((t) => /Run details could not be read: Error: rsid is down/.test(t)));
});

test('an audit lists each root with what would happen to it, in plain words', async () => {
  receipt = RECEIPT;
  const dlg = await open();
  dlg.onkeydown({ key: 'Enter', target: dlg, ctrlKey: false, metaKey: false, altKey: false, isComposing: false, preventDefault() {}, stopPropagation() {} });
  await pump();
  const rows = dlg.querySelectorAll('.admin-item').slice(0, 2);
  assert.equal(rows[0].textContent, `${RUN} · /sandboxes/aCan be settled: its work is already mergedproof: integrated ancestor`);
  assert.equal(rows[1].textContent, '22222222-2222-4222-8222-222222222222 · /sandboxes/bKept: the work folder has uncommitted changesproof: none · 3 files changed');
  assert.deepEqual(rows.map((r) => r.className.split(/\s+/).at(-1)), ['ok', 'warn']);
});
