// Reproduce #492's modes -> counts transition five times under the desktop-vim-repeat job's CPU quota.
import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { findChrome } from './smoke-lib.mjs';
import { runVimSuite } from './smoke-vim.mjs';

const chromeBin = findChrome();
if (!chromeBin) throw new Error('runner_unavailable: no Chrome');
const signal = AbortSignal.timeout(240000);
for (let iteration = 1; iteration <= 5; iteration++) {
  const started = Date.now();
  const report = await runVimSuite({ chromeBin, signal, only: ['modes', 'counts'],
    out: mkdtempSync(join(tmpdir(), 'rsi-vim-repeat-')) });
  console.log(JSON.stringify({ iteration, elapsed_ms: Date.now() - started, results: report.results }));
  assert.deepEqual(report.results.map((r) => [r.id, r.status]), [['modes', 'pass'], ['counts', 'pass']]);
}
console.log('PASS: five modes -> counts runs through real key events');
