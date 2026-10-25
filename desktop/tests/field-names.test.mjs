// Every text field and log region has an accessible name that is not its placeholder (local #326, from audit 4 / #316):
// WebKit falls back to the placeholder only while the field is empty, so once the user types VoiceOver reads a bare "edit text".
// The walk is in a real browser against the fake daemon (the audit toolkit, scripts/audit-lib.mjs); it skips without Chrome or dist.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { findChrome } from '../scripts/smoke-lib.mjs';
import { open, openSessionByTitle, unnamedFields } from '../scripts/audit-lib.mjs';
import { runViews } from '../scripts/audit.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const SKIP = !findChrome() || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false;

test('the session screen names the message box and the question\'s answer box, and the names are not the placeholders', { skip: SKIP, timeout: 90000 }, async () => {
  const a = await open({ width: 760, height: 480 });
  try {
    await openSessionByTitle(a.page, 'Fake waiting worker');
    assert.ok(await a.page.waitFor(`!!document.querySelector('#message')`, 5000));
    assert.deepEqual(await a.page.eval(unnamedFields), []);
    const message = await a.page.eval(`(() => { const m = document.getElementById('message'); return { name: m.getAttribute('aria-label') || [...m.labels].map((l) => l.textContent).join(' '), placeholder: m.placeholder }; })()`);
    assert.match(message.name, /message/i);
    assert.notEqual(message.name.trim(), message.placeholder.trim());
    const answer = await a.page.eval(`(() => { const t = document.querySelector('#approval-banner textarea'); return t ? { name: t.getAttribute('aria-label') ?? '', placeholder: t.placeholder } : null; })()`);
    assert.ok(answer, 'the waiting worker shows the question\'s answer box');
    assert.ok(answer.name.length > 0 && answer.name !== answer.placeholder, `answer box name "${answer.name}" vs placeholder "${answer.placeholder}"`);
  } finally { await a.close(); }
});

test('the checker flags a field named only by its placeholder, and accepts aria-label, aria-labelledby and a <label>', { skip: SKIP, timeout: 60000 }, async () => {
  const a = await open({ width: 760, height: 480 });
  try {
    const flagged = await a.page.eval(`(() => {
      const box = document.createElement('div'); box.id = 'probe';
      box.innerHTML = '<input id="p1" placeholder="only a hint"><textarea id="p2" placeholder="same" aria-label="same"></textarea>'
        + '<input id="p3" aria-label="Named"><span id="lab">Named by id</span><input id="p4" aria-labelledby="lab">'
        + '<label>Wrapped <input id="p5"></label><label for="p6">For</label><input id="p6"><div id="p7" role="log"></div><div id="p8" role="log" aria-label="Log"></div>';
      document.body.append(box);
      const found = ${unnamedFields};
      box.remove();
      return found;
    })()`);
    assert.deepEqual(flagged.filter((f) => /#p\d/.test(f)), ['input#p1 "only a hint"', 'textarea#p2 "same"', 'div#p7']);
  } finally { await a.close(); }
});

test('no field, textarea, select or log region in any of the audited views is named only by its placeholder or not at all', { skip: SKIP, timeout: 600000 }, async () => {
  const out = mkdtempSync(join(tmpdir(), 'rsi-field-names-'));
  const r = await runViews({ out, size: [760, 480], fast: true });
  const unopened = r.views.filter((v) => !v.opened).map((v) => `${v.view}: ${v.why}`);
  assert.deepEqual(unopened, [], 'every view opens');
  const unnamed = r.views.filter((v) => v.unnamed.length).map((v) => `${v.view}: ${v.unnamed.join(', ')}`);
  assert.deepEqual(unnamed, [], 'give each an aria-label, aria-labelledby or a <label> (a placeholder is only a hint)');
});
