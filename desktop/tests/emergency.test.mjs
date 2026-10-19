// Emergency stop (hub #3008 follow-up): registry entry, keys, the confirm gate,
// the report-to-pause-state mapping and the Tauri command's wiring.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, appMenuModel, keyHints, paletteEntries, unavailable } from '../dist/registry.js';
import {
  EMERGENCY_ACTIONS, EMERGENCY_COMMAND_BINDINGS, EMERGENCY_KEY_BINDINGS, EMERGENCY_RULES, isEmergencyAction,
} from '../dist/actions/emergency-spec.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  NO_PAUSES, applyStopReport, dropPause, parseStopReport, prunePauses, stopMessage, stopNeedsConfirm,
} from '../dist/emergency-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const ID_A = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';
const ID_B = '0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10';

const snap = (over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, ...over,
});

test('the emergency stop is registered with the TUI keys and is always available', () => {
  assert.deepEqual(EMERGENCY_ACTIONS.map((a) => a.id), ['emergency-stop']);
  assert.ok(ACTIONS.some((a) => a.id === 'emergency-stop'));
  assert.equal(typeof EMERGENCY_RULES['emergency-stop'], 'function');
  assert.ok(isEmergencyAction('emergency-stop'));
  assert.equal(isEmergencyAction('hard-pause'), false);
  assert.deepEqual(keyHints('emergency-stop'), ['<Space>X', ':stopall']);
  assert.equal(unavailable('emergency-stop', snap()), null);
  assert.equal(unavailable('emergency-stop', snap({ open: { status: 'Completed', readOnly: true, pauseLevel: 'none', hasQuestion: false } })), null);
  assert.ok(paletteEntries(snap(), 'other').some((e) => e.id === 'emergency-stop'));
  const session = appMenuModel('other', snap()).find((m) => m.id === 'session');
  assert.ok(session.items.some((n) => n.kind === 'item' && n.id === 'act:emergency-stop'));
});

test('<Space>X, :stopall and :stop-all run the action; the key path has no press-twice guard like the TUI', () => {
  const e = new KeyEngine(desktopKeymap());
  assert.equal(e.feed('Space').kind, 'pending');
  assert.deepEqual(e.feed('X'), { kind: 'run', action: 'emergency-stop', count: null, keys: '<Space>X' });
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('stopall').action, 'emergency-stop');
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('stop-all').action, 'emergency-stop');
  // Unlike X (hard pause, press twice) the TUI runs <Space>X on one press.
  assert.equal(EMERGENCY_KEY_BINDINGS['<Space>X'].confirm, undefined);
  assert.equal(EMERGENCY_COMMAND_BINDINGS.stopall.confirm, undefined);
  assert.equal(desktopKeymap().root.children.get('X').leaf.binding.confirm, 'twice');
});

test('only the key path (chord and : line) skips the confirm dialog, as the TUI does', () => {
  assert.equal(stopNeedsConfirm('key'), false);
  for (const source of ['menu', 'context', 'palette', 'button']) assert.equal(stopNeedsConfirm(source), true, source);
  // The DOM side asks before it calls the command, and only when the source needs it.
  const ts = readFileSync(here('../src/actions/emergency.ts'), 'utf8');
  const ask = ts.indexOf('stopNeedsConfirm(inv.source)');
  const dialog = ts.indexOf('confirmDialog(', ask);
  const call = ts.indexOf("invoke<unknown>('emergency_stop_all')");
  assert.ok(ask > 0 && dialog > ask && call > dialog, 'confirm gate comes before the command');
  assert.equal(ts.split("'emergency_stop_all'").length - 1, 1, 'one call site');
});

test('the daemon report maps to the TUI toast and to local HARD pauses', () => {
  const report = parseStopReport({
    previous_mode: 'normal', current_mode: 'stop_all',
    cancelled_invocations: ['i1', 'i2', 'i3'], skipped_invocations: ['i4'],
    interrupted_sessions: [ID_A.toUpperCase(), ID_B],
  });
  assert.deepEqual(report, { cancelled: 3, interrupted: [ID_A, ID_B] });
  assert.equal(stopMessage(report), 'Emergency stop applied: 3 cancelled, 2 interrupted');
  const pauses = applyStopReport(NO_PAUSES, report);
  assert.deepEqual([...pauses], [[ID_A, 'hard'], [ID_B, 'hard']]);
  assert.equal(NO_PAUSES.size, 0, 'the empty map is not mutated');
  // Clearing or downgrading one pause drops only that marker; a gone session drops its marker.
  assert.deepEqual([...dropPause(pauses, ID_A)], [[ID_B, 'hard']]);
  assert.equal(dropPause(pauses, 'nope'), pauses);
  assert.deepEqual([...prunePauses(pauses, [ID_B, 'other'])], [[ID_B, 'hard']]);
  assert.equal(prunePauses(pauses, [ID_A, ID_B]), pauses);
});

test('an empty or odd report is handled without marking anything', () => {
  assert.deepEqual(parseStopReport({}), { cancelled: 0, interrupted: [] });
  assert.equal(stopMessage(parseStopReport({})), 'Emergency stop applied: 0 cancelled, 0 interrupted');
  assert.deepEqual(parseStopReport({ interrupted_sessions: [1, null, '', ID_A], cancelled_invocations: 'x' }), { cancelled: 0, interrupted: [ID_A] });
  assert.equal(parseStopReport(null), null);
  assert.equal(parseStopReport('ok'), null);
  assert.equal(applyStopReport(NO_PAUSES, { cancelled: 2, interrupted: [] }), NO_PAUSES);
});

test('the Tauri command takes no arguments and is in the generated handler list', () => {
  const rs = readFileSync(here('../src-tauri/src/emergency.rs'), 'utf8');
  assert.match(rs, /pub async fn emergency_stop_all\(state: State<'_, AppState>\) -> CmdResult/);
  assert.match(readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8'), /emergency::emergency_stop_all/);
  assert.match(readFileSync(here('../src-tauri/src/lib.rs'), 'utf8'), /^pub mod emergency;$/m);
});
