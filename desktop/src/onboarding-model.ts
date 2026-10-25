// First-run guidance for the empty detail pane (pure, no DOM; tests/onboarding.test.mjs).
// A newcomer opens the app and sees one of: connecting, "cannot reach rsid"
// (with the socket it looked at, a Start rsid button for an installed service
// and the terminal commands under For developers, local #232), "connected, no
// sessions yet" (with the first three things to do), or the plain prompt.
import type { Platform } from './keyengine.js';
import { displayHint } from './registry.js';

export type DaemonLink =
  | { kind: 'unknown' }
  | { kind: 'unreachable'; socket: string; error?: string }
  | { kind: 'backend-error'; error: string }
  | { kind: 'connected'; socket: string };

export type OnboardingId = 'connecting' | 'daemon-down' | 'backend-error' | 'no-sessions' | 'no-projects' | 'select';

export interface OnboardingButton {
  /** A registry action id; the page's data-action wiring runs it. */
  action: 'refresh' | 'launch' | 'palette' | 'project-new';
  label: string;
  primary?: boolean;
}

/** What `daemon_service` (src-tauri/src/service.rs) found: can this window start rsid? */
export type DaemonService =
  | { kind: 'unknown' }
  | {
    kind: 'installed';
    manager: 'launchd' | 'systemd';
    name: string;
    path: string;
    /** What the service manager says right now (local #258); absent from an older backend. */
    state?: 'running' | 'stopped' | 'unknown';
    pid?: number | null;
    /** The socket this window looks at, the one the service holds, and whether they are the same. */
    socket?: { app: string; service: string | null; matches: boolean };
  }
  | { kind: 'not-installed'; manager: string; looked: string[] }
  | { kind: 'unsupported'; reason: string };

/** What `start_daemon` answered when the service manager ran. */
export interface StartOutcome {
  ok: boolean;
  manager: string;
  command: string;
  status: number | null;
  stderr: string;
  /** Set when nothing ran because starting could not help: already running, or another socket. */
  refused?: 'running' | 'wrong-socket' | null;
}

/** Where a Start rsid press is: not pressed, waiting for rsid to answer, or failed. */
export type StartRun =
  | { kind: 'idle' }
  | { kind: 'starting' }
  | { kind: 'restarting' }
  | { kind: 'failed'; text: string; detail: string | null };

export interface DaemonStart {
  service: DaemonService;
  run: StartRun;
}

export const NO_START: DaemonStart = { service: { kind: 'unknown' }, run: { kind: 'idle' } };

/** The daemon-down card's Start rsid row (local #232); not a registry action, boot.ts wires it. */
export interface StartDaemonView {
  /** null on a system where this window cannot start rsid (the status line says why). */
  button: { label: string; enabled: boolean; primary: boolean; busy: boolean } | null;
  /** One plain line: progress, why the button is off, or what failed. */
  status: string | null;
  /** The raw failure (command, exit status, stderr) for the Details section. */
  detail: string | null;
}

/** A closed disclosure under the card: the terminal commands, for people who use them. */
export interface DeveloperHelp {
  summary: string;
  lines: string[];
}

export interface OnboardingView {
  id: OnboardingId;
  title: string;
  lines: string[];
  buttons: OnboardingButton[];
  start?: StartDaemonView;
  developer?: DeveloperHelp;
}

export const START_LABEL = 'Start rsid';
export const STARTING_LABEL = 'Starting rsid…';
/** Offered instead of Start rsid when the service is running but not answering (local #258). */
export const RESTART_LABEL = 'Restart rsid';
export const RESTARTING_LABEL = 'Restarting rsid…';
/** How long the card waits for the socket after the service manager accepted the start. */
export const START_WAIT_SECONDS = 45;

