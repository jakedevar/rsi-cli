// A write-stub bridge for mouse-first UX audits (local #277, from audit 4 / #262).
//
// The smoke's read-only bridge (smoke-bridge.mjs) refuses every write command, so a flow that sends a message, stops a
// turn, archives a session or changes a setting only ever shows the error path. This wrapper lets the page believe the
// write worked, without letting it reach anything:
//   * a READ command (READ_COMMANDS) or a shim-local one (LOCAL_COMMANDS) goes to the wrapped bridge unchanged,
//     so reads still come from the fake daemon;
//   * ANY other command is a write: it is recorded, answered with a stub and NEVER forwarded. The wrapped bridge's
//     `call` is not even reachable for it, so no write can hit a socket, the fake daemon or a live rsid;
//   * `down` is an unreachable rsid like the real backend: daemon_info answers { connected: false, error } (it never throws,
//     src-tauri daemon.rs) and every other non-local command fails (the daemon-down card, the failed-send toast);
//     `failBackend` is the other case, where the backend itself does not answer and daemon_info throws too;
//   * `failWrites` makes chosen writes fail on demand with a chosen message.
// Arguments are recorded with credential-looking fields masked (redactConfig), never verbatim.
import { LOCAL_COMMANDS, READ_COMMANDS, createBridge, redactConfig } from './smoke-bridge.mjs';

/** What the Rust side says when the socket is gone (src-tauri/src/daemon.rs, macOS errno). */
export const DOWN_MESSAGE = 'cannot reach rsid: daemon.sock: Connection refused (os error 61)';

/**
 * Commands the read-only smoke refuses on purpose (its steps check the page copes with the refusal) but a view needs an answer to
 * show itself under audit: the stub bridge answers them locally, with no daemon traffic. DiscoverModels asks the provider what it offers
 * (daemon-side network); the manager policy classifier is a pure Rust function (manager_ops.rs) the page cannot run.
 */
export const AUDIT_LOCAL_COMMANDS = {
  discover_models: (a) => ({ models: a?.provider === 'Codex' ? [['gpt-5.5', 'GPT-5.5'], ['gpt-5.5-mini', 'GPT-5.5 mini']] : [['opus', 'Opus'], ['sonnet', 'Sonnet'], ['haiku', 'Haiku']] }),
  mgr_policy_classify: () => ({ preset: 'execute', profile: null, suggestions: [], root_group_mismatch: false, validation_error: null }),
};

/**
 * `list_*` and `get_*` commands that change something, so the stub answers them as writes instead of reading them. None exist
 * today. tests/audit-tooling.test.mjs fails when desktop/src names a `list_*` or `get_*` command that is in neither
 * READ_COMMANDS, LOCAL_COMMANDS nor this set: an unlisted one would be stubbed as a write and answered `null` (local #298).
 */
export const GET_LIST_WRITES = new Set();

/**
 * `call(method, params)` performs a daemon READ (the same hook createBridge takes). `stubs` maps a write command to
 * its answer: a value, or `(args) => value`; an unlisted write answers `null`. `local` is createBridge's override map.
 *
 * Returns `{ invoke, writes, log, setDown, failWrites, clearFailures, isDown }`:
 *   writes()       the recorded writes, oldest first: `{ cmd, args, outcome: 'stubbed' | 'failed' }`
 *   log            every decision, as createBridge's `record` plus the writes
 *   setDown(bool)  rsid unreachable (true) or back (false): daemon_info answers `{ socket, connected: false, error: DOWN_MESSAGE }`
 *                  like the real backend, so the page shows the daemon-down card (Start rsid, Retry now); the rest throw
 *   failBackend(bool)  the desktop backend itself not answering: every non-local command throws, daemon_info included
 *                  (the page shows the backend-error card, "Quit and reopen")
 *   failWrites(spec, message?)  spec = a command name, a list of names, or `true` (every write); the message defaults to
 *                  a generic daemon error. `clearFailures()` ends it.
 */
export function createStubBridge({ call, record = () => {}, stubs = {}, local = {} } = {}) {
  const log = [];
  const writeLog = [];
  const note = (e) => { log.push(e); record(e); };
  const inner = createBridge({ call, record: note, local });
  let down = false;
  let backendFailed = false;
  let failing = null;
  const isRead = (cmd) => Object.hasOwn(READ_COMMANDS, cmd);
  const isLocal = (cmd) => Object.hasOwn(LOCAL_COMMANDS, cmd);
  const failsWrite = (cmd) => failing !== null && (failing.all || failing.cmds.has(cmd));

  return {
    log,
    writes: () => writeLog.map((w) => ({ ...w })),
    isDown: () => down,
    isBackendFailed: () => backendFailed,
    setDown(v) { down = Boolean(v); },
    failBackend(v = true) { backendFailed = Boolean(v); },
    failWrites(spec, message = 'rsid refused the request: stubbed failure') {
      failing = spec === true ? { all: true, cmds: new Set(), message } : { all: false, cmds: new Set([spec].flat()), message };
    },
    clearFailures() { failing = null; },
    async invoke(cmd, args) {
      if (isLocal(cmd)) return inner.invoke(cmd, args);
      if (Object.hasOwn(AUDIT_LOCAL_COMMANDS, cmd)) {
        note({ cmd, outcome: 'local' });
        return (Object.hasOwn(local, cmd) ? local[cmd] : AUDIT_LOCAL_COMMANDS[cmd])(args);
      }
      if (down && !backendFailed && cmd === 'daemon_info') {
        note({ cmd, outcome: 'error', reason: DOWN_MESSAGE });
        return READ_COMMANDS.daemon_info.fallback(new Error(DOWN_MESSAGE));
      }
      if (down || backendFailed) {
        note({ cmd, outcome: 'error', reason: DOWN_MESSAGE });
        throw new Error(DOWN_MESSAGE);
      }
      if (isRead(cmd)) return inner.invoke(cmd, args);
      // A write. It stops here.
      const masked = redactConfig(args ?? null);
      if (failsWrite(cmd)) {
        writeLog.push({ cmd, args: masked, outcome: 'failed' });
        note({ cmd, outcome: 'error', write: true, reason: failing.message });
        throw new Error(failing.message);
      }
      writeLog.push({ cmd, args: masked, outcome: 'stubbed' });
      note({ cmd, outcome: 'stubbed', write: true });
      if (Object.hasOwn(stubs, cmd)) return typeof stubs[cmd] === 'function' ? stubs[cmd](args) : stubs[cmd];
      return null;
    },
  };
}
