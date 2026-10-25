#!/usr/bin/env node
// Permission-free smoke for the desktop app (hub #3000 acceptance, local #109).
//
//   node scripts/smoke.mjs [--live] [--out DIR] [--chrome PATH] [--steps GROUP[,GROUP]]
//   node scripts/smoke.mjs --first-run [--out DIR]   first-run guidance and boot evidence (smoke-firstrun.mjs)
//
// It serves the built desktop/dist (run `npm run build` first) to a headless Chrome, injects a shim for
// the Tauri API (scripts/smoke-bridge.mjs), drives keys through CDP and screenshots every step.
//   default  a fake rsid on a throwaway Unix socket (scripts/smoke-fake.mjs): needs no daemon, no GUI permission.
//   --live   the running rsid (~/.rsi/daemon.sock or $RSI_DAEMON_SOCKET), READ-ONLY: the bridge forwards an exact
//            allowlist of read methods and refuses (and logs) everything else.
//   --steps  only the boot and the named step groups (scripts/smoke-groups.mjs: core, settings, manager, keyboard, a11y); no flag runs
//            every step in order. tests/smoke.test.mjs runs one group per test, each with its own time budget (local #250).
//            report.json records each step's durationMs.
// Exit: 0 every step passed or was substituted/blocked-by-daemon, 1 a step failed, 3 runner_unavailable (no Chrome).
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createBridge, defaultSocket, rpcCall } from './smoke-bridge.mjs';
import { runIsolated } from './smoke-e2e.mjs';
import { E2E_EXTRAS, E2E_SECTIONS, openTextRowByMouse, settingsMouse } from './smoke-e2e-sections.mjs';
import { startFakeDaemon } from './smoke-fake.mjs';
import { contrastAudit } from './audit-contrast.mjs';
import { GROUP_NAMES, PRELUDE, loadStepFactories, selectedIds, slowestSteps, validateSteps } from './smoke-groups.mjs';
import { managerSteps } from './smoke-manager-steps.mjs';
import { tabSteps } from './smoke-tab-steps.mjs';
import { runFirstRun } from './smoke-firstrun.mjs';
import { connectPage, domSnapshot, findChrome, launchChrome, newToken, serve } from './smoke-lib.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const DIST = resolve(here, '..', 'dist');
// Steps that live in their own files (scripts/smoke-steps/*.mjs): a new step adds a file there and edits nothing else (local #281).
const STEP_FILES = await loadStepFactories(join(here, 'smoke-steps'));

