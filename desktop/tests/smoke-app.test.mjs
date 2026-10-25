// The packaged-app acceptance check (scripts/smoke-app.mjs, scripts/app-driver.m; local #259): arguments, availability
// (anything but a macOS GUI session with clang is runner_unavailable, never green), the copy's environment and cleanup
// stay on the test bundle id, the driver refuses any other bundle and compiles. The real run builds and drives the app
// (minutes from cold), so it is on demand (`npm run smoke:app`); here it runs only when RSI_SMOKE_APP names a built app.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  APP_WALK_STEPS, TEST_BUNDLE_ID, appEnv, builtAppFrom, hardenedRuntime, libraryLeftovers, main, parseArgs, unavailableReason, walkSteps,
} from '../scripts/smoke-app.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const DRIVER = here('../scripts/app-driver.m');

test('smoke:app arguments: --app (or RSI_SMOKE_APP), --out and --keep-run; anything else is refused', () => {
  assert.deepEqual(parseArgs([], {}), { app: null, out: '/tmp/desktop-smoke-app', keepRun: false });
  assert.deepEqual(parseArgs(['--app', '/x/RSI Desktop.app', '--out', '/tmp/o', '--keep-run'], {}), { app: '/x/RSI Desktop.app', out: '/tmp/o', keepRun: true });
  assert.equal(parseArgs([], { RSI_SMOKE_APP: '/y.app' }).app, '/y.app');
  assert.throws(() => parseArgs(['--chrome'], {}), /unknown argument --chrome/);
  assert.throws(() => parseArgs(['--app'], {}), /--app needs a value/);
});

test('not macOS, no clang or no GUI session is runner_unavailable with its reason, and main says so with 0 checks', async () => {
  assert.match(unavailableReason({ platform: 'linux', clang: '/c', gui: true }), /macOS app; this host is linux/);
  assert.match(unavailableReason({ platform: 'darwin', clang: null, gui: true }), /no clang/);
  assert.match(unavailableReason({ platform: 'darwin', clang: '/c', gui: false }), /no GUI \(Aqua\) session/);
  assert.equal(unavailableReason({ platform: 'darwin', clang: '/c', gui: true }), null);
  const lines = [];
  const log = console.log;
  console.log = (l) => lines.push(String(l));
  try {
    assert.equal(await main([], { platform: 'win32' }), 3);
    assert.equal(await main([], { platform: 'darwin', clang: '/c', gui: false }), 3);
  } finally { console.log = log; }
  assert.match(lines[0], /^runner_unavailable: .*win32; 0 checks executed$/);
  assert.match(lines[1], /^runner_unavailable: no GUI .*; 0 checks executed$/);
});

test('the copy gets the test HOME, the fake socket and the driver; no RSI_* or DYLD_* of the caller, and the caller keeps none', () => {
  const base = { PATH: '/bin', USER: 'u', RSI_SESSION_TOKEN: 'secret', RSI_DAEMON_SOCKET: '/live.sock', DYLD_LIBRARY_PATH: '/x', HOME: '/Users/u' };
  const env = appEnv({ base, home: '/tmp/r/home', socket: '/tmp/r/d.sock', ctl: '/tmp/r/ctl', dylib: '/tmp/d.dylib' });
  assert.deepEqual(env, {
    PATH: '/bin', USER: 'u', HOME: '/tmp/r/home', CFFIXED_USER_HOME: '/tmp/r/home', SHELL: '/bin/zsh',
    RSI_DAEMON_SOCKET: '/tmp/r/d.sock', RSI_APP_DRIVER_CTL: '/tmp/r/ctl', DYLD_INSERT_LIBRARIES: '/tmp/d.dylib',
  });
  assert.equal(base.DYLD_INSERT_LIBRARIES, undefined);
  assert.equal(process.env.DYLD_INSERT_LIBRARIES, undefined);
});

test('cleanup names only the test bundle id\'s ~/Library folders and refuses the operator\'s bundle id', () => {
  const paths = libraryLeftovers('/Users/u');
  assert.ok(paths.length >= 4);
  for (const p of paths) {
    assert.ok(p.startsWith('/Users/u/Library/'), p);
    assert.ok(p.split('/').pop().startsWith(TEST_BUNDLE_ID), p);
  }
  assert.ok(paths.includes(`/Users/u/Library/WebKit/${TEST_BUNDLE_ID}`));
  assert.throws(() => libraryLeftovers('/Users/u', 'dev.rsi.desktop'), /refusing to clean dev\.rsi\.desktop/);
  assert.throws(() => libraryLeftovers('/Users/u', `${TEST_BUNDLE_ID}/../x`), /refusing/);
});

