// The UX audit toolkit (local #277): the write-stub bridge, the WCAG contrast walker, the scale fixtures and the
// open() knobs. The pure parts run anywhere; one test boots the built app in headless Chrome (skipped without Chrome or dist).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runInNewContext } from 'node:vm';
import { fileURLToPath } from 'node:url';
import { READ_METHODS, createBridge, renderMarkdownSubset, rpcCall } from '../scripts/smoke-bridge.mjs';
import { FIXTURES, HOME_PROJECT, SESSION_IDS, longConversation, scaleFixtures, startFakeDaemon } from '../scripts/smoke-fake.mjs';
import { findChrome, transportReport } from '../scripts/smoke-lib.mjs';
import { DOWN_MESSAGE, createStubBridge } from '../scripts/smoke-stub-bridge.mjs';
import { compose, contrastAudit, contrastExpression, judge, luminance, neededRatio, parseColor, ratioOf } from '../scripts/audit-contrast.mjs';
import { THEME_STORAGE_KEY, awaitApp, createReadControls, mediaFeatures, open, openSessionByTitle, refusalText, seedEntries, sleep } from '../scripts/audit-lib.mjs';
import { main as auditMain, parseArgs, parseSlow, probeViewOpen, runViews } from '../scripts/audit.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const S1 = SESSION_IDS.lead;

// ---------- the write-stub bridge ----------

function stubbed(opts = {}) {
  const calls = [];
  const bridge = createStubBridge({ call: async (m, p) => { calls.push({ m, p }); return { ok: m }; }, ...opts });
  return { calls, bridge };
}

test('a write is recorded and answered with a stub, and never reaches the daemon call', async () => {
  const { calls, bridge } = stubbed({ stubs: { archive_session: { archived: true }, interrupt_session: (a) => ({ stopped: a.sessionId }) } });
  assert.deepEqual(await bridge.invoke('send_message', { sessionId: S1, text: 'hello' }), null);
  assert.deepEqual(await bridge.invoke('archive_session', { sessionId: S1 }), { archived: true });
  assert.deepEqual(await bridge.invoke('interrupt_session', { sessionId: S1 }), { stopped: S1 });
  assert.deepEqual(await bridge.invoke('update_issue_status', {}), null, 'a command the bridge has never heard of is a write too');
  assert.deepEqual(calls, [], 'no write was handed to the daemon call');
  assert.deepEqual(bridge.writes().map((w) => [w.cmd, w.outcome]), [
    ['send_message', 'stubbed'], ['archive_session', 'stubbed'], ['interrupt_session', 'stubbed'], ['update_issue_status', 'stubbed'],
  ]);
  assert.deepEqual(bridge.writes()[0].args, { sessionId: S1, text: 'hello' });
});

test('reads still go to the daemon call and shim-local commands answer themselves', async () => {
  const { calls, bridge } = stubbed();
  assert.deepEqual(await bridge.invoke('get_conversation', { sessionId: S1, sinceSequence: 4 }), { ok: 'GetConversation' });
  assert.deepEqual(calls, [{ m: 'GetConversation', p: { session_id: S1, since_sequence: 4 } }]);
  assert.equal(await bridge.invoke('live_link'), false);
  assert.deepEqual(bridge.writes(), [], 'reads are not writes');
});

test('no write reaches a socket: a real fake daemon only ever hears reads', async () => {
  const fake = await startFakeDaemon();
  try {
    const bridge = createStubBridge({ call: (m, p) => rpcCall(fake.socket, m, p) });
    await bridge.invoke('list_sessions');
    for (const cmd of ['send_message', 'launch_session', 'archive_session', 'delete_issue', 'update_daemon_config', 'set_provider_credential', 'interrupt_session', 'nonsense']) {
      await bridge.invoke(cmd, { sessionId: S1, text: 'x' });
    }
    bridge.setDown(true);
    await assert.rejects(bridge.invoke('send_message', {}), /cannot reach rsid/);
    await assert.rejects(bridge.invoke('list_sessions'), /cannot reach rsid/);
    bridge.setDown(false);
    bridge.failWrites(true);
    await assert.rejects(bridge.invoke('send_message', {}), /stubbed failure/);
    assert.deepEqual(fake.received.map((r) => r.method), ['ListSessions'], 'the daemon heard one read and nothing else');
    assert.ok(fake.received.every((r) => READ_METHODS.has(r.method)));
  } finally { await fake.stop(); }
});

