// Gap sweep G3 (hub #3000): the Command Mode rows — :manual, :rotation-abandon,
// :manager-reseat, :cold-restore, :cold-archive, :context, :blank <text> and
// the window and tab substitutes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, paletteEntries, unavailable } from '../dist/registry.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import { COMMANDMODE_ACTIONS, COMMANDMODE_COMMANDS, COMMANDMODE_RULES } from '../dist/actions/commandmode.js';
import {
  ACTIVE_TASK_MAX, CONFIRM_WINDOW_MS, RESEAT_USAGE, WINDOW_SUBSTITUTES, abandonMessage, blankLaunch, bytesText,
  coldHasProblems, coldLastRunLine, coldStartedMessage, coldStatusLine, contextMessage, contextText, parseAbandonTarget,
  parseColdArchive, parseReseatArgs, reseatMessage, restoreMessage, secondPress,
} from '../dist/commandmode-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const snap = (target) => ({
  open: null, cursorOnRow: !!target, rowCount: 1, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, commandMode: { target },
});
const cmd = (line) => new KeyEngine(desktopKeymap()).runCommand(line);

// ---------- registry ----------

test('every Command Mode action is registered with one rule', () => {
  const ids = COMMANDMODE_ACTIONS.map((a) => a.id);
  assert.equal(new Set(ids).size, ids.length);
  assert.deepEqual(Object.keys(COMMANDMODE_RULES).sort(), [...ids].sort());
  for (const id of ids) assert.ok(ACTIONS.some((a) => a.id === id), id);
  const entries = paletteEntries(snap({ id: 's1', project_id: 'p', status: 'Running' }), 'other');
  for (const id of ids) assert.ok(entries.some((e) => e.id === id && e.enabled), `${id} is enabled`);
});

test('the TUI commands and their aliases resolve, with the argument', () => {
  const want = {
    manual: 'cm-manual', man: 'cm-manual', 'manual pdf': 'cm-manual', 'rotation-abandon': 'cm-rotation-abandon',
    'rotation-abandon codex/gpt-5': 'cm-rotation-abandon', 'manager-reseat Codex down': 'cm-manager-reseat',
    'cold-restore': 'cm-cold-restore', 'cold-archive': 'cm-cold-archive', 'cold-archive move': 'cm-cold-archive',
    context: 'cm-context', ctx: 'cm-context', 'ctx ship it': 'cm-context', split: 'cm-split', sp: 'cm-split',
    vsplit: 'cm-vsplit', vs: 'cm-vsplit', only: 'cm-only', on: 'cm-only', tabnew: 'cm-tabnew', tabe: 'cm-tabnew',
    tabclose: 'cm-tabclose', tabc: 'cm-tabclose',
  };
  for (const [line, action] of Object.entries(want)) assert.equal(cmd(line).action, action, `:${line}`);
  assert.equal(cmd('ctx ship it').arg, 'ship it');
  assert.equal(cmd('rotation-abandon codex/gpt-5').arg, 'codex/gpt-5');
  // :blank stays the core launch action; the runtime claims it when it carries text.
  assert.equal(cmd('blank fix the build').action, 'launch');
  assert.equal(cmd('blank fix the build').arg, 'fix the build');
  assert.equal(Object.keys(COMMANDMODE_COMMANDS).length, 11);
});

test('session commands need a target; the rest are always available', () => {
  for (const id of ['cm-rotation-abandon', 'cm-manager-reseat', 'cm-cold-restore', 'cm-context']) {
    assert.equal(unavailable(id, snap(null)), 'No session selected', id);
    assert.equal(unavailable(id, snap({ id: 's', status: 'Running' })), null, id);
  }
  for (const id of ['cm-manual', 'cm-cold-archive', 'cm-split', 'cm-vsplit', 'cm-only', 'cm-tabnew', 'cm-tabclose']) {
    assert.equal(unavailable(id, snap(null)), null, id);
  }
});

// ---------- models ----------

test(':rotation-abandon parses provider[/model] case-insensitively', () => {
  assert.deepEqual(parseAbandonTarget(undefined), { provider: null, model: null });
  assert.deepEqual(parseAbandonTarget('  '), { provider: null, model: null });
  assert.deepEqual(parseAbandonTarget('codex/gpt-5'), { provider: 'Codex', model: 'gpt-5' });
  assert.deepEqual(parseAbandonTarget('CLAUDE'), { provider: 'Claude', model: null });
  assert.deepEqual(parseAbandonTarget('openrouter/openai/gpt-x'), { provider: 'OpenRouter', model: 'openai/gpt-x' });
  assert.match(parseAbandonTarget('gpt/x').error, /^Unknown provider `gpt`/);
});

