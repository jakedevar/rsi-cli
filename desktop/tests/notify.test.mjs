// Local #294: when the desktop posts an OS notification, and what the Settings rows and the Rust command see.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  DEFAULT_NOTIFY_PREFS, FRESH_TRACKER, MAX_SEPARATE, createNotifier, needsBody, parseNotifyPrefs, planNotices, serializeNotifyPrefs,
} from '../dist/notify-model.js';
import { sectionRows, valueTooltip } from '../dist/settings-model.js';
import { createStubBridge } from '../scripts/smoke-stub-bridge.mjs';

const ASK = { questions: [{ header: 'Pick', question: 'Fake worker asks a question', options: [] }] };
const ASK2 = { questions: [{ header: 'Pick', question: 'And now another one?', options: [] }] };
let n = 0;
const ids = new Map();
const uuid = (name) => {
  if (!ids.has(name)) ids.set(name, `00000000-0000-4000-8000-${String(++n).padStart(12, '0')}`);
  return ids.get(name);
};
const sess = (name, status, extra = {}) => ({ id: uuid(name), status, title: name, query: name, provider: 'Claude', working_dir: '/w', ...extra });
const ON = { needsMe: true, finished: false };
const away = { prefs: ON, focused: false, openId: undefined };

test('the defaults are needs-me on, finished off, and a bad stored value falls back to them', () => {
  assert.deepEqual(DEFAULT_NOTIFY_PREFS, { needsMe: true, finished: false });
  assert.deepEqual(parseNotifyPrefs(null), DEFAULT_NOTIFY_PREFS);
  assert.deepEqual(parseNotifyPrefs('not json'), DEFAULT_NOTIFY_PREFS);
  assert.deepEqual(parseNotifyPrefs('{"needsMe":"yes"}'), DEFAULT_NOTIFY_PREFS);
  assert.deepEqual(parseNotifyPrefs(serializeNotifyPrefs({ needsMe: false, finished: true })), { needsMe: false, finished: true });
});

test('a session already waiting at launch posts nothing; one that starts waiting later posts once', () => {
  const w = sess('Fake worker', 'WaitingApproval', { pending_question: ASK });
  const first = planNotices(FRESH_TRACKER, [w], away);
  assert.deepEqual(first.notices, [], 'seeding only');
  const again = planNotices(first.tracker, [w], away);
  assert.deepEqual(again.notices, []);
  const run = sess('Other', 'Running');
  const t2 = planNotices(again.tracker, [w, run], away).tracker;
  const r = planNotices(t2, [w, sess('Other', 'WaitingApproval', { pending_question: ASK })], away);
  assert.equal(r.notices.length, 1);
  assert.deepEqual(r.notices[0], { sessionId: uuid('Other'), kind: 'needs', title: 'Other', body: 'Fake worker asks a question' });
  assert.deepEqual(planNotices(r.tracker, [w, sess('Other', 'WaitingApproval', { pending_question: ASK })], away).notices, [], 'the same wait never repeats');
});

test('an approval with no question says it wants an approval; the body is one short line', () => {
  assert.equal(needsBody({ status: 'WaitingApproval' }), 'Waiting for your approval');
  const long = { status: 'Running', pending_question: { questions: [{ header: '', question: `line one\n${'x'.repeat(300)}`, options: [] }] } };
  const body = needsBody(long);
  assert.ok(body.length <= 140 && !body.includes('\n') && body.endsWith('…'), body);
});

test('a refetch that drops and returns a waiting session, or a reconnect, does not repeat it', () => {
  const run = sess('A', 'Running');
  const wait = sess('A', 'WaitingApproval', { pending_question: ASK });
  let t = planNotices(FRESH_TRACKER, [run], away).tracker;
  const r1 = planNotices(t, [wait], away);
  assert.equal(r1.notices.length, 1);
  t = planNotices(r1.tracker, [], away).tracker; // the list came back empty for a moment
  assert.deepEqual(planNotices(t, [wait], away).notices, [], 'back in the list with the same request');
});

test('answered and then asking something else posts again', () => {
  const t0 = planNotices(FRESH_TRACKER, [sess('A', 'Running')], away).tracker;
  const r1 = planNotices(t0, [sess('A', 'WaitingApproval', { pending_question: ASK })], away);
  const r2 = planNotices(r1.tracker, [sess('A', 'Running')], away);
  const r3 = planNotices(r2.tracker, [sess('A', 'WaitingApproval', { pending_question: ASK2 })], away);
  assert.equal(r3.notices.length, 1);
  assert.equal(r3.notices[0].body, 'And now another one?');
});

test('no notification while the window is in front and that session is open; another session still notifies', () => {
  const t0 = planNotices(FRESH_TRACKER, [sess('A', 'Running'), sess('B', 'Running')], away).tracker;
  const wait = [sess('A', 'WaitingApproval', { pending_question: ASK }), sess('B', 'WaitingApproval', { pending_question: ASK })];
  const r = planNotices(t0, wait, { prefs: ON, focused: true, openId: uuid('A') });
  assert.deepEqual(r.notices.map((x) => x.sessionId), [uuid('B')]);
  // The window loses focus later: the wait that was watched is not announced after the fact.
  assert.deepEqual(planNotices(r.tracker, wait, away).notices, []);
  // Open session but the window is behind: notify.
  const t1 = planNotices(FRESH_TRACKER, [sess('A', 'Running')], away).tracker;
  assert.equal(planNotices(t1, [wait[0]], { prefs: ON, focused: false, openId: uuid('A') }).notices.length, 1);
});