test('down fails reads and writes like an unreachable rsid, and comes back', async () => {
  const { calls, bridge } = stubbed();
  bridge.setDown(true);
  assert.equal(bridge.isDown(), true);
  await assert.rejects(bridge.invoke('list_sessions'), (e) => e.message === DOWN_MESSAGE);
  await assert.rejects(bridge.invoke('send_message', {}), (e) => e.message === DOWN_MESSAGE);
  assert.equal(await bridge.invoke('live_link'), false, 'a shim-local command does not need rsid');
  // The real backend never throws from daemon_info: an unreachable rsid is an answer, and the page draws the daemon-down card from it.
  assert.deepEqual(await bridge.invoke('daemon_info'), { socket: 'daemon.sock', connected: false, error: DOWN_MESSAGE });
  assert.deepEqual(calls, []);
  assert.deepEqual(bridge.writes(), [], 'a write that could not be sent is not a stubbed write');
  bridge.setDown(false);
  assert.deepEqual(await bridge.invoke('daemon_info'), { socket: 'daemon.sock', connected: true, health: { ok: 'GetHealthStatus' } });
  assert.deepEqual(await bridge.invoke('list_sessions'), { ok: 'ListSessions' });
});

test('failWrites fails the named writes (or all) on demand with a message, records them as failed, and clears', async () => {
  const { bridge } = stubbed();
  bridge.failWrites(['send_message', 'archive_session'], 'rsid said no');
  await assert.rejects(bridge.invoke('send_message', { text: 'x' }), /rsid said no/);
  await assert.rejects(bridge.invoke('archive_session', {}), /rsid said no/);
  assert.equal(await bridge.invoke('interrupt_session', {}), null, 'a write not named still succeeds');
  assert.deepEqual(await bridge.invoke('list_sessions'), { ok: 'ListSessions' }, 'reads are not writes');
  bridge.failWrites('interrupt_session');
  await assert.rejects(bridge.invoke('interrupt_session', {}), /stubbed failure/);
  bridge.failWrites(true, 'all down');
  await assert.rejects(bridge.invoke('anything_at_all', {}), /all down/);
  bridge.clearFailures();
  assert.equal(await bridge.invoke('send_message', {}), null);
  assert.deepEqual(bridge.writes().map((w) => w.outcome), ['failed', 'failed', 'stubbed', 'failed', 'failed', 'stubbed']);
});

test('recorded write arguments never carry a credential', async () => {
  const { bridge } = stubbed();
  await bridge.invoke('set_provider_credential', { slot: 'anthropic', secret: 'sk-live-1234', nested: { apiKey: 'abc', note: 'ok' } });
  const [w] = bridge.writes();
  assert.equal(w.args.secret, '••••');
  assert.equal(w.args.nested.apiKey, '••••');
  assert.equal(w.args.nested.note, 'ok');
  assert.ok(!JSON.stringify(bridge.log).includes('sk-live-1234'));
});

test('the stub bridge is a drop-in for createBridge: same invoke(cmd, args) and the record hook hears every decision', async () => {
  const heard = [];
  const { bridge } = stubbed({ record: (e) => heard.push(e.outcome) });
  await bridge.invoke('list_sessions');
  await bridge.invoke('send_message', {});
  assert.deepEqual(heard, ['forwarded', 'stubbed']);
  assert.equal(typeof createBridge, 'function');
});

// ---------- contrast ----------

test('parseColor reads rgb, rgba, transparent and color(srgb)', () => {
  assert.deepEqual(parseColor('rgb(255, 0, 128)'), { r: 255, g: 0, b: 128, a: 1 });
  assert.deepEqual(parseColor('rgba(0, 0, 0, 0.5)'), { r: 0, g: 0, b: 0, a: 0.5 });
  assert.deepEqual(parseColor('rgb(0 0 0 / 25%)'), { r: 0, g: 0, b: 0, a: 0.25 });
  assert.deepEqual(parseColor('transparent'), { r: 0, g: 0, b: 0, a: 0 });
  const mixed = parseColor('color(srgb 1 0.5 0 / 0.5)');
  assert.deepEqual([mixed.r, mixed.g, mixed.b, mixed.a], [255, 127.5, 0, 0.5]);
  assert.equal(parseColor('oklch(0.5 0.1 200)'), null);
  assert.equal(parseColor(undefined), null);
});

test('the WCAG ratio: black on white is 21, a colour on itself is 1, #777 on white is just under 4.5', () => {
  const white = { r: 255, g: 255, b: 255 };
  assert.equal(Math.round(ratioOf({ r: 0, g: 0, b: 0 }, white) * 100) / 100, 21);
  assert.equal(ratioOf(white, white), 1);
  assert.ok(luminance(white) > 0.99);
  const grey = { r: 0x77, g: 0x77, b: 0x77 };
  assert.ok(ratioOf(grey, white) > 4.4 && ratioOf(grey, white) < 4.5);
  assert.equal(ratioOf(grey, white), ratioOf(white, grey), 'the order of the two colours does not matter');
});

