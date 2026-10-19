#!/usr/bin/env node
// Permission-free smoke for the desktop app (hub #3000 acceptance, local #109).
//
//   node scripts/smoke.mjs [--live] [--out DIR] [--chrome PATH] [--keep-going]
//   node scripts/smoke.mjs --first-run [--out DIR]   first-run guidance and boot evidence (smoke-firstrun.mjs)
//
// It serves the built desktop/dist (run `npm run build` first) to a headless Chrome, injects a shim for
// the Tauri API (scripts/smoke-bridge.mjs), drives keys through CDP and screenshots every step.
//   default  a fake rsid on a throwaway Unix socket (scripts/smoke-fake.mjs): needs no daemon, no GUI permission.
//   --live   the running rsid (~/.rsi/daemon.sock or $RSI_DAEMON_SOCKET), READ-ONLY: the bridge forwards an exact
//            allowlist of read methods and refuses (and logs) everything else.
// Exit: 0 every step passed or was substituted/blocked-by-daemon, 1 a step failed, 3 runner_unavailable (no Chrome).
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createBridge, defaultSocket, rpcCall } from './smoke-bridge.mjs';
import { runIsolated } from './smoke-e2e.mjs';
import { E2E_EXTRAS, E2E_SECTIONS } from './smoke-e2e-sections.mjs';
import { startFakeDaemon } from './smoke-fake.mjs';
import { runFirstRun } from './smoke-firstrun.mjs';
import { connectPage, findChrome, launchChrome, newToken, serve } from './smoke-lib.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const DIST = resolve(here, '..', 'dist');

