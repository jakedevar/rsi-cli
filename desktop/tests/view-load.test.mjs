// Local #310: the shared frame every view that must read from rsid opens with (src/view-load.ts). The real module and dom.ts run
// against a small fake document: the frame is up in the same turn as the choice, "Loading…" is replaced in the same place by the
// view or by the daemon's message with Retry, and an answer that comes after the user left draws nothing.
import { test } from 'node:test';
import assert from 'node:assert/strict';

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
class D extends N {
  showModal() { this.open = true; }
  close() { if (!this.open) return; this.open = false; this.onclose?.(); this.dispatchEvent({ type: 'close' }); }
}

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


globalThis.__TAURI__ = { core: { invoke: async () => null } };

const { loadView, loadingView } = await import('../dist/view-load.js');

const reset = () => { byId.clear(); doc.body.replaceChildren(); focused = null; timers.length = 0; byId.set('toast', Object.assign(doc.createElement('div'), { id: 'toast' })); };
const frame = () => doc.getElementById('view-load-dialog');
const status = () => frame().querySelector('.view-load-status');
const buttonText = (t) => frame().querySelectorAll('button').find((b) => b.textContent.startsWith(t));
const tick = () => new Promise((r) => setTimeout(r, 0));

/** A read the test settles by hand. */
function deferred() {
  let resolve; let reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

/** loadView over one deferred read; counts reads and opens. */
function view(key = 'admin-schedules', d = deferred()) {
  const seen = { reads: 0, opened: [] };
  const p = loadView({
    key, title: 'Scheduled jobs', failed: 'Failed to load scheduled jobs',
    read: () => { seen.reads += 1; return d.promise; },
    open: (data) => { seen.opened.push(data); },
  });
  return { seen, p, d };
}

// ---------- loading ----------

test('the frame is open in the same turn as the choice, with Loading… busy, the heading, Close and a hidden Retry', () => {
  reset();
  const { seen } = view();
  assert.equal(frame().open, true, 'a dialog is up before any read has returned');
  assert.equal(frame().querySelector('h2').textContent, 'Scheduled jobs');
  assert.equal(status().textContent, 'Loading…');
  assert.equal(status().getAttribute('aria-busy'), 'true');
  assert.equal(status().getAttribute('role'), 'status');
  assert.equal(buttonText('Close').hidden, false);
  assert.equal(buttonText('Retry').hidden, true, 'Retry has its place from the start, so the frame does not change size');
  assert.equal(seen.reads, 1);
  assert.deepEqual(seen.opened, []);
  assert.equal(loadingView(), 'admin-schedules');
  assert.equal(doc.activeElement, frame(), 'the dialog has focus, so keys reach it');
});

// ---------- success ----------

test('when the read returns the frame closes and the view opens from what was read, once', async () => {
  reset();
  const { seen, p, d } = view();
  d.resolve({ jobs: [1, 2] });
  await p;
  assert.equal(frame().open, false);
  assert.deepEqual(seen.opened, [{ jobs: [1, 2] }]);
  assert.equal(loadingView(), null);
});

// ---------- refusal + Retry ----------

test('a refused read shows the daemon\'s message in the same place, with Retry focused; the view is not opened', async () => {
  reset();
  const { seen, p, d } = view();
  const place = status();
  d.reject(new Error('daemon error -32000: audit: this read is refused [audit_refused]'));
  await p;
  assert.equal(frame().open, true, 'the frame stays up');
  assert.equal(status(), place, 'the message replaces Loading… in the same element');
  assert.equal(status().getAttribute('role'), 'alert');
  assert.equal(status().getAttribute('aria-busy'), 'false');
  assert.match(status().textContent, /^Failed to load scheduled jobs: .*refus/i);
  assert.ok(status().querySelector('.err-text'), 'the plain words, with the raw error under Details');
  assert.equal(buttonText('Retry').hidden, false);
  assert.equal(doc.activeElement, buttonText('Retry'));
  assert.deepEqual(seen.opened, []);
});

test('Retry reads again: Loading… is back, and the next answer opens the view', async () => {
  reset();
  const d1 = deferred();
  const seen = { reads: 0, opened: [] };
  const reads = [d1];
  const p = loadView({ key: 'k', title: 'T', failed: 'Failed', read: () => { seen.reads += 1; return reads[seen.reads - 1].promise; }, open: (x) => { seen.opened.push(x); } });
  d1.reject(new Error('rsid is draining'));
  await p;
  const d2 = deferred();
  reads.push(d2);
  buttonText('Retry').click();
  assert.equal(seen.reads, 2);
  assert.equal(status().textContent, 'Loading…');
  assert.equal(buttonText('Retry').hidden, true);
  d2.resolve('rows');
  await tick(); await tick();
  assert.deepEqual(seen.opened, ['rows']);
  assert.equal(frame().open, false);
});

test('a second refusal after Retry shows the new message, still with Retry', async () => {
  reset();
  const ds = [deferred(), deferred()];
  let n = 0;
  const p = loadView({ key: 'k', title: 'T', failed: 'Failed', read: () => ds[n++].promise, open: () => {} });
  ds[0].reject(new Error('first'));
  await p;
  buttonText('Retry').click();
  ds[1].reject(new Error('second'));
  await tick(); await tick();
  assert.match(status().textContent, /second/);
  assert.equal(buttonText('Retry').hidden, false);
});

test('keys: r retries only after a failure, q closes', async () => {
  reset();
  const { seen, p, d } = view();
  frame().onkeydown({ key: 'r', preventDefault() {} });
  assert.equal(seen.reads, 1, 'r does nothing while loading');
  d.reject(new Error('no'));
  await p;
  frame().onkeydown({ key: 'r', preventDefault() {} });
  assert.equal(seen.reads, 2);
  frame().onkeydown({ key: 'q', preventDefault() {} });
  assert.equal(frame().open, false);
});

// ---------- stale answers ----------

test('closing the frame before the read returns: the late answer draws nothing', async () => {
  reset();
  const { seen, p, d } = view();
  buttonText('Close').click();
  assert.equal(frame().open, false);
  d.resolve('late');
  await p;
  assert.deepEqual(seen.opened, []);
  assert.equal(frame().open, false, 'the frame did not come back');
});

test('closing the frame before a read fails: the late error draws nothing', async () => {
  reset();
  const { p, d } = view();
  buttonText('Close').click();
  d.reject(new Error('late'));
  await p;
  assert.equal(frame().open, false);
});

test('choosing another view while one loads: the first answer is dropped, the second opens', async () => {
  reset();
  const a = view('a');
  const b = view('b');
  assert.equal(doc.body.querySelectorAll('dialog').length, 1, 'one frame, not two');
  assert.equal(loadingView(), 'b');
  a.d.resolve('from a');
  await a.p;
  assert.deepEqual(a.seen.opened, [], 'the view left behind does not draw');
  assert.equal(frame().open, true);
  b.d.resolve('from b');
  await b.p;
  assert.deepEqual(b.seen.opened, ['from b']);
  assert.deepEqual(a.seen.opened, []);
});

test('closing, then choosing the same view again: only the new read draws', async () => {
  reset();
  const first = view('a');
  buttonText('Close').click();
  const second = view('a');
  first.d.resolve('old');
  await first.p;
  assert.deepEqual(first.seen.opened, []);
  second.d.resolve('new');
  await second.p;
  assert.deepEqual(second.seen.opened, ['new']);
});

test('a Retry answer from before the user pressed Retry again is dropped', async () => {
  reset();
  const ds = [deferred(), deferred(), deferred()];
  let n = 0;
  const opened = [];
  const p = loadView({ key: 'k', title: 'T', failed: 'F', read: () => ds[n++].promise, open: (x) => { opened.push(x); } });
  ds[0].reject(new Error('x'));
  await p;
  buttonText('Retry').click();
  ds[1].reject(new Error('y'));
  await tick(); await tick();
  buttonText('Retry').click();
  ds[2].resolve('third');
  await tick(); await tick();
  assert.deepEqual(opened, ['third']);
});

// ---------- the same view chosen again ----------

test('choosing the same view again while it loads is the same load: one frame, one read', () => {
  reset();
  const d = deferred();
  const first = view('admin-schedules', d);
  const reads = first.seen.reads;
  const again = loadView({ key: 'admin-schedules', title: 'Scheduled jobs', failed: 'F', read: () => { first.seen.reads += 1; return d.promise; }, open: () => { first.seen.opened.push('dup'); } });
  assert.equal(first.seen.reads, reads, 'no second read');
  assert.equal(doc.body.querySelectorAll('dialog').length, 1);
  assert.equal(again, first.p, 'it joins the load');
});

test('the open callback throwing is not swallowed as a read failure', async () => {
  reset();
  const d = deferred();
  const p = loadView({ key: 'k', title: 'T', failed: 'F', read: () => d.promise, open: () => { throw new Error('boom'); } });
  d.resolve(1);
  await assert.rejects(p, /boom/);
});
