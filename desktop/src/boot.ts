// DOM side of first-run guidance and boot evidence (models: onboarding-model.ts,
// boot-model.ts). All text goes in with textContent.
import { api } from './api.js';
import { daemonStart, noteDaemonLink, startDaemonPressed } from './daemon-start.js';
import { actionRow, fillDaemonBanner, startStatus } from './daemon-banner.js';
import { el } from './dom.js';
import { cardPrimaryButton, focusCardPrimary, keepFocus } from './focus-return.js';
import { BOOT_STEPS, bootBanner, summarizeBoot, type BootReport, type BootStep } from './boot-model.js';
import { daemonBannerView, onboardingView, type DaemonLink } from './onboarding-model.js';
import { reportRuntimeError } from './runtime-notice.js';
import { state } from './state.js';
import { currentPlatform, isVimKeysOn } from './vimkeys.js';
import { ownProjects } from './project-model.js';

let link: DaemonLink = { kind: 'unknown' };
let lastKey = '';
const pageErrors: string[] = [];
let steps: BootStep[] | null = null;
let sessionCount = 0;

/** Called by refreshHealth: a link change redraws the guidance. */
export function setDaemonLink(next: DaemonLink): void {
  const was = link.kind;
  link = next;
  const down = isDown(next);
  const flipped = down !== state.daemonDown;
  state.daemonDown = down;
  noteDaemonLink(next);
  renderOnboarding();
  // The open session's header and controls go stale or live again with the link (local #266).
  if (flipped) for (const cb of downListeners) cb();
  if (steps && was !== 'connected' && next.kind === 'connected' && reconnect) reconnect();
  else if (steps) publishBoot();
}

let reconnect: (() => void) | null = null;
const downListeners: Array<() => void> = [];

function isDown(l: DaemonLink): boolean {
  return l.kind === 'unreachable' || l.kind === 'backend-error';
}

/** Run `cb` when the link goes down or comes back (the open session's header redraws). */
export function onDaemonDownChange(cb: () => void): void {
  downListeners.push(cb);
}

/** Run `cb` when the link comes up after boot (an offline start, or rsid restarted). */
export function onReconnect(cb: () => void): void {
  reconnect = cb;
}

export function daemonLink(): DaemonLink {
  return link;
}

let watchingVim = false;

/** The Welcome text names the Vim keys switch's state: read it again when the switch flips (it sets data-vim-keys on <html>). */
function watchVimSwitch(): void {
  if (watchingVim || typeof MutationObserver === 'undefined') return;
  watchingVim = true;
  new MutationObserver(() => renderOnboarding()).observe(document.documentElement, { attributes: true, attributeFilter: ['data-vim-keys'] });
}

/** Fill the empty detail pane with the right first-run guidance. */
export function renderOnboarding(): void {
  renderDaemonBanner();
  const host = document.getElementById('empty');
  if (!host) return;
  watchVimSwitch();
  const v = onboardingView(link, state.sessions.length, currentPlatform(), ownProjects(state.projects).length, isVimKeysOn(), daemonStart());
  const key = JSON.stringify(v);
  if (key === lastKey) return;
  lastKey = key;
  host.dataset.onboarding = v.id;
  const kids: HTMLElement[] = [el('h2', 'onboard-title', v.title)];
  for (const line of v.lines) kids.push(el('p', 'onboard-line', line));
  const row = actionRow('onboard-actions', v.start, v.buttons, () => void startDaemonPressed());
  if (row) kids.push(row);
  if (v.start?.status) kids.push(startStatus(v.start.status, v.start.detail));
  if (v.developer) {
    const d = el('details', 'onboard-developer');
    d.append(el('summary', undefined, v.developer.summary));
    for (const line of v.developer.lines) d.append(el('p', 'onboard-line', line));
    kids.push(d);
  }
  host.replaceChildren(...kids);
  // The card's main action takes focus when nothing else has it (local #244).
  focusCardPrimary(host);
}

let bannerKey = '';

/**
 * The daemon-down help over whatever is open (local #266). The card in #empty shows only with no session
 * selected; this strip sits above the layout so a session, the Issues list or the dashboard cannot hide it.
 * CSS hides it while the card itself is on screen (styles.css), so the two never repeat each other.
 * It never takes focus: the user may be typing a message.
 */
