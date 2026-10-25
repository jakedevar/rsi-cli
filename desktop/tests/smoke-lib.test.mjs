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
import { connectPage, domSnapshot, findChrome, launchChrome, waitForReady } from '../scripts/smoke-lib.mjs';
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

// Executable shebang fixtures, like the detached-helper regression below.
const fakeChromeSkip = process.platform === 'win32' && 'executable shebang fixture';
function fakeChrome(t, source) {
  const dir = mkdtempSync(join(tmpdir(), 'rsi-launch-chrome-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const bin = join(dir, 'chrome.mjs');
  writeFileSync(bin, `#!${process.execPath}\n${source}\n`);
  chmodSync(bin, 0o755);
  return bin;
}

test('Chrome startup tolerates a DevTools port delayed beyond five seconds', { skip: fakeChromeSkip, timeout: 45000 }, async (t) => {
  const bin = fakeChrome(t, `
    import { mkdirSync, writeFileSync } from 'node:fs';
    import { join } from 'node:path';
    const profile = process.argv.find((a) => a.startsWith('--user-data-dir=')).slice(16);
    setTimeout(() => {
      mkdirSync(profile, { recursive: true });
      writeFileSync(join(profile, 'DevToolsActivePort'), '9222\\n/devtools/browser/delayed\\n');
    }, 5250);
    setInterval(() => {}, 1000);
  `);
  const browser = await launchChrome(bin, { signal: t.signal });
  try { assert.equal(browser.cdpPort, 9222); } finally { await browser.stop(); }
});

test('Chrome timeout with empty stderr reports that the browser is still running', { skip: fakeChromeSkip, timeout: 5000 }, async (t) => {
  const bin = fakeChrome(t, 'setInterval(() => {}, 1000);');
  await assert.rejects(launchChrome(bin, { launchTimeout: 100, signal: t.signal }),
    /DevTools port within 100ms: still running \(pid \d+\); stderr: \(empty\)/);
});

for (const [state, source] of [
  ['exit code 23', 'process.exit(23);'],
  ['signal SIGTERM', "process.kill(process.pid, 'SIGTERM');"],
]) {
  test(`Chrome failure with empty stderr reports ${state}`, { skip: fakeChromeSkip, timeout: 15000 }, async (t) => {
    const bin = fakeChrome(t, source);
    await assert.rejects(launchChrome(bin, { launchTimeout: 10000, signal: t.signal }), (error) => {
      assert.match(error.message, /Chrome exited before opening DevTools/);
      assert.ok(error.message.includes(`${state}; stderr: (empty)`), error.message);
      return true;
    });
  });
}

function cdpStub(t, { opening = true, enable = true } = {}) {
  const socket = {
    closed: false,
    sent: [],
    send(data) {
      const { id, method, params } = JSON.parse(data);
      socket.sent.push({ method, params });
      if (enable && (method.endsWith('.enable') || method === 'Emulation.setEmulatedMedia')) queueMicrotask(() => socket.onmessage({ data: JSON.stringify({ id, result: {} }) }));
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

test('every page starts as a dark system, so the checklist sees the dark default (local #274)', { timeout: 3000 }, async (t) => {
  const socket = cdpStub(t);
  const page = await connectPage(1);
  try {
    const media = socket.sent.find((m) => m.method === 'Emulation.setEmulatedMedia');
    assert.deepEqual(media?.params, { features: [{ name: 'prefers-color-scheme', value: 'dark' }] });
  } finally { page.close(); }
});

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

test('CDP loss attributes every pending and subsequent command without reconnecting', { timeout: 3000 }, async (t) => {
  const socket = cdpStub(t);
  const page = await connectPage(1, { browser: { diagnostics: () => 'browser pid=123, stopRequested=false; signal SIGKILL' } });
  try {
    const matches = (method) => (error) => {
      assert.match(error.message, /code=1006, reason=transport gone, wasClean=false/);
      assert.match(error.message, /browser pid=123.*signal SIGKILL/);
      assert.ok(error.message.includes(method), error.message);
      return true;
    };
    const evaluate = assert.rejects(page.eval('42'), matches('Runtime.evaluate'));
    const screenshot = assert.rejects(page.shot('/unused.png'), matches('Page.captureScreenshot'));
    socket.onclose({ code: 1006, reason: 'transport gone', wasClean: false });
    await Promise.all([evaluate, screenshot]);
    await assert.rejects(page.eval('43'), matches('Runtime.evaluate'));
    await assert.rejects(page.waitFor('true'), matches('CDP socket closed'));
    assert.equal(page.closed, true);
    assert.equal(socket.closed, true);
    assert.equal(socket.sent.filter((m) => m.method === 'Runtime.evaluate').length, 1, 'no command replay after transport loss');
  } finally { page.close(); }
});

test('CDP socket errors preserve the underlying error and browser evidence', { timeout: 3000 }, async (t) => {
  const socket = cdpStub(t);
  const page = await connectPage(1, { browser: { diagnostics: () => 'still running (pid 123)' } });
  const cause = new Error('bad frame');
  const rejected = assert.rejects(page.eval('42'), (error) => {
    assert.match(error.message, /CDP socket failed: bad frame.*still running \(pid 123\).*Runtime.evaluate/);
    assert.equal(error.cause.cause, cause);
    return true;
  });
  socket.onerror({ error: cause });
  await rejected;
  assert.equal(page.closed, true);
});

test('CDP retains failed resource URLs without request headers or bodies', async (t) => {
  const socket = cdpStub(t);
  const page = await connectPage(1);
  const event = (method, params) => socket.onmessage({ data: JSON.stringify({ method, params }) });
  try {
    event('Network.requestWillBeSent', { requestId: 'module', request: {
      url: 'http://fake/boot.js', headers: { 'x-smoke-token': 'synthetic-secret' }, postData: 'synthetic body',
    } });
    event('Network.loadingFailed', { requestId: 'module', type: 'Script', errorText: 'net::ERR_ABORTED', canceled: true });
    assert.deepEqual(page.events.filter((e) => e.method.startsWith('Network.')), [{
      method: 'Network.loadingFailed', params: { url: 'http://fake/boot.js', type: 'Script', errorText: 'net::ERR_ABORTED', canceled: true },
    }]);
    event('Network.requestWillBeSent', { requestId: 'done', request: { url: 'http://fake/done.js' } });
    event('Network.loadingFinished', { requestId: 'done' });
    event('Network.loadingFailed', { requestId: 'done', errorText: 'late event' });
    assert.equal(page.events.at(-1).params.url, undefined, 'finished requests release their URL');
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
test('a real browser exit rejects in-flight CDP work with lifetime evidence and no retry', { skip: !chrome && 'runner_unavailable: no Chrome', timeout: 30000 }, async (t) => {
  const browser = await launchChrome(chrome, { signal: t.signal });
  let page;
  try {
    page = await connectPage(browser.cdpPort, { signal: t.signal, browser });
    const pending = assert.rejects(page.eval('new Promise(() => {})'), (error) => {
      assert.match(error.message, /CDP socket (closed|failed)/);
      assert.ok(error.message.includes(`browser pid=${browser.pid}`), error.message);
      assert.match(error.message, /stopRequested=false/);
      assert.match(error.message, /Runtime\.(evaluate|awaitPromise)/);
      return true;
    });
    // Only our launched PID: simulate unexpected browser death while a command is pending.
    process.kill(browser.pid, 'SIGTERM');
    await pending;
    await assert.rejects(page.eval('42'), /CDP socket (closed|failed)/);
    assert.equal(page.closed, true);
  } finally { page?.close(); await browser.stop(); }
});

test('CDP interactions wait for delayed, visible, enabled controls and diagnose missing targets', { skip: !chrome && 'runner_unavailable: no Chrome', timeout: 60000 }, async (t) => {
  let browser, page;
  try {
    browser = await launchChrome(chrome, { signal: t.signal });
    page = await connectPage(browser.cdpPort, { signal: t.signal });
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

test('Chrome launches with a long job TMPDIR and leaves the parent environment intact', { skip: !chrome && 'runner_unavailable: no Chrome', timeout: 60000 }, async () => {
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
    `], { env: { ...process.env, TMPDIR: long }, timeout: 45000 });
    assert.match(result.stdout, /long TMPDIR browser passed/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

// #138: Chrome's browser process exits at once on SIGTERM, but on macOS a helper outside its process group keeps the
// inherited stderr pipe open for tens of seconds. stop() used to wait for the pipe ('close'), so a smoke run hung
// past its test timeout whenever the host was busy. A fake browser reproduces that: it detaches a straggler that holds
// stderr and outlives it.
test('stop() returns once the browser exits, even when a detached helper keeps its stderr open', { skip: process.platform === 'win32' && 'no process groups', timeout: 45000 }, async () => {
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

// Local #285: a flaky step that passes on re-run must leave something to read.
test('domSnapshot returns the page state and the last uncaught page errors, and never throws', async () => {
  const page = {
    events: [{ method: 'Log.entryAdded', params: {} }, { method: 'Runtime.exceptionThrown', params: { exceptionDetails: { exception: { description: 'Error: boom\n at x' } } } }],
    async eval(expression, options) {
      assert.match(expression, /show-finished/);
      assert.ok(options.timeout <= 3000, 'the snapshot is bounded');
      return { url: 'http://127.0.0.1/', rows: [], showFinished: { checked: true, stored: null } };
    },
  };
  const snap = await domSnapshot(page);
  assert.deepEqual(snap.showFinished, { checked: true, stored: null });
  assert.deepEqual(snap.pageErrors, ['Error: boom']);
  const broken = await domSnapshot({ events: [], async eval() { throw new Error('socket closed'); } });
  assert.equal(broken.unavailable, 'socket closed');
});

test('the show-finished smoke step waits on conditions: no fixed sleep sits in it', () => {
  const src = readFileSync(new URL('../scripts/smoke.mjs', import.meta.url), 'utf8');
  const start = src.indexOf("id: 'show-finished'");
  const body = src.slice(start, src.indexOf("id: 'filter-prompt'", start));
  assert.ok(body.length > 500, 'the step was found');
  assert.doesNotMatch(body, /\bsleep\(/);
});

test('waitForReady gives up at once on a stop cause and says what it was', async () => {
  let polls = 0;
  const page = { async eval() { polls++; return { ready: false, excerpt: 'not yet' }; } };
  let cause;
  const started = Date.now();
  setTimeout(() => { cause = 'Chrome cancelled the load of app.js'; }, 150);
  await assert.rejects(waitForReady(page, 'x', { step: 'boot', timeout: 30000, stop: () => cause }),
    /boot: stopped after \d+ms: Chrome cancelled the load of app\.js; state: not yet/);
  assert.ok(Date.now() - started < 5000 && polls > 0);
});