test('large text needs 3:1, everything else 4.5:1', () => {
  assert.equal(neededRatio(16, '400'), 4.5);
  assert.equal(neededRatio(18.66, '400'), 4.5);
  assert.equal(neededRatio(18.66, '700'), 3);
  assert.equal(neededRatio(24, '400'), 3);
  assert.equal(neededRatio(13, '700'), 4.5);
});

test('judge composites translucent layers and the text alpha, and falls back to white', () => {
  const black = { r: 0, g: 0, b: 0, a: 1 };
  assert.deepEqual(compose({ r: 0, g: 0, b: 0, a: 0.5 }, { r: 255, g: 255, b: 255, a: 1 }), { r: 127.5, g: 127.5, b: 127.5, a: 1 });
  assert.equal(judge(black, [], 16, '400').ok, true, 'no background layer: white');
  const onDark = judge({ r: 60, g: 60, b: 60, a: 1 }, [{ r: 20, g: 20, b: 20, a: 1 }], 16, '400');
  assert.equal(onDark.ok, false);
  assert.ok(onDark.ratio < 4.5);
  // A 50% black panel over a white page behind a white text: the text sits on mid grey (#808080).
  const panel = judge({ r: 255, g: 255, b: 255, a: 1 }, [{ r: 0, g: 0, b: 0, a: 0.5 }, { r: 255, g: 255, b: 255, a: 1 }], 16, '400');
  assert.equal(panel.bg.r, 127.5);
  assert.equal(panel.ok, false);
  // Half-transparent text over white is lighter than its colour says.
  const faded = judge({ r: 0, g: 0, b: 0, a: 0.4 }, [{ r: 255, g: 255, b: 255, a: 1 }], 16, '400');
  assert.equal(faded.ok, false);
  // The bottom layer is painted first: [element, parent]: the nearer layer wins.
  assert.equal(judge(black, [{ r: 255, g: 255, b: 255, a: 1 }, { r: 0, g: 0, b: 0, a: 1 }], 16, '400').ok, true);
});

/** Run an expression in a context; the result crosses realms, so it comes back as plain JSON. */
const runIn = (expression, context) => JSON.parse(JSON.stringify(runInNewContext(expression, context)));

/** A minimal DOM the walker can run in: elements are plain objects; the page is a list of them under `body`. */
function fakeDom(specs) {
  const body = { tagName: 'BODY', id: '', className: '', parentElement: null, style: { backgroundColor: 'rgb(0, 0, 0)' } };
  const make = (s) => {
    const el = {
      tagName: (s.tag ?? 'span').toUpperCase(), id: s.id ?? '', className: s.cls ?? '', parentElement: s.parent ?? body, style: s.style ?? {}, hidden: Boolean(s.hidden),
      checkVisibility: () => !s.hidden,
      closest: (sel) => (s.disabled && /:disabled/.test(sel) ? el : null),
      getBoundingClientRect: () => ({ width: s.empty ? 0 : 100, height: 20 }),
    };
    el.text = s.text;
    return el;
  };
  const els = specs.map(make);
  const walk = els.filter((e) => e.text != null);
  let i = -1;
  const context = {
    NodeFilter: { SHOW_TEXT: 4 },
    getComputedStyle: (e) => ({ backgroundColor: 'rgba(0, 0, 0, 0)', color: 'rgb(255, 255, 255)', fontSize: '16px', fontWeight: '400', ...e.style }),
    document: {
      body,
      querySelector: () => null,
      createTreeWalker: () => ({ nextNode() { i++; this.currentNode = walk[i] ? { textContent: walk[i].text, parentElement: walk[i] } : null; return Boolean(walk[i]); }, currentNode: null }),
    },
  };
  return context;
}

test('the contrast expression runs the very functions above in a DOM and reports each text under AA', () => {
  const dom = fakeDom([
    { text: 'fine', style: { color: 'rgb(255, 255, 255)' } },
    { text: 'dim label', cls: 'muted small', id: 'dim', style: { color: 'rgb(90, 90, 90)' } },
    { text: 'big dim heading', style: { color: 'rgb(110, 110, 110)', fontSize: '30px' } },
    { text: 'on a panel', style: { backgroundColor: 'rgb(240, 240, 240)', color: 'rgb(255, 255, 255)' } },
    { text: 'hidden', hidden: true, style: { color: 'rgb(0, 0, 0)' } },
    { text: 'disabled', disabled: true, style: { color: 'rgb(0, 0, 0)' } },
    { text: 'empty box', empty: true, style: { color: 'rgb(0, 0, 0)' } },
    { text: '   ' },
  ]);
  const out = runIn(contrastExpression(), dom);
  assert.equal(out.checked, 4, 'hidden, disabled, zero-size and blank texts are not counted');
  assert.equal(out.failed, 2);
  assert.deepEqual(out.failures.map((f) => f.text), ['dim label', 'on a panel']);
  assert.equal(out.failures[0].selector, '#dim');
  assert.equal(out.failures[0].need, 4.5);
  assert.ok(out.failures[0].ratio < 4.5);
  assert.deepEqual([out.failures[0].fg, out.failures[0].bg], ['#5a5a5a', '#000000']);
  // The 30 px heading is large text: 3:1 on black is met by #6e6e6e (about 4.1:1), so it is not listed.
  assert.ok(!out.failures.some((f) => f.text === 'big dim heading'));
});

