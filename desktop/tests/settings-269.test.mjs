// Settings is easy to find and use (local #269): a click on a row starts the edit, values read as words, Close with unapplied
// changes asks Apply / Discard / Keep editing, and Cmd-, / Ctrl-, opens the pane. The pure parts run on the model; the pane and
// the close prompt run in settings.ts against a minimal fake document (no browser), with the daemon's config and update faked.
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
    if (name === 'open') { if (!n.open) return false; continue; }
    if (name === 'hidden') { if (!n.hidden) return false; continue; }
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
    this.tabIndex = 0; this.id = '';
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
    };
  }
  get tagName() { return this.tag.toUpperCase(); }
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
  after(node) { const p = this.parent; node.parent = p; p.children.splice(p.children.indexOf(this) + 1, 0, node); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); this.parent = null; }
  replaceWith(node) { const p = this.parent; node.parent = p; p.children[p.children.indexOf(this)] = node; this.parent = null; }
  contains(n) { return this.walk().includes(n); }
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

const body = new FakeEl('body');
const doc = {
  documentElement: { dataset: {} }, title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener() {},
  contains: () => true,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
const layout = new FakeEl('div'); layout.className = 'layout';
const issuesView = new FakeEl('div'); issuesView.id = 'issues-view'; issuesView.hidden = true;
const toastEl = new FakeEl('div'); toastEl.id = 'toast';
body.append(layout, issuesView, toastEl);
const windowKeydown = [];
const storage = new Map();
globalThis.window = globalThis;
globalThis.document = doc;
globalThis.localStorage = { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)), removeItem: (k) => storage.delete(k) };
globalThis.addEventListener = (type, fn) => { if (type === 'keydown') windowKeydown.push(fn); };
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};

const model = await import('../dist/settings-model.js');
const { SETTINGS_ROWS, DAEMON_WIDGETS } = await import('../dist/settings.generated.js');
const { settingsApi } = await import('../dist/settings-api.js');
const settings = await import('../dist/settings.js');
const { askCloseChoice } = await import('../dist/settings-close.js');

// ---------- the fake daemon ----------

const CONFIG = { title_model_provider: 'openrouter', title_model_fallback: 'haiku' };
const updates = [];
let refuse = false;
settingsApi.getConfig = async () => ({ ...CONFIG });
settingsApi.update = async (u) => { updates.push(u); if (refuse) throw new Error('daemon refused'); for (const x of u) CONFIG[x.field] = x.value; return null; };

const find = (kind) => SETTINGS_ROWS.find((r) => r.owner.kind === 'daemon' && r.kind !== 'read_only' && r.kind !== 'action' && DAEMON_WIDGETS[r.owner.args[0]]?.widget === kind);
const SWITCH = find('switch');
const SWITCH2 = SETTINGS_ROWS.filter((r) => r.owner.kind === 'daemon' && r.kind !== 'read_only' && r.kind !== 'action' && DAEMON_WIDGETS[r.owner.args[0]]?.widget === 'switch' && r.id !== SWITCH.id)[0];
const CHOICE = find('choice');
const settle = () => new Promise((r) => setTimeout(r, 0));

const view = () => body.walk().find((n) => n.id === 'settings-view');
const rowLi = (label) => view().querySelectorAll('.settings-row').find((li) => li.querySelector('.sv-label').textContent === label);
const valueOf = (label) => rowLi(label).querySelector('.sv-value');
const status = () => view().querySelector('.settings-head-status').textContent;
const button = (key) => view().querySelector(`[data-settings="${key}"]`);
const closeDialog = () => body.walk().find((n) => n.id === 'settings-close');
const choiceButtons = () => closeDialog().querySelectorAll('button');
// The header buttons are served by one delegated listener on the header, as in the page.
const press = (b) => (b.dataset.settings ? view().querySelector('.settings-head').fire('click', { target: b }) : b.fire('click'));
const toastText = () => toastEl.textContent;

settings.initSettings();
settings.openSettings(SWITCH.section);
await settle();

// ---------- values read as words ----------

