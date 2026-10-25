// New session dialog: the `More options` button opens the settings side by mouse (local #292). prompts.ts and launch.ts run
// against a minimal fake document and a scripted bridge (no browser needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';

// ---------- the fake page ----------

function matchesSimple(n, sel) {
  const id = /^#([\w-]+)$/.exec(sel);
  if (id) return n.id === id[1];
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)$/i.exec(sel);
  if (!m) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  return m[2].split('.').filter(Boolean).every((c) => n.className.split(/\s+/).includes(c));
}
const matches = (n, selector) => selector.split(',').map((s) => s.trim()).some((sel) => matchesSimple(n, sel.split(/\s+/).at(-1)));

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this._value = ''; this.title = ''; this.open = false; this.tabIndex = 0;
    this.focused = false; this.id = ''; this.checked = false;
  }
  get value() { return this._value; }
  set value(v) { this._value = String(v); }
  get options() { return this.children; }
  get firstChild() { return this.children[0] ?? null; }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...kids) { for (const k of kids) { k.parent?.removeChild?.(k); k.parent = this; this.children.push(k); } }
  after(node) { const i = this.parent.children.indexOf(this); node.parent = this.parent; this.parent.children.splice(i + 1, 0, node); }
  removeChild(k) { this.children = this.children.filter((c) => c !== k); }
  remove() { this.parent?.removeChild(this); this.parent = null; }
  replaceChildren(...kids) { this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  // Events bubble to the parent chain like the page's (the form hears the Folder box's input).
  dispatchEvent(ev) {
    const e = Object.assign(ev, { target: ev.target ?? this, stopped: false, stopPropagation() { this.stopped = true; } });
    for (let n = this; n && !e.stopped; n = ev.bubbles ? n.parent : null) for (const f of n.listeners[ev.type] ?? []) f(e);
    return true;
  }
  click() {
    // A checkbox flips before its click handlers run and fires `change` after them, like the page's.
    if (this.type === 'checkbox') this.checked = !this.checked;
    this.dispatchEvent(new Event('click', { bubbles: true }));
    if (this.type === 'checkbox') this.dispatchEvent(new Event('change', { bubbles: true }));
    // A submit button submits its form.
    if (this.type === 'submit') this.closest('form')?.requestSubmit(this);
  }
  contains(n) { for (let x = n; x; x = x.parent) if (x === this) return true; return false; }
  closest(sel) { for (let n = this; n; n = n.parent) if (matches(n, sel)) return n; return null; }
  focus() { doc.activeElement = this; this.focused = true; }
  get label() { return this.getAttribute('aria-label'); }
  showModal() { this.open = true; }
  close() { this.open = false; this.dispatchEvent(new Event('close')); }
  // A method=dialog form's submit closes its dialog.
  requestSubmit() { this.dispatchEvent(new Event('submit')); this.parent?.close?.(); }
}

const ids = new Map();
const named = new Map();
const mk = (tag, id, parent, name) => {
  const e = new FakeEl(tag); e.id = id ?? ''; if (id) ids.set(id, e); if (name) { e.name = name; named.set(name, e); } parent?.append(e); return e;
};
const body = new FakeEl('body');
const doc = {
  body, activeElement: null, head: new FakeEl('head'),
  addEventListener() {}, querySelectorAll: () => [], querySelector: (s) => body.querySelector(s),
  getElementById: (id) => body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
};

