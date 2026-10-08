import test from 'node:test';
import assert from 'node:assert/strict';
import { AnswerError, createApi } from '../api.js';
import { answerProblem, createDraftStore, describeDraft, randomUuid, submitDraft } from '../state.js';

const CSRF = 'b'.repeat(64);
const KEY_A = '11111111-1111-4111-8111-111111111111';
const KEY_B = '22222222-2222-4222-8222-222222222222';
const KEY_C = '33333333-3333-4333-8333-333333333333';

const json = (value, status = 200, headers = {}) => ({
  ok: status >= 200 && status < 300,
  status,
  headers: new Headers({ 'content-type': 'application/json', ...headers }),
  text: async () => JSON.stringify(value),
});
const bare = (status, headers = {}) => ({ ok: status >= 200 && status < 300, status, headers: new Headers(headers), text: async () => '' });

const summary = {
  id: 's-1', project_id: 'p-1', parent_id: null, continued_from: null,
  kind: { state: 'known', value: 'Standard' }, status: { state: 'known', value: 'WaitingApproval' },
  provider: { state: 'known', value: 'Harness' }, own_title: 'Manager', updated_at: '2026-10-07T10:00:00Z',
  attention: { requires_local_action: true, incomplete: false },
};
const target = (over = {}) => ({
  decision_key: 'question:s-1', row_version: '7', target_digest: 'sha256:abc', kind: 'question',
  title: 'Use the reserved version?', detail: 'It blocks the migration.', options: ['Yes', 'No'], ...over,
});
const targetsDoc = (items = [target()]) => ({ manager: 'configured', fence: { policy_version: '3', scope_version: '4' }, items, truncated: false });

// A fake gateway: `targets` answers the decision-targets GET, `answer` the POST.
function gateway({ targets = () => json(targetsDoc()), answer = () => json({ event_sequence: 5, key: 'r-1', row_version: 8, deduplicated: false }) } = {}) {
  const calls = [];
  let sessions = 0;
  const fetchImpl = async (path, init = {}) => {
    calls.push({ path, init });
    if (path === '/auth/bootstrap') return json({ nonce: 'n'.repeat(64) });
    if (path === '/auth/session') { sessions += 1; return json({ csrf: CSRF }); }
    if (path === '/api/v1/projects') return json({ items: [{ id: 'p-1', name: 'P' }] });
    if (path === '/api/v1/projects/p-1/sessions') return json({ items: [summary] });
    if (path === '/api/v1/projects/p-1/sessions/s-1') return json({ item: { summary } });
    if (path === '/api/v1/projects/p-1/sessions/s-1/history') return json({ items: [] });
    if (path === '/api/v1/projects/p-1/sessions/s-1/decisions') return json({ items: [] });
    if (path === '/api/v1/projects/p-1/sessions/s-1/decision-targets') return targets(init);
    if (path === '/api/v1/projects/p-1/sessions/s-1/decisions/answer' || path === '/api/v1/projects/p-1/sessions/s-1/decisions/answer-pending') return answer(init);
    return bare(404);
  };
  return { fetchImpl, calls, sessions: () => sessions, posts: () => calls.filter((call) => call.init.method === 'POST' && call.path.endsWith('/decisions/answer')) };
}

async function opened(gw) {
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  return api;
}

const request = (over = {}) => ({
  decisionKey: 'question:s-1', rowVersion: '7', digest: 'sha256:abc', fence: { policyVersion: '3', scopeVersion: '4' },
  answer: 'Yes', idempotencyKey: KEY_A, ...over,
});

// ---- the targets read ----

test('targets keep the exact version, digest and fence strings the host sent', async () => {
  const gw = gateway({ targets: () => json(targetsDoc([target({ row_version: '9007199254740993' })])) });
  const loaded = await (await opened(gw)).getSession('s-1');
  assert.equal(loaded.targets.state, 'ok');
  assert.equal(loaded.targets.ready, true);
  assert.deepEqual(loaded.targets.fence, { policyVersion: '3', scopeVersion: '4' });
  assert.deepEqual(loaded.targets.items, [{
    decisionKey: 'question:s-1', rowVersion: '9007199254740993', digest: 'sha256:abc', kind: 'question',
    title: 'Use the reserved version?', detail: 'It blocks the migration.', options: ['Yes', 'No'],
  }]);
});

