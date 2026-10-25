import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, appMenuModel, menuItems, contextMenuModel, keyHints, paletteEntries, unavailable } from '../dist/registry.js';
import {
  LIFECYCLE_ACTIONS, LIFECYCLE_COMMAND_BINDINGS, LIFECYCLE_KEY_BINDINGS, LIFECYCLE_RULES, isLifecycleAction,
} from '../dist/actions/lifecycle-spec.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  AGGREGATE_MS, EMPTY_NOTICES, MAX_NOTICES, PURGE_CONFIRM_MS, activeNotices, deleteBlock, dismissAll, dismissAt,
  downgradeBlock, lifecycleTarget, listKey, noticeRows, noticeTtl, openQuestion, openTrash, pickOption, previewQuery,
  pushNotice, purgeStep, questionHint, questionKey, questionResponse, questionTarget, recentCompletions, sessionNotices,
  setQuestionText, toastKind, trashGroup, trashMove, trashRemove, trashRows, trashSelected, unarchiveBlock,
} from '../dist/lifecycle-model.js';
import { DECLINE_RESPONSE } from '../dist/approvals-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const NOW = Date.parse('2026-10-18T12:00:00Z');
const ago = (ms) => new Date(NOW - ms).toISOString();
const HOUR = 3_600_000;
const DAY = 24 * HOUR;

const sess = (id, status, over = {}) => ({
  id, status, provider: 'Claude', query: `query ${id}`, working_dir: '/w',
  created_at: ago(DAY), updated_at: ago(HOUR), ...over,
});
const opt = (label) => ({ label, description: `${label} desc` });
const withQuestion = (id, questions) => sess(id, 'Running', { pending_question: { questions } });
const single = { question: 'Which?', header: 'Pick', options: [opt('A'), opt('B'), opt('C')] };
const multi = { question: 'Which ones?', header: 'Many', options: [opt('X'), opt('Y'), opt('Z')], multiSelect: true };
const free = { question: 'Say something', header: 'Free' };

const snap = (over = {}, lifecycle = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null,
  lifecycle: { target: { status: 'Completed' }, questions: 0, notices: 0, completions: 2, ...lifecycle },
  ...over,
});

// ---------- registry ----------

test('every lifecycle action is registered, in the palette and has a rule', () => {
  const ids = LIFECYCLE_ACTIONS.map((a) => a.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const id of ids) {
    assert.ok(ACTIONS.some((a) => a.id === id), id);
    assert.equal(typeof LIFECYCLE_RULES[id], 'function', id);
    assert.ok(isLifecycleAction(id), id);
  }
  const entries = paletteEntries(snap(), 'other');
  for (const id of ids) assert.ok(entries.some((e) => e.id === id), `${id} is in the palette`);
  assert.deepEqual(Object.keys(LIFECYCLE_RULES).sort(), [...ids, 'answer-question'].sort());
  assert.equal(isLifecycleAction('archive'), false);
});

test('lifecycle keys and commands show up as hints from the TUI notation', () => {
  assert.deepEqual(keyHints('delete-session'), ['DD', ':delete']);
  assert.deepEqual(keyHints('unarchive-session'), ['U']);
  assert.deepEqual(keyHints('downgrade-pause'), ['<Space>hs', ':pause soft']);
  assert.deepEqual(keyHints('open-trash'), ['<Space>gX']);
  assert.deepEqual(keyHints('open-notifications'), ['<Space>n', ':alerts']);
  assert.deepEqual(keyHints('open-recent'), ['gr']);
  assert.ok(keyHints('answer-question').includes('<Space>gq'));
});

test('the keys run their actions through the TUI keymap', () => {
  const run = (keys) => {
    const e = new KeyEngine(desktopKeymap());
    let out;
    for (const k of keys) out = e.feed(k === ' ' ? 'Space' : k);
    return out;
  };
  assert.deepEqual(run(['D', 'D']), { kind: 'run', action: 'delete-session', count: null, keys: 'DD' });
  assert.equal(run(['U']).action, 'unarchive-session');
  assert.equal(run([' ', 'h', 's']).action, 'downgrade-pause');
  assert.equal(run([' ', 'g', 'X']).action, 'open-trash');
  assert.equal(run([' ', 'n']).action, 'open-notifications');
  assert.equal(run(['g', 'r']).action, 'open-recent');
  assert.equal(run([' ', 'g', 'q']).action, 'answer-question');
  const cmd = (line) => {
    const e = new KeyEngine(desktopKeymap());
    return e.runCommand(line);
  };
  assert.equal(cmd('delete').action, 'delete-session');
  assert.equal(cmd('del').action, 'delete-session');
  assert.equal(cmd('pause soft').action, 'downgrade-pause');
  assert.equal(cmd('alerts').action, 'open-notifications');
  assert.equal(cmd('att').action, 'open-notifications');
  for (const b of [...Object.values(LIFECYCLE_KEY_BINDINGS), ...Object.values(LIFECYCLE_COMMAND_BINDINGS)]) {
    assert.ok(isLifecycleAction(b.action), b.action);
  }
});

