#!/usr/bin/env node
// The on-demand desktop UX audit runner (local #277). NOT part of `npm test` or the smoke groups: it reports numbers for a
// human to read, it does not pass or fail a build.
//
//   npm run audit -- sizes|themes|scale|all [--out DIR] [--sessions N] [--events N]
//   npm run audit -- themes [--themes all|KEY,KEY]   every built-in theme (14) plus default dark and system light; --themes picks some; each theme's result is appended to themes.jsonl as it ends
//   npm run audit -- views [--size WxH] [--theme T] [--out DIR]   every main view once: contrast, cut-off controls, scroll
//   npm run audit -- views --slow 3000 [--size WxH]  every view with each daemon read delayed 3 s: is anything drawn within 300 ms?
//   npm run audit -- views --refuse [--size WxH]     every view with each daemon read refused: the error text each shows
//   npm run audit -- keys [--overlay "Sort Picker,Trash"]   press every key each key table lists (dialogs, panes, forms; the tables not swept are named in report.json `skipped`), in the fake-daemon page; FAIL when a key changes nothing (audit-keysweep.mjs)
//   npm run audit -- flow FILE.mjs [--out DIR]       run your own flow with the toolkit (docs/ux-audit.md)
//
// It boots the built dist/ against the fake daemon with the write-stub bridge (writes are recorded, never sent), writes
// screenshots and `report.json` under --out (default /tmp/rsi-audit-<time>), and prints a short summary.
//   sizes   760x480, 1024x640, 1280x800, 2560x1440: document scroll size, cut-off controls and overflowing text per view
//   themes  default dark, default under a light system and every built-in theme (THEMES of the built app, 14): text-contrast failures in EVERY main view
//   views   one window size and one theme (--size, --theme), every main view: contrast, cut-off controls, document scroll
//   scale   --sessions (800) sessions and a --events (5000) event conversation: first paint, DOM size, long tasks, scroll,
//           filter typing, opening the long conversation, polling, composer latency
import { appendFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import * as lib from './audit-lib.mjs';
import { auditViews } from './audit-views.mjs';

const { open, sleep, clickSel, clickText, mouse, openSessionByTitle, openPaletteAction, contrastAudit, scaleFixtures, waitForRowCount } = lib;

const SIZES = [[760, 480], [1024, 640], [1280, 800], [2560, 1440]];
/** The runs for these saved theme keys, after the two default runs (dark, and dark under a light system). Light runs under a light system. */
export function themeRunsFor(keys) {
  return [
    { name: 'default-dark', theme: null, scheme: null },
    { name: 'default-system-light', theme: null, scheme: 'light' },
    ...keys.map((key) => ({ name: key, theme: key, scheme: key === 'light' ? 'light' : 'dark' })),
  ];
}
const THEME_RUNS = themeRunsFor(['light', 'high-contrast', 'transparent', 'truly-transparent']);
const SESSION_TITLE = 'Fake waiting worker';
const VIEW_SIZE = [1024, 640];

/** A theme argument: a THEME_RUNS name (`dark`, `system-light` too) or a saved theme key such as `light` or `high-contrast`. */
export function themeRun(name) {
  if (name === null || name === undefined) return THEME_RUNS[0];
  const alias = { dark: 'default-dark', 'system-light': 'default-system-light' }[name] ?? name;
  const known = THEME_RUNS.find((r) => r.name === alias);
  if (known) return known;
  if (!/^[a-z][a-z0-9-]*$/.test(name)) throw new Error(`--theme takes a theme name such as dark, system-light, light or high-contrast, not ${name}`);
  return { name, theme: name, scheme: name === 'light' ? 'light' : null };
}

/** `--slow 3000` -> 3000 (ms each daemon read waits). */
export function parseSlow(text) {
  const ms = Number(text);
  if (!/^\d+$/.test(text ?? '') || ms < 1 || ms > 60000) throw new Error(`--slow takes the milliseconds each read waits (1 to 60000), such as 3000, not ${text}`);
  return ms;
}

/** `--themes light,ruby` -> ['light', 'ruby'] (saved theme keys; `all` or no flag -> null, every built-in theme). */
export function parseThemeKeys(text) {
  if (text === undefined || text === 'all') return null;
  const keys = text.split(',').map((k) => k.trim());
  if (!keys.length || keys.some((k) => !/^[a-z][a-z0-9-]*$/.test(k))) throw new Error(`--themes takes all or comma-separated theme keys such as light,ruby, not ${text}`);
  return keys;
}

/** `--size 760x480` -> [760, 480]. */
export function parseSize(text) {
  const m = /^(\d{3,5})x(\d{3,5})$/.exec(text ?? '');
  if (!m) throw new Error(`--size takes WIDTHxHEIGHT such as 1024x640, not ${text}`);
  return [Number(m[1]), Number(m[2])];
}

/** Run `fn`; a view that cannot be reached (the UI changed) is a note in the report, not a crash of the audit. */
async function tryStep(notes, name, fn) {
  try { return await fn(); } catch (e) { notes.push({ step: name, error: String(e.message ?? e).split('\n')[0] }); return undefined; }
}

const ARGS = { out: null, sessions: 800, events: 5000, size: null, theme: null, slow: null, refuse: false, themes: null, overlay: null };
export function parseArgs(argv) {
  const [command, ...rest] = argv;
  const o = { command: command ?? 'all', ...ARGS, file: null };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === '--out') o.out = rest[++i];
    else if (a === '--sessions') o.sessions = Number(rest[++i]);
    else if (a === '--events') o.events = Number(rest[++i]);
    else if (a === '--size') o.size = parseSize(rest[++i]);
    else if (a === '--theme') { o.theme = rest[++i]; themeRun(o.theme); }
    else if (a === '--slow') o.slow = parseSlow(rest[++i]);
    else if (a === '--refuse') o.refuse = true;
    else if (a === '--overlay') o.overlay = (rest[++i] ?? '').split(',').map((n) => n.trim()).filter(Boolean);
    else if (a === '--themes') o.themes = parseThemeKeys(rest[++i]);
    else if (o.command === 'flow' && !o.file && !a.startsWith('--')) o.file = a;
    else throw new Error(`unknown argument ${a}`);
  }
  if (!['sizes', 'themes', 'scale', 'all', 'flow', 'views', 'keys'].includes(o.command)) throw new Error(`the command is sizes, themes, views, keys, scale, all or flow, not ${o.command}`);
  if (o.command === 'flow' && !o.file) throw new Error('flow takes the path of a .mjs file');
  if ((o.size || o.theme) && o.command !== 'views') throw new Error('--size and --theme belong to the views command');
  if (o.overlay && o.command !== 'keys') throw new Error('--overlay belongs to the keys command');
  if (o.overlay && o.overlay.length === 0) throw new Error('--overlay takes overlay names, such as "Sort Picker,Trash"');
  if (o.themes && o.command !== 'themes' && o.command !== 'all') throw new Error('--themes belongs to the themes command');
  if ((o.slow !== null || o.refuse) && o.command !== 'views') throw new Error('--slow and --refuse belong to the views command');
  if (o.slow !== null && o.refuse) throw new Error('--slow and --refuse are two runs: pick one');
  if (!Number.isInteger(o.sessions) || o.sessions < 1 || !Number.isInteger(o.events) || o.events < 0) throw new Error('--sessions and --events take whole numbers');
  return o;
}

