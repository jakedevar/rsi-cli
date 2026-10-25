// Real-key evidence for the text fields' Vim editing (hub #3106, local #116): the scripts/smoke-vim.mjs suite in
// headless Chrome against the fake daemon. Skipped with a reason when Chrome or desktop/dist is missing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { BOOT_ATTEMPTS, SCENARIOS, VIM_FIXTURES, freshVimPage, parseVimArgs, runVimSuite } from '../scripts/smoke-vim.mjs';
import { READ_METHODS } from '../scripts/smoke-bridge.mjs';
import { connectPage, findChrome, launchChrome } from '../scripts/smoke-lib.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

test('the suite names its scenarios once and parses its arguments', () => {
  const ids = SCENARIOS.map((s) => s.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const want of ['modes', 'counts', 'text-objects', 'one-line-field', 'colon-viewer', 'vim-off', 'secret-field']) assert.ok(ids.includes(want), want);
  assert.deepEqual(parseVimArgs([]), { out: '/tmp/desktop-vim', chrome: null });
  assert.deepEqual(parseVimArgs(['--out', '/x', '--chrome', '/c']), { out: '/x', chrome: '/c' });
  assert.throws(() => parseVimArgs(['--live']), /unknown argument/);
});

test('every fixture the suite adds is a read the bridge allows', () => {
  for (const method of Object.keys(VIM_FIXTURES)) assert.ok(READ_METHODS.has(method), `${method} is not on the read allowlist`);
});

const chrome = findChrome();
test('Vim startup waits for boot and a painted session row, and reports page errors and DOM on failure', {
  skip: !chrome && 'runner_unavailable: no Chrome', timeout: 60000,
}, async (t) => {
  let browser, page;
  const url = (body) => `data:text/html,${encodeURIComponent(body)}`;
  try {
    browser = await launchChrome(chrome, { signal: t.signal });
    page = await connectPage(browser.cdpPort, { signal: t.signal });
    await freshVimPage(page, [], { url: url(`<ul id="session-list"><li>Loading</li></ul><script>
      setTimeout(() => { document.documentElement.dataset.boot = 'ready'; }, 100);
      setTimeout(() => {
        document.querySelector('li').outerHTML = '<li class="session-item" data-id="fixture">Fixture</li>';
      }, 250);
    </script>`) });
    assert.equal(await page.eval(`document.querySelector('#session-list .session-item').dataset.id`), 'fixture');
    // A row alone is not readiness: boot can still be reading settings/live.
    await freshVimPage(page, [], { url: url(`<ul id="session-list"><li class="session-item" data-id="fixture">Fixture</li></ul><script>
      setTimeout(() => { document.documentElement.dataset.boot = 'ready'; window.finishedBoot = true; }, 150);
    </script>`) });
    assert.equal(await page.eval('window.finishedBoot'), true);
    for (const state of ['failed', 'offline', null]) {
      await assert.rejects(freshVimPage(page, [], { timeout: 150, url: url(`<div id="conn">bridge test state</div><script>
        console.error('synthetic startup error');
        ${state ? `document.documentElement.dataset.boot = ${JSON.stringify(state)};` : ''}
        document.documentElement.dataset.bootReport = 'synthetic boot report';
      </script>`) }), (error) => {
        assert.match(error.message, state ? new RegExp(`Vim app boot ${state}`) : /Vim app boot: timed out/);
        const diagnostics = JSON.parse(error.message.split('; startup diagnostics: ')[1]);
        assert.equal(diagnostics.dom.boot, state);
        assert.equal(diagnostics.dom.bootReport, 'synthetic boot report');
        assert.equal(diagnostics.dom.bridgeInstalled, false);
        assert.match(diagnostics.dom.html, /bridge test state/);
        assert.ok(diagnostics.console.some((e) => e.params.args?.some((a) => a.value === 'synthetic startup error')));
        assert.deepEqual(diagnostics.bridge, []);
        return true;
      });
    }
  } finally { page?.close(); await browser?.stop(); }
});

