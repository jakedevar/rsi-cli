// DOM and polling for the newer-build notice (model: build-notice-model.ts). A persistent, non-modal strip
// under the top bar, never focused (the user may be typing). The aria-live host in index.html is only redrawn when
// the notice changes, so a screen reader hears it once, not on every poll. All text goes in with textContent.
import { api } from './api.js';
import { restartForNewBuild } from './daemon-start.js';
import { el, toast } from './dom.js';
import { RESTART_UNAVAILABLE, buildNoticeView, noticeKey, type BuildNotice } from './build-notice-model.js';

/** Read GetDaemonInfo at most this often (the daemon caches the local scan for 30 s). */
export const BUILD_POLL_MS = 30_000;
/** localStorage key for the build SHA the user dismissed. */
export const DISMISSED_KEY = 'rsi.desktop.buildNoticeDismissed.v1';

let notice: BuildNotice | null = null;
let drawnKey = '';
let busy = false;

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

function dismissedSha(): string | null {
  try { return storage()?.getItem(DISMISSED_KEY) ?? null; } catch { return null; }
}

/** Read the daemon's build facts once. A failed or old-rsid read keeps what is shown and says nothing. */
export async function refreshBuildNotice(): Promise<void> {
  let info: unknown;
  try {
    info = await api.daemonBuild();
  } catch {
    return;
  }
  notice = buildNoticeView(info);
  renderBuildNotice();
}

async function restartPressed(): Promise<void> {
  if (busy) return;
  busy = true;
  try {
    if (!(await restartForNewBuild())) toast(RESTART_UNAVAILABLE);
  } finally {
    busy = false;
  }
  await refreshBuildNotice();
}

function dismissPressed(): void {
  if (notice === null) return;
  try { storage()?.setItem(DISMISSED_KEY, notice.builtSha); } catch { /* no storage: dismissed until the next start */ }
  memoryDismissed = notice.builtSha;
  renderBuildNotice();
}

let memoryDismissed: string | null = null;

/** Draw the notice, or clear it. Redraws only when what is shown changes. */
export function renderBuildNotice(): void {
  const host = document.getElementById('build-notice');
  if (!host) return;
  const dismissed = memoryDismissed ?? dismissedSha();
  const key = noticeKey(notice, dismissed);
  if (key === drawnKey) return;
  drawnKey = key;
  host.hidden = key === '';
  host.dataset.buildNotice = key === '' ? '' : 'newer';
  if (notice === null || key === '') { host.replaceChildren(); return; }
  const restart = el('button', 'btn primary', 'Restart rsid');
  restart.type = 'button';
  restart.dataset.buildRestart = '';
  restart.addEventListener('click', () => void restartPressed());
  const dismiss = el('button', 'btn', 'Dismiss');
  dismiss.type = 'button';
  dismiss.dataset.buildDismiss = '';
  dismiss.title = 'Hide this until a different build appears';
  dismiss.addEventListener('click', dismissPressed);
  const actions = el('div', 'build-notice-actions');
  actions.append(restart, dismiss);
  host.replaceChildren(
    el('div', 'build-notice-head', notice.headline),
    el('div', 'build-notice-line', notice.facts),
    el('div', 'build-notice-line', notice.action),
    actions,
  );
}

/** Wire the poll once the app is up: now, then every BUILD_POLL_MS. */
export function startBuildNotice(): void {
  void refreshBuildNotice();
  window.setInterval(() => void refreshBuildNotice(), BUILD_POLL_MS);
}

/** Test seam: forget what was read, shown and dismissed in this page. */
export function resetBuildNotice(): void {
  notice = null; drawnKey = ''; memoryDismissed = null; busy = false;
}
