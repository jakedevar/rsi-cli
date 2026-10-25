// One error presentation (local #207, from the #205 audit): every daemon error class maps to a
// plain sentence and a next step, an unknown code falls back with the raw text kept as Details,
// credentials never show, and the toast / error line / list row surfaces render the new text the
// same with the Vim keys switch on or off.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { errorSentence, parseDaemonError, plainError, presentError, redactCredentials } from '../dist/errors-model.js';
import { pushNotice, EMPTY_NOTICES, toastKind } from '../dist/lifecycle-model.js';
import { dialogKeyBlocked, focusOwnsKey, paneKeyAllowed } from '../dist/modal-gate.js';

const RAW = {
  unreachable: 'cannot reach rsid: /home/u/.rsi/daemon.sock: No such file or directory (os error 2)',
  closed: 'daemon i/o error: connection closed before a response',
  timeout: 'daemon did not answer in time',
  stale: 'daemon error -32000: row changed [stale_version]',
  notFound: 'daemon error -32000: session not found: 11111111-1111-4111-8111-111111111111',
  denied: 'daemon error -32000: agent_issue_authority_denied: the caller does not own Issue #9',
  outdated: 'daemon error -32601: Method not found: ListGraphRuns',
  invalid: 'daemon error -32602: invalid params: missing field `title`',
  refusedNext: 'daemon error -32000: deploy draining | next_action: retry the same request after the deploy settles [deploy_draining]',
  refusedCode: 'daemon error -32000: the queue is frozen [landing_queue_frozen]',
  unknown: 'daemon error -32000: something odd happened',
};

test('each named class maps to its plain sentence and next step', () => {
  const cases = [
    [RAW.unreachable, 'unreachable', "RSI's background service (rsid) is not running or cannot be reached.", 'Start rsid, or wait for it to restart; the app reconnects on its own.'],
    [RAW.closed, 'unreachable', "RSI's background service (rsid) is not running or cannot be reached.", 'Start rsid, or wait for it to restart; the app reconnects on its own.'],
    [RAW.timeout, 'timeout', 'rsid did not answer in time.', 'It may be busy; try again in a moment.'],
    [RAW.stale, 'stale', 'Someone else changed this first.', 'Refresh and try again.'],
    [RAW.notFound, 'not_found', 'rsid could not find that: session not found: 11111111-1111-4111-8111-111111111111.', 'Refresh the list; it may have been removed or archived.'],
    [RAW.denied, 'denied', 'rsid refused this action: the caller does not own Issue #9.', 'You do not have permission for it; ask the operator or the manager who owns it.'],
    [RAW.outdated, 'outdated', 'This version of rsid does not support that action.', 'Update rsid and restart it so it matches the app.'],
    [RAW.invalid, 'invalid', 'rsid refused the request as invalid: invalid params: missing field `title`.', 'Check the values and try again.'],
    [RAW.refusedCode, 'refused', 'rsid refused this action: the queue is frozen.', 'See Details for the reason.'],
  ];
  for (const [raw, kind, text, next] of cases) {
    const p = plainError(raw);
    assert.equal(p.kind, kind, raw);
    assert.equal(p.text, text, raw);
    assert.equal(p.next, next, raw);
    assert.equal(p.detail, raw, 'the raw error is kept whole for Details');
    assert.doesNotMatch(errorSentence(p), /daemon error -32/);
  }
  // A thrown Error and a String(Error) read the same as the bare string.
  assert.equal(plainError(new Error(RAW.stale)).text, 'Someone else changed this first.');
  assert.equal(plainError(`Error: ${RAW.stale}`).kind, 'stale');
});

test("the daemon's own next_action is the next step when it sends one", () => {
  const p = plainError(RAW.refusedNext);
  assert.equal(p.kind, 'restarting');
  assert.equal(p.code, 'deploy_draining');
  assert.equal(p.next, 'Retry the same request after the deploy settles.');
  assert.equal(p.detail, RAW.refusedNext);
  assert.deepEqual(parseDaemonError(RAW.refusedNext), {
    rpcCode: -32000, message: 'deploy draining', code: 'deploy_draining', nextAction: 'retry the same request after the deploy settles',
  });
  const denied = plainError('daemon error -32000: not yours | next_action: ask the manager [agent_spawn_denied]');
  assert.equal(denied.kind, 'denied');
  assert.equal(denied.next, 'Ask the manager.');
});

test('an unknown code falls back to a generic sentence and keeps Details', () => {
  const p = plainError(RAW.unknown);
  assert.equal(p.kind, 'unknown');
  assert.equal(p.text, 'rsid could not do this: something odd happened.');
  assert.equal(p.next, 'If it keeps happening, copy the Details into a bug report.');
  assert.equal(p.detail, RAW.unknown);
  assert.equal(p.rpcCode, -32000);
  // A client-side message is shown as it is.
  const local = plainError('Pick a project first');
  assert.equal(local.kind, 'local');
  assert.equal(local.text, 'Pick a project first');
});

test('a composed message keeps its prefix and swaps the daemon error for plain words', () => {
  const launch = presentError(`Launch failed: ${RAW.unreachable}`);
  assert.equal(launch.text, "Launch failed: RSI's background service (rsid) is not running or cannot be reached. Start rsid, or wait for it to restart; the app reconnects on its own.");
  assert.equal(launch.detail, RAW.unreachable);
  const stale = presentError(`Save failed: Error: ${RAW.stale}`);
  assert.equal(stale.text, 'Save failed: Someone else changed this first. Refresh and try again.');
  assert.equal(stale.detail, `Error: ${RAW.stale}`);
  assert.deepEqual(presentError('Copied the link'), { text: 'Copied the link', detail: null, kind: 'local' });
});

