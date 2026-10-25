// The "AI work is stopped" banner and the way back (local #224): the confirm copy, the banner
// following the model-control mode, the confirmed Resume, and the plain refusal that names it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  STOPPED_BADGE_LABEL, STOPPED_BADGE_TIP, STOP_CONFIRM_BODY, STOP_CONFIRM_TITLE, bannerText, resumeConfirmBody,
  resumeMessage, stopNeedsConfirm, stopToast,
} from '../dist/emergency-model.js';
import { createModeWatch } from '../dist/mode-watch-model.js';
import { MODEL_WORK_STOPPED_NEXT, plainError, presentError } from '../dist/errors-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

function harness({ mode = 'stop_all', confirm = true, setFails = false } = {}) {
  const h = { mode, views: [], toasts: [], confirms: [], sets: [], reads: 0, resumed: 0 };
  h.watch = createModeWatch({
    readStatus: async () => { h.reads += 1; if (h.mode === 'throw') throw new Error('cannot reach rsid'); return { mode: h.mode }; },
    setMode: async (slug) => {
      h.sets.push(slug);
      if (setFails) throw new Error('daemon error -32000: policy update refused [model_control_denied]');
      h.mode = 'normal';
      return { current_mode: 'normal' };
    },
    confirm: async (title, message, label) => { h.confirms.push({ title, message, label }); return confirm; },
    toast: (msg, ok = false) => h.toasts.push({ msg, ok }),
    render: (v) => h.views.push(v),
    afterResume: () => { h.resumed += 1; },
  });
  return h;
}

test('the stop confirm says what happens in plain words and drops the jargon', () => {
  assert.equal(STOP_CONFIRM_TITLE, 'Stop all AI work now?');
  assert.equal(
    STOP_CONFIRM_BODY,
    'Running sessions stop and nothing new starts, not even new sessions, until you choose Resume AI work.',
  );
  for (const jargon of ['invocation', 'HARD', 'Clear a pause']) {
    assert.ok(!`${STOP_CONFIRM_TITLE} ${STOP_CONFIRM_BODY}`.includes(jargon));
  }
  // The key paths still run without the dialog; menus, the palette and buttons ask.
  assert.equal(stopNeedsConfirm('key'), false);
  for (const s of ['menu', 'palette', 'context', 'button']) assert.equal(stopNeedsConfirm(s), true);
});

test('the toast after a stop names the way back, the row badge says Stopped and keeps the HARD meaning', () => {
  assert.equal(
    stopToast({ cancelled: 3, interrupted: ['a', 'b'] }),
    'Emergency stop applied: 3 cancelled, 2 interrupted. Choose Resume AI work in the banner at the top to start again.',
  );
  assert.equal(STOPPED_BADGE_LABEL, 'Stopped');
  assert.match(STOPPED_BADGE_TIP, /HARD pause/);
  assert.match(STOPPED_BADGE_TIP, /Resume AI work/);
});

test('every restricted mode has a banner line and normal has none', () => {
  assert.equal(bannerText('stop-all'), 'All AI work is stopped. Nothing new can start, not even new sessions or messages.');
  for (const m of ['pause-background', 'deny-paid', 'local-only']) assert.ok(bannerText(m), m);
  assert.equal(bannerText('normal'), null);
  assert.equal(bannerText(null), null);
});

test('the banner appears when the mode read returns stop_all and leaves after Resume', async () => {
  const h = harness();
  await h.watch.refresh();
  assert.equal(h.views.at(-1).mode, 'stop-all');
  assert.match(h.views.at(-1).text, /^All AI work is stopped/);
  assert.equal(h.views.at(-1).busy, false);

  await h.watch.resume();
  assert.deepEqual(h.confirms.map((c) => [c.title, c.label]), [['Resume AI work?', 'Resume AI work']]);
  assert.equal(h.confirms[0].message, resumeConfirmBody('stop-all'));
  assert.match(h.confirms[0].message, /New sessions and messages can start again/);
  assert.deepEqual(h.sets, ['normal']);
  assert.deepEqual(h.toasts, [{ msg: resumeMessage(), ok: true }]);
  assert.equal(h.views.at(-1), null, 'the banner is gone');
  assert.equal(h.watch.view(), null);
  assert.equal(h.resumed, 1);
});

test('a change made elsewhere (the TUI, Settings) shows and clears on the next read', async () => {
  const h = harness({ mode: 'normal' });
  await h.watch.refresh();
  assert.equal(h.views.at(-1), null);
  h.mode = 'local_only';
  await h.watch.refresh();
  assert.equal(h.views.at(-1).mode, 'local-only');
  h.mode = 'normal';
  await h.watch.refresh();
  assert.equal(h.views.at(-1), null);
});

test('declining the confirm changes nothing; Resume does nothing while the mode is normal', async () => {
  const no = harness({ confirm: false });
  await no.watch.refresh();
  await no.watch.resume();
  assert.deepEqual(no.sets, []);
  assert.equal(no.watch.view().mode, 'stop-all');
  assert.deepEqual(no.toasts, []);

  const normal = harness({ mode: 'normal' });
  await normal.watch.refresh();
  await normal.watch.resume();
  assert.equal(normal.confirms.length, 0);
});

test('a refused Resume keeps the banner, shows the reason in plain words and is not a success', async () => {
  const h = harness({ setFails: true });
  await h.watch.refresh();
  await h.watch.resume();
  const v = h.watch.view();
  assert.equal(v.mode, 'stop-all');
  assert.equal(v.busy, false);
  assert.match(v.error, /^Could not resume AI work: .*policy update refused/);
  assert.equal(h.toasts.at(-1).ok, false);
  assert.equal(h.resumed, 0);
  assert.match(presentError(v.error).text, /refused/);
});

test('a failed read leaves the banner as it was', async () => {
  const h = harness();
  await h.watch.refresh();
  h.mode = 'throw';
  await h.watch.refresh();
  assert.equal(h.watch.view().mode, 'stop-all');
});

test('plainError names the way back for the stop-all denial, even when the daemon sends its own next step', () => {
  const raw = 'daemon error -32000: all model work is stopped for session launch [model_control_denied]';
  const p = plainError(raw);
  assert.equal(p.next, 'Choose Resume AI work in the banner (or Settings › Safety & Spend › Model Control).');
  assert.equal(p.next, MODEL_WORK_STOPPED_NEXT);
  assert.match(p.text, /all model work is stopped for session launch/);
  assert.equal(p.kind, 'refused');
  assert.equal(p.detail, raw);
  const withNext = plainError('daemon error -32000: all model work is stopped for message send | next_action: ask the operator');
  assert.equal(withNext.next, MODEL_WORK_STOPPED_NEXT);
  // Another refusal keeps its own next step.
  assert.equal(plainError('daemon error -32000: deploy draining | next_action: retry later [deploy_draining]').next, 'Retry later.');
});

test('the page has the banner slot and the Settings row names the way back', () => {
  assert.match(readFileSync(here('../static/index.html'), 'utf8'), /<div id="ai-banner" class="ai-banner" role="status"[^>]*hidden><\/div>/);
  assert.match(readFileSync(here('../src/settings-state.ts'), 'utf8'), /Resume AI work in the banner/);
  const ts = readFileSync(here('../src/actions/emergency.ts'), 'utf8');
  assert.equal(ts.split("'set_model_control_mode'").length - 1, 1, 'the resume uses the Settings row call');
  assert.equal(ts.split("'model_control_overview'").length - 1, 1);
});
