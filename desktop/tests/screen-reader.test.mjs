// Screen-reader basics (local #245): toasts are spoken and stay above open dialogs, every dialog has
// an accessible name, opening a session does not read its history aloud, and a session that starts
// waiting for you is announced once. The fake DOM follows tab-stops.test.mjs (it has replaceChildren
// and querySelector, which the toast needs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

/** One simple selector: tag, .class, [attr] or [attr="v"]; enough for TAB_STOP_SELECTOR and the modules here. */
function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel.trim());
  if (!m || n.tag.startsWith('#')) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name, val] = /^\[([\w-]+)(?:="([^"]*)")?\]$/.exec(a) ?? [];
    if (name === 'open') { if (!n.open) return false; continue; }
    const v = n.getAttribute(name);
    if (v === null || (val !== undefined && v !== val)) return false;
  }
  return true;
}
const matchesAny = (n, selector) => selector.split(',').some((s) => matchesSimple(n, s.trim().split(/\s+/).at(-1)));

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.scrollTop = 0; this.scrollHeight = 0; this.clientHeight = 0; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.open = false;
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
      add(c) { this.toggle(c, true); }, remove(c) { this.toggle(c, false); }, contains(c) { return self.className.split(/\s+/).includes(c); },
    };
  }
  // Reflected attributes, as in a real DOM: tabIndex, type, href, id.
  get tabIndex() { const v = this.attrs.tabindex; return v === undefined ? -1 : Number(v); }
  set tabIndex(v) { this.attrs.tabindex = String(v); }
  get type() { return this.attrs.type ?? (this.tag === 'input' ? 'text' : this.tag === 'button' ? 'submit' : ''); }
  set type(v) { this.attrs.type = String(v); }
  get href() { return this.attrs.href ?? ''; }
  set href(v) { this.attrs.href = String(v); }
  get id() { return this.attrs.id ?? ''; }
  set id(v) { this.attrs.id = String(v); }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...kids) {
    for (const k of kids) {
      if (typeof k === 'string') { this.append(Object.assign(new FakeEl('#text'), { _text: k })); continue; }
      if (k.tag === '#frag') { this.append(...k.children); continue; }
      k.parent = this; this.children.push(k);
    }
  }
  prepend(...kids) { const rest = this.children; this.children = []; this.append(...kids); this.children.push(...rest); }
  replaceChildren(...kids) { this._text = ''; this.children = []; this.append(...kids); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); this.parent = null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  hasAttribute(k) { return k in this.attrs; }
  removeAttribute(k) { delete this.attrs[k]; }
  toggleAttribute(k, on) { if (on ?? !(k in this.attrs)) this.attrs[k] = ''; else delete this.attrs[k]; }
  matches(sel) { return matchesAny(this, sel); }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matchesAny(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  removeEventListener() {}
  fire(t, ev = {}) { const e = { target: this, currentTarget: this, preventDefault() { e.prevented = true; }, stopPropagation() {}, ...ev }; for (const f of this.listeners[t] ?? []) f(e); return e; }
  closest(sel) { for (let n = this; n; n = n.parent) if (!n.tag.startsWith('#') && matchesAny(n, sel)) return n; return null; }
  contains(n) { for (; n; n = n.parent) if (n === this) return true; return false; }
  focus() { doc.activeElement = this; }
  blur() {}
  select() {}
  scrollIntoView() {}
  // No layout: the conversation pane reads its scroll position and rectangles (local #306); zeros mean "at the bottom".
  getBoundingClientRect() { return { top: 0, bottom: 0, height: 0 }; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  showModal() { this.open = true; }
  close() { if (!this.open) return; this.open = false; this.onclose?.(); this.fire('close'); }
}

const body = new FakeEl('body');
const doc = {
  documentElement: new FakeEl('html'), title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener() {}, removeEventListener() {},
  contains: () => true,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
const toast = new FakeEl('div'); toast.id = 'toast'; toast.hidden = true; body.append(toast);
const events = new FakeEl('div'); events.id = 'events'; events.setAttribute('role', 'log'); events.setAttribute('aria-live', 'polite'); body.append(events);
const realSetInterval = globalThis.setInterval;
globalThis.setInterval = (...a) => realSetInterval(...a).unref();
globalThis.window = globalThis;
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
globalThis.addEventListener ??= () => {};
globalThis.document = doc;
globalThis.navigator ??= { platform: 'MacIntel', userAgent: 'Macintosh' };


globalThis.__TAURI__ = { core: { invoke: async () => ({}) } };
const a11y = await import('../dist/a11y.js');
const dom = await import('../dist/dom.js');
const approvals = await import('../dist/approvals.js');
const model = await import('../dist/approvals-model.js');
const { state } = await import('../dist/state.js');

const tick = () => new Promise((r) => setTimeout(r, 0));
const html = readFileSync(here('../static/index.html'), 'utf8');
const toasts = [];
dom.onToast((msg, ok) => toasts.push({ msg, ok }));

function make(tag, attrs = {}, kids = []) {
  const e = new FakeEl(tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  e.append(...kids);
  return e;
}

/** The accessible name of a dialog: aria-label, else the text of the element aria-labelledby points at. */
function dialogName(dlg) {
  const label = dlg.getAttribute('aria-label');
  if (label) return label.trim();
  const ids = (dlg.getAttribute('aria-labelledby') ?? '').split(/\s+/).filter(Boolean);
  const named = ids.map((id) => dlg.walk().find((n) => n.id === id)?.textContent.trim() ?? '').filter(Boolean);
  return named.join(' ');
}

// ---------- toasts ----------

test('the toast is a live region: status and polite for a confirmation, alert and assertive for an error', () => {
  assert.match(html, /<div id="toast"[^>]*popover="manual"/);
  assert.match(html, /<div id="toast"[^>]*role="status"[^>]*aria-live="polite"/);
  dom.toast('Answer sent', true);
  assert.equal(toast.getAttribute('role'), 'status');
  assert.equal(toast.getAttribute('aria-live'), 'polite');
  assert.equal(toast.textContent, 'Answer sent');
  assert.equal(toast.hidden, false);
  dom.toast('daemon error -32000: boom');
  assert.equal(toast.getAttribute('role'), 'alert');
  assert.equal(toast.getAttribute('aria-live'), 'assertive');
  assert.ok(toast.textContent.length > 0 && toast.hidden === false);
  dom.toast('Launched', true);
  assert.equal(toast.getAttribute('role'), 'status');
  assert.deepEqual(toasts.slice(-1)[0], { msg: 'Launched', ok: true });
});

test('with a modal dialog open the toast moves into it (the page behind is inert) and is shown as a popover', () => {
  const calls = [];
  toast.showPopover = () => calls.push('show');
  toast.hidePopover = () => calls.push('hide');
  const dlg = make('dialog'); body.append(dlg); dlg.showModal();
  dom.toast('Theme: Latte', true);
  assert.equal(toast.parent, dlg, 'a child of the open dialog: the only place the inert page leaves reachable');
  assert.deepEqual(calls, ['hide', 'show'], 'lifted above the dialog content');
  assert.equal(toast.hidden, false);
  delete toast.showPopover; delete toast.hidePopover;
  dlg.close(); dlg.remove();
  assert.equal(toast.parent, body, 'back on the page when the dialog closes');
  assert.equal(toast.hidden, true);
});

test('without the popover API the toast still moves into the open dialog and back; with two dialogs open it takes the topmost', () => {
  const dlg = make('dialog'); body.append(dlg); dlg.showModal();
  dom.toast('Job enabled', true);
  assert.equal(toast.parent, dlg);
  assert.equal(toast.hidden, false);
  const top = make('dialog'); body.append(top); top.showModal();
  dom.toast('Job disabled', true);
  assert.equal(toast.parent, top);
  top.close(); top.remove();
  assert.equal(toast.parent, body);
  dlg.close(); dlg.remove();
  assert.equal(toast.parent, body, 'a close of the lower dialog does not move a toast that already left it');
  dom.toast('Back on the page', true);
  assert.equal(toast.parent, body);
});

// ---------- dialog names ----------

test('every <dialog> in the static page is named by aria-label or by a heading it points at', () => {
  const tags = [...html.matchAll(/<dialog\b[^>]*>/g)].map((m) => m[0]);
  assert.ok(tags.length >= 3, `${tags.length} dialogs`);
  for (const tag of tags) {
    const label = /aria-label="([^"]+)"/.exec(tag)?.[1];
    const by = /aria-labelledby="([^"]+)"/.exec(tag)?.[1];
    assert.ok(label || by, `unnamed dialog: ${tag}`);
    if (by) assert.match(html, new RegExp(`<h[1-6] id="${by}">[^<]+</h[1-6]>`), `${tag}: no heading ${by} with text`);
  }
});

