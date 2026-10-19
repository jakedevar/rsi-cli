// Admin overlays (slice 13a, hub #3015): registry entries, keys, and the pure
// rules in admin-model.ts (request shapes, credential display, list keys).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, paletteEntries } from '../dist/registry.js';
import { ADMIN_ACTIONS, ADMIN_COMMAND_BINDINGS, ADMIN_KEY_BINDINGS, ADMIN_RULES } from '../dist/actions/admin-spec.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  CLEAR_CONFIRM_MS, CLI_NAMES, KEY_SLOTS, RECURRENCES, checkTarget, clearStep, contractValue, credentialSet,
  credentialValue, custodyFailed, cycleRecurrence, deleteChord, diagLines, editScheduleForm, formatAge, jobRow,
  keyRows, loginValue, maskedEcho, mcpForm, mcpRequest, mcpValue, median, moveCursor, newScheduleForm, normalizeTime,
  recurrenceLabel, scheduleRequest, splitList, wakeLabel,
  MAX_ADOPT_PATHS, MAX_REMOTE_PROJECTS, adoptAll, adoptOne, adoptSummary, applyBlock, countsLine, formatBytes, formatScope,
  linkForm, linkRequest, linkRow, nextSatTab, pageCursor, parseIdList, parseScope, peerForm, peerRequest, peerRow,
  remoteDeviceEdit, remoteProjectEdit, remoteSummary, satSessionRow, scratchVerdict, settlementApply, settlementKey,
  toggleLinkEdit, togglePeerEdit,
} from '../dist/admin-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const NOW = Date.parse('2026-10-18T12:00:00Z');
const ago = (ms) => new Date(NOW - ms).toISOString();
const SECRET = 'sk-live-NEVER-SHOWN';
const snap = (over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, ...over,
});

// ---------- registry ----------

test('every admin action is registered with one rule and is in the palette', () => {
  const ids = ADMIN_ACTIONS.map((a) => a.id);
  assert.deepEqual(Object.keys(ADMIN_RULES).sort(), [...ids].sort());
  for (const id of ids) {
    assert.ok(ACTIONS.some((a) => a.id === id), id);
    assert.equal(ADMIN_RULES[id](snap()), null, id);
  }
  const entries = paletteEntries(snap(), 'other');
  for (const id of ids) assert.ok(entries.some((e) => e.id === id && e.enabled), `${id} in the palette`);
  assert.deepEqual(entries.find((e) => e.id === 'admin-schedules').keys.length > 0, true);
});

test('<Space>K and :diagnostics (:diag) run their overlays through the TUI keymap', () => {
  const e = new KeyEngine(desktopKeymap());
  let out;
  for (const k of ['Space', 'K']) out = e.feed(k);
  assert.equal(out.action, 'admin-schedules');
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('diagnostics').action, 'admin-diagnostics');
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('diag').action, 'admin-diagnostics');
  for (const b of [...Object.values(ADMIN_KEY_BINDINGS), ...Object.values(ADMIN_COMMAND_BINDINGS)]) {
    assert.ok(ADMIN_ACTIONS.some((a) => a.id === b.action), b.action);
  }
});

// ---------- shared ----------

test('formatAge, moveCursor and splitList follow the TUI', () => {
  assert.equal(formatAge(5_000), '5s');
  assert.equal(formatAge(120_000), '2m');
  assert.equal(formatAge(3 * 3_600_000), '3h');
  assert.equal(formatAge(2 * 86_400_000), '2d');
  assert.equal(formatAge(-1), '0s');
  assert.equal(moveCursor(0, 3, 'j'), 1);
  assert.equal(moveCursor(2, 3, 'j'), 2);
  assert.equal(moveCursor(0, 3, 'k'), 0);
  assert.equal(moveCursor(1, 3, 'G'), 2);
  assert.equal(moveCursor(2, 3, 'g'), 0);
  assert.equal(moveCursor(0, 0, 'j'), null);
  assert.equal(moveCursor(0, 3, 'x'), null);
  assert.deepEqual(splitList(' a, ,b ,c'), ['a', 'b', 'c']);
  assert.equal(median([3, 1, 2]), 2);
  assert.equal(median([1, 2, 3, 4]), 2.5);
  assert.equal(median([]), null);
});

// ---------- scheduled jobs ----------

