// The audit tooling fixes of local #298: the rsid-down switch, the reads a view makes on open, the scale step's filter wait,
// and the every-view walk. The pure parts run anywhere; the browser tests skip without Chrome or dist.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { LOCAL_COMMANDS, READ_COMMANDS, READ_METHODS, createBridge } from '../scripts/smoke-bridge.mjs';
import { FIXTURES } from '../scripts/smoke-fake.mjs';
import { findChrome } from '../scripts/smoke-lib.mjs';
import { AUDIT_LOCAL_COMMANDS, DOWN_MESSAGE, GET_LIST_WRITES, createStubBridge } from '../scripts/smoke-stub-bridge.mjs';
import { open, openPaletteAction, openSessionByTitle, sleep, waitForRowCount } from '../scripts/audit-lib.mjs';
import { parseArgs, parseSize, runScale, runThemes, runViews, themeRun } from '../scripts/audit.mjs';
import { AUDIT_SKIPPED, AUDIT_VIEWS, auditViews, looksLikeAView, unclassifiedViews } from '../scripts/audit-views.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

// ---------- 1. rsid down ----------

test('setDown answers daemon_info like the real backend and throws for everything else; failBackend throws for daemon_info too', async () => {
  const bridge = createStubBridge({ call: async () => ({ status: 'ok' }) });
  bridge.setDown(true);
  assert.deepEqual(await bridge.invoke('daemon_info'), { socket: 'daemon.sock', connected: false, error: DOWN_MESSAGE });
  for (const cmd of ['list_sessions', 'get_session', 'send_message', 'list_mcp_servers']) {
    await assert.rejects(bridge.invoke(cmd, {}), (e) => e.message === DOWN_MESSAGE, cmd);
  }
  assert.equal(await bridge.invoke('live_link'), false, 'a shim-local command still answers');
  bridge.setDown(false);
  assert.equal((await bridge.invoke('daemon_info')).connected, true);
  bridge.failBackend();
  assert.equal(bridge.isBackendFailed(), true);
  await assert.rejects(bridge.invoke('daemon_info'), (e) => e.message === DOWN_MESSAGE, 'the old behaviour: the backend itself did not answer');
  await assert.rejects(bridge.invoke('list_sessions'), /cannot reach rsid/);
  bridge.failBackend(false);
  assert.equal((await bridge.invoke('daemon_info')).connected, true);
});

// ---------- 2. reads ----------

const srcFiles = (dir) => readdirSync(dir).flatMap((n) => {
  const p = join(dir, n);
  return statSync(p).isDirectory() ? srcFiles(p) : n.endsWith('.ts') ? [p] : [];
});

