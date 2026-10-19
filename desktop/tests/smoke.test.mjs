// The permission-free smoke harness (scripts/smoke*.mjs): the Tauri shim, the read-only invoke bridge,
// its secret and Origin gate, the fake daemon, and (when Chrome exists) one full headless run.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runInNewContext } from 'node:vm';
import { fileURLToPath } from 'node:url';
import {
  LOCAL_COMMANDS, READ_COMMANDS, READ_METHODS, authorizeInvoke, createBridge, redactConfig, rpcCall, shimSource,
} from '../scripts/smoke-bridge.mjs';
import { FIXTURES, SESSION_IDS, startFakeDaemon } from '../scripts/smoke-fake.mjs';
import { findChrome, newToken, serve } from '../scripts/smoke-lib.mjs';
import { main as smokeMain, parseArgs } from '../scripts/smoke.mjs';

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
    'boot_report', 'graph_validate', 'live_link', 'providers', 'pty_kill', 'pty_resize', 'pty_spawn', 'pty_write', 'render_markdown', 'set_app_menu', 'set_attention_count', 'sync_menu_state',
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
    assert.ok(log.some((e) => e.cmd === 'send_message' && e.outcome === 'refused'));
    assert.equal(log.filter((e) => e.cmd === '(request)').length, 5, 'every gate refusal is logged');
  } finally { server?.closeAllConnections(); server?.close(); await fake.stop(); }
});

test('findChrome reports nothing when no browser exists, and honours the override', () => {
  assert.equal(findChrome({ PATH: '/nope' }, () => false), null);
  assert.equal(findChrome({ RSI_SMOKE_CHROME: '/x/chrome', PATH: '' }, (p) => p === '/x/chrome'), '/x/chrome');
  assert.equal(findChrome({ PATH: '/usr/bin:/opt/bin' }, (p) => p === '/opt/bin/chromium'), '/opt/bin/chromium');
});

test('smoke arguments: --live, --out, --chrome; anything else is refused', () => {
  assert.deepEqual(parseArgs([]), { live: false, out: '/tmp/desktop-smoke', chrome: null, isolatedWrite: false, rsid: null, only: null, firstRun: false });
  assert.deepEqual(parseArgs(['--live', '--out', '/x', '--chrome', '/c']), { live: true, out: '/x', chrome: '/c', isolatedWrite: false, rsid: null, only: null, firstRun: false });
  assert.throws(() => parseArgs(['--mutate']), /unknown argument/);
});

test('every canned fixture is a read the bridge allows', () => {
  for (const m of Object.keys(FIXTURES)) assert.ok(READ_METHODS.has(m), `${m} is not an allowlisted method`);
});

const chrome = findChrome();
test('the full headless smoke passes against the fake daemon (needs Chrome and a built dist)', { skip: !chrome || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false, timeout: 120000 }, async (t) => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-smoke-out-'));
  const code = await smokeMain(['--out', out], { signal: t.signal });
  const report = JSON.parse(readFileSync(join(out, 'report.json'), 'utf8'));
  assert.equal(code, 0, JSON.stringify(report.results.filter((r) => r.status === 'fail')));
  assert.equal(report.mode, 'fake');
  assert.deepEqual(report.results.map((r) => r.id), [
    'boot', 'session-list', 'open-session', 'issues-workspace', 'command-mode', 'help', 'settings', 'hooks', 'providers', 'bridges', 'remote-button-keys', 'schedules-button-keys', 'dialog-vim-gate', 'dialog-hints-follow-vim', 'settings-button-keys', 'terminal',
  ]);
  assert.ok(report.results.every((r) => r.status === 'pass'), JSON.stringify(report.results));
  assert.ok(report.results.every((r) => existsSync(r.screenshot)));
  // The one refusal is the remote-button-keys positive control: the dialog's own Enter tries the trust write.
  assert.deepEqual(report.refusals, ['remote_set_config: not on the read-only allowlist', 'toggle_scheduled_job: not on the read-only allowlist']);
  assert.ok(report.fakeDaemonReceived.every((m) => READ_METHODS.has(m)), `daemon saw ${report.fakeDaemonReceived}`);
});