const job = (over = {}) => ({
  id: 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae',
  name: 'nightly',
  message: 'run the suite',
  schedule: { recurrence: { type: 'EveryHours', value: 6 }, anchor: '2026-10-01T03:30:00Z' },
  last_fired_at: ago(3_600_000),
  next_fire_at: new Date(NOW + 2 * 3_600_000).toISOString(),
  enabled: true,
  working_dir: '/srv/repo',
  provider: 'Claude',
  model: null,
  project_id: null,
  wake_mode: 'fresh',
  ...over,
});

test('schedule rows show recurrence, next and last fire, wake mode and holds', () => {
  assert.equal(recurrenceLabel({ type: 'Once' }), 'once');
  assert.equal(recurrenceLabel({ type: 'EveryMonths', value: 2 }), 'every 2 mo');
  assert.equal(wakeLabel({ on_terminal: 'x' }), 'on_terminal');
  assert.equal(wakeLabel('resume'), 'resume');
  const r = jobRow(job(), [], NOW);
  assert.equal(r.mark, '●');
  assert.equal(r.detail, 'every 6 h  next in 2h  last 1h ago');
  const held = jobRow(job({ enabled: false, wake_mode: 'resume', next_fire_at: ago(60_000) }),
    [{ job_id: job().id, parent_session_id: 'p', running_children: ['a', 'b'], held_since: ago(1), release_at: ago(0) }], NOW);
  assert.equal(held.mark, '○');
  assert.match(held.detail, /1m overdue/);
  assert.match(held.detail, /wake:resume/);
  assert.match(held.detail, /held for 2 running children/);
});

test('the schedule form builds the TUI create and update requests', () => {
  const f = newScheduleForm(new Date(NOW));
  assert.equal(f.date, '2026-10-18');
  assert.equal(f.time, '00:00:00');
  assert.equal(RECURRENCES[f.recurrence].tag, 'Once');
  assert.deepEqual(scheduleRequest({ ...f, name: ' a ', message: ' b ' }), {
    ok: true, wire: { name: 'a', message: 'b', recurrence: 'Once', interval: null, anchor: '2026-10-18T00:00:00Z' },
  });
  const every = scheduleRequest({ ...f, name: 'a', message: 'b', recurrence: 4, interval: '0', time: '7:05', workingDir: ' /srv ', provider: 'Codex', model: ' m ' });
  assert.deepEqual(every.wire, {
    name: 'a', message: 'b', recurrence: 'EveryDays', interval: 1, anchor: '2026-10-18T07:05:00Z', working_dir: '/srv', provider: 'Codex', model: 'm',
  });
  const edit = editScheduleForm(job());
  assert.equal(edit.editingId, job().id);
  assert.equal(RECURRENCES[edit.recurrence].tag, 'EveryHours');
  assert.equal(edit.interval, '6');
  assert.equal(edit.date, '2026-10-01');
  assert.equal(edit.time, '03:30:00');
  // An edit sends name, message and schedule only (as the TUI's UpdateScheduledJob).
  assert.deepEqual(scheduleRequest(edit).wire, {
    name: 'nightly', message: 'run the suite', recurrence: 'EveryHours', interval: 6, anchor: '2026-10-01T03:30:00Z',
  });
});

test('the schedule form refuses what the TUI refuses, in its order', () => {
  const f = { ...newScheduleForm(new Date(NOW)), name: 'a', message: 'b' };
  assert.equal(scheduleRequest({ ...f, name: ' ' }).error, 'Job name is required');
  assert.equal(scheduleRequest({ ...f, message: '' }).error, 'Job message/prompt is required');
  assert.equal(scheduleRequest({ ...f, date: '2026-02-30' }).error, 'Invalid date format (use YYYY-MM-DD)');
  assert.equal(scheduleRequest({ ...f, date: '18-10-2026' }).error, 'Invalid date format (use YYYY-MM-DD)');
  assert.equal(scheduleRequest({ ...f, time: '25:00' }).error, 'Invalid time format (use HH:MM:SS or HH:MM)');
  assert.equal(normalizeTime(''), '00:00:00');
  assert.equal(normalizeTime('9:30'), '09:30:00');
  assert.equal(normalizeTime('09:30:61'), null);
  assert.equal(cycleRecurrence(0, -1), RECURRENCES.length - 1);
  assert.equal(cycleRecurrence(RECURRENCES.length - 1, 1), 0);
});