/** Layout at each window size, on the main views. */
export async function runSizes({ out, sizes = SIZES }) {
  const results = [];
  for (const [w, h] of sizes) {
    const a = await open({ width: w, height: h });
    const notes = [];
    const views = [];
    try {
      await sleep(600);
      const record = async (view) => {
        await a.page.shot(join(out, `size-${w}x${h}-${view}.png`));
        views.push({ view, doc: await a.page.eval(lib.docScroll), cutOff: await a.page.eval(lib.cutOffControls), overflow: await a.page.eval(lib.overflowingText) });
      };
      await record('list');
      await tryStep(notes, 'session', async () => { await openSessionByTitle(a.page, SESSION_TITLE); await record('session'); });
      await tryStep(notes, 'collapsed', async () => { await clickText(a.page, 'Collapse'); await record('collapsed'); await clickText(a.page, 'Expand'); });
      await tryStep(notes, 'launch', async () => { await clickText(a.page, 'New session'); await sleep(300); await record('launch'); await a.page.key('Escape'); await sleep(300); });
      await tryStep(notes, 'issues', async () => { await clickText(a.page, 'Issues'); await sleep(800); await record('issues'); await clickText(a.page, 'Close'); });
      for (const view of ['settings', 'dashboard']) {
        await tryStep(notes, view, async () => {
          await clickText(a.page, 'Actions…'); await sleep(250); await a.page.keys(view); await a.page.key('Enter'); await sleep(1000);
          await record(view); await a.page.key('Escape'); await sleep(300);
        });
      }
    } finally { await a.close(); }
    results.push({ width: w, height: h, views, notes });
  }
  return results;
}

