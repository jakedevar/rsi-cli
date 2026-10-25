// A Failed or Interrupted session says why and what to do (local #225, from the #221 audit):
// the card text for Failed with and without a reason, for Interrupted, for Completed, and for an
// error event; next steps name a visible button with the Vim key second; an Emergency stop points
// at the "Resume AI work" banner; the card draws with its buttons and a Details disclosure; the
// composer and transcript say a message continues a finished session.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  ERROR_LINE_CHARS, cardText, isErrorEvent, isFinishedStatus, lastError, launchPrefill, outcomeCard, terminalSentence, wantsCard,
} from '../dist/session-outcome-model.js';
import { continuePlaceholder, sendPlaceholder } from '../dist/keylabel.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

const session = (o = {}) => ({
  id: '0f9e8d7c-aaaa-4bbb-8ccc-000000000001', status: 'Failed', provider: 'Claude', model: 'opus', project_id: 'p1',
  query: 'fix the login bug', terminal_reason: null, stop_reason: null, ...o,
});
const err = (content, metadata) => ({ event_type: 'System', content, metadata });
const msg = (content) => ({ event_type: 'Message', content });

test('Failed without a reason: still says it failed, that there is no reason, and what to do', () => {
  const c = outcomeCard(session(), []);
  assert.equal(c.tone, 'failed');
  assert.equal(c.title, 'This session failed.');
  assert.equal(c.reason, null);
  assert.equal(c.error, null);
  assert.equal(c.detail, null);
  assert.deepEqual(c.steps.map((s) => s.id), ['continue', 'new-like']);
  // Cmd-N / Ctrl-N opens an empty New session, not this prefilled one, so the step names no key (#305).
  assert.equal(c.nextLine, 'Click Continue with a message (or press i) · Click New session like this one');
});

test('Failed with a reason: the reason is a plain sentence and the raw values sit under Details', () => {
  const c = outcomeCard(session({ terminal_reason: 'budget_exhausted', stop_reason: 'max_tokens' }), []);
  assert.equal(c.reason, 'It ran out of its budget. Stopped because the reply hit the length limit.');
  assert.equal(c.detail, 'terminal_reason: budget_exhausted\nstop_reason: max_tokens');
  assert.ok(!c.title.includes('budget_exhausted'));
});

test('an unknown reason is shown as given, never dropped', () => {
  const c = outcomeCard(session({ terminal_reason: 'brand_new_reason', stop_reason: 'odd' }), []);
  assert.equal(c.reason, 'Reason given: brand_new_reason. Stopped because: odd.');
  assert.equal(c.detail, 'terminal_reason: brand_new_reason\nstop_reason: odd');
  assert.equal(terminalSentence('provider_error:codex_usage_limit'), 'The provider’s usage limit was reached.');
  assert.equal(terminalSentence('provider_error:boom'), 'The AI provider reported an error.');
});

test('Interrupted has its own title and the same next steps', () => {
  const c = outcomeCard(session({ status: 'Interrupted', terminal_reason: 'interrupted:daemon_restart' }), []);
  assert.equal(c.tone, 'interrupted');
  assert.equal(c.title, 'This session was interrupted.');
  assert.equal(c.reason, 'RSI’s background service restarted while it was running.');
  assert.deepEqual(c.steps.map((s) => s.id), ['continue', 'new-like']);
});

test('Completed gets a card only when it ended for a reason other than completed', () => {
  assert.equal(outcomeCard(session({ status: 'Completed' }), []), null);
  assert.equal(outcomeCard(session({ status: 'Completed', terminal_reason: 'completed' }), []), null);
  assert.equal(outcomeCard(session({ status: 'Running', terminal_reason: 'api_error' }), []), null);
  const c = outcomeCard(session({ status: 'Completed', terminal_reason: 'max_turns' }), [err('boom', { is_error: true })]);
  assert.equal(c.tone, 'note');
  assert.equal(c.reason, 'It reached its limit on turns.');
  assert.equal(c.error, null);
  assert.deepEqual(c.steps.map((s) => s.id), ['new-like']);
  assert.equal(wantsCard({ status: 'Failed' }), true);
  assert.equal(wantsCard({ status: 'Running' }), false);
});

test('an is_error event: flagged by metadata, or a System line that says error or failed', () => {
  assert.equal(isErrorEvent(msg('no problem')), false);
  assert.equal(isErrorEvent({ event_type: 'ToolResult', content: 'oops', metadata: { is_error: true } }), true);
  assert.equal(isErrorEvent({ event_type: 'ToolResult', content: 'oops', metadata: '{"is_error":true}' }), true);
  assert.equal(isErrorEvent(err('The command Failed to run', { is_error: false })), false);
  assert.equal(isErrorEvent(err('The command Failed to run')), true);
  assert.equal(isErrorEvent(err('all fine')), false);
});