test('dd deletes only on a second d on the same job', () => {
  let s = deleteChord(null, 'a');
  assert.deepEqual(s, { armed: 'a', run: null });
  assert.deepEqual(deleteChord('b', 'a'), { armed: 'a', run: null });
  s = deleteChord(s.armed, 'a');
  assert.deepEqual(s, { armed: null, run: 'a' });
  assert.deepEqual(deleteChord('a', undefined), { armed: null, run: null });
});

// ---------- provider keys ----------

const meta = (over = {}) => ({
  slot: 'openrouter', state: 'vault', fingerprint: '1a2b3c4d', route: 'harness', cli_exposure: 'none', generation: 2,
  check: { at: ago(120_000), class: 'valid', credit_remaining: 4.5, detail_code: 'ok' },
  ...over,
});

test('provider key rows show the TUI metadata and never a key', () => {
  assert.equal(credentialValue(meta(), NOW), 'vault  fp:1a2b3c4d  check:valid(2m)  credit:4.50  route:harness  cli:none');
  assert.equal(credentialValue(meta({ state: 'absent', fingerprint: undefined, check: undefined }), NOW),
    'absent  fp:-  check:no check  credit:-  route:harness  cli:none');
  assert.equal(credentialSet('vault'), true);
  assert.equal(credentialSet('env_compat'), true);
  assert.equal(credentialSet('cleared'), false);
  const list = { env_compat: false, check_ttl_secs: 600, credentials: [meta(), meta({ slot: 'groq', state: 'cleared' })] };
  const rows = keyRows(list, null, null, NOW);
  assert.equal(rows.length, KEY_SLOTS.length + 2 * CLI_NAMES.length);
  assert.equal(rows[0].label, 'openrouter');
  assert.equal(rows[0].set, true);
  assert.equal(rows.find((r) => r.name === 'groq').set, false);
  assert.equal(rows.find((r) => r.name === 'anthropic').value, 'loading…');
  assert.deepEqual(rows.slice(KEY_SLOTS.length).map((r) => r.label), ['claude login', 'codex login', 'claude contract', 'codex contract']);
  // Even a daemon that sent a secret field: the row is built from the allowlisted fields only.
  const leaky = keyRows({ ...list, credentials: [{ ...meta(), secret: SECRET, value: SECRET }] }, null, null, NOW);
  assert.ok(!JSON.stringify(leaky).includes(SECRET));
});

test('login and contract rows follow the TUI', () => {
  assert.equal(loginValue({ provider: 'claude', remedy: 'claude login', login: { state: 'logged_out', method: 'oauth', checked_at: ago(60_000), hosts: [{ host: 'mini', state: 'logged_in' }] } }, NOW),
    'logged_out  method:oauth  checked 1m ago  fix: claude login  mini:logged_in');
  assert.equal(loginValue({ provider: 'codex', login: { state: 'unknown', method: 'none', checked_at: null } }, NOW),
    'unknown  method:none  never probed  launches are not refused');
  const degraded = { contract_state: 'degraded', reason: 'codex_hook_untrusted', cli_version: '0.9', checked_at: ago(3_600_000), catalog: { trust: 'trusted', reason: 'ok' } };
  assert.equal(custodyFailed(degraded), true);
  assert.equal(custodyFailed({ ...degraded, reason: 'other' }), false);
  assert.equal(contractValue({ provider: 'codex', contract: degraded }, true, NOW),
    'degraded  reason:codex_hook_untrusted  0.9  checked 1h ago  catalog:trusted reason:ok  Codex launches are refused');
  assert.equal(contractValue({ provider: 'claude', contract: { contract_state: 'unchecked' } }, false, NOW),
    'unchecked  version unknown  never checked');
});

test('d twice within 2 s clears, c checks the row or probes the CLIs', () => {
  let s = clearStep(null, 'groq', NOW);
  assert.equal(s.run, false);
  assert.equal(clearStep(s.armed, 'groq', NOW + CLEAR_CONFIRM_MS).run, true);
  assert.equal(clearStep(s.armed, 'groq', NOW + CLEAR_CONFIRM_MS + 1).run, false);
  assert.equal(clearStep(s.armed, 'exa', NOW + 10).run, false);
  const rows = keyRows(null, null, null, NOW);
  assert.deepEqual(checkTarget(rows[7]), { cmd: 'check_provider_credential', args: { slot: 'groq' } });
  assert.deepEqual(checkTarget(rows[KEY_SLOTS.length]), { cmd: 'get_provider_login_state', args: { refresh: true } });
  assert.deepEqual(checkTarget(rows[KEY_SLOTS.length + 2]), { cmd: 'get_provider_contract_state', args: { refresh: true } });
  assert.equal(checkTarget(undefined), null);
});

