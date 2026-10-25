// First-run guidance and boot evidence (hub #3016, Issue #117).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { bootBanner, bootLine, summarizeBoot } from '../dist/boot-model.js';
import { onboardingView, shortError, startFailure, startTimedOut } from '../dist/onboarding-model.js';
import { ACTIONS } from '../dist/registry.js';
import { BOOT_ATTEMPTS, findChrome } from '../scripts/smoke-lib.mjs';
import { freshFirstRunPage, runFirstRun, scenarios } from '../scripts/smoke-firstrun.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const known = new Set(ACTIONS.map((a) => a.id));

test('daemon missing: the title, the socket, the terminal commands under For developers and a Retry button', () => {
  const v = onboardingView({ kind: 'unreachable', socket: '/home/x/.rsi/daemon.sock', error: 'no such file' }, 0, 'mac');
  assert.equal(v.id, 'daemon-down');
  assert.equal(v.title, 'Cannot reach the rsid daemon');
  const text = v.lines.join('\n');
  assert.match(text, /\/home\/x\/\.rsi\/daemon\.sock/);
  assert.match(text, /no such file/);
  assert.equal(v.developer.summary, 'For developers');
  assert.match(v.developer.lines.join('\n'), /run rsid, or \.\/scripts\/dev-daemon\.sh from a checkout; make release-install installs and starts it/);
  assert.match(v.developer.lines.join('\n'), /RSI_DAEMON_SOCKET/);
  assert.deepEqual(v.buttons.map((b) => b.action), ['refresh', 'palette']);
  assert.match(v.buttons[1].label, /Cmd-K/);
});

// Start rsid (local #232): src-tauri/src/service.rs answers daemon_service and start_daemon.
const down = { kind: 'unreachable', socket: '/home/x/.rsi/daemon.sock' };
const launchd = { kind: 'installed', manager: 'launchd', name: 'dev.rsi.rsid', path: '/Users/x/Library/LaunchAgents/dev.rsi.rsid.plist' };
const systemd = { kind: 'installed', manager: 'systemd', name: 'rsid.service', path: '/home/x/.config/systemd/user/rsid.service' };
const card = (service, run = { kind: 'idle' }) => onboardingView(down, 0, 'mac', 1, false, { service, run });

test('an installed service: Start rsid is the first, primary button and the card says what it starts', () => {
  const v = card(launchd);
  assert.deepEqual(v.start.button, { label: 'Start rsid', enabled: true, primary: true, busy: false });
  assert.equal(v.start.status, null);
  assert.deepEqual(v.buttons.map((b) => [b.action, b.label, Boolean(b.primary)]), [['refresh', 'Retry now', false], ['palette', 'Actions… (Cmd-K)', false]]);
  assert.match(v.lines[2], /installed as a login service \(dev\.rsi\.rsid\)\. Start rsid asks macOS to start it/);
  assert.match(card(systemd).lines[2], /systemd user service rsid\.service\. Start rsid asks systemd to start it/);
  assert.match(v.developer.lines.join('\n'), /make release-install/, 'the terminal commands stay under For developers');
});

test('pressed: Starting rsid… and a disabled, busy button until rsid answers', () => {
  const v = card(launchd, { kind: 'starting' });
  assert.deepEqual(v.start.button, { label: 'Starting rsid…', enabled: false, primary: true, busy: true });
  assert.match(v.start.status, /^Starting rsid… This window reconnects as soon as it answers\.$/);
  assert.equal(v.start.detail, null);
});

