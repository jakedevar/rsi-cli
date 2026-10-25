// The macOS WebKit keyboard smoke (scripts/smoke-webkit.mjs, scripts/webkit-smoke.swift; local #247): the plan's
// JavaScript parses, the judge reads focus sequences right, an unsupported host is runner_unavailable, and on macOS with
// swiftc and a window server one real WKWebView run passes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  CONTROL_ID, MODES, STEPS, WRY_PREFERENCE, buildPlan, findSwiftc, harnessEnv, judge, main, snapshotLeaks, parseArgs, parseHarnessOutput, probeSource, setupSource, stepsFor, unavailableReason,
} from '../scripts/smoke-webkit.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const press = (o) => JSON.stringify({ focus: o.focus ?? 'button', hit: o.hit ?? null, forbidden: o.forbidden ?? null, inScope: o.inScope ?? null, done: !!o.done });
const step = (over = {}) => ({ id: 's', targets: [['A', 'a'], ['B', 'b']], max: 10, ...over });

test('webkit smoke arguments: --out and --keep-keyboard-setting; anything else is refused', () => {
  assert.deepEqual(parseArgs(['--out', '/tmp/x', '--keep-keyboard-setting']), { out: '/tmp/x', keepKeyboardSetting: true });
  assert.equal(parseArgs([]).keepKeyboardSetting, false);
  assert.throws(() => parseArgs(['--chrome', 'x']), /unknown argument --chrome/);
});

test('a host that is not macOS, or has no swiftc, is runner_unavailable with its reason', async () => {
  assert.match(unavailableReason({ platform: 'linux', swiftc: '/usr/bin/swiftc' }), /needs macOS/);
  assert.match(unavailableReason({ platform: 'darwin', swiftc: null }), /no swiftc/);
  assert.equal(unavailableReason({ platform: 'darwin', swiftc: '/usr/bin/swiftc' }), null);
  assert.equal(findSwiftc({ PATH: '/nope' }, () => false), null);
  assert.equal(findSwiftc({ PATH: '/a:/b' }, (p) => p === '/b/swiftc'), '/b/swiftc');
  assert.equal(findSwiftc({ RSI_SMOKE_SWIFTC: '/x/swiftc', PATH: '/b' }, () => true), '/x/swiftc');
  const lines = [];
  const log = console.log;
  console.log = (l) => lines.push(String(l));
  try { assert.equal(await main([], { platform: 'win32', swiftc: null }), 3); } finally { console.log = log; }
  assert.match(lines.join('\n'), /^runner_unavailable: .*win32; 0 steps executed$/);
});

test('every step\'s setup and probe is valid JavaScript, and the plan carries them to the harness', () => {
  for (const mode of MODES) {
    for (const s of stepsFor(mode)) {
      assert.doesNotThrow(() => new Function(`return ${setupSource(s)}`), `${mode.id} ${s.id} setup`);
      assert.doesNotThrow(() => new Function(`return ${probeSource(s)}`), `${mode.id} ${s.id} probe`);
    }
  }
  const plan = buildPlan('http://127.0.0.1:1/');
  assert.equal(plan.url, 'http://127.0.0.1:1/');
  assert.equal(plan.tabFocusesLinks, true, 'the default plan mirrors the shipped app');
  assert.deepEqual(plan.steps.map((s) => s.id), STEPS.map((s) => s.id));
  assert.ok(plan.steps.every((s) => s.max > 0 && s.probe.includes('JSON.stringify')));
  assert.equal(plan.steps.find((s) => s.id === 'page-shift-tab').shift, true);
  assert.equal(STEPS[0].id, CONTROL_ID);
});

test('the smoke runs the shipped wry configuration first and WebKit\'s default as a second, with a control for each', () => {
  assert.deepEqual(MODES.map((m) => [m.id, m.tabFocusesLinks]), [['wry', true], ['default', false]]);
  assert.equal(buildPlan('http://x/', MODES[1]).tabFocusesLinks, false);
  const [wry, off] = MODES.map((m) => stepsFor(m)[0]);
  assert.equal(wry.id, CONTROL_ID);
  assert.equal(off.id, CONTROL_ID);
  // The shipped configuration must reach the plain button; the default must skip it and reach only the stamped one.
  assert.deepEqual(wry.targets.map(([l]) => l), ['plain button', 'stamped button']);
  assert.equal(wry.forbidden, undefined);
  assert.deepEqual(off.targets.map(([l]) => l), ['stamped button']);
  assert.deepEqual(off.forbidden.map(([l]) => l), ['plain button']);
  // Everything after the control is shared, so both configurations walk the same page.
  assert.deepEqual(stepsFor(MODES[0]).slice(1), stepsFor(MODES[1]).slice(1));
});