test('a model-role row reads as words, not field=value, and keeps the raw fields under Details', async () => {
  settings.openSettings('model_roles');
  await settle();
  const shown = valueOf('Title model').textContent;
  assert.equal(shown, 'Provider: openrouter · Fallback model: haiku');
  assert.doesNotMatch(shown, /=|title_model_/);
  rowLi('Title model').fire('click', { target: rowLi('Title model').querySelector('.sv-label') });
  const details = view().querySelector('.settings-details');
  assert.ok(details, 'the info card has a Details block');
  assert.match(details.textContent, /title_model_provider: openrouter/);
  assert.match(details.textContent, /title_model_base_url: \(not set\)/);
  const card = view().querySelector('.settings-info').textContent;
  assert.doesNotMatch(card, /\b[A-Z][a-z]+(?:[A-Z][a-z]+)+\b[^\n]*\)|localStorage|UpdateDaemonConfig|GetDaemonConfig/, 'no RPC or storage names outside Details');
});

test('a switch shows On or Off and a choice shows its value between arrows', async () => {
  settings.openSettings(SWITCH.section);
  await settle();
  assert.match(valueOf(SWITCH.label).textContent, /^(● On|○ Off)$/);
  settings.openSettings(CHOICE.section);
  await settle();
  const choice = valueOf(CHOICE.label);
  assert.deepEqual(choice.children.map((c) => c.textContent).filter((t, i) => i !== 1), ['◂', '▸']);
  assert.equal(choice.title.length > 10, true, 'the value says what a click does');
});

// ---------- one click ----------

test('one click on a switch value flips it; a click on its label only selects the row', async () => {
  settings.openSettings(SWITCH.section);
  await settle();
  const before = valueOf(SWITCH.label).textContent;
  rowLi(SWITCH.label).fire('click', { target: rowLi(SWITCH.label).querySelector('.sv-label') });
  assert.equal(valueOf(SWITCH.label).textContent, before, 'the label click changed nothing');
  assert.match(status(), /No pending changes/);
  rowLi(SWITCH.label).fire('click', { target: valueOf(SWITCH.label) });
  assert.notEqual(valueOf(SWITCH.label).textContent, before, 'the value click flipped it at once');
  assert.match(status(), /^1 pending change · Apply saves it/);
  settings.discardPending();
});

test('one click on a choice value steps forward; the left arrow steps back', async () => {
  settings.openSettings(CHOICE.section);
  await settle();
  const text = () => valueOf(CHOICE.label).children[1].textContent;
  const next = () => rowLi(CHOICE.label).fire('click', { target: valueOf(CHOICE.label).children[2] });
  const back = () => rowLi(CHOICE.label).fire('click', { target: valueOf(CHOICE.label).children[0] });
  next();
  const first = text();
  next();
  const second = text();
  assert.notEqual(second, first, 'the right arrow moved on');
  back();
  assert.equal(text(), first, 'the left arrow came back');
  assert.match(status(), /^1 pending change/);
  settings.discardPending();
});

test('one click on a text row opens its editor; a read-only row only selects', async () => {
  settings.openSettings('model_roles');
  await settle();
  const li = rowLi('Stall classifier model');
  assert.ok(li, 'the row exists');
  assert.ok(li.querySelector('.sv-edit'), 'a pencil shows the value is editable');
  li.fire('click', { target: li.querySelector('.sv-label') });
  const dlg = body.walk().find((n) => n.id === 'settings-text');
  assert.equal(dlg?.open, true, 'a click anywhere on the row opened the editor');
  dlg.close();
});

test('a model-role row served by a provider dialog opens it on the first click', async () => {
  const ran = [];
  settings.openSettings('model_roles');
  await settle();
  // The operator-config slice registers this overlay in the real app.
  const { registerRowOverlay } = settings;
  registerRowOverlay('title_model', { enter: 'Choose which provider this model uses', stored: 'the daemon', run: (k) => ran.push(k) });
  settings.openSettings('model_roles');
  rowLi('Title model').fire('click', { target: rowLi('Title model').querySelector('.sv-label') });
  assert.deepEqual(ran, ['Enter']);
  assert.match(valueOf('Title model').title, /Click to open/);
});

// ---------- closing ----------

test('Close with nothing unapplied closes at once, with no prompt', async () => {
  settings.openSettings(SWITCH.section);
  await settle();
  press(button('close'));
  await settle();
  assert.equal(view().hidden, true);
  assert.equal(closeDialog(), undefined);
});