test('the last error: newest event, first line, 120 characters, credentials redacted, whole text for Copy', () => {
  const long = 'x'.repeat(300);
  const events = [err('old error', { is_error: true }), msg('hello'), err(`${long}\nsecond line`, { is_error: true }), msg('after')];
  const e = lastError(events);
  assert.equal([...e.line].length, ERROR_LINE_CHARS);
  assert.ok(e.full.includes('second line'));
  const secret = lastError([err('auth failed: api_key=sk-ant-abcdefghijklmnop1234', { is_error: true })]);
  assert.ok(!secret.line.includes('sk-ant'));
  assert.equal(lastError([msg('fine')]), null);
  const c = outcomeCard(session({ terminal_reason: 'api_error' }), events);
  assert.equal(c.error.line.length, ERROR_LINE_CHARS);
  assert.ok(cardText(c).includes('Last error: xxx'));
});

test('a login, key or missing-program failure adds the Check provider login step', () => {
  const byReason = outcomeCard(session({ terminal_reason: 'provider_error:codex 401 unauthorized' }), []);
  assert.deepEqual(byReason.steps.map((s) => s.id), ['continue', 'new-like', 'login']);
  assert.ok(byReason.nextLine.includes('Click Check provider login'));
  const byError = outcomeCard(session(), [err('claude: command not found', { is_error: true })]);
  assert.ok(byError.steps.some((s) => s.id === 'login'));
  const other = outcomeCard(session({ terminal_reason: 'budget_exhausted' }), [err('out of budget', { is_error: true })]);
  assert.ok(!other.steps.some((s) => s.id === 'login'));
});

test('with Vim keys off the page wording is the keyless form and every step still names its button', () => {
  const on = outcomeCard(session(), [], {}, true);
  assert.ok(on.nextLine.includes('(or press i)'));
  const cmd = outcomeCard(session(), [], {}, true, 'mac');
  assert.ok(cmd.nextLine.endsWith('Click New session like this one'), cmd.nextLine);
  for (const s of on.steps) assert.ok(on.nextLine.includes(s.label));
});

test('an Emergency stop points at the Resume AI work banner and offers no dead buttons', () => {
  const stopped = outcomeCard(session({ status: 'Interrupted' }), [], { aiStopMode: 'stop-all', stoppedByEmergency: true });
  assert.equal(stopped.title, 'This session was stopped by Emergency stop.');
  assert.deepEqual(stopped.steps, []);
  assert.ok(stopped.nextLine.includes('Resume AI work'));
  assert.ok(stopped.nextLine.includes('banner'));
  // The mode is back to normal but the session is still paused: Resume on the session.
  const paused = outcomeCard(session({ status: 'Interrupted' }), [], { aiStopMode: null, stoppedByEmergency: true });
  assert.deepEqual(paused.steps, []);
  assert.ok(paused.nextLine.includes('Resume'));
  // While AI work is stopped any finished session says where the way back is.
  assert.ok(outcomeCard(session(), [], { aiStopMode: 'stop-all' }).nextLine.includes('Resume AI work'));
  // Other modes do not block messages: normal steps.
  assert.deepEqual(outcomeCard(session(), [], { aiStopMode: 'deny-paid' }).steps.map((s) => s.id), ['continue', 'new-like']);
});

test('equal cards have equal keys, so a poll does not redraw an open Details', () => {
  assert.equal(outcomeCard(session(), []).key, outcomeCard(session(), []).key);
  assert.notEqual(outcomeCard(session(), []).key, outcomeCard(session({ terminal_reason: 'api_error' }), []).key);
});

test('a new session like this one carries the request, provider, model and project', () => {
  assert.deepEqual(launchPrefill(session()), { query: 'fix the login bug', provider: 'Claude', model: 'opus', project_id: 'p1' });
  assert.deepEqual(launchPrefill(session({ model: null, project_id: null })), { query: 'fix the login bug', provider: 'Claude' });
});

test('the composer of a finished session says a message continues it; a running one is unchanged', () => {
  assert.equal(continuePlaceholder('linux'), 'Send a message to continue this session (Ctrl+Enter to send)');
  assert.equal(continuePlaceholder('mac'), 'Send a message to continue this session (Cmd+Enter to send)');
  assert.equal(sendPlaceholder('linux'), 'Message this session (Ctrl+Enter to send)');
  for (const st of ['Failed', 'Interrupted', 'Completed']) assert.equal(isFinishedStatus(st), true, st);
  for (const st of ['Running', 'Starting', 'WaitingApproval', 'Archived']) assert.equal(isFinishedStatus(st), false, st);
});

