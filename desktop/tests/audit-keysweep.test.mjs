// The overlay key sweep, the key fixtures and the all-themes run (local #327): the pure parts run anywhere; the browser tests run the
// sweep against the built app and the fake daemon (skipped without Chrome or dist).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { findChrome } from '../scripts/smoke-lib.mjs';
import { FIXTURES, fixtureResponse } from '../scripts/smoke-fake.mjs';
import * as lib from '../scripts/audit-lib.mjs';
import { parseArgs, parseThemeKeys, reloadPage, themeRunsFor, themeRunsFromApp } from '../scripts/audit.mjs';
import { KEY_LOCAL, dagFixtures, keyFixtures } from '../scripts/audit-fixtures.mjs';
import { SKIPPED_OVERLAYS, SWEEP_OVERLAYS, changedFields, coverage, failures, judgePress, parseKeySpec, plan, runKeySweep, snapshotFor, summaryLine, sweepOverlay, wasTyped } from '../scripts/audit-keysweep.mjs';
import { WALK_STUBS } from '../scripts/audit-listbox-walk.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const chrome = findChrome();
const SKIP = !chrome || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false;
const labels = (spec) => parseKeySpec(spec).presses.map((p) => p.label);

// ---------- key specs ----------

test('parseKeySpec: alternatives, chords, named keys and modifiers become presses', () => {
  assert.deepEqual(labels('j / k, Down / Up'), ['j', 'k', 'Down', 'Up']);
  assert.deepEqual(parseKeySpec('j / k, Down / Up').presses[2].steps, [{ key: 'ArrowDown' }]);
  assert.deepEqual(parseKeySpec('Ctrl-D / Ctrl-U, PageDown / PageUp').presses.map((p) => p.steps[0]), [
    { key: 'd', ctrl: true }, { key: 'u', ctrl: true }, { key: 'PageDown' }, { key: 'PageUp' },
  ]);
  assert.deepEqual(parseKeySpec('Shift-Tab').presses[0].steps, [{ key: 'Tab', shift: true }]);
  assert.deepEqual(parseKeySpec('g / G').presses.map((p) => p.steps[0]), [{ key: 'g' }, { key: 'G', shift: true }]);
  assert.deepEqual(parseKeySpec('Esc (normal mode), Ctrl-Q').presses.map((p) => p.label), ['Esc', 'Ctrl-Q']);
  assert.deepEqual(parseKeySpec('Ctrl-Alt-G').presses[0].steps, [{ key: 'g', ctrl: true, alt: true }]);
  assert.deepEqual(parseKeySpec('dd').presses[0].steps, [{ key: 'd' }, { key: 'd' }], 'dd is a chord');
  assert.deepEqual(parseKeySpec('Space e').presses[0].steps, [{ key: ' ' }, { key: 'e' }], 'Space e is a chord');
  assert.deepEqual(labels('h j k l, w b'), ['h', 'j', 'k', 'l', 'w b'], 'four keys, then a two-key chord');
  assert.deepEqual(parseKeySpec('1-9').presses[0].steps, [{ key: '1' }], 'a digit range presses its first digit');
  assert.deepEqual(parseKeySpec('').presses, [], 'a blank cell has nothing to press');
  assert.deepEqual(labels(':'), [':'], 'the colon key is a key, not a description');
  assert.deepEqual(parseKeySpec('?').presses[0].steps, [{ key: '?' }]);
});

test('parseKeySpec: typed text, descriptions and the global leader are skipped with a reason, not pressed', () => {
  const t = parseKeySpec('Type, Backspace');
  assert.deepEqual(t.presses.map((p) => p.label), ['Backspace']);
  assert.deepEqual(t.skipped.map((s) => s.label), ['Type']);
  for (const spec of ['Other keys', 'Ctrl+Arrows', 'no overlay open', 'Grant page: j / k, h / l, Space, digits']) assert.deepEqual(parseKeySpec(spec).presses, [], spec);
  const leader = parseKeySpec('Space N / Space ;');
  assert.deepEqual(leader.presses, []);
  assert.equal(leader.skipped.length, 2);
  assert.match(leader.skipped[0].why, /leader/);
});

