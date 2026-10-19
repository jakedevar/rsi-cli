// An isolated, temporary rsid for the real-daemon end-to-end smoke (hub #3105, local #115).
//
// `startIsolatedRsid` spawns a built rsid binary with its OWN state: a fresh HOME, database and socket
// under a mkdtemp directory, and a scrubbed environment (no RSI_* variable, no session token, no provider
// key). It records the socket it started in a module-private registry; the bridge's write mode
// (smoke-bridge.mjs `createBridge({ isolated })`) refuses any socket that is not in it. So a write can
// never reach ~/.rsi, the operator's daemon or anything this process did not start itself.
//
// The finished session (with its conversation) comes from `rsi-e2e-seed` (crates/rsid/src/bin/rsi-e2e-seed.rs, local #122):
// a test-only binary behind rsid's `e2e-seed` cargo feature, which release builds refuse. It writes through the
// real Store API into this run's database while the daemon is stopped, and only into a state directory this
// file marked (MARKER_FILE). No SQL function is stubbed and the daemon has no RPC that imports history.
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { OFFLINE_ENV, startEgressGuard } from './smoke-egress.mjs';
import { rpcCall } from './smoke-rpc.mjs';

/** Written into every state directory this module creates; `rsi-e2e-seed` refuses a directory without it. */
export const MARKER_FILE = '.rsi-e2e-isolated-state';
export const MARKER_TEXT = 'rsi-e2e-isolated-state v1\n';

/** The cargo command that builds the daemon and the seeder together. */
export const BUILD_COMMAND = 'cargo build -p rsid --features e2e-seed --bin rsid --bin rsi-e2e-seed';

/** Sockets this process started itself (the only ones the write bridge accepts). */
const OWNED = new Map();

/** True only for the socket of an isolated rsid started by `startIsolatedRsid` in this process and still running. */
export function isOwnedIsolatedSocket(socket) {
  const h = OWNED.get(socket);
  return !!h && h.running;
}

/** Throws unless `socket` is a live isolated socket this process started (and is not the operator's). */
export function assertIsolatedSocket(socket) {
  if (typeof socket !== 'string' || !socket) throw new Error('isolated-write: no socket');
  const operator = [join(homedir(), '.rsi', 'daemon.sock'), process.env.RSI_DAEMON_SOCKET, process.env.RSI_DAEMON_SOCKET_PATH]
    .filter(Boolean).map((p) => resolve(p));
  if (operator.includes(resolve(socket))) throw new Error('isolated-write refuses the operator\'s daemon socket');
  if (!isOwnedIsolatedSocket(socket)) throw new Error('isolated-write refuses a socket this run did not start itself');
  return socket;
}

/** Find a built rsid: an explicit path, $RSI_E2E_RSID, else this checkout's target/{debug,release}/rsid. */
export function findRsid(explicit, env = process.env, exists = existsSync) {
  const repo = resolve(dirname(new URL(import.meta.url).pathname), '..', '..');
  const candidates = [explicit, env.RSI_E2E_RSID, join(repo, 'target', 'debug', 'rsid'), join(repo, 'target', 'release', 'rsid')]
    .filter(Boolean);
  return candidates.find((c) => exists(c)) ?? null;
}

/** Find the seeder: $RSI_E2E_SEEDER, else `rsi-e2e-seed` next to the rsid binary under test. Null when it was not built. */
export function findSeeder(rsid, env = process.env, exists = existsSync) {
  const candidates = [env.RSI_E2E_SEEDER, rsid ? join(dirname(rsid), 'rsi-e2e-seed') : null].filter(Boolean);
  return candidates.find((c) => exists(c)) ?? null;
}

const git = (cwd, args, home) => spawnSync('git', args, {
  cwd, encoding: 'utf8',
  env: { PATH: process.env.PATH, HOME: home, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
    GIT_AUTHOR_NAME: 'e2e', GIT_AUTHOR_EMAIL: 'e2e@example.invalid', GIT_COMMITTER_NAME: 'e2e', GIT_COMMITTER_EMAIL: 'e2e@example.invalid' },
});