async function stageOne() {
  settings.openSettings(SWITCH.section);
  await settle();
  rowLi(SWITCH.label).fire('click', { target: valueOf(SWITCH.label) });
  assert.match(status(), /^1 pending change/);
}

test('Close with a staged change asks Apply, Discard or Keep editing, and Keep editing keeps it', async () => {
  await stageOne();
  press(button('close'));
  await settle();
  assert.equal(closeDialog().open, true);
  assert.deepEqual(choiceButtons().map((b) => b.textContent), ['Keep editing', 'Discard', 'Apply change']);
  assert.match(closeDialog().textContent, /Apply your changes\?/);
  assert.match(closeDialog().textContent, /1 change you have not applied yet/);
  assert.equal(doc.activeElement.textContent, 'Keep editing', 'the safe answer has focus');
  press(choiceButtons()[0]);
  await settle();
  assert.equal(view().hidden, false, 'still editing');
  assert.match(status(), /^1 pending change/);
  assert.equal(closeDialog(), undefined, 'the prompt is gone');
});

test('Esc on the prompt means Keep editing', async () => {
  press(button('close'));
  await settle();
  const dlg = closeDialog();
  dlg.fire('cancel');
  await settle();
  assert.equal(view().hidden, false);
  assert.match(status(), /^1 pending change/);
});

test('Discard closes and drops the change', async () => {
  press(button('close'));
  await settle();
  press(choiceButtons()[1]);
  await settle();
  assert.equal(view().hidden, true);
  assert.equal(updates.length, 0, 'nothing was sent');
  settings.openSettings(SWITCH.section);
  await settle();
  assert.match(status(), /No pending changes/);
});

test('Apply sends the change through the existing update, then closes', async () => {
  await stageOne();
  press(button('close'));
  await settle();
  press(choiceButtons()[2]);
  await settle(); await settle();
  assert.equal(updates.length, 1);
  assert.equal(updates[0].length, 1);
  assert.equal(updates[0][0].field, SWITCH.owner.args[0]);
  assert.equal(view().hidden, true, 'closed after applying');
  assert.equal(settings.settingsPending(), 0);
});

test('Apply that the daemon refuses keeps Settings open with the change still pending', async () => {
  refuse = true;
  settings.openSettings(SWITCH2.section);
  await settle();
  rowLi(SWITCH2.label).fire('click', { target: valueOf(SWITCH2.label) });
  press(button('close'));
  await settle();
  press(choiceButtons()[2]);
  await settle(); await settle();
  assert.equal(view().hidden, false, 'not closed');
  assert.match(status(), /^1 pending change/);
  assert.match(toastText(), /not applied/);
  refuse = false;
  settings.discardPending();
  press(button('close'));
  await settle();
});

test('Esc and q ask too, and another full-window view keeps the staged change instead of dropping it', async () => {
  await stageOne();
  settings.closeSettings({ keepStaged: true });
  assert.equal(view().hidden, true);
  assert.match(toastText(), /1 unapplied change is kept/);
  settings.openSettings(SWITCH.section);
  await settle();
  assert.match(status(), /^1 pending change/, 'the change survived the other view');
  settings.discardPending();
  const src = readFileSync(fileURLToPath(new URL('../src/settings.ts', import.meta.url)), 'utf8');
  assert.equal((src.match(/case 'close': void requestCloseSettings\(\); break;/g) ?? []).length, 2, 'the Close button and the Esc / q key');
});

test('the close prompt is one-shot and removes itself', async () => {
  const p = askCloseChoice(3);
  assert.equal(closeDialog().open, true);
  assert.deepEqual(choiceButtons().map((b) => b.textContent), ['Keep editing', 'Discard', 'Apply 3 changes']);
  press(choiceButtons()[2]);
  assert.equal(await p, 'apply');
  assert.equal(closeDialog(), undefined);
});

// ---------- the shortcut ----------