export function parseArgs(argv) {
  const o = { live: false, out: '/tmp/desktop-smoke', chrome: null, isolatedWrite: false, rsid: null, only: null, firstRun: false, steps: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--live') o.live = true;
    else if (a === '--out') o.out = argv[++i];
    else if (a === '--chrome') o.chrome = argv[++i];
    else if (a === '--isolated-write') o.isolatedWrite = true;
    else if (a === '--first-run') o.firstRun = true;
    else if (a === '--rsid') o.rsid = argv[++i];
    else if (a === '--only') o.only = argv[++i].split(',').filter(Boolean);
    else if (a === '--steps') {
      o.steps = (argv[++i] ?? '').split(',').filter(Boolean);
      const bad = o.steps.filter((g) => !GROUP_NAMES.includes(g));
      if (!o.steps.length || bad.length) throw new Error(`--steps takes ${GROUP_NAMES.join(', ')}${bad.length ? ` (not ${bad.join(', ')})` : ''}`);
    }
    else throw new Error(`unknown argument ${a}`);
  }
  if (o.firstRun && (o.live || o.isolatedWrite)) throw new Error('--first-run uses its own bridge and cannot be combined with --live or --isolated-write');
  if (o.steps && (o.firstRun || o.isolatedWrite)) throw new Error('--steps selects headless smoke steps; --first-run and --isolated-write have their own');
  if (o.isolatedWrite && o.live) throw new Error('--live is read-only and cannot be combined with --isolated-write');
  return o;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const DIALOG = `(document.querySelector('dialog[open]')||{}).innerText||''`;

/** The checklist. Each step returns { status: 'pass'|'fail'|'substituted'|'blocked-by-daemon', note }. */
export function buildSteps({ live, bridgeLog }) {
  const cursorIndex = `[...document.querySelectorAll('#session-list li')].findIndex(l => l.classList.contains('cursor'))`;
  const daemonLacks = (method) => bridgeLog.some((e) => e.method === method && e.outcome === 'error' && /Method not found/.test(e.reason ?? ''));
  const pass = (note) => ({ status: 'pass', note });
  const fail = (note) => ({ status: 'fail', note });

  // Settings asks before it drops unapplied changes (local #269), so discard them first and let Esc just close.
  const discardSettings = `(() => { const v = document.getElementById('settings-view'); const d = v && !v.hidden && v.querySelector('[data-settings="discard"]'); if (d && !d.disabled) d.click(); })()`;
  const closeAll = async (p) => { await p.eval(discardSettings); for (let i = 0; i < 3; i++) { await p.key('Escape'); await sleep(150); } };
  const command = async (p, text) => { await p.keys(`:${text}`); await sleep(150); await p.key('Enter'); await sleep(1200); };
  const palette = async (p, text) => {
    await p.element('#open-palette', '(e) => e.click()', { step: 'open Actions palette' });
    await p.element('#palette-input', undefined, { step: 'Actions palette accepts input' });
    await sleep(500);
    await p.keys(text);
    await sleep(300);
    await p.key('Enter');
    await sleep(1200);
  };
  const dialogStep = (what, group, title, run, method, expect) => ({
    id: what, group, title: `${what} dialog opens`,
    async run(p) {
      await run(p);
      const text = await p.eval(DIALOG);
      if (text.includes(title) && (!expect || text.includes(expect))) return pass(`dialog "${title}" open`);
      if (live && daemonLacks(method)) return { status: 'blocked-by-daemon', note: `the running rsid answers Method not found for ${method} (an older rsid than hub #3039?); the fake-daemon run covers the dialog` };
      return fail(`no dialog titled "${title}"; saw: ${JSON.stringify(text.slice(0, 120))}`);
    },
    after: closeAll,
  });

  return [
    {
      id: 'session-list', group: 'core', title: 'the session list renders, and j/k moves the cursor',
      async run(p) {
        const n = await p.eval(`document.querySelectorAll('#session-list li').length`);
        if (n < 1) return fail('the session list is empty');
        await p.key('j'); await sleep(250);
        const a = await p.eval(cursorIndex);
        await p.key('j'); await sleep(250);
        const b = await p.eval(cursorIndex);
        await p.key('k'); await sleep(250);
        const c = await p.eval(cursorIndex);
        if (a < 0) return fail('j put no cursor on a row');
        if (n > a + 1 && b !== a + 1) return fail(`second j moved the cursor ${a} -> ${b}, not down one`);
        if (n > a + 1 && c !== a) return fail(`k moved the cursor ${b} -> ${c}, not back to ${a}`);
        return pass(`${n} rows; j ${a}, j ${b}, k ${c}`);
      },
    },
    {
      id: 'open-session', group: 'core', title: 'Enter opens a session conversation',
      async run(p) {
        // Walk the cursor onto the first leaf session (a row that is not a container), then Enter.
        const target = await p.eval(`[...document.querySelectorAll('#session-list li')].findIndex(l => !l.classList.contains('container'))`);
        if (target < 0) return fail('no leaf session row to open');
        for (let guard = 0; guard < 60; guard++) {
          const cur = await p.eval(cursorIndex);
          if (cur === target) break;
          await p.key(cur < target ? 'j' : 'k'); await sleep(120);
        }
        await p.key('Enter');
        const shown = await p.waitFor(`!document.getElementById('session-view').hidden && document.getElementById('s-title').textContent.trim() !== ''`, 6000);
        if (!shown) return fail('Enter did not open the session view');
        await sleep(800);
        const events = await p.eval(`document.getElementById('events').children.length`);
        const text = await p.eval(`document.getElementById('events').innerText`);
        if (!live && !text.includes('I fixed the list view')) return fail(`the conversation text is missing: ${JSON.stringify(text.slice(0, 120))}`);
        if (live && !bridgeLog.some((e) => e.method === 'GetConversation' && e.outcome === 'forwarded')) return fail('GetConversation was not forwarded');
        return pass(`${events} conversation nodes`);
      },
      after: closeAll,
    },
    {
      // Local #277: the default theme's text meets WCAG AA (4.5:1, 3:1 for large text) on the list and on a dialog. The walker is
      // scripts/audit-contrast.mjs, the one the on-demand audits use (`npm run audit -- themes` runs it under every theme: Light and
      // High Contrast have known misses, local #274, so only the default theme is gated).
      id: 'text-contrast', group: 'core', title: 'every visible text in the default theme meets WCAG AA contrast, on the session list and the New session dialog',
      async run(p) {
        const surfaces = [['session list', async () => {}], ['New session dialog', async () => { await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' }); await sleep(300); }]];
        const notes = [];
        for (const [where, open] of surfaces) {
          await open();
          const r = await contrastAudit(p);
          if (r.checked < 10) return fail(`${where}: the walker only found ${r.checked} texts (it is broken, or the page is not drawn)`);
          if (r.failed) return fail(`${where}: ${r.failed} of ${r.checked} texts are under AA: ${r.failures.slice(0, 5).map((f) => `${f.selector} "${f.text}" ${f.fg} on ${f.bg} ${f.ratio}:1 (needs ${f.need})`).join('; ')}`);
          notes.push(`${where} ${r.checked} texts`);
        }
        return pass(`all visible text is at least AA: ${notes.join(', ')}`);
      },
      after: closeAll,
    },
    {
      // Local #226: with Vim keys off the list is one tab stop (an ARIA tree); arrows move, Enter opens, Left folds.
      id: 'session-list-keyboard', group: 'core', title: 'with Vim keys off Tab reaches the session list, ArrowDown and Enter open the second row, ArrowLeft folds the lead',
      async run(p) {
        const setVim = async (on) => {
          await p.eval(`(() => { const c = document.getElementById('vim-keys'); c.checked = ${on}; c.dispatchEvent(new Event('change')); })()`);
          await sleep(200);
        };
        const rows = `[...document.querySelectorAll('#session-list .session-item')]`;
        const focused = `document.activeElement === document.getElementById('session-list')`;
        const activeIndex = `${rows}.findIndex((l) => l.id === document.getElementById('session-list').getAttribute('aria-activedescendant'))`;
        await setVim(false);
        await p.eval(`(() => { document.activeElement?.blur?.(); return true; })()`);
        let tabs = 0;
        for (; tabs < 60 && !(await p.eval(focused)); tabs++) { await p.key('Tab'); await sleep(40); }
        if (!(await p.eval(focused))) return fail(`Tab never reached the session list in ${tabs} presses`);
        const role = await p.eval(`document.getElementById('session-list').getAttribute('role')`);
        if (role !== 'tree') return fail(`the session list has role ${role}, not tree`);
        if ((await p.eval(activeIndex)) < 0) return fail('the focused list has no active row');
        await p.key('Home'); await sleep(200);
        if ((await p.eval(activeIndex)) !== 0) return fail(`Home put the cursor on row ${await p.eval(activeIndex)}, not the first`);
        await p.key('ArrowDown'); await sleep(200);
        if ((await p.eval(activeIndex)) !== 1) return fail(`ArrowDown put the cursor on row ${await p.eval(activeIndex)}, not the second`);
        const second = await p.eval(`${rows}[1].dataset.id`);
        const selected = await p.eval(`${rows}.map((l) => l.getAttribute('aria-selected')).join(',')`);
        if (!selected.split(',').every((v) => v === 'true' || v === 'false') || !selected.startsWith('false,true')) return fail(`rows' aria-selected: ${selected}`);
        await p.key('Enter');
        const opened = await p.waitFor(`document.querySelector('#session-list .session-item[aria-current="true"]')?.dataset.id === ${JSON.stringify(second)} && !document.getElementById('session-view').hidden`, 4000);
        if (!opened) return fail('ArrowDown then Enter did not open the second row');
        if (!(await p.eval(focused))) return fail('the list lost focus after Enter');
        await p.key('Home'); await sleep(200);
        const lead = await p.eval(`${rows}[0].getAttribute('aria-expanded')`);
        if (lead !== 'true') return pass(`Tab x${tabs} reached the list; ArrowDown + Enter opened the second row (the first row is not an open container: ${lead})`);
        const before = await p.eval(`${rows}.length`);
        await p.key('ArrowLeft'); await sleep(250);
        const folded = await p.eval(`${rows}[0].getAttribute('aria-expanded')`);
        const after = await p.eval(`${rows}.length`);
        if (folded !== 'false' || after >= before) return fail(`ArrowLeft did not fold the lead (aria-expanded ${folded}, rows ${before} -> ${after})`);
        await p.key('ArrowRight'); await sleep(250);
        if ((await p.eval(`${rows}[0].getAttribute('aria-expanded')`)) !== 'true') return fail('ArrowRight did not unfold the lead');
        const label = await p.eval(`${rows}[0].getAttribute('aria-label')`);
        return pass(`Tab x${tabs} reached the list; ArrowDown + Enter opened the second row; ArrowLeft folded the lead (${before} -> ${after} rows), ArrowRight unfolded it; lead reads ${JSON.stringify(label)}`);
      },
      async after(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
      },
    },
    {
      // Local #244: with Vim keys off, focus never falls to the page body after these everyday actions.
      id: 'focus-return', group: 'keyboard', title: 'with Vim keys off closing Settings, Graph review and the DAG browser returns focus, Enter on a Graph review button keeps it, Esc in the message box lands on the session list, opening Issues focuses its list',
      async run(p) {
        const setVim = async (on) => {
          await p.eval(`(() => { const c = document.getElementById('vim-keys'); c.checked = ${on}; c.dispatchEvent(new Event('change')); })()`);
          await sleep(200);
        };
        const where = `(() => { const a = document.activeElement; return a ? (a.id || a.dataset?.gkey || a.tagName) : 'none'; })()`;
        const onBody = `document.activeElement === document.body || document.activeElement === null`;
        await setVim(false);
        await p.eval(`(() => { document.getElementById('open-palette').focus(); return true; })()`);
        for (const [name, palettePhrase, view] of [['Settings', 'Settings', 'settings-view'], ['Graph review', 'Graph review', 'graph-view'], ['the DAG browser', 'Recursive DAG browser', 'dag-view']]) {
          await palette(p, palettePhrase);
          if (!(await p.waitFor(`!document.getElementById('${view}').hidden`, 4000))) return fail(`${name} did not open`);
          await p.key('Escape'); await sleep(300);
          if (!(await p.waitFor(`document.getElementById('${view}').hidden`, 3000))) return fail(`Esc did not close ${name}`);
          if (await p.eval(onBody)) return fail(`after closing ${name} focus is on the page body`);
        }
        // Enter on Graph review's Saved keeps focus inside the view.
        await palette(p, 'Graph review');
        if (!(await p.waitFor(`!document.getElementById('graph-view').hidden`, 4000))) return fail('Graph review did not open');
        await p.eval(`(() => { document.querySelector('#graph-view [data-gkey="o"]').focus(); return true; })()`);
        await p.key('Enter'); await sleep(400);
        const inside = await p.eval(`document.getElementById('graph-view').contains(document.activeElement) && document.activeElement !== document.getElementById('graph-view') || document.activeElement === document.getElementById('graph-view')`);
        if (!inside) return fail(`Enter on Saved moved focus out of Graph review to ${await p.eval(where)}`);
        await closeAll(p);
        // Esc in the message box puts focus on the session list and ArrowDown then moves the cursor.
        await p.eval(`(() => { document.getElementById('session-list').focus(); return true; })()`);
        await p.key('ArrowDown'); await sleep(150);
        await p.key('Enter'); await sleep(900);
        if (await p.eval(`document.getElementById('session-view').hidden`)) return fail('no session opened for the message box check');
        await p.eval(`(() => { document.getElementById('message').focus(); return true; })()`);
        await p.key('Escape'); await sleep(200);
        if (!(await p.eval(`document.activeElement === document.getElementById('session-list')`))) return fail(`Esc in the message box left focus on ${await p.eval(where)}, not the session list`);
        const row = `document.getElementById('session-list').getAttribute('aria-activedescendant')`;
        const before = await p.eval(row);
        await p.key('ArrowDown'); await sleep(200);
        if ((await p.eval(row)) === before) return fail('ArrowDown after Esc did not move the session-list cursor');
        // Opening the Issues workspace focuses its list.
        await palette(p, 'Issues');
        if (!(await p.waitFor(`!document.getElementById('issues-view').hidden`, 6000))) return fail('the Issues workspace did not open');
        await sleep(300);
        if (!(await p.eval(`document.activeElement === document.getElementById('issues-list')`))) return fail(`opening Issues left focus on ${await p.eval(where)}, not #issues-list`);
        return pass('Settings, Graph review and the DAG browser return focus on Esc; Saved keeps focus; Esc in the message box focuses the list and ArrowDown moves; Issues opens on its list');
      },
      async after(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
      },
    },
    {
      id: 'issues-workspace', group: 'core', title: 'the Issues workspace lists Issues',
      async run(p) {
        await command(p, 'issues');
        const shown = await p.waitFor(`!document.getElementById('issues-view').hidden && document.querySelectorAll('#issues-list li').length > 0`, 8000);
        if (!shown) return fail(`no Issues listed; status: ${JSON.stringify(await p.eval(`document.getElementById('issues-status').innerText`))}`);
        const n = await p.eval(`document.querySelectorAll('#issues-list li').length`);
        if (!live && !(await p.eval(`document.getElementById('issues-list').innerText`)).includes('First fake Issue')) return fail('the fake Issues are not listed');
        return pass(`${n} Issues listed`);
      },
      after: closeAll,
    },
    {
      id: 'command-mode', group: 'core', title: ': command mode opens',
      async run(p) {
        await p.keys(':'); await sleep(400);
        const open = await p.eval(`!document.getElementById('cmdline').hidden && document.getElementById('cmdline').innerText.startsWith(':')`);
        return open ? pass('the command line is open') : fail('the command line did not open');
      },
      after: closeAll,
    },
    {
      id: 'help', group: 'core', title: '? help opens',
      async run(p) {
        await p.keys('?'); await sleep(600);
        const text = await p.eval(DIALOG);
        return text.includes('Contextual') && text.includes('All Commands') ? pass('the key help dialog is open') : fail(`no help dialog; saw ${JSON.stringify(text.slice(0, 100))}`);
      },
      after: closeAll,
    },
    {
      id: 'settings', group: 'settings', title: 'Settings opens',
      async run(p) {
        await command(p, 'set');
        const ok = await p.eval(`!document.getElementById('settings-view').hidden && document.getElementById('settings-view').innerText.includes('Settings')`);
        return ok ? pass('the Settings pane is open') : fail('the Settings pane did not open');
      },
      after: closeAll,
    },
    dialogStep('hooks', 'settings', 'Claude hooks', (p) => command(p, 'hooks'), 'ListClaudeHooks', !live ? 'fake-hook' : undefined),
    dialogStep('providers', 'settings', 'Custom providers', (p) => palette(p, 'providers'), 'ListCustomProviders', !live ? 'Fake gateway' : undefined),
    dialogStep('bridges', 'settings', 'Message bridges', (p) => palette(p, 'bridges'), 'ListMessageBridges', !live ? 'Signal' : undefined),
    {
      // Hub #3112: a mouse-focused button keeps Enter and Space; only the dialog's own focus runs the row action,
      // which writes trust (remote_set_config). The bridge never forwards that write, but it logs the attempt.
      id: 'remote-button-keys', group: 'settings', refuses: ['remote_set_config'], title: 'Enter/Space on a clicked Remote dialog button never write remote trust',
      async run(p) {
        await palette(p, 'Remote access');
        if (!(await p.waitFor(`${DIALOG}.includes('synthetic-phone') || ${DIALOG}.includes('Remote')`, 4000))) return fail('the Remote dialog did not open');
        const writes = () => bridgeLog.filter((e) => e.cmd === 'remote_set_config').length;
        const button = (label) => `[...document.querySelectorAll('dialog[open] button')].find((b) => b.textContent.trim().startsWith(${JSON.stringify(label)}))`;
        const before = writes();
        await p.click(button('Devices'));
        await p.key('Enter'); await sleep(300);
        await p.key(' '); await sleep(300);
        await p.click(button('Projects'));
        await p.click(button('Refresh'));
        await p.key(' '); await sleep(300);
        await p.key('Enter'); await sleep(600);
        const focusedButton = await p.eval(`document.activeElement?.tagName === 'BUTTON'`);
        if (writes() !== before) return fail(`a clicked button's Enter/Space sent remote_set_config (${writes() - before} writes)`);
        if (live) return pass('clicked Devices, Projects and Refresh, then Enter/Space: no remote_set_config (positive control runs in the fake mode)');
        // Positive control: with the dialog itself focused, Enter is the row action and does try to write.
        await p.element('dialog[open]', '(d) => { d.tabIndex = -1; d.focus(); return document.activeElement === d; }', { step: 'focus Remote dialog' });
        await p.key('Enter'); await sleep(600);
        if (writes() === before) return fail('the row action never ran, so the button check proved nothing');
        return pass(`clicked buttons kept Enter/Space (focus on a button: ${focusedButton}); the dialog's own Enter tried remote_set_config (refused by the read-only bridge)`);
      },
      after: closeAll,
    },
    {
      // Hub #3120: in Scheduled Jobs, Space toggles and Enter edits the selected job; a clicked Refresh keeps both.
      id: 'schedules-button-keys', group: 'settings', refuses: ['toggle_scheduled_job'], title: 'Enter/Space on a clicked Scheduled Jobs button never toggle or edit a job',
      async run(p) {
        await palette(p, 'Scheduled jobs');
        if (!(await p.waitFor(`${DIALOG}.includes('Refresh (r)')`, 4000))) return fail('the Scheduled jobs dialog did not open');
        const toggles = () => bridgeLog.filter((e) => e.cmd === 'toggle_scheduled_job').length;
        const formOpen = `[...document.querySelectorAll('dialog[open] h2')].some((h) => h.textContent === 'Edit scheduled job')`;
        const before = toggles();
        const refresh = `[...document.querySelectorAll('dialog[open] button')].find((b) => b.textContent.trim().startsWith('Refresh'))`;
        await p.click(refresh);
        await p.key(' '); await sleep(400);
        await p.click(refresh);
        await p.key('Enter'); await sleep(600);
        if (toggles() !== before) return fail(`Space on Refresh toggled the job (${toggles() - before} toggle calls)`);
        if (await p.eval(formOpen)) return fail('Enter on Refresh opened the job editor');
        if (live) return pass('clicked Refresh, then Space and Enter: no toggle_scheduled_job and no editor (positive control runs in the fake mode)');
        await p.element('dialog[open]', '(d) => { d.tabIndex = -1; d.focus(); }', { step: 'focus Scheduled jobs dialog' });
        await p.key(' '); await sleep(600);
        if (toggles() === before) return fail('the dialog\'s own Space never toggled the job, so the button check proved nothing');
        return pass("a clicked Refresh kept Space and Enter; the dialog's own Space tried toggle_scheduled_job (refused by the read-only bridge)");
      },
      after: closeAll,
    },
    {
      // Hub #3101 follow-up (local #111): overlay dialogs follow the Vim keys switch like the panes. The one
      // capture-phase dialog gate (src/dialog-gate.ts) drops dialog letters with the switch off; buttons still work.
      id: 'dialog-vim-gate', group: 'settings', title: 'with Vim keys off a dialog letter does nothing and its button works; with Vim keys on the letter acts',
      async run(p) {
        const setVim = async (on) => {
          await p.eval(`(() => { const c = document.getElementById('vim-keys'); c.checked = ${on}; c.dispatchEvent(new Event('change')); })()`);
          await sleep(200);
        };
        const lists = () => bridgeLog.filter((e) => e.cmd === 'list_scheduled_jobs').length;
        const formOpen = `[...document.querySelectorAll('dialog[open] h2')].some((h) => h.textContent === 'New scheduled job')`;
        const focusDialog = () => p.element('dialog[open]', '(d) => { d.tabIndex = -1; d.focus(); }', { step: 'focus Scheduled jobs dialog' });
        await palette(p, 'Scheduled jobs');
        if (!(await p.waitFor(`${DIALOG}.includes('Refresh (r)')`, 4000))) return fail('the Scheduled jobs dialog did not open');
        await setVim(false);
        await focusDialog();
        const before = lists();
        await p.key('r'); await sleep(500);
        await p.key('n'); await sleep(500);
        if (lists() !== before) return fail('with Vim keys off the dialog letter r still refreshed the list');
        if (await p.eval(formOpen)) return fail('with Vim keys off the dialog letter n still opened the new-job form');
        const refresh = `[...document.querySelectorAll('dialog[open] button')].find((b) => b.textContent.trim().startsWith('Refresh'))`;
        await p.click(refresh); await sleep(600);
        if (lists() === before) return fail('with Vim keys off the Refresh button no longer works');
        await setVim(true);
        await focusDialog();
        const mid = lists();
        await p.key('r'); await sleep(600);
        if (lists() === mid) return fail('with Vim keys on the dialog letter r did nothing');
        await p.key('n'); await sleep(500);
        if (!(await p.eval(formOpen))) return fail('with Vim keys on the dialog letter n did not open the new-job form');
        return pass('Vim off: r and n did nothing, the Refresh button listed again; Vim on: r listed and n opened the form');
      },
      async after(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
      },
    },
    {
      // Local #129: dialog hint lines and "(x)" button labels follow the Vim keys switch.
      id: 'dialog-hints-follow-vim', group: 'settings', title: 'with Vim keys off the dialog hint line and button labels drop the letter keys; with Vim keys on they return',
      async run(p) {
        const setVim = async (on) => {
          await p.eval(`(() => { const c = document.getElementById('vim-keys'); c.checked = ${on}; c.dispatchEvent(new Event('change')); })()`);
          await sleep(300);
        };
        const labels = `[...document.querySelectorAll('dialog[open] button')].map((b) => b.textContent.trim()).join('|')`;
        const note = `[...document.querySelectorAll('dialog[open] .dialog-note')].map((n) => n.textContent).join(' ')`;
        await palette(p, 'Scheduled jobs');
        if (!(await p.waitFor(`${DIALOG}.includes('Scheduled jobs')`, 4000))) return fail('the Scheduled jobs dialog did not open');
        await setVim(true);
        const onLabels = await p.eval(labels); const onNote = await p.eval(note);
        if (!onLabels.includes('Refresh (r)') || !onNote.includes('j/k move')) return fail(`with Vim keys on the TUI wording is missing: ${onLabels} / ${onNote}`);
        if ((await p.eval(`document.documentElement.dataset.vimKeys`)) !== 'on') return fail('html[data-vim-keys] is not on');
        await setVim(false);
        const offLabels = await p.eval(labels); const offNote = await p.eval(note);
        if (/\((?:[A-Za-z]|Space|dd)\)/.test(offLabels)) return fail(`with Vim keys off a button label still shows a letter key: ${offLabels}`);
        if (!offLabels.split('|').includes('Refresh')) return fail(`the Refresh button is gone: ${offLabels}`);
        if (/j\/k|\bt run now|\bdd delete/.test(offNote) || !offNote.includes('Enter') || !offNote.includes('Esc')) return fail(`with Vim keys off the hint line is wrong: ${offNote}`);
        await setVim(true);
        if (!(await p.eval(labels)).includes('Refresh (r)') || !(await p.eval(note)).includes('j/k move')) return fail('turning Vim keys back on did not restore the TUI wording');
        // Local #132: a title= tooltip and a toast follow the switch too (the real toast() of dom.js, the real Actions button title).
        const tip = `document.getElementById('open-palette').getAttribute('title')`;
        const toastText = `document.getElementById('toast').textContent`;
        const toastMsg = 'Open a session first (j/k, then Enter); n launches a manager';
        const showToast = () => p.eval(`import('./dom.js').then((m) => m.toast(${JSON.stringify(toastMsg)}))`);
        await setVim(true);
        const onTip = await p.eval(tip);
        if (!onTip || !/<Space>;|:commands/.test(onTip)) return fail(`with Vim keys on the Actions tooltip should name its Vim keys: ${onTip}`);
        await showToast(); await sleep(200);
        if ((await p.eval(toastText)) !== toastMsg) return fail(`with Vim keys on the toast should read as written: ${await p.eval(toastText)}`);
        await setVim(false);
        const offTip = await p.eval(tip);
        if (/<Space>|:commands|\bgt\b/.test(offTip) || !/(?:Ctrl|Cmd)-K/.test(offTip)) return fail(`with Vim keys off the Actions tooltip should keep only the Ctrl/Cmd-K chord: ${offTip}`);
        if ((await p.eval(toastText)) !== 'Open a session first (Enter)') return fail(`with Vim keys off the shown toast still names letter keys: ${await p.eval(toastText)}`);
        await showToast(); await sleep(200);
        const offToast = await p.eval(toastText);
        if (offToast !== 'Open a session first (Enter)') return fail(`a toast raised with Vim keys off still names letter keys: ${offToast}`);
        await setVim(true);
        const backTip = await p.eval(tip);
        if (backTip !== onTip || (await p.eval(toastText)) !== toastMsg) return fail(`turning Vim keys back on did not restore the tooltip and toast: ${backTip} / ${await p.eval(toastText)}`);
        // Local #133: the notice log (toasts recorded raw), ':' command prose and a Settings value cell follow the switch too.
        await closeAll(p);
        const noticeMsg = 'Nothing to open: n launches a manager, or focus a session and run :manager global appoint';
        const notices = `[...document.querySelectorAll('dialog[open] .notice-msg')].map((n) => n.textContent).join('|')`;
        await setVim(false);
        await p.eval(`import('./dom.js').then((m) => m.toast(${JSON.stringify(noticeMsg)}))`); await sleep(200);
        const offToastCmd = await p.eval(toastText);
        if (/:manager|\bn launches/.test(offToastCmd) || !offToastCmd.includes('(command palette)')) return fail(`with Vim keys off the toast should point at the command palette: ${offToastCmd}`);
        await p.eval(`document.getElementById('open-palette').click()`); await sleep(500);
        await p.keys('Notifications'); await sleep(300); await p.key('Enter'); await sleep(900);
        if (!(await p.waitFor(`${DIALOG}.includes('Notifications')`, 4000))) return fail('the Notifications dialog did not open');
        const offLog = await p.eval(notices);
        if (!offLog.includes('Appoint global manager') || /:manager|\bn launches/.test(offLog)) return fail(`the notice log still names Vim keys with the switch off: ${offLog}`);
        await setVim(true);
        if (!(await p.eval(notices)).includes(noticeMsg)) return fail('turning Vim keys on did not restore the logged notice as it was recorded');
        await closeAll(p);
        const cellsOf = (section, needle) => `import('./settings.js').then((m) => { m.openSettings(${JSON.stringify(section)}); return new Promise((r) => setTimeout(() => r([...document.querySelectorAll('#settings-view .sv-value')].map((n) => n.textContent).filter((t) => t.includes(${JSON.stringify(needle)})).join('|')), 250)); })`;
        const sections = [['budgets', 'Enter lists', '▾ Enter lists · a adds', /\ba adds/], ['provider_keys', 'Enter opens', '▾ Enter opens · s r c d i', /s r c d i/]];
        let offCells = '';
        for (const [section, needle, onText, offRe] of sections) {
          await setVim(true);
          const onCell = await p.eval(cellsOf(section, needle));
          if (!onCell.includes(onText)) return fail(`with Vim keys on the Settings value cell should read as written: ${onCell}`);
          await setVim(false);
          const offCell = await p.eval(`[...document.querySelectorAll('#settings-view .sv-value')].map((n) => n.textContent).filter((t) => t.includes(${JSON.stringify(needle)})).join('|')`);
          if (offRe.test(offCell) || !offCell.includes(needle)) return fail(`with Vim keys off the Settings value cell still names letter keys: ${offCell}`);
          offCells += `${offCell};`;
          await setVim(true);
          if (!(await p.eval(`[...document.querySelectorAll('#settings-view .sv-value')].map((n) => n.textContent).join('|')`)).includes(onText)) return fail('turning Vim keys on did not restore the Settings value cell');
        }
        // Local #139: the Settings info card (facts, summary, a row-key fact) and the section summary follow the switch both ways.
        const cardOf = (section) => `import('./settings.js').then((m) => { m.openSettings(${JSON.stringify(section)}); return new Promise((r) => setTimeout(() => r(JSON.stringify({ card: document.querySelector('#settings-view .settings-info').textContent, sum: document.querySelector('#settings-view .settings-section-summary').textContent, dts: [...document.querySelectorAll('#settings-view .settings-info dt')].map((d) => [d.textContent, getComputedStyle(d).display]) })), 300)); })`;
        const cardNow = `JSON.stringify({ card: document.querySelector('#settings-view .settings-info').textContent, sum: document.querySelector('#settings-view .settings-section-summary').textContent, dts: [...document.querySelectorAll('#settings-view .settings-info dt')].map((d) => [d.textContent, getComputedStyle(d).display]) })`;
        await setVim(true);
        const onBudgets = JSON.parse(await p.eval(cardOf('budgets')));
        if (!onBudgets.card.includes('(a adds, Enter edits, d deletes)') || !onBudgets.card.includes('a adds, Enter edits, d deletes')) return fail(`with Vim keys on the budgets info card should read as written: ${onBudgets.card}`);
        await setVim(false);
        const offBudgets = JSON.parse(await p.eval(cardNow));
        if (/\ba adds|\bd deletes|w applies/.test(offBudgets.card + offBudgets.sum) || !offBudgets.card.includes('Opens the budget policies (Enter edits)')) return fail(`with Vim keys off the budgets info card still names letter keys: ${offBudgets.card}`);
        await setVim(true);
        if (!JSON.parse(await p.eval(cardNow)).card.includes('(a adds, Enter edits, d deletes)')) return fail('turning Vim keys on did not restore the budgets info card');
        await setVim(false);
        const offKeys = JSON.parse(await p.eval(cardOf('provider_keys')));
        if (/\bs sets|\br rotates|\bi imports/.test(offKeys.card + offKeys.sum) || !offKeys.card.includes('values are never shown')) return fail(`with Vim keys off the Provider keys info card still names letter keys: ${offKeys.card}`);
        const offHooks = JSON.parse(await p.eval(cardOf('claude_hooks')));
        if (offHooks.dts.some(([t, d]) => t === 'a') || offHooks.dts.some(([t, d]) => t === '' && d !== 'none') || /\ba adds|\bd deletes/.test(offHooks.card + offHooks.sum)) return fail(`with Vim keys off the hooks info card still shows its "a" fact or letter keys: ${JSON.stringify(offHooks)}`);
        await setVim(true);
        const onHooks = JSON.parse(await p.eval(cardNow));
        if (!onHooks.dts.some(([t, d]) => t === 'a' && d !== 'none') || !onHooks.card.includes('Add hook')) return fail(`turning Vim keys on did not restore the hooks "a" fact: ${JSON.stringify(onHooks.dts)}`);
        offCells += `card "${offBudgets.card.slice(0, 90)}";`;
        await p.eval(`import('./settings.js').then((m) => m.openSettings('theme_colors'))`); await sleep(250); // leave the pane where the next step expects it
        return pass(`Vim off: Settings cells "${offCells}", notice log "${offLog}", buttons [${offLabels}], hint "${offNote.trim()}", Actions tooltip "${offTip.split('\n')[0]}", toast "${offToast}"; Vim on: "Refresh (r)", "j/k move", tooltip "${onTip.split('\n')[0]}" and the toast return`);
      },
      async after(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
      },
    },
    {
      // Hub #3120: the Settings pane's Space and Enter activate the selected row (Built-in theme opens its picker).
      id: 'settings-button-keys', group: 'settings', title: 'Enter/Space on a clicked Settings button never activate a row',
      async run(p) {
        await command(p, 'set');
        if (!(await p.waitFor(`!document.getElementById('settings-view').hidden`, 4000))) return fail('the Settings pane did not open');
        const pickerOpen = `!!document.querySelector('dialog[open]')`;
        const refresh = `[...document.querySelectorAll('#settings-view button')].find((b) => b.textContent.trim().startsWith('Refresh'))`;
        await p.click(refresh);
        await p.key(' '); await sleep(500);
        if (await p.eval(pickerOpen)) return fail('Space on a clicked Refresh activated the selected row');
        await p.click(refresh);
        await p.key('Enter'); await sleep(500);
        if (await p.eval(pickerOpen)) return fail('Enter on a clicked Refresh activated the selected row');
        // Positive control: with no control focused, the pane's own Enter activates the row (the theme picker).
        await p.eval(`document.activeElement && document.activeElement.blur()`);
        await p.key('Enter'); await sleep(700);
        if (!(await p.eval(pickerOpen))) return fail("the pane's own Enter never activated the row, so the button check proved nothing");
        return pass("a clicked Refresh kept Space and Enter; the pane's own Enter opened the row's picker");
      },
      after: closeAll,
    },
    {
      // Hub #3280: the shared Settings text editor is opened, finished and cancelled by mouse alone.
      id: 'settings-text-mouse', group: 'settings', refuses: ['update_daemon_config'], title: 'a Settings text editor opened by mouse saves, stages and cancels by mouse',
      async run(p) {
        await command(p, 'set');
        if (!(await p.waitFor(`!document.getElementById('settings-view').hidden`, 4000))) return fail('the Settings pane did not open');
        const writes = () => bridgeLog.filter((e) => e.cmd === 'update_daemon_config').length;
        const bench = { group: 'PROVIDERS & SANDBOXES', section: 'benchmarks', label: 'Benchmark docker hosts' };
        // Cancel: the typed value is dropped and no write is attempted.
        if (!(await openTextRowByMouse(p, bench))) return fail('clicking the Benchmark docker hosts value never opened its editor');
        if (!(await p.eval(`!!(${settingsMouse.button('Save')}) && !!(${settingsMouse.button('Cancel')})`))) return fail('the editor has no Save and Cancel buttons');
        await p.keys('ssh://smoke-cancelled'); await sleep(150);
        await p.click(settingsMouse.button('Cancel')); await sleep(500);
        if (await p.eval(settingsMouse.open)) return fail('Cancel did not close the editor');
        if (writes() !== 0) return fail('Cancel attempted a daemon write');
        // Save: the write is attempted at once (this bridge is read-only, so it is refused and the editor shows why).
        if (!(await openTextRowByMouse(p, bench))) return fail('the editor did not reopen by mouse');
        await p.keys('ssh://smoke-host'); await sleep(150);
        await p.click(settingsMouse.button('Save'));
        await p.waitFor(`(${settingsMouse.error}) !== ''`, 4000);
        const shown = await p.eval(settingsMouse.error);
        if (writes() !== 1 || !shown || !(await p.eval(settingsMouse.open))) return fail(`Save attempted ${writes()} writes; the editor showed "${shown}"`);
        await p.click(settingsMouse.button('Cancel')); await sleep(400);
        if (await p.eval(settingsMouse.open)) return fail('Cancel did not close the editor after an error');
        // Stage: a staged text row leaves one pending change and writes nothing; Cancel leaves it as it was.
        const model = { group: 'MODELS', section: 'model_roles', label: 'Stall classifier model' };
        if (!(await openTextRowByMouse(p, model))) return fail('the Stall classifier model editor did not open by mouse');
        await p.keys('smoke-cancelled-model'); await sleep(150);
        await p.click(settingsMouse.button('Cancel')); await sleep(400);
        const afterCancel = await p.eval(settingsMouse.status);
        if (!afterCancel.includes('No pending')) return fail(`Cancel staged a value: ${afterCancel}`);
        if (!(await openTextRowByMouse(p, model))) return fail('the Stall classifier model editor did not reopen by mouse');
        if (!(await p.eval(`!!(${settingsMouse.button('Stage')})`))) return fail("a staged row's editor has no Stage button");
        await p.keys('smoke-model'); await sleep(150);
        await p.click(settingsMouse.button('Stage')); await sleep(400);
        const staged = await p.eval(settingsMouse.status);
        if (!staged.includes('1 pending') || writes() !== 1 || (await p.eval(settingsMouse.open))) return fail(`Stage left "${staged}" with ${writes()} write attempts`);
        return pass(`by mouse: Cancel attempted no write; Save attempted update_daemon_config at once (refused here: "${shown.slice(0, 60)}"); Stage left "${staged}"`);
      },
      after: closeAll,
    },
    {
      // Local #269: Settings is easy to find and use. Top-bar button and Ctrl-, open it; one click on a row's value edits it;
      // the values read as words; Close with unapplied changes asks Apply / Discard / Keep editing.
      id: 'settings-easy', group: 'settings', refuses: ['update_daemon_config'], title: 'Settings opens from the top bar and Ctrl-,, one click edits a row, and Close asks about unapplied changes',
      async run(p) {
        const writes = () => bridgeLog.filter((e) => e.cmd === 'update_daemon_config').length;
        const visible = `!document.getElementById('settings-view').hidden`;
        const header = (text) => `[...document.querySelectorAll('#settings-view .settings-head button')].find((b) => b.textContent.trim().startsWith(${JSON.stringify(text)}))`;
        const prompt = `document.querySelector('#settings-close[open]')`;
        const promptButtons = `[...document.querySelectorAll('#settings-close[open] button')].map((b) => b.textContent)`;
        const promptButton = (text) => `[...document.querySelectorAll('#settings-close[open] button')].find((b) => b.textContent === ${JSON.stringify(text)})`;
        if (!(await p.eval(`(() => { const b = document.getElementById('open-settings'); return !!b && b.textContent.trim() === 'Settings' && b.dataset.action === 'settings-open' && !b.closest('[hidden]'); })()`))) return fail('the top bar has no visible Settings button');
        await p.click(`document.getElementById('open-settings')`);
        if (!(await p.waitFor(visible, 4000))) return fail('the top-bar Settings button did not open Settings');
        await p.click(header('Close')); await sleep(300);
        if (await p.eval(visible) || await p.eval(`!!${prompt}`)) return fail('Close with nothing unapplied did not simply close');
        await p.eval(`document.activeElement && document.activeElement.blur()`);
        await p.key(',', { ctrl: true });
        if (!(await p.waitFor(visible, 4000))) return fail('Ctrl-, did not open Settings');
        // Words, not field=value, and one click to start the edit (Title model opens its provider dialog).
        await p.click(settingsMouse.group('MODELS')); await sleep(250);
        await p.click(settingsMouse.tab('model_roles')); await sleep(250);
        const shown = await p.eval(`${settingsMouse.value('Title model')}.textContent`);
        if (!shown || /=|title_model_/.test(shown)) return fail(`Title model shows "${shown}", not words`);
        await p.click(settingsMouse.value('Title model'));
        if (!(await p.waitFor(`!!document.querySelector('dialog[open]')`, 3000))) return fail('one click on Title model did not open its editor');
        await p.key('Escape'); await sleep(300);
        // A daemon boolean changes with one click and stays pending until applied.
        await p.click(settingsMouse.group('AGENT AUTOMATION')); await sleep(250);
        await p.click(settingsMouse.tab('retries_recovery')); await sleep(250);
        const before = await p.eval(`${settingsMouse.value('Retry on failure')}.textContent`);
        if (!/^(● On|○ Off)$/.test(before)) return fail(`Retry on failure shows "${before}", not On or Off`);
        await p.click(settingsMouse.value('Retry on failure')); await sleep(300);
        const after = await p.eval(`${settingsMouse.value('Retry on failure')}.textContent`);
        const pending = await p.eval(settingsMouse.status);
        if (after === before || !pending.includes('1 pending')) return fail(`a click on the value left "${before}" -> "${after}" and "${pending}"`);
        // Close asks; Keep editing keeps the change.
        await p.click(header('Close')); await sleep(300);
        if (!(await p.eval(`!!${prompt}`))) return fail('Close with a pending change did not ask');
        const buttons = JSON.stringify(await p.eval(promptButtons));
        if (buttons !== JSON.stringify(['Keep editing', 'Discard', 'Apply change'])) return fail(`the close prompt offers ${buttons}`);
        await p.click(promptButton('Keep editing')); await sleep(300);
        if (!(await p.eval(visible)) || !(await p.eval(settingsMouse.status)).includes('1 pending')) return fail('Keep editing did not keep the pane and the change');
        // Apply goes through the existing update (refused by this read-only bridge, so the pane stays open with the change pending).
        const sent = writes();
        await p.click(header('Close')); await sleep(300);
        await p.click(promptButton('Apply change')); await sleep(800);
        if (writes() !== sent + 1) return fail(`Apply attempted ${writes() - sent} updates`);
        if (!(await p.eval(visible)) || !(await p.eval(settingsMouse.status)).includes('1 pending')) return fail('a refused Apply closed Settings or lost the change');
        // Discard drops it and closes.
        await p.click(header('Close')); await sleep(300);
        await p.click(promptButton('Discard')); await sleep(400);
        if (await p.eval(visible) || writes() !== sent + 1) return fail('Discard did not close without another write');
        return pass(`top-bar button and Ctrl-, open Settings; Title model reads "${shown}" and one click opened its editor; Retry on failure ${before} -> ${after} on one click; Close asked Keep editing / Discard / Apply change; Apply attempted one update (refused here); Discard closed`);
      },
      after: closeAll,
    },
    {
      // Local #274: the app follows the system light/dark setting (emulated with prefers-color-scheme), an explicit theme does
      // not, native controls follow the painted theme, and badge and button text on a filled accent reaches 4.5:1.
      id: 'theme-system', group: 'settings', title: 'the app follows prefers-color-scheme live, an explicit theme ignores it, native controls follow the theme, and badges and danger buttons reach 4.5:1',
      async run(p) {
        const emulate = (value) => p.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value }] });
        const theme = `document.documentElement.dataset.theme`;
        const scheme = `getComputedStyle(document.documentElement).colorScheme`;
        const checkboxScheme = `(() => { const c = document.querySelector('input[type="checkbox"]'); return c ? getComputedStyle(c).colorScheme : null; })()`;
        const bodyBg = `getComputedStyle(document.body).backgroundColor`;
        // WCAG contrast of the real computed colours of the badges and the primary and danger buttons.
        const contrast = `(() => {
          const lum = (c) => { const [r, g, b] = c.match(/[\\d.]+/g).slice(0, 3).map(Number).map((v) => { v /= 255; return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }); return 0.2126 * r + 0.7152 * g + 0.0722 * b; };
          const ratio = (a, b) => { const x = lum(a), y = lum(b); return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05); };
          const host = document.createElement('div');
          host.innerHTML = '<span class="attn-badge">Question</span><button class="btn danger">Decline</button><button class="btn primary">Send</button><span class="pause-badge">Paused</span>';
          document.body.append(host);
          const out = {};
          for (const [name, sel] of [['attn-badge', '.attn-badge'], ['pause-badge', '.pause-badge'], ['btn primary', '.btn.primary'], ['btn danger', '.btn.danger']]) {
            const cs = getComputedStyle(host.querySelector(sel));
            out[name] = ratio(cs.color, cs.backgroundColor);
          }
          host.remove();
          return out;
        })()`;
        // Fresh profile: nothing stored, so it matches the system.
        await p.eval(`(() => { localStorage.removeItem('rsi.desktop.theme.v1'); return import('./themes.js').then((m) => { m.applyThemeByName('system'); return true; }); })()`);
        await emulate('dark'); await sleep(250);
        if ((await p.eval(theme)) !== 'desktop-default' || (await p.eval(scheme)) !== 'dark') return fail(`a dark system is not the dark default (${await p.eval(theme)}, ${await p.eval(scheme)})`);
        const darkBg = await p.eval(bodyBg);
        await emulate('light');
        if (!(await p.waitFor(`${theme} === 'light'`, 4000))) return fail(`the page did not follow a light system live (data-theme ${await p.eval(theme)})`);
        const lightBg = await p.eval(bodyBg);
        if (lightBg === darkBg) return fail(`the page background did not change with the system (${lightBg})`);
        const lightScheme = await p.eval(scheme);
        const lightBox = await p.eval(checkboxScheme);
        if (lightScheme !== 'light' || (lightBox !== null && lightBox !== 'light')) return fail(`native controls stay dark on the Light theme: root ${lightScheme}, checkbox ${lightBox}`);
        const lightContrast = await p.eval(contrast);
        const low = Object.entries(lightContrast).filter(([, v]) => v !== null && v < 4.5).map(([k, v]) => `${k} ${v.toFixed(2)}`);
        if (low.length) return fail(`Light text below 4.5:1: ${low.join(', ')}`);
        // The picker offers "Match system" first, on its own row.
        await p.eval(`import('./themes.js').then((m) => { m.openThemePicker(); return true; })`);
        await sleep(250);
        const rows = await p.eval(`[...document.querySelectorAll('#theme-picker .pick-item')].slice(0, 3).map((r) => r.textContent)`);
        await closeAll(p);
        if (!rows[0]?.includes('Match system (light/dark)') || !rows[0].includes('●')) return fail(`the theme picker's first row is ${JSON.stringify(rows)}, not the marked "Match system (light/dark)"`);
        // An explicit theme ignores the system, either way.
        await p.eval(`import('./themes.js').then((m) => { m.applyThemeByName('high-contrast'); return true; })`);
        await emulate('dark'); await sleep(250);
        const explicitDark = await p.eval(theme);
        await emulate('light'); await sleep(250);
        const explicitLight = await p.eval(theme);
        const hcContrast = await p.eval(contrast);
        const hcLow = Object.entries(hcContrast).filter(([, v]) => v !== null && v < 4.5).map(([k, v]) => `${k} ${v.toFixed(2)}`);
        // Back to a fresh profile's behaviour so later steps see the dark default.
        await p.eval(`import('./themes.js').then((m) => { m.applyThemeByName('system'); return true; })`);
        await emulate('dark'); await sleep(250);
        if (explicitDark !== 'high-contrast' || explicitLight !== 'high-contrast') return fail(`an explicit theme followed the system (${explicitDark}, ${explicitLight})`);
        if (hcLow.length) return fail(`High Contrast text below 4.5:1: ${hcLow.join(', ')}`);
        const fmt = (o) => Object.entries(o).filter(([, v]) => v !== null).map(([k, v]) => `${k} ${v.toFixed(2)}`).join(', ');
        return pass(`dark system -> desktop default, light system -> Light live (background ${darkBg} -> ${lightBg}, color-scheme ${lightScheme}, checkbox ${lightBox}); high-contrast stayed through both; Light: ${fmt(lightContrast)}; High Contrast: ${fmt(hcContrast)}; picker first row "Match system (light/dark)"`);
      },
      after: closeAll,
    },
    // Local #234: the manager, dashboard, budget, Trash, approval and model-switch views render the fake daemon's populated
    // answers (scripts/smoke-fake.mjs), not their error state. Each step names the content it saw.
    ...managerSteps({ live, closeAll, palette, command }),
    // Local #243: with Vim keys off no pane takes Tab, so Tab reaches each view's buttons.
    ...tabSteps({ closeAll, palette, command, bridgeLog }),
    {
      // Local #264: finished sessions stay listed (Show completed is on by default, as in the TUI), muted, the choice
      // is remembered across a reload, and the open session stays listed when it finishes while they are hidden.
      id: 'show-finished', group: 'core', title: 'a finished session stays in the list by default, muted; Show completed hides it, is remembered across a reload, and never hides the open session',
      async run(p) {
        // Local #285: every wait below is a condition (with the time it took recorded), never a fixed sleep: a loaded gate
        // runs the click -> open -> render chain slower than any guess.
        const box = `document.getElementById('show-finished')`;
        const done = `[...document.querySelectorAll('#session-list .session-item.st-Completed')]`;
        const stored = `localStorage.getItem('rsi.desktop.showFinished')`;
        const rowCount = `document.querySelectorAll('#session-list .session-item').length`;
        const marks = [];
        const began = Date.now();
        const mark = (label) => marks.push(`${label} ${Date.now() - began} ms`);
        const bad = (why) => fail(`${why} [timings: ${marks.join(', ') || 'none yet'}]`);
        const until = async (label, expression, ms = 8000) => { const ok = await p.waitFor(expression, ms); mark(ok ? label : `${label} TIMED OUT`); return ok; };
        const selectedId = `document.querySelector('#session-list .session-item.selected')?.dataset.id`;
        if (!(await p.eval(`${box}.checked`))) return bad('a fresh profile starts with Show completed unticked');
        const label = await p.eval(`${box}.closest('label').textContent.trim()`);
        if (label !== 'Show completed') return bad(`the toggle reads ${JSON.stringify(label)}, not "Show completed"`);
        await until('finished row listed', `${done}.length > 0`, 3000);
        const n = await p.eval(`${done}.length`);
        if (n < 1) return pass('no finished session in this daemon: the default is still on, nothing to hide (the fake daemon has one)');
        const muted = await p.eval(`(() => { const c = (e) => getComputedStyle(e.querySelector('.label')).color;
          const live = [...document.querySelectorAll('#session-list .session-item')].find((e) => !e.classList.contains('st-Completed') && !e.classList.contains('selected'));
          const f = ${done}[0]; return { finished: c(f), other: live ? c(live) : null }; })()`);
        if (muted.other && muted.finished === muted.other) return bad(`a finished row is not de-emphasised: both labels are ${muted.finished}`);
        const id = await p.eval(`${done}[0].dataset.id`);
        await closeAll(p);
        await p.click(`${done}[0]`);
        if (!(await until('finished row opened', `(${selectedId}) === ${JSON.stringify(id)}`))) return bad(`clicking the finished row did not open it (selected ${await p.eval(selectedId)}; ${await p.eval(`(() => { const r = ${done}[0]?.getBoundingClientRect(); if (!r) return 'the row is gone'; const t = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); return t ? t.tagName + '#' + t.id + '.' + t.className : 'none'; })()`)})`);
        await p.click(box);
        if (!(await until('unticked and stored', `!${box}.checked && ${stored} === 'off'`))) return bad(`unticking left checked=${await p.eval(`${box}.checked`)}, stored=${await p.eval(stored)}`);
        // The open finished session stays; the list has re-rendered once the other finished rows (none here) are gone.
        if (!(await until('open finished row kept', `!!document.querySelector('#session-list [data-id="${id}"]')`, 4000))) return bad(`the open finished session dropped out of the list when Show completed was unticked (${await p.eval(`import('./state.js').then((m) => JSON.stringify({ sel: m.state.selectedId, id: ${JSON.stringify(id)}, rows: [...document.querySelectorAll('#session-list .session-item')].map((e) => e.dataset.id + ':' + e.className) }))`)})`);
        // Open a different row and the finished one goes.
        const other = await p.eval(`(() => { const e = [...document.querySelectorAll('#session-list .session-item')].find((x) => x.dataset.id !== "${id}"); return e ? e.dataset.id : null; })()`);
        if (other) {
          await p.click(`document.querySelector('#session-list [data-id="${other}"]')`);
          if (!(await until('another row opened', `(${selectedId}) === ${JSON.stringify(other)}`))) return bad(`clicking another row did not open it (selected ${await p.eval(selectedId)})`);
          if (!(await until('finished row hidden', `!document.querySelector('#session-list [data-id="${id}"]')`, 4000))) return bad('a finished session stayed listed after another session was opened, with Show completed unticked');
        }
        const url = await p.eval(`location.href`);
        await p.goto(url);
        if (!(await until('list after reload', `${rowCount} > 0`, 12000))) return bad('the list did not render after a reload');
        if (await p.eval(`${box}.checked`)) return bad('Show completed came back ticked after a reload, so the choice is not remembered');
        if ((await p.eval(`${done}.length`)) !== 0) return bad('finished rows were listed after a reload with Show completed off');
        await p.click(box);
        if (!(await until('ticked again', `${stored} === 'on' && ${done}.length > 0`))) return bad(`ticking Show completed again did not list the finished session and store on (stored ${await p.eval(stored)}, finished rows ${await p.eval(`${done}.length`)})`);
        await p.goto(url);
        if (!(await until('list after second reload', `${rowCount} > 0`, 12000))) return bad('the list did not render after the second reload');
        if (!(await p.eval(`${box}.checked`))) return bad('Show completed came back unticked after being ticked');
        return pass(`${n} finished row(s) listed by default and muted; unticked hides them but keeps the open one; off and on both survive a reload (${marks.join(', ')})`);
      },
      async after(p) { await closeAll(p); },
    },
    {
      // Local #272: the filter finds a session by a word from its prompt (the TUI does), says how many matches Show
      // completed and the archive hide and offers them, and words the Jobs tab's empty list as its own.
      id: 'filter-prompt', group: 'core', title: 'the filter matches prompt text, counts and offers the completed and archived matches it hides, and the Jobs tab words its own empty list',
      async run(p) {
        const box = `document.getElementById('show-finished')`;
        const search = `document.getElementById('search')`;
        const listText = `document.getElementById('session-list').textContent`;
        const rows = `[...document.querySelectorAll('#session-list .session-item')].map((e) => e.dataset.id)`;
        const type = async (text) => { await p.eval(`(() => { const b = ${search}; b.value = ${JSON.stringify(text)}; b.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`); await sleep(500); };
        const waitText = (needle) => p.waitFor(`${listText}.includes(${JSON.stringify(needle)})`, 6000);
        if (!(await p.eval(`${listText}.includes('Fake completed session')`))) return pass('no fake completed session in this daemon: nothing to search for (the fake daemon has one)');
        await closeAll(p);
        // 1. A word only the prompt holds finds the Completed session.
        await type('csv');
        if (!(await p.eval(`${listText}.includes('Fake completed session')`)) || (await p.eval(`${rows}.length`)) !== 1) return fail(`"csv" is only in a prompt and listed ${await p.eval(`JSON.stringify(${rows})`)}, not the Completed session`);
        // 2. Hide completed: the match is counted and offered, with the archived one.
        if (await p.eval(`${box}.checked`)) { await p.click(box); await sleep(400); }
        await type('csv');
        if (!(await waitText('1 archived session'))) return fail(`with Show completed off, the empty list says ${JSON.stringify(await p.eval(listText))}, not the hidden completed and archived matches`);
        const text = await p.eval(listText);
        if (!text.includes('1 completed session hidden')) return fail(`the hidden completed match is not counted: ${JSON.stringify(text)}`);
        const buttons = await p.eval(`[...document.querySelectorAll('#session-list .session-empty button')].map((b) => b.textContent)`);
        if (JSON.stringify(buttons) !== JSON.stringify(['Show completed', 'Show archived', 'Clear filter'])) return fail(`the empty row offers ${JSON.stringify(buttons)}`);
        await p.click(`document.querySelector('#session-list .session-empty button[data-empty="show-finished"]')`); await sleep(500);
        if (!(await p.eval(`${box}.checked`)) || !(await p.eval(`${listText}.includes('Fake completed session')`))) return fail('Show completed in the empty row did not list the completed match');
        // 3. Show archived opens the Archived tab with the filter kept.
        await p.click(box); await sleep(300);
        await type('csv');
        if (!(await waitText('Show archived'))) return fail('the archived offer did not come back');
        await p.click(`document.querySelector('#session-list .session-empty button[data-empty="show-archived"]')`); await sleep(700);
        if (!(await p.eval(`${listText}.includes('Fake archived session')`))) return fail(`Show archived did not list the archived match: ${JSON.stringify(await p.eval(listText))}`);
        await p.click(`document.getElementById('view-active')`); await sleep(400);
        // 4. The Jobs tab: its own wording, with no word about Show completed or All projects.
        await type('');
        await p.click(`document.getElementById('view-jobs')`); await sleep(500);
        const jobs = await p.eval(listText);
        if (!jobs.startsWith('No scheduled-job sessions yet')) return fail(`the Jobs tab says ${JSON.stringify(jobs)}`);
        await p.click(`document.getElementById('view-active')`); await sleep(400);
        await type('');
        if (!(await p.eval(`${box}.checked`))) { await p.click(box); await sleep(300); }
        return pass('"csv" finds the Completed session by its prompt; with Show completed off the empty list counts 1 completed and 1 archived match with Show completed, Show archived and Clear filter; Show archived opens the Archived tab; the Jobs tab reads "No scheduled-job sessions yet"');
      },
      async after(p) { await closeAll(p); },
    },
    {
      id: 'terminal', group: 'core', title: 'the terminal overlay opens (its PTY is stubbed: Tauri-only)',
      async run(p) {
        await command(p, 'term');
        const overlay = await p.eval(`(() => { const o = document.getElementById('terminal-overlay'); return !!o && !o.hidden; })()`);
        if (!overlay) return fail('the terminal overlay did not open');
        // Local #229: the mode label follows the Vim keys switch (hidden without it, INSERT with it).
        const label = await p.eval(`(() => { const m = document.querySelector('#terminal-overlay .terminal-mode'); const on = document.getElementById('vim-keys')?.checked === true;
          return { on, hidden: !m || m.hidden, text: m ? m.textContent : '' }; })()`);
        if (label.on ? label.hidden || label.text !== 'INSERT' : !label.hidden) return fail(`the mode label ${JSON.stringify(label)} does not follow the Vim keys switch`);
        return { status: 'pass', note: `the terminal overlay is open (the shim stubs the PTY: no shell runs, and the real shell is Tauri-only); Vim keys ${label.on ? 'on: label ' + label.text : 'off: no mode label'}` };
      },
      after: closeAll,
    },
    {
      // Local #267: Attach image beside Send (a Tab stop with Vim keys off) and dropping image files onto the conversation.
      id: 'attach-image', group: 'core', title: 'Attach image sits beside Send and Tab reaches it, a dropped non-image is refused in plain words, and a drop outside the session explains itself',
      async run(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); c.checked = false; c.dispatchEvent(new Event('change')); })()`);
        // The terminal step leaves its overlay open over the list; hide it so the row below can be clicked.
        if (await p.eval(`document.getElementById('terminal-overlay')?.hidden === false`)) { await p.click(`document.querySelector('#terminal-overlay button')`); await sleep(300); }
        if (await p.eval(`document.getElementById('session-view').hidden`)) {
          await p.click(`[...document.querySelectorAll('#session-list .session-item')].find((l) => !l.classList.contains('container'))`);
          if (!(await p.waitFor(`!document.getElementById('session-view').hidden`, 4000))) return fail('no session could be opened');
        }
        const layout = await p.eval(`(() => { const a = document.getElementById('attach-image'), s = document.querySelector('#composer button[type=submit]'); const ra = a.getBoundingClientRect(), rs = s.getBoundingClientRect();
          return { text: a.textContent.trim(), shown: ra.width > 0, tab: a.tabIndex, sameRow: Math.abs(ra.top - rs.top) < 8, before: ra.right <= rs.left + 1 }; })()`);
        if (layout.text !== 'Attach image' || !layout.shown || layout.tab !== 0 || !layout.sameRow || !layout.before) return fail(`the button is not a visible Tab stop beside Send: ${JSON.stringify(layout)}`);
        await p.eval(`document.getElementById('message').focus()`);
        await p.key('Tab'); await sleep(150);
        if ((await p.eval(`document.activeElement && document.activeElement.id`)) !== 'attach-image') return fail('Tab from the message box did not reach Attach image');
        const accept = await p.eval(`(() => { const i = document.getElementById('attach-file'); return i.type + ' ' + i.accept; })()`);
        if (accept !== 'file image/png,image/jpeg,image/gif,image/webp') return fail(`the file chooser is "${accept}", not limited to image types`);
        const drop = (target, name, type) => `(() => { const dt = new DataTransfer(); dt.items.add(new File(['x'], ${JSON.stringify(name)}, { type: ${JSON.stringify(type)} }));
          const ev = new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }); ${target}.dispatchEvent(ev); return ev.defaultPrevented; })()`;
        const toastText = `document.getElementById('toast').textContent`;
        const here = await p.eval(`location.href`);
        if (!(await p.eval(drop(`document.getElementById('events')`, 'notes.txt', 'text/plain')))) return fail('a dropped file was not kept by the page (the window would open it)');
        if (!(await p.eval(toastText)).includes('"notes.txt" was not attached: only PNG, JPEG, GIF and WebP images can be attached')) return fail(`a dropped text file said "${await p.eval(toastText)}"`);
        await p.eval(drop(`document.getElementById('session-list')`, 'a.png', 'image/png'));
        if (!(await p.eval(toastText)).includes('open a session and drop it onto the conversation')) return fail(`a drop on the session list said "${await p.eval(toastText)}"`);
        if ((await p.eval(`location.href`)) !== here) return fail('a drop navigated the page');
        return pass('Attach image is a visible Tab stop beside Send with an image-only chooser; a dropped text file and a drop outside the session each got a plain message');
      },
      async after(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
      },
    },
    {
      // Local #242: macOS WebKit tabs to a button, link, checkbox or summary only with an explicit tabindex
      // while "Keyboard navigation" is off. Chrome tabs to them anyway, so the check reads the attribute.
      id: 'tab-stops', group: 'a11y', title: 'every visible enabled button, link, checkbox and summary is an explicit tab stop, on the page and in each dialog; the conversation is a named tab stop',
      async run(p) {
        const missing = `(() => {
          const scope = document.querySelector('dialog[open]') ?? document;
          return [...scope.querySelectorAll('button, a[href], input[type="checkbox"], input[type="radio"], summary')]
            .filter((e) => !e.disabled && e.getClientRects().length > 0 && getComputedStyle(e).visibility !== 'hidden')
            .filter((e) => e.getAttribute('tabindex') !== '0')
            .map((e) => e.tagName.toLowerCase() + ' ' + JSON.stringify(e.id || e.textContent.trim().slice(0, 30)) + ' tabindex=' + e.getAttribute('tabindex'));
        })()`;
        const count = `(document.querySelector('dialog[open]') ?? document).querySelectorAll('[tabindex="0"]').length`;
        const seen = [];
        const check = async (where) => {
          await sleep(300);
          const bad = await p.eval(missing);
          if (bad.length) return `${where}: ${bad.slice(0, 6).join(', ')}${bad.length > 6 ? ` and ${bad.length - 6} more` : ''}`;
          seen.push(`${where} ${await p.eval(count)}`);
          return null;
        };
        const surfaces = [
          ['page', async () => {}],
          ['launch', async () => { await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' }); await p.waitFor(`!!document.querySelector('#launch-dialog[open]')`, 3000); }],
          ['confirm', async () => { await p.eval(`import('./approvals.js').then((m) => { void m.confirmDialog('Smoke: archive this session?', 'Archive', { title: 'Archive' }); return true; })`); await p.waitFor(`!!document.querySelector('dialog.confirm[open]')`, 3000); }],
          ['question', async () => { await p.eval(`import('./actions/lifecycle.js').then((m) => { m.openQuestionModal(); return true; })`); await p.waitFor(`!!document.querySelector('#question-dialog[open]')`, 3000); }],
          ['Trash', async () => { await palette(p, 'Trash'); await p.waitFor(`${DIALOG}.includes('Trash')`, 4000); }],
          ['Scheduled jobs', async () => { await palette(p, 'Scheduled jobs'); await p.waitFor(`${DIALOG}.includes('Refresh')`, 4000); }],
        ];
        for (const [where, open] of surfaces) {
          await open();
          if (where !== 'page' && !(await p.eval(`!!document.querySelector('dialog[open]')`))) return fail(`the ${where} dialog did not open`);
          const bad = await check(where);
          if (bad) return fail(`controls without tabindex="0": ${bad}`);
          await closeAll(p);
        }
        const events = await p.eval(`(() => { const e = document.getElementById('events'); return { tab: e.getAttribute('tabindex'), label: e.getAttribute('aria-label') }; })()`);
        if (events.tab !== '0' || !events.label) return fail(`#events is not a named tab stop: ${JSON.stringify(events)}`);
        return pass(`tab stops (count with tabindex=0): ${seen.join('; ')}; #events tabindex=0 aria-label=${JSON.stringify(events.label)}`);
      },
      after: closeAll,
    },
    {
      // Local #245: screen-reader basics. A toast is spoken and stays above an open dialog, every dialog has an
      // accessible name, and #events is a polite log (opening a session must not read its history aloud).
      id: 'screen-reader', group: 'a11y', title: 'toasts are live regions above open dialogs, every dialog has an accessible name, and the conversation is a log that announces additions only',
      async run(p) {
        const name = `(() => {
          const d = [...document.querySelectorAll('dialog[open]')].pop();
          if (!d) return null;
          const label = (d.getAttribute('aria-label') || '').trim();
          if (label) return label;
          return (d.getAttribute('aria-labelledby') || '').split(/\\s+/).filter(Boolean).map((id) => (document.getElementById(id)?.textContent || '').trim()).filter(Boolean).join(' ');
        })()`;
        const names = [];
        const unnamed = [];
        const surfaces = [
          ['New session', async () => { await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' }); }],
          ['Archive confirm', async () => { await p.eval(`import('./approvals.js').then((m) => { void m.confirmDialog('Smoke: archive this session?', 'Archive', { title: 'Archive' }); return true; })`); }],
          ['question', async () => { await p.eval(`import('./actions/lifecycle.js').then((m) => { m.openQuestionModal(); return true; })`); }],
          ['Trash', async () => { await palette(p, 'Trash'); }],
          ['Scheduled jobs', async () => { await palette(p, 'Scheduled jobs'); }],
          ['Choose theme', async () => { await p.eval(`import('./themes.js').then((m) => { m.openThemePicker(); return true; })`); }],
          ['Diagnostics', async () => { await palette(p, 'Diagnostics'); }],
          ['Memory search', async () => { await palette(p, 'Memory search'); }],
          ['Notifications', async () => { await palette(p, 'Notifications'); }],
          ['Emergency stop', async () => { await palette(p, 'Emergency stop all'); }],
        ];
        for (const [where, open] of surfaces) {
          await open();
          await sleep(250);
          const got = await p.eval(name);
          if (got === null || got === 'Command palette') { unnamed.push(`${where} (did not open)`); await closeAll(p); continue; }
          if (!got) unnamed.push(where); else names.push(`${where}: ${got}`);
          if (where === 'Emergency stop' || where === 'Archive confirm') {
            const role = await p.eval(`[...document.querySelectorAll('dialog[open]')].pop().getAttribute('role')`);
            if (role !== 'alertdialog') unnamed.push(`${where} role=${role}`);
          }
          // A toast while this dialog is open must be the topmost thing at its own centre.
          if (where === 'Scheduled jobs' || where === 'Choose theme') {
            const top = await p.eval(`(() => {
              import('./dom.js').then((m) => m.toast('Smoke: job enabled', true));
              return new Promise((r) => setTimeout(() => {
                const t = document.getElementById('toast'); const b = t.getBoundingClientRect();
                const hit = document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2);
                r({ open: t.matches(':popover-open'), hit: !!hit && (hit === t || t.contains(hit)), role: t.getAttribute('role'), live: t.getAttribute('aria-live') });
              }, 200));
            })()`);
            if (!top.open || !top.hit || top.role !== 'status' || top.live !== 'polite') unnamed.push(`${where}: toast ${JSON.stringify(top)}`);
          }
          await closeAll(p);
        }
        if (unnamed.length) return fail(`dialogs without an accessible name, or a toast under a dialog: ${unnamed.join('; ')}`);
        const err = await p.eval(`(() => { import('./dom.js').then((m) => m.toast('daemon error -32000: smoke')); const t = document.getElementById('toast'); return new Promise((r) => setTimeout(() => r({ role: t.getAttribute('role'), live: t.getAttribute('aria-live') }), 100)); })()`);
        if (err.role !== 'alert' || err.live !== 'assertive') return fail(`an error toast is ${JSON.stringify(err)}, not role=alert assertive`);
        const log = await p.eval(`(() => { const e = document.getElementById('events'); return { role: e.getAttribute('role'), live: e.getAttribute('aria-live'), relevant: e.getAttribute('aria-relevant'), busy: e.getAttribute('aria-busy') }; })()`);
        // Right after a full rebuild the log is busy and not live for a moment (the fake daemon replays the whole history on each poll).
        const settled = log.busy === 'true' ? log.live === 'off' : log.live === 'polite';
        if (log.role !== 'log' || log.relevant !== 'additions' || !settled) return fail(`#events is ${JSON.stringify(log)}`);
        return pass(`dialog names: ${names.join('; ')}; the toast is the top layer above them, role=status polite (alert assertive for an error); #events is role=log polite additions`);
      },
      after: closeAll,
    },
    // Steps from scripts/smoke-steps/*.mjs, after the ones above, in file-name order.
    ...STEP_FILES.flatMap((factory) => factory({ live, bridgeLog, pass, fail, sleep, closeAll, command, palette, DIALOG, cursorIndex, daemonLacks })),
  ];
}

