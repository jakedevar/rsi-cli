#!/usr/bin/env node
// The macOS WebKit keyboard smoke (local #247). Every other desktop smoke drives headless Chrome, but the shipped macOS
// app runs WKWebView, whose Tab order differs: with System Settings › Keyboard › "Keyboard navigation" off (the macOS
// default) and WebKit's default preferences it skips every button, checkbox, link and summary that has no tabindex (#242,
// found by hand). The shipped app's wry turns tabFocusesLinks on, which lifts that skip (#257, see below).
//
//   node scripts/smoke-webkit.mjs [--out DIR] [--keep-keyboard-setting]     (npm run smoke:webkit)
//
// It serves the built desktop/dist (run `npm run build` first) on the same fake rsid and read-only bridge the Chrome smoke
// uses (scripts/smoke-fake.mjs, scripts/smoke-bridge.mjs), compiles scripts/webkit-smoke.swift with swiftc (cached by
// source hash under the temp directory) and runs it with "Keyboard navigation" off for that process
// (-AppleKeyboardUIMode 0, whatever the host's setting; --keep-keyboard-setting uses the host's). The harness sends real
// Tab and Shift-Tab key events through an NSWindow; this script judges the focus sequences and writes DIR/report.json.
// Exit: 0 every step passed, 1 a step failed, 2 bad usage or no dist, 3 runner_unavailable (not macOS, no swiftc, no
// window server session, or the control step shows Keyboard navigation stayed on); 0 steps executed is never a pass.
// Isolation (local #263): the harness is a bundle-less binary, which WebKit would give ~/Library/WebKit/webkit-smoke and
// ~/Library/Caches/webkit-smoke in the operator's home, kept across runs. Each run therefore gets a temp HOME (removed
// afterwards, harnessEnv) and the page a non-persistent data store, so nothing is written under the real ~/Library and
// every run starts clean. tests/smoke-webkit.test.mjs compares snapshotLeaks() before and after a real run.
// The harness never activates: its window shows for about ten seconds, the operator's keyboard focus stays put.
//
// Two configurations (local #257). The shipped app's wry sets WKPreferences.tabFocusesLinks = YES on every macOS webview
// (WRY_PREFERENCE below), and with it on WebKit's Tab reaches buttons whatever "Keyboard navigation" says. So the smoke
// runs every step twice: `wry` mirrors the shipped configuration (the order the user gets; its control asserts the plain
// button IS reached, so a harness that stopped applying the preference is caught), and `default` leaves the preference at
// WebKit's default (off), where the tabindex stamps of #242 are the only thing that makes buttons reachable. Decision:
// keep both, because the stamps are redundant in the shipped app only for as long as wry sets the preference. The wry
// version in src-tauri/Cargo.lock is pinned in WRY_CHECKED (tests/smoke-webkit.test.mjs fails when it moves), so a wry
// upgrade forces someone to re-read the preference before the `wry` mode may be trusted again.
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { rm } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import { delimiter, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createBridge, rpcCall } from './smoke-bridge.mjs';
import { SESSION_IDS, startFakeDaemon } from './smoke-fake.mjs';
import { EXISTING_INSTALL_SEED, newToken, serve } from './smoke-lib.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const DIST = resolve(here, '..', 'dist');
const HARNESS = join(here, 'webkit-smoke.swift');
/** One harness run (one configuration) stays under a minute (#247). */
const RUN_TIMEOUT_MS = 55_000;

/**
 * What the shipped app's wry does to WKPreferences, read from the wry source (not guessed): the version whose source was
 * read, the file and the exact line. tests/smoke-webkit.test.mjs compares `version` with src-tauri/Cargo.lock and, when
 * the crate source is in the cargo registry, with `line`, and fails when either moves: re-read wry's WKPreferences
 * setup, then update this record and the `wry` mode below.
 */
export const WRY_PREFERENCE = {
  version: '0.57.0', file: 'src/wkwebview/mod.rs', key: 'tabFocusesLinks',
  line: '_preference.setValue_forKey(Some(&_yes), ns_string!("tabFocusesLinks"));',
};