test('the contrast expression caps the failures it lists but counts them all, and says when the root is missing', () => {
  const many = Array.from({ length: 5 }, (_, k) => ({ text: `dim ${k}`, style: { color: 'rgb(60, 60, 60)' } }));
  const out = runIn(contrastExpression({ limit: 2 }), fakeDom(many));
  assert.equal(out.failed, 5);
  assert.equal(out.failures.length, 2);
  assert.deepEqual(runIn(contrastExpression({ root: '#nope' }), fakeDom([])), { checked: 0, failures: [], missing: true });
});

// ---------- knobs ----------

test('open() seeds Vim keys off, answers the first-run question, and stores a theme choice the app decodes', () => {
  assert.deepEqual(seedEntries(), { 'rsi.desktop.vimKeysAsked': 'answered', 'rsi.desktop.vimKeys': 'off' });
  assert.equal(seedEntries({ vim: true })['rsi.desktop.vimKeys'], 'on');
  assert.deepEqual(JSON.parse(seedEntries({ theme: 'light' })[THEME_STORAGE_KEY]), { theme: 'light', overrides: {} });
  assert.deepEqual(JSON.parse(seedEntries({ theme: { theme: 'high-contrast', overrides: { muted: '#aaaaaa' } } })[THEME_STORAGE_KEY]), { theme: 'high-contrast', overrides: { muted: '#aaaaaa' } });
  assert.equal(seedEntries({ seedStorage: { extra: '1' } }).extra, '1');
  assert.equal(THEME_STORAGE_KEY, 'rsi.desktop.theme.v1');
});

test('the system colour scheme is light, dark or untouched, and other media features pass through', () => {
  assert.deepEqual(mediaFeatures(), []);
  assert.deepEqual(mediaFeatures({ scheme: 'light' }), [{ name: 'prefers-color-scheme', value: 'light' }]);
  assert.deepEqual(mediaFeatures({ scheme: 'dark', media: [{ name: 'prefers-contrast', value: 'more' }] }), [{ name: 'prefers-color-scheme', value: 'dark' }, { name: 'prefers-contrast', value: 'more' }]);
  assert.throws(() => mediaFeatures({ scheme: 'sepia' }), /scheme must be/);
});

test('audit.mjs arguments: a command, --out, --sessions, --events, and flow FILE', () => {
  assert.equal(parseArgs([]).command, 'all');
  assert.deepEqual(parseArgs(['scale', '--sessions', '3000', '--events', '20000', '--out', '/x']), { command: 'scale', out: '/x', sessions: 3000, events: 20000, file: null, size: null, theme: null, slow: null, refuse: false, themes: null, overlay: null });
  assert.equal(parseArgs(['flow', 'a.mjs']).file, 'a.mjs');
  assert.throws(() => parseArgs(['flow']), /path of a .mjs/);
  assert.throws(() => parseArgs(['nope']), /sizes, themes, views, keys, scale, all or flow/);
  assert.throws(() => parseArgs(['sizes', '--sessions', 'x']), /whole numbers/);
  assert.throws(() => parseArgs(['sizes', '--bad']), /unknown argument/);
});

// ---------- scale fixtures ----------

test('scaleFixtures builds N sessions across projects, parented under epics, and one long conversation', () => {
  const fx = scaleFixtures({ sessions: 100, events: 40, projects: 3 });
  assert.equal(fx.sessions.length, 100);
  assert.equal(fx.projects.length, 3);
  assert.deepEqual(fx.fixtures.ListProjects(), [...fx.projects, HOME_PROJECT], 'the built-in RSI home is listed after the generated projects (desktop #319)');
  assert.equal(new Set(fx.sessions.map((s) => s.id)).size, 100, 'ids are unique');
  assert.ok(fx.sessions.every((s) => /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-8[0-9a-f]{3}-[0-9a-f]{12}$/.test(s.id)), 'ids are UUIDs');
  assert.ok(fx.sessions.every((s) => fx.projects.some((p) => p.id === s.project_id)));
  const epics = fx.sessions.filter((s) => s.session_kind === 'Epic');
  assert.equal(epics.length, 5);
  assert.ok(fx.sessions.filter((s) => s.parent_id).every((s) => epics.some((e) => e.id === s.parent_id)), 'every child hangs under an Epic');
  assert.ok(new Set(fx.sessions.map((s) => s.status)).size >= 5);
  assert.equal(fx.longSessionId, fx.sessions[7].id);
});

