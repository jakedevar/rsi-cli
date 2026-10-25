// Gap sweep G2 (hub #3000): session operations — pin, testing-needed,
// auto-rotation, cancel retry, rotate, commit and push, landing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, keyHints, paletteEntries, unavailable } from '../dist/registry.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import { SESSIONOPS_ACTIONS, SESSIONOPS_COMMANDS, SESSIONOPS_KEYS, SESSIONOPS_RULES } from '../dist/actions/sessionops.js';
import {
  CI_COMMIT, LAND_WATCH_MAX, TOGGLES, commitBlock, landKey,
  retryBadge, retryCancelled, retryMessage, rotateBlock, rowMarks, settle, settledMessage,
  testingBlock, toggleFailure, toggleMessage, toggledAt, watchIds,
} from '../dist/sessionops-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

const sess = (over = {}) => ({ id: 's1', status: 'Running', session_kind: 'Standard', ...over });
const snap = (over = {}, ops = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null,
  ops: { target: sess(), rotateTarget: sess(), readOnly: false, ...ops },
  ...over,
});

// ---------- registry ----------

test('every session op is registered with one rule and a palette entry', () => {
  const ids = SESSIONOPS_ACTIONS.map((a) => a.id);
  assert.equal(new Set(ids).size, ids.length);
  assert.deepEqual(Object.keys(SESSIONOPS_RULES).sort(), [...ids].sort());
  const entries = paletteEntries(snap(), 'other');
  for (const id of ids) {
    assert.ok(ACTIONS.some((a) => a.id === id), id);
    assert.ok(entries.some((e) => e.id === id && e.enabled), `${id} is enabled in the palette`);
  }
});

test('the TUI keys and : commands run the session ops', () => {
  assert.deepEqual(keyHints('toggle-pin'), ['P']);
  assert.deepEqual(keyHints('toggle-testing'), ['<Space>t']);
  assert.deepEqual(keyHints('toggle-rotation'), ['<Space>r']);
  assert.deepEqual(keyHints('cancel-retry'), ['<Space>k']);
  assert.deepEqual(keyHints('rotate-session'), ['R', ':rotate']);
  assert.deepEqual(keyHints('commit-push'), ['<Space>x']);
  assert.deepEqual(keyHints('land-session'), ['<Space>gl', ':land']);
  assert.equal(Object.keys(SESSIONOPS_KEYS).length, 7);
  assert.deepEqual(Object.keys(SESSIONOPS_COMMANDS).sort(), ['land', 'rotate']);
  const feed = (keys) => {
    const e = new KeyEngine(desktopKeymap());
    let out;
    for (const k of keys) out = e.feed(k);
    return out;
  };
  assert.deepEqual(feed(['P']), { kind: 'run', action: 'toggle-pin', count: null, keys: 'P' });
  assert.equal(feed(['Space', 'g', 'l']).action, 'land-session');
  assert.equal(feed(['Space', 'x']).action, 'commit-push');
  assert.equal(feed(['R']).action, 'rotate-session');
  const cmd = new KeyEngine(desktopKeymap()).runCommand('rot');
  assert.equal(cmd.action, 'rotate-session');
});

test('the rules name what is missing', () => {
  const none = snap({ open: null }, { target: null, rotateTarget: null });
  for (const id of ['toggle-pin', 'toggle-testing', 'toggle-rotation', 'cancel-retry', 'rotate-session', 'land-session']) {
    assert.match(unavailable(id, none), /move the cursor/, id);
  }
  assert.match(unavailable('commit-push', none), /Open a session first/);
  const ro = snap({ open: { status: 'Archived', readOnly: true, pauseLevel: 'none', hasQuestion: false } }, { readOnly: true });
  assert.equal(unavailable('toggle-pin', ro), null, 'pinning an archived row is allowed');
  assert.equal(unavailable('toggle-rotation', ro), 'This session is read-only');
  assert.equal(unavailable('commit-push', ro), 'This session is read-only');
  const epic = snap({}, { target: sess({ session_kind: 'Epic' }), rotateTarget: sess({ session_kind: 'Epic' }) });
  assert.match(unavailable('toggle-testing', epic), /leaf/);
  assert.match(unavailable('rotate-session', epic), /no context/);
  const starting = snap({}, { target: sess({ status: 'Starting' }), rotateTarget: sess({ status: 'Starting' }) });
  assert.equal(unavailable('commit-push', starting), 'Session is starting — wait for it to begin');
  assert.equal(unavailable('rotate-session', starting), 'Can only rotate running or stopped sessions');
  assert.equal(unavailable('land-session', snap({ modalOpen: true })), 'Close the open dialog first');
});

// ---------- model ----------