test('nameDialog names a dialog from its first non-empty heading, keeps a name the code set, and renames a reused dialog', () => {
  const dlg = make('dialog', { id: 'trash-dialog' }, [make('div', {}, [make('h2', {}, [])]), make('h3', {}, ['Trash'])]);
  assert.equal(a11y.nameDialog(dlg), true);
  assert.equal(dialogName(dlg), 'Trash');
  // The same element rebuilt with another title (dialogEl reuses its id).
  dlg.replaceChildren(make('h2', {}, ['Recent sessions']));
  a11y.nameDialog(dlg);
  assert.equal(dialogName(dlg), 'Recent sessions');
  const labelled = make('dialog', { 'aria-label': 'Command palette' }, [make('h2', {}, ['Ignored'])]);
  assert.equal(a11y.nameDialog(labelled), true);
  assert.equal(labelled.getAttribute('aria-labelledby'), null);
  const chosen = make('dialog', { 'aria-labelledby': 'mine' }, [make('h2', { id: 'mine' }, ['Mine']), make('h2', {}, ['Other'])]);
  a11y.nameDialog(chosen);
  assert.equal(chosen.getAttribute('aria-labelledby'), 'mine');
  assert.equal(a11y.nameDialog(make('dialog', {}, [make('p', {}, ['No heading'])])), false);
});

