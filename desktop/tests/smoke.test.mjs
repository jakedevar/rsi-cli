// The permission-free smoke harness (scripts/smoke*.mjs): the Tauri shim, the read-only invoke bridge,
// its secret and Origin gate, the fake daemon, and (when Chrome exists) one full headless run.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runInNewContext } from 'node:vm';
import { fileURLToPath } from 'node:url';
import {
  LOCAL_COMMANDS, READ_COMMANDS, READ_METHODS, authorizeInvoke, createBridge, redactConfig, rpcCall, shimSource, SHIM_ATTEMPTS,
} from '../scripts/smoke-bridge.mjs';
import { FIXTURES, SESSION_IDS, startFakeDaemon } from '../scripts/smoke-fake.mjs';
import { findChrome, newToken, serve } from '../scripts/smoke-lib.mjs';
import { GROUP_NAMES, PRELUDE, SMOKE_GROUPS, expectedRefusals, groupSteps, loadStepFactories, selectedIds, slowestSteps, validateSteps } from '../scripts/smoke-groups.mjs';
import { buildSteps, main as smokeMain, parseArgs } from '../scripts/smoke.mjs';

const S1 = SESSION_IDS.lead;
const here = (p) => fileURLToPath(new URL(p, import.meta.url));

function recorder() {
  const calls = [];
  const log = [];
  return { calls, log, bridge: createBridge({ call: async (m, p) => { calls.push({ m, p }); return { ok: m }; }, record: (e) => log.push(e) }) };
}

test('an allowed read forwards to the same rsid method the Rust handler calls, with its params', async () => {
  const { calls, log, bridge } = recorder();
  await bridge.invoke('get_conversation', { sessionId: S1, sinceSequence: 4 });
  await bridge.invoke('list_sessions');
  assert.deepEqual(calls, [
    { m: 'GetConversation', p: { session_id: S1, since_sequence: 4 } },
    { m: 'ListSessions', p: null },
  ]);
  assert.deepEqual(log.map((e) => e.outcome), ['forwarded', 'forwarded']);
});

test('a mutation is refused, recorded, and never forwarded', async () => {
  const { calls, log, bridge } = recorder();
  for (const cmd of ['send_message', 'launch_session', 'put_claude_hooks', 'set_provider_credential', 'update_daemon_config',
    'archive_session', 'delete_issue', 'interrupt_session', 'trigger_scheduled_job', 'not_a_command']) {
    await assert.rejects(bridge.invoke(cmd, { sessionId: S1, text: 'x' }), /not on the read-only allowlist/, cmd);
  }
  assert.deepEqual(calls, [], 'no daemon call was made');
  assert.equal(log.length, 10);
  assert.ok(log.every((e) => e.outcome === 'refused'));
});

test('inherited object keys are not commands', async () => {
  const { calls, bridge } = recorder();
  for (const cmd of ['__proto__', 'constructor', 'toString', 'hasOwnProperty']) {
    await assert.rejects(bridge.invoke(cmd, {}), /allowlist/, cmd);
  }
  assert.deepEqual(calls, []);
});

test('every allowlisted rsid method is a read, by name', () => {
  for (const m of READ_METHODS) assert.match(m, /^(Get|List|OperatorTodoList$|RemoteGetStatus$|ReadProjectFile$)/, m);
  assert.ok(![...READ_METHODS].some((m) => /^(Set|Put|Create|Update|Delete|Archive|Launch|Send|Interrupt|Trigger|Toggle|Purge|Upsert|Clear|Import|Answer)/.test(m)));
  for (const cmd of Object.keys(READ_COMMANDS)) assert.ok(READ_METHODS.has(READ_COMMANDS[cmd].method));
  // The shim-local commands touch no daemon: none may be a write that merely looks local.
  assert.deepEqual(Object.keys(LOCAL_COMMANDS).sort(), [
    'boot_report', 'choose_folder', 'daemon_service', 'graph_validate', 'live_link', 'manual_sections', 'notify_permission', 'notify_session', 'open_notification_settings', 'providers', 'pty_kill', 'pty_resize', 'pty_spawn', 'pty_write', 'render_markdown', 'restart_daemon', 'set_app_menu', 'set_attention_count', 'set_window_theme', 'start_daemon', 'sync_menu_state',
  ]);
});

