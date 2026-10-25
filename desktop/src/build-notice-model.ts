// The "a newer RSI build exists" notice (parity with the TUI's #363 banner). Model only: GetDaemonInfo's
// answer in, the plain words out. DOM and polling: build-notice.ts. The TUI's own text is
// `DaemonInfoV1::local_build_warning()` (crates/rsi-common/src/agent_daemon_info.rs): it shows while
// `local_build.differs` holds (a different SHA, strictly newer than the running executable).

/** How many characters of a SHA the notice shows (the TUI shows 9). */
export const SHORT_SHA = 9;

export interface BuildNotice {
  /** The full SHA of the newer build: a dismissal is kept for this build only. */
  builtSha: string;
  running: string;
  built: string;
  headline: string;
  /** Running and built builds, in plain words. */
  facts: string;
  /** What to do, for a build that is already installed and for one that is only built. */
  action: string;
}

export const BUILD_NOTICE_HEADLINE = 'A newer RSI build is available.';
export const RELEASE_INSTALL_COMMAND = 'make release-install';
export const RESTART_UNAVAILABLE = `This app cannot restart rsid here. In a terminal, run ${RELEASE_INSTALL_COMMAND}: it installs the newer build and restarts rsid.`;

function text(v: unknown): string | null {
  return typeof v === 'string' && v.trim() !== '' ? v : null;
}

/**
 * The notice for one `GetDaemonInfo` answer, or null: no notice for the same build, for no local build, for an
 * answer that is not the shape (an older rsid) or for a build with no SHA.
 */
export function buildNoticeView(info: unknown): BuildNotice | null {
  if (typeof info !== 'object' || info === null) return null;
  const rec = info as Record<string, unknown>;
  const running = text(rec.build_sha);
  const local = rec.local_build;
  if (running === null || typeof local !== 'object' || local === null) return null;
  const lb = local as Record<string, unknown>;
  const builtSha = text(lb.sha);
  if (lb.differs !== true || builtSha === null) return null;
  const built = builtSha.slice(0, SHORT_SHA);
  const was = running.slice(0, SHORT_SHA);
  return {
    builtSha,
    running: was,
    built,
    headline: BUILD_NOTICE_HEADLINE,
    facts: `rsid is running build ${was}. Build ${built} was made on this computer.`,
    action: `Restart rsid to use it if it is installed. If it is only built, run ${RELEASE_INSTALL_COMMAND} in a terminal: it installs the build and restarts rsid.`,
  };
}

/** Is this notice hidden: the user dismissed this very build. A different newer build shows again. */
export function isDismissed(notice: BuildNotice | null, dismissedSha: string | null): boolean {
  return notice !== null && dismissedSha !== null && dismissedSha === notice.builtSha;
}

/** Banner key: the DOM is redrawn (and a screen reader told) only when this changes, not on every poll. */
export function noticeKey(notice: BuildNotice | null, dismissedSha: string | null): string {
  return notice === null || isDismissed(notice, dismissedSha) ? '' : `${notice.running}>${notice.builtSha}`;
}
