// The overlay key sweep (local #321, audit #316). The `?` help shows each overlay's key table generated from the TUI
// (src/overlay-help.generated.ts) and marks every row bound. This test presses every key such a table lists in the real
// overlay, from a fresh open, and fails when the key does nothing: it must change the overlay's state (selection, filter,
// open/closed, a button run, a toast, the help dialog) or sit in NATIVE with the reason the browser or another handler
// owns it. A TUI key added to a swept table later fails here until the desktop handles it (or the table is corrected).
// OVERLAY_TABLES that this fixture cannot drive are listed in NOT_SWEPT, so a brand-new table fails until someone
// classifies it. Runs against a minimal fake document (no browser needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.tagName = tag.toUpperCase(); this.children = []; this.listeners = {}; this._text = ''; this.className = '';
    this.attrs = {}; this.parent = null; this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = '';
    this.title = ''; this.open = false; this.tabIndex = 0; this.id = ''; this.placeholder = ''; this.type = '';
    const self = this;
    this.classList = {
      add(c) { self.classList.toggle(c, true); },
      remove(c) { self.classList.toggle(c, false); },
      contains(c) { return self.className.split(/\s+/).includes(c); },
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
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); this.parent = null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  removeAttribute(k) { delete this.attrs[k]; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  closest() { return null; }
  focus() { doc.activeElement = this; }
  select() {}
  get elements() { return { namedItem: () => new FakeEl('input') }; }
  scrollIntoView() {}
  showModal() { this.open = true; }
  close() { if (!this.open) return; this.open = false; for (const f of this.listeners.close ?? []) f({}); this.onclose?.(); }
}
function matches(n, sel) {
  return sel.split(',').some((s) => {
    const m = /^([a-z0-9]*)((?:\.[\w-]+)*)$/i.exec(s.trim());
    if (!m || n.tag === '#text') return false;
    if (m[1] && n.tag !== m[1].toLowerCase()) return false;
    return m[2].split('.').filter(Boolean).every((c) => n.className.split(/\s+/).includes(c));
  });
}
const body = new FakeEl('body');
// The app shell elements commands.snapshot() and dispatch() read; the overlays under test are looked up for real.
const SHELL_IDS = { test: (id) => !/^(memory-dialog|admin-schedules-dialog|admin-schedule-form-dialog|keys-help)$/.test(id) };
const shell = (id) => { const e = new FakeEl(/input|search/.test(id) ? 'input' : 'div'); e.id = id; body.append(e); return e; };
const doc = {
  documentElement: { dataset: {} }, title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener() {}, contains: () => true,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => body.walk().find((n) => n.id === id) ?? (SHELL_IDS.test(id) ? shell(id) : null),
  createElement: (tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
body.append(Object.assign(new FakeEl('div'), { id: 'toast' }));
const storage = new Map();
globalThis.window = globalThis;
globalThis.document = doc;
globalThis.localStorage = { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)), removeItem: (k) => storage.delete(k) };
globalThis.addEventListener = () => {};
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};
// A key may start work that needs more shell than this fixture has (the picker leader's command dispatch, the schedule
// form's field lookups). That work runs after the test body ended and only proves the key was taken; its own failures are
// ignored here, nothing else is.
const beyondFixture = (e) => /^Error: missing #/.test(String(e)) || /openScheduleForm|dist\/(commands|launch)\.js/.test(String(e?.stack));
process.on('unhandledRejection', (e) => { if (!beyondFixture(e)) throw e; });
process.on('uncaughtException', (e) => { if (!beyondFixture(e)) throw e; });
const memoryHits = Array.from({ length: 4 }, (_, i) => ({ path: `m${i}.md`, start_line: 1, end_line: 2, score: 1, snippet: `hit ${i}`, source: 'memory' }));
const jobs = Array.from({ length: 4 }, (_, i) => ({
  id: `j${i}`, name: `job ${i}`, message: 'm', schedule: { recurrence: { type: 'Once' }, anchor: '2026-10-01T00:00:00Z' },
  last_fired_at: null, next_fire_at: '2030-01-01T00:00:00Z', enabled: true,
}));
globalThis.__TAURI__ = { core: { invoke: async (cmd) => {
  if (cmd === 'memory_search') return memoryHits;
  if (cmd === 'list_scheduled_job_holds' || cmd === 'providers') return [];
  if (cmd === 'list_scheduled_jobs') return { jobs, next_cursor: null, include_history: false };
  return null;
} } };