test('answers off (403) and no manager or fence give no form and no error', async () => {
  const off = await (await opened(gateway({ targets: () => bare(403) }))).getSession('s-1');
  assert.deepEqual([off.targets.state, off.targets.ready, off.targets.items], ['off', false, []]);
  const none = await (await opened(gateway({ targets: () => json({ manager: 'not_configured', fence: null, items: [], truncated: false }) }))).getSession('s-1');
  assert.deepEqual([none.targets.state, none.targets.ready], ['ok', false]);
  const noFence = await (await opened(gateway({ targets: () => json({ ...targetsDoc(), fence: null }) }))).getSession('s-1');
  assert.deepEqual([noFence.targets.state, noFence.targets.ready, noFence.targets.items], ['ok', false, []]);
});

test('a failed or malformed targets read degrades to error and keeps the session readable', async () => {
  const down = await (await opened(gateway({ targets: () => bare(502) }))).getSession('s-1');
  assert.equal(down.targets.state, 'error');
  assert.equal(down.session.id, 's-1');
  const bad = await (await opened(gateway({ targets: () => json(targetsDoc([target({ row_version: '07' })])) }))).getSession('s-1');
  assert.equal(bad.targets.state, 'error');
});

// ---- the answer POST ----

test('an answer is one POST with the CSRF token, exact fields and the draft key', async () => {
  const gw = gateway();
  const api = await opened(gw);
  const receipt = await api.answerDecision('s-1', request());
  assert.deepEqual(receipt, { key: 'r-1', deduplicated: false });
  const [post] = gw.posts();
  assert.equal(post.path, '/api/v1/projects/p-1/sessions/s-1/decisions/answer');
  assert.equal(post.init.headers['x-rsi-csrf'], CSRF);
  assert.deepEqual(JSON.parse(post.init.body), {
    decision_key: 'question:s-1', expected_row_version: '7', target_digest: 'sha256:abc',
    fence: { policy_version: '3', scope_version: '4' }, answer: 'Yes', idempotency_key: KEY_A,
  });
});

test('an invalid answer never reaches the network', async () => {
  const gw = gateway();
  const api = await opened(gw);
  for (const bad of [
    request({ answer: '' }),
    request({ answer: 'a\0b' }),
    request({ answer: 'é'.repeat(1025) }),
    request({ idempotencyKey: 'AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA' }),
    request({ rowVersion: 7 }),
    request({ fence: { policyVersion: '0', scopeVersion: '4' } }),
  ]) {
    await assert.rejects(api.answerDecision('s-1', bad), (error) => error instanceof AnswerError && error.kind === 'invalid');
  }
  await assert.rejects(api.answerDecision('s-9', request()), (error) => error.kind === 'invalid');
  assert.equal(gw.posts().length, 0);
});