test('scaleFixtures serves the long conversation, honours since_sequence, and counts the polling reads', () => {
  const fx = scaleFixtures({ sessions: 20, events: 40 });
  assert.deepEqual(fx.counters, { ListSessions: 0, GetConversation: 0 });
  assert.equal(fx.fixtures.GetConversation({ session_id: fx.longSessionId }).length, 40);
  assert.deepEqual(fx.fixtures.GetConversation({ session_id: fx.longSessionId, since_sequence: 37 }).map((e) => e.sequence), [38, 39, 40]);
  assert.equal(fx.fixtures.GetConversation({ session_id: fx.sessions[1].id }).length, 2, 'any other session keeps the two-message conversation');
  fx.fixtures.ListSessions();
  assert.deepEqual(fx.counters, { ListSessions: 1, GetConversation: 3 });
  assert.equal(fx.fixtures.GetSession({ session_id: fx.sessions[3].id }).title, fx.sessions[3].title);
});

test('scaleFixtures only adds reads, is deterministic, and refuses nonsense sizes', () => {
  const baseSessions = JSON.stringify(FIXTURES.ListSessions());
  for (const m of Object.keys(scaleFixtures({ sessions: 2, events: 1 }).fixtures)) assert.ok(READ_METHODS.has(m), `${m} is not an allowlisted method`);
  assert.deepEqual(scaleFixtures({ sessions: 30, events: 12 }).sessions, scaleFixtures({ sessions: 30, events: 12 }).sessions);
  assert.deepEqual(longConversation('x', 9), longConversation('x', 9));
  assert.deepEqual(longConversation('x', 4).map((e) => e.event_type), ['Message', 'ToolUse', 'ToolResult', 'Message']);
  assert.deepEqual(longConversation('x', 0), []);
  assert.throws(() => scaleFixtures({ sessions: 0 }), /sessions must be/);
  assert.throws(() => scaleFixtures({ events: -1 }), /events must be/);
  assert.equal(scaleFixtures({ sessions: 3, events: 1, longIndex: 99 }).longSessionId, scaleFixtures({ sessions: 3 }).sessions[2].id, 'an index past the end is the last session');
  assert.equal(JSON.stringify(FIXTURES.ListSessions()), baseSessions, 'the base fixtures are untouched');
});

// ---------- the toolkit in a real browser ----------

const chrome = findChrome();
const SKIP = !chrome || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false;

test('open() applies size, theme and system scheme, stubs a write the page makes, and the contrast walker finds a planted miss', { skip: SKIP, timeout: 60000 }, async () => {
  const a = await open({ width: 1024, height: 640, theme: 'light', scheme: 'light', stubs: { send_message: { queued: true } } });
  try {
    const state = await a.page.eval(`({ w: innerWidth, h: innerHeight, light: matchMedia('(prefers-color-scheme: light)').matches, stored: localStorage.getItem('rsi.desktop.theme.v1'), vim: localStorage.getItem('rsi.desktop.vimKeys') })`);
    assert.deepEqual([state.w, state.h, state.light, state.vim], [1024, 640, true, 'off']);
    assert.deepEqual(JSON.parse(state.stored), { theme: 'light', overrides: {} });
    // The page's own Tauri surface reaches the stub bridge: the write is recorded and answered, never sent.
    const reply = await a.page.eval(`globalThis.__TAURI__.core.invoke('send_message', { sessionId: '${S1}', text: 'hi' })`);
    assert.deepEqual(reply, { queued: true });
    assert.deepEqual(a.writes().map((w) => [w.cmd, w.outcome]), [['send_message', 'stubbed']]);
    assert.ok(a.fake.received.every((r) => READ_METHODS.has(r.method)), `the daemon heard ${a.fake.received.map((r) => r.method)}`);
    // On-demand failure and a daemon that is down.
    a.bridge.failWrites('send_message', 'boom');
    assert.equal(await a.page.eval(`globalThis.__TAURI__.core.invoke('send_message', {}).then(() => 'ok', (e) => String(e))`), 'boom');
    a.bridge.setDown(true);
    assert.match(await a.page.eval(`globalThis.__TAURI__.core.invoke('list_sessions').then(() => 'ok', (e) => String(e))`), /cannot reach rsid/);
    a.bridge.setDown(false);
    // The walker: a planted grey-on-grey text is the only miss it adds.
    const before = await contrastAudit(a.page);
    await a.page.eval(`(() => { const p = document.createElement('p'); p.id = 'planted'; p.textContent = 'planted low contrast'; p.style.cssText = 'position:fixed;left:0;top:0;z-index:9;color:#777;background:#888'; document.body.append(p); })()`);
    const after = await contrastAudit(a.page);
    assert.equal(after.checked, before.checked + 1);
    assert.equal(after.failed, before.failed + 1);
    const planted = after.failures.find((f) => f.selector === '#planted');
    assert.ok(planted && planted.ratio < 1.5 && planted.need === 4.5, JSON.stringify(planted));
  } finally { await a.close(); }
});