test('the menus list the lifecycle actions and the row menu offers the session ones', () => {
  const menus = appMenuModel('other', snap());
  const ids = (name) => menuItems(menus.find((m) => m.id === name).items).map((n) => n.id);
  assert.ok(ids('session').includes('act:delete-session'));
  assert.ok(ids('session').includes('act:unarchive-session'));
  assert.ok(ids('go').includes('act:open-trash'));
  assert.ok(ids('go').includes('act:open-notifications'));
  assert.ok(ids('go').includes('act:open-recent'));
  const row = contextMenuModel('session-row', snap()).filter((n) => n.kind === 'item').map((n) => n.id);
  for (const id of ['delete-session', 'unarchive-session', 'downgrade-pause']) assert.ok(row.includes(`ctx:${id}`), id);
});

test('availability: delete, unarchive, downgrade, recent and the question panel', () => {
  assert.equal(unavailable('delete-session', snap()), null);
  assert.match(unavailable('delete-session', snap({}, { target: { status: 'Running' } })), /Interrupt it first/);
  assert.match(unavailable('delete-session', snap({}, { target: { status: 'Archived' } })), /unarchive/);
  assert.match(unavailable('delete-session', snap({}, { target: null })), /Open a session/);
  assert.match(unavailable('unarchive-session', snap()), /No archived session/);
  assert.equal(unavailable('unarchive-session', snap({}, { target: { status: 'Archived' } })), null);
  const paused = (level) => snap({ open: { status: 'Interrupted', readOnly: false, pauseLevel: level, hasQuestion: false } });
  assert.equal(unavailable('downgrade-pause', paused('hard')), null);
  assert.match(unavailable('downgrade-pause', paused('soft')), /already SOFT/);
  assert.match(unavailable('downgrade-pause', paused('none')), /no operator pause/);
  assert.match(unavailable('downgrade-pause', snap({ open: null })), /Open a session/);
  assert.equal(unavailable('open-trash', snap({ open: null })), null);
  assert.equal(unavailable('open-notifications', snap()), null);
  assert.match(unavailable('open-recent', snap({}, { completions: 0 })), /No session has finished/);
  // The question panel opens for the open session's question or for any waiting session.
  assert.match(unavailable('answer-question', snap()), /no question/);
  assert.equal(unavailable('answer-question', snap({}, { questions: 2 })), null);
  assert.equal(unavailable('answer-question', snap({ open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: true } })), null);
  assert.match(unavailable('delete-session', snap({ modalOpen: true })), /Close the open dialog/);
});

