// The desktop UX audit toolkit (local #277, from audit 4 / #262): one `open()` that boots the built app against the
// fake daemon with a window size, a theme, a system colour scheme and Vim keys of your choosing, and a bridge that
// stubs every write; plus the checks an audit keeps rewriting (contrast, cut-off controls, overflow, long tasks).
//
// Nothing here talks to a real rsid: reads come from the fake daemon (smoke-fake.mjs), writes are recorded and answered
// with a stub (smoke-stub-bridge.mjs) and never forwarded. See docs/ux-audit.md for how to run an audit.
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { FIXTURES, startFakeDaemon } from './smoke-fake.mjs';
import { connectPage, domSnapshot, findChrome, launchChrome, newToken, serve } from './smoke-lib.mjs';
import { daemonErrorText, rpcCall } from './smoke-rpc.mjs';
import { createStubBridge } from './smoke-stub-bridge.mjs';
import { contrastAudit } from './audit-contrast.mjs';

export { FIXTURES, contrastAudit };
export { PROJECT_ID, SESSION_IDS, scaleFixtures } from './smoke-fake.mjs';
export { DOWN_MESSAGE, createStubBridge } from './smoke-stub-bridge.mjs';
export { daemonErrorText };

export const DIST = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');
export const THEME_STORAGE_KEY = 'rsi.desktop.theme.v1';
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * The localStorage entries `open()` seeds before the app starts. Pure, so a test can check it without a browser.
 * `theme` is a theme key (`'light'`, `'high-contrast'`, …) or `{ theme, overrides }`; null keeps the desktop default.
 * `vim: false` is the new-install default (Vim keys off); the first-run Vim question is marked answered either way.
 */
export function seedEntries({ vim = false, theme = null, seedStorage = {} } = {}) {
  const choice = theme === null ? null : typeof theme === 'string' ? { theme, overrides: {} } : { theme: theme.theme ?? null, overrides: theme.overrides ?? {} };
  return {
    'rsi.desktop.vimKeysAsked': 'answered',
    'rsi.desktop.vimKeys': vim ? 'on' : 'off',
    ...(choice ? { [THEME_STORAGE_KEY]: JSON.stringify(choice) } : {}),
    ...seedStorage,
  };
}

/** The `Emulation.setEmulatedMedia` features for a system colour scheme and any others (`[{ name, value }]`). */
export function mediaFeatures({ scheme = null, media = [] } = {}) {
  if (scheme !== null && scheme !== 'light' && scheme !== 'dark') throw new Error(`scheme must be 'light', 'dark' or null, not ${JSON.stringify(scheme)}`);
  return [...(scheme ? [{ name: 'prefers-color-scheme', value: scheme }] : []), ...media];
}

/**
 * The knobs on daemon READS, changeable while the page runs (local #313). Returns `{ delayMs, refuse, calls, wrap(call) }`: `delayMs` and
 * `refuse` are plain objects you assign to (`a.reads.refuse.ListMcpServers = 'busy'`, `a.reads.delayMs = 3000`); `wrap(call)` is the bridge's
 * read hook with them applied, and `calls` records each read as `{ method, at, delayedMs, refused }` (`at`: ms since the page opened).
 *   delayMs  a number (every read waits that long before it is forwarded) or `{ [method]: ms, '*': ms }`
 *   refuse   `{ [method]: message | { message, code?, rpcCode?, nextAction? }, '*': … }`: the read fails the way the real backend reports a
 *            daemon refusal (src-tauri daemon.rs): `daemon error -32000: <message> [<code>]`. A refused read is delayed too.
 */
export function createReadControls({ delayMs = 0, refuse = {} } = {}) {
  const t0 = Date.now();
  const ctl = { delayMs, refuse, calls: [] };
  const delayFor = (method) => {
    const d = ctl.delayMs;
    if (typeof d === 'number') return d;
    if (d && typeof d === 'object') return Object.hasOwn(d, method) ? Number(d[method]) : Object.hasOwn(d, '*') ? Number(d['*']) : 0;
    return 0;
  };
  const refusalFor = (method) => {
    const r = ctl.refuse;
    if (!r || typeof r !== 'object') return null;
    return Object.hasOwn(r, method) ? r[method] : Object.hasOwn(r, '*') ? r['*'] : null;
  };
  ctl.wrap = (call) => async (method, params) => {
    const wait = delayFor(method);
    const refusal = refusalFor(method);
    ctl.calls.push({ method, at: Date.now() - t0, delayedMs: wait, refused: refusal !== null && refusal !== undefined });
    if (wait > 0) await sleep(wait);
    if (refusal !== null && refusal !== undefined) throw new Error(refusalText(refusal));
    return call(method, params);
  };
  return ctl;
}

