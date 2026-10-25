// DOM builders for the daemon-down help (models: onboarding-model.ts; wiring: boot.ts). The card in the
// empty detail pane and the strip above an open session (local #266) share these, so both offer the same
// Start rsid / Restart rsid / Retry now with the same service state (local #258). All text is textContent.
import { el, errorDetails, renderMessage } from './dom.js';
import type { DaemonBannerView, OnboardingButton, StartDaemonView } from './onboarding-model.js';

/** Start rsid / Restart rsid first (it is not a registry action, so it has its own click), then the registry buttons. */
export function actionRow(cls: string, start: StartDaemonView | undefined, buttons: OnboardingButton[], onStart: () => void): HTMLElement | null {
  const sb = start?.button;
  if (buttons.length === 0 && !sb) return null;
  const row = el('div', cls);
  if (sb) {
    const btn = el('button', `btn${sb.primary ? ' primary' : ''}`, sb.label);
    btn.type = 'button';
    btn.dataset.startDaemon = '';
    btn.disabled = !sb.enabled;
    if (sb.busy) btn.setAttribute('aria-busy', 'true');
    btn.addEventListener('click', onStart);
    row.append(btn);
  }
  for (const b of buttons) {
    const btn = el('button', `btn${b.primary ? ' primary' : ''}`, b.label);
    btn.type = 'button';
    btn.dataset.action = b.action;
    row.append(btn);
  }
  return row;
}

/** The Start rsid line: progress or the failure in plain words, the raw output under Details (#207). */
export function startStatus(text: string, detail: string | null): HTMLElement {
  const p = el('div', 'onboard-line onboard-start-status');
  p.setAttribute('role', 'status');
  renderMessage(p, text);
  if (detail !== null) p.append(errorDetails(detail));
  return p;
}

/** Fill the strip (or empty it for null). */
export function fillDaemonBanner(host: HTMLElement, v: DaemonBannerView | null, onStart: () => void): void {
  if (!v) { host.replaceChildren(); return; }
  const kids: HTMLElement[] = [el('div', 'daemon-banner-head', v.headline)];
  if (v.summary) kids.push(el('div', 'daemon-banner-line', v.summary));
  const row = actionRow('daemon-banner-actions', v.start, v.buttons, onStart);
  if (row) kids.push(row);
  if (v.start?.status) kids.push(startStatus(v.start.status, v.start.detail));
  if (v.more.length > 0 || v.developer) {
    const d = el('details', 'daemon-banner-more');
    d.append(el('summary', undefined, 'More'));
    for (const line of v.more) d.append(el('p', 'onboard-line', line));
    if (v.developer) {
      d.append(el('p', 'onboard-line daemon-banner-dev', v.developer.summary));
      for (const line of v.developer.lines) d.append(el('p', 'onboard-line', line));
    }
    kids.push(d);
  }
  host.replaceChildren(...kids);
}
