// The daemon-down help over an open session (local #266). The card in the empty detail pane is hidden when a
// session is selected, so the same service-state-aware actions also render as a strip above the layout.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { daemonBannerView, NEEDS_RSID, onboardingView, STALE_NOTE } from '../dist/onboarding-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const src = (f) => readFileSync(here(`../src/${f}`), 'utf8');

/** A small DOM: enough for el(), replaceChildren and reading the result back. */
class Node {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.children = []; this.dataset = {}; this.attrs = {}; this.listeners = {};
    this.className = ''; this.disabled = false; this._text = ''; this.tabIndex = -1; this.hidden = false;
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this._text = ''; this.children = []; this.append(...nodes); }
  setAttribute(k, v) { this.attrs[k] = v; }
  addEventListener(type, cb) { (this.listeners[type] ??= []).push(cb); }
  click() { for (const cb of this.listeners.click ?? []) cb(); }
  all(pred, out = []) { for (const c of this.children) { if (pred(c)) out.push(c); c.all(pred, out); } return out; }
}
globalThis.document = { createElement: (t) => new Node(t) };
const { fillDaemonBanner } = await import('../dist/daemon-banner.js');

const down = { kind: 'unreachable', socket: '/home/x/.rsi/daemon.sock', error: 'no such file' };
const launchd = { kind: 'installed', manager: 'launchd', name: 'dev.rsi.rsid', path: '/Users/x/Library/LaunchAgents/dev.rsi.rsid.plist' };
const running = { ...launchd, state: 'running', pid: 4242, socket: { app: '/home/x/.rsi/daemon.sock', service: '/home/x/.rsi/daemon.sock', matches: true } };
const otherSocket = { ...launchd, state: 'running', pid: 4242, socket: { app: '/tmp/none.sock', service: '/home/x/.rsi/daemon.sock', matches: false } };
const idle = { ...launchd, state: 'stopped', pid: null, socket: { app: '/home/x/.rsi/daemon.sock', service: '/home/x/.rsi/daemon.sock', matches: true } };
const banner = (service, run = { kind: 'idle' }, link = down) => daemonBannerView(link, 'mac', false, { service, run });

/** Render the strip and read back its buttons and text. */
function render(view) {
  const host = new Node('div');
  const pressed = [];
  fillDaemonBanner(host, view, () => pressed.push('start'));
  const buttons = host.all((n) => n.tagName === 'BUTTON');
  return { host, pressed, buttons, labels: buttons.map((b) => b.textContent), text: host.textContent };
}

test('no strip while rsid answers or before the first answer', () => {
  assert.equal(daemonBannerView({ kind: 'connected', socket: '/s' }, 'mac'), null);
  assert.equal(daemonBannerView({ kind: 'unknown' }, 'mac'), null);
  const r = render(null);
  assert.equal(r.host.children.length, 0);
});

test('with a session open and rsid down the strip says rsid is not running and offers Start rsid and Retry now', () => {
  const r = render(banner(launchd));
  assert.match(r.text, /rsid is not running/);
  assert.match(r.text, /last known state/, 'the shown state is marked as possibly out of date');
  assert.deepEqual(r.labels.slice(0, 2), ['Start rsid', 'Retry now']);
  const [start, retry] = r.buttons;
  assert.equal(start.disabled, false);
  assert.equal(start.dataset.startDaemon, '');
  assert.equal(retry.dataset.action, 'refresh', 'Retry now runs the registry action the card uses');
  start.click();
  assert.deepEqual(r.pressed, ['start'], 'Start rsid runs the same press handler as the card');
  assert.match(r.text, /asks macOS to start it/);
});

test('the strip never offers a Start the card would not: a running service gets a confirmed Restart, not Start rsid', () => {
  const r = render(banner(running));
  assert.ok(!r.labels.includes('Start rsid'), `got ${JSON.stringify(r.labels)}`);
  assert.equal(r.labels[0], 'Restart rsid');
  assert.match(r.text, /running \(pid 4242\) but is not answering/);
  assert.match(r.text, /interrupts any sessions it is running; you are asked first/);
});

test('the strip carries the card\'s wrong-socket refusal: Start rsid is off with both sockets named, no Restart', () => {
  const r = render(banner(otherSocket));
  assert.equal(r.labels[0], 'Start rsid');
  assert.equal(r.buttons[0].disabled, true);
  assert.ok(!r.labels.includes('Restart rsid'));
  assert.match(r.text, /\/tmp\/none\.sock/);
  assert.match(r.text, /\/home\/x\/\.rsi\/daemon\.sock/);
});

test('the strip and the card read the same service state (one source, two surfaces)', () => {
  for (const [name, service, run] of [
    ['idle', idle, { kind: 'idle' }], ['unloaded', launchd, { kind: 'idle' }], ['running', running, { kind: 'idle' }],
    ['restarting', running, { kind: 'restarting' }], ['other socket', otherSocket, { kind: 'idle' }],
    ['failed', launchd, { kind: 'failed', text: 'macOS (launchctl) could not start rsid: exit status 113.', detail: 'raw' }],
    ['not installed', { kind: 'not-installed', manager: 'launchd', looked: ['/x'] }, { kind: 'idle' }],
    ['unsupported', { kind: 'unsupported', reason: 'no service manager' }, { kind: 'idle' }],
    ['unknown', { kind: 'unknown' }, { kind: 'idle' }],
  ]) {
    const card = onboardingView(down, 1, 'mac', 1, false, { service, run });
    const strip = banner(service, run);
    assert.deepEqual(strip.start, card.start, `${name}: same Start rsid row`);
    assert.deepEqual(strip.buttons.map((b) => b.label), ['Retry now'], `${name}: Retry now`);
  }
});