test('Ctrl-, is claimed from anywhere, with the Vim keys switch off or on; other keys and an open dialog are left alone', async () => {
  press(button('close'));
  await settle();
  assert.equal(view().hidden, true);
  // The handler claims the chord and then dispatches settings-open; dispatch needs the whole app, so a throw after the claim is fine here.
  const chord = (ev) => {
    let prevented = false;
    for (const fn of windowKeydown) {
      try { fn({ preventDefault() { prevented = true; }, stopPropagation() {}, stopImmediatePropagation() {}, target: body, ...ev }); } catch { /* dispatch needs the app */ }
    }
    return prevented;
  };
  assert.equal(chord({ key: ',', ctrlKey: true }), true);
  assert.equal(chord({ key: ',' }), false, 'a plain comma is typed text');
  assert.equal(chord({ key: ',', ctrlKey: true, shiftKey: true }), false);
  const dlg = new FakeEl('dialog'); dlg.open = true; body.append(dlg);
  assert.equal(chord({ key: ',', ctrlKey: true }), false, 'an open dialog keeps the key');
  dlg.remove();
});

// ---------- the pure rules ----------

const R = (id) => model.sectionRows(SETTINGS_ROWS.find((r) => r.id === id).section, {}).find((r) => r.id === id);

test('rowClick: edit and open rows act on the first click; switches and choices only on their value; read-only rows select', () => {
  const sw = { widget: { type: 'switch' } };
  const ch = { widget: { type: 'choice', options: ['a'] } };
  const tx = { widget: { type: 'text', valueType: 'string' } };
  assert.equal(model.rowClick(sw, false, true), 'activate');
  assert.equal(model.rowClick(sw, false, false), 'select');
  assert.equal(model.rowClick(ch, false, true), 'activate');
  assert.equal(model.rowClick(ch, false, false), 'select');
  assert.equal(model.rowClick(tx, false, false), 'activate');
  assert.equal(model.rowClick({ widget: { type: 'local', local: 'theme' } }, false, false), 'activate');
  assert.equal(model.rowClick({ widget: { type: 'local', local: 'vim-keys' } }, false, false), 'select');
  assert.equal(model.rowClick({ widget: { type: 'local', local: 'vim-keys' } }, false, true), 'activate');
  assert.equal(model.rowClick({ widget: { type: 'display' } }, false, true), 'select');
  assert.equal(model.rowClick({ widget: { type: 'display' } }, true, false), 'activate', 'a display row with its own dialog opens it');
  assert.equal(model.rowClick({ widget: { type: 'tui-only', why: 'x' } }, false, true), 'select');
  assert.equal(model.rowClick({ widget: { type: 'partial', why: 'x' } }, false, true), 'select');
});

test('roleSummary: words for what is set, Default for nothing, the address left to Details', () => {
  const f = ['title_model_local', 'title_model_provider', 'title_model_base_url', 'title_model_fallback'];
  assert.equal(model.roleSummary(f, {}), 'Default');
  assert.equal(model.roleSummary(f, { title_model_local: null, title_model_provider: '', title_model_base_url: 'http://x' }), 'Default');
  assert.equal(model.roleSummary(f, { title_model_provider: 'bedrock', title_model_base_url: 'http://x' }), 'Provider: bedrock');
  assert.equal(model.roleSummary(f, { title_model_local: true }), 'Local model');
  assert.equal(model.roleSummary(['memory_model_local', 'memory_model_fallback', 'memory_model_fallback_provider'], { memory_model_fallback: 'sonnet', memory_model_fallback_provider: 'x' }), 'Fallback model: sonnet · Fallback provider: x');
  assert.equal(model.roleSummary(['dream_model', 'dream_model_provider'], { dream_model: 'opus' }), 'Model: opus');
});

test('rowValue of a model-role row is that summary, never field=value', () => {
  assert.equal(model.rowValue(R('title_model'), { title_model_provider: 'p' }, model.EMPTY_STAGING), 'Provider: p');
});

test('closeCopy names the count and the three answers', () => {
  const one = model.closeCopy(1);
  assert.equal(one.title, 'Apply your changes?');
  assert.deepEqual([one.apply, one.discard, one.keep], ['Apply change', 'Discard', 'Keep editing']);
  assert.match(one.body, /1 change you have not applied yet/);
  const many = model.closeCopy(4);
  assert.equal(many.apply, 'Apply 4 changes');
  assert.match(many.body, /4 changes you have not applied yet/);
  for (const c of [one, many]) assert.doesNotMatch(Object.values(c).join(' '), /\b(RPC|staged|UpdateDaemonConfig)\b/);
});

