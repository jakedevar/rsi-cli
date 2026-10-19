// The real-rsid end-to-end smoke (hub #3105, local #115): the isolated-write bridge mode and the isolated
// daemon it demands, the synthetic-credential and write allowlists, the PARITY coverage audit, and (opt in)
// one full run against a built rsid in headless Chrome.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { chmodSync, existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  READ_METHODS, SYNTHETIC_SECRET, WRITE_COMMANDS, WRITE_METHODS, createBridge, redactCredentialList, rpcCall, validateWorkflowLocal,
} from '../scripts/smoke-bridge.mjs';
import { OFFLINE_KEY_CHECK, judgeEgress, parseLsofDirect, parseProxyRequest, startEgressGuard } from '../scripts/smoke-egress.mjs';
import {
  MARKER_FILE, MARKER_TEXT, assertIsolatedSocket, findRsid, findSeeder, isOwnedIsolatedSocket, seedSynthetic, startIsolatedRsid,
} from '../scripts/smoke-isolated.mjs';
import { paritySections } from '../scripts/smoke-e2e.mjs';
import { E2E_EXTRAS, E2E_SECTIONS, SYNTHETIC_KEY } from '../scripts/smoke-e2e-sections.mjs';
import { findChrome } from '../scripts/smoke-lib.mjs';
import { main as smokeMain, parseArgs } from '../scripts/smoke.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const P = '11111111-1111-4111-8111-111111111111';
const I = '22222222-2222-4222-8222-222222222222';

/**
 * A stand-in "rsid": a node script that listens on $RSI_DAEMON_SOCKET_PATH, records every method it is called
 * with (and the environment it was started in) to a log, and answers `{}`. It lets the tests exercise the
 * real isolation code path (spawn, scrubbed environment, ownership registry) without a Rust build.
 */
function stubRsid() {
  const dir = mkdtempSync(join(tmpdir(), 'rsi-e2e-stub-'));
  const bin = join(dir, 'rsid');
  const log = join(dir, 'calls.log');
  writeFileSync(bin, `#!${process.execPath}
const net = require('node:net');
const fs = require('node:fs');
fs.writeFileSync(${JSON.stringify(join(dir, 'env.json'))}, JSON.stringify(process.env));
net.createServer((c) => {
  let b = '';
  c.on('data', (d) => {
    b += d;
    const i = b.indexOf('\\n');
    if (i < 0) return;
    const req = JSON.parse(b.slice(0, i));
    fs.appendFileSync(${JSON.stringify(log)}, JSON.stringify({ method: req.method, params: req.params }) + '\\n');
    c.end(JSON.stringify({ jsonrpc: '2.0', id: req.id, result: { echoed: req.method, slot: req.params && req.params.slot, state: 'vault', fingerprint: 'abcd1234', secret: 'must-not-reach-the-page' } }) + '\\n');
  });
  c.on('error', () => {});
}).listen(process.env.RSI_DAEMON_SOCKET_PATH);
`);
  chmodSync(bin, 0o755);
  const calls = () => (existsSync(log) ? readFileSync(log, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l)) : []);
  return { bin, dir, calls, env: () => JSON.parse(readFileSync(join(dir, 'env.json'), 'utf8')) };
}

