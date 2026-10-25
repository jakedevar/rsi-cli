// DOM side of runtime errors after boot (model: runtime-model.ts; Issue #295). One dismissible notice, fixed to a
// corner so it never moves the layout (the composer stays on screen at 760x480) and never takes focus or blocks
// input. All text goes in with textContent. Repeats of one error update the count on the notice already shown.
import { el, errorDetails, lowerToast, raiseToast, recordNotice } from './dom.js';
import { keepFocus, setOpener } from './focus-return.js';
import { RuntimeTracker, repeatNote, runtimeLogLine, runtimeNotice } from './runtime-model.js';

const NOTICE_VIEW = 'runtime-notice';
const tracker = new RuntimeTracker();
let current: { key: string; count: number } | null = null;
let repeatEl: HTMLElement | null = null;

/** What the user is looking at, in words: the open dialog's label or title, or null on the main screen. */
export function currentViewName(): string | null {
  try {
    const open = document.querySelectorAll<HTMLElement>('dialog[open]');
    const dlg = open.length > 0 ? open[open.length - 1] : null;
    if (!dlg) return null;
    const label = dlg.getAttribute('aria-label') ?? dlg.querySelector('h1, h2, h3')?.textContent ?? '';
    const t = label.replace(/\s+/g, ' ').trim();
    return t === '' ? null : t.length > 40 ? `${t.slice(0, 39)}…` : t;
  } catch {
    return null;
  }
}

function host(): HTMLElement {
  let n = document.getElementById('runtime-notice');
  if (!n) {
    n = el('div', 'runtime-notice');
    n.id = 'runtime-notice';
    n.setAttribute('popover', 'manual');
    n.setAttribute('role', 'alert');
    n.setAttribute('aria-live', 'assertive');
    n.hidden = true;
    document.body.append(n);
    // Tabbing into the notice remembers the control focus came from, so Dismiss gives it back (local #297).
    n.addEventListener('focusin', (ev) => {
      const from = (ev as FocusEvent).relatedTarget as HTMLElement | null;
      if (from && !n!.contains(from)) setOpener(NOTICE_VIEW, from);
    });
  }
  return n;
}

/** Dismiss the notice (its button; also what a test presses). */
export function dismissRuntimeNotice(): void {
  const n = document.getElementById('runtime-notice');
  if (n) lowerToast(n, NOTICE_VIEW);
}

function draw(n: HTMLElement, text: string, detail: string, count: number): void {
  const body = el('div', 'runtime-notice-text');
  body.append(el('span', 'err-text', text));
  repeatEl = el('span', 'runtime-notice-repeat', repeatNote(count) ? ` ${repeatNote(count)}` : '');
  body.append(repeatEl);
  const close = el('button', 'btn small runtime-notice-dismiss', 'Dismiss');
  close.type = 'button';
  close.setAttribute('aria-label', 'Dismiss this notice');
  close.addEventListener('click', () => dismissRuntimeNotice());
  const head = el('div', 'runtime-notice-head');
  head.append(body, close);
  // A new error replaces the notice: focus on its Dismiss or Details moves to the rebuilt one, not the page.
  keepFocus(n, () => n.replaceChildren(head, errorDetails(detail)));
}

/**
 * One runtime error: the notice (first sight only; a repeat updates its count), the notification browser, and the
 * process log through `log`. Returns whether it was shown. `text` replaces the generic sentence when the caller
 * already has plain words for it (the menu bar, #305); `raw` stays under Details.
 */
export function reportRuntimeError(raw: string, log: (line: string) => void, now = Date.now(), text?: string): boolean {
  const generic = runtimeNotice(raw, currentViewName());
  if (!generic) return false;
  const note = text ? { ...generic, text } : generic;
  const tally = tracker.note(note.key, now);
  if (tally.log) log(runtimeLogLine(raw, tally.count));
  const n = host();
  if (tally.show) {
    current = { key: note.key, count: 1 };
    draw(n, note.text, note.detail, 1);
    recordNotice(note.text, note.detail);
    raiseToast(n);
    return true;
  }
  // A repeat of the notice on screen: only its count changes (kept open or dismissed as the user left it).
  if (current?.key === note.key) {
    current.count = tally.count;
    // Update the count in place: redrawing would close a Details section the user has open.
    if (!n.hidden && repeatEl) repeatEl.textContent = ` ${repeatNote(tally.count)}`;
  }
  return false;
}