const dialog = mk('dialog', 'launch-dialog', body);
const form = mk('form', 'launch-form', dialog);
form.elements = { namedItem: (n) => named.get(n) ?? null };
form.reset = () => {};
mk('h2', null, form);
const promptLabel = mk('label', null, form);
const queryField = mk('textarea', null, promptLabel, 'query');
const provider = mk('select', null, form, 'provider');
const modelField = mk('input', null, form, 'model');
const effort = mk('select', null, form, 'effort');
const row = mk('div', null, form); row.className = 'row';
const project = mk('select', null, row, 'project_id');
project.form = form;
const folder = mk('input', null, row, 'working_dir');
mk('button', 'launch-folder-pick', row);
mk('button', 'launch-model-pick', form);
const actions = mk('div', null, form); actions.className = 'actions';
const more = mk('button', 'launch-more', actions);
more.setAttribute('aria-expanded', 'false');
const launch = mk('button', null, actions); launch.type = 'submit'; launch.textContent = 'Launch';
const filter = mk('select', 'project-filter', body);
mk('div', 'toast', body);
for (const id of ['conn', 'session-list', 'list-empty', 'status']) mk('div', id, body);
provider.append(Object.assign(new FakeEl('option'), { _value: 'Claude' }));
effort.append(...['', 'low', 'medium', 'high'].map((v) => Object.assign(new FakeEl('option'), { _value: v })));

const calls = [];
const windowKeys = [];
globalThis.window = { setTimeout: () => 1, clearTimeout() {}, addEventListener: (t, f) => { if (t === 'keydown') windowKeys.push(f); }, removeEventListener() {}, localStorage: { getItem: () => null, setItem() {} } };
globalThis.document = doc;
globalThis.Element = FakeEl;
globalThis.Node = FakeEl;
globalThis.MutationObserver = class { observe() {} };
globalThis.Event = class { constructor(type, init = {}) { this.type = type; Object.assign(this, init); } };
globalThis.FormData = class { constructor(f) { this.f = f; } get(k) { return this.f.elements.namedItem(k)?.value ?? null; } };
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};
globalThis.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === 'launch_prompt' || cmd === 'launch_session' || cmd === 'launch') return { id: 'new-1' };
      return [];
    },
  },
};

const { state } = await import('../dist/state.js');
const { openLaunch, submitLaunch } = await import('../dist/launch.js');
const { initPrompts, sideHint } = await import('../dist/prompts.js');
initPrompts();

const P1 = '11111111-1111-4111-8111-111111111111';
state.projects = [{ id: P1, name: 'Alpha', path: '/work/alpha', color: '' }];
state.sessions = []; state.archived = []; state.selectedId = null; state.view = 'active';
filter.value = '';
await openLaunch();
await new Promise((r) => setImmediate(r));

// The page's MutationObserver redraws the summary when the dialog opens; the fake has none, so a change event does it.
form.dispatchEvent(new Event('change', { bubbles: true }));
const side = () => doc.getElementById('launch-side');
const summary = () => doc.getElementById('launch-summary')?.textContent ?? '';
const boxes = () => side()?.walk().filter((n) => n.type === 'checkbox') ?? [];
const buttons = () => side()?.walk().filter((n) => n.tag === 'button') ?? [];
const openSide = () => { if (!side()) more.click(); };
const key = (k, target = doc.activeElement) => {
  const ev = { key: k, target, ctrlKey: false, defaultPrevented: false, isComposing: false, preventDefault() { this.defaultPrevented = true; }, stopImmediatePropagation() { this.stopped = true; } };
  for (const f of windowKeys) if (!ev.stopped) f(ev);
  return ev;
};

// ---------- the button ----------

test('More options is a button with aria-expanded, closed to begin with, and the settings side does not exist yet', () => {
  assert.equal(more.tag, 'button');
  assert.equal(more.getAttribute('aria-expanded'), 'false');
  assert.equal(more.getAttribute('aria-controls') ?? 'launch-side', 'launch-side');
  assert.equal(side(), null);
});

test('pressing it shows the settings side and hides only the prompt box; pressing it again brings the prompt back with the draft', () => {
  queryField.value = 'fix the flaky test';
  more.click();
  assert.equal(more.getAttribute('aria-expanded'), 'true');
  assert.ok(side(), 'the side exists');
  assert.equal(promptLabel.hidden, true);
  assert.equal(side().id, 'launch-side', 'aria-controls points at it');
  assert.match(side().textContent, /Launch settings/);
  more.click();
  assert.equal(more.getAttribute('aria-expanded'), 'false');
  assert.equal(side(), null);
  assert.equal(promptLabel.hidden, false);
  assert.equal(queryField.value, 'fix the flaky test', 'the draft is kept');
  assert.equal(doc.activeElement, queryField, 'focus goes back to the prompt');
});

