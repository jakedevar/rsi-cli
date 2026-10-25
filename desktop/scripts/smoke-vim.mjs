#!/usr/bin/env node
// Real-key evidence for the desktop text fields' Vim editing (hub #3106, local #116).
//
//   node scripts/smoke-vim.mjs [--out DIR] [--chrome PATH]
//
// Boots the built desktop/dist in headless Chrome against the fake daemon of scripts/smoke-fake.mjs (read-only
// bridge, no rsid and no GUI permission) and drives CDP key events through MOUNTED controls: the launch dialog's
// textarea, an Issue form's one-line title and body, the message box, the Provider keys secret field. Each
// scenario asserts the resulting text, selection, mode and the `:` line's effect. The Issue lifecycle writes and the
// secret submission against a real rsid are the isolated run's (`smoke.mjs --isolated-write`, sections overlay-ref
// and TUI view modules); nothing here writes.
// Exit: 0 all pass, 1 a scenario failed, 2 no dist, 3 runner_unavailable (no Chrome).
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createBridge, rpcCall } from './smoke-bridge.mjs';
import { FIXTURES, startFakeDaemon } from './smoke-fake.mjs';
import { BOOT_ATTEMPTS, bootWithRetries, connectPage, findChrome, launchChrome, newToken, serve } from './smoke-lib.mjs';
export { BOOT_ATTEMPTS } from './smoke-lib.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const DIST = resolve(here, '..', 'dist');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pass = (note) => ({ status: 'pass', note });
const fail = (note) => ({ status: 'fail', note });

/** Fixtures for the surfaces this suite opens, on top of the smoke's. */
export const VIM_FIXTURES = {
  ...FIXTURES,
  // A three-file synthetic project for the file viewer's `:` line.
  ListProjectDirectory: (p) => ({ root: '/tmp/fake-project', path: p.path ?? '', truncated: false, entries: [
    { name: 'README.md', path: 'README.md', kind: 'file', size: 40, hidden: false, ignored: false },
  ] }),
  ListProjectFiles: () => ({ root: '/tmp/fake-project', paths: ['README.md'], truncated: false, source: 'walk' }),
  ReadProjectFile: (p) => ({ root: '/tmp/fake-project', path: p.path, size: 40, binary: false, truncated: false,
    content: 'line one\nline two\nline three\nline four\nline five\n' }),
  GetProjectFileGitGutter: (p) => ({ root: '/tmp/fake-project', path: p.path, status: 'tracked', markers: [] }),
  ListProviderCredentials: () => ({ credentials: [{ slot: 'anthropic', provider: 'Claude', state: 'absent', fingerprint: null, label: 'Anthropic API key' }] }),
};

export function parseVimArgs(argv) {
  const o = { out: '/tmp/desktop-vim', chrome: null };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--out') o.out = argv[++i];
    else if (argv[i] === '--chrome') o.chrome = argv[++i];
    else throw new Error(`unknown argument ${argv[i]}`);
  }
  return o;
}

const q = (s) => JSON.stringify(s);

/**
 * A navigation is not app readiness: boot awaits several bridge reads after DOMContentLoaded.
 * Retry only loads interrupted by Chrome (cancellation or ERR_NETWORK_CHANGED), using the shared first-run policy.
 * An interrupted module ends its wait at once; exhaustion retains every attempt's transport evidence.
 */
export async function freshVimPage(page, bridgeLog, { url = `${page.origin}/`, timeout = 12000, attempts = BOOT_ATTEMPTS } = {}) {
  const eventStart = page.events.length;
  const bridgeStart = bridgeLog.length;
  let failure;
  try {
    return await bootWithRetries(page, url, (stop) => bootOnce(page, timeout, stop), { attempts });
  } catch (error) { failure = error; }
  let dom;
  try {
    dom = await page.eval(`({ url: location.href, readyState: document.readyState,
      boot: document.documentElement.dataset.boot ?? null,
      bootReport: document.documentElement.dataset.bootReport ?? null,
      bridgeInstalled: !!globalThis.__TAURI__?.core?.invoke,
      invokeRetries: (globalThis.__smokeInvokeRetries ?? []).slice(-5),
      connection: document.querySelector('#conn')?.outerHTML,
      html: document.documentElement.outerHTML.slice(0, 16000) })`);
  } catch (captureError) { dom = { captureError: captureError.message }; }
  const diagnostics = {
    console: page.events.slice(eventStart).filter((e) => e.method === 'Runtime.exceptionThrown'
      || e.method === 'Log.entryAdded'
      || (e.method === 'Runtime.consoleAPICalled' && ['error', 'warning', 'assert'].includes(e.params.type))),
    network: page.events.slice(eventStart).filter((e) => e.method === 'Network.loadingFailed'),
    bridge: bridgeLog.slice(bridgeStart),
    ...(failure.bootAttempts?.length > 1 ? { attempts: failure.bootAttempts } : {}),
    dom,
  };
  throw new Error(`${failure.message}; startup diagnostics: ${JSON.stringify(diagnostics)}`, { cause: failure });
}

/** One navigation to a booted app: the boot report says ready and a fixture session row is painted. */
async function bootOnce(page, timeout, stop) {
  const deadline = Date.now() + timeout;
  const boot = await page.ready(`document.documentElement.dataset.boot`, { step: 'Vim app boot', timeout,
    action: `() => ({ state: document.documentElement.dataset.boot, report: document.documentElement.dataset.bootReport })`,
    stop });
  if (boot.state !== 'ready') throw new Error(`Vim app boot ${boot.state}: ${boot.report}`);
  // A generic li can be the empty-list placeholder. Require a painted fixture row.
  await page.element('#session-list .session-item[data-id]', undefined, {
    step: 'Vim session list', timeout: Math.max(0, deadline - Date.now()), stop,
  });
}

