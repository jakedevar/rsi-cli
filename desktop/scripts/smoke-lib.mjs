// Helpers for desktop/scripts/smoke.mjs: static server for dist, headless Chrome, a tiny CDP client.
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { delimiter, extname, join, normalize } from 'node:path';
import { authorizeInvoke, createBridge, shimSource } from './smoke-bridge.mjs';

/** Marks a fresh profile as an existing install, so the first-run Vim keys question (local #208) stays away. */
export const EXISTING_INSTALL_SEED = "try { if (localStorage.getItem('rsi.desktop.vimKeysAsked') === null) localStorage.setItem('rsi.desktop.vimKeysAsked', 'answered'); } catch {}";

const MIME = { '.mjs': 'text/javascript', '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.map': 'application/json', '.svg': 'image/svg+xml', '.woff2': 'font/woff2' };

/** Find a Chrome/Chromium binary, or null (the caller reports `runner_unavailable`). */
export function findChrome(env = process.env, exists = existsSync) {
  const candidates = [
    env.RSI_SMOKE_CHROME,
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
    ...['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser'].flatMap((n) =>
      (env.PATH ?? '').split(delimiter).filter(Boolean).map((d) => join(d, n))),
  ].filter(Boolean);
  return candidates.find((c) => exists(c)) ?? null;
}

/** How long a finished call id keeps its answer for a re-sent request. */
const CALL_MEMORY_MS = 60_000;

/**
 * Serve `dist` on 127.0.0.1 with the shim injected into index.html and POST /__invoke wired to
 * the bridge. Requests must carry the per-run token (the page does; other local processes don't).
 */
export function serve({ dist, bridge, token, onRefusal, signal }) {
  signal?.throwIfAborted();
  const calls = new Map();
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    if (url.pathname === '/__invoke') {
      const gate = authorizeInvoke(req, { token, origin: `http://127.0.0.1:${server.address().port}` });
      if (!gate.ok) { onRefusal?.({ cmd: '(request)', outcome: 'refused', reason: gate.reason }); res.writeHead(403).end('forbidden'); return; }
      let body = '';
      req.on('data', (d) => { body += d; if (body.length > 1_000_000) req.destroy(); });
      req.on('end', async () => {
        // A call id runs once; the page re-sends it when Chrome cancels the request before an answer arrives (shimSource).
        const callId = req.headers['x-smoke-call'];
        const known = typeof callId === 'string' && callId.length <= 64 ? calls : null;
        let answer = known?.get(callId);
        if (!answer) {
          answer = (async () => {
            try {
              const { cmd, args } = JSON.parse(body);
              return { result: (await bridge.invoke(String(cmd), args)) ?? null };
            } catch (e) { return { error: e instanceof Error ? e.message : String(e) }; }
          })();
          if (known) {
            known.set(callId, answer);
            setTimeout(() => known.delete(callId), CALL_MEMORY_MS).unref();
          }
        }
        res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(await answer));
      });
      return;
    }
    if (url.pathname === '/__shim.js') { res.writeHead(200, { 'content-type': 'text/javascript' }).end(shimSource(token)); return; }
    let rel = url.pathname === '/' ? '/index.html' : decodeURIComponent(url.pathname);
    const file = normalize(join(dist, rel));
    if (!file.startsWith(dist) || !existsSync(file)) { res.writeHead(404).end('not found'); return; }
    let data = readFileSync(file);
    if (rel === '/index.html') data = Buffer.from(data.toString('utf8').replace('<head>', '<head><script src="/__shim.js"></script>'));
    res.writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream' }).end(data);
  });
  const close = () => { server.closeAllConnections(); server.close(); };
  signal?.addEventListener('abort', close, { once: true });
  server.once('close', () => signal?.removeEventListener('abort', close));
  return new Promise((resolve, reject) => {
    server.once('error', (error) => { close(); reject(error); });
    server.listen(0, '127.0.0.1', () => {
      if (signal?.aborted) { close(); reject(signal.reason); }
      else resolve({ server, port: server.address().port });
    });
  });
}

export const newToken = () => randomBytes(16).toString('hex');

export const BOOT_ATTEMPTS = 3;