test('while Starting rsid… the strip shows progress and the button is busy and off', () => {
  const r = render(banner(launchd, { kind: 'starting' }));
  assert.equal(r.labels[0], 'Starting rsid…');
  assert.equal(r.buttons[0].disabled, true);
  assert.equal(r.buttons[0].attrs['aria-busy'], 'true');
});

test('a failed start shows its plain failure and the raw output under Details', () => {
  const r = render(banner(launchd, { kind: 'failed', text: 'macOS (launchctl) could not start rsid: exit status 113.', detail: '$ launchctl kickstart x\nexit status 113\nnope' }));
  assert.match(r.text, /exit status 113/);
  assert.match(r.text, /Details/);
  assert.equal(r.labels[0], 'Start rsid');
  assert.equal(r.buttons[0].disabled, false, 'it can be pressed again');
});

test('what rsid is and where the window looked, and the terminal commands, sit under More', () => {
  const r = render(banner(launchd));
  const more = r.host.all((n) => n.tagName === 'DETAILS' && n.className === 'daemon-banner-more')[0];
  assert.ok(more, 'a More disclosure');
  assert.match(more.textContent, /Looking for it at \/home\/x\/\.rsi\/daemon\.sock/);
  assert.match(more.textContent, /make release-install/);
  assert.match(more.textContent, /RSI_DAEMON_SOCKET/);
});

test('a backend error also gets the strip, with Try again', () => {
  const r = render(daemonBannerView({ kind: 'backend-error', error: 'ipc closed' }, 'mac'));
  assert.match(r.text, /The desktop backend did not answer/);
  assert.match(r.text, /ipc closed/);
  assert.deepEqual(r.labels, ['Try again']);
});

test('stale wording is plain: what is shown is last known, and the disabled controls say why', () => {
  assert.equal(STALE_NOTE, 'last known: rsid is not running');
  assert.match(NEEDS_RSID, /Needs rsid/);
});

// ---------- wiring (the real page is covered by the first-run smoke scenario daemon-down-open-session) ----------

test('the strip sits above the layout, never takes focus and is hidden while the card is on screen', () => {
  const html = readFileSync(here('../static/index.html'), 'utf8');
  assert.ok(html.indexOf('id="daemon-banner"') > 0 && html.indexOf('id="daemon-banner"') < html.indexOf('class="layout"'), 'above .layout so the Issues list and dashboard do not hide it');
  assert.match(html, /id="daemon-banner"[^>]*role="status"[^>]*hidden/);
  const css = readFileSync(here('../static/styles.css'), 'utf8');
  assert.match(css, /body:has\(\.layout:not\(\[hidden\]\) #empty:not\(\[hidden\]\)\) \.daemon-banner \{ display: none; \}/);
  const boot = src('boot.ts');
  assert.match(boot, /keepFocus\(host, \(\) => fillDaemonBanner\(/);
  assert.doesNotMatch(boot.slice(boot.indexOf('export function renderDaemonBanner')), /focusCardPrimary\(host\)/, 'the strip never steals focus from the message box');
});

test('a link change redraws the strip, the open session and the controls', () => {
  const boot = src('boot.ts');
  assert.match(boot, /export function renderOnboarding\(\): void \{\n  renderDaemonBanner\(\);/, 'every card redraw (link change, service state, Start press) also redraws the strip');
  assert.match(boot, /state\.daemonDown = down;/);
  assert.match(boot, /if \(flipped\) for \(const cb of downListeners\) cb\(\);/);
  const sessions = src('sessions.ts');
  assert.match(sessions, /onDaemonDownChange\(\(\) => renderHeader\(\)\);/);
  assert.match(sessions, /state\.daemonDown \? \[el\('span', 'stale-note', STALE_NOTE\)\]/, 'the header marks the state as last known');
  assert.match(sessions, /\|\| state\.daemonDown;/, 'Stop this turn is off while rsid is down');
  const approvals = src('approvals.ts');
  assert.match(approvals, /pauseBtn\.disabled = !live \|\| down;/);
  // Archive and Restore follow archiveControls(status, down): both are off while rsid is down (local #268, tests/archive-restore.test.mjs).
  assert.match(approvals, /const ac = archiveControls\(s\.status, down\);/);
  assert.match(approvals, /archiveBtn\.disabled = ac\.archiveDisabled;/);
  assert.match(approvals, /restoreBtn\.disabled = ac\.restoreDisabled;/);
});

test('Send stays enabled while rsid is down and a failed send keeps the draft', () => {
  const sessions = src('sessions.ts');
  assert.doesNotMatch(sessions, /daemonDown[^\n]*composer|composer[^\n]*daemonDown/, 'the composer is not tied to the link');
  const actions = src('actions.ts');
  const send = actions.slice(actions.indexOf('export async function sendMessage'), actions.indexOf('export async function interrupt'));
  assert.ok(send.indexOf("box.value = ''") < send.indexOf('catch'), 'the box is cleared only after deliver() returned');
  assert.match(send, /catch \(e\) \{\n    toast\(`Send failed/);
});

test('the top-bar pill is a button while rsid is down and moves to the help', () => {
  const sessions = src('sessions.ts');
  assert.match(sessions, /conn\.setAttribute\('role', 'button'\);\n    conn\.tabIndex = 0;/);
  assert.match(sessions, /ev\.key !== 'Enter' && ev\.key !== ' '/);
  assert.match(src('main.ts'), /wireConnPill\(\);/);
  assert.match(src('boot.ts'), /export function focusDaemonHelp/);
});