/** What a read refusal looks like in the report and what `--refuse` sends (the real backend format, src-tauri daemon.rs). */
export const REFUSE_ALL = { '*': { message: 'audit: this read is refused', code: 'audit_refused' } };
/** A view drawing something within this many ms of being chosen is what the slow and refused modes ask about. */
export const DRAW_BUDGET_MS = 300;

/** The page's visible state: its text plus which dialogs are open. The palette is left out (it closes when an action is chosen). */
const pageSignature = `(() => [document.body.innerText, ...[...document.querySelectorAll('dialog[open]')].filter((d) => d.id !== 'palette').map((d) => d.id || d.className)].join('\\u0001'))()`;
const paletteIsOpen = `!!document.getElementById('palette')?.open`;
const visibleProblemText = `(() => {
  const toast = document.getElementById('toast')?.checkVisibility() ? document.getElementById('toast').innerText.trim().replace(/\\s*\\n\\s*/g, ' ') : '';
  const text = document.body.innerText.split('\\n').map((l) => l.trim()).filter(Boolean);
  const line = text.find((l) => /daemon error|cannot reach|refus|could not|failed to|unavailable|error:/i.test(l)) ?? '';
  const retry = [...document.querySelectorAll('dialog[open] button')].some((b) => /^Retry/.test(b.textContent.trim()) && b.checkVisibility());
  return { toast: toast.slice(0, 200), errorLine: line.slice(0, 200), retry };
})()`;

/**
 * Choose a view (palette row, real mouse) and report what the page drew and when: `drawnWithin300` (anything visible changed within
 * ${DRAW_BUDGET_MS} ms of the click: a dialog, a "Loading…" line, a toast, a pane), `firstDrawMs` (null when nothing came within `maxMs`),
 * the error text shown once it settled and the reads the page made. The caller sets `a.reads` (delay or refusal) before it calls this.
 */
export async function probeViewOpen(a, view, { maxMs = 2000, shotPath = null, settleBusyMs = 0 } = {}) {
  const base = await a.page.eval(pageSignature);
  const callsBefore = a.reads.calls.length;
  const r = await openPaletteAction(a.page, view.title, { settleMs: 0 });
  const row = { opened: r.opened };
  if (!r.opened) { row.why = r.why; return row; }
  const t0 = Date.now();
  const drawn = async () => !(await a.page.eval(paletteIsOpen)) && (await a.page.eval(pageSignature)) !== base;
  let firstDrawMs = null;
  let shot = false;
  while (Date.now() - t0 < maxMs) {
    if (await drawn()) { firstDrawMs = Date.now() - t0; break; }
    if (shotPath && !shot && Date.now() - t0 >= DRAW_BUDGET_MS) { shot = true; await a.page.shot(shotPath); }
    await sleep(25);
  }
  row.firstDrawMs = firstDrawMs;
  row.drawnWithin300 = firstDrawMs !== null && firstDrawMs <= DRAW_BUDGET_MS;
  await sleep(firstDrawMs === null ? 0 : 400);
  // Refused reads answer at once, so a view that is still measuring or loading after the first draw has not finished (Diagnostics samples frames).
  if (settleBusyMs && firstDrawMs !== null) await a.page.waitFor(`!document.querySelector('[aria-busy="true"]') && !/measuring…|Loading…/.test(document.body.innerText)`, settleBusyMs);
  Object.assign(row, await a.page.eval(visibleProblemText));
  row.reads = [...new Set(a.reads.calls.slice(callsBefore).map((c) => c.method))];
  return row;
}

