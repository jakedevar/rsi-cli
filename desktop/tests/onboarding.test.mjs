// First-run guidance and boot evidence (hub #3016, Issue #117).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { bootBanner, bootLine, summarizeBoot } from '../dist/boot-model.js';
import { onboardingView, shortError } from '../dist/onboarding-model.js';
import { ACTIONS } from '../dist/registry.js';
import { findChrome } from '../scripts/smoke-lib.mjs';
import { runFirstRun, scenarios } from '../scripts/smoke-firstrun.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const known = new Set(ACTIONS.map((a) => a.id));

test('daemon missing: the title, the socket, how to start rsid and a Retry button', () => {
  const v = onboardingView({ kind: 'unreachable', socket: '/home/x/.rsi/daemon.sock', error: 'no such file' }, 0, 'mac');
  assert.equal(v.id, 'daemon-down');
  assert.equal(v.title, 'Cannot reach the rsid daemon');
  const text = v.lines.join('\n');
  assert.match(text, /\/home\/x\/\.rsi\/daemon\.sock/);
  assert.match(text, /no such file/);
  assert.match(text, /make release-install/);
  assert.match(text, /RSI_DAEMON_SOCKET/);
  assert.deepEqual(v.buttons.map((b) => b.action), ['refresh', 'palette']);
  assert.match(v.buttons[1].label, /Cmd-K/);
});

test('connected with no sessions: New session first, the palette next, Vim keys last', () => {
  const v = onboardingView({ kind: 'connected', socket: 's' }, 0, 'other');
  assert.equal(v.id, 'no-sessions');
  assert.match(v.lines[0], /New session \(Ctrl-N\)/);
  assert.match(v.lines[1], /Ctrl-K/);
  assert.match(v.lines.join('\n'), /Settings/);
  assert.deepEqual(v.buttons.map((b) => b.action), ['launch', 'palette']);
  assert.match(onboardingView({ kind: 'connected', socket: 's' }, 0, 'mac').lines[0], /Cmd-N/);
});

test('connected with sessions, connecting and a backend error each have their own view', () => {
  assert.equal(onboardingView({ kind: 'connected', socket: 's' }, 4, 'mac').id, 'select');
  assert.equal(onboardingView({ kind: 'connected', socket: 's' }, 4, 'mac').title, 'Select a session.');
  assert.equal(onboardingView({ kind: 'unknown' }, 0, 'mac').id, 'connecting');
  const b = onboardingView({ kind: 'backend-error', error: 'boom\n at x' }, 0, 'mac');
  assert.equal(b.id, 'backend-error');
  assert.equal(b.lines[0], 'boom at x');
});

test('every button the guidance offers is a registered action', () => {
  const links = [{ kind: 'unknown' }, { kind: 'unreachable', socket: 's' }, { kind: 'backend-error', error: 'e' }, { kind: 'connected', socket: 's' }];
  for (const l of links) for (const n of [0, 3]) for (const b of onboardingView(l, n, 'mac').buttons) assert.ok(known.has(b.action), b.action);
});

test('a long daemon error is cut to one line', () => {
  const t = shortError(`x\n${'y'.repeat(500)}`);
  assert.ok(t.length <= 200 && !t.includes('\n') && t.endsWith('…'));
});

const ok = (name) => ({ name, ok: true });

test('boot is ready only when every step passed and the page threw nothing', () => {
  const steps = ['daemon', 'projects', 'labels', 'sessions', 'settings', 'live'].map(ok);
  const r = summarizeBoot('connected', steps, 3, []);
  assert.equal(r.state, 'ready');
  assert.equal(bootLine(r), 'boot=ready daemon=connected sessions=3 steps=daemon:ok,projects:ok,labels:ok,sessions:ok,settings:ok,live:ok');
  assert.equal(bootBanner(r), null);
});

test('a failed Settings read is a failed boot with a visible banner (it used to be invisible)', () => {
  const steps = [ok('daemon'), { name: 'settings', ok: false, error: 'Method not found: GetDaemonConfig' }, ok('live')];
  const r = summarizeBoot('connected', steps, 2, []);
  assert.equal(r.state, 'failed');
  assert.match(bootBanner(r), /settings: Method not found: GetDaemonConfig/);
  assert.match(bootLine(r), /settings:failed/);
});

test('an unreachable daemon is an expected offline boot, not a failure, unless the page threw', () => {
  const steps = [{ name: 'daemon', ok: false, error: 'no socket' }, { name: 'sessions', ok: false, skipped: true, error: 'x' }];
  const off = summarizeBoot('unreachable', steps, 0, []);
  assert.equal(off.state, 'offline');
  assert.equal(bootBanner(off), null);
  assert.equal(bootLine(off), 'boot=offline daemon=unreachable sessions=0 steps=daemon:failed,sessions:skipped');
  const threw = summarizeBoot('unreachable', steps, 0, ['TypeError: x']);
  assert.equal(threw.state, 'failed');
  assert.match(bootBanner(threw), /page error: TypeError: x/);
  assert.match(bootLine(threw), /page_errors=1/);
});

test('an uncaught page error fails an otherwise good boot', () => {
  const r = summarizeBoot('connected', [ok('daemon')], 1, ['ReferenceError: q']);
  assert.equal(r.state, 'failed');
});

test('main.ts reports boot from the real steps and installs the error capture first', () => {
  const main = readFileSync(here('../src/main.ts'), 'utf8');
  assert.ok(main.indexOf('installBootErrorCapture()') < main.indexOf('wire();'), 'capture before anything can throw');
  for (const step of ['projects', 'labels', 'sessions', 'settings', 'live']) assert.match(main, new RegExp(`'${step}'`));
  assert.match(main, /finishBoot\(/);
});

test('the Rust side logs the boot line (boot_report) and the start line', () => {
  const rs = readFileSync(here('../src-tauri/src/boot.rs'), 'utf8');
  assert.match(rs, /#\[tauri::command\]\s+pub fn boot_report/);
  const lib = readFileSync(here('../src-tauri/src/lib.rs'), 'utf8');
  assert.match(lib, /rsi-desktop: started version=/);
  assert.match(readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8'), /boot::boot_report/);
});

test('every first-run scenario has checks', () => {
  const ids = scenarios().map((s) => s.id);
  assert.deepEqual(ids, ['daemon-missing', 'connected-empty', 'connected-sessions', 'settings-fails', 'daemon-recovers']);
});

const chrome = findChrome();
test('first run in headless Chrome: offline, empty, populated, settings failure, recovery (needs Chrome and a built dist)', { skip: !chrome || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false, timeout: 180000 }, async (t) => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-firstrun-'));
  const logs = [];
  const orig = console.log;
  console.log = (...a) => logs.push(a.join(' '));
  let code;
  try { code = await runFirstRun({ out, chromeBin: chrome, dist: here('../dist'), signal: t.signal }); } finally { console.log = orig; }
  assert.equal(code, 0, logs.join('\n'));
});