test('the example flow runs through `audit.mjs flow`: Send is stubbed, then fails while rsid is down, with screenshots', { skip: SKIP, timeout: 60000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-flow-'));
  const lines = [];
  const log = console.log;
  console.log = (...a) => lines.push(a.join(' '));
  try { assert.equal(await auditMain(['flow', here('../scripts/audit-flow-example.mjs'), '--out', out]), 0); } finally { console.log = log; }
  assert.ok(lines.some((l) => l.startsWith('writes after Send: [{"cmd":"queue_operator_message","outcome":"stubbed"}]')), lines.join('\n'));
  assert.ok(lines.some((l) => l.startsWith('composer keeps the text while rsid is down: "again"')), lines.join('\n'));
  assert.ok(lines.some((l) => l === 'daemon-down card: true; Start rsid true; Retry now true'), lines.join('\n'));
  assert.ok(existsSync(join(out, 'flow-send.png')) && existsSync(join(out, 'flow-send-down.png')) && existsSync(join(out, 'flow-daemon-down-card.png')));
});

// ---------- slow and refused reads, fixtures that fail, Markdown in the shim (local #313) ----------

test('a refused read fails with the real backend text: daemon error <code>: <message> [<code>]; a typed code and next step travel with it', () => {
  assert.equal(refusalText('rsid is busy'), 'daemon error -32000: rsid is busy');
  assert.equal(refusalText({ message: 'row changed', code: 'stale_version' }), 'daemon error -32000: row changed [stale_version]');
  assert.equal(refusalText({ message: 'deploy draining', code: 'deploy_draining', nextAction: 'retry after the deploy settles', rpcCode: -32001 }), 'daemon error -32001: deploy draining | next_action: retry after the deploy settles [deploy_draining]');
});

test('read controls delay and refuse by method or for every read, change while the page runs, and record each call', async () => {
  const heard = [];
  const ctl = createReadControls();
  const call = ctl.wrap(async (m) => { heard.push(m); return { ok: m }; });
  const t = async (m) => { const t0 = Date.now(); const r = await call(m, {}).then((v) => v, (e) => e.message); return { r, ms: Date.now() - t0 }; };
  assert.deepEqual((await t('ListSessions')).r, { ok: 'ListSessions' }, 'no knob: the read goes through');
  ctl.delayMs = { ListMcpServers: 250 };
  assert.ok((await t('ListMcpServers')).ms >= 240, 'the named read waits');
  assert.ok((await t('ListSessions')).ms < 200, 'an unnamed read does not');
  ctl.delayMs = 120;
  assert.ok((await t('ListSessions')).ms >= 110, 'a number delays every read');
  ctl.delayMs = { '*': 100, ListSessions: 0 };
  assert.ok((await t('ListSessions')).ms < 90 && (await t('ListProjects')).ms >= 90, 'the wildcard is the default, a method overrides it');
  ctl.delayMs = 0;
  ctl.refuse.ListMcpServers = 'busy';
  assert.equal((await t('ListMcpServers')).r, 'daemon error -32000: busy');
  assert.deepEqual((await t('ListSessions')).r, { ok: 'ListSessions' }, 'only the named read is refused');
  ctl.refuse = { '*': { message: 'no', code: 'audit_refused' } };
  assert.equal((await t('ListSessions')).r, 'daemon error -32000: no [audit_refused]');
  ctl.refuse = {};
  assert.deepEqual((await t('ListSessions')).r, { ok: 'ListSessions' }, 'the refusal ends when it is cleared');
  assert.ok(!heard.includes('ListMcpServers') || heard.filter((m) => m === 'ListMcpServers').length === 1, 'a refused read is never forwarded');
  const refused = ctl.calls.filter((c) => c.refused).map((c) => c.method);
  assert.deepEqual(refused, ['ListMcpServers', 'ListSessions'], 'each refused call is recorded');
  assert.ok(ctl.calls.every((c) => Number.isFinite(c.at) && Number.isFinite(c.delayedMs)));
});