/**
 * Reload and wait for the NEW document's list. A marker set on the old document tells them apart: right after `Page.reload` the old page
 * is still there for a moment, and its rows would satisfy "the list has a row". The title row itself is waited for by openSessionByTitle,
 * because the list paints in steps (local #314).
 */
export async function reloadPage(page, { readyMs = 30000, attemptMs = 8000 } = {}) {
  const url = await page.eval(`location.href`);
  await page.eval(`window.__auditStale = true`);
  // A page with unsaved edits asks "leave this page?" (beforeunload) and every command after it hangs until the dialog is answered: the
  // key sweep leaves the file viewer in insert mode. Reload without waiting, then accept a dialog if one is showing.
  const reloaded = page.send('Page.reload', {}).catch(() => {});
  await new Promise((r) => setTimeout(r, 250));
  await page.send('Page.handleJavaScriptDialog', { accept: true }).catch(() => {}); // no dialog showing is an error we ignore
  await reloaded;
  const back = `!window.__auditStale && document.readyState !== 'loading' && document.querySelectorAll('#session-list li').length > 0`;
  // A reload takes about 2 s even on a saturated host, so a document that has not come back in `attemptMs` was never reloaded (the command
  // was dropped, or its renderer died): awaitApp navigates to the same address again instead of waiting out the rest (local #608, #620).
  await lib.awaitApp(page, { ready: back, url, readyMs, attemptMs, failure: `the app was not back within ${readyMs} ms of a reload` });
}

/**
 * Open each view in turn on a fresh page (a reload between views, so one view's state never leaks into the next) and measure it.
 * One row per view: `{ view, title, opened, why?, checked, failed, failures, cutOff, overflow, unnamed, doc, toast }`. A view that cannot be reached
 * (unavailable with this session open, or the palette row is gone) is a row with `opened: false` and the reason, never a crash.
 */
export async function walkViews(a, views, { out, label, fast = false, probe = null }) {
  const rows = [];
  for (const v of views) {
    const row = { view: v.id, title: v.title };
    try {
      await reloadPage(a.page);
      await sleep(fast ? 200 : 500);
      await openSessionByTitle(a.page, SESSION_TITLE);
      if (v.before) {
        const first = await openPaletteAction(a.page, v.before);
        if (!first.opened) throw new Error(`could not open ${v.before} first: ${first.why}`);
      }
      if (probe) {
        // Slow or refused reads are switched on just before the view is chosen (the session list is already up), and off again after.
        a.reads.delayMs = probe.slowMs ?? 0;
        a.reads.refuse = probe.refuse ? REFUSE_ALL : {};
        try {
          Object.assign(row, await probeViewOpen(a, v, { maxMs: probe.slowMs ? probe.slowMs * 3 + 1500 : 2000, shotPath: fast ? null : join(out, `view-${label}-${v.id}-300ms.png`), settleBusyMs: probe.refuse ? 2500 : 0 }));
        } finally { a.reads.delayMs = 0; a.reads.refuse = {}; }
        rows.push(row);
        continue;
      }
      const writesBefore = a.writes().length;
      const r = await openPaletteAction(a.page, v.title, { settleMs: fast ? 500 : 900 });
      row.opened = r.opened;
      // Opening a view makes no write. A command stubbed while it opened is a read the bridge does not list (answered null), so the
      // view above is showing the wrong thing: READ_COMMANDS needs it, and smoke-fake.mjs a fixture.
      row.stubbedOnOpen = [...new Set(a.writes().slice(writesBefore).map((w) => w.cmd))];
      if (!r.opened) row.why = r.why;
      else {
        Object.assign(row, await contrastAudit(a.page));
        row.cutOff = await a.page.eval(lib.cutOffControls);
        row.overflow = await a.page.eval(lib.overflowingText);
        row.unnamed = await a.page.eval(lib.unnamedFields);
        row.doc = await a.page.eval(lib.docScroll);
        row.toast = await a.page.eval(`document.getElementById('toast')?.checkVisibility() ? document.getElementById('toast').innerText.slice(0, 160) : ''`);
        if (!fast) await a.page.shot(join(out, `view-${label}-${v.id}.png`));
      }
    } catch (e) { row.opened = false; row.why = String(e.message ?? e).split('\n')[0]; }
    rows.push(row);
  }
  return rows;
}