test('the masked echo shows a count, never the text', () => {
  assert.equal(maskedEcho(0), '');
  assert.equal(maskedEcho(3), '••• (3)');
  assert.equal(maskedEcho(100).length, 64 + ' (100)'.length);
});

// ---------- MCP servers ----------

const server = {
  definition: { id: 'docs', command: '/usr/bin/mcp', args: ['serve', '--verbose'], secret_env_names: ['MCP_DOCS_TOKEN'], working_dir: null, enabled: false },
  credential: { id: 'docs', state: 'vault', fingerprint: 'ffee', generation: 1 },
};

test('MCP rows and the form follow the TUI', () => {
  assert.equal(mcpValue(server), 'enabled:off  command:/usr/bin/mcp  args:2  secrets:MCP_DOCS_TOKEN  state:vault  fp:ffee  generation:1');
  const f = mcpForm(server);
  assert.deepEqual(f, { id: 'docs', command: '/usr/bin/mcp', args: 'serve,--verbose', envNames: 'MCP_DOCS_TOKEN', workingDir: '', enabled: false, editing: true });
  assert.deepEqual(mcpRequest({ ...f, enabled: true, workingDir: ' /srv ' }), {
    ok: true,
    server: { id: 'docs', command: '/usr/bin/mcp', args: ['serve', '--verbose'], secret_env_names: ['MCP_DOCS_TOKEN'], working_dir: '/srv', enabled: true },
  });
  assert.equal(mcpForm().editing, false);
  assert.match(mcpRequest({ ...f, id: 'Docs' }).error, /lowercase/);
  assert.match(mcpRequest({ ...f, id: 'a--b' }).error, /lowercase/);
  assert.match(mcpRequest({ ...f, command: 'mcp' }).error, /absolute/);
  assert.match(mcpRequest({ ...f, workingDir: 'rel' }).error, /working_dir/);
});

test('an MCP definition listed with masked credential arguments cannot be saved back (hub #3102)', () => {
  // The shape list_mcp_servers returns after admin.rs redact_args (synthetic values masked there).
  const masked = {
    ...server,
    definition: { ...server.definition, args: ['serve', '--token', '[redacted]', '--header', 'Authorization: [redacted]', '--port', '8080'] },
  };
  const f = mcpForm(masked);
  assert.equal(f.args, 'serve,--token,[redacted],--header,Authorization: [redacted],--port,8080');
  assert.match(mcpRequest(f).error, /\[redacted\]; type its value again/);
  const retyped = mcpRequest({ ...f, args: 'serve,--token,synthetic-value,--port,8080' });
  assert.equal(retyped.ok, true);
  assert.deepEqual(retyped.server.args, ['serve', '--token', 'synthetic-value', '--port', '8080']);
});

// ---------- diagnostics ----------

test('diagnostics lines use an em dash for an unknown value', () => {
  assert.deepEqual(diagLines({ roundTripMs: 1.25, frameMs: null, sessions: 4, daemon: 'connected' }), [
    ['daemon round trip', '1.3 ms'], ['frame interval', '—'], ['sessions listed', '4'], ['daemon', 'connected'],
  ]);
});

// ---------- credential custody in the DOM code ----------

test('the admin DOM never renders, logs or stores a typed secret', () => {
  const src = readFileSync(here('../src/admin-keys.ts'), 'utf8');
  // The secret leaves the password field only as an invoke argument.
  assert.ok(!/console\./.test(src), 'no console logging');
  assert.ok(!/localStorage|sessionStorage|clipboard/.test(src), 'no storage or clipboard');
  assert.ok(!/textContent\s*=\s*secret|innerHTML/.test(src), 'never rendered');
  assert.match(src, /type = 'password'|secretField/);
  const dom = readFileSync(here('../src/admin-dom.ts'), 'utf8');
  assert.match(dom, /input\.type = 'password'/);
});

// ---------- part 2: satellites, remote, scratch, settlement ----------