const { openPicker } = await import('../dist/pickers.js');
const { runAssist } = await import('../dist/assist.js');
const { openSchedules } = await import('../dist/admin-schedules.js');
const { OVERLAY_TABLES } = await import('../dist/overlay-help.generated.js');
const { isKeyHelpOpen } = await import('../dist/keyhelp.js');

// ---------- key notation ----------

/** The TUI notation in a table's Keys column, split into single keys: "Down / Up, Ctrl-J / Ctrl-K" -> four. */
function keysOf(cell) { return cell.split(/\s*,\s*|\s+\/\s+/).map((k) => k.trim()).filter(Boolean); }

const NAMED = { Down: 'ArrowDown', Up: 'ArrowUp', Esc: 'Escape', Space: ' ' };
/** A KeyboardEvent-like object for one key token, or null for text entry (`Type`, `Backspace`). */
function eventOf(token, target) {
  const base = { target, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, isComposing: false, prevented: false, preventDefault() { this.prevented = true; }, stopPropagation() {} };
  const ctrl = /^Ctrl-(\w)$/.exec(token);
  if (ctrl) return { ...base, key: ctrl[1].toLowerCase(), ctrlKey: true };
  return { ...base, key: NAMED[token] ?? token };
}
function press(dlg, ev) {
  for (const f of dlg.listeners.keydown ?? []) f(ev);
  dlg.onkeydown?.(ev);
  return ev;
}

// ---------- the overlays under test ----------

const ITEMS = ['alpha', 'beta', 'gamma', 'delta', 'epsilon'].map((label, i) => ({ id: String(i + 1), label, current: i === 2 }));

/** Open one overlay fresh. `state()` is everything a key may change; `type(text)` replaces the filter text. */
const OPENERS = {
  'Sort Picker': () => picker({ title: 'Sort', filterable: false, vimKeys: true }),
  'Project Picker': () => picker({ title: 'Project', filterable: true, ctrl: true }),
  'Label Picker': () => picker({ title: 'Label', filterable: true, ctrl: true }),
  'Parent Picker': () => picker({ title: 'Parent', filterable: true }),
  'Memory Search': async () => {
    runAssist('memory-search', {});
    const dlg = doc.getElementById('memory-dialog');
    const input = dlg.querySelector('input');
    input.value = 'q';
    input.listeners.input[0]({});
    await new Promise((r) => setTimeout(r, 5));
    press(dlg, eventOf('Down', input)); // a middle row, so Up and Ctrl-K have somewhere to go
    return {
      dlg, input,
      state: () => JSON.stringify({ open: dlg.open, sel: dlg.querySelectorAll('li').findIndex((l) => l.getAttribute('aria-selected') === 'true'), rows: dlg.querySelectorAll('li').length }),
    };
  },
  'Scheduled Jobs Browser': async () => {
    await openSchedules();
    const dlg = doc.getElementById('admin-schedules-dialog');
    press(dlg, eventOf('j', { tagName: 'DIV' })); // a middle row, so k has somewhere to go
    helpSeen.length = 0;
    return {
      dlg, input: null,
      state: () => JSON.stringify({ open: dlg.open, sel: dlg.querySelectorAll('li').findIndex((l) => l.className.includes('sel')), help: helpSeen.length, forms: doc.getElementById('admin-schedule-form-dialog')?.open === true }),
    };
  },
};
const helpSeen = [];

function picker({ title, filterable, vimKeys, ctrl }) {
  const log = { picked: [], ran: [] };
  const buttons = ctrl ? ['n', 'e', 'd'].map((c) => ({ text: c, ctrl: c, run: (item) => log.ran.push(c + (item?.id ?? '')) })) : undefined;
  const dlg = openPicker({ title, hint: '', items: ITEMS, filterable, vimKeys, buttons, onPick: (item) => log.picked.push(item.id) });
  const input = dlg.querySelector('input');
  return {
    dlg, input: filterable ? input : null,
    state: () => JSON.stringify({
      open: dlg.open, log,
      sel: dlg.querySelectorAll('li').findIndex((l) => l.getAttribute('aria-selected') === 'true'),
      rows: dlg.querySelectorAll('li').length, filter: input.value,
    }),
  };
}