test('installDialogNames names a dialog the moment it opens, and one that gains its heading while open', () => {
  let observed = null;
  class FakeObserver {
    constructor(cb) { this.cb = cb; }
    observe(target, opts) { observed = { target, opts, cb: this.cb }; }
  }
  const page = make('body');
  assert.equal(a11y.installDialogNames(page, FakeObserver), true);
  assert.equal(observed.opts.attributeFilter[0], 'open');
  const dlg = make('dialog', { id: 'scheduled' }, [make('h2', {}, ['Scheduled jobs'])]);
  page.append(dlg);
  observed.cb([{ type: 'childList', target: page, addedNodes: [dlg] }]);
  assert.equal(dialogName(dlg), '', 'closed dialogs are left until they open');
  dlg.showModal();
  observed.cb([{ type: 'attributes', target: dlg, addedNodes: [] }]);
  assert.equal(dialogName(dlg), 'Scheduled jobs');
  const late = make('dialog', { id: 'late' }); page.append(late); late.showModal();
  observed.cb([{ type: 'attributes', target: late, addedNodes: [] }]);
  assert.equal(dialogName(late), '');
  const h = make('h3', {}, ['Notifications']); late.append(h);
  observed.cb([{ type: 'childList', target: late, addedNodes: [h] }]);
  assert.equal(dialogName(late), 'Notifications');
  assert.equal(a11y.installDialogNames(make('body'), undefined), false);
  const main = readFileSync(here('../src/main.ts'), 'utf8');
  assert.match(main, /installDialogNames\(document\.body\)/);
});