/** What identifies the daemon build under test: the binary's SHA-256, the git HEAD of the checkout holding it, and the last commit that touched the daemon's sources. */
export function describeBinary(rsid) {
  const real = realpathSync(rsid);
  const sha256 = createHash('sha256').update(readFileSync(real)).digest('hex');
  const top = spawnSync('git', ['-C', dirname(real), 'rev-parse', '--show-toplevel'], { encoding: 'utf8' });
  let head = null;
  let dirty = null;
  let rustCommit = null;
  if (top.status === 0) {
    const root = top.stdout.trim();
    const g = (args) => spawnSync('git', ['-C', root, ...args], { encoding: 'utf8' }).stdout.trim();
    head = g(['rev-parse', 'HEAD']) || null;
    // "Dirty" means the daemon's own sources differ from HEAD (desktop-only edits do not change the binary).
    dirty = g(['status', '--porcelain', '--untracked-files=no', '--', 'crates', 'Cargo.toml', 'Cargo.lock']) !== '';
    rustCommit = g(['log', '-1', '--format=%H', '--', 'crates', 'Cargo.toml', 'Cargo.lock']) || null;
  }
  return { path: real, sha256, checkoutHead: head, checkoutDirty: dirty, lastRustCommit: rustCommit };
}

/**
 * Start an isolated rsid. Resolves { socket, dir, home, pid, binary, stop(), running }.
 * Everything lives under `dir` (a fresh /tmp/rsi-e2e-* directory), which `stop()` removes.
 */
export async function startIsolatedRsid(rsid, { keep = false } = {}) {
  // A short base: a Unix socket path is limited to about 104 bytes (macOS).
  const dir = mkdtempSync(join(existsSync('/tmp') ? '/tmp' : process.env.TMPDIR ?? '/tmp', 'rsi-e2e-'));
  const home = join(dir, 'home');
  const socket = join(dir, 'd.sock');
  mkdirSync(home, { recursive: true });
  mkdirSync(join(dir, 'tmp'), { recursive: true });
  writeFileSync(join(dir, MARKER_FILE), MARKER_TEXT);
  const log = join(dir, 'rsid.log');
  // A scrubbed environment: only what a daemon needs, never a token, a provider key or an inherited RSI_*
  // variable. The only RSI_* variables are the isolated socket and rsid's offline mode (RSI_OFFLINE=1, #130):
  // from its first boot no provider probe, poll, catalog fetch or credential check opens a connection.
  // Every proxy variable also points at a local refusing listener (smoke-egress.mjs), the witness: any
  // attempt that slipped past the mode is recorded there, refused and fails the run.
  const egress = await startEgressGuard();
  const env = { PATH: process.env.PATH ?? '/usr/bin:/bin', HOME: home, TMPDIR: join(dir, 'tmp'), LANG: 'C', RSI_DAEMON_SOCKET_PATH: socket, ...OFFLINE_ENV, ...egress.env() };
  const out = openSync(log, 'a');
  let stopped = false;
  const handle = {
    socket, dir, home, log, egress, rsid, pid: null, binary: describeBinary(rsid), running: false, child: null,
    async stop() {
      // `running` is false while a restart is mid-way (or after one failed): stop must still clean up.
      if (stopped) return;
      stopped = true;
      handle.running = false;
      OWNED.delete(socket);
      await killChild();
      await egress.close();
      if (!keep) rmSync(dir, { recursive: true, force: true });
    },
    /** Stop the daemon, run `whileStopped` (if any) on its state, and start it again (so it loads what was seeded). */
    async restart(whileStopped) {
      handle.running = false;
      await killChild();
      rmSync(socket, { force: true });
      if (whileStopped) await whileStopped();
      await boot();
    },
  };
  const killChild = async () => {
    const c = handle.child;
    if (!c || c.exitCode !== null) return;
    c.kill('SIGTERM');
    await new Promise((r) => { const t = setTimeout(() => { c.kill('SIGKILL'); r(); }, 8000); c.once('exit', () => { clearTimeout(t); r(); }); });
  };
  const boot = async () => {
    // ExecuteWorkflow from the desktop sends no working_dir (as the TUI), so rsid resolves the repository from its
    // own cwd: seedSynthetic points `cwd` at the synthetic git project before the restart that loads the seed.
    const child = spawn(rsid, [], { env, cwd: handle.cwd ?? dir, stdio: ['ignore', out, out] });
    handle.child = child;
    handle.pid = child.pid;
    handle.running = true;
    OWNED.set(socket, handle);
    child.once('exit', () => { if (handle.child === child) handle.running = false; });
    for (let i = 0; i < 150; i++) {
      if (!handle.running) break;
      if (existsSync(socket)) {
        try { await rpcCall(socket, 'GetHealthStatus', null, 3000); return; } catch { /* not ready yet */ }
      }
      await new Promise((r) => setTimeout(r, 200));
    }
    await handle.stop();
    throw new Error(`the isolated rsid did not become ready (log kept at ${log})`);
  };
  await boot();
  return handle;
}