/** Reads a view makes in the background whatever view is open; they are not the view's own. */
const BACKGROUND_READS = new Set(['GetConversation', 'ListSessions', 'GetModelControlStatus', 'OperatorTodoList', 'ListOperatorMessages', 'GetHealthStatus']);

/** A background read that is the view's own read in one view: Diagnostics measures GetHealthStatus, Pending messages lists ListOperatorMessages. */
const OWN_BACKGROUND = { 'admin-diagnostics': ['GetHealthStatus'], 'pending-messages': ['ListOperatorMessages'] };

/**
 * The two counts the slow and refused modes exist for (local #310): `blank` views that drew nothing within ${DRAW_BUDGET_MS} ms, and
 * `silentRefusals`, views that made a read of their own, had it refused and showed no error text (no toast, no error line).
 * Background polls do not count as the view's own read, unless the view is the one that reads them (OWN_BACKGROUND).
 */
export function summarizeProbe(rows, mode) {
  const probed = rows.filter((r) => r.opened && 'drawnWithin300' in r);
  const blank = probed.filter((r) => !r.drawnWithin300).map((r) => r.view);
  const ownReads = (r) => r.reads.filter((m) => !BACKGROUND_READS.has(m) || (OWN_BACKGROUND[r.view] ?? []).includes(m));
  const silentRefusals = mode === 'refuse' ? probed.filter((r) => ownReads(r).length > 0 && !r.toast && !r.errorLine).map((r) => r.view) : [];
  return { probed: probed.length, blank, silentRefusals };
}

/** Every main view at one window size and one theme (local #298). `fast`: shorter settles and no screenshots (the test that guards the reads). */
export async function runViews({ out, size = VIEW_SIZE, theme = null, views = null, fast = false, slowMs = null, refuse = false }) {
  const run = themeRun(theme);
  const list = views ?? (await auditViews(await loadRegistry()));
  const [width, height] = size;
  const a = await open({ width, height, theme: run.theme, scheme: run.scheme });
  try {
    await sleep(600);
    const probe = slowMs || refuse ? { slowMs, refuse } : null;
    const mode = probe ? (refuse ? 'refuse' : `slow-${slowMs}`) : null;
    const views = await walkViews(a, list, { out, label: `${run.name}-${width}x${height}${mode ? `-${mode}` : ''}`, fast, probe });
    return { width, height, theme: run.name, ...(mode ? { mode, summary: summarizeProbe(views, refuse ? 'refuse' : 'slow') } : {}), views };
  } finally { await a.close(); }
}

/** The built app's action registry (the view list is checked against it). */
async function loadRegistry() {
  const dist = new URL('../dist/', import.meta.url);
  const registry = await import(new URL('registry.js', dist));
  await import(new URL('actions/index.generated.js', dist));
  return registry;
}

/** The theme runs: the two default runs and every built-in theme (`THEMES` of the built app), or just `keys` of them. */
export async function themeRunsFromApp(keys = null) {
  const { THEMES } = await import(new URL('settings.generated.js', new URL('../dist/', import.meta.url)));
  const known = THEMES.map((t) => t.key);
  const unknown = (keys ?? []).filter((k) => !known.includes(k));
  if (unknown.length) throw new Error(`--themes names ${unknown.join(', ')}, which the app does not build in (known: ${known.join(', ')})`);
  return themeRunsFor(keys ?? known);
}