test('arguments that carry a credential are refused even on an allowed command', async () => {
  const { calls, log, bridge } = recorder();
  await assert.rejects(bridge.invoke('list_sessions', { secret: 'x' }), /credential/);
  await assert.rejects(bridge.invoke('get_session', { sessionId: S1, nested: { apiKey: 'x' } }), /credential/);
  assert.deepEqual(calls, []);
  assert.ok(log.every((e) => e.outcome === 'refused'));
});

test('a malformed id is refused before any daemon call', async () => {
  const { calls, bridge } = recorder();
  await assert.rejects(bridge.invoke('get_session', { sessionId: 'not-a-uuid' }), /UUID/);
  assert.deepEqual(calls, []);
});

test('GetDaemonConfig reaches the page redacted', async () => {
  const bridge = createBridge({ call: async () => ({
    title_model: 'haiku', api_key: 'sk-live', 'x.bearer_token': 'b', remote_url: 'https://user:pw@host/p', nested: { password: 'p', keep: 1 }, empty_secret: '',
  }) });
  const out = await bridge.invoke('get_daemon_config');
  assert.equal(out.title_model, 'haiku');
  assert.equal(out.api_key, '••••');
  assert.equal(out['x.bearer_token'], '••••');
  assert.equal(out.remote_url, 'https://••••@host/p');
  assert.deepEqual(out.nested, { password: '••••', keep: 1 });
  assert.equal(out.empty_secret, '');
  assert.equal(redactConfig(null), null);
});

test('the gate needs POST, the per-run secret and the harness origin', () => {
  const ctx = { token: 't0k', origin: 'http://127.0.0.1:5555' };
  const ok = { method: 'POST', headers: { 'x-smoke-token': 't0k', origin: ctx.origin } };
  assert.deepEqual(authorizeInvoke(ok, ctx), { ok: true });
  assert.equal(authorizeInvoke({ ...ok, method: 'GET' }, ctx).reason, 'method');
  assert.equal(authorizeInvoke({ ...ok, headers: { origin: ctx.origin } }, ctx).reason, 'token');
  assert.equal(authorizeInvoke({ ...ok, headers: { ...ok.headers, 'x-smoke-token': 'nope' } }, ctx).reason, 'token');
  assert.equal(authorizeInvoke({ ...ok, headers: { 'x-smoke-token': 't0k' } }, ctx).reason, 'origin');
  assert.equal(authorizeInvoke({ ...ok, headers: { ...ok.headers, origin: 'http://evil.example' } }, ctx).reason, 'origin');
  assert.equal(authorizeInvoke({ ...ok, headers: { ...ok.headers, origin: 'http://127.0.0.1:1' } }, ctx).reason, 'origin');
});

test('the shim installs the only Tauri surface the frontend uses and carries the secret', async () => {
  const seen = [];
  const sandbox = {
    fetch: async (url, init) => { seen.push({ url, init }); return { json: async () => ({ result: [1, 2] }) }; },
    JSON,
  };
  sandbox.globalThis = sandbox;
  runInNewContext(shimSource('s3cr3t'), sandbox);
  assert.equal(typeof sandbox.__TAURI__.core.invoke, 'function');
  assert.equal(typeof sandbox.__TAURI__.event.listen, 'function');
  assert.deepEqual(await sandbox.__TAURI__.core.invoke('list_sessions'), [1, 2]);
  assert.equal(seen[0].url, '/__invoke');
  assert.equal(seen[0].init.method, 'POST');
  assert.equal(seen[0].init.headers['x-smoke-token'], 's3cr3t');
  assert.deepEqual(JSON.parse(seen[0].init.body), { cmd: 'list_sessions', args: null });
  const unlisten = await sandbox.__TAURI__.event.listen('daemon-event', () => {});
  assert.equal(typeof unlisten, 'function');
  const failing = { fetch: async () => ({ json: async () => ({ error: 'refused' }) }), JSON };
  failing.globalThis = failing;
  runInNewContext(shimSource('x'), failing);
  await assert.rejects(failing.__TAURI__.core.invoke('send_message'), (e) => e === 'refused');
  assert.ok(!shimSource('s').includes('RSI_SESSION_TOKEN'));
});