/** The configurations every step runs in. `tabFocusesLinks` is the WKPreferences value the harness sets. */
export const MODES = [
  { id: 'wry', tabFocusesLinks: true, title: `shipped configuration (wry ${WRY_PREFERENCE.version} sets tabFocusesLinks = YES)` },
  { id: 'default', tabFocusesLinks: false, title: 'WebKit default (tabFocusesLinks off): only the tabindex stamps make buttons reachable' },
];

export function parseArgs(argv) {
  const o = { out: '/tmp/desktop-smoke-webkit', keepKeyboardSetting: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--out') o.out = argv[++i];
    else if (a === '--keep-keyboard-setting') o.keepKeyboardSetting = true;
    else throw new Error(`unknown argument ${a}`);
  }
  return o;
}

/** swiftc on PATH (or $RSI_SMOKE_SWIFTC), or null; the caller reports `runner_unavailable`. */
export function findSwiftc(env = process.env, exists = existsSync) {
  const candidates = [env.RSI_SMOKE_SWIFTC, ...(env.PATH ?? '').split(delimiter).filter(Boolean).map((d) => join(d, 'swiftc'))].filter(Boolean);
  return candidates.find((c) => exists(c)) ?? null;
}

/** Why this host cannot run the WebKit smoke, or null when it can try. */
export function unavailableReason({ platform = process.platform, swiftc = findSwiftc() } = {}) {
  if (platform !== 'darwin') return `the WebKit smoke needs macOS (WKWebView); this host is ${platform}`;
  if (!swiftc) return 'no swiftc on PATH (install the Xcode command line tools, or set RSI_SMOKE_SWIFTC)';
  return null;
}

/** `selector`'s button whose text starts with `label`, as an expression. */
const buttonIn = (selector, label) =>
  `[...document.querySelectorAll(${JSON.stringify(`${selector} button`)})].find((b) => b.textContent.trim().startsWith(${JSON.stringify(label)}))`;
const q = (selector) => `document.querySelector(${JSON.stringify(selector)})`;
const sleep = (ms) => `await new Promise((r) => setTimeout(r, ${ms}))`;
const waitFor = (expr, what) => `{ const end = Date.now() + 4000; while (!(${expr})) { if (Date.now() > end) throw new Error(${JSON.stringify(`${what} did not happen`)}); ${sleep(50)}; } }`;
const setVim = (on) => `{ const c = document.getElementById('vim-keys'); if (c.checked !== ${on}) { c.checked = ${on}; c.dispatchEvent(new Event('change')); } ${sleep(150)}; }`;
const shown = (id) => `(() => { const v = document.getElementById(${JSON.stringify(id)}); return !!v && !v.hidden; })()`;
const closeDialogs = `{ document.getElementById('wk-control')?.remove(); for (const d of document.querySelectorAll('dialog[open]')) d.close(); ${sleep(100)}; }`;
/** A button in the control fixture (a shadow root, so the page's tab-stop stamp never reaches it). */
const control = (id) => `document.getElementById('wk-control')?.shadowRoot?.getElementById(${JSON.stringify(id)})`;
const blur = `document.activeElement?.blur?.();`;

/**
 * The checks: #242 (Tab reaches every control with Keyboard navigation off) and #243 (with Vim keys off no pane takes
 * Tab, so it follows the native order). Each step's `targets` are reached by real Tab (or Shift-Tab) presses; `ordered`
 * targets must be reached in that order; a `scope` must hold the focus on every press (a modal dialog keeps Tab inside).
 */
export const CONTROL_ID = 'control-plain-button';
const controlSetup = [closeDialogs, `{ const host = document.createElement('div'); host.id = 'wk-control'; host.style.cssText = 'position:fixed;top:0;left:0;z-index:99999;background:#fff';
      host.attachShadow({ mode: 'open' }).innerHTML = '<input id="wk-a" aria-label="start"><button id="wk-b">Plain button</button><button id="wk-c" tabindex="0">Stamped button</button>';
      document.body.append(host); ${sleep(50)}; host.shadowRoot.getElementById('wk-a').focus(); }`];
