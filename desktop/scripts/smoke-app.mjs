#!/usr/bin/env node
// The packaged-app acceptance check on macOS (local #259, the #255 method made repeatable).
//
//   node scripts/smoke-app.mjs [--app "PATH/RSI Desktop.app"] [--out DIR] [--keep-run]      (npm run smoke:app)
//
// The headless smokes run Chrome, and #247's harness runs a bare WKWebView; neither is the shipped app (#257: wry
// configures WebKit differently). This check runs the real packaged app:
//   1. Builds it with `scripts/install-desktop.sh --build-only` (no install: ~/Applications and ~/.local/bin are never
//      touched), or takes --app (or $RSI_SMOKE_APP).
//   2. Copies the bundle into a temp run directory under its own bundle id (dev.rsi.desktop.smoke-app), so WebKit's
//      localStorage (the Vim keys switch, first-run answers) never meets the operator's dev.rsi.desktop store. HOME and
//      CFFIXED_USER_HOME point at the run directory, so the terminal's shell and AppKit's per-user data stay there too.
//   3. Compiles scripts/app-driver.m into a dylib and loads it with DYLD_INSERT_LIBRARIES into that copy only (the
//      variable is set on that one child's environment; the driver unsets it at once). It needs no TCC grant: it sends
//      key events to the app's own window, runs page JavaScript, takes WKWebView snapshots and reads the app's own
//      window-level AX tree, all in-process. "Keyboard navigation" is off for that process (-AppleKeyboardUIMode 0).
//   4. Runs the checks against the smoke's fake rsid (scripts/smoke-fake.mjs): the daemon-down card (the app starts
//      on a socket nobody listens on; Start rsid is never pressed), the reconnect when the fake comes up, Tab to New
//      session / Stop / Send and back, Tab inside the confirm and New session dialogs, F6 between dashboard panes with
//      Vim keys off, Esc reaching vim in the terminal's real PTY, toast and dialog ARIA, and the window's AX tree.
//   5. Quits the app, stops the fake, kills nothing it did not start, deletes the test bundle's ~/Library folders and
//      writes DIR/report.json plus PNG snapshots (default /tmp/desktop-smoke-app).
// Exit: 0 every check passed, 1 a check failed, 2 bad usage, 3 runner_unavailable (not macOS, no clang, no GUI
// session, the app cannot be built, or a hardened-runtime app that ignores DYLD_INSERT_LIBRARIES); never 0 with 0 checks.
// It never talks to the live rsid and never changes a system or operator setting.
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { closeSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, readdirSync, renameSync, rmSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir, userInfo } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SESSION_IDS, startFakeDaemon } from './smoke-fake.mjs';
import { STEPS as WEBKIT_STEPS, judge, probeSource, setupSource } from './smoke-webkit.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(here, '..', '..');
const DRIVER = join(here, 'app-driver.m');
/** The copy's bundle id; app-driver.m refuses to act in any bundle whose id does not start with it. */
export const TEST_BUNDLE_ID = 'dev.rsi.desktop.smoke-app';
/** keyengine.ts VIM_KEYS_ASKED_KEY: 'pending' on a fresh store, 'answered' once the first-run question is settled. */
const VIM_KEYS_ASKED = 'rsi.desktop.vimKeysAsked';
/** The checks after the build: launch to quit, cleanup included. */
const RUN_TIMEOUT_MS = 180_000;
const BUILD_TIMEOUT_MS = 25 * 60_000;

export function parseArgs(argv, env = process.env) {
  const o = { app: env.RSI_SMOKE_APP || null, out: '/tmp/desktop-smoke-app', keepRun: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--app') o.app = argv[++i];
    else if (a === '--out') o.out = argv[++i];
    else if (a === '--keep-run') o.keepRun = true;
    else throw new Error(`unknown argument ${a}`);
    if (o.app === undefined || o.out === undefined) throw new Error(`${a} needs a value`);
  }
  return o;
}

/** clang from the Xcode command line tools, or null. */
export function findClang(run = spawnSync) {
  const r = run('xcrun', ['--find', 'clang'], { encoding: 'utf8' });
  return r.status === 0 && r.stdout.trim() ? r.stdout.trim() : null;
}

/** True in a logged-in GUI (Aqua) session; an ssh login or a LaunchDaemon has no window server for the app. */
export function guiSession(run = spawnSync) {
  const r = run('launchctl', ['managername'], { encoding: 'utf8' });
  return r.status === 0 && r.stdout.trim() === 'Aqua';
}

/** Why this host cannot run the packaged-app check, or null. */
export function unavailableReason({ platform = process.platform, clang, gui } = {}) {
  if (platform !== 'darwin') return `the packaged-app check drives the macOS app; this host is ${platform}`;
  if (!clang) return 'no clang (install the Xcode command line tools: xcode-select --install)';
  if (!gui) return 'no GUI (Aqua) session: launchctl managername is not Aqua (an ssh login or a background job)';
  return null;
}