test('credentials never show, in the sentence or in Details', () => {
  const secret = 'sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv';
  const raw = `daemon error -32000: provider refused key ${secret} (token=abc123def456, Authorization: Bearer eyJhbGciOiJIUzI1NiJ9xx)`;
  const p = plainError(raw);
  for (const s of [p.text, p.next, p.detail]) {
    assert.doesNotMatch(s, /sk-ant-api03|abc123def456|eyJhbGciOiJIUzI1NiJ9xx/);
  }
  assert.match(p.detail, /provider refused key \[redacted\]/);
  assert.equal(redactCredentials('ghp_0123456789abcdefghijABCDEFGHIJ and AKIAABCDEFGHIJKLMNOP'), '[redacted] and [redacted]');
  assert.equal(redactCredentials('RSI_SESSION_TOKEN=s3cr3tvalue'), 'RSI_SESSION_TOKEN=[redacted]');
  assert.equal(redactCredentials('row changed [stale_version]'), 'row changed [stale_version]');
});

test('the notification log keeps the raw error and files it as an error', () => {
  assert.equal(toastKind('Someone else changed this first. Refresh and try again.', false, RAW.stale), 'error');
  const s = pushNotice(EMPTY_NOTICES, { kind: 'error', message: 'Someone else changed this first.', detail: RAW.stale }, 1000);
  assert.equal(s.items[0].detail, RAW.stale);
});

// ---------- the DOM surfaces, on a small fake document ----------

class FakeNode {
  constructor(tag) {
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.className = '';
    this.hidden = false;
    this.attrs = {};
    this.dataset = {};
    this.listeners = {};
    this.style = {};
    this._text = '';
    this.parent = null;
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...nodes) { for (const n of nodes) { n.parent = this; this.children.push(n); } }
  replaceChildren(...nodes) { this._text = ''; this.children = []; this.append(...nodes); }
  setAttribute(k, v) { this.attrs[k] = v; }
  addEventListener(type, cb) { (this.listeners[type] ??= []).push(cb); }
  querySelector(sel) {
    const tag = sel.toUpperCase();
    for (const c of this.children) {
      if (c.tagName === tag || (sel.startsWith('.') && c.className.split(' ').includes(sel.slice(1)))) return c;
      const deep = c.querySelector(sel);
      if (deep) return deep;
    }
    return null;
  }
  closest(sel) {
    for (let n = this; n; n = n.parent) {
      for (const part of sel.split(',').map((x) => x.trim().toUpperCase())) if (n.tagName === part) return n;
    }
    return null;
  }
}

function installDom() {
  const toastEl = new FakeNode('div');
  toastEl.hidden = true;
  globalThis.document = {
    getElementById: (id) => (id === 'toast' ? toastEl : null),
    createElement: (tag) => new FakeNode(tag),
  };
  globalThis.window = { setTimeout: () => 1, clearTimeout: () => undefined };
  return toastEl;
}

const parts = (node) => ({
  text: node.querySelector('.err-text')?.textContent ?? node.textContent,
  details: node.querySelector('details'),
});

for (const vim of [true, false]) {
  test(`the toast, an error line and a list row render the plain text with Details (Vim keys ${vim ? 'on' : 'off'})`, async () => {
    const toastEl = installDom();
    globalThis.localStorage = { getItem: () => (vim ? '1' : '0') };
    const { errorBlock, onToast, renderMessage, toast } = await import('../dist/dom.js');

    const heard = [];
    onToast((msg, ok, detail) => heard.push({ msg, ok, detail }));
    toast(`Launch failed: ${RAW.unreachable}`);
    const t = parts(toastEl);
    assert.equal(t.text, "Launch failed: RSI's background service (rsid) is not running or cannot be reached. Start rsid, or wait for it to restart; the app reconnects on its own.");
    assert.equal(toastEl.hidden, false);
    assert.equal(t.details.querySelector('summary').textContent, 'Details');
    assert.equal(t.details.querySelector('pre').textContent, RAW.unreachable);
    assert.equal(t.details.querySelector('button').textContent, 'Copy');
    assert.deepEqual(heard.at(-1), { msg: t.text, ok: false, detail: RAW.unreachable });

    // A success toast is untouched.
    toast('Copied link', true);
    assert.equal(toastEl.textContent, 'Copied link');
    assert.equal(toastEl.querySelector('details'), null);

    // A dialog's error line (showError renders through renderMessage).
    const line = new FakeNode('div');
    renderMessage(line, `Answer failed: ${RAW.stale}`);
    assert.equal(parts(line).text, 'Answer failed: Someone else changed this first. Refresh and try again.');
    assert.equal(parts(line).details.querySelector('pre').textContent, RAW.stale);

    // The Issues list's load-failure row.
    const row = errorBlock('li', 'issues-empty issues-load-error', `Could not load Issues: ${RAW.timeout}`);
    assert.equal(parts(row).text, 'Could not load Issues: rsid did not answer in time. It may be busy; try again in a moment.');

    // The Details controls take Enter and Space with Vim keys on or off; inside a dialog
    // the gate never blocks them, and the panes still get their keys under the same switch.
    const dialog = new FakeNode('dialog');
    dialog.append(line);
    const summary = line.querySelector('summary');
    const copy = line.querySelector('details').querySelector('button');
    for (const target of [summary, copy]) {
      assert.equal(focusOwnsKey('Enter', target), true);
      assert.equal(focusOwnsKey(' ', target), true);
      assert.equal(dialogKeyBlocked(vim, { key: 'Enter', target }), false);
    }
    assert.equal(paneKeyAllowed(vim, { key: 'j' }), vim);
    assert.equal(paneKeyAllowed(vim, { key: 'Escape' }), true);
  });
}
