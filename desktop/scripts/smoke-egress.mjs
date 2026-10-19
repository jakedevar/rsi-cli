// Egress guard for the isolated rsid of the real-daemon end-to-end smoke (hub #3105, local #123, #130).
//
// The isolated rsid must make no outbound network connection, from its first boot on, even with synthetic
// values. It runs in rsid's offline mode (`RSI_OFFLINE=1`, crates/rsid-core/src/offline.rs): no provider probe,
// poll, catalog fetch or credential check opens a connection; each reports a typed offline state instead
// (a provider-key check reads `unknown` with detail `offline_unchecked`).
//
// This guard is the witness that the mode holds. The harness also points every proxy variable at THIS local
// listener, which records the request line and answers 403 before closing, so an attempt that slipped past
// the mode is recorded and still never leaves the machine. Three checks, all at the end of the run
// (smoke-e2e.mjs), and no tolerance window:
//   1. the proxy log must be empty: any attempt, to any host, at any time (first boot included) fails;
//   2. a socket sample of the rsid process (lsof): any established connection to a non-loopback address is a
//      direct connection that bypassed the proxy and fails the run. Sampled after each section; when lsof
//      is unavailable that is reported as skipped, never as clean;
//   3. when the run set a provider key, rsid must have recorded that key's check as `offline_unchecked`:
//      proof the check ran and short-circuited offline (not merely that it was never reached).
import { spawnSync } from 'node:child_process';
import { createServer } from 'node:net';

/** The environment that switches the isolated rsid offline (rsid_core::offline::ENV_OFFLINE). */
export const OFFLINE_ENV = Object.freeze({ RSI_OFFLINE: '1' });
/** The detail code rsid records for a provider-key check skipped offline (REASON_OFFLINE_UNCHECKED). */
export const OFFLINE_KEY_CHECK = 'offline_unchecked';

/** Start the refusing local proxy. Resolves { port, attempts, env(), close(), url }. */
export async function startEgressGuard() {
  const attempts = [];
  const sockets = new Set();
  const server = createServer((sock) => {
    sockets.add(sock);
    sock.on('close', () => sockets.delete(sock));
    sock.on('error', () => {});
    let seen = false;
    sock.on('data', (buf) => {
      if (seen) return;
      seen = true;
      attempts.push({ ...parseProxyRequest(buf.toString('latin1')), at: Date.now() });
      sock.end('HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const port = server.address().port;
  const url = `http://127.0.0.1:${port}`;
  return {
    port, url, attempts,
    /** Proxy variables (both cases; reqwest reads either) plus a loopback bypass so local traffic stays local. */
    env() {
      return {
        HTTP_PROXY: url, HTTPS_PROXY: url, ALL_PROXY: url, http_proxy: url, https_proxy: url, all_proxy: url,
        NO_PROXY: '127.0.0.1,localhost,::1', no_proxy: '127.0.0.1,localhost,::1',
      };
    },
    async close() {
      for (const s of sockets) s.destroy();
      await new Promise((r) => server.close(() => r()));
    },
  };
}

/** The host and kind of one proxied request line (`CONNECT host:443 ...` or `GET http://host/path ...`). */
export function parseProxyRequest(head) {
  const line = head.split(/\r?\n/, 1)[0] ?? '';
  const [method = '', target = ''] = line.split(' ');
  let host = '';
  if (method.toUpperCase() === 'CONNECT') host = target.replace(/:\d+$/, '');
  else { try { host = new URL(target).hostname; } catch { host = ''; } }
  return { method: method.toUpperCase(), host: host.toLowerCase(), refused: true };
}

/**
 * Judge the guard. `attempts` are the proxy's records since rsid's first boot, `direct` the non-loopback
 * sockets lsof saw. `keyCheck` is `{ set, detail }`: whether the run set a provider key, and the detail code
 * rsid recorded for that key's check (`null` when none was seen). Zero attempts are required: there is no
 * first-boot window and no allowed host.
 */
export function judgeEgress({ attempts, direct, keyCheck = { set: false, detail: null }, sampled }) {
  const problems = [];
  if (attempts.length) problems.push(`outbound attempt from the offline rsid: ${attempts.map((a) => `${a.method} ${a.host || '?'}`).join(', ')}`);
  if (direct.length) problems.push(`direct (non-proxy) connection from rsid: ${direct.join(', ')}`);
  if (keyCheck.set && keyCheck.detail !== OFFLINE_KEY_CHECK) problems.push(`a provider key was set but rsid recorded its check as ${keyCheck.detail ?? 'nothing'}, not ${OFFLINE_KEY_CHECK}, so the offline short-circuit is unproven`);
  return {
    ok: problems.length === 0,
    problems,
    attempts: attempts.map((a) => `${a.method} ${a.host}`),
    keyCheck,
    directConnections: direct,
    socketSample: sampled ? 'lsof' : 'skipped (lsof unavailable)',
  };
}

/** Non-loopback, connected TCP peers in `lsof -nP` output (the NAME column holds `local->remote`). */
export function parseLsofDirect(text) {
  const out = [];
  for (const line of text.split('\n')) {
    const m = line.match(/(\S+)->(\S+)(?:\s+\(\w+\))?\s*$/);
    if (!m) continue;
    const remote = m[2].replace(/:\d+$/, '').replace(/^\[|\]$/g, '');
    if (remote === '127.0.0.1' || remote === '::1' || remote === 'localhost' || remote.startsWith('127.')) continue;
    out.push(m[2]);
  }
  return out;
}

/** Sample rsid's connected sockets. Resolves { sampled, direct }; `sampled` is false when lsof cannot run. */
export function sampleDirectConnections(pid, run = spawnSync) {
  if (!pid) return { sampled: false, direct: [] };
  const r = run('lsof', ['-nP', '-a', '-p', String(pid), '-i'], { encoding: 'utf8', timeout: 8000 });
  if (r.error || r.status === null || r.status > 1) return { sampled: false, direct: [] };
  return { sampled: true, direct: parseLsofDirect(r.stdout ?? '') };
}
