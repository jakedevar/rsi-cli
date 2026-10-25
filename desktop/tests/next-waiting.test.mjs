// Local #312: after an answer the session pane offers the next session that waits for you ("Next waiting (N)"), in the session
// list's order; taking it opens that session with its question's first option focused; with none left it says so, and #297's
// focus rules hold when the offer is ignored. The real approvals module runs against the small fake document of
// focus-after-actions.test.mjs (focus fixup included).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

// ---------- the fake document ----------

class N {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.children = []; this.parentNode = null; this.className = ''; this.id = '';
    this.hidden = false; this.disabled = false; this.dataset = {}; this.attrs = {}; this.style = {}; this.listeners = {};
    this._text = ''; this.tabIndex = -1; this.title = ''; this.value = ''; this.checked = false; this.open = false;
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { for (const c of this.children) c.parentNode = null; this.children = []; this._text = String(v); }
  get parentElement() { return this.parentNode; }
  get isConnected() { for (let n = this; n; n = n.parentNode) if (n === doc.body) return true; return false; }
  get classList() {
    const self = this;
    return {
      add: (c) => { if (!self.has(c)) self.className = `${self.className} ${c}`.trim(); },
      remove: (c) => { self.className = self.className.split(/\s+/).filter((x) => x && x !== c).join(' '); },
      toggle: (c, on) => { if (on ?? !self.has(c)) self.classList.add(c); else self.classList.remove(c); },
      contains: (c) => self.has(c),
    };
  }
  has(c) { return this.className.split(/\s+/).includes(c); }
  append(...kids) { for (const k of kids) { k.parentNode?.children.splice(k.parentNode.children.indexOf(k), 1); k.parentNode = this; this.children.push(k); } }
  replaceChildren(...kids) { for (const c of this.children) c.parentNode = null; this.children = []; this._text = ''; this.append(...kids); }
  remove() { this.parentNode?.children.splice(this.parentNode.children.indexOf(this), 1); this.parentNode = null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  contains(n) { for (let x = n; x; x = x.parentNode) if (x === this) return true; return false; }
  closest(sel) { for (let n = this; n; n = n.parentNode) if (n.matches(sel)) return n; return null; }
  matches(sel) {
    return sel.split(',').some((part) => {
      const m = /^\s*([a-z0-9]*)((?:\.[\w-]+)*)(?:\[(\w+)\])?\s*$/i.exec(part);
      if (!m) return false;
      const [, tag, cls, attr] = m;
      if (tag && this.tagName !== tag.toUpperCase()) return false;
      if (cls && !cls.split('.').filter(Boolean).every((c) => this.has(c))) return false;
      if (attr && !this[attr] && this.attrs[attr] === undefined) return false;
      return true;
    });
  }
  querySelectorAll(sel) { const out = []; const walk = (n) => { for (const c of n.children) { if (c.matches(sel)) out.push(c); walk(c); } }; walk(this); return out; }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  dispatchEvent(ev) {
    const e = Object.assign(ev, { target: ev.target ?? this, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } });
    for (let n = this; n; n = n.parentNode) for (const f of n.listeners[ev.type] ?? []) f(e);
    return e;
  }
  hiddenChain() { for (let n = this; n; n = n.parentNode) if (n.hidden) return true; return false; }
  focus() { if (this.isConnected && !this.disabled && !this.hiddenChain()) focused = this; }
  blur() { if (focused === this) focused = null; }
  click() { return this.dispatchEvent({ type: 'click', detail: 0 }); }
}
class D extends N {}

const findById = (root, id) => { for (const c of root.children) { if (c.id === id) return c; const r = findById(c, id); if (r) return r; } return null; };
let focused = null;
const byId = new Map();
const docListeners = {};
const doc = {
  body: new N('body'),
  createElement: (tag) => (tag === 'dialog' ? new D(tag) : new N(tag)),
  createTextNode: (t) => Object.assign(new N('#text'), { _text: t }),
  getElementById: (id) => byId.get(id) ?? findById(doc.body, id),
  querySelector: (s) => doc.body.querySelector(s),
  querySelectorAll: (s) => doc.body.querySelectorAll(s),
  addEventListener: (t, f) => { (docListeners[t] ??= []).push(f); },
  // Browser focus fixup: a focused element that is removed, hidden or disabled is no longer the active element.
  get activeElement() { return focused && focused.isConnected && !focused.hiddenChain() && !focused.disabled ? focused : doc.body; },
};
const timers = [];
globalThis.document = doc;
globalThis.HTMLElement = N;
globalThis.HTMLDialogElement = D;
globalThis.window = { setTimeout: (f) => { timers.push(f); return timers.length; }, clearTimeout() {}, matchMedia: () => ({ matches: false }) };
const calls = [];
globalThis.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === 'get_operator_pause') return { pause_level: 'none' };
      return null;
    },
  },
};