/** The error text of a refused read: the real backend's `daemon error <code>: <message>[ | next_action: …][ [<code>]]`. */
export function refusalText(refusal) {
  const r = typeof refusal === 'string' ? { message: refusal } : refusal;
  return daemonErrorText({ code: r.rpcCode ?? -32000, message: String(r.message ?? 'refused'), data: { ...(r.code ? { code: r.code } : {}), ...(r.nextAction ? { next_action: r.nextAction } : {}) } });
}

const seedSource = (entries) => `try{${Object.entries(entries).map(([k, v]) => `localStorage.setItem(${JSON.stringify(k)},${JSON.stringify(String(v))});`).join('')}}catch{}`;

/**
 * Wait until `ready` holds on `url`'s document, loading it again when it does not come (local #620, hub #3659). A launch or a reload that
 * takes minutes on a saturated host is not slowness: a reload measured 1.3-2.6 s, so a document absent after `attemptMs` was never loaded (the
 * navigation was dropped, or its renderer stopped answering). Each miss therefore navigates to the same address again, inside the same
 * `readyMs` budget (not raised), and each wait bounds its evaluations by `evalMs`, so a renderer that stops answering cannot hold one
 * attempt for the whole command timeout. On timeout the error carries what attributes it: the navigations made, the page's own state, the
 * DevTools crash/failed-load/log events and `extra()` (Chrome's exit state and stderr tail, the bridge log).
 */
export async function awaitApp(page, { ready, url, failure, readyMs = 30000, attemptMs = 8000, evalMs = 3000, extra = () => ({}) }) {
  const deadline = Date.now() + readyMs;
  let navigations = 0;
  let lastNavigation = null;
  while (Date.now() < deadline) {
    if (await page.waitFor(ready, Math.min(attemptMs, deadline - Date.now()), { evalMs })) return { navigations };
    if (Date.now() >= deadline) break;
    navigations += 1;
    lastNavigation = await page.send('Page.navigate', { url }, 5000).then((r) => r?.errorText ?? 'accepted', (e) => e.message);
  }
  const state = JSON.stringify({ navigations, lastNavigation, page: await domSnapshot(page), ...extra() }).slice(0, 3000);
  throw new Error(`${failure}; state: ${state}`);
}

/**
 * Boot the app. Options:
 *   width, height   the window (CSS px; `Emulation.setDeviceMetricsOverride`), default 1280x800; `scale` is the device scale
 *   scheme          'light' | 'dark' | null: the emulated system `prefers-color-scheme`; `media` adds other features
 *   theme           the app's saved theme (see seedEntries); `vim` Vim keys on (default off); `seedStorage` extra localStorage
 *   fixtures        fake-daemon read results (default FIXTURES; `scaleFixtures().fixtures` for many sessions)
 *   reads           `{ delayMs, refuse }` for daemon reads, live on the returned `reads` (see createReadControls)
 *   stubs           write command -> answer or `(args) => answer`; `local` overrides a shim-local command
 *   ready           a page expression to wait for (default: the session list has a row); `readyMs` how long
 *   dist, chrome    override the built dist directory or the Chrome binary
 * Returns { page, bridge, reads, writes, log, fake, port, firstPaintMs, resize, close }. Always `await close()`.
 */