test(':manager-reseat needs a provider and a reason', () => {
  assert.deepEqual(parseReseatArgs('codex/gpt-6-astra claude is down'), { provider: 'Codex', model: 'gpt-6-astra', reason: 'claude is down' });
  assert.deepEqual(parseReseatArgs('Claude  rate limited  '), { provider: 'Claude', model: null, reason: 'rate limited' });
  assert.deepEqual(parseReseatArgs('codex'), { error: RESEAT_USAGE });
  assert.deepEqual(parseReseatArgs(''), { error: RESEAT_USAGE });
  assert.match(parseReseatArgs('nope down').error, /Unknown provider/);
});

test('the confirm gate needs the same command again within 10 s', () => {
  const first = secondPress(null, 'a', 1000);
  assert.equal(first.confirmed, false);
  assert.deepEqual(first.next, { key: 'a', at: 1000 });
  assert.equal(secondPress(first.next, 'a', 1000 + CONFIRM_WINDOW_MS - 1).confirmed, true);
  assert.equal(secondPress(first.next, 'a', 1000 + CONFIRM_WINDOW_MS).confirmed, false);
  const other = secondPress(first.next, 'b', 1500);
  assert.equal(other.confirmed, false);
  assert.deepEqual(other.next, { key: 'b', at: 1500 });
  assert.equal(secondPress(first.next, 'a', 2000).next, null);
});

test('receipts read as the TUI toasts them', () => {
  assert.equal(abandonMessage({ rotation_id: 'r1', holder_id: 'h1' }), 'Rotation r1 abandoned: the replacement rotates from h1');
  assert.equal(reseatMessage({ successor_session_id: 'n', predecessor_session_id: 'o' }), 'Manager reseated: n replaces o');
  assert.equal(restoreMessage({ outcome: 'restored', events: 1 }), 'Restored from cold storage: 1 event back in the live database');
  assert.equal(restoreMessage({ outcome: 'restored', events: 3 }), 'Restored from cold storage: 3 events back in the live database');
  assert.equal(restoreMessage({ outcome: 'already_hot' }), 'Already in the live database: nothing to restore');
  assert.equal(coldStartedMessage({ run_id: 'abcdef123456', mode: 'stage', scheduled: false }), 'Cold-storage stage pass started (run abcdef12)');
  assert.equal(coldStartedMessage({ run_id: 'abcdef123456', mode: 'compact', scheduled: true }), 'Cold-storage compact scheduled for the next supervised restart (run abcdef12)');
});

test(':cold-archive parses its sub-commands', () => {
  assert.deepEqual(parseColdArchive(undefined), { kind: 'status' });
  assert.deepEqual(parseColdArchive('status'), { kind: 'status' });
  assert.deepEqual(parseColdArchive('run'), { kind: 'run', mode: 'stage' });
  assert.deepEqual(parseColdArchive('stage'), { kind: 'run', mode: 'stage' });
  for (const m of ['move', 'verify', 'compact']) assert.deepEqual(parseColdArchive(m), { kind: 'run', mode: m });
  assert.match(parseColdArchive('purge').error, /Unknown cold-archive action `purge`/);
});

test('the cold-storage status reads like the TUI line', () => {
  assert.equal(bytesText(12), '12 B');
  assert.equal(bytesText(3 * 1024), '3.0 KiB');
  assert.equal(bytesText(1.5 * 1024 ** 3), '1.5 GiB');
  const status = {
    settings: { enabled: true }, live_db: { file_bytes: 2 * 1024 ** 2 },
    archives: [{ bytes: 1024 }, { bytes: 1024 }], totals: { staged_sessions: 4, verified_sessions: 3, cold_sessions: 2, restored_sessions: 1, sessions_with_error: 0 },
    compaction: { pending: true, due: true }, running: false, errors: [], last_run: null,
  };
  assert.equal(coldStatusLine(status), 'archive on · live DB 2.0 MiB · 2 files 2.0 KiB · 4 staged (3 verified) · 2 cold · 1 restored · compaction at next restart · idle');
  assert.equal(coldLastRunLine(status), 'no pass has run yet');
  assert.equal(coldHasProblems(status), false);
  const bad = { ...status, errors: ['disk   full'], totals: { ...status.totals, sessions_with_error: 1 }, last_run: {
    mode: 'move', trigger: 'operator', outcome: 'failed', started_at: 'T', sessions_staged: 2, sessions_moved: 1,
    raw_bytes: 2048, compressed_bytes: 1024, error_code: 'backup_stale', error_detail: null,
  } };
  assert.match(coldStatusLine(bad), /^1 open problem · 1 session with archive errors · problem: disk full · archive on/);
  assert.equal(coldLastRunLine(bad), 'move (operator) · failed · T · 2 staged · 1 moved · 2.0 KiB → 1.0 KiB · problem: backup_stale · 1 open problem · 1 session with archive errors · problem: disk full');
  assert.equal(coldHasProblems(bad), true);
});