const P = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';
const Q = '7b0c9e7e-8a8b-4c1e-9f57-3f5c2b6a9d10';
const R = '3f2a1b0c-0d1e-4f5a-8b6c-7d8e9f0a1b2c';
const satPeer = (over = {}) => ({
  config: { peer_id: P, label: 'mac-mini', expected_installation_id: Q, enabled: true, read_enabled: true, dispatch_enabled: true },
  row_version: 4,
  links: [{ link_id: R, direction: 'dial_home_reverse', socket_path: '/run/rsi/mini.sock', ssh_target: null, trust_reference: 'known_hosts:mini', enabled: true, priority: 1 }],
  observation: { state: 'paired', installation_id: Q, incarnation_id: null, last_observed_at: null, last_error: null, cached_session_count: 12, stale: true },
  dispatch_scope: [R], dispatch_project_scope: [Q],
  ...over,
});

test('the four part-2 admin actions are registered with rules', () => {
  for (const id of ['admin-satellites', 'admin-remote', 'admin-scratch', 'admin-settlement']) {
    assert.ok(ADMIN_ACTIONS.some((a) => a.id === id), id);
    assert.equal(ADMIN_RULES[id](snap()), null, id);
  }
});

test('the peer form saves through one fail-closed command (hub #3111)', () => {
  const src = readFileSync(here('../src/admin-ops.ts'), 'utf8');
  const form = src.slice(src.indexOf('function openPeerForm'), src.indexOf('function openLinkForm'));
  assert.match(form, /invoke\('save_satellite_peer', \{ save: \{ edit: r\.edit, sessions: r\.scope\.sessions, projects: r\.scope\.projects, \.\.\.previous \} \}\)/);
  assert.equal((form.match(/invoke\(/g) ?? []).length, 1);
});

test('satellite scope text parses and formats as the TUI', () => {
  assert.deepEqual(parseScope(` ${R} , project:${Q} `), { ok: true, sessions: [R], projects: [Q] });
  assert.equal(formatScope([R], [Q]), `${R},project:${Q}`);
  assert.match(parseScope('nope').error, /Invalid scope session ID `nope`/);
  assert.match(parseScope(`project:${P.toUpperCase()}`).error, /Invalid scope project ID/);
  // The daemon's bounds, refused before any save starts (hub #3111).
  assert.match(parseScope(`${R},${R}`).error, /nil or repeated/);
  assert.match(parseScope('00000000-0000-0000-0000-000000000000').error, /nil or repeated/);
  const ids = Array.from({ length: 65 }, (_, i) => `00000000-0000-4000-8000-${String(i).padStart(12, '0')}`);
  assert.match(parseScope(ids.join(',')).error, /at most 64 sessions/);
  assert.equal(parseScope(ids.slice(0, 64).join(',')).ok, true);
  assert.deepEqual(parseIdList(`${P},${Q}`), { ok: true, ids: [P, Q] });
  assert.equal(parseIdList(`project:${P}`).error, 'Hub installation IDs cannot be project: entries');
  assert.equal(nextSatTab('peers'), 'links');
  assert.equal(nextSatTab('links'), 'sessions');
  assert.equal(nextSatTab('sessions'), 'peers');
});

test('satellite rows show config, observation and links', () => {
  assert.deepEqual(peerRow(satPeer()), { label: 'mac-mini', value: 'enabled:on  read:on  dispatch:on  state:paired (stale)  links:1  cached:12' });
  assert.match(peerRow(satPeer({ observation: null })).value, /state:unobserved/);
  assert.deepEqual(linkRow(satPeer().links[0]), { label: '/run/rsi/mini.sock', value: 'dial_home_reverse  ssh:-  trust:known_hosts:mini  enabled:on  priority:1' });
  assert.deepEqual(satSessionRow({ key: {}, summary: { session_id: R, title: null, provider: 'Claude', status: 'Running', working_dir: null, updated_at: ago(120_000) } }, NOW),
    { label: R, value: 'Claude  Running  2m ago' });
});

test('peer edits: the form, the request with its scope, and the d toggle', () => {
  const f = peerForm(satPeer(), 'unused');
  assert.equal(f.scope, `${R},project:${Q}`);
  assert.equal(f.isNew, false);
  const r = peerRequest(f, 9);
  assert.deepEqual(r.edit, {
    expected_registry_revision: 9, peer_id: P, label: 'mac-mini', expected_installation_id: Q,
    enabled: true, read_enabled: true, dispatch_enabled: true, repair_quarantine: false,
  });
  assert.deepEqual(r.scope, { sessions: [R], projects: [Q] });
  assert.equal(peerForm(satPeer(), 'x', true).repairQuarantine, true);
  const fresh = peerForm(undefined, R);
  assert.equal(fresh.peerId, R);
  assert.equal(fresh.isNew, true);
  assert.equal(peerRequest({ ...fresh, label: 'x', expectedInstallationId: 'bad' }, 1).error, 'Invalid installation ID: UUID must be canonical lowercase hyphenated text');
  assert.equal(peerRequest({ ...fresh, label: ' ' }, 1).error, 'A peer needs a label');
  // Disabling also clears reads and dispatch (the daemon refuses them on a disabled peer).
  assert.deepEqual(togglePeerEdit(satPeer(), 9), {
    expected_registry_revision: 9, peer_id: P, label: 'mac-mini', expected_installation_id: Q,
    enabled: false, read_enabled: false, dispatch_enabled: false, repair_quarantine: false,
  });
  const off = satPeer({ config: { ...satPeer().config, enabled: false, read_enabled: true } });
  assert.equal(togglePeerEdit(off, 1).read_enabled, true);
});

test('link edits: the form, the request and the d toggle', () => {
  const l = satPeer().links[0];
  const f = linkForm(P, l, 'unused');
  assert.deepEqual(linkRequest({ ...f, sshTarget: ' rsi@mini ' }, 5).edit, {
    expected_registry_revision: 5, peer_id: P, link_id: R, direction: 'dial_home_reverse', socket_path: '/run/rsi/mini.sock',
    ssh_target: 'rsi@mini', trust_reference: 'known_hosts:mini', enabled: true, priority: 1,
  });
  const fresh = linkForm(P, undefined, Q);
  assert.equal(fresh.linkId, Q);
  assert.equal(linkRequest(fresh, 1).error, 'A link needs a socket path');
  assert.equal(linkRequest({ ...fresh, socketPath: '/s', priority: '256' }, 1).error, 'Priority is 0 to 255');
  assert.equal(toggleLinkEdit(P, l, 3).enabled, false);
  assert.equal(toggleLinkEdit(P, l, 3).expected_registry_revision, 3);
});

const remote = (over = {}) => ({
  enabled: true, canonical_host: 'hub.ts.net', url: 'https://hub.ts.net/', unit_installed: true, gateway_running: true,
  serve_route_present: true, funnel_off: true, serve_pending_command: null,
  tailscale: { reachable: true, backend_state: 'Running', version: '1.102.4', pinned_version: '1.102.3', version_qualified: true },
  peers: [{ id: 'n1', name: 'phone', os: 'iOS', online: true, allowed: true }, { id: 'n2', name: 'tablet', os: 'iOS', online: false, allowed: false }],
  projects: [{ id: 'p1', name: 'rsi', exposed: true }, { id: 'p2', name: 'web', exposed: false }],
  allow_answers: false, allow_messages: true, errors: [],
  ...over,
});

test('remote edits send the whole allowed or exposed list, capped at 32 projects', () => {
  assert.deepEqual(remoteDeviceEdit(remote(), 0), { allowed_node_ids: [] });
  assert.deepEqual(remoteDeviceEdit(remote(), 1), { allowed_node_ids: ['n1', 'n2'] });
  assert.equal(remoteDeviceEdit(remote(), 5), null);
  assert.deepEqual(remoteProjectEdit(remote(), 1), { ok: true, edit: { project_ids: ['p1', 'p2'] } });
  assert.deepEqual(remoteProjectEdit(remote(), 0), { ok: true, edit: { project_ids: [] } });
  const full = remote({ projects: [...Array.from({ length: MAX_REMOTE_PROJECTS }, (_, i) => ({ id: `x${i}`, name: '', exposed: true })), { id: 'y', name: '', exposed: false }] });
  assert.equal(remoteProjectEdit(full, MAX_REMOTE_PROJECTS).error, 'At most 32 projects can be exposed');
  const rows = remoteSummary(remote({ serve_pending_command: 'sudo tailscale set --operator=$USER', errors: ['boom'] }));
  assert.deepEqual(rows[0], ['Remote', 'enabled']);
  assert.ok(rows.some(([k, v]) => k === 'run once' && v.includes('tailscale set')));
  assert.ok(rows.some(([k, v]) => k === 'problem' && v === 'boom'));
  assert.ok(rows.some(([k, v]) => k === 'messages from phone' && v === 'on'));
});

test('scratch adoption confirms exactly the listed paths', () => {
  const list = [
    { path: '/tmp/rsi-a', kind: 'worker_tmp', blocker: null, bytes: 2048 },
    { path: '/var/tmp/rsi-b', kind: 'var_tmp', blocker: 'dirty_worktree', bytes: 10, detail: null },
    { path: '/tmp/rsi-c', kind: 'lander', blocker: 'held', bytes: 0, detail: 'pid 42 cargo' },
  ];
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(2048), '2.0 KiB');
  assert.equal(scratchVerdict(list[0]), 'adoptable now');
  assert.equal(scratchVerdict(list[1]), 'dirty worktree');
  assert.deepEqual(adoptOne(list[0]), { ok: true, paths: ['/tmp/rsi-a'] });
  assert.equal(adoptOne(list[2]).error, 'Not adoptable: held (pid 42 cargo)');
  assert.deepEqual(adoptAll(list), { ok: true, paths: ['/tmp/rsi-a'] });
  assert.equal(adoptAll([list[1]]).error, 'No directory is adoptable now');
  const many = Array.from({ length: 40 }, (_, i) => ({ path: `/tmp/${i}`, kind: 'worker_tmp', blocker: null, bytes: 0 }));
  assert.equal(adoptAll(many).paths.length, MAX_ADOPT_PATHS);
  assert.equal(adoptSummary([{ path: '/a', adopted: true }, { path: '/b', adopted: false, refusal: 'young' }]), 'Adopted 1 of 2; /b: young');
});