test('every tauri command the lifecycle adapter invokes is registered in the Rust handler list', () => {
  const ts = readFileSync(here('../src/actions/lifecycle.ts'), 'utf8');
  const lib = readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8');
  const used = [...ts.matchAll(/invoke(?:<[^>]*>)?\(\s*'([a-z_]+)'/g)].map((m) => m[1]);
  assert.deepEqual([...new Set(used)].sort(), ['delete_session', 'list_deleted_sessions', 'purge_session', 'unarchive_session', 'undelete_session']);
  for (const name of used) assert.match(lib, new RegExp(`lifecycle::${name}\\b`), name);
});

// ---------- preconditions ----------

test('delete refuses running, starting, archived and trashed sessions like the TUI', () => {
  assert.match(deleteBlock('Running'), /Interrupt it first/);
  assert.match(deleteBlock('Starting'), /Interrupt it first/);
  assert.match(deleteBlock('Archived'), /archived/);
  assert.match(deleteBlock('Deleted'), /already in the trash/);
  for (const ok of ['Completed', 'Failed', 'Interrupted', 'WaitingApproval']) assert.equal(deleteBlock(ok), null, ok);
});

test('unarchive and downgrade preconditions', () => {
  assert.equal(unarchiveBlock('Archived'), null);
  assert.match(unarchiveBlock('Completed'), /No archived session/);
  assert.match(unarchiveBlock(undefined), /No archived session/);
  assert.equal(downgradeBlock('hard'), null);
  assert.match(downgradeBlock('soft'), /already SOFT/);
  assert.match(downgradeBlock('none'), /no operator pause/);
});

test('the target is the open session, else the list cursor, and the preview is one short line', () => {
  assert.deepEqual(lifecycleTarget({ id: 'a' }, { id: 'b' }), { id: 'a' });
  assert.deepEqual(lifecycleTarget(undefined, { id: 'b' }), { id: 'b' });
  assert.equal(lifecycleTarget(undefined, undefined), undefined);
  assert.equal(previewQuery({ query: 'fix\n  the   build', title: null }), 'fix the build');
  assert.equal(previewQuery({ query: 'q', title: '  My title ' }), 'My title');
  assert.equal(previewQuery({ query: 'x'.repeat(100), title: null }, 10), `${'x'.repeat(10)}…`);
});

// ---------- trash ----------

test('trash groups sessions under the TUI time-period headers', () => {
  assert.equal(trashGroup(ago(HOUR), NOW), 'Past Day');
  assert.equal(trashGroup(ago(30 * HOUR), NOW), 'Past 2 Days');
  assert.equal(trashGroup(ago(50 * HOUR), NOW), 'Past 3 Days');
  assert.equal(trashGroup(ago(5 * DAY), NOW), 'Past Week');
  assert.equal(trashGroup(ago(20 * DAY), NOW), 'Past Month');
  assert.equal(trashGroup(ago(40 * DAY), NOW), 'Older');
  assert.equal(trashGroup('not a date', NOW), 'Older');
  const sessions = [sess('old', 'Deleted', { updated_at: ago(40 * DAY) }), sess('new', 'Deleted', { updated_at: ago(HOUR) }), sess('mid', 'Deleted', { updated_at: ago(5 * DAY) })];
  const st = openTrash(sessions);
  assert.deepEqual(st.sessions.map((s) => s.id), ['new', 'mid', 'old'], 'newest first');
  const rows = trashRows(st.sessions, NOW);
  assert.deepEqual(rows.map((r) => (r.kind === 'header' ? r.title : r.session.id)), ['Past Day', 'new', 'Past Week', 'mid', 'Older', 'old']);
});

test('trash motion clamps and restore or purge keeps the selection on the same row', () => {
  let st = openTrash([sess('a', 'Deleted'), sess('b', 'Deleted', { updated_at: ago(2 * HOUR) }), sess('c', 'Deleted', { updated_at: ago(3 * HOUR) })]);
  assert.equal(trashSelected(st).id, 'a');
  st = trashMove(st, 'prev');
  assert.equal(st.selected, 0);
  st = trashMove(st, 'next');
  st = trashMove(st, 'next');
  st = trashMove(st, 'next');
  assert.equal(trashSelected(st).id, 'c', 'stops at the end');
  st = trashMove(st, 'first');
  assert.equal(st.selected, 0);
  st = trashMove(st, 'last');
  assert.equal(st.selected, 2);
  st = trashRemove(st, 'c');
  assert.equal(trashSelected(st).id, 'b', 'the last row removed: selection clamps up');
  st = trashMove(st, 'first');
  st = trashRemove(st, 'a');
  assert.equal(trashSelected(st).id, 'b', 'the next row takes the place');
  st = trashRemove(st, 'b');
  assert.equal(trashSelected(st), undefined);
  assert.equal(trashMove(st, 'next'), st);
  assert.equal(purgeStep(st, NOW).run, null);
});

test('purge needs a second D on the same session inside the window', () => {
  let st = openTrash([sess('a', 'Deleted'), sess('b', 'Deleted', { updated_at: ago(2 * HOUR) })]);
  let step = purgeStep(st, NOW);
  assert.equal(step.run, null, 'the first D only arms it');
  assert.deepEqual(step.st.purge, { id: 'a', at: NOW });
  st = step.st;
  step = purgeStep(st, NOW + PURGE_CONFIRM_MS - 1);
  assert.equal(step.run, 'a');
  assert.equal(step.st.purge, null);
  // Too late: it arms again instead of purging.
  step = purgeStep(st, NOW + PURGE_CONFIRM_MS);
  assert.equal(step.run, null);
  assert.deepEqual(step.st.purge, { id: 'a', at: NOW + PURGE_CONFIRM_MS });
  // Moving to another row drops the arming, so D then j then D never purges.
  st = trashMove(purgeStep(openTrash([sess('a', 'Deleted'), sess('b', 'Deleted', { updated_at: ago(2 * HOUR) })]), NOW).st, 'next');
  assert.equal(st.purge, null);
  assert.equal(purgeStep(st, NOW + 1).run, null);
});

// ---------- notifications ----------

test('notices are active until dismissed or past their TTL, newest first, then history', () => {
  let s = EMPTY_NOTICES;
  s = pushNotice(s, { kind: 'info', message: 'older' }, NOW);
  s = pushNotice(s, { kind: 'error', message: 'newer', sessionId: 'x' }, NOW + 1000);
  assert.deepEqual(noticeRows(s, NOW + 2000).map((n) => n.message), ['newer', 'older']);
  assert.equal(activeNotices(s, NOW + 2000), 2);
  assert.equal(noticeTtl('error'), 10_000);
  assert.equal(noticeTtl('info'), 5_000);
  // `older` (5 s) has expired by NOW + 6 s; `newer` (error, 10 s) has not.
  assert.equal(activeNotices(s, NOW + 6000), 1);
  assert.deepEqual(noticeRows(s, NOW + 6000).map((n) => n.message), ['newer', 'older']);
  assert.equal(activeNotices(s, NOW + 20_000), 0);
  assert.deepEqual(noticeRows(s, NOW + 20_000).map((n) => n.message), ['newer', 'older'], 'history keeps the later expiry first');
});

test('x dismisses the selected active notice into the top of the history; history rows stay put', () => {
  let s = EMPTY_NOTICES;
  s = pushNotice(s, { kind: 'info', message: 'one', sessionId: 'a' }, NOW);
  s = pushNotice(s, { kind: 'success', message: 'two', sessionId: 'b' }, NOW + 1000);
  const t = NOW + 2000;
  s = dismissAt(s, 1, t);
  assert.deepEqual(noticeRows(s, t).map((n) => n.message), ['two', 'one']);
  assert.equal(activeNotices(s, t), 1);
  assert.equal(dismissAt(s, 1, t), s, 'a history row cannot be dismissed again');
  assert.equal(dismissAt(s, 9, t), s, 'out of range is a no-op');
  const all = dismissAll(s, t + 1);
  assert.equal(activeNotices(all, t + 1), 0);
  assert.deepEqual(noticeRows(all, t + 1).map((n) => n.message), ['two', 'one']);
});

test('a burst of the same notice on one session folds into a counted line', () => {
  let s = EMPTY_NOTICES;
  s = pushNotice(s, { kind: 'completed', message: 'Completed: A', sessionId: 'a' }, NOW);
  s = pushNotice(s, { kind: 'completed', message: 'Completed: A', sessionId: 'a' }, NOW + AGGREGATE_MS - 1);
  s = pushNotice(s, { kind: 'completed', message: 'Completed: A', sessionId: 'a' }, NOW + AGGREGATE_MS + 100);
  assert.deepEqual(s.items.map((n) => n.message), ['(3) Completed: A']);
  s = pushNotice(s, { kind: 'completed', message: 'Completed: B', sessionId: 'b' }, NOW + AGGREGATE_MS + 150);
  assert.equal(s.items.length, 2, 'another session is its own notice');
});

test('the store keeps at most MAX_NOTICES, dropping the oldest', () => {
  let s = EMPTY_NOTICES;
  for (let i = 0; i < MAX_NOTICES + 25; i++) s = pushNotice(s, { kind: 'info', message: `m${i}` }, NOW + i * 1000);
  assert.equal(s.items.length, MAX_NOTICES);
  assert.equal(s.items[0].message, 'm25');
});

test('toasts become info, success or error notices', () => {
  assert.equal(toastKind('Sent', true), 'success');
  assert.equal(toastKind('Delete failed: nope', false), 'error');
  assert.equal(toastKind('Model switch unavailable: x', false), 'error');
  assert.equal(toastKind('Open a session first', false), 'info');
});

test('session transitions raise completed, failed, interrupted and waiting notices', () => {
  const first = sessionNotices(new Map(), [sess('a', 'Running'), sess('b', 'Completed')]);
  assert.deepEqual(first.notices, [], 'the first look records nothing');
  const waiting = withQuestion('c', [single]);
  const second = sessionNotices(first.seen, [sess('a', 'Completed'), sess('b', 'Completed'), waiting]);
  assert.deepEqual(second.notices.map((n) => [n.kind, n.sessionId]), [['completed', 'a']], 'a new session raises nothing the first time');
  const third = sessionNotices(second.seen, [sess('a', 'Completed'), sess('b', 'Completed'), { ...waiting, status: 'Running' }, sess('d', 'Running')]);
  assert.deepEqual(third.notices, [], 'no change, nothing raised');
  const quiet = sess('e', 'Running');
  const base = sessionNotices(new Map(), [quiet, sess('f', 'Running'), sess('g', 'Running'), sess('h', 'Completed')]);
  const next = sessionNotices(base.seen, [
    withQuestion('e', [single]), sess('f', 'Failed'), sess('g', 'Interrupted'), sess('h', 'Completed'),
  ]);
  assert.deepEqual(next.notices.map((n) => [n.kind, n.sessionId]).sort(), [['attention', 'e'], ['failed', 'f'], ['info', 'g']]);
  // The same question is not announced again on the next poll.
  const again = sessionNotices(next.seen, [withQuestion('e', [single]), sess('f', 'Failed'), sess('g', 'Interrupted'), sess('h', 'Completed')]);
  assert.deepEqual(again.notices, []);
});

// ---------- recent completions and lists ----------

test('recent completions are the six latest finished sessions, newest first', () => {
  const all = [
    sess('run', 'Running'), sess('arch', 'Archived'),
    ...Array.from({ length: 8 }, (_, i) => sess(`c${i}`, i % 3 === 0 ? 'Failed' : i % 3 === 1 ? 'Interrupted' : 'Completed', { updated_at: ago((i + 1) * HOUR) })),
  ];
  const r = recentCompletions(all);
  assert.deepEqual(r.map((s) => s.id), ['c0', 'c1', 'c2', 'c3', 'c4', 'c5']);
  assert.deepEqual(recentCompletions([sess('a', 'Running')]), []);
});

test('list keys move j/k/g/G and clamp', () => {
  assert.equal(listKey(0, 3, 'j'), 1);
  assert.equal(listKey(2, 3, 'j'), 2);
  assert.equal(listKey(0, 3, 'k'), 0);
  assert.equal(listKey(2, 3, 'ArrowUp'), 1);
  assert.equal(listKey(1, 3, 'g'), 0);
  assert.equal(listKey(0, 3, 'G'), 2);
  assert.equal(listKey(0, 3, 'x'), null);
  assert.equal(listKey(0, 0, 'j'), null);
});

// ---------- question modal ----------

const key = (k, mods = {}) => ({ key: k, ...mods });
const press = (st, ...keys) => keys.reduce((s, k) => questionKey(s, typeof k === 'string' ? key(k) : k).st, st);

test('the question modal opens on the open session, else the first waiting one', () => {
  const a = withQuestion('a', [single]);
  const b = withQuestion('b', [multi]);
  const idle = sess('i', 'Running');
  assert.equal(questionTarget([idle, a, b], 'b').id, 'b');
  assert.equal(questionTarget([idle, a, b], 'i').id, 'a');
  assert.equal(questionTarget([idle, a, b], null).id, 'a');
  assert.equal(questionTarget([idle], 'i'), undefined);
  assert.equal(questionTarget([sess('w', 'WaitingApproval')], 'w'), undefined, 'a bare approval has no question to answer');
});

test('single-select: j/k wrap and commit as the cursor lands, digits and Space pick', () => {
  let st = openQuestion([single]);
  assert.equal(st.mode, 'normal');
  st = press(st, 'j');
  assert.deepEqual([st.cursor[0], st.chosen[0]], [1, [1]]);
  st = press(st, 'j', 'j');
  assert.deepEqual([st.cursor[0], st.chosen[0]], [0, [0]], 'wraps past the last option');
  st = press(st, 'k');
  assert.deepEqual([st.cursor[0], st.chosen[0]], [2, [2]], 'wraps back');
  st = press(st, '2');
  assert.deepEqual([st.cursor[0], st.chosen[0]], [1, [1]]);
  st = press(st, '9');
  assert.deepEqual(st.chosen[0], [1], 'a number past the options is ignored');
  st = press(st, 'ArrowUp', ' ');
  assert.deepEqual(st.chosen[0], [0]);
  assert.equal(questionResponse(st).text, 'A');
});

test('multi-select: j/k only move; Space and digits toggle; the answer joins the labels in order', () => {
  let st = openQuestion([multi]);
  st = press(st, 'j');
  assert.deepEqual([st.cursor[0], st.chosen[0]], [1, []], 'moving does not choose');
  st = press(st, ' ', '3');
  assert.deepEqual(st.chosen[0], [1, 2]);
  st = press(st, '1', '3');
  assert.deepEqual(st.chosen[0], [1, 0]);
  assert.equal(questionResponse(st).text, 'X, Y', 'labels in option order');
});

test('Enter and Backspace move between questions and a multi-question answer lists them', () => {
  let st = openQuestion([single, multi]);
  st = press(st, '1', 'Enter');
  assert.equal(st.current, 1);
  st = press(st, 'Enter');
  assert.equal(st.current, 1, 'no question past the last');
  st = press(st, ' ');
  st = press(st, 'Backspace');
  assert.equal(st.current, 0);
  st = press(st, 'Backspace');
  assert.equal(st.current, 0);
  st = press(st, 'Enter');
  assert.deepEqual(questionResponse(st), { ok: true, text: 'Q1 (Pick): A\nQ2 (Many): X' });
});

test('an unanswered question is named and nothing is sent', () => {
  const st = press(openQuestion([single, multi]), '1');
  assert.deepEqual(questionResponse(st), { ok: false, error: 'Please answer question 2' });
});

test('i opens the free-text box, which replaces the chosen options; Esc goes back; typing clears the picks', () => {
  let st = press(openQuestion([single]), '2');
  assert.deepEqual(st.chosen[0], [1]);
  st = press(st, 'i');
  assert.equal(st.mode, 'insert');
  assert.deepEqual(st.chosen[0], [], 'the options are cleared when the box opens');
  st = setQuestionText(st, 'my own answer');
  assert.equal(questionResponse(st).text, 'my own answer');
  const back = questionKey(st, key('Escape'));
  assert.deepEqual([back.st.mode, back.effect, back.consumed], ['normal', null, true]);
  // Picking an option afterwards empties the text again.
  const picked = pickOption(back.st, 0);
  assert.equal(picked.text[0], '');
  assert.equal(questionResponse(picked).text, 'A');
  // Typing over a chosen option drops the choice.
  const typed = setQuestionText(pickOption(openQuestion([single]), 1), 'x');
  assert.deepEqual(typed.chosen[0], []);
});

test('insert mode: Enter and Ctrl/Cmd-Enter submit, Shift-Enter and letters go to the box', () => {
  const st = press(openQuestion([single]), 'i');
  assert.equal(questionKey(st, key('Enter')).effect, 'submit');
  assert.equal(questionKey(st, key('Enter', { shift: true })).consumed, false, 'Shift-Enter is a newline');
  assert.equal(questionKey(st, key('a')).consumed, false);
  assert.equal(questionKey(st, key('d')).effect, null, 'd in the box is a letter, not decline');
  assert.equal(questionKey(st, key('Enter', { ctrl: true })).effect, 'submit');
  assert.equal(questionKey(st, key('Enter', { meta: true })).effect, 'submit');
});

test('normal mode: Ctrl-Enter submits, d lets the agent decide, Esc and q hide, other keys are left alone', () => {
  const st = openQuestion([single]);
  assert.equal(questionKey(st, key('Enter', { ctrl: true })).effect, 'submit');
  assert.equal(questionKey(st, key('d')).effect, 'decline');
  assert.equal(questionKey(st, key('Escape')).effect, 'hide');
  assert.equal(questionKey(st, key('q')).effect, 'hide');
  assert.equal(questionKey(st, key('x')).consumed, false);
  assert.equal(questionKey(st, key('j', { ctrl: true })).consumed, false, 'Ctrl-J is not j');
  assert.equal(questionKey(st, key('0')).consumed, false, '0 picks nothing');
  assert.equal(DECLINE_RESPONSE, '(declined — no preference; use your best judgment)');
});

test('a question without options starts in the free-text box and Esc hides it', () => {
  let st = openQuestion([free]);
  assert.equal(st.mode, 'insert');
  assert.equal(questionKey(st, key('Escape')).effect, 'hide');
  st = setQuestionText(st, '  hello  ');
  assert.equal(questionResponse(st).text, 'hello');
  // Moving onto a question with options lands in normal mode, and back to one without goes to the box.
  let two = openQuestion([single, free]);
  two = press(two, '1', 'Enter');
  assert.equal(two.mode, 'insert');
  two = press({ ...two, mode: 'normal' }, 'Backspace');
  assert.equal(two.mode, 'normal');
  assert.match(questionHint(two), /Ctrl\/Cmd-Enter/);
  assert.match(questionHint({ ...two, mode: 'insert' }), /Shift-Enter/);
});