/** Text contrast under each theme and system scheme, in every main view. `runs` default to every built-in theme (themeRunsFromApp). */
export async function runThemes({ out, runs = null, views = null, sink = null }) {
  const list = views ?? (await auditViews(await loadRegistry()));
  const results = [];
  for (const run of runs ?? (await themeRunsFromApp())) {
    const a = await open({ theme: run.theme, scheme: run.scheme, width: VIEW_SIZE[0], height: VIEW_SIZE[1] });
    try {
      await sleep(600);
      const base = await a.page.eval(`({ bodyBg: getComputedStyle(document.body).backgroundColor, colorScheme: getComputedStyle(document.documentElement).colorScheme })`);
      const rows = await walkViews(a, list, { out, label: `${run.name}-${VIEW_SIZE[0]}x${VIEW_SIZE[1]}` });
      const result = { ...run, ...base, views: rows, notes: rows.filter((r) => !r.opened).map((r) => ({ step: r.view, error: r.why })) };
      results.push(result);
      sink?.(result);
    } finally { await a.close(); }
  }
  return results;
}

/** A big install: list, filter and conversation cost. */
export async function runScale({ out, sessions = 800, events = 5000 }) {
  const fx = scaleFixtures({ sessions, events });
  const a = await open({ fixtures: fx.fixtures, width: 1280, height: 800 });
  const r = { sessions, events, firstListPaintMs: a.firstPaintMs };
  const notes = [];
  try {
    await a.page.eval(lib.observeLongTasks);
    await sleep(1200);
    const drain = () => a.page.eval(lib.drainLongTasks);
    const count = (sel) => a.page.eval(`document.querySelectorAll(${JSON.stringify(sel)}).length`);
    r.listRows = await count('#session-list li');
    r.domNodes = await a.page.eval(`document.getElementsByTagName('*').length`);
    r.bootLongTasks = await drain();
    const lc0 = fx.counters.ListSessions;
    await sleep(6000);
    r.idle6s = { listSessionsCalls: fx.counters.ListSessions - lc0, longTasks: await drain() };
    await tryStep(notes, 'list scroll', async () => {
      const t0 = Date.now();
      for (let i = 0; i < 20; i++) { await a.page.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: 170, y: 500, deltaX: 0, deltaY: 800 }); await sleep(30); }
      r.listWheel20 = { ms: Date.now() - t0, longTasks: await drain() };
    });
    await tryStep(notes, 'filter', async () => {
      await clickSel(a.page, 'input[placeholder^="Filter sessions"]');
      const rowsBefore = await count('#session-list li');
      const t0 = Date.now();
      await a.page.keys('task 7');
      const typingMs = Date.now() - t0;
      // The list applies the filter after a debounce: wait for the row count to move, then to stop moving.
      const applied = await waitForRowCount(a.page, '#session-list li', { from: rowsBefore });
      r.filterTyping6 = { typingMs, rowsBefore, rowsAfter: applied.rows, settled: applied.settled, settleMs: applied.settled ? typingMs + applied.ms : null, longTasks: await drain() };
      for (let i = 0; i < 6; i++) await a.page.key('Backspace');
      await waitForRowCount(a.page, '#session-list li', { from: applied.rows, timeoutMs: 5000 });
      await drain();
    });
    await tryStep(notes, 'long conversation', async () => {
      const row = `[...document.querySelectorAll('#session-list li')].find((l) => l.dataset.id === '${fx.longSessionId}')`;
      await a.page.eval(`(${row})?.scrollIntoView({ block: 'center' })`);
      const at = await a.page.eval(`(() => { const e = ${row}; if (!e) return null; const r = e.getBoundingClientRect(); return { x: r.x + 60, y: r.y + 12 }; })()`);
      if (!at) throw new Error('the long session row is not in the DOM');
      fx.counters.GetConversation = 0;
      const t0 = Date.now();
      await mouse(a.page, at.x, at.y);
      if (!(await a.page.waitFor(`document.querySelectorAll('#events .ev').length > 50`, 60000))) throw new Error('the conversation never rendered');
      const first = Date.now() - t0;
      // Settled: the rendered event count and the poll count stop changing for 700 ms (the page may render a window of the history).
      let last = '';
      for (let still = 0, end = Date.now() + 20000; still < 7 && Date.now() < end; await sleep(100)) {
        const now = `${await count('#events .ev')}/${fx.counters.GetConversation}`;
        still = now === last ? still + 1 : 0;
        last = now;
      }
      r.openLongConversation = { firstEventsMs: first, settledMs: Date.now() - t0, evNodes: await count('#events .ev'), domNodes: await a.page.eval(`document.getElementsByTagName('*').length`), getConversationCalls: fx.counters.GetConversation, longTasks: await drain() };
      await a.page.shot(join(out, 'scale-long-conversation.png'));
      const w0 = Date.now();
      for (let i = 0; i < 30; i++) { await a.page.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: 800, y: 400, deltaX: 0, deltaY: -1500 }); await sleep(20); }
      r.conversationWheel30 = { ms: Date.now() - w0, longTasks: await drain() };
      await clickSel(a.page, '#message');
      const k0 = Date.now();
      await a.page.keys('typing latency check');
      r.composer20Keys = { ms: Date.now() - k0, longTasks: await drain() };
    });
    await tryStep(notes, 'long conversation reading position', async () => {
      // Local #306: after "Show earlier" and after new events the reader keeps the event they were looking at.
      const probe = (body) => a.page.eval(`(async () => { const st = await import('./state.js'); const cv = await import('./conversation.js'); const pane = document.getElementById('events'); const look = () => { const t = pane.getBoundingClientRect().top; for (const n of pane.querySelectorAll('.ev')) { const q = n.getBoundingClientRect(); if (q.bottom > t) return { seq: Number(n.dataset.seq), y: Math.round(q.top - t) }; } return null; }; const yOf = (seq) => { const n = pane.querySelector('.ev[data-seq="' + seq + '"]'); return n ? Math.round(n.getBoundingClientRect().top - pane.getBoundingClientRect().top) : null; }; ${body} })()`);
      await probe(`pane.scrollTop = 0; return true;`);
      await sleep(100);
      const earlier = await probe(`const b = look(); const hidden = (pane.querySelector('.more') || {}).textContent || null; const nodesBefore = pane.querySelectorAll('.ev').length; const m = pane.querySelector('.more'); if (m) m.click(); return { hidden, event: b && b.seq, yBefore: b && b.y, yAfter: b && yOf(b.seq), scrollTop: Math.round(pane.scrollTop), maxScrollTop: pane.scrollHeight - pane.clientHeight, nodesBefore, nodesAfter: pane.querySelectorAll('.ev').length };`);
      const grown = await probe(`pane.scrollTop = Math.round((pane.scrollHeight - pane.clientHeight) / 2); const b = look(); const t0 = Math.round(pane.scrollTop); const evs = st.state.events; const last = evs[evs.length - 1]; st.state.events = evs.concat([1, 2, 3].map((k) => ({ ...last, id: last.id + k, sequence: last.sequence + k, content: 'new event ' + k }))); cv.renderEvents(); const j = document.getElementById('jump-latest'); return { scrollTopBefore: t0, scrollTopAfter: Math.round(pane.scrollTop), event: b && b.seq, yBefore: b && b.y, yAfter: b && yOf(b.seq), jumpButton: j && !j.hidden ? j.textContent : null };`);
      r.longConversationScroll = { showEarlier: earlier, newEventsMidConversation: grown };
    });
    r.writes = a.writes().length;
  } finally { await a.close(); }
  return { ...r, notes };
}