test('the shim re-sends an invoke whose request Chrome cancelled, under one call id, and reports a failure that persists', async () => {
  const seen = [];
  let failures = 2;
  const sandbox = {
    fetch: async (url, init) => {
      seen.push(init.headers['x-smoke-call']);
      if (failures-- > 0) throw new TypeError('Failed to fetch');
      return { json: async () => ({ result: 'ok' }) };
    },
    JSON, Math, setTimeout, performance,
  };
  sandbox.globalThis = sandbox;
  runInNewContext(shimSource('t'), sandbox);
  assert.equal(await sandbox.__TAURI__.core.invoke('list_labels'), 'ok');
  assert.equal(seen.length, 3);
  assert.equal(new Set(seen).size, 1, 'every attempt of one invoke carries the same call id');
  assert.deepEqual(JSON.parse(JSON.stringify(sandbox.__smokeInvokeRetries.map((r) => [r.cmd, r.attempt, r.error]))), [['list_labels', 1, 'TypeError: Failed to fetch'], ['list_labels', 2, 'TypeError: Failed to fetch']]);
  await sandbox.__TAURI__.core.invoke('list_labels');
  assert.notEqual(seen.at(-1), seen[0], 'the next invoke is a new call');
  failures = Infinity;
  seen.length = 0;
  await assert.rejects(sandbox.__TAURI__.core.invoke('list_labels'), /Failed to fetch/);
  assert.equal(seen.length, SHIM_ATTEMPTS);
});

test('the frontend reaches Tauri only in the four files whose surface the shim covers', () => {
  // A new file that touches __TAURI__ may need a shim member: this fails until the shim (and this list) is reviewed.
  const found = [];
  const walk = (dir) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith('.ts') && readFileSync(p, 'utf8').includes('__TAURI__')) found.push(p.slice(here('../src/').length));
    }
  };
  walk(here('../src'));
  assert.deepEqual(found.sort(), ['actions/terminal.ts', 'api.ts', 'live.ts', 'menus.ts']);
  const members = new Set();
  for (const f of found) for (const m of readFileSync(here(`../src/${f}`), 'utf8').matchAll(/__TAURI__\??\.(\w+)|tauri\(\)\.(\w+)|tauri\??\.(\w+)/g)) members.add(m[1] ?? m[2] ?? m[3]);
  assert.deepEqual([...members].filter(Boolean).sort(), ['core', 'event']);
});

test('the fake daemon speaks the daemon.rs framing and records what it receives', async () => {
  const fake = await startFakeDaemon();
  try {
    assert.deepEqual(await rpcCall(fake.socket, 'GetOperatorPause', { session_id: S1 }), { pause_level: 'none' });
    await assert.rejects(rpcCall(fake.socket, 'SendMessage', { text: 'x' }), /Method not found: SendMessage/);
    assert.deepEqual(fake.received.map((r) => r.method), ['GetOperatorPause', 'SendMessage']);
  } finally { await fake.stop(); }
  assert.ok(!existsSync(fake.socket));
});

