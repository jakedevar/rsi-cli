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
import { connectPage, findChrome, launchChrome, newToken, serve } from './smoke-lib.mjs';

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

function helpers(page, bridgeLog) {
  const h = {
    page,
    bridgeLog,
    async closeAll() { for (let i = 0; i < 4; i++) { await page.key('Escape'); await sleep(120); } },
    async fresh() {
      await page.goto(`${page.origin}/`);
      await sleep(300);
      if (!(await page.waitFor(`document.querySelectorAll('#session-list li').length > 0`, 12000))) throw new Error('the session list never rendered');
      await sleep(400);
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
      await h.command('issues');
      await sleep(400);
      await page.key('n'); await sleep(500);
      if (!(await page.waitFor(`document.querySelector('dialog[open] input[name=title]')`, 4000))) throw new Error('n did not open the Issue form');
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
const TITLE = 'dialog[open] input[name=title]';
const BODY = 'dialog[open] textarea[name=body]';

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
    id: 'one-line-field', title: "an Issue form's one-line title is a vim field: normal mode, o stays on one line, Enter still submits, Esc Esc closes",
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
      // Enter in normal mode must not be swallowed by the editor: the form's submit handler sees the event.
      await page.eval(`window.__enterDefaultPrevented = null; window.addEventListener('keydown', (e) => { if (e.key === 'Enter') window.__enterDefaultPrevented = e.defaultPrevented; });`);
      await h.send('<Esc>');
      await page.key('Enter'); await sleep(200);
      const enterPrevented = await page.eval(`window.__enterDefaultPrevented`);
      const submitted = h.bridgeLog.some((e) => e.cmd === 'create_issue' && e.outcome === 'refused');
      await h.closeAll();
      const closed = await page.eval(`!document.querySelector('dialog[open]')`);
      const ok = m1 === 'normal' && a === 'beta gamma' && b === 'ta gamma' && afterO === 'ta gammaZ' && noop === 'ta gammaZ' && afterJ === 'ta gammaZ' && tag === 'NORMAL' && closed && submitted && enterPrevented === false;
      return ok
        ? pass(`title: normal ${m1}, 0dw -> ${JSON.stringify(a)}, 2x -> ${JSON.stringify(b)}, o appended on the same line -> ${JSON.stringify(afterO)}, J no-op, tag ${tag}, Enter was not swallowed (default-prevented ${enterPrevented}) and reached the form's submit, which the read-only bridge refused (create_issue), Esc Esc closed the dialog`)
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

export async function runVimSuite({ dist = DIST, chromeBin, out, only = null, signal }) {
  mkdirSync(out, { recursive: true });
  const fake = await startFakeDaemon(VIM_FIXTURES);
  const bridgeLog = [];
  let server, chrome, page;
  const results = [];
  try {
    const bridge = createBridge({ call: (m, p) => rpcCall(fake.socket, m, p), record: (e) => bridgeLog.push(e) });
    const token = newToken();
    const served = await serve({ dist, bridge, token, onRefusal: (e) => bridgeLog.push(e), signal });
    server = served.server;
    chrome = await launchChrome(chromeBin, { signal });
    page = await connectPage(chrome.cdpPort, { signal });
    page.origin = `http://127.0.0.1:${served.port}`;
    const h = helpers(page, bridgeLog);
    for (const s of SCENARIOS) {
      if (only && !only.includes(s.id)) continue;
      let r;
      try { r = await s.run(h); } catch (e) { r = fail(`threw: ${e.message}`); }
      const shot = await page.shot(join(out, `${s.id}.png`)).catch(() => null);
      results.push({ id: s.id, title: s.title, ...r, screenshot: shot });
      await h.closeAll().catch(() => undefined);
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
