import test from 'node:test';
import assert from 'node:assert/strict';
import { createPoller, createViewState, enterDetail, groupSessions, returnToList } from '../state.js';

const now = Date.parse('2026-09-26T12:00:00Z');
const row = (id, status, daysAgo = 0) => ({ id, title: id, status, provider: 'Codex', updatedAt: new Date(now - daysAgo * 86400000).toISOString() });

test('session groups preserve attention, in-flight, recent and quiet order', () => {
  const groups = groupSessions([
    row('quiet', 'Completed', 20), row('running', 'Running'), row('failed', 'Failed'),
    row('recent', 'Completed', 2), row('waiting', 'WaitingApproval'),
  ], now);
  assert.deepEqual(groups.map((group) => group.name), ['NEEDS YOU', 'IN FLIGHT', 'RECENT', 'QUIET']);
  assert.deepEqual(groups.map((group) => group.sessions.map((item) => item.id)), [['failed', 'waiting'], ['running'], ['recent'], ['quiet']]);
});

test('Back restores the selected identity and saved list offset', () => {
  const initial = createViewState();
  const detail = enterDetail(initial, 's-7', 428);
  const list = returnToList(detail);
  assert.equal(list.screen, 'list');
  assert.equal(list.selectedId, 's-7');
  assert.equal(list.listScrollTop, 428);
});

test('a needsYou session joins the needs-you group without a status change', () => {
  const groups = groupSessions([row('running', 'Running'), { ...row('flagged', 'Running'), needsYou: true }], now);
  assert.deepEqual(groups.map((group) => group.name), ['NEEDS YOU', 'IN FLIGHT']);
  assert.deepEqual(groups[0].sessions.map((session) => session.id), ['flagged']);
  assert.deepEqual(groups[1].sessions.map((session) => session.id), ['running']);
});

const flush = async () => { for (let i = 0; i < 25; i += 1) await Promise.resolve(); };

function createFakeClock() {
  let now = 0;
  let nextId = 0;
  const timers = new Map();
  return {
    get now() { return now; },
    setTimer(fn, delay) { const id = (nextId += 1); timers.set(id, { fn, at: now + delay }); return id; },
    clearTimer(id) { timers.delete(id); },
    pending: () => [...timers.values()],
    async advance(ms) {
      now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now) { timers.delete(id); timer.fn(); }
      }
      await flush();
    },
  };
}

function pollerOptions(clock, overrides) {
  return {
    run: async () => {},
    intervalMs: 1000,
    maxIntervalMs: 8000,
    setTimer: clock.setTimer,
    clearTimer: clock.clearTimer,
    isVisible: () => true,
    ...overrides,
  };
}

test('a successful run reschedules at the base interval', async () => {
  const clock = createFakeClock();
  let runs = 0;
  const poller = createPoller(pollerOptions(clock, { run: async () => { runs += 1; } }));
  poller.start();
  assert.equal(runs, 1);
  await flush();
  assert.equal(clock.pending().length, 1);
  assert.equal(clock.pending()[0].at, 1000);
  await clock.advance(1000);
  assert.equal(runs, 2);
  assert.equal(poller.currentInterval(), 1000);
  poller.stop();
});

test('failures back off 2x up to the max and a success resets the interval', async () => {
  const clock = createFakeClock();
  const outcomes = [false, false, false, false, true];
  let runs = 0;
  const poller = createPoller(pollerOptions(clock, {
    intervalMs: 1000,
    maxIntervalMs: 4000,
    run: async () => {
      const ok = outcomes[runs];
      runs += 1;
      if (!ok) throw new Error('boom');
    },
  }));
  poller.start();
  await flush();
  assert.equal(runs, 1);
  assert.equal(poller.currentInterval(), 2000);
  await clock.advance(2000);
  assert.equal(runs, 2);
  assert.equal(poller.currentInterval(), 4000);
  await clock.advance(4000);
  assert.equal(runs, 3);
  assert.equal(poller.currentInterval(), 4000);
  await clock.advance(4000);
  assert.equal(runs, 4);
  await clock.advance(4000);
  assert.equal(runs, 5);
  assert.equal(poller.currentInterval(), 1000);
  assert.equal(clock.pending()[0].at - clock.now, 1000);
  poller.stop();
});

test('kick during an in-flight run does not start a second run', async () => {
  const clock = createFakeClock();
  let runs = 0;
  let release;
  const poller = createPoller(pollerOptions(clock, {
    run: () => { runs += 1; return new Promise((resolve) => { release = resolve; }); },
  }));
  poller.start();
  assert.equal(runs, 1);
  poller.kick();
  poller.kick();
  assert.equal(runs, 1);
  release();
  await flush();
  assert.equal(runs, 1);
  assert.equal(clock.pending().length, 1);
  poller.stop();
});

test('a hidden page schedules nothing and becoming visible runs immediately', async () => {
  const clock = createFakeClock();
  let visible = false;
  let runs = 0;
  const poller = createPoller(pollerOptions(clock, {
    isVisible: () => visible,
    run: async () => { runs += 1; },
  }));
  poller.start();
  await flush();
  assert.equal(runs, 0);
  assert.equal(clock.pending().length, 0);
  visible = true;
  poller.onVisibilityChange();
  assert.equal(runs, 1);
  await flush();
  assert.equal(clock.pending().length, 1);
  visible = false;
  poller.onVisibilityChange();
  assert.equal(clock.pending().length, 0);
  poller.stop();
});

test('stop clears the pending timer', async () => {
  const clock = createFakeClock();
  const poller = createPoller(pollerOptions(clock));
  poller.start();
  await flush();
  assert.equal(clock.pending().length, 1);
  poller.stop();
  assert.equal(clock.pending().length, 0);
});
