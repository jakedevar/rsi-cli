// The section table of the real-rsid end-to-end smoke: one executable interaction per H2 section of the
// generated desktop/PARITY.md, plus the Vim-keys toggle (hub #3101's DOM gap). Every step drives the built
// app with CDP key events against an ISOLATED rsid (smoke-isolated.mjs) and asserts the end state both in the
// DOM and, for writes, in the daemon itself through a direct read (never through the page).
import { existsSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { issueLifecycle, sleep } from './smoke-e2e.mjs';

const pass = (note, evidence = {}) => ({ status: 'pass', note, evidence });
const fail = (note, evidence = {}) => ({ status: 'fail', note, evidence });

/** The synthetic credential the Provider keys step types. Never a real key. */
export const SYNTHETIC_KEY = 'synthetic-e2e-key-0001';

const cursorIndex = `[...document.querySelectorAll('#session-list li')].findIndex(l => l.classList.contains('cursor'))`;

/** Show completed sessions (the seeded one is finished), then park focus on the page body. */
async function showFinished(h) {
  const { page } = h;
  if (!(await page.eval(`document.getElementById('show-finished').checked`))) await page.click(`document.getElementById('show-finished')`);
  await sleep(400);
  await page.eval(`document.activeElement && document.activeElement.blur()`);
}

/** Walk the list cursor with j/k onto the seeded leaf session. */
async function cursorOnLeaf(h) {
  await showFinished(h);
  const target = await h.page.eval(`[...document.querySelectorAll('#session-list li')].findIndex(l => !l.classList.contains('container') && l.innerText.includes('E2E finished session'))`);
  if (target < 0) throw new Error('the seeded leaf session is not listed');
  for (let guard = 0; guard < 40; guard++) {
    const cur = await h.page.eval(cursorIndex);
    if (cur === target) return;
    await h.page.key(cur < target ? 'j' : 'k');
    await sleep(120);
  }
  throw new Error('could not move the cursor onto the leaf session');
}

async function openLeaf(h) {
  await cursorOnLeaf(h);
  await h.page.key('Enter');
  if (!(await h.page.waitFor(`!document.getElementById('session-view').hidden && document.getElementById('events').innerText.includes('I fixed the answer function')`, 8000))) {
    throw new Error('Enter did not open the leaf session with its real conversation');
  }
  await sleep(300);
}

const vimOn = (page) => page.eval(`document.getElementById('vim-keys').checked`);
const stored = (page) => page.eval(`localStorage.getItem('rsi.desktop.vimKeys')`);

export const E2E_SECTIONS = [
  {
    id: 'global', section: 'Global Keybindings', parityRow: 'global:enter/quick-launch-input-daemon-config-pending',
    source: 'desktop/src/keybindings.ts (Ctrl-N), desktop/src/actions/nav-run.ts, desktop/src-tauri/src/settings.rs',
    interaction: 'Ctrl+N opens the launch dialog only once rsid has answered GetDaemonConfig; the dialog offers rsid\'s providers',
    async run(h) {
      const { page } = h;
      await page.key('n', { ctrl: true });
      if (!(await page.waitFor(`document.getElementById('launch-dialog').open`, 4000))) return fail(`Ctrl+N did not open the launch dialog; toast: ${await h.text('#toast')}`);
      const providers = await page.eval(`[...document.querySelectorAll('#launch-form select')].map((s) => [...s.options].map((o) => o.value))`);
      const forwarded = h.bridgeLog.some((e) => e.cmd === 'get_daemon_config' && e.outcome === 'forwarded');
      const pending = (await h.text('#toast')).includes('still loading');
      await h.closeAll();
      if (!forwarded) return fail('GetDaemonConfig was never forwarded to rsid');
      if (pending) return fail('the launch dialog reported the daemon config as still loading');
      return pass(`rsid answered GetDaemonConfig and the launch dialog opened (${providers.flat().length} option values from rsid)`, { providers: providers.flat().length });
    },
  },
  {
    id: 'normal', section: 'Normal Mode Keybindings', parityRow: 'key:P',
    source: 'desktop/src/sessions.ts, desktop/src/actions/sessionops.ts, desktop/src-tauri/src/sessionops.rs',
    interaction: 'j/k put the cursor on the leaf session, P pins it (TogglePin) and Enter opens its real conversation',
    async run(h) {
      const { page, seed } = h;
      await cursorOnLeaf(h);
      const before = await h.rpc('GetSession', { session_id: seed.leafId });
      await page.key('P', { shift: true });
      let pinned = null;
      for (let i = 0; i < 30 && !pinned; i++) { pinned = (await h.rpc('GetSession', { session_id: seed.leafId })).pinned_at; if (!pinned) await sleep(150); }
      if (before.pinned_at) return fail('the seeded session was already pinned');
      if (!pinned) return fail('P did not pin the session in rsid');
      await page.key('Enter');
      const shown = await page.waitFor(`document.getElementById('events').innerText.includes('I fixed the answer function')`, 8000);
      return shown ? pass('P pinned the session in rsid (pinned_at set) and Enter showed its conversation read from rsid', { pinned_at: pinned })
        : fail('Enter did not show the conversation');
    },
  },
  {
    id: 'command', section: 'Command Mode', parityRow: 'cmd:issues',
    source: 'desktop/src/commands.ts, desktop/src/vimkeys.ts, desktop/src/issues.ts',
    interaction: ':issues opens the Issues workspace listing the Issues stored in rsid',
    async run(h) {
      await h.openIssues();
      const text = await h.text('#issues-list');
      const both = text.includes('Seed Issue alpha') && text.includes('Seed Issue beta');
      return both ? pass('`:issues` listed both seeded Issues read from rsid', { rows: (text.match(/Seed Issue/g) ?? []).length }) : fail(`the seeded Issues are not listed: ${JSON.stringify(text.slice(0, 160))}`);
    },
  },
  {
    id: 'overlay', section: 'Overlay Keybindings', parityRow: 'overlay:file-explorer-overlay-space-e',
    source: 'desktop/src/files.ts, desktop/src/actions/files.ts, desktop/src-tauri/src/files.rs',
    interaction: '<Space>e opens the file explorer on the session root; j and Enter open README.md from the synthetic git project in the viewer',
    async run(h) {
      const { page } = h;
      await openLeaf(h);
      await page.keys(' e');
      if (!(await page.waitFor(`(document.querySelector('.files-overlay')||{}).innerText?.includes('README.md')`, 6000))) return fail('the explorer did not list README.md from rsid');
      const listing = await h.text('.files-overlay');
      await page.key('j'); await sleep(250);
      await page.key('Enter');
      const open = await page.waitFor(`(document.querySelector('.files-overlay')||{}).innerText?.includes('synthetic project')`, 6000);
      const gutter = h.bridgeLog.some((e) => e.cmd === 'get_project_file_git_gutter' && e.outcome === 'forwarded');
      if (!open) return fail('Enter did not open README.md in the viewer');
      return gutter ? pass('the explorer listed src/ and README.md from rsid; Enter opened README.md with its git gutter read from rsid', { listing: listing.split('\n').filter((l) => /src|README/.test(l)) })
        : fail('the viewer opened but the git gutter was never read from rsid');
    },
  },
  {
    id: 'overlay-ref', section: 'Overlay Key Reference (generated)', parityRow: 'overlay-ref:provider-key-form',
    source: 'desktop/src/admin-keys.ts, desktop/src-tauri/src/admin.rs',
    interaction: 'Provider keys: s types a SYNTHETIC key into the write-only form; rsid stores only a fingerprint; dd (twice) clears it',
    async run(h) {
      const { page } = h;
      await h.palette('Provider keys');
      if (!(await page.waitFor(`${'(document.querySelector(\'dialog[open]\')||{}).innerText?.includes("Provider keys")'}`, 5000))) return fail(`the Provider keys dialog did not open: ${(await h.dialog()).slice(0, 80)}`);
      const slot = await page.eval(`(document.querySelector('dialog[open] .sel')||{}).innerText||''`);
      await page.key('s');
      if (!(await page.waitFor(`document.querySelector('dialog[open] input[type=password], dialog[open] input')`, 4000))) return fail('s did not open the key form');
      await page.element('dialog[open] input', '(e) => e.focus()', { step: 'focus provider credential' });
      await page.keys(SYNTHETIC_KEY);
      const typed = await page.element('dialog[open] input', '(e) => e.value');
      await page.key('Enter');
      const set = await (async () => {
        for (let i = 0; i < 30; i++) {
          const l = await h.rpc('ListProviderCredentials', null);
          const c = l.credentials.find((x) => x.state !== 'absent' && x.state !== 'cleared');
          if (c) return c;
          await sleep(200);
        }
        return null;
      })();
      if (typed !== SYNTHETIC_KEY) return fail('the synthetic key was not typed into the form');
      if (!set) return fail('rsid holds no credential after the save');
      // The offline rsid (#130) still runs the key check, which short-circuits to `offline_unchecked` without a
      // request; the egress guard (smoke-egress.mjs) requires that detail whenever a key was set.
      let keyCheck = set.check?.detail_code ?? null;
      for (let i = 0; i < 25 && !keyCheck; i++) {
        await sleep(200);
        keyCheck = (await h.rpc('ListProviderCredentials', null)).credentials.find((x) => x.slot === set.slot)?.check?.detail_code ?? null;
      }
      const leaked = JSON.stringify({ log: h.bridgeLog, dom: await page.eval('document.body.innerText'), meta: set }).includes(SYNTHETIC_KEY);
      // Re-open the dialog and clear the slot with the two-step dd.
      await h.closeAll();
      await h.palette('Provider keys');
      await page.waitFor(`(document.querySelector('dialog[open]')||{}).innerText?.includes('Provider keys')`, 5000);
      const row = await page.eval(`(document.querySelector('dialog[open] .sel')||{}).innerText||''`);
      await page.key('d'); await sleep(300);
      const armed = await page.element('dialog[open]', `(e) => e.innerText.includes('Press d again')`);
      const stillSet = (await h.rpc('ListProviderCredentials', null)).credentials.some((x) => x.slot === set.slot && x.state !== 'absent' && x.state !== 'cleared');
      await page.key('d'); await sleep(900);
      const cleared = (await h.rpc('ListProviderCredentials', null)).credentials.find((x) => x.slot === set.slot);
      await h.closeAll();
      if (leaked) return fail('the synthetic key value appeared in the page, the bridge log or the metadata');
      if (!armed || !stillSet) return fail(`the first d must only arm the confirmation (armed ${armed}, still set ${stillSet})`);
      return cleared?.state === 'absent' || cleared?.state === 'cleared'
        ? pass(`${set.slot}: set -> fingerprint ${set.fingerprint} only (key check ${keyCheck ?? 'not recorded'}); the first d armed a confirmation, the second cleared it (state ${cleared?.state})`, { slot: set.slot, keyCheck, selectedRow: slot.split('\n')[0] || row.split('\n')[0] })
        : fail(`the slot is still ${cleared?.state}`);
    },
  },
  {
    id: 'input-bar', section: 'Input Bar Keybindings (Session Detail View)', parityRow: 'section:insert-mode',
    source: 'desktop/src/actions/input.ts, desktop/src/vim-field.ts, desktop/src-tauri/src/input.rs',
    interaction: 'In an open session, i enters the message box and `/` lists the slash commands rsid reports; Ctrl+Enter keeps the draft',
    async run(h) {
      const { page } = h;
      await openLeaf(h);
      await page.key('i'); await sleep(300);
      if (!(await page.eval(`document.activeElement.id === 'message'`))) return fail('i did not focus the message box');
      await page.keys('/');
      if (!(await page.waitFor(`(document.getElementById('g1-suggest')||{}).innerText?.includes('e2e-hello')`, 6000))) return fail('the slash suggestions never listed the project command from rsid');
      const listed = (await h.text('#g1-suggest')).split('\n')[0];
      await page.keys('e2e'); await sleep(300);
      await page.key('Enter', { ctrl: true }); await sleep(800);
      // Sending would start a provider turn (ContinueSession): the isolated mode refuses it and records why.
      const refused = h.refused('send_message');
      const draft = await page.eval(`document.getElementById('message').value`);
      const evidence = { suggestion: listed, draft, sendRefused: refused.length };
      return refused.length === 1 && draft.length > 0
        ? { status: 'pass', note: `\`/\` listed ${listed} from rsid; Ctrl+Enter reached send_message, which the harness refused (a send starts a provider turn: skipped by design), and the draft was kept`, evidence: { ...evidence, skipped: 'send: ContinueSession launches a provider CLI' } }
        : fail(`send_message refusals ${refused.length}, draft ${JSON.stringify(draft)}`, evidence);
    },
  },
  {
    id: 'vim-text', section: 'Vim Text Editing (Input Bar & Overlay)', parityRow: 'section:operators-text-objects',
    source: 'desktop/src/vim-editor.ts, desktop/src/vim-field.ts, desktop/src/actions/input.ts',
    interaction: 'In the message box of a session read from rsid: insert text, Esc, 0 dw deletes a word, u undoes it',
    async run(h) {
      const { page } = h;
      await openLeaf(h);
      await page.key('i'); await sleep(250);
      await page.keys('hello world'); await sleep(150);
      await page.key('Escape'); await sleep(200);
      const normal = await page.eval(`(document.querySelector('.composer')||{}).innerText.includes('NORMAL')`);
      await page.keys('0dw'); await sleep(200);
      const deleted = await page.eval(`document.getElementById('message').value`);
      await page.keys('u'); await sleep(200);
      const undone = await page.eval(`document.getElementById('message').value`);
      return normal && deleted === 'world' && undone === 'hello world'
        ? pass('insert, Esc (NORMAL), 0 dw -> "world", u -> "hello world" in the session\'s message box', { deleted, undone })
        : fail(`normal ${normal}, after dw ${JSON.stringify(deleted)}, after u ${JSON.stringify(undone)}`);
    },
  },
  {
    id: 'session-detail', section: 'Session Detail and Recent Files', parityRow: 'section:recent-files',
    source: 'desktop/src/actions/input.ts, desktop/src/input-model.ts, desktop/src/files.ts',
    interaction: 'gf1 opens the newest file the session\'s Edit event touched (src/lib.rs) in the viewer, read from the synthetic project through rsid',
    async run(h) {
      const { page } = h;
      await openLeaf(h);
      await page.keys('gf1');
      if (!(await page.waitFor(`(document.querySelector('.files-overlay')||{}).innerText?.includes('pub fn answer')`, 8000))) return fail(`gf1 did not open src/lib.rs: ${(await h.text('#toast')) || (await h.text('.files-overlay')).slice(0, 100)}`);
      const read = h.bridgeLog.some((e) => e.cmd === 'read_project_file' && e.outcome === 'forwarded');
      return read ? pass('gf1 opened src/lib.rs (the file the Edit tool event named); its content was read from rsid') : fail('the viewer opened without a read_project_file');
    },
  },
  {
    id: 'notes', section: 'Notes', parityRow: 'section:modal-toggle-behavior',
    source: 'desktop/src/actions/input.ts, desktop/src/input-model.ts, desktop/src/pickers.ts',
    interaction: '<Space>p opens the project picker listing rsid\'s project (its filter field keeps a second <Space>p as text); <Space>s opens the sort picker and a second <Space>s closes it',
    async run(h) {
      const { page } = h;
      await page.keys(' p');
      if (!(await page.waitFor(`(document.querySelector('dialog[open]')||{}).innerText?.includes('e2e-project')`, 5000))) return fail(`the project picker did not list e2e-project: ${(await h.dialog()).slice(0, 80)}`);
      await page.keys(' p'); await sleep(400);
      const stayed = await page.eval(`document.querySelector('dialog[open]') !== null`);
      await h.closeAll();
      await page.keys(' s');
      if (!(await page.waitFor(`document.querySelector('dialog[open]') !== null`, 4000))) return fail('<Space>s did not open the sort picker');
      await page.keys(' s'); await sleep(500);
      const closed = await page.eval(`document.querySelector('dialog[open]') === null`);
      if (!stayed) return fail('a second <Space>p typed into the picker\'s text field closed it');
      return closed ? pass('<Space>p listed the project from rsid and kept a second <Space>p as filter text; <Space>s opened the sort picker and <Space>s again closed it') : fail('pressing <Space>s again did not close the sort picker');
    },
  },
  {
    id: 'tui-view-modules', section: 'TUI view modules', parityRow: 'module:ui/issues.rs',
    source: 'desktop/src/issues.ts, desktop/src-tauri/src/issues.rs',
    interaction: 'Issue create, edit, cancel, archive and restore through the Issues workspace, each checked in rsid',
    async run(h) {
      const steps = await issueLifecycle(h);
      const bad = steps.filter((s) => s.status !== 'pass');
      return { status: bad.length ? 'fail' : 'pass', note: bad.length ? `${bad.length} lifecycle step(s) failed: ${bad[0].id}: ${bad[0].note}` : 'create, edit, cancel, archive, restore all held in rsid', steps };
    },
  },
];

/** Not a PARITY section: the hub #3101 gap and the Settings write. */
export const E2E_EXTRAS = [
  {
    extra: true,
    id: 'vim-keys-toggle', section: '(extra) hub #3101: the Vim keys switch as real DOM key events', parityRow: 'section:standard-editing-mode',
    source: 'desktop/src/vimkeys.ts, desktop/src/modal-gate.ts, desktop/src/issues.ts',
    interaction: 'Space on the Settings row turns Vim keys off and Ctrl+K "Vim keys" Enter turns it on; with it off the Issues workspace ignores n and j and the command line but keeps the arrows',
    async run(h) {
      const { page } = h;
      const steps = [];
      const step = async (id, title, run) => {
        let r;
        try { r = await run(); } catch (e) { r = fail(`threw: ${e.message}`); }
        r.shot = await page.shot(`${h.out}/vim-${id}.png`);
        steps.push({ id: `vim-${id}`, title, ...r });
        return r;
      };
      const formOpen = `document.querySelector('#issue-dialog[open]') !== null`;
      const issuesOpen = async () => {
        if (await page.eval(`document.getElementById('issues-view').hidden`)) await page.click(`document.getElementById('open-issues')`);
        if (!(await page.waitFor(`!document.getElementById('issues-view').hidden && document.getElementById('issues-list').innerText.includes('Seed Issue')`, 8000))) throw new Error(`the Issues workspace did not list the seeded Issues: ${JSON.stringify(await h.text('#issues-status'))}`);
        await sleep(300);
        await page.eval(`document.activeElement && document.activeElement.blur()`);
        await page.element('#issues-list', '(e) => e.focus()', { step: 'focus Issues list' });
      };
      await h.openIssues();
      await step('on', 'Vim keys on: n opens the New Issue form and : the command line', async () => {
        if (!(await vimOn(page))) return fail('the Vim keys switch starts off');
        await page.key('n');
        const opened = await page.waitFor(formOpen, 3000);
        await h.closeAll();
        await page.keys(':'); await sleep(300);
        const cmd = await page.eval(`!document.getElementById('cmdline').hidden`);
        await h.closeAll();
        return opened && cmd ? pass('n opened the Issue form and : the command line with Vim keys on', { stored: await stored(page) }) : fail(`n opened ${opened}, command line ${cmd}`);
      });
      await step('settings-off', 'Settings: search "vim keys", Enter, Space turns the switch off (real key events)', async () => {
        await page.keys(':set'); await page.key('Enter'); await sleep(900);
        if (!(await page.waitFor(`!document.getElementById('settings-view').hidden`, 4000))) return fail('Settings did not open');
        await page.key('/'); await sleep(200);
        await page.keys('vim keys'); await sleep(300);
        await page.key('Enter'); await sleep(400);
        // The search lands on the first hit (the daemon's Editing mode row); n steps to the next match.
        for (let i = 0; i < 4 && !(await page.eval(`(document.querySelector('#settings-view .settings-row.sel')||{}).innerText?.includes('Vim keys (this desktop)')`)); i++) { await page.key('n'); await sleep(300); }
        if (!(await page.eval(`(document.querySelector('#settings-view .settings-row.sel')||{}).innerText?.includes('Vim keys (this desktop)')`))) return fail('the search never reached the Vim keys row');
        await page.key(' '); await sleep(500);
        const off = !(await vimOn(page));
        const persisted = await stored(page);
        await h.closeAll();
        return off && persisted === 'off' ? pass('Space on the Vim keys row turned the switch off; localStorage holds off', { stored: persisted }) : fail(`switch off ${off}, stored ${persisted}`);
      });
      await step('off', 'Vim keys off: n and j do nothing, : opens no command line, ArrowDown still moves the cursor', async () => {
        await issuesOpen();
        const rowAt = () => page.eval(`[...document.querySelectorAll('#issues-list li')].findIndex((r) => r.classList.contains('selected') || r.getAttribute('aria-selected') === 'true')`);
        for (let i = 0; i < 4; i++) { await page.key('ArrowUp'); await sleep(120); }
        const start = await rowAt();
        await page.key('n'); await sleep(500);
        const nOpened = await page.eval(formOpen);
        if (nOpened) await page.key('Escape');
        await page.key('j'); await sleep(300);
        const afterJ = await rowAt();
        await page.key('ArrowDown'); await sleep(300);
        const afterArrow = await rowAt();
        await page.keys(':'); await sleep(300);
        const cmdline = await page.eval(`!document.getElementById('cmdline').hidden`);
        if (cmdline) await page.key('Escape');
        if (nOpened) return fail('n opened the Issue form with Vim keys off');
        if (afterJ !== start) return fail(`j moved the cursor ${start} -> ${afterJ} with Vim keys off`);
        if (afterArrow === start) return fail(`ArrowDown no longer moves the cursor with Vim keys off (rows ${await page.eval(`document.querySelectorAll('#issues-list li').length`)}, start ${start}, after j ${afterJ}, after ArrowDown ${afterArrow}, vim ${await vimOn(page)}, focus ${await page.eval('document.activeElement.tagName')})`);
        if (cmdline) return fail(': opened the command line with Vim keys off');
        return pass(`n opened nothing, j stayed on row ${start}, ArrowDown moved to ${afterArrow}, : opened no command line`);
      });
      await step('palette-on', 'Ctrl+K, type "Vim keys", Enter turns the switch back on; n opens the form again', async () => {
        await page.key('k', { ctrl: true }); await sleep(500);
        if (!(await page.eval(`document.getElementById('palette').open`))) return fail('Ctrl+K did not open the command palette');
        await page.keys('Vim keys'); await sleep(300);
        await page.key('Enter'); await sleep(600);
        if (!(await vimOn(page))) return fail('the palette action did not turn Vim keys on');
        await issuesOpen();
        await page.key('n');
        const opened = await page.waitFor(formOpen, 3000);
        await h.closeAll();
        const persisted = await stored(page);
        return opened && persisted === 'on' ? pass('the palette turned Vim keys on; n opens the form again; localStorage holds on', { stored: persisted }) : fail(`n opened ${opened}, stored ${persisted}`);
      });
      const bad = steps.filter((s) => s.status !== 'pass');
      return { status: bad.length ? 'fail' : 'pass', note: bad.length ? `${bad[0].id}: ${bad[0].note}` : 'the switch went off (Settings row) and on (palette) as real key events; the Issues pane honoured it', steps };
    },
  },
  {
    extra: true,
    id: 'settings-write', section: '(extra) Settings boot and a synthetic write', parityRow: 'overlay:settings-pane-space-or-set',
    source: 'desktop/src/settings.ts, desktop/src/actions/settings.ts, desktop/src-tauri/src/settings.rs',
    interaction: ':set boots from rsid\'s config; the Retry on failure switch is toggled by keys and applied with w (UpdateDaemonConfig)',
    async run(h) {
      const { page } = h;
      const read = async () => (await h.rpc('GetDaemonConfig', {})).retry_enabled;
      const before = await read();
      await h.command('set');
      if (!(await page.waitFor(`!document.getElementById('settings-view').hidden && document.getElementById('settings-view').innerText.includes('AGENT AUTOMATION')`, 5000))) return fail('Settings did not boot from rsid');
      await page.key('/'); await sleep(200);
      await page.keys('retry on failure'); await sleep(300);
      await page.key('Enter'); await sleep(400);
      await page.key(' '); await sleep(400);
      const pending = await page.eval(`document.getElementById('settings-view').innerText.includes('1 pending')`);
      await page.key('w');
      let after = before;
      for (let i = 0; i < 20 && after === before; i++) { await sleep(200); after = await read(); }
      await h.closeAll();
      return pending && after === !before ? pass(`Retry on failure ${before} -> ${after} in rsid after Space and w (UpdateDaemonConfig)`, { before, after }) : fail(`pending ${pending}, before ${before}, after ${after}`);
    },
  },
  {
    extra: true,
    id: 'settings-state', section: '(extra) Settings daemon-state rows (local #125)', parityRow: 'overlay:settings-pane-space-or-set',
    source: 'desktop/src/settings-state.ts, desktop/src/settings-model.ts, desktop/src-tauri/src/settings_state.rs',
    interaction: 'the status rows load their line from rsid; the Model control mode row is stepped with Space and applied with w (UpdateModelControlPolicy), read back from rsid; a TUI-only row says so',
    async run(h) {
      const { page } = h;
      const mode = async () => (await h.rpc('GetModelControlStatus', { recent_limit: 0 })).mode;
      const before = await mode();
      await h.command('set');
      if (!(await page.waitFor(`!document.getElementById('settings-view').hidden && document.getElementById('settings-view').innerText.includes('AGENT AUTOMATION')`, 5000))) return fail('Settings did not boot from rsid');
      const rowLabel = () => page.eval(`(document.querySelector('.settings-row.sel .sv-label') || {}).textContent || ''`);
      const rowText = () => page.eval(`(document.querySelector('.settings-row.sel') || {}).innerText || ''`);
      const jump = async (query, title) => {
        await page.key('/'); await sleep(200); await page.keys(query); await sleep(300); await page.key('Enter'); await sleep(500);
        // A summary can match too: n walks the hits until the row is the one named.
        for (let i = 0; i < 12 && title && (await rowLabel()) !== title; i++) { await page.key('n'); await sleep(250); }
      };
      const observed = {};
      for (const [key, query, title] of [['storage', 'sandbox storage', 'Sandbox storage'], ['pools', 'warm build pools', 'Warm build pools'], ['cold', 'cold-storage status', 'Cold-storage status'], ['usage', 'usage and telemetry', 'Usage and telemetry']]) {
        await jump(query, title);
        await sleep(600);
        observed[key] = (await rowText()).replace(/\s+/g, ' ').trim();
        if ((await rowLabel()) !== title) return fail(`the search for "${query}" landed on '${observed[key]}', not the ${title} row`, { observed });
      }
      await jump('submit on enter', 'Submit on Enter');
      observed.tuiOnly = (await rowText()).replace(/\s+/g, ' ').trim();
      await jump('model control mode', 'Model control mode');
      const modeRow = (await rowText()).replace(/\s+/g, ' ').trim();
      await page.key(' '); await sleep(400);
      const pending = await page.eval(`document.getElementById('settings-view').innerText.includes('1 pending')`);
      await page.key('w');
      let after = before;
      for (let i = 0; i < 20 && after === before; i++) { await sleep(200); after = await mode(); }
      await h.closeAll();
      const loaded = Object.entries(observed).filter(([k]) => k !== 'tuiOnly').every(([, t]) => t !== '' && !t.endsWith('…') && !t.includes('Enter opens ▸'));
      const evidence = { before, after, modeRow, observed };
      if (!loaded) return fail('a daemon-state row never loaded its line', evidence);
      if (!observed.tuiOnly.includes('TUI only')) return fail('the TUI-only row does not say so', evidence);
      return pending && after !== before && after === 'pause_background'
        ? pass(`Model control mode ${before} -> ${after} in rsid after Space and w; the status rows loaded; Submit on Enter reads TUI only`, evidence)
        : fail(`pending ${pending}, before ${before}, after ${after}`, evidence);
    },
  },
  {
    extra: true,
    id: 'files-edit', section: '(extra) #113: the file editor and the explorer changes against rsid', parityRow: 'overlay:file-viewer-opened-from-file-explorer',
    source: 'desktop/src/files.ts, desktop/src/files-ops.ts, desktop/src/files-edit-model.ts, desktop/src/files-editor-model.ts, desktop/src-tauri/src/files.rs',
    interaction: 'A edits README.md, :w saves it (WriteProjectFile), an outside change makes :w refuse, :w! forces, :e! reloads, Ctrl-S and :wq save; in the tree a creates, r renames, c copies, d trashes and u undoes, and a path outside the root is refused; while editing, colours, search hits, bracket match and folds show, brackets auto-pair, a mixed line-ending file keeps each line\'s break and Ctrl-V pastes the clipboard; every end state is read back from the isolated rsid',
    async run(h) {
      const { page } = h;
      const steps = [];
      const root = h.seed.root;
      const proj = { project_id: h.seed.projectId };
      const step = async (id, title, run) => {
        let r;
        try { r = await run(); } catch (e) { r = fail(`threw: ${e.message}`); }
        r.shot = await page.shot(`${h.out}/files-${id}.png`).catch(() => null);
        steps.push({ id: `files-${id}`, title, ...r });
        return r;
      };
      const read = (path) => h.rpc('ReadProjectFile', { ...proj, path });
      const list = async (path) => (await h.rpc('ListProjectDirectory', { ...proj, path })).entries.map((e) => e.name);
      const toastHas = (re) => page.waitFor(`/${re}/.test((document.getElementById('toast')||{}).innerText||'')`, 4000);
      const area = () => page.eval(`(document.querySelector('.files-edit')||{}).value ?? null`);
      const meta = () => page.eval(`(document.querySelector('.files-viewer .files-meta')||{}).innerText||''`);
      const colon = async (cmd) => { await page.keys(':'); await sleep(250); await page.keys(cmd); await page.key('Enter'); await sleep(700); };
      const selectInTree = async (name) => {
        for (let i = 0; i < 30; i++) {
          const sel = await page.eval(`(document.querySelector('.files-row.selected .files-name')||{}).innerText||''`);
          if (sel === name || sel === `${name}/`) return true;
          await page.key('j'); await sleep(120);
        }
        return false;
      };
      const prompt = async (key, text) => {
        await page.keys(key);
        if (!(await page.waitFor(`!document.querySelector('.files-prompt').hidden`, 3000))) {
          throw new Error(`${key} opened no prompt (focus ${await page.eval(`document.activeElement.tagName + '.' + document.activeElement.className`)}; toast ${JSON.stringify(await h.text('#toast'))})`);
        }
        if (text !== undefined) await page.keys(text);
      };
      /** Poll rsid until `pick(await read)` is `want` (a save is asynchronous; a toast can be an older one). */
      const eventually = async (path, want) => { let c = ''; for (let i = 0; i < 25; i++) { c = (await read(path)).content; if (c === want) return c; await sleep(200); } return c; };
      const treeHas = (name) => page.eval(`[...document.querySelectorAll('.files-row .files-name')].some((n) => n.innerText === ${JSON.stringify(name)})`);

      await openLeaf(h);
      await page.keys(' e');
      if (!(await page.waitFor(`(document.querySelector('.files-overlay')||{}).innerText?.includes('README.md')`, 6000))) return fail('the explorer did not list README.md');
      const original = (await read('README.md')).content;

      await step('save', 'A appends at the end of the line, Esc, :w saves it; rsid reads the new text back', async () => {
        if (!(await selectInTree('README.md'))) return fail('could not select README.md');
        await page.key('Enter');
        if (!(await page.waitFor(`(document.querySelector('.files-viewer')||{}).innerText?.includes('synthetic project')`, 6000))) return fail('the viewer did not open README.md');
        await page.key('A', { shift: true }); await sleep(400);
        if (!(await page.waitFor(`!!document.querySelector('.files-edit')`, 3000))) return fail(`A did not open the editor: ${await h.text('#toast')}`);
        await page.keys(' edited'); await page.key('Escape'); await sleep(200);
        const dirty = (await meta()).includes('[+] unsaved');
        await colon('w');
        if (!(await toastHas('Saved: README.md'))) return fail(`:w did not report a save: ${JSON.stringify(await h.text('#toast'))}; meta ${await meta()}`);
        const disk = await read('README.md');
        const want = original.replace('# synthetic project\n', '# synthetic project edited\n');
        const clean = !(await meta()).includes('[+]');
        return dirty && clean && disk.content === want && disk.version
          ? pass('A, " edited", Esc, :w: the buffer was unsaved, then clean; rsid reads the edited README.md back with a new version', { line: disk.content.split('\n')[0], version: disk.version })
          : fail(`dirty ${dirty}, clean ${clean}, disk ${JSON.stringify(disk.content)}`);
      });

      await step('stale', 'a change from outside makes :w refuse with the changed-on-disk message; rsid keeps the outside text', async () => {
        const theirs = '# changed by someone else\n';
        writeFileSync(join(root, 'README.md'), theirs);
        await page.key('A', { shift: true }); await sleep(200);
        await page.keys(' more'); await page.key('Escape'); await sleep(200);
        await colon('w');
        if (!(await toastHas('Save refused'))) return fail(`:w was not refused: ${JSON.stringify(await h.text('#toast'))}`);
        const text = await h.text('#toast');
        const disk = (await read('README.md')).content;
        const conflict = (await meta()).includes('changed on disk');
        return text.includes('file changed on disk: README.md') && text.includes(':w!') && text.includes(':e!') && disk === theirs && conflict
          ? pass('the stale :w was refused with the TUI wording and left the outside text in rsid', { toast: text })
          : fail(`toast ${JSON.stringify(text)}, disk ${JSON.stringify(disk)}, conflict ${conflict}`);
      });

      await step('force', ':w! overwrites the outside change; rsid reads the buffer back', async () => {
        const buffer = await area();
        await colon('w!');
        if (!(await toastHas('overwrote the disk copy'))) return fail(`:w! did not report an overwrite: ${JSON.stringify(await h.text('#toast'))}`);
        const disk = (await read('README.md')).content;
        return disk === buffer && buffer.includes('more')
          ? pass(':w! replaced the outside text with the buffer', { first: disk.split('\n')[0] })
          : fail(`disk ${JSON.stringify(disk)}, buffer ${JSON.stringify(buffer)}`);
      });

      await step('reload', ':e! discards the buffer and shows the outside text', async () => {
        const theirs = 'outside two\n';
        writeFileSync(join(root, 'README.md'), theirs);
        await page.key('A', { shift: true }); await sleep(200);
        await page.keys('zzz'); await page.key('Escape'); await sleep(200);
        await colon('e!');
        const ok = await page.waitFor(`(document.querySelector('.files-edit')||{}).value === ${JSON.stringify(theirs)}`, 5000);
        const clean = !(await meta()).includes('[+]');
        const disk = (await read('README.md')).content;
        return ok && clean && disk === theirs ? pass(':e! reloaded the outside text and dropped the buffer; rsid still holds it', { buffer: await area() }) : fail(`reloaded ${ok}, clean ${clean}, disk ${JSON.stringify(disk)}`);
      });

      await step('ctrl-s', 'Ctrl-S saves; :wq saves and closes the viewer', async () => {
        await page.key('A', { shift: true }); await sleep(200);
        await page.keys('!'); await page.key('Escape'); await sleep(200);
        await page.key('s', { ctrl: true });
        const first = await eventually('README.md', 'outside two!\n');
        await page.key('A', { shift: true }); await sleep(200);
        await page.keys('?'); await page.key('Escape'); await sleep(200);
        await colon('wq');
        const closed = await page.waitFor(`document.querySelector('.files-viewer')?.hidden !== false`, 4000);
        const second = (await read('README.md')).content;
        return first === 'outside two!\n' && second === 'outside two!?\n' && closed
          ? pass('Ctrl-S wrote "outside two!"; :wq wrote "outside two!?" and closed the viewer', { first, second })
          : fail(`first ${JSON.stringify(first)}, second ${JSON.stringify(second)}, closed ${closed}`);
      });

      await step('draft', 'an unsaved buffer survives opening another file (a dot in the tree), :q refuses it and :q! discards it', async () => {
        const row = (name) => `[...document.querySelectorAll('.files-row')].find((r) => r.querySelector('.files-name').innerText === ${JSON.stringify(name)})`;
        await page.eval(`document.querySelector('.files-tree').focus()`);
        if (!(await selectInTree('README.md'))) return fail('could not select README.md');
        await page.key('Enter');
        if (!(await page.waitFor(`(document.querySelector('.files-viewer')||{}).innerText?.includes('outside two')`, 6000))) return fail('the viewer did not open README.md');
        await page.key('A', { shift: true }); await sleep(300);
        await page.keys('zz'); await page.key('Escape'); await sleep(200);
        // Mouse: expand src/, then open lib.rs (a second click on a row activates it).
        await page.click(row('src/')); await sleep(300); await page.click(row('src/')); await sleep(700);
        await page.click(row('lib.rs')); await sleep(300); await page.click(row('lib.rs')); await sleep(900);
        const other = await page.waitFor(`(document.querySelector('.files-viewer')||{}).innerText?.includes('pub fn answer')`, 4000);
        const dot = await page.eval(`!!${row('README.md')}?.querySelector('.files-draft')`);
        await page.click(row('README.md')); await sleep(300); await page.click(row('README.md')); await sleep(900);
        const back = await page.waitFor(`((document.querySelector('.files-edit')||{}).value || '').includes('zz')`, 4000);
        const unsaved = (await meta()).includes('[+] unsaved');
        await colon('q');
        const refused = (await h.text('#toast')).includes('No write since last change');
        const stillOpen = await page.eval(`document.querySelector('.files-viewer')?.hidden === false`);
        await colon('q!');
        const closed = await page.waitFor(`document.querySelector('.files-viewer')?.hidden !== false`, 3000);
        const disk = (await read('README.md')).content;
        const dotGone = await page.eval(`!${row('README.md')}?.querySelector('.files-draft')`);
        return other && dot && back && unsaved && refused && stillOpen && closed && disk === 'outside two!?\n' && dotGone
          ? pass('the dirty README buffer was kept as a draft (dot in the tree), came back with its changes, :q refused, :q! closed without writing', { disk })
          : fail(`other ${other}, dot ${dot}, back ${back}, unsaved ${unsaved}, :q refused ${refused}, still open ${stillOpen}, closed ${closed}, disk ${JSON.stringify(disk)}, dot gone ${dotGone}`);
      });

      await step('add', 'a creates notes/todo.txt (parents too); r renames, c copies; rsid lists each result', async () => {
        await page.eval(`document.querySelector('.files-tree').focus()`);
        await sleep(200);
        if (!(await selectInTree('README.md'))) return fail('could not select README.md');
        await prompt('a', 'notes/todo.txt'); await page.key('Enter'); await sleep(900);
        const made = await list('notes');
        await prompt('r');
        await page.key('u', { ctrl: true }); await page.keys('tasks'); await page.key('Enter'); await sleep(900);
        const renamed = await list('notes');
        await prompt('c');
        await page.key('e', { ctrl: true }); await page.keys('.bak'); await page.key('Enter'); await sleep(900);
        const copied = await list('notes');
        return made.join() === 'todo.txt' && renamed.join() === 'tasks.txt' && copied.join() === 'tasks.txt,tasks.txt.bak'
          ? pass('a, r (caret before the extension) and c each landed in rsid', { made, renamed, copied })
          : fail(`made ${made}, renamed ${renamed}, copied ${copied}`);
      });

      await step('refuse', 'a name outside the root and an existing name are refused with the TUI wording and change nothing', async () => {
        if (!(await selectInTree('README.md'))) return fail('could not select README.md');
        await prompt('a', '../escape.txt'); await page.key('Enter'); await sleep(500);
        const escape = await h.text('#toast');
        await page.key('Escape'); await sleep(200);
        await prompt('a', 'README.md'); await page.key('Enter'); await sleep(700);
        const exists = await h.text('#toast');
        await page.key('Escape'); await sleep(200);
        const top = await list('');
        const sibling = existsSync(join(root, '..', 'escape.txt'));
        return escape.includes('Path must stay inside') && exists.includes('README.md already exists') && !top.includes('escape.txt') && !sibling
          ? pass('"../escape.txt" and "README.md" were refused; nothing was created', { escape, exists })
          : fail(`escape ${JSON.stringify(escape)}, exists ${JSON.stringify(exists)}, root ${top}, sibling ${sibling}`);
      });

      await step('trash-undo', 'd asks, y moves src/lib.rs to the trash, u brings it back byte for byte', async () => {
        const before = await read('src/lib.rs');
        await page.key('g'); await sleep(100);
        if (!(await selectInTree('src'))) return fail('could not select src');
        await page.key('l'); await sleep(700);
        if (!(await selectInTree('lib.rs'))) return fail('could not select src/lib.rs');
        await page.keys('d');
        if (!(await page.waitFor(`!document.querySelector('.files-prompt').hidden && document.querySelector('.files-prompt-label').innerText.includes('Delete lib.rs?')`, 3000))) return fail('d did not ask to delete lib.rs');
        const nothingYet = (await list('src')).includes('lib.rs');
        await page.keys('y'); await sleep(900);
        const gone = !(await list('src')).includes('lib.rs');
        await page.keys('u'); await sleep(900);
        const back = await read('src/lib.rs');
        return nothingYet && gone && back.content === before.content && back.content.includes('41')
          ? pass('lib.rs left src after y and came back unchanged after u', { bytes: back.size, version: back.version })
          : fail(`kept until confirmed ${nothingYet}, gone ${gone}, restored ${JSON.stringify(back.content)}`);
      });

      await step('mouse', 'the toolbar adds a folder and the right-click menu deletes it; Undo brings it back', async () => {
        if (!(await selectInTree('README.md'))) return fail('could not select README.md');
        await page.click(`document.querySelector('.files-op[data-op="mkdir"]')`);
        if (!(await page.waitFor(`!document.querySelector('.files-prompt').hidden`, 3000))) return fail('the + Folder button opened no prompt');
        await page.keys('docs'); await page.key('Enter'); await sleep(900);
        const made = (await list('')).includes('docs');
        await page.eval(`(() => { const r = [...document.querySelectorAll('.files-row')].find((x) => x.innerText.includes('docs')); const b = r.getBoundingClientRect(); r.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: b.x + 20, clientY: b.y + 5 })); })()`);
        if (!(await page.waitFor(`!!document.querySelector('.files-menu')`, 3000))) return fail('the right-click menu did not open');
        await page.click(`[...document.querySelectorAll('.files-menu-item')].find((i) => i.innerText.includes('Delete'))`);
        await page.waitFor(`!document.querySelector('.files-prompt').hidden`, 3000);
        await page.click(`document.querySelector('.files-prompt-buttons .danger')`);
        await sleep(900);
        const gone = !(await list('')).includes('docs');
        await page.click(`document.querySelector('.files-op[data-op="undo"]')`); await sleep(900);
        const back = (await list('')).includes('docs');
        return made && gone && back ? pass('+ Folder created docs/, the context-menu Delete trashed it, Undo restored it', { made, gone, back }) : fail(`made ${made}, gone ${gone}, back ${back}`);
      });

      // ---- #134: what the viewer has while editing ----
      const lines = () => page.eval(`document.querySelectorAll('.files-body .files-line').length`);
      const has = (sel) => page.eval(`!!document.querySelector(${JSON.stringify(sel)})`);
      const count = (sel) => page.eval(`document.querySelectorAll(${JSON.stringify(sel)}).length`);
      const openFromTree = async (name, parent) => {
        await page.eval(`document.querySelector('.files-tree').focus()`);
        await sleep(200);
        await page.key('g'); await sleep(100);
        if (parent) {
          if (!(await selectInTree(parent))) return false;
          const open = await page.eval(`[...document.querySelectorAll('.files-row')].some((r) => r.querySelector('.files-name').innerText === ${JSON.stringify(name)})`);
          if (!open) { await page.key('l'); await sleep(700); }
        }
        if (!(await selectInTree(name))) return false;
        await page.key('Enter'); await sleep(500);
        return true;
      };

      await step('editor-view', 'while editing: syntax colours, search hits, bracket match and folds work, and the caret skips a closed fold', async () => {
        if (!(await openFromTree('lib.rs', 'src'))) return fail('could not open src/lib.rs');
        if (!(await page.waitFor(`(document.querySelector('.files-viewer')||{}).innerText?.includes('pub fn answer')`, 6000))) return fail('the viewer did not open lib.rs');
        await page.key('i'); await sleep(300);
        if (!(await page.waitFor(`!!document.querySelector('.files-edit')`, 3000))) return fail('i did not open the editor');
        await page.key('Escape'); await sleep(200);
        const colours = await has('.files-body.editing .files-text .hl-keyword');
        // Search: / answer Enter highlights the hit; n / N step.
        await page.keys('/'); await sleep(200); await page.keys('answer'); await sleep(200); await page.key('Enter'); await sleep(300);
        const hit = (await has('.files-text .v-current')) && /\/answer 1\/1/.test(await meta());
        // Bracket match: the cursor on "(" marks the pair.
        await page.keys('gg'); await sleep(100); await page.keys('13l'); await sleep(300);
        const bracket = (await count('.files-text .v-bracket')) >= 2;
        // Folds: zc closes the function, j steps over it, zo opens it.
        await page.keys('gg'); await page.keys('zc'); await sleep(300);
        const closed = (await has('.files-fold-summary')) && (await lines()) === 2 && /1 fold closed/.test(await meta());
        await page.keys('j'); await sleep(300);
        const stepped = /4:1/.test(await meta());
        await page.keys('k'); await sleep(300);
        const back = /· 1:/.test(await meta());
        await page.keys('zo'); await sleep(300);
        const opened = (await lines()) === 4 && !(await has('.files-fold-summary'));
        return colours && hit && bracket && closed && stepped && back && opened
          ? pass('in the editor lib.rs showed keyword colours, /answer lit 1/1 hits, the cursor on "(" marked its pair, zc folded the function (j stepped over it) and zo opened it', { meta: await meta() })
          : fail(`colours ${colours}, hit ${hit}, bracket ${bracket}, closed ${closed}, stepped ${stepped}, back ${back}, opened ${opened}; meta ${await meta()}`);
      });

      await step('autopair', 'typing ( [ { pairs them, a closer types over its closer, Backspace removes an empty pair, :set noautopair stops it; :w saves; the gutter marks the new lines', async () => {
        await page.keys('G'); await sleep(100);
        await page.keys('o'); await sleep(200);
        await page.keys('foo(x) {'); await sleep(200);
        const typed = await area();
        const paired = typed.endsWith('\nfoo(x) {}');
        await page.key('Backspace'); await sleep(150);
        const unpaired = (await area()).endsWith('\nfoo(x) ');
        await page.key('Escape'); await sleep(200);
        await colon('set noautopair');
        const off = (await h.text('#toast')).includes('Auto-pair: off');
        await page.keys('o'); await sleep(200); await page.keys('('); await sleep(200);
        const plain = (await area()).endsWith('\nfoo(x) \n(');
        await page.key('Escape'); await sleep(200);
        await colon('set autopair');
        await colon('w');
        const want = await area();
        const disk = await eventually('src/lib.rs', want);
        await sleep(600);
        const signs = await count('.files-sign.added');
        return paired && unpaired && off && plain && disk === want && signs >= 1
          ? pass('"foo(x) {" typed as foo(x) {}, Backspace removed the empty {}, noautopair typed a lone "(", :w wrote it and the gutter marks the added lines', { signs, tail: want.slice(-14) })
          : fail(`paired ${paired}, unpaired ${unpaired}, off ${off}, plain ${plain}, saved ${disk === want}, signs ${signs}; buffer ${JSON.stringify(want)}`);
      });

      await step('mixed-eol', 'a file with mixed line endings opens for editing; :w on it unedited rewrites it; an edit keeps every other line\'s own break', async () => {
        await colon('q');
        await page.eval(`document.querySelector('.files-tree').focus()`);
        const mixed = 'one\r\ntwo\nthree\r\nfour\rfive';
        writeFileSync(join(root, 'mixed.txt'), mixed);
        await page.key('R', { shift: true }); await sleep(700);
        if (!(await openFromTree('mixed.txt'))) return fail('could not open mixed.txt');
        if (!(await page.waitFor(`(document.querySelector('.files-viewer')||{}).innerText?.includes('three')`, 6000))) return fail('the viewer did not open mixed.txt');
        const before = (await read('mixed.txt')).version;
        await colon('w');
        const rewrote = await toastHas('Saved: mixed.txt');
        const after = await read('mixed.txt');
        // `A` on line 2 appends "!" and saves.
        await page.key('j'); await sleep(150);
        await page.key('A', { shift: true }); await sleep(300);
        if (!(await page.waitFor(`!!document.querySelector('.files-edit')`, 3000))) return fail(`A did not open the editor on a mixed file: ${await h.text('#toast')}`);
        await page.keys('!'); await page.key('Escape'); await sleep(200);
        await colon('w');
        const one = await eventually('mixed.txt', 'one\r\ntwo!\nthree\r\nfour\rfive');
        // A new line below keeps the others' breaks.
        await page.keys('o'); await sleep(200); await page.keys('new'); await page.key('Escape'); await sleep(200);
        await colon('w');
        const two = await eventually('mixed.txt', 'one\r\ntwo!\nnew\nthree\r\nfour\rfive');
        return rewrote && after.content === mixed && after.version && before !== undefined && one === 'one\r\ntwo!\nthree\r\nfour\rfive' && two === 'one\r\ntwo!\nnew\nthree\r\nfour\rfive'
          ? pass('a clean :w rewrote mixed.txt unchanged, an edit changed only its line, a new line took the break above it and every other line kept CRLF, LF or CR', { two })
          : fail(`rewrote ${rewrote}, unchanged ${after.content === mixed}, one ${JSON.stringify(one)}, two ${JSON.stringify(two)}`);
      });

      await step('paste', 'Ctrl-V pastes the system clipboard into the buffer from normal mode, as the TUI does (it has no visual-block mode)', async () => {
        await page.send('Browser.grantPermissions', { permissions: ['clipboardReadWrite', 'clipboardSanitizedWrite'] }).catch(() => undefined);
        const primed = await page.eval(`navigator.clipboard.writeText('PASTED\\r\\ntext').then(() => true, (e) => String(e))`);
        if (primed !== true) return fail(`the clipboard could not be primed in this browser: ${primed}`);
        await page.key('g'); await page.key('g'); await sleep(100);
        await page.key('v', { ctrl: true }); await sleep(600);
        const buffer = await area();
        const insert = /INSERT/.test(await meta());
        await page.key('Escape'); await sleep(200);
        await colon('e!');
        return buffer.startsWith('PASTED\ntext') && insert
          ? pass('Ctrl-V put the clipboard text (CRLF as LF) at the caret and left the editor in insert mode', { head: buffer.slice(0, 16) })
          : fail(`buffer starts ${JSON.stringify(buffer.slice(0, 20))}, meta ${await meta()}`);
      });

      const bad = steps.filter((s) => s.status !== 'pass');
      const writes = h.writes().map((e) => e.cmd);
      if (!['write_project_file', 'create_project_entry', 'move_project_entry', 'copy_project_entry', 'trash_project_entry', 'undo_project_file_operation'].every((c) => writes.includes(c))) {
        steps.push({ id: 'files-writes', title: 'every change went through the bridge', status: 'fail', note: `forwarded writes: ${[...new Set(writes)]}` });
        return { status: 'fail', note: 'a change command never reached rsid through the bridge', steps };
      }
      return { status: bad.length ? 'fail' : 'pass', note: bad.length ? `${bad[0].id}: ${bad[0].note}` : 'saved, refused a stale save, forced, reloaded, saved with Ctrl-S and :wq, added, renamed, copied, trashed and undid entries, and edited with colours, search, folds, bracket match, auto-pairing, mixed line endings and a clipboard paste, each read back from rsid', steps };
    },
  },
  {
    extra: true,
    id: 'graph-run', section: '(extra) Local #128: a graph run binds the session\'s workflow', parityRow: 'overlay:graph-review-overlay-space-v-or-graph',
    source: 'desktop/src/graphs.ts, desktop/src/graph-model.ts, desktop/src-tauri/src/graphs.rs',
    interaction: '<Space>v on the finished session, o and Enter open a saved one-node workflow, r runs it: ExecuteWorkflow, then UpdateSessionWorkflow links the selected session (the TUI\'s run_graph_draft); the workflow_id and the execution are read back from rsid',
    async run(h) {
      const { page } = h;
      const steps = [];
      const step = (id, title, r) => { steps.push({ id: `graph-${id}`, title, ...r }); return r; };
      // Harness setup, straight to the isolated rsid: one saved workflow with one session node pinned to the `local`
      // provider, which no daemon here is configured for. rsid accepts the run, then ends it `failed` at its own sandbox
      // or provider admission before any model is called, so the section needs no real provider, key or network.
      const workflowId = crypto.randomUUID();
      const now = new Date().toISOString();
      const definition = {
        version: '1.0', name: 'E2E graph run', description: 'one session node, no real provider',
        nodes: [{ id: 'step', name: 'Say hello', type: 'action', description: '', instructions: 'Say hello.', inputs: [], outputs: [], tags: [], hooks: [], pattern_params: {}, provider: 'local' }],
        edges: [], metadata: { topology_steps: JSON.stringify({ step: { kind: 'session' } }) },
      };
      await h.rpc('UpsertWorkflowDefinition', { document: { workflow: { id: workflowId, title: 'E2E graph run', stage: 'Planning', artifact_path: null, project_id: h.seed.projectId, created_at: now, updated_at: now }, definition } });
      const before = await h.rpc('GetSession', { session_id: h.seed.leafId });
      const unlinked = !before.session?.workflow_id && !before.workflow_id;
      step('precondition', 'the finished session starts with no workflow', unlinked ? pass('rsid: the seeded session has no workflow_id') : fail(`the session already has a workflow: ${JSON.stringify(before.session?.workflow_id ?? before.workflow_id)}`));
      if (!unlinked) return { status: 'fail', note: 'the seeded session already had a workflow', steps };

      await cursorOnLeaf(h);
      await page.keys(' v');
      const opened = await page.waitFor(`!document.getElementById('graph-view').hidden`, 6000);
      step('open', '<Space>v opens the graph review', opened ? pass('the graph review is open') : fail('<Space>v did not open the graph review'));
      if (!opened) return { status: 'fail', note: 'the graph review did not open', steps };
      await page.key('o');
      const listed = await page.waitFor(`document.getElementById('graph-view').innerText.includes('E2E graph run')`, 6000);
      step('saved', 'o lists the saved workflow from rsid', listed ? pass('the saved-workflow picker lists "E2E graph run" (ListWorkflows)') : fail(`the picker did not list the workflow: ${(await h.text('#graph-view')).slice(0, 120)}`));
      if (!listed) return { status: 'fail', note: 'the saved workflow was not listed', steps };
      await page.key('Enter');
      const loaded = await page.waitFor(`(document.querySelector('.graph-canvas')||{}).innerText?.includes('Say hello')`, 6000);
      step('load', 'Enter opens the workflow', loaded ? pass('the workflow\'s node is drawn (GetWorkflowDefinition)') : fail('the workflow did not load'));
      if (!loaded) return { status: 'fail', note: 'the workflow did not load', steps };

      const forwardedBefore = h.writes().length;
      await page.key('r');
      // The run ends `failed` at rsid's admission within a second; the toast that names the execution proves the ack.
      const started = await page.waitFor(`/Workflow started/.test((document.getElementById('toast')||{}).innerText||'')`, 15000);
      step('execute', 'r runs the workflow (ExecuteWorkflow accepted by rsid)', started ? pass('the page announced "Workflow started"') : fail(`no run was accepted: ${(await h.text('#toast')) || (await h.text('#graph-view')).slice(0, 100)}`));

      // The authority is rsid: wait for the link there, never read it from the page.
      let linked = null;
      for (let i = 0; i < 25 && linked !== workflowId; i++) {
        await sleep(200);
        const got = await h.rpc('GetSession', { session_id: h.seed.leafId });
        linked = got.session?.workflow_id ?? got.workflow_id ?? null;
      }
      step('bind', 'UpdateSessionWorkflow linked the session (read back from rsid)', linked === workflowId ? pass(`GetSession: workflow_id = ${workflowId}`, { workflowId }) : fail(`the session's workflow_id in rsid is ${JSON.stringify(linked)}, not ${workflowId}`));

      const sent = h.writes().slice(forwardedBefore).map((e) => e.cmd);
      const wrote = sent.includes('graph_execute_workflow') && sent.includes('graph_bind_session_workflow');
      step('writes', 'both calls reached rsid through the bridge, in order', wrote && sent.indexOf('graph_execute_workflow') < sent.indexOf('graph_bind_session_workflow') ? pass(`forwarded: ${sent.join(', ')}`) : fail(`forwarded: ${sent.join(', ') || 'nothing'}`));

      // The execution rsid recorded for that run (the id its ack carried): it ends (`failed` here, by design) before any provider call.
      const executionId = h.writes('graph_execute_workflow').map((e) => e.execution_id).find(Boolean) ?? null;
      let ex = null;
      for (let i = 0; executionId && i < 40; i++) {
        ex = (await h.rpc('GetWorkflowExecution', { execution_id: executionId })).lookup?.execution ?? null;
        if (ex && ['succeeded', 'failed', 'cancelled', 'blocked'].includes(ex.status)) break;
        await sleep(250);
      }
      const finished = ex?.status ?? null;
      const terminal = ['succeeded', 'failed', 'cancelled', 'blocked'].includes(finished) && ex.workflow_id === workflowId;
      step('execution', 'rsid recorded the execution for the workflow and ended it', terminal ? pass(`GetWorkflowExecution ${executionId.slice(0, 8)}: ${finished}${ex.error ? ` (${String(ex.error).slice(0, 80)})` : ''}`, { executionId, status: finished }) : fail(`rsid's execution for the run: ${JSON.stringify(ex?.status ?? null)} (id ${executionId})`));
      await h.closeAll();
      const bad = steps.filter((x) => x.status !== 'pass');
      return { status: bad.length ? 'fail' : 'pass', note: bad.length ? `${bad[0].id}: ${bad[0].note}` : `the run was accepted, UpdateSessionWorkflow set the session's workflow_id to the workflow in rsid, and the execution ended "${finished}" with no provider`, evidence: { workflowId, linked, executionId, execution: finished }, steps };
    },
  },
];