/** The copy's environment: a short allowlist of the caller's, the test HOME, the fake socket and the driver. */
export function appEnv({ base = process.env, home, socket, ctl, dylib }) {
  const env = {};
  for (const k of ['PATH', 'USER', 'LOGNAME', 'LANG', 'LC_ALL', 'LC_CTYPE', 'TMPDIR']) if (base[k]) env[k] = base[k];
  // No RSI_* (session tokens, sockets) and no DYLD_* reach the app or its shells from the caller.
  return { ...env, HOME: home, CFFIXED_USER_HOME: home, SHELL: '/bin/zsh', RSI_DAEMON_SOCKET: socket, RSI_APP_DRIVER_CTL: ctl, DYLD_INSERT_LIBRARIES: dylib };
}

/** The test bundle's per-user folders under ~/Library (WebKit can write there whatever HOME says); exact id only. */
export function libraryLeftovers(home = userInfo().homedir, id = TEST_BUNDLE_ID) {
  if (!id.startsWith(TEST_BUNDLE_ID) || /[/\s]/.test(id)) throw new Error(`refusing to clean ${id}: not a test bundle id`);
  const lib = join(home, 'Library');
  return [
    join(lib, 'WebKit', id), join(lib, 'Caches', id), join(lib, 'HTTPStorages', id), join(lib, 'HTTPStorages', `${id}.binarycookies`),
    join(lib, 'Saved Application State', `${id}.savedState`), join(lib, 'Preferences', `${id}.plist`),
  ];
}

/** `built: <path>.app` from install-desktop.sh's output, or null. */
export function builtAppFrom(log) {
  const m = /^built: (.+\.app)\s*$/m.exec(log);
  return m ? m[1] : null;
}

/** True when codesign says the binary has the hardened runtime (dyld then ignores DYLD_INSERT_LIBRARIES). */
export function hardenedRuntime(codesignOutput) {
  const m = /flags=0x[0-9a-f]+\(([^)]*)\)/.exec(codesignOutput);
  return !!m && m[1].split(',').includes('runtime');
}

function buildApp(out) {
  const log = join(out, 'build.log');
  const fd = openSync(log, 'w');
  const started = Date.now();
  const r = spawnSync('bash', [join(ROOT, 'scripts', 'install-desktop.sh'), '--build-only'], { cwd: ROOT, stdio: ['ignore', fd, fd], timeout: BUILD_TIMEOUT_MS });
  closeSync(fd);
  const seconds = Math.round((Date.now() - started) / 1000);
  const text = readFileSync(log, 'utf8');
  const app = builtAppFrom(text);
  if (r.status !== 0 || !app) {
    const tail = text.trim().split('\n').slice(-3).join(' | ');
    return { unavailable: `the app could not be built (scripts/install-desktop.sh --build-only exit ${r.status ?? r.signal ?? r.error?.message}; ${tail}; log ${log})`, seconds };
  }
  return { app, seconds, log };
}

function compileDriver() {
  const source = readFileSync(DRIVER);
  const dir = join(tmpdir(), `rsi-app-driver-${createHash('sha256').update(source).digest('hex').slice(0, 16)}`);
  const dylib = join(dir, 'app-driver.dylib');
  if (existsSync(dylib)) return dylib;
  mkdirSync(dir, { recursive: true });
  const tmp = `${dylib}.${process.pid}`;
  // Through xcrun, which supplies the SDK (the bare clang path finds no Cocoa headers).
  const r = spawnSync('xcrun', ['clang', '-dynamiclib', '-fobjc-arc', '-Wall', '-framework', 'Cocoa', '-framework', 'WebKit', '-framework', 'ApplicationServices', '-o', tmp, DRIVER], { encoding: 'utf8', timeout: 60_000 });
  if (r.status !== 0) throw new Error(`compiling app-driver.m failed: ${(r.stderr ?? '').slice(-2000)}`);
  renameSync(tmp, dylib);
  return dylib;
}