test('every list_* / get_* command desktop/src names is a read, a shim-local command, or listed as a write', () => {
  const named = new Set();
  for (const f of srcFiles(here('../src'))) {
    for (const m of readFileSync(f, 'utf8').matchAll(/['"`]((?:list|get)_[a-z0-9_]+)['"`]/g)) named.add(m[1]);
  }
  assert.ok(named.size > 40, `the scan found ${named.size} commands`);
  const unlisted = [...named].filter((c) => !Object.hasOwn(READ_COMMANDS, c) && !Object.hasOwn(LOCAL_COMMANDS, c) && !Object.hasOwn(AUDIT_LOCAL_COMMANDS, c) && !GET_LIST_WRITES.has(c));
  assert.deepEqual(unlisted, [], `these would be stubbed as writes and answered null: add each to READ_COMMANDS (with a fixture) or LOCAL_COMMANDS: ${unlisted.join(', ')}`);
  assert.ok(Object.hasOwn(LOCAL_COMMANDS, 'manual_sections'), 'Open manual reads manual_sections');
  for (const cmd of Object.keys(READ_COMMANDS)) assert.ok(READ_METHODS.has(READ_COMMANDS[cmd].method));
});

/** What each command a view makes on open is called with, and a check on the answer. */
const OPEN_READS = {
  list_mcp_servers: [{}, (r) => r.servers.length >= 2],
  list_satellite_peers: [{}, (r) => r.peers[0].links.length > 0],
  get_satellite_scope_status: [{}, (r) => Array.isArray(r.inbound)],
  get_satellite_inbound_policy: [{}, (r) => r.scope_roots.length > 0],
  list_hub_satellite_sessions: [{ peerId: 'abababab-abab-4aba-8aba-abababababab', offset: 0 }, (r) => r.sessions.length > 0],
  list_legacy_scratch: [{}, (r) => r.candidates.some((c) => c.blocker) && r.candidates.some((c) => !c.blocker)],
  list_source_worktree_cohorts: [{}, (r) => r.length > 0],
  list_source_worktree_settlement_runs: [{}, (r) => r.length > 0],
  get_source_worktree_settlement_run: [{ runId: 'ffffffff-ffff-4fff-8fff-ffffffffffff' }, (r) => r.run_id === 'ffffffff-ffff-4fff-8fff-ffffffffffff'],
  get_drain_restart_status: [{}, (r) => r.state === 'idle'],
  list_skills: [{}, (r) => r.skills.length >= 2],
  get_skill: [{ name: 'code-review' }, (r) => r.name === 'code-review' && r.content.length > 20],
  list_prompt_files: [{}, (r) => r.files.length >= 2],
  get_prompt_file: [{ name: 'release-notes.md' }, (r) => r.content.length > 10],
  get_entity_card: [{ entityType: 'project', entityId: 'p' }, (r) => r.facts.length > 0],
  manual_sections: [{}, (r) => r.length >= 2 && r.every((s) => s.title && s.html)],
  // Found by the every-view walk (`stubbedOnOpen`): reads that are not list_* / get_* by name.
  discover_models: [{ provider: 'Claude' }, (r) => r.models.length >= 2],
  fleet_overview: [{}, (r) => r.agents.length > 0 && r.groups.length > 0 && r.totals.length === 3],
  portfolio_nodes_list: [{ includeRevoked: false }, (r) => r.nodes.length > 0],
  manager_nodes_list: [{ projectId: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', after: null }, (r) => r.rows.length > 0],
  friction_rollup: [{ windowHours: 24 }, (r) => r.rows.length > 0],
  operator_escalations: [{ includeClosed: false }, (r) => r.escalations.length > 0 && r.notices.length > 0],
  operator_restart_status: [{}, (r) => r.pending === false && Array.isArray(r.blockers)],
  mgr_policy_classify: [{ policy: {}, scopeMode: 'project' }, (r) => r.preset === 'execute' && Array.isArray(r.suggestions)],
  list_provider_credentials: [{}, (r) => r.credentials.length >= 2],
  get_provider_login_state: [{}, (r) => r.providers.length >= 2],
  get_provider_contract_state: [{}, (r) => r.providers.length >= 2],
  cold_archive_status: [{}, (r) => r.totals.cold_sessions > 0 && r.last_run.outcome === 'ok'],
};

test('the reads a view makes on open return realistic fixture data through the real bridge, not null or an empty list', async () => {
  const bridge = createBridge({ call: async (m, p) => { assert.ok(FIXTURES[m], `${m} has a fixture`); return FIXTURES[m](p); } });
  const wrap = createStubBridge({ call: async (m, p) => FIXTURES[m](p) });
  assert.ok(Object.keys(OPEN_READS).length >= 27);
  for (const [cmd, [args, ok]] of Object.entries(OPEN_READS)) {
    const result = Object.hasOwn(AUDIT_LOCAL_COMMANDS, cmd) ? await wrap.invoke(cmd, args) : await bridge.invoke(cmd, args);
    assert.ok(result !== null && ok(result), `${cmd}: ${JSON.stringify(result)?.slice(0, 120)}`);
    assert.deepEqual(await wrap.invoke(cmd, args), result, `${cmd} answers the same through the stub bridge`);
    assert.ok(Object.hasOwn(READ_COMMANDS, cmd) || Object.hasOwn(LOCAL_COMMANDS, cmd) || Object.hasOwn(AUDIT_LOCAL_COMMANDS, cmd), `${cmd} is a read or local, not a stubbed write`);
  }
  assert.deepEqual(wrap.writes(), [], 'none of them is a write');
});

test('a read whose arguments are malformed is refused, not forwarded', async () => {
  const bridge = createBridge({ call: async () => { throw new Error('must not be called'); } });
  await assert.rejects(bridge.invoke('list_hub_satellite_sessions', { peerId: 'nope', offset: 0 }), /peer_id must be a UUID/);
  await assert.rejects(bridge.invoke('get_source_worktree_settlement_run', { runId: 'x' }), /run_id must be a UUID/);
  await assert.rejects(bridge.invoke('get_skill', {}), /name is malformed/);
});

// ---------- 3. the scale step waits for the filter ----------

/** A page whose row count follows `rows(t)`, with `t` ms since the page was made. */
function fakeRowsPage(rows) {
  const t0 = Date.now();
  return { eval: async () => rows(Date.now() - t0) };
}

test('waitForRowCount waits out a debounce and reports the settled count and the time it took', async () => {
  const page = fakeRowsPage((t) => (t < 200 ? 800 : t < 260 ? 400 : 131));
  const r = await waitForRowCount(page, 'li', { from: 800, timeoutMs: 3000, stableMs: 100, stepMs: 10 });
  assert.equal(r.settled, true);
  assert.equal(r.rows, 131, 'the count after the filter applied, not the 800 read before the debounce');
  assert.ok(r.ms >= 190 && r.ms < 1500, `${r.ms} ms`);
});

test('waitForRowCount says so when the count never moves, and refuses a call with no starting count', async () => {
  const r = await waitForRowCount(fakeRowsPage(() => 800), 'li', { from: 800, timeoutMs: 250, stepMs: 10 });
  assert.equal(r.settled, false);
  assert.equal(r.rows, 800);
  await assert.rejects(waitForRowCount(fakeRowsPage(() => 1), 'li', {}), /needs the row count/);
});

// ---------- 3b. the session row is waited for (local #314) ----------

/** A page whose session row is drawn `rowAfterMs` after it was made; clicks are recorded. */
function slowRowPage(rowAfterMs) {
  const t0 = Date.now();
  const clicks = [];
  const drawn = () => Date.now() - t0 >= rowAfterMs;
  return {
    clicks,
    eval: async (expr) => (expr.includes('#session-list li') && expr.includes('innerText.includes') ? (drawn() ? { x: 70, y: 22 } : null) : JSON.stringify({ rows: ['▾'], filter: '' })),
    waitFor: async (expr, ms) => { const end = Date.now() + ms; while (Date.now() < end) { if (drawn()) return true; await sleep(10); } return false; },
    send: async (type, p) => { clicks.push([type, p.type]); },
  };
}

test('openSessionByTitle waits for a row that paints late instead of reading the list once, and names what the list showed when it never comes', async () => {
  const late = slowRowPage(400);
  await openSessionByTitle(late, 'Fake waiting worker');
  assert.deepEqual(late.clicks.map((c) => c[1]), ['mousePressed', 'mouseReleased'], 'the row appeared after 400 ms and was clicked');
  const never = slowRowPage(60000);
  await assert.rejects(openSessionByTitle(never, 'Fake waiting worker', { waitMs: 150 }), (e) => /no session row containing "Fake waiting worker" within 150 ms \(the list showed .*"rows":\["▾"\]/.test(e.message));
  assert.deepEqual(never.clicks, []);
});

// ---------- 4. every view ----------

test('--size, --theme and the views command parse; a bad size or theme is refused', () => {
  assert.deepEqual(parseSize('760x480'), [760, 480]);
  assert.throws(() => parseSize('760'), /WIDTHxHEIGHT/);
  assert.throws(() => parseSize('axb'), /WIDTHxHEIGHT/);
  const o = parseArgs(['views', '--size', '1280x800', '--theme', 'light', '--out', '/x']);
  assert.deepEqual([o.command, o.size, o.theme, o.out], ['views', [1280, 800], 'light', '/x']);
  assert.throws(() => parseArgs(['sizes', '--size', '1024x640']), /belong to the views command/);
  assert.throws(() => parseArgs(['views', '--theme', 'Not A Theme']), /--theme takes/);
  assert.deepEqual(themeRun('dark'), themeRun(null));
  assert.deepEqual(themeRun('system-light'), { name: 'default-system-light', theme: null, scheme: 'light' });
  assert.deepEqual(themeRun('high-contrast'), { name: 'high-contrast', theme: 'high-contrast', scheme: 'dark' });
  assert.equal(themeRun('light').scheme, 'light');
});

const built = existsSync(here('../dist/registry.js'));
const registry = async () => {
  const dist = new URL('../dist/', import.meta.url);
  const r = await import(new URL('registry.js', dist));
  await import(new URL('actions/index.generated.js', dist));
  return r;
};

test('the view list names registered actions, once each, and covers the audit 2 set of 38', { skip: built ? false : 'no dist: run npm run build' }, async () => {
  const reg = await registry();
  const views = await auditViews(reg);
  assert.equal(views.length, AUDIT_VIEWS.length);
  assert.equal(new Set(AUDIT_VIEWS).size, AUDIT_VIEWS.length, 'no view twice');
  assert.ok(views.length >= 38, `${views.length} views`);
  for (const title of ['Settings…', 'Issues workspace', 'Global dashboard', 'MCP servers…', 'Satellite registry…', 'Open manual', 'Skill settings…', 'Choose theme…']) {
    assert.ok(views.some((v) => v.title === title), `the audit walks ${title}`);
  }
  await assert.rejects(auditViews({ ACTIONS: [] }), /no longer registers/);
});

test('an action that looks like it opens a screen is either audited or skipped with a reason, so a new view cannot be forgotten', { skip: built ? false : 'no dist: run npm run build' }, async () => {
  const reg = await registry();
  assert.deepEqual(unclassifiedViews(reg), [], 'classify each in scripts/audit-views.mjs: AUDIT_VIEWS (it opens a view) or AUDIT_SKIPPED (with the reason)');
  assert.ok(AUDIT_SKIPPED.every(([, why]) => typeof why === 'string' && why.length > 10));
  assert.equal(looksLikeAView({ id: 'brand-new', title: 'Brand new thing…' }), true);
  assert.deepEqual(unclassifiedViews({ ACTIONS: [{ id: 'brand-new', title: 'Brand new thing…' }, { id: 'refresh', title: 'Refresh' }] }), ['brand-new']);
});

// ---------- the fixes in a real browser ----------

const SKIP = !findChrome() || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false;
const quiet = async (fn) => { const log = console.log; console.log = () => {}; try { return await fn(); } finally { console.log = log; } };

test('setDown shows the daemon-down card with Start rsid and Retry now; failBackend shows the backend-error card', { skip: SKIP, timeout: 90000 }, async () => {
  const a = await open({ width: 1024, height: 640 });
  try {
    await openSessionByTitle(a.page, 'Fake worker');
    a.bridge.setDown(true);
    assert.ok(await a.page.waitFor(`document.body.innerText.includes('rsid is not running')`, 20000), 'the daemon-down card');
    const buttons = await a.page.eval(`[...document.querySelectorAll('button')].filter((b) => b.checkVisibility()).map((b) => b.innerText.trim())`);
    assert.ok(buttons.includes('Start rsid') && buttons.includes('Retry now'), buttons.join(' | '));
    assert.ok(!(await a.page.eval(`document.body.innerText.includes('Quit and reopen')`)), 'not the backend-error card');
    a.bridge.setDown(false);
    assert.ok(await a.page.waitFor(`!document.body.innerText.includes('rsid is not running')`, 20000), 'the card goes away when rsid is back');
    a.bridge.failBackend();
    assert.ok(await a.page.waitFor(`document.body.innerText.includes('did not answer') || document.body.innerText.includes('Quit and reopen')`, 20000), 'the backend-error card');
  } finally { await a.close(); }
});

test('the views Actions opens with data (MCP servers, Satellite registry, Open manual, Skill settings) show it, with no error toast', { skip: SKIP, timeout: 120000 }, async () => {
  const a = await open({ width: 1024, height: 640 });
  try {
    const shows = { 'MCP servers…': 'github', 'Satellite registry…': 'mac-mini', 'Open manual': 'Getting started', 'Skill settings…': 'code-review' };
    for (const [title, text] of Object.entries(shows)) {
      const r = await openPaletteAction(a.page, title);
      assert.equal(r.opened, true, `${title}: ${r.why}`);
      assert.ok(await a.page.waitFor(`[...document.querySelectorAll('dialog[open]')].some((d) => d.innerText.includes(${JSON.stringify(text)}))`, 5000), `${title} shows ${text}`);
      const toast = await a.page.eval(`document.getElementById('toast')?.checkVisibility() ? document.getElementById('toast').innerText : ''`);
      assert.equal(toast, '', `${title} raised no error: ${toast}`);
      await a.page.key('Escape');
      await sleep(300);
    }
    assert.deepEqual(a.writes(), []);
  } finally { await a.close(); }
});

test('openPaletteAction reports a row that does not exist instead of throwing, and leaves the palette closed', { skip: SKIP, timeout: 60000 }, async () => {
  const a = await open({ width: 1024, height: 640 });
  try {
    const r = await openPaletteAction(a.page, 'No such action…');
    assert.equal(r.opened, false);
    assert.match(r.why, /no palette row titled "No such action…"/);
    assert.equal(await a.page.eval(`document.getElementById('palette')?.open ?? false`), false);
  } finally { await a.close(); }
});

test('runViews writes one row per view with contrast, cut-off controls and scroll, and an unreachable view is a row, not a crash', { skip: SKIP, timeout: 120000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-views-'));
  const views = [{ id: 'admin-mcp', title: 'MCP servers…' }, { id: 'cm-manual', title: 'Open manual' }, { id: 'issues-new', title: 'New Issue…', before: 'Issues workspace' }, { id: 'gone', title: 'A view that is not there…' }];
  const r = await runViews({ out, size: [760, 480], theme: 'light', views });
  assert.deepEqual([r.width, r.height, r.theme], [760, 480, 'light']);
  assert.deepEqual(r.views.map((v) => v.view), ['admin-mcp', 'cm-manual', 'issues-new', 'gone']);
  const [mcp, manual, newIssue, gone] = r.views;
  for (const v of [mcp, manual, newIssue]) {
    assert.equal(v.opened, true, v.why);
    assert.ok(v.checked > 20, `${v.view} checked ${v.checked} texts`);
    assert.ok(Array.isArray(v.cutOff) && v.doc.vw === 760 && v.doc.vh === 480 && Array.isArray(v.overflow));
    assert.ok(existsSync(join(out, `view-light-760x480-${v.view}.png`)));
  }
  assert.equal(gone.opened, false);
  assert.match(gone.why, /no palette row titled/);
});

test('no main view opens a read the bridge does not list: nothing is stubbed while a view opens, and none shows a TypeError', { skip: SKIP || !built ? 'runner_unavailable: no Chrome or no dist' : false, timeout: 600000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-allviews-'));
  const r = await runViews({ out, fast: true });
  assert.equal(r.views.length, AUDIT_VIEWS.length, 'one row per view');
  const unopened = r.views.filter((v) => !v.opened).map((v) => `${v.view}: ${v.why}`);
  assert.deepEqual(unopened, [], 'every main view opens');
  const stubbed = r.views.filter((v) => v.stubbedOnOpen.length).map((v) => `${v.view}: ${v.stubbedOnOpen.join(', ')}`);
  assert.deepEqual(stubbed, [], 'a command stubbed while a view opens is a read missing from READ_COMMANDS (and a fixture in smoke-fake.mjs)');
  const broken = r.views.filter((v) => /TypeError|Cannot read properties|not support that action/.test(v.toast)).map((v) => `${v.view}: ${v.toast}`);
  assert.deepEqual(broken, [], 'a view that fails on a missing fixture hides the real view');
});

/** The views that drew nothing for as long as rsid took to answer, before local #310 gave them one shared frame (view-load.ts). */
const FORMERLY_BLANK = [
  'mgr-restart', 'admin-schedules', 'admin-remote', 'admin-satellites', 'admin-mcp', 'admin-provider-keys', 'admin-scratch', 'admin-settlement',
  'open-providers', 'open-bridges', 'open-hooks', 'open-trash', 'pending-messages', 'switch-model', 'prompt-creator', 'cm-cold-archive', 'card', 'card-user',
];

const formerlyBlank = async () => {
  const all = await auditViews(await registry());
  return FORMERLY_BLANK.map((id) => all.find((v) => v.id === id));
};

test('with every read delayed, each view that used to stay blank shows a frame within 300 ms, at 760x480 (local #310)', { skip: SKIP || !built ? 'runner_unavailable: no Chrome or no dist' : false, timeout: 400000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-slowframe-'));
  const r = await runViews({ out, size: [760, 480], fast: true, slowMs: 1500, views: await formerlyBlank() });
  assert.equal(r.views.length, FORMERLY_BLANK.length);
  assert.deepEqual(r.views.filter((v) => !v.opened).map((v) => `${v.view}: ${v.why}`), [], 'every view opens');
  assert.deepEqual(r.summary.blank, [], 'no view is blank at 300 ms while its reads are slow');
});

test('with every read refused, each view that used to stay blank says what rsid said and offers Retry, at 760x480 (local #310)', { skip: SKIP || !built ? 'runner_unavailable: no Chrome or no dist' : false, timeout: 400000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-refuseframe-'));
  const r = await runViews({ out, size: [760, 480], fast: true, refuse: true, views: await formerlyBlank() });
  assert.deepEqual(r.views.filter((v) => !v.opened).map((v) => `${v.view}: ${v.why}`), [], 'every view opens');
  assert.deepEqual(r.summary.silentRefusals, [], 'no refused read is silent');
  const noRetry = r.views.filter((v) => !v.retry).map((v) => v.view);
  assert.deepEqual(noRetry, [], 'a refused read offers Retry');
  const noWords = r.views.filter((v) => !/refus/i.test(`${v.toast} ${v.errorLine}`)).map((v) => v.view);
  assert.deepEqual(noWords, [], 'the daemon\'s message is shown');
});

test('themes walks every view in each theme (two themes here, three views), one row per view', { skip: SKIP, timeout: 180000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-themes-'));
  const views = [{ id: 'settings-open', title: 'Settings…' }, { id: 'open-trash', title: 'Trash…' }, { id: 'admin-satellites', title: 'Satellite registry…' }];
  const runs = [themeRun('dark'), themeRun('high-contrast')];
  const results = await runThemes({ out, runs, views });
  assert.deepEqual(results.map((t) => t.name), ['default-dark', 'high-contrast']);
  for (const t of results) {
    assert.deepEqual(t.views.map((v) => v.view), views.map((v) => v.id), `${t.name} covers every listed view`);
    assert.ok(t.views.every((v) => v.opened && v.checked > 10), `${t.name}: ${JSON.stringify(t.notes)}`);
  }
});

test('the scale step waits for the filter and reports its settle time and the filtered row count', { skip: SKIP, timeout: 180000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-scale-'));
  const r = await quiet(() => runScale({ out, sessions: 300, events: 120 }));
  assert.deepEqual(r.notes, []);
  const f = r.filterTyping6;
  assert.equal(f.rowsBefore, 300);
  assert.equal(f.settled, true);
  assert.ok(f.rowsAfter > 0 && f.rowsAfter < 300, `the filter had applied: ${f.rowsAfter} rows`);
  assert.ok(Number.isFinite(f.settleMs) && f.settleMs >= f.typingMs, `settle time ${f.settleMs} ms`);
});