/** A start_daemon result or refusal as the card's failure line and Details text. */
export function startFailure(result: StartOutcome | string, verb: 'start' | 'restart' = 'start'): { text: string; detail: string } {
  if (typeof result === 'string') {
    const why = shortError(result) || 'no reason given';
    return { text: `Could not ${verb} rsid: ${why}.`, detail: result || why };
  }
  const status = result.status === null ? 'no exit status (it did not finish)' : `exit status ${result.status}`;
  const tool = result.manager === 'launchd' ? 'macOS (launchctl)' : result.manager === 'systemd' ? 'systemd' : 'The service manager';
  return {
    text: `${tool} could not ${verb} rsid: ${status}. Open Details for its message, then try again or start rsid from a terminal (For developers, below).`,
    detail: `$ ${result.command}\n${status}\n${result.stderr.trim() || '(nothing on stderr)'}`,
  };
}

/** The service manager accepted the start but the socket never answered. */
export function startTimedOut(manager: string, command: string, seconds = START_WAIT_SECONDS, verb: 'start' | 'restart' = 'start'): { text: string; detail: string } {
  const log = manager === 'systemd' ? 'journalctl --user -u rsid.service' : '~/.rsi/daemon.log';
  return {
    text: `rsid was ${verb === 'restart' ? 'restarted' : 'started'} but did not answer within ${seconds} seconds. It may still be starting: this window keeps checking. If it never answers, its log says why (${log}).`,
    detail: `$ ${command}\nexit status 0; no answer on the socket within ${seconds} s`,
  };
}

export function serviceLine(service: DaemonService): string {
  if (service.kind === 'installed') {
    const what = service.manager === 'launchd' ? `a login service (${service.name})` : `the systemd user service ${service.name}`;
    if (service.socket && !service.socket.matches) {
      return `rsid is installed as ${what}, but it serves a different socket than the one this window looks at, so starting it would not help.`;
    }
    if (service.state === 'running') {
      const pid = typeof service.pid === 'number' ? ` (pid ${service.pid})` : '';
      return `rsid is installed as ${what} and is already running${pid}, but it is not answering. ${START_LABEL} would do nothing, so it is not offered.`;
    }
    const manager = service.manager === 'launchd' ? 'macOS' : 'systemd';
    return `rsid is installed as ${what}. ${START_LABEL} asks ${manager} to start it; this window reconnects as soon as it answers.`;
  }
  return 'Once rsid is running, this window reconnects on its own within a few seconds.';
}

/** The service is running but this window cannot reach it: Start would do nothing, so offer a confirmed Restart (local #258). */
function runningView(service: Extract<DaemonService, { kind: 'installed' }>, run: StartRun): StartDaemonView {
  if (run.kind === 'restarting') {
    return { button: { label: RESTARTING_LABEL, enabled: false, primary: false, busy: true }, status: `${RESTARTING_LABEL} This window reconnects as soon as it answers.`, detail: null };
  }
  const manager = service.manager === 'launchd' ? 'macOS' : 'systemd';
  const pid = typeof service.pid === 'number' ? ` (pid ${service.pid})` : '';
  const button = { label: RESTART_LABEL, enabled: true, primary: false, busy: false };
  if (run.kind === 'failed') return { button, status: run.text, detail: run.detail };
  return {
    button,
    status: `rsid's service is running${pid} but is not answering at ${service.socket?.app ?? 'this socket'}. It may still be starting, or it may be stuck. Retry now checks again. ${RESTART_LABEL} asks ${manager} to stop it and start it again, which interrupts any sessions it is running; you are asked first.`,
    detail: null,
  };
}