/** Talks to app-driver.m through its control directory. */
class Driver {
  constructor(ctl, child) { this.ctl = ctl; this.child = child; this.id = 0; }
  async send(cmd, timeoutMs = 20_000) {
    const id = ++this.id;
    const tmp = join(this.ctl, '.cmd.tmp');
    writeFileSync(tmp, JSON.stringify({ ...cmd, id }));
    renameSync(tmp, join(this.ctl, 'cmd.json'));
    const resp = join(this.ctl, `resp-${id}.json`);
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (this.child.exitCode !== null || this.child.signalCode !== null) throw new Error(`the app exited (${this.child.exitCode ?? this.child.signalCode}) during ${cmd.op}`);
      if (existsSync(resp)) {
        const r = JSON.parse(readFileSync(resp, 'utf8'));
        unlinkSync(resp);
        return r;
      }
      await sleep(20);
    }
    throw new Error(`the driver did not answer ${cmd.op} within ${timeoutMs} ms`);
  }
  /** Page JavaScript (an async function body); its value, or a thrown Error. */
  async js(body, timeoutMs) {
    const r = await this.send({ op: 'js', js: body }, timeoutMs);
    if (!r.ok) throw new Error(`page js failed: ${r.error}`);
    return r.result === null ? null : JSON.parse(r.result);
  }
  async keys(...keys) { const r = await this.send({ op: 'key', keys }); if (!r.ok) throw new Error(r.errors.join('; ')); }
  async type(text) { const r = await this.send({ op: 'type', text }); if (!r.ok) throw new Error(r.error); }
  async snap(path) { const r = await this.send({ op: 'snap', path }); return r.ok ? path : null; }
  /** Poll a page expression until truthy; false on timeout. */
  async waitFor(expr, timeoutMs = 8000) {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      try { if (await this.js(`return !!(${expr});`)) return true; } catch { /* a reload in flight */ }
      await sleep(100);
    }
    return false;
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const q = (s) => `document.querySelector(${JSON.stringify(s)})`;
const setVimOff = `{ const c = document.getElementById('vim-keys'); if (c && c.checked) { c.checked = false; c.dispatchEvent(new Event('change')); } await new Promise((r) => setTimeout(r, 150)); }`;
const closeDialogs = `{ for (const d of document.querySelectorAll('dialog[open]')) d.close(); await new Promise((r) => setTimeout(r, 100)); }`;
/** The focused element, through shadow roots, as a short label. */
const FOCUS = `(() => { let e = document.activeElement; while (e && e.shadowRoot && e.shadowRoot.activeElement) e = e.shadowRoot.activeElement;
  if (!e || e === document.body) return 'body'; const t = (e.getAttribute('aria-label') || e.textContent || '').trim().replace(/\\s+/g, ' ').slice(0, 30);
  return e.tagName.toLowerCase() + (e.id ? '#' + e.id : '') + (e.className && typeof e.className === 'string' ? '.' + e.className.split(' ')[0] : '') + (t ? ' "' + t + '"' : ''); })()`;
/** A dialog's accessible name from aria-label or aria-labelledby. */
const dialogName = (sel) => `(() => { const d = ${q(sel)}; if (!d) return null; const l = (d.getAttribute('aria-label') || '').trim(); if (l) return l;
  return (d.getAttribute('aria-labelledby') || '').split(/\\s+/).filter(Boolean).map((id) => (document.getElementById(id)?.textContent || '').trim()).join(' '); })()`;

/** The #247 WebKit steps this check runs in the real app (the control fixture is for the bare WKWebView; see #257). */
export const APP_WALK_STEPS = ['page-tab', 'page-shift-tab', 'confirm-dialog', 'launch-dialog'];
export const walkSteps = () => APP_WALK_STEPS.map((id) => WEBKIT_STEPS.find((s) => s.id === id));

/** Child processes of `pid` as { pid, comm }. */
function children(pid) {
  const r = spawnSync('ps', ['-axo', 'pid=,ppid=,comm='], { encoding: 'utf8' });
  return (r.stdout ?? '').split('\n').map((l) => l.trim().split(/\s+/)).filter((p) => p.length >= 3 && Number(p[1]) === pid)
    .map(([p, , ...c]) => ({ pid: Number(p), comm: c.join(' ') }));
}
const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };

