// Errors after boot (Issue #295): the startup banner is for startup only; a later error is a dismissible notice in
// plain words with the technical text under Details, folded when it repeats, and one log line each.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isScriptFault, plainError, presentError } from '../dist/errors-model.js';
import { COALESCE_MS, LOG_BUDGET, LOG_WINDOW_MS, RuntimeTracker, repeatNote, runtimeLogLine, runtimeNotice } from '../dist/runtime-model.js';

const NULL_PEERS = "Cannot read properties of null (reading 'peers')";

test('script faults are told apart from daemon answers and plain messages', () => {
  assert.ok(isScriptFault(`TypeError: ${NULL_PEERS}`));
  assert.ok(isScriptFault(`Uncaught TypeError: ${NULL_PEERS}`));
  assert.ok(isScriptFault(NULL_PEERS));
  assert.ok(isScriptFault("null is not an object (evaluating 'x.entries')"));
  assert.ok(isScriptFault('foo.bar is not a function'));
  assert.ok(!isScriptFault('daemon error -32000: stale_version'));
  assert.ok(!isScriptFault('Pick a folder first.'));
  assert.ok(!isScriptFault('The TypeError class is documented here'));
});

test('a view loader toast reads in plain words with the raw text kept as Details, never a bare TypeError', () => {
  const p = presentError(`Satellite registry load failed: TypeError: ${NULL_PEERS}`);
  assert.equal(p.kind, 'internal');
  assert.match(p.text, /^Satellite registry load failed: The app could not read rsid's answer\./);
  assert.ok(!/TypeError|Cannot read/.test(p.text), p.text);
  assert.match(p.detail, /TypeError: Cannot read properties of null \(reading 'peers'\)/);
});

test('plainError names the app-side fault and keeps daemon errors as they were', () => {
  const p = plainError(new TypeError(NULL_PEERS));
  assert.equal(p.kind, 'internal');
  assert.match(p.next, /copy the Details/);
  assert.equal(plainError('daemon did not answer in time').kind, 'timeout');
  assert.equal(plainError('Pick a folder first.').kind, 'local');
});

test('a runtime notice names the view, hides the technical text and keeps it as detail', () => {
  const n = runtimeNotice(`TypeError: ${NULL_PEERS}`, 'the Settings dialog');
  assert.match(n.text, /^The app could not read rsid's answer for the Settings dialog\. The rest of the app still works\./);
  assert.ok(!/TypeError/.test(n.text));
  assert.match(n.detail, /TypeError/);
  const other = runtimeNotice('Error: something odd', null);
  assert.match(other.text, /^Something went wrong\. The rest of the app still works\./);
  assert.equal(other.detail, 'Error: something odd');
  const daemon = runtimeNotice('Error: daemon did not answer in time', null);
  assert.match(daemon.text, /rsid did not answer in time\./);
  assert.match(daemon.text, /still works/);
});

test('harmless browser notices and empty messages make no notice; credentials are redacted from the detail', () => {
  assert.equal(runtimeNotice('ResizeObserver loop completed with undelivered notifications.', null), null);
  assert.equal(runtimeNotice('  ', null), null);
  const n = runtimeNotice('Error: failed with token=abcd1234efgh5678', null);
  assert.ok(!n.detail.includes('abcd1234efgh5678'), n.detail);
});

test('the same error folds into one notice; a changing number does not make a new one', () => {
  const t = new RuntimeTracker();
  const a = runtimeNotice(`TypeError: ${NULL_PEERS}`, null).key;
  assert.deepEqual(t.note(a, 1000), { show: true, log: true, count: 1 });
  assert.equal(t.note(a, 1500).show, false);
  assert.equal(t.note(a, 2000).count, 3);
  assert.equal(t.note(a, 2500).log, false, 'after the third, the log stays quiet');
  assert.equal(t.note(a, 1000 + COALESCE_MS).show, true, 'after the window the error is shown again');
  assert.equal(runtimeNotice('Error: row 12 failed', null).key, runtimeNotice('Error: row 13 failed', null).key);
  assert.equal(repeatNote(1), '');
  assert.equal(repeatNote(4), 'Happened 4 times.');
});

test('the log has a budget: a flood of different errors cannot fill it', () => {
  const t = new RuntimeTracker();
  let logged = 0;
  for (let i = 0; i < 200; i++) if (t.note(`distinct error ${'x'.repeat(i)}`, 5000).log) logged += 1;
  assert.equal(logged, LOG_BUDGET);
  assert.equal(t.note('later error', 5000 + LOG_WINDOW_MS).log, true, 'a new window has its own budget');
  assert.equal(runtimeLogLine('TypeError:  a\n b', 1), 'runtime error: TypeError: a b');
  assert.equal(runtimeLogLine('x', 3), 'runtime error (x3): x');
});

// ---- the page: boot.ts and runtime-notice.ts against a small fake DOM ----

class Node {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.children = []; this.dataset = {}; this.attrs = {}; this.listeners = {};
    this.className = ''; this.id = ''; this.hidden = false; this.tabIndex = -1; this._text = ''; this.parentNode = null;
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...n) { for (const c of n) { c.parentNode = this; this.children.push(c); } }
  replaceChildren(...n) { this._text = ''; this.children = []; this.append(...n); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  addEventListener(t, cb) { (this.listeners[t] ??= []).push(cb); }
  click() { for (const cb of this.listeners.click ?? []) cb({ stopPropagation() {} }); }
  closest() { return null; }
  querySelector(sel) { return this.all((n) => sel.split(',').some((s) => n.tagName === s.trim().toUpperCase()))[0] ?? null; }
  all(pred, out = []) { for (const c of this.children) { if (pred(c)) out.push(c); c.all(pred, out); } return out; }
  hidePopover() { this.popover = false; }
  showPopover() { this.popover = true; }
}

const byId = new Map();
const mk = (id, tag = 'div') => { const n = new Node(tag); n.id = id; byId.set(id, n); return n; };
const bannerEl = mk('boot-banner'); bannerEl.hidden = true;
mk('toast');
const body = new Node('body');
const docEl = new Node('html');
const handlers = {};
const bootLog = [];
globalThis.document = {
  documentElement: docEl, body,
  createElement: (t) => new Node(t),
  getElementById: (id) => byId.get(id) ?? null,
  querySelectorAll: () => [],
};
body.append = (...n) => { for (const c of n) { if (c.id) byId.set(c.id, c); } };
globalThis.window = { addEventListener: (t, cb) => { handlers[t] = cb; }, setTimeout, clearTimeout };
globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { if (cmd === 'boot_report') bootLog.push(args.report); } } };
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };

// sessions.js first: boot.js and sessions.js import each other, and the app's own entry order starts at sessions.
await import('../dist/sessions.js');
const boot = await import('../dist/boot.js');
const ok = (name) => ({ name, ok: true });

test('an unhandled rejection after boot leaves the boot state and banner alone and shows a plain, dismissible notice', async () => {
  boot.installBootErrorCapture();
  boot.setDaemonLink({ kind: 'connected', socket: '/s' });
  boot.finishBoot(['daemon', 'projects', 'labels', 'sessions', 'settings', 'live'].map(ok));
  const before = { boot: docEl.dataset.boot, report: docEl.dataset.bootReport };
  assert.equal(before.boot, 'ready');
  bootLog.length = 0;
  handlers.unhandledrejection({ reason: new TypeError(NULL_PEERS) });
  assert.equal(docEl.dataset.boot, before.boot);
  assert.equal(docEl.dataset.bootReport, before.report, 'the boot report did not change');
  assert.equal(bannerEl.hidden, true);
  assert.equal(bannerEl.textContent, '');
  assert.ok(!/did not start/.test(bannerEl.textContent));
  const notice = byId.get('runtime-notice');
  assert.ok(notice, 'a runtime notice exists');
  assert.equal(notice.attrs.role, 'alert');
  assert.match(notice.textContent, /The app could not read rsid's answer\. The rest of the app still works\./);
  const text = notice.all((n) => n.className === 'err-text')[0].textContent;
  assert.ok(!/TypeError/.test(text), text);
  const details = notice.all((n) => n.tagName === 'DETAILS')[0];
  assert.match(details.textContent, /TypeError: Cannot read properties of null/);
  // One log line, through the boot_report command, carrying the error.
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(bootLog.length, 1);
  assert.match(bootLog[0].pageErrors[0], /^runtime error: TypeError: Cannot read properties of null/);
  // Dismiss hides it.
  const dismiss = notice.all((n) => n.tagName === 'BUTTON' && n.textContent === 'Dismiss')[0];
  assert.ok(dismiss, 'a Dismiss button');
  dismiss.click();
  assert.equal(notice.hidden, true);
});

test('the same error again is counted on the notice, not stacked, and a page error event is a runtime error too', async () => {
  bootLog.length = 0;
  const notice = byId.get('runtime-notice');
  handlers.unhandledrejection({ reason: new Error('boom one') });
  assert.equal(notice.hidden, false, 'a new error shows the notice again');
  for (let i = 0; i < 4; i++) handlers.error({ message: 'Uncaught Error: boom one', error: new Error('boom one') });
  assert.match(notice.textContent, /Happened 5 times\./);
  assert.equal([...byId.keys()].filter((k) => k === 'runtime-notice').length, 1);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(bootLog.length, 3, 'three lines, then quiet');
  assert.equal(docEl.dataset.boot, 'ready');
  assert.equal(bannerEl.hidden, true);
});

test('main.ts and boot.ts keep the split: errors before finishBoot count against boot, after it do not', async () => {
  const { readFileSync } = await import('node:fs');
  const src = readFileSync(new URL('../src/boot.ts', import.meta.url), 'utf8');
  assert.match(src, /if \(steps\) \{\s*reportRuntimeError\(raw, logRuntimeLine\);\s*return;\s*\}/);
  assert.match(src, /pageErrors\.push\(boot\)/);
});
