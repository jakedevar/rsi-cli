// The session header in plain words and the stop / pause controls (local #212).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  PAUSE_BUTTON, RESUME_BUTTON, STOP_BUTTON, hardPauseConfirm, hardPauseNeedsConfirm,
  headDetails, headMeta, runHardPause, shortSessionId,
} from '../dist/session-head-model.js';
import { ACTIONS, tooltipText } from '../dist/registry.js';
import { CORE_KEY_BINDINGS } from '../dist/keybindings.js';
import { pauseLabel } from '../dist/approvals-model.js';

const ID = '0f9e8d7c-1111-2222-3333-444455556666';
const session = (o = {}) => ({
  id: ID, status: 'WaitingApproval', provider: 'Claude', model: 'opus', query: 'fix the build',
  working_dir: '/Users/dev/code/rsi', git_branch: 'mac-mini/212', title: 'Fix the build', agent_role: null, ...o,
});

test('the header leads with the display name, then the status in plain words', () => {
  const m = headMeta(session(), 'Rsi');
  assert.equal(m.name, 'Fix the build');
  assert.equal(m.status, 'Waiting for you');
  assert.deepEqual(m.parts, ['Claude', 'opus', 'Rsi']);
  assert.equal(headMeta(session({ agent_role: 'Reviewer', epic_spawn_ordinal: 2 }), undefined).name, 'Reviewer #2 · Fix the build');
  assert.deepEqual(headMeta(session({ model: null }), undefined).parts, ['Claude', 'default model']);
});

test('id, folder and branch are not in the primary meta; they stay copyable in the details', () => {
  const m = headMeta(session(), 'Rsi');
  const primary = [m.name, m.status, ...m.parts].join(' ');
  for (const dev of [ID, '/Users/dev/code/rsi', 'mac-mini/212']) assert.ok(!primary.includes(dev), dev);
  assert.equal(shortSessionId(ID), '0f9e8d7c');
  assert.deepEqual(headDetails(session()).map((d) => [d.label, d.value]), [
    ['Session ID', ID], ['Folder', '/Users/dev/code/rsi'], ['Branch', 'mac-mini/212'],
  ]);
  assert.deepEqual(headDetails(session({ git_branch: null })).map((d) => d.label), ['Session ID', 'Folder']);
});

test('the header markup carries the secondary row and the renamed buttons, with the same action ids', () => {
  const html = readFileSync(fileURLToPath(new URL('../static/index.html', import.meta.url)), 'utf8');
  assert.match(html, /id="s-secondary"/);
  assert.match(html, /data-action="soft-interrupt">Stop this turn</);
  assert.match(html, /data-action="hard-pause">Pause until I resume</);
  assert.match(html, /data-action="clear-pause">Resume</);
  assert.match(html, /id="interrupt" class="btn" /, 'only the pause is styled danger');
  assert.match(html, /id="hard-pause" class="btn danger"/);
});

test('tooltips say in plain words what each control does and how they differ', () => {
  const stop = tooltipText('soft-interrupt', 'other');
  const pause = tooltipText('hard-pause', 'other');
  const resume = tooltipText('clear-pause', 'other');
  assert.equal(stop, `Stop this turn (x, :kill)\n${STOP_BUTTON.help}`);
  assert.match(pause, /^Pause until I resume \(X\)\n/);
  assert.match(stop, /Stops what the agent is doing right now/);
  assert.match(pause, /starts nothing new, not even by a manager, until you choose Resume/);
  assert.match(pause, /Stop this turn only interrupts once/);
  assert.match(resume, /Lifts the pause/);
  for (const t of [stop, pause, resume]) assert.ok(!/\bhard pause\b|\bsoft pause\b/i.test(t), 'no TUI jargon');
  assert.equal(ACTIONS.filter((a) => ['soft-interrupt', 'hard-pause', 'clear-pause'].includes(a.id)).length, 3);
  assert.equal(PAUSE_BUTTON.label, 'Pause until I resume');
  assert.equal(RESUME_BUTTON.label, 'Resume');
});

test('the pause chip is plain', () => {
  assert.equal(pauseLabel('hard'), 'Paused until you resume');
  assert.equal(pauseLabel('none'), '');
});

test('the confirm names the session and says what it does', () => {
  assert.equal(
    hardPauseConfirm('Fix the build'),
    'Pause “Fix the build”? It stops the current turn and starts nothing new until you choose Resume.',
  );
});

test('Pause until I resume: Cancel sends nothing, Confirm sends the one RPC', async () => {
  let sent = 0;
  let asked = '';
  const send = async () => { sent += 1; };
  const cancel = await runHardPause({ name: 'S', confirmFirst: true, confirm: async (m) => { asked = m; return false; }, send });
  assert.equal(cancel, 'cancelled');
  assert.equal(sent, 0);
  assert.equal(asked, hardPauseConfirm('S'));
  const ok = await runHardPause({ name: 'S', confirmFirst: true, confirm: async () => true, send });
  assert.equal(ok, 'sent');
  assert.equal(sent, 1);
});

test('a failed send surfaces to the caller', async () => {
  await assert.rejects(runHardPause({
    name: 'S', confirmFirst: false, confirm: async () => assert.fail('no ask'), send: async () => { throw new Error('daemon error'); },
  }), /daemon error/);
});

test('vim keys on: X (pressed twice) already confirmed and sends at once; off: only buttons, menus and the palette run it, and they ask', async () => {
  assert.equal(CORE_KEY_BINDINGS.X.action, 'hard-pause');
  assert.equal(CORE_KEY_BINDINGS.X.confirm, 'twice');
  assert.equal(hardPauseNeedsConfirm('key'), false);
  for (const source of ['button', 'menu', 'context', 'palette']) assert.equal(hardPauseNeedsConfirm(source), true, source);
  let asked = 0;
  let sent = 0;
  const run = (source) => runHardPause({
    name: 'S', confirmFirst: hardPauseNeedsConfirm(source), confirm: async () => { asked += 1; return true; }, send: async () => { sent += 1; },
  });
  await run('key');
  assert.deepEqual([asked, sent], [0, 1]);
  await run('button');
  assert.deepEqual([asked, sent], [1, 2]);
});
