// A browser startup/CDP failure must leave no server, socket or child keeping a
// test file alive. Exercise failures in subprocesses so the event loop is tested.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { connectPage, findChrome, launchChrome } from '../scripts/smoke-lib.mjs';
import { scenarios } from '../scripts/smoke-firstrun.mjs';

const exec = promisify(execFile);
const script = (name) => new URL(`../scripts/${name}.mjs`, import.meta.url).href;
const dist = fileURLToPath(new URL('../dist/', import.meta.url));

for (const suite of ['smoke', 'smoke-vim', 'smoke-firstrun']) {
  test(`${suite} exits promptly and cleans up when Chrome fails to launch`, { timeout: 10000 }, async () => {
    const out = mkdtempSync(join(tmpdir(), 'rsi-launch-failure-'));
    // Node rejects Chrome's flags and exits immediately, on every platform.
    const call = suite === 'smoke'
      ? `m.main(['--out', ${JSON.stringify(out)}, '--chrome', process.execPath])`
      : suite === 'smoke-vim'
        ? `m.runVimSuite({ out: ${JSON.stringify(out)}, chromeBin: process.execPath })`
        : `m.runFirstRun({ out: ${JSON.stringify(out)}, chromeBin: process.execPath, dist: ${JSON.stringify(dist)} })`;
    try {
      const result = await exec(process.execPath, ['--input-type=module', '-e', `
        import assert from 'node:assert/strict';
        const m = await import(${JSON.stringify(script(suite))});
        await assert.rejects(${call}, /Chrome exited before opening DevTools/);
        console.log('launch failed and cleanup completed');
      `], { timeout: 5000 });
      assert.match(result.stdout, /launch failed and cleanup completed/);
    } finally { rmSync(out, { recursive: true, force: true }); }
  });
}

test('a missing Chrome executable fails promptly with its spawn error', { timeout: 3000 }, async () => {
  await assert.rejects(launchChrome('/no-such-rsi-chrome'), /ENOENT/);
});

function cdpStub(t, { opening = true, enable = true } = {}) {
  const socket = {
    closed: false,
    send(data) {
      const { id, method } = JSON.parse(data);
      if (enable && method.endsWith('.enable')) queueMicrotask(() => socket.onmessage({ data: JSON.stringify({ id, result: {} }) }));
    },
    close() { socket.closed = true; },
  };
  t.mock.method(globalThis, 'fetch', async () => ({ json: async () => [{ type: 'page', webSocketDebuggerUrl: 'ws://fake' }] }));
  t.mock.method(globalThis, 'WebSocket', function () {
    if (opening) queueMicrotask(() => socket.onopen());
    return socket;
  });
  return socket;
}

test('CDP opening and initialization timeouts close the socket', { timeout: 3000 }, async (t) => {
  for (const options of [{ opening: false }, { enable: false }]) {
    const socket = cdpStub(t, options);
    await assert.rejects(connectPage(1, { commandTimeout: 30 }), /timed out/);
    assert.equal(socket.closed, true);
    t.mock.restoreAll();
  }
});

test('a lost CDP connection rejects pending commands and closes the socket', { timeout: 3000 }, async (t) => {
  const socket = cdpStub(t);
  const page = await connectPage(1);
  try {
    const command = page.eval('42');
    const rejected = assert.rejects(command, /CDP socket closed/);
    socket.onclose();
    await rejected;
    assert.equal(socket.closed, true);
  } finally { page.close(); }
});

test('test cancellation closes CDP and rejects a pending command', { timeout: 3000 }, async (t) => {
  const socket = cdpStub(t);
  const abort = new AbortController();
  const page = await connectPage(1, { signal: abort.signal });
  try {
    const rejected = assert.rejects(page.eval('42'), /test expired/);
    abort.abort(new Error('test expired'));
    await rejected;
    assert.equal(socket.closed, true);
  } finally { page.close(); }
});