export async function open({
  width = 1280, height = 800, scale = 1, scheme = null, media = [], theme = null, vim = false, seedStorage = {},
  fixtures = FIXTURES, reads: readOptions = {}, stubs = {}, local = {}, ready = `document.querySelectorAll('#session-list li').length > 0`, readyMs = 30000,
  dist = DIST, chrome: chromeBin = findChrome(),
} = {}) {
  if (!existsSync(join(dist, 'index.html'))) throw new Error(`${dist} has no index.html: run \`npm run build\` in desktop/ first`);
  if (!chromeBin) throw new Error('runner_unavailable: no Chrome/Chromium found (set RSI_SMOKE_CHROME)');
  const features = mediaFeatures({ scheme, media });
  const fake = await startFakeDaemon(fixtures);
  const reads = createReadControls(readOptions);
  const bridge = createStubBridge({ call: reads.wrap((m, p) => rpcCall(fake.socket, m, p)), stubs, local });
  const token = newToken();
  let server, chrome, page;
  const close = async () => {
    page?.close();
    server?.closeAllConnections();
    server?.close();
    try { await chrome?.stop(); } finally { await fake.stop(); }
  };
  try {
    ({ server } = await serve({ dist, bridge, token, onRefusal: (e) => bridge.log.push(e) }));
    chrome = await launchChrome(chromeBin, { width, height });
    page = await connectPage(chrome.cdpPort, { commandTimeout: 30000, browser: chrome });
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: seedSource(seedEntries({ vim, theme, seedStorage })) });
    await page.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: scale, mobile: false });
    if (features.length) await page.send('Emulation.setEmulatedMedia', { features });
    const started = Date.now();
    const url = `http://127.0.0.1:${server.address().port}/`;
    await page.send('Page.navigate', { url });
    await awaitApp(page, {
      ready: `document.readyState !== 'loading' && (${ready})`, url, readyMs, failure: `the app was not ready within ${readyMs} ms: ${ready}`,
      extra: () => ({ chrome: chrome.diagnostics(), bridgeLog: bridge.log.slice(-5) }),
    });
    const firstPaintMs = Date.now() - started;
    return {
      page, bridge, reads, fake, firstPaintMs, port: server.address().port, close,
      writes: () => bridge.writes(), log: bridge.log,
      resize: (w, h) => page.send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: scale, mobile: false }),
    };
  } catch (e) {
    await close().catch(() => {});
    throw e;
  }
}

// ---------- mouse helpers ----------

export async function mouse(page, x, y) {
  for (const type of ['mousePressed', 'mouseReleased']) await page.send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1 });
}

/** Click the centre of the first element matching `selector` (scrolled into view). Throws when none is visible. */
export async function clickSel(page, selector, settleMs = 400) {
  const at = await page.eval(`(() => { const e = document.querySelector(${JSON.stringify(selector)}); if (!e) return null; e.scrollIntoView({ block: 'center' }); const r = e.getBoundingClientRect(); return r.width ? { x: r.x + r.width / 2, y: r.y + r.height / 2 } : null; })()`);
  if (!at) throw new Error(`no visible ${selector}`);
  await mouse(page, at.x, at.y);
  await sleep(settleMs);
}

/** Click a visible control by its exact text or aria-label. Throws when none matches. */
export async function clickText(page, text, { scope = 'button, a, summary, [role=button], [role=tab], [role=menuitem], li', settleMs = 500 } = {}) {
  const at = await page.eval(`(() => {
    const t = ${JSON.stringify(text)};
    const e = [...document.querySelectorAll(${JSON.stringify(scope)})].find((n) => n.checkVisibility() && ((n.innerText || n.textContent || '').trim() === t || n.getAttribute('aria-label') === t));
    if (!e) return null;
    e.scrollIntoView({ block: 'center' });
    const r = e.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  })()`);
  if (!at) throw new Error(`no visible control "${text}"`);
  await mouse(page, at.x, at.y);
  await sleep(settleMs);
}

/**
 * Open a session by clicking its row (matched by a title substring). The row is waited for (up to `waitMs`), never read once: the
 * list paints in steps (the first `li` can be a group header), so "the list has a row" does not mean this row is drawn yet, and
 * under CPU load the rest follows a few hundred ms later (local #314). A missing row is an error naming what the list did show.
 */
export async function openSessionByTitle(page, title, { waitMs = 15000 } = {}) {
  const find = `(() => { const e = [...document.querySelectorAll('#session-list li')].find((l) => l.innerText.includes(${JSON.stringify(title)})); if (!e) return null; e.scrollIntoView({ block: 'center' }); const r = e.getBoundingClientRect(); return r.width ? { x: r.x + 60, y: r.y + 12 } : null; })()`;
  await page.waitFor(find, waitMs);
  const at = await page.eval(find);
  if (!at) {
    const shown = await page.eval(`JSON.stringify({ rows: [...document.querySelectorAll('#session-list li')].map((l) => l.innerText.split('\\n')[0]).slice(0, 12), filter: document.querySelector('input[placeholder^="Filter sessions"]')?.value ?? null })`);
    throw new Error(`no session row containing "${title}" within ${waitMs} ms (the list showed ${shown})`);
  }
  await mouse(page, at.x, at.y);
  await sleep(1000);
}