export function renderDaemonBanner(): void {
  const host = document.getElementById('daemon-banner');
  if (!host) return;
  const v = daemonBannerView(link, currentPlatform(), isVimKeysOn(), daemonStart());
  const key = JSON.stringify(v);
  if (key === bannerKey) return;
  bannerKey = key;
  host.hidden = v === null;
  host.dataset.daemonBanner = v?.id ?? '';
  keepFocus(host, () => fillDaemonBanner(host, v, () => void startDaemonPressed()));
}

/** The top-bar pill, pressed while rsid is down: move to the help (the strip, or the card when it is the one on screen). */
export function focusDaemonHelp(): void {
  const banner = document.getElementById('daemon-banner');
  const empty = document.getElementById('empty');
  const target = banner && !banner.hidden && banner.offsetParent !== null ? banner : empty;
  if (!target) return;
  (cardPrimaryButton(target) as HTMLElement | null)?.focus();
}

/**
 * Uncaught errors BEFORE boot completed count against boot (they were invisible before). Once every step has
 * reported (`steps` is set) the app has started: a later error is a runtime error, shown as a dismissible notice
 * in plain words and logged, and never touches the boot banner or html[data-boot] (Issue #295).
 */
export function installBootErrorCapture(): void {
  // `raw` is the error as the plain-words mapping reads it (`TypeError: ...`); `boot` is the older one-line form.
  const note = (raw: string, boot: string): void => {
    if (steps) {
      reportRuntimeError(raw, logRuntimeLine);
      return;
    }
    if (pageErrors.length < 20) pageErrors.push(boot);
  };
  const named = (e: unknown): string => (e instanceof Error ? `${e.name}: ${e.message}` : String(e));
  window.addEventListener('error', (ev) => {
    const m = ev.message || String(ev.error);
    note(ev.error instanceof Error ? named(ev.error) : m, m);
  });
  window.addEventListener('unhandledrejection', (ev) => {
    note(named(ev.reason), `unhandled rejection: ${String((ev.reason as Error | undefined)?.message ?? ev.reason)}`);
  });
}

/** One process-log line for a runtime error: the boot report's page-error slot carries it (src-tauri/src/boot.rs prints `page: ...`). */
function logRuntimeLine(line: string): void {
  void api.bootReport({ ...currentReport(), pageErrors: [line] }).catch(() => undefined);
}

/** Run one boot step; a throw or a returned error string fails it, nothing else. */
export async function bootStep(name: string, fn: () => Promise<string | null | void>): Promise<BootStep> {
  let step: BootStep;
  try {
    const err = await fn();
    step = err ? { name, ok: false, error: err } : { name, ok: true };
  } catch (e) {
    step = { name, ok: false, error: String((e as Error)?.message ?? e) };
  }
  return step;
}

export function finishBoot(done: BootStep[]): BootReport {
  // Keep the documented order; drop nothing.
  steps = [...done].sort((a, b) => BOOT_STEPS.indexOf(a.name as never) - BOOT_STEPS.indexOf(b.name as never));
  sessionCount = state.sessions.length;
  return publishBoot();
}

function currentReport(): BootReport {
  const daemon = link.kind === 'connected' ? 'connected' : link.kind === 'backend-error' ? 'error' : 'unreachable';
  // A daemon that is down makes its dependent reads fail by nature: report them skipped.
  const adj = (steps ?? []).map((s) => (daemon !== 'connected' && s.name !== 'daemon' && !s.ok ? { ...s, skipped: true } : s));
  return summarizeBoot(daemon, adj, sessionCount, pageErrors);
}

let publishedKey = '';

function publishBoot(): BootReport {
  const r = currentReport();
  const key = JSON.stringify(r);
  if (key === publishedKey) return r;
  publishedKey = key;
  const root = document.documentElement;
  root.dataset.boot = r.state;
  root.dataset.bootReport = key;
  const banner = document.getElementById('boot-banner');
  if (banner) {
    const text = bootBanner(r);
    banner.hidden = text === null;
    banner.textContent = text ?? '';
  }
  // The process log line (rsi-desktop: boot …); a refusal here must never break the app.
  void api.bootReport(r).catch(() => undefined);
  return r;
}