/**
 * Seed the synthetic world the end-to-end steps run against. All of it goes straight to the isolated
 * daemon (these are harness setup calls, not the page's): a synthetic git project, two Issues, a Group
 * container and one finished leaf session with a conversation that edits a real file.
 * Returns the ids and paths the steps assert on.
 */
export async function seedSynthetic(handle) {
  assertIsolatedSocket(handle.socket);
  const { socket, dir, home } = handle;
  const root = join(dir, 'proj');
  handle.cwd = root;
  mkdirSync(join(root, 'src'), { recursive: true });
  writeFileSync(join(root, 'README.md'), '# synthetic project\n\nA throwaway repository for the desktop end-to-end smoke.\n');
  writeFileSync(join(root, 'src', 'lib.rs'), 'pub fn answer() -> u32 {\n    41\n}\n');
  // One project slash command, so the message box's `/` suggestions have something real to list.
  mkdirSync(join(root, '.claude', 'commands'), { recursive: true });
  writeFileSync(join(root, '.claude', 'commands', 'e2e-hello.md'), '---\ndescription: Say hello (end-to-end smoke)\n---\nSay hello.\n');
  git(root, ['init', '-q', '-b', 'main'], home);
  git(root, ['add', '-A'], home);
  git(root, ['commit', '-q', '-m', 'synthetic initial commit'], home);
  // A graph run (ExecuteWorkflow) takes its base from `origin/rolling`: a bare repository beside the project, on the
  // local disk, plays that remote, so the run needs no network.
  const originDir = join(dir, 'origin.git');
  git(dir, ['init', '-q', '--bare', '-b', 'rolling', originDir], home);
  git(root, ['remote', 'add', 'origin', originDir], home);
  git(root, ['push', '-q', 'origin', 'main:rolling'], home);

  const project = await rpcCall(socket, 'CreateProject', { name: 'e2e-project', path: root });
  const projectId = project.id;
  const issueA = await rpcCall(socket, 'CreateIssueV2', {
    project_id: projectId, title: 'Seed Issue alpha', body: 'First seeded Issue.', priority: 2, labels: ['seed'], idempotency_key: 'e2e-seed-alpha',
  });
  const issueB = await rpcCall(socket, 'CreateIssueV2', {
    project_id: projectId, title: 'Seed Issue beta', body: 'Second seeded Issue.', priority: 3, labels: ['seed'], idempotency_key: 'e2e-seed-beta',
  });
  const group = await rpcCall(socket, 'CreateContainer', {
    kind: 'Group', name: 'E2E group', parent_id: null, project_id: projectId, tags: ['e2e'], topology_id: null,
  });

  // One finished leaf session. A real provider is never launched and no row is written by hand: while the
  // daemon is stopped the test-only seeder (see the header) writes the session and its conversation through
  // the store's own API into this run's database, and the daemon restarts to load them.
  const seeder = findSeeder(handle.rsid);
  if (!seeder) {
    const error = new Error(`no rsi-e2e-seed next to ${handle.rsid} (build it with \`${BUILD_COMMAND}\` or set RSI_E2E_SEEDER)`);
    error.code = 'runner_unavailable';
    throw error;
  }
  const groupId = group.id ?? group.session?.id ?? group.container?.id;
  const leafId = '5e5e5e5e-0000-4000-8000-000000000001';
  const spec = {
    session: { id: leafId, query: 'Fix the answer function', title: 'E2E finished session', working_dir: root, project_id: projectId, parent_id: groupId ?? null },
    events: [
      { event_type: 'Message', role: 'User', content: 'Fix the answer function.' },
      { event_type: 'ToolUse', role: 'Assistant', content: '', tool_name: 'Edit', tool_use_id: 'toolu_e2e_1',
        tool_input: { file_path: join(root, 'src', 'lib.rs'), old_string: '41', new_string: '42' } },
      { event_type: 'Message', role: 'Assistant', content: 'I fixed the answer function and it now returns 42.' },
    ],
  };
  const specPath = join(dir, 'seed-spec.json');
  writeFileSync(specPath, JSON.stringify(spec));
  await handle.restart(async () => {
    const r = spawnSync(seeder, ['--state-dir', dir, '--spec', specPath], { encoding: 'utf8', env: { PATH: process.env.PATH, HOME: home, TMPDIR: join(dir, 'tmp') } });
    if (r.status !== 0) throw new Error(`rsi-e2e-seed failed (${r.status}): ${(r.stderr || r.stdout || '').trim()}`);
  });
  return { root, projectId, issueIds: [issueA.id ?? issueA.issue?.id, issueB.id ?? issueB.issue?.id], groupId: group.id, leafId, rawGroup: group };
}