function startView(start: DaemonStart): StartDaemonView {
  const { service, run } = start;
  switch (service.kind) {
    case 'unknown':
      return { button: { label: START_LABEL, enabled: false, primary: false, busy: false }, status: 'Checking whether rsid is installed on this computer…', detail: null };
    case 'unsupported':
      return {
        button: null,
        status: `This window cannot start rsid here: ${shortError(service.reason)}. Start rsid yourself (For developers, below); this window reconnects on its own.`,
        detail: null,
      };
    case 'not-installed':
      return {
        button: { label: START_LABEL, enabled: false, primary: false, busy: false },
        status: 'Start rsid is off: rsid is not installed as a service on this computer, so there is nothing for this window to start. Install rsid first (see Install in the desktop README), or start it from a terminal (For developers, below).',
        detail: service.looked.length > 0 ? `Looked for: ${service.looked.join(', ')}` : null,
      };
    case 'installed':
      if (service.socket && !service.socket.matches) {
        const held = service.socket.service ?? 'its own socket';
        return {
          button: { label: START_LABEL, enabled: false, primary: false, busy: false },
          status: `Start rsid is off: this window is looking at ${service.socket.app}, but the login service holds ${held}, so neither starting nor restarting it would make this window connect. Quit and reopen the app without RSI_DAEMON_SOCKET, or with it set to ${held}.`,
          detail: null,
        };
      }
      if (service.state === 'running') return runningView(service, run);
      if (run.kind === 'starting' || run.kind === 'restarting') {
        return { button: { label: STARTING_LABEL, enabled: false, primary: true, busy: true }, status: `${STARTING_LABEL} This window reconnects as soon as it answers.`, detail: null };
      }
      if (run.kind === 'failed') {
        return { button: { label: START_LABEL, enabled: true, primary: true, busy: false }, status: run.text, detail: run.detail };
      }
      return { button: { label: START_LABEL, enabled: true, primary: true, busy: false }, status: null, detail: null };
  }
}

const MAX_ERROR = 200;

/** One short, single-line error for display. */
export function shortError(e: string | undefined): string {
  const t = (e ?? '').replace(/\s+/g, ' ').trim();
  return t.length > MAX_ERROR ? `${t.slice(0, MAX_ERROR - 1)}…` : t;
}

/** What a project is, in words a first-time user needs (desktop #210). */
export const PROJECT_EXPLAINER =
  'A project is a folder with your code that sessions work in. Create one first so new sessions start in the right place; you can also start a session without a project.';

function noSessionLines(key: (k: string) => string, vimOn: boolean): string[] {
  return [
    `Start one with New session (${key('Ctrl-N')}): describe the task, pick a provider and send.`,
    `Every feature is in the menu bar and in the Actions… palette (${key('Ctrl-K')}): type a few letters of its name.`,
    'Sessions you start here or in the TUI appear in this list as they happen.',
    vimOn
      ? 'Know the TUI? Its Vim keys work here (? lists them). Prefer the mouse? Turn off Vim keys with the switch in the top bar, or in Settings › Workspace.'
      : 'Use the rsi TUI\'s Vim keys? Turn on Vim keys (the switch in the top bar, or Settings › Workspace).',
  ];
}