function viewLine(label, v) {
  if (v.opened && 'drawnWithin300' in v) {
    const when = v.firstDrawMs === null ? 'nothing drawn' : v.drawnWithin300 ? `drawn in ${v.firstDrawMs} ms` : `blank at ${DRAW_BUDGET_MS} ms, drawn in ${v.firstDrawMs} ms`;
    const shown = [v.toast && `toast "${v.toast}"`, v.errorLine && `text "${v.errorLine}"`].filter(Boolean).join(', ');
    return `${label} ${v.view}: ${when}${shown ? `, ${shown}` : ''}${v.retry ? ' (Retry offered)' : ''} [reads: ${v.reads.join(', ') || 'none'}]`;
  }
  if (!v.opened) return `${label} ${v.view}: not opened (${v.why})`;
  const fails = v.failed ? ` (e.g. ${v.failures.slice(0, 3).map((f) => `${f.selector} ${f.fg} on ${f.bg} ${f.ratio}:1`).join('; ')})` : '';
  const stubbed = v.stubbedOnOpen?.length ? ` [stubbed on open: ${v.stubbedOnOpen.join(', ')}]` : '';
  return `${label} ${v.view}: ${v.failed}/${v.checked} texts under WCAG AA${fails}${stubbed}, cut off ${v.cutOff?.length ?? 0}, scroll ${v.doc?.sw}x${v.doc?.sh} in ${v.doc?.vw}x${v.doc?.vh}${v.toast ? `, toast "${v.toast}"` : ''}`;
}