/** The checks, in order; each returns { status, note, evidence? } and may throw (a fail). */
function checks(ctx) {
  const { d, out, run } = ctx;
  const snap = (name) => d.snap(join(out, name));
  return [
    {
      id: 'daemon-down', title: 'started on a socket nobody listens on, the app shows the daemon-down card; Start rsid is shown, never pressed',
      async run() {
        if (!(await d.waitFor(`${q('#empty')}?.dataset.onboarding === 'daemon-down'`, 20_000))) return fail(`no daemon-down card: onboarding=${await d.js(`return ${q('#empty')}?.dataset.onboarding ?? null;`)}`);
        const card = await d.js(`const h = ${q('#empty')}; const s = h.querySelector('button[data-start-daemon]');
          return { title: h.querySelector('h2')?.textContent, text: h.textContent, boot: document.documentElement.dataset.boot,
            start: s ? { label: s.textContent, disabled: s.disabled } : null, retry: !!h.querySelector('button[data-action="refresh"]'),
            status: h.querySelector('.onboard-start-status')?.getAttribute('role') ?? null };`);
        const evidence = await snap('01-daemon-down.png');
        if (card.title !== 'Cannot reach the rsid daemon') return fail(`card title ${JSON.stringify(card.title)}`);
        if (!card.text.includes(ctx.socket)) return fail(`the card does not name the socket ${ctx.socket}`);
        if (!card.start || card.start.label !== 'Start rsid') return fail(`no Start rsid button: ${JSON.stringify(card.start)}`);
        // The test HOME has no ~/Library/LaunchAgents/dev.rsi.rsid.plist, so there is nothing to start: disabled, explained.
        if (!card.start.disabled || card.status !== 'status') return fail(`Start rsid should be disabled with a status line in the test HOME: ${JSON.stringify(card.start)} status=${card.status}`);
        if (!card.retry) return fail('no Retry now button');
        return pass(`boot=${card.boot}; "${card.title}" names ${ctx.socket}; Start rsid shown, disabled with a status line (no service in the test HOME), never pressed; Retry now present`, evidence);
      },
    },
    {
      id: 'reconnect', title: 'when the (fake) rsid comes up on that socket the app reconnects on its own and lists the sessions',
      async run() {
        ctx.fake = await startFakeDaemon();
        symlinkSync(ctx.fake.socket, ctx.socket);
        const started = Date.now();
        const ok = await d.waitFor(`document.documentElement.dataset.boot === 'ready' && document.querySelectorAll('#session-list li').length > 0`, 30_000);
        if (!ok) return fail(`not ready 30 s after the fake came up: boot=${await d.js(`return document.documentElement.dataset.boot ?? null;`)}`);
        return pass(`ready ${((Date.now() - started) / 1000).toFixed(1)} s after the fake came up, with no key or click; sessions listed`);
      },
    },
    ...walkSteps().map((step) => ({
      id: step.id, title: `${step.title} (real app, #247 step)`,
      async run() {
        const r = await d.send({ op: 'walk', setup: setupSource(step), probe: probeSource(step), key: step.shift ? 'shift-tab' : 'tab', max: step.max, settleMs: 300 }, 60_000);
        const j = judge(step, r.ok || r.setupError ? r : null);
        const evidence = await snap(`02-${step.id}.png`);
        return { status: j.status, note: j.note, evidence, focusOrder: j.focusOrder };
      },
    })),
    {
      id: 'f6-panes', title: 'with Vim keys off F6 and Shift-F6 move between the dashboard projects table and its conversation',
      async run() {
        await d.js(`${closeDialogs} ${setVimOff} void (await import('./dashboard.js')).openDashboard(); return true;`);
        if (!(await d.waitFor(`${q('#dashboard-view')} && !${q('#dashboard-view')}.hidden && document.activeElement?.classList.contains('dash-table-wrap')`))) return fail(`the dashboard did not open with focus on the projects table (focus ${await d.js(`return ${FOCUS};`)})`);
        const where = `return { pane: document.activeElement?.closest('.dash-conv-log') ? 'conversation' : document.activeElement?.closest('.dash-table-wrap') ? 'table' : ${FOCUS}, hints: ${q('#dashboard-view .dash-hints')}?.textContent ?? '' };`;
        const seen = [];
        let evidence = null;
        for (const key of ['f6', 'f6', 'shift-f6', 'shift-f6']) {
          await d.keys(key);
          await sleep(150);
          const w = await d.js(where);
          seen.push(`${key}→${w.pane}`);
          if (!evidence) { evidence = await snap('03-dashboard-after-f6.png'); ctx.hint = w.hints; }
        }
        await d.keys('esc');
        await sleep(150);
        const closed = await d.js(`return !${q('#dashboard-view')} || ${q('#dashboard-view')}.hidden;`);
        if (!closed) await d.js(`[...document.querySelectorAll('#dashboard-view button')].find((b) => /^Close/.test(b.textContent.trim()))?.click(); return true;`).catch(() => {});
        const want = ['f6→conversation', 'f6→table', 'shift-f6→conversation', 'shift-f6→table'];
        if (seen.join() !== want.join()) return fail(`F6 went ${seen.join(', ')}; expected ${want.join(', ')}`, evidence);
        if (!/F6/.test(ctx.hint ?? '')) return fail(`the conversation pane's hint does not name F6: ${JSON.stringify(ctx.hint)}`, evidence);
        return pass(`${seen.join(', ')}; hint "${(ctx.hint ?? '').slice(0, 80)}"; Esc ${closed ? 'closed' : 'did not close'} the dashboard`, evidence);
      },
    },
    {
      id: 'terminal-esc', title: 'in the terminal (a real PTY) Esc reaches the shell: vim leaves insert mode and :wq writes the file',
      async run() {
        const file = join(run, 'vim-esc.txt');
        await d.js(`${closeDialogs} ${setVimOff} (await import('./commands.js')).dispatch('toggle-terminal', { source: 'palette' }); return true;`);
        if (!(await d.waitFor(`document.activeElement?.classList.contains('xterm-helper-textarea')`, 10_000))) return fail(`the terminal did not take focus (focus ${await d.js(`return ${FOCUS};`)})`);
        const shell = await until(() => children(ctx.pid).find((c) => /(^|\/)-?zsh$/.test(c.comm)), 10_000);
        if (!shell) return fail(`no shell started under the app (children: ${JSON.stringify(children(ctx.pid))})`);
        await sleep(800);
        await d.type(`vim -u NONE -i NONE -N ${file}\r`);
        const vim = await until(() => children(shell.pid).find((c) => /(^|\/)vim$/.test(c.comm)), 8000);
        if (!vim) return fail(`vim did not start under the shell (pid ${shell.pid})`);
        await sleep(500);
        await d.type('ihello from smoke');
        await sleep(300);
        const evidence = await snap('04-terminal-vim-insert.png');
        await d.keys('esc');
        await sleep(300);
        const after = await d.js(`return { open: !!${q('#terminal-overlay')} && !${q('#terminal-overlay')}.hidden, focus: document.activeElement?.classList.contains('xterm-helper-textarea') };`);
        await d.type(':wq\r');
        const gone = await until(() => !alive(vim.pid) || null, 8000);
        const text = existsSync(file) ? readFileSync(file, 'utf8') : null;
        await snap('05-terminal-after-wq.png');
        if (gone) { await d.type('exit\r'); await until(() => !alive(shell.pid) || null, 5000); }
        await d.js(`if (${q('#terminal-overlay')} && !${q('#terminal-overlay')}.hidden) (await import('./commands.js')).dispatch('toggle-terminal', { source: 'palette' }); return true;`).catch(() => {});
        if (!after.open || !after.focus) return fail(`Esc closed the terminal or took its focus: ${JSON.stringify(after)}`, evidence);
        if (text !== 'hello from smoke\n' || !gone) return fail(`after Esc and :wq the file holds ${JSON.stringify(text)} and vim ${gone ? 'exited' : 'still runs'}: Esc did not reach vim`, evidence);
        return pass(`zsh (pid ${shell.pid}) under the app, vim (pid ${vim.pid}) under zsh; after "ihello from smoke", Esc kept the overlay and focus, and :wq wrote ${JSON.stringify(text)} and ended vim`, evidence);
      },
    },
    {
      id: 'dialog-aria', title: 'the Archive confirm is a named modal alertdialog, an error toast is an assertive alert, a confirmation toast is a polite status inside the open dialog, the conversation is a polite log',
      async run() {
        const problems = [];
        await d.js(`${closeDialogs} ${setVimOff} (await import('./sessions.js')).select(${JSON.stringify(SESSION_IDS.worker)}); return true;`);
        if (!(await d.waitFor(`${q('#archive')}?.offsetParent`))) return fail('the session view with Archive did not open');
        await d.js(`${q('#archive')}.focus(); return true;`);
        await d.keys('enter');
        if (!(await d.waitFor(`!!${q('dialog.confirm[open]')}`, 4000))) return fail('real Enter on Archive did not open the confirm');
        const confirm = await d.js(`const c = ${q('dialog.confirm[open]')}; return { role: c.getAttribute('role'), modal: c.matches(':modal'), name: ${dialogName('dialog.confirm[open]')}, focus: ${FOCUS} };`);
        const ev1 = await snap('06-archive-confirm.png');
        if (confirm.role !== 'alertdialog' || !confirm.modal || !confirm.name) problems.push(`confirm ${JSON.stringify(confirm)}`);
        // Tab to the dialog's Archive button and confirm with real Enter: the fake refuses ArchiveSession, an error toast.
        let onArchive = false;
        for (let i = 0; i < 6 && !onArchive; i++) {
          await d.keys('tab');
          onArchive = await d.js(`const e = document.activeElement; return !!e && e.closest('dialog.confirm[open]') !== null && e.textContent.trim().startsWith('Archive');`);
        }
        if (!onArchive) return fail(`Tab never reached the confirm's Archive button (${JSON.stringify(confirm)})`, ev1);
        await d.keys('enter');
        const shown = await d.waitFor(`!${q('#toast')}.hidden && /Archive/.test(${q('#toast')}.textContent)`, 5000);
        const err = await d.js(`const t = ${q('#toast')}; return { hidden: t.hidden, text: t.textContent.trim().slice(0, 80), role: t.getAttribute('role'), live: t.getAttribute('aria-live'), atomic: t.getAttribute('aria-atomic') };`);
        await snap('07-error-toast.png');
        if (!shown || err.role !== 'alert' || err.live !== 'assertive') problems.push(`error toast ${JSON.stringify(err)}`);
        if (!ctx.fake.received.some((r) => r.method === 'ArchiveSession')) problems.push('the fake rsid never received ArchiveSession');
        // New session by real Enter, then a confirmation toast while it is open: inside the dialog, polite, on top.
        await d.js(`${q('#toast')}.hidden = true; ${q('#new-session')}.focus(); return true;`);
        await d.keys('enter');
        if (!(await d.waitFor(`!!${q('#launch-dialog[open]')}`, 4000))) return fail(`real Enter on New session did not open the dialog; ${problems.join('; ')}`, ev1);
        const inDialog = await d.js(`(await import('./dom.js')).toast('Smoke: saved', true); await new Promise((r) => setTimeout(r, 200));
          const t = ${q('#toast')}; const b = t.getBoundingClientRect(); const hit = document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2);
          return { parent: t.parentElement?.id || t.parentElement?.tagName, role: t.getAttribute('role'), live: t.getAttribute('aria-live'), top: !!hit && (hit === t || t.contains(hit)), name: ${dialogName('#launch-dialog')} };`);
        const ev2 = await snap('08-status-toast-in-dialog.png');
        if (inDialog.parent !== 'launch-dialog' || inDialog.role !== 'status' || inDialog.live !== 'polite' || !inDialog.top || inDialog.name !== 'New session') problems.push(`toast in dialog ${JSON.stringify(inDialog)}`);
        await d.keys('esc');
        await sleep(250);
        const back = await d.js(`return { open: !!${q('#launch-dialog[open]')}, parent: ${q('#toast')}.parentElement?.tagName, focus: ${FOCUS} };`);
        if (back.open || back.parent !== 'BODY' || !back.focus.startsWith('button#new-session')) problems.push(`after Esc ${JSON.stringify(back)}`);
        const log = await d.js(`const e = ${q('#events')}; return { role: e.getAttribute('role'), live: e.getAttribute('aria-live'), relevant: e.getAttribute('aria-relevant'), label: e.getAttribute('aria-label') };`);
        if (log.role !== 'log' || log.live !== 'polite' || log.relevant !== 'additions') problems.push(`#events ${JSON.stringify(log)}`);
        if (problems.length) return fail(problems.join('; '), ev2);
        return pass(`confirm: alertdialog, modal, named "${confirm.name.slice(0, 50)}", focus ${confirm.focus}; error toast role=alert aria-live=assertive "${err.text.slice(0, 40)}"; `
          + `toast in New session: role=status polite, topmost, dialog named "${inDialog.name}"; Esc closed it, the toast went back to <body>, focus to New session; #events role=log polite additions`, ev2);
      },
    },
    {
      id: 'ax-window', title: "the app's own AX tree (window level) names its window; the web content's tree needs an Accessibility grant and is not covered",
      async run() {
        const path = join(out, 'ax-window.json');
        const r = await d.send({ op: 'ax', path, depth: 4 });
        const windows = (r.tree?.children ?? []).filter((c) => c.AXRole === 'AXWindow');
        if (!windows.length) return fail(`no AXWindow under the app element (axTrusted=${r.axTrusted})`);
        return pass(`AXWindow ${windows.map((w) => JSON.stringify(w.AXTitle ?? '')).join(', ')} with ${windows[0].children?.length ?? 0} children; axTrusted=${r.axTrusted}`, path);
      },
    },
  ];
}