test('the write bridge refuses a socket this run did not start (the operator\'s, a typo, a fake)', () => {
  const unowned = '/tmp/definitely-not-started-by-this-run.sock';
  assert.throws(() => assertIsolatedSocket(unowned), /did not start itself/);
  assert.throws(() => assertIsolatedSocket(''), /no socket/);
  const operator = join(process.env.HOME ?? '/root', '.rsi', 'daemon.sock');
  assert.throws(() => assertIsolatedSocket(operator), /operator's daemon socket|did not start itself/);
  assert.throws(() => createBridge({ call: async () => ({}), isolated: { socket: unowned } }), /did not start itself/);
  assert.equal(isOwnedIsolatedSocket(unowned), false);
});

test('an isolated rsid runs in a scrubbed environment under its own temporary HOME and socket', async () => {
  const stub = stubRsid();
  process.env.RSI_SESSION_TOKEN = 'must-not-leak';
  process.env.ANTHROPIC_API_KEY = 'must-not-leak';
  const h = await startIsolatedRsid(stub.bin);
  try {
    const env = stub.env();
    assert.equal(env.RSI_SESSION_TOKEN, undefined, 'the session token never reaches the daemon');
    assert.equal(env.ANTHROPIC_API_KEY, undefined, 'no provider key reaches the daemon');
    assert.deepEqual(Object.keys(env).filter((k) => k.startsWith('RSI_')).sort(), ['RSI_DAEMON_SOCKET_PATH', 'RSI_OFFLINE']);
    assert.equal(env.RSI_OFFLINE, '1', 'the isolated rsid runs offline from its first boot (#130)');
    // Outbound is routed to the local refusing guard: every proxy variable names it, loopback is exempt.
    for (const k of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy']) assert.equal(env[k], h.egress.url, k);
    assert.match(env.NO_PROXY, /127\.0\.0\.1/);
    assert.equal(env.RSI_DAEMON_SOCKET_PATH, h.socket);
    assert.ok(env.HOME.startsWith(h.dir) && h.dir.startsWith('/tmp/rsi-e2e-'), `HOME ${env.HOME} is under the temporary directory`);
    assert.ok(h.socket.startsWith(h.dir));
    assert.equal(isOwnedIsolatedSocket(h.socket), true);
    assert.equal(assertIsolatedSocket(h.socket), h.socket);
  } finally {
    delete process.env.RSI_SESSION_TOKEN;
    delete process.env.ANTHROPIC_API_KEY;
    await h.stop();
  }
  assert.equal(isOwnedIsolatedSocket(h.socket), false, 'a stopped daemon is no longer a write target');
  assert.equal(existsSync(h.dir), false, 'its state directory is removed');
  assert.throws(() => createBridge({ call: async () => ({}), isolated: { socket: h.socket } }), /did not start itself/);
});

test('in isolated-write mode the Issue writes reach the owned daemon with the params the Rust handlers send', async () => {
  const stub = stubRsid();
  const h = await startIsolatedRsid(stub.bin);
  try {
    const log = [];
    const bridge = createBridge({ call: (m, p) => rpcCall(h.socket, m, p), record: (e) => log.push(e), isolated: { socket: h.socket } });
    await bridge.invoke('create_issue', { projectId: P, draft: { title: 'T', body: 'b', priority: 2, labels: ['x'], assignee: 'me', idempotency_key: 'k1' } });
    await bridge.invoke('update_issue', { projectId: P, issueId: I, write: { expected_row_version: 2, idempotency_key: 'k2', patch: { title: 'New', priority: null } } });
    await bridge.invoke('update_issue_status', { projectId: P, issueId: I, write: { expected_row_version: 3, idempotency_key: 'k3', status: 'Cancelled' } });
    await bridge.invoke('archive_issue', { projectId: P, issueId: I, write: { expected_row_version: 4, idempotency_key: 'k4' } });
    await bridge.invoke('restore_issue', { projectId: P, issueId: I, write: { expected_row_version: 5, idempotency_key: 'k5' } });
    await bridge.invoke('toggle_pin', { sessionId: I });
    await bridge.invoke('update_daemon_config', { updates: [{ field: 'retry_enabled', value: true }] });
    assert.deepEqual(stub.calls().filter((c) => c.method !== 'GetHealthStatus'), [
      { method: 'CreateIssueV2', params: { project_id: P, title: 'T', body: 'b', priority: 2, labels: ['x'], assignee: 'me', idempotency_key: 'k1' } },
      { method: 'UpdateIssue', params: { project_id: P, issue_id: I, expected_row_version: 2, idempotency_key: 'k2', patch: { title: 'New', priority: null } } },
      { method: 'UpdateIssueStatusV2', params: { project_id: P, issue_id: I, expected_row_version: 3, idempotency_key: 'k3', status: 'Cancelled' } },
      { method: 'ArchiveIssue', params: { project_id: P, issue_id: I, expected_row_version: 4, idempotency_key: 'k4' } },
      { method: 'RestoreIssue', params: { project_id: P, issue_id: I, expected_row_version: 5, idempotency_key: 'k5' } },
      { method: 'TogglePin', params: { session_id: I } },
      { method: 'UpdateDaemonConfig', params: { updates: [{ field: 'retry_enabled', value: true }] } },
    ]);
    assert.ok(log.filter((e) => e.outcome === 'forwarded').every((e) => e.write === true), 'every forwarded write is marked');
  } finally { await h.stop(); }
});

test('local #128: the graph run reaches the owned daemon with the params graphs.rs sends, and the reads and the stand-in preflight answer', async () => {
  const stub = stubRsid();
  const h = await startIsolatedRsid(stub.bin);
  const W = '33333333-3333-4333-8333-333333333333';
  const wf = { version: '1.0', name: 'n', nodes: [{ id: 'a', name: 'A', type: 'action' }], edges: [] };
  try {
    const log = [];
    const bridge = createBridge({ call: (m, p) => rpcCall(h.socket, m, p), record: (e) => log.push(e), isolated: { socket: h.socket } });
    await bridge.invoke('graph_list_workflows', { projectId: P });
    await bridge.invoke('graph_list_workflows', { projectId: null });
    await bridge.invoke('graph_get_workflow', { workflowId: W });
    await bridge.invoke('graph_get_execution', { executionId: W });
    await bridge.invoke('graph_upsert_workflow', { document: { workflow: { id: W, title: 't', stage: 'Planning', artifact_path: null, project_id: P, created_at: 'c', updated_at: 'u' }, definition: wf } });
    await bridge.invoke('graph_execute_workflow', { workflowId: W, workflow: wf, projectId: P });
    assert.deepEqual(await bridge.invoke('graph_bind_session_workflow', { sessionId: I, workflowId: W }), { ok: true });
    assert.deepEqual(stub.calls().filter((c) => c.method !== 'GetHealthStatus'), [
      { method: 'ListWorkflows', params: { project_id: P } },
      { method: 'ListWorkflows', params: {} },
      { method: 'GetWorkflowDefinition', params: { workflow_id: W } },
      { method: 'GetWorkflowExecution', params: { execution_id: W } },
      { method: 'UpsertWorkflowDefinition', params: { document: { workflow: { id: W, title: 't', stage: 'Planning', artifact_path: null, project_id: P, created_at: 'c', updated_at: 'u' }, definition: wf } } },
      { method: 'ExecuteWorkflow', params: { workflow_id: W, workflow: wf, input: null, dry_run: false, project_id: P, working_dir: null, parent_id: null } },
      { method: 'UpdateSessionWorkflow', params: { session_id: I, workflow_id: W } },
    ]);
    // Only the three writes are marked; the reads are not.
    assert.deepEqual(log.filter((e) => e.write).map((e) => e.cmd), ['graph_upsert_workflow', 'graph_execute_workflow', 'graph_bind_session_workflow']);
    // Malformed requests never reach the socket.
    await assert.rejects(bridge.invoke('graph_bind_session_workflow', { sessionId: 'x', workflowId: W }), /UUID/);
    await assert.rejects(bridge.invoke('graph_execute_workflow', { workflowId: W, workflow: { nodes: [] }, projectId: null }), /nodes and edges/);
    await assert.rejects(bridge.invoke('graph_upsert_workflow', { document: { workflow: { id: W, title: ' ', stage: 'Planning', created_at: 'c', updated_at: 'u' }, definition: wf } }), /title is empty/);
    await assert.rejects(bridge.invoke('graph_upsert_workflow', { document: { workflow: { id: W, title: 't', stage: 'Nope', created_at: 'c', updated_at: 'u' }, definition: wf } }), /stage/);
    assert.equal(stub.calls().filter((c) => c.method !== 'GetHealthStatus').length, 7);
    // graph_interrupt_execution and every other graph write stay refused.
    await assert.rejects(bridge.invoke('graph_interrupt_execution', { executionId: W }), /not on the read-only allowlist/);
    await assert.rejects(bridge.invoke('dag_live_run', { graphId: W, maxSteps: 1 }), /not on the read-only allowlist/);
  } finally { await h.stop(); }
  assert.equal(validateWorkflowLocal(wf).executable, true);
  const bad = validateWorkflowLocal({ nodes: [{ id: 'a', type: 'action' }, { id: 'a', type: 'topology' }], edges: [{ source: 'a', target: 'zz' }] });
  assert.equal(bad.executable, false);
  assert.deepEqual(bad.diagnostics.map((d) => d.code).sort(), ['duplicate_node_id', 'unknown_edge_node', 'unsupported_node_kind']);
});

test('local #128: the graph-run extra is registered, binds through the keys, and the synthetic project has a local origin/rolling', () => {
  const extra = E2E_EXTRAS.find((e) => e.id === 'graph-run');
  assert.ok(extra, 'graph-run is an extra');
  assert.equal(extra.parityRow, 'overlay:graph-review-overlay-space-v-or-graph');
  const body = readFileSync(here('../scripts/smoke-e2e-sections.mjs'), 'utf8');
  const at = body.slice(body.indexOf("id: 'graph-run'"));
  for (const key of ["' v'", "'o'", "'Enter'", "'r'", 'GetSession', 'GetWorkflowExecution']) assert.ok(at.includes(key), `graph-run uses ${key}`);
  const iso = readFileSync(here('../scripts/smoke-isolated.mjs'), 'utf8');
  assert.match(iso, /'push', '-q', 'origin', 'main:rolling'/, 'origin/rolling exists for the run');
  assert.match(iso, /cwd: handle\.cwd \?\? dir/, 'the daemon starts inside the synthetic project');
  const audit = JSON.parse(readFileSync(here('../parity-audit.json'), 'utf8'));
  assert.equal(audit.groups['graph-review-dashboard'].interaction, 'dom-e2e');
});

test('isolated-write still refuses what starts a provider, deletes, or is not in the table', async () => {
  const stub = stubRsid();
  const h = await startIsolatedRsid(stub.bin);
  try {
    const log = [];
    const bridge = createBridge({ call: (m, p) => rpcCall(h.socket, m, p), record: (e) => log.push(e), isolated: { socket: h.socket } });
    for (const cmd of ['send_message', 'launch_session', 'delete_session', 'purge_session', 'put_claude_hooks', 'trigger_scheduled_job', 'remote_set_config', 'not_a_command']) {
      await assert.rejects(bridge.invoke(cmd, { sessionId: I, text: 'x' }), /not on the read-only allowlist/, cmd);
    }
    assert.deepEqual(stub.calls().filter((c) => c.method !== 'GetHealthStatus'), [], 'nothing reached the daemon');
    assert.equal(log.filter((e) => e.outcome === 'refused').length, 8);
    // Malformed writes are refused before the socket too.
    await assert.rejects(bridge.invoke('create_issue', { projectId: 'nope', draft: { title: 'T', idempotency_key: 'k' } }), /UUID/);
    await assert.rejects(bridge.invoke('update_issue_status', { projectId: P, issueId: I, write: { expected_row_version: 1, idempotency_key: 'k', status: 'Blocked' } }), /status/);
    await assert.rejects(bridge.invoke('update_daemon_config', { updates: [{ field: 'api_key', value: 'x' }] }), /plain daemon config field/);
    await assert.rejects(bridge.invoke('update_daemon_config', { updates: [{ field: 'retry_enabled', value: { a: 1 } }] }), /plain value/);
    assert.deepEqual(stub.calls().filter((c) => c.method !== 'GetHealthStatus'), []);
  } finally { await h.stop(); }
});

test('credentials: only the synthetic marker is forwarded, and the answer is allowlisted before the page sees it', async () => {
  const stub = stubRsid();
  const h = await startIsolatedRsid(stub.bin);
  try {
    const log = [];
    const bridge = createBridge({ call: (m, p) => rpcCall(h.socket, m, p), record: (e) => log.push(e), isolated: { socket: h.socket } });
    assert.match(SYNTHETIC_KEY, SYNTHETIC_SECRET);
    await assert.rejects(bridge.invoke('set_provider_credential', { slot: 'openrouter', secret: 'sk-real-looking-0000000000', rotate: false }), /synthetic credentials only/);
    await assert.rejects(bridge.invoke('set_provider_credential', { slot: 'not-a-slot', secret: SYNTHETIC_KEY, rotate: false }), /not a provider key slot/);
    assert.deepEqual(stub.calls().filter((c) => c.method !== 'GetHealthStatus'), [], 'a real-looking secret never reaches the daemon');
    const meta = await bridge.invoke('set_provider_credential', { slot: 'openrouter', secret: SYNTHETIC_KEY, rotate: false });
    assert.deepEqual(meta, { slot: 'openrouter', state: 'vault', fingerprint: 'abcd1234' }, 'the answer is the redacted metadata only');
    await bridge.invoke('set_provider_credential', { slot: 'openrouter', secret: SYNTHETIC_KEY, rotate: true });
    await bridge.invoke('clear_provider_credential', { slot: 'openrouter' });
    assert.deepEqual(stub.calls().filter((c) => c.method !== 'GetHealthStatus').map((c) => c.method), ['SetProviderCredential', 'RotateProviderCredential', 'ClearProviderCredential']);
    assert.ok(!JSON.stringify(log).includes(SYNTHETIC_KEY), 'the bridge log never holds the secret');
    // Another command's arguments may not smuggle a credential through the write table.
    await assert.rejects(bridge.invoke('create_issue', { projectId: P, draft: { title: 'T', idempotency_key: 'k' }, api_key: 'x' }), /carry a credential/);
  } finally { await h.stop(); }
});

test('the default bridge (and --live) never gains a write: the table is only reachable through an isolated grant', async () => {
  const calls = [];
  const bridge = createBridge({ call: async (m) => { calls.push(m); return {}; } });
  for (const cmd of Object.keys(WRITE_COMMANDS)) await assert.rejects(bridge.invoke(cmd, {}), /not on the read-only allowlist/, cmd);
  assert.deepEqual(calls, []);
  for (const m of WRITE_METHODS) assert.ok(!READ_METHODS.has(m), `${m} is on both lists`);
  assert.throws(() => parseArgs(['--live', '--isolated-write']), /cannot be combined/);
  assert.deepEqual(parseArgs(['--isolated-write', '--rsid', '/r', '--only', 'a,b']), { live: false, out: '/tmp/desktop-smoke', chrome: null, isolatedWrite: true, rsid: '/r', only: ['a', 'b'], firstRun: false });
});

test('redactCredentialList keeps only the metadata the desktop shows', () => {
  const out = redactCredentialList({ env_compat: true, check_ttl_secs: 600, extra: 1, credentials: [{ slot: 's', state: 'vault', fingerprint: 'f', secret: 'x', check: { class: 'valid', token: 'x' } }] });
  assert.deepEqual(out, { env_compat: true, check_ttl_secs: 600, credentials: [{ slot: 's', state: 'vault', fingerprint: 'f', check: { class: 'valid' } }] });
});

test('findRsid honours an explicit path, then $RSI_E2E_RSID, and reports nothing when there is no binary', () => {
  assert.equal(findRsid('/x/rsid', {}, (p) => p === '/x/rsid'), '/x/rsid');
  assert.equal(findRsid(null, { RSI_E2E_RSID: '/y/rsid' }, (p) => p === '/y/rsid'), '/y/rsid');
  assert.equal(findRsid(null, {}, () => false), null);
});

test('every H2 section of the generated PARITY.md has an end-to-end interaction, and each names a real row', () => {
  const md = readFileSync(here('../PARITY.md'), 'utf8');
  const sections = paritySections(md);
  assert.equal(sections.length, 10, 'PARITY.md has ten sections');
  assert.deepEqual(E2E_SECTIONS.map((s) => s.section), sections, 'one end-to-end section per PARITY section, in file order');
  for (const s of [...E2E_SECTIONS, ...E2E_EXTRAS]) {
    assert.ok(md.includes(`\`${s.parityRow}\``), `${s.id}: ${s.parityRow} is a row of PARITY.md`);
    assert.ok(s.source && s.interaction && typeof s.run === 'function', `${s.id} records its source and interaction`);
  }
  assert.equal(new Set([...E2E_SECTIONS, ...E2E_EXTRAS].map((s) => s.id)).size, E2E_SECTIONS.length + E2E_EXTRAS.length, 'ids are unique');
  // A row of each section must sit under THAT section (a wrong row id would otherwise pass the includes check).
  for (const s of E2E_SECTIONS) {
    const at = md.indexOf(`\n## ${s.section}\n`);
    const next = md.indexOf('\n## ', at + 4);
    const body = md.slice(at, next < 0 ? md.length : next);
    assert.ok(body.includes(`\`${s.parityRow}\``), `${s.id}: ${s.parityRow} is a row of the section "${s.section}"`);
  }
  assert.ok(E2E_EXTRAS.some((s) => s.id === 'vim-keys-toggle'), 'the Vim keys toggle (hub #3101) is covered by real DOM key events');
});

test('the Issue lifecycle covers create, edit, cancel, archive and restore', () => {
  const src = readFileSync(here('../scripts/smoke-e2e.mjs'), 'utf8');
  for (const id of ['create', 'edit', 'cancel', 'archive', 'restore']) assert.match(src, new RegExp(`record\\('${id}'`), id);
  for (const m of ['CreateIssueV2', 'UpdateIssue', 'UpdateIssueStatusV2', 'ArchiveIssue', 'RestoreIssue']) assert.ok(WRITE_METHODS.has(m), m);
});

test('the egress guard records and refuses a proxied request locally', async () => {
  const guard = await startEgressGuard();
  try {
    const answer = await new Promise((resolve, reject) => {
      const c = net.connect(guard.port, '127.0.0.1', () => c.write('CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\n\r\n'));
      let buf = '';
      c.on('data', (d) => { buf += d; });
      c.on('close', () => resolve(buf));
      c.on('error', reject);
    });
    assert.match(answer, /^HTTP\/1\.1 403/);
    assert.equal(guard.attempts.length, 1);
    assert.deepEqual({ ...guard.attempts[0], at: 0 }, { method: 'CONNECT', host: 'api.anthropic.com', refused: true, at: 0 });
  } finally { await guard.close(); }
});

test('the egress verdict requires zero attempts from first boot, no direct connection and an offline key check', () => {
  assert.deepEqual(parseProxyRequest('GET http://example.com/x HTTP/1.1\r\n'), { method: 'GET', host: 'example.com', refused: true });
  const offlineKey = { set: true, detail: OFFLINE_KEY_CHECK };
  assert.equal(judgeEgress({ attempts: [], direct: [], keyCheck: offlineKey, sampled: true }).ok, true);
  assert.equal(judgeEgress({ attempts: [], direct: [], sampled: true }).ok, true, 'no key set, no attempt: clean');
  // Every host fails, a provider-check host and the old first-boot Codex poll host included: no window, no allowlist.
  for (const host of ['api.anthropic.com', 'openrouter.ai', 'chatgpt.com', 'telemetry.example.com']) {
    const verdict = judgeEgress({ attempts: [{ method: 'CONNECT', host, refused: true }], direct: [], keyCheck: offlineKey, sampled: true });
    assert.equal(verdict.ok, false, host);
    assert.match(verdict.problems[0], new RegExp(host.replace(/\./g, '\\.')));
  }
  assert.equal(judgeEgress({ attempts: [], direct: ['203.0.113.10:443'], sampled: true }).ok, false, 'a direct connection bypassing the proxy');
  assert.equal(judgeEgress({ attempts: [], direct: [], keyCheck: { set: true, detail: null }, sampled: true }).ok, false, 'a key was set but no offline check was recorded');
  assert.equal(judgeEgress({ attempts: [], direct: [], keyCheck: { set: true, detail: 'connect_error' }, sampled: true }).ok, false, 'a key check that tried the network');
  assert.equal(judgeEgress({ attempts: [], direct: [], sampled: false }).socketSample, 'skipped (lsof unavailable)');
  const lsof = [
    'rsid 1 u IPv4 0x1 0t0 TCP 127.0.0.1:50000->127.0.0.1:60000 (ESTABLISHED)',
    'rsid 1 u IPv4 0x2 0t0 TCP 10.0.0.5:50001->203.0.113.11:443 (ESTABLISHED)',
    'rsid 1 u IPv4 0x3 0t0 TCP *:8080 (LISTEN)',
  ].join('\n');
  assert.deepEqual(parseLsofDirect(lsof), ['203.0.113.11:443']);
});

// ---------- the seeded finished session (local #122) ----------

/** A stand-in `rsi-e2e-seed`: records its arguments, the spec it was given and whether the daemon was stopped. */
function stubSeeder(dir, { fail = false } = {}) {
  const bin = join(dir, 'rsi-e2e-seed');
  writeFileSync(bin, `#!${process.execPath}
const fs = require('node:fs');
const args = process.argv.slice(2);
const stateDir = args[args.indexOf('--state-dir') + 1];
const spec = JSON.parse(fs.readFileSync(args[args.indexOf('--spec') + 1], 'utf8'));
fs.writeFileSync(${JSON.stringify(join(dir, 'seeder.json'))}, JSON.stringify({ args, spec, daemonStopped: !fs.existsSync(stateDir + '/d.sock'), marker: fs.readFileSync(stateDir + '/${MARKER_FILE}', 'utf8') }));
${fail ? "console.error('refused: no marker'); process.exit(2);" : ''}
`);
  chmodSync(bin, 0o755);
  return { bin, record: () => JSON.parse(readFileSync(join(dir, 'seeder.json'), 'utf8')) };
}

test('findSeeder honours $RSI_E2E_SEEDER, then the binary next to rsid, and reports nothing otherwise', () => {
  assert.equal(findSeeder('/x/rsid', { RSI_E2E_SEEDER: '/y/seed' }, (p) => p === '/y/seed'), '/y/seed');
  assert.equal(findSeeder('/x/rsid', {}, (p) => p === '/x/rsi-e2e-seed'), '/x/rsi-e2e-seed');
  assert.equal(findSeeder('/x/rsid', {}, () => false), null);
});

test('the isolated state directory is marked, and the finished session is seeded by rsi-e2e-seed while the daemon is stopped', async () => {
  const stub = stubRsid();
  const seeder = stubSeeder(stub.dir);
  const h = await startIsolatedRsid(stub.bin);
  try {
    assert.equal(readFileSync(join(h.dir, MARKER_FILE), 'utf8'), MARKER_TEXT, 'the seeder\'s marker is in the state directory');
    const seed = await seedSynthetic(h);
    const r = seeder.record();
    assert.deepEqual(r.args.slice(0, 2), ['--state-dir', h.dir]);
    assert.equal(r.daemonStopped, true, 'the daemon is stopped while the seeder writes its database');
    assert.equal(r.marker, MARKER_TEXT);
    assert.equal(r.spec.session.id, seed.leafId);
    assert.equal(r.spec.session.working_dir, seed.root);
    assert.deepEqual(r.spec.events.map((e) => [e.event_type, e.role, e.tool_name ?? null]), [['Message', 'User', null], ['ToolUse', 'Assistant', 'Edit'], ['Message', 'Assistant', null]]);
    assert.equal(r.spec.events[1].tool_input.file_path, join(seed.root, 'src', 'lib.rs'));
    assert.equal(isOwnedIsolatedSocket(h.socket), true, 'the daemon is running again after the seed');
  } finally {
    await h.stop();
  }
});

test('a seeder refusal or a missing seeder fails the seed instead of faking history', async () => {
  const stub = stubRsid();
  stubSeeder(stub.dir, { fail: true });
  const h = await startIsolatedRsid(stub.bin);
  try {
    await assert.rejects(seedSynthetic(h), /rsi-e2e-seed failed \(2\): refused: no marker/);
  } finally {
    await h.stop();
  }
  const bare = stubRsid();
  const h2 = await startIsolatedRsid(bare.bin);
  try {
    await assert.rejects(seedSynthetic(h2), (e) => e.code === 'runner_unavailable' && /rsi-e2e-seed/.test(e.message));
  } finally {
    await h2.stop();
  }
});

test('the harness no longer stubs the store: no SQL function, no hand-written row, no node:sqlite', () => {
  const src = readFileSync(here('../scripts/smoke-isolated.mjs'), 'utf8').split('\n').filter((l) => !l.trimStart().startsWith('//')).join('\n');
  for (const needle of ['node:sqlite', 'DatabaseSync', 'rsi_rfc3339_nanos_is_canonical', 'rsi_uuid_is_canonical', 'db.function', 'INSERT INTO']) {
    assert.equal(src.includes(needle), false, `smoke-isolated.mjs must not contain ${needle}`);
  }
});

// The full run needs Chrome, a built dist AND an explicit opt-in: a built rsid in $RSI_E2E_RSID.
const chrome = findChrome();
const rsid = process.env.RSI_E2E_RSID;
const why = !chrome ? 'runner_unavailable: no Chrome' : !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no dist (npm run build)'
  : !rsid || !existsSync(rsid) ? 'runner_unavailable: set RSI_E2E_RSID to a built rsid binary to run the real-rsid end-to-end smoke' : false;
test('the real-rsid end-to-end smoke passes against an isolated rsid', { skip: why, timeout: 600000 }, async (t) => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-e2e-out-'));
  const code = await smokeMain(['--isolated-write', '--rsid', rsid, '--out', out], { signal: t.signal });
  const report = JSON.parse(readFileSync(join(out, 'e2e-report.json'), 'utf8'));
  const failed = report.results.flatMap((r) => [r, ...r.steps]).filter((r) => r.status === 'fail');
  assert.equal(code, 0, JSON.stringify(failed));
  assert.deepEqual(report.parity.uncovered, []);
  assert.ok(report.results.every((r) => r.runner.sha && r.daemon.sha256 && r.command && r.source));
  for (const m of ['CreateIssueV2', 'UpdateIssue', 'UpdateIssueStatusV2', 'ArchiveIssue', 'RestoreIssue', 'SetProviderCredential', 'ClearProviderCredential', 'UpdateDaemonConfig', 'TogglePin']) {
    assert.ok(report.forwardedWrites.includes(m), `${m} reached the isolated daemon`);
  }
  assert.deepEqual(report.refusals, ['send_message: not on the read-only allowlist']);
  // Local #123, #130: no outbound attempt from rsid's first boot on. The offline rsid recorded the provider-key
  // check as offline_unchecked and nothing reached the refusing proxy.
  assert.equal(report.egress.ok, true, JSON.stringify(report.egress));
  assert.deepEqual(report.egress.attempts, []);
  assert.deepEqual(report.egress.keyCheck, { set: true, detail: OFFLINE_KEY_CHECK });
  assert.deepEqual(report.egress.directConnections, []);
});