test('the wry version in src-tauri/Cargo.lock is the one whose WKPreferences setup was read (#257): bump WRY_PREFERENCE only after re-reading it', () => {
  const lock = readFileSync(here('../src-tauri/Cargo.lock'), 'utf8');
  const version = lock.match(/\[\[package\]\]\nname = "wry"\nversion = "([^"]+)"/)?.[1];
  assert.ok(version, 'wry is not in src-tauri/Cargo.lock: the shipped webview changed, so re-derive the configuration the smoke mirrors');
  assert.equal(version, WRY_PREFERENCE.version,
    `wry moved from ${WRY_PREFERENCE.version} to ${version}. scripts/smoke-webkit.mjs mirrors what wry sets on WKPreferences ` +
    `(tabFocusesLinks, ${WRY_PREFERENCE.file}). Re-read that file in ~/.cargo/registry/src/*/wry-${version}/, and the other ` +
    'WKPreferences/WKWebViewConfiguration settings it makes, then update WRY_PREFERENCE and the `wry` mode (scripts/webkit-smoke.swift).');
  // When the crate source is in the cargo registry, the recorded line is still there verbatim.
  const registry = join(homedir(), '.cargo', 'registry', 'src');
  const source = existsSync(registry) ? readdirSync(registry).map((d) => join(registry, d, `wry-${version}`, WRY_PREFERENCE.file)).find(existsSync) : undefined;
  if (source) {
    const text = readFileSync(source, 'utf8').replace(/\s+/g, ' ');
    assert.ok(text.includes(WRY_PREFERENCE.line.replace(/\s+/g, ' ')), `wry ${version} no longer contains: ${WRY_PREFERENCE.line}`);
  }
});

test('the checks cover the #242 and #243 expectations: New session, Send, Stop, the dialogs, the conversation, Settings and Issues', () => {
  const labels = Object.fromEntries(STEPS.map((s) => [s.id, s.targets.map(([l]) => l)]));
  for (const l of ['New session', 'Stop this turn', 'Conversation', 'Send']) assert.ok(labels['page-tab'].includes(l), l);
  assert.deepEqual(labels['confirm-dialog'], ['Cancel', 'Archive']);
  assert.ok(labels['question-dialog'].includes('Submit'));
  assert.deepEqual(labels['launch-dialog'], ['Cancel', 'Launch']);
  assert.deepEqual(labels['settings-tab'], ['Refresh', 'Close']);
  assert.deepEqual(labels['issues-tab'], ['Refresh', 'New Issue', 'Close']);
  assert.ok(STEPS.filter((s) => s.id.endsWith('-dialog')).every((s) => s.scope), 'each dialog step checks the focus stays inside');
});

test('the judge passes targets reached in order and names the press numbers', () => {
  const r = judge(step({ ordered: true }), { presses: [press({}), press({ hit: 'A' }), press({ hit: 'B' })] });
  assert.equal(r.status, 'pass');
  assert.equal(r.note, 'Tab reached A (2), B (3)');
  assert.equal(r.focusOrder.length, 3);
});

