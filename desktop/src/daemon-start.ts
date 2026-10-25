// The daemon-down card's Start rsid (local #232; model: onboarding-model.ts, backend:
// src-tauri/src/service.rs). It asks the backend once whether rsid is installed as an
// OS service, runs start_daemon on a press, then polls the socket until rsid answers.
import { api } from './api.js';
import { confirmDialog } from './approvals.js';
import { NO_START, RESTART_LABEL, START_WAIT_SECONDS, startFailure, startTimedOut, type DaemonLink, type DaemonStart } from './onboarding-model.js';

let start: DaemonStart = NO_START;
let asking = false;
let askedAt = 0;
let onChange: () => void = () => undefined;
let poll: () => Promise<DaemonLink> = async () => ({ kind: 'unknown' });

const POLL_MS = 1000;
/** While rsid is down, read the service again at most this often: its state changes (a crash loop, a manual start). */
const ASK_EVERY_MS = 3000;

/** `changed` redraws the card; `check` reads the daemon link once (sessions.ts refreshHealth). */
export function wireDaemonStart(changed: () => void, check: () => Promise<DaemonLink>): void {
  onChange = changed;
  poll = check;
}

export function daemonStart(): DaemonStart {
  return start;
}

/** Read through a call: an await can change it, which TypeScript's narrowing does not see. */
function starting(): boolean {
  return start.run.kind === 'starting' || start.run.kind === 'restarting';
}

function set(next: DaemonStart): void {
  start = next;
  onChange();
}

/** Ask the backend about the service (installed? running? which socket?) and show the answer. */
async function askService(): Promise<void> {
  asking = true;
  askedAt = Date.now();
  try {
    const service = await api.daemonService();
    set({ ...start, service });
  } catch (e: unknown) {
    set({ ...start, service: { kind: 'unsupported', reason: `the desktop backend could not check (${String((e as Error)?.message ?? e)})` } });
  } finally {
    asking = false;
  }
}

/** Called on every link change: read the service while rsid is down; forget a press once it is up. */
export function noteDaemonLink(link: DaemonLink): void {
  if (link.kind === 'connected') {
    askedAt = 0;
    if (start.run.kind !== 'idle') set({ ...start, run: { kind: 'idle' } });
    return;
  }
  if (link.kind !== 'unreachable' || asking || starting() || Date.now() - askedAt < ASK_EVERY_MS) return;
  void askService();
}

/** Restart stops a running rsid: ask first, in words about what is lost. */
async function confirmRestart(): Promise<boolean> {
  const svc = start.service;
  const pid = svc.kind === 'installed' && typeof svc.pid === 'number' ? ` (pid ${svc.pid})` : '';
  return confirmDialog(
    `rsid's service is running${pid} but not answering. Restarting stops it and starts it again.`,
    RESTART_LABEL,
    { title: 'Restart rsid?', detail: ['Any session the running rsid is working on is interrupted. Sessions you can read again after it answers.'], cancelLabel: 'Keep it running' },
  );
}

/** Restart for the newer-build notice: rsid answers, so say what is lost, not that it is stuck. */
async function confirmBuildRestart(): Promise<boolean> {
  return confirmDialog(
    'Restarting rsid stops it and starts it again, so it runs the newer build.',
    RESTART_LABEL,
    { title: 'Restart rsid?', detail: ['Any session rsid is working on is interrupted. Sessions you can read again after it answers.'], cancelLabel: 'Keep it running' },
  );
}

/**
 * The newer-build notice's Restart rsid (build-notice.ts). Reads the service first (it is only read while rsid is down).
 * Returns false when this window cannot restart rsid (not a service it manages, another socket, or not running), so the
 * notice can say what to run instead; true once the press was handled, confirmed or declined.
 */
export async function restartForNewBuild(): Promise<boolean> {
  if (starting()) return true;
  await askService();
  const svc = start.service;
  if (svc.kind !== 'installed' || svc.state !== 'running' || svc.socket?.matches === false) return false;
  await startDaemonPressed(confirmBuildRestart);
  return true;
}

/** The card's one service button: Start rsid, or Restart rsid (confirmed) when the service is already running. */
export async function startDaemonPressed(confirm: () => Promise<boolean> = confirmRestart): Promise<void> {
  if (start.service.kind !== 'installed' || starting()) return;
  const manager = start.service.manager;
  const restart = start.service.state === 'running';
  if (restart && !(await confirm())) return;
  const verb = restart ? 'restart' : 'start';
  set({ ...start, run: { kind: restart ? 'restarting' : 'starting' } });
  let command = '';
  try {
    const out = await (restart ? api.restartDaemon() : api.startDaemon());
    command = out.command;
    if (out.refused) {
      // The window's picture was stale (it started or stopped meanwhile): read it again, run nothing more.
      set({ ...start, run: { kind: 'idle' } });
      await askService();
      return;
    }
    if (!out.ok) {
      set({ ...start, run: { kind: 'failed', ...startFailure(out, verb) } });
      return;
    }
  } catch (e) {
    set({ ...start, run: { kind: 'failed', ...startFailure(String((e as Error)?.message ?? e), verb) } });
    return;
  }
  const deadline = Date.now() + START_WAIT_SECONDS * 1000;
  while (Date.now() < deadline) {
    const link = await poll();
    // A connected link resets the run (noteDaemonLink) and the card goes away.
    if (link.kind === 'connected' || !starting()) return;
    await new Promise((r) => setTimeout(r, POLL_MS));
  }
  if (starting()) set({ ...start, run: { kind: 'failed', ...startTimedOut(manager, command, START_WAIT_SECONDS, verb) } });
}