function summarize(report) {
  const lines = [];
  for (const s of report.sizes ?? []) {
    for (const v of s.views) lines.push(`${s.width}x${s.height} ${v.view}: scroll ${v.doc.sw}x${v.doc.sh} in ${v.doc.vw}x${v.doc.vh}, cut off ${v.cutOff.length}, overflowing text ${v.overflow.length}`);
    for (const n of s.notes) lines.push(`${s.width}x${s.height} ${n.step}: skipped (${n.error})`);
  }
  for (const t of report.themes ?? []) {
    for (const v of t.views) lines.push(viewLine(t.name, v));
    for (const n of t.notes) lines.push(`${t.name} ${n.step}: skipped (${n.error})`);
  }
  if (report.views) for (const v of report.views.views) lines.push(viewLine(`${report.views.theme} ${report.views.width}x${report.views.height}`, v));
  const sum = report.views?.summary;
  if (sum) lines.push(`summary (${report.views.mode}): ${sum.probed} views, ${sum.blank.length} blank at ${DRAW_BUDGET_MS} ms${sum.blank.length ? ` (${sum.blank.join(', ')})` : ''}${report.views.mode === 'refuse' ? `, ${sum.silentRefusals.length} silent refusals${sum.silentRefusals.length ? ` (${sum.silentRefusals.join(', ')})` : ''}` : ''}`);
  if (report.scale) lines.push(`scale ${JSON.stringify(report.scale)}`);
  return lines;
}

export async function main(argv = process.argv.slice(2)) {
  const opts = parseArgs(argv);
  const out = resolve(opts.out ?? `/tmp/rsi-audit-${new Date().toISOString().replace(/[:.]/g, '-')}`);
  mkdirSync(out, { recursive: true });
  if (opts.command === 'flow') {
    const mod = await import(pathToFileURL(resolve(opts.file)).href);
    if (typeof mod.default !== 'function') throw new Error(`${opts.file} must export a default async function ({ lib, out })`);
    await mod.default({ lib, out });
    return 0;
  }
  if (opts.command === 'keys') {
    const { SKIPPED_OVERLAYS, runKeySweep, failures } = await import('./audit-keysweep.mjs');
    const results = await runKeySweep({ out, overlays: opts.overlay });
    writeFileSync(join(out, 'report.json'), `${JSON.stringify({ out, command: 'keys', at: new Date().toISOString(), skipped: SKIPPED_OVERLAYS, results }, null, 2)}\n`);
    const bad = failures(results);
    console.log(`keys: ${results.length} overlays, ${bad.length} failing; rows: ${join(out, 'keys.jsonl')}, report: ${join(out, 'report.json')}`);
    return bad.length ? 1 : 0;
  }
  const report = { out, command: opts.command, at: new Date().toISOString() };
  if (opts.command === 'sizes' || opts.command === 'all') report.sizes = await runSizes({ out });
  if (opts.command === 'themes' || opts.command === 'all') report.themes = await runThemes({ out, runs: await themeRunsFromApp(opts.themes), sink: (t) => appendFileSync(join(out, 'themes.jsonl'), `${JSON.stringify(t)}\n`) });
  if (opts.command === 'views') report.views = await runViews({ out, size: opts.size ?? VIEW_SIZE, theme: opts.theme, slowMs: opts.slow, refuse: opts.refuse });
  if (opts.command === 'scale' || opts.command === 'all') report.scale = await runScale({ out, sessions: opts.sessions, events: opts.events });
  writeFileSync(join(out, 'report.json'), `${JSON.stringify(report, null, 2)}\n`);
  for (const l of summarize(report)) console.log(l);
  console.log(`report: ${join(out, 'report.json')}`);
  return 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((c) => process.exit(c), (e) => { console.error(e.message ?? e); process.exit(e.message?.startsWith('runner_unavailable') ? 3 : 2); });
}