/** The control proves the configuration of this run: a button without tabindex is skipped by default and reached with the wry preference. */
const controlStep = (mode) => mode.tabFocusesLinks
  ? { id: CONTROL_ID, title: 'control: with tabFocusesLinks on (as wry sets it) WebKit stops on a button that has no tabindex, and on one with tabindex="0"',
      setup: controlSetup, targets: [['plain button', control('wk-b')], ['stamped button', control('wk-c')]], max: 4 }
  : { id: CONTROL_ID, title: 'control: with Keyboard navigation off and tabFocusesLinks off WebKit skips a button that has no tabindex and stops on one with tabindex="0"',
      setup: controlSetup, targets: [['stamped button', control('wk-c')]], forbidden: [['plain button', control('wk-b')]], max: 3 };

const PAGE_STEPS = [
  {
    id: 'page-tab', title: 'with Vim keys off Tab reaches New session, Stop this turn, Archive, the conversation, the message box and Send in page order',
    setup: [closeDialogs, setVim(false), `(await import('./sessions.js')).select(${JSON.stringify(SESSION_IDS.worker)});`,
      waitFor(`${shown('session-view')} && ${q('#interrupt')}?.offsetParent`, 'the session view opening'), sleep(300),
      // Start the walk at the first control in the page, so the order is the page's own.
      `${q('#vim-keys')}.focus();`],
    targets: [['New session', q('#new-session')], ['Stop this turn', q('#interrupt')], ['Archive', q('#archive')],
      ['Conversation', q('#events')], ['message box', q('#message')], ['Send', q('#composer button[type="submit"]')]],
    ordered: true, max: 60,
  },
  {
    id: 'page-shift-tab', title: 'Shift-Tab from Send walks back through the message box, the conversation, Archive and Stop this turn',
    setup: [closeDialogs, setVim(false), `${q('#composer button[type="submit"]')}.focus();`],
    targets: [['message box', q('#message')], ['Conversation', q('#events')], ['Archive', q('#archive')], ['Stop this turn', q('#interrupt')]],
    ordered: true, shift: true, max: 20,
  },
  {
    id: 'confirm-dialog', title: 'in a confirm dialog (Archive) Tab reaches Cancel and Archive and stays in the dialog',
    setup: [closeDialogs, setVim(false), `void (await import('./approvals.js')).confirmDialog('WebKit smoke: archive this session?', 'Archive', { title: 'Archive' });`,
      waitFor(`!!${q('dialog.confirm[open]')}`, 'the confirm dialog opening')],
    targets: [['Cancel', buttonIn('dialog.confirm[open]', 'Cancel')], ['Archive', buttonIn('dialog.confirm[open]', 'Archive')]],
    scope: 'dialog.confirm[open]', min: 5, max: 8,
  },
  {
    id: 'question-dialog', title: 'in the question dialog Tab reaches the answer box, Let the agent decide, Hide and Submit and stays in the dialog',
    setup: [closeDialogs, setVim(false), `(await import('./actions/lifecycle.js')).openQuestionModal();`,
      waitFor(`!!${q('#question-dialog[open]')}`, 'the question dialog opening')],
    targets: [['answer box', q('#question-dialog textarea')], ['Let the agent decide', buttonIn('#question-dialog', 'Let the agent decide')],
      ['Hide', buttonIn('#question-dialog', 'Hide')], ['Submit', buttonIn('#question-dialog', 'Submit')]],
    scope: '#question-dialog', min: 8, max: 16,
  },
  {
    id: 'launch-dialog', title: 'in the New session dialog Tab reaches Cancel and Launch and stays in the dialog',
    setup: [closeDialogs, setVim(false), `${q('#new-session')}.click();`, waitFor(`!!${q('#launch-dialog[open]')}`, 'the launch dialog opening')],
    targets: [['Cancel', q('#launch-cancel')], ['Launch', q('#launch-dialog button[type="submit"]')]],
    scope: '#launch-dialog', max: 40,
  },
  {
    id: 'settings-tab', title: 'with Vim keys off Tab follows native order in Settings and reaches Refresh, then Close',
    setup: [closeDialogs, setVim(true), `(await import('./settings.js')).openSettings();`, waitFor(shown('settings-view'), 'Settings opening'),
      sleep(300), setVim(false), blur],
    targets: [['Refresh', buttonIn('#settings-view', 'Refresh')], ['Close', buttonIn('#settings-view', 'Close')]],
    ordered: true, max: 80,
  },
  {
    id: 'issues-tab', title: 'with Vim keys off Tab follows native order in Issues and reaches Refresh, New Issue, then Close',
    setup: [closeDialogs, `${buttonIn('#settings-view', 'Close')}?.click();`, setVim(true), `${q('#open-issues')}.click();`,
      waitFor(shown('issues-view'), 'Issues opening'), sleep(300), setVim(false), blur],
    targets: [['Refresh', buttonIn('#issues-view', 'Refresh')], ['New Issue', buttonIn('#issues-view', 'New Issue')], ['Close', buttonIn('#issues-view', 'Close')]],
    ordered: true, max: 80,
  },
];