test(':context caps the active task at 500 bytes and a bare command clears it', () => {
  assert.equal(contextText(undefined), null);
  assert.equal(contextText('  '), null);
  assert.equal(contextText(' ship it '), 'ship it');
  assert.equal(contextText('x'.repeat(600)).length, ACTIVE_TASK_MAX);
  const wide = contextText('é'.repeat(400));
  assert.equal(new TextEncoder().encode(wide).length, 500);
  assert.equal(contextMessage(null), 'Active task cleared');
  assert.equal(contextMessage('t'), 'Active task set: t');
});

test(':blank <text> launches on the default model in the filtered project', () => {
  assert.deepEqual(blankLaunch('fix it', null, null), { query: 'fix it', provider: 'Claude' });
  assert.deepEqual(
    blankLaunch('fix it', { provider: 'Codex', model: 'gpt-6-astra' }, { id: 'p1', path: '/w/p1' }),
    { query: 'fix it', provider: 'Codex', model: 'gpt-6-astra', project_id: 'p1', working_dir: '/w/p1' },
  );
  assert.deepEqual(blankLaunch('t', { provider: 'Claude', model: '' }, { id: 'p1', path: null }), { query: 't', provider: 'Claude', project_id: 'p1' });
});

test('each window and tab command names its desktop substitute', () => {
  assert.deepEqual(Object.keys(WINDOW_SUBSTITUTES).sort(), ['only', 'split', 'tabclose', 'tabnew', 'vsplit']);
  for (const text of Object.values(WINDOW_SUBSTITUTES)) assert.ok(text.length > 20);
});

// ---------- request shapes ----------

test('the run side sends the Tauri commands the Rust module defines', () => {
  const run = readFileSync(here('../src/actions/commandmode-run.ts'), 'utf8');
  const rust = readFileSync(here('../src-tauri/src/commandmode.rs'), 'utf8');
  for (const c of ['abandon_rotation', 'reseat_manager', 'cold_restore', 'cold_archive_status', 'cold_archive_run', 'update_active_task']) {
    assert.match(rust, new RegExp(`pub async fn ${c}\\(`), c);
    assert.match(run, new RegExp(`'${c}'`), c);
  }
  assert.match(rust, /pub fn manual_sections\(\)/);
  assert.match(run, /'manual_sections'/);
  // Tauri maps camelCase invoke args onto the snake_case parameters.
  assert.match(run, /'abandon_rotation', \{ sessionId: t\.id, provider: parsed\.provider, model: parsed\.model, idempotencyKey: mint\(\) \}/);
  assert.match(rust, /session_id: String,\s*provider: Option<String>,\s*model: Option<String>,\s*idempotency_key: String,/);
  assert.match(run, /'reseat_manager', \{ projectId: t\.project_id, provider: parsed\.provider, model: parsed\.model, reason: parsed\.reason \}/);
  assert.match(rust, /project_id: String,\s*provider: String,\s*model: Option<String>,\s*reason: String,/);
  assert.match(run, /'update_active_task', \{ sessionId: t\.id, activeTask: text \}/);
  assert.match(rust, /session_id: String,\s*active_task: Option<String>,/);
  assert.match(run, /'cold_archive_run', \{ mode: cmd\.mode \}/);
  for (const m of ['AbandonBlockedRotation', 'ReseatHarnessManager', 'RestoreColdSession', 'GetColdArchiveStatus', 'RunColdArchiveNow', 'UpdateActiveTask']) {
    assert.match(rust, new RegExp(`"${m}"`), m);
  }
});
