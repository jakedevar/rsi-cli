// Explicit tab stops (local #242): on macOS WKWebView with "Keyboard navigation" off, Tab skips
// every button, checkbox, radio, link and summary that has no tabindex. These tests fail when a
// rendered control of those kinds lacks tabindex="0", when a deliberate tabindex (a roving -1) is
// overwritten, when a text field, select or textarea is stamped, or when any positive tabindex
// appears. The fake DOM's tabIndex follows the attribute only (-1 without one), which is how the
// WebKit tab order treats these controls. A fake that renders toasts needs replaceChildren and
// querySelector, which this one has.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
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
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.open = false;
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
const toast = new FakeEl('div'); toast.id = 'toast'; body.append(toast);
const realSetInterval = globalThis.setInterval;
globalThis.setInterval = (...a) => realSetInterval(...a).unref();
globalThis.window = globalThis;
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
globalThis.addEventListener ??= () => {};
globalThis.document = doc;
globalThis.navigator ??= { platform: 'MacIntel', userAgent: 'Macintosh' };

const { TAB_STOP_SELECTOR, installTabStops, makeScrollRegion, stampTabStop, stampTabStops } = await import('../dist/tab-stops.js');
const dom = await import('../dist/dom.js');
const approvals = await import('../dist/approvals.js');

const tick = () => new Promise((r) => setTimeout(r, 0));

/** Every control WebKit would skip in `root` that is not a tab stop: [] when all are reachable. */
function missingTabStops(root) {
  return root.walk().filter((n) => !n.tag.startsWith('#') && n.matches(TAB_STOP_SELECTOR) && n.tabIndex !== 0)
    .map((n) => `${n.tag}${n.type && n.tag === 'input' ? `[type=${n.type}]` : ''} "${n.textContent.trim().slice(0, 30)}" tabindex=${n.getAttribute('tabindex')}`);
}