const DIGEST = `sha256:${'ab'.repeat(32)}`;
const audit = (over = {}) => ({
  repository_identity: 'github.com/o/r', canonical_repo_dir: '/srv/r', target_ref: 'refs/heads/rolling', target_oid: 'abc',
  plan_digest: DIGEST, authorization_phrase: `APPLY github.com/o/r ${DIGEST}`, applyable: true,
  counts: { observed: 3, eligible: 2, retained: 1, settled: 0, refused: 0, recovery_required: 0, unattempted: 0 }, items: [],
  ...over,
});

test('settlement apply needs a fresh applyable audit of the selection and the exact phrase', () => {
  assert.equal(applyBlock(audit(), 'github.com/o/r'), null);
  assert.equal(applyBlock(audit(), 'github.com/o/other'), 'Run a fresh applyable audit before Apply');
  assert.equal(applyBlock(null, 'github.com/o/r'), 'Run a fresh applyable audit before Apply');
  assert.equal(applyBlock(audit({ applyable: false, authorization_phrase: null, refusal: 'dirty' }), 'github.com/o/r'), 'Apply refused by audit: dirty');
  assert.equal(applyBlock(audit({ applyable: false, authorization_phrase: null }), 'github.com/o/r'), 'Apply refused by audit: no eligible worktree roots to settle');
  const key = settlementKey(Q);
  assert.equal(key, `desktop-${Q}`);
  assert.match(key, /^[A-Za-z0-9._:-]{1,128}$/);
  assert.deepEqual(settlementApply(audit(), `APPLY github.com/o/r ${DIGEST}`, key), {
    ok: true, apply: { repository_identity: 'github.com/o/r', plan_digest: DIGEST, authorization: `APPLY github.com/o/r ${DIGEST}`, idempotency_key: key },
  });
  for (const typed of ['', `apply github.com/o/r ${DIGEST}`, `APPLY github.com/o/r ${DIGEST} `, 'APPLY github.com/o/r']) {
    assert.equal(settlementApply(audit(), typed, key).error, 'Authorization phrase does not match exactly', JSON.stringify(typed));
  }
  assert.equal(countsLine(audit().counts), 'observed 3 · eligible 2 · retained 1 · settled 0 · refused 0 · recovery 0 · unattempted 0');
  assert.equal(pageCursor(0, 30, 'J'), 10);
  assert.equal(pageCursor(25, 30, 'PageDown'), 29);
  assert.equal(pageCursor(5, 30, 'K'), 0);
  assert.equal(pageCursor(5, 30, 'j'), 6);
});

test('the settlement phrase is never prefilled', () => {
  const src = readFileSync(here('../src/admin-ops.ts'), 'utf8');
  assert.ok(!/phrase\.value\s*=\s*(audit|a)\b/.test(src), 'the typed phrase is never set from the audit');
  assert.match(src, /phrase\.value = ''/);
});