test('Esc on a box inside the side goes back to the prompt instead of closing the dialog', () => {
  more.click();
  const box = boxes()[0];
  box.focus();
  const ev = key('Escape', box);
  assert.equal(ev.defaultPrevented, true, 'the native close is prevented');
  assert.equal(side(), null);
  assert.equal(more.getAttribute('aria-expanded'), 'false');
});

// ---------- the real controls ----------

test('the side has a worktree checkbox with a name; ticking it makes the summary say Own git worktree: yes', () => {
  assert.match(summary(), /Own git worktree: no/);
  more.click();
  const worktree = boxes()[0];
  assert.match(worktree.parent.textContent, /Run in its own git worktree/);
  assert.equal(worktree.checked, false);
  worktree.focus(); // a keyboard user is on the box when they press Space
  worktree.click();
  assert.match(summary(), /Own git worktree: yes/);
  assert.equal(boxes()[0].checked, true, 'redrawn ticked');
  assert.equal(doc.activeElement, boxes()[0], 'focus stays on the box after the redraw');
});

test('the manager checkbox needs a project (it says so and stays unticked), then appoints once one is chosen', () => {
  project.value = '';
  project.dispatchEvent(new Event('change', { bubbles: true }));
  openSide();
  const manager = boxes()[1];
  assert.match(manager.parent.textContent, /choose a project first/);
  manager.click();
  assert.equal(boxes()[1].checked, false, 'no project: it stays off');
  project.value = P1;
  project.dispatchEvent(new Event('change', { bubbles: true }));
  boxes()[1].click();
  assert.equal(boxes()[1].checked, true);
  assert.ok(buttons().some((b) => b.label === 'Next scope'), 'scope and policy buttons appear');
  assert.ok(buttons().some((b) => b.label === 'Next grant preset'));
  boxes()[1].click();
  assert.equal(boxes()[1].checked, false);
  assert.ok(!buttons().some((b) => b.label === 'Next scope'));
});

test('model and effort have buttons with accessible names', () => {
  openSide();
  const names = buttons().map((b) => b.label);
  for (const n of ['Pick the model for this launch', 'Lower the effort', 'Raise the effort']) assert.ok(names.includes(n), n);
  const before = effort.value;
  buttons().find((b) => b.label === 'Raise the effort').click();
  assert.notEqual(effort.value, before);
});

test('every button and box in the side is a Tab stop', () => {
  openSide();
  for (const n of [...boxes(), ...buttons()]) assert.ok(n.tabIndex >= 0, n.label ?? n.type);
});

test('the hint names buttons with the Vim keys off and the keys with them on', () => {
  const off = sideHint(false);
  assert.match(off, /Tick a box or press a button/);
  assert.match(off, /More options/);
  assert.doesNotMatch(off, /j\/k/);
  const on = sideHint(true);
  assert.match(on, /j\/k move/);
  assert.match(on, /Ctrl-O, Tab or Esc back/);
});

// ---------- launching ----------

test('Launch after ticking the worktree box sends launch_prompt with sandbox: true', async () => {
  // A fresh side, worktree ticked.
  openSide();
  if (!boxes()[0].checked) boxes()[0].click();
  queryField.value = 'do the thing';
  project.value = P1;
  folder.value = '/work/alpha';
  calls.length = 0;
  await submitLaunch({ preventDefault() {} });
  const sent = calls.find((c) => c.cmd === 'launch_prompt');
  assert.ok(sent, `launch_prompt not called: ${JSON.stringify(calls.map((c) => c.cmd))}`);
  assert.equal(sent.args.request.sandbox, true);
  assert.equal(sent.args.request.query, 'do the thing');
  assert.equal(more.getAttribute('aria-expanded'), 'false', 'the side is closed after a launch');
});