const pass = (note, evidence = null) => ({ status: 'pass', note, evidence });
const fail = (note, evidence = null) => ({ status: 'fail', note, evidence });
async function until(f, timeoutMs) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) { const v = f(); if (v) return v; await sleep(100); }
  return null;
}

/** Launch the copy, run the checks, quit and clean up. Always returns { results, info, cleanup }. */
async function runApp({ app, dylib, out, keepRun }) {
  const run = mkdtempSync('/tmp/rsi-smoke-app-');
  const home = join(run, 'home');
  const ctl = join(run, 'ctl');
  mkdirSync(home);
  mkdirSync(ctl);
  // An empty zsh startup file: no new-user wizard, no operator rc, a plain prompt.
  writeFileSync(join(home, '.zshrc'), "PS1='smoke%# '\n");
  const socket = join(run, 'd.sock');
  const copy = join(run, 'RSI Desktop Smoke.app');
  const results = [];
  const cleanup = { library: [], processes: [] };
  const ctx = { out, run, socket, fake: null, pid: null };
  let child = null;
  let info = null;
  const appExit = { code: null };
  const deadline = setTimeout(() => { if (child?.exitCode === null) child.kill('SIGKILL'); }, RUN_TIMEOUT_MS);
  try {
    for (const p of libraryLeftovers()) rmSync(p, { recursive: true, force: true });
    const cp = spawnSync('cp', ['-R', app, copy]);
    if (cp.status !== 0) throw new Error(`copying ${app} failed: ${cp.stderr}`);
    const pl = spawnSync('plutil', ['-replace', 'CFBundleIdentifier', '-string', TEST_BUNDLE_ID, join(copy, 'Contents', 'Info.plist')], { encoding: 'utf8' });
    if (pl.status !== 0) throw new Error(`plutil failed: ${pl.stderr}`);
    const bin = join(copy, 'Contents', 'MacOS', 'rsi-desktop');
    const stderr = openSync(join(out, 'app.stderr'), 'w');
    // The binary itself, not `open`: the copy's Info.plist (its bundle id) is the one AppKit reads, and env reaches it.
    child = spawn(bin, [], { env: appEnv({ home, socket, ctl, dylib }), stdio: ['ignore', stderr, stderr] });
    closeSync(stderr);
    child.once('exit', (code, sig) => { appExit.code = code ?? sig; });
    ctx.pid = child.pid;
    const d = new Driver(ctl, child);
    ctx.d = d;
    const end = Date.now() + 30_000;
    while (Date.now() < end) {
      try { info = await d.send({ op: 'info' }, 2000); if (info.hasWeb) break; } catch (e) { if (child.exitCode !== null) throw e; }
      await sleep(200);
    }
    if (!info?.hasWeb) throw new Error(`the app never showed its web view (driver ${info ? 'answered' : 'silent'}; see ${join(out, 'app.stderr')})`);
    await d.send({ op: 'deactivate' });
    // A fresh WebKit store (the first-run Vim keys question is pending) shows the copy is not on the operator's.
    await d.waitFor(`document.readyState === 'complete' && localStorage.getItem(${JSON.stringify(VIM_KEYS_ASKED)}) !== null`, 10_000);
    const asked = await d.js(`return localStorage.getItem(${JSON.stringify(VIM_KEYS_ASKED)});`);
    const launchOk = info.bundle === TEST_BUNDLE_ID && info.appleKeyboardUIMode === 0 && !info.dyldInsertInherited && asked === 'pending';
    results.push({ id: 'launch', title: 'the copy runs under the test bundle id on a fresh WebKit store, with Keyboard navigation off for that process, the driver loaded and DYLD_INSERT_LIBRARIES cleared',
      status: launchOk ? 'pass' : 'fail', note: `pid ${info.pid} bundle ${info.bundle} first-run ${VIM_KEYS_ASKED}=${asked} AppleKeyboardUIMode=${info.appleKeyboardUIMode} tabFocusesLinks=${info.tabFocusesLinks} WebKit ${info.webkit} axTrusted=${info.axTrusted} DYLD inherited=${info.dyldInsertInherited}` });
    // Answer the first-run question as an existing install would have (Vim keys stay off), then reload so boot reads it.
    await d.js(`localStorage.setItem(${JSON.stringify(VIM_KEYS_ASKED)}, 'answered'); setTimeout(() => location.reload(), 50); return true;`);
    await sleep(800);
    await d.waitFor(`document.readyState === 'complete' && !!document.documentElement.dataset.boot`, 15_000);
    for (const c of checks(ctx)) {
      const t0 = Date.now();
      let r;
      try { r = await c.run(); } catch (e) { r = fail(`threw: ${e.message}`); }
      results.push({ id: c.id, title: c.title, ...r, durationMs: Date.now() - t0 });
      if (child.exitCode !== null || child.signalCode !== null) break;
    }
  } catch (e) {
    results.push({ id: 'harness', title: 'the run completes', status: 'fail', note: e.message });
  } finally {
    clearTimeout(deadline);
    if (child && child.exitCode === null && child.signalCode === null) {
      const kids = children(child.pid);
      try { await ctx.d.send({ op: 'quit' }, 3000); } catch { /* fall through to signals */ }
      if (!(await until(() => child.exitCode !== null || child.signalCode !== null, 8000))) {
        child.kill('SIGTERM');
        if (!(await until(() => child.exitCode !== null || child.signalCode !== null, 3000))) child.kill('SIGKILL');
        cleanup.processes.push(`app pid ${child.pid} needed a signal`);
      }
      // A shell the app started outlives it only if it ignored the hangup: it is ours, end it.
      for (const k of kids) if (alive(k.pid)) { try { process.kill(k.pid, 'SIGKILL'); cleanup.processes.push(`killed leftover ${k.comm} ${k.pid}`); } catch { /* gone */ } }
    }
    await until(() => appExit.code !== null || !child, 3000);
    if (ctx.fake) await ctx.fake.stop();
    // Settle, then delete what the test bundle id wrote under ~/Library (exact id only).
    await sleep(500);
    for (const p of libraryLeftovers()) if (existsSync(p)) { rmSync(p, { recursive: true, force: true }); cleanup.library.push(p); }
    if (!keepRun) rmSync(run, { recursive: true, force: true });
  }
  if (ctx.fake) {
    cleanup.fakeRequests = ctx.fake.received.length;
    cleanup.fakeMethods = [...new Set(ctx.fake.received.map((r) => r.method))].sort();
  }
  cleanup.appExit = appExit.code;
  cleanup.appAlive = child ? alive(child.pid) : false;
  cleanup.runDir = keepRun ? run : `${run} (deleted)`;
  return { results, info, cleanup };
}