function helpers(page, bridgeLog) {
  const h = {
    page,
    bridgeLog,
    async closeAll() { for (let i = 0; i < 4; i++) { await page.key('Escape'); await sleep(120); } },
    async fresh() {
      await freshVimPage(page, bridgeLog);
    },
    /** Real key events for a vim-style sequence: `<Esc>`, `<Enter>`, `<C-r>`, `<Left>`, otherwise one key per character. */
    async send(seq) {
      for (const m of seq.matchAll(/<([^>]+)>|./gsu)) {
        const t = m[1] ?? m[0];
        if (m[1] === undefined) await page.key(t, { shift: /[A-Z~!@#$%^&*()_+{}|:"<>?]/.test(t) });
        else if (t === 'Esc') await page.key('Escape');
        else if (/^C-(.)$/.test(t)) await page.key(t.slice(2), { ctrl: true });
        else if (t === 'Space') await page.key(' ');
        else await page.key(t.replace(/^Left$/, 'ArrowLeft').replace(/^Right$/, 'ArrowRight'));
        await sleep(40);
      }
      await sleep(120);
    },
    val: (sel) => page.element(sel, '(e) => e.value'),
    mode: (sel) => page.element(sel, `(e) => e.dataset.vimMode ?? ''`),
    sel: (sel) => page.element(sel, '(e) => [e.selectionStart, e.selectionEnd]'),
    focus: (sel) => page.element(sel, '(e) => e.focus()', { step: 'focus field' }),
    async command(text) { await page.keys(`:${text}`); await sleep(150); await page.key('Enter'); await sleep(900); },
    async palette(text) {
      await page.element('#open-palette', '(e) => e.click()', { step: 'open Actions palette' });
      await page.element('#palette-input', undefined, { step: 'Actions palette accepts input' });
      await sleep(400);
      await page.keys(text);
      await sleep(300);
      await page.key('Enter');
      await sleep(900);
    },
    /** Set a field's text through typed keys: select everything the way a person would and retype. */
    async retype(sel, text) {
      await h.focus(sel);
      await page.element(sel, '(e) => e.select()', { step: 'select field text' });
      await page.key('Backspace');
      await page.keys(text);
      await sleep(100);
    },
    async openLaunch({ viaMenu = false } = {}) {
      if (viaMenu) await h.palette('New session');
      else await page.key('n', { ctrl: true });
      if (!(await page.waitFor(`document.getElementById('launch-dialog').open`, 4000))) throw new Error(`${viaMenu ? 'the Actions menu' : 'Ctrl+N'} did not open the launch dialog`);
      await h.focus('#launch-form textarea[name=query]');
      await sleep(150);
    },
    async openIssueForm() {
      // A second form in the same run: the workspace is already open, and `:issues` would be typed into its list as keys.
      if (await page.eval(`document.getElementById('issues-view').hidden`)) { await h.command('issues'); await sleep(400); }
      await page.element('#issues-list', '(e) => e.focus()', { step: 'focus the Issues list' });
      await page.key('n'); await sleep(500);
      if (!(await page.waitFor(`document.querySelector('dialog[open] [name=title]')`, 4000))) throw new Error('n did not open the Issue form');
    },
    /** The palette's "Vim keys" action (the top-bar checkbox is not reachable, hub #3101); reads the stored setting back. */
    async toggleVimKeys(on) {
      const stored = () => page.eval(`window.localStorage.getItem('rsi.desktop.vimKeys')`);
      if ((await stored()) === (on ? 'on' : 'off')) return;
      await h.palette('Vim keys');
      await h.closeAll();
      const now = await stored();
      if (now !== (on ? 'on' : 'off')) throw new Error(`Vim keys did not turn ${on ? 'on' : 'off'} (stored ${now})`);
    },
  };
  return h;
}

const LAUNCH_Q = '#launch-form textarea[name=query]';
const TITLE = 'dialog[open] [name=title]';
const BODY = 'dialog[open] textarea[name=body]';

const FORM_FIELDS = [['title', TITLE], ['description', BODY], ['labels', 'dialog[open] [name=labels]'], ['assignee', 'dialog[open] [name=assignee]']];
const FORM_TEXT = 'one two three';

/** Close every open dialog without keys (Esc would also leave the Issues workspace behind it). */
const closeDialogs = (page) => page.eval(`document.querySelectorAll('dialog[open]').forEach((d) => d.close())`);

/**
 * One mounted form field through the TUI's contract (hub #3421): it opens in INSERT with the caret at the end of its
 * text, Esc gives NORMAL only (the dialog stays open), a motion and an operator edit in place, u undoes, i is INSERT
 * again. Returns the problem, or '' when it all held.
 */
async function exerciseField(h, sel) {
  const { page } = h;
  await page.element(sel, `(e) => { e.value = ${q(FORM_TEXT)}; e.dispatchEvent(new CustomEvent('rsi-resync')); e.blur(); e.focus(); }`, { step: 'refocus field' });
  await sleep(200);
  const opened = await page.element(sel, '(e) => [e.dataset.vimMode, e.selectionStart, e.selectionEnd, document.activeElement === e]');
  if (opened[0] !== 'insert' || opened[1] !== FORM_TEXT.length || opened[2] !== FORM_TEXT.length || !opened[3]) return `opened as ${JSON.stringify(opened)}, not INSERT at the end`;
  await page.keys('!');
  if ((await h.val(sel)) !== FORM_TEXT + '!') return `typing gave ${JSON.stringify(await h.val(sel))}`;
  await h.send('<Esc>');
  const tag = await page.eval(`(document.querySelector(${q(sel + ' + .vim-mode-tag')})||{}).textContent||''`);
  if ((await h.mode(sel)) !== 'normal' || tag !== 'NORMAL') return `Esc gave mode ${await h.mode(sel)} tag ${tag}`;
  if (!(await page.eval(`!!document.querySelector('dialog[open]')`))) return 'Esc in INSERT closed the dialog';
  await h.send('0dw');
  const a = await h.val(sel);
  await h.send('$x');
  const b = await h.val(sel);
  await h.send('u');
  const c = await h.val(sel);
  await h.send('i');
  const m = await h.mode(sel);
  await h.send('<Esc>');
  if (a !== 'two three!' || b !== 'two three' || c !== 'two three!' || m !== 'insert') return `0dw → ${JSON.stringify(a)}, $x → ${JSON.stringify(b)}, u → ${JSON.stringify(c)}, i → ${m}`;
  return '';
}

export const SCENARIOS = [
  {
    id: 'modes', title: 'insert, normal, visual and visual-line modes in a dialog textarea, with the mode tag',
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openLaunch();
      const m0 = await h.mode(LAUNCH_Q);
      await page.keys('one two three');
      await h.send('<Esc>');
      const m1 = await h.mode(LAUNCH_Q);
      const tag1 = await page.eval(`(document.querySelector('${LAUNCH_Q} + .vim-mode-tag')||{}).textContent||''`);
      await h.send('v'); const m2 = await h.mode(LAUNCH_Q);
      await h.send('V'); const m3 = await h.mode(LAUNCH_Q);
      await h.send('<Esc>'); const m4 = await h.mode(LAUNCH_Q);
      const open = await page.eval(`document.getElementById('launch-dialog').open`);
      await h.send('i'); const m5 = await h.mode(LAUNCH_Q);
      await h.closeAll();
      const got = [m0, m1, m2, m3, m4, m5].join(',');
      return got === 'insert,normal,visual,visual-line,normal,insert' && tag1 === 'NORMAL' && open
        ? pass(`modes ${got}; the tag read ${tag1}; the first Esc left the dialog open`)
        : fail(`modes ${got}, tag ${JSON.stringify(tag1)}, dialog open ${open}`);
    },
  },
  {
    id: 'counts', title: 'counts: 3w, 2dw, d2w, 3x, 3~, 3rX, 2fb, 5j, 2dd, 3J, 3G and a 0 inside a count',
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openLaunch();
      const cases = [
        ['a b c d e f', '<Esc>0dw', 'b c d e f'],
        ['a b c d e f', '<Esc>02dw', 'c d e f'],
        ['a b c d e f', '<Esc>0d2w', 'c d e f'],
        ['abcdef', '<Esc>03x', 'def'],
        ['abcdef', '<Esc>03~', 'ABCdef'],
        ['abcdef', '<Esc>03rX', 'XXXdef'],
        ['xabcabcabc', '<Esc>0l2fbx', 'xabcacabc'],
        ['abcdefghijkl', '<Esc>010x', 'kl'],
        ['a b c d e f', '<Esc>03wD', 'a b c '],
      ];
      const bad = [];
      for (const [start, keys, want] of cases) {
        await h.retype(LAUNCH_Q, start);
        await h.send(keys);
        const got = await h.val(LAUNCH_Q);
        if (got !== want) bad.push(`${start} ${keys} => ${JSON.stringify(got)} (wanted ${JSON.stringify(want)})`);
        await h.send('i');
      }
      // Counts over lines need real newlines: Shift+Enter in a textarea.
      await h.retype(LAUNCH_Q, '1');
      for (const l of ['2', '3', '4', '5', '6']) { await page.key('Enter', { shift: true }); await page.keys(l); }
      await h.send('<Esc>gg5jx');
      const five = await h.val(LAUNCH_Q);
      if (five !== '1\n2\n3\n4\n5\n') bad.push(`5j x => ${JSON.stringify(five)}`);
      await h.send('gg2dd');
      const two = await h.val(LAUNCH_Q);
      if (two !== '3\n4\n5\n') bad.push(`2dd => ${JSON.stringify(two)}`);
      await h.send('3J');
      const joined = await h.val(LAUNCH_Q);
      if (joined !== '3 4 5') bad.push(`3J => ${JSON.stringify(joined)}`);
      await h.send('gg2Gx');
      await h.closeAll();
      return bad.length ? fail(bad.join('; ')) : pass(`${cases.length + 3} count cases through the launch dialog's textarea`);
    },
  },
  {
    id: 'text-objects', title: 'text objects: diw, daw, ciw + text, di", da", di(, da[, yi{ then P, dis, dip, and viw selection',
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openLaunch();
      const bad = [];
      const check = async (start, keys, want, extra) => {
        await h.retype(LAUNCH_Q, start);
        await h.send(keys);
        if (extra) await page.keys(extra.type);
        if (extra?.esc) await h.send('<Esc>');
        const got = await h.val(LAUNCH_Q);
        if (got !== want) bad.push(`${start} ${keys}${extra ? extra.type : ''} => ${JSON.stringify(got)} (wanted ${JSON.stringify(want)})`);
        await h.send('i');
      };
      await check('one two three', '<Esc>0wdiw', 'one  three');
      await check('one two three', '<Esc>0wdaw', 'one three');
      await check('one two three', '<Esc>0wciw', 'one X three', { type: 'X', esc: true });
      await check('say "hello" now', '<Esc>0fldi"', 'say "" now');
      await check('say "hello" now', '<Esc>0flda"', 'say  now');
      await check('f(a, b)', '<Esc>0fbdi(', 'f()');
      await check('xs[1, 2]', '<Esc>0f2da[', 'xs');
      await check('{ a b }', '<Esc>0fbdi{', '{}');
      await check('say "hello" now', '<Esc>0flyi"$p', 'say "hello" nowhello');
      // viw selects the word (real selection range), then c replaces it.
      await h.retype(LAUNCH_Q, 'one two three');
      await h.send('<Esc>0wviw');
      const range = await h.sel(LAUNCH_Q);
      const m = await h.mode(LAUNCH_Q);
      await h.send('<Esc>');
      await h.closeAll();
      if (range[0] !== 4 || range[1] !== 7 || m !== 'visual') bad.push(`viw selected ${range} in ${m}`);
      return bad.length ? fail(bad.join('; ')) : pass('nine operator/object edits and a viw selection (4..7) through real key events');
    },
  },
  {
    id: 'dot-undo-register', title: 'dot repeat of dw and of ciw + text, u / Ctrl-r, yy p, search f ; ,',
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openLaunch();
      const bad = [];
      await h.retype(LAUNCH_Q, 'a b c d');
      await h.send('<Esc>0dw.');
      let v = await h.val(LAUNCH_Q); if (v !== 'c d') bad.push(`dw . => ${JSON.stringify(v)}`);
      await h.send('u'); v = await h.val(LAUNCH_Q); if (v !== 'b c d') bad.push(`u => ${JSON.stringify(v)}`);
      await h.send('<C-r>'); v = await h.val(LAUNCH_Q); if (v !== 'c d') bad.push(`C-r => ${JSON.stringify(v)}`);
      await h.send('i');
      await h.retype(LAUNCH_Q, 'aa bb cc');
      await h.send('<Esc>0ciw'); await page.keys('X'); await h.send('<Esc>w.');
      v = await h.val(LAUNCH_Q); if (v !== 'X X cc') bad.push(`ciwX w . => ${JSON.stringify(v)}`);
      await h.send('i');
      await h.retype(LAUNCH_Q, 'a,b,c,d');
      await h.send('<Esc>0f,;x');
      v = await h.val(LAUNCH_Q); if (v !== 'a,bc,d') bad.push(`f, ; x => ${JSON.stringify(v)}`);
      await h.send('i');
      await h.retype(LAUNCH_Q, 'line');
      await h.send('<Esc>yyp');
      v = await h.val(LAUNCH_Q); if (v !== 'line\nline') bad.push(`yyp => ${JSON.stringify(v)}`);
      await h.closeAll();
      return bad.length ? fail(bad.join('; ')) : pass('dot, undo, redo, character search and a linewise put behave as the TUI');
    },
  },
  {
    id: 'one-line-field', title: "an Issue form's one-line title is a wrapping vim field: normal mode, o stays on one line, Enter still submits and adds no line, Esc Esc closes",
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openIssueForm();
      await h.focus(TITLE);
      await page.keys('alpha beta gamma');
      await h.send('<Esc>');
      const m1 = await h.mode(TITLE);
      await h.send('0dw');
      const a = await h.val(TITLE);
      await h.send('2x');
      const b = await h.val(TITLE);
      await h.send('o');
      await page.keys('Z');
      await h.send('<Esc>');
      const afterO = await h.val(TITLE);
      await h.send('0ci"'); // no quotes: nothing changes
      const noop = await h.val(TITLE);
      await h.send('J');
      const afterJ = await h.val(TITLE);
      const tag = await page.eval(`(document.querySelector('${TITLE} + .vim-mode-tag')||{}).textContent||''`);
      // Enter in normal mode must not be swallowed by the editor: the field takes it to submit the form, never as a line break.
      await page.eval(`window.__enterDefaultPrevented = null; window.addEventListener('keydown', (e) => { if (e.key === 'Enter') window.__enterDefaultPrevented = e.defaultPrevented; });`);
      await h.send('<Esc>');
      await page.key('Enter'); await sleep(200);
      const afterEnter = await h.val(TITLE);
      const enterPrevented = await page.eval(`window.__enterDefaultPrevented`);
      const submitted = h.bridgeLog.some((e) => e.cmd === 'create_issue' && e.outcome === 'refused');
      await h.closeAll();
      const closed = await page.eval(`!document.querySelector('dialog[open]')`);
      const ok = m1 === 'normal' && a === 'beta gamma' && b === 'ta gamma' && afterO === 'ta gammaZ' && noop === 'ta gammaZ' && afterJ === 'ta gammaZ' && tag === 'NORMAL' && closed && submitted && enterPrevented === true && !afterEnter.includes('\n');
      return ok
        ? pass(`title: normal ${m1}, 0dw -> ${JSON.stringify(a)}, 2x -> ${JSON.stringify(b)}, o appended on the same line -> ${JSON.stringify(afterO)}, J no-op, tag ${tag}, Enter was taken by the field to submit (default-prevented ${enterPrevented}, no line added) and reached the form's submit, which the read-only bridge refused (create_issue), Esc Esc closed the dialog`)
        : fail(`mode ${m1}, ${JSON.stringify([a, b, afterO, noop, afterJ])}, tag ${JSON.stringify(tag)}, closed ${closed}, submit seen ${submitted}, Enter prevented ${enterPrevented}`);
    },
  },
  {
    id: 'multi-line-field', title: "an Issue form's body textarea: o / O open lines, J joins, dd removes",
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openIssueForm();
      await h.focus(BODY);
      await page.keys('first');
      await h.send('<Esc>');
      await h.send('o'); await page.keys('second'); await h.send('<Esc>');
      await h.send('ggO'); await page.keys('zero'); await h.send('<Esc>');
      const lines = await h.val(BODY);
      await h.send('ggJ');
      const joined = await h.val(BODY);
      await h.send('dd');
      const after = await h.val(BODY);
      await h.closeAll();
      return lines === 'zero\nfirst\nsecond' && joined === 'zero first\nsecond' && after === 'second'
        ? pass('o, O, J and dd on a multi-line body')
        : fail(`lines ${JSON.stringify(lines)}, joined ${JSON.stringify(joined)}, after dd ${JSON.stringify(after)}`);
    },
  },
  {
    id: 'issue-editor-fields', title: "every text field of the Issue editor (title, description, labels, assignee) is a vim field: it opens in INSERT at the end of its text, Esc gives NORMAL only, a motion and an operator edit in place, u undoes, and Ctrl-Enter submits",
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openIssueForm();
      const problems = [];
      for (const [name, sel] of FORM_FIELDS) {
        const bad = await exerciseField(h, sel);
        if (bad) problems.push(`${name}: ${bad}`);
      }
      const kinds = await page.eval(`['title', 'body', 'labels', 'assignee'].map((n) => { const e = document.querySelector('dialog[open] [name=' + n + ']'); return n + ':' + e.tagName + ':' + (e.dataset.line ?? 'multi'); }).join(' ')`);
      const before = h.bridgeLog.filter((e) => e.cmd === 'create_issue' && e.outcome === 'refused').length;
      await h.focus(BODY);
      await page.key('Enter', { ctrl: true }); await sleep(300);
      const submitted = h.bridgeLog.filter((e) => e.cmd === 'create_issue' && e.outcome === 'refused').length - before;
      await closeDialogs(page);
      if (kinds !== 'title:TEXTAREA:single body:TEXTAREA:multi labels:TEXTAREA:single assignee:TEXTAREA:single') problems.push(`field kinds ${kinds}`);
      if (submitted !== 1) problems.push(`Ctrl-Enter reached the submit ${submitted} times`);
      return problems.length ? fail(problems.join('; ')) : pass(`title, description, labels and assignee: ${kinds}; each opened in INSERT at the end, Esc → NORMAL, 0 dw $x u edited in place; Ctrl-Enter submitted once (the read-only bridge refused create_issue)`);
    },
  },
  {
    id: 'issue-form-esc-contract', title: 'Esc leaves INSERT for NORMAL only; Esc in NORMAL on a dirty form shows UNSAVED CHANGES; a further Esc discards; Tab / Shift-Tab move between fields; the mode shows in the field border',
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openIssueForm();
      const open = () => page.eval(`!!document.querySelector('dialog[open] [name=title]')`);
      const err = () => page.eval(`(document.querySelector('dialog[open] .form-error')||{hidden:true}).hidden ? '' : document.querySelector('dialog[open] .form-error').textContent`);
      const border = () => page.element(TITLE, `(e) => getComputedStyle(e).borderTopColor`);
      const steps = [];
      const trace = [];
      const snap = async (label) => trace.push(`${label}: ${JSON.stringify(await page.eval(`(() => { const e = document.querySelector('dialog[open] [name=title]'); return e ? { mode: e.dataset.vimMode, value: e.value, sel: [e.selectionStart, e.selectionEnd], active: document.activeElement?.name, err: document.querySelector('dialog[open] .form-error')?.textContent ?? null } : null; })()`))}`);
      // A form nobody typed in: Esc leaves INSERT, the next Esc closes it.
      await h.focus(TITLE);
      await h.send('<Esc>'); steps.push(['pristine Esc 1 → normal, open', (await h.mode(TITLE)) === 'normal' && (await open())]);
      await h.send('<Esc>'); steps.push(['pristine Esc 2 closes', !(await open())]);
      await h.openIssueForm(); await h.focus(TITLE);
      await page.keys('abc');
      await snap('typed');
      const insertBorder = await border();
      steps.push(['opens in insert, typed abc', (await h.mode(TITLE)) === 'insert' && (await h.val(TITLE)) === 'abc']);
      // Tab / Shift-Tab move between fields; each opens in INSERT at the end of its text.
      await page.key('Tab'); await sleep(150);
      steps.push(['Tab → description in insert', (await page.eval(`document.activeElement.name`)) === 'body' && (await h.mode(BODY)) === 'insert']);
      await page.key('Tab', { shift: true }); await sleep(150);
      await snap('shift-tab');
      steps.push(['Shift-Tab → title in insert at its end', (await page.eval(`document.activeElement.name`)) === 'title' && (await h.mode(TITLE)) === 'insert' && JSON.stringify(await h.sel(TITLE)) === '[3,3]']);
      await h.send('<Esc>');
      await snap('esc');
      const normalBorder = await border();
      steps.push(['Esc in INSERT → NORMAL only (open, no warning)', (await h.mode(TITLE)) === 'normal' && (await open()) && (await err()) === '']);
      steps.push(['the mode shows in the border', insertBorder !== normalBorder]);
      await h.send('<Esc>');
      steps.push(['Esc in NORMAL on a dirty form → UNSAVED CHANGES', (await open()) && /UNSAVED CHANGES/.test(await err())]);
      await h.send('<Esc>');
      steps.push(['a further Esc discards', !(await open())]);
      // The mouse still works: a click lands in the field and the field is a vim field in INSERT.
      await h.openIssueForm();
      await page.click(`document.querySelector('dialog[open] [name=assignee]')`);
      await sleep(150);
      await page.keys('me');
      steps.push(['a click focuses a field in insert and types', (await h.val('dialog[open] [name=assignee]')) === 'me' && (await h.mode('dialog[open] [name=assignee]')) === 'insert']);
      await closeDialogs(page);
      const bad = steps.filter(([, ok]) => !ok).map(([n]) => n);
      return bad.length ? fail(`failed: ${bad.join('; ')}; borders ${insertBorder} / ${normalBorder}; ${trace.join(' | ')}`) : pass(`${steps.length} checks: ${steps.map(([n]) => n).join('; ')}`);
    },
  },
  {
    id: 'title-limit', title: 'the title is cut at 200 characters in the editor: a live counter from 150, typing past 200 is refused and a paste cut with a visible hint; no way of editing adds a line to a one-line field',
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.openIssueForm();
      const status = () => page.eval(`(() => { const s = document.querySelector('dialog[open] [name=title] ~ .line-status'); return s ? { text: s.textContent, hidden: s.hidden, state: s.dataset.state } : null; })()`);
      const setTitle = async (text) => {
        await page.element(TITLE, `(e) => { e.value = ${q(text)}; e.dispatchEvent(new CustomEvent('rsi-resync')); e.blur(); e.focus(); }`, { step: 'set title' });
        await sleep(150);
      };
      const steps = [];
      await setTitle('x'.repeat(100));
      steps.push(['no counter below 150', (await status())?.hidden === true]);
      await setTitle('y'.repeat(150));
      steps.push(['counter 150/200 from 150', (await status())?.text === '150/200' && !(await status()).hidden]);
      await setTitle('z'.repeat(197));
      await page.keys('abcde');
      let v = await h.val(TITLE);
      let s = await status();
      steps.push(['typing past 200 is refused with the hint', v.length === 200 && v.endsWith('zabc') && s?.state === 'refused' && /TITLE LIMIT/.test(s.text) && /200 characters/.test(s.text)]);
      await page.key('Backspace'); await sleep(100);
      s = await status();
      steps.push(['Backspace clears the hint, counter 199/200', (await h.val(TITLE)).length === 199 && s?.text === '199/200' && s.state === 'count']);
      // A paste is cut to what fits and its line break becomes a space.
      const paste = (text) => page.element(TITLE, `(e) => { const dt = new DataTransfer(); dt.setData('text/plain', ${q(text)}); e.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true })); }`, { step: 'paste' });
      await paste('A\nBCD'); await sleep(150);
      v = await h.val(TITLE); s = await status();
      steps.push(['a paste past 200 is cut with the hint', v.length === 200 && v.endsWith('abA') && s?.state === 'refused']);
      await setTitle('ab');
      await h.focus(TITLE);
      await page.key('Enter', { shift: true }); await sleep(100);
      await paste('x\ny'); await sleep(150);
      v = await h.val(TITLE);
      steps.push(['no line break by Shift-Enter or a paste', v === 'abx y']);
      // A vim edit that would pass the limit is undone whole, like the TUI's refused key.
      await setTitle('q'.repeat(150)); await h.send('<Esc>');
      await h.send('yy'); for (let i = 0; i < 3; i++) await h.send('P');
      v = await h.val(TITLE);
      steps.push(['P past 200 is refused whole', v === 'q'.repeat(150)]);
      await closeDialogs(page);
      const bad = steps.filter(([, ok]) => !ok).map(([n]) => n);
      return bad.length ? fail(`failed: ${bad.join('; ')}`) : pass(`${steps.length} checks: ${steps.map(([n]) => n).join('; ')}`);
    },
  },
  {
    id: 'project-form-fields', title: "the project form's name and folder are wrapping vim fields (create): INSERT at the end, Esc → NORMAL only, 0 dw $x u edit in place, Enter submits, Esc in NORMAL closes",
    async run(h) {
      const { page } = h;
      await h.fresh();
      await h.palette('New project');
      if (!(await page.waitFor(`document.querySelector('dialog.pick-form-dialog[open] [name=name]')`, 5000))) return fail('New project did not open the form');
      const NAME = 'dialog.pick-form-dialog[open] [name=name]';
      const PATH = 'dialog.pick-form-dialog[open] [name=path]';
      const problems = [];
      for (const [name, sel] of [['name', NAME], ['folder', PATH]]) {
        const bad = await exerciseField(h, sel);
        if (bad) problems.push(`${name}: ${bad}`);
      }
      const kinds = await page.eval(`['name', 'path'].map((n) => { const e = document.querySelector('dialog.pick-form-dialog[open] [name=' + n + ']'); return n + ':' + e.tagName + ':' + e.dataset.line; }).join(' ')`);
      if (kinds !== 'name:TEXTAREA:single path:TEXTAREA:single') problems.push(`field kinds ${kinds}`);
      // The last field was left in NORMAL: leave and re-enter each so focus puts it in INSERT again before retyping.
      for (const [sel, text] of [[PATH, ''], [NAME, 'demo']]) {
        await page.element(sel, '(e) => { e.blur(); e.focus(); }', { step: 'refocus field' });
        await h.retype(sel, text);
      }
      const before = h.bridgeLog.filter((e) => e.cmd === 'create_project' && e.outcome === 'refused').length;
      await page.key('Enter'); await sleep(400);
      const submitted = h.bridgeLog.filter((e) => e.cmd === 'create_project' && e.outcome === 'refused').length - before;
      if (submitted !== 1) problems.push(`Enter reached the submit ${submitted} times (the read-only bridge refuses create_project)`);
      if ((await h.val(NAME)) !== 'demo') problems.push(`Enter changed the name to ${JSON.stringify(await h.val(NAME))}`);
      await h.focus(NAME);
      await h.send('<Esc>');
      const stillOpen = await page.eval(`!!document.querySelector('dialog.pick-form-dialog[open]')`);
      await h.send('<Esc>');
      const closed = await page.eval(`!document.querySelector('dialog.pick-form-dialog[open]')`);
      if (!stillOpen || !closed) problems.push(`Esc in INSERT left the form open ${stillOpen}, Esc in NORMAL closed it ${closed}`);
      await closeDialogs(page);
      return problems.length ? fail(problems.join('; ')) : pass(`name and folder: ${kinds}; each opened in INSERT at the end, Esc → NORMAL, 0 dw $x u edited in place; Enter submitted once; Esc then Esc closed the form`);
    },
  },
  {
    id: 'issue-filter-dependency-fields', title: 'the Issue filter, search and dependency inputs are wrapping vim fields too (every text field of every Issue form)',
    async run(h) {
      const { page } = h;
      await h.fresh();
      await h.command('issues'); await sleep(600);
      const problems = [];
      for (const [key, title] of [['f', 'Filter Issues'], ['/', 'Search Issues'], ['b', 'Dependencies of']]) {
        await page.key(key); await sleep(500);
        const got = await page.eval(`(document.querySelector('dialog[open] h2')||{}).textContent||''`);
        if (!got.startsWith(title)) { problems.push(`${key} opened ${JSON.stringify(got)}, not ${title}`); await closeDialogs(page); continue; }
        const names = await page.eval(`[...document.querySelectorAll('dialog[open] .form-fields [name]')].filter((e) => e.tagName === 'TEXTAREA' || (e.tagName === 'INPUT' && e.type === 'text')).map((e) => e.name + ':' + e.tagName + ':' + (e.dataset.line ?? ''))`);
        if (names.length === 0) problems.push(`${title}: no text field`);
        for (const n of names) {
          const [name, tag, line] = n.split(':');
          if (tag !== 'TEXTAREA' || line !== 'single') problems.push(`${title}/${name} is ${tag} ${line}, not a wrapping line field`);
          const bad = await exerciseField(h, `dialog[open] [name=${name}]`);
          if (bad) problems.push(`${title}/${name}: ${bad}`);
        }
        await closeDialogs(page);
      }
      return problems.length ? fail(problems.join('; ')) : pass('filter (text, priorities, assignee, labels, provenance), search and the dependency number: every text field is a wrapping line field and edits as a vim field');
    },
  },
  {
    id: 'form-fields-vim-off', title: 'with Vim keys off the Issue form fields edit plainly: no mode, Esc on a dirty form warns before it discards, Enter submits and adds no line, the title is still cut at 200',
    async run(h) {
      const { page } = h;
      await h.fresh();
      await h.toggleVimKeys(false);
      // Vim's `:issues` and `n` are off: the Actions menu opens the workspace and its button the form.
      await h.palette('Issues workspace');
      await page.element('#issues-view:not([hidden])', undefined, { step: 'Issues workspace open' });
      await sleep(400);
      await page.click(`document.querySelector('#issues-view [data-action=issues-new]')`);
      if (!(await page.waitFor(`document.querySelector('dialog[open] [name=title]')`, 4000))) return fail('New Issue did not open the form');
      await h.focus(TITLE);
      await page.keys('plain text');
      const steps = [];
      await page.key('Home'); await page.key('Delete'); await sleep(100);
      steps.push(['plain Home/Delete editing', (await h.val(TITLE)) === 'lain text']);
      steps.push(['no vim mode', (await h.mode(TITLE)) === '']);
      await page.key('Enter'); await sleep(300);
      steps.push(['Enter adds no line and submits', !(await h.val(TITLE)).includes('\n') && h.bridgeLog.some((e) => e.cmd === 'create_issue' && e.outcome === 'refused')]);
      await page.element(TITLE, `(e) => { e.value = ${q('w'.repeat(199))}; e.dispatchEvent(new CustomEvent('rsi-resync')); e.blur(); e.focus(); }`, { step: 'long title' });
      await sleep(100);
      await page.keys('ab');
      steps.push(['the cap holds without Vim keys', (await h.val(TITLE)).length === 200]);
      await page.key('Escape'); await sleep(200);
      const warned = await page.eval(`!!document.querySelector('dialog[open] [name=title]') && /UNSAVED CHANGES/.test(document.querySelector('dialog[open] .form-error').textContent)`);
      steps.push(['Esc on a dirty form warns first', warned]);
      await page.key('Escape'); await sleep(200);
      steps.push(['a further Esc discards', await page.eval(`!document.querySelector('dialog[open] [name=title]')`)]);
      await h.toggleVimKeys(true);
      const bad = steps.filter(([, ok]) => !ok).map(([n]) => n);
      return bad.length ? fail(`failed: ${bad.join('; ')}`) : pass(`${steps.length} checks: ${steps.map(([n]) => n).join('; ')}`);
    },
  },
  {
    id: 'colon-viewer', title: "the file viewer's colon line: :3 goes to line 3, :w / :wq / :x on an unedited file rewrite it like the TUI (this fake daemon gives no file version, so each is refused with that reason and the viewer stays open), :q closes the viewer",
    async run(h) {
      const { page } = h;
      await h.fresh();
      await page.key('j'); await sleep(150); await page.key('j'); await sleep(150);
      await page.key('Enter');
      await page.waitFor(`!document.getElementById('session-view').hidden`, 6000);
      await sleep(300);
      await page.keys(' e');
      if (!(await page.waitFor(`(document.querySelector('.files-overlay')||{}).innerText?.includes('README.md')`, 6000))) return fail('the explorer did not list README.md');
      await page.key('Enter');
      if (!(await page.waitFor(`(document.querySelector('.files-overlay')||{}).innerText?.includes('line three')`, 6000))) return fail('the viewer did not show README.md');
      const bad = [];
      const toast = () => page.eval(`(document.getElementById('toast')||{}).innerText||''`);
      const cursorLine = () => page.eval(`(document.querySelector('.files-overlay .cursor, .files-overlay [aria-current=true], .files-overlay .files-cursor')||{}).innerText||''`);
      await h.send(':'); await sleep(200);
      const lineOpen = await page.eval(`!document.querySelector('.files-overlay .files-cmdline')?.hidden`);
      await page.keys('3'); await h.send('<Enter>'); await sleep(400);
      const at3 = await cursorLine();
      if (!/line three/.test(at3)) bad.push(`:3 left the cursor on ${JSON.stringify(at3)}`);
      for (const c of ['w', 'wq', 'x']) {
        await h.send(':'); await page.keys(c); await h.send('<Enter>'); await sleep(300);
        const t = await toast();
        const still = await page.eval(`(document.querySelector('.files-overlay')||{}).innerText?.includes('line three')`);
        if (!/could not fingerprint/i.test(t) || !still) bad.push(`:${c} said ${JSON.stringify(t)}, viewer open ${still}`);
      }
      await h.send(':'); await page.keys('q'); await h.send('<Enter>'); await sleep(400);
      const closed = await page.eval(`!(document.querySelector('.files-overlay')||{}).innerText?.includes('line three')`);
      if (!closed) bad.push(':q did not close the viewer');
      if (!lineOpen) bad.push('the viewer command line did not open');
      await h.closeAll();
      return bad.length ? fail(bad.join('; ')) : pass(':3 moved the cursor to "line three"; :w, :wq, :x each tried to rewrite the unedited file, were refused (no file version here) and left the viewer open; :q closed it');
    },
  },
  {
    id: 'vim-off', title: 'with Vim keys off: ordinary editing, Esc closes, the mouse and menu still work, no mode tag',
    async run(h) {
      const { page } = h;
      await h.fresh(); await h.toggleVimKeys(false);
      // Ctrl+N is a vim-key chord; with Vim keys off the Actions menu (a real click) opens the dialog.
      await h.openLaunch({ viaMenu: true });
      await page.keys('hello world');
      const typed = await h.val(LAUNCH_Q);
      await page.key('ArrowLeft'); await page.key('Backspace');
      const edited = await h.val(LAUNCH_Q);
      // Plain letters are text, not commands: dw types.
      await page.key('End');
      await page.keys(' dw');
      const literal = await h.val(LAUNCH_Q);
      const mode = await h.mode(LAUNCH_Q);
      const tag = await page.eval(`!!document.querySelector('${LAUNCH_Q} + .vim-mode-tag')`);
      // A real double click (mouse) on the first word selects it.
      const at = await page.element(LAUNCH_Q, '(e) => { const r = e.getBoundingClientRect(); return { x: r.x + 18, y: r.y + 14 }; }', { step: 'double click launch query' });
      for (const type of ['mousePressed', 'mouseReleased']) await page.send('Input.dispatchMouseEvent', { type, x: at.x, y: at.y, button: 'left', clickCount: 2 });
      await sleep(150);
      const all = await h.sel(LAUNCH_Q);
      // Esc is the dialog's own: one press closes it.
      await page.key('Escape'); await sleep(250);
      const closed = await page.eval(`!document.getElementById('launch-dialog').open`);
      // The menu: the palette button still opens by mouse.
      await page.click(`document.getElementById('open-palette')`); await sleep(400);
      const palette = await page.eval(`!!document.querySelector('dialog[open] #palette-input, #palette-input')&&getComputedStyle(document.getElementById('palette-input')).display!=='none'`);
      await h.closeAll();
      await h.toggleVimKeys(true);
      return typed === 'hello world' && edited === 'hello word' && literal === 'hello word dw' && mode === '' && !tag && closed && palette && all[0] === 0 && all[1] === 5
        ? pass(`typed, ArrowLeft+Backspace and literal "dw" worked; no mode attribute or tag; Esc closed the dialog; a double click selected ${all}; the palette opened by mouse`)
        : fail(`typed ${JSON.stringify(typed)}, edited ${JSON.stringify(edited)}, literal ${JSON.stringify(literal)}, mode ${JSON.stringify(mode)}, tag ${tag}, closed ${closed}, palette ${palette}`);
    },
  },
  {
    id: 'secret-field', title: 'a secret field stays masked and is never a vim field: Esc and normal-mode letters type as text',
    async run(h) {
      const { page } = h;
      await h.fresh();
      await h.palette('Provider keys');
      if (!(await page.waitFor(`(document.querySelector('dialog[open]')||{}).innerText?.includes('Provider keys')`, 5000))) return fail('the Provider keys dialog did not open');
      await page.key('s');
      if (!(await page.waitFor(`document.querySelector('dialog[open] input[type=password]')`, 4000))) return fail('s did not open the key form');
      const SEL = 'dialog[open] input[type=password]';
      await h.focus(SEL);
      await page.keys('synthetic-e2e-abc');
      const type = await page.element(SEL, '(e) => e.type');
      const vimAttr = await page.element(SEL, `(e) => e.dataset.vimMode ?? ''`);
      const tag = await page.eval(`!!document.querySelector(${q(SEL + ' + .vim-mode-tag')})`);
      const value = await h.val(SEL);
      const shown = await page.eval(`document.body.innerText.includes('synthetic-e2e-abc')`);
      // Esc is the dialog's own here: one press closes it, it does not enter normal mode.
      await page.key('Escape'); await sleep(250);
      const closed = await page.eval(`!document.querySelector(${q(SEL)})`);
      await h.closeAll();
      return type === 'password' && vimAttr === '' && !tag && !shown && value === 'synthetic-e2e-abc' && closed
        ? pass(`input type=${type}: typed as text, no vim mode attribute or tag, the value is not in the page text, one Esc closed the form`)
        : fail(`type ${type}, vim attr ${JSON.stringify(vimAttr)}, tag ${tag}, value shown in the page ${shown}, closed ${closed}`);
    },
  },
];