test('Vim startup diagnostics retain current bridge calls even if DOM capture fails', async () => {
  const bridgeLog = [{ method: 'previous scenario', outcome: 'error' }];
  const page = {
    events: [{ method: 'Network.loadingFailed', params: { url: 'http://fake/previous.js' } }], origin: 'http://fake',
    async goto() {
      bridgeLog.push({ method: 'ListSessions', outcome: 'started' });
      this.events.push({ method: 'Network.loadingFailed', params: { url: 'http://fake/boot.js', errorText: 'net::ERR_ABORTED' } });
      this.events.push({ method: 'Log.entryAdded', params: { entry: { level: 'error', text: 'module load failed' } } });
    },
    async ready() { throw new Error('stalled boot'); },
    async eval() { throw new Error('lost context'); },
  };
  await assert.rejects(freshVimPage(page, bridgeLog), (error) => {
    const diagnostics = JSON.parse(error.message.split('; startup diagnostics: ')[1]);
    assert.equal(diagnostics.dom.captureError, 'lost context');
    assert.deepEqual(diagnostics.bridge, [{ method: 'ListSessions', outcome: 'started' }]);
    assert.deepEqual(diagnostics.network, [{ method: 'Network.loadingFailed', params: { url: 'http://fake/boot.js', errorText: 'net::ERR_ABORTED' } }]);
    assert.equal(diagnostics.console[0].params.entry.text, 'module load failed');
    return true;
  });
});

// A scripted page: each goto() plays the next attempt's DevTools events, then boot answers or fails (no Chrome).
function scriptedPage(attempts) {
  let n = 0;
  return {
    events: [], origin: 'http://fake', navigations: 0,
    async goto() {
      this.navigations++;
      const a = attempts[Math.min(n++, attempts.length - 1)];
      for (const url of a.canceled ?? []) this.events.push({ method: 'Network.loadingFailed', params: { url, type: 'Fetch', errorText: 'net::ERR_ABORTED', canceled: true } });
      for (const params of a.failed ?? []) this.events.push({ method: 'Network.loadingFailed', params });
      this.attempt = a;
    },
    async ready(expression, options) {
      const stopped = options.stop?.();
      if (stopped) throw new Error(`Vim app boot: stopped: ${stopped}`);
      if (this.attempt.boot === 'timeout') throw new Error('Vim app boot: timed out');
      return { state: this.attempt.boot, report: 'report' };
    },
    async element() { return true; },
    async eval() { return { invokeRetries: [] }; },
  };
}

test('Vim startup repeats a boot whose loads Chrome cancelled, and names every attempt when it never boots', async () => {
  const cancelled = { boot: 'failed', canceled: ['http://fake/__invoke'] };
  const page = scriptedPage([cancelled, { boot: 'timeout', canceled: ['http://fake/todo-model.js'] }, { boot: 'ready' }]);
  await freshVimPage(page, []);
  assert.equal(page.navigations, 3, 'a cancelled fetch and a cancelled module each cost one more navigation');
  const stuck = scriptedPage([cancelled]);
  await assert.rejects(freshVimPage(stuck, []), (error) => {
    assert.equal(stuck.navigations, BOOT_ATTEMPTS);
    const diagnostics = JSON.parse(error.message.split('; startup diagnostics: ')[1]);
    assert.equal(diagnostics.attempts.length, BOOT_ATTEMPTS);
    assert.deepEqual(diagnostics.attempts[0].canceled, ['__invoke']);
    assert.match(diagnostics.attempts[0].error, /Vim app boot failed/);
    return true;
  });
});

test('Vim startup never repeats a boot without an interrupted load', async () => {
  const page = scriptedPage([{ boot: 'failed' }, { boot: 'ready' }]);
  await assert.rejects(freshVimPage(page, []), /Vim app boot failed/);
  assert.equal(page.navigations, 1, 'an app fault is reported, not retried away');
});