test('the needs-me switch off posts nothing, and turning it on later does not announce old waits', () => {
  const t0 = planNotices(FRESH_TRACKER, [sess('A', 'Running')], away).tracker;
  const wait = [sess('A', 'WaitingApproval', { pending_question: ASK })];
  const off = planNotices(t0, wait, { prefs: { needsMe: false, finished: false }, focused: false, openId: undefined });
  assert.deepEqual(off.notices, []);
  assert.deepEqual(planNotices(off.tracker, wait, away).notices, []);
});

test('finished notifications are off by default and cover Completed and Failed of sessions seen alive', () => {
  const t0 = planNotices(FRESH_TRACKER, [sess('A', 'Running'), sess('B', 'Running'), sess('C', 'Running'), sess('D', 'Completed')], away).tracker;
  const end = [sess('A', 'Completed'), sess('B', 'Failed'), sess('C', 'Archived'), sess('D', 'Completed')];
  assert.deepEqual(planNotices(t0, end, away).notices, [], 'default off');
  const r = planNotices(t0, end, { prefs: { needsMe: true, finished: true }, focused: false, openId: undefined });
  assert.deepEqual(r.notices.map((x) => [x.title, x.kind, x.body]), [['A', 'finished', 'Finished'], ['B', 'finished', 'Stopped with an error']]);
  assert.deepEqual(planNotices(r.tracker, end, { prefs: { needsMe: true, finished: true }, focused: false, openId: undefined }).notices, [], 'once');
});

test('a burst of waiting sessions becomes one notification that opens the first', () => {
  const names = ['A', 'B', 'C', 'D', 'E'];
  const t0 = planNotices(FRESH_TRACKER, names.map((x) => sess(x, 'Running')), away).tracker;
  const r = planNotices(t0, names.map((x) => sess(x, 'WaitingApproval', { pending_question: ASK })), away);
  assert.equal(r.notices.length, 1);
  assert.equal(r.notices[0].title, '5 sessions need you');
  assert.equal(r.notices[0].sessionId, uuid('A'));
  assert.ok(MAX_SEPARATE >= 1);
});

test('through the stub bridge: a wait while unfocused calls notify_session once, and a second refresh adds none', async () => {
  const calls = [];
  // notify_session is a shim-local command (it touches no daemon); the stub records what the page asked the OS to show.
  const bridge = createStubBridge({ call: async () => ({}), local: { notify_session: (a) => { calls.push(a); return null; } } });
  let focused = false;
  const notifier = createNotifier({
    prefs: () => ON,
    focused: () => focused,
    post: (x) => bridge.invoke('notify_session', { sessionId: x.sessionId, title: x.title, body: x.body }),
  });
  const run = sess('Fake worker', 'Running');
  const wait = sess('Fake worker', 'WaitingApproval', { pending_question: ASK });
  notifier.update([run], undefined);
  assert.equal(calls.length, 0);
  notifier.update([wait], undefined);
  notifier.update([wait], undefined);
  await new Promise((r) => setImmediate(r));
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0], { sessionId: uuid('Fake worker'), title: 'Fake worker', body: 'Fake worker asks a question' });
  // Focused with that session open: nothing for a new wait.
  focused = true;
  notifier.update([sess('Quiet', 'Running')], uuid('Quiet'));
  notifier.update([sess('Quiet', 'WaitingApproval', { pending_question: ASK })], uuid('Quiet'));
  await new Promise((r) => setImmediate(r));
  assert.equal(calls.length, 1);
  assert.equal(bridge.writes().length, 0, 'no daemon write');
});

test('a failed post is reported and does not stop later ones', async () => {
  const seen = [];
  const notifier = createNotifier({
    prefs: () => ON, focused: () => false,
    post: async () => { throw new Error('denied'); },
    failed: (e) => seen.push(String(e.message)),
  });
  notifier.update([sess('A', 'Running')], undefined);
  notifier.update([sess('A', 'WaitingApproval')], undefined);
  await new Promise((r) => setImmediate(r));
  assert.deepEqual(seen, ['denied']);
});

test('Settings has the two switches and the test button in Workspace › Session List, after the daemon rows', () => {
  const rows = sectionRows('session_list', {});
  const tail = rows.slice(-3);
  assert.deepEqual(tail.map((r) => r.id), ['desktop_notify_needs', 'desktop_notify_finished', 'desktop_notify_test']);
  assert.deepEqual(tail.map((r) => r.widget.local), ['notify-needs', 'notify-finished', 'notify-test']);
  assert.match(tail[0].summary, /System Settings › Notifications/);
  assert.equal(valueTooltip(tail[0], false), 'Click to turn on or off');
  assert.equal(valueTooltip(tail[2], false), 'Click to send a test notification');
});

test('the webview gets no notification permission, and the badge follows the attention count', () => {
  const root = join(fileURLToPath(new URL('.', import.meta.url)), '..');
  const caps = readFileSync(join(root, 'src-tauri/capabilities/default.json'), 'utf8');
  assert.doesNotMatch(caps, /notification/);
  const approvals = readFileSync(join(root, 'src-tauri/src/approvals.rs'), 'utf8');
  assert.match(approvals, /set_badge_count\(badge_count\(count\)\)/);
  const lib = readFileSync(join(root, 'src-tauri/src/lib.rs'), 'utf8');
  assert.match(lib, /tauri_plugin_notification::init\(\)/);
});
