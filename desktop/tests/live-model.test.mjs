import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  classifyEvent, mergeRefresh, parseLink, parseLiveEvent, pollIntervals, FAST_POLL, SLOW_POLL,
} from '../dist/live-model.js';

const A = 'aaaaaaaa-0000-0000-0000-000000000001';
const B = 'bbbbbbbb-0000-0000-0000-000000000002';

test('conversation events refresh only the selected conversation', () => {
  assert.deepEqual(classifyEvent({ event_type: 'conversation_event', session_id: A }, A),
    { sessions: false, conversation: true });
  assert.deepEqual(classifyEvent({ event_type: 'conversation_event', session_id: B }, A),
    { sessions: false, conversation: false });
  assert.deepEqual(classifyEvent({ event_type: 'conversation_event', session_id: A }, null),
    { sessions: false, conversation: false });
});

test('session events refresh the list, and the conversation when selected', () => {
  assert.deepEqual(classifyEvent({ event_type: 'session_status_changed', session_id: A }, A),
    { sessions: true, conversation: true });
  assert.deepEqual(classifyEvent({ event_type: 'session_status_changed', session_id: B }, A),
    { sessions: true, conversation: false });
  assert.deepEqual(classifyEvent({ event_type: 'session_created', session_id: B }, null),
    { sessions: true, conversation: false });
});

test('resync events refresh everything; unrelated events nothing', () => {
  assert.deepEqual(classifyEvent({ event_type: 'subscription_reset' }, A), { sessions: true, conversation: true });
  assert.deepEqual(classifyEvent({ event_type: 'oversize' }, null), { sessions: true, conversation: false });
  assert.deepEqual(classifyEvent({ event_type: 'dream_started' }, A), { sessions: false, conversation: false });
});

test('mergeRefresh ORs; poll intervals slow when linked, fast when down', () => {
  assert.deepEqual(mergeRefresh({ sessions: true, conversation: false }, { sessions: false, conversation: true }),
    { sessions: true, conversation: true });
  assert.equal(pollIntervals(true), SLOW_POLL);
  assert.equal(pollIntervals(false), FAST_POLL);
  assert.ok(SLOW_POLL.sessionsMs > FAST_POLL.sessionsMs && SLOW_POLL.eventsMs > FAST_POLL.eventsMs);
});

test('payload parsers reject malformed input', () => {
  assert.deepEqual(parseLiveEvent({ event_type: 'x', session_id: A }), { event_type: 'x', session_id: A });
  assert.deepEqual(parseLiveEvent({ event_type: 'x' }), { event_type: 'x', session_id: null });
  assert.equal(parseLiveEvent(null), null);
  assert.equal(parseLiveEvent({ event_type: 3 }), null);
  assert.equal(parseLink({ up: true }), true);
  assert.equal(parseLink({ up: 'yes' }), null);
});