export async function main(argv = process.argv.slice(2), { platform = process.platform, clang, gui } = {}) {
  const started = Date.now();
  const opts = parseArgs(argv);
  if (platform === 'darwin') {
    clang ??= findClang();
    gui ??= guiSession();
  }
  const why = unavailableReason({ platform, clang, gui });
  const unavailable = (reason, extra = {}) => {
    mkdirSync(opts.out, { recursive: true });
    writeFileSync(join(opts.out, 'report.json'), `${JSON.stringify({ status: 'runner_unavailable', reason, checksExecuted: 0, elapsedMs: Date.now() - started, ...extra }, null, 2)}\n`);
    console.log(`runner_unavailable: ${reason}; 0 checks executed`);
    return 3;
  };
  if (why) { console.log(`runner_unavailable: ${why}; 0 checks executed`); return 3; }
  mkdirSync(opts.out, { recursive: true });
  for (const f of readdirSync(opts.out)) if (/\.(png|json|stderr)$/.test(f)) rmSync(join(opts.out, f), { force: true });
  let app = opts.app;
  let build = null;
  if (!app) {
    console.log('building the app: scripts/install-desktop.sh --build-only (no install; a few minutes from cold)…');
    build = buildApp(opts.out);
    if (build.unavailable) return unavailable(build.unavailable, { buildSeconds: build.seconds });
    app = build.app;
  }
  const bin = join(app, 'Contents', 'MacOS', 'rsi-desktop');
  if (!existsSync(bin)) { console.error(`no app binary at ${bin}`); return 2; }
  const sign = spawnSync('codesign', ['-dv', '--verbose=2', bin], { encoding: 'utf8' });
  if (hardenedRuntime(`${sign.stdout}${sign.stderr}`)) return unavailable(`${app} has the hardened runtime, so dyld ignores DYLD_INSERT_LIBRARIES; run it on an ad-hoc (make desktop) build`);
  const dylib = compileDriver();
  const sha = createHash('sha256').update(readFileSync(bin)).digest('hex');
  const { results, info, cleanup } = await runApp({ app, dylib, out: opts.out, keepRun: opts.keepRun });
  const checks = results.filter((r) => r.status === 'pass' || r.status === 'fail');
  const status = checks.length > 0 && checks.every((r) => r.status === 'pass') ? 'pass' : 'fail';
  const report = {
    status, app, appSha256: sha, bundleId: TEST_BUNDLE_ID, buildSeconds: build?.seconds ?? null, buildLog: build?.log ?? null,
    webkit: info?.webkit ?? null, tabFocusesLinks: info?.tabFocusesLinks ?? null, keyboardNavigation: 'off for the app process (AppleKeyboardUIMode=0, argument domain)',
    elapsedMs: Date.now() - started, results, cleanup,
    notCovered: ['VoiceOver speech and the web-content AX tree (a cross-process remote element: needs an operator Accessibility grant)',
      'pressing Start rsid (it would ask launchd to start the live service; the argv path is covered by service::tests)', 'the native menu bar (needs System Events)'],
  };
  writeFileSync(join(opts.out, 'report.json'), `${JSON.stringify(report, null, 2)}\n`);
  for (const r of results) console.log(`${r.status.toUpperCase().padEnd(5)} ${r.id.padEnd(15)} ${r.note}`);
  console.log(`app=${app} sha256=${sha.slice(0, 12)} webkit=${report.webkit} build=${report.buildSeconds ?? 'skipped (--app)'}s elapsed=${report.elapsedMs}ms`);
  console.log(`cleanup: app exit ${cleanup.appExit}, alive=${cleanup.appAlive}, library removed ${cleanup.library.length ? cleanup.library.join(', ') : 'nothing'}${cleanup.processes.length ? `, ${cleanup.processes.join(', ')}` : ''}`);
  console.log(`report: ${join(opts.out, 'report.json')}`);
  return status === 'pass' ? 0 : 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((c) => process.exit(c), (e) => { console.error(e); process.exit(2); });
}