/** The checks one configuration runs: its control, then the shared steps. */
export const stepsFor = (mode) => [controlStep(mode), ...PAGE_STEPS];
/** The shipped configuration's steps (same ids in every mode). */
export const STEPS = stepsFor(MODES[0]);

/** The setup the harness awaits before the first press: reset the probe state, then the step's own lines. */
export function setupSource(step) {
  return `(async () => { window.__wk = { n: 0, hits: [] }; ${step.setup.join(';\n')};\n return true; })()`;
}

/** The probe the harness runs after every press: a JSON string with the focused element, the target hit and done. */
export function probeSource(step) {
  const list = (pairs = []) => `[${pairs.map(([label, expr]) => `[${JSON.stringify(label)}, () => (${expr})]`).join(', ')}]`;
  const min = step.min ?? 0;
  return `(() => {
    const w = window.__wk; w.n++;
    let e = document.activeElement;
    while (e && e.shadowRoot && e.shadowRoot.activeElement) e = e.shadowRoot.activeElement;
    const text = !e || e === document.body ? '' : (e.getAttribute('aria-label') || e.textContent || e.value || '').trim().replace(/\\s+/g, ' ').slice(0, 30);
    const focus = !e || e === document.body ? 'body' : e.tagName.toLowerCase() + (e.id ? '#' + e.id : '') + ' "' + text + '"';
    const match = (pairs) => pairs.find(([, f]) => { const t = f(); return !!t && t === e; });
    const hit = match(${list(step.targets)});
    const forbidden = match(${list(step.forbidden)});
    if (hit && !w.hits.includes(hit[0])) w.hits.push(hit[0]);
    const scope = ${step.scope ? q(step.scope) : 'null'};
    return JSON.stringify({ focus, hit: hit ? hit[0] : null, forbidden: forbidden ? forbidden[0] : null, inScope: scope && e && e !== document.body ? scope.contains(e) : null,
      done: w.hits.length === ${step.targets.length} && w.n >= ${min} });
  })()`;
}

/** The plan webkit-smoke.swift reads. */
export function buildPlan(url, mode = MODES[0], steps = stepsFor(mode)) {
  return {
    url, tabFocusesLinks: mode.tabFocusesLinks, userScript: EXISTING_INSTALL_SEED, timeoutSeconds: 45,
    ready: `document.documentElement.dataset.boot === 'ready' && document.querySelectorAll('#session-list li').length > 0`,
    steps: steps.map((s) => ({ id: s.id, setup: setupSource(s), probe: probeSource(s), max: s.max, shift: !!s.shift, settleMs: 250 })),
  };
}