test('each refusal is told apart and says whether anything was sent', async () => {
  const cases = [
    [() => json({ error: 'decision_changed' }, 409), 'stale', /decision changed/],
    [() => bare(403), 'denied', /refused this answer/],
    [() => json({ error: 'rate_limited' }, 429, { 'Retry-After': '7' }), 'rate_limited', /Nothing was sent\. Try again in 7 seconds/],
    [() => json({ error: 'busy' }, 503, { 'Retry-After': '1' }), 'busy', /busy\. Nothing was sent\. Try again in 1 second\./],
    [() => json({ error: 'audit_unavailable' }, 503), 'busy', /Nothing was sent/],
    [() => json({ error: 'outcome_unknown' }, 504), 'unknown', /did not reply in time.*not known whether/],
    [() => json({ error: 'outcome_unknown' }, 502), 'unknown', /failed after sending.*not known whether/],
    [() => bare(502), 'unavailable', /Nothing was sent.*rsid/],
    [() => bare(504), 'unavailable', /Nothing was sent/],
    [() => json({ error: 'invalid_answer' }, 400), 'invalid', /could not accept/],
    [() => bare(418), 'unexpected', /418/],
  ];
  for (const [answer, kind, message] of cases) {
    const api = await opened(gateway({ answer }));
    await assert.rejects(api.answerDecision('s-1', request()), (error) => {
      assert.ok(error instanceof AnswerError);
      assert.equal(error.kind, kind);
      assert.match(error.message, message);
      return true;
    });
  }
  const limited = await opened(gateway({ answer: () => json({ error: 'rate_limited' }, 429, { 'Retry-After': '7' }) }));
  await assert.rejects(limited.answerDecision('s-1', request()), (error) => error.retryAfter === 7);
});

test('a lost connection after the POST is an unknown outcome, not a failure to send', async () => {
  const gw = gateway({ answer: () => { throw new TypeError('network down'); } });
  const api = await opened(gw);
  await assert.rejects(api.answerDecision('s-1', request()), (error) => error.kind === 'unknown' && /not known whether your answer was delivered/.test(error.message));
});

test('one 401 re-bootstraps and resends the identical POST; a second is a session error', async () => {
  let first = true;
  const gw = gateway({ answer: () => { if (first) { first = false; return bare(401); } return json({ key: 'r-2', deduplicated: true }); } });
  const api = await opened(gw);
  assert.deepEqual(await api.answerDecision('s-1', request()), { key: 'r-2', deduplicated: true });
  const [one, two] = gw.posts();
  assert.equal(one.init.body, two.init.body);
  assert.equal(gw.sessions(), 2);
  const dead = await opened(gateway({ answer: () => bare(401) }));
  await assert.rejects(dead.answerDecision('s-1', request()), (error) => error.kind === 'session');
});

// ---- drafts ----

const item = (over = {}) => ({ decisionKey: 'question:s-1', rowVersion: '7', digest: 'sha256:abc', kind: 'question', title: 'Q', detail: '', options: ['Yes', 'No'], ...over });
const fence = { policyVersion: '3', scopeVersion: '4' };
const keys = () => { const queue = [KEY_A, KEY_B, KEY_C]; return () => queue.shift(); };
const fakeApi = (...results) => {
  const sent = [];
  return { sent, answerDecision: async (_session, req) => { sent.push(req); const next = results.shift(); if (next instanceof Error) throw next; return next ?? { key: 'r-1', deduplicated: false }; } };
};

