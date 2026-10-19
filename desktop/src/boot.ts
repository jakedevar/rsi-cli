// DOM side of first-run guidance and boot evidence (models: onboarding-model.ts,
// boot-model.ts). All text goes in with textContent.
import { api } from './api.js';
import { el } from './dom.js';
import { BOOT_STEPS, bootBanner, summarizeBoot, type BootReport, type BootStep } from './boot-model.js';
import { onboardingView, type DaemonLink } from './onboarding-model.js';
import { state } from './state.js';
import { currentPlatform } from './vimkeys.js';

let link: DaemonLink = { kind: 'unknown' };
let lastKey = '';
const pageErrors: string[] = [];
let steps: BootStep[] | null = null;
let sessionCount = 0;

/** Called by refreshHealth: a link change redraws the guidance. */
export function setDaemonLink(next: DaemonLink): void {
  const was = link.kind;
  link = next;
  renderOnboarding();
  if (steps && was !== 'connected' && next.kind === 'connected' && reconnect) reconnect();
  else if (steps) publishBoot();
}

let reconnect: (() => void) | null = null;

/** Run `cb` when the link comes up after boot (an offline start, or rsid restarted). */
export function onReconnect(cb: () => void): void {
  reconnect = cb;
}

export function daemonLink(): DaemonLink {
  return link;
}

/** Fill the empty detail pane with the right first-run guidance. */
export function renderOnboarding(): void {
  const host = document.getElementById('empty');
  if (!host) return;
  const v = onboardingView(link, state.sessions.length, currentPlatform());
  const key = JSON.stringify(v);
  if (key === lastKey) return;
  lastKey = key;
  host.dataset.onboarding = v.id;
  const kids: HTMLElement[] = [el('h2', 'onboard-title', v.title)];
  for (const line of v.lines) kids.push(el('p', 'onboard-line', line));
  if (v.buttons.length > 0) {
    const row = el('div', 'onboard-actions');
    for (const b of v.buttons) {
      const btn = el('button', `btn${b.primary ? ' primary' : ''}`, b.label);
      btn.type = 'button';
      btn.dataset.action = b.action;
      row.append(btn);
    }
    kids.push(row);
  }
  host.replaceChildren(...kids);
}

/** Uncaught errors count against boot: they were invisible before. */
export function installBootErrorCapture(): void {
  const note = (m: string): void => {
    if (pageErrors.length < 20) pageErrors.push(m);
    if (steps) publishBoot();
  };
  window.addEventListener('error', (ev) => note(ev.message || String(ev.error)));
  window.addEventListener('unhandledrejection', (ev) => note(`unhandled rejection: ${String((ev.reason as Error | undefined)?.message ?? ev.reason)}`));
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