function make(tag, attrs = {}, kids = []) {
  const e = new FakeEl(tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  e.append(...kids);
  return e;
}

test('the stamp gives every button, link with href, checkbox, radio and summary tabindex 0', () => {
  const root = make('div', {}, [
    make('button', {}, ['Send']), make('a', { href: 'https://example.com' }, ['link']), make('input', { type: 'checkbox' }),
    make('input', { type: 'radio' }), make('details', {}, [make('summary', {}, ['Details'])]),
    make('button', { disabled: '' }, ['Disabled']),
  ]);
  assert.equal(stampTabStops(root), 6);
  assert.deepEqual(missingTabStops(root), []);
  assert.ok(root.walk().filter((n) => n.matches?.(TAB_STOP_SELECTOR)).every((n) => n.getAttribute('tabindex') === '0'));
});

test('a deliberate tabindex is left alone, and fields WebKit already tabs to are never stamped', () => {
  const roving = make('button', { tabindex: '-1' }, ['Row action']);
  const kept = make('a', { href: '#x', tabindex: '0' });
  const text = make('input', { type: 'text' });
  const bare = make('input');
  const area = make('textarea');
  const select = make('select');
  const anchor = make('a', {}, ['no href']);
  const div = make('div');
  const root = make('div', {}, [roving, kept, text, bare, area, select, anchor, div]);
  assert.equal(stampTabStops(root), 0);
  assert.equal(roving.getAttribute('tabindex'), '-1');
  assert.equal(kept.getAttribute('tabindex'), '0');
  for (const n of [text, bare, area, select, anchor, div]) assert.equal(n.getAttribute('tabindex'), null, n.tag);
  assert.equal(stampTabStop(make('input', { type: 'range' })), false);
});

test('el() makes buttons and summaries tab stops as they are created; other tags get no tabindex', () => {
  assert.equal(dom.el('button', 'btn', 'New session').tabIndex, 0);
  assert.equal(dom.el('summary', undefined, 'Details').tabIndex, 0);
  assert.equal(dom.el('div').getAttribute('tabindex'), null);
  assert.equal(dom.el('input').getAttribute('tabindex'), null);
  assert.equal(dom.el('textarea').getAttribute('tabindex'), null);
});

test('installTabStops stamps the page at once, then every control added or retyped later', () => {
  const records = [];
  let observed = null;
  class FakeObserver {
    constructor(cb) { this.cb = cb; }
    observe(target, opts) { observed = { target, opts, cb: this.cb }; }
  }
  const page = make('div', {}, [make('button', {}, ['Issues']), make('input', { type: 'checkbox', id: 'vim-keys' })]);
  assert.equal(installTabStops(page, FakeObserver), true);
  assert.deepEqual(missingTabStops(page), []);
  assert.equal(observed.target, page);
  assert.deepEqual(observed.opts.attributeFilter, ['type', 'href']);
  assert.equal(observed.opts.subtree, true);
  // A rendered markdown link and a dialog appended later.
  const dlg = make('dialog', {}, [make('p', {}, [make('a', { href: 'https://example.com' }, ['docs'])]), make('button', {}, ['OK'])]);
  page.append(dlg);
  records.push({ type: 'childList', target: page, addedNodes: [dlg, Object.assign(new FakeEl('#text'), { _text: 'x' })] });
  // A checkbox made through el('input') whose type is set afterwards.
  const box = make('input');
  page.append(box);
  records.push({ type: 'childList', target: page, addedNodes: [box] });
  box.type = 'checkbox';
  records.push({ type: 'attributes', target: box, addedNodes: [] });
  observed.cb(records);
  assert.deepEqual(missingTabStops(page), []);
  assert.equal(box.getAttribute('tabindex'), '0');
  // No MutationObserver: the first pass still runs.
  const bare = make('div', {}, [make('button')]);
  assert.equal(installTabStops(bare, undefined), false);
  assert.deepEqual(missingTabStops(bare), []);
});

test('the conversation is a focusable, named scroll region; markup that names it already keeps its name', () => {
  const events = make('div', { id: 'events', 'aria-live': 'polite' });
  makeScrollRegion(events, 'Conversation');
  assert.equal(events.tabIndex, 0);
  assert.equal(events.getAttribute('aria-label'), 'Conversation');
  const labelled = make('div', { 'aria-label': 'Transcript', tabindex: '-1' });
  makeScrollRegion(labelled, 'Conversation');
  assert.equal(labelled.getAttribute('aria-label'), 'Transcript');
  assert.equal(labelled.getAttribute('tabindex'), '-1');
  const main = readFileSync(here('../src/main.ts'), 'utf8');
  assert.match(main, /installTabStops\(document\.body\)/);
  assert.match(main, /makeScrollRegion\(\$\('events'\), 'Conversation'\)/);
});

// ---------- rendered controls (el() alone, no observer) ----------

test('a rendered error Details section, an empty state button and a confirm dialog are all tab stops', async () => {
  const details = dom.errorDetails('daemon error -32000: boom');
  const empty = dom.emptyState('div', 'budgets-empty', 'No budget policies.', { label: 'Add policy', key: 'a', run() {} });
  dom.toast('daemon error -32000: something odd happened');
  assert.deepEqual(missingTabStops(details), []);
  assert.deepEqual(missingTabStops(empty), []);
  assert.deepEqual(missingTabStops(toast), []);
  assert.ok(toast.querySelector('summary'), 'the toast rendered its Details');
  const done = approvals.confirmDialog('Archive this session?', 'Archive', { title: 'Archive' });
  const dlg = body.children.find((n) => n.tag === 'dialog' && n.className === 'confirm');
  assert.ok(dlg, 'the confirm dialog rendered');
  assert.equal(dlg.querySelectorAll('button').length, 2);
  assert.deepEqual(missingTabStops(dlg), []);
  dlg.close();
  assert.equal(await done, false);
});

test('the question dialog\'s option checkboxes and buttons are tab stops', async () => {
  const { state } = await import('../dist/state.js');
  const lifecycle = await import('../dist/actions/lifecycle.js');
  const now = '2026-10-20T12:00:00.000000000Z';
  state.sessions = [{
    id: '00000000-0000-4000-8000-000000000001', title: 'Waiting worker', status: 'WaitingApproval', provider: 'Claude',
    working_dir: '/tmp', created_at: now, updated_at: now, parent_id: null, kind: 'Session',
    pending_question: { questions: [{ question: 'Which checks?', header: 'Checks', multiSelect: true, options: [{ label: 'tests', description: '' }, { label: 'smoke', description: '' }] }] },
  }];
  state.selectedId = state.sessions[0].id;
  lifecycle.openQuestionModal();
  await tick();
  const dlg = body.walk().find((n) => n.tag === 'dialog' && n.id === 'question-dialog');
  assert.ok(dlg, 'the question dialog rendered');
  const buttons = dlg.querySelectorAll('button').map((b) => b.textContent);
  assert.ok(buttons.some((t) => t.startsWith('Let the agent decide')) && buttons.some((t) => t.startsWith('Submit')), buttons.join('|'));
  stampTabStops(dlg); // the boot observer's pass over controls not made by el() (option inputs)
  assert.deepEqual(missingTabStops(dlg), []);
  dlg.close();
});

// ---------- the static page and the sources ----------

/** The opening tags of static/index.html as fake elements (enough to run the boot stamp over the real markup). */
function staticPage() {
  const html = readFileSync(here('../static/index.html'), 'utf8');
  const page = make('body');
  for (const m of html.matchAll(/<([a-z][a-z0-9]*)\b([^>]*)>/gi)) {
    const e = new FakeEl(m[1].toLowerCase());
    for (const a of m[2].matchAll(/([\w-]+)(?:="([^"]*)")?/g)) e.setAttribute(a[1], a[2] ?? '');
    page.append(e);
  }
  return page;
}

test('the boot stamp reaches every button, link, checkbox and summary in static/index.html', () => {
  const page = staticPage();
  const buttons = page.querySelectorAll('button');
  assert.ok(buttons.length > 20, `${buttons.length} buttons in index.html`);
  for (const id of ['new-session', 'interrupt', 'hard-pause', 'archive', 'launch-cancel']) assert.ok(page.walk().some((n) => n.id === id), id);
  stampTabStops(page);
  assert.deepEqual(missingTabStops(page), []);
  assert.equal(page.walk().find((n) => n.id === 'vim-keys').getAttribute('tabindex'), '0');
  // Deliberate tab stops in the markup are unchanged.
  assert.equal(page.walk().find((n) => n.id === 'issues-inspector').getAttribute('tabindex'), '-1');
});

function sources(dir, out = []) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) sources(p, out);
    else if (/\.(ts|html)$/.test(name) && !name.includes('.generated.')) out.push(p);
  }
  return out;
}

test('no source sets a positive tabindex, and buttons and summaries are only made through el()', () => {
  for (const p of [...sources(here('../src')), ...sources(here('../static'))]) {
    const text = readFileSync(p, 'utf8');
    assert.doesNotMatch(text, /tabIndex\s*=\s*[1-9]|tabindex="?[1-9]|setAttribute\('tabindex',\s*'[1-9]/, p);
    assert.doesNotMatch(text, /createElement\(['"](button|summary)['"]\)/, `${p}: make buttons and summaries with el() so they get a tab stop`);
    if (!p.endsWith(join('src', 'dom.ts'))) {
      assert.doesNotMatch(text, /createElement\(tag\)|function el\b/, `${p}: use the el() of dom.ts, which gives buttons and summaries a tab stop`);
    }
  }
});