export function onboardingView(link: DaemonLink, sessionCount: number, platform: Platform, projectCount = 1, vimOn = false, daemonStart: DaemonStart = NO_START): OnboardingView {
  const key = (k: string): string => displayHint(k, platform);
  switch (link.kind) {
    case 'unknown':
      return { id: 'connecting', title: 'Connecting to rsid…', lines: [], buttons: [] };
    case 'backend-error':
      return {
        id: 'backend-error',
        title: 'The desktop backend did not answer',
        lines: [
          shortError(link.error),
          'Quit and reopen the app. If it keeps happening, start rsi-desktop from a terminal: it prints a line starting “rsi-desktop: boot” and any error to that terminal.',
        ],
        buttons: [{ action: 'refresh', label: 'Try again', primary: true }],
      };
    case 'unreachable': {
      const last = shortError(link.error);
      const start = startView(daemonStart);
      return {
        id: 'daemon-down',
        title: 'Cannot reach the rsid daemon',
        lines: [
          'RSI Desktop is a window onto rsid, the daemon that runs your sessions. It keeps none of its own, so the list stays empty until rsid is running.',
          `Looking for it at ${link.socket}.${last ? ` Last error: ${last}` : ''}`,
          serviceLine(daemonStart.service),
        ],
        // Start rsid (start.button) comes first in the row and is the primary button when it can run.
        buttons: [
          { action: 'refresh', label: 'Retry now', primary: !start.button?.primary },
          { action: 'palette', label: `Actions… (${key('Ctrl-K')})` },
        ],
        start,
        developer: {
          summary: 'For developers',
          lines: [
            'Start rsid in a terminal: run rsid, or ./scripts/dev-daemon.sh from a checkout; make release-install installs and starts it. This window reconnects on its own within a few seconds.',
            'Using another daemon? Quit and reopen the app with RSI_DAEMON_SOCKET set to its socket path.',
          ],
        },
      };
    }
    case 'connected':
      if (sessionCount === 0 && projectCount === 0) {
        return {
          id: 'no-projects',
          title: 'Welcome. Start by creating a project.',
          lines: [PROJECT_EXPLAINER, ...noSessionLines(key, vimOn)],
          buttons: [
            { action: 'project-new', label: 'Create your first project', primary: true },
            { action: 'launch', label: 'New session' },
            { action: 'palette', label: 'Actions…' },
          ],
        };
      }
      if (sessionCount === 0) {
        return {
          id: 'no-sessions',
          title: 'Connected to rsid. No sessions yet.',
          lines: noSessionLines(key, vimOn),
          buttons: [
            { action: 'launch', label: 'New session', primary: true },
            { action: 'palette', label: 'Actions…' },
          ],
        };
      }
      return {
        id: 'select',
        title: 'Select a session.',
        lines: projectCount === 0
          ? ['Click one in the list to read its conversation and message it.', PROJECT_EXPLAINER]
          : ['Click one in the list to read its conversation and message it.'],
        buttons: projectCount === 0 ? [{ action: 'project-new', label: 'New project…' }] : [],
      };
  }
}

/**
 * The same help as a strip above whatever the user has open (local #266): the card lives in the empty
 * detail pane, so with a session open it was hidden while the top-bar pill said "daemon unreachable".
 * It reuses the card's service state, so it never offers a Start the card would not (local #258).
 */
export interface DaemonBannerView {
  id: 'daemon-down' | 'backend-error';
  headline: string;
  /** One plain sentence: what the service state means and what happens next. */
  summary: string | null;
  buttons: OnboardingButton[];
  start?: StartDaemonView;
  /** Closed under "More": what rsid is and where this window looked. */
  more: string[];
  developer?: DeveloperHelp;
}

/** null while rsid answers (or has not been tried yet); a strip when the link is down. */
export function daemonBannerView(link: DaemonLink, platform: Platform, vimOn = false, daemonStart: DaemonStart = NO_START): DaemonBannerView | null {
  if (link.kind !== 'unreachable' && link.kind !== 'backend-error') return null;
  const v = onboardingView(link, 1, platform, 1, vimOn, daemonStart);
  if (link.kind === 'backend-error') {
    return { id: 'backend-error', headline: v.title, summary: v.lines[0] ?? null, buttons: v.buttons, more: v.lines.slice(1) };
  }
  return {
    id: 'daemon-down',
    headline: 'rsid is not running. What this window shows is the last known state and may be out of date.',
    summary: serviceLine(daemonStart.service),
    // Retry now only: the Actions… palette is one click away in the top bar.
    buttons: v.buttons.filter((b) => b.action === 'refresh'),
    ...(v.start ? { start: v.start } : {}),
    more: v.lines.slice(0, 2),
    ...(v.developer ? { developer: v.developer } : {}),
  };
}

/** The "last known" tag beside a session's status while rsid cannot be reached. */
export const STALE_NOTE = 'last known: rsid is not running';
/** Why Stop, Pause and Archive are off while rsid cannot be reached. */
export const NEEDS_RSID = 'Needs rsid, which is not running. This works again when it is back.';