/** Chrome can interrupt loopback loads even though the server and origin never changed (#664, #670). */
export async function bootWithRetries(page, url, ready, { attempts = BOOT_ATTEMPTS } = {}) {
  const tried = [];
  for (let attempt = 1; attempt <= attempts; attempt++) {
    const from = page.events.length;
    const interrupted = () => page.events.slice(from).filter((e) => e.method === 'Network.loadingFailed'
      && (e.params.canceled || e.params.errorText === 'net::ERR_NETWORK_CHANGED'));
    const stop = () => {
      const module = interrupted().find((e) => e.params.type === 'Script' || /\.m?js(?:[?#]|$)/.test(String(e.params.url)));
      return module && `Chrome interrupted the load of ${module.params.url}: ${module.params.errorText}`;
    };
    try {
      await page.goto(url);
      return await ready(stop);
    } catch (error) {
      const loads = interrupted();
      tried.push({ attempt, error: error.message.slice(0, 300),
        canceled: loads.filter((e) => e.params.canceled).map((e) => String(e.params.url ?? '?').split('/').pop()),
        network: loads.map((e) => e.params),
      });
      // A timeout or app error alone is not evidence of a transient transport failure.
      // Never replay interactions: only the caller's initial navigation/readiness belongs here.
      if (!loads.length || page.closed || page.signal?.aborted || attempt === attempts) {
        throw Object.assign(new Error(error.message, { cause: error }), { bootAttempts: tried });
      }
    }
  }
}

/** Poll a condition or usable element; run element reads/actions in the same CDP evaluation. */
export async function waitForReady(page, expression, { step = 'page readiness', timeout = 5000, element = false, action = '(e) => true', stop } = {}) {
  const deadline = Date.now() + timeout;
  let excerpt = 'no page response';
  let lastError;
  do {
    const remaining = deadline - Date.now();
    if (remaining <= 0) break;
    // `stop` names a cause that makes waiting pointless (a module load Chrome cancelled); give up on it at once.
    const stopped = stop?.();
    if (stopped) throw new Error(`${step}: stopped after ${timeout - remaining}ms: ${stopped}; state: ${excerpt}`);
    try {
      const result = await page.eval(`(() => {
        const e = (${expression});
        const r = ${element} && e ? e.getBoundingClientRect() : null;
        const usable = ${element} ? !!(e && r.width > 0 && r.height > 0
          && e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })
          && !e.matches(':disabled') && !e.closest('[inert], [hidden], [aria-disabled="true"]')) : !!e;
        if (usable) return { ready: true, value: (${action})(e) };
        return { ready: false, excerpt: JSON.stringify({
          url: location.href, boot: document.documentElement.dataset.boot,
          onboarding: document.querySelector('#empty')?.dataset.onboarding,
          target: ${element} ? e?.outerHTML?.slice(0, 500) : undefined,
          dom: document.body?.innerText?.slice(0, 1200),
        }) };
      })()`, { timeout: remaining });
      if (result.ready) return result.value;
      excerpt = result.excerpt;
      lastError = undefined;
    } catch (error) {
      lastError = error;
      // Cancellation/transport failures cannot become ready by polling.
      if (page.signal?.aborted) throw page.signal.reason;
      if (page.closed) throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, Math.min(100, Math.max(0, deadline - Date.now()))));
  } while (Date.now() < deadline);
  throw new Error(`${step}: timed out after ${timeout}ms waiting for ${expression}; state: ${excerpt}${lastError ? `; last error: ${lastError.message}` : ''}`);
}

/**
 * What the page's DevTools events say about its renderer and loads (local #620): a crashed or detached target, the failed requests and
 * the last browser log lines. A readiness timeout that cannot say whether the renderer died or the app never drew is not attributable.
 */
export function transportReport(page) {
  const events = page.events ?? [];
  const named = (...methods) => events.filter((e) => methods.includes(e.method));
  const report = {};
  const crashes = named('Inspector.targetCrashed', 'Inspector.detached', 'Target.targetCrashed').map((e) => e.method + (e.params?.reason ? `: ${e.params.reason}` : ''));
  if (crashes.length) report.renderer = crashes.slice(-3);
  const failed = named('Network.loadingFailed').slice(-5).map((e) => `${e.params.errorText} ${e.params.url}`);
  if (failed.length) report.failedRequests = failed;
  const logs = named('Log.entryAdded').slice(-5).map((e) => `${e.params.entry?.level}: ${String(e.params.entry?.text ?? '').slice(0, 200)}`);
  if (logs.length) report.log = logs;
  return report;
}

/**
 * What a failed step needs to be diagnosed after the fact (local #285): the visible state of the list and the open
 * session, the app's own boot report, the show-finished preference, any open dialog and the last uncaught page errors.
 * Bounded, and it never throws: a page that cannot answer reports why.
 */
export async function domSnapshot(page) {
  try {
    const snap = await page.eval(`(() => {
      const cut = (t, n) => String(t ?? '').replace(/\\s+/g, ' ').trim().slice(0, n);
      return {
        url: location.href, readyState: document.readyState, boot: document.documentElement.dataset.boot ?? null,
        showFinished: { checked: document.getElementById('show-finished')?.checked ?? null, stored: (() => { try { return localStorage.getItem('rsi.desktop.showFinished'); } catch { return 'unreadable'; } })() },
        search: document.getElementById('search')?.value ?? null,
        selectedId: document.querySelector('#session-list .session-item.selected')?.dataset.id ?? null,
        rows: [...document.querySelectorAll('#session-list li')].slice(0, 60).map((e) => ({ id: e.dataset.id ?? null, cls: e.className, text: cut(e.textContent, 80) })),
        sessionView: { hidden: document.getElementById('session-view')?.hidden ?? null, title: cut(document.getElementById('s-title')?.textContent, 120) },
        dialogs: [...document.querySelectorAll('dialog[open]')].map((d) => ({ id: d.id, text: cut(d.textContent, 200) })),
        active: document.activeElement ? document.activeElement.tagName + '#' + document.activeElement.id : null,
        text: cut(document.body?.innerText, 1500),
      };
    })()`, { timeout: 3000 });
    const errors = (page.events ?? []).filter((e) => e.method === 'Runtime.exceptionThrown').slice(-5)
      .map((e) => (e.params.exceptionDetails.exception?.description ?? e.params.exceptionDetails.text ?? '').split('\n')[0]);
    const retries = await page.eval('(globalThis.__smokeInvokeRetries ?? []).slice(-5)', { timeout: 1000 }).catch(() => []);
    return { ...snap, pageErrors: errors, ...(retries.length ? { invokeRetries: retries } : {}), ...transportReport(page) };
  } catch (error) {
    return { unavailable: String(error?.message ?? error).slice(0, 300) };
  }
}

/** Launch headless Chrome with a throwaway profile; resolves with { cdpPort, pid, stop }. */
export async function launchChrome(bin, { width = 1400, height = 900, signal, launchTimeout = 30000 } = {}) {
  signal?.throwIfAborted();
  // Chrome puts SingletonSocket under TMPDIR, independently of --user-data-dir.
  // A daemon job's TMPDIR makes that path exceed sockaddr_un.sun_path (108 bytes
  // on Linux), which Chrome CHECKs before bind(). Keep only this child's temp
  // state short; do not change the test runner's job-owned TMPDIR or sandbox flags.
  const scratch = mkdtempSync(join(process.platform === 'win32' ? tmpdir() : '/tmp', 'rsi-chrome-'));
  const profile = join(scratch, 'profile');
  // A bounded desktop lane owns the outer group, including Chrome's renderers.
  const ownGroup = process.platform !== 'win32' && process.env.DESKTOP_LANE_PROCESS_GROUP !== '1';
  const child = spawn(bin, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--no-first-run',
    '--no-default-browser-check', '--disable-gpu', '--hide-scrollbars', `--window-size=${width},${height}`, 'about:blank',
  ], { detached: ownGroup, stdio: ['ignore', 'ignore', 'pipe'], env: { ...process.env, TMPDIR: scratch, TMP: scratch, TEMP: scratch } });
  let stderr = '';
  let spawnError;
  let exited = false;
  const started = Date.now();
  let stopRequested = false;
  const done = new Promise((resolve) => {
    // Settle on 'exit', never on 'close' (#138). 'close' also waits for every holder of the stderr pipe, and a
    // Chrome helper that left the process group (macOS keeps one for tens of seconds after the browser itself
    // is gone, longer under load) holds it open: stop() then hangs past the test's own timeout. The pipe is
    // released at exit; the profile directory is removed with retries below.
    child.once('exit', () => { exited = true; child.stderr.destroy(); resolve(); });
    child.once('error', (error) => { spawnError = error; exited = true; resolve(); });
  });
  child.stderr.on('data', (data) => { stderr = (stderr + data).slice(-4096); });
  // Outside the lane, a private group reaches Chrome's renderers too. Inside
  // the lane, normal shutdown stops Chrome; the lane owns group-wide cleanup.
  const kill = (sig) => {
    if (!ownGroup) { if (!exited) child.kill(sig); }
    else if (child.pid) {
      // ESRCH: the group is gone. EPERM: macOS refuses a signal to a group whose only members are
      // exited helpers not yet reaped (#138); either way there is nothing left to signal.
      try { process.kill(-child.pid, sig); } catch (error) { if (error.code !== 'ESRCH' && error.code !== 'EPERM') throw error; }
    }
  };
  let stopping;
  const stop = () => stopping ??= (async () => {
    stopRequested = true;
    signal?.removeEventListener('abort', onAbort);
    kill('SIGTERM');
    const force = setTimeout(() => kill('SIGKILL'), 1000);
    try { await done; } finally { clearTimeout(force); kill('SIGKILL'); }
    await rm(scratch, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  })();
  const onAbort = () => { void stop().catch(() => {}); };
  signal?.addEventListener('abort', onAbort, { once: true });
  // Snapshot before stop() sends signals: a silent, slow browser and a crashed
  // browser need different diagnoses. Keep stderr's absence explicit too.
  const diagnostics = () => {
    const state = spawnError ? `spawn error: ${spawnError.message}`
      : child.signalCode ? `signal ${child.signalCode}`
        : child.exitCode !== null ? `exit code ${child.exitCode}`
          : `still running (pid ${child.pid})`;
    return `${state}; stderr: ${stderr.trim() || '(empty)'}; browser pid=${child.pid}, ageMs=${Date.now() - started}, ownGroup=${ownGroup}, stopRequested=${stopRequested}`;
  };
  try {
    // Concurrent Cargo builds can delay startup past five seconds. Allow one
    // bounded 30 s launch within the desktop lane's 10 minute npm budget;
    // the caller's abort signal still enforces its own test/group deadline.
    const deadline = Date.now() + launchTimeout;
    while (Date.now() < deadline) {
      signal?.throwIfAborted();
      if (exited) throw new Error(`Chrome exited before opening DevTools (${diagnostics()})`);
      const f = join(profile, 'DevToolsActivePort');
      if (existsSync(f)) {
        const port = Number(readFileSync(f, 'utf8').split('\n')[0]);
        if (port > 0) return { cdpPort: port, pid: child.pid, stop, diagnostics };
      }
      await new Promise((r) => setTimeout(r, 50));
    }
    throw new Error(`Chrome did not open a DevTools port within ${launchTimeout}ms: ${diagnostics()}`);
  } catch (error) {
    await stop();
    throw error;
  }
}

const evalError = (details) => new Error(details.exception?.description ?? details.text);

/** The by-value result of a handle-returning `Runtime.evaluate`: a promise is awaited through its handle, an object is copied. */
async function evalValue(send, r, timeout, objectGroup) {
  if (r.exceptionDetails) throw evalError(r.exceptionDetails);
  const { result } = r;
  if (!result.objectId) return result.value;
  if (result.subtype === 'promise') {
    const a = await send('Runtime.awaitPromise', { promiseObjectId: result.objectId, returnByValue: true }, timeout);
    if (a.exceptionDetails) throw evalError(a.exceptionDetails);
    return a.result.value;
  }
  const c = await send('Runtime.callFunctionOn', { objectId: result.objectId, functionDeclaration: 'function () { return this; }', returnByValue: true, objectGroup }, timeout);
  if (c.exceptionDetails) throw evalError(c.exceptionDetails);
  return c.result.value;
}

/** Minimal CDP client over Node's built-in WebSocket. */
export async function connectPage(cdpPort, { signal, commandTimeout = 8000, browser } = {}) {
  signal?.throwIfAborted();
  const targets = await (await fetch(`http://127.0.0.1:${cdpPort}/json/list`, { signal: AbortSignal.any([AbortSignal.timeout(commandTimeout), ...(signal ? [signal] : [])]) })).json();
  signal?.throwIfAborted();
  const page = targets.find((t) => t.type === 'page');
  if (!page) throw new Error('no page target');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  const pending = new Map();
  let closed = false;
  let closeError;
  const evidence = () => JSON.stringify({
    target: page.id, cdpPort, node: process.version,
    browser: browser?.diagnostics(), transport: transportReport({ events }),
  });
  const lost = (message, cause) => new Error(`${message}; ${evidence()}`, { cause });
  const close = (error = new Error('CDP socket closed')) => {
    if (closed) return;
    closed = true;
    closeError = error;
    signal?.removeEventListener('abort', onAbort);
    for (const { rej, timer, method } of pending.values()) {
      clearTimeout(timer);
      rej(signal?.aborted ? signal.reason : new Error(`${error.message}; pending command: ${method}`, { cause: error }));
    }
    pending.clear();
    ws.close();
  };
  const onAbort = () => close(signal.reason);
  signal?.addEventListener('abort', onAbort, { once: true });
  const events = [];
  try {
    await new Promise((res, rej) => {
      const timer = setTimeout(() => finish(new Error('CDP socket open timed out')), commandTimeout);
      const aborted = () => finish(signal.reason);
      const finish = (error) => {
        clearTimeout(timer);
        signal?.removeEventListener('abort', aborted);
        error ? rej(error) : res();
      };
      ws.onopen = () => finish();
      ws.onerror = (event) => finish(lost(`CDP socket failed: ${event.error?.message ?? event.message ?? 'unknown WebSocket error'}`, event.error));
      ws.onclose = (event) => finish(lost(`CDP socket closed before opening: code=${event.code}, reason=${event.reason}, wasClean=${event.wasClean}`));
      signal?.addEventListener('abort', aborted, { once: true });
    });
  } catch (error) { close(); throw error; }
  ws.onclose = (event = {}) => close(lost(`CDP socket closed: code=${event.code}, reason=${event.reason}, wasClean=${event.wasClean}`));
  ws.onerror = (event = {}) => close(lost(`CDP socket failed: ${event.error?.message ?? event.message ?? 'unknown WebSocket error'}`, event.error));
  let id = 0;
  const requests = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(String(m.data));
    if (msg.id && pending.has(msg.id)) {
      const { res, rej, timer } = pending.get(msg.id);
      clearTimeout(timer);
      pending.delete(msg.id);
      if (msg.error) rej(new Error(`${msg.error.message}`)); else res(msg.result);
    } else if (msg.method?.startsWith('Network.')) {
      const { requestId } = msg.params;
      if (msg.method === 'Network.requestWillBeSent') requests.set(requestId, msg.params.request.url);
      if (msg.method === 'Network.loadingFailed') events.push({ method: msg.method, params: {
        url: requests.get(requestId), type: msg.params.type, errorText: msg.params.errorText, canceled: msg.params.canceled,
      } });
      if (['Network.loadingFinished', 'Network.loadingFailed'].includes(msg.method)) requests.delete(requestId);
    } else if (msg.method) events.push(msg);
  };
  const send = (method, params = {}, timeout = commandTimeout) => new Promise((res, rej) => {
    if (closed) { rej(signal?.aborted ? signal.reason : new Error(`${closeError.message}; command: ${method}`, { cause: closeError })); return; }
    const i = ++id;
    const timer = setTimeout(() => { pending.delete(i); rej(new Error(`CDP command timed out: ${method}`)); }, Math.min(timeout, commandTimeout));
    pending.set(i, { res, rej, timer, method });
    try { ws.send(JSON.stringify({ id: i, method, params })); }
    catch (error) { clearTimeout(timer); pending.delete(i); rej(error); }
  });
  try {
    await send('Page.enable');
    await send('Runtime.enable');
    await send('Log.enable');
    await send('Network.enable');
    // The app follows the system appearance (local #274) and headless Chrome reports light; the checklist was written for the
    // dark default, so every page starts as a dark system. The theme-system step flips it with the same call.
    await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] });
  } catch (error) { close(); throw error; }
  let seeded = false;
  const page_ = {
    send,
    events,
    close,
    signal,
    get closed() { return closed; },
    /**
     * Evaluate in the page and return the result by value, waiting for a returned promise.
     * `Runtime.evaluate` with `awaitPromise` leaves the awaited promise reachable only from the page's own pending work, so Chrome
     * may collect it mid-flight ("Promise was collected", seen on a first dynamic `import()` under load, local #608). The result is
     * therefore held by a remote-object handle, which roots the promise, and awaited with `Runtime.awaitPromise`.
     */
    async eval(expression, { timeout = commandTimeout } = {}) {
      const group = 'smoke-eval';
      const r = await send('Runtime.evaluate', { expression, returnByValue: false, awaitPromise: false, objectGroup: group }, timeout);
      try {
        return await evalValue(send, r, timeout, group);
      } finally {
        if (r.result?.objectId) send('Runtime.releaseObjectGroup', { objectGroup: group }).catch(() => {});
      }
    },
    async goto(url) {
      // A throwaway profile is a first run (local #208: asked "Use Vim-style keys?", default No);
      // the smokes drive the app as an existing install that kept the old default, so Vim keys stay on.
      if (!seeded) {
        await send('Page.addScriptToEvaluateOnNewDocument', { source: EXISTING_INSTALL_SEED });
        seeded = true;
      }
      const marker = newToken();
      await this.eval(`document.documentElement.dataset.smokeNavigation = ${JSON.stringify(marker)}`);
      await send('Page.navigate', { url });
      // Page.navigate may return while Runtime still evaluates in the previous document.
      await this.ready(`document.documentElement.dataset.smokeNavigation !== ${JSON.stringify(marker)} && document.URL === ${JSON.stringify(url)} && document.readyState !== 'loading'`, { step: `navigate to ${url}` });
    },
    ready(expression, options) { return waitForReady(this, expression, options); },
    element(selector, action = '(e) => true', options = {}) {
      return this.ready(`document.querySelector(${JSON.stringify(selector)})`, { step: `read/interact with ${selector}`, ...options, element: true, action });
    },
    async key(key, { shift = false, ctrl = false, meta = false, alt = false } = {}) {
      const printable = key.length === 1;
      const mods = (alt ? 1 : 0) | (shift ? 8 : 0) | (ctrl ? 2 : 0) | (meta ? 4 : 0);
      const code = printable ? (/[a-z]/i.test(key) ? `Key${key.toUpperCase()}` : key === ' ' ? 'Space' : /[0-9]/.test(key) ? `Digit${key}` : '') : key;
      const base = { key, code, modifiers: mods, windowsVirtualKeyCode: printable ? (/^[A-Za-z0-9 ]$/.test(key) ? key.toUpperCase().charCodeAt(0) : 0) : ({ Enter: 13, Escape: 27, Tab: 9, Backspace: 8, Delete: 46, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Home: 36, End: 35, PageUp: 33, PageDown: 34 }[key] ?? 0) };
      await send('Input.dispatchKeyEvent', { type: 'keyDown', ...base, ...(printable && !ctrl && !meta && !alt ? { text: key } : key === 'Enter' && !ctrl ? { text: '\r' } : {}) });
      await send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
    },
    async keys(seq) { for (const k of seq) await this.key(k, { shift: /[A-Z:?]/.test(k) }); },
    /** A trusted mouse click at the centre of the element `expression` evaluates to (it takes focus, as a real click does). */
    async click(expression, options = {}) {
      const at = await this.ready(expression, { step: 'click', ...options, element: true,
        action: `(e) => { e.scrollIntoView({ block: 'center', inline: 'center' }); const r = e.getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; }` });
      for (const type of ['mousePressed', 'mouseReleased']) await send('Input.dispatchMouseEvent', { type, x: at.x, y: at.y, button: 'left', clickCount: 1 });
    },
    async shot(path) {
      const r = await send('Page.captureScreenshot', { format: 'png' });
      mkdirSync(join(path, '..'), { recursive: true });
      writeFileSync(path, Buffer.from(r.data, 'base64'));
      return path;
    },
    /** `evalMs` bounds each evaluation, so a renderer that stops answering costs one short attempt, not the whole command timeout. */
    async waitFor(expression, ms = 8000, { evalMs } = {}) {
      const end = Date.now() + ms;
      while (Date.now() < end) {
        signal?.throwIfAborted();
        if (closed) throw closeError;
        try { if (await this.eval(expression, evalMs ? { timeout: evalMs } : {})) return true; } catch { /* page still loading */ }
        await new Promise((r) => setTimeout(r, 100));
      }
      return false;
    },
  };
  return page_;
}

export { createBridge };