test('a confirm dialog is an alertdialog named by its title (or by its message when it has no title)', async () => {
  const done = approvals.confirmDialog('Stop every running session?', 'Stop all', { title: 'Emergency stop' });
  const dlg = body.children.filter((n) => n.tag === 'dialog' && n.className === 'confirm').at(-1);
  assert.equal(dlg.getAttribute('role'), 'alertdialog');
  assert.equal(dialogName(dlg), 'Emergency stop');
  assert.equal(dlg.walk().find((n) => n.id === dlg.getAttribute('aria-describedby')).textContent, 'Stop every running session?');
  dlg.close();
  assert.equal(await done, false);
  const bare = approvals.confirmDialog('Archive this session?', 'Archive');
  const dlg2 = body.children.filter((n) => n.tag === 'dialog' && n.className === 'confirm').at(-1);
  assert.equal(dialogName(dlg2), 'Archive this session?');
  assert.equal(dlg2.getAttribute('role'), 'alertdialog');
  dlg2.close();
  await bare;
});

test('the question dialog and the theme picker are named once they open', async () => {
  const lifecycle = await import('../dist/actions/lifecycle.js');
  const themes = await import('../dist/themes.js');
  const now = '2026-10-20T12:00:00.000000000Z';
  state.sessions = [{
    id: '00000000-0000-4000-8000-000000000001', title: 'Waiting worker', status: 'WaitingApproval', provider: 'Claude',
    working_dir: '/tmp', created_at: now, updated_at: now, parent_id: null, kind: 'Session',
    pending_question: { questions: [{ question: 'Which checks?', header: 'Checks', multiSelect: false, options: [{ label: 'tests', description: '' }] }] },
  }];
  state.selectedId = state.sessions[0].id;
  lifecycle.openQuestionModal();
  await tick();
  const q = body.walk().find((n) => n.tag === 'dialog' && n.id === 'question-dialog');
  assert.ok(q, 'the question dialog rendered');
  a11y.nameDialog(q); // what the boot observer does when the dialog opens
  assert.notEqual(dialogName(q), '');
  q.close();
  doc.documentElement.style = { setProperty() {}, removeProperty() {} };
  themes.openThemePicker();
  const t = body.walk().find((n) => n.tag === 'dialog' && n.id === 'theme-picker');
  assert.ok(t, 'the theme picker rendered');
  a11y.nameDialog(t);
  assert.equal(dialogName(t), 'Choose theme');
  t.close();
});

// ---------- the conversation ----------