/** Judge one step from the harness's answers: { status, note, focusOrder }. */
export function judge(step, run) {
  if (!run) return { status: 'fail', note: 'the harness never ran this step', focusOrder: [] };
  const presses = run.presses.map((p) => { try { return JSON.parse(p); } catch { return { focus: `unreadable: ${p}`, hit: null }; } });
  const focusOrder = presses.map((p) => p.focus ?? `error: ${p.error}`);
  const key = step.shift ? 'Shift-Tab' : 'Tab';
  if (run.setupError) return { status: 'fail', note: `setup failed: ${run.setupError}`, focusOrder };
  if (presses.length === 0) return { status: 'fail', note: `no ${key} press was sent`, focusOrder };
  const bad = presses.findIndex((p) => p.forbidden);
  if (bad >= 0) return { status: 'fail', note: `${key} press ${bad + 1} focused the ${presses[bad].forbidden}, which WebKit skips with Keyboard navigation off (is it on?)`, focusOrder };
  const reached = [];
  for (const p of presses) if (p.hit && !reached.includes(p.hit)) reached.push(p.hit);
  const labels = step.targets.map(([l]) => l);
  const missing = labels.filter((l) => !reached.includes(l));
  if (missing.length) return { status: 'fail', note: `${key} never reached ${missing.join(', ')} in ${presses.length} presses (reached: ${reached.join(', ') || 'none'})`, focusOrder };
  if (step.ordered && reached.join('\n') !== labels.join('\n')) return { status: 'fail', note: `${key} reached ${reached.join(', ')}, not in the order ${labels.join(', ')}`, focusOrder };
  const out = step.scope ? presses.findIndex((p) => p.inScope === false) : -1;
  if (out >= 0) return { status: 'fail', note: `${key} press ${out + 1} left ${step.scope}: focus on ${presses[out].focus}`, focusOrder };
  const at = labels.map((l) => `${l} (${presses.findIndex((p) => p.hit === l) + 1})`);
  return { status: 'pass', note: `${key} reached ${at.join(', ')}${step.scope ? `; ${presses.length} presses stayed in the dialog` : ''}`, focusOrder };
}

/** Parse the harness's stdout: its result object, or null. */
export function parseHarnessOutput(stdout) {
  const line = stdout.split('\n').find((l) => l.startsWith('WEBKIT-SMOKE-RESULT '));
  if (!line) return null;
  try { return JSON.parse(line.slice('WEBKIT-SMOKE-RESULT '.length)); } catch { return null; }
}

/** Compile the harness once per source (the binary is cached by hash): { bin } or { unavailable: reason }. */
function compileHarness(swiftc) {
  const source = readFileSync(HARNESS);
  const dir = join(tmpdir(), `rsi-webkit-smoke-${createHash('sha256').update(source).digest('hex').slice(0, 16)}`);
  const bin = join(dir, 'webkit-smoke');
  if (existsSync(bin)) return { bin };
  mkdirSync(dir, { recursive: true });
  const tmp = `${bin}.${process.pid}`;
  const r = spawnSync(swiftc, ['-O', '-o', tmp, HARNESS], { encoding: 'utf8', timeout: 40_000 });
  if (r.status !== 0) {
    // /usr/bin/swiftc is a stub until the Xcode command line tools are installed: that host has no Swift, not a bad harness.
    if (/xcrun: error|active developer path|command line tools/i.test(r.stderr ?? '')) return { unavailable: `swiftc cannot run: ${(r.stderr ?? '').trim().split('\n')[0]}` };
    throw new Error(`swiftc failed (${r.error?.message ?? r.status}): ${(r.stderr ?? '').slice(-2000)}`);
  }
  renameSync(tmp, bin);
  return { bin };
}

/**
 * The harness's environment: the caller's, with the home directory moved into `home` (a temp dir the caller removes).
 * The harness is a bundle-less binary, so WebKit would otherwise keep its website data and caches in the operator's
 * ~/Library/WebKit/webkit-smoke and ~/Library/Caches/webkit-smoke (#263). HOME covers the C side, CFFIXED_USER_HOME
 * what NSHomeDirectory reads first; the page itself also runs on a non-persistent store (webkit-smoke.swift).
 */
export function harnessEnv(home, env = process.env) {
  return { ...env, HOME: home, CFFIXED_USER_HOME: home };
}

