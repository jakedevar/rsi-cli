// The newer-build notice (local #322, parity with the TUI's #363 banner): GetDaemonInfo's build_sha and
// local_build.differs in, a persistent non-modal strip out. Model text, show / hide, dismiss kept for that build,
// a failed read keeping the last state, no redraw (so no new announcement) on an unchanged poll, and the backend wiring.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { SHORT_SHA, buildNoticeView, isDismissed, noticeKey } from '../dist/build-notice-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const read = (p) => readFileSync(here(p), 'utf8');

const A = 'a'.repeat(40);
const B = 'b'.repeat(40);
const C = 'c'.repeat(40);
const info = (over = {}) => ({ build_sha: A, local_build: { sha: B, path: '/t/release/rsid', built_at: '2026-10-20T08:00:00.000000000Z', differs: true }, ...over });

test('model: a newer local build names the running and the built short SHAs and says how to get it', () => {
  const n = buildNoticeView(info());
  assert.equal(SHORT_SHA, 9);
  assert.equal(n.builtSha, B);
  assert.equal(n.headline, 'A newer RSI build is available.');
  assert.match(n.facts, /rsid is running build aaaaaaaaa\./);
  assert.match(n.facts, /Build bbbbbbbbb was made on this computer\./);
  assert.match(n.action, /Restart rsid/);
  assert.match(n.action, /make release-install/);
});

test('model: the same build, no local build, differs false or an answer that is not the shape give no notice', () => {
  assert.equal(buildNoticeView(info({ local_build: { sha: A, path: '/p', built_at: 'x', differs: false } })), null, 'same build');
  assert.equal(buildNoticeView(info({ local_build: { sha: B, path: '/p', built_at: 'x', differs: false } })), null, 'older or equal artifact');
  assert.equal(buildNoticeView({ build_sha: A }), null, 'no local build');
  assert.equal(buildNoticeView(info({ local_build: null })), null);
  assert.equal(buildNoticeView(info({ local_build: { sha: '', differs: true } })), null, 'no SHA to name');
  assert.equal(buildNoticeView({ local_build: { sha: B, differs: true } }), null, 'no running build to compare');
  for (const bad of [null, undefined, 'x', 3, [], {}]) assert.equal(buildNoticeView(bad), null);
});

test('model: a dismissal covers that build only, and the key is empty whenever nothing is shown', () => {
  const n = buildNoticeView(info());
  assert.equal(isDismissed(n, B), true);
  assert.equal(isDismissed(n, C), false, 'a different newer build shows again');
  assert.equal(isDismissed(n, null), false);
  assert.equal(isDismissed(null, B), false);
  assert.equal(noticeKey(n, null), `${'a'.repeat(9)}>${B}`);
  assert.equal(noticeKey(n, B), '');
  assert.equal(noticeKey(null, null), '');
});