test('a refused start shows the exit status in words and the command and stderr under Details', () => {
  const f = startFailure({ ok: false, manager: 'launchd', command: 'launchctl kickstart gui/501/dev.rsi.rsid', status: 113, stderr: 'Could not find service\n' });
  assert.match(f.text, /^macOS \(launchctl\) could not start rsid: exit status 113\./);
  assert.equal(f.detail, '$ launchctl kickstart gui/501/dev.rsi.rsid\nexit status 113\nCould not find service');
  const v = card(launchd, { kind: 'failed', ...f });
  assert.equal(v.start.status, f.text);
  assert.equal(v.start.detail, f.detail);
  assert.equal(v.start.button.enabled, true, 'it can be tried again');
  const none = startFailure({ ok: false, manager: 'systemd', command: 'systemctl --user start rsid.service', status: null, stderr: '' });
  assert.match(none.text, /^systemd could not start rsid: no exit status/);
  assert.match(none.detail, /\(nothing on stderr\)$/);
  const refused = startFailure('no installed rsid launchd service was found (looked for /x)');
  assert.equal(refused.text, 'Could not start rsid: no installed rsid launchd service was found (looked for /x).');
  assert.match(startTimedOut('systemd', 'systemctl --user start rsid.service').text, /did not answer within 45 seconds.*journalctl --user -u rsid\.service/);
  assert.match(startTimedOut('launchd', 'launchctl kickstart x', 10).detail, /no answer on the socket within 10 s/);
});

const running = { ...launchd, state: 'running', pid: 4242, socket: { app: '/home/x/.rsi/daemon.sock', service: '/home/x/.rsi/daemon.sock', matches: true } };
const otherSocket = { ...launchd, state: 'running', pid: 4242, socket: { app: '/tmp/none.sock', service: '/home/x/.rsi/daemon.sock', matches: false } };

