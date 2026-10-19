// Evidence that boot COMPLETED (pure; tests/onboarding.test.mjs). A window that
// opens proves nothing: a step that threw (the Settings config read once did)
// left a normal-looking, half-working app. Each boot step reports ok or the
// error; the result is written to the page (`data-boot` on <html>, readable by
// the CDP harness), to a visible banner when a step failed, and to the
// process log through the `boot_report` command ("rsi-desktop: boot …").

export type BootState = 'ready' | 'offline' | 'failed';

export interface BootStep {
  name: string;
  ok: boolean;
  /** Not attempted (the daemon is down, so reads of its config are skipped). */
  skipped?: boolean;
  error?: string;
}

export interface BootReport {
  state: BootState;
  daemon: 'connected' | 'unreachable' | 'error';
  sessions: number;
  steps: BootStep[];
  /** Uncaught page errors and unhandled rejections seen so far. */
  pageErrors: string[];
}

export const BOOT_STEPS = ['daemon', 'projects', 'labels', 'sessions', 'settings', 'live'] as const;

const clip = (t: string, n = 160): string => {
  const one = t.replace(/\s+/g, ' ').trim();
  return one.length > n ? `${one.slice(0, n - 1)}…` : one;
};

/**
 * ready: every step ran and passed. offline: the app is up but rsid is not
 * reachable (the onboarding panel explains it). failed: rsid answered (or the
 * backend errored) and some step or the page still failed.
 */
export function summarizeBoot(
  daemon: BootReport['daemon'],
  steps: readonly BootStep[],
  sessions: number,
  pageErrors: readonly string[],
): BootReport {
  const clean = steps.map((s) => ({
    name: s.name,
    ok: s.ok,
    ...(s.skipped ? { skipped: true } : {}),
    ...(s.error ? { error: clip(s.error) } : {}),
  }));
  const bad = clean.some((s) => !s.ok && !s.skipped) || pageErrors.length > 0;
  const state: BootState = daemon === 'unreachable'
    ? (pageErrors.length ? 'failed' : 'offline')
    : bad || daemon === 'error' ? 'failed' : 'ready';
  return { state, daemon, sessions, steps: clean, pageErrors: pageErrors.map((e) => clip(e)).slice(0, 5) };
}

/** The one log line (also the Rust side's format). */
export function bootLine(r: BootReport): string {
  const steps = r.steps.map((s) => `${s.name}:${s.skipped ? 'skipped' : s.ok ? 'ok' : 'failed'}`).join(',');
  const errs = r.pageErrors.length ? ` page_errors=${r.pageErrors.length}` : '';
  return `boot=${r.state} daemon=${r.daemon} sessions=${r.sessions} steps=${steps}${errs}`;
}

/** Text for the visible banner, or null when nothing needs the user's eye. */
export function bootBanner(r: BootReport): string | null {
  if (r.state !== 'failed') return null;
  const parts = r.steps.filter((s) => !s.ok && !s.skipped).map((s) => `${s.name}: ${s.error ?? 'failed'}`);
  parts.push(...r.pageErrors.map((e) => `page error: ${e}`));
  if (r.daemon === 'error' && parts.length === 0) parts.push('the desktop backend did not answer');
  return `Part of the app did not start (${parts.join('; ')}). Quit and reopen it; if this stays, run rsi-desktop from a terminal and read its log.`;
}