/** The entries under the two ~/Library folders a bundle-less WebKit process writes to, named like the harness (`webkit-smoke*`), with a recursive listing of each. */
export const LEAK_ROOTS = [['Library', 'WebKit'], ['Library', 'Caches']];
export function snapshotLeaks(home = homedir(), name = 'webkit-smoke') {
  const out = {};
  const walk = (dir, rel) => {
    let entries;
    try { entries = readdirSync(dir, { withFileTypes: true }); } catch { return; }
    for (const e of entries) {
      const full = join(dir, e.name);
      const key = `${rel}/${e.name}`;
      if (e.isDirectory()) { out[key] = 'dir'; walk(full, key); } else { const st = lstatSync(full); out[key] = `${st.size}:${st.mtimeMs}`; }
    }
  };
  for (const root of LEAK_ROOTS) {
    const base = join(home, ...root);
    let names = [];
    try { names = readdirSync(base).filter((n) => n.startsWith(name)); } catch { /* no such folder: nothing there */ }
    for (const n of names) { const rel = `${root.join('/')}/${n}`; out[rel] = 'dir'; walk(join(base, n), rel); }
  }
  return out;
}

function runHarness(bin, planPath, { keepKeyboardSetting, signal, timeout, home }) {
  const args = [planPath, ...(keepKeyboardSetting ? [] : ['-AppleKeyboardUIMode', '0'])];
  return new Promise((resolveRun) => {
    const child = spawn(bin, args, { stdio: ['ignore', 'pipe', 'pipe'], env: harnessEnv(home) });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (d) => { stdout += d; });
    child.stderr.on('data', (d) => { stderr = (stderr + d).slice(-4000); });
    const kill = () => child.kill('SIGKILL');
    const timer = setTimeout(kill, timeout);
    signal?.addEventListener('abort', kill, { once: true });
    child.once('error', (error) => { clearTimeout(timer); resolveRun({ code: -1, stdout, stderr: `${stderr}${error.message}` }); });
    child.once('exit', (code, sig) => {
      clearTimeout(timer);
      signal?.removeEventListener('abort', kill);
      resolveRun({ code: code ?? -1, signal: sig, stdout, stderr });
    });
  });
}

/** One configuration's results: boot, the preference as WebKit read it back, then each step. */
function judgeMode(mode, out, run) {
  const tag = (r) => ({ ...r, id: `${mode.id}/${r.id}`, mode: mode.id });
  if (!out || !out.booted) {
    return [tag({ id: 'boot', title: 'the app boots in WKWebView on the fake daemon', status: 'fail',
      note: `harness exit ${run.code}${run.signal ? ` (${run.signal})` : ''}; ${run.stderr.trim().slice(-600) || 'no output'}`, focusOrder: [] })];
  }
  const steps = stepsFor(mode);
  const results = [
    { id: 'boot', title: 'the app boots in WKWebView on the fake daemon', status: 'pass', note: 'booted, data-boot=ready, session list present', focusOrder: [] },
    // The harness reads the preference back from WebKit: a run that did not apply it checks the wrong configuration.
    { id: 'preference', title: `WKPreferences.${WRY_PREFERENCE.key} is ${mode.tabFocusesLinks ? 'on' : 'off'} in this run`,
      status: out.tabFocusesLinks === mode.tabFocusesLinks ? 'pass' : 'fail', note: `WebKit reports ${out.tabFocusesLinks}`, focusOrder: [] },
    ...steps.map((step) => ({ id: step.id, title: step.title, ...judge(step, out.steps.find((s) => s.id === step.id)) })),
  ];
  if (out.timedOut) results.push({ id: 'timeout', title: 'the harness finishes inside its budget', status: 'fail', note: 'the harness timed out', focusOrder: [] });
  return results.map(tag);
}