test('#events is a polite log that announces additions only', () => {
  assert.match(html, /<div id="events"[^>]*role="log"/);
  assert.match(html, /<div id="events"[^>]*aria-live="polite"/);
  assert.match(html, /<div id="events"[^>]*aria-relevant="additions"/);
  assert.doesNotMatch(html, /<div id="events"[^>]*role="(?!log)/);
});

test('rebuilding the whole conversation is silent: busy and not live while it is replaced, live again after', async () => {
  const seen = [];
  events.setAttribute('aria-live', 'polite');
  a11y.quietRebuild(events, () => seen.push({ busy: events.getAttribute('aria-busy'), live: events.getAttribute('aria-live') }));
  assert.deepEqual(seen, [{ busy: 'true', live: 'off' }]);
  assert.equal(events.getAttribute('aria-busy'), 'true', 'still silent right after the rebuild');
  await new Promise((r) => setTimeout(r, a11y.REBUILD_QUIET_MS + 80));
  assert.equal(events.getAttribute('aria-busy'), null);
  assert.equal(events.getAttribute('aria-live'), 'polite');
});

test('opening a session puts its history in a log that is not live; a later event is announced', async () => {
  const conversation = await import('../dist/conversation.js');
  const at = (n) => `2026-10-20T12:00:0${n}.000000000Z`;
  const ev = (sequence, content) => ({ id: sequence, session_id: 's1', sequence, event_type: 'assistant_message', content, created_at: at(sequence), tool_name: null });
  state.events = [ev(1, 'first reply'), ev(2, 'second reply')];
  state.renderedCount = 0; state.renderedFirstSeq = undefined; state.renderLimit = 200;
  const states = [];
  const replace = events.replaceChildren.bind(events);
  events.replaceChildren = (...kids) => { states.push({ busy: events.getAttribute('aria-busy'), live: events.getAttribute('aria-live'), n: kids.length }); replace(...kids); };
  conversation.renderEvents();
  assert.deepEqual(states.map((s) => [s.busy, s.live]), [['true', 'off']], 'the history went in while the log was not live');
  assert.ok(states[0].n >= 2);
  await new Promise((r) => setTimeout(r, a11y.REBUILD_QUIET_MS + 80));
  assert.equal(events.getAttribute('aria-live'), 'polite');
  assert.equal(events.getAttribute('aria-busy'), null);
  // A new event appended while viewing is a plain append into the live log.
  state.events = [...state.events, ev(3, 'third reply')];
  conversation.renderEvents();
  assert.equal(states.length, 1, 'appending does not rebuild');
  assert.equal(events.getAttribute('aria-live'), 'polite');
  events.replaceChildren = replace;
});

// ---------- a session that starts waiting for you ----------

test('newlyNeedingYou: the first refresh only seeds, then a session entering the attention set is new once', () => {
  const s = (id, status, q = []) => ({ id, status, pending_question: q.length ? { questions: q } : null });
  const first = model.newlyNeedingYou(null, [s('a', 'WaitingApproval'), s('b', 'Running')]);
  assert.deepEqual([...first.now], ['a']);
  assert.deepEqual(first.fresh, []);
  const second = model.newlyNeedingYou(first.now, [s('a', 'WaitingApproval'), s('b', 'WaitingApproval'), s('c', 'Running', [{ question: 'q?' }])]);
  assert.deepEqual(second.fresh.map((x) => x.id), ['b', 'c']);
  const third = model.newlyNeedingYou(second.now, [s('a', 'Running'), s('b', 'WaitingApproval'), s('c', 'Running', [{ question: 'q?' }])]);
  assert.deepEqual(third.fresh, []);
  const back = model.newlyNeedingYou(third.now, [s('a', 'WaitingApproval'), s('b', 'WaitingApproval')]);
  assert.deepEqual(back.fresh.map((x) => x.id), ['a'], 'leaving and coming back is new again');
  assert.equal(model.waitingAnnouncement([]), '');
  assert.equal(model.waitingAnnouncement(['Fix login']), 'Fix login is waiting for you');
  assert.equal(model.waitingAnnouncement(['A', 'B']), '2 sessions are waiting for you: A, B');
  assert.equal(model.waitingAnnouncement(['A', 'B', 'C', 'D', 'E']), '5 sessions are waiting for you: A, B, C and 2 more');
});

test('a session going Running to WaitingApproval writes one "is waiting for you" toast, and a second refresh does not repeat it', async () => {
  const lines = [];
  approvals.initApprovals({ toast: (msg, ok) => { lines.push({ msg, ok }); dom.toast(msg, ok); }, refresh() {}, selected: () => undefined });
  const now = '2026-10-20T12:00:00.000000000Z';
  const sess = (status) => ({
    id: '00000000-0000-4000-8000-0000000000aa', title: 'Fix login', query: 'x', status, provider: 'Claude', working_dir: '/tmp',
    created_at: now, updated_at: now, parent_id: null, kind: 'Session', pending_question: null,
  });
  approvals.updateAttention([sess('Running')]); // the first refresh seeds
  assert.deepEqual(lines, []);
  approvals.updateAttention([sess('WaitingApproval')]);
  assert.deepEqual(lines, [{ msg: 'Fix login is waiting for you', ok: true }]);
  assert.equal(toast.textContent, 'Fix login is waiting for you');
  assert.equal(toast.getAttribute('role'), 'status');
  approvals.updateAttention([sess('WaitingApproval')]);
  assert.equal(lines.length, 1, 'a second refresh does not repeat it');
  approvals.updateAttention([sess('Running')]);
  approvals.updateAttention([sess('WaitingApproval')]);
  assert.equal(lines.length, 2, 'waiting again is announced again');
});
