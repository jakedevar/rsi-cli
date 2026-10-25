// Manager surfaces (hub #3011, slice 9): registry entries, the TUI keys and
// `:` commands that open each view, and the availability rules.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { keyHints, unavailable } from '../dist/registry.js';
import { MANAGERS_ACTIONS, MANAGERS_COMMAND_BINDINGS, MANAGERS_KEY_BINDINGS, MANAGERS_RULES, isManagersAction } from '../dist/actions/managers.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';

const base = { open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false, showFinished: false, modalOpen: false, event: null };
const snap = (managers) => ({ ...base, managers });

const run = (...tokens) => {
  const e = new KeyEngine(desktopKeymap());
  let out;
  for (const t of tokens) out = e.feed(t);
  return out;
};

test('the TUI keys open the board, decisions, policy and the global workspace', () => {
  assert.equal(run('Space', 'g', 'b').action, 'mgr-board');
  assert.equal(run('Space', 'g', 'd').action, 'mgr-decisions');
  assert.equal(run('Space', 'g', 'p').action, 'mgr-policy');
  assert.equal(run('g', 'm').action, 'mgr-global');
  assert.equal(MANAGERS_KEY_BINDINGS.gm.action, 'mgr-global');
});

test('every manager : command resolves, aliases included', () => {
  const cmd = (line) => new KeyEngine(desktopKeymap()).runCommand(line);
  const want = {
    manager: 'mgr-board', 'manager board': 'mgr-board', 'manager decisions': 'mgr-decisions', 'manager inbox': 'mgr-inbox',
    'manager inspect': 'mgr-inspect', 'manager policy': 'mgr-policy', 'manager scope': 'mgr-scope', 'manager appoint': 'mgr-appoint',
    'manager clear': 'mgr-clear', 'manager tree': 'mgr-tree', 'global-manager': 'mgr-global', gm: 'mgr-global',
    'manager workspace': 'mgr-global', 'manager global': 'mgr-global-grant', 'manager global revoke': 'mgr-global-revoke',
    dashboard: 'mgr-dashboard', gd: 'mgr-dashboard',
    fleet: 'mgr-fleet', 'manager portfolio': 'mgr-portfolio', 'manager escalations all': 'mgr-escalations',
    'manager friction 48': 'mgr-friction',
  };
  for (const [line, action] of Object.entries(want)) assert.equal(cmd(line).action, action, `:${line}`);
  assert.equal(cmd('manager friction 48').arg, '48');
  for (const b of Object.values(MANAGERS_COMMAND_BINDINGS)) assert.ok(isManagersAction(b.action));
});

test('key hints name the TUI bindings', () => {
  assert.deepEqual(keyHints('mgr-board'), ['<Space>gb', ':manager', ':manager board']);
  assert.deepEqual(keyHints('mgr-global'), ['gm', ':global-manager']);
  assert.deepEqual(keyHints('mgr-dashboard'), ['gd', ':dashboard']);
  assert.deepEqual(keyHints('mgr-tree'), [':manager tree']);
});

test('project views need a project; appoint needs a seat candidate; close needs an open view', () => {
  assert.equal(unavailable('mgr-tree', snap({ view: null, hasProject: false, seatCandidate: false })), null);
  assert.match(unavailable('mgr-board', snap({ view: null, hasProject: false, seatCandidate: false })), /No project/);
  assert.equal(unavailable('mgr-board', snap({ view: null, hasProject: true, seatCandidate: false })), null);
  assert.match(unavailable('mgr-appoint', snap({ view: null, hasProject: true, seatCandidate: false })), /Select the session/);
  assert.equal(unavailable('mgr-appoint', snap({ view: null, hasProject: true, seatCandidate: true })), null);
  assert.match(unavailable('mgr-close', snap({ view: null, hasProject: true, seatCandidate: true })), /No manager view/);
  assert.equal(unavailable('mgr-refresh', snap({ view: 'fleet', hasProject: true, seatCandidate: true })), null);
  assert.deepEqual(Object.keys(MANAGERS_RULES).sort(), MANAGERS_ACTIONS.map((a) => a.id).sort());
});