test('randomUuid is a canonical lowercase v4, also without randomUUID', () => {
  const canonical = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
  assert.match(randomUuid(), canonical);
  const noSecure = { getRandomValues: (bytes) => bytes.fill(255) };
  assert.match(randomUuid(noSecure), canonical);
  assert.equal(randomUuid({ randomUUID: () => 'AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA' }), 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa');
});

test('answer text limits match the gateway', () => {
  assert.equal(answerProblem('ok'), '');
  assert.notEqual(answerProblem('   '), '');
  assert.notEqual(answerProblem('a\0'), '');
  assert.equal(answerProblem('a'.repeat(2048)), '');
  assert.notEqual(answerProblem('a'.repeat(2049)), '');
});

test('a draft keeps its text and key across polls of the same decision', () => {
  const store = createDraftStore({ newKey: keys() });
  const draft = store.ensure('s-1', item());
  store.setText(draft, 'half typed');
  store.sync('s-1', [item({ title: 'Q (refreshed)' })]);
  store.sync('s-1', [item()]);
  const same = store.get('s-1', 'question:s-1');
  assert.equal(same, draft);
  assert.equal(same.text, 'half typed');
  assert.equal(same.idempotencyKey, KEY_A);
});

test('a decision that changed under a draft discards it with a notice and a new key', () => {
  const store = createDraftStore({ newKey: keys() });
  const draft = store.ensure('s-1', item());
  store.setText(draft, 'old answer');
  store.sync('s-1', [item({ rowVersion: '8' })]);
  const next = store.get('s-1', 'question:s-1');
  assert.notEqual(next, draft);
  assert.equal(next.text, '');
  assert.equal(next.rowVersion, '8');
  assert.equal(next.idempotencyKey, KEY_B);
  assert.match(describeDraft(next).message, /decision changed.*answer again/);
});

test('idle drafts go when the decision stops waiting; a sent answer stays visible', async () => {
  const store = createDraftStore({ newKey: keys() });
  store.setText(store.ensure('s-1', item({ decisionKey: 'question:idle' })), 'x');
  const sent = store.ensure('s-1', item());
  store.setText(sent, 'Yes');
  await submitDraft({ api: fakeApi(), store, draft: sent, fence });
  store.sync('s-1', []);
  assert.equal(store.get('s-1', 'question:idle'), null);
  assert.equal(store.get('s-1', 'question:s-1').status, 'receipt');
});

test('drafts of different sessions and decisions never mix', () => {
  const store = createDraftStore({ newKey: keys() });
  const one = store.ensure('s-1', item());
  const two = store.ensure('s-2', item());
  const other = store.ensure('s-1', item({ decisionKey: 'approval:9' }));
  assert.equal(new Set([one, two, other]).size, 3);
  assert.equal(new Set([one.idempotencyKey, two.idempotencyKey, other.idempotencyKey]).size, 3);
});

test('a receipt is shown plainly, without claiming the decision is answered', async () => {
  const store = createDraftStore({ newKey: keys() });
  const draft = store.ensure('s-1', item());
  store.setText(draft, 'Yes');
  const api = fakeApi({ key: 'r-42', deduplicated: false });
  assert.equal(await submitDraft({ api, store, draft, fence }), 'receipt');
  assert.deepEqual(api.sent, [{ decisionKey: 'question:s-1', rowVersion: '7', digest: 'sha256:abc', fence, answer: 'Yes', idempotencyKey: KEY_A }]);
  const view = describeDraft(draft);
  assert.equal(view.tone, 'ok');
  assert.match(view.message, /accepted the answer\. Receipt r-42\./);
  assert.match(view.message, /does not mark it answered/);
  assert.deepEqual([view.canEdit, view.canSend, view.canDismiss], [false, false, true]);
  const replay = createDraftStore({ newKey: keys() }).ensure('s-1', item());
  replay.status = 'receipt';
  replay.receipt = { key: 'r-42', deduplicated: true };
  assert.match(describeDraft(replay).message, /already been recorded/);
  store.dismiss(draft);
  assert.equal(store.get('s-1', 'question:s-1'), null);
});

test('409 discards the draft and the next draft carries the refresh notice', async () => {
  const store = createDraftStore({ newKey: keys() });
  const draft = store.ensure('s-1', item());
  store.setText(draft, 'Yes');
  await submitDraft({ api: fakeApi(new AnswerError('stale', 'This decision changed.', 409)), store, draft, fence });
  assert.equal(store.get('s-1', 'question:s-1'), null);
  const next = store.ensure('s-1', item({ rowVersion: '8' }));
  assert.equal(next.text, '');
  assert.equal(next.idempotencyKey, KEY_B);
  assert.match(describeDraft(next).message, /This decision changed\. Refresh shows its current state; your draft was discarded\./);
});

test('a refusal that sent nothing keeps the draft; the same text reuses the key, new text mints one', async () => {
  const store = createDraftStore({ newKey: keys() });
  const draft = store.ensure('s-1', item());
  store.setText(draft, 'Yes');
  const limited = new AnswerError('rate_limited', 'Too many answers too quickly. Nothing was sent. Try again in 5 seconds.', 429, 5);
  assert.equal(await submitDraft({ api: fakeApi(limited), store, draft, fence }), 'editing');
  assert.match(describeDraft(draft).message, /Try again in 5 seconds/);
  assert.equal(draft.text, 'Yes');
  const api = fakeApi();
  await submitDraft({ api, store, draft, fence });
  assert.equal(api.sent[0].idempotencyKey, KEY_A);
  const other = store.ensure('s-1', item({ decisionKey: 'question:two' }));
  store.setText(other, 'Yes');
  await submitDraft({ api: fakeApi(limited), store, draft: other, fence });
  store.setText(other, 'No');
  assert.equal(other.idempotencyKey, KEY_C);
});

test('an unknown outcome locks the answer; retry resends the identical request', async () => {
  const store = createDraftStore({ newKey: keys() });
  const draft = store.ensure('s-1', item());
  store.setText(draft, 'Yes');
  const unknown = new AnswerError('unknown', 'The RSI host did not reply in time, so it is not known whether your answer was delivered.', 504);
  const api = fakeApi(unknown);
  assert.equal(await submitDraft({ api, store, draft, fence }), 'unknown');
  const view = describeDraft(draft);
  assert.equal(view.tone, 'warn');
  assert.match(view.message, /not known whether.*locked.*same key and cannot be applied twice/);
  assert.deepEqual([view.canEdit, view.canSend, view.canRetry], [false, false, true]);
  store.setText(draft, 'No');
  assert.equal(draft.text, 'Yes', 'the locked text cannot be edited');
  // The fence moved on in the meantime; the retry still sends what went out.
  assert.equal(await submitDraft({ api, store, draft, fence: { policyVersion: '9', scopeVersion: '9' } }), 'receipt');
  assert.deepEqual(api.sent[1], api.sent[0]);
});

test('sending twice at once is refused and a missing fence or empty answer sends nothing', async () => {
  const store = createDraftStore({ newKey: keys() });
  const draft = store.ensure('s-1', item());
  const api = fakeApi();
  assert.equal(await submitDraft({ api, store, draft, fence }), 'editing');
  assert.match(describeDraft(draft).message, /Write or choose an answer/);
  store.setText(draft, 'Yes');
  assert.equal(await submitDraft({ api, store, draft, fence: null }), 'editing');
  assert.match(describeDraft(draft).message, /fence is not available/);
  assert.equal(api.sent.length, 0);
  draft.status = 'sending';
  assert.equal(await submitDraft({ api, store, draft, fence }), 'sending');
  assert.equal(api.sent.length, 0);
});


test('pending questions without a manager use the exact occurrence and device-free browser body', async () => {
  const decision = `pending-question:${KEY_B}`;
  const gw = gateway({ targets: () => json({manager:'not_configured', fence:null, items:[], pending_items:[{
    decision_id:decision, target_digest:'sha256:pending', kind:'question', title:'Which option?', detail:'', options:[], receipt:null
  }], truncated:false}), answer: () => json({receipt_key:KEY_A, state:'queued', outcome:null}) });
  const api = await opened(gw);
  const loaded = await api.getSession('s-1');
  assert.equal(loaded.targets.ready, true);
  assert.equal(loaded.targets.fence, null);
  const store = createDraftStore({newKey: () => KEY_A});
  const draft = store.ensure('s-1', loaded.targets.items[0]);
  store.setText(draft, 'Use the safe option');
  await submitDraft({ store, draft, fence: null, api });
  const post = gw.calls.find((call) => call.path.endsWith('/decisions/answer-pending'));
  assert.deepEqual(JSON.parse(post.init.body), {decision_id:decision, expected_target_digest:'sha256:pending', answer:'Use the safe option', idempotency_key:KEY_A});
  assert.equal(draft.receipt.state, 'queued');
});

test('an uncertain pending receipt stays visible and cannot be automatically resent', () => {
  const draft = {status:'receipt', receipt:{key:KEY_A, state:'uncertain'}};
  const view = describeDraft(draft);
  assert.equal(view.canRetry, false);
  assert.equal(view.canEdit, false);
  assert.match(view.message, /will not send it again/);
});
