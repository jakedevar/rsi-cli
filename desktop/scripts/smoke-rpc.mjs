// One JSON-RPC 2.0 call over a daemon Unix socket (one line out, one line in), as src-tauri/src/daemon.rs does.
import { createConnection } from 'node:net';
import { homedir } from 'node:os';
import { join } from 'node:path';

export function defaultSocket() {
  return process.env.RSI_DAEMON_SOCKET || join(homedir(), '.rsi', 'daemon.sock');
}

/** One JSON-RPC 2.0 call over the daemon socket (one line out, one line in), as daemon.rs does. */
export function rpcCall(socketPath, method, params, timeoutMs = 20000) {
  return new Promise((resolve, reject) => {
    const sock = createConnection(socketPath);
    let buf = '';
    const timer = setTimeout(() => { sock.destroy(); reject(new Error('daemon did not answer in time')); }, timeoutMs);
    const done = (fn, v) => { clearTimeout(timer); sock.destroy(); fn(v); };
    sock.on('error', (e) => done(reject, new Error(`cannot reach rsid: ${e.message}`)));
    sock.on('data', (d) => {
      buf += d.toString('utf8');
      const nl = buf.indexOf('\n');
      if (nl < 0) return;
      try {
        const res = JSON.parse(buf.slice(0, nl));
        if (res.error) done(reject, new Error(`daemon error ${res.error.code}: ${res.error.message}`));
        else done(resolve, res.result ?? null);
      } catch (e) { done(reject, new Error(`malformed daemon response: ${e.message}`)); }
    });
    sock.on('connect', () => sock.write(`${JSON.stringify({ jsonrpc: '2.0', id: 1, method, params: params ?? null })}\n`));
  });
}