test('the HTTP bridge: no secret or a foreign origin is refused, a read forwards, a mutation never reaches the daemon', { timeout: 10000 }, async (t) => {
  const fake = await startFakeDaemon();
  const log = [];
  const bridge = createBridge({ call: (m, p) => rpcCall(fake.socket, m, p), record: (e) => log.push(e) });
  const token = newToken();
  const dist = mkdtempSync(join(tmpdir(), 'rsi-smoke-dist-'));
  let server;
  try {
    const served = await serve({ dist, bridge, token, onRefusal: (e) => log.push(e), signal: t.signal });
    server = served.server;
    const origin = `http://127.0.0.1:${served.port}`;
    const post = (headers, body) => fetch(`${origin}/__invoke`, { method: 'POST', headers: { 'content-type': 'application/json', ...headers }, body: JSON.stringify(body), signal: t.signal });
    assert.equal(server.address().address, '127.0.0.1');
    const good = { 'x-smoke-token': token, origin };
    assert.equal((await post({ origin }, { cmd: 'list_labels' })).status, 403, 'no secret');
    assert.equal((await post({ 'x-smoke-token': 'wrong', origin }, { cmd: 'list_labels' })).status, 403, 'wrong secret');
    assert.equal((await post({ 'x-smoke-token': token, origin: 'http://evil.example' }, { cmd: 'list_labels' })).status, 403, 'foreign origin');
    assert.equal((await post({ 'x-smoke-token': token }, { cmd: 'list_labels' })).status, 403, 'no origin');
    assert.equal((await fetch(`${origin}/__invoke`, { headers: good })).status, 403, 'GET');
    assert.deepEqual(fake.received, [], 'nothing reached the daemon so far');

    const read = await (await post(good, { cmd: 'list_projects' })).json();
    assert.equal(read.result[0].name, 'Fake project');
    const write = await (await post(good, { cmd: 'send_message', args: { sessionId: S1, text: 'hi' } })).json();
    assert.match(write.error, /not on the read-only allowlist/);
    assert.deepEqual(fake.received.map((r) => r.method), ['ListProjects']);
    // A re-sent call id replays the first answer and reaches the daemon once.
    const again = { ...good, 'x-smoke-call': 'call-1' };
    const first = await (await post(again, { cmd: 'list_labels' })).json();
    const replay = await (await post(again, { cmd: 'list_labels' })).json();
    assert.deepEqual(replay, first);
    assert.deepEqual(fake.received.map((r) => r.method), ['ListProjects', 'ListLabels'], 'one call id runs once');
    assert.ok(log.some((e) => e.cmd === 'send_message' && e.outcome === 'refused'));
    assert.equal(log.filter((e) => e.cmd === '(request)').length, 5, 'every gate refusal is logged');
  } finally { server?.closeAllConnections(); server?.close(); await fake.stop(); }
});

test('findChrome reports nothing when no browser exists, and honours the override', () => {
  assert.equal(findChrome({ PATH: '/nope' }, () => false), null);
  assert.equal(findChrome({ RSI_SMOKE_CHROME: '/x/chrome', PATH: '' }, (p) => p === '/x/chrome'), '/x/chrome');
  assert.equal(findChrome({ PATH: '/usr/bin:/opt/bin' }, (p) => p === '/opt/bin/chromium'), '/opt/bin/chromium');
});

const STEPS = buildSteps({ live: false, bridgeLog: [] });
const step = (id, group, extra = {}) => ({ id, group, title: id, run: async () => ({ status: 'pass', note: '' }), ...extra });

test('every smoke step runs in exactly one group, and the groups add up to the whole checklist (local #250, #281)', () => {
  // A step with no group would pass every group test and never run: validateSteps names it.
  assert.deepEqual(validateSteps(STEPS), []);
  const groups = groupSteps(STEPS);
  const all = STEPS.map((s) => s.id);
  assert.deepEqual(GROUP_NAMES, ['core', 'settings', 'manager', 'keyboard', 'a11y']);
  assert.deepEqual(Object.keys(groups), GROUP_NAMES);
  // Each id is in exactly one group and the groups hold every step.
  assert.deepEqual(Object.values(groups).flat().sort(), [...all].sort());
  assert.ok(Object.values(groups).every((ids) => ids.length > 0), 'a group with no step runs nothing');
  assert.ok(!all.includes(PRELUDE), 'boot is the prelude every group runs, not a step');
  // A group keeps the order the full run visits its steps in.
  for (const name of GROUP_NAMES) assert.deepEqual(groups[name], all.filter((id) => selectedIds(STEPS, [name]).has(id)), `${name} is out of order`);
  assert.deepEqual(parseArgs([]).steps, null, 'no flag runs every step');
});

