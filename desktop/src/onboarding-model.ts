// First-run guidance for the empty detail pane (pure, no DOM; tests/onboarding.test.mjs).
// A newcomer opens the app and sees one of: connecting, "cannot reach rsid"
// (with the socket it looked at and how to start the daemon), "connected, no
// sessions yet" (with the first three things to do), or the plain prompt.
import type { Platform } from './keyengine.js';
import { displayHint } from './registry.js';

export type DaemonLink =
  | { kind: 'unknown' }
  | { kind: 'unreachable'; socket: string; error?: string }
  | { kind: 'backend-error'; error: string }
  | { kind: 'connected'; socket: string };

export type OnboardingId = 'connecting' | 'daemon-down' | 'backend-error' | 'no-sessions' | 'select';

export interface OnboardingButton {
  /** A registry action id; the page's data-action wiring runs it. */
  action: 'refresh' | 'launch' | 'palette';
  label: string;
  primary?: boolean;
}

export interface OnboardingView {
  id: OnboardingId;
  title: string;
  lines: string[];
  buttons: OnboardingButton[];
}

const MAX_ERROR = 200;

/** One short, single-line error for display. */
export function shortError(e: string | undefined): string {
  const t = (e ?? '').replace(/\s+/g, ' ').trim();
  return t.length > MAX_ERROR ? `${t.slice(0, MAX_ERROR - 1)}…` : t;
}

export function onboardingView(link: DaemonLink, sessionCount: number, platform: Platform): OnboardingView {
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
      return {
        id: 'daemon-down',
        title: 'Cannot reach the rsid daemon',
        lines: [
          'RSI Desktop is a window onto rsid, the daemon that runs your sessions. It keeps none of its own, so the list stays empty until rsid is running.',
          `Looking for it at ${link.socket}.${last ? ` Last error: ${last}` : ''}`,
          'Start rsid in a terminal: run rsid, or ./scripts/dev-daemon.sh from a checkout; make release-install installs and starts it. This window reconnects on its own within a few seconds.',
          'Using another daemon? Quit and reopen the app with RSI_DAEMON_SOCKET set to its socket path.',
        ],
        buttons: [
          { action: 'refresh', label: 'Retry now', primary: true },
          { action: 'palette', label: `Actions… (${key('Ctrl-K')})` },
        ],
      };
    }
    case 'connected':
      if (sessionCount === 0) {
        return {
          id: 'no-sessions',
          title: 'Connected to rsid. No sessions yet.',
          lines: [
            `Start one with New session (${key('Ctrl-N')}): describe the task, pick a provider and send.`,
            `Every feature is in the menu bar and in the Actions… palette (${key('Ctrl-K')}): type a few letters of its name.`,
            'Sessions you start here or in the TUI appear in this list as they happen.',
            'Know the TUI? Its Vim keys work here (? lists them). Prefer the mouse? Turn them off in Settings (View › Settings…).',
          ],
          buttons: [
            { action: 'launch', label: 'New session', primary: true },
            { action: 'palette', label: 'Actions…' },
          ],
        };
      }
      return {
        id: 'select',
        title: 'Select a session.',
        lines: ['Click one in the list to read its conversation and message it.'],
        buttons: [],
      };
  }
}