// ---------- the DOM: the card and its buttons for a Failed fixture session ----------

class FakeNode {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.children = []; this.className = ''; this.hidden = false;
    this.attrs = {}; this.dataset = {}; this.listeners = {}; this.style = {}; this._text = ''; this.title = '';
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...nodes) { for (const n of nodes) this.children.push(n); }
  replaceChildren(...nodes) { this._text = ''; this.children = []; this.append(...nodes); }
  setAttribute(k, v) { this.attrs[k] = v; }
  addEventListener(type, cb) { (this.listeners[type] ??= []).push(cb); }
  click() { for (const f of this.listeners.click ?? []) f({ stopPropagation() {} }); }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  querySelector(sel) {
    return this.walk().slice(1).find((n) => (sel.startsWith('.') ? n.className.split(' ').includes(sel.slice(1)) : n.tagName === sel.toUpperCase())) ?? null;
  }
}
globalThis.document = { getElementById: () => null, createElement: (t) => new FakeNode(t) };
globalThis.window = { setTimeout: () => 1, clearTimeout: () => undefined };
const { drawOutcomeCard } = await import('../dist/session-outcome-view.js');

test('the card for a Failed fixture draws its sentence, last error, buttons, key line and Details', () => {
  const card = outcomeCard(
    session({ terminal_reason: 'provider_error:codex 401 unauthorized', stop_reason: 'provider_error:codex 401 unauthorized' }),
    [err('claude: not logged in. Run `claude login`.\nmore', { is_error: true })],
  );
  const host = new FakeNode('div');
  const calls = [];
  drawOutcomeCard(host, card, { step: (id) => calls.push(['step', id]), copyError: (t) => calls.push(['copy', t]) });
  assert.equal(host.hidden, false);
  assert.ok(host.className.includes('outcome-failed'));
  const texts = host.walk().map((n) => n.textContent);
  assert.ok(texts.includes('This session failed.'));
  assert.ok(texts.some((t) => t.startsWith('The AI provider reported an error.') || t.startsWith('The provider')));
  assert.ok(host.querySelector('.outcome-error-text').textContent.startsWith('claude: not logged in.'));
  const buttons = host.walk().filter((n) => n.tagName === 'BUTTON');
  assert.deepEqual(
    buttons.map((b) => b.textContent).filter((t) => t !== 'Copy'),
    ['Continue with a message', 'New session like this one', 'Check provider login'],
  );
  assert.equal(host.querySelector('.outcome-next').dataset.keyTip, '');
  assert.ok(host.querySelector('.err-details'), 'the raw reason sits under a Details disclosure');
  assert.ok(host.querySelector('.err-raw').textContent.includes('terminal_reason: provider_error:codex 401 unauthorized'));
  buttons.find((b) => b.textContent === 'Continue with a message').click();
  buttons.find((b) => b.dataset.outcomeStep === 'login').click();
  buttons.find((b) => b.textContent === 'Copy' && b.title.includes('whole error')).click();
  assert.deepEqual(calls[0], ['step', 'continue']);
  assert.deepEqual(calls[1], ['step', 'login']);
  assert.equal(calls[2][0], 'copy');
  assert.ok(calls[2][1].includes('more'));
});

test('a running session hides and empties the card', () => {
  const host = new FakeNode('div');
  drawOutcomeCard(host, outcomeCard(session(), []), { step() {}, copyError() {} });
  drawOutcomeCard(host, outcomeCard(session({ status: 'Running' }), []), { step() {}, copyError() {} });
  assert.equal(host.hidden, true);
  assert.equal(host.children.length, 0);
});

test('the markup and wiring: the card sits under the header, errors draw in the error style', () => {
  const html = readFileSync(here('../static/index.html'), 'utf8');
  assert.ok(html.indexOf('id="approval-banner"') < html.indexOf('id="outcome-card"'));
  assert.ok(html.indexOf('id="outcome-card"') < html.indexOf('id="events"'));
  const css = readFileSync(here('../static/styles.css'), 'utf8');
  assert.ok(css.includes('.ev-error'));
  const conv = readFileSync(here('../src/conversation.ts'), 'utf8');
  assert.ok(conv.includes('ev-error'));
  const sessions = readFileSync(here('../src/sessions.ts'), 'utf8');
  assert.ok(sessions.includes('continuePlaceholder') && sessions.includes('renderOutcome()'));
});