test('the service is running but not answering: no Start rsid, a Restart rsid that says what it costs (local #258)', () => {
  const v = card(running);
  assert.deepEqual(v.start.button, { label: 'Restart rsid', enabled: true, primary: false, busy: false });
  assert.doesNotMatch(JSON.stringify(v), /"Start rsid"/);
  assert.match(v.lines[2], /installed as a login service \(dev\.rsi\.rsid\) and is already running \(pid 4242\), but it is not answering\. Start rsid would do nothing, so it is not offered\./);
  assert.match(v.start.status, /^rsid's service is running \(pid 4242\) but is not answering at \/home\/x\/\.rsi\/daemon\.sock\./);
  assert.match(v.start.status, /Retry now checks again\. Restart rsid asks macOS to stop it and start it again, which interrupts any sessions it is running; you are asked first\./);
  assert.equal(v.buttons[0].label, 'Retry now');
  assert.equal(v.buttons[0].primary, true, 'Retry now is the primary button: it is the safe one');
  assert.match(card({ ...running, manager: 'systemd', name: 'rsid.service' }).start.status, /asks systemd to stop it/);
  const none = card({ ...running, pid: null });
  assert.match(none.start.status, /^rsid's service is running but is not answering/);
});

test('restarting: a disabled, busy Restarting rsid… button; a failed restart says restart and can be tried again', () => {
  const v = card(running, { kind: 'restarting' });
  assert.deepEqual(v.start.button, { label: 'Restarting rsid…', enabled: false, primary: false, busy: true });
  assert.match(v.start.status, /^Restarting rsid… This window reconnects as soon as it answers\.$/);
  const f = startFailure({ ok: false, manager: 'launchd', command: 'launchctl kickstart -k gui/501/dev.rsi.rsid', status: 3, stderr: 'nope' }, 'restart');
  assert.match(f.text, /^macOS \(launchctl\) could not restart rsid: exit status 3\./);
  assert.equal(startFailure('boom', 'restart').text, 'Could not restart rsid: boom.');
  assert.match(startTimedOut('launchd', 'x', 45, 'restart').text, /^rsid was restarted but did not answer within 45 seconds/);
  const failed = card(running, { kind: 'failed', ...f });
  assert.equal(failed.start.button.label, 'Restart rsid');
  assert.equal(failed.start.button.enabled, true);
  assert.equal(failed.start.status, f.text);
});

test('the window looks at another socket than the service holds: Start is off and the reason names both (local #258)', () => {
  for (const svc of [otherSocket, { ...otherSocket, state: 'stopped' }, { ...otherSocket, state: undefined }]) {
    const v = card(svc);
    assert.deepEqual(v.start.button, { label: 'Start rsid', enabled: false, primary: false, busy: false });
    assert.match(v.start.status, /this window is looking at \/tmp\/none\.sock, but the login service holds \/home\/x\/\.rsi\/daemon\.sock, so neither starting nor restarting it would make this window connect\./);
    assert.match(v.start.status, /without RSI_DAEMON_SOCKET, or with it set to \/home\/x\/\.rsi\/daemon\.sock\./);
    assert.match(v.lines[2], /serves a different socket than the one this window looks at/);
    assert.equal(v.buttons[0].primary, true);
  }
});

test('loaded but not running, not loaded, or an older backend: Start rsid is offered as before (local #258)', () => {
  const sock = { app: '/home/x/.rsi/daemon.sock', service: '/home/x/.rsi/daemon.sock', matches: true };
  for (const svc of [{ ...launchd, state: 'stopped', pid: null, socket: sock }, { ...launchd, state: 'unknown', pid: null, socket: sock }, launchd]) {
    const v = card(svc);
    assert.deepEqual(v.start.button, { label: 'Start rsid', enabled: true, primary: true, busy: false });
    assert.equal(v.start.status, null);
    assert.match(v.lines[2], /Start rsid asks macOS to start it/);
  }
});

test('nothing installed: Start rsid is off with a plain reason; Windows explains only; checking keeps Retry primary', () => {
  const missing = card({ kind: 'not-installed', manager: 'systemd', looked: ['/home/x/.config/systemd/user/rsid.service'] });
  assert.deepEqual(missing.start.button, { label: 'Start rsid', enabled: false, primary: false, busy: false });
  assert.match(missing.start.status, /rsid is not installed as a service on this computer/);
  assert.match(missing.start.status, /Install in the desktop README/);
  assert.equal(missing.start.detail, 'Looked for: /home/x/.config/systemd/user/rsid.service');
  assert.equal(missing.buttons[0].primary, true, 'Retry now is primary when Start rsid cannot run');
  const win = card({ kind: 'unsupported', reason: 'starting rsid from this window is not supported on this system' });
  assert.equal(win.start.button, null);
  assert.match(win.start.status, /not supported on this system/);
  assert.match(win.developer.lines.join('\n'), /run rsid/);
  const checking = onboardingView(down, 0, 'mac');
  assert.equal(checking.start.button.enabled, false);
  assert.match(checking.start.status, /Checking whether rsid is installed/);
});

test('the desktop starts rsid only through the service manager: a fixed argv, no shell, no rsid exec', () => {
  const rs = readFileSync(here('../src-tauri/src/service.rs'), 'utf8');
  const prod = rs.slice(0, rs.indexOf('#[cfg(test)]'));
  assert.match(prod, /#\[tauri::command\]\s+pub async fn start_daemon\(\)/, 'start_daemon takes no argument from the webview');
  assert.match(prod, /#\[tauri::command\]\s+pub async fn daemon_service\(\)/);
  assert.match(prod, /#\[tauri::command\]\s+pub async fn restart_daemon\(\)/, 'restart_daemon takes no argument either');
  assert.match(prod, /"kickstart", "-k"/);
  assert.equal((prod.match(/Command::new/g) ?? []).length, 1);
  assert.doesNotMatch(prod, /"(sh|bash|zsh|cmd|powershell)"|"-c"|"rsid"/);
  assert.match(prod, /"kickstart"/);
  assert.match(prod, /"--user", "start", SYSTEMD_UNIT/);
  assert.match(prod, /#\[cfg\(not\(any\(target_os = "macos", target_os = "linux"\)\)\)\]/, 'every other OS gets the unsupported error');
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

test('connected with no projects: say what a project is and offer Create your first project first', () => {
  const v = onboardingView({ kind: 'connected', socket: 's' }, 0, 'mac', 0);
  assert.equal(v.id, 'no-projects');
  assert.match(v.lines[0], /A project is a folder with your code that sessions work in/);
  assert.deepEqual(v.buttons.map((b) => [b.action, b.label]), [['project-new', 'Create your first project'], ['launch', 'New session'], ['palette', 'Actions…']]);
  assert.equal(v.buttons[0].primary, true);
  assert.match(v.lines.join('\n'), /New session \(Cmd-N\)/, 'the session steps are still there');
  assert.equal(onboardingView({ kind: 'connected', socket: 's' }, 0, 'mac', 2).id, 'no-sessions', 'with a project the old screen stays');
  const withSessions = onboardingView({ kind: 'connected', socket: 's' }, 4, 'mac', 0);
  assert.equal(withSessions.id, 'select');
  assert.deepEqual(withSessions.buttons.map((b) => b.action), ['project-new']);
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
  for (const l of links) for (const n of [0, 3]) for (const pc of [0, 2]) for (const b of onboardingView(l, n, 'mac', pc).buttons) assert.ok(known.has(b.action), b.action);
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
  assert.deepEqual(ids, ['daemon-missing', 'connected-empty', 'connected-no-projects', 'connected-sessions', 'settings-fails', 'daemon-recovers', 'daemon-starts', 'daemon-running', 'daemon-other-socket', 'daemon-down-open-session']);
});

const chrome = findChrome();
test('first-run boot recovers from interrupted loopback modules, but keeps timeouts and app faults visible', async () => {
  const sc = scenarios().find((s) => s.id === 'daemon-missing');
  const ready = { boot: 'offline', start: { disabled: false } };
  const failed = { url: 'http://127.0.0.1:1234/main.js', type: 'Script', errorText: 'net::ERR_NETWORK_CHANGED', canceled: false };
  function pageFor(attempts) {
    return {
      events: [], navigations: 0,
      async goto() {
        this.attempt = attempts[Math.min(this.navigations++, attempts.length - 1)];
        if (this.attempt.failed) this.events.push({ method: 'Network.loadingFailed', params: this.attempt.failed });
      },
      async element(selector, action, { stop }) {
        const cause = stop();
        if (cause) throw new Error(`page shell: stopped: ${cause}`);
      },
      async ready(expression, { stop }) {
        const cause = stop();
        if (cause) throw new Error(`boot: stopped: ${cause}`);
        if (this.attempt.error) throw new Error(this.attempt.error);
        assert.equal(sc.ready(ready), true);
        return ready;
      },
      async eval() { throw new Error('synthetic DOM capture unavailable'); },
    };
  }
  const page = pageFor([{ failed }, {}]);
  assert.deepEqual(await freshFirstRunPage(page, 'http://127.0.0.1:1234/', sc, []), ready);
  assert.equal(page.navigations, 2);
  for (const error of ['boot timed out after 5000ms', 'fixture app error']) {
    const fault = pageFor([{ error }, {}]);
    await assert.rejects(freshFirstRunPage(fault, 'http://127.0.0.1:1234/', sc, []), (e) => {
      assert.ok(e.message.includes(error));
      assert.equal(fault.navigations, 1);
      return true;
    });
  }
  const stuck = pageFor([{ failed }]);
  await assert.rejects(freshFirstRunPage(stuck, 'http://127.0.0.1:1234/', sc, []), (e) => {
    const diagnostics = JSON.parse(e.message.split('; startup diagnostics: ')[1]);
    assert.equal(stuck.navigations, BOOT_ATTEMPTS);
    assert.equal(diagnostics.attempts.length, BOOT_ATTEMPTS);
    assert.deepEqual(diagnostics.attempts[0].network, [failed]);
    assert.equal(diagnostics.dom.unavailable, 'synthetic DOM capture unavailable');
    return true;
  });
});

test('first run in headless Chrome: offline, empty, populated, settings failure, recovery (needs Chrome and a built dist)', { skip: !chrome || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false, timeout: 180000 }, async (t) => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-firstrun-'));
  const logs = [];
  const orig = console.log;
  console.log = (...a) => logs.push(a.join(' '));
  let code;
  try { code = await runFirstRun({ out, chromeBin: chrome, dist: here('../dist'), signal: t.signal }); } finally { console.log = orig; }
  assert.equal(code, 0, logs.join('\n'));
});
