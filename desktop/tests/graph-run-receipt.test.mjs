// Hub #3279: the graph-run receipt separates an executed, passing real-rsid run from a skipped or partial one.
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { buildReceipt, judgeGraphRunReport, main, REQUIRED_STEPS } from '../scripts/graph-run-receipt.mjs';

const step = (id, status = 'pass') => ({ id, title: id, status, note: `${id} ${status}` });
const report = (stepsOverride, extra = {}) => ({
  results: [{
    id: 'graph-run', status: 'pass', runner: { sha: 'a'.repeat(40) }, daemon: { sha256: 'b'.repeat(64), pid: 7 },
    steps: stepsOverride ?? [step('graph-execution'), ...REQUIRED_STEPS.map((id) => step(id))], ...extra,
  }],
});

test('all three seeded steps passing is an executed, ok judgement', () => {
  const j = judgeGraphRunReport(report());
  assert.equal(j.ok, true);
  assert.equal(j.executed, true);
  assert.deepEqual(j.steps, Object.fromEntries(REQUIRED_STEPS.map((id) => [id, 'pass'])));
});

test('a report without the graph-run section is not executed and not ok (a skipped run)', () => {
  const j = judgeGraphRunReport({ results: [{ id: 'command', steps: [] }] });
  assert.equal(j.ok, false);
  assert.equal(j.executed, false);
  assert.equal(judgeGraphRunReport(null).ok, false);
});

test('a failing, skipped or missing required step rejects the run', () => {
  for (const status of ['fail', 'skipped', 'substituted']) {
    const j = judgeGraphRunReport(report([step('graph-seeded-daemon'), step('graph-seeded-render', status), step('graph-seeded-readback')]));
    assert.equal(j.ok, false, status);
    assert.equal(j.executed, true, status);
  }
  const missing = judgeGraphRunReport(report([step('graph-seeded-daemon'), step('graph-seeded-render')]));
  assert.equal(missing.ok, false);
  assert.equal(missing.executed, false);
  assert.equal(missing.steps['graph-seeded-readback'], 'missing');
});

test('a section without a daemon sha256 is not a real-rsid run', () => {
  const j = judgeGraphRunReport(report(undefined, { daemon: {} }));
  assert.equal(j.ok, false);
});

test('the receipt is ok only with exit code 0 and carries source, daemon sha, command and exit code', () => {
  const r = report();
  const receipt = buildReceipt({ report: r, judgement: judgeGraphRunReport(r), sourceSha: 'c'.repeat(40), command: 'node x', exitCode: 0, rsidPath: '/t/rsid' });
  assert.equal(receipt.ok, true);
  assert.equal(receipt.skipped, false);
  assert.equal(receipt.source_sha, 'c'.repeat(40));
  assert.equal(receipt.daemon.sha256, 'b'.repeat(64));
  assert.equal(receipt.exit_code, 0);
  assert.equal(receipt.command, 'node x');
  assert.equal(buildReceipt({ report: r, judgement: judgeGraphRunReport(r), sourceSha: 'c', command: 'x', exitCode: 1 }).ok, false);
});

test('the CLI writes the receipt: exit 0 for a pass, 1 for a missing report', () => {
  const dir = mkdtempSync(join(tmpdir(), 'graph-receipt-test-'));
  const base = ['--out', join(dir, 'r.json'), '--source-sha', 'd'.repeat(40), '--command', 'cmd', '--exit-code', '0'];
  writeFileSync(join(dir, 'e2e-report.json'), JSON.stringify(report()));
  assert.equal(main(['--report', join(dir, 'e2e-report.json'), ...base]), 0);
  assert.equal(JSON.parse(readFileSync(join(dir, 'r.json'), 'utf8')).ok, true);
  assert.equal(main(['--report', join(dir, 'absent.json'), ...base]), 1);
  const rejected = JSON.parse(readFileSync(join(dir, 'r.json'), 'utf8'));
  assert.equal(rejected.ok, false);
  assert.equal(rejected.skipped, true);
});
