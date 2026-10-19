// Real-key evidence for the text fields' Vim editing (hub #3106, local #116): the scripts/smoke-vim.mjs suite in
// headless Chrome against the fake daemon. Skipped with a reason when Chrome or desktop/dist is missing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SCENARIOS, VIM_FIXTURES, parseVimArgs, runVimSuite } from '../scripts/smoke-vim.mjs';
import { READ_METHODS } from '../scripts/smoke-bridge.mjs';
import { findChrome } from '../scripts/smoke-lib.mjs';

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
const why = !chrome ? 'runner_unavailable: no Chrome' : !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no dist (npm run build)' : false;
test('every vim-field scenario passes through real key events in headless Chrome', { skip: why, timeout: 240000 }, async (t) => {
  const report = await runVimSuite({ chromeBin: chrome, out: mkdtempSync(join(tmpdir(), 'rsi-vim-out-')), signal: t.signal });
  const failed = report.results.filter((r) => r.status !== 'pass');
  assert.deepEqual(failed.map((r) => `${r.id}: ${r.note}`), []);
  assert.equal(report.results.length, SCENARIOS.length);
});