test('toggle answers carry the new timestamp and the TUI toasts', () => {
  assert.equal(toggledAt('pin', { pinned_at: '2026-10-18T00:00:00Z' }), '2026-10-18T00:00:00Z');
  assert.equal(toggledAt('pin', { pinned_at: null }), null);
  assert.equal(toggledAt('testing', { testing_needed_at: 'x' }), 'x');
  assert.equal(toggledAt('rotation', { rotation_disabled_at: 'y' }), 'y');
  assert.equal(toggledAt('rotation', null), null);
  assert.equal(toggleMessage('pin', 'x'), 'Session pinned');
  assert.equal(toggleMessage('pin', null), 'Session unpinned');
  assert.equal(toggleMessage('testing', 'x'), 'Marked for testing');
  assert.equal(toggleMessage('testing', null), 'Testing mark removed');
  assert.equal(toggleMessage('rotation', 'x'), 'Auto-rotation disabled');
  assert.equal(toggleMessage('rotation', null), 'Auto-rotation enabled');
  assert.equal(toggleFailure('pin'), 'Pin failed');
  assert.deepEqual(Object.values(TOGGLES).map((t) => t.command), ['toggle_pin', 'toggle_testing_needed', 'toggle_rotation_disabled']);
  assert.equal(testingBlock({ session_kind: 'Task' }), null);
});

test('cancel retry trusts the daemon bool; the badge shows the attempt budget', () => {
  assert.equal(retryCancelled({ cancelled: true }), true);
  assert.equal(retryCancelled({ cancelled: false }), false);
  assert.equal(retryCancelled({}), false);
  assert.equal(retryMessage(true), 'Retry cancelled');
  assert.equal(retryMessage(false), 'No pending retry to cancel');
  assert.equal(retryBadge({ retry_attempt: 1, max_retries: 3 }), '↻1/3');
  assert.equal(retryBadge({ max_retries: 2 }), '↻0/2');
  assert.equal(retryBadge({ max_retries: 0 }), null);
});

test('rotate and commit follow the TUI status checks', () => {
  for (const st of ['Running', 'Completed', 'Interrupted', 'Failed']) assert.equal(rotateBlock({ status: st }), null, st);
  assert.match(rotateBlock({ status: 'WaitingApproval' }), /running or stopped/);
  assert.equal(CI_COMMIT, '/ci_commit');
  assert.equal(commitBlock({ status: 'Running' }), null);
  assert.equal(commitBlock({ status: 'Completed' }), null);
  assert.match(commitBlock({ status: 'Starting' }), /starting/);
});

test('row markers lead with the pin and keep the TUI glyphs', () => {
  const marks = rowMarks(sess({
    pinned_at: 'a', testing_needed_at: 'b', rotation_disabled_at: 'c', retry_attempt: 1, max_retries: 2, rotation_depth: 3,
  }));
  assert.deepEqual(marks.map((m) => m.glyph), ['◆', '☐', '⊘', '↻1/2', '↺3']);
  assert.deepEqual(rowMarks(sess()), []);
});

// ---------- landing ----------

const src = (over = {}) => ({ session_id: 's1', branch: 'rsi/x', commit: 'c'.repeat(40), ahead: 2, on_rolling: false, dirty: false, title: null, ...over });
const entry = (state, outcome = null) => ({ id: 'e1', sequence: 7, source_commit: 'a'.repeat(40), state, outcome });

test('the replay key of a confirmed landing keeps the TUI shape', () => {
  assert.equal(landKey('u'), 'operator-land-u');
});

test('settled entries are announced with their outcome and dropped from the watch', () => {
  const done = { ...entry('published', { landed_sha: 'b'.repeat(40) }), id: 'd' };
  const refused = { ...entry('refused', { refusal: 'queue_x' }), id: 'r' };
  const running = { ...entry('gating'), id: 'g' };
  assert.match(settledMessage('T', done), /^Landed “T”\./);
  assert.match(settledMessage('T', refused), /^Not landed: “T”\. queue_x$/);
  const watching = [{ entryId: 'd', label: 'done' }, { entryId: 'g', label: 'running' }, { entryId: 'r', label: 'refused' }, { entryId: 'gone', label: 'x' }];
  const { settled, rest } = settle(watching, [done, running, refused]);
  assert.deepEqual(settled.map((s) => s.label), ['done', 'refused']);
  assert.deepEqual(rest.map((w) => w.entryId), ['g', 'gone']);
  const many = Array.from({ length: 70 }, (_, i) => ({ entryId: String(i), label: '' }));
  assert.equal(watchIds(many).length, LAND_WATCH_MAX);
});

// ---------- request shapes ----------

test('the run side sends the Tauri commands the Rust module defines', () => {
  const run = readFileSync(here('../src/actions/sessionops-run.ts'), 'utf8');
  const rust = readFileSync(here('../src-tauri/src/sessionops.rs'), 'utf8');
  for (const cmd of ['toggle_pin', 'toggle_testing_needed', 'toggle_rotation_disabled', 'cancel_retry', 'rotate_session', 'land_session', 'rolling_queue_entries']) {
    assert.match(rust, new RegExp(`pub async fn ${cmd}\\(`), cmd);
  }
  for (const cmd of ['cancel_retry', 'rotate_session', 'land_session', 'rolling_queue_entries']) {
    assert.match(run, new RegExp(`'${cmd}'`), cmd);
  }
  // Tauri maps camelCase invoke args onto the snake_case parameters.
  assert.match(run, /'land_session', \{ sessionId: t\.id, commit: null, idempotencyKey: null, dryRun: true \}/);
  assert.match(run, /'rolling_queue_entries', \{ entryIds: /);
  assert.match(rust, /session_id: String,\s*commit: Option<String>,\s*idempotency_key: Option<String>,\s*dry_run: bool,/);
});