test('the top bar has a Settings button that runs the settings-open action', () => {
  const html = readFileSync(fileURLToPath(new URL('../static/index.html', import.meta.url)), 'utf8');
  assert.match(html, /<button id="open-settings" class="btn" type="button" data-action="settings-open">Settings<\/button>/);
  assert.ok(html.indexOf('id="open-settings"') < html.indexOf('id="open-palette"'), 'next to Issues and Actions…');
});

// ---------- Settings › Context rotation lists each manager seat's coordinator budget (#325) ----------

// The TUI test's fixture (context_settings_render_the_authoritative_coordinator_budget), as the daemon serializes it.
const SEAT_ID = '3f2a9c1d-7b64-4e0a-9d51-0c8e2a6b7f10';
const SEAT = {
  session_id: SEAT_ID,
  budget: {
    provider: 'Codex', model: 'gpt-6.1-sol',
    window: { active_tokens: 258400, capacity: {}, evidence: { source: 'legacy_unverified', confidence: 'degraded' } },
    measured_window_tokens: null,
    coordinator_cap: 40, coordinator_cap_unit: 'percent', coordinator_cap_source: 'coordinator_context_cap_tokens', coordinator_cap_tokens: 103360,
    succession_pct: 53.0, succession_source: 'context_rotation_global_pct', rotation_enabled: true,
    effective_tokens: 103360, effective_pct: 40.0, binding_limit: 'coordinator_context_cap_tokens',
  },
};
const capCard = async () => {
  settings.openSettings('context_rotation');
  settings.refreshSettings(); // the config is re-read, as after Refresh or a reopen
  await settle();
  const li = rowLi(SETTINGS_ROWS.find((r) => r.id === 'coordinator_context_cap').label);
  assert.ok(li, 'the Coordinator context cap row is listed');
  li.fire('click', { target: li.querySelector('.sv-label') });
  return view().querySelector('.settings-info');
};

test('the Coordinator context cap card lists one plain-words line group per manager seat', async () => {
  CONFIG.coordinator_context_budgets = [SEAT, { ...SEAT, session_id: '99999999-0000-4000-8000-000000000000', budget: { ...SEAT.budget, provider: 'Claude', model: null, coordinator_cap: 0, coordinator_cap_unit: 'off', coordinator_cap_tokens: null, effective_tokens: null, effective_pct: null, binding_limit: null } }];
  const card = await capCard();
  const seats = card.querySelectorAll('.settings-seat');
  assert.equal(seats.length, 2, 'one entry per seat');
  const first = seats[0].textContent;
  assert.match(first, /Seat 3f2a9c1d · Codex \/ gpt-6\.1-sol/);
  assert.match(first, /Context window: 258,400 tokens \(an unverified estimate\)/);
  assert.match(first, /Coordinator cap: 40% of the window, which is 103,360 tokens/);
  assert.match(first, /Handing over to a fresh session at 53% full \(the global rotation percentage\)/);
  assert.match(first, /Rotates at 103,360 tokens \(40%\); the coordinator context cap is the limit that decides it/);
  const second = seats[1].textContent;
  assert.match(second, /Seat 99999999 · Claude \/ unknown model/);
  assert.match(second, /Coordinator cap: off for this seat/);
  assert.match(second, /Rotation point: not set/);
  assert.equal(card.querySelector('.settings-seats-empty'), null, 'no empty state when seats exist');
  // The TUI's one-line summary stays one click away under Details.
  assert.match(card.querySelector('.settings-details').textContent, /Seat 3f2a9c1d-7b64-4e0a-9d51-0c8e2a6b7f10: Codex\/gpt-6\.1-sol: window 258400 tokens \(legacy_unverified\); cap 40 percent \[coordinator_context_cap_tokens\]; succession 53\.00% \[context_rotation_global_pct\]; effective 103360 tokens \(40\.00%\) binds coordinator_context_cap_tokens/);
  assert.doesNotMatch(card.querySelector('.settings-seats').textContent, /[a-z]+_[a-z]+_[a-z_]+/, 'no config names outside Details');
});