test('the judge fails a missed target, a wrong order, a press that left the dialog, a forbidden stop and a setup error', () => {
  assert.match(judge(step(), { presses: [press({ hit: 'A' })] }).note, /never reached B in 1 presses/);
  assert.match(judge(step({ ordered: true }), { presses: [press({ hit: 'B' }), press({ hit: 'A' })] }).note, /not in the order A, B/);
  assert.match(judge(step({ scope: 'dialog' }), { presses: [press({ hit: 'A', inScope: true }), press({ inScope: false, focus: 'button#x' }), press({ hit: 'B' })] }).note,
    /press 2 left dialog: focus on button#x/);
  assert.match(judge(step({ shift: true }), { presses: [press({ forbidden: 'plain button' })] }).note, /Shift-Tab press 1 focused the plain button/);
  assert.match(judge(step(), { setupError: 'boom', presses: [press({})] }).note, /setup failed: boom/);
  assert.match(judge(step(), { presses: [] }).note, /no Tab press was sent/);
  assert.equal(judge(step(), undefined).status, 'fail');
});

test('a press that leaves the page (body, the window wrap) does not count as leaving the dialog', () => {
  const r = judge(step({ scope: 'dialog' }), { presses: [press({ hit: 'A', inScope: true }), press({ focus: 'body' }), press({ hit: 'B', inScope: true })] });
  assert.equal(r.status, 'pass', r.note);
});

test('the harness result line is parsed; anything else is null', () => {
  assert.deepEqual(parseHarnessOutput('noise\nWEBKIT-SMOKE-RESULT {"booted":true,"steps":[]}\n'), { booted: true, steps: [] });
  assert.equal(parseHarnessOutput('runner_unavailable: no window server session'), null);
  assert.equal(parseHarnessOutput('WEBKIT-SMOKE-RESULT {broken'), null);
});

const swiftc = findSwiftc();
const why = unavailableReason({ swiftc }) ?? (!existsSync(here('../dist/index.html')) ? 'no built dist' : null);
test('the WebKit smoke passes in a real WKWebView against the fake daemon (macOS, swiftc and a window server)', { skip: why ? `runner_unavailable: ${why}` : false, timeout: 150000 }, async (t) => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-webkit-out-'));
  const started = Date.now();
  const before = snapshotLeaks();
  const code = await main(['--out', out], { signal: t.signal, swiftc });
  // #263: a run writes nothing under the real ~/Library/{WebKit,Caches}/webkit-smoke*, whether or not an older run left some there.
  assert.deepEqual(snapshotLeaks(), before, 'the WebKit smoke wrote under the real ~/Library/WebKit or ~/Library/Caches (webkit-smoke*)');
  if (Object.keys(before).length === 0) assert.deepEqual(Object.keys(snapshotLeaks()), []);
  if (code === 3) {
    // No window server for this process, or Keyboard navigation could not be turned off: nothing was judged.
    t.skip('runner_unavailable: the harness could not show the default WebKit Tab order on this host (see its output)');
    return;
  }
  const report = JSON.parse(readFileSync(join(out, 'report.json'), 'utf8'));
  assert.equal(code, 0, JSON.stringify(report.results.filter((r) => r.status !== 'pass')));
  assert.deepEqual(report.results.map((r) => r.id), MODES.flatMap((m) => [`${m.id}/boot`, `${m.id}/preference`, ...stepsFor(m).map((s) => `${m.id}/${s.id}`)]));
  assert.ok(report.results.every((r) => r.status === 'pass'), JSON.stringify(report.results));
  assert.equal(report.engine, 'WebKit (WKWebView)');
  assert.ok(Date.now() - started < 120000, `the WebKit smoke took ${Date.now() - started}ms`);
});

test('the harness runs with a temp HOME and a non-persistent data store, so nothing lands in the real ~/Library (#263)', () => {
  const env = harnessEnv('/tmp/h', { HOME: '/Users/real', PATH: '/bin' });
  assert.deepEqual(env, { HOME: '/tmp/h', CFFIXED_USER_HOME: '/tmp/h', PATH: '/bin' });
  assert.equal(harnessEnv('/tmp/h').HOME, '/tmp/h');
  assert.match(readFileSync(here('../scripts/webkit-smoke.swift'), 'utf8'), /config\.websiteDataStore = WKWebsiteDataStore\.nonPersistent\(\)/);
});

test('snapshotLeaks sees a webkit-smoke folder appear, grow or change under Library/WebKit and Library/Caches, and ignores other names', () => {
  const home = mkdtempSync(join(tmpdir(), 'rsi-leak-home-'));
  try {
    mkdirSync(join(home, 'Library', 'WebKit'), { recursive: true });
    mkdirSync(join(home, 'Library', 'Caches', 'other-app'), { recursive: true });
    const empty = snapshotLeaks(home);
    assert.deepEqual(empty, {}, 'nothing named webkit-smoke*: an empty snapshot');
    mkdirSync(join(home, 'Library', 'Caches', 'webkit-smoke', 'WebKit'), { recursive: true });
    const created = snapshotLeaks(home);
    assert.deepEqual(Object.keys(created).sort(), ['Library/Caches/webkit-smoke', 'Library/Caches/webkit-smoke/WebKit']);
    writeFileSync(join(home, 'Library', 'WebKit', 'unrelated'), 'x');
    assert.deepEqual(snapshotLeaks(home), created, 'a sibling that is not webkit-smoke* is not counted');
    writeFileSync(join(home, 'Library', 'Caches', 'webkit-smoke', 'WebKit', 'marker'), 'x');
    assert.notDeepEqual(snapshotLeaks(home), created, 'a new file inside an existing leftover folder is a change');
    // Only what this test created is removed: the folder is its own temp home.
  } finally { rmSync(home, { recursive: true, force: true }); }
});