const permissive = (target) => new Proxy(target, {
  get(t, k) { if (k in t) return t[k]; if (typeof k === 'symbol') return undefined; return (...a) => (k === 'closest' || k === 'querySelector' ? null : k === 'querySelectorAll' ? [] : undefined); },
});
class Node {
  constructor(tag) {
    this.tagName = String(tag).toUpperCase(); this.children = []; this.dataset = {}; this.attrs = {}; this.listeners = {}; this.parent = null;
    this.className = ''; this.disabled = false; this._text = ''; this.tabIndex = -1; this.hidden = false; this.title = ''; this.id = ''; this.placeholder = ''; this.value = '';
    this.style = { setProperty() {} };
    const classes = new Set();
    this.classList = { add: (c) => classes.add(c), remove: (c) => classes.delete(c), toggle: (c, on) => { if (on === undefined ? !classes.has(c) : on) classes.add(c); else classes.delete(c); }, contains: (c) => classes.has(c) };
    return permissive(this);
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  replaceChildren(...n) { this._text = ''; this.children = []; this.append(...n); }
  append(...n) { for (const c of n) { c.parent = this; this.children.push(c); } }
  prepend(...n) { for (const c of n) c.parent = this; this.children.unshift(...n); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  removeAttribute(k) { delete this.attrs[k]; }
  addEventListener(t, cb) { (this.listeners[t] ??= []).push(cb); }
  focus() { globalThis.document.activeElement = this; }
  contains(n) { return n === this || this.children.some((c) => c.contains?.(n)); }
  get firstChild() { return this.children[0] ?? null; }
  get nextSibling() { return this.parent ? this.parent.children[this.parent.children.indexOf(this) + 1] ?? null : null; }
  removeChild(c) { this.children = this.children.filter((x) => x !== c); c.parent = null; return c; }
  insertBefore(node, ref) { if (node.parent) node.parent.children = node.parent.children.filter((x) => x !== node); const at = ref ? this.children.indexOf(ref) : this.children.length; this.children.splice(at, 0, node); node.parent = this; return node; }
  all(pred, out = []) { for (const c of this.children) { if (pred(c)) out.push(c); c.all(pred, out); } return out; }
}

const byId = new Map();
const nodeFor = (id) => { let n = byId.get(id); if (!n) { n = new Node('div'); n.id = id; byId.set(id, n); } return n; };
const body = new Node('body');
const store = new Map();
globalThis.document = {
  documentElement: new Node('html'), body, activeElement: body,
  createElement: (t) => new Node(t),
  getElementById: (id) => nodeFor(id),
  querySelectorAll: () => [], querySelector: () => null, addEventListener() {},
};
const intervals = [];
globalThis.window = {
  addEventListener() {}, setTimeout, clearTimeout,
  setInterval: (fn, ms) => { intervals.push(ms); return 1; },
  localStorage: { getItem: (k) => store.get(k) ?? null, setItem: (k, v) => { store.set(k, String(v)); }, removeItem: (k) => { store.delete(k); } },
};

const remote = { info: null, fail: null, calls: [], service: { kind: 'not-installed', manager: 'launchd', looked: [] } };
globalThis.__TAURI__ = { core: { invoke: async (cmd) => {
  remote.calls.push(cmd);
  if (cmd === 'get_daemon_build') { if (remote.fail) throw remote.fail; return remote.info; }
  if (cmd === 'daemon_service') return remote.service;
  return null;
} } };

const { BUILD_POLL_MS, DISMISSED_KEY, refreshBuildNotice, renderBuildNotice, resetBuildNotice, startBuildNotice } = await import('../dist/build-notice.js');
const host = nodeFor('build-notice');
const fresh = () => { resetBuildNotice(); store.clear(); remote.fail = null; remote.calls.length = 0; host.hidden = true; host.children = []; };
const button = (attr) => host.all((n) => n.tagName === 'BUTTON' && attr in n.dataset)[0];

test('DOM: shown when a newer build is reported, with the facts in plain words, a Restart rsid and a Dismiss button', async () => {
  fresh();
  remote.info = info();
  await refreshBuildNotice();
  assert.equal(host.hidden, false);
  assert.equal(host.dataset.buildNotice, 'newer');
  assert.match(host.textContent, /A newer RSI build is available\./);
  assert.match(host.textContent, /running build aaaaaaaaa/);
  assert.match(host.textContent, /Build bbbbbbbbb/);
  assert.match(host.textContent, /make release-install/);
  assert.equal(button('buildRestart').textContent, 'Restart rsid');
  assert.equal(button('buildDismiss').textContent, 'Dismiss');
});

test('DOM: hidden when the build is the same, when there is no local build, and for an older rsid that has no such field', async () => {
  for (const answer of [info({ local_build: { sha: A, path: '/p', built_at: 'x', differs: false } }), { build_sha: A }, { status: 'ok' }]) {
    fresh();
    remote.info = answer;
    await refreshBuildNotice();
    assert.equal(host.hidden, true, JSON.stringify(answer));
    assert.equal(host.children.length, 0);
  }
});

test('DOM: it clears when the next read says the build matches (rsid restarted onto it)', async () => {
  fresh();
  remote.info = info();
  await refreshBuildNotice();
  assert.equal(host.hidden, false);
  remote.info = info({ build_sha: B, local_build: { sha: B, path: '/p', built_at: 'x', differs: false } });
  await refreshBuildNotice();
  assert.equal(host.hidden, true);
  assert.equal(host.children.length, 0);
});

test('DOM: a failed read (method not found on an older rsid, rsid down) keeps the last state and shows no error', async () => {
  fresh();
  remote.info = info();
  await refreshBuildNotice();
  const shown = host.children.length;
  remote.fail = new Error('Method not found: GetDaemonInfo');
  const toasts = nodeFor('toast').textContent;
  await refreshBuildNotice();
  assert.equal(host.hidden, false, 'still showing');
  assert.equal(host.children.length, shown);
  assert.equal(nodeFor('toast').textContent, toasts, 'no toast');
  // Nothing was known and the read fails: still nothing, still no error.
  fresh();
  remote.fail = new Error('cannot reach rsid');
  await refreshBuildNotice();
  assert.equal(host.hidden, true);
});

test('DOM: an unchanged poll does not redraw the strip, so a screen reader hears the notice once', async () => {
  fresh();
  remote.info = info();
  await refreshBuildNotice();
  const first = host.children[0];
  for (let i = 0; i < 3; i += 1) await refreshBuildNotice();
  assert.equal(host.children[0], first, 'same nodes: no replaceChildren on an identical poll');
  assert.equal(host.getAttribute('aria-live') ?? 'polite', 'polite');
  const html = read('../static/index.html');
  assert.match(html, /<div id="build-notice" class="build-notice" role="status" aria-live="polite" hidden><\/div>/);
});

test('DOM: Dismiss hides it for that build and keeps it hidden across polls; a different newer build shows it again', async () => {
  fresh();
  remote.info = info();
  await refreshBuildNotice();
  button('buildDismiss').listeners.click[0]();
  assert.equal(host.hidden, true);
  assert.equal(store.get(DISMISSED_KEY), B, 'remembered for this build');
  await refreshBuildNotice();
  await refreshBuildNotice();
  assert.equal(host.hidden, true, 'stays dismissed across polls');
  // A page reload: the dismissal is read back from storage.
  resetBuildNotice();
  await refreshBuildNotice();
  assert.equal(host.hidden, true, 'and across a restart of the app');
  remote.info = info({ local_build: { sha: C, path: '/p', built_at: 'x', differs: true } });
  await refreshBuildNotice();
  assert.equal(host.hidden, false, 'a different build is new news');
  assert.match(host.textContent, /Build ccccccccc/);
});

test('DOM: Restart rsid on a window that cannot restart rsid says what to run instead, and leaves the notice up', async () => {
  fresh();
  remote.info = info();
  await refreshBuildNotice();
  button('buildRestart').listeners.click[0]();
  await new Promise((r) => setTimeout(r, 20));
  assert.ok(remote.calls.includes('daemon_service'), 'it reads the service first');
  assert.match(nodeFor('toast').textContent, /make release-install/);
  assert.equal(host.hidden, false);
});

test('wiring: one read now and then every 30 s, the read is GetDaemonInfo through an allowlisted command, and the fake fixture carries the field', async () => {
  fresh();
  remote.info = info();
  intervals.length = 0;
  startBuildNotice();
  assert.deepEqual(intervals, [BUILD_POLL_MS]);
  assert.equal(BUILD_POLL_MS, 30_000);
  const { READ_COMMANDS } = await import('../scripts/smoke-bridge.mjs');
  assert.equal(READ_COMMANDS.get_daemon_build.method, 'GetDaemonInfo');
  assert.deepEqual(READ_COMMANDS.get_daemon_build.params(), {});
  const { FIXTURES } = await import('../scripts/smoke-fake.mjs');
  const f = FIXTURES.GetDaemonInfo();
  assert.equal(f.build_sha, f.local_build.sha);
  assert.equal(f.local_build.differs, false, 'the audit walks do not show the strip');
  assert.match(read('../src/main.ts'), /startBuildNotice\(\);/);
  assert.match(read('../src-tauri/src/lib.rs'), /async fn get_daemon_build[\s\S]*METHOD_DAEMON_INFO/);
});