test('plan: every listed key once, jump-to-first after the keys that move off it, closing keys last, expected no-ops attached', () => {
  const tables = [{ name: 'T', rows: [
    { keys: 'Esc / q', action: 'Close', detail: '' }, { keys: 'g / G', action: 'First / last', detail: '' },
    { keys: 'j / k', action: 'Move', detail: '' }, { keys: 'Type, Backspace', action: 'Filter', detail: '' }, { keys: 'Space N', action: 'Leader', detail: '' },
  ] }];
  const p = plan('T', tables, { 'Type, Backspace': 'the filter is empty' });
  assert.deepEqual(p.presses.map((x) => x.label), ['G', 'j', 'k', 'Backspace', 'g', 'Esc', 'q']);
  assert.equal(p.presses.find((x) => x.label === 'g').toFirst, true);
  assert.equal(p.presses.find((x) => x.label === 'Backspace').noop, 'the filter is empty');
  assert.equal(p.presses.find((x) => x.label === 'j').noop, undefined);
  assert.deepEqual(p.skipped.map((s) => s.label), ['Type', 'Space N']);
  assert.throws(() => plan('Nope', tables), /no overlay key table named Nope/);
});

test('judgePress: a snapshot difference is a change, none is a FAIL unless the key is a listed expected no-op', () => {
  const snap = { dialogs: 'd', text: 'a', active: 'x', selected: 's', values: '', scroll: 0, toast: '', rows: 3, page: '', writes: 0 };
  assert.deepEqual(changedFields(snap, { ...snap, selected: 't' }), ['selected']);
  assert.deepEqual(judgePress(snap, { ...snap, writes: 1 }), { status: 'changed', changed: ['writes'] });
  assert.deepEqual(judgePress(snap, { ...snap, dialogs: '' }).changed, ['dialogs'], 'closing the overlay is a change');
  assert.deepEqual(judgePress(snap, { ...snap }), { status: 'no-change', changed: [] });
  assert.deepEqual(judgePress(snap, { ...snap }, 'r refreshes'), { status: 'expected-no-op', changed: [], why: 'r refreshes' });
  assert.deepEqual(judgePress(snap, { ...snap }, null, '#321'), { status: 'known-gap', changed: [], why: '#321' }, 'a gap that is filed is not a FAIL');
  assert.deepEqual(judgePress(snap, { ...snap, toast: 'x' }, null, '#321'), { status: 'changed', changed: ['toast'], fixed: '#321' }, 'a known gap that starts to work says so');
  assert.equal(judgePress(snap, { ...snap, toast: 'x' }, 'r refreshes').status, 'changed', 'an expected no-op that does change is just a change');
});