test('a stalled CDP evaluation respects the readiness deadline', { timeout: 3000 }, async (t) => {
  cdpStub(t);
  const page = await connectPage(1);
  try {
    await assert.rejects(page.element('#launch', undefined, { step: 'stalled launch', timeout: 50 }), /stalled launch: timed out after 50ms.*#launch.*CDP command timed out/);
  } finally { page.close(); }
});

const chrome = findChrome();
test('CDP interactions wait for delayed, visible, enabled controls and diagnose missing targets', { skip: !chrome && 'runner_unavailable: no Chrome', timeout: 15000 }, async () => {
  let browser, page;
  try {
    browser = await launchChrome(chrome);
    page = await connectPage(browser.cdpPort);
    const url = `data:text/html,${encodeURIComponent('<p>Navigation test</p>')}`;
    await page.goto(url);
    const firstDocument = await page.eval('performance.timeOrigin');
    await page.goto(url);
    assert.ok(await page.eval(`performance.timeOrigin > ${firstDocument}`), 'same-URL navigation reaches a new document');
    await page.eval(`(() => {
      document.documentElement.dataset.boot = 'offline';
      document.body.innerHTML = '<p>Loading test state</p>';
      window.clicked = [];
      setTimeout(() => {
        const b = document.createElement('button');
        b.id = 'delayed'; b.hidden = true; b.disabled = true; b.textContent = 'Ready';
        b.onclick = () => window.clicked.push({ hidden: b.hidden, disabled: b.disabled });
        document.body.append(b);
        setTimeout(() => { b.hidden = false; }, 100);
        setTimeout(() => { b.disabled = false; }, 250);
      }, 100);
    })()`);
    await page.click(`document.querySelector('#delayed')`, { step: 'delayed launch' });
    assert.deepEqual(await page.eval('window.clicked'), [{ hidden: false, disabled: false }]);
    assert.equal(await page.element('#delayed', '(e) => e.textContent'), 'Ready');
    await assert.rejects(page.element('#missing', undefined, { step: 'missing launch', timeout: 150 }), (error) => {
      assert.match(error.message, /missing launch: timed out after 150ms/);
      assert.match(error.message, /#missing/);
      assert.match(error.message, /Loading test state/);
      assert.match(error.message, /offline/);
      return true;
    });
    await page.eval(`document.querySelector('#delayed').setAttribute('aria-disabled', 'true')`);
    await assert.rejects(page.click(`document.querySelector('#delayed')`, { step: 'disabled launch', timeout: 150 }), /disabled launch: timed out.*aria-disabled/);
    assert.equal(await page.eval('window.clicked.length'), 1);
    // Reproduce the original race deterministically: boot has completed, but
    // the New session action and then its dialog render on later turns.
    await page.eval(`(() => {
      document.documentElement.dataset.boot = 'ready';
      document.body.innerHTML = '<div id="empty"></div><dialog id="launch-dialog">Launch</dialog>';
      setTimeout(() => {
        const b = document.createElement('button');
        b.dataset.action = 'launch'; b.textContent = 'New session';
        b.onclick = () => setTimeout(() => document.querySelector('#launch-dialog').showModal(), 150);
        document.querySelector('#empty').append(b);
      }, 150);
    })()`);
    const checks = await scenarios().find((s) => s.id === 'connected-empty').after(page);
    assert.deepEqual(checks, [[true, 'the New session button opens the launch dialog']]);
  } finally { page?.close(); await browser?.stop(); }
});

test('Chrome launches with a long job TMPDIR and leaves the parent environment intact', { skip: !chrome && 'runner_unavailable: no Chrome', timeout: 15000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), 'rsi-long-tmp-'));
  // Longer than a Unix socket address, just like ~/.rsi/jobs/<uuid>.tmp
  // plus Chrome's scoped directory and SingletonSocket in the crashing jobs.
  const long = join(root, 'job-temporary-directory-'.repeat(5));
  try {
    const result = await exec(process.execPath, ['--input-type=module', '-e', `
      import assert from 'node:assert/strict';
      import { mkdirSync } from 'node:fs';
      import { connectPage, launchChrome } from ${JSON.stringify(script('smoke-lib'))};
      mkdirSync(process.env.TMPDIR, { recursive: true });
      const parentTmp = process.env.TMPDIR;
      let browser, page;
      try {
        browser = await launchChrome(${JSON.stringify(chrome)});
        page = await connectPage(browser.cdpPort);
        assert.equal(await page.eval('6 * 7'), 42);
        assert.equal(process.env.TMPDIR, parentTmp);
        console.log('long TMPDIR browser passed');
      } finally { page?.close(); await browser?.stop(); }
    `], { env: { ...process.env, TMPDIR: long }, timeout: 10000 });
    assert.match(result.stdout, /long TMPDIR browser passed/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

// #138: Chrome's browser process exits at once on SIGTERM, but on macOS a helper outside its process group keeps the
// inherited stderr pipe open for tens of seconds. stop() used to wait for the pipe ('close'), so a smoke run hung
// past its test timeout whenever the host was busy. A fake browser reproduces that: it detaches a straggler that holds
// stderr and outlives it.
test('stop() returns once the browser exits, even when a detached helper keeps its stderr open', { skip: process.platform === 'win32' && 'no process groups', timeout: 15000 }, async () => {
  const dir = mkdtempSync(join(tmpdir(), 'rsi-fake-chrome-'));
  const bin = join(dir, 'fake-chrome.mjs');
  const pidFile = join(dir, 'straggler.pid');
  writeFileSync(bin, `#!${process.execPath}
import { spawn } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const profile = process.argv.find((a) => a.startsWith('--user-data-dir=')).slice(16);
mkdirSync(profile, { recursive: true });
const helper = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 20000)'], { detached: true, stdio: ['ignore', 'ignore', 'inherit'] });
writeFileSync(${JSON.stringify(pidFile)}, String(helper.pid));
helper.unref();
writeFileSync(join(profile, 'DevToolsActivePort'), '9\\n/devtools/browser/x\\n');
setInterval(() => {}, 1000);
`);
  chmodSync(bin, 0o755);
  let helperPid;
  try {
    const chrome = await launchChrome(bin);
    helperPid = Number(readFileSync(pidFile, 'utf8'));
    const started = Date.now();
    await chrome.stop();
    assert.ok(Date.now() - started < 5000, `stop() took ${Date.now() - started}ms`);
  } finally {
    if (helperPid) try { process.kill(helperPid, 'SIGKILL'); } catch {}
    rmSync(dir, { recursive: true, force: true });
  }
});