export async function main(argv = process.argv.slice(2), { signal, platform = process.platform, swiftc = findSwiftc() } = {}) {
  const started = Date.now();
  const opts = parseArgs(argv);
  const why = unavailableReason({ platform, swiftc });
  if (why) { console.log(`runner_unavailable: ${why}; 0 steps executed`); return 3; }
  if (!existsSync(join(DIST, 'index.html'))) {
    console.error('desktop/dist is missing: run `npm run build` in desktop/ first');
    return 2;
  }
  mkdirSync(opts.out, { recursive: true });
  const { bin, unavailable } = compileHarness(swiftc);
  if (unavailable) { console.log(`runner_unavailable: ${unavailable}; 0 steps executed`); return 3; }
  const fake = await startFakeDaemon();
  const bridgeLog = [];
  const scratch = mkdtempSync(join(tmpdir(), 'rsi-webkit-plan-'));
  let server;
  const runs = [];
  try {
    const bridge = createBridge({ call: (m, p) => rpcCall(fake.socket, m, p), record: (e) => bridgeLog.push(e) });
    const served = await serve({ dist: DIST, bridge, token: newToken(), onRefusal: (e) => bridgeLog.push(e), signal });
    server = served.server;
    // One window at a time: each run is its own harness process (the preference belongs to a webview's configuration).
    for (const mode of MODES) {
      const planPath = join(scratch, `plan-${mode.id}.json`);
      writeFileSync(planPath, JSON.stringify(buildPlan(`http://127.0.0.1:${served.port}/`, mode)));
      // A fresh home per run: nothing carries over between runs and nothing lands in the operator's ~/Library (#263).
      const home = join(scratch, `home-${mode.id}`);
      mkdirSync(home, { recursive: true });
      const run = await runHarness(bin, planPath, { keepKeyboardSetting: opts.keepKeyboardSetting, signal, timeout: RUN_TIMEOUT_MS, home });
      runs.push({ mode, run });
      if (run.code === 3) break;
    }
  } finally {
    server?.closeAllConnections();
    server?.close();
    await fake.stop();
    await rm(scratch, { recursive: true, force: true });
  }
  const noWindow = runs.find((r) => r.run.code === 3);
  if (noWindow) { console.log(noWindow.run.stdout.trim() || 'runner_unavailable: no window server session; 0 steps executed'); return 3; }
  const results = [];
  let webkitVersion = null;
  let fullKeyboardAccess = null;
  for (const { mode, run } of runs) {
    const out = parseHarnessOutput(run.stdout);
    webkitVersion ??= out?.webkitVersion ?? null;
    fullKeyboardAccess ??= out?.fullKeyboardAccess ?? null;
    results.push(...judgeMode(mode, out, run));
  }
  const report = {
    engine: 'WebKit (WKWebView)', webkitVersion, fullKeyboardAccess,
    keyboardNavigation: opts.keepKeyboardSetting ? 'host setting' : 'off (-AppleKeyboardUIMode 0)', mode: 'fake', harness: bin,
    modes: MODES.map(({ id, tabFocusesLinks, title }) => ({ id, tabFocusesLinks, title })), wry: WRY_PREFERENCE,
    elapsedMs: Date.now() - started, results,
    refusals: bridgeLog.filter((e) => e.outcome === 'refused').map((r) => `${r.cmd}: ${r.reason}`),
    fakeDaemonReceived: [...new Set(fake.received.map((r) => r.method))].sort(),
  };
  // The default-mode control fixture is not the app: when WebKit tabs to its plain button with the preference off,
  // Keyboard navigation was on for this run, so that mode cannot show the default order. That is the host, not a
  // regression: runner_unavailable, never a pass. A failure in the shipped (wry) mode still fails the run.
  const failedWry = results.some((r) => r.mode === 'wry' && r.status !== 'pass');
  const keyboardOn = !opts.keepKeyboardSetting && results.find((r) => r.id === `default/${CONTROL_ID}`)?.status === 'fail';
  if (keyboardOn && !failedWry) report.status = 'runner_unavailable';
  writeFileSync(join(opts.out, 'report.json'), `${JSON.stringify(report, null, 2)}\n`);
  if (keyboardOn && !failedWry) {
    console.log(`runner_unavailable: macOS Keyboard navigation stayed on in the harness (the default-mode control button without tabindex took focus), so the default Tab order cannot be shown; report: ${join(opts.out, 'report.json')}`);
    return 3;
  }
  for (const r of results) console.log(`${r.status.toUpperCase().padEnd(5)} ${r.id.padEnd(26)} ${r.note}`);
  console.log(`webkit=${report.webkitVersion} fullKeyboardAccess=${report.fullKeyboardAccess} elapsed=${report.elapsedMs}ms`);
  console.log(`report: ${join(opts.out, 'report.json')}`);
  return results.every((r) => r.status === 'pass') ? 0 : 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((c) => process.exit(c), (e) => { console.error(e); process.exit(2); });
}
