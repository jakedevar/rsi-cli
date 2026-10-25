// The status card under a Failed or Interrupted session's header (local #225): DOM only.
// Text goes in with textContent; the raw reason sits under the shared Details disclosure
// (dom.ts `errorDetails`, #207) and the last error line goes through `renderMessage`.
import { el, errorDetails, renderMessage } from './dom.js';
import type { OutcomeCard, OutcomeStepId } from './session-outcome-model.js';

export interface OutcomeHandlers {
  /** A next-step button. */
  step(id: OutcomeStepId): void;
  /** Copy the last error's full text. */
  copyError(text: string): void;
}

/** Draw (or hide, with null) the card into `host`. */
export function drawOutcomeCard(host: HTMLElement, card: OutcomeCard | null, h: OutcomeHandlers): void {
  host.hidden = card === null;
  if (!card) { host.replaceChildren(); return; }
  host.className = `outcome-card outcome-${card.tone}`;
  host.setAttribute('role', 'status');
  const kids: HTMLElement[] = [el('div', 'outcome-title', card.title)];
  if (card.reason) kids.push(el('div', 'outcome-reason', card.reason));
  if (card.error) {
    const row = el('div', 'outcome-error');
    row.append(el('span', 'outcome-error-label', 'Last error: '));
    const msg = el('span', 'outcome-error-text');
    renderMessage(msg, card.error.line);
    row.append(msg);
    const full = card.error.full;
    const copy = el('button', 'btn link small', 'Copy');
    copy.type = 'button';
    copy.title = 'Copy the whole error message';
    copy.addEventListener('click', () => h.copyError(full));
    row.append(copy);
    kids.push(row);
  }
  if (card.steps.length > 0) {
    const bar = el('div', 'outcome-steps');
    for (const s of card.steps) {
      const b = el('button', s.id === 'continue' ? 'btn small primary' : 'btn small', s.label);
      b.type = 'button';
      b.title = s.help;
      b.dataset.outcomeStep = s.id;
      b.addEventListener('click', () => h.step(s.id));
      bar.append(b);
    }
    kids.push(bar);
  }
  if (card.nextLine) {
    // data-key-tip: with Vim keys off the key-hints pass drops "(or press i)" and keeps the button name.
    const next = el('div', 'outcome-next', card.nextLine);
    next.dataset.keyTip = '';
    kids.push(next);
  }
  if (card.detail) kids.push(errorDetails(card.detail));
  host.replaceChildren(...kids);
}