test('Vim startup reloads modules interrupted by a network change without the canceled bit', async () => {
  const interrupted = { boot: 'timeout', failed: [{ url: 'http://fake/main.js?v=1', type: 'Script', errorText: 'net::ERR_NETWORK_CHANGED', canceled: false }] };
  const page = scriptedPage([interrupted, { boot: 'ready' }]);
  await freshVimPage(page, []);
  assert.equal(page.navigations, 2);
  const stuck = scriptedPage([interrupted]);
  await assert.rejects(freshVimPage(stuck, []), (error) => {
    const diagnostics = JSON.parse(error.message.split('; startup diagnostics: ')[1]);
    assert.equal(stuck.navigations, BOOT_ATTEMPTS);
    assert.equal(diagnostics.attempts.length, BOOT_ATTEMPTS);
    for (const attempt of diagnostics.attempts) {
      assert.match(attempt.error, /stopped: Chrome interrupted.*net::ERR_NETWORK_CHANGED/);
      assert.deepEqual(attempt.network, interrupted.failed);
    }
    return true;
  });
});

test('Vim startup reports permanent module failures and ignores interruptions from previous boots', async () => {
  for (const page of [
    scriptedPage([{ boot: 'timeout', failed: [{ url: 'http://fake/main.js', type: 'Script', errorText: 'net::ERR_CONNECTION_REFUSED' }] }]),
    scriptedPage([{ boot: 'failed' }]),
  ]) {
    page.events.push({ method: 'Network.loadingFailed', params: { url: 'http://fake/previous.js', errorText: 'net::ERR_NETWORK_CHANGED' } });
    await assert.rejects(freshVimPage(page, []), /Vim app boot/);
    assert.equal(page.navigations, 1);
  }
});

test('a cancelled module load ends the boot wait at once', async () => {
  const page = scriptedPage([{ boot: 'timeout', canceled: ['http://fake/dashboard-model.js'] }, { boot: 'ready' }]);
  await freshVimPage(page, [], { timeout: 60000 });
  assert.equal(page.navigations, 2);
});

const why = !chrome ? 'runner_unavailable: no Chrome' : !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no dist (npm run build)' : false;
test('Vim startup is isolated across repeated scenarios', { skip: why, timeout: 90000 }, async (t) => {
  const scenarios = Array.from({ length: 30 }, (_, i) => ({ id: `startup-${i}`, title: 'isolated app boot',
    async run(h) {
      const start = Date.now();
      await h.fresh();
      // Each scenario starts with the app's defaults, even after its predecessor
      // changes persistent UI state and leaves a dialog open.
      assert.equal(await h.page.eval(`localStorage.getItem('rsi.desktop.vimKeys')`), null);
      assert.equal(await h.page.eval(`document.getElementById('launch-dialog').open`), false);
      await h.page.eval(`localStorage.setItem('rsi.desktop.vimKeys', 'off'); document.getElementById('launch-dialog').showModal()`);
      return { status: 'pass', note: `isolated boot in ${Date.now() - start}ms` };
    },
  }));
  const report = await runVimSuite({ chromeBin: chrome, signal: t.signal, scenarios,
    out: mkdtempSync(join(tmpdir(), 'rsi-vim-startup-')) });
  assert.deepEqual(report.results.filter((r) => r.status !== 'pass').map((r) => `${r.id}: ${r.note}`), []);
  assert.equal(report.results.length, scenarios.length);
  t.diagnostic(report.results.map((r) => r.note).join(', '));
});

test('every vim-field scenario passes through real key events in headless Chrome', { skip: why, timeout: 240000 }, async (t) => {
  const report = await runVimSuite({ chromeBin: chrome, out: mkdtempSync(join(tmpdir(), 'rsi-vim-out-')), signal: t.signal });
  const failed = report.results.filter((r) => r.status !== 'pass');
  assert.deepEqual(failed.map((r) => `${r.id}: ${r.note}`), []);
  assert.equal(report.results.length, SCENARIOS.length);
});