// ---------- page expressions ----------

/** Names of every visible button, link, summary and role=button (innerText, aria-label or title). */
export const visibleButtons = `[...document.querySelectorAll('button, [role=button], summary, a[href]')].filter((e) => e.checkVisibility()).map((e) => (e.getAttribute('aria-label') || e.innerText || e.title || '').trim().replace(/\\s+/g, ' ')).filter(Boolean)`;

/** The document's scroll size against the viewport: a horizontal scrollbar at a small size shows as `sw > vw`. */
export const docScroll = `({ sw: document.documentElement.scrollWidth, sh: document.documentElement.scrollHeight, vw: innerWidth, vh: innerHeight })`;

/**
 * Controls cut off at the current window size: a visible button, input, select, textarea or tab outside the viewport
 * (right, left or bottom edge), or clipped by an `overflow: hidden|clip` ancestor. Resolves to a list of strings.
 */
export const cutOffControls = `(() => {
  const out = [];
  const name = (e) => (e.innerText || e.placeholder || e.getAttribute('aria-label') || e.id || e.tagName).trim().slice(0, 30);
  for (const e of document.querySelectorAll('button, input, select, textarea, [role=tab]')) {
    if (!e.checkVisibility()) continue;
    const r = e.getBoundingClientRect();
    if (r.width === 0) continue;
    if (r.right > innerWidth + 1 || r.left < -1 || r.bottom > innerHeight + 1) out.push(name(e) + ' [' + Math.round(r.left) + '..' + Math.round(r.right) + ' x ' + Math.round(r.top) + '..' + Math.round(r.bottom) + ']');
    for (let p = e.parentElement; p && p !== document.body; p = p.parentElement) {
      const cs = getComputedStyle(p);
      if (/(hidden|clip)/.test(cs.overflowX + cs.overflowY)) {
        const pr = p.getBoundingClientRect();
        if (r.right > pr.right + 1 || r.left < pr.left - 1) { out.push('CLIPPED-BY-' + (p.id || p.className) + ': ' + name(e)); break; }
      }
    }
  }
  return [...new Set(out)];
})()`;

/**
 * Fields and log regions with no accessible name of their own (local #326): a visible input, textarea, select, contenteditable or
 * `role=log` whose name (aria-labelledby, aria-label, or a wrapping or `for` <label>) is empty or only repeats its placeholder. A
 * placeholder is a hint that is gone once text is typed, so VoiceOver then reads the field as bare "edit text". Hidden, button, submit and
 * file inputs are skipped (a button is named by its text).
 * Resolves to a list of `tag#id.class "placeholder"` strings.
 */
export const unnamedFields = `(() => {
  const out = [];
  const text = (n) => (n.textContent || '').replace(/\\s+/g, ' ').trim();
  const labelled = (e) => (e.getAttribute('aria-labelledby') || '').split(/\\s+/).map((id) => document.getElementById(id)).filter(Boolean).map(text).join(' ').trim();
  const labels = (e) => {
    let t = '';
    for (const l of e.labels || []) { const c = l.cloneNode(true); c.querySelectorAll('input, textarea, select').forEach((x) => x.remove()); t += ' ' + text(c); }
    return t.trim();
  };
  const sel = 'input:not([type=hidden]):not([type=button]):not([type=submit]):not([type=file]), textarea, select, [contenteditable=""], [contenteditable="true"], [role=log], [role=textbox], [role=combobox], [role=searchbox]';
  for (const e of document.querySelectorAll(sel)) {
    if (!e.checkVisibility()) continue;
    const name = labelled(e) || (e.getAttribute('aria-label') || '').trim() || labels(e);
    const hint = (e.getAttribute('placeholder') || '').trim();
    if (name && name !== hint) continue;
    out.push(e.tagName.toLowerCase() + (e.id ? '#' + e.id : '') + (typeof e.className === 'string' && e.className ? '.' + e.className.split(/\\s+/)[0] : '') + (hint ? ' "' + hint.slice(0, 40) + '"' : ''));
  }
  return [...new Set(out)];
})()`;

