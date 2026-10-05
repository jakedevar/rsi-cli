import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  DECLINE_RESPONSE, attentionCount, formatAnswers, needsAttention, pauseLabel, questionKey,
} from '../dist/approvals-model.js';
import { visibleSessions } from '../dist/model.js';

const q = (header, labels, multiSelect = false) => ({
  header, question: `${header}?`, multiSelect, options: labels.map((label) => ({ label, description: '' })),
});

test('needsAttention covers WaitingApproval and live pending questions only', () => {
  assert.equal(needsAttention({ status: 'WaitingApproval' }), true);
  assert.equal(needsAttention({ status: 'Running', pending_question: { questions: [q('a', ['x'])] } }), true);
  assert.equal(needsAttention({ status: 'Running', pending_question: { questions: [] } }), false);
  assert.equal(needsAttention({ status: 'Completed', pending_question: { questions: [q('a', ['x'])] } }), false);
  assert.equal(attentionCount([{ status: 'WaitingApproval' }, { status: 'Running' }, { status: 'Failed' }]), 1);
});

test('formatAnswers mirrors the TUI wire format', () => {
  const single = formatAnswers([q('Pick', ['Yes', 'No'])], [{ selected: [1], text: '' }]);
  assert.deepEqual(single, { ok: true, text: 'No' });
  const multi = formatAnswers(
    [q('A', ['x', 'y', 'z'], true), q('B', [])],
    [{ selected: [2, 0], text: '' }, { selected: [], text: '  free text ' }],
  );
  assert.deepEqual(multi, { ok: true, text: 'Q1 (A): x, z\nQ2 (B): free text' });
  assert.equal(formatAnswers([q('A', ['x'])], [{ selected: [], text: ' ' }]).ok, false);
  assert.equal(formatAnswers([q('A', ['x'])], [{ selected: [5], text: '' }]).ok, false);
  assert.equal(formatAnswers([], []).ok, false);
  assert.match(DECLINE_RESPONSE, /declined/);
});

test('questionKey changes only when the request changes', () => {
  const a = { id: '1', pending_question: { questions: [q('a', ['x'])] } };
  assert.equal(questionKey(a), questionKey({ ...a }));
  assert.notEqual(questionKey(a), questionKey({ id: '1', pending_question: { questions: [q('b', ['x'])] } }));
});

test('pauseLabel', () => {
  assert.equal(pauseLabel('none'), '');
  assert.equal(pauseLabel('hard'), 'hard pause');
});

test('attention sessions sort to the top of the list', () => {
  const base = { provider: 'Claude', query: 'x', working_dir: '/w', created_at: '2026-10-01T00:00:00Z' };
  const all = [
    { ...base, id: 'run', status: 'Running', updated_at: '2026-10-05T00:00:00Z' },
    { ...base, id: 'wait', status: 'WaitingApproval', updated_at: '2026-10-01T00:00:00Z' },
  ];
  const ids = visibleSessions(all, { projectId: null, text: '', showFinished: false }).map((x) => x.id);
  assert.deepEqual(ids, ['wait', 'run']);
});