test('the build\'s app path and the hardened-runtime check read the tools\' real output', () => {
  assert.equal(builtAppFrom('…\nbuilt: /t/release/rsi-desktop\nbuilt: /t/release/bundle/macos/RSI Desktop.app\n'), '/t/release/bundle/macos/RSI Desktop.app');
  assert.equal(builtAppFrom('error: cargo is not installed\n'), null);
  assert.equal(hardenedRuntime('CodeDirectory v=20400 size=1 flags=0x20002(adhoc,linker-signed) hashes=1'), false);
  assert.equal(hardenedRuntime('CodeDirectory v=20500 size=1 flags=0x10000(runtime) hashes=1'), true);
  assert.equal(hardenedRuntime('code object is not signed at all'), false);
});

test('the real-app walk reuses #247\'s WebKit steps (Tab to New session, Stop, Send; Shift-Tab back; dialogs keep Tab)', () => {
  const steps = walkSteps();
  assert.deepEqual(steps.map((s) => s?.id), APP_WALK_STEPS);
  const page = steps.find((s) => s.id === 'page-tab');
  assert.deepEqual(page.targets.map(([l]) => l).filter((l) => ['New session', 'Stop this turn', 'Send'].includes(l)), ['New session', 'Stop this turn', 'Send']);
});

test('the driver acts only in a dev.rsi.desktop.smoke-app copy with its control directory, and clears DYLD_INSERT_LIBRARIES first', () => {
  const src = readFileSync(DRIVER, 'utf8');
  assert.match(src, new RegExp(`kBundlePrefix = @"${TEST_BUNDLE_ID.replaceAll('.', '\\.')}"`));
  const ctor = src.slice(src.indexOf('__attribute__((constructor))'));
  assert.ok(ctor.indexOf('unsetenv("DYLD_INSERT_LIBRARIES")') < ctor.indexOf('hasPrefix:kBundlePrefix'), 'unsetenv before any return');
  assert.match(ctor, /if \(!c \|\| !\[bundle hasPrefix:kBundlePrefix\]\)/);
  assert.match(ctor, /AppleKeyboardUIMode": @0 \} forName:NSArgumentDomain/);
});

test('the driver compiles with the Xcode command line tools (macOS only)', (t) => {
  if (process.platform !== 'darwin') return t.skip(`runner_unavailable: ${process.platform} has no Cocoa`);
  if (spawnSync('xcrun', ['--find', 'clang'], { encoding: 'utf8' }).status !== 0) return t.skip('runner_unavailable: no xcrun clang');
  const dir = mkdtempSync(join(tmpdir(), 'rsi-app-driver-test-'));
  try {
    const r = spawnSync('xcrun', ['clang', '-dynamiclib', '-fobjc-arc', '-Wall', '-Werror', '-framework', 'Cocoa', '-framework', 'WebKit',
      '-framework', 'ApplicationServices', '-o', join(dir, 'd.dylib'), DRIVER], { encoding: 'utf8', timeout: 60_000 });
    assert.equal(r.status, 0, r.stderr);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('the real packaged app passes every check (on demand: RSI_SMOKE_APP names a built "RSI Desktop.app")', { timeout: 240_000 }, async (t) => {
  const app = process.env.RSI_SMOKE_APP;
  if (!app) return t.skip('runner_unavailable: RSI_SMOKE_APP unset; run `npm run smoke:app` (builds the app) or set it to a built bundle');
  const out = mkdtempSync(join(tmpdir(), 'rsi-smoke-app-test-'));
  try {
    const code = await main(['--app', app, '--out', out]);
    if (code === 3) return t.skip('runner_unavailable: see the printed reason');
    const report = JSON.parse(readFileSync(join(out, 'report.json'), 'utf8'));
    assert.equal(code, 0, report.results.filter((r) => r.status !== 'pass').map((r) => `${r.id}: ${r.note}`).join('\n'));
    assert.equal(report.cleanup.appAlive, false);
  } finally { rmSync(out, { recursive: true, force: true }); }
});
