// Helpers for desktop/scripts/smoke.mjs: static server for dist, headless Chrome, a tiny CDP client.
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { delimiter, extname, join, normalize } from 'node:path';
import { authorizeInvoke, createBridge, shimSource } from './smoke-bridge.mjs';

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

/**
 * Serve `dist` on 127.0.0.1 with the shim injected into index.html and POST /__invoke wired to
 * the bridge. Requests must carry the per-run token (the page does; other local processes don't).
 */
export function serve({ dist, bridge, token, onRefusal, signal }) {
  signal?.throwIfAborted();
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    if (url.pathname === '/__invoke') {
      const gate = authorizeInvoke(req, { token, origin: `http://127.0.0.1:${server.address().port}` });
      if (!gate.ok) { onRefusal?.({ cmd: '(request)', outcome: 'refused', reason: gate.reason }); res.writeHead(403).end('forbidden'); return; }
      let body = '';
      req.on('data', (d) => { body += d; if (body.length > 1_000_000) req.destroy(); });
      req.on('end', async () => {
        let out;
        try {
          const { cmd, args } = JSON.parse(body);
          out = { result: (await bridge.invoke(String(cmd), args)) ?? null };
        } catch (e) { out = { error: e instanceof Error ? e.message : String(e) }; }
        res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(out));
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

/** Poll a condition or usable element; run element reads/actions in the same CDP evaluation. */
export async function waitForReady(page, expression, { step = 'page readiness', timeout = 5000, element = false, action = '(e) => true' } = {}) {
  const deadline = Date.now() + timeout;
  let excerpt = 'no page response';
  let lastError;
  do {
    const remaining = deadline - Date.now();
    if (remaining <= 0) break;
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

/** Launch headless Chrome with a throwaway profile; resolves with { cdpPort, pid, stop }. */
export async function launchChrome(bin, { width = 1400, height = 900, signal, launchTimeout = 5000 } = {}) {
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
    signal?.removeEventListener('abort', onAbort);
    kill('SIGTERM');
    const force = setTimeout(() => kill('SIGKILL'), 1000);
    try { await done; } finally { clearTimeout(force); kill('SIGKILL'); }
    await rm(scratch, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  })();
  const onAbort = () => { void stop().catch(() => {}); };
  signal?.addEventListener('abort', onAbort, { once: true });
  try {
    const deadline = Date.now() + launchTimeout;
    while (Date.now() < deadline) {
      signal?.throwIfAborted();
      if (exited) throw new Error(`Chrome exited before opening DevTools (${spawnError?.message ?? child.signalCode ?? child.exitCode}): ${stderr.trim()}`);
      const f = join(profile, 'DevToolsActivePort');
      if (existsSync(f)) {
        const port = Number(readFileSync(f, 'utf8').split('\n')[0]);
        if (port > 0) return { cdpPort: port, pid: child.pid, stop };
      }
      await new Promise((r) => setTimeout(r, 50));
    }
    throw new Error(`Chrome did not open a DevTools port within ${launchTimeout}ms: ${stderr.trim()}`);
  } catch (error) {
    await stop();
    throw error;
  }
}

/** Minimal CDP client over Node's built-in WebSocket. */
export async function connectPage(cdpPort, { signal, commandTimeout = 8000 } = {}) {
  signal?.throwIfAborted();
  const targets = await (await fetch(`http://127.0.0.1:${cdpPort}/json/list`, { signal: AbortSignal.any([AbortSignal.timeout(commandTimeout), ...(signal ? [signal] : [])]) })).json();
  signal?.throwIfAborted();
  const page = targets.find((t) => t.type === 'page');
  if (!page) throw new Error('no page target');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  const pending = new Map();
  let closed = false;
  const close = (error = new Error('CDP socket closed')) => {
    if (closed) return;
    closed = true;
    signal?.removeEventListener('abort', onAbort);
    for (const { rej, timer } of pending.values()) { clearTimeout(timer); rej(error); }
    pending.clear();
    ws.close();
  };
  const onAbort = () => close(signal.reason);
  signal?.addEventListener('abort', onAbort, { once: true });
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
      ws.onerror = () => finish(new Error('CDP socket failed'));
      ws.onclose = () => finish(new Error('CDP socket closed before opening'));
      signal?.addEventListener('abort', aborted, { once: true });
    });
  } catch (error) { close(); throw error; }
  ws.onclose = () => close();
  ws.onerror = () => close(new Error('CDP socket failed'));
  let id = 0;
  const events = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(String(m.data));
    if (msg.id && pending.has(msg.id)) {
      const { res, rej, timer } = pending.get(msg.id);
      clearTimeout(timer);
      pending.delete(msg.id);
      if (msg.error) rej(new Error(`${msg.error.message}`)); else res(msg.result);
    } else if (msg.method) events.push(msg);
  };
  const send = (method, params = {}, timeout = commandTimeout) => new Promise((res, rej) => {
    if (closed) { rej(new Error(`CDP socket closed: ${method}`)); return; }
    const i = ++id;
    const timer = setTimeout(() => { pending.delete(i); rej(new Error(`CDP command timed out: ${method}`)); }, Math.min(timeout, commandTimeout));
    pending.set(i, { res, rej, timer });
    try { ws.send(JSON.stringify({ id: i, method, params })); }
    catch (error) { clearTimeout(timer); pending.delete(i); rej(error); }
  });
  try {
    await send('Page.enable');
    await send('Runtime.enable');
  } catch (error) { close(); throw error; }
  const page_ = {
    send,
    events,
    close,
    signal,
    get closed() { return closed; },
    async eval(expression, { timeout = commandTimeout } = {}) {
      const r = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true }, timeout);
      if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
      return r.result.value;
    },
    async goto(url) {
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
    async key(key, { shift = false, ctrl = false } = {}) {
      const printable = key.length === 1;
      const mods = (shift ? 8 : 0) | (ctrl ? 2 : 0);
      const code = printable ? (/[a-z]/i.test(key) ? `Key${key.toUpperCase()}` : key === ' ' ? 'Space' : /[0-9]/.test(key) ? `Digit${key}` : '') : key;
      const base = { key, code, modifiers: mods, windowsVirtualKeyCode: printable ? (/^[A-Za-z0-9 ]$/.test(key) ? key.toUpperCase().charCodeAt(0) : 0) : ({ Enter: 13, Escape: 27, Tab: 9, Backspace: 8, Delete: 46, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Home: 36, End: 35, PageUp: 33, PageDown: 34 }[key] ?? 0) };
      await send('Input.dispatchKeyEvent', { type: 'keyDown', ...base, ...(printable && !ctrl ? { text: key } : key === 'Enter' && !ctrl ? { text: '\r' } : {}) });
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
    async waitFor(expression, ms = 8000) {
      const end = Date.now() + ms;
      while (Date.now() < end) {
        signal?.throwIfAborted();
        if (closed) throw new Error('CDP socket closed while waiting for page');
        try { if (await this.eval(expression)) return true; } catch { /* page still loading */ }
        await new Promise((r) => setTimeout(r, 100));
      }
      return false;
    },
  };
  return page_;
}

export { createBridge };