const settle = () => new Promise((r) => setTimeout(r, 20));


/** A fresh page: the elements approvals.ts looks up by id, with the next-waiting bar under the banner. */
function page() {
  byId.clear(); doc.body.replaceChildren(); focused = null; timers.length = 0; calls.length = 0;
  const mk = (tag, id, parent = doc.body, extra = {}) => {
    const e = doc.createElement(tag);
    e.id = id; Object.assign(e, extra); byId.set(id, e); parent.append(e); return e;
  };
  const list = mk('ul', 'session-list', doc.body, { tabIndex: 0 });
  const view = mk('section', 'session-view');
  const head = mk('div', 's-head', view);
  const actions = mk('div', 'head-actions', head);
  mk('button', 'hard-pause', actions);
  const banner = mk('div', 'approval-banner', view, { hidden: true });
  const bar = mk('div', 'next-waiting', view, { hidden: true });
  const text = mk('span', 'next-waiting-text', bar);
  const go = mk('button', 'next-waiting-go', bar, { hidden: true });
  const events = mk('div', 'events', view, { tabIndex: 0 });
  const message = mk('textarea', 'message', view);
  mk('div', 'toast');
  return { list, banner, bar, text, go, events, message };
}
const buttonNamed = (root, label) => root.querySelectorAll('button').find((b) => b.textContent === label);

const session = (n, over = {}) => ({ id: `s${n}`, title: `Session ${n}`, status: 'Running', provider: 'Claude', working_dir: '/w/rsi', git_branch: 'rsi/x', ...over });
const ask = (q) => ({ questions: [{ question: q, header: q, options: [{ label: 'Yes', description: '' }, { label: 'No', description: '' }] }] });
const waiting = (n) => session(n, { pending_question: ask(`Question ${n}?`) });

const { renderApproval, initApprovals } = await import('../dist/approvals.js');
const { toast } = await import('../dist/dom.js');
const { state } = await import('../dist/state.js');
const { nextWaiting, nextWaitingButton, NONE_WAITING } = await import('../dist/next-waiting-model.js');

// ---------- the model: the order is the list's ----------

test('next waiting is the one after the answered row in list order, wrapping, and the count leaves the answered one out', () => {
  const q = [waiting(1), waiting(2), waiting(3)];
  assert.deepEqual([nextWaiting(q, 's1').next.id, nextWaiting(q, 's1').left], ['s2', 2]);
  assert.deepEqual([nextWaiting(q, 's2').next.id, nextWaiting(q, 's2').left], ['s3', 2]);
  assert.deepEqual([nextWaiting(q, 's3').next.id, nextWaiting(q, 's3').left], ['s1', 2]);
});

test('an answered row that already left the queue: the first waiting row is next', () => {
  const q = [waiting(2), waiting(3)];
  assert.deepEqual([nextWaiting(q, 's1').next.id, nextWaiting(q, 's1').left], ['s2', 2]);
});

test('nothing else waiting: no next and a count of zero', () => {
  assert.deepEqual(nextWaiting([], 's1'), { next: null, left: 0 });
  assert.deepEqual(nextWaiting([waiting(1)], 's1'), { next: null, left: 0 });
  assert.equal(nextWaitingButton(3), 'Next waiting (3)');
});

// ---------- the pane ----------

/** Three waiting sessions in list order; `open` selects one the way a row click does and redraws the banner. */
function triage(p, order = ['1', '2', '3']) {
  state.view = 'active';
  const all = Object.fromEntries(order.map((n) => [n, waiting(n)]));
  let current = all[order[0]];
  const opened = [];
  initApprovals({
    toast: () => {},
    refresh: () => { renderApproval(current); },
    selected: () => current,
    waiting: () => order.map((n) => all[n]).filter((s) => s.pending_question),
    open: (id) => { opened.push(id); current = all[id.slice(1)]; renderApproval(current); },
  });
  renderApproval(current);
  return {
    all, opened,
    answer: (via = 'Submit answer') => {
      if (via === 'Submit answer') p.banner.querySelector('input').checked = true;
      const btn = buttonNamed(p.banner, via);
      btn.focus();
      current.pending_question = undefined; // the daemon took the answer
      btn.click();
    },
  };
}

test('after Submit answer the pane offers "Next waiting (2)" and names the next session; focus is on that button', async () => {
  const p = page();
  const t = triage(p);
  t.answer();
  await settle();
  assert.equal(p.banner.hidden, true);
  assert.equal(p.bar.hidden, false);
  assert.equal(p.go.hidden, false);
  assert.equal(p.go.textContent, 'Next waiting (2)');
  assert.equal(p.text.textContent, 'Session 2 is waiting for you');
  assert.equal(doc.activeElement, p.go, 'Enter on the focused control takes it');
});

