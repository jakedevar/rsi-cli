import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  eventKind, mergeEvents, maxSequence, relativeTime, sessionLabel, visibleSessions,
} from '../dist/model.js';

const base = {
  provider: 'Claude', query: 'first line\nsecond', working_dir: '/w',
  created_at: '2026-10-01T00:00:00Z',
};
const s = (id, status, updated_at, extra = {}) => ({ ...base, id, status, updated_at, ...extra });

test('label prefers role identity, then title, then first prompt line', () => {
  assert.equal(sessionLabel(s('a', 'Running', 'x', { agent_role: 'Implementer', epic_spawn_ordinal: 2, title: 'T' })), 'Implementer #2 · T');
  assert.equal(sessionLabel(s('a', 'Running', 'x', { title: 'Raw title' })), 'Raw title');
  assert.equal(sessionLabel(s('a', 'Running', 'x')), 'first line');
});

test('visible sessions: active first, newest first, finished opt-in, project filter', () => {
  const all = [
    s('1', 'Completed', '2026-10-03T00:00:00Z', { project_id: 'p' }),
    s('2', 'Running', '2026-10-01T00:00:00Z', { project_id: 'p' }),
    s('3', 'Failed', '2026-10-02T00:00:00Z', { project_id: 'q' }),
    s('4', 'Archived', '2026-10-04T00:00:00Z', { project_id: 'p' }),
  ];
  assert.deepEqual(visibleSessions(all, { projectId: null, text: '', showFinished: false }).map((x) => x.id), ['2', '3']);
  assert.deepEqual(visibleSessions(all, { projectId: null, text: '', showFinished: true }).map((x) => x.id), ['2', '1', '3']);
  assert.deepEqual(visibleSessions(all, { projectId: 'p', text: '', showFinished: true }).map((x) => x.id), ['2', '1']);
});

test('mergeEvents dedupes by sequence and keeps order', () => {
  const e = (sequence, content) => ({ sequence, content });
  const merged = mergeEvents([e(1, 'a'), e(2, 'b')], [e(2, 'B'), e(3, 'c')]);
  assert.deepEqual(merged.map((x) => x.content), ['a', 'B', 'c']);
  assert.equal(maxSequence(merged), 3);
  const cached = [e(1, 'a')];
  assert.equal(mergeEvents(cached, []), cached);
});

test('eventKind maps daemon event types', () => {
  assert.equal(eventKind({ event_type: 'Message', role: 'User' }), 'user');
  assert.equal(eventKind({ event_type: 'Message', role: 'Assistant' }), 'assistant');
  assert.equal(eventKind({ event_type: 'ToolUse' }), 'tool');
  assert.equal(eventKind({ event_type: 'ToolResult' }), 'result');
  assert.equal(eventKind({ event_type: 'Thinking' }), 'thinking');
  assert.equal(eventKind({ event_type: 'Plan' }), 'system');
});

test('relativeTime buckets', () => {
  const now = Date.parse('2026-10-04T12:00:00Z');
  assert.equal(relativeTime('2026-10-04T11:59:30Z', now), '30s');
  assert.equal(relativeTime('2026-10-04T11:00:00Z', now), '1h');
  assert.equal(relativeTime('2026-10-02T12:00:00Z', now), '2d');
});

test('elideBlobs replaces long base64 runs and keeps prose', async () => {
  const { elideBlobs } = await import('../dist/model.js');
  const blob = 'A'.repeat(4096);
  assert.equal(elideBlobs(`img ${blob} end`), 'img [3 KiB base64 elided] end');
  assert.equal(elideBlobs('short text stays'), 'short text stays');
});