test('a fake-daemon fixture may be async, throw, reject or return an error object, and the fake keeps answering', async () => {
  const fake = await startFakeDaemon({
    Plain: () => ({ v: 1 }),
    Late: async () => { await sleep(120); return { v: 'late' }; },
    Throws: () => { throw new Error('boom'); },
    Rejects: async () => { throw new Error('nope'); },
    Errors: () => ({ error: { code: -32000, message: 'refused by fixture', data: { code: 'audit_refused' } } }),
    ErrorsNoCode: () => ({ error: { message: 'plain' } }),
    HasErrorField: () => ({ error: 'a string is a normal result', other: 1 }),
  });
  try {
    const ask = (m) => rpcCall(fake.socket, m, {}).then((v) => v, (e) => e.message);
    assert.deepEqual(await ask('Plain'), { v: 1 });
    const t0 = Date.now();
    assert.deepEqual(await ask('Late'), { v: 'late' });
    assert.ok(Date.now() - t0 >= 100, 'answered late');
    assert.match(await ask('Throws'), /^daemon error -32603: fixture Throws threw: boom$/);
    assert.match(await ask('Rejects'), /^daemon error -32603: fixture Rejects threw: nope$/);
    assert.equal(await ask('Errors'), 'daemon error -32000: refused by fixture [audit_refused]');
    assert.equal(await ask('ErrorsNoCode'), 'daemon error -32000: plain');
    assert.deepEqual(await ask('HasErrorField'), { error: 'a string is a normal result', other: 1 }, 'only {error:{message}} alone is an error answer');
    assert.match(await ask('Missing'), /^daemon error -32601: Method not found: Missing$/);
    assert.deepEqual(await ask('toString'), (await ask('toString')), 'an inherited name is not a fixture');
    assert.deepEqual(await ask('Plain'), { v: 1 }, 'the fake is still up after every failing fixture');
    assert.equal(fake.received.length, 11);
  } finally { await fake.stop(); }
});

test('--slow and --refuse parse for the views command only, and exclude each other', () => {
  assert.equal(parseSlow('3000'), 3000);
  assert.throws(() => parseSlow('abc'), /--slow takes/);
  assert.throws(() => parseSlow('0'), /--slow takes/);
  assert.throws(() => parseSlow(undefined), /--slow takes/);
  const slow = parseArgs(['views', '--slow', '3000', '--size', '760x480']);
  assert.deepEqual([slow.slow, slow.refuse], [3000, false]);
  assert.deepEqual([parseArgs(['views', '--refuse']).refuse, parseArgs(['views']).slow], [true, null]);
  assert.throws(() => parseArgs(['sizes', '--slow', '100']), /belong to the views command/);
  assert.throws(() => parseArgs(['themes', '--refuse']), /belong to the views command/);
  assert.throws(() => parseArgs(['views', '--slow', '100', '--refuse']), /pick one/);
});

test('the shim renders a small Markdown subset, escaping everything first', () => {
  assert.equal(renderMarkdownSubset('plain text'), '<p>plain text</p>');
  assert.equal(renderMarkdownSubset('a\nb'), '<p>a<br>b</p>');
  assert.equal(renderMarkdownSubset('Say **hi** to `x`\n\n- one\n- two\n\n1. a\n2. b'), '<p>Say <strong>hi</strong> to <code>x</code></p><ul><li>one</li><li>two</li></ul><ol><li>a</li><li>b</li></ol>');
  assert.equal(renderMarkdownSubset('```\nlet x = <1> & 2;\n```\nafter'), '<pre><code>let x = &lt;1&gt; &amp; 2;</code></pre><p>after</p>');
  assert.equal(renderMarkdownSubset('<script>alert(1)</script> **<b>**'), '<p>&lt;script&gt;alert(1)&lt;/script&gt; <strong>&lt;b&gt;</strong></p>');
  const bridge = createBridge({ call: async () => null });
  return bridge.invoke('render_markdown', { texts: ['- one\n- two'] }).then((html) => assert.deepEqual(html, ['<ul><li>one</li><li>two</li></ul>']));
});

test('open() takes reads: a refused read shows the real error text in the view, a delayed one draws late, and both are switched on and off live', { skip: SKIP, timeout: 120000 }, async () => {
  const a = await open({ width: 1024, height: 640 });
  try {
    assert.ok(a.reads.calls.length > 0 && a.reads.calls.every((c) => !c.refused), 'the boot reads went through');
    await openSessionByTitle(a.page, 'Fake waiting worker');
    const view = { id: 'admin-mcp', title: 'MCP servers…' };
    // Refused: the view shows the real backend format of the error.
    a.reads.refuse.ListMcpServers = { message: 'mcp read refused', code: 'audit_refused' };
    const refused = await probeViewOpen(a, view);
    assert.equal(refused.opened, true);
    assert.ok(refused.reads.includes('ListMcpServers'), JSON.stringify(refused.reads));
    assert.match(`${refused.toast} ${refused.errorLine}`, /mcp read refused/, JSON.stringify(refused));
    assert.ok(a.reads.calls.some((c) => c.method === 'ListMcpServers' && c.refused));
    // Delayed: the call is held for the delay and the view still opens once it returns.
    await a.page.send('Page.reload', {});
    await a.page.waitFor(`document.querySelectorAll('#session-list li').length > 0`, 15000);
    a.reads.refuse = {};
    a.reads.delayMs = { ListMcpServers: 1200 };
    await openSessionByTitle(a.page, 'Fake waiting worker');
    const slow = await probeViewOpen(a, view, { maxMs: 5000 });
    assert.equal(slow.opened, true);
    assert.ok(slow.firstDrawMs !== null, JSON.stringify(slow));
    assert.ok(a.reads.calls.some((c) => c.method === 'ListMcpServers' && c.delayedMs === 1200));
    assert.ok(a.fake.received.every((r) => READ_METHODS.has(r.method)), 'the daemon heard reads only');
  } finally { await a.close(); }
});

