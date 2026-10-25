// Desktop #324 (audit 4 finding 7, hub #3270): the manager grant editor marks each launch the global manager's grant does
// not cover, as the TUI's grant page does ("✗ not granted by global manager" plus the ":manager portfolio launch add" fix).
// The rule is a port of rsi-common's grant_narrowing (portfolio_effective_launches / intersect_launches); the cases below
// mirror its unit test. The editor runs against a fake document and a canned rsid (the Tauri bridge's invoke), no browser.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { body, doc } from './fake-doc.mjs';
import {
  NOT_GRANTED_HINT, NOT_GRANTED_MARK, ancestorLaunches, intersectLaunches, launchText, portfolioEffectiveLaunches, uncoveredLaunchTexts,
} from '../dist/managers-model.js';
import { grantEditor, launchCoverage } from '../dist/manager-launch.js';

const P = '3f1c2a10-0000-4000-8000-000000000001';
const launch = (model, effort = 'high') => ({ provider: 'Codex', model, effort });
const A = launch('a');
const B = launch('b');
const anyA = { ...A, effort: null };
const node = (over = {}) => ({ state: 'active', grant: { project_ids: [P], allowed_launches: [A], project_policy: { allowed_launches: [] }, ...over } });

test('intersection keeps the narrower entry in both directions, like launch_intersection_keeps_exact_effort_in_both_directions', () => {
  assert.deepEqual(intersectLaunches([anyA], [A]), [A]);
  assert.deepEqual(intersectLaunches([A], [anyA]), [A]);
  assert.deepEqual(intersectLaunches([anyA], [anyA]), [anyA]);
  assert.deepEqual(intersectLaunches([A], [{ ...A, effort: 'low' }]), []);
  assert.deepEqual(intersectLaunches([anyA], [B]), []);
});

test('effective launches narrow by the project policy list and fall back to the grant when disjoint', () => {
  assert.deepEqual(portfolioEffectiveLaunches([anyA], { allowed_launches: [A] }), [A]);
  assert.deepEqual(portfolioEffectiveLaunches([A], { allowed_launches: [anyA] }), [A]);
  assert.deepEqual(portfolioEffectiveLaunches([anyA], { allowed_launches: [] }), [anyA]);
  assert.deepEqual(portfolioEffectiveLaunches([anyA], { allowed_launches: [B] }), [anyA], 'a disjoint policy list keeps what the operator granted');
  assert.deepEqual(portfolioEffectiveLaunches([anyA], null), [anyA]);
});

test('ancestor launches: live nodes covering the project, intersected; none when nothing covers it', () => {
  assert.deepEqual(ancestorLaunches([node()], P), [A]);
  assert.equal(ancestorLaunches([node({ project_ids: ['other'] })], P), null);
  assert.equal(ancestorLaunches([{ ...node(), state: 'revoked' }], P), null);
  assert.deepEqual(ancestorLaunches([node({ allowed_launches: [A, B] }), node({ allowed_launches: [B] })], P), [B]);
});

test('ancestors [A] over a catalog [A, B] mark only B (a_launch_the_ancestor_grant_does_not_cover_carries_the_hint)', () => {
  assert.deepEqual([...uncoveredLaunchTexts([A, B], [A])], [launchText(B)]);
  assert.deepEqual([...uncoveredLaunchTexts([A, B], null)], [], 'no ancestor grant: no mark');
  assert.deepEqual([...uncoveredLaunchTexts([A, B], [anyA, { ...B, effort: null }])], [], 'a grant that allows both marks nothing');
});

// ---------- the editor on a fake document, against a canned rsid ----------

let nodes = [];
let refuse = false;
globalThis.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      if (cmd === 'portfolio_nodes_list') { if (refuse) throw new Error('daemon error -32000: draining'); return { nodes }; }
      if (cmd === 'mgr_policy_check') return args.policy;
      throw new Error(`unexpected ${cmd}`);
    },
  },
};

const openForm = () => body.walk().filter((n) => n.tag === 'dialog' && n.open).at(-1);
/** The launch rows (the form's other checklist is the capabilities). */
const labels = (dlg) => dlg.querySelectorAll('label').filter((l) => l.textContent.trim().startsWith('Codex/'));
const policy = { mode: 'status', capabilities: [], allowed_launches: [A, B], max_created_containers: 0, max_created_sessions: 0, max_active_sessions: 4, max_spend_usd: null };

async function openEditor(ancestors) {
  const done = grantEditor('Policy of Fake’s manager', policy, null, [A, B], ancestors);
  await new Promise((r) => setImmediate(r));
  return { done, dlg: openForm() };
}
const text = (l) => l.textContent;
const markOf = (l) => l.querySelector('small.mgr-mark');

test('an uncovered launch shows the mark and the fix; a covered one reads as before', async () => {
  const { done, dlg } = await openEditor([A]);
  const [rowA, rowB] = labels(dlg);
  assert.equal(text(rowA).trim(), launchText(A), 'a covered launch has no mark text');
  assert.equal(markOf(rowA), null);
  assert.ok(text(rowB).includes(launchText(B)) && text(rowB).includes(NOT_GRANTED_MARK), text(rowB));
  assert.equal(markOf(rowB).hidden, false, 'B is checked, so its mark shows');
  const note = dlg.querySelector('.mgr-mark-note');
  assert.equal(note.textContent, NOT_GRANTED_HINT);
  assert.match(note.textContent, /:manager portfolio launch add/);
  assert.equal(note.hidden, false);
  dlg.querySelector('form').fire('submit');
  assert.ok(await done);
});

test('unchecking the uncovered launch hides its mark and the fix; checking it brings them back', async () => {
  const { done, dlg } = await openEditor([A]);
  const rowB = labels(dlg)[1];
  const box = rowB.querySelector('input');
  box.checked = false;
  box.fire('change');
  assert.equal(markOf(rowB).hidden, true);
  assert.equal(dlg.querySelector('.mgr-mark-note').hidden, true);
  box.checked = true;
  box.fire('change');
  assert.equal(markOf(rowB).hidden, false);
  assert.equal(dlg.querySelector('.mgr-mark-note').hidden, false);
  dlg.querySelector('form').fire('submit');
  await done;
});

test('with no covering grant nothing is marked and there is no extra paragraph', async () => {
  const { done, dlg } = await openEditor(null);
  assert.equal(dlg.querySelectorAll('small.mgr-mark').length, 0);
  assert.equal(dlg.querySelector('.mgr-mark-note'), null);
  assert.deepEqual(labels(dlg).map((l) => text(l).trim()), [launchText(A), launchText(B)], 'the rows read exactly as before');
  dlg.querySelector('form').fire('submit');
  await done;
});

test('launchCoverage reads the live nodes from rsid; a refusal leaves the editor unmarked', async () => {
  nodes = [node()];
  assert.deepEqual(await launchCoverage(P), [A]);
  nodes = [];
  assert.equal(await launchCoverage(P), null);
  nodes = [node()];
  refuse = true;
  assert.equal(await launchCoverage(P), null);
  refuse = false;
});

test('the project manager editor passes the coverage in; the global grant editor does not', async () => {
  const { readFileSync } = await import('node:fs');
  const src = readFileSync(new URL('../src/manager-launch.ts', import.meta.url), 'utf8');
  assert.match(src, /grantEditor\(`Policy of \$\{role\.name\}’s manager`[^\n]*await launchCoverage\(role\.projectId\)\)/);
  assert.match(src, /grantEditor\('Global grant'[^\n]*catalog\);/, 'the global seat is the top of the tree: no ancestor to mark against');
});