test('a step with no group, two groups, an unknown group or a repeated id fails and is named (local #281)', () => {
  assert.deepEqual(validateSteps([step('a', 'core'), step('b', 'keyboard')]), []);
  assert.match(validateSteps([step('lonely', undefined)])[0], /step "lonely" declares no group/);
  assert.match(validateSteps([step('lonely', '')])[0], /step "lonely" declares no group/);
  assert.match(validateSteps([step('twice', ['core', 'keyboard'])])[0], /step "twice" declares more than one group/);
  assert.match(validateSteps([step('twice', 'core', { groups: ['core', 'keyboard'] })])[0], /step "twice" declares more than one group/);
  assert.match(validateSteps([step('odd', 'nowhere')])[0], /step "odd" declares the unknown group "nowhere"/);
  assert.match(validateSteps([step('odd', 7)])[0], /step "odd" declares the unknown group 7/);
  // The same id in two groups is two steps with one id: a repeated id, whichever group each names.
  assert.match(validateSteps([step('dup', 'core'), step('dup', 'keyboard')])[0], /two steps share the id "dup"/);
  assert.match(validateSteps([step(PRELUDE, 'core')])[0], /prelude/);
  assert.equal(validateSteps([step('x', undefined), step('y', 'nowhere'), step('z', 'core')]).length, 2, 'every bad step is reported, not only the first');
});

test('groups, selection and expected refusals are derived from the steps themselves (local #281)', () => {
  const steps = [step('a', 'core'), step('b', 'keyboard', { refuses: ['w1'] }), step('c', 'core', { refuses: ['w2', 'w2'] }), step('d', 'a11y')];
  assert.deepEqual(groupSteps(steps), { core: ['a', 'c'], settings: [], manager: [], keyboard: ['b'], a11y: ['d'] });
  assert.deepEqual([...selectedIds(steps, ['core', 'a11y'])], ['a', 'c', 'd']);
  assert.deepEqual(expectedRefusals(steps, 'core'), ['w2: not on the read-only allowlist', 'w2: not on the read-only allowlist']);
  assert.deepEqual(expectedRefusals(steps, 'settings'), []);
});