test('the views command with --slow and --refuse reports, per view, whether anything was drawn within 300 ms and the error text shown', { skip: SKIP, timeout: 240000 }, async () => {
  const views = [{ id: 'settings-open', title: 'Settings…' }, { id: 'admin-mcp', title: 'MCP servers…' }, { id: 'open-trash', title: 'Trash…' }];
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-reads-'));
  const refuse = await runViews({ out, views, fast: true, refuse: true });
  assert.equal(refuse.mode, 'refuse');
  assert.deepEqual(refuse.views.map((v) => v.view), views.map((v) => v.id));
  for (const v of refuse.views) {
    assert.equal(v.opened, true, `${v.view}: ${v.why}`);
    assert.equal(typeof v.drawnWithin300, 'boolean');
    assert.ok(v.reads.length > 0, `${v.view} made a read`);
  }
  const mcp = refuse.views.find((v) => v.view === 'admin-mcp');
  assert.match(`${mcp.toast} ${mcp.errorLine}`, /audit: this read is refused/, 'the refusal text is shown: the report carries it');
  const slow = await runViews({ out, views: [views[1]], fast: true, slowMs: 700 });
  assert.equal(slow.mode, 'slow-700');
  const row = slow.views[0];
  assert.ok(row.firstDrawMs === null || typeof row.firstDrawMs === 'number');
  assert.equal(row.drawnWithin300, row.firstDrawMs !== null && row.firstDrawMs <= 300);
  assert.ok(row.reads.includes('ListMcpServers'));
});

// Local #620 / hub #3659: a launch or reload whose document never came is loaded again, and a timeout names what happened.
function stalledPage({ comesAfter }) {
  const page = {
    navigations: [], waits: [],
    events: [{ method: 'Inspector.targetCrashed', params: {} }, { method: 'Network.loadingFailed', params: { url: 'http://127.0.0.1/app.js', errorText: 'net::ERR_ABORTED' } }],
    async waitFor(expression, ms, options) {
      page.waits.push({ ms, evalMs: options?.evalMs });
      return page.navigations.length >= comesAfter;
    },
    async send(method, params) { page.navigations.push({ method, params }); return {}; },
    async eval() { return { url: 'about:blank', readyState: 'complete' }; },
  };
  return page;
}

test('awaitApp navigates to the same address again when the document did not come, within the same budget', async () => {
  const page = stalledPage({ comesAfter: 2 });
  const r = await awaitApp(page, { ready: 'true', url: 'http://127.0.0.1:1/', failure: 'x', readyMs: 5000, attemptMs: 10 });
  assert.equal(r.navigations, 2);
  assert.deepEqual(page.navigations.map((n) => [n.method, n.params.url]), [['Page.navigate', 'http://127.0.0.1:1/'], ['Page.navigate', 'http://127.0.0.1:1/']]);
  assert.ok(page.waits.every((w) => w.ms <= 10 && w.evalMs > 0), 'each attempt is short and bounds its evaluations');
});

test('awaitApp times out with the navigations made, the page state and the DevTools crash and failed-load events', async () => {
  const page = stalledPage({ comesAfter: Infinity });
  await assert.rejects(awaitApp(page, { ready: 'true', url: 'http://127.0.0.1:1/', failure: 'the app was not ready within 60 ms', readyMs: 60, attemptMs: 20, extra: () => ({ chrome: 'still running (pid 1); stderr: (empty)' }) }), (error) => {
    assert.match(error.message, /^the app was not ready within 60 ms; state: /);
    assert.match(error.message, /"navigations":\d+/);
    assert.match(error.message, /Inspector\.targetCrashed/);
    assert.match(error.message, /net::ERR_ABORTED http:\/\/127\.0\.0\.1\/app\.js/);
    assert.match(error.message, /still running \(pid 1\)/);
    return true;
  });
});

test('transportReport reports log lines and nothing else for a healthy page', () => {
  assert.deepEqual(transportReport({ events: [{ method: 'Log.entryAdded', params: { entry: { level: 'info', text: 'hi' } } }] }), { log: ['info: hi'] });
  assert.deepEqual(transportReport({ events: [] }), {});
});
