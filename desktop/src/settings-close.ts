// The Settings close prompt (local #269): closing the pane with changes that were not applied asks Apply, Discard or Keep
// editing instead of throwing them away. Plain buttons, so the mouse and Tab (with Vim keys off) both finish it; Esc and
// clicking outside mean Keep editing, the safe answer.
import { el } from './dom.js';
import { type CloseChoice, closeCopy } from './settings-model.js';
import { focusOwnsKey } from './modal-gate.js';

/** Ask what to do with `n` unapplied changes; resolves once, with the button pressed ('keep' on Esc). */
export function askCloseChoice(n: number): Promise<CloseChoice> {
  const copy = closeCopy(n);
  document.getElementById('settings-close')?.remove();
  const d = document.createElement('dialog');
  d.id = 'settings-close';
  d.className = 'pick';
  d.setAttribute('aria-label', copy.title);
  document.body.append(d);
  return new Promise<CloseChoice>((resolve) => {
    let done = false;
    const finish = (choice: CloseChoice): void => {
      if (done) return;
      done = true;
      if (d.open) d.close();
      d.remove();
      resolve(choice);
    };
    const button = (text: string, choice: CloseChoice, cls: string): HTMLButtonElement => {
      const b = el('button', cls, text);
      b.type = 'button';
      b.dataset.closeChoice = choice;
      b.addEventListener('click', () => finish(choice));
      return b;
    };
    const keep = button(copy.keep, 'keep', 'btn');
    const actions = el('div', 'pick-actions');
    actions.append(keep, button(copy.discard, 'discard', 'btn'), button(copy.apply, 'apply', 'btn primary'));
    const form = el('div');
    form.append(el('h2', '', copy.title), el('div', 'pick-empty', copy.body), actions);
    d.replaceChildren(form);
    // Esc closes a modal dialog by itself (a cancel); that is Keep editing, and the pane behind never sees the key.
    d.addEventListener('cancel', (ev) => { ev.preventDefault(); finish('keep'); });
    d.addEventListener('click', (ev) => { if (ev.target === d) finish('keep'); });
    d.onkeydown = (ev) => { if (focusOwnsKey(ev.key, ev.target as Element | null)) return; if (ev.key === 'Escape') ev.stopPropagation(); };
    d.showModal();
    keep.focus();
  });
}