test('a seat whose limit is the rotation percentage is told so, from the re-read config', async () => {
  CONFIG.coordinator_context_budgets = [{ ...SEAT, budget: { ...SEAT.budget, effective_tokens: 136952, effective_pct: 53, binding_limit: 'context_rotation_codex_pct', rotation_enabled: true } }];
  await capCard();
  assert.match(view().querySelector('.settings-info').textContent, /Rotates at 136,952 tokens \(53%\); the codex rotation percentage is the limit that decides it/);
});

test('no manager seats: the card says so instead of showing nothing', async () => {
  CONFIG.coordinator_context_budgets = [];
  const card = await capCard();
  assert.equal(card.querySelectorAll('.settings-seat').length, 0);
  assert.match(card.querySelector('.settings-seats-empty').textContent, /No manager seats are running/);
  delete CONFIG.coordinator_context_budgets;
  assert.match((await capCard()).querySelector('.settings-seats-empty').textContent, /No manager seats are running/, 'a daemon that sends no list reads the same');
});

// ---------- screen reader (local #330) ----------

test('the group rail is a named listbox and, with the keys on the rail, the pane points at the selected group', async () => {
  settings.openSettings(SWITCH.section);
  await settle();
  const rail = view().querySelector('.settings-rail');
  const groups = rail.querySelectorAll('.settings-group');
  assert.equal(rail.getAttribute('role'), 'listbox');
  assert.equal(rail.getAttribute('aria-label'), 'Settings groups');
  assert.ok(groups.length >= 2);
  const other = groups.find((g) => g.getAttribute('aria-selected') !== 'true');
  other.fire('click');
  await settle();
  const now = view().querySelector('.settings-rail').querySelectorAll('.settings-group');
  const selected = now.filter((g) => g.getAttribute('aria-selected') === 'true');
  assert.equal(selected.length, 1, 'one selected group');
  assert.equal(selected[0].dataset.group, other.dataset.group, 'the clicked group is the selected one');
  assert.equal(selected[0].getAttribute('role'), 'option');
  assert.ok(selected[0].className.includes('sel'));
  assert.equal(view().getAttribute('aria-activedescendant'), selected[0].id, 'the pane that holds the keys points at the group');
});

test('the rows are a named grid of row / rowheader / gridcell, and with the keys on the rows the pane points at the current row (#334)', async () => {
  settings.openSettings(SWITCH.section);
  await settle();
  const grid = () => view().querySelector('.settings-rows');
  const rows = () => grid().querySelectorAll('.settings-row');
  assert.equal(grid().getAttribute('role'), 'grid');
  assert.equal(grid().getAttribute('aria-label'), 'Settings rows');
  assert.ok(rows().length >= 2);
  assert.ok(rows().every((li) => li.getAttribute('role') === 'row'), 'every row is a grid row, not an option');
  // every child of a row is a cell: the label names the row, the value cell holds whatever the row edits
  for (const li of rows()) {
    assert.deepEqual(li.children.map((c) => c.getAttribute('role')), ['gridcell', 'rowheader', 'gridcell', 'gridcell']);
    assert.equal(li.querySelector('.sv-value').getAttribute('role'), 'gridcell');
    assert.equal(li.querySelector('.sv-label').getAttribute('role'), 'rowheader');
  }
  // click the label of the second row: it only selects, and the pane (which holds the keys) names that row
  const target = rows()[1];
  target.fire('click', { target: target.querySelector('.sv-label') });
  await settle();
  const selected = rows().filter((li) => li.getAttribute('aria-selected') === 'true');
  assert.equal(selected.length, 1, 'one current row');
  assert.equal(selected[0].dataset.index, '1');
  assert.ok(selected[0].className.includes('sel'));
  assert.equal(view().getAttribute('aria-activedescendant'), selected[0].id, 'the pane points at the current row');
  // with the keys back on the rail the pane points at the group again, not at a row
  view().querySelector('.settings-rail').querySelectorAll('.settings-group').find((g) => g.getAttribute('aria-selected') !== 'true').fire('click');
  await settle();
  const g = view().querySelector('.settings-rail').querySelectorAll('.settings-group').find((x) => x.getAttribute('aria-selected') === 'true');
  assert.equal(view().getAttribute('aria-activedescendant'), g.id);
});