test('a new step in its own file under scripts/smoke-steps joins the run without editing any existing file (local #281)', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'rsi-smoke-steps-'));
  try {
    assert.deepEqual(await loadStepFactories(join(dir, 'missing')), [], 'no directory means no extra steps');
    // Two slices, two files: neither edits the other's or a shared list.
    writeFileSync(join(dir, 'slice-b.mjs'), `export default (kit) => [{ id: 'step-b', group: 'keyboard', title: 'b', run: async () => kit.pass('b') }];\n`);
    writeFileSync(join(dir, 'slice-a.mjs'), `export default (kit) => [{ id: 'step-a', group: 'core', title: 'a', run: async () => kit.pass('a') }];\n`);
    writeFileSync(join(dir, '_ignored.mjs'), `throw new Error('files starting with _ are never loaded');\n`);
    writeFileSync(join(dir, 'notes.md'), 'not a step file\n');
    const factories = await loadStepFactories(dir);
    const extra = factories.flatMap((f) => f({ pass: (note) => ({ status: 'pass', note }) }));
    assert.deepEqual(extra.map((s) => s.id), ['step-a', 'step-b'], 'file-name order');
    assert.deepEqual(validateSteps([...STEPS, ...extra]), []);
    assert.deepEqual(groupSteps([...STEPS, ...extra]).keyboard.slice(-1), ['step-b']);
    writeFileSync(join(dir, 'bad.mjs'), 'export const steps = [];\n');
    await assert.rejects(loadStepFactories(dir), /must export default a function/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('slowestSteps names the slowest results first and skips those without a duration', () => {
  assert.deepEqual(slowestSteps([{ id: 'a', durationMs: 1500 }, { id: 'b', durationMs: 90000 }, { id: 'c' }, { id: 'd', durationMs: 400 }], 2), ['b 90.0 s', 'a 1.5 s']);
});

test('smoke arguments: --live, --out, --chrome; anything else is refused', () => {
  assert.deepEqual(parseArgs([]), { live: false, out: '/tmp/desktop-smoke', chrome: null, isolatedWrite: false, rsid: null, only: null, firstRun: false, steps: null });
  assert.deepEqual(parseArgs(['--live', '--out', '/x', '--chrome', '/c']), { live: true, out: '/x', chrome: '/c', isolatedWrite: false, rsid: null, only: null, firstRun: false, steps: null });
  assert.deepEqual(parseArgs(['--steps', 'core,keyboard']).steps, ['core', 'keyboard']);
  assert.throws(() => parseArgs(['--steps', 'nope']), new RegExp(`--steps takes ${GROUP_NAMES.join(', ')} \\(not nope\\)`));
  assert.throws(() => parseArgs(['--steps']), /--steps takes/);
  assert.throws(() => parseArgs(['--steps', 'core', '--first-run']), /--steps selects/);
  assert.throws(() => parseArgs(['--mutate']), /unknown argument/);
});

test('the usage line in smoke.mjs names only flags parseArgs accepts (local #234)', () => {
  const usage = readFileSync(here('../scripts/smoke.mjs'), 'utf8').split('\n').filter((l) => /^\/\/\s+node scripts\/smoke\.mjs/.test(l));
  assert.ok(usage.length >= 1, 'the header has no usage line');
  for (const line of usage) {
    // `--out DIR` takes a value (an upper-case placeholder follows the flag); `--live` does not. `--steps` validates its value against the group names.
    const flags = [...line.matchAll(/(--[a-z][a-z-]*)(\s+[A-Z]+)?/g)].map((m) => [m[1], !!m[2]]);
    assert.ok(flags.length > 0, line);
    for (const [flag, takesValue] of flags) {
      // parseArgs throws "unknown argument" for a flag it does not know.
      assert.doesNotThrow(() => parseArgs(takesValue ? [flag, flag === '--steps' ? GROUP_NAMES[0] : 'x'] : [flag]), `${flag} (in "${line.trim()}") is refused by parseArgs`);
    }
  }
  assert.throws(() => parseArgs(['--keep-going']), /unknown argument --keep-going/);
});

test('the manager, dashboard, trash and model-switch reads forward the Tauri handlers\' params and nothing else', async () => {
  const { calls, bridge } = recorder();
  const P = '11111111-1111-4111-8111-111111111111';
  await bridge.invoke('manager_tree', { after: null, limit: 200 });
  await bridge.invoke('manager_tree', { after: 'project:x', limit: 50 });
  await bridge.invoke('harness_manager_get', { projectId: P });
  await bridge.invoke('harness_manager_policy_get', { projectId: P });
  await bridge.invoke('harness_manager_scope_page', { projectId: P, afterId: null });
  await bridge.invoke('harness_manager_state', { projectId: P, section: 'decisions', cursor: null, limit: 32 });
  await bridge.invoke('global_manager_get');
  await bridge.invoke('global_manager_workspace');
  await bridge.invoke('global_manager_budget', { nodeId: null });
  await bridge.invoke('global_dashboard', { window: '7d' });
  await bridge.invoke('list_deleted_sessions', { projectId: null });
  await bridge.invoke('get_session_model_switch_options', { sessionId: P });
  assert.deepEqual(calls, [
    { m: 'GetManagerTree', p: { limit: 200 } },
    { m: 'GetManagerTree', p: { limit: 50, after: 'project:x' } },
    { m: 'GetHarnessManager', p: { project_id: P } },
    { m: 'GetHarnessManagerPolicy', p: { project_id: P } },
    { m: 'ListHarnessManagerScope', p: { project_id: P, limit: 64 } },
    { m: 'GetHarnessManagerState', p: { project_id: P, query: { section: 'decisions', limit: 32 } } },
    { m: 'GetGlobalManager', p: {} },
    { m: 'GetGlobalManagerWorkspace', p: {} },
    { m: 'GetGlobalManagerBudget', p: {} },
    { m: 'GetGlobalDashboard', p: { window: '7d' } },
    { m: 'ListDeletedSessions', p: {} },
    { m: 'GetSessionModelSwitchOptions', p: { session_id: P } },
  ]);
  // The Tauri builders refuse these before any daemon call; so does the bridge.
  const before = calls.length;
  await assert.rejects(bridge.invoke('manager_tree', { after: null, limit: 201 }), /limit/);
  await assert.rejects(bridge.invoke('harness_manager_state', { projectId: P, section: 'nope', cursor: null, limit: 32 }), /section/);
  await assert.rejects(bridge.invoke('global_dashboard', { window: '1y' }), /window/);
  await assert.rejects(bridge.invoke('harness_manager_get', { projectId: 'nope' }), /UUID/);
  assert.equal(calls.length, before);
});

test('budget_policies keeps only the mode and the policies of the model-control status, as budgets.rs does', async () => {
  const bridge = createBridge({ call: async (m, p) => ({ m, p, mode: 'normal', policies: [{ scope_kind: 'global' }], active_invocations: [1] }), record: () => {} });
  assert.deepEqual(await bridge.invoke('budget_policies'), { mode: 'normal', policies: [{ scope_kind: 'global' }] });
});

test('every manager view the smoke checks has a canned answer for each read it makes', () => {
  for (const m of ['GetManagerTree', 'GetHarnessManager', 'GetHarnessManagerPolicy', 'ListHarnessManagerScope', 'GetHarnessManagerState', 'GetGlobalManager',
    'GetGlobalManagerWorkspace', 'GetGlobalManagerBudget', 'GetGlobalDashboard', 'ListDeletedSessions', 'GetSessionModelSwitchOptions', 'GetModelControlStatus', 'GetUsageStats']) {
    assert.equal(typeof FIXTURES[m], 'function', `no canned answer for ${m}`);
    assert.ok(READ_METHODS.has(m), `${m} is not on the read allowlist`);
  }
  const waiting = FIXTURES.ListSessions().find((s) => s.status === 'WaitingApproval');
  assert.equal(waiting.pending_question.questions[0].options.length, 2);
  assert.equal(FIXTURES.ListDeletedSessions()[0].status, 'Deleted');
  assert.equal(FIXTURES.GetGlobalDashboard({ window: '1h' }).window, '1h');
  assert.equal(FIXTURES.GetGlobalDashboard({ window: '7d' }).window_seconds, 604800);
  const state = FIXTURES.GetHarnessManagerState({ query: { section: 'decisions' } });
  assert.equal(state.rows[0].status, 'pending');
});

test('every canned fixture is a read the bridge allows', () => {
  for (const m of Object.keys(FIXTURES)) assert.ok(READ_METHODS.has(m), `${m} is not an allowlisted method`);
});

const chrome = findChrome();
const SKIP = !chrome || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false;

// One test per group, each with its own budget (SMOKE_GROUPS[group].budgetMs). Measured on a laptop (local #281): core 24 s,
// settings 52 s, manager 28 s, keyboard 44 s, a11y 30 s. A group over its budget is too big: split it in scripts/smoke-groups.mjs.
// The ids and the refusals a group must report come from its steps (`group`, `refuses`), not from a list here.
for (const group of GROUP_NAMES) {
  const BUDGET_MS = SMOKE_GROUPS[group].budgetMs;
  test(`the headless smoke group "${group}" passes against the fake daemon (needs Chrome and a built dist)`, { skip: SKIP, timeout: BUDGET_MS }, async (t) => {
    const out = mkdtempSync(join(tmpdir(), `rsi-smoke-${group}-`));
    const heard = [];
    // A timeout kills the test without a report: print what the group had measured a few seconds before its budget ends.
    const warn = setTimeout(() => console.log(`smoke group "${group}" is nearly out of its ${BUDGET_MS / 1000} s budget; slowest so far: ${slowestSteps(heard).join(', ')}`), BUDGET_MS - 3000);
    let report;
    try {
      const code = await smokeMain(['--steps', group, '--out', out], { signal: t.signal, onStep: (r) => heard.push(r) });
      report = JSON.parse(readFileSync(join(out, 'report.json'), 'utf8'));
      try {
        assert.equal(code, 0, JSON.stringify(report.results.filter((r) => r.status === 'fail')));
        assert.equal(report.mode, 'fake');
        assert.deepEqual(report.steps, [group]);
        assert.deepEqual(report.results.map((r) => r.id), [PRELUDE, ...groupSteps(STEPS)[group]]);
        assert.ok(report.results.every((r) => r.status === 'pass'), JSON.stringify(report.results));
        assert.ok(report.results.every((r) => existsSync(r.screenshot)));
        assert.ok(report.results.every((r) => Number.isFinite(r.durationMs) && r.durationMs >= 0), 'every step records its duration');
        assert.deepEqual(report.refusals, expectedRefusals(STEPS, group));
        assert.ok(report.fakeDaemonReceived.every((m) => READ_METHODS.has(m)), `daemon saw ${report.fakeDaemonReceived}`);
      } catch (e) {
        console.log(`smoke group "${group}" failed; slowest steps: ${slowestSteps(report.results).join(', ')}`);
        throw e;
      }
    } finally { clearTimeout(warn); }
  });
}