/** `onStep(result)` hears each result as it finishes, so a caller can report the slowest steps of a run that never ends. */
export async function main(argv = process.argv.slice(2), { signal, onStep } = {}) {
  const opts = parseArgs(argv);
  const ungrouped = validateSteps(buildSteps({ live: false, bridgeLog: [] }));
  if (ungrouped.length) { console.error(`the smoke steps are not grouped: ${ungrouped.join('; ')}`); return 2; }
  if (!existsSync(join(DIST, 'index.html'))) {
    console.error('desktop/dist is missing: run `npm run build` in desktop/ first');
    return 2;
  }
  const bin = opts.chrome ?? findChrome();
  if (!bin) {
    console.log('runner_unavailable: no Chrome/Chromium found (set RSI_SMOKE_CHROME or pass --chrome); 0 steps executed');
    return 3;
  }
  if (opts.firstRun) return runFirstRun({ out: opts.out, chromeBin: bin, dist: DIST, signal });
  if (opts.isolatedWrite) return runIsolated(opts, { dist: DIST, chromeBin: bin, sections: [...E2E_SECTIONS, ...E2E_EXTRAS], signal });
  mkdirSync(opts.out, { recursive: true });
  let fake = null;
  let socket = defaultSocket();
  if (opts.live) {
    if (!existsSync(socket)) { console.error(`--live: no daemon socket at ${socket}`); return 2; }
  } else {
    fake = await startFakeDaemon();
    socket = fake.socket;
  }
  const bridgeLog = [];
  let server, chrome, page;
  const results = [];
  try {
    const bootStarted = Date.now();
    const bridge = createBridge({ call: (m, p) => rpcCall(socket, m, p), record: (e) => bridgeLog.push(e) });
    const token = newToken();
    const served = await serve({ dist: DIST, bridge, token, onRefusal: (e) => bridgeLog.push(e), signal });
    server = served.server;
    const port = served.port;
    chrome = await launchChrome(bin, { signal });
    page = await connectPage(chrome.cdpPort, { signal, browser: chrome });
    await page.goto(`http://127.0.0.1:${port}/`);
    const booted = await page.waitFor(`document.querySelectorAll('#session-list li').length > 0`, 12000);
    await sleep(500);
    const pageErrors = page.events.filter((e) => e.method === 'Runtime.exceptionThrown')
      .map((e) => e.params.exceptionDetails.exception?.description ?? e.params.exceptionDetails.text);
    const bootShot = await page.shot(join(opts.out, '00-boot.png'));
    // Boot COMPLETED, not just a window: the app's own report (html[data-boot], src/boot-model.ts). Live mode records it only,
    // because an older rsid may lack a read a step needs (the step then says so in the report).
    const bootState = await page.eval(`document.documentElement.dataset.boot ?? 'none'`);
    const bootFailed = !opts.live && bootState !== 'ready';
    results.push({
      id: PRELUDE, title: 'the app boots with no uncaught page error, connects and reports every boot step ok',
      status: booted && pageErrors.length === 0 && !bootFailed ? 'pass' : 'fail',
      note: pageErrors.length ? `uncaught: ${pageErrors[0].split('\n')[0]}` : !booted ? 'the session list never rendered' : `booted, session list present, data-boot=${bootState}`,
      screenshot: bootShot, durationMs: Date.now() - bootStarted,
    });
    onStep?.(results[0]);
    if (results[0].status === 'pass') {
      let i = 1;
      const steps = buildSteps({ live: opts.live, bridgeLog });
      const wanted = opts.steps ? selectedIds(steps, opts.steps) : null;
      for (const step of steps) {
        if (wanted && !wanted.has(step.id)) continue;
        const started = Date.now();
        let r;
        try { r = await step.run(page); } catch (e) { r = { status: 'fail', note: `threw: ${e.message}` }; }
        // Local #285: a failed step leaves its DOM state beside the screenshot (taken before the step's cleanup and the
        // screenshot change anything), so a flake that passes on re-run can still be read afterwards.
        const failedDom = r.status === 'fail' ? await domSnapshot(page) : null;
        r.screenshot = await page.shot(join(opts.out, `${String(i++).padStart(2, '0')}-${step.id}.png`));
        const result = { id: step.id, group: step.group, title: step.title, ...r };
        results.push(result);
        await step.after?.(page);
        await page.waitFor(`!document.querySelector('dialog[open]')`, 1500);
        result.durationMs = Date.now() - started;
        if (failedDom) {
          const domPath = join(opts.out, `${String(i - 1).padStart(2, '0')}-${step.id}.dom.json`);
          writeFileSync(domPath, `${JSON.stringify({ step: step.id, note: result.note, durationMs: result.durationMs, dom: failedDom }, null, 2)}\n`);
          result.dom = domPath;
          result.note = `${result.note} [dom: ${domPath}, ${(result.durationMs / 1000).toFixed(1)} s]`;
        }
        onStep?.(result);
      }
    }
  } finally {
    page?.close();
    server?.closeAllConnections();
    server?.close();
    try { await chrome?.stop(); } finally { await fake?.stop(); }
  }
  const refusals = bridgeLog.filter((e) => e.outcome === 'refused');
  const forwarded = [...new Set(bridgeLog.filter((e) => e.outcome === 'forwarded').map((e) => e.method))].sort();
  const report = {
    mode: opts.live ? 'live' : 'fake', steps: opts.steps ?? 'all', socket: opts.live ? socket : '(fake)', chrome: bin, results,
    forwardedMethods: forwarded, refusals: refusals.map((r) => `${r.cmd}: ${r.reason}`),
    ...(fake ? { fakeDaemonReceived: [...new Set(fake.received.map((r) => r.method))].sort() } : {}),
  };
  writeFileSync(join(opts.out, 'report.json'), `${JSON.stringify(report, null, 2)}\n`);
  for (const r of results) console.log(`${r.status.toUpperCase().padEnd(17)} ${r.id.padEnd(17)} ${r.note}\n${' '.repeat(35)}${r.screenshot} (${((r.durationMs ?? 0) / 1000).toFixed(1)} s)`);
  console.log(`steps=${report.steps === 'all' ? 'all' : report.steps.join(',')} total=${(results.reduce((t, r) => t + (r.durationMs ?? 0), 0) / 1000).toFixed(1)} s slowest: ${slowestSteps(results).join(', ')}`);
  console.log(`mode=${report.mode} forwarded=[${forwarded.join(', ')}] refused=${refusals.length}${refusals.length ? ` (${[...new Set(report.refusals)].join('; ')})` : ''}`);
  console.log(`report: ${join(opts.out, 'report.json')}`);
  return results.some((r) => r.status === 'fail') ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((c) => process.exit(c), (e) => { console.error(e); process.exit(2); });
}