/** Keys the browser owns, so the overlay's own handler never sees them. */
const NATIVE = {
  Esc: 'a modal <dialog> closes itself on Escape (the cancel event)',
  Type: 'typing is the focused input’s own behaviour; its input event re-filters (checked below)',
  Backspace: 'text editing in the focused input',
};

/**
 * Keys whose effect lands after an await (a daemon call, a form that opens): the sweep only demands the handler took the
 * key (preventDefault) when the state snapshot cannot show the effect from a fresh open.
 */
const HANDLED_ONLY = { 'Scheduled Jobs Browser': ['n', 'Enter', 'e', 'Space', 't', 'd', 'dd', 'r', 'H', 'm', 'q'] };

/** Overlay tables this fixture cannot drive (each needs the app shell or live data); a new table fails until it is classified. */
const NOT_SWEPT = [
  'Theme Role Editor',
  'Create Entity / Normal',
  'Create Entity / Insert',
  'Create Entity / Body',
  'Create Entity / Topology',
  'Model Picker',
  'Theme Picker',
  'File Explorer',
  'File Explorer / Finder',
  'File Explorer / Viewer Focus',
  'File Viewer',
  'Telescope',
  'Command Palette',
  'Manager Scope',
  'Manager Board',
  'Manager Decisions',
  'Manager Policy',
  'Manager / Text Entry',
  'Global Manager Workspace',
  'Global Dashboard',
  'Notifications',
  'Recent Completions',
  'Question / Normal',
  'Question / Insert',
  'Trash',
  'Source Worktree Settlement',
  'Graph Review',
  'Graph Review / Empty',
  'Graph Review / Node Detail',
  'Graph Review / Edit Field',
  'Graph Review / Picker',
  'Recursive DAG Browser',
  'Project Form',
  'Label Form',
  'Provider Form',
  'Provider Key Form',
  'Message Bridge Form',
  'Hook Form',
  'Hook Save Conflict',
  'Budget Policy Form',
  'Schedule Form',
  'Rename Session',
  'Text Area Background',
  'Daemon Text Setting',
  'Prompt Preview',
  'Skill Preview',
  'To-do Pop-up',
  'Diagnostics',
  'Session Info',
  'Rate Session',
  'Switch Model / Effort',
  'Terminal',
  'AI Command',
  'AI Chat',
  'Card Editor',
  'Dialectic',
  'Input Modal',
  'New Session Prompt',
  'New Session Settings',
  'Continue Prompt',
  'Fleet',
  'Fleet Filter',
  'Fleet Sort',
  'Fleet Usage',
  'Screens Without an Overlay Catalog',
  'Surfaces Documented by Hand',
];

test('every generated overlay table is swept here or listed in NOT_SWEPT (a new TUI overlay cannot slip by)', () => {
  const names = OVERLAY_TABLES.map((t) => t.name);
  assert.ok(names.length >= 32, 'the #316 sweep covered 32 tables; the generated help has ' + names.length);
  const known = new Set([...Object.keys(OPENERS), ...NOT_SWEPT]);
  assert.deepEqual(names.filter((n) => !known.has(n)), [], 'classify each new overlay table: add an OPENERS entry (preferred) or NOT_SWEPT');
  assert.deepEqual([...known].filter((n) => !names.includes(n)), [], 'a swept or listed table no longer exists in the generated help');
  for (const n of Object.keys(OPENERS)) assert.ok(names.includes(n), n);
});