test('Let the agent decide offers the next session too', async () => {
  const p = page();
  const t = triage(p);
  t.answer('Let the agent decide');
  await settle();
  assert.equal(p.go.textContent, 'Next waiting (2)');
});

test('taking it opens that session and focuses the first option of its question', async () => {
  const p = page();
  const t = triage(p);
  t.answer();
  await settle();
  p.go.click();
  assert.deepEqual(t.opened, ['s2']);
  assert.equal(p.banner.hidden, false);
  assert.equal(p.bar.hidden, true, 'the offer goes once taken');
  assert.equal(doc.activeElement, p.banner.querySelector('input'), 'the first option');
});

test('three waiting sessions answered in a row: after the first, 2 clicks each (option, Submit answer)', async () => {
  const p = page();
  const t = triage(p);
  const seen = [];
  let clicks = 0;
  for (const n of ['1', '2', '3']) {
    const radio = p.banner.querySelector('input');
    radio.checked = true; clicks += 1; // click an option
    const send = buttonNamed(p.banner, 'Submit answer');
    send.focus();
    t.all[n].pending_question = undefined;
    clicks += 1; send.click(); // click Submit answer
    await settle();
    seen.push(p.go.hidden ? NONE_WAITING : p.go.textContent);
    if (!p.go.hidden) p.go.click(); // the taking is Enter on the focused button, not a click
  }
  assert.equal(clicks, 6, '2 clicks per question');
  assert.deepEqual(seen, ['Next waiting (2)', 'Next waiting (1)', NONE_WAITING]);
  assert.deepEqual(t.opened, ['s2', 's3']);
});

test('the last answer says "No more sessions waiting", has no button, and focus stays on the message box (#297)', async () => {
  const p = page();
  const t = triage(p, ['1']);
  t.answer();
  await settle();
  assert.equal(p.bar.hidden, false);
  assert.equal(p.text.textContent, 'No more sessions waiting');
  assert.equal(p.go.hidden, true);
  assert.equal(doc.activeElement, p.message);
});

test('the offer follows the list: a session that stops waiting leaves the count; one that starts waiting joins it', async () => {
  const p = page();
  const t = triage(p);
  t.answer();
  await settle();
  assert.equal(p.go.textContent, 'Next waiting (2)');
  t.all['3'].pending_question = undefined;
  renderApproval(t.all['1']);
  assert.equal(p.go.textContent, 'Next waiting (1)');
  assert.equal(p.text.textContent, 'Session 2 is waiting for you');
});

test('ignoring the offer: opening another session clears it, and a click elsewhere keeps the pane as #297 left it', async () => {
  const p = page();
  const t = triage(p);
  t.answer();
  await settle();
  p.message.focus();
  renderApproval(t.all['1']);
  assert.equal(doc.activeElement, p.message, 'a redraw does not pull focus back to the offer');
  renderApproval(t.all['3']);
  assert.equal(p.bar.hidden, true, 'another session is open: no offer');
  renderApproval(t.all['1']);
  assert.equal(p.bar.hidden, true, 'and it does not come back');
});

test('a pointer click (focus left on the page) still lands on the offer', async () => {
  const p = page();
  const t = triage(p);
  p.banner.querySelector('input').checked = true;
  t.all['1'].pending_question = undefined;
  buttonNamed(p.banner, 'Submit answer').click();
  await settle();
  assert.equal(p.go.textContent, 'Next waiting (2)');
  assert.equal(doc.activeElement, p.go);
});

// ---------- with archive-when-finished (local #308 x #312) ----------

test('an answer on a session marked to archive still offers Next waiting, and taking it works', async () => {
  const p = page();
  const t = triage(p);
  t.all['1'].pending_archive = true; // marked: archives when it finishes
  t.answer();
  await settle();
  assert.equal(p.bar.hidden, false);
  assert.equal(p.go.textContent, 'Next waiting (2)');
  assert.equal(p.text.textContent, 'Session 2 is waiting for you');
  p.go.click();
  assert.deepEqual(t.opened, ['s2']);
});

test('the marked session finishing and showing as Archived keeps the offer: Next waiting still works', async () => {
  const p = page();
  const t = triage(p);
  t.all['1'].pending_archive = true;
  t.answer();
  await settle();
  // The agent finished and the session was archived: the pane shows it read-only as Archived (local #309).
  t.all['1'].status = 'Archived';
  renderApproval(t.all['1']);
  assert.equal(p.bar.hidden, false);
  assert.equal(p.go.textContent, 'Next waiting (2)');
  p.go.click();
  assert.deepEqual(t.opened, ['s2']);
});