export function parseArgs(argv) {
  const o = { live: false, out: '/tmp/desktop-smoke', chrome: null, isolatedWrite: false, rsid: null, only: null, firstRun: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--live') o.live = true;
    else if (a === '--out') o.out = argv[++i];
    else if (a === '--chrome') o.chrome = argv[++i];
    else if (a === '--isolated-write') o.isolatedWrite = true;
    else if (a === '--first-run') o.firstRun = true;
    else if (a === '--rsid') o.rsid = argv[++i];
    else if (a === '--only') o.only = argv[++i].split(',').filter(Boolean);
    else throw new Error(`unknown argument ${a}`);
  }
  if (o.firstRun && (o.live || o.isolatedWrite)) throw new Error('--first-run uses its own bridge and cannot be combined with --live or --isolated-write');
  if (o.isolatedWrite && o.live) throw new Error('--live is read-only and cannot be combined with --isolated-write');
  return o;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const DIALOG = `(document.querySelector('dialog[open]')||{}).innerText||''`;

/** The checklist. Each step returns { status: 'pass'|'fail'|'substituted'|'blocked-by-daemon', note }. */
function buildSteps({ live, bridgeLog }) {
  const cursorIndex = `[...document.querySelectorAll('#session-list li')].findIndex(l => l.classList.contains('cursor'))`;
  const daemonLacks = (method) => bridgeLog.some((e) => e.method === method && e.outcome === 'error' && /Method not found/.test(e.reason ?? ''));
  const pass = (note) => ({ status: 'pass', note });
  const fail = (note) => ({ status: 'fail', note });

  const closeAll = async (p) => { for (let i = 0; i < 3; i++) { await p.key('Escape'); await sleep(150); } };
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
  const dialogStep = (what, title, run, method, expect) => ({
    id: what, title: `${what} dialog opens`,
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
      id: 'session-list', title: 'the session list renders, and j/k moves the cursor',
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
      id: 'open-session', title: 'Enter opens a session conversation',
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
      id: 'issues-workspace', title: 'the Issues workspace lists Issues',
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
      id: 'command-mode', title: ': command mode opens',
      async run(p) {
        await p.keys(':'); await sleep(400);
        const open = await p.eval(`!document.getElementById('cmdline').hidden && document.getElementById('cmdline').innerText.startsWith(':')`);
        return open ? pass('the command line is open') : fail('the command line did not open');
      },
      after: closeAll,
    },
    {
      id: 'help', title: '? help opens',
      async run(p) {
        await p.keys('?'); await sleep(600);
        const text = await p.eval(DIALOG);
        return text.includes('Contextual') && text.includes('All Commands') ? pass('the key help dialog is open') : fail(`no help dialog; saw ${JSON.stringify(text.slice(0, 100))}`);
      },
      after: closeAll,
    },
    {
      id: 'settings', title: 'Settings opens',
      async run(p) {
        await command(p, 'set');
        const ok = await p.eval(`!document.getElementById('settings-view').hidden && document.getElementById('settings-view').innerText.includes('Settings')`);
        return ok ? pass('the Settings pane is open') : fail('the Settings pane did not open');
      },
      after: closeAll,
    },
    dialogStep('hooks', 'Claude hooks', (p) => command(p, 'hooks'), 'ListClaudeHooks', !live ? 'fake-hook' : undefined),
    dialogStep('providers', 'Custom providers', (p) => palette(p, 'providers'), 'ListCustomProviders', !live ? 'Fake gateway' : undefined),
    dialogStep('bridges', 'Message bridges', (p) => palette(p, 'bridges'), 'ListMessageBridges', !live ? 'Signal' : undefined),
    {
      // Hub #3112: a mouse-focused button keeps Enter and Space; only the dialog's own focus runs the row action,
      // which writes trust (remote_set_config). The bridge never forwards that write, but it logs the attempt.
      id: 'remote-button-keys', title: 'Enter/Space on a clicked Remote dialog button never write remote trust',
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
      id: 'schedules-button-keys', title: 'Enter/Space on a clicked Scheduled Jobs button never toggle or edit a job',
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
      id: 'dialog-vim-gate', title: 'with Vim keys off a dialog letter does nothing and its button works; with Vim keys on the letter acts',
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
      id: 'dialog-hints-follow-vim', title: 'with Vim keys off the dialog hint line and button labels drop the letter keys; with Vim keys on they return',
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
      id: 'settings-button-keys', title: 'Enter/Space on a clicked Settings button never activate a row',
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
      id: 'terminal', title: 'the terminal overlay opens (its PTY is stubbed: Tauri-only)',
      async run(p) {
        await command(p, 'term');
        const overlay = await p.eval(`(() => { const o = document.getElementById('terminal-overlay'); return !!o && !o.hidden; })()`);
        if (overlay) return { status: 'pass', note: 'the terminal overlay is open (the shim stubs the PTY: no shell runs, and the real shell is Tauri-only)' };
        return fail('the terminal overlay did not open');
      },
      after: closeAll,
    },
  ];
}

export async function main(argv = process.argv.slice(2), { signal } = {}) {
  const opts = parseArgs(argv);
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
    const bridge = createBridge({ call: (m, p) => rpcCall(socket, m, p), record: (e) => bridgeLog.push(e) });
    const token = newToken();
    const served = await serve({ dist: DIST, bridge, token, onRefusal: (e) => bridgeLog.push(e), signal });
    server = served.server;
    const port = served.port;
    chrome = await launchChrome(bin, { signal });
    page = await connectPage(chrome.cdpPort, { signal });
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
      id: 'boot', title: 'the app boots with no uncaught page error, connects and reports every boot step ok',
      status: booted && pageErrors.length === 0 && !bootFailed ? 'pass' : 'fail',
      note: pageErrors.length ? `uncaught: ${pageErrors[0].split('\n')[0]}` : !booted ? 'the session list never rendered' : `booted, session list present, data-boot=${bootState}`,
      screenshot: bootShot,
    });
    if (results[0].status === 'pass') {
      let i = 1;
      for (const step of buildSteps({ live: opts.live, bridgeLog })) {
        let r;
        try { r = await step.run(page); } catch (e) { r = { status: 'fail', note: `threw: ${e.message}` }; }
        r.screenshot = await page.shot(join(opts.out, `${String(i++).padStart(2, '0')}-${step.id}.png`));
        results.push({ id: step.id, title: step.title, ...r });
        await step.after?.(page);
        await page.waitFor(`!document.querySelector('dialog[open]')`, 1500);
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
    mode: opts.live ? 'live' : 'fake', socket: opts.live ? socket : '(fake)', chrome: bin, results,
    forwardedMethods: forwarded, refusals: refusals.map((r) => `${r.cmd}: ${r.reason}`),
    ...(fake ? { fakeDaemonReceived: [...new Set(fake.received.map((r) => r.method))].sort() } : {}),
  };
  writeFileSync(join(opts.out, 'report.json'), `${JSON.stringify(report, null, 2)}\n`);
  for (const r of results) console.log(`${r.status.toUpperCase().padEnd(17)} ${r.id.padEnd(17)} ${r.note}\n${' '.repeat(35)}${r.screenshot}`);
  console.log(`mode=${report.mode} forwarded=[${forwarded.join(', ')}] refused=${refusals.length}${refusals.length ? ` (${[...new Set(report.refusals)].join('; ')})` : ''}`);
  console.log(`report: ${join(opts.out, 'report.json')}`);
  return results.some((r) => r.status === 'fail') ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((c) => process.exit(c), (e) => { console.error(e); process.exit(2); });
}