for (const name of Object.keys(OPENERS)) {
  const table = OVERLAY_TABLES.find((t) => t.name === name);
  const seen = new Set();
  for (const row of table?.rows ?? []) {
    for (const token of keysOf(row.keys)) {
      // The leader (Space N, Space ;) and the help chord are checked in their own tests below.
      if (['Space N', 'Space ;', 'N', ';'].includes(token) || token === 'Ctrl-Alt-G' || seen.has(token)) continue;
      seen.add(token);
      test(`${name}: ${token} (${row.action}) changes state or is native`, async () => {
        if (NATIVE[token]) return;
        const o = await OPENERS[name]();
        const before = o.state();
        const ev = pressToken(o, token);
        const after = o.state();
        if (token === '?') { await new Promise((r) => setTimeout(r, 50)); assert.equal(isKeyHelpOpen(), true, `${name}: ? opens the keyboard help`); return; }
        if (HANDLED_ONLY[name]?.includes(token)) { assert.ok(ev.prevented, `${name}: "${token}" is listed in the ? help but the handler ignores it`); return; }
        assert.ok(ev.prevented || after !== before, `${name}: "${token}" is listed in the ? help but did nothing (${row.action})`);
        assert.notEqual(after, before, `${name}: "${token}" (${row.action}) took the key but changed no state`);
      });
    }
  }
}

function pressToken(o, token) {
  const target = o.input ?? { tagName: 'DIV' };
  if (/^[a-z]{2}$/.test(token)) { let last; for (const c of token) last = press(o.dlg, eventOf(c, target)); return last; }
  return press(o.dlg, eventOf(token, target));
}

test('the picker filter: typing and clearing re-filter the list', async () => {
  const o = await OPENERS['Project Picker']();
  const all = JSON.parse(o.state()).rows;
  o.input.value = 'bet';
  o.input.listeners.input[0]({});
  assert.equal(JSON.parse(o.state()).rows, 1, 'only "beta" holds "bet"');
  o.input.value = '';
  o.input.listeners.input[0]({});
  assert.equal(JSON.parse(o.state()).rows, all);
});

test('the picker leader: Space arms it (the hint shows it); Space N / Space ; themselves are pickerLeader, tests/overlays.test.mjs', async () => {
  const o = await OPENERS['Sort Picker']();
  const hint = o.dlg.querySelector('.pick-hint');
  assert.equal(hint.className.includes('leader'), false);
  const ev = press(o.dlg, eventOf('Space', { tagName: 'DIV' }));
  assert.equal(ev.prevented, true);
  assert.equal(hint.className.includes('leader'), true);
  assert.equal(JSON.parse(o.state()).open, true, 'Space only arms the leader');
});

test('the picker lands where the help says: g/G and Home/End jump to the first and last row, Ctrl-J/K move one', async () => {
  const sel = (o) => JSON.parse(o.state()).sel;
  const sort = await OPENERS['Sort Picker']();
  assert.equal(sel(sort), 2);
  pressToken(sort, 'G'); assert.equal(sel(sort), 4);
  pressToken(sort, 'g'); assert.equal(sel(sort), 0);
  const proj = await OPENERS['Project Picker']();
  pressToken(proj, 'End'); assert.equal(sel(proj), 4);
  pressToken(proj, 'Home'); assert.equal(sel(proj), 0);
  pressToken(proj, 'Ctrl-J'); assert.equal(sel(proj), 1);
  pressToken(proj, 'Ctrl-K'); assert.equal(sel(proj), 0);
});

test('with the filter being typed in, g and G stay text (only the Vim picker without a filter box jumps)', async () => {
  const proj = await OPENERS['Project Picker']();
  const before = proj.state();
  const ev = pressToken(proj, 'G');
  assert.equal(ev.prevented, false);
  assert.equal(proj.state(), before);
});

test('Memory search: Ctrl-J / Ctrl-K move the selection like Ctrl-N / Ctrl-P and the arrows', async () => {
  const sel = (o) => JSON.parse(o.state()).sel;
  const o = await OPENERS['Memory Search']();
  assert.equal(sel(o), 1, 'the opener moved to the second hit');
  pressToken(o, 'Ctrl-J'); assert.equal(sel(o), 2);
  pressToken(o, 'Ctrl-N'); assert.equal(sel(o), 3);
  pressToken(o, 'Ctrl-K'); assert.equal(sel(o), 2);
  pressToken(o, 'Ctrl-P'); assert.equal(sel(o), 1);
});