export async function runVimSuite({ dist = DIST, chromeBin, out, only = null, signal, scenarios = SCENARIOS }) {
  mkdirSync(out, { recursive: true });
  const fake = await startFakeDaemon(VIM_FIXTURES);
  const bridgeLog = [];
  let server, chrome, page;
  const results = [];
  try {
    const bridge = createBridge({ call: (m, p) => {
      bridgeLog.push({ method: m, outcome: 'started' });
      return rpcCall(fake.socket, m, p);
    }, record: (e) => bridgeLog.push(e) });
    const token = newToken();
    const served = await serve({ dist, bridge, token, onRefusal: (e) => bridgeLog.push(e), signal });
    server = served.server;
    for (const s of scenarios) {
      if (only && !only.includes(s.id)) continue;
      try {
        // Independent scenarios need independent renderers and module caches. Repeated
        // navigation in a reused runtime produced canceled module loads under the CPU quota (#492),
        // leaving readyState=complete with no main/boot execution and no console error.
        chrome = await launchChrome(chromeBin, { signal });
        page = await connectPage(chrome.cdpPort, { signal });
        page.origin = `http://127.0.0.1:${served.port}`;
        let r;
        try { r = await s.run(helpers(page, bridgeLog)); } catch (e) { r = fail(`threw: ${e.message}`); }
        const shot = await page.shot(join(out, `${s.id}.png`)).catch(() => null);
        results.push({ id: s.id, title: s.title, ...r, screenshot: shot });
      } finally {
        page?.close();
        await chrome?.stop();
        page = undefined;
        chrome = undefined;
      }
    }
  } finally {
    page?.close();
    server?.closeAllConnections();
    server?.close();
    try { await chrome?.stop(); } finally { await fake.stop(); }
  }
  const refusals = bridgeLog.filter((e) => e.outcome === 'refused');
  const report = { chrome: chromeBin, results, refusals: refusals.map((r) => `${r.cmd}: ${r.reason}`) };
  writeFileSync(join(out, 'vim-report.json'), `${JSON.stringify(report, null, 2)}\n`);
  return report;
}

export async function main(argv = process.argv.slice(2)) {
  const opts = parseVimArgs(argv);
  if (!existsSync(join(DIST, 'index.html'))) { console.error('desktop/dist is missing: run `npm run build` in desktop/ first'); return 2; }
  const bin = opts.chrome ?? findChrome();
  if (!bin) { console.log('runner_unavailable: no Chrome/Chromium found (set RSI_SMOKE_CHROME or pass --chrome); 0 scenarios executed'); return 3; }
  const report = await runVimSuite({ chromeBin: bin, out: opts.out });
  for (const r of report.results) console.log(`${r.status.toUpperCase().padEnd(5)} ${r.id.padEnd(20)} ${r.note}`);
  console.log(`report: ${join(opts.out, 'vim-report.json')} refused=${report.refusals.length}`);
  return report.results.some((r) => r.status === 'fail') ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((c) => process.exit(c), (e) => { console.error(e); process.exit(2); });
}