test('summaryLine and failures name each key that changed nothing and each overlay that did not open', () => {
  const ok = { overlay: 'A', opened: true, helpLists: true, rows: [{ status: 'changed' }, { status: 'expected-no-op' }] };
  const bad = { overlay: 'B', opened: true, helpLists: false, rows: [{ status: 'no-change', label: 'g', spec: 'g / G', action: 'First' }] };
  const shut = { overlay: 'C', opened: false, why: 'no palette row', rows: [] };
  assert.equal(summaryLine(ok), 'A: 2 keys, 1 changed, 1 expected no-op, 0 FAIL');
  assert.match(summaryLine({ ...ok, rows: [{ status: 'typed' }, { status: 'changed' }] }), /^A: 2 keys, 1 changed, 0 expected no-op, 1 only typed into a text box \(not verified\), 0 FAIL/);
  assert.match(summaryLine(bad), /1 FAIL \(the \? help does not list this overlay\)\n {4}FAIL g \(g \/ G: First\) changed nothing/);
  assert.match(summaryLine(shut), /C: not opened \(no palette row\)/);
  assert.deepEqual(failures([ok, bad, shut]), ['B: g', 'C: not opened']);
  const known = { overlay: 'D', opened: true, helpLists: true, rows: [{ status: 'known-gap', label: 'Home', why: '#321' }, { status: 'changed', label: 'End', fixed: '#321' }] };
  assert.match(summaryLine(known), /^D: 2 keys, 1 changed, 0 expected no-op, 1 known gap \(#321\), 0 FAIL\n {4}FIXED End: it changes something now, so drop it from `known` \(#321\)/);
  assert.deepEqual(failures([known]), [], 'a filed gap does not fail the run');
});

test('the swept overlays name key tables and palette actions the built app has, and every config names keys the table lists', async () => {
  const { OVERLAY_TABLES } = await import('../dist/overlay-help.generated.js');
  const { ACTIONS } = await import('../dist/registry.js');
  for (const [name, cfg] of Object.entries(SWEEP_OVERLAYS)) {
    assert.ok(OVERLAY_TABLES.some((t) => t.name === name), `${name} is a key table of the app`);
    assert.ok(cfg.action || cfg.open, `${name} says how it is opened`);
    if (cfg.action) assert.ok(ACTIONS.some((a) => a.id === cfg.action), `${name}: ${cfg.action} is a registered action`);
    if (cfg.pane) assert.ok(cfg.sel, `${name}: a pane that is not a dialog names the selector of its root`);
    const modes = cfg.modes ?? [];
    const p = plan(name, OVERLAY_TABLES, cfg.noop ?? {}, modes, cfg.remap ?? {}, cfg.known ?? {});
    assert.ok(p.presses.length >= 2, `${name} has keys to press`);
    const listed = (key) => p.presses.some((x) => x.spec === key || x.label === key);
    for (const spec of Object.keys(cfg.noop ?? {})) assert.ok(listed(spec), `${name}: the expected no-op ${spec} is a key it lists`);
    for (const spec of Object.keys(cfg.remap ?? {})) assert.ok(listed(spec), `${name}: the remapped key ${spec} is a key it lists`);
    for (const [spec, issue] of Object.entries(cfg.known ?? {})) { assert.ok(listed(spec), `${name}: the known gap ${spec} is a key it lists`); assert.match(issue, /^#\d+$/, 'a known gap names its Issue'); }
    for (const m of modes) for (const spec of m.specs) assert.ok(listed(spec), `${name}: the mode key ${spec} is a key it lists`);
  }
});

test('every key table of the app is swept or skipped with a reason, and nothing is both (a table added later must be classified)', async () => {
  const { OVERLAY_TABLES } = await import('../dist/overlay-help.generated.js');
  const c = coverage(OVERLAY_TABLES);
  assert.deepEqual(c.missing, [], `these tables are neither in SWEEP_OVERLAYS nor in SKIPPED_OVERLAYS: ${c.missing.join(', ')}`);
  assert.deepEqual(c.both, []);
  assert.deepEqual(c.stale, [], 'a swept or skipped name that is not a table of the app');
  assert.equal(c.swept.length + c.skipped.length, OVERLAY_TABLES.length);
  for (const [name, why] of Object.entries(SKIPPED_OVERLAYS)) assert.ok(why.length > 20, `${name} says why it is skipped`);
  assert.ok(c.swept.length >= 60, 'the sweep covers the dialogs, the panes, the forms and the typed-filter overlays');
  assert.deepEqual(coverage([{ name: 'New TUI Overlay' }]).missing, ['New TUI Overlay'], 'an unclassified table is reported');
});

test('plan: modes, remaps and the step-off-the-first-row pre-step', () => {
  const tables = [{ name: 'T', rows: [
    { keys: 'Up / Down', action: 'Move', detail: '' }, { keys: 'Backspace', action: 'Delete', detail: '' }, { keys: 'Ctrl-A', action: 'AI', detail: '' },
    { keys: 'Ctrl-U', action: 'Page', detail: '' }, { keys: 'h / l', action: 'Option', detail: '' },
  ] }];
  const p = plan('T', tables, {}, [{ text: 'ab', specs: ['Backspace'] }, { specs: ['h'], enter: [{ key: 'l' }] }], { 'Ctrl-A': 'Ctrl-Alt-A' });
  const by = Object.fromEntries(p.presses.map((x) => [x.label, x]));
  assert.equal(by.Up.offFirst, true, 'a key that moves back is pressed after stepping off the first row');
  assert.equal(by.Down.offFirst, undefined);
  assert.equal(by['Ctrl-U'].offFirst, true);
  assert.equal(by.Backspace.mode, 0);
  assert.equal(by.h.mode, 1);
  assert.equal(by.l.mode, undefined);
  assert.deepEqual(by['Ctrl-A'].steps, [{ key: 'a', ctrl: true, alt: true }], 'pressed as the desktop binds it');
  assert.equal(by['Ctrl-A'].remappedTo, 'Ctrl-Alt-A');
  assert.equal(by['Ctrl-A'].label, 'Ctrl-A', 'it keeps the table\'s name for the key');
});

test('wasTyped: a plain character with a text box focused is typed text, a chord or a named key is not', () => {
  const press = (key, extra = {}) => ({ steps: [{ key, ...extra }] });
  assert.equal(wasTyped({ typing: true }, press('n'), ['values', 'caret']), true);
  assert.equal(wasTyped({ typing: true }, press('i'), ['text']), false, 'a Vim box that only switched mode changed its indicator, not its value');
  assert.equal(wasTyped({ typing: true }, press('l'), ['caret']), false, 'a caret move is the key working');
  assert.equal(wasTyped({ typing: true }, press('n'), ['values', 'selected']), false, 'it also changed something that is not the field');
  assert.equal(wasTyped({ typing: false }, press('n'), ['values']), false);
  assert.equal(wasTyped({ typing: true }, press('p', { ctrl: true }), ['values']), false);
  assert.equal(wasTyped({ typing: true }, press('Enter'), ['values']), false);
  assert.equal(wasTyped({ typing: true }, { steps: [{ key: ']' }, { key: 'a' }] }, ['values']), false, 'a chord');
});

test('snapshotFor reads a pane that is not a dialog, and says when it is gone', () => {
  assert.match(snapshotFor('#dag-view'), /document\.querySelector\("#dag-view"\)/);
  assert.match(snapshotFor('#dag-view'), /present: "#dag-view" \? !!pane\?\.checkVisibility\(\)/);
  assert.match(snapshotFor(null), /present: null \? [^:]+: true/);
});

// ---------- the fixtures ----------

test('keyFixtures gives each keyboard list the issue names two or more rows, and leaves the rest of the fake daemon alone', async () => {
  const fx = keyFixtures(FIXTURES);
  const ask = async (method, params = {}) => (await fixtureResponse(fx[method], { id: 1, method, params })).result;
  assert.equal((await ask('ListDeletedSessions')).length, 3, 'Trash');
  assert.equal((await ask('ListScheduledJobs')).length, 3, 'Scheduled jobs');
  assert.equal((await ask('ListLabels')).length, 3, 'labels');
  assert.equal((await ask('ListHarnessManagerScope')).rows.length, 3, 'Manager scope');
  assert.equal((await ask('GetHarnessManagerState', { query: { section: 'decisions' } })).rows.length, 3, 'Manager decisions');
  assert.equal((await ask('GetHarnessManagerState', { query: { section: 'health' } })).rows.length, 1, 'the other sections are the base ones');
  assert.equal((await ask('ListSourceWorktreeCohorts')).length, 3, 'Settlement cohorts');
  assert.equal((await ask('ListSourceWorktreeSettlementRuns')).length, 3, 'Settlement runs');
  assert.equal((await ask('ListSatellitePeers')).peers.length, 3, 'Satellite registry');
  const remote = await ask('RemoteGetStatus');
  assert.deepEqual([remote.peers.length, remote.projects.length], [3, 3], 'Remote access');
  assert.equal((await ask('ListProjectDirectory', {})).entries.length, 3, 'the file explorer');
  assert.equal((await ask('ListProjectFiles', {})).paths.length, 4, 'Find file');
  assert.equal((await ask('OperatorTodoList')).open.length, 3, 'the to-do panel');
  const completed = (await ask('ListSessions')).filter((s) => s.status === 'Completed');
  assert.ok(completed.length >= 3, 'Recent completions');
  assert.equal(new Set((await ask('ListSessions')).map((s) => s.id)).size, (await ask('ListSessions')).length, 'session ids are unique');
  assert.equal(fx.GetFleetOverview, FIXTURES.GetFleetOverview, 'a fixture it does not change is the base one');
  assert.equal((await ask('ListWorkflows')).length, 3, 'the saved-workflow picker');
  const doc = (await ask('GetWorkflowDefinition')).document;
  assert.deepEqual(doc.definition.nodes.map((n) => n.type), ['action', 'topology', 'action'], 'a fold key needs a topology node');
  assert.equal(doc.definition.edges.filter((e) => e.source === 'n1').length, 2, 'the plan branches, so j / k have a neighbour');
  const file = await ask('ReadProjectFile', { path: 'notes.txt' });
  assert.ok(file.content.split('\n').length > 100 && file.truncated === false, 'a file longer than the viewer, so a page key scrolls');
  const caps = KEY_LOCAL.discover_models().effort_capabilities;
  assert.deepEqual(caps.map((c) => c.model), ['opus', 'sonnet', 'haiku']);
  assert.ok(caps.every((c) => c.supported_efforts.length >= 3), 'the switch dialog can step effort');
});

test('dagFixtures: the recursive DAG browser has the capabilities it asks for and rows in every panel', async () => {
  const fx = dagFixtures();
  const ask = async (method, params = {}) => (await fixtureResponse(fx[method], { id: 1, method, params })).result;
  const caps = await ask('GetDaemonCapabilities');
  for (const c of ['recursive_dag_inspection', 'recursive_dag_run_inspection', 'recursive_dag_live_status_inspection', 'recursive_dag_artifact_list_pagination', 'recursive_dag_cancellation_control', 'recursive_dag_recovery_control', 'recursive_dag_scheduler_control']) assert.equal(caps[c], true, c);
  const graphs = await ask('ListRecursiveTaskGraphs');
  assert.equal(graphs.length, 2);
  const newest = [...graphs].sort((a, b) => b.updated_at.localeCompare(a.updated_at))[0];
  const detail = await ask('GetRecursiveTaskGraph');
  assert.equal(detail.graph.id, newest.id, 'the graph the browser loads first is the one with the detail');
  assert.equal(detail.nodes.length, 5, 'Tasks');
  assert.equal(detail.nodes.filter((n) => n.parent_task_id === null).length, 1, 'one root task');
  assert.equal(detail.graph.root_task_id, detail.nodes.find((n) => n.parent_task_id === null).id);
  assert.equal((await ask('ListRecursiveSchedulerRuns')).length, 2, 'Runs');
  assert.equal((await ask('GetRecursiveRecoveryStatus')).deferred_graphs.length, 1, 'Recovery');
  assert.equal((await ask('ListRecursiveCancellationRequests')).length, 1, 'Cancellations');
  assert.equal((await ask('ListStaleRecursiveLiveAttemptHeartbeats')).length, 1, 'Heartbeats');
  assert.equal((await ask('ListRecursiveLiveAttempts')).length, 2, 'Live');
  const page = await ask('ListRecursiveExecutionArtifactSummaries');
  assert.equal(page.items.length, 3, 'Artifacts');
  assert.ok(page.items.every((i) => i.graph_id === newest.id), 'the browser drops artifact rows of another graph');
  const ids = [...detail.nodes.map((n) => n.id), ...graphs.map((g) => g.id), ...page.items.map((i) => i.id)];
  assert.equal(new Set(ids).size, ids.length, 'ids are unique');
  assert.ok(ids.every((i) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(i)), 'ids are UUIDs, which the bridge validates');
  assert.equal(typeof keyFixtures(FIXTURES).ListRecursiveTaskGraphs, 'function', 'the key fixtures include them');
});

// ---------- the command line and the theme runs ----------

test('audit keys takes --overlay and audit themes takes --themes; each belongs to its own command', () => {
  assert.deepEqual(parseArgs(['keys', '--overlay', 'Trash,Sort Picker']).overlay, ['Trash', 'Sort Picker']);
  assert.equal(parseArgs(['keys']).overlay, null, 'no flag sweeps every overlay');
  assert.throws(() => parseArgs(['views', '--overlay', 'Trash']), /--overlay belongs to the keys command/);
  assert.throws(() => parseArgs(['keys', '--overlay', '']), /--overlay takes overlay names/);
  assert.deepEqual(parseArgs(['themes', '--themes', 'light,ruby']).themes, ['light', 'ruby']);
  assert.equal(parseArgs(['themes', '--themes', 'all']).themes, null);
  assert.equal(parseArgs(['themes']).themes, null);
  assert.throws(() => parseArgs(['views', '--themes', 'light']), /--themes belongs to the themes command/);
  assert.throws(() => parseThemeKeys('Not A Theme'), /--themes takes/);
});

test('themes walks the two default runs and all 14 built-in themes, not six', async () => {
  const runs = await themeRunsFromApp();
  const { THEMES } = await import('../dist/settings.generated.js');
  assert.equal(THEMES.length, 14);
  assert.equal(runs.length, 16);
  assert.deepEqual(runs.slice(0, 2).map((r) => r.name), ['default-dark', 'default-system-light']);
  assert.deepEqual(runs.slice(2).map((r) => r.theme), THEMES.map((t) => t.key), 'every THEMES key, in order');
  assert.equal(new Set(runs.map((r) => r.name)).size, 16, 'run names are unique (screenshot labels)');
  assert.deepEqual(runs.find((r) => r.theme === 'light'), { name: 'light', theme: 'light', scheme: 'light' });
  assert.equal(runs.find((r) => r.theme === 'ruby').scheme, 'dark');
  assert.deepEqual((await themeRunsFromApp(['light', 'ruby'])).map((r) => r.name), ['default-dark', 'default-system-light', 'light', 'ruby']);
  await assert.rejects(() => themeRunsFromApp(['nope']), /--themes names nope, which the app does not build in/);
  assert.deepEqual(themeRunsFor([]).map((r) => r.name), ['default-dark', 'default-system-light']);
});

// ---------- in the browser ----------

test('in the browser: the sweep presses every key of Trash and Notifications and none changes nothing; keys.jsonl is written as it goes', { skip: SKIP, timeout: 400000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-audit-keys-'));
  const results = await runKeySweep({ out, overlays: ['Trash', 'Notifications'], quiet: true });
  assert.deepEqual(failures(results), []);
  assert.deepEqual(results.map((r) => [r.overlay, r.opened, r.helpLists]), [['Trash', true, true], ['Notifications', true, true]], 'the ? help lists both overlays');
  const trash = results[0].rows;
  assert.deepEqual(trash.map((r) => r.label), ['j', 'k', 'Down', 'Up', 'G', 'U', 'D', 'Esc', 'q'], 'every key of the Trash table, closing keys last');
  assert.ok(trash.every((r) => r.status === 'changed'), JSON.stringify(trash.filter((r) => r.status !== 'changed')));
  const notices = results[1].rows;
  assert.deepEqual(notices.filter((r) => r.status === 'expected-no-op').map((r) => r.label), ['Enter', 'Ctrl+0']);
  assert.ok(notices.find((r) => r.label === 'N').changed.includes('text'), 'N dismisses the seeded notices');
  const lines = readFileSync(join(out, 'keys.jsonl'), 'utf8').trim().split('\n').map((l) => JSON.parse(l));
  assert.equal(lines.filter((l) => l.summary).length, 2, 'one summary line per overlay');
  assert.equal(lines.filter((l) => l.label).length, trash.length + notices.length, 'one line per key');
});

test('in the browser: a listed key that does nothing is reported as no-change', { skip: SKIP, timeout: 200000 }, async () => {
  const name = 'Audit Probe (Trash)';
  SWEEP_OVERLAYS[name] = { action: 'open-trash' };
  const a = await lib.open({ width: 1280, height: 800, vim: true, stubs: WALK_STUBS, local: KEY_LOCAL, fixtures: keyFixtures(FIXTURES) });
  try {
    await lib.sleep(500);
    const tables = [{ name, rows: [{ keys: 'j / k', action: 'Move', detail: '' }, { keys: 'x', action: 'Nothing in Trash', detail: '' }, { keys: '#', action: 'Nothing either', detail: '' }] }];
    const r = await sweepOverlay(a, name, { lib, reload: reloadPage, tables });
    assert.deepEqual(r.rows.map((x) => [x.label, x.status]), [['j', 'changed'], ['k', 'changed'], ['x', 'no-change'], ['#', 'no-change']]);
    assert.deepEqual(failures([r]), [`${name}: x`, `${name}: #`]);
  } finally { delete SWEEP_OVERLAYS[name]; await a.close(); }
});
