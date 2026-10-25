// A dialog that closes while its opener is gone used to leave focus on the page (local #297): the browser restores focus
// only to an element still on the page, and most dialogs here are built when they open and removed when they close. This is
// the one net under all of them: when a dialog has closed and nothing holds focus, it goes back to the last control focused
// outside any dialog, else to the session list. A dialog or view that already returned focus is left alone.
import { type FocusEl, focusStranded, landFocus } from './focus-return.js';

let lastOutside: FocusEl | null = null;

export function initDialogFocus(doc: Document = document): void {
  doc.addEventListener('focusin', (ev) => {
    const t = ev.target;
    if (t instanceof HTMLElement && !t.closest('dialog')) lastOutside = t;
  }, true);
  // `close` does not bubble: listen in the capture phase for every dialog.
  doc.addEventListener('close', (ev) => {
    if (!(ev.target instanceof HTMLDialogElement)) return;
    // After the dialog's own handlers (remove, resolve, the opener's return) have run.
    window.setTimeout(() => {
      if (doc.querySelector('dialog[open]') || !focusStranded()) return;
      landFocus([lastOutside, 'session-list']);
    }, 0);
  }, true);
}