/** Visible leaf text wider than its box while its overflow is `visible` (text running out of its container). */
export const overflowingText = `(() => {
  const out = [];
  for (const e of document.querySelectorAll('body *')) {
    if (!e.checkVisibility() || e.children.length > 0) continue;
    const r = e.getBoundingClientRect();
    if (r.width === 0 || !(e.innerText || '').trim()) continue;
    if (e.scrollWidth > e.clientWidth + 2 && getComputedStyle(e).overflowX === 'visible') out.push({ tag: e.tagName, id: e.id, cls: String(e.className).slice(0, 40), text: e.innerText.slice(0, 60), scrollWidth: e.scrollWidth, clientWidth: e.clientWidth });
  }
  return out.slice(0, 40);
})()`;

/** Start recording long tasks (> 50 ms). `drainLongTasks` returns and clears their durations. */
export const observeLongTasks = `(() => { window.__lt = []; new PerformanceObserver((l) => { for (const e of l.getEntries()) window.__lt.push(Math.round(e.duration)); }).observe({ type: 'longtask', buffered: true }); return true; })()`;
export const drainLongTasks = `window.__lt ? window.__lt.splice(0) : []`;

// ---------- waiting for the page ----------

/**
 * Wait until the number of elements matching `selector` differs from `from` and then stays the same for `stableMs`
 * (a debounced filter has applied, and settled). Polls every `stepMs` and measures from the call. Resolves to
 * `{ settled, ms, rows }`: `settled` false means the count never changed from `from` within `timeoutMs`, so `rows` is still
 * what it was and the audit must not call that a result.
 */
export async function waitForRowCount(page, selector, { from, timeoutMs = 10000, stableMs = 150, stepMs = 25 } = {}) {
  if (!Number.isInteger(from)) throw new Error('waitForRowCount needs the row count to move away from (from)');
  const read = () => page.eval(`document.querySelectorAll(${JSON.stringify(selector)}).length`);
  const t0 = Date.now();
  let changedAt = null;
  let rows = await read();
  for (let last = rows; Date.now() - t0 < timeoutMs; await sleep(stepMs)) {
    rows = await read();
    if (rows !== last) { changedAt = null; last = rows; }
    if (rows !== from) {
      changedAt ??= Date.now();
      if (Date.now() - changedAt >= stableMs) return { settled: true, ms: changedAt - t0, rows };
    }
  }
  return { settled: false, ms: Date.now() - t0, rows };
}

/**
 * Open a palette action by its exact title with real mouse events, the way a person does: open Actions…, type the title,
 * click the row whose title is exactly it. Resolves to `{ opened: true }`, or `{ opened: false, why }` when the row is
 * missing or the action is unavailable here (the palette says why on the row); the palette is closed again in that case.
 */
export async function openPaletteAction(page, title, { settleMs = 900 } = {}) {
  await clickSel(page, '#open-palette', 300);
  await page.keys(title.replace(/…$/, ''));
  await sleep(300);
  const row = await page.eval(`(() => {
    const want = ${JSON.stringify(title)};
    const li = [...document.querySelectorAll('#palette-list .palette-item')].find((n) => (n.querySelector('.palette-title')?.textContent ?? '').trim() === want);
    if (!li) return { found: false, top: document.querySelector('#palette-list .palette-item')?.innerText.replace(/\\s+/g, ' ').slice(0, 80) ?? null };
    li.scrollIntoView({ block: 'nearest' });
    const r = li.getBoundingClientRect();
    return { found: true, disabled: li.getAttribute('aria-disabled') === 'true', why: li.title, x: r.x + r.width / 2, y: r.y + r.height / 2 };
  })()`);
  if (!row.found || row.disabled) {
    await page.key('Escape');
    await sleep(200);
    return { opened: false, why: row.found ? `unavailable here: ${row.why}` : `no palette row titled "${title}" (top row: ${row.top})` };
  }
  await mouse(page, row.x, row.y);
  await sleep(settleMs);
  return { opened: true };
}
