#!/usr/bin/env node
// Receipt for the real-rsid graph-run E2E (hub #3279, #3000 criterion 4).
//
//   node scripts/graph-run-receipt.mjs --report DIR/e2e-report.json --out DIR/graph-run-receipt.json
//        --source-sha SHA --command "..." --exit-code N [--rsid PATH]
//
// A green npm/Cargo aggregate does not prove the opt-in real-rsid test ran (it is skipped without
// RSI_E2E_RSID). This judge accepts ONLY an e2e-report.json that EXECUTED the `graph-run` scenario against an
// isolated real rsid and in which graph-seeded-daemon, graph-seeded-render and graph-seeded-readback all
// passed. A missing report, a missing section, a missing step, a skipped step or any non-`pass` status is
// `executed: false` / `ok: false`, never a pass. Exit: 0 receipt ok, 1 evidence rejected.
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';

export const SCENARIO = 'graph-run';
export const REQUIRED_STEPS = ['graph-seeded-daemon', 'graph-seeded-render', 'graph-seeded-readback'];

/** Judge a parsed e2e-report.json. Pure: returns { ok, executed, steps, reasons }. */
export function judgeGraphRunReport(report) {
  const reasons = [];
  const results = Array.isArray(report?.results) ? report.results : [];
  const section = results.find((r) => r?.id === SCENARIO);
  if (!section) {
    return { ok: false, executed: false, steps: {}, reasons: [`the report has no ${SCENARIO} section (the scenario did not execute)`] };
  }
  const bySteps = Array.isArray(section.steps) ? section.steps : [];
  const steps = {};
  for (const id of REQUIRED_STEPS) {
    const found = bySteps.find((s) => s?.id === id);
    steps[id] = found ? String(found.status) : 'missing';
    if (!found) reasons.push(`${id}: step missing from the report`);
    else if (found.status !== 'pass') reasons.push(`${id}: ${found.status}${found.note ? ` (${String(found.note).slice(0, 200)})` : ''}`);
  }
  if (!section.daemon?.sha256) reasons.push('the section records no daemon sha256 (not a real-rsid run)');
  if (!section.runner?.sha) reasons.push('the section records no runner/source sha');
  const executed = REQUIRED_STEPS.every((id) => steps[id] !== 'missing');
  return { ok: reasons.length === 0, executed, steps, reasons };
}

/** Build the retained receipt from the judgement plus the job's facts. */
export function buildReceipt({ report, judgement, sourceSha, command, exitCode, rsidPath }) {
  const section = (report?.results ?? []).find((r) => r?.id === SCENARIO) ?? {};
  return {
    schema: 'desktop-graph-run-e2e-receipt/v1',
    issue: 3279,
    scenario: SCENARIO,
    ok: judgement.ok && exitCode === 0,
    executed: judgement.executed,
    skipped: !judgement.executed,
    source_sha: sourceSha,
    daemon: { path: rsidPath ?? null, sha256: section.daemon?.sha256 ?? null, pid: section.daemon?.pid ?? null },
    command,
    exit_code: exitCode,
    required_steps: judgement.steps,
    reasons: judgement.reasons,
  };
}

export const sha256File = (path) => createHash('sha256').update(readFileSync(path)).digest('hex');

function parse(argv) {
  const o = {};
  for (let i = 0; i < argv.length; i += 2) {
    const k = argv[i];
    if (!k.startsWith('--') || argv[i + 1] === undefined) throw new Error(`bad argument ${k}`);
    o[k.slice(2)] = argv[i + 1];
  }
  for (const k of ['report', 'out', 'source-sha', 'command', 'exit-code']) if (o[k] === undefined) throw new Error(`missing --${k}`);
  return o;
}

export function main(argv) {
  const o = parse(argv);
  let report = null;
  try { report = JSON.parse(readFileSync(o.report, 'utf8')); } catch (e) { report = { error: String(e.message) }; }
  const judgement = judgeGraphRunReport(report);
  if (report?.error) judgement.reasons.unshift(`e2e-report.json unreadable: ${report.error}`);
  const receipt = buildReceipt({ report, judgement, sourceSha: o['source-sha'], command: o.command, exitCode: Number(o['exit-code']), rsidPath: o.rsid });
  if (receipt.exit_code !== 0 && !receipt.reasons.length) receipt.reasons.push(`the smoke command exited ${receipt.exit_code}`);
  writeFileSync(o.out, `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(`graph-run receipt: ${receipt.ok ? 'OK' : 'REJECTED'} executed=${receipt.executed} exit=${receipt.exit_code} steps=${JSON.stringify(receipt.required_steps)}`);
  for (const r of receipt.reasons) console.log(`  - ${r}`);
  console.log(`receipt: ${o.out}`);
  return receipt.ok ? 0 : 1;
}

if (process.argv[1] && import.meta.url === new URL(`file://${process.argv[1]}`).href) {
  try { process.exit(main(process.argv.slice(2))); } catch (e) { console.error(e.message); process.exit(2); }
}
